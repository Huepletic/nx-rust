//! nx-server: encode-probe (Milestone 1)
//! Захватывает N кадров с Wayland-output, кодирует в H.264 через VA-API (ffmpeg),
//! сохраняет .h264 и контрольный PNG. Верификация — что encode/decode не изменил кадр.
//! Использование: nx-server encode <output-name|auto> <frames> <out.h264> <ref.png>

use anyhow::{bail, Context, Result};
use memmap2::MmapMut;
use std::io::{Read, Write};
use std::os::fd::{AsFd, AsRawFd, RawFd};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};
use wayland_client::protocol::{
    wl_buffer::WlBuffer,
    wl_compositor::WlCompositor,
    wl_output::{Event as OutputEvent, WlOutput},
    wl_registry::{Event as RegistryEvent, WlRegistry},
    wl_shm::{Format, WlShm},
    wl_shm_pool::WlShmPool,
    wl_surface::WlSurface,
};
use wayland_client::event_created_child;
use wayland_client::{Connection, Dispatch, EventQueue, QueueHandle};

use wayland_protocols::wp::idle_inhibit::zv1::client::{
    zwp_idle_inhibit_manager_v1::ZwpIdleInhibitManagerV1,
    zwp_idle_inhibitor_v1::ZwpIdleInhibitorV1,
};
use wayland_protocols::wp::linux_dmabuf::zv1::client::{
    zwp_linux_buffer_params_v1::{
        Event as ParamsEvent, Flags as DmabufFlags, ZwpLinuxBufferParamsV1,
    },
    zwp_linux_dmabuf_v1::ZwpLinuxDmabufV1,
};
use wayland_protocols_wlr::screencopy::v1::client::{
    zwlr_screencopy_frame_v1::{Event as FrameEvent, ZwlrScreencopyFrameV1},
    zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1,
};

use crate::dma::DumbBuffer;

#[derive(Clone, Copy)]
struct BufferParams {
    format: u32,
    width: u32,
    height: u32,
    stride: u32,
}

/// Событие zwlr_screencopy_frame_v1.linux_dmabuf (v3): комозитор подсказывает
/// формат/геометрию для dmabuf-буфера (1088x800 → 'AR24').
#[derive(Clone, Copy)]
struct DmabufParams {
    format: u32,
    width: u32,
    height: u32,
}

/// DRM fourcc 'AR24'/'XR24' little-endian (875713089 / 879048216)
const DRM_FORMAT_ARGB8888: u32 = u32::from_le_bytes(*b"AR24");
const DRM_FORMAT_XRGB8888: u32 = u32::from_le_bytes(*b"XR24");

struct App {
    shm: Option<WlShm>,
    screencopy_mgr: Option<ZwlrScreencopyManagerV1>,
    dma_mgr: Option<ZwpLinuxDmabufV1>,
    compositor: Option<WlCompositor>,
    idle_inhibit_mgr: Option<ZwpIdleInhibitManagerV1>,
    outputs: Vec<(u32, WlOutput, String)>,
    /// По одному состоянию на слот: 2 — пайплайн двух screencopy-запросов,
    /// 1 — старый последовательный режим (A/B через NX_PIPE).
    frames: [FrameState; 2],
}

#[derive(Default)]
struct FrameState {
    buffer_ready: bool,
    failed: bool,
    params: Option<BufferParams>,
    dmabuf: Option<DmabufParams>,
    copy_done: bool,
    /// wl_buffer, пришедший событием created (создание dmabuf — асинхронное)
    dma_wl: Option<WlBuffer>,
    dma_failed: bool,
}

/// Событийное ожидание вместо опроса roundtrip+sleep.
///
/// Старая схема: крутим `roundtrip` (write+read сокета каждые ~0.1–0.5мс), а
/// каждые 25 итераций спим 4мс. Событие compositor'а могло прилететь сразу
/// после засыпания → до 4мс латентности реакции на каждое ожидание
/// (buffer_params и copy/Ready), то есть ~2 кадра/с на каждое.
///
/// Новая схема: flush накопленных запросов → dispatch уже прочитанного →
/// `poll()` на wayland-fd спит ровно до события или wall-clock дедлайна →
/// `prepare_read().read()` → dispatch. Просыпаемся в момент прихода байтов.
///
/// `legacy_sleep` — для A/B-режима `NX_WAIT=rt` (старая схема roundtrip-опроса
/// с sleep 4мс каждые 25 итераций — как было в copy; у params sleep не было).
/// Нужен честный «до/после» в одинаковых условиях окружения без пересборки.
fn wait_for(
    queue: &mut EventQueue<App>,
    state: &mut App,
    deadline: Duration,
    mut ready: impl FnMut(&App) -> bool,
    legacy_sleep: bool,
) -> Result<bool> {
    use std::os::fd::AsRawFd;
    use wayland_client::backend::WaylandError;

    // A/B: NX_WAIT=rt — старая схема (roundtrip-спин, квантование по sleep).
    static LEGACY: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    let legacy = *LEGACY.get_or_init(|| {
        std::env::var("NX_WAIT").ok().as_deref() == Some("rt")
    });
    // NX_DBG: построчный тайминг первых ожиданий (мс от входа в wait_for) —
    // локализация систематической разницы params rt vs poll.
    static DBG_N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let dbg = std::env::var_os("NX_DBG").is_some()
        && DBG_N.fetch_add(1, std::sync::atomic::Ordering::Relaxed) < 12;
    let label = if deadline.as_millis() == 100 { "params" } else { "copy" };
    let ms_now = |t0: &Instant| t0.elapsed().as_secs_f64() * 1000.0;
    let t_entry = Instant::now();
    let mut dbg_flush = false;
    let mut dbg_wake = false;

    let start = Instant::now();
    if legacy {
        let mut tries = 0u32;
        loop {
            if ready(state) {
                if dbg { eprintln!("[dbg] {label} ready @{:.2}мс (rt)", ms_now(&t_entry)); }
                return Ok(true);
            }
            if start.elapsed() >= deadline {
                if dbg { eprintln!("[dbg] {label} TIMEOUT @{:.2}мс (rt)", ms_now(&t_entry)); }
                return Ok(false);
            }
            queue.roundtrip(state)?;
            tries += 1;
            if legacy_sleep && tries % 25 == 0 {
                std::thread::sleep(Duration::from_millis(4));
            }
        }
    }

    loop {
        if ready(state) {
            if dbg { eprintln!("[dbg] {label} ready @{:.2}мс (poll)", ms_now(&t_entry)); }
            return Ok(true);
        }
        // Отправляем накопленные запросы (capture_output/copy). WouldBlock не
        // фатален: неотправленное остаётся в буфере и уйдёт на следующем flush.
        if let Err(e) = queue.flush() {
            match e {
                WaylandError::Io(io) if io.kind() == std::io::ErrorKind::WouldBlock => {}
                _ => return Err(e.into()),
            }
        }
        if dbg && !dbg_flush {
            dbg_flush = true;
            eprintln!("[dbg] {label} flush @{:.2}мс", ms_now(&t_entry));
        }
        queue.dispatch_pending(state)?;
        if ready(state) {
            if dbg { eprintln!("[dbg] {label} ready @{:.2}мс (poll, из queue)", ms_now(&t_entry)); }
            return Ok(true);
        }
        let left = deadline.checked_sub(start.elapsed()).unwrap_or_default();
        if left.is_zero() {
            if dbg { eprintln!("[dbg] {label} TIMEOUT @{:.2}мс (poll)", ms_now(&t_entry)); }
            return Ok(false);
        }
        let mut fds = [libc::pollfd {
            fd: queue.as_fd().as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        }];
        // Спим в poll до события либо дедлайна (квантование ≤50мс, дедлайн
        // и так меньше — так мы просто перепроверяем готовность в цикле).
        let ms = (left.as_millis().min(50) as i32).max(1);
        let r = unsafe { libc::poll(fds.as_mut_ptr(), 1, ms) };
        if r < 0 {
            let e = std::io::Error::last_os_error();
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e.into());
        }
        if r > 0 {
            if dbg && !dbg_wake {
                dbg_wake = true;
                eprintln!("[dbg] {label} poll-wake @{:.2}мс", ms_now(&t_entry));
            }
            // Есть байты: читаем их в guarded-режиме и разбираем события.
            if let Some(guard) = queue.prepare_read() {
                // EAGAIN/WouldBlock не фатален: неблокирующий wayland-сокет
                // возвращает его на частичном сообщении / spurious-wake после
                // poll. Раньше `?` гнал его наверх и убивал поток захвата
                // («Io error: Resource temporarily unavailable (os error 11)»
                // → «capture: поток завершён» → замороженная картинка). Байтов
                // нет/неполно — крутим цикл дальше.
                match guard.read() {
                    Ok(_) => {}
                    Err(WaylandError::Io(io))
                        if io.kind() == std::io::ErrorKind::WouldBlock => {}
                    Err(e) => return Err(e.into()),
                }
            }
            queue.dispatch_pending(state)?;
            if dbg {
                eprintln!("[dbg] {label} read+dispatch @{:.2}мс", ms_now(&t_entry));
            }
        }
    }
}

