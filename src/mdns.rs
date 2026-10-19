//! mDNS/DNS-SD ответчик: объявляем сервер как `_nvstream._tcp.local` (GameStream).
//!
//! Fix E (корневая причина «Offline»): Moonlight при старте шлёт multicast-запросы
//! PTR/SRV/TXT `_nvstream._tcp.local` и, не получив ответа, помечает сервер Offline.
//! Паринг, записи в базе клиента и mёртвые адреса тут ни при чём. На хосте
//! avahi-daemon неактивен, а systemd-resolved держит 0.0.0.0:5353 без `_nvstream`,
//! поэтому обнаружение раньше держалось на временном /tmp/mdns_answer.py.
//! Прототип проверен живым прогоном (Moonlight сразу находил сервер) — здесь та же
//! логика внутри процесса сервера, чтобы убийство скрипта ничего не ломало.
//!
//! Порт 5353 занят resolved — поднимаемся с SO_REUSEADDR + SO_REUSEPORT, как прототип.

use anyhow::{Context, Result};
use socket2::{Domain, Protocol, Socket, SockAddr, Type};
use std::net::{Ipv4Addr, SocketAddr, UdpSocket};

/// mDNS-группа и порт.
const MDNS_GROUP: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 251);
const MDNS_PORT: u16 = 5353;

/// Типы записей DNS.
const T_A: u16 = 1;
const T_PTR: u16 = 12;
const T_TXT: u16 = 16;
const T_SRV: u16 = 33;
const T_ANY: u16 = 255;

/// GameStream-сервис (как у Sunshine).
const SVC_TYPE: &str = "_nvstream._tcp.local";

/// Коды секций/флагов DNS.
const QR_RESPONSE: u16 = 0x8000;
const AA_AUTHORITY: u16 = 0x0400;
/// Cache-flush: уникальные записи (SRV/TXT/A) инвалидируют прошлый RR.
const CLASS_FLUSH: u16 = 0x8001;
const CLASS_IN: u16 = 0x0001;
/// QU-бит в классе вопроса: клиент просит unicast-ответ.
const CLASS_QU: u16 = 0x8000;

/// Поднять mDNS-ответчик отдельными потоками на весь рантайм сервера.
///
/// `hostname` — имя сервера из `--name` (инстанс `hostname._nvstream._tcp.local`),
/// `port` — HTTP-порт NVHTTP (48989), его слушает Moonlight после обнаружения.
pub fn spawn(hostname: &str, port: u16) -> Result<()> {
    let recv_sock = bind().context("mDNS bind 5353")?;

    // Вступаем в группу на всех интерфейсах: запрос может прийти с любого
    // (RNDIS к планшету, Ethernet и т.п.). По ОДНОМУ адресу на интерфейс:
    // на wlan0 может быть secondary-адрес (.181 при основном .180) — второй
    // join по тому же интерфейсу падает с EADDRINUSE (наблюдалось 2026-10-19).
    let mut joined: Vec<String> = Vec::new();
    for (dev, addr, _mac) in crate::nvhttp::ifaddrs() {
        let std::net::IpAddr::V4(ip) = addr else { continue };
        if ip.is_loopback() || joined.contains(&dev) {
            continue;
        }
        recv_sock
            .join_multicast_v4(&MDNS_GROUP, &ip)
            .with_context(|| format!("mDNS join {} on {} ({})", MDNS_GROUP, ip, dev))?;
        joined.push(dev);
    }
    let ips: Vec<Ipv4Addr> = crate::nvhttp::ifaddrs()
        .into_iter()
        .filter_map(|(_dev, addr, _mac)| match addr {
            std::net::IpAddr::V4(a4) if !a4.is_loopback() => Some(a4),
            _ => None,
        })
        .collect();
    recv_sock.set_multicast_loop_v4(true).ok();
    recv_sock.set_multicast_ttl_v4(255).ok();

    println!(
        "mDNS: отвечаю на {}.{} -> {}:{} (интерфейсов: {})",
        hostname,
        SVC_TYPE,
        ips.first()
            .map(|i| i.to_string())
            .unwrap_or_else(|| "?".into()),
        port,
        ips.len()
    );

    let h_recv = hostname.to_string();
    // Дуп того же fd: приём — через std-сокет (удобный API), отправка — через
    // socket2::Socket (единственный способ выставить IP_MULTICAST_IF перед каждым
    // ответом). Оба fd указывают на один сокет, поэтому membership/REUSEPORT общие.
    let send_sock = recv_sock.try_clone().context("mDNS clone send sock")?;
    let recv_sock: UdpSocket = recv_sock.into();
    std::thread::Builder::new()
        .name("mdns-recv".into())
        .spawn(move || recv_loop(recv_sock, send_sock, h_recv, port))
        .context("старт mdns-recv")?;

    // Несolicited-анонсы: клиент мог стартануть до того, как мы поднялись,
    // и держать устаревший кэш — шлём объявление сразу и периодически.
    let h_ann = hostname.to_string();
    std::thread::Builder::new()
        .name("mdns-announce".into())
        .spawn(move || announce_loop(h_ann, port))
        .context("старт mdns-announce")?;

    Ok(())
}

