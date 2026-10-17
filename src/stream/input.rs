//! Инъекция ввода клиента в Wayland-сессию хоста через /dev/uinput.
//!
//! Клиент шлёт input-пачки типом 0x0206 (control ENet; расшифровка — control.rs):
//!   [u32 BE size (без этого поля)] [u32 LE magic] [тело]
//! Магики/поля — moonlight-common-c/src/Input.h; байтовый порядок проверен по
//! InputStream.c клиента:
//!   * magic — LE32, keyCode клавиатуры — LE16;
//!   * shorts мыши/скролла — BE16 (клиент пишет BE16);
//!   * netfloat тача — LE float.
//! Инъекция синхронная, прямо в потоке control-сервиса: путь «пакет от клиента
//! → write(/dev/uinput)» — микросекунды. Лаг ~800мс из-за которого звалась эта
//! задача жил НЕ в инъекции: input-канал ENet надёжный (RELIABLE), и при
//! переполнении Wi-Fi-линка ретраится сотнями миллисекунд — см. разбор латентности.
//!
//! Виртуальных устройств два:
//!   * REL-указатель+клавиатура ("nx-server virtual pointer") — для относительного
//!     тачпада/мыши (magic 0x06/0x07) и всех клавиш;
//!   * ABS-указатель ("nx-server virtual touch cursor", EV_ABS ABS_X/ABS_Y) — для
//!     прямого тача. libinput позиционирует его 1:1 по абсолютной оси, минуя
//!     ускорение курсора Hyprland (из-за которого пересчёт абсолюта в дельты врёт).
//!     Диапазон ABS_X/Y 0..32767 мапится на логический bbox ВСЕХ мониторов, поэтому
//!     долю вывода переводим в глобальную логическую координату и нормируем bbox'ом.
//!
//! Режим курсора клиента бывает три — поддерживаем все:
//!   * относительный тачпад (magic 0x06/0x07) → REL_X/REL_Y как пришли;
//!   * абсолютная мышь (0x05) → ABS-устройство (прямое позиционирование);
//!   * touchscreen-тач SS_TOUCH (0x55000002) → ABS + клик по кнопке.
//! Кнопки/скролл идут на то устройство, чей режим был последним (REL или ABS),
//! чтобы нажатие не удваивалось двумя указателями.

use super::*;
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use xkbcommon::xkb;

// ---- linux/input-event-codes ----
const EV_SYN: u16 = 0x00;
const EV_KEY: u16 = 0x01;
const EV_REL: u16 = 0x02;
const SYN_REPORT: u16 = 0x00;
const REL_X: u16 = 0x00;
const REL_Y: u16 = 0x01;
const REL_HWHEEL: u16 = 0x06;
const REL_WHEEL: u16 = 0x08;
const REL_WHEEL_HI_RES: u16 = 0x0b;
const REL_HWHEEL_HI_RES: u16 = 0x0c;
const BTN_LEFT: u16 = 0x110;
const BTN_RIGHT: u16 = 0x111;
const BTN_MIDDLE: u16 = 0x112;

// ---- GameStream input magics (moonlight-common-c/src/Input.h) ----
const KEY_DOWN: u32 = 0x03;
const KEY_UP: u32 = 0x04;
const MOUSE_ABS: u32 = 0x05;
const MOUSE_REL: u32 = 0x06;
const MOUSE_REL_G5: u32 = 0x07;
const BTN_DOWN_G5: u32 = 0x08;
const BTN_UP_G5: u32 = 0x09; // коллизия со старым SCROLL (GFE) — разводим по длине тела
const SCROLL_G5: u32 = 0x0a;
const SS_HSCROLL: u32 = 0x5500_0001;
const SS_TOUCH: u32 = 0x5500_0002;
// 0x0d = ENABLE_HAPTICS («клиент поддерживает вибро») — не поддерживаем, игнор.

// модификаторы пакета клавиатуры (Limelight.h MODIFIER_*)
const MOD_SHIFT: u8 = 0x01;
const MOD_CTRL: u8 = 0x02;
const MOD_ALT: u8 = 0x04;
const MOD_META: u8 = 0x08;

// uinput-ioctl (linux/uinput.h, значения стабильны с ядра 4.5)
const UI_DEV_CREATE: libc::c_ulong = 0x5501;
const UI_DEV_SETUP: libc::c_ulong = 0x405c_5503;
const UI_SET_EVBIT: libc::c_ulong = 0x4004_5564;
const UI_SET_KEYBIT: libc::c_ulong = 0x4004_5565;
const UI_SET_RELBIT: libc::c_ulong = 0x4004_5566;
const UI_SET_ABSBIT: libc::c_ulong = 0x4004_5567;
const UI_ABS_SETUP: libc::c_ulong = 0x401c_5504;
const BUS_VIRTUAL: u16 = 0x06;
const KEY_MAX: u16 = 0x2ff;

// linux/input-event-codes: абсолютные оси указателя
const EV_ABS: u16 = 0x03;
const ABS_X: u16 = 0x00;
const ABS_Y: u16 = 0x01;
/// верх диапазона ABS (как у QEMU usb-tablet) — libinput нормирует к нему
const ABS_MAX: i32 = 32767;

/// Прямой тач: Moonlight жмёт ЛКМ и «на 100 мс от касания», и при сдвиге пальца
/// >20 px (см. AbsoluteTouchContext#tapConfirmed) — от этого свайп по веб-странице
/// превращался в выделение текста. Ждём: свайп → ЛКМ не жмём, тап → клик,
/// «подержал >= DRAG_HOLD_MS и повёл» → перетаскивание.
const DRAG_HOLD_MS: u128 = 150;
/// Допуск дрожания пальца, при котором касание не считается свайпом.
const DRAG_PX: f64 = 24.0;

