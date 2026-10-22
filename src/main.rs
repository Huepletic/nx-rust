//! nx-server: свой GameStream/NX-сервер на Rust.
//! Поддерживает Wayland/Hyprland, виртуальный (headless) дисплей, захват + VA-API H.264.
//!
//! Подкоманды:
//!   nx-server capture <output|auto> <out.png>        — один кадр в PNG (M0)
//!   nx-server encode  <output|auto> <frames> <h264> <png> — кодирование (M1)
//!   nx-server serve   [--pin 1234] [--name HOST]      — NVHTTP-сервер (M2)

mod dma;
mod mdns;
mod nvhttp;
mod portal;
mod probes;
mod stream;
mod vaapi;

use anyhow::{bail, Context, Result};
use std::sync::Arc;

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        usage();
    }
    match args[1].as_str() {
        "capture" => {
            if args.len() < 4 { usage(); }
            capture_cmd(&args[2], &args[3])
        }
        "encode" => {
            if args.len() < 6 { usage(); }
            let frames: usize = args[3].parse().context("frames")?;
            probes::encode_probe(&args[2], frames, &args[4], &args[5])
        }
        "copybench" => {
            if args.len() < 4 { usage(); }
            let frames: usize = args[3].parse().context("frames")?;
            probes::copy_bench(&args[2], frames)
        }
        "capturebench" => {
            if args.len() < 4 { usage(); }
            let secs: u64 = args[3].parse().context("seconds")?;
            probes::capture_bench(&args[2], secs)
        }
        "inputtest" => {
            if args.len() < 3 { usage(); }
            stream::input::selftest(&args[2])
        }
        "vaprobe" => {
            // Z4a-пробник: dumb-dmabuf (BGRX, паттерн) → VaEncoder → /tmp/p3_rs.h264.
            let frames: usize = if args.len() > 2 { args[2].parse().context("frames")? } else { 5 };
            vaapi::selftest(frames)
        }
        "backend" => {
            let b = probes::backend_for("auto");
            println!("{}", b.name());
            Ok(())
        }
        "serve" => serve_cmd(&args[2..]),
        _ => { usage(); }
    }
}

fn usage() -> ! {
    eprintln!(
        "usage:\n  nx-server capture <output|auto> <out.png>\n  \
         nx-server encode <output|auto> <frames> <out.h264> <ref.png>\n  \
         nx-server copybench <output|auto> <frames>\n  \
         nx-server capturebench <output|auto> <seconds>\n  \
         nx-server inputtest <output|auto>\n  \
         nx-server backend\n  \
         nx-server serve [--pin NNNN] [--name HOSTNAME] [--http-port N] [--https-port N]"
    );
    std::process::exit(2);
}

fn capture_cmd(want: &str, out: &str) -> Result<()> {
    probes::capture_probe(want, out)
}

