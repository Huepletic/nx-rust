//! KDE Plasma / GNOME Wayland: захват экрана через xdg-desktop-portal
//! ScreenCast + PipeWire.
//!
//! KWin не реализует wlr-screencopy. Штатный путь для KDE Wayland —
//! org.freedesktop.portal.ScreenCast: портал показывает диалог выбора
//! монитора/окна, отдаёт fd PipeWire-сессии и node id. Дальше мы сами
//! подключаемся к этому node как consumer и получаем BGRA/BGRx кадры.
//!
//! Сессия портала создаётся в `probe_output_size`, чтобы узнать размер
//! захвата до запуска ffmpeg, и хранится здесь до `capture_loop`.

use anyhow::{bail, Context, Result};
use ashpd::desktop::{
    screencast::{
        CursorMode, Screencast, SelectSourcesOptions, SourceType, Stream as PortalStream,
    },
    PersistMode,
};
use pipewire as pw;
use pw::properties::properties;
use pw::spa;
use std::io::Cursor;
use std::os::fd::OwnedFd;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use crate::probes::FrameRef;

pub struct PortalCapture {
    pub width: u32,
    pub height: u32,
    pub position: (i32, i32),
    fd: OwnedFd,
    node_id: u32,
    /// Сессия портала должна жить весь стрим: при её закрытии KWin может
    /// убрать screencast-ноду.
    session: ashpd::desktop::Session<Screencast>,
    /// zbus-задачи ashpd крутятся на этом runtime; держим его живым.
    _runtime: tokio::runtime::Runtime,
}

static CAPTURE: Mutex<Option<PortalCapture>> = Mutex::new(None);
static GEOM: Mutex<Option<((i32, i32), (u32, u32))>> = Mutex::new(None);

/// Открыть портал (диалог выбора экрана) и запомнить сессию.
/// Возвращает размер выбранного потока.
pub fn open_capture() -> Result<(u32, u32)> {
    let mut slot = CAPTURE.lock().unwrap();
    if let Some(pc) = slot.as_ref() {
        return Ok((pc.width, pc.height));
    }
    let pc = PortalCapture::open().context("xdg-desktop-portal ScreenCast")?;
    let size = (pc.width, pc.height);
    *GEOM.lock().unwrap() = Some((pc.position, size));
    *slot = Some(pc);
    Ok(size)
}

/// Запустить захват уже открытой портальной сессии.
pub fn capture_loop<F>(fps: u32, stop: Arc<AtomicBool>, sink: F) -> Result<()>
where
    F: FnMut(FrameRef) -> Result<()>,
{
    let pc = CAPTURE
        .lock()
        .unwrap()
        .take()
        .context("portal: сессия не открыта (probe_output_size не вызван)")?;
    pc.run(fps, stop, sink)
}

/// Геометрия выбранного потока: (x, y, w, h) в логических координатах.
pub fn geometry() -> Option<(i32, i32, u32, u32)> {
    GEOM.lock()
        .unwrap()
        .map(|((x, y), (w, h))| (x, y, w, h))
}

pub fn size() -> Option<(u32, u32)> {
    GEOM.lock().unwrap().map(|(_, s)| s)
}

