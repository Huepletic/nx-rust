//! nvhttp: GameStream NVHTTP-сервер (HTTPS + mTLS), Milestone 2.
//! Реализует /serverinfo, /applist, /pair (4 фазы), /launch, /resume, /cancel, /poll.
//! Спека: docs/gamestream-spec.md (по коду Sunshine).

pub mod pairing;

use anyhow::{Context, Result};
use axum::extract::{ConnectInfo, Query, State};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use serde::Deserialize;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::{Arc, Mutex};

use self::pairing::PairingState;

/// Общее состояние сервера.
pub struct ServerState {
    pub hostname: String,
    pub unique_id: String,
    pub clients: Mutex<Vec<PairedClient>>,
    pub pairing: Mutex<HashMap<String, PairingState>>, // uniqueid -> state
    /// PIN: фиксированный (--pin) либо динамический из /tmp/nx-pin (с экрана планшета).
    /// Как Sunshine: getservercert блокируется, пока пользователь введёт PIN (nvhttp.cpp:750).
    pub pin: Mutex<String>,
    pub server_cert_pem: String,
    pub server_key_pem: String,
    pub http_port: u16,
    pub https_port: u16,
    /// Stream-состояние (RTSP/RTP/control), Milestone 3.
    pub stream: Arc<crate::stream::StreamState>,
}

#[derive(Clone)]
pub struct PairedClient {
    pub name: String,
    pub uuid: String,
    pub uniqueid: String,
    pub cert_pem: String,
}

/// Запуск NVHTTP: HTTPS (по умолчанию 48984) + HTTP (по умолчанию 48989).
/// Стандартные 47984/47989 заняты референсным Sunshine — не трогаем.
pub async fn serve(state: Arc<ServerState>, http_port: u16, https_port: u16) -> Result<()> {
    // У Sunshine серверinfo разный по схемам (PairStatus=1 только по HTTPS) —
    // поэтому два роутера, различающиеся хендлером /serverinfo (nvhttp.cpp:1243).
    let app = build_router(state.clone(), false);

    // HTTPS — сам роутер + acceptor с mTLS
    let https_addr = SocketAddr::from(([0, 0, 0, 0], https_port));
    let config = axum_server::tls_rustls::RustlsConfig::from_pem(
        state.server_cert_pem.clone().into_bytes(),
        state.server_key_pem.clone().into_bytes(),
    )
    .await
    .context("rustls config")?;

    let app2 = build_router(state.clone(), true);
    tokio::spawn(async move {
        axum_server::bind_rustls(https_addr, config)
            .serve(app2.into_make_service_with_connect_info::<SocketAddr>())
            .await
            .expect("HTTPS server failed");
    });

    // HTTP — тот же роутер (Moonlight дергает discovery по HTTP)
    let http_addr = SocketAddr::from(([0, 0, 0, 0], http_port));
    let listener = tokio::net::TcpListener::bind(http_addr)
        .await
        .with_context(|| format!("bind HTTP {}", http_port))?;
    println!(
        "nvhttp: HTTP :{} + HTTPS :{} (PIN={} , ждём /tmp/nx-pin)",
        http_port, https_port, state.pin.lock().unwrap()
    );
    axum::serve(listener, app.into_make_service_with_connect_info::<SocketAddr>())
        .await
        .context("HTTP server")?;
    Ok(())
}

/// Роутер одного «интерфейса» (HTTP или HTTPS): https=1 → serverinfo с PairStatus=1.
fn build_router(st: Arc<ServerState>, https: bool) -> Router {
    let si: axum::routing::MethodRouter<Arc<ServerState>> = if https {
        get(serverinfo_https)
    } else {
        get(serverinfo_http)
    };
    Router::new()
        .route("/serverinfo", si)
        .route("/applist", get(applist))
        .route("/pair", get(pair))
        .route("/launch", get(launch))
        .route("/resume", get(resume))
        .route("/cancel", get(cancel))
        .route("/poll", get(poll))
        .route("/unpair", get(unpair))
        .layer(axum::middleware::from_fn(reqlog))
        .with_state(st)
}

fn tnow() -> u128 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

