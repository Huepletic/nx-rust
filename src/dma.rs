//! Аллокация dma-buf для screencopy (linux_dmabuf-путь).
//!
//! Кандидаты и причины отказов — проверено на этой машине (2026-10-16):
//! - `/dev/dma_heap/system` — 0600 root:root, пользователю et недоступен;
//! - `/dev/dri/renderD128` — MODE_CREATE_DUMB возвращает EPERM (render-нода
//!   без modeset);
//! - `/dev/dri/cardN` (группа video) — работает: dumb → PRIME → mmap(dmabuf).
//!
//! dumb-буфер линейный (32bpp), модификатор = DRM_FORMAT_MOD_LINEAR = 0,
//! страйд возвращается ядром (для 1088 — ровно width*4 = 4352).

use anyhow::{Context, Result};
use memmap2::MmapMut;
use std::os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd};

const DRM_IOCTL_BASE: u32 = 0x64; // '_IOW*' как 'd'-код drm.h
const fn iowr(nr: u32, size: u32) -> u32 {
    (3 << 30) | (size << 16) | (DRM_IOCTL_BASE << 8) | nr
}
const fn iow(nr: u32, size: u32) -> u32 {
    (1 << 30) | (size << 16) | (DRM_IOCTL_BASE << 8) | nr
}

/// struct drm_mode_create_dumb {height, width, bpp, flags, handle, pitch} —
/// порядок полей проверен прямым замером (handle=1, pitch=4352).
const DRM_IOCTL_MODE_CREATE_DUMB: libc::c_ulong = iowr(0xB2, 24) as libc::c_ulong;
/// struct drm_prime_handle {handle, flags, fd} — ПОРЯДОК ПОЛЕЙ ВАЖЕН: fd идёт
/// последним (написать fd во второй u32 = мусорный flags → EINVAL на PRIME).
/// Распространённая ошибка — 8-байтная drm_prime_fd_from_handle: её нет в uapi.
const DRM_IOCTL_PRIME_HANDLE_TO_FD: libc::c_ulong = iowr(0x2D, 12) as libc::c_ulong;
const DRM_IOCTL_GEM_CLOSE: libc::c_ulong = iow(0x09, 8) as libc::c_ulong;

#[repr(C)]
#[derive(Default)]
struct CreateDumb {
    height: u32,
    width: u32,
    bpp: u32,
    flags: u32,
    handle: u32,
    pitch: u32,
}

#[repr(C)]
struct PrimeHandle {
    handle: u32,
    /// только для handle→fd
    flags: u32,
    fd: i32,
}

#[repr(C)]
#[derive(Default)]
struct GemClose {
    handle: u32,
    pad: u32,
}

pub struct DumbBuffer {
    drm_fd: RawFd,
    handle: u32,
    /// Страйд в байтах (из ядра; обычно width*4)
    pub stride: u32,
    /// CPU-доступ к пикселям: sink читает кадр через mmap dmabuf
    pub map: MmapMut,
    dmabuf: OwnedFd,
}

impl DumbBuffer {
    pub fn alloc(width: u32, height: u32) -> Result<Self> {
        let mut last: Option<anyhow::Error> = None;
        for path in ["/dev/dri/card0", "/dev/dri/card1", "/dev/dri/card2", "/dev/dri/card3"] {
            if !std::path::Path::new(path).exists() {
                continue;
            }
            match Self::open_on(path, width, height) {
                Ok(b) => return Ok(b),
                Err(e) => last = Some(e),
            }
        }
        Err(last.unwrap_or_else(|| anyhow::anyhow!("нет /dev/dri/card*")))
    }

    fn open_on(path: &str, width: u32, height: u32) -> Result<Self> {
        let cpath = std::ffi::CString::new(path).context("path")?;
        let fd = unsafe { libc::open(cpath.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
        if fd < 0 {
            anyhow::bail!("open {path}: {}", std::io::Error::last_os_error());
        }

        let mut cre = CreateDumb { height, width, bpp: 32, ..Default::default() };
        if unsafe { libc::ioctl(fd, DRM_IOCTL_MODE_CREATE_DUMB, &mut cre as *mut _) } < 0 {
            let e = std::io::Error::last_os_error();
            unsafe { libc::close(fd) };
            anyhow::bail!("MODE_CREATE_DUMB {path}: {e}");
        }

        // DRM_RDWR обязателен: без него fd выдаётся read-only,
        // а mmap(dmabuf, PROT_WRITE) на таком fd падает с EACCES.
        let mut pr = PrimeHandle {
            handle: cre.handle,
            fd: -1,
            flags: (libc::O_RDWR | libc::O_CLOEXEC) as u32,
        };
        if unsafe { libc::ioctl(fd, DRM_IOCTL_PRIME_HANDLE_TO_FD, &mut pr as *mut _) } < 0 {
            let e = std::io::Error::last_os_error();
            unsafe {
                libc::ioctl(fd, DRM_IOCTL_GEM_CLOSE, &GemClose { handle: cre.handle, pad: 0 } as *const _);
                libc::close(fd);
            }
            anyhow::bail!("PRIME_HANDLE_TO_FD {path}: {e}");
        }
        let dmabuf = unsafe { OwnedFd::from_raw_fd(pr.fd) };

        // mmap через дуплицированный fd: оригинальный OwnedFd держим до
        // конца жизни (закрытие fd не отменяет маппинг, но GEM-close'у fd нужен).
        let dup = unsafe { libc::dup(dmabuf.as_raw_fd()) };
        if dup < 0 {
            let e = std::io::Error::last_os_error();
            unsafe {
                libc::ioctl(fd, DRM_IOCTL_GEM_CLOSE, &GemClose { handle: cre.handle, pad: 0 } as *const _);
                libc::close(fd);
            }
            anyhow::bail!("dup(dmabuf): {e}");
        }
        let file = unsafe { std::fs::File::from_raw_fd(dup) };
        let map = match unsafe { MmapMut::map_mut(&file) } {
            Ok(m) => m,
            Err(e) => {
                drop(file);
                unsafe {
                    libc::ioctl(fd, DRM_IOCTL_GEM_CLOSE, &GemClose { handle: cre.handle, pad: 0 } as *const _);
                    libc::close(fd);
                }
                anyhow::bail!("mmap(dmabuf): {e}");
            }
        };
        drop(file);

        Ok(Self {
            drm_fd: fd,
            handle: cre.handle,
            stride: cre.pitch,
            map,
            dmabuf,
        })
    }

    pub fn dmabuf_fd(&self) -> BorrowedFd<'_> {
        // SAFETY: OwnedFd живёт, пока живёт DumbBuffer
        unsafe { BorrowedFd::borrow_raw(self.dmabuf.as_raw_fd()) }
    }
}

impl Drop for DumbBuffer {
    fn drop(&mut self) {
        unsafe {
            libc::ioctl(self.drm_fd, DRM_IOCTL_GEM_CLOSE, &GemClose { handle: self.handle, pad: 0 } as *const _);
            libc::close(self.drm_fd);
        }
        // map (munmap) и dmabuf (close) закроются полями
    }
}