/// Длительность синтезированного тапа, мс.
/// Измерено инжектором /tmp/uinj.py на живом Hyprland: пару BTN_LEFT 1,0
/// в ОДНОМ SYN-кадре libinput/Hyprland отбрасывает целиком (клик не доходит),
/// а press и release в раздельных кадрах с паузой работают как настоящая мышь.
/// Реальный тап человека — 50…150 мс; берём 30 мс — достаточно стабильно и почти незаметно
/// по латентности (клиент и так ждёт release 100+ мс).
const TAP_HOLD_MS: u64 = 30;

/// Длительность синтезированного клика ПКМ/СКМ при прямом таче, мс.
/// Измерено инжектором /tmp/uinj.py mode=rt<мс> на живом Hyprland + thunar:
/// ПКМ, продержанная 150/300/500 мс, открывает контекстное меню; 700/924 мс — нет
/// (GTK трактует долгое удержание правой кнопки как drag, а не как клик).
/// Клиент Moonlight при long-press держит ПКМ ~900 мс — за порогом.
const RT_CLICK_HOLD_MS: u64 = 150;

/// _IOC(_IOC_READ,'E',0x18,96) — состояние всех клавиш evdev-ноды (для самотеста)
const EVIOCGKEY: libc::c_ulong = 0x80604518;

#[repr(C)]
#[derive(Clone, Copy)]
struct InputEvent {
    sec: i64,
    usec: i64,
    etype: u16,
    code: u16,
    value: i32,
}

impl InputEvent {
    fn to_bytes(self) -> [u8; 24] {
        let mut b = [0u8; 24];
        b[0..8].copy_from_slice(&self.sec.to_le_bytes());
        b[8..16].copy_from_slice(&self.usec.to_le_bytes());
        b[16..18].copy_from_slice(&self.etype.to_le_bytes());
        b[18..20].copy_from_slice(&self.code.to_le_bytes());
        b[20..24].copy_from_slice(&self.value.to_le_bytes());
        b
    }
}

#[repr(C)]
struct UinputSetup {
    id: [u16; 4], // bustype, vendor, product, version
    name: [u8; 80],
    ff_effects_max: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct InputAbsInfo {
    value: i32,
    minimum: i32,
    maximum: i32,
    fuzz: i32,
    flat: i32,
    resolution: i32,
}

/// linux/uinput.h `struct uinput_abs_setup` (code u16 + padding + absinfo) = 28 байт,
/// ровно как в _IOW('U', 4, …) = 0x401c5504.
#[repr(C)]
struct UinputAbsSetup {
    code: u16,
    absinfo: InputAbsInfo,
}

/// Лениво создаётся при первом input-пакете сессии (симулятор инъекции не требует).
static INIT_FAILED: AtomicBool = AtomicBool::new(false);
/// Диагностика «тапы не кликают»: первые фреймы входящего ввода — какие magic
/// реально шлёт клиент (SS_TOUCH прямой тач vs MOUSE_REL/BTN виртуальная мышь).
static DIAG_FRAMES: AtomicU64 = AtomicU64::new(0);
static DIAG_TOUCH: AtomicU64 = AtomicU64::new(0);
/// Отсчёт миллисекунд diag-вывода — для хронологии пакетов клиента.
static DIAG_T0: std::sync::OnceLock<std::time::Instant> = std::sync::OnceLock::new();
/// Лимит строк трассировки веток ЛКМ: дальше молчим, чтобы не заливать лог
/// (при удержании settle_left дёргается на каждый кадр движения).
static TRACE_N: AtomicU64 = AtomicU64::new(0);

/// Трассировка веток обработки ЛКМ (первые TRACE_MAX строк).
fn trace(msg: &str) {
    const TRACE_MAX: u64 = 120;
    let n = TRACE_N.fetch_add(1, Ordering::Relaxed);
    if n >= TRACE_MAX {
        return;
    }
    let t = DIAG_T0
        .get_or_init(std::time::Instant::now)
        .elapsed()
        .as_millis();
    println!("input: trace #{n} +{t}ms {msg}");
}

pub struct InputInjector {
    dev: std::fs::File,
    keysym2evdev: HashMap<u16, u16>,
    /// абсолютный указатель (EV_ABS) для прямого тача; None — если не создался
    abs_dev: Option<std::fs::File>,
    /// логический bbox всего рабочего стола (x0, y0, x1, y1) — область маппинга ABS
    bbox: (f64, f64, f64, f64),
    /// последнее движение было абсолютным (тач) → кнопки шлём на ABS-устройство
    mode_abs: bool,
    /// отложенная ЛКМ при прямом таче: (время прихода, gx, gy) — решаем по движению
    pending_left: Option<(std::time::Instant, f64, f64)>,
    /// момент прихода down отложенной ЛКМ — чтобы отличить тап от удержания
    pending_hold_ms: Option<std::time::Instant>,
    /// ЛКМ реально удерживается (pending_left подтверждён движением)
    left_held: bool,
    /// отслеживаемая глобальная позиция курсора, логические координаты
    cursor: (f64, f64),
    /// захватываемый вывод: (глобальное x, y, логический w, h)
    out: (f64, f64, f64, f64),
    held_mods: u8,
    unknown_keysyms: u64,
}

impl InputInjector {
    pub fn new(output: &str) -> Result<Self> {
        let dev = OpenOptions::new()
            .write(true)
            .open("/dev/uinput")
            .context("открыть /dev/uinput (пользователь должен быть в группе input)")?;
        let fd = dev.as_raw_fd();
        unsafe {
            ui_ioctl(fd, UI_SET_EVBIT, EV_KEY as libc::c_ulong).context("UI_SET_EVBIT EV_KEY")?;
            ui_ioctl(fd, UI_SET_EVBIT, EV_REL as libc::c_ulong).context("UI_SET_EVBIT EV_REL")?;
            // регистрируем весь диапазон клавиш — проще, чем гадать раскладку
            for code in 1..=KEY_MAX {
                ui_ioctl(fd, UI_SET_KEYBIT, code as libc::c_ulong)
                    .context("UI_SET_KEYBIT")?;
            }
            for rel in [REL_X, REL_Y, REL_WHEEL, REL_HWHEEL, REL_WHEEL_HI_RES, REL_HWHEEL_HI_RES] {
                ui_ioctl(fd, UI_SET_RELBIT, rel as libc::c_ulong).context("UI_SET_RELBIT")?;
            }
            let name = b"nx-server virtual pointer\0";
            let mut setup = UinputSetup {
                id: [BUS_VIRTUAL, 0x1d6b, 0x1042, 0x0001],
                name: [0; 80],
                ff_effects_max: 0,
            };
            setup.name[..name.len()].copy_from_slice(name);
            ui_ioctl_ptr(fd, UI_DEV_SETUP, &setup as *const UinputSetup)
                .context("UI_DEV_SETUP")?;
            ui_ioctl(fd, UI_DEV_CREATE, 0).context("UI_DEV_CREATE")?;
        }
        // libinput/гипрленд подхватывают устройство не мгновенно
        std::thread::sleep(Duration::from_millis(200));

        let abs_dev = match create_abs_device() {
            Ok(f) => {
                println!("input: абсолютный указатель (EV_ABS) готов — прямой тач 1:1");
                Some(f)
            }
            Err(e) => {
                println!(
                    "input: абсолютный указатель не создался: {:#} (тач уйдёт в дельты REL)",
                    e
                );
                None
            }
        };
        let bbox = desktop_bbox().context("логический bbox мониторов")?;

        let keysym2evdev = build_keysym_map();
        let (ox, oy, lw, lh) = output_geometry(output)?;
        let cursor = cursor_pos().unwrap_or((ox + lw / 2.0, oy + lh / 2.0));
        println!(
            "input: uinput-инъекция готова — вывод {} лог. {}x{} @ {},{} (bbox {},{}..{},{}; keysyms: {})",
            output,
            lw as u32,
            lh as u32,
            ox as i64,
            oy as i64,
            bbox.0 as i64,
            bbox.1 as i64,
            bbox.2 as i64,
            bbox.3 as i64,
            keysym2evdev.len()
        );
        Ok(Self {
            dev,
            keysym2evdev,
            abs_dev,
            bbox,
            mode_abs: false,
            pending_left: None,
            pending_hold_ms: None,
            left_held: false,
            cursor,
            out: (ox, oy, lw, lh),
            held_mods: 0,
            unknown_keysyms: 0,
        })
    }