impl PortalCapture {
    fn open() -> Result<Self> {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .context("tokio runtime для портала")?;
        let (session, stream, fd) = match runtime.block_on(open_portal(true)) {
            Ok(v) => v,
            Err(first) => {
                // Самая частая причина отказа — протухший restore-токен
                // (обновился портал/поменялся монитор). Ретрай имеет смысл
                // только если токен был; Cancelled пользователя не трогаем.
                let had_token = read_token().is_some();
                if matches!(
                    first.downcast_ref::<ashpd::Error>(),
                    Some(ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled))
                ) || !had_token
                {
                    return Err(first.context("портал ScreenCast"));
                }
                println!("portal: не удалось с сохранённым токеном ({first:#}); пробую без него");
                if let Some(p) = token_path() {
                    let _ = std::fs::remove_file(p);
                }
                runtime.block_on(open_portal(false))?
            }
        };
        let (w, h) = stream
            .size()
            .context("портал не сообщил размер выбранного потока")?;
        if w <= 0 || h <= 0 {
            bail!("портал вернул некорректный размер {w}x{h}");
        }
        let position = stream.position().unwrap_or((0, 0));
        Ok(Self {
            width: w as u32,
            height: h as u32,
            position,
            fd,
            node_id: stream.pipe_wire_node_id(),
            session,
            _runtime: runtime,
        })
    }

    fn run<F>(self, fps: u32, stop: Arc<AtomicBool>, sink: F) -> Result<()>
    where
        F: FnMut(FrameRef) -> Result<()>,
    {
        let width = self.width;
        let height = self.height;
        let fd = self.fd;
        let node_id = self.node_id;
        // Держим портальную сессию и её tokio-runtime до конца захвата:
        // при закрытии сессии KWin может убрать screencast-ноду.
        let session = self.session;
        let runtime = self._runtime;

        pw::init();
        let mainloop = std::rc::Rc::new(pw::main_loop::MainLoopBox::new(None)?);
        let context = pw::context::ContextBox::new(mainloop.loop_(), None)?;
        let core = context.connect_fd(fd, None)?;

        let error = Arc::new(Mutex::new(None::<String>));
        let data = UserData {
            format: spa::param::video::VideoInfoRaw::default(),
            width,
            height,
            buf: Vec::new(),
            sink,
            stop: stop.clone(),
            last_emit: Instant::now() - Duration::from_secs(1),
            period: Duration::from_micros(1_000_000 / u64::from(fps.max(1))),
            error: error.clone(),
        };

        let stream = pw::stream::StreamBox::new(
            &core,
            "nx-rust-portal",
            properties! {
                *pw::keys::MEDIA_TYPE => "Video",
                *pw::keys::MEDIA_CATEGORY => "Capture",
                *pw::keys::MEDIA_ROLE => "Screen",
            },
        )?;

        let listener = stream
            .add_local_listener_with_user_data(data)
            .state_changed(|_, _, old, new| {
                println!("portal: state {:?} -> {:?}", old, new);
            })
            .param_changed(|_, user_data, id, param| {
                let Some(param) = param else {
                    return;
                };
                if id != spa::param::ParamType::Format.as_raw() {
                    return;
                }
                let (media_type, media_subtype) =
                    match spa::param::format_utils::parse_format(param) {
                        Ok(v) => v,
                        Err(_) => return,
                    };
                if media_type != spa::param::format::MediaType::Video
                    || media_subtype != spa::param::format::MediaSubtype::Raw
                {
                    return;
                }
                if let Err(e) = user_data.format.parse(param) {
                    set_error(user_data, format!("разбор формата PipeWire: {e}"));
                    return;
                }
                let sz = user_data.format.size();
                let fr = user_data.format.framerate();
                println!(
                    "portal: формат {:?} {}x{} @ {}/{}",
                    user_data.format.format(),
                    sz.width,
                    sz.height,
                    fr.num,
                    fr.denom
                );
                if sz.width != user_data.width || sz.height != user_data.height {
                    set_error(
                        user_data,
                        format!(
                            "поток {}x{}, а ffmpeg ждёт {}x{}: пересоздайте поток или выберите другой монитор",
                            sz.width, sz.height, user_data.width, user_data.height
                        ),
                    );
                    return;
                }
            })
            .process(|stream, user_data| {
                if user_data.stop.load(Ordering::Relaxed) {
                    return;
                }
                let Some(mut buffer) = stream.dequeue_buffer() else {
                    return;
                };
                let datas = buffer.datas_mut();
                if datas.is_empty() {
                    return;
                }
                let data = &mut datas[0];
                let (size, offset, stride) = {
                    let c = data.chunk();
                    (c.size() as usize, c.offset() as usize, c.stride())
                };
                let Some(src_all) = data.data() else {
                    return;
                };

                let w = user_data.format.size().width;
                let h = user_data.format.size().height;
                if w == 0 || h == 0 {
                    return;
                }
                let w = w as usize;
                let h = h as usize;
                let row = w * 4;
                let stride = if stride > 0 { stride as usize } else { row };

                if offset + size > src_all.len() || size < row * h {
                    set_error(
                        user_data,
                        format!(
                            "PipeWire-буфер короче кадра: size={size} offset={offset} len={} stride={stride} row={row} h={h}",
                            src_all.len()
                        ),
                    );
                    return;
                }
                let src = &src_all[offset..offset + size];

                // Не отдаём энкодеру больше fps, чем просил клиент.
                if user_data.last_emit.elapsed() < user_data.period {
                    return;
                }
                user_data.last_emit = Instant::now();

                let fmt = user_data.format.format();
                let is_bgr = fmt.as_raw() == spa::param::video::VideoFormat::BGRx.as_raw()
                    || fmt.as_raw() == spa::param::video::VideoFormat::BGRA.as_raw();
                let is_rgb = fmt.as_raw() == spa::param::video::VideoFormat::RGBx.as_raw()
                    || fmt.as_raw() == spa::param::video::VideoFormat::RGBA.as_raw();
                if !is_bgr && !is_rgb {
                    set_error(
                        user_data,
                        format!("PipeWire отдал неподдерживаемый формат {fmt:?}"),
                    );
                    return;
                }

                if stride == row && is_bgr {
                    let frame = &src[..row * h];
                    if let Err(e) = (user_data.sink)(FrameRef::Bgra(frame, w as u32, h as u32)) {
                        set_error(user_data, format!("{e:#}"));
                    }
                    return;
                }

                let buf = &mut user_data.buf;
                buf.clear();
                buf.reserve(row * h);
                for y in 0..h {
                    let start = y * stride;
                    if start + row > size {
                        set_error(user_data, "PipeWire-буфер обрезан по строке".to_string());
                        return;
                    }
                    buf.extend_from_slice(&src[start..start + row]);
                }
                if is_rgb {
                    for px in buf.chunks_exact_mut(4) {
                        px.swap(0, 2);
                    }
                }
                if let Err(e) = (user_data.sink)(FrameRef::Bgra(buf, w as u32, h as u32)) {
                    set_error(user_data, format!("{e:#}"));
                }
            })
            .register()?;

        let obj = pw::spa::pod::object!(
            spa::utils::SpaTypes::ObjectParamFormat,
            spa::param::ParamType::EnumFormat,
            pw::spa::pod::property!(
                spa::param::format::FormatProperties::MediaType,
                Id,
                spa::param::format::MediaType::Video
            ),
            pw::spa::pod::property!(
                spa::param::format::FormatProperties::MediaSubtype,
                Id,
                spa::param::format::MediaSubtype::Raw
            ),
            pw::spa::pod::property!(
                spa::param::format::FormatProperties::VideoFormat,
                Choice,
                Enum,
                Id,
                spa::param::video::VideoFormat::BGRx,
                spa::param::video::VideoFormat::BGRx,
                spa::param::video::VideoFormat::BGRA,
                spa::param::video::VideoFormat::RGBx,
                spa::param::video::VideoFormat::RGBA,
            ),
            pw::spa::pod::property!(
                spa::param::format::FormatProperties::VideoSize,
                Choice,
                Range,
                Rectangle,
                spa::utils::Rectangle {
                    width,
                    height
                },
                spa::utils::Rectangle {
                    width,
                    height
                },
                spa::utils::Rectangle {
                    width,
                    height
                }
            ),
            pw::spa::pod::property!(
                spa::param::format::FormatProperties::VideoFramerate,
                Choice,
                Range,
                Fraction,
                spa::utils::Fraction {
                    num: fps.max(1),
                    denom: 1
                },
                spa::utils::Fraction { num: 1, denom: 1 },
                spa::utils::Fraction {
                    num: 1000,
                    denom: 1
                }
            ),
        );
        let values: Vec<u8> = pw::spa::pod::serialize::PodSerializer::serialize(
            Cursor::new(Vec::new()),
            &pw::spa::pod::Value::Object(obj),
        )
        .context("сериализация формата PipeWire")?
        .0
        .into_inner();

        let mut params = [spa::pod::Pod::from_bytes(&values)
            .context("Pod::from_bytes для формата PipeWire")?];

        stream.connect(
            spa::utils::Direction::Input,
            Some(node_id),
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )?;

        // Проверяем stop/error раз в 100 мс, даже если экран статичен
        // и compositor не присылает новых кадров.
        let timer = mainloop.loop_().add_timer({
            let ml = mainloop.clone();
            let st = stop.clone();
            let err = error.clone();
            move |_| {
                if st.load(Ordering::Relaxed) || err.lock().unwrap().is_some() {
                    ml.quit();
                }
            }
        });
        timer
            .update_timer(Some(Duration::from_millis(100)), Some(Duration::from_millis(100)))
            .into_result()
            .context("portal stop-timer")?;

        mainloop.run();
        drop(timer);
        drop(listener);
        drop(stream);

        let result = if let Some(e) = error.lock().unwrap().take() {
            Err(anyhow::anyhow!("portal capture: {e}"))
        } else {
            Ok(())
        };
        drop(session);
        drop(runtime);
        result
    }
}

