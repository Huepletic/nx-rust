//! RTSP-сервер :49010 — по одному TCP-соединению на запрос (так же ходит Moonlight:
//! клиент открывает НОВОЕ соединение на каждый RTSP-запрос и читает ответ до EOF).
//! Формат: `RTSP/1.0 200 OK\r\nCSeq: N\r\nHeader: value\r\n\r\n[payload]`,
//! ровно один пробел после двоеточия (парсер клиента — strtok по " :\r\n").

use super::*;
use std::io::{Read, Write};
use std::net::{Ipv4Addr, TcpListener, TcpStream};

pub fn start(st: Arc<StreamState>) -> Result<()> {
    let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, RTSP_PORT))
        .with_context(|| format!("bind TCP {}", RTSP_PORT))?;
    std::thread::spawn(move || {
        loop {
            match listener.accept() {
                Ok((sock, addr)) => {
                    let st = st.clone();
                    std::thread::spawn(move || {
                        if let Err(e) = handle_conn(sock, addr, &st) {
                            println!("rtsp[{}]: {}", addr, e);
                        }
                    });
                }
                Err(e) => println!("rtsp: accept: {}", e),
            }
        }
    });
    Ok(())
}

struct Req {
    method: String,
    target: String,
    cseq: String,
    body: Vec<u8>,
}

fn find_headers_end(b: &[u8]) -> Option<usize> {
    b.windows(4).position(|w| w == b"\r\n\r\n")
}

fn read_req(sock: &mut TcpStream) -> Result<Req> {
    let mut buf = Vec::with_capacity(8192);
    let mut tmp = [0u8; 4096];
    let header_end = loop {
        if let Some(p) = find_headers_end(&buf) {
            break p;
        }
        if buf.len() > 1 << 20 {
            anyhow::bail!("слишком большой запрос");
        }
        let n = sock.read(&mut tmp).context("чтение запроса")?;
        if n == 0 {
            anyhow::bail!("обрыв до конца заголовков");
        }
        buf.extend_from_slice(&tmp[..n]);
    };
    let headers = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = headers.split("\r\n");
    let mut f = lines.next().unwrap_or("").split_whitespace();
    let method = f.next().unwrap_or("").to_string();
    let target = f.next().unwrap_or("").to_string();
    let mut cseq = "0".to_string();
    let mut content_len = 0usize;
    for l in lines {
        if let Some((n, v)) = l.split_once(':') {
            let n = n.trim();
            let v = v.trim();
            if n.eq_ignore_ascii_case("cseq") {
                cseq = v.to_string();
            }
            if n.eq_ignore_ascii_case("content-length") {
                content_len = v.parse().unwrap_or(0);
            }
        }
    }
    let mut body = buf[header_end + 4..].to_vec();
    while body.len() < content_len {
        let n = sock.read(&mut tmp).context("чтение тела")?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&tmp[..n]);
    }
    body.truncate(content_len.min(body.len()));
    Ok(Req { method, target, cseq, body })
}

fn resp(status: &str, cseq: &str, headers: &[(&str, String)], payload: Option<&[u8]>) -> Vec<u8> {
    let mut s = format!("RTSP/1.0 {}\r\nCSeq: {}\r\n", status, cseq);
    for (n, v) in headers {
        s.push_str(&format!("{}: {}\r\n", n, v));
    }
    if let Some(p) = payload {
        s.push_str(&format!("Content-Length: {}\r\n", p.len()));
    }
    s.push_str("\r\n");
    let mut out = s.into_bytes();
    if let Some(p) = payload {
        out.extend_from_slice(p);
    }
    out
}

fn handle_conn(mut sock: TcpStream, addr: SocketAddr, st: &Arc<StreamState>) -> Result<()> {
    sock.set_read_timeout(Some(std::time::Duration::from_secs(30)))?;
    sock.set_write_timeout(Some(std::time::Duration::from_secs(10)))?;
    let req = read_req(&mut sock)?;
    println!(
        "rtsp: {} {} {} CSeq={} body={}B",
        addr, req.method, req.target, req.cseq, req.body.len()
    );
    let out: Vec<u8> = match req.method.as_str() {
        "OPTIONS" => resp(
            "200 OK",
            &req.cseq,
            &[("Public", "OPTIONS, DESCRIBE, SETUP, ANNOUNCE, PLAY, TEARDOWN".to_string())],
            None,
        ),
        "DESCRIBE" => resp(
            "200 OK",
            &req.cseq,
            &[("Content-Type", "application/sdp".to_string())],
            Some(sdp().as_bytes()),
        ),
        "SETUP" => setup_resp(st, &req),
        "ANNOUNCE" => {
            let cfg = parse_announce(&req.body);
            println!(
                "rtsp: ANNOUNCE packetSize={} fps={} viewport={}x{} bitrate={}k fec_min={} reliableUdp={} encryption={}",
                cfg.packet_size, cfg.max_fps, cfg.client_w, cfg.client_h,
                cfg.bitrate_kbps, cfg.fec_min_required, cfg.use_reliable_udp, cfg.encryption_enabled
            );
            *st.announce.lock().unwrap() = Some(cfg.clone());
            // Сессия стартует на ANNOUNCE (решение по 7.1.431), PLAY просто 200.
            pipeline::start(st, cfg)?;
            resp("200 OK", &req.cseq, &[], None)
        }
        "PLAY" => resp("200 OK", &req.cseq, &[], None),
        "TEARDOWN" => {
            stop_pipeline(st);
            resp("200 OK", &req.cseq, &[], None)
        }
        other => {
            println!("rtsp: неизвестный метод {}", other);
            resp("404 Not Found", &req.cseq, &[], None)
        }
    };
    sock.write_all(&out)?;
    sock.shutdown(std::net::Shutdown::Both).ok();
    Ok(())
}