    fn emit(&mut self, evs: &[(u16, u16, i32)]) {
        write_events(&self.dev, evs);
    }

    pub fn rel(&mut self, dx: i32, dy: i32) {
        self.mode_abs = false;
        if dx == 0 && dy == 0 {
            return;
        }
        let mut evs = Vec::with_capacity(2);
        if dx != 0 {
            evs.push((EV_REL, REL_X, dx));
        }
        if dy != 0 {
            evs.push((EV_REL, REL_Y, dy));
        }
        self.emit(&evs);
        let (ox, oy, lw, lh) = self.out;
        self.cursor.0 = (self.cursor.0 + dx as f64).clamp(ox, ox + lw - 1.0);
        self.cursor.1 = (self.cursor.1 + dy as f64).clamp(oy, oy + lh - 1.0);
    }

    /// Прямое (абсолютное) позиционирование курсора долей вывода: тач, absolute-мышь.
    /// Через EV_ABS-устройство — libinput ставит курсор 1:1, ускорение не влияет.
    /// Если ABS-устройства нет — деградация в дельты REL (как раньше).
    fn abs_move(&mut self, fx: f64, fy: f64) {
        self.mode_abs = true;
        let (ox, oy, lw, lh) = self.out;
        let fx = fx.clamp(0.0, 1.0);
        let fy = fy.clamp(0.0, 1.0);
        let gx = ox + fx * lw;
        let gy = oy + fy * lh;
        // движение пальца решает судьбу отложенной ЛКМ (до отправки координат)
        if self.abs_dev.is_some() {
            self.settle_left(gx, gy);
        }
        match self.abs_dev.as_ref() {
            Some(dev) => {
                let (bx0, by0, bx1, by1) = self.bbox;
                let bw = (bx1 - bx0).max(1.0);
                let bh = (by1 - by0).max(1.0);
                let ax = (((gx - bx0) / bw) * ABS_MAX as f64)
                    .round()
                    .clamp(0.0, ABS_MAX as f64) as i32;
                let ay = (((gy - by0) / bh) * ABS_MAX as f64)
                    .round()
                    .clamp(0.0, ABS_MAX as f64) as i32;
                write_events(dev, &[(EV_ABS, ABS_X, ax), (EV_ABS, ABS_Y, ay)]);
                // синхронизируем трекинг, чтобы возврат к REL не «прыгнул»
                self.cursor = (gx, gy);
            }
            None => self.move_frac(fx, fy),
        }
    }

    /// Абсолютная цель (доля ширины/высоты вывода) → дельта к нашему отслеживанию.
    /// Fallback, когда EV_ABS-устройство недоступно.
    fn move_frac(&mut self, fx: f64, fy: f64) {
        let (ox, oy, lw, lh) = self.out;
        let tx = ox + fx.clamp(0.0, 1.0) * lw;
        let ty = oy + fy.clamp(0.0, 1.0) * lh;
        let dx = (tx - self.cursor.0).round() as i32;
        let dy = (ty - self.cursor.1).round() as i32;
        self.rel(dx, dy);
    }