struct UserData<F> {
    format: spa::param::video::VideoInfoRaw,
    width: u32,
    height: u32,
    buf: Vec<u8>,
    sink: F,
    stop: Arc<AtomicBool>,
    last_emit: Instant,
    period: Duration,
    error: Arc<Mutex<Option<String>>>,
}

fn set_error<F>(user_data: &mut UserData<F>, msg: String) {
    let mut err = user_data.error.lock().unwrap();
    if err.is_none() {
        *err = Some(msg);
    }
}

async fn open_portal(use_token: bool) -> Result<(ashpd::desktop::Session<Screencast>, PortalStream, OwnedFd)> {
    let proxy = Screencast::new().await?;
    let session = proxy.create_session(Default::default()).await?;
    let token = if use_token { read_token() } else { None };

    // KDE/GNOME могут не поддерживать persist_mode=Application, а старый
    // restore-токен — протухнуть. Пробуем несколько вариантов, но не глотаем
    // Cancelled: если пользователь отказался, повторять диалог нельзя.
    let mut attempts: Vec<(Option<&str>, PersistMode, CursorMode)> = Vec::new();
    if let Some(tok) = token.as_deref() {
        attempts.push((Some(tok), PersistMode::Application, CursorMode::Embedded));
    }
    attempts.push((None, PersistMode::DoNot, CursorMode::Embedded));
    attempts.push((None, PersistMode::DoNot, CursorMode::Hidden));
    let mut last_err: Option<ashpd::Error> = None;
    let mut selected = false;
    for (tok, persist, cursor) in attempts {
        let opts = select_options(tok, persist, cursor);
        match proxy.select_sources(&session, opts).await {
            Ok(_) => {
                selected = true;
                break;
            }
            Err(e) => {
                if matches!(
                    &e,
                    ashpd::Error::Response(ashpd::desktop::ResponseError::Cancelled)
                ) {
                    return Err(e.into());
                }
                println!("portal: select_sources не удался ({e}); пробую другой режим");
                last_err = Some(e);
            }
        }
    }
    if !selected {
        return Err(last_err
            .map(anyhow::Error::from)
            .unwrap_or_else(|| anyhow::anyhow!("портал не принял параметры ScreenCast")));
    }

    let response = proxy
        .start(&session, None, Default::default())
        .await?
        .response()?;
    let stream = response
        .streams()
        .first()
        .cloned()
        .context("портал не вернул ни одного потока (монитор не выбран?)")?;
    if let Some(tok) = response.restore_token() {
        write_token(tok);
    }

    let fd = proxy
        .open_pipe_wire_remote(&session, Default::default())
        .await?;
    Ok((session, stream, fd))
}

fn select_options(
    token: Option<&str>,
    persist: PersistMode,
    cursor: CursorMode,
) -> SelectSourcesOptions {
    SelectSourcesOptions::default()
        .set_cursor_mode(cursor)
        .set_sources(SourceType::Monitor | SourceType::Window)
        .set_multiple(false)
        .set_persist_mode(persist)
        .set_restore_token(token)
}

fn token_path() -> Option<PathBuf> {
    let state = std::env::var_os("NX_STATE_DIR")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".local/state/nx-rust")))?;
    let _ = std::fs::create_dir_all(&state);
    Some(state.join("portal.restore_token"))
}

fn read_token() -> Option<String> {
    let p = token_path()?;
    let s = std::fs::read_to_string(p).ok()?;
    let s = s.trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

fn write_token(token: &str) {
    if let Some(p) = token_path() {
        let _ = std::fs::write(&p, token);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o600));
        }
    }
}