/// Capture-probe: один кадр с output → PNG.
pub fn capture_probe(want: &str, out_path: &str) -> Result<()> {
    let conn = Connection::connect_to_env().context("нет WAYLAND_DISPLAY")?;
    let display = conn.display();
    let mut event_queue = conn.new_event_queue();
    let qh = event_queue.handle();
    let mut state = App {
        shm: None,
        screencopy_mgr: None,
        dma_mgr: None,
        compositor: None,
        idle_inhibit_mgr: None,
        outputs: Vec::new(),
        frames: Default::default(),
    };
    let _registry = display.get_registry(&qh, ());
    event_queue.roundtrip(&mut state)?;
    event_queue.roundtrip(&mut state)?;
    let mgr = state.screencopy_mgr.clone().context("нет zwlr_screencopy_manager_v1")?;
    let shm = state.shm.clone().context("нет wl_shm")?;
    let (out_name, out_obj) = pick_output(&state, want)?;

    state.frames[0] = FrameState::default();
    let frame = mgr.capture_output(0, &out_obj, &qh, 0usize);
    let mut tries = 0;
    while !state.frames[0].buffer_ready && !state.frames[0].failed {
        event_queue.roundtrip(&mut state)?;
        if { tries += 1; tries > 100 } { bail!("timeout buffer_params"); }
    }
    let p = state.frames[0].params.context("нет buffer_params")?;
    let (w, h, stride) = (p.width, p.height, p.stride);
    let size = (stride * h) as usize;
    let (pool, mmap, _file) = create_shm_pool(&qh, &shm, size)?;
    let buffer: WlBuffer = pool.create_buffer(
        0, w as i32, h as i32, stride as i32,
        params_format_as_wl(p.format), &qh, (),
    );
    frame.copy(&buffer);
    let mut tries = 0;
    while !state.frames[0].copy_done && !state.frames[0].failed {
        event_queue.roundtrip(&mut state)?;
        if { tries += 1; tries > 200 } { bail!("timeout copy"); }
    }
    if state.frames[0].failed { bail!("failed copy"); }
    let raw = bgra_from_xrgb(&mmap, w, h, stride);
    let rgba = raw_bgra_to_rgba(&raw, w, h);
    save_png(&rgba, w, h, out_path)?;
    println!("OK: {} -> {} ({}x{})", out_name, out_path, w, h);
    buffer.destroy();
    pool.destroy();
    Ok(())
}

fn pick_output(state: &App, want: &str) -> Result<(String, WlOutput)> {
    if want == "auto" {
        let first = &state.outputs[0];
        return Ok((first.2.clone(), first.1.clone()));
    }
    state
        .outputs
        .iter()
        .find(|(_, _, name)| name == want)
        .map(|(_, o, name)| (name.clone(), o.clone()))
        .with_context(|| format!("output '{}' не найден; есть: {:?}", want,
            state.outputs.iter().map(|o| o.2.clone()).collect::<Vec<_>>()))
}