    pub fn button(&mut self, gfe_button: u8, down: bool) {
        // Limelight.h: BUTTON_LEFT=1, BUTTON_MIDDLE=2, BUTTON_RIGHT=3
        let code = match gfe_button {
            1 => BTN_LEFT,
            2 => BTN_MIDDLE,
            3 => BTN_RIGHT,
            _ => return,
        };
        // При прямом таче ЛКМ решаем по движению пальца (см. left_button)
        if gfe_button == 1 && self.mode_abs && self.abs_dev.is_some() {
            self.left_button(down);
            return;
        }
        // При удержании (long-press) клиент Moonlight держит кнопку ~900 мс, а
        // измерено инжектором (/tmp/uinj.py mode=rt<мс>, порог ~600 мс): ПКМ,
        // продержанная дольше, контекстное меню НЕ открывает (GTK трактует как drag).
        // Поэтому при прямом таче правую/среднюю кнопку синтезируем как короткий
        // клик: press -> пауза -> release, а приход up от клиента игнорируем.
        if self.mode_abs && self.abs_dev.is_some() && (gfe_button == 2 || gfe_button == 3) {
            if !down {
                trace(&format!("btn{gfe_button}: UP клиента игнорируем — клик уже синтезирован"));
                return;
            }
            let dev = self.abs_dev.as_ref().unwrap();
            trace(&format!(
                "btn{gfe_button}: down клиента -> синтез короткого клика ({RT_CLICK_HOLD_MS}мс)"
            ));
            write_events(dev, &[(EV_KEY, code, 1)]);
            std::thread::sleep(std::time::Duration::from_millis(RT_CLICK_HOLD_MS));
            write_events(dev, &[(EV_KEY, code, 0)]);
            trace(&format!("btn{gfe_button}: клик синтезирован (press+release раздельно)"));
            return;
        }
        let evs = [(EV_KEY, code, down as i32)];
        // Кнопку шлём на устройство текущего режима — иначе тач-позиция (ABS)
        // и клик (REL) окажутся на разных указателях.
        match self.abs_dev.as_ref() {
            Some(dev) if self.mode_abs => write_events(dev, &evs),
            _ => self.emit(&evs),
        }
    }

    /// ЛКМ при прямом таче.
    ///
    /// Moonlight жмёт её и по таймеру 100 мс от касания, и при сдвиге пальца
    /// >20 px (AbsoluteTouchContext#tapConfirmed) — поэтому палец «зажимал мышь»
    /// при обычном движении и по веб-странице выделялся текст. Поэтому:
    ///   * нажатие не отправляем сразу, а откладываем;
    ///   * палец поехал → свайп: ЛКМ не жмём вовсе (чистое движение курсора);
    ///   * палец простоял >= DRAG_HOLD_MS и поехал → перетаскивание;
    ///   * оторвали, ничего не случилось → тап, жмём и отпускаем клик.
    fn left_button(&mut self, down: bool) {
        // Трассировка ветки: тап «не кликает» — надо видеть, какая ветка отработала
        // и что было в состоянии на момент события (а не додумывать).
        if down {
            if self.left_held {
                trace("lb: DOWN отклонён — left_held уже true");
                return;
            }
            if self.pending_left.is_none() {
                self.pending_left =
                    Some((std::time::Instant::now(), self.cursor.0, self.cursor.1));
                self.pending_hold_ms = Some(std::time::Instant::now());
            }
            trace(&format!(
                "lb: DOWN принят -> pending set (mode_abs={} abs_dev={})",
                self.mode_abs,
                self.abs_dev.is_some()
            ));
            return; // ждём вердикта движения
        }
        let Some(dev) = self.abs_dev.as_ref() else {
            trace("lb: UP отклонён — abs_dev=None");
            return;
        };
        if self.pending_left.take().is_some() {
            let held = self
                .pending_hold_ms
                .map(|t0| t0.elapsed().as_millis())
                .unwrap_or(0);
            if held >= DRAG_HOLD_MS as u128 {
                // Палец стоял на месте, но долго — это удержание, а не тап.
                // Клиент Moonlight сам пришлёт ПКМ (правый клик), а синтезированный
                // здесь ЛКМ-тап ЕЁ ГАСИТ: измерено инжектором /tmp/uinj.py mode=hold —
                // «ЛКМ 30мс + ПКМ» контекстное меню не открывает, «ПКМ» один — открывает.
                trace(&format!(
                    "lb: UP -> удержание ({held}ms) — ЛКМ-тап НЕ синтезируем, ПКМ даст клиент"
                ));
                self.pending_hold_ms = None;
                return;
            }
            // Тап. PRESS и RELEASE обязаны уехать в РАЗНЫХ SYN-кадрах:
            // измерено инжектором (/tmp/uinj.py mode=split|same), одно и то же устройство
            // и точка — пару [1,0,SYN] libinput/Hyprland проигнорирует (клик не доходит
            // вообще), а [1,SYN] … [0,SYN] с паузой работает как настоящая мышь.
            trace("lb: UP -> тап: press в своём кадре");
            write_events(dev, &[(EV_KEY, BTN_LEFT, 1)]);
            // держим нажатие TAP_HOLD_MS, как настоящая мышь, и отпускаем отдельным кадром
            std::thread::sleep(std::time::Duration::from_millis(TAP_HOLD_MS));
            write_events(dev, &[(EV_KEY, BTN_LEFT, 0)]);
            trace("lb: UP -> тап: release в своём кадре");
            self.pending_hold_ms = None;
        } else if self.left_held {
            self.left_held = false;
            trace("lb: UP -> release [BTN_LEFT 0] (left_held)");
            write_events(dev, &[(EV_KEY, BTN_LEFT, 0)]);
        } else {
            trace("lb: UP отклонён — ни pending, ни left_held (свайп)");
            // это был свайп — нажатия не было, отпускать нечего
        }
    }

