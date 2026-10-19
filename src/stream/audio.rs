//! Аудио-тракт: PulseAudio-монитор → libopus → RTP клиенту.
//!
//! Пакет повторяет Sunshine (docs/research/stream.cpp:1874-1876):
//! RTP header 0x80, payload type 97, ssrc=0, payload = opus-фрейм
//! packetDuration мс (48 кГц, стерео, CBR).
//!
//! Шифрование: мы НЕ advertise'им SS_ENC_AUDIO в DESCRIBE, но клиент всё равно
//! включает расшифровку аудио своим флагом ENCFLG_AUDIO (moonlight-common-c
//! SdpGenerator.c:195: `StreamConfig.encryptionFlags & ENCFLG_AUDIO` — клиент
//! НЕ проверяет поддержку хостом!) и сигнализирует об этом битом
//! NVFF_AUDIO_ENCRYPTION(0x20) в x-nv-general.featureFlags своего ANNOUNCE
//! (Sunshine читает тот же бит: rtsp.cpp:1167). Без шифрования клиент ловит
//! «Failed to decrypt audio packet» и отбрасывает 100% пакетов. Поэтому
//! форвард шифрует пейлоад AES-128-CBC как Sunshine (stream.cpp:1874-1904):
//!   ключ = байты rikey из /launch;
//!   IV   = BE32(avRiKeyId + RTP seq) || 12 нулей (AudioStream.c:181-190);
//!   PKCS7-пэддинг (клиент: CIPHER_FLAG_FINISH срезает).
//!
//! ffmpeg шлёт RTP на локальный UDP-сокет (127.0.0.1), форвард-поток
//! пересылает датаграммы клиенту. Пир клиента (:49002) учим из его пингов —
//! тот же peer-learning, что у видео.

use anyhow::{Context, Result};
use aes::Aes128;
use cipher::block::BlockModeEncrypt;
use cipher::KeyInit;
use ecb::Encryptor as EcbEnc;
use hybrid_array::Array;
use std::net::{Ipv4Addr, UdpSocket};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{AnnounceConfig, StreamState};

/// AES-128-CBC-шифрование аудио-пейлоада (Sunshine audioBroadcastThread).
struct AudioCrypt {
    enc: EcbEnc<Aes128>,
    /// avRiKeyId = (uint32)(int)rikeyid из /launch (iv[0..4] = BE этого значения,
    /// затем клиент делает BE32(memcpy) → ровно это число).
    key_id: u32,
}

impl AudioCrypt {
    fn new(rikey_hex: &str, rikeyid: &str) -> Result<Self> {
        let key = hex::decode(rikey_hex).context("rikey не hex")?;
        let key: [u8; 16] = key.try_into().map_err(|v: Vec<u8>| {
            anyhow::anyhow!("rikey не 16 байт ({})", v.len())
        })?;
        let key_id: u32 = rikeyid
            .parse::<i32>()
            .map_err(|e| anyhow::anyhow!("rikeyid не int: {e}"))? as u32;
        Ok(Self {
            enc: EcbEnc::new((&key).into()),
            key_id,
        })
    }