/// Encode-probe: N кадров → H.264 (VA-API) + эталонный PNG первого кадра.
pub fn encode_probe(want: &str, nframes: usize, out_h264: &str, ref_png: &str) -> Result<()> {

    let conn = Connection::connect_to_env().context("нет WAYLAND_DISPLAY")?;
    let display = conn.display();
    let mut event_queue = conn.new_event_queue();
    let qh = event_queue.handle();

    let mut state = App {
        shm: None,
        screencopy_mgr: None,
        dma_mgr: None,
        compositor: None,
        idle_inhibit_mgr: None,
        outputs: Vec::new(),
        frames: Default::default(),
    };

    let _registry = display.get_registry(&qh, ());
    event_queue.roundtrip(&mut state)?;
    event_queue.roundtrip(&mut state)?;

    let mgr = state.screencopy_mgr.clone().context("нет zwlr_screencopy_manager_v1")?;
    let shm = state.shm.clone().context("нет wl_shm")?;

    let (out_name, out_obj) = if want == "auto" {
        let first = &state.outputs[0];
        (first.2.clone(), first.1.clone())
    } else {
        state
            .outputs
            .iter()
            .find(|(_, _, name)| name == want)
            .map(|(_, o, name)| (name.clone(), o.clone()))
            .with_context(|| format!("output '{}' не найден", want))?
    };

    // Первый захват — узнать размеры
    let (w, h, stride) = {
        let frame = mgr.capture_output(0, &out_obj, &qh, 0usize);
        let mut tries = 0;
        while !state.frames[0].buffer_ready && !state.frames[0].failed {
            event_queue.roundtrip(&mut state)?;
            if { tries += 1; tries > 100 } { bail!("timeout buffer_params"); }
        }
        let p = state.frames[0].params.context("нет buffer_params")?;
        frame.destroy();
        (p.width, p.height, p.stride)
    };
    println!("capture: {} {}x{} stride={}", out_name, w, h, stride);

    // VA-API через ffmpeg: сырой BGRA на входе (pipe), h264_vaapi на выходе
    let mut enc = Command::new("ffmpeg")
        .args([
            "-y",
            "-loglevel", "error",
            "-f", "rawvideo",
            "-pixel_format", "bgra",
            "-video_size", &format!("{}x{}", w, h),
            "-framerate", "60",
            "-i", "pipe:0",
            "-vaapi_device", "/dev/dri/renderD128",
            "-vf", "format=nv12,hwupload",
            "-c:v", "h264_vaapi",
            "-b:v", "10M",
            "-g", "120",
            "-bf", "0",              // без B-frames — низкая латентность
            "-f", "h264",
            out_h264,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .context("запуск ffmpeg (нужен libva + h264_vaapi)")?;
    let mut enc_stdin = enc.stdin.take().context("ffmpeg stdin")?;

    let mut first_raw: Option<Vec<u8>> = None;
    let size = (stride * h) as usize;
    for i in 0..nframes {
        // свежий state.frame на каждый кадр
        state.frames[0] = FrameState::default();
        let frame = mgr.capture_output(0, &out_obj, &qh, 0usize);
        let mut tries = 0;
        while !state.frames[0].buffer_ready && !state.frames[0].failed {
            event_queue.roundtrip(&mut state)?;
            if { tries += 1; tries > 100 } { bail!("timeout buffer_params кадр {}", i); }
        }
        let p = state.frames[0].params.context("нет buffer_params")?;

        let (pool, mmap, _file) = create_shm_pool(&qh, &shm, size)?;
        let buffer: WlBuffer = pool.create_buffer(
            0, w as i32, h as i32, stride as i32,
            params_format_as_wl(p.format), &qh, (),
        );
        frame.copy(&buffer);
        let mut tries = 0;
        while !state.frames[0].copy_done && !state.frames[0].failed {
            event_queue.roundtrip(&mut state)?;
            if { tries += 1; tries > 200 } { bail!("timeout copy кадр {}", i); }
        }
        if state.frames[0].failed { bail!("failed copy кадр {}", i); }
        frame.destroy();

        // raw BGRA (little-endian XRGB8888 → BGRA)
        let raw = bgra_from_xrgb(&mmap, w, h, stride);
        if i == 0 {
            first_raw = Some(raw.clone());
        }
        enc_stdin.write_all(&raw).context("запись в ffmpeg")?;
        buffer.destroy();
        pool.destroy();
        drop(mmap);
        if i % 30 == 0 { println!("frame {}/{}", i, nframes); }
    }
    drop(enc_stdin);
    let status = enc.wait().context("ожидание ffmpeg")?;
    if !status.success() {
        let mut err = String::new();
        enc.stderr.take().map(|mut e| { e.read_to_string(&mut err).ok(); });
        bail!("ffmpeg завершился с ошибкой: {}", err);
    }

    // Контрольный PNG из первого кадра (эталон)
    if let Some(raw) = first_raw {
        let rgba = raw_bgra_to_rgba(&raw, w, h);
        save_png(&rgba, w, h, ref_png)?;
        println!("эталонный PNG: {}", ref_png);
    }
    let meta = std::fs::metadata(out_h264).context("h264 файл")?;
    println!("OK: {} кадров -> {} ({} байт, {} Мбит/кадр≈{})",
        nframes, out_h264, meta.len(),
        (meta.len() * 8 / nframes as u64) / 1024, "");
    Ok(())
}

/// Бенчмарк латентности screencopy: время buffer_params и copy-ready на кадр
/// (изолирует compositor: без ffmpeg/RTP). Для диагностики «почему захват ~3fps».
pub fn copy_bench(want: &str, n: usize) -> Result<()> {
    let conn = Connection::connect_to_env().context("нет WAYLAND_DISPLAY")?;
    let display = conn.display();
    let mut event_queue = conn.new_event_queue();
    let qh = event_queue.handle();
    let mut state = App {
        shm: None,
        screencopy_mgr: None,
        dma_mgr: None,
        compositor: None,
        idle_inhibit_mgr: None,
        outputs: Vec::new(),
        frames: Default::default(),
    };
    let _registry = display.get_registry(&qh, ());
    event_queue.roundtrip(&mut state)?;
    event_queue.roundtrip(&mut state)?;
    let mgr = state.screencopy_mgr.clone().context("нет zwlr_screencopy_manager_v1")?;
    let shm = state.shm.clone().context("нет wl_shm")?;
    let (out_name, out_obj) = pick_output(&state, want)?;

    state.frames[0] = FrameState::default();
    let fr = mgr.capture_output(0, &out_obj, &qh, 0usize);
    let mut tries = 0;
    while !state.frames[0].buffer_ready && !state.frames[0].failed {
        event_queue.roundtrip(&mut state)?;
        if { tries += 1; tries > 1000 } { bail!("timeout buffer_params"); }
    }
    let p = state.frames[0].params.context("нет buffer_params")?;
    let (w, h, stride) = (p.width, p.height, p.stride);
    fr.destroy();
    let size = (stride * h) as usize;
    println!("copybench: {} {}x{} stride={}", out_name, w, h, stride);

    let mut lats: Vec<u128> = Vec::new();
    for i in 0..n {
        state.frames[0] = FrameState::default();
        let t0 = std::time::Instant::now();
        let frame = mgr.capture_output(0, &out_obj, &qh, 0usize);
        let mut tries = 0;
        while !state.frames[0].buffer_ready && !state.frames[0].failed {
            event_queue.roundtrip(&mut state)?;
            if { tries += 1; tries > 3000 } { bail!("timeout params кадр {}", i); }
        }
        let t_params = t0.elapsed();
        let p = state.frames[0].params.context("нет buffer_params")?;
        let (pool, _mmap, _file) = create_shm_pool(&qh, &shm, size)?;
        let buffer: WlBuffer = pool.create_buffer(
            0, w as i32, h as i32, stride as i32,
            params_format_as_wl(p.format), &qh, (),
        );
        let t1 = std::time::Instant::now();
        frame.copy(&buffer);
        let mut tries = 0u32;
        while !state.frames[0].copy_done && !state.frames[0].failed {
            event_queue.roundtrip(&mut state)?;
            tries += 1;
            if tries % 20 == 0 {
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            if tries > 2000 { break; }
        }
        let t_copy = t1.elapsed();
        let done = state.frames[0].copy_done;
        frame.destroy();
        buffer.destroy();
        pool.destroy();
        lats.push(t_copy.as_millis());
        println!(
            "frame {}: params={}ms copy={}ms{}",
            i, t_params.as_millis(), t_copy.as_millis(),
            if done { "" } else { "  <<COPY TIMEOUT" }
        );
    }
    lats.sort_unstable();
    println!(
        "copy ms: min={} med={} max={}",
        lats.first().copied().unwrap_or(0),
        lats.get(lats.len() / 2).copied().unwrap_or(0),
        lats.last().copied().unwrap_or(0)
    );
    Ok(())
}

/// Откуда берём кадры. KWin на Wayland не умеет zwlr-screencopy, поэтому
/// для KDE/GNOME используется xdg-desktop-portal + PipeWire; для X11 — x11grab.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Wlr,
    Portal,
    X11,
}

impl Backend {
    pub fn name(self) -> &'static str {
        match self {
            Backend::Wlr => "wlr-screencopy",
            Backend::Portal => "xdg-desktop-portal/PipeWire",
            Backend::X11 => "x11grab",
        }
    }
}

/// NX_CAPTURE=auto|wlr|portal|x11. Явное имя "portal"/"x11" тоже работает.
pub fn backend_for(want: &str) -> Backend {
    let forced = std::env::var("NX_CAPTURE")
        .unwrap_or_default()
        .to_ascii_lowercase();
    match forced.as_str() {
        "wlr" | "wayland" | "screencopy" => return Backend::Wlr,
        "portal" | "pipewire" => return Backend::Portal,
        "x11" | "xorg" => return Backend::X11,
        _ => {}
    }
    match want {
        "portal" => return Backend::Portal,
        "x11" => return Backend::X11,
        _ => {}
    }

    let session = session_type();
    if session == "x11" {
        return Backend::X11;
    }
    if has_wlr_screencopy().unwrap_or(false) {
        return Backend::Wlr;
    }
    if session == "wayland" {
        return Backend::Portal;
    }

    let desktop = std::env::var("XDG_CURRENT_DESKTOP")
        .unwrap_or_default()
        .to_ascii_lowercase();
    let kde = desktop.contains("kde")
        || desktop.contains("plasma")
        || std::env::var("KDE_FULL_SESSION").as_deref() == Ok("true")
        || std::env::var("DESKTOP_SESSION")
            .map(|s| s.to_ascii_lowercase().contains("plasma"))
            .unwrap_or(false);
    let gnome = desktop.contains("gnome");
    let has_wayland = !wayland_socket_names().is_empty();
    if (kde || gnome) && has_wayland {
        return Backend::Portal;
    }
    if std::env::var_os("DISPLAY").is_some() && std::env::var_os("WAYLAND_DISPLAY").is_none() {
        return Backend::X11;
    }
    if has_wayland {
        return Backend::Portal;
    }
    if std::env::var_os("DISPLAY").is_some() {
        return Backend::X11;
    }
    // Последний шанс: пусть probe выдаст внятную ошибку про WAYLAND_DISPLAY.
    Backend::Wlr
}

fn session_type() -> String {
    if let Ok(s) = std::env::var("XDG_SESSION_TYPE") {
        if !s.trim().is_empty() {
            return s.trim().to_ascii_lowercase();
        }
    }
    if let Ok(id) = std::env::var("XDG_SESSION_ID") {
        if let Ok(out) = Command::new("loginctl")
            .args(["show-session", &id, "-p", "Type", "--value"])
            .output()
        {
            if out.status.success() {
                return String::from_utf8_lossy(&out.stdout).trim().to_ascii_lowercase();
            }
        }
    }
    String::new()
}

fn wayland_socket_names() -> Vec<String> {
    let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") else {
        return Vec::new();
    };
    let mut names = Vec::new();
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let n = e.file_name().to_string_lossy().to_string();
            if n.starts_with("wayland-") && !n.ends_with(".lock") {
                names.push(n);
            }
        }
    }
    names.sort();
    names
}

/// Есть ли в текущем compositor'е zwlr_screencopy_manager_v1.
/// Отдельное соединение, без выбора output; KWin возвращает false.
pub fn has_wlr_screencopy() -> Result<bool> {
    let conn = Connection::connect_to_env().context("нет WAYLAND_DISPLAY")?;
    let display = conn.display();
    let mut event_queue = conn.new_event_queue();
    let qh = event_queue.handle();
    let mut state = App {
        shm: None,
        screencopy_mgr: None,
        dma_mgr: None,
        compositor: None,
        idle_inhibit_mgr: None,
        outputs: Vec::new(),
        frames: Default::default(),
    };
    let _registry = display.get_registry(&qh, ());
    event_queue.roundtrip(&mut state)?;
    event_queue.roundtrip(&mut state)?;
    Ok(state.screencopy_mgr.is_some())
}