    /// Вердикт по отложенной ЛКМ: дистанция дальше допуска — свайп или drag.
    fn settle_left(&mut self, gx: f64, gy: f64) {
        let Some((t0, gx0, gy0)) = self.pending_left else { return };
        let d = (gx - gx0).hypot(gy - gy0);
        if d <= DRAG_PX {
            return; // палец ещё на месте — дрожание, тап ещё может получиться
        }
        let ms = t0.elapsed().as_millis();
        trace(&format!(
            "settle: дистанция {d:.1}px > {DRAG_PX}, выдержка {ms}ms (порог {DRAG_HOLD_MS})"
        ));
        self.pending_left = None;
        self.pending_hold_ms = None;
        if ms >= DRAG_HOLD_MS {
            self.left_held = true;
            if let Some(dev) = self.abs_dev.as_ref() {
                write_events(dev, &[(EV_KEY, BTN_LEFT, 1)]);
                trace("settle: drag -> BTN_LEFT 1 (left_held=true)");
            }
        } else {
            trace("settle: СВАЙП — pending снят, ЛКМ не жмём (клика не будет)");
        }
        // иначе это свайп: двигаем курсор, ЛКМ не нажимаем
    }

    /// Скролл: клиент шлёт WHEEL_DELTA-доли (±120 = одна насечка, бывает плавным).
    pub fn wheel(&mut self, v: i16, horizontal: bool) {
        if v == 0 {
            return;
        }
        let notches = {
            let n = v as i32 / 120;
            if n == 0 {
                v.signum() as i32
            } else {
                n
            }
        };
        if horizontal {
            self.emit(&[(EV_REL, REL_HWHEEL_HI_RES, v as i32), (EV_REL, REL_HWHEEL, notches)]);
        } else {
            self.emit(&[(EV_REL, REL_WHEEL_HI_RES, v as i32), (EV_REL, REL_WHEEL, notches)]);
        }
    }

    pub fn key(&mut self, keysym: u16, down: bool, modifiers: u8) {
        // Клиент держит модификатор по флагу, а события нажатия модификатора не
        // прислал (он-скрин-клавиатура, буфер обмена) — дожимаем синтетически.
        let synth = if down && !is_modifier_keysym(keysym) {
            modifiers & (MOD_SHIFT | MOD_CTRL | MOD_ALT | MOD_META) & !self.held_mods
        } else {
            0
        };
        for (bit, sym) in MOD_KEYS {
            if synth & bit != 0 {
                self.key_raw(sym, true);
            }
        }
        self.key_raw(keysym, down);
        for (bit, sym) in MOD_KEYS {
            if synth & bit != 0 {
                self.key_raw(sym, false);
            }
        }
    }

    fn key_raw(&mut self, keysym: u16, down: bool) {
        if is_modifier_keysym(keysym) {
            let bit = mod_bit(keysym);
            if down {
                self.held_mods |= bit;
            } else {
                self.held_mods &= !bit;
            }
        }
        match self.keysym2evdev.get(&keysym) {
            Some(&code) => self.emit(&[(EV_KEY, code, down as i32)]),
            None => {
                self.unknown_keysyms += 1;
                if self.unknown_keysyms <= 8 {
                    println!("input: keysym 0x{:04x} не найден в раскладке — пропуск", keysym);
                }
            }
        }
    }

    fn touch(&mut self, event_type: u8, fx: f64, fy: f64) {
        // LI_TOUCH_EVENT: 0=down 1=up 2=move 3=cancel 4=hover
        match event_type {
            0 => {
                self.abs_move(fx, fy);
                self.button(1, true);
            }
            1 | 3 => {
                self.abs_move(fx, fy);
                self.button(1, false);
            }
            _ => self.abs_move(fx, fy),
        }
    }

    fn frame(&mut self, magic: u32, body: &[u8]) {
        let n = DIAG_FRAMES.fetch_add(1, Ordering::Relaxed);
        // Нажатия кнопок логируем всегда: при long-press клиент шлёт их десятками
        // среди кадров движения, и лимит сгорал на 0x05 — ПКМ в diag не попадала.
        let is_button = magic == BTN_DOWN_G5 || magic == BTN_UP_G5;
        if is_button || n < 40 {
            let t = DIAG_T0.get_or_init(std::time::Instant::now).elapsed().as_millis();
            // hex тела — чтобы различать код кнопки клиента: 1=ЛКМ, 3=ПКМ
            let hex: String = body.iter().take(12).map(|b| format!("{b:02x}")).collect();
            println!(
                "input: diag фрейм #{n} +{t}ms magic=0x{magic:08x} body={}B [{hex}]",
                body.len()
            );
        }
        match magic {
            MOUSE_REL | MOUSE_REL_G5 => {
                if body.len() >= 4 {
                    let dx = i16::from_be_bytes([body[0], body[1]]) as i32;
                    let dy = i16::from_be_bytes([body[2], body[3]]) as i32;
                    self.rel(dx, dy);
                }
            }
            MOUSE_ABS => {
                // x(0..2), y(2..4), unused(4..6), width(6..8), height(8..10)
                if body.len() >= 10 {
                    let x = f64::from(i16::from_be_bytes([body[0], body[1]]));
                    let y = f64::from(i16::from_be_bytes([body[2], body[3]]));
                    let w = f64::from(i16::from_be_bytes([body[6], body[7]]).max(1));
                    let h = f64::from(i16::from_be_bytes([body[8], body[9]]).max(1));
                    self.abs_move(x / w, y / h);
                }
            }
            BTN_DOWN_G5 => {
                if !body.is_empty() {
                    self.button(body[0], true);
                }
            }
            BTN_UP_G5 if body.len() == 1 => self.button(body[0], false),
            SCROLL_G5 => {
                if body.len() >= 2 {
                    self.wheel(i16::from_be_bytes([body[0], body[1]]), false);
                }
            }
            BTN_UP_G5 => {
                // GFE-вариант скролла (magic коллидирует с button-up, но тело 6+ байт)
                if body.len() >= 2 {
                    self.wheel(i16::from_be_bytes([body[0], body[1]]), false);
                }
            }
            SS_HSCROLL => {
                if body.len() >= 2 {
                    self.wheel(i16::from_be_bytes([body[0], body[1]]), true);
                }
            }
            KEY_DOWN | KEY_UP => {
                // NV_KEYBOARD_PACKET: header(8) + flags(1) + keyCode LE16 + modifiers + zero2
                if body.len() >= 4 {
                    let keysym = u16::from_le_bytes([body[1], body[2]]);
                    let modifiers = body[3];
                    self.key(keysym, magic == KEY_DOWN, modifiers);
                }
            }
            SS_TOUCH => {
                // eventType u8, zero u8, rotation LE16, pointerId LE32, x/y/pressure netfloat LE
                if body.len() >= 16 {
                    let event_type = body[0];
                    let fx = f32::from_le_bytes([body[8], body[9], body[10], body[11]]);
                    let fy = f32::from_le_bytes([body[12], body[13], body[14], body[15]]);
                    let tn = DIAG_TOUCH.fetch_add(1, Ordering::Relaxed);
                    if tn < 20 {
                        println!("input: diag touch #{tn} type={event_type} fx={fx:.3} fy={fy:.3}");
                    }
                    self.touch(event_type, f64::from(fx), f64::from(fy));
                }
            }
            _ => {} // геймпады/пен/батарейки — мимо
        }
    }