/// Лог каждого запроса: путь, имена query-ключей, статус ответа (диагностика клиента).
async fn reqlog(req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let q = req.uri().query().unwrap_or("").to_string();
    let keys: Vec<String> = q
        .split('&')
        .filter_map(|kv| kv.split('=').next().map(str::to_string))
        .collect();
    let method = req.method().clone();
    let path = req.uri().path().to_string();
    let resp = next.run(req).await;
    println!("[{}] {} {} keys=[{}] -> {}", tnow(), method, path, keys.join(","), resp.status());
    resp
}

// === ответы ===

fn xml_ok(body: String) -> Response {
    let full = format!(r#"<?xml version="1.0" encoding="utf-8"?><root status_code="200">{}</root>"#, body);
    (
        [(axum::http::header::CONTENT_TYPE, "text/xml")],
        full,
    )
        .into_response()
}

fn xml_err(code: u32, msg: &str) -> Response {
    let full = format!(
        r#"<?xml version="1.0" encoding="utf-8"?><root status_code="{}" status_message="{}"/>"#,
        code, msg
    );
    (
        [(axum::http::header::CONTENT_TYPE, "text/xml")],
        full,
    )
        .into_response()
}

/// Версия как у эталонного Sunshine: `7.1.431.-1` (4 части, major≥7 →
/// Moonlight использует SHA-256 для паринга, PairingManager.java:188).
const APP_VERSION: &str = "7.1.431.-1";

fn serverinfo_common(st: &ServerState, https: bool, client_ip: std::net::IpAddr) -> Response {
    // Sunshine: PairStatus=1 только по HTTPS — клиент переходит на HTTPS
    // с закреплённым сертификатом только после успешного паринга (nvhttp.cpp:1243).
    let pair_status = if https { 1 } else { 0 };
    // LocalIP и MAC должны указывать на СЕРВЕР (тот же интерфейс, куда пришёл клиент),
    // иначе клиент запоминает собственный адрес как адрес сервера и «теряет» хост при
    // смене подсети. Раньше сюда передавался адрес клиента — см. server_local_ip().
    let local_ip = server_local_ip(client_ip);
    let mac = if https {
        local_mac_for(client_ip)
    } else {
        "00:00:00:00:00:00".to_string()
    };
    // H.264-only (SCM_H264): клиент без этого может попросить HEVC, а encoder наш h264_vaapi.
    let current_game = st.stream.current_game.load(std::sync::atomic::Ordering::Relaxed);
    let state_str = if current_game != 0 {
        "SUNSHINE_SERVER_BUSY"
    } else {
        "SUNSHINE_SERVER_FREE"
    };
    let body = format!(
        "<hostname>{h}</hostname>\
         <appversion>{av}</appversion>\
         <GfeVersion>3.23.0.74</GfeVersion>\
         <uniqueid>{uid}</uniqueid>\
         <HttpsPort>{hsp}</HttpsPort>\
         <ExternalPort>{exp}</ExternalPort>\
         <MaxLumaPixelsHEVC>0</MaxLumaPixelsHEVC>\
         <mac>{mac}</mac>\
         <LocalIP>{lip}</LocalIP>\
         <ServerCodecModeSupport>1</ServerCodecModeSupport>\
         <PairStatus>{ps}</PairStatus>\
         <currentgame>{cg}</currentgame>\
         <state>{ss}</state>",
        h = st.hostname,
        av = APP_VERSION,
        uid = st.unique_id,
        ps = pair_status,
        hsp = st.https_port,
        exp = st.http_port,
        mac = mac,
        lip = local_ip,
        cg = current_game,
        ss = state_str
    );
    xml_ok(body)
}

async fn serverinfo_http(
    State(st): State<Arc<ServerState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Response {
    serverinfo_common(&st, false, addr.ip())
}

async fn serverinfo_https(
    State(st): State<Arc<ServerState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
) -> Response {
    serverinfo_common(&st, true, addr.ip())
}

/// MAC **того** интерфейса, на который пришёл клиент (как platf::get_mac_address у Sunshine).
///
/// Раньше брался MAC дефолтного интерфейса — на хосте с несколькими сетями (Wi-Fi + USB-RNDIS)
/// это отдавало MAC wlan0 клиенту, сидящему в RNDIS-подсети: клиент сравнивал MAC со своим
/// и решал, что сервер «в другой сети» → Moonlight показывал Offline при живом сервере.
/// Если адрес клиента не нашли — тихий fallback на прежнее поведение (дефолтный интерфейс).
fn local_mac_for(client: std::net::IpAddr) -> String {
    let placeholder = "00:00:00:00:00:00".to_string();
    let std::net::IpAddr::V4(c4) = client else {
        return default_iface_mac(placeholder);
    };
    // Ищем не равенство адресов, а интерфейс в той же /24, что и клиент:
    // клиент сидит на .89, сервер слушает на .1 — точного совпадения не бывает.
    for (dev, addr, mac) in ifaddrs() {
        let std::net::IpAddr::V4(a4) = addr else { continue };
        if dev != "lo" && !mac.is_empty() && a4.octets()[..3] == c4.octets()[..3] {
            return mac;
        }
    }
    default_iface_mac(placeholder)
}

/// MAC дефолтного интерфейса (маршрут 0.0.0.0/0) — прежнее поведение как fallback.
fn default_iface_mac(placeholder: String) -> String {
    let Ok(route) = std::fs::read_to_string("/proc/net/route") else {
        return placeholder;
    };
    for line in route.lines().skip(1) {
        let mut f = line.split_whitespace();
        let (dev, dest) = (f.next().unwrap_or(""), f.next().unwrap_or(""));
        if dest == "00000000" {
            if let Ok(mac) = std::fs::read_to_string(format!("/sys/class/net/{}/address", dev)) {
                let mac = mac.trim().to_lowercase();
                if mac.len() == 17 {
                    return mac;
                }
            }
        }
    }
    placeholder
}

/// IPv4-адрес **сервера** на том интерфейсе, в подсети которого сидит клиент.
///
/// Клиент (Moonlight) по полю LocalIP определяет, «своя» ли это сеть, и запоминает адрес
/// для будущих подключений. Раньше сюда передавался адрес клиента (ConnectInfo.addr.ip()),
/// и клиент записывал в базу собственный IP как адрес сервера — при смене подсети RNDIS
/// (192.168.242.x → 192.168.178.x) оба адреса клиента становились мёртвыми и связь не восстанавливалась.
///
/// Адреса клиента и сервера лежат в одной /24, но не равны (клиент .89, сервер .1),
/// поэтому ищем не равенство, а совпадение подсети (/24 для IPv4).
fn server_local_ip(client_ip: std::net::IpAddr) -> std::net::IpAddr {
    use std::net::Ipv4Addr;
    let std::net::IpAddr::V4(c4) = client_ip else {
        return client_ip; // IPv6 не поддерживаем — прежнее поведение
    };
    for (_dev, addr, _mac) in ifaddrs() {
        let std::net::IpAddr::V4(a4) = addr else { continue };
        // /24: первые три октета должны совпадать, петля (lo) пропускаем
        if a4.octets()[..3] == c4.octets()[..3] && !a4.is_loopback() {
            return addr;
        }
    }
    client_ip // fallback: прежнее поведение
}

/// Список (интерфейс, IPv4, MAC) через libc::getifaddrs — без новых зависимостей.
/// `pub(crate)` — его же читает mDNS-ответчик (выбор интерфейса для анонса).
pub(crate) fn ifaddrs() -> Vec<(String, std::net::IpAddr, String)> {
    use std::net::{Ipv4Addr, SocketAddrV4};
    let mut out = Vec::new();
    unsafe {
        let mut list: *mut libc::ifaddrs = std::ptr::null_mut();
        if libc::getifaddrs(&mut list) != 0 {
            return out;
        }
        let mut cur = list;
        while !cur.is_null() {
            let ifa = &*cur;
            if !ifa.ifa_addr.is_null() && (*ifa.ifa_addr).sa_family == libc::AF_INET as u16 {
                let sin = &*(ifa.ifa_addr as *const libc::sockaddr_in);
                let ip = std::net::IpAddr::V4(Ipv4Addr::from(u32::from_be(sin.sin_addr.s_addr)));
                let name = if ifa.ifa_name.is_null() {
                    String::new()
                } else {
                    std::ffi::CStr::from_ptr(ifa.ifa_name).to_string_lossy().into_owned()
                };
                let mac = std::fs::read_to_string(format!("/sys/class/net/{}/address", name))
                    .map(|m| m.trim().to_lowercase())
                    .unwrap_or_default();
                out.push((name, ip, mac));
            }
            cur = ifa.ifa_next;
        }
        libc::freeifaddrs(list);
    }
    let _ = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0); // keep import used on all targets
    out
}

async fn applist(State(_st): State<Arc<ServerState>>) -> Response {
    // одно виртуальное приложение — «Desktop»
    let body = "<App><IsHdrSupported>0</IsHdrSupported><AppTitle>Desktop</AppTitle><ID>1</ID></App>";
    xml_ok(body.to_string())
}

#[derive(Deserialize)]
pub struct PairParams {
    #[serde(default)]
    uniqueid: String,
    #[serde(default)]
    phrase: String,
    #[serde(default)]
    clientcert: String,
    #[serde(default)]
    salt: String,
    #[serde(default)]
    devicename: String,
    #[serde(default)]
    clientchallenge: String,
    #[serde(default)]
    serverchallengeresp: String,
    #[serde(default)]
    clientpairingsecret: String,
}

async fn pair(
    State(st): State<Arc<ServerState>>,
    Query(p): Query<PairParams>,
) -> Response {
    // Фаза 1: как Sunshine — держим запрос, пока PIN не придёт (/tmp/nx-pin).
    // Moonlight шлёт getservercert без read-timeout именно под это (PairingManager.java:206).
    if p.phrase == "getservercert" {
        let pin = wait_for_pin(&st).await;
        *st.pin.lock().unwrap() = pin;
    }
    let which = if !p.clientchallenge.is_empty() {
        "clientchallenge"
    } else if !p.serverchallengeresp.is_empty() {
        "serverchallengeresp"
    } else if !p.clientpairingsecret.is_empty() {
        "clientpairingsecret"
    } else {
        &p.phrase
    };
    println!(
        "pair: phase={} uid={} cert_len={} salt_len={}",
        which,
        p.uniqueid,
        p.clientcert.len(),
        p.salt.len()
    );
    match pairing::handle(&st, &p.phrase, &p) {
        Ok(xml) => {
            println!("pair: phase={} -> 200", which);
            (
                [(axum::http::header::CONTENT_TYPE, "text/xml")],
                xml,
            )
                .into_response()
        }
        Err((code, msg)) => {
            println!("pair: phase={} -> ERR {}: {}", which, code, msg);
            xml_err(code, &msg)
        }
    }
}

/// Ждём PIN из /tmp/nx-pin (до 120 с), иначе возвращаем текущий (--pin).
/// NX_PIN_WAIT=0 — не ждать (регресс-тесты без планшета).
async fn wait_for_pin(st: &ServerState) -> String {
    if std::env::var("NX_PIN_WAIT").ok().as_deref() == Some("0") {
        return st.pin.lock().unwrap().clone();
    }
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(120);
    loop {
        if let Ok(s) = tokio::fs::read_to_string("/tmp/nx-pin").await {
            let t = s.trim().to_string();
            if t.len() == 4 && t.bytes().all(|b| b.is_ascii_digit()) {
                println!("pairing: принят PIN из /tmp/nx-pin={}", t);
                return t;
            }
        }
        if tokio::time::Instant::now() >= deadline {
            println!("pairing: таймаут ожидания PIN — fallback на --pin");
            return st.pin.lock().unwrap().clone();
        }
        tokio::time::sleep(std::time::Duration::from_millis(250)).await;
    }
}

/// Разпарка: удаляем клиента по uniqueid (Moonlight дёргает /unpair при ошибках).
async fn unpair(
    State(st): State<Arc<ServerState>>,
    Query(p): Query<HashMap<String, String>>,
) -> Response {
    let uid = p.get("uniqueid").cloned().unwrap_or_default();
    st.clients.lock().unwrap().retain(|c| c.uniqueid != uid);
    st.pairing.lock().unwrap().remove(&uid);
    println!("unpair: uniqueid={}", uid);
    xml_ok(String::new())
}

#[derive(Deserialize)]
struct LaunchParams {
    #[serde(default)]
    rikey: String,
    #[serde(default)]
    rikeyid: String,
    #[serde(default)]
    appid: String,
    #[serde(default)]
    mode: String,
}

async fn launch(
    State(st): State<Arc<ServerState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: axum::http::HeaderMap,
    Query(p): Query<LaunchParams>,
) -> Response {
    use std::sync::atomic::Ordering;
    // mode: "WxHxFPS"
    let mut mode = p.mode.split('x');
    let parse = |s: &str, d: u32| s.parse().unwrap_or(d);
    let (w, h, fps) = (
        parse(mode.next().unwrap_or(""), 1920),
        parse(mode.next().unwrap_or(""), 1080),
        parse(mode.next().unwrap_or(""), 60),
    );
    let appid: i32 = p.appid.parse().unwrap_or(1);

    // av_ping_payload = hex(8 случайных байт) → 16 ASCII hex; control_connect_data = RAND u32.
    let (ping, ccd) = {
        use std::io::Read;
        let mut b = [0u8; 8];
        std::fs::File::open("/dev/urandom")
            .and_then(|mut f| f.read_exact(&mut b))
            .expect("/dev/urandom");
        let mut c = [0u8; 4];
        std::fs::File::open("/dev/urandom")
            .and_then(|mut f| f.read_exact(&mut c))
            .expect("/dev/urandom");
        (hex::encode(b), u32::from_le_bytes(c))
    };

    let sess = Arc::new(crate::stream::LaunchSession {
        appid,
        width: w,
        height: h,
        fps,
        rikey: p.rikey.clone(),
        rikeyid: p.rikeyid.clone(),
        av_ping_payload: ping,
        control_connect_data: ccd,
    });
    println!(
        "launch: appid={} mode={} ({}x{}@{}) rikey={} rikeyid={}",
        p.appid, p.mode, w, h, fps, p.rikey, p.rikeyid
    );
    *st.stream.launch.lock().unwrap() = Some(sess);
    st.stream.current_game.store(appid, Ordering::Relaxed);

    // sessionUrl0: rtsp://<наш IP>:<порт> (клиент берёт порт из последнего двоеточия)
    let host_hdr = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok());
    let ip = crate::stream::local_ip_for(addr, host_hdr);
    let body = format!(
        "<resume>0</resume><gamesession>1</gamesession><sessionUrl0>rtsp://{}:{}</sessionUrl0>",
        ip,
        crate::stream::RTSP_PORT
    );
    xml_ok(body)
}