/// Размер первого кадра output (pipeline: узнать w/h до запуска ffmpeg).
/// Для KDE/GNOME здесь открывается портал (диалог выбора экрана); для X11 —
/// xrandr.
pub fn probe_output_size(want: &str) -> Result<(u32, u32)> {
    match backend_for(want) {
        Backend::Portal => return crate::portal::open_capture(),
        Backend::X11 => return x11_screen_size(),
        Backend::Wlr => {}
    }
    wlr_probe_output_size(want)
}

fn wlr_probe_output_size(want: &str) -> Result<(u32, u32)> {
    let conn = Connection::connect_to_env().context("нет WAYLAND_DISPLAY")?;
    let display = conn.display();
    let mut event_queue = conn.new_event_queue();
    let qh = event_queue.handle();
    let mut state = App {
        shm: None,
        screencopy_mgr: None,
        dma_mgr: None,
        compositor: None,
        idle_inhibit_mgr: None,
        outputs: Vec::new(),
        frames: Default::default(),
    };
    let _registry = display.get_registry(&qh, ());
    event_queue.roundtrip(&mut state)?;
    event_queue.roundtrip(&mut state)?;
    let mgr = state.screencopy_mgr.clone().context("нет zwlr_screencopy_manager_v1")?;
    let (_name, out_obj) = pick_output(&state, want)?;
    let frame = mgr.capture_output(0, &out_obj, &qh, 0usize);
    let mut tries = 0;
    while !state.frames[0].buffer_ready && !state.frames[0].failed {
        event_queue.roundtrip(&mut state)?;
        if { tries += 1; tries > 100 } { bail!("timeout buffer_params"); }
    }
    let p = state.frames[0].params.context("нет buffer_params")?;
    frame.destroy();
    Ok((p.width, p.height))
}

/// Размер X11-экрана (KDE Plasma X11 / любой Xorg). xrandr --current даёт
/// размер всего Screen, а не одного монитора: x11grab снимает весь экран.
pub fn x11_screen_size() -> Result<(u32, u32)> {
    if std::env::var_os("DISPLAY").is_none() {
        bail!("X11: не задан DISPLAY");
    }
    if let Ok(out) = Command::new("xrandr").arg("--current").output() {
        if out.status.success() {
            let txt = String::from_utf8_lossy(&out.stdout);
            if let Some(v) = parse_xrandr_current(&txt) {
                return Ok(v);
            }
            if let Some(v) = parse_xrandr_connected(&txt) {
                return Ok(v);
            }
        }
    }
    if let Ok(out) = Command::new("xdpyinfo").output() {
        if out.status.success() {
            let txt = String::from_utf8_lossy(&out.stdout);
            for line in txt.lines() {
                if !line.contains("dimensions:") {
                    continue;
                }
                for tok in line.split_whitespace() {
                    if let Some(v) = parse_x11_geometry(tok) {
                        return Ok(v);
                    }
                }
            }
        }
    }
    bail!("X11: не удалось определить размер экрана (xrandr/xdpyinfo)")
}

