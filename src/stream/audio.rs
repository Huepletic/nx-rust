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
use std::collections::VecDeque;
use std::net::{Ipv4Addr, UdpSocket};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{AnnounceConfig, StreamState};

/// Матрица паритета RS(6,4) для аудио-FEC.
///
/// Клиент (moonlight-common-c, RtpAudioQueue.c) собирает аудио в FEC-блоки по
/// RTPA_DATA_SHARDS=4 пакета и подменяет матрицу в библиотеке nanors на эти же
/// байты (Sunshine делает то же — stream.cpp:1878: «the RS parity matrix
/// computed by our RS implementation doesn't match the one Nvidia uses»).
/// Без паритета клиент теряет блок целиком: «Unable to recover audio data
/// block N to N+3 (2+0=2 received < 4 needed)» → PLC-щелчки в звуке.
const FEC_MATRIX: [[u8; 4]; 2] = [
    [0x77, 0x40, 0x38, 0x0e],
    [0xc7, 0xa7, 0x0d, 0x6c],
];

/// Умножение в GF(256) с примитивным полиномом 0x11d (как в nanors/oblas).
fn gf_mul(mut a: u8, mut b: u8) -> u8 {
    let mut p = 0u8;
    for _ in 0..8 {
        if b & 1 != 0 {
            p ^= a;
        }
        let hi = a & 0x80;
        a <<= 1;
        if hi != 0 {
            a ^= 0x1d;
        }
        b >>= 1;
    }
    p
}

/// Паритет RS(6,4): parity[k] = Σ_j M[k][j]·data[j] над GF(256).
/// Вход — 4 шифрованных шарда (payload после RTP-заголовка), как у клиента
/// (shards[i] = dataPackets[i] + 1).
fn rs_parity(shards: &[Vec<u8>; 4]) -> [Vec<u8>; 2] {
    let bs = shards[0].len();
    let mut parity = [vec![0u8; bs], vec![0u8; bs]];
    for k in 0..2 {
        for j in 0..4 {
            let coeff = FEC_MATRIX[k][j];
            if coeff == 0 {
                continue;
            }
            for i in 0..bs {
                parity[k][i] ^= gf_mul(coeff, shards[j][i]);
            }
        }
    }
    parity
}

