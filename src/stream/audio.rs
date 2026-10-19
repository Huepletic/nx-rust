//! Аудио-тракт: PulseAudio-монитор → libopus → RTP клиенту.
//!
//! Пакет повторяет Sunshine (docs/research/stream.cpp:1874-1876):
//! RTP header 0x80, payload type 97, ssrc=0, payload = opus-фрейм
//! packetDuration мс (48 кГц, стерео, CBR). Шифрование не включаем —
//! в DESCRIBE advertise'им только CONTROL_V2, SS_ENC_AUDIO не negotiated.
//!
//! ffmpeg шлёт RTP на локальный UDP-сокет (127.0.0.1), форвард-поток
//! пересылает датаграммы клиенту. Пир клиента (:49002) учим из его пингов —
//! тот же peer-learning, что у видео.

use anyhow::{Context, Result};
use std::net::{Ipv4Addr, UdpSocket};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{AnnounceConfig, StreamState};

/// Источник PulseAudio: monitor дефолтного sink'а (не микрофон!).
/// NX_AUDIO_SOURCE переопределяет (например, для testsink).
fn pulse_monitor_source() -> String {
    if let Ok(s) = std::env::var("NX_AUDIO_SOURCE") {
        return s;
    }
    let sink = Command::new("pactl")
        .args(["get-default-sink"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .unwrap_or_default();
    if sink.is_empty() {
        "default".to_string()
    } else {
        format!("{}.monitor", sink)
    }
}

/// Старт аудио-конвейера. Вызывается ПОСЛЕ установки PipelineHandle (child
/// ffmpeg'а привязывается к handle'у — stop() конвейера убивает и его).
pub fn start(st: Arc<StreamState>, cfg: AnnounceConfig, stop: Arc<AtomicBool>) -> Result<()> {
    if cfg.audio_channels == 0 {
        println!("audio: клиент отказался от звука (numChannels=0)");
        return Ok(());
    }
    if cfg.audio_channels > 2 {
        // Мультистрим-opus с channel mapping требует fmtp surround-params из
        // DESCRIBE и mapping-таблиц; планшет по умолчанию шлёт стерео.
        println!(
            "audio: клиент запросил {} каналов — отдаю стерео",
            cfg.audio_channels
        );
    }
    // opus допускает 2.5/5/10/20/40/60 мс; packetDuration клиента (дефолт 5).
    let dur = match cfg.packet_duration {
        0..=5 => 5.0,
        6..=10 => 10.0,
        11..=20 => 20.0,
        21..=40 => 40.0,
        _ => 60.0,
    };
    let kbps = std::env::var("NX_AUDIO_BITRATE_KBPS")
        .ok()
        .and_then(|s| s.parse::<u32>().ok())
        .filter(|&b| (16..=512).contains(&b))
        .unwrap_or(96);
    let src = pulse_monitor_source();

    // Локальный сокет: ffmpeg пишет RTP сюда, форвард уводит клиенту.
    let lsock = UdpSocket::bind((Ipv4Addr::LOCALHOST, 0)).context("bind локального аудио-сокета")?;
    lsock.set_read_timeout(Some(Duration::from_millis(500))).ok();
    let lport = lsock.local_addr()?.port();

    let mut cmd = Command::new("ffmpeg");
    cmd.args([
        "-loglevel", "error",
        "-f", "pulse",
        "-i", &src,
        "-ac", "2",
        "-ar", "48000",
        "-c:a", "libopus",
        // 2050 = restricted_lowdelay (строковое имя эта версия ffmpeg не ест,
        // проверено руками: 'Undefined constant' → берёт только число)
        "-application", "2050",
        "-frame_duration", &dur.to_string(),
        "-b:a", &format!("{}k", kbps),
        "-vbr", "off", // CBR — как у Sunshine
        "-payload_type", "97",
        "-ssrc", "0",
        "-rtpflags", "skip_rtcp",
        "-f", "rtp",
        &format!("rtp://127.0.0.1:{}", lport),
    ]);
    // Фиксированная задержка звука для синхронизации со видео (NX_AUDIO_DELAY_MS).
    // Видео-путь ~100 мс (захват+кодирование+сеть), аудио-путь ~20 мс —
    // без задержки звук убегает вперёд картинки.
    if let Ok(s) = std::env::var("NX_AUDIO_DELAY_MS") {
        if let Ok(ms) = s.parse::<u32>() {
            if ms > 0 {
                cmd.args(["-af", &format!("adelay={}:all=1", ms)]);
            }
        }
    }
    use std::os::unix::process::CommandExt;
    unsafe {
        // PDEATHSIG умирает вместе с СОЗДАВШИМ ПОТОКОМ (не с процессом!) —
        // спавнить ffmpeg можно только из долгоживущего потока: раньше spawn
        // шёл из RTSP-потока-обработчика, тот завершался после соединения и
        // SIGKILL убивал только что запущенный ffmpeg (наблюдалось 2026-10-19:
        // "ffmpeg звука завершился" сразу после старта, лог пустой).
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
            Ok(())
        });
    }
    let st_sup = st.clone();
    let stop_sup = stop.clone();
    std::thread::Builder::new()
        .name("audio-sup".into())
        .spawn(move || {
            let mut child = match cmd
                .stdout(Stdio::null())
                .stderr(Stdio::from(
                    std::fs::File::create("/tmp/nx-audio.log").expect("создание /tmp/nx-audio.log"),
                ))
                .spawn()
            {
                Ok(c) => c,
                Err(e) => {
                    println!("audio: запуск ffmpeg не удался: {e}");
                    return;
                }
            };
            // Привязка к жизненному циклу конвейера.
            {
                let g = st_sup.pipeline.lock().unwrap();
                match g.as_ref() {
                    Some(h) => {
                        *h.audio_child.lock().unwrap() = Some(child);
                    }
                    None => {
                        drop(g);
                        let _ = child.kill();
                        let _ = child.wait();
                        println!("audio: pipeline уже остановлен — аудио не стартует");
                        return;
                    }
                }
            }
            println!(
                "audio: старт (pulse '{}' → opus {}k/{}ms → rtp://127.0.0.1:{})",
                src, kbps, dur, lport
            );
            // Супервизор: ждать stop либо смерти ffmpeg (после kill() ребёнок
            // становится зомби до wait() — супервизор и убирает его).
            while !stop_sup.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(500));
                let dead = {
                    let g = st_sup.pipeline.lock().unwrap();
                    match g.as_ref() {
                        Some(h) => {
                            let mut c = h.audio_child.lock().unwrap();
                            match c.as_mut() {
                                Some(ch) => ch.try_wait().ok().flatten().is_some(),
                                None => true,
                            }
                        }
                        None => true,
                    }
                };
                if dead {
                    println!("audio: ffmpeg звука завершился");
                    break;
                }
            }
            if let Some(mut c) = st_sup
                .pipeline
                .lock()
                .unwrap()
                .as_ref()
                .and_then(|h| h.audio_child.lock().unwrap().take())
            {
                let _ = c.kill();
                let _ = c.wait();
            }
        })
        .context("старт audio-sup")?;

    // Форвард: локальный RTP → аудио-пир клиента.
    let st_f = st.clone();
    let stop_f = stop.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 2048];
        let mut pkts = 0u64;
        let mut bytes = 0u64;
        let mut since = Instant::now();
        while !stop_f.load(Ordering::Relaxed) {
            match lsock.recv_from(&mut buf) {
                Ok((n, _)) => {
                    let peer = *st_f.audio_peer.lock().unwrap();
                    if let Some(p) = peer {
                        // отправляем с :49002 (server_port из SETUP) — тот же
                        // сокет, с которого клиент ждёт медиа
                        if let Err(e) = st_f.audio_sock.send_to(&buf[..n], p) {
                            println!("audio: send_to {p}: {e}");
                        }
                        pkts += 1;
                        bytes += n as u64;
                    }
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) => {}
                Err(e) => {
                    println!("audio: локальный recv: {e}");
                    break;
                }
            }
            if since.elapsed() >= Duration::from_secs(10) {
                // ffmpeg-звук умер? (не крутится сэмплы → пакетов нет)
                let dead = {
                    let g = st_f.pipeline.lock().unwrap();
                    g.as_ref().and_then(|h| {
                        h.audio_child
                            .lock()
                            .unwrap()
                            .as_mut()
                            .map(|c| c.try_wait().ok().flatten().is_some())
                    })
                };
                if dead == Some(true) {
                    println!("audio: ffmpeg звука завершился — форвард стоп");
                    break;
                }
                let peer = *st_f.audio_peer.lock().unwrap();
                println!(
                    "audio: {:.1} pkt/s, {} kbps (peer {}, {} мс/фрейм)",
                    pkts as f64 / since.elapsed().as_secs_f64(),
                    bytes * 8 / 1000 / since.elapsed().as_secs().max(1),
                    peer.map(|p| p.to_string()).unwrap_or_else(|| "не учтён".into()),
                    dur
                );
                pkts = 0;
                bytes = 0;
                since = Instant::now();
            }
        }
    });

    Ok(())
}