fn parse_u32_token(tok: &str) -> Option<u32> {
    let digits: String = tok.chars().filter(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

fn parse_xrandr_current(txt: &str) -> Option<(u32, u32)> {
    for line in txt.lines() {
        let Some(pos) = line.find("current ") else {
            continue;
        };
        let rest = &line[pos + "current ".len()..];
        let mut it = rest.split_whitespace();
        let w = parse_u32_token(it.next()?)?;
        if it.next()? != "x" {
            continue;
        }
        let h = parse_u32_token(it.next()?)?;
        if w > 0 && h > 0 {
            return Some((w, h));
        }
    }
    None
}

fn parse_x11_geometry(tok: &str) -> Option<(u32, u32)> {
    let x = tok.find('x')?;
    let w: u32 = tok[..x]
        .trim_matches(|c: char| !c.is_ascii_digit())
        .parse()
        .ok()?;
    let rest = &tok[x + 1..];
    let end = rest
        .find(|c: char| !c.is_ascii_digit())
        .unwrap_or(rest.len());
    let h: u32 = rest[..end].parse().ok()?;
    if w > 0 && h > 0 { Some((w, h)) } else { None }
}

fn parse_xrandr_connected(txt: &str) -> Option<(u32, u32)> {
    for line in txt.lines() {
        if !line.contains(" connected") {
            continue;
        }
        for tok in line.split_whitespace() {
            if let Some(v) = parse_x11_geometry(tok) {
                return Some(v);
            }
        }
    }
    None
}

/// X11-захват: отдельный ffmpeg x11grab → raw BGRA в общий pipeline.
/// На KDE Plasma X11 это единственный путь: wlr-screencopy там нет,
/// а портал ScreenCast на X11 не обязателен.
fn x11_capture_loop<F>(
    fps: u32,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    mut sink: F,
) -> Result<()>
where
    F: FnMut(FrameRef) -> Result<()>,
{
    use std::sync::atomic::Ordering;

    let (w, h) = x11_screen_size()?;
    let display = std::env::var("DISPLAY").context("X11: нет DISPLAY")?;
    let fps = fps.max(1);
    let log_path = std::env::var("NX_X11GRAB_LOG")
        .unwrap_or_else(|_| "/tmp/nx-x11grab.log".to_string());

    let mut cmd = Command::new("ffmpeg");
    cmd.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-f",
        "x11grab",
        "-framerate",
        &fps.to_string(),
        "-video_size",
        &format!("{}x{}", w, h),
        "-i",
        &format!("{}+0,0", display),
        "-f",
        "rawvideo",
        "-pix_fmt",
        "bgra",
        "-",
    ])
    .stdout(Stdio::piped());
    if let Ok(f) = std::fs::File::create(&log_path) {
        cmd.stderr(Stdio::from(f));
    } else {
        cmd.stderr(Stdio::null());
    }
    let mut child = cmd.spawn().context("запуск ffmpeg x11grab")?;
    let mut stdout = child.stdout.take().context("x11grab: нет stdout")?;
    let mut buf = vec![0u8; (w as usize) * (h as usize) * 4];
    let mut result = Ok(());
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        match stdout.read_exact(&mut buf) {
            Ok(()) => {
                if let Err(e) = sink(FrameRef::Bgra(&buf, w, h)) {
                    result = Err(e);
                    break;
                }
            }
            Err(e) => {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                result = Err(anyhow::anyhow!(
                    "ffmpeg x11grab: {e} (лог {})",
                    log_path
                ));
                break;
            }
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    result
}

/// Захват ТОЙ ЖЕ веткой `capture_loop`, но sink молчит (нет ffmpeg, нет RTP) —
/// измеряет потолок композитора + zwlr_screencopy в изоляции от энкодера.
pub fn capture_bench(want: &str, secs: u64) -> Result<()> {
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let s = stop.clone();
    std::thread::spawn(move || {
        std::thread::sleep(std::time::Duration::from_secs(secs));
        s.store(true, std::sync::atomic::Ordering::Relaxed);
    });
    let started = std::time::Instant::now();
    // NX_SINK_READ=1: closure читает все пиксели каждого кадра (как реальный стрим
    // в pipeline.rs — extend_from_slice в пул-буфер). Без флага closure только
    // считает — bench не меряет стоимость чтения mmap (shm cached vs dmabuf UC).
    let read_px = std::env::var("NX_SINK_READ").ok().as_deref() == Some("1");
    let mut pool: Vec<u8> = Vec::new();
    let r = capture_loop(want, 60, stop, |fr| {
        // FrameRef::Dma намеренно НЕ читаем: bench меряет только композитор+screencopy.
        if read_px {
            if let FrameRef::Bgra(data, w, h) = fr {
                pool.clear();
                pool.extend_from_slice(&data[..(w * h * 4) as usize]);
            }
        }
        Ok(())
    });
    if let Err(e) = r {
        println!("capturebench: {} (через {:.1}с)", e, started.elapsed().as_secs_f64());
    }
    Ok(())
}

/// Захват в цикле с пейсингом по fps; backend выбирается автоматически.
pub fn capture_loop<F>(
    want: &str,
    fps: u32,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    sink: F,
) -> Result<()>
where
    F: FnMut(FrameRef) -> Result<()>,
{
    match backend_for(want) {
        Backend::Portal => return crate::portal::capture_loop(fps, stop, sink),
        Backend::X11 => return x11_capture_loop(fps, stop, sink),
        Backend::Wlr => {}
    }
    wlr_capture_loop(want, fps, stop, sink)
}

/// Захват в цикле с пейсингом по fps; каждый кадр отдаётся в sink как BGRA
/// (альфа не важна — свес в YUV её игнорирует).
/// stride == w*4 → отдаём mmap напрямую (XRGB8888 LE байтово = BGRA), иначе построчный копир.
fn wlr_capture_loop<F>(
    want: &str,
    fps: u32,
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
    mut sink: F,
) -> Result<()>
where
    F: FnMut(FrameRef) -> Result<()>,
{
    use std::sync::atomic::Ordering;
    use std::time::{Duration, Instant};

    let conn = Connection::connect_to_env().context("нет WAYLAND_DISPLAY")?;
    let display = conn.display();
    let mut event_queue = conn.new_event_queue();
    let qh = event_queue.handle();
    let mut state = App {
        shm: None,
        screencopy_mgr: None,
        dma_mgr: None,
        compositor: None,
        idle_inhibit_mgr: None,
        outputs: Vec::new(),
        frames: Default::default(),
    };
    let _registry = display.get_registry(&qh, ());
    event_queue.roundtrip(&mut state)?;
    event_queue.roundtrip(&mut state)?;
    let mgr = state.screencopy_mgr.clone().context("нет zwlr_screencopy_manager_v1")?;
    let shm = state.shm.clone().context("нет wl_shm")?;
    let dma_mgr = state.dma_mgr.clone();
    let (out_name, out_obj) = pick_output(&state, want)?;

    // (B) idle-inhibit на время захвата: пока идёт стрим, композитор не шлёт
    // idle-события → hypridle не срабатывает → `loginctl lock-session`/hyprlock
    // не включается. Без этого лоченый экран останавливал screencopy (нет commit'ов
    // → 120-мс дедлайн copy → смерть стрима ~через 15 мин бездействия).
    let _idle_guard = match (state.compositor.clone(), state.idle_inhibit_mgr.clone()) {
        (Some(comp), Some(inh)) => {
            let surf = comp.create_surface(&qh, ());
            let inhibitor = inh.create_inhibitor(&surf, &qh, ());
            println!("capture: idle-inhibit активен (гидлых/лок не сработает до конца захвата)");
            Some((surf, inhibitor))
        }
        _ => {
            println!("capture: zwp_idle_inhibit_manager_v1 недоступен — лок сессии может оборвать стрим");
            None
        }
    };

    // Переключатели A/B:
    //   NX_PATH=dmabuf|shm — путь захвата. Дефолт shm: на этом AMD APU mmap
    //     dmabuf (dumb, UC/WC) читается CPU ~158 мс/кадр (3.48 МБ) — реальный
    //     стрим падал до 5.9 fps. dmabuf — только по явному NX_PATH=dmabuf
    //     (bench без чтения пикселей, NX_SINK_READ=0). NX_ENCODER=vaapi тоже
    //     форсирует dmabuf: энкодер импортирует fd, CPU не читает пиксели.
    //   NX_PIPE=0|1        — глубина пайплайна: 0 = старый последовательный (1 слот),
    //                        1 = два параллельных screencopy-запроса (2 слота)
    let force_dma = std::env::var("NX_ENCODER").as_deref() == Ok("vaapi");
    let want_dma = (force_dma || std::env::var("NX_PATH").as_deref() == Ok("dmabuf")) && dma_mgr.is_some();
    let pipe = std::env::var("NX_PIPE").as_deref() != Ok("0");
    let nslots: usize = if pipe { 2 } else { 1 };
    let mut use_dma = want_dma;
    println!("capture: {} target_fps={} path_req={} slots={}", out_name, fps,
        if want_dma { "dmabuf" } else { "shm" }, nslots);

    // Буферы по слотам (переиспользуются между кадрами: shm-пул 13.9 МБ
    // и dumb-буфер нельзя аллоцировать на каждый кадр).
    let mut bufs: [SlotBuf; 2] = [SlotBuf::None, SlotBuf::None];

    // Стартовый dmabuf-пробник: одна цепочка alloc → wl_buffer(created) →
    // copy → ready ДО входа в цикл. Любая ошибка — тихий fallback на shm
    // (с причиной), а не падение захвата.
    if want_dma {
        state.frames[0] = FrameState::default();
        let fr = mgr.capture_output(0, &out_obj, &qh, 0usize);
        let mut probe_ok = wait_for(
            &mut event_queue,
            &mut state,
            Duration::from_millis(100),
            |s| s.frames[0].buffer_ready || s.frames[0].failed,
            false,
        )? && !state.frames[0].failed
            && state.frames[0].dmabuf.is_some();
        if probe_ok {
            probe_ok = ensure_slot_buf(0, &mut use_dma, &mut bufs, &mut event_queue, &mut state,
                    &qh, &shm, dma_mgr.clone())
                .is_ok();
        }
        if probe_ok {
            let dma_wl = match &bufs[0] {
                SlotBuf::Dma { wl, .. } => Some(wl.clone()),
                _ => None,
            };
            if let Some(wl) = dma_wl {
                fr.copy(&wl);
                probe_ok = wait_for(
                    &mut event_queue,
                    &mut state,
                    Duration::from_millis(300),
                    |s| s.frames[0].copy_done || s.frames[0].failed,
                    false,
                )? && state.frames[0].copy_done;
            } else {
                probe_ok = false;
            }
        }
        if probe_ok {
            println!("capture: путь=dmabuf slots={} (пробник ok)", nslots);
        } else if force_dma {
            bail!("NX_ENCODER=vaapi требует dmabuf-путь, но пробник не прошёл");
        } else {
            println!("capture: dmabuf-пробник не прошёл — путь=shm slots={}", nslots);
            use_dma = false;
            drop_slot_buf(&mut bufs[0]);
            if let Some(b) = state.frames[0].dma_wl.take() {
                b.destroy();
            }
            state.frames[0].dma_failed = false;
        }
        fr.destroy();
    } else if want_dma {
        println!("capture: нет zwp_linux_dmabuf_v1 — путь=shm slots={}", nslots);
    } else {
        println!("capture: путь=shm slots={}", nslots);
    }

    let period = Duration::from_micros(1_000_000 / fps.max(1) as u64);
    let mut next = Instant::now();
    let mut i: u64 = 0;
    // Тайминг по стадиям (мкс). В пайплайне стадии слотов перекрываются, поэтому
    // «всего» и fps считаются по wall-clock окна, а не по сумме стадий.
    let (mut a_params, mut a_copy, mut a_sink, mut a_alloc, mut a_n) = (0u128, 0u128, 0u128, 0u128, 0u64);
    let (mut w_params, mut w_copy, mut w_sink) = (0u128, 0u128, 0u128);
    let mut c_hb = [0u32; 5];
    let mut win_start = Instant::now();
    // NX_SHOT=<path.png> — сохранить первый кадр цикла (проверка ориентации
    // dmabuf-пути против shm тем же визуальным диффом).
    let mut nx_shot = std::env::var("NX_SHOT").ok();
    while !stop.load(Ordering::Relaxed) {
        // Шаг 1: выпускаем ВСЕ capture-запросы пары сразу — оба кадра висят
        // pending у комозитора до следующего commit'а headless-выхода.
        let mut frames: [Option<ZwlrScreencopyFrameV1>; 2] = [None, None];
        let mut t_copied = [Instant::now(); 2];
        for s in 0..nslots {
            state.frames[s] = FrameState::default();
            frames[s] = Some(mgr.capture_output(0, &out_obj, &qh, s));
        }
        // Некопеечное, но убирает ожидание flush'а из первого params-wait.
        let _ = event_queue.flush();

        // Шаг 2: params каждого слота → буфер → copy. В пайплайне request'ы
        // обоих слотов уже в пути, поэтому второй params-wait обычно мгновенный.
        //
        // (A) Устойчивость: таймаут/failed слота больше НЕ убивает поток —
        // лоченый экран (hyprlock) или короткий GPU-stall (смена DPM-тактов)
        // рвали весь стрим по одному 100–120-мс дедлайну. Отбрасываем слот,
        // в следующей итерации запросим кадр заново; фатал — только 20 отказов
        // подряд (~несколько секунд мёртвого композитора).
        let mut fails_in_row = 0u32;
        let mut iter_ok = false;
        let mut t_a = [Instant::now(); 2];
        let mut t_b = [Instant::now(); 2];
        for s in 0..nslots {
            t_a[s] = Instant::now();
            // buffer_params: событийное ожидание (poll на wayland-fd) вместо
            // roundtrip-спина — просыпаемся в момент прихода события.
            let timed_out = !wait_for(
                &mut event_queue,
                &mut state,
                Duration::from_millis(100),
                |st| st.frames[s].buffer_ready || st.frames[s].failed,
                false,
            )? && !state.frames[s].buffer_ready;
            if timed_out || state.frames[s].failed {
                fails_in_row += 1;
                if fails_in_row >= 20 {
                    bail!("слот {}: {} кадр {} ({} подряд) — композитор мёртв", s,
                        if timed_out { "timeout buffer_params" } else { "failed frame" }, i, fails_in_row);
                }
                println!("capture: слот {} {}: перезапрос (подряд {})", s,
                    if timed_out { "timeout buffer_params" } else { "failed frame" }, fails_in_row);
                if let Some(f) = frames[s].take() { f.destroy(); }
                state.frames[s] = FrameState::default();
                continue;
            }
            t_b[s] = Instant::now();
            ensure_slot_buf(s, &mut use_dma, &mut bufs, &mut event_queue, &mut state,
                &qh, &shm, dma_mgr.clone())?;
            let t_setup = Instant::now();
            let wl = match &bufs[s] {
                SlotBuf::Shm { wl, .. } => wl.clone(),
                SlotBuf::Dma { wl, .. } => wl.clone(),
                SlotBuf::None => bail!("слот {} без буфера", s),
            };
            frames[s].as_ref().unwrap().copy(&wl);
            t_copied[s] = t_setup;
            a_params += t_b[s].duration_since(t_a[s]).as_micros();
            a_alloc += t_setup.duration_since(t_b[s]).as_micros();
        }

        // Шаг 3: ready каждого слота → sink. В пайплайне ожидание ready слота 0
        // идёт параллельно копированию слота 1 (оба запроса уже в комозиторе).
        for s in 0..nslots {
            if frames[s].is_none() { continue; } // слот отброшен на params-этапе
            let timed_out = !wait_for(
                &mut event_queue,
                &mut state,
                Duration::from_millis(120),
                |st| st.frames[s].copy_done || st.frames[s].failed,
                true,
            )? && !state.frames[s].copy_done;
            if timed_out || state.frames[s].failed {
                fails_in_row += 1;
                if fails_in_row >= 20 {
                    bail!("слот {}: {} кадр {} ({} подряд) — композитор мёртв", s,
                        if timed_out { "timeout copy" } else { "failed copy" }, i, fails_in_row);
                }
                println!("capture: слот {} {}: перезапрос (подряд {})", s,
                    if timed_out { "timeout copy" } else { "failed copy" }, fails_in_row);
                if let Some(f) = frames[s].take() { f.destroy(); }
                state.frames[s] = FrameState::default();
                continue;
            }
            iter_ok = true;
            let t_c = Instant::now();
            frames[s].take().unwrap().destroy(); // иначе объекты screencopy-frame текутся (60/с) — Hyprland падает
            // Слот-индекс в референсе: vaapi-энкодер держит по VA-поверхности на
            // слот (import_slot(slot)) — dmabuf-буферы слотов живут между кадрами.
            sink_slot(&bufs[s], s, &mut sink, &mut nx_shot)?;
            let t_d = Instant::now();
            i += 1;
            a_copy += t_c.duration_since(t_copied[s]).as_micros();
            a_sink += t_d.duration_since(t_c).as_micros();
            a_n += 1;
            w_params = w_params.max(t_copied[s].duration_since(t_a[s]).as_micros());
            w_copy = w_copy.max(t_c.duration_since(t_copied[s]).as_micros());
            w_sink = w_sink.max(t_d.duration_since(t_c).as_micros());
            let cm = t_c.duration_since(t_copied[s]).as_millis();
            c_hb[match cm {
                0..=9 => 0,
                10..=16 => 1,
                17..=24 => 2,
                25..=34 => 3,
                _ => 4,
            }] += 1;
        }
        // Прогресс в итерации = композитор жив — сбрасываем счётчик отказов.
        if iter_ok { fails_in_row = 0; }
        if a_n >= 180 {
            println!("capture: {} кадров", i);
            let n = a_n.max(1);
            let el = win_start.elapsed().as_micros().max(1);
            println!(
                "capture: тайминг/кадр мс — params(ждём буфер) {:.2} (макс {:.2}) | alloc {:.3} | copy/ready {:.2} (макс {:.2}) | sink {:.2} (макс {:.2}) | всего {:.2} => {:.1} fps",
                a_params as f64 / n as f64 / 1000.0,
                w_params as f64 / 1000.0,
                a_alloc as f64 / n as f64 / 1000.0,
                a_copy as f64 / n as f64 / 1000.0,
                w_copy as f64 / 1000.0,
                a_sink as f64 / n as f64 / 1000.0,
                w_sink as f64 / 1000.0,
                el as f64 / n as f64 / 1000.0,
                n as f64 * 1_000_000.0 / el as f64
            );
            println!(
                "capture: гистограмма copy/ready мс — <10:{} 10-16:{} 17-24:{} 25-34:{} >=35:{} (из {})",
                c_hb[0], c_hb[1], c_hb[2], c_hb[3], c_hb[4], n
            );
            a_params = 0;
            a_alloc = 0;
            a_copy = 0;
            a_sink = 0;
            a_n = 0;
            w_params = 0;
            w_copy = 0;
            w_sink = 0;
            c_hb = [0u32; 5];
            win_start = Instant::now();
        }

        // Пейсинг: в пайплайне пара даёт nslots кадров → период × nslots
        // (60 fps = 2 кадра за 33.3 мс).
        next += period * nslots as u32;
        let now = Instant::now();
        if next > now {
            std::thread::sleep(next - now);
        } else {
            next = now; // отстали от плана — не копим долг
        }
        // Ready слота мог прилететь во время sleep'а (оба кадра ждут один commit).
        // Диспатчим прочитанное сразу, а не в следующем wait_for — иначе событие
        // ждёт квантования poll и кадр копируется к самому дедлайну (таймаут при
        // задержке >3мс; в реальном стриме с ffmpeg — 62-й кадр, timeout copy).
        event_queue.dispatch_pending(&mut state)?;
    }
    for b in bufs.iter_mut() {
        drop_slot_buf(b);
    }
    Ok(())
}

fn params_format_as_wl(f: u32) -> Format {
    if f == Format::Argb8888 as u32 { Format::Argb8888 } else { Format::Xrgb8888 }
}

/// XRGB8888 (stride) → сырой BGRA для ffmpeg
fn bgra_from_xrgb(data: &[u8], w: u32, h: u32, stride: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        let row = &data[(y * stride) as usize..(y * stride + w * 4) as usize];
        for px in row.chunks_exact(4) {
            // XRGB LE: B G R X → BGRA
            out.extend_from_slice(&[px[0], px[1], px[2], 255]);
        }
    }
    out
}