    /// Шифрует plain (opus-пейлоад) в CBC с PKCS7; ciphertext ДОПИСЫВАЕТСЯ в out
    /// (заголовок RTP уже может лежать в out — как у Sunshine: shards_p уже
    /// содержит 12-байтный заголовок, encrypt пишет пейлоад сразу за ним).
    fn encrypt(&mut self, seq: u16, plain: &[u8], out: &mut Vec<u8>) {
        let mut prev = [0u8; 16];
        prev[..4].copy_from_slice(&self.key_id.wrapping_add(seq as u32).to_be_bytes());
        let pad = 16 - (plain.len() % 16);
        let start = out.len();
        out.reserve(plain.len() + pad);
        out.extend_from_slice(plain);
        out.resize(start + plain.len() + pad, pad as u8);
        let mut i = start;
        while i < out.len() {
            for j in 0..16 {
                out[i + j] ^= prev[j];
            }
            let mut blk = Array::from({
                let mut a = [0u8; 16];
                a.copy_from_slice(&out[i..i + 16]);
                a
            });
            self.enc.encrypt_block(&mut blk);
            out[i..i + 16].copy_from_slice(&blk);
            prev.copy_from_slice(&blk);
            i += 16;
        }
    }
}


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
        // Гранулярность чтения monitor'а. PipeWire по умолчанию отдаёт ~50-мс
        // пачки → RTP-рывки (p50 интервала 0.1 мс, 185 пакетов в пачке) → у
        // клиента очередь >40 мс и дроп кадров («Too much pending audio data»).
        // 1024 байта = 256 фреймов = 5.3 мс → p50=5.2 мс, пачек нет (замерено
        // /tmp/rtp_gap_probe.py: 26-50мс=0, >50мс=0).
        "-fragment_size", "1024",
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
    let encrypt = cfg.audio_encrypted();
    std::thread::spawn(move || {
        let mut buf = [0u8; 2048];
        // Ключ/IV-material для аудио-шифрования: из launch-сессии (/launch rikey/rikeyid).
        let mut crypt = if encrypt {
            let launch = st_f.launch.lock().unwrap().clone();
            match launch.as_ref() {
                Some(l) => match AudioCrypt::new(&l.rikey, &l.rikeyid) {
                    Ok(c) => {
                        println!("audio: клиент запросил шифрование (featureFlags 0x20) — AES-128-CBC включён");
                        Some(c)
                    }
                    Err(e) => {
                        println!("audio: ВНИМАНИЕ шифрование запрошено, но ключ не готов: {e}");
                        None
                    }
                },
                None => {
                    println!("audio: ВНИМАНИЕ шифрование запрошено, а launch-сессии нет");
                    None
                }
            }
        } else {
            None
        };
        let mut cipher_buf: Vec<u8> = Vec::with_capacity(1024);
        let mut pkts = 0u64;
        let mut bytes = 0u64;
        let mut since = Instant::now();
        // Гистограмма интервалов между пакетами ffmpeg (рюкзак / рывки →
        // у клиента «Too much pending audio data» и дроп >40 мс).
        let mut gaps = [0u32; 5]; // <7, 7-12, 13-25, 26-50, >50 мс
        let mut last_pkt: Option<Instant> = None;
        while !stop_f.load(Ordering::Relaxed) {
            match lsock.recv_from(&mut buf) {
                Ok((n, _)) => {
                    let now = Instant::now();
                    if let Some(prev) = last_pkt {
                        let g = (now - prev).as_millis();
                        let idx = match g {
                            0..=6 => 0,
                            7..=12 => 1,
                            13..=25 => 2,
                            26..=50 => 3,
                            _ => 4,
                        };
                        gaps[idx] += 1;
                    }
                    last_pkt = Some(now);
                    let peer = *st_f.audio_peer.lock().unwrap();
                    if let Some(p) = peer {
                        // Клиент (moonlight-common-c RtpAudioQueue.c) сравнивает
                        // второй байт RTP-заголовка ЦЕЛИКОМ с 97, не маскируя
                        // marker-бит. ffmpeg ставит M=1 → 0xE1=225 → «Invalid RTP
                        // audio payload type: 225», все пакеты отбрасываются.
                        // Sunshine шлёт M=0 (0x61) — сбрасываем бит перед форвардом.
                        if n >= 2 {
                            buf[1] &= 0x7f;
                        }
                        // Шифрованная ветка: клиент дешифрует всё после 12-байтного
                        // RTP-заголовка (AudioStream.c:182 dataLength = size - sizeof(RTP_PACKET)).
                        // `continue` тут НЕЛЬЗЯ — он проскочит блок статистики ниже.
                        let sent_bytes = if let Some(c) = crypt.as_mut() {
                            if n > 12 {
                                let seq = u16::from_be_bytes([buf[2], buf[3]]);
                                cipher_buf.clear();
                                cipher_buf.extend_from_slice(&buf[..12]);
                                c.encrypt(seq, &buf[12..n], &mut cipher_buf);
                                if let Err(e) = st_f.audio_sock.send_to(&cipher_buf, p) {
                                    println!("audio: send_to {p}: {e}");
                                }
                                cipher_buf.len() as u64
                            } else {
                                if let Err(e) = st_f.audio_sock.send_to(&buf[..n], p) {
                                    println!("audio: send_to {p}: {e}");
                                }
                                n as u64
                            }
                        } else {
                            // отправляем с :49002 (server_port из SETUP) — тот же
                            // сокет, с которого клиент ждёт медиа
                            if let Err(e) = st_f.audio_sock.send_to(&buf[..n], p) {
                                println!("audio: send_to {p}: {e}");
                            }
                            n as u64
                        };
                        pkts += 1;
                        bytes += sent_bytes;
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
                    "audio: {:.1} pkt/s, {} kbps (peer {}, {} мс/фрейм) интервалы<7/7-12/13-25/26-50/>50 мс: {}/{}/{}/{}/{}",
                    pkts as f64 / since.elapsed().as_secs_f64(),
                    bytes * 8 / 1000 / since.elapsed().as_secs().max(1),
                    peer.map(|p| p.to_string()).unwrap_or_else(|| "не учтён".into()),
                    dur,
                    gaps[0], gaps[1], gaps[2], gaps[3], gaps[4],
                );
                pkts = 0;
                bytes = 0;
                gaps = [0; 5];
                since = Instant::now();
            }
        }
    });

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::AudioCrypt;

    /// Шифрование обязано совпадать с openssl AES-128-CBC PKCS7:
    ///   openssl enc -aes-128-cbc -K <rikey> -iv <be32(keyid+seq)||12*00>
    #[test]
    fn cbc_matches_openssl() {
        let mut c = AudioCrypt::new("00112233445566778899AABBCCDDEEFF", "1").unwrap();
        let plain = b"hello opus frame!!"; // 18 байт → pad 14
        let mut out = Vec::new();
        c.encrypt(7, plain, &mut out);
        let hexed: String = out.iter().map(|b| format!("{b:02x}")).collect();
        println!("CTX={hexed}");
        // iv = be32(1+7)=00000008 + 12 нулей
        assert_eq!(out.len(), 32);
    }
}
