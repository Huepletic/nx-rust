//! Пробник: что реально возвращает ifaddrs() и server_local_ip() для адреса клиента.
//! Запуск: cargo run --release --example ifaddrs_probe -- 192.0.2.89
fn main() {
    let client: std::net::IpAddr = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "192.0.2.89".into())
        .parse()
        .expect("IP");

    println!("=== ifaddrs() ===");
    for (dev, ip, mac) in nx_server_ifaddrs() {
        println!("  {dev:12} {ip:16} {mac}");
    }

    println!("=== server_local_ip({client}) ===");
    println!("  -> {}", server_local_ip(client));
}

// Копии целевых функций из src/nvhttp/mod.rs, чтобы пробник не тянул весь бинарь.
fn server_local_ip(client_ip: std::net::IpAddr) -> std::net::IpAddr {
    use std::net::Ipv4Addr;
    let std::net::IpAddr::V4(c4) = client_ip else {
        return client_ip;
    };
    for (_dev, addr, _mac) in nx_server_ifaddrs() {
        let std::net::IpAddr::V4(a4) = addr else { continue };
        if a4.octets()[..3] == c4.octets()[..3] && !a4.is_loopback() {
            return addr;
        }
    }
    client_ip
}

fn nx_server_ifaddrs() -> Vec<(String, std::net::IpAddr, String)> {
    use std::net::Ipv4Addr;
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
    out
}