/// Выбор вывода для стрима:
///  - явное имя (например HEADLESS-4) — используем, при отсутствии HEADLESS-* создаём;
///  - "auto" — первый существующий HEADLESS-1..8; если ни одного нет — создаём.
/// hyprctl нумерует headless-выводы монотонно и не переиспользует имена
/// (HEADLESS-1, -2, -3…), поэтому фиксировать имя нельзя — ищем первый существующий.
fn resolve_output(want: &str) -> Result<String> {
    // KDE Plasma Wayland не умеет wlr-screencopy: там портал + PipeWire.
    // KDE Plasma X11 снимаем через ffmpeg x11grab. В обоих случаях
    // hyprctl/HEADLESS-* не нужны, выход — синтетическое имя backend'а.
    let backend = probes::backend_for(want);
    if backend != probes::Backend::Wlr {
        let name = match backend {
            probes::Backend::Portal => "portal",
            probes::Backend::X11 => "x11",
            probes::Backend::Wlr => unreachable!(),
        };
        println!(
            "output: backend {} → '{}' (Hyprland headless не используется)",
            backend.name(),
            name
        );
        return Ok(name.to_string());
    }

    if want != "auto" {
        if probes::probe_output_size(want).is_ok() {
            return Ok(want.to_string());
        }
        if !want.starts_with("HEADLESS-") {
            bail!("output '{}' не найден на Wayland", want);
        }
    }
    for i in 1..=8 {
        let name = format!("HEADLESS-{}", i);
        if probes::probe_output_size(&name).is_ok() {
            println!("output: захватываем виртуальный монитор {}", name);
            return Ok(name);
        }
    }
    create_virtual_output()?;
    for _ in 0..120 {
        // до ~6с: вывод появляется мгновенно, но быстрые пробы с очередями Wayland — с запасом
        for i in 1..=8 {
            let name = format!("HEADLESS-{}", i);
            if probes::probe_output_size(&name).is_ok() {
                println!("output: создан виртуальный монитор {}, захватываем", name);
                return Ok(name);
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    bail!("виртуальный монитор не появился после hyprctl output create headless");
}

/// Создаём headless-вывод через свежий инстанс гипрленда (самый новый каталог
/// $XDG_RUNTIME_DIR/hypr — env у фонового сервиса протухает после перезагрузки).
fn create_virtual_output() -> Result<()> {
    let sig = fresh_instance_sig()?;
    println!(
        "output: нет виртуального монитора — создаю (hyprctl --instance {} output create headless)",
        sig
    );
    let out = std::process::Command::new("hyprctl")
        .args(["--instance", &sig, "output", "create", "headless"])
        .output()
        .context("запуск hyprctl")?;
    if !out.status.success() {
        bail!(
            "hyprctl output create headless: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(())
}

/// Свежая сигнатура инстанса Hyprland (самый новый каталог $XDG_RUNTIME_DIR/hypr):
/// env у фонового сервиса протухает после перезагрузки сессии.
pub(crate) fn fresh_instance_sig() -> Result<String> {
    let rundir =
        std::env::var("XDG_RUNTIME_DIR").unwrap_or_else(|_| "/run/user/1000".to_string());
    let hyprdir = std::path::Path::new(&rundir).join("hypr");
    let mut insts: Vec<(std::time::SystemTime, String)> = Vec::new();
    for e in std::fs::read_dir(&hyprdir)
        .with_context(|| format!("читать {}", hyprdir.display()))?
    {
        let e = e?;
        let n = e.file_name().to_string_lossy().replace(".socket.sock", "");
        if !n.contains('_') {
            continue;
        }
        if let Ok(t) = e.metadata().and_then(|m| m.modified()) {
            insts.push((t, n));
        }
    }
    insts.sort();
    insts
        .last()
        .map(|(_, n)| n.clone())
        .context("нет инстанций Hyprland в $XDG_RUNTIME_DIR/hypr")
}

fn serve_cmd(args: &[String]) -> Result<()> {
    let mut pin = "0000".to_string();
    let mut name = "nx-rust".to_string();
    let mut http_port: u16 = 48989;
    let mut https_port: u16 = 48984;
    // Дефолт продукта — отдельный виртуальный монитор HEADLESS-* (не мирроринг eDP-1):
    // headless рисует полный десктоп-экспириенс (панель/обои/окна рабочего стола)
    // и никогда не трогает физический экран. "auto" = первый существующий HEADLESS-*,
    // при отсутствии — создаём (гипрленд нумерует их монотонно: -1, -2, -3…).
    // Размер буфера = клиентскому viewport → масштабирование identity → full-bleed.
    let mut output = std::env::var("NX_OUTPUT").unwrap_or_else(|_| "auto".to_string());
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--pin" => { pin = args.get(i + 1).context("--pin NNNN")?.clone(); i += 2; }
            "--name" => { name = args.get(i + 1).context("--name HOST")?.clone(); i += 2; }
            "--http-port" => { http_port = args.get(i + 1).context("--http-port N")?.parse()?; i += 2; }
            "--https-port" => { https_port = args.get(i + 1).context("--https-port N")?.parse()?; i += 2; }
            "--output" => { output = args.get(i + 1).context("--output NAME")?.clone(); i += 2; }
            other => bail!("неизвестный флаг: {}", other),
        }
    }

    // Самоподписанный серверный сертификат (RSA-2048) — как у Sunshine при первом старте.
    // uniqueid и сертификат персистентны: Moonlight запоминает uniqueid и закрепляет серт,
    // их смена при рестарте = «Polling returned the wrong PC» и сломанный паринг.
    let state_dir = std::path::PathBuf::from(
        std::env::var("NX_STATE_DIR").unwrap_or_else(|_| {
            format!("{}/.local/state/nx-rust", std::env::var("HOME").unwrap_or_default())
        }),
    );
    std::fs::create_dir_all(&state_dir).context("state dir")?;

    let unique_id = {
        let p = state_dir.join("uniqueid");
        match std::fs::read_to_string(&p) {
            Ok(s) if s.trim().len() == 32 => s.trim().to_string(),
            _ => {
                let id = nvhttp::gen_unique_id();
                std::fs::write(&p, &id)?;
                println!("state: новый uniqueid {}", id);
                id
            }
        }
    };

    let (cert_pem, key_pem) = {
        let cp = state_dir.join("cert.pem");
        let kp = state_dir.join("key.pem");
        match (std::fs::read_to_string(&cp), std::fs::read_to_string(&kp)) {
            (Ok(c), Ok(k)) if c.contains("BEGIN CERTIFICATE") && k.contains("PRIVATE KEY") => (c, k),
            _ => {
                let (c, k) = nvhttp::gen_selfsigned().context("генерация сертификата")?;
                std::fs::write(&cp, &c)?;
                std::fs::write(&kp, &k)?;
                println!("state: новый серверный сертификат");
                (c, k)
            }
        }
    };

    // HEADLESS-* вывод должен существовать до захвата: если его нет — создаём
    // виртуальный монитор (правило monitors.lua применится само).
    let output = resolve_output(&output).context("wayland output")?;

    // Stream-сервисы (RTSP/RTP/ENet) — биндим свои порты ДО старта NVHTTP:
    // занятые порты = понятный фейл старта, а не молчаливый отказ клиента.
    let stream_state = stream::StreamState::new(&output).context("stream ports")?;
    stream::start(&stream_state).context("stream start")?;

    let state = Arc::new(nvhttp::ServerState {
        hostname: name,
        unique_id,
        clients: Default::default(),
        pairing: Default::default(),
        pin: std::sync::Mutex::new(pin),
        server_cert_pem: cert_pem,
        server_key_pem: key_pem,
        http_port,
        https_port,
        stream: stream_state,
    });

    // Блокирующий runtime на весь рантайм сервера
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;

    // mDNS/DNS-SD: объявляем `_nvstream._tcp.local`, чтобы Moonlight находил сервер.
    // До этого обнаружение держалось на временном /tmp/mdns_answer.py (Fix E).
    mdns::spawn(&state.hostname, http_port).context("mDNS responder")?;

    rt.block_on(nvhttp::serve(state, http_port, https_port))
}