/// SDP: только H.264. НЕ добавлять: AV1/90000, sprop-parameter-sets=AAAAAU (HEVC),
/// x-nv-video[0].refPicInvalidation.
/// encryptionSupported:1 = SS_ENC_CONTROL_V2 (Sunshine шлёт 1|4): клиент включает
/// шифрование control AES-128-GCM со стандартным 12-байтным IV (V2-стиль);
/// биты VIDEO(2)/AUDIO(4) не advertise → все медиа-потоки plaintext (MVP).
/// encryptionRequested не шлём — иначе клиент может ВЫНУДИТЬ видеошифрование
/// при client opt-out (sdpg.c:284 «despite client opt-out»).
fn sdp() -> String {
    "v=0\r\n\
     o=- 0 0 IN IP4 127.0.0.1\r\n\
     s=NxStream\r\n\
     c=IN IP4 0.0.0.0\r\n\
     t=0 0\r\n\
     a=x-ss-general.featureFlags:0\r\n\
     a=x-ss-general.encryptionSupported:1\r\n\
     m=video 0 RTP/AVP 96\r\n\
     a=rtpmap:96 H264/90000\r\n\
     m=audio 0 RTP/AVP 97\r\n\
     a=rtpmap:97 opus/48000/2\r\n\
     a=fmtp:97 surround-params=21101\r\n"
        .to_string()
}

/// SETUP: Session обязателен (клиент падает с "missing session attribute"),
/// Transport — server_port своего порта, X-SS-Ping-Payload (audio/video)
/// либо X-SS-Connect-Data (control). Эталон: rtsp.cpp cmd_setup.
fn setup_resp(st: &Arc<StreamState>, req: &Req) -> Vec<u8> {
    let ty = req
        .target
        .split('=')
        .nth(1)
        .and_then(|s| s.split('/').next())
        .unwrap_or("");
    let launch = st.launch.lock().unwrap().clone();
    let ping = launch
        .as_ref()
        .map(|l| l.av_ping_payload.clone())
        .unwrap_or_else(|| "0".repeat(16));
    let ccd = launch.as_ref().map(|l| l.control_connect_data).unwrap_or(0);
    let (port, extra): (u16, Vec<(&str, String)>) = match ty {
        "audio" => (AUDIO_PORT, vec![("X-SS-Ping-Payload", ping)]),
        "video" => (VIDEO_PORT, vec![("X-SS-Ping-Payload", ping)]),
        "control" => (CONTROL_PORT, vec![("X-SS-Connect-Data", ccd.to_string())]),
        _ => return resp("404 Not Found", &req.cseq, &[], None),
    };
    let mut headers = vec![
        ("Session", "DEADBEEFCAFE;timeout = 90".to_string()),
        ("Transport", format!("unicast;server_port={}", port)),
    ];
    headers.extend(extra);
    resp("200 OK", &req.cseq, &headers, None)
}

/// ANNOUNCE: `a=name:value ` (возможен один trailing space) — срезаем ровно один.
fn parse_announce(body: &[u8]) -> AnnounceConfig {
    let text = String::from_utf8_lossy(body);
    let mut cfg = AnnounceConfig::default();
    for line in text.split('\n') {
        let line = line.trim_end_matches('\r');
        if !line.starts_with("a=") {
            continue;
        }
        let Some((name, mut val)) = line[2..].split_once(':') else {
            continue;
        };
        if val.ends_with(' ') {
            val = &val[..val.len() - 1];
        }
        match name {
            "x-nv-video[0].packetSize" => cfg.packet_size = val.parse().unwrap_or(1024),
            "x-nv-video[0].maxFPS" => cfg.max_fps = val.parse().unwrap_or(60),
            "x-nv-video[0].clientViewportWd" => cfg.client_w = val.parse().unwrap_or(0),
            "x-nv-video[0].clientViewportHt" => cfg.client_h = val.parse().unwrap_or(0),
            "x-nv-vqos[0].fec.minRequiredFecPackets" => {
                cfg.fec_min_required = val.parse().unwrap_or(0)
            }
            "x-nv-general.useReliableUdp" => cfg.use_reliable_udp = val.parse().unwrap_or(1),
            "x-ss-general.encryptionEnabled" => cfg.encryption_enabled = val.parse().unwrap_or(0),
            "x-nv-audio.surround.numChannels" => {
                cfg.audio_channels = val.parse().unwrap_or(2)
            }
            "x-nv-aqos.packetDuration" => cfg.packet_duration = val.parse().unwrap_or(5),
            "x-ml-video.configuredBitrateKbps" => {
                let b: u64 = val.parse().unwrap_or(0);
                if b > 0 {
                    cfg.bitrate_kbps = b;
                }
            }
            _ => {}
        }
    }
    // Клиентский assert: packetSize кратен 16. Иначе — безопасный дефолт.
    if cfg.packet_size < 512 || cfg.packet_size % 16 != 0 {
        println!(
            "rtsp: ANNOUNCE packetSize={} не кратен 16 — беру 1024",
            cfg.packet_size
        );
        cfg.packet_size = 1024;
    }
    if cfg.max_fps == 0 {
        cfg.max_fps = 60;
    }
    // bit0=CONTROL_V2 (расшифровываем в control.rs), bit1=VIDEO, bit2=AUDIO (не поддерживаем)
    if cfg.encryption_enabled & !1 != 0 {
        println!(
            "rtsp: ВНИМАНИЕ клиент запросил encryptionEnabled={} — видео/аудио-шифрование не поддерживается",
            cfg.encryption_enabled
        );
    } else if cfg.encryption_enabled != 0 {
        println!("rtsp: control V2 шифрование включено клиентом (encryptionEnabled=1)");
    }
    cfg
}