fn raw_bgra_to_rgba(data: &[u8], w: u32, h: u32) -> Vec<u8> {
    let mut out = Vec::with_capacity((w * h * 4) as usize);
    for px in data.chunks_exact(4) {
        out.extend_from_slice(&[px[2], px[1], px[0], 255]);
    }
    out
}

fn save_png(rgba: &[u8], w: u32, h: u32, path: &str) -> Result<()> {
    use std::io::BufWriter;
    let file = std::fs::File::create(path)?;
    let mut enc = png::Encoder::new(BufWriter::new(file), w, h);
    enc.set_color(png::ColorType::Rgba);
    enc.set_depth(png::BitDepth::Eight);
    enc.write_header()?.write_image_data(rgba)?;
    Ok(())
}

fn create_shm_pool(
    qh: &QueueHandle<App>,
    shm: &WlShm,
    size: usize,
) -> Result<(WlShmPool, MmapMut, std::fs::File)> {
    let dir = std::env::var("XDG_RUNTIME_DIR").context("нет XDG_RUNTIME_DIR")?;
    let mut path = std::path::PathBuf::from(dir);
    path.push(format!("nx-shm-{}-{}", std::process::id(), fastrand()));
    let file = std::fs::OpenOptions::new().read(true).write(true).create(true).open(&path)?;
    file.set_len(size as u64)?;
    std::fs::remove_file(&path).ok();
    let mmap = unsafe { MmapMut::map_mut(&file)? };
    let pool = shm.create_pool(file.as_fd(), size as i32, qh, ());
    Ok((pool, mmap, file))
}

fn fastrand() -> u64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().subsec_nanos() as u64
}