    fn parse(&mut self, pay: &[u8]) {
        let mut off = 0usize;
        while off + 8 <= pay.len() {
            let size = u32::from_be_bytes(pay[off..off + 4].try_into().unwrap()) as usize;
            let magic = u32::from_le_bytes(pay[off + 4..off + 8].try_into().unwrap());
            if size < 4 || off + 4 + size > pay.len() {
                break;
            }
            let body = &pay[off + 8..off + 4 + size];
            off += 4 + size;
            self.frame(magic, body);
        }
    }
}

const MOD_KEYS: [(u8, u16); 4] = [
    (MOD_SHIFT, 0xffe1), // Shift_L
    (MOD_CTRL, 0xffe3),  // Control_L
    (MOD_ALT, 0xffe9),   // Alt_L
    (MOD_META, 0xffeb),  // Super_L
];

fn is_modifier_keysym(sym: u16) -> bool {
    (0xffe1..=0xffee).contains(&sym)
}

fn mod_bit(sym: u16) -> u8 {
    match sym {
        0xffe1 | 0xffe2 => MOD_SHIFT,
        0xffe3 | 0xffe4 => MOD_CTRL,
        0xffe9 | 0xffea => MOD_ALT,
        0xffe7 | 0xffe8 | 0xffeb | 0xffec => MOD_META,
        _ => 0,
    }
}

/// Карта «X11-кейсимв → evdev-код» из дефолтной раскладки xkb:
/// keycode = evdev+8, символ ищем по всем уровням (a→A на shift и т.п.).
fn build_keysym_map() -> HashMap<u16, u16> {
    let mut map = HashMap::new();
    let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    // пустые строки = дефолтные rules/model/layout/variant из окружения хоста
    let keymap = match xkb::Keymap::new_from_names(
        &ctx,
        "",
        "",
        "",
        "",
        None,
        xkb::KEYMAP_COMPILE_NO_FLAGS,
    ) {
        Some(k) => k,
        None => {
            println!("input: xkb-карта не собралась — клавиатура будет молчать");
            return map;
        }
    };
    let layout: xkb::LayoutIndex = 0;
    let min = keymap.min_keycode().raw();
    let max = keymap.max_keycode().raw();
    for kc in min..=max {
        let keycode = xkb::Keycode::new(kc);
        let levels = keymap.num_levels_for_key(keycode, layout);
        for level in 0..levels {
            for sym in keymap.key_get_syms_by_level(keycode, layout, level) {
                let raw = sym.raw();
                if raw > 0 && raw <= u32::from(u16::MAX) {
                    map.entry(raw as u16).or_insert(kc.saturating_sub(8) as u16);
                }
            }
        }
    }
    map
}

/// Собрать input_event-байты (24 байта/событие) и завершить SYN_REPORT.
fn encode_events(evs: &[(u16, u16, i32)]) -> Vec<u8> {
    let mut buf = Vec::with_capacity((evs.len() + 1) * 24);
    for &(etype, code, value) in evs {
        buf.extend_from_slice(&InputEvent { sec: 0, usec: 0, etype, code, value }.to_bytes());
    }
    buf.extend_from_slice(
        &InputEvent { sec: 0, usec: 0, etype: EV_SYN, code: SYN_REPORT, value: 0 }.to_bytes(),
    );
    buf
}

/// Записать готовые события на конкретное uinput-устройство.
/// EAGAIN/штатные гонки при переполнении — не фатальны, но их логируем:
/// тап «не кликает» — нужно знать, дошли ли байты до ядра вообще.
fn write_events(dev: &std::fs::File, evs: &[(u16, u16, i32)]) {
    let buf = encode_events(evs);
    let want = buf.len();
    match (&*dev).write(&buf) {
        Ok(n) if n == want => {}
        Ok(n) => trace(&format!(
            "write: ЧАСТИЧНАЯ запись {n}/{want} байт (evs={} шт)",
            evs.len()
        )),
        Err(e) => trace(&format!(
            "write: ОШИБКА {e} при {want} байт (evs={} шт)",
            evs.len()
        )),
    }
}

/// Второе uinput-устройство — абсолютный указатель (EV_ABS ABS_X/ABS_Y + кнопки).
/// Диапазон 0..ABS_MAX; libinput мапит его на логический bbox всех мониторов.
fn create_abs_device() -> Result<std::fs::File> {
    let dev = OpenOptions::new()
        .write(true)
        .open("/dev/uinput")
        .context("открыть /dev/uinput для абсолютного указателя")?;
    let fd = dev.as_raw_fd();
    unsafe {
        ui_ioctl(fd, UI_SET_EVBIT, EV_KEY as libc::c_ulong).context("ABS UI_SET_EVBIT EV_KEY")?;
        ui_ioctl(fd, UI_SET_EVBIT, EV_ABS as libc::c_ulong).context("ABS UI_SET_EVBIT EV_ABS")?;
        ui_ioctl(fd, UI_SET_EVBIT, EV_SYN as libc::c_ulong).context("ABS UI_SET_EVBIT EV_SYN")?;
        for code in [BTN_LEFT, BTN_RIGHT, BTN_MIDDLE] {
            ui_ioctl(fd, UI_SET_KEYBIT, code as libc::c_ulong).context("ABS UI_SET_KEYBIT")?;
        }
        for axis in [ABS_X, ABS_Y] {
            ui_ioctl(fd, UI_SET_ABSBIT, axis as libc::c_ulong).context("ABS UI_SET_ABSBIT")?;
            let setup = UinputAbsSetup {
                code: axis,
                absinfo: InputAbsInfo {
                    value: 0,
                    minimum: 0,
                    maximum: ABS_MAX,
                    fuzz: 0,
                    flat: 0,
                    resolution: 0,
                },
            };
            ui_ioctl_ptr(fd, UI_ABS_SETUP, &setup as *const UinputAbsSetup)
                .context("UI_ABS_SETUP")?;
        }
        let name = b"nx-server virtual touch cursor\0";
        let mut setup = UinputSetup {
            id: [BUS_VIRTUAL, 0x1d6b, 0x1043, 0x0001],
            name: [0; 80],
            ff_effects_max: 0,
        };
        setup.name[..name.len()].copy_from_slice(name);
        ui_ioctl_ptr(fd, UI_DEV_SETUP, &setup as *const UinputSetup).context("ABS UI_DEV_SETUP")?;
        ui_ioctl(fd, UI_DEV_CREATE, 0).context("ABS UI_DEV_CREATE")?;
    }
    std::thread::sleep(Duration::from_millis(200));
    Ok(dev)
}

/// Логический bbox всех включённых мониторов (x0, y0, x1, y1) — та область, на которую
/// libinput мапит абсолютные оси. Проверено на Host: eDP-1 1600x900 + HEADLESS-4 1088x800
/// + DP-2 1920x1080 → (0,0)..(6016,1700).
fn desktop_bbox() -> Result<(f64, f64, f64, f64)> {
    let v: serde_json::Value =
        serde_json::from_str(&hyprctl_json("monitors")?).context("парс monitors")?;
    let mons = v.as_array().context("hyprctl monitors: не массив")?;
    let (mut x0, mut y0) = (f64::INFINITY, f64::INFINITY);
    let (mut x1, mut y1) = (f64::NEG_INFINITY, f64::NEG_INFINITY);
    for m in mons {
        if m["disabled"].as_bool().unwrap_or(false) {
            continue;
        }
        let num = |k: &str| m[k].as_f64().unwrap_or(0.0);
        let scale = num("scale").max(0.1);
        let x = num("x");
        let y = num("y");
        let w = num("width") / scale;
        let h = num("height") / scale;
        x0 = x0.min(x);
        y0 = y0.min(y);
        x1 = x1.max(x + w);
        y1 = y1.max(y + h);
    }
    if !x0.is_finite() || x1 <= x0 || y1 <= y0 {
        anyhow::bail!("пустой bbox мониторов");
    }
    Ok((x0, y0, x1, y1))
}

/// Найти evdev-ноду нашего абсолютного устройства — нужна для самотеста кнопок
/// (uinput-дескриптор не отдаёт EVIOCGKEY, а нода /dev/input/eventN — отдаёт).
fn abs_evdev_node() -> Option<String> {
    let txt = std::fs::read_to_string("/proc/bus/input/devices").ok()?;
    // Устройств с одним именем может быть несколько (сервер уже держит своё) —
    // берём самое новое, по номеру inputN в Sysfs.
    let mut best: Option<(i64, String)> = None;
    for block in txt.split("\n\n") {
        if !block.contains(r#"N: Name="nx-server virtual touch cursor""#) {
            continue;
        }
        let mut idx = -1;
        let mut node = None;
        for line in block.lines() {
            if let Some(rest) = line.strip_prefix("S: Sysfs=") {
                if let Some(last) = rest.rsplit('/').next() {
                    if let Some(n) = last.strip_prefix("input") {
                        idx = n.parse::<i64>().unwrap_or(-1);
                    }
                }
            }
            if let Some(rest) = line.strip_prefix("H: Handlers=") {
                for tok in rest.split_whitespace() {
                    if tok.starts_with("event") {
                        node = Some(format!("/dev/input/{}", tok));
                    }
                }
            }
        }
        if let Some(node) = node {
            if best.as_ref().map_or(true, |(i, _)| idx > *i) {
                best = Some((idx, node));
            }
        }
    }
    best.map(|(_, n)| n)
}

/// Фактическое состояние клавиши на evdev-ноде (только чтение).
fn key_pressed(node: &str, code: u16) -> bool {
    let Ok(path) = std::ffi::CString::new(node) else { return false };
    let fd = unsafe {
        libc::open(path.as_ptr(), libc::O_RDONLY | libc::O_NONBLOCK)
    };
    if fd < 0 {
        return false;
    }
    let mut buf = [0u8; 96];
    let r = unsafe { libc::ioctl(fd, EVIOCGKEY, buf.as_mut_ptr() as libc::c_ulong) };
    unsafe { libc::close(fd) };
    if r < 0 {
        return false;
    }
    let code = code as usize;
    (*buf.get(code / 8).unwrap_or(&0) >> (code % 8)) & 1 == 1
}

fn hyprctl_json(sub: &str) -> Result<String> {
    let sig = crate::fresh_instance_sig()?;
    let out = std::process::Command::new("hyprctl")
        .args(["--instance", &sig, "-j", sub])
        .output()
        .context("запуск hyprctl")?;
    if !out.status.success() {
        anyhow::bail!(
            "hyprctl -j {}: {}",
            sub,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).to_string())
}

/// Глобальное начало и логический размер захватываемого вывода.
fn output_geometry(output: &str) -> Result<(f64, f64, f64, f64)> {
    let v: serde_json::Value =
        serde_json::from_str(&hyprctl_json("monitors")?).context("парс monitors")?;
    let mons = v.as_array().context("hyprctl monitors: не массив")?;
    let m = mons
        .iter()
        .find(|m| m["name"] == output)
        .with_context(|| format!("вывод '{}' не найден в hyprctl monitors", output))?;
    let num = |k: &str| m[k].as_f64().unwrap_or(0.0);
    let scale = num("scale").max(0.1);
    Ok((num("x"), num("y"), num("width") / scale, num("height") / scale))
}

fn cursor_pos() -> Result<(f64, f64)> {
    let v: serde_json::Value =
        serde_json::from_str(&hyprctl_json("cursorpos")?).context("парс cursorpos")?;
    Ok((v["x"].as_f64().unwrap_or(0.0), v["y"].as_f64().unwrap_or(0.0)))
}

unsafe fn ui_ioctl(fd: libc::c_int, req: libc::c_ulong, arg: libc::c_ulong) -> Result<()> {
    if libc::ioctl(fd, req, arg) < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

unsafe fn ui_ioctl_ptr<T>(fd: libc::c_int, req: libc::c_ulong, ptr: *const T) -> Result<()> {
    if libc::ioctl(fd, req, ptr as libc::c_ulong) < 0 {
        return Err(std::io::Error::last_os_error().into());
    }
    Ok(())
}

/// Точка входа из control.rs: ленивая инициализация + разбор пачки.
pub fn handle(st: &StreamState, pay: &[u8]) {
    {
        let mut guard = st.input.lock().unwrap();
        if guard.is_none() {
            if INIT_FAILED.load(Ordering::Relaxed) {
                return;
            }
            match InputInjector::new(&st.output) {
                Ok(inj) => *guard = Some(inj),
                Err(e) => {
                    INIT_FAILED.store(true, Ordering::Relaxed);
                    println!("input: инъекция недоступна (один раз): {:#}", e);
                    return;
                }
            }
        }
        if let Some(inj) = guard.as_mut() {
            inj.parse(pay);
        }
    }
}

/// Самотест инъекции из CLI (`nx-server inputtest <output>`): создаёт uinput-устройства
/// и прогоняет ABS-позиции по углам/центру вывода — проверка EV_ABS без стрима.
pub fn selftest(output: &str) -> Result<()> {
    let mut inj = InputInjector::new(output)?;
    println!(
        "input: ABS-устройство: {}",
        if inj.abs_dev.is_some() { "есть" } else { "НЕТ (fallback в REL-дельты)" }
    );
    let (ox, oy, lw, lh) = inj.out;
    for (fx, fy) in [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0), (0.5, 0.5)] {
        inj.abs_move(fx, fy);
        std::thread::sleep(Duration::from_millis(300));
        let c = cursor_pos().unwrap_or((f64::NAN, f64::NAN));
        println!(
            "  frac({:.2},{:.2}) желаем ({:.0},{:.0}) → курсор ({:.0},{:.0})",
            fx,
            fy,
            ox + fx * lw,
            oy + fy * lh,
            c.0,
            c.1
        );
    }

    // Проверка логики «свайп ≠ клик» — смотрим НЕ отчёты, а реальное состояние
    // кнопки на evdev-ноде (EVIOCGKEY).
    match abs_evdev_node() {
        None => println!("--- логику тача проверить не удалось (нода не найдена) ---"),
        Some(node) => {
            let s = |on: bool| if on { "ВЖАТА" } else { "отпущена" };
            let expect = |on: bool, want: bool| if on == want { "OK" } else { "ПРОВАЛ" };
            println!("--- логика прямого тача ({}) ---", node);

            // A) свайп: нажатие и тут же движение пальца → ЛКМ нажиматься не должна
            inj.abs_move(0.40, 0.40);
            std::thread::sleep(Duration::from_millis(60));
            inj.button(1, true);
            inj.abs_move(0.55, 0.55);
            std::thread::sleep(Duration::from_millis(80));
            let a = key_pressed(&node, BTN_LEFT);
            println!("  A свайп  → BTN_LEFT {} (надо отпущена) {}", s(a), expect(a, false));
            inj.button(1, false);
            std::thread::sleep(Duration::from_millis(80));
            let a2 = key_pressed(&node, BTN_LEFT);
            println!("  A отрыв  → BTN_LEFT {} (надо отпущена) {}", s(a2), expect(a2, false));

            // B) drag: нажатие, палец простоял >= DRAG_HOLD_MS, потом поехал → держим
            inj.abs_move(0.30, 0.30);
            std::thread::sleep(Duration::from_millis(60));
            inj.button(1, true);
            std::thread::sleep(Duration::from_millis(DRAG_HOLD_MS as u64 + 100));
            inj.abs_move(0.45, 0.45);
            std::thread::sleep(Duration::from_millis(80));
            let b = key_pressed(&node, BTN_LEFT);
            println!("  B drag   → BTN_LEFT {} (надо ВЖАТА) {}", s(b), expect(b, true));
            inj.button(1, false);
            std::thread::sleep(Duration::from_millis(80));
            let b2 = key_pressed(&node, BTN_LEFT);
            println!("  B отрыв  → BTN_LEFT {} (надо отпущена) {}", s(b2), expect(b2, false));

            // C) тап: нажатие и отпускание без движения → клик и чистое отпускание
            inj.abs_move(0.60, 0.60);
            std::thread::sleep(Duration::from_millis(60));
            inj.button(1, true);
            inj.button(1, false);
            std::thread::sleep(Duration::from_millis(80));
            let c = key_pressed(&node, BTN_LEFT);
            println!("  C тап    → BTN_LEFT {} (надо отпущена, клик ушёл) {}", s(c), expect(c, false));
        }
    }
    Ok(())
}
