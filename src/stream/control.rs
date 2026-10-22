//! Control-канал: ENet поверх UDP :49001.
//! Клиент при appversion ≥ 7.1.431 ВСЕГДА шифрует control (cs.c:309
//! encryptedControlStream = APP_VERSION_AT_LEAST(7, 1, 431)):
//!   [NVCTL_ENCRYPTED_PACKET_HEADER: type u16 LE =0x0001, length u16 LE, seq u32 LE]
//!   [16B AES-GCM tag][ciphertext: {inner type u16 LE, payloadLength u16 LE} + payload]
//! Ключ = байты rikey из /launch (AES-128). Мы рекламируем в SDP
//! encryptionSupported:1 (SS_ENC_CONTROL_V2) → клиент шифрует V2-стилем:
//! IV = 12 байт, iv[0..4]=seq LE, iv[10]='C' (client), iv[11]='C' (control).
//! Start A (0x0305)/Start B (0x0307) идут через sendMessageAndDiscardReply — ответов не ждём.
//! Enet::new() — singleton на процесс, создаётся один раз здесь.

use super::*;
use anyhow::{Context, Result};
use enet::{Address, BandwidthLimit, ChannelLimit, Enet, Event, Host};
use std::net::Ipv4Addr;
use std::time::Duration;

pub fn start(st: Arc<StreamState>) {
    std::thread::spawn(move || {
        if let Err(e) = run(&st) {
            println!("control: {}", e);
        }
    });
}

fn run(st: &Arc<StreamState>) -> Result<()> {
    let enet = Enet::new().context("enet init (singleton на процесс)")?;
    let addr = Address::new(Ipv4Addr::UNSPECIFIED, CONTROL_PORT);
    let mut host: Host<()> = enet
        .create_host(
            Some(&addr),
            8,
            ChannelLimit::Maximum,
            BandwidthLimit::Unlimited,
            BandwidthLimit::Unlimited,
        )
        .context("enet create_host")?;
    println!("control: ENet слушает :{}", CONTROL_PORT);

    let mut input_pkts: u64 = 0;
    loop {
        match host.service(500) {
            Ok(Some(Event::Connect(ref p))) => {
                st.touch();
                println!("control: connect от {:?}", p.address());
            }
            Ok(Some(Event::Disconnect(_, _data))) => {
                st.touch();
                println!("control: disconnect — клиент ушёл, останавливаем конвейер");
                super::stop_pipeline(&st);
            }
            Ok(Some(Event::Receive { ref sender, channel_id, ref packet })) => {
                st.touch();
                let a = sender.address();
                let addr = SocketAddr::from(std::net::SocketAddrV4::new(*a.ip(), a.port()));
                handle_pkt(st, addr, channel_id, packet.data(), &mut input_pkts);
            }
            Ok(None) => {}
            Err(e) => {
                println!("control: service: {}", e);
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

/// Расшифровка 0x0001 → внутренний пакет [type u16 LE][payloadLength u16 LE][payload].
fn decrypt_control(st: &StreamState, data: &[u8]) -> Option<Vec<u8>> {
    use aes_gcm::aead::{Aead, Payload};
    use aes_gcm::{Aes128Gcm, KeyInit, Nonce};

    if data.len() < 8 + 16 + 4 {
        return None;
    }
    let seq = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
    let key_hex = st.launch.lock().unwrap().as_ref()?.rikey.clone();
    let key = hex::decode(&key_hex).ok()?;
    if key.len() != 16 {
        println!("control: rikey не 16 байт ({})", key.len());
        return None;
    }
    let cipher = Aes128Gcm::new_from_slice(&key).ok()?;
    // V2-стиль IV (клиент включает его, т.к. мы advertise SS_ENC_CONTROL_V2 в SDP):
    // 12 байт, iv[0..4] = seq LE, iv[10]='C' (client), iv[11]='C' (control);
    // сервер→клиент 'H','C' — понадобится, когда начнём слать control-пакеты.
    let mut iv = [0u8; 12];
    iv[..4].copy_from_slice(&seq.to_le_bytes());
    iv[10] = b'C';
    iv[11] = b'C';
    // Длина в заголовке = seq(4) + tag(16) + ciphertext; хвост после неё не берём
    let declared = u16::from_le_bytes([data[2], data[3]]) as usize;
    let end = (8 + declared).min(data.len());
    if end < 8 + 16 + 4 {
        return None;
    }
    // На проводе: [tag 16B][ciphertext], а aes-gcm ждёт ciphertext||tag
    let mut msg = Vec::with_capacity(end - 8);
    msg.extend_from_slice(&data[24..end]);
    msg.extend_from_slice(&data[8..24]);
    let plain = cipher
        .decrypt(Nonce::from_slice(&iv), Payload { msg: &msg, aad: b"" })
        .ok()?;
    if plain.len() < 4 {
        return None;
    }
    let inner_len = 4 + u16::from_le_bytes([plain[2], plain[3]]) as usize;
    if plain.len() < inner_len {
        println!(
            "control: расшифрованный пакет короче заявленного ({} < {})",
            plain.len(),
            inner_len
        );
        return None;
    }
    Some(plain)
}

fn handle_pkt(st: &StreamState, addr: SocketAddr, channel: u8, data: &[u8], input_pkts: &mut u64) {
    if data.len() < 2 {
        return;
    }
    let mut ty = u16::from_le_bytes([data[0], data[1]]);
    let mut body: &[u8] = data;
    let mut plain_owned: Option<Vec<u8>> = None;
    if ty == 0x0001 {
        match decrypt_control(st, data) {
            Some(plain) => {
                ty = u16::from_le_bytes([plain[0], plain[1]]);
                plain_owned = Some(plain);
                body = plain_owned.as_ref().unwrap();
            }
            None => {
                println!("control: НЕ РАСШИФРОВАНО 0x0001 len={} от {}", data.len(), addr);
                return;
            }
        }
    }
    let pay = &body[4.min(body.len())..];
    match ty {
        0x0305 => println!("control: Start A (ch={}, payload {}B) от {}", channel, pay.len(), addr),
        0x0307 => println!("control: Start B (ch={}, payload {}B) от {}", channel, pay.len(), addr),
        0x0302 => {
            // Клиент потерял опорный кадр и просит ключевой. Форсируем IDR на
            // следующем кадре — иначе клиент ждёт естественного IDR (GOP=40
            // кадров ≈0.7 с) и картинка стоит.
            st.force_idr.store(true, std::sync::atomic::Ordering::Relaxed);
            println!("control: IDR-request от {} — форсирую ключевой кадр", addr);
        }
        0x0109 => println!("control: termination от {}", addr),
        0x0200 => {} // periodic ping — молча
        0x0201 | 0x0204 => {} // loss/frame stats — молча
        0x0206 => {
            // input: тач/мышь/клавиатура клиента → uinput-инъекция (input.rs)
            super::input::handle(st, pay);
            *input_pkts += 1;
            if *input_pkts <= 3 || *input_pkts % 5000 == 0 {
                println!(
                    "control: input #{} len={} от {}",
                    *input_pkts,
                    pay.len(),
                    addr
                );
            }
        }
        other => {
            println!(
                "control: pkt 0x{:04x} len={} ch={} от {}",
                other,
                body.len(),
                channel,
                addr
            );
        }
    }
}