// === буферы по слотам (shm-pool или dumb dma-buf) ===

/// Глобальный реестр живых dmabuf-слотов: fd + stride + dims. Обновляется
/// capture-потоком при (пере)создании слота; encoder-поток pipeline-ветки
/// vaapi читает fd для import_slot (fd жив, пока жив слот-буфер).
pub static SLOT_DMABUF: [std::sync::Mutex<Option<(RawFd, u32, u32, u32)>>; 2] = [
    std::sync::Mutex::new(None),
    std::sync::Mutex::new(None),
];

/// fd dmabuf-слота (или None). Используется encoder-потоком vaapi.
pub fn slot_dmabuf_fd(slot: usize) -> Option<RawFd> {
    SLOT_DMABUF[slot.min(1)].lock().unwrap().as_ref().map(|t| t.0)
}

/// wl_buffer + пиксельные данные для слота. shm и dmabuf различаются только
/// аллокацией; для sink оба отдают mmap (строка фиксированной длины).
enum SlotBuf {
    None,
    Shm {
        pool: WlShmPool,
        mmap: MmapMut,
        _file: std::fs::File,
        wl: WlBuffer,
        w: u32,
        h: u32,
        stride: u32,
        format: u32,
    },
    Dma {
        dumb: DumbBuffer,
        wl: WlBuffer,
        w: u32,
        h: u32,
        stride: u32,
    },
}

fn drop_slot_buf(b: &mut SlotBuf) {
    match std::mem::replace(b, SlotBuf::None) {
        SlotBuf::Shm { wl, pool, .. } => {
            wl.destroy();
            pool.destroy();
        }
        SlotBuf::Dma { wl, .. } => {
            // слот-индекс тут неизвестен — реестр чистится в ensure/create при пересоздании;
            // encoder сам проверяет slot_ready/переподключение по ошибке encode.
            wl.destroy()
        }
        SlotBuf::None => {}
    }
}

/// Приводит bufs[slot] к текущему состоянию слота: dmabuf (если разрешён и
/// поддерживается) либо shm. Пересоздаёт буфер только при смене геометрии.
#[allow(clippy::too_many_arguments)]
fn ensure_slot_buf(
    slot: usize,
    use_dma: &mut bool,
    bufs: &mut [SlotBuf; 2],
    event_queue: &mut EventQueue<App>,
    state: &mut App,
    qh: &QueueHandle<App>,
    shm: &WlShm,
    dma_mgr: Option<ZwpLinuxDmabufV1>,
) -> Result<()> {
    // Решаем путь по свежим событиям этого capture-запроса.
    let dma = *use_dma && {
        match state.frames[slot].dmabuf {
            Some(d) if d.format == DRM_FORMAT_ARGB8888 || d.format == DRM_FORMAT_XRGB8888 => true,
            Some(d) => {
                eprintln!("capture: dmabuf формат 0x{:08x} не поддерживаем — fallback shm", d.format);
                *use_dma = false;
                false
            }
            None => {
                eprintln!("capture: нет события linux_dmabuf — fallback shm");
                *use_dma = false;
                false
            }
        }
    };

    // Уже готово?
    let ready = match (&bufs[slot], dma) {
        (SlotBuf::Dma { w, h, .. }, true) => {
            let d = state.frames[slot].dmabuf.unwrap();
            *w == d.width && *h == d.height
        }
        (SlotBuf::Shm { w, h, stride, format, .. }, false) => match &state.frames[slot].params {
            Some(p) => *w == p.width && *h == p.height && *stride == p.stride && *format == p.format,
            None => false,
        },
        _ => false,
    };
    if ready {
        return Ok(());
    }

    drop_slot_buf(&mut bufs[slot]);

    if dma {
        let d = state.frames[slot].dmabuf.unwrap();
        match dma_create_slot(slot, d, event_queue, state, qh, &dma_mgr) {
            Ok(sb) => {
                bufs[slot] = sb;
                return Ok(());
            }
            Err(e) => {
                eprintln!("capture: dmabuf-буфер не создан ({e:#}) — fallback shm");
                *use_dma = false;
                state.frames[slot].dma_failed = false;
                if let Some(b) = state.frames[slot].dma_wl.take() {
                    b.destroy();
                }
            }
        }
    }

    // shm: тот же кэшированный путь, что и раньше
    let p = state.frames[slot].params.context("нет buffer_params")?;
    let size = (p.stride * p.height) as usize;
    let (pool, mmap, file) = create_shm_pool(qh, shm, size)?;
    let wl: WlBuffer = pool.create_buffer(
        0,
        p.width as i32,
        p.height as i32,
        p.stride as i32,
        params_format_as_wl(p.format),
        qh,
        (),
    );
    bufs[slot] = SlotBuf::Shm {
        pool,
        mmap,
        _file: file,
        wl,
        w: p.width,
        h: p.height,
        stride: p.stride,
        format: p.format,
    };
    Ok(())
}

/// dumb-буфер → dmabuf → wl_buffer через zwp_linux_buffer_params_v1.
/// create() асинхронен: wl_buffer приходит событием created (или failed).
/// modifier: DRM_FORMAT_MOD_LINEAR = 0 (dumb-буфер всегда линейный).
#[allow(clippy::too_many_arguments)]
fn dma_create_slot(
    slot: usize,
    d: DmabufParams,
    event_queue: &mut EventQueue<App>,
    state: &mut App,
    qh: &QueueHandle<App>,
    dma_mgr: &Option<ZwpLinuxDmabufV1>,
) -> Result<SlotBuf> {
    let mgr = dma_mgr.as_ref().context("нет zwp_linux_dmabuf_v1")?;
    let dumb = DumbBuffer::alloc(d.width, d.height).context("alloc dumb")?;
    let stride = dumb.stride;
    let params = mgr.create_params(qh, slot);
    params.add(dumb.dmabuf_fd(), 0, 0, stride, 0, 0);
    params.create(d.width as i32, d.height as i32, d.format, DmabufFlags::empty());
    let ok = wait_for(
        event_queue,
        state,
        Duration::from_millis(300),
        |s| s.frames[slot].dma_wl.is_some() || s.frames[slot].dma_failed,
        false,
    )?;
    let wl = state.frames[slot].dma_wl.take();
    params.destroy();
    if !ok || state.frames[slot].dma_failed || wl.is_none() {
        state.frames[slot].dma_failed = false;
        anyhow::bail!("created/failed: wl_buffer не создан");
    }
    {
        // реестр для encoder-потока (fd живёт, пока жив dumb)
        *SLOT_DMABUF[slot].lock().unwrap() = Some((dumb.dmabuf_fd().as_raw_fd(), stride, d.width, d.height));
    }
    Ok(SlotBuf::Dma {
        dumb,
        wl: wl.unwrap(),
        w: d.width,
        h: d.height,
        stride,
    })
}

