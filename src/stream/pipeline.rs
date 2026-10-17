//! Видео-конвейер: wayland-capture → ffmpeg(h264_vaapi) → AU-парсер → RTP/UDP.
//! RTP-пакет повторяет Sunshine stream.cpp: [12 RTP][4 reserved][16 NV_VIDEO_PACKET]
//! + чанк кадра; последний чанк zero-pad через shards, lastPayloadLen компенсирует.
//! percentage=0 → parity-шардов нет, FEC-поля всё равно выставляются.

use super::*;
use crate::probes;
use anyhow::{Context, Result};
use std::io::{Read, Write};
use std::net::SocketAddr;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Instant;

const FLAG_PIC_DATA: u8 = 0x1;
const FLAG_EOF: u8 = 0x2;
const FLAG_SOF: u8 = 0x4;
const VSZ: usize = 32; // sizeof(video_packet_raw_t)
const MAX_RTP_HEADER: usize = 16;
const NV_OFF: usize = 16; // смещение NV_VIDEO_PACKET внутри слота
const GOP: u32 = 30;

/// Запуск конвейера на ANNOUNCE: ffmpeg + capture-поток + RTP-поток.
pub fn start(st: &Arc<StreamState>, cfg: AnnounceConfig) -> Result<()> {
    let launch = st
        .launch
        .lock()
        .unwrap()
        .clone()
        .context("ANNOUNCE пришёл без launch-сессии (/launch не вызывался)")?;
    // предыдущий конвейер (без обнуления current_game — сессия продолжается)
    if let Some(h) = st.pipeline.lock().unwrap().take() {
        h.stop();
    }

    // Размеры захвата нужны ffmpeg заранее — быстрый пробный захват.
    st.touch(); // сессия началась — watchdog даёт клиенту15с на подключение
    let (cw, ch) = probes::probe_output_size(&st.output).context("probe_output_size")?;
    let (tw, th) = if cfg.client_w > 0 && cfg.client_h > 0 {
        (cfg.client_w, cfg.client_h)
    } else if launch.width > 0 && launch.height > 0 {
        (launch.width, launch.height)
    } else {
        (cw, ch)
    };
    let fps = if cfg.max_fps > 0 { cfg.max_fps } else { launch.fps.max(1) };
    let bitrate_k = cfg.bitrate_kbps.max(1_000);

    // Конвейер целиком на GPU: hwupload поднимает BGRA-кадр в VAAPI-поверхность,
    // а scale_vaapi делает И масштаб, И конверсию BGRA->NV12. Раньше конверсия
    // NV12 шла на CPU (swscale) и была узким местом: ~13 fps против ~35 fps.
    let vf = if (tw, th) != (cw, ch) {
        format!("hwupload,scale_vaapi=format=nv12:w={}:h={}", tw, th)
    } else {
        "hwupload,scale_vaapi=format=nv12".to_string()
    };
    let mut enc_cmd = Command::new("ffmpeg");
    enc_cmd
        .args([
            "-y",
            "-loglevel", "error",
            "-f", "rawvideo",
            "-pixel_format", "bgra",
            "-video_size",
            &format!("{}x{}", cw, ch),
            "-framerate",
            &fps.to_string(),
            "-i", "pipe:0",
            "-vaapi_device", "/dev/dri/renderD128",
        ])
        .args([
            "-vf",
            &vf,
            "-c:v", "h264_vaapi",
            // Низкая латентность: CBR с маленьким VBV, без очереди кадров.
            "-rc_mode", "CBR",
            "-b:v",
            &format!("{}k", bitrate_k),
            "-maxrate",
            &format!("{}k", bitrate_k),
            "-bufsize",
            &format!("{}k", (bitrate_k / 2).max(500)),
            "-async_depth", "1", // дефолт 2: лишний кадр в очереди энкодера
            "-g",
            &GOP.to_string(),
            "-bf", "0", // без B-frames — низкая латентность
            "-f", "h264",
            "pipe:1",
        ]);
    // Живой тюнинг энкодера без пересборки: NX_ENC_EXTRA="-quality 4 -profile main"
    if let Ok(extra) = std::env::var("NX_ENC_EXTRA") {
        enc_cmd.args(extra.split_whitespace());
    }
    let mut enc = enc_cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::from(
            std::fs::File::create("/tmp/nx-ffmpeg.log").context("создание /tmp/nx-ffmpeg.log")?,
        ))
        .spawn()
        .context("запуск ffmpeg (h264_vaapi)")?;
    let mut stdin = enc.stdin.take().context("ffmpeg stdin")?;
    let stdout = enc.stdout.take().context("ffmpeg stdout")?;

    let stop = Arc::new(AtomicBool::new(false));

    // capture → кадровый слот → writer-поток → ffmpeg stdin.
    //
    // Раньше capture-поток сам писал в stdin: `write_all` на 3.48 МБ (BGRA 1088x800)
    // блокировался, пока ffmpeg не отчитается чтением, и захват простаивал 7.2 мс/кадр —
    // это четверть всего периода (28.5 мс → 35 fps). Замер: ffmpeg в изоляции выдаёт
    // 83 fps, а внутри конвейера отчитывался 34.45 — то есть энкодер голодал, а не тормозил.
    //
    // Теперь записи в stdin нет на критическом пути захвата. Слот глубины 1: если энкодер
    // отстаёт, старый кадр вытесняется новым — латентность не копится, всегда уходит самый
    // свежий кадр. Пул из двух буферов, чтобы не аллоцировать 3.48 МБ на кадр (ранее такие
    // аллокации роняли захват до единиц fps).
    let free: Arc<Mutex<Vec<Vec<u8>>>> = Arc::new(Mutex::new(vec![Vec::new(), Vec::new()]));
    let ready: Arc<Mutex<Option<Vec<u8>>>> = Arc::new(Mutex::new(None));
    let dropped = Arc::new(std::sync::atomic::AtomicU64::new(0));

    // capture → ready-слот
    let output = st.output.clone();
    let stop_c = stop.clone();
    let free_c = free.clone();
    let ready_c = ready.clone();
    let dropped_c = dropped.clone();
    let capture_done = Arc::new(AtomicBool::new(false));
    let done_c = capture_done.clone();
    std::thread::spawn(move || {
        let r = probes::capture_loop(&output, fps, stop_c, move |data, _w, _h| {
            let mut buf = free_c.lock().unwrap().pop().unwrap_or_default();
            buf.clear();
            buf.extend_from_slice(data);
            // вытесняем старейший неотправленный кадр, если writer не успел
            let stale = ready_c.lock().unwrap().replace(buf);
            if let Some(stale) = stale {
                free_c.lock().unwrap().push(stale);
                dropped_c.fetch_add(1, Ordering::Relaxed);
            }
            Ok(())
        });
        if let Err(e) = r {
            println!("capture: {}", e);
        }
        println!("capture: поток завершён");
        done_c.store(true, Ordering::Relaxed);
    });

    // ready-слот → ffmpeg stdin
    let stop_w = stop.clone();
    let free_w = free.clone();
    let ready_w = ready.clone();
    let dropped_w = dropped.clone();
    std::thread::spawn(move || {
        let mut sent = 0u64;
        let mut last_log = Instant::now();
        let mut sent_at_log = 0u64;
        loop {
            let frame = ready_w.lock().unwrap().take();
            let Some(mut buf) = frame else {
                if stop_w.load(Ordering::Relaxed) && capture_done.load(Ordering::Relaxed) {
                    break; // захват кончился и слот пуст → отдаём stdin (EOF) и выходим
                }
                std::thread::sleep(std::time::Duration::from_millis(1));
                continue;
            };
            // write_all блокируется, пока ffmpeg прочитает кадр — это норма и не мешает захвату
            let w = stdin.write_all(&buf);
            buf.clear();
            free_w.lock().unwrap().push(buf);
            if let Err(e) = w {
                println!("writer: {}", e);
                break;
            }
            sent += 1;
            if last_log.elapsed().as_millis() >= 5000 {
                let secs = last_log.elapsed().as_secs_f64();
                println!(
                    "writer: {:.1} fps в ffmpeg (всего {}), вытеснено старых кадров {}",
                    (sent - sent_at_log) as f64 / secs,
                    sent,
                    dropped_w.load(Ordering::Relaxed)
                );
                sent_at_log = sent;
                last_log = Instant::now();
            }
        }
        println!("writer: поток завершён");
        // stdin падает здесь → ffmpeg получает EOF и завершается → stdout EOF → rtp-поток выйдет
    });

    // ffmpeg stdout → AU → RTP
    let st_s = st.clone();
    let stop_s = stop.clone();
    let launch_s = launch.clone();
    let cfg_s = cfg.clone();
    std::thread::spawn(move || {
        if let Err(e) = rtp_sender_loop(&st_s, stdout, &launch_s, &cfg_s, fps, stop_s) {
            println!("rtp: {}", e);
        }
        println!("rtp: поток завершён");
    });

    *st.pipeline.lock().unwrap() = Some(PipelineHandle {
        stop,
        child: Mutex::new(Some(enc)),
    });
    println!(
        "stream: pipeline старт: захват {}x{}@{} → поток {}x{} @{}k packetsize={}",
        cw, ch, fps, tw, th, bitrate_k, cfg.packet_size
    );
    Ok(())
}