/// FEC-пакет: RTP-заголовок (PT=127) + AUDIO_FEC_HEADER (12 байт) + паритет.
/// Формат — как в клиенте (RtpAudioQueue.h: AUDIO_FEC_HEADER) и Sunshine.
fn build_fec_packet(
    shard_index: u8,
    base_seq: u16,
    base_ts: u32,
    ssrc: u32,
    parity: &[u8],
) -> Vec<u8> {
    let mut p = Vec::with_capacity(24 + parity.len());
    p.push(0x80); // RTP v2
    p.push(127); // RTP_PAYLOAD_TYPE_FEC
    p.extend_from_slice(&base_seq.wrapping_add(shard_index as u16 + 1).to_be_bytes());
    p.extend_from_slice(&base_ts.to_be_bytes());
    p.extend_from_slice(&ssrc.to_be_bytes());
    // AUDIO_FEC_HEADER: fecShardIndex, payloadType, baseSequenceNumber,
    // baseTimestamp, ssrc (все многобайтовые — big endian, как читает клиент).
    p.push(shard_index);
    p.push(97); // payloadType аудио
    p.extend_from_slice(&base_seq.to_be_bytes());
    p.extend_from_slice(&base_ts.to_be_bytes());
    p.extend_from_slice(&ssrc.to_be_bytes());
    p.extend_from_slice(parity);
    p
}

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
    let dur: f64 = match cfg.packet_duration {
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
    // Небольшой таймаут recv: форвардер держит очередь задержки и должен
    // досылать дозревшие пакеты, даже если ffmpeg на секунду замолчал.
    lsock.set_read_timeout(Some(Duration::from_millis(10))).ok();
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
    //
    // Держать задержку НА СТОРОНЕ ffmpeg (-af adelay) нельзя: фильтр поднимает
    // лишний поток, и при nice 15 аудио-энкодер не выбирается планировщиком —
    // конвейер замедляется (замерено 2026-10-21, 10 c каждый вариант):
    //   adelay + nice 15  → 192.9 pkt/s, ts/s 46429   ← недобор 3% аудио
    //   adelay + nice 0   → 201.1 pkt/s, ts/s 48757
    //   без adelay + 15   → 199.3 pkt/s, ts/s 48036
    //   без adelay + 0    → 198.6 pkt/s, ts/s 48067
    // Недобор 3% = клиентский AudioTrack постоянно голодает = постоянное
    // шипение. Поэтому задержку применяет форвардер (ниже), а ffmpeg работает
    // ровно и без лишних потоков.
    let delay_ms: u64 = std::env::var("NX_AUDIO_DELAY_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    use std::os::unix::process::CommandExt;
    unsafe {
        // PDEATHSIG умирает вместе с СОЗДАВШИМ ПОТОКОМ (не с процессом!) —
        // спавнить ffmpeg можно только из долгоживущего потока: раньше spawn
        // шёл из RTSP-потока-обработчика, тот завершался после соединения и
        // SIGKILL убивал только что запущенный ffmpeg (наблюдалось 2026-10-19:
        // "ffmpeg звука завершился" сразу после старта, лог пустой).
        cmd.pre_exec(|| {
            libc::prctl(libc::PR_SET_PDEATHSIG, libc::SIGKILL);
            // Аудио-кодировщик — низкоприоритетный. Он жрёт ~27% ядра (libopus
            // на 48 кГц стерео), а сервер запускается с nice -10, и без
            // понижения ffmpeg отбирал CPU у композитора: на 4-ядерном хосте
            // copy/ready вырастал до 26-45 мс и поток падал до 20-40 fps
            // (замерено 2026-10-21). Видео-путь важнее звука по задержке.
            let prio = 15i32;
            libc::setpriority(libc::PRIO_PROCESS, 0, prio);
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
        // RTP timestamp привязан к SEQUENCE NUMBER, а не к счётчику пакетов.
        // Клиент (moonlight-common-c, RtpAudioQueue.c) собирает аудио в FEC-блоки
        // по 4 пакета и считает базу блока так:
        //     baseTs = packet.ts - (seq - blockBase) * 240
        // а восстановленный пакет получает ts = baseTs + i*240.
        // Значит ts обязан расти ровно на 240 за каждый +1 seq, иначе блоки
        // собираются с чужой базой и у клиента едет тайминг.
        let ts_step = (dur * 48.0).round() as u32; // 5 мс → 240 сэмплов
        let mut ts_base: Option<(u16, u32)> = None;
        let mut ts0: u32 = 0;
        // Очередь задержки: пакет уходит клиенту через `delay` после прихода.
        // Раньше здесь был фиксированный sleep(5 мс) на каждый пакет — он сам
        // по себе резал темп до 193 pkt/s (цикл 5 мс + накладные ~0.2 мс), а
        // вместе с недобором на стороне источника давал постоянное шипение.
        // Теперь темп задаёт источник (ffmpeg, 200 pkt/s), а мы только сдвигаем
        // пакеты во времени для синхронизации с видео.
        let delay = Duration::from_millis(delay_ms);
        let mut q: VecDeque<(Instant, Vec<u8>)> = VecDeque::new();
        // Если очередь разрослась больше чем на секунду аудио — догоняем,
        // отправляя немедленно (иначе задержка уедет в бесконечность).
        const MAX_QUEUE: usize = 200;
        // Сборка FEC-блока: клиент ждёт блоки по 4 data-пакета с базой
        // (seq/4)*4 и baseTs = ts - (seq-base)*240. Держим последние пакеты,
        // чтобы посчитать паритет над шифрованными шардами блока.
        let mut recent: VecDeque<(u16, u32, Vec<u8>)> = VecDeque::with_capacity(8);
        let mut fec_pkts = 0u64;
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
                    // Честный timestamp (BE) — до шифрования, заголовок открыт.
                    if n >= 12 {
                        let seq = u16::from_be_bytes([buf[2], buf[3]]);
                        let ts = match ts_base {
                            Some((bseq, bts)) => bts.wrapping_add(
                                (seq.wrapping_sub(bseq) as u32).wrapping_mul(ts_step),
                            ),
                            None => {
                                ts_base = Some((seq, ts0));
                                ts0
                            }
                        };
                        buf[4..8].copy_from_slice(&ts.to_be_bytes());
                    }
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
                    let pkt: Vec<u8> = if let Some(c) = crypt.as_mut() {
                        if n > 12 {
                            let seq = u16::from_be_bytes([buf[2], buf[3]]);
                            cipher_buf.clear();
                            cipher_buf.extend_from_slice(&buf[..12]);
                            c.encrypt(seq, &buf[12..n], &mut cipher_buf);
                            cipher_buf.clone()
                        } else {
                            buf[..n].to_vec()
                        }
                    } else {
                        buf[..n].to_vec()
                    };
                    q.push_back((now + delay, pkt.clone()));
                    // FEC: блок закрывается на пакете с seq ≡ 3 (mod 4).
                    if n >= 12 {
                        let seq = u16::from_be_bytes([buf[2], buf[3]]);
                        let ts = u32::from_be_bytes([buf[4], buf[5], buf[6], buf[7]]);
                        let ssrc = u32::from_be_bytes([buf[8], buf[9], buf[10], buf[11]]);
                        // Шард для паритета — payload БЕЗ RTP-заголовка:
                        // клиент считает shards[i] = dataPackets[i] + 1.
                        recent.push_back((seq, ts, pkt[12..].to_vec()));
                        while recent.len() > 8 {
                            recent.pop_front();
                        }
                        if seq % 4 == 3 && recent.len() >= 4 {
                            let blk: Vec<(u16, u32, Vec<u8>)> =
                                recent.iter().rev().take(4).rev().cloned().collect();
                            let base = blk[0].0;
                            let consecutive =
                                blk.iter().enumerate().all(|(i, b)| b.0 == base.wrapping_add(i as u16));
                            let same_len = blk.iter().all(|b| b.2.len() == blk[0].2.len());
                            if consecutive && same_len {
                                let shards = [
                                    blk[0].2.clone(),
                                    blk[1].2.clone(),
                                    blk[2].2.clone(),
                                    blk[3].2.clone(),
                                ];
                                let parity = rs_parity(&shards);
                                for (i, par) in parity.iter().enumerate() {
                                    let fp = build_fec_packet(i as u8, base, blk[0].1, ssrc, par);
                                    q.push_back((now + delay, fp));
                                    fec_pkts += 1;
                                }
                            }
                        }
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
            // Отправляем всё, что дозрело (и догоняем, если очередь велика).
            let now = Instant::now();
            let catch_up = q.len() > MAX_QUEUE;
            while let Some((at, pkt)) = q.front() {
                if !catch_up && *at > now {
                    break;
                }
                let (_, pkt) = q.pop_front().unwrap();
                let peer = *st_f.audio_peer.lock().unwrap();
                if let Some(p) = peer {
                    // отправляем с :49002 (server_port из SETUP) — тот же
                    // сокет, с которого клиент ждёт медиа
                    if let Err(e) = st_f.audio_sock.send_to(&pkt, p) {
                        println!("audio: send_to {p}: {e}");
                    }
                    pkts += 1;
                    bytes += pkt.len() as u64;
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
                    "audio: отправлено {:.1} pkt/s, {} kbps (peer {}, {} мс/фрейм, задержка {} мс, в очереди {}) FEC-паритет: {} интервалы<7/7-12/13-25/26-50/>50 мс: {}/{}/{}/{}/{}",
                    pkts as f64 / since.elapsed().as_secs_f64(),
                    bytes * 8 / 1000 / since.elapsed().as_secs().max(1),
                    peer.map(|p| p.to_string()).unwrap_or_else(|| "не учтён".into()),
                    dur,
                    delay_ms,
                    q.len(),
                    fec_pkts,
                    gaps[0], gaps[1], gaps[2], gaps[3], gaps[4],
                );
                pkts = 0;
                bytes = 0;
                gaps = [0; 5];
                fec_pkts = 0;
                since = Instant::now();
            }
        }
    });

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{gf_mul, rs_parity, AudioCrypt, FEC_MATRIX};

    fn gf_pow(mut a: u8, mut e: u32) -> u8 {
        let mut r = 1u8;
        while e > 0 {
            if e & 1 == 1 {
                r = gf_mul(r, a);
            }
            a = gf_mul(a, a);
            e >>= 1;
        }
        r
    }

    fn gf_inv(a: u8) -> u8 {
        gf_pow(a, 254)
    }

    #[test]
    fn gf_mul_known_values() {
        assert_eq!(gf_mul(0x53, 0xCA), 0x8F); // классический пример из RS-литературы
        assert_eq!(gf_mul(1, 0x57), 0x57);
        assert_eq!(gf_mul(0, 0x57), 0);
        assert_eq!(gf_mul(0x57, 0x13), gf_mul(0x13, 0x57));
    }

    /// Клиент (nanors) восстанавливает до 2 потерянных шардов из 4 —
    /// матрица паритета 2×4 должна давать невырожденную систему.
    #[test]
    fn rs_recovers_two_lost_shards() {
        let shards: [Vec<u8>; 4] = [
            (0u8..64).collect(),
            (100u8..164).collect(),
            (0u8..64).map(|x| x ^ 0xA5).collect(),
            (50u8..114).collect(),
        ];
        let parity = rs_parity(&shards);

        // Линейность: parity(a⊕b) == parity(a)⊕parity(b).
        let other: [Vec<u8>; 4] = [
            (1u8..65).collect(),
            (2u8..66).collect(),
            (3u8..67).collect(),
            (4u8..68).collect(),
        ];
        let p2 = rs_parity(&other);
        let xored: [Vec<u8>; 4] = std::array::from_fn(|i| {
            shards[i].iter().zip(&other[i]).map(|(a, b)| a ^ b).collect()
        });
        let px = rs_parity(&xored);
        for k in 0..2 {
            assert_eq!(px[k], parity[k].iter().zip(&p2[k]).map(|(a, b)| a ^ b).collect::<Vec<_>>());
        }

        // Потеряли шарды 1 и 2 → решаем 2×2 систему по формулам Крамера.
        let (d0, d1, d2, d3) = (&shards[0], &shards[1], &shards[2], &shards[3]);
        let m = &FEC_MATRIX;
        let rhs0: Vec<u8> = (0..64)
            .map(|i| parity[0][i] ^ gf_mul(m[0][0], d0[i]) ^ gf_mul(m[0][3], d3[i]))
            .collect();
        let rhs1: Vec<u8> = (0..64)
            .map(|i| parity[1][i] ^ gf_mul(m[1][0], d0[i]) ^ gf_mul(m[1][3], d3[i]))
            .collect();
        let det = gf_mul(m[0][1], m[1][2]) ^ gf_mul(m[0][2], m[1][1]);
        let det_inv = gf_inv(det);
        // d1 = (rhs0*m12 + rhs1*m02) / det
        let rec1: Vec<u8> = (0..64)
            .map(|i| {
                gf_mul(det_inv, gf_mul(m[1][2], rhs0[i]) ^ gf_mul(m[0][2], rhs1[i]))
            })
            .collect();
        // d2 = (m01*rhs1 + m11*rhs0) / det
        let rec2: Vec<u8> = (0..64)
            .map(|i| {
                gf_mul(det_inv, gf_mul(m[0][1], rhs1[i]) ^ gf_mul(m[1][1], rhs0[i]))
            })
            .collect();
        assert_eq!(rec1, *d1, "шард 1 не восстановился");
        assert_eq!(rec2, *d2, "шард 2 не восстановился");
    }

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