/// Референс кадра для sink: пиксели (shm-путь, либо чтение dmabuf) или
/// dmabuf-слот (zero-copy путь — энкодер импортирует fd, CPU не читает).
pub enum FrameRef<'a> {
    Bgra(&'a [u8], u32, u32),
    Dma { slot: usize, dumb: &'a DumbBuffer, w: u32, h: u32, stride: u32 },
}

/// Отдаёт кадр слота в sink как BGRA (XRGB/ARGB LE байтово = BGRA, alpha
/// ffmpeg-вским bgra игнорируется). stride == w*4 → отдаём mmap напрямую.
fn sink_slot<F>(buf: &SlotBuf, slot: usize, sink: &mut F, nx_shot: &mut Option<String>) -> Result<()>
where
    F: FnMut(FrameRef) -> Result<()>,
{
    if let Some(p) = nx_shot.take() {
        let (data, w, h, stride) = match buf {
            SlotBuf::Shm { mmap, w, h, stride, .. } => (&mmap[..], *w, *h, *stride),
            SlotBuf::Dma { dumb, w, h, stride, .. } => (&dumb.map[..], *w, *h, *stride),
            SlotBuf::None => bail!("sink: слот без буфера"),
        };
        let raw = bgra_from_xrgb(data, w, h, stride);
        save_png(&raw_bgra_to_rgba(&raw, w, h), w, h, &p)?;
        println!("capture: NX_SHOT {p}");
    }
    match buf {
        SlotBuf::Shm { mmap, w, h, stride, .. } => {
            if *stride == *w * 4 {
                // LE XRGB/ARGB = B G R X/A, ffmpeg-вский bgra игнорирует альфу → пишем как есть
                sink(FrameRef::Bgra(&mmap[..(*w * *h * 4) as usize], *w, *h))
            } else {
                let mut buf2 = Vec::with_capacity((*w * *h * 4) as usize);
                for y in 0..*h {
                    buf2.extend_from_slice(&mmap[(y * stride) as usize..(y * stride + w * 4) as usize]);
                }
                sink(FrameRef::Bgra(&buf2, *w, *h))
            }
        }
        SlotBuf::Dma { dumb, w, h, stride, .. } => {
            if *stride == *w * 4 {
                sink(FrameRef::Dma { slot, dumb, w: *w, h: *h, stride: *stride })
            } else {
                // длинный страйд → CPU-выравнивание в сплошной BGRA (редко)
                let mut buf2 = Vec::with_capacity((*w * *h * 4) as usize);
                for y in 0..*h {
                    buf2.extend_from_slice(&dumb.map[(y * stride) as usize..(y * stride + w * 4) as usize]);
                }
                sink(FrameRef::Bgra(&buf2, *w, *h))
            }
        }
        SlotBuf::None => bail!("sink: слот без буфера"),
    }
}

// === диспетчеризация ===

impl Dispatch<WlRegistry, ()> for App {
    fn event(state: &mut Self, registry: &WlRegistry, event: RegistryEvent, _: &(), _: &Connection, qh: &QueueHandle<Self>) {
        if let RegistryEvent::Global { name, interface, version } = event {
            match interface.as_str() {
                "wl_shm" => { state.shm = Some(registry.bind::<WlShm, _, _>(name, 1.min(version), qh, ())); }
                "zwlr_screencopy_manager_v1" => {
                    state.screencopy_mgr = Some(registry.bind::<ZwlrScreencopyManagerV1, _, _>(name, 3.min(version), qh, ()));
                }
                "zwp_linux_dmabuf_v1" => {
                    // v3: модификаторы есть в add(), но валидации
                    // «format+modifier обязан быть advertised» ещё нет (это с v4,
                    // там же fatal protocol error) — провал импорта приходит
                    // неблокирующим событием failed.
                    state.dma_mgr = Some(registry.bind::<ZwpLinuxDmabufV1, _, _>(name, 3.min(version), qh, ()));
                }
                "wl_output" => {
                    let out = registry.bind::<WlOutput, _, _>(name, 4.min(version), qh, name);
                    state.outputs.push((name, out, format!("output#{}", name)));
                }
                "wl_compositor" => {
                    state.compositor = Some(registry.bind::<WlCompositor, _, _>(name, 4.min(version), qh, ()));
                }
                "zwp_idle_inhibit_manager_v1" => {
                    state.idle_inhibit_mgr = Some(registry.bind::<ZwpIdleInhibitManagerV1, _, _>(name, 1.min(version), qh, ()));
                }
                _ => {}
            }
        }
    }
}

impl Dispatch<WlOutput, u32> for App {
    fn event(state: &mut Self, _: &WlOutput, event: OutputEvent, &id: &u32, _: &Connection, _: &QueueHandle<Self>) {
        match event {
            OutputEvent::Name { name } => {
                if let Some(e) = state.outputs.iter_mut().find(|(n, _, _)| *n == id) {
                    if !name.is_empty() { e.2 = name; }
                }
            }
            OutputEvent::Description { description } => {
                if let Some(e) = state.outputs.iter_mut().find(|(n, _, _)| *n == id) {
                    if !description.is_empty() && e.2.starts_with("output#") { e.2 = description; }
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<WlShm, ()> for App {
    fn event(_: &mut Self, _: &WlShm, _: wayland_client::protocol::wl_shm::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<WlShmPool, ()> for App {
    fn event(_: &mut Self, _: &WlShmPool, _: wayland_client::protocol::wl_shm_pool::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<WlBuffer, ()> for App {
    fn event(_: &mut Self, _: &WlBuffer, _: wayland_client::protocol::wl_buffer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<WlCompositor, ()> for App {
    fn event(_: &mut Self, _: &WlCompositor, _: wayland_client::protocol::wl_compositor::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<WlSurface, ()> for App {
    fn event(_: &mut Self, _: &WlSurface, _: wayland_client::protocol::wl_surface::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<ZwpIdleInhibitManagerV1, ()> for App {
    fn event(_: &mut Self, _: &ZwpIdleInhibitManagerV1, _: wayland_protocols::wp::idle_inhibit::zv1::client::zwp_idle_inhibit_manager_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<ZwpIdleInhibitorV1, ()> for App {
    fn event(_: &mut Self, _: &ZwpIdleInhibitorV1, _: wayland_protocols::wp::idle_inhibit::zv1::client::zwp_idle_inhibitor_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<ZwlrScreencopyManagerV1, ()> for App {
    fn event(_: &mut Self, _: &ZwlrScreencopyManagerV1, _: wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_manager_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<ZwlrScreencopyFrameV1, usize> for App {
    fn event(state: &mut Self, _: &ZwlrScreencopyFrameV1, event: FrameEvent, slot: &usize, _: &Connection, _: &QueueHandle<Self>) {
        let f = &mut state.frames[*slot];
        match event {
            FrameEvent::Buffer { format, width, height, stride } => {
                f.params = Some(BufferParams { format: format.into(), width, height, stride });
            }
            FrameEvent::LinuxDmabuf { format, width, height } => {
                f.dmabuf = Some(DmabufParams { format, width, height });
            }
            FrameEvent::BufferDone => { f.buffer_ready = true; }
            FrameEvent::Ready { .. } => { f.copy_done = true; }
            FrameEvent::Failed => { f.failed = true; }
            _ => {}
        }
    }
}

impl Dispatch<ZwpLinuxDmabufV1, ()> for App {
    fn event(_: &mut Self, _: &ZwpLinuxDmabufV1, _: wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_dmabuf_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<ZwpLinuxBufferParamsV1, usize> for App {
    event_created_child!(App, ZwpLinuxBufferParamsV1, [
        wayland_protocols::wp::linux_dmabuf::zv1::client::zwp_linux_buffer_params_v1::EVT_CREATED_OPCODE => (WlBuffer, ())
    ]);
    fn event(state: &mut Self, _: &ZwpLinuxBufferParamsV1, event: ParamsEvent, slot: &usize, _: &Connection, _: &QueueHandle<Self>) {
        match event {
            ParamsEvent::Created { buffer } => { state.frames[*slot].dma_wl = Some(buffer); }
            ParamsEvent::Failed => { state.frames[*slot].dma_failed = true; }
            _ => {}
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn x11_parsers() {
        assert_eq!(
            parse_xrandr_current("Screen 0: minimum 320 x 200, current 1920 x 1080, maximum 16384 x 16384"),
            Some((1920, 1080))
        );
        assert_eq!(
            parse_x11_geometry("1920x1080+0+0"),
            Some((1920, 1080))
        );
        assert_eq!(parse_x11_geometry("832x624"), Some((832, 624)));
        assert_eq!(parse_x11_geometry("abc"), None);
        assert_eq!(
            crate::stream::input::parse_x11_rect("1920x1080+0+0"),
            Some((0, 0, 1920, 1080))
        );
        assert_eq!(
            crate::stream::input::parse_x11_rect("1920x1080+1920+0"),
            Some((1920, 0, 1920, 1080))
        );
        assert_eq!(
            crate::stream::input::parse_x11_rect("1920x1080+0+1080"),
            Some((0, 1080, 1920, 1080))
        );
        assert_eq!(
            crate::stream::input::parse_x11_rect("1920x1080-100+0"),
            Some((-100, 0, 1920, 1080))
        );
        assert_eq!(
            crate::stream::input::parse_x11_rect("1920/344x1080/193+0+0"),
            Some((0, 0, 1920, 1080))
        );
    }

    #[test]
    fn forced_backend_overrides_everything() {
        // NX_CAPTURE приоритетнее даже при полном отсутствии сессии.
        std::env::set_var("NX_CAPTURE", "x11");
        assert_eq!(backend_for("auto"), Backend::X11);
        std::env::set_var("NX_CAPTURE", "portal");
        assert_eq!(backend_for("whatever"), Backend::Portal);
        std::env::set_var("NX_CAPTURE", "wlr");
        assert_eq!(backend_for("HEADLESS-1"), Backend::Wlr);
        std::env::remove_var("NX_CAPTURE");
    }
}