async fn resume(State(_st): State<Arc<ServerState>>) -> Response {
    xml_err(400, "resume not yet implemented")
}

async fn cancel(State(st): State<Arc<ServerState>>) -> Response {
    crate::stream::stop_pipeline(&st.stream);
    xml_ok("<cancel>1</cancel>".to_string())
}

async fn poll(
    State(_st): State<Arc<ServerState>>,
    Query(_p): Query<HashMap<String, String>>,
) -> Response {
    xml_ok("<fsbumode>0</fsbumode>".to_string())
}

// === служебное ===

/// Самоподписанный RSA-2048 сертификат сервера (как GFE/Sunshine при первой генерации).
pub fn gen_selfsigned() -> Result<(String, String)> {
    use std::process::Command;
    let dir = std::env::temp_dir().join(format!("nxcert-{}", std::process::id()));
    std::fs::create_dir_all(&dir)?;
    let key = dir.join("key.pem");
    let crt = dir.join("cert.pem");
    let out = Command::new("openssl")
        .args([
            "req", "-x509", "-newkey", "rsa:2048", "-nodes",
            "-keyout", key.to_str().unwrap(),
            "-out", crt.to_str().unwrap(),
            "-days", "3650",
            "-subj", "/CN=nx-rust-server/O=NxRust",
        ])
        .output()
        .context("openssl req")?;
    if !out.status.success() {
        anyhow::bail!("openssl req: {}", String::from_utf8_lossy(&out.stderr));
    }
    let cert_pem = std::fs::read_to_string(&crt)?;
    let key_pem = std::fs::read_to_string(&key)?;
    std::fs::remove_dir_all(&dir).ok();
    Ok((cert_pem, key_pem))
}

/// Случайный uniqueid (32 hex), как у Sunshine.
pub fn gen_unique_id() -> String {
    use std::io::Read;
    let mut b = [0u8; 16];
    std::fs::File::open("/dev/urandom")
        .and_then(|mut f| f.read_exact(&mut b))
        .expect("/dev/urandom");
    hex::encode(b)
}