/// Peer-learning: клиент шлёт SS_PING (payload[16] + seq BE32 = 20B, либо legacy "PING")
/// со своего rtpSocket на наш video-порт; источник = адрес, куда слать RTP.
pub fn start_peer_learning(st: Arc<StreamState>) {
    std::thread::spawn(move || {
        let sock = st.video_sock.clone();
        let mut buf = [0u8; 2048];
        loop {
            match sock.recv_from(&mut buf) {
                Ok((n, src)) => {
                    st.touch(); // SS_PING — ежесекундный пульс клиента: сбрасываем watchdog
                    let ping: Option<Vec<u8>> = st
                        .launch
                        .lock()
                        .unwrap()
                        .as_ref()
                        .map(|l| l.av_ping_payload.clone().into_bytes());
                    let is_ping = (n >= 16
                        && ping
                            .as_ref()
                            .map(|p| &buf[..16] == p.as_slice())
                            .unwrap_or(false))
                        || (n == 4 && &buf[..4] == b"PING");
                    if is_ping {
                        let mut g = st.video_peer.lock().unwrap();
                        if *g != Some(src) {
                            println!("video: peer {} ({}B ping)", src, n);
                            *g = Some(src);
                        }
                    }
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(e) => {
                    println!("video: recv: {}", e);
                    break;
                }
            }
        }
    });
}

/// Audio MVP: бинд :49002 уже сделан в StreamState::new, дреним входящие (пинги клиента).
pub fn start_audio_drain(st: Arc<StreamState>) {
    std::thread::spawn(move || {
        let sock = st.audio_sock.clone();
        let mut buf = [0u8; 4096];
        let mut total = 0u64;
        loop {
            match sock.recv_from(&mut buf) {
                Ok((n, src)) => {
                    st.touch(); // пинги клиента на аудио-порт — тоже пульс
                    total += 1;
                    if total <= 3 || total % 1000 == 0 {
                        println!("audio: {} пакетов (последний {}B от {})", total, n, src);
                    }
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(e) => {
                    println!("audio: recv: {}", e);
                    break;
                }
            }
        }
    });
}

// === ffmpeg stdout → кадры → RTP ===

fn rtp_sender_loop(
    st: &Arc<StreamState>,
    mut stdout: impl Read,
    _launch: &Arc<LaunchSession>,
    cfg: &AnnounceConfig,
    _fps: u32,
    stop: Arc<AtomicBool>,
) -> Result<()> {
    let sock = st.video_sock.clone();
    let mut parser = AuParser::default();
    let mut chunk = vec![0u8; 64 * 1024];
    let epoch = Instant::now();
    let mut frame_index: u32 = 0;
    let mut lowseq: u32 = 0;
    let mut frames_sent = 0u64;
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        // read блокируется; stop() убивает ffmpeg → EOF или ошибка → выход
        let n = stdout.read(&mut chunk)?;
        if n == 0 {
            println!("rtp: ffmpeg stdout EOF");
            break;
        }
        for (au, ftype) in parser.feed(&chunk[..n]) {
            // peer ещё не известен — кадр дропаем целиком, счётчики не двигаем:
            // первый отправленный кадр будет frame_index=0, lowseq=0 (чистый старт).
            let Some(peer) = *st.video_peer.lock().unwrap() else {
                continue;
            };
            let ts90 = (epoch.elapsed().as_micros() as u64 * 90 / 1_000_000) as u32;
            send_frame(&sock, peer, cfg, &au, ftype, frame_index, ts90, &mut lowseq)?;
            frame_index = frame_index.wrapping_add(1);
            frames_sent += 1;
            if frames_sent % 300 == 0 {
                println!(
                    "rtp: {} кадров ({} B, type={}) → {}",
                    frames_sent, au.len(), ftype, peer
                );
            }
        }
    }
    Ok(())
}

// === AU-парсер (Annex-B → access units) ===

/// Начало нового AU: VCL-NAL (type 1..=5) с first_mb_in_slice==0.
/// first_mb==0 ↔ первый бит rbsp равен 1 (ue(0) = "1") → nal[sc+1] & 0x80.
/// Кэшируем SPS/PPS: если IDR-AU остался без них (потерялись в начале потока) —
/// вставляем перед AU, иначе декодер не стартанёт.
#[derive(Default)]
struct AuParser {
    buf: Vec<u8>,
    cur: Vec<u8>,
    cur_has_vcl: bool,
    cur_has_sps: bool,
    cur_is_idr: bool,
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
}

impl AuParser {
    fn feed(&mut self, data: &[u8]) -> Vec<(Vec<u8>, u8)> {
        self.buf.extend_from_slice(data);
        let mut out = Vec::new();
        let mut scan = 0usize;
        while let Some((p1, l1)) = find_sc(&self.buf, scan) {
            let Some((p2, _)) = find_sc(&self.buf, p1 + l1) else {
                break; // последний NAL ещё не завершён — ждём данных
            };
            let nal = self.buf[p1..p2].to_vec();
            scan = p2;
            self.handle_nal(nal, &mut out);
        }
        self.buf.drain(..scan);
        out
    }

    fn handle_nal(&mut self, nal: Vec<u8>, out: &mut Vec<(Vec<u8>, u8)>) {
        let sc = if nal.len() >= 4 && nal[1] == 0 && nal[2] == 0 && nal[3] == 1 { 4 } else { 3 };
        if nal.len() <= sc {
            return;
        }
        let ntype = nal[sc] & 0x1f;
        if ntype == 7 {
            self.sps = Some(nal.clone());
        }
        if ntype == 8 {
            self.pps = Some(nal.clone());
        }
        let is_vcl = (1..=5).contains(&ntype);
        let first_mb0 = is_vcl && nal.len() > sc + 1 && (nal[sc + 1] & 0x80) != 0;
        if is_vcl && first_mb0 && self.cur_has_vcl {
            let mut au = std::mem::take(&mut self.cur);
            let ft = if self.cur_is_idr { 2u8 } else { 1u8 };
            if self.cur_is_idr && !self.cur_has_sps {
                if let (Some(s), Some(p)) = (&self.sps, &self.pps) {
                    let mut with = Vec::with_capacity(s.len() + p.len() + au.len());
                    with.extend_from_slice(s);
                    with.extend_from_slice(p);
                    with.extend_from_slice(&au);
                    au = with;
                }
            }
            out.push((au, ft));
            self.cur_has_vcl = false;
            self.cur_has_sps = false;
            self.cur_is_idr = false;
        }
        if is_vcl {
            self.cur_has_vcl = true;
            if ntype == 5 {
                self.cur_is_idr = true;
            }
        }
        if ntype == 7 {
            self.cur_has_sps = true;
        }
        self.cur.extend_from_slice(&nal); // start code остаётся внутри AU
    }
}

fn find_sc(b: &[u8], from: usize) -> Option<(usize, usize)> {
    let mut i = from;
    while i + 3 <= b.len() {
        if b[i] == 0 && b[i + 1] == 0 {
            if b[i + 2] == 1 {
                return Some((i, 3));
            }
            if i + 4 <= b.len() && b[i + 2] == 0 && b[i + 3] == 1 {
                return Some((i, 4));
            }
        }
        i += 1;
    }
    None
}

// === RTP-пакетизация (по Sunshine stream.cpp) ===

#[allow(clippy::too_many_arguments)]
fn send_frame(
    sock: &std::net::UdpSocket,
    peer: SocketAddr,
    cfg: &AnnounceConfig,
    au: &[u8],
    ftype: u8,
    frame_index: u32,
    ts90: u32,
    lowseq: &mut u32,
) -> Result<()> {
    let packetsize = cfg.packet_size;
    let blocksize = packetsize + MAX_RTP_HEADER; // полный UDP-пакет = packetsize+16
    let pbs = blocksize - VSZ; // данных на пакет = packetsize-16 (клиентский assert)

    // 8B short frame header (ВНУТРИ данных, перед AU)
    let mut last = ((au.len() + 8) % pbs) as u16;
    if last == 0 {
        last = pbs as u16;
    }
    let mut data = Vec::with_capacity(8 + au.len());
    data.push(0x01); // headerType
    data.extend_from_slice(&0u16.to_le_bytes()); // frame_processing_latency
    data.push(ftype); // 1=P, 2=IDR
    data.extend_from_slice(&last.to_le_bytes());
    data.extend_from_slice(&[0, 0]);
    data.extend_from_slice(au);

    // Разметка: [32B нулевой слот][чанк pbs] × N
    let nelem = (data.len() + pbs - 1) / pbs;
    let mut framed = vec![0u8; nelem * blocksize];
    for i in 0..nelem {
        let s = i * pbs;
        let e = (s + pbs).min(data.len());
        framed[i * blocksize + VSZ..i * blocksize + VSZ + (e - s)].copy_from_slice(&data[s..e]);
    }

    // FEC-блоки (percentage=0 → parity=0, но поля FEC заполняются как у Sunshine)
    let max_per_block = 255 * blocksize;
    let mut nblocks = (framed.len() + max_per_block - 1) / max_per_block;
    if nblocks == 0 {
        nblocks = 1;
    }
    if nblocks > 4 {
        println!("rtp: кадр {} B — блоков >4, режу до 4", au.len());
        nblocks = 4;
    }
    let unaligned = framed.len() / nblocks;
    let aligned = ((unaligned + blocksize - 1) / blocksize) * blocksize;
    for b in 0..nblocks {
        let start = b * aligned;
        let end = if b == nblocks - 1 {
            framed.len()
        } else {
            ((b + 1) * aligned).min(framed.len())
        };
        let mut block = framed[start..end].to_vec();
        let pp = (block.len() + blocksize - 1) / blocksize; // пакетов в блоке
        block.resize(pp * blocksize, 0); // выравнивание нулями (shards-путь Sunshine)
        let ds = pp as u32;
        for x in 0..pp {
            let o = x * blocksize;
            // NV_VIDEO_PACKET @ o+16 (LE)
            write_u32_le(&mut block[o + NV_OFF..], (*lowseq + x as u32) << 8);
            write_u32_le(&mut block[o + NV_OFF + 4..], frame_index);
            let mut flags = FLAG_PIC_DATA;
            if x == 0 {
                flags |= FLAG_SOF;
            }
            if x == pp - 1 {
                flags |= FLAG_EOF;
            }
            block[o + NV_OFF + 8] = flags;
            block[o + NV_OFF + 9] = 0; // extraFlags
            block[o + NV_OFF + 10] = 0x10; // multiFecFlags
            block[o + NV_OFF + 11] = ((b as u8) << 4) | (((nblocks - 1) as u8) << 6);
            write_u32_le(&mut block[o + NV_OFF + 12..], ((x as u32) << 12) | (ds << 22));
            // RTP @ o: header=0x80|FLAG_EXTENSION, packetType=0 (video), ssrc=0
            block[o] = 0x90;
            block[o + 1] = 0;
            block[o + 2..o + 4].copy_from_slice(&(*lowseq as u16 + x as u16).to_be_bytes());
            block[o + 4..o + 8].copy_from_slice(&ts90.to_be_bytes());
        }
        for x in 0..pp {
            let o = x * blocksize;
            sock.send_to(&block[o..o + blocksize], peer)?;
        }
        *lowseq += pp as u32;
    }
    Ok(())
}

fn write_u32_le(b: &mut [u8], v: u32) {
    b[..4].copy_from_slice(&v.to_le_bytes());
}
