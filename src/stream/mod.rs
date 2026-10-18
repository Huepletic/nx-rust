//! stream: RTSP + RTP-видео + ENet-control (Milestone 3).
//! Живёт рядом с NVHTTP: /launch создаёт launch-сессию и отдаёт sessionUrl0,
//! клиент ходит в RTSP :49010, видео идёт RTP/UDP :49000, control — ENet :49001,
//! audio-drain — :49002. Спека: docs/gamestream-spec.md, эталон — Sunshine stream.cpp/rtsp.cpp.

pub mod control;
pub mod input;
pub mod pipeline;
pub mod rtsp;

use anyhow::{Context, Result};
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicI32};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

pub const VIDEO_PORT: u16 = 49000;
pub const CONTROL_PORT: u16 = 49001;
pub const AUDIO_PORT: u16 = 49002;
pub const RTSP_PORT: u16 = 49010;

/// Launch-сессия, созданная /launch (клиент запускает стрим).
#[derive(Debug)]
pub struct LaunchSession {
    pub appid: i32,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub rikey: String,
    pub rikeyid: String,
    /// 16 ASCII-hex символов (hex от 8 байт) — X-SS-Ping-Payload.
    pub av_ping_payload: String,
    pub control_connect_data: u32,
}

/// Параметры из RTSP ANNOUNCE (SDP `a=name:value`).
#[derive(Debug, Clone)]
pub struct AnnounceConfig {
    pub packet_size: usize,
    pub max_fps: u32,
    pub client_w: u32,
    pub client_h: u32,
    pub fec_min_required: u32,
    pub use_reliable_udp: u32,
    pub encryption_enabled: u32,
    pub bitrate_kbps: u64,
}

impl Default for AnnounceConfig {
    fn default() -> Self {
        Self {
            packet_size: 1024,
            max_fps: 60,
            client_w: 0,
            client_h: 0,
            fec_min_required: 0,
            use_reliable_udp: 1,
            encryption_enabled: 0,
            bitrate_kbps: 10_000,
        }
    }
}

/// Живой конвейер: ffmpeg-дочерник + флаг остановки. stop() бьёт и по флагу, и по kill.
pub struct PipelineHandle {
    pub stop: Arc<AtomicBool>,
    pub child: Mutex<Option<std::process::Child>>,
}

impl PipelineHandle {
    pub fn stop(&self) {
        self.stop.store(true, std::sync::atomic::Ordering::Relaxed);
        if let Some(mut c) = self.child.lock().unwrap().take() {
            let _ = c.kill();
        }
    }
}

pub struct StreamState {
    pub launch: Mutex<Option<Arc<LaunchSession>>>,
    pub announce: Mutex<Option<AnnounceConfig>>,
    pub video_peer: Mutex<Option<SocketAddr>>,
    pub video_sock: Arc<UdpSocket>,
    pub audio_sock: Arc<UdpSocket>,
    pub pipeline: Mutex<Option<PipelineHandle>>,
    /// appid текущей сессии (serverinfo currentgame/state).
    pub current_game: AtomicI32,
    /// Wayland-output для захвата ("auto" или имя), NX_OUTPUT.
    pub output: String,
    /// Последняя активность клиента (control/аудио/SS_PING): watchdog останавливает
    /// конвейер при уходе клиента (TEARDOWN может не прийти — экран уснул/сеть упала).
    pub last_activity: Mutex<Instant>,
    /// uinput-инъекция ввода клиента (мышь/клавиатура): лениво на первом 0x0206.
    /// Инжектор создаётся в control-потоке (проверенный контекст), живёт и
    /// парсит в ОТДЕЛЬНОМ потоке-воркере: control-цикл только кладёт пакет в
    /// очередь (mpsc) и сразу крутится дальше. Раньше parse() выполнялся прямо
    /// в ENet-потоке, а внутри есть синхронные слипы (тап 30 мс, синтез клика
    /// 150 мс) — они тормозили приём, RELIABLE-канал копил ретрансмиссии,
    /// и ввод ощутимо отставал от пальца.
    pub input: Mutex<Option<std::sync::mpsc::Sender<Vec<u8>>>>,
}