/// Сокет 5353 с REUSEADDR/REUSEPORT — иначе bind не пройдёт мимо systemd-resolved.
/// Остаёмся в `socket2::Socket`: только у него есть `set_multicast_if_v4`
/// (нужен, чтобы ответ уходил ровно тем интерфейсом, с которого пришёл запрос).
fn bind() -> Result<Socket> {
    let sock = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))
        .context("mDNS socket")?;
    sock.set_reuse_address(true).context("SO_REUSEADDR")?;
    sock.set_reuse_port(true).context("SO_REUSEPORT")?;
    sock.bind(&SocketAddr::from((Ipv4Addr::UNSPECIFIED, MDNS_PORT)).into())
        .context("bind 0.0.0.0:5353")?;
    Ok(sock)
}

/// Цикл приёма: на каждый запрос собираем ответ с адресом того интерфейса,
/// с которого он пришёл (иначе уйдём мультикастом не туда).
fn recv_loop(recv: UdpSocket, send: Socket, hostname: String, port: u16) {
    let mut buf = [0u8; 4096];
    loop {
        let (n, src) = match recv.recv_from(&mut buf) {
            Ok(v) => v,
            Err(_) => continue, // EINTR/таймаут — не фатально
        };
        let Some((questions, want_unicast)) = parse_questions(&buf[..n]) else {
            continue;
        };
        if questions.is_empty() {
            continue;
        }
        // Адрес ответа = наш IPv4 в той же /24, что и запросивший (тот же сегмент
        // и тот же интерфейс). Иначе — первый не-петлевой адрес.
        let reply_ip = same_subnet_ip(src.ip())
            .or_else(|| local_ipv4s().into_iter().find(|i| !i.is_loopback()));
        let Some(reply_ip) = reply_ip else { continue };

        let resp = build_response(&questions, &hostname, port, reply_ip);
        if resp.is_empty() {
            continue;
        }

        if want_unicast {
            let _ = send.send_to(&resp, &SockAddr::from(src));
            continue;
        }
        // Мультикаст-ответ: жёстко указываем исходящий интерфейс, иначе ядро
        // выберет дефолтный маршрут и планшет ответ не увидит.
        let _ = send.set_multicast_if_v4(&reply_ip);
        let _ = send.send_to(&resp, &SockAddr::from(SocketAddr::from((MDNS_GROUP, MDNS_PORT))));
    }
}