impl StreamState {
    pub fn touch(&self) {
        *self.last_activity.lock().unwrap() = Instant::now();
    }

    /// Биндим свои UDP-порты сразу: занятые порты = фейл старта, как у Sunshine.
    pub fn new(output: &str) -> Result<Arc<Self>> {
        let video_sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, VIDEO_PORT))
            .with_context(|| format!("bind UDP {}", VIDEO_PORT))?;
        let audio_sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, AUDIO_PORT))
            .with_context(|| format!("bind UDP {}", AUDIO_PORT))?;
        video_sock
            .set_read_timeout(Some(std::time::Duration::from_millis(500)))
            .ok();
        audio_sock
            .set_read_timeout(Some(std::time::Duration::from_millis(500)))
            .ok();
        Ok(Arc::new(Self {
            launch: Mutex::new(None),
            announce: Mutex::new(None),
            video_peer: Mutex::new(None),
            video_sock: Arc::new(video_sock),
            audio_sock: Arc::new(audio_sock),
            pipeline: Mutex::new(None),
            current_game: AtomicI32::new(0),
            output: output.to_string(),
            last_activity: Mutex::new(Instant::now()),
            input: Mutex::new(None),
        }))
    }
}

/// Старт всех stream-сервисов: RTSP TCP :49010, control ENet :49001,
/// peer-learning (SS_PING) :49000, audio-drain :49002.
pub fn start(st: &Arc<StreamState>) -> Result<()> {
    rtsp::start(st.clone())?;
    control::start(st.clone());
    pipeline::start_peer_learning(st.clone());
    pipeline::start_audio_drain(st.clone());
    // Watchdog: клиент ушёл без TEARDOWN (экран уснул, сеть упала) → конвейер
    // останавливаем сами. Вчера такое превратилось в 10-часовой encode вхолостую,
    // который в итоге ронял Hyprland в его же screenshare-коде.
    {
        let st = st.clone();
        std::thread::spawn(move || loop {
            std::thread::sleep(Duration::from_secs(5));
            let idle = st.last_activity.lock().unwrap().elapsed();
            let running = st.pipeline.lock().unwrap().is_some();
            if running && idle > Duration::from_secs(15) {
                println!("stream: клиент молчит >15s (нет пингов/control/аудио) — автостоп конвейера");
                stop_pipeline(&st);
            }
        });
    }
    println!(
        "stream: RTSP :{} + video :{} + control :{} + audio :{}",
        RTSP_PORT, VIDEO_PORT, CONTROL_PORT, AUDIO_PORT
    );
    Ok(())
}

/// Остановка живого конвейера (вызывается из /cancel и RTSP TEARDOWN);
/// current_game обнуляется здесь — стоп всегда значит конец сессии.
pub fn stop_pipeline(st: &StreamState) {
    let old = st.pipeline.lock().unwrap().take();
    *st.announce.lock().unwrap() = None;
    st.current_game.store(0, std::sync::atomic::Ordering::Relaxed);
    if let Some(h) = old {
        h.stop();
        println!("stream: pipeline остановлен");
    }
}

/// Локальный IP, которым сервер виден клиенту: Host-заголовок запроса,
/// иначе UDP-trick (connect к клиенту и local_addr).
pub fn local_ip_for(remote: SocketAddr, host_header: Option<&str>) -> std::net::IpAddr {
    if let Some(h) = host_header {
        let host = h.split(',').next().unwrap_or("").trim();
        let s = if host.starts_with('[') {
            host[1..].split(']').next().unwrap_or("")
        } else if host.matches(':').count() == 1 {
            host.split(':').next().unwrap_or(host)
        } else {
            host
        };
        if let Ok(ip) = s.parse() {
            return ip;
        }
    }
    if let Ok(sock) = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0)) {
        if sock.connect(remote).is_ok() {
            if let Ok(la) = sock.local_addr() {
                return la.ip();
            }
        }
    }
    std::net::IpAddr::from(Ipv4Addr::LOCALHOST)
}