/// Периодические unsolicited-объявления со всех не-петлевых адресов.
fn announce_loop(hostname: String, port: u16) {
    // Первое объявление — сразу (клиент мог только что стартануть), дальше раз в 60с.
    let mut tick: u64 = 0;
    loop {
        if tick == 0 || tick % 60 == 0 {
            for ip in local_ipv4s() {
                if ip.is_loopback() {
                    continue;
                }
                if let Ok(sock) = bind() {
                    let _ = sock.set_multicast_if_v4(&ip);
                    let pkt = announcement(&hostname, port, ip);
                    let _ = sock.send_to(&pkt, &SockAddr::from(SocketAddr::from((MDNS_GROUP, MDNS_PORT))));
                }
            }
        }
        tick += 1;
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

/// (вопросы, нужен ли unicast). `None` — это ответ, не запрос, либо мусор.
fn parse_questions(pkt: &[u8]) -> Option<(Vec<(String, u16, bool)>, bool)> {
    if pkt.len() < 12 {
        return None;
    }
    let flags = u16::from_be_bytes([pkt[2], pkt[3]]);
    if flags & QR_RESPONSE != 0 {
        return None; // это ответ — отвечать не на что
    }
    let qdcount = u16::from_be_bytes([pkt[4], pkt[5]]) as usize;
    let mut off = 12;
    let mut out = Vec::new();
    let mut any_qu = false;
    for _ in 0..qdcount {
        let (name, next) = decode_name(pkt, off)?;
        off = next;
        if off + 4 > pkt.len() {
            return None;
        }
        let qtype = u16::from_be_bytes([pkt[off], pkt[off + 1]]);
        let qclass = u16::from_be_bytes([pkt[off + 2], pkt[off + 3]]);
        off += 4;
        let qu = qclass & CLASS_QU != 0;
        any_qu |= qu;
        out.push((normalize(&name), qtype, qu));
    }
    if out.is_empty() {
        return None;
    }
    Some((out, any_qu))
}

/// Ответ на вопросы клиента: PTR/SRV/TXT/A для `_nvstream._tcp.local`.
fn build_response(questions: &[(String, u16, bool)], hostname: &str, port: u16, ip: Ipv4Addr) -> Vec<u8> {
    let instance = format!("{}.{}", hostname, SVC_TYPE);
    let target = format!("{}.local", normalize(hostname));

    let mut answers: Vec<u8> = Vec::new();
    for (name, qtype, _) in questions {
        let name = name.as_str();
        let qtype = *qtype;
        // Клиент спрашивает и об ИНСТАНСЕ (archpizdetc._nvstream._tcp.local),
        // и о САМОМ СЕРВИСЕ (_nvstream._tcp.local) — это browsing: на SRV/TXT
        // сервиса отвечаем записями инстанса (так делал python-прототип).
        let service = name == normalize(SVC_TYPE)
            || name == "_nvstream._tcp"
            || name.ends_with("._nvstream._tcp.local");
        let is_instance = name == normalize(&instance);

        if service || is_instance {
            if qtype == T_PTR || qtype == T_ANY {
                // PTR регистрируем только на имя сервиса — инстанс PTR не имеет.
                if service {
                    answers.extend(rr(&normalize(SVC_TYPE), T_PTR, &encode_name(&instance), 4500, false));
                }
            }
            if qtype == T_SRV || qtype == T_ANY {
                answers.extend(rr(&instance, T_SRV, &srv_rdata(port, &target), 120, true));
            }
            if qtype == T_TXT || qtype == T_ANY {
                answers.extend(rr(&instance, T_TXT, &txt_rdata(), 4500, true));
            }
            if service && (qtype == T_A || qtype == T_ANY) {
                answers.extend(rr(&target, T_A, &ip.octets(), 120, true));
            }
        } else if name == target && (qtype == T_A || qtype == T_ANY) {
            answers.extend(rr(&target, T_A, &ip.octets(), 120, true));
        }
    }

    if answers.is_empty() {
        return Vec::new();
    }
    let mut pkt = Vec::with_capacity(12 + 32 + answers.len());
    // QR=1, AA=1
    pkt.extend_from_slice(&0u16.to_be_bytes());
    pkt.extend_from_slice(&(QR_RESPONSE | AA_AUTHORITY).to_be_bytes());
    pkt.extend_from_slice(&(questions.len() as u16).to_be_bytes()); // QD
    // Все пакеты ответов кладём в AN — так делал проверенный прототип.
    let ancount = count_records(&answers);
    pkt.extend_from_slice(&(ancount as u16).to_be_bytes());
    pkt.extend_from_slice(&0u16.to_be_bytes()); // NS
    pkt.extend_from_slice(&0u16.to_be_bytes()); // AR
    for (name, qtype, _) in questions {
        pkt.extend(encode_name(name));
        pkt.extend_from_slice(&qtype.to_be_bytes());
        pkt.extend_from_slice(&CLASS_IN.to_be_bytes());
    }
    pkt.extend(answers);
    pkt
}

/// Unsolicited-объявление (без вопросов) — шлём сразу после старта и по таймеру.
fn announcement(hostname: &str, port: u16, ip: Ipv4Addr) -> Vec<u8> {
    let instance = format!("{}.{}", hostname, SVC_TYPE);
    let target = format!("{}.local", normalize(hostname));
    let mut answers = Vec::new();
    answers.extend(rr(&normalize(SVC_TYPE), T_PTR, &encode_name(&instance), 4500, false));
    answers.extend(rr(&instance, T_SRV, &srv_rdata(port, &target), 120, true));
    answers.extend(rr(&instance, T_TXT, &txt_rdata(), 4500, true));
    answers.extend(rr(&target, T_A, &ip.octets(), 120, true));

    let mut pkt = Vec::with_capacity(12 + answers.len());
    pkt.extend_from_slice(&0u16.to_be_bytes());
    pkt.extend_from_slice(&(QR_RESPONSE | AA_AUTHORITY).to_be_bytes());
    pkt.extend_from_slice(&0u16.to_be_bytes()); // QD
    pkt.extend_from_slice(&(count_records(&answers) as u16).to_be_bytes());
    pkt.extend_from_slice(&0u16.to_be_bytes());
    pkt.extend_from_slice(&0u16.to_be_bytes());
    pkt.extend(answers);
    pkt
}

/// RDATA SRV: priority, weight, port, target.
fn srv_rdata(port: u16, target: &str) -> Vec<u8> {
    let mut v = Vec::with_capacity(6 + target.len() + 2);
    v.extend_from_slice(&0u16.to_be_bytes());
    v.extend_from_slice(&0u16.to_be_bytes());
    v.extend_from_slice(&port.to_be_bytes());
    v.extend(encode_name(target));
    v
}

/// TXT как у прототипа: `version=1.9`.
fn txt_rdata() -> Vec<u8> {
    let kv = "version=1.9";
    let mut v = Vec::with_capacity(kv.len() + 1);
    v.push(kv.len() as u8);
    v.extend_from_slice(kv.as_bytes());
    v
}

/// Один Resource Record.
fn rr(name: &str, rtype: u16, rdata: &[u8], ttl: u32, flush: bool) -> Vec<u8> {
    let class = if flush { CLASS_FLUSH } else { CLASS_IN };
    let mut out = encode_name(name);
    out.extend_from_slice(&rtype.to_be_bytes());
    out.extend_from_slice(&class.to_be_bytes());
    out.extend_from_slice(&ttl.to_be_bytes());
    out.extend_from_slice(&(rdata.len() as u16).to_be_bytes());
    out.extend_from_slice(rdata);
    out
}

/// Сколько RR внутри готового блока ответов (для ANCOUNT).
fn count_records(block: &[u8]) -> usize {
    let mut n = 0;
    let mut off = 0;
    while off < block.len() {
        let Some((_, next)) = decode_name(block, off) else { break };
        off = next + 10; // type(2) class(2) ttl(4) rdlength(2)
        if off > block.len() {
            break;
        }
        let rdlen = u16::from_be_bytes([block[off - 2], block[off - 1]]) as usize;
        off += rdlen;
        n += 1;
    }
    n
}

/// DNS-имя → байты (лейблы + нуль).
fn encode_name(name: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(name.len() + 2);
    for label in normalize(name).split('.') {
        if label.is_empty() {
            continue;
        }
        let b = label.as_bytes();
        out.push(b.len() as u8);
        out.extend_from_slice(b);
    }
    out.push(0);
    out
}

/// Разбор имени с поддержкой compression-указателей (0xC0).
/// Возвращает (имя, смещение сразу за именем в ПОТОКЕ, где читали).
fn decode_name(buf: &[u8], start: usize) -> Option<(String, usize)> {
    let mut labels: Vec<String> = Vec::new();
    let mut off = start;
    let mut end = start; // смещение после имени в исходном потоке
    let mut jumped = false;
    let mut hops = 0usize;

    loop {
        if off >= buf.len() {
            return None;
        }
        let len = buf[off];
        if len == 0 {
            if !jumped {
                end = off + 1;
            }
            break;
        }
        if len & 0xC0 == 0xC0 {
            if off + 1 >= buf.len() {
                return None;
            }
            let ptr = (((len as usize) & 0x3F) << 8) | buf[off + 1] as usize;
            if !jumped {
                end = off + 2;
                jumped = true;
            }
            off = ptr;
            hops += 1;
            if hops > 32 {
                return None; // петля указателей
            }
            continue;
        }
        let l = len as usize;
        if off + 1 + l > buf.len() {
            return None;
        }
        labels.push(String::from_utf8_lossy(&buf[off + 1..off + 1 + l]).into_owned());
        off += 1 + l;
        if !jumped {
            end = off;
        }
    }
    Some((labels.join("."), end))
}

/// Привести имя к нижнему регистру без конечной точки — так сравниваем имена.
fn normalize(name: &str) -> String {
    name.trim_end_matches('.').to_ascii_lowercase()
}

/// Наш IPv4 в той же /24, что и `peer` (тот же сегмент → тот же интерфейс).
fn same_subnet_ip(peer: std::net::IpAddr) -> Option<Ipv4Addr> {
    let std::net::IpAddr::V4(p4) = peer else { return None };
    crate::nvhttp::ifaddrs().into_iter().find_map(|(_dev, addr, _mac)| {
        let std::net::IpAddr::V4(a4) = addr else { return None };
        (a4.octets()[..3] == p4.octets()[..3] && !a4.is_loopback()).then_some(a4)
    })
}

/// Все не-петлевые IPv4 хоста (для вступления в группу и анонсов).
fn local_ipv4s() -> Vec<Ipv4Addr> {
    crate::nvhttp::ifaddrs()
        .into_iter()
        .filter_map(|(_dev, addr, _mac)| match addr {
            std::net::IpAddr::V4(a4) if !a4.is_loopback() => Some(a4),
            _ => None,
        })
        .collect()
}
