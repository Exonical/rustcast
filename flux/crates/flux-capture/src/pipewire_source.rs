//! Real PipeWire capture stream via the official `pipewire` Rust bindings.
//!
//! [`PipewireStreamSource`] implements [`PipewireFrameSource`]. PipeWire is
//! callback-driven and its main loop is `!Send`, so the stream lives entirely
//! on a dedicated thread spawned by [`connect`](PipewireStreamSource::connect):
//!
//! 1. The thread rides the portal-provided fd (`Context::connect_fd`) so it
//!    reuses the already-authorized PipeWire connection.
//! 2. It offers an `EnumFormat` param advertising packed BGRx/RGBx/BGRA/RGBA
//!    `video/x-raw` (DMA-BUF and shared-memory buffers both acceptable) and
//!    lets the server fixate one.
//! 3. The `param_changed` callback records the negotiated
//!    [`NegotiatedFormat`]; the `process` callback turns each PipeWire buffer
//!    into a [`CapturedFrame`] and pushes it through the latest-wins
//!    [`FrameBridge`](crate::bridge), from which `recv_frame` pulls.
//!
//! DMA-BUF buffers are emitted zero-copy as [`GpuFrameHandle::DmaBuf`] (the
//! per-plane fds are `dup`'d so they outlive PipeWire's buffer recycling);
//! shared-memory buffers fall back to a CPU copy into a buffer reused from the
//! bridge's pool (see [`PipewireFrameSource::recycle_frame`]).

use std::cell::RefCell;
use std::os::fd::{BorrowedFd, OwnedFd, RawFd};
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use flux_core::error::{FluxError, Result};
use flux_core::cursor::CursorMetadata;
use flux_core::frame::CapturedFrame;
#[cfg(unix)]
use flux_core::frame::{DmaBufHandle, DmaBufPlane, GpuFrameHandle};
use flux_core::types::{PixelFormat, Resolution};

use pipewire as pw;
use pw::spa;
use spa::buffer::DataType;
use spa::param::format::{MediaSubtype, MediaType};
use spa::param::format_utils::parse_format;
use spa::param::video::{VideoFormat, VideoInfoRaw};
use spa::pod::{Pod, Property, PropertyFlags, Value};
use spa::utils::{Direction, Id, SpaTypes};

use crate::bridge::{FrameBridge, FrameSink, FrameSource};
use crate::cursor::parse_spa_meta_cursor;
use crate::session::{BufferKind, FormatPrefs, NegotiatedFormat, PipewireFrameSource};
use crate::traits::CursorUpdateSink;

/// DRM format modifier sentinel meaning "no/invalid modifier" (linear or
/// unspecified). Matches `DRM_FORMAT_MOD_INVALID`.
const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

/// Size requested for `SPA_META_Cursor`: the `spa_meta_cursor` header (28), a
/// `spa_meta_bitmap` header (20) and a 384x384 ARGB bitmap, matching what
/// Mutter reserves (`CURSOR_META_SIZE`).
const CURSOR_META_SIZE: i32 = 28 + 20 + 384 * 384 * 4;

/// Shared, lock-protected view of the format the stream fixated.
type SharedFormat = Arc<Mutex<Option<NegotiatedFormat>>>;

/// Commands delivered into the PipeWire loop thread.
enum StreamCmd {
    Quit,
    SetSize { resolution: Resolution, framerate: u32 },
}

/// A live PipeWire capture stream feeding a [`FrameBridge`].
pub struct PipewireStreamSource {
    source: Option<FrameSource>,
    format: SharedFormat,
    thread: Option<ThreadHandle>,
    cursor_sink: Option<CursorUpdateSink>,
}

struct ThreadHandle {
    /// Sends commands into the PipeWire loop thread.
    cmd: pw::channel::Sender<StreamCmd>,
    join: JoinHandle<()>,
}

impl Default for PipewireStreamSource {
    fn default() -> Self {
        Self::new()
    }
}

impl PipewireStreamSource {
    pub fn new() -> Self {
        Self {
            source: None,
            format: Arc::new(Mutex::new(None)),
            thread: None,
            cursor_sink: None,
        }
    }

    /// Receive cursor updates decoded from `SPA_META_Cursor` buffer metadata.
    /// Must be called before connecting.
    pub fn set_cursor_sink(&mut self, sink: Option<CursorUpdateSink>) {
        self.cursor_sink = sink;
    }

    /// Re-offer the stream formats at a new size/framerate. Compositors that
    /// derive the output mode from the negotiated format (Mutter virtual
    /// monitors) resize accordingly; the change is visible once frames arrive
    /// at the new resolution.
    pub fn request_size(&self, resolution: Resolution, framerate: u32) -> Result<()> {
        let handle = self
            .thread
            .as_ref()
            .ok_or_else(|| FluxError::Capture("PipeWire stream not connected".into()))?;
        handle
            .cmd
            .send(StreamCmd::SetSize { resolution, framerate })
            .map_err(|_| FluxError::Capture("PipeWire stream thread is gone".into()))
    }

    /// Connect to `node_id` through the local PipeWire daemon (no portal fd),
    /// for compositors whose stream nodes live on the user's own daemon.
    pub fn connect_local(&mut self, node_id: u32, prefs: FormatPrefs) -> Result<()> {
        self.spawn(None, node_id, prefs)
    }

    fn spawn(&mut self, fd: Option<OwnedFd>, node_id: u32, prefs: FormatPrefs) -> Result<()> {
        if self.thread.is_some() {
            return Err(FluxError::Capture("PipeWire stream already connected".into()));
        }

        let (sink, source) = FrameBridge::new();
        self.source = Some(source);
        let format = Arc::clone(&self.format);

        let (cmd_tx, cmd_rx) = pw::channel::channel::<StreamCmd>();
        let cursor_sink = self.cursor_sink.clone();
        let join = std::thread::Builder::new()
            .name("flux-pipewire".into())
            .spawn(move || {
                if let Err(e) = run_stream(fd, node_id, prefs, sink.clone(), format, cmd_rx, cursor_sink) {
                    tracing::error!("PipeWire capture thread exited with error: {e}");
                }
                // Make sure a consumer blocked in `recv` wakes up on exit.
                sink.close();
            })
            .map_err(|e| FluxError::Capture(format!("failed to spawn PipeWire thread: {e}")))?;

        self.thread = Some(ThreadHandle { cmd: cmd_tx, join });
        Ok(())
    }
}

impl PipewireFrameSource for PipewireStreamSource {
    fn connect(&mut self, pipewire_fd: RawFd, node_id: u32, prefs: FormatPrefs) -> Result<()> {
        // The portal owns the fd it handed us; `dup` it so this stream owns an
        // independent descriptor for `Context::connect_fd` (which takes/closes
        // an `OwnedFd`).
        let owned = dup_fd(pipewire_fd)?;
        self.spawn(Some(owned), node_id, prefs)
    }

    fn recv_frame(&mut self, timeout: Duration) -> Result<Option<CapturedFrame>> {
        match &self.source {
            Some(source) => Ok(source.recv(timeout)),
            None => Err(FluxError::Capture("PipeWire stream not connected".into())),
        }
    }

    fn negotiated_format(&self) -> Option<NegotiatedFormat> {
        self.format.lock().unwrap().clone()
    }

    fn recycle_frame(&mut self, frame: CapturedFrame) {
        if let Some(source) = &self.source {
            source.recycle(frame.data);
        }
    }

    fn disconnect(&mut self) -> Result<()> {
        if let Some(handle) = self.thread.take() {
            // Best-effort: signal the loop to quit and join the thread.
            let _ = handle.cmd.send(StreamCmd::Quit);
            let _ = handle.join.join();
        }
        self.source = None;
        Ok(())
    }
}

impl Drop for PipewireStreamSource {
    fn drop(&mut self) {
        let _ = self.disconnect();
    }
}

/// Body of the dedicated PipeWire loop thread.
fn run_stream(
    fd: Option<OwnedFd>,
    node_id: u32,
    prefs: FormatPrefs,
    sink: FrameSink,
    format: SharedFormat,
    cmd_rx: pw::channel::Receiver<StreamCmd>,
    cursor_sink: Option<CursorUpdateSink>,
) -> Result<()> {
    pw::init();

    let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(|e| pw_err("create main loop", e))?;
    let context = pw::context::ContextRc::new(&mainloop, None).map_err(|e| pw_err("create context", e))?;
    let core = match fd {
        Some(fd) => context
            .connect_fd_rc(fd, None)
            .map_err(|e| pw_err("connect to PipeWire fd", e))?,
        None => context
            .connect_rc(None)
            .map_err(|e| pw_err("connect to local PipeWire daemon", e))?,
    };

    let stream = pw::stream::StreamRc::new(
        core.clone(),
        "flux-capture",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )
    .map_err(|e| pw_err("create stream", e))?;
    let prefs = Rc::new(RefCell::new(prefs));

    // Per-frame sequence counter, owned by the process callback.
    let seq = Arc::new(Mutex::new(0u64));

    let format_cb = Arc::clone(&format);
    let format_proc = Arc::clone(&format);
    let sink_proc = sink.clone();
    let seq_proc = Arc::clone(&seq);

    let mut last_cursor: Option<CursorMetadata> = None;

    let _listener = stream
        .add_local_listener::<()>()
        .state_changed(|_stream, _ud, old, new| {
            tracing::debug!("PipeWire stream state: {old:?} -> {new:?}");
        })
        .param_changed(move |_stream, _ud, id, param| {
            let Some(param) = param else { return };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            match parse_format(param) {
                Ok((MediaType::Video, MediaSubtype::Raw)) => {}
                _ => return,
            }
            let mut info = VideoInfoRaw::new();
            if info.parse(param).is_err() {
                return;
            }
            let negotiated = negotiated_from_info(&info);
            tracing::info!(
                "PipeWire fixated format: {:?} {}x{} modifier={:#x}",
                negotiated.format,
                negotiated.resolution.width,
                negotiated.resolution.height,
                info.modifier(),
            );
            *format_cb.lock().unwrap() = Some(negotiated);
        })
        .process(move |stream, _ud| {
            let Some(mut buffer) = DequeuedBuffer::dequeue(stream) else {
                return;
            };
            if let Some(sink) = &cursor_sink
                && let Some(bytes) = buffer.meta(spa::sys::SPA_META_Cursor)
                && let Some(update) = parse_spa_meta_cursor(bytes)
                && cursor_changed(last_cursor.as_ref(), &update)
            {
                sink(update.clone());
                last_cursor = Some(update);
            }
            let negotiated = format_proc.lock().unwrap().clone();
            let Some(negotiated) = negotiated else {
                return;
            };
            let mut seq = seq_proc.lock().unwrap();
            // A buffer that carries no new frame (cursor-only update) must not
            // be pushed as a stale frame; the sequence only advances for frames.
            if let Some(frame) = build_frame(buffer.datas_mut(), &negotiated, *seq + 1, &sink_proc) {
                *seq += 1;
                sink_proc.push(frame);
            }
        })
        .register()
        .map_err(|e| pw_err("register listener", e))?;

    let params = build_stream_params(&prefs.borrow())?;
    let mut param_refs: Vec<&Pod> = params.iter().map(|p| p.as_ref()).collect();
    stream
        .connect(
            Direction::Input,
            Some(node_id),
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut param_refs,
        )
        .map_err(|e| pw_err("connect stream", e))?;

    // Quit the loop on disconnect and re-offer formats on size changes.
    let ml = mainloop.clone();
    let cmd_stream = stream.clone();
    let cmd_prefs = Rc::clone(&prefs);
    let _cmd = cmd_rx.attach(mainloop.loop_(), move |cmd| match cmd {
        StreamCmd::Quit => ml.quit(),
        StreamCmd::SetSize { resolution, framerate } => {
            {
                let mut prefs = cmd_prefs.borrow_mut();
                prefs.resolution = resolution;
                prefs.framerate = framerate;
            }
            let params = match build_stream_params(&cmd_prefs.borrow()) {
                Ok(params) => params,
                Err(e) => {
                    tracing::warn!("PipeWire: cannot rebuild stream params for {resolution}: {e}");
                    return;
                }
            };
            let mut refs: Vec<&Pod> = params.iter().map(|p| p.as_ref()).collect();
            match cmd_stream.update_params(&mut refs) {
                Ok(()) => tracing::info!("PipeWire: requested stream size {resolution}@{framerate}"),
                Err(e) => tracing::warn!("PipeWire: update_params for {resolution} failed: {e}"),
            }
        }
    });

    mainloop.run();
    Ok(())
}

/// Owned, serialized SPA pod backing a `&Pod` handed to `Stream::connect`.
struct OwnedPod(Vec<u8>);

impl OwnedPod {
    fn as_ref(&self) -> &Pod {
        Pod::from_bytes(&self.0).expect("serialized pod is valid")
    }
}

/// Everything the stream offers: the format parameters plus the cursor
/// metadata request.
fn build_stream_params(prefs: &FormatPrefs) -> Result<Vec<OwnedPod>> {
    let mut params = build_format_params(prefs)?;
    params.push(build_cursor_meta_param()?);
    Ok(params)
}

/// `SPA_PARAM_Meta` asking for `SPA_META_Cursor` on every buffer, so
/// compositors in cursor-metadata mode can deliver pointer position and shape
/// out of band.
fn build_cursor_meta_param() -> Result<OwnedPod> {
    use spa::pod::Object;

    let object = Object {
        type_: SpaTypes::ObjectParamMeta.as_raw(),
        id: spa::param::ParamType::Meta.as_raw(),
        properties: vec![
            Property::new(
                spa::sys::SPA_PARAM_META_type,
                Value::Id(Id(spa::sys::SPA_META_Cursor)),
            ),
            Property::new(spa::sys::SPA_PARAM_META_size, Value::Int(CURSOR_META_SIZE)),
        ],
    };
    serialize_object(object)
}

fn serialize_object(object: spa::pod::Object) -> Result<OwnedPod> {
    use spa::pod::serialize::PodSerializer;

    let bytes = PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Object(object))
        .map_err(|e| FluxError::Capture(format!("failed to serialize pod: {e}")))?
        .0
        .into_inner();
    Ok(OwnedPod(bytes))
}

/// Whether a cursor update differs from the last one handed to the sink: a new
/// shape, or a different position/hotspot (including hidden).
fn cursor_changed(last: Option<&CursorMetadata>, new: &CursorMetadata) -> bool {
    new.bitmap.is_some()
        || last.is_none_or(|last| last.position != new.position || last.hotspot != new.hotspot)
}

/// Whether a buffer's first data chunk carries a captured frame. Mutter marks
/// cursor-only buffers with an empty or `CORRUPTED` chunk.
fn chunk_has_frame(size: u32, flags: i32) -> bool {
    size > 0 && flags & spa::sys::SPA_CHUNK_FLAG_CORRUPTED as i32 == 0
}

/// A buffer dequeued from a stream, returned to it on drop. The `pipewire`
/// crate's safe `Buffer` does not expose buffer metadata, so this wraps the
/// raw `pw_buffer`.
struct DequeuedBuffer<'s> {
    stream: &'s pw::stream::Stream,
    raw: *mut pw::sys::pw_buffer,
}

impl<'s> DequeuedBuffer<'s> {
    fn dequeue(stream: &'s pw::stream::Stream) -> Option<Self> {
        // SAFETY: called from the stream's `process` callback; a non-null
        // pointer is returned to the same stream in `Drop`.
        let raw = unsafe { stream.dequeue_raw_buffer() };
        (!raw.is_null()).then_some(Self { stream, raw })
    }

    fn spa_buffer(&self) -> Option<&spa::sys::spa_buffer> {
        // SAFETY: `raw` is a live pw_buffer until `Drop`; its `buffer` field,
        // when non-null, points at the spa_buffer PipeWire allocated for it.
        unsafe { (*self.raw).buffer.as_ref() }
    }

    fn datas_mut(&mut self) -> &mut [spa::buffer::Data] {
        let Some(buffer) = self.spa_buffer() else {
            return &mut [];
        };
        let (datas, n_datas) = (buffer.datas, buffer.n_datas as usize);
        if datas.is_null() || n_datas == 0 {
            return &mut [];
        }
        // SAFETY: `spa::buffer::Data` is a transparent wrapper over `spa_data`
        // and `datas` points at `n_datas` of them, valid while the buffer is
        // dequeued; the exclusive borrow of `self` guards against aliasing.
        unsafe { std::slice::from_raw_parts_mut(datas as *mut spa::buffer::Data, n_datas) }
    }

    /// Bytes of the buffer's metadata block of the given `SPA_META_*` type.
    fn meta(&self, meta_type: u32) -> Option<&[u8]> {
        let buffer = self.spa_buffer()?;
        if buffer.metas.is_null() {
            return None;
        }
        // SAFETY: `metas` points at `n_metas` entries valid while the buffer
        // is dequeued; each meta's `data` spans `size` bytes.
        unsafe {
            std::slice::from_raw_parts(buffer.metas, buffer.n_metas as usize)
                .iter()
                .find(|meta| meta.type_ == meta_type && !meta.data.is_null())
                .map(|meta| std::slice::from_raw_parts(meta.data as *const u8, meta.size as usize))
        }
    }
}

impl Drop for DequeuedBuffer<'_> {
    fn drop(&mut self) {
        // SAFETY: `raw` was dequeued from `stream` and is returned once.
        unsafe { self.stream.queue_raw_buffer(self.raw) }
    }
}

/// Build the `EnumFormat` parameter list offered to the server.
///
/// We advertise packed 32-bit RGB formats (the encoder's CPU-upload and
/// DMA-BUF paths both handle these) as a choice, plus size/framerate ranges
/// hinted from [`FormatPrefs`]. With `dmabuf_modifiers` set, each format also
/// gets a DMA-BUF `EnumFormat` (modifier choice, mandatory and not fixated so
/// the producer picks the modifier) ahead of the shared-memory fallback.
/// DMA-BUF frames carry no CPU data, so callers must not offer modifiers until
/// the encoder can import DMA-BUF.
fn build_format_params(prefs: &FormatPrefs) -> Result<Vec<OwnedPod>> {
    use spa::utils::{Choice, ChoiceEnum, ChoiceFlags};

    let formats = preferred_video_formats(&prefs.formats);
    let default_format = formats[0];

    let mut params = Vec::new();
    if !prefs.dmabuf_modifiers.is_empty() {
        let modifiers: Vec<i64> = prefs.dmabuf_modifiers.iter().map(|m| *m as i64).collect();
        for format in &formats {
            let modifier_choice = Value::Choice(spa::pod::ChoiceValue::Long(Choice(
                ChoiceFlags::empty(),
                ChoiceEnum::Enum {
                    default: modifiers[0],
                    alternatives: modifiers.clone(),
                },
            )));
            params.push(serialize_object(enum_format_object(
                prefs,
                Value::Id(Id(format.as_raw())),
                Some(modifier_choice),
            ))?);
        }
    }

    let format_choice = Value::Choice(spa::pod::ChoiceValue::Id(Choice(
        ChoiceFlags::empty(),
        ChoiceEnum::Enum {
            default: Id(default_format.as_raw()),
            alternatives: formats.iter().map(|f| Id(f.as_raw())).collect(),
        },
    )));
    params.push(serialize_object(enum_format_object(prefs, format_choice, None))?);
    Ok(params)
}

/// One `EnumFormat` object for `format` (an Id or an Id choice), optionally
/// with a mandatory, non-fixated DMA-BUF modifier choice.
fn enum_format_object(prefs: &FormatPrefs, format: Value, modifier: Option<Value>) -> spa::pod::Object {
    use spa::pod::Object;
    use spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Fraction, Rectangle};

    let width = prefs.resolution.width.max(1);
    let height = prefs.resolution.height.max(1);
    let fps = prefs.framerate.max(1);

    let size = if prefs.exact_size {
        Value::Rectangle(Rectangle { width, height })
    } else {
        Value::Choice(spa::pod::ChoiceValue::Rectangle(Choice(
            ChoiceFlags::empty(),
            ChoiceEnum::Range {
                default: Rectangle { width, height },
                min: Rectangle { width: 1, height: 1 },
                max: Rectangle {
                    width: 8192,
                    height: 8192,
                },
            },
        )))
    };

    let max_framerate = Value::Choice(spa::pod::ChoiceValue::Fraction(Choice(
        ChoiceFlags::empty(),
        ChoiceEnum::Range {
            default: Fraction { num: fps, denom: 1 },
            min: Fraction { num: 1, denom: 1 },
            max: Fraction { num: fps, denom: 1 },
        },
    )));

    let framerate_choice = Value::Choice(spa::pod::ChoiceValue::Fraction(Choice(
        ChoiceFlags::empty(),
        ChoiceEnum::Range {
            default: Fraction { num: fps, denom: 1 },
            // Compositors (notably mutter) advertise screen-cast framerate as a
            // variable `0/1`; the offered range must include 0 or the formats
            // are rejected outright ("no more input formats").
            min: Fraction { num: 0, denom: 1 },
            max: Fraction { num: 1000, denom: 1 },
        },
    )));

    let mut object = Object {
        type_: SpaTypes::ObjectParamFormat.as_raw(),
        id: spa::param::ParamType::EnumFormat.as_raw(),
        properties: vec![
            Property::new(
                spa::param::format::FormatProperties::MediaType.as_raw(),
                Value::Id(Id(MediaType::Video.as_raw())),
            ),
            Property::new(
                spa::param::format::FormatProperties::MediaSubtype.as_raw(),
                Value::Id(Id(MediaSubtype::Raw.as_raw())),
            ),
            Property::new(spa::param::format::FormatProperties::VideoFormat.as_raw(), format),
            Property::new(spa::param::format::FormatProperties::VideoSize.as_raw(), size),
            Property {
                key: spa::param::format::FormatProperties::VideoFramerate.as_raw(),
                flags: PropertyFlags::empty(),
                value: framerate_choice,
            },
            Property {
                key: spa::param::format::FormatProperties::VideoMaxFramerate.as_raw(),
                flags: PropertyFlags::empty(),
                value: max_framerate,
            },
        ],
    };
    if let Some(modifier) = modifier {
        object.properties.push(Property {
            key: spa::param::format::FormatProperties::VideoModifier.as_raw(),
            flags: PropertyFlags::MANDATORY | dont_fixate(),
            value: modifier,
        });
    }
    object
}

/// `SPA_POD_PROP_FLAG_DONT_FIXATE`, which the `libspa` crate only names behind
/// its `v0_3_33` feature.
fn dont_fixate() -> PropertyFlags {
    PropertyFlags::from_bits_retain(spa::sys::SPA_POD_PROP_FLAG_DONT_FIXATE)
}

/// Map our preferred [`PixelFormat`]s to SPA video formats, always producing a
/// non-empty list (falls back to a sane packed-RGB set).
fn preferred_video_formats(prefs: &[PixelFormat]) -> Vec<VideoFormat> {
    let mut out = Vec::new();
    for p in prefs {
        match p {
            PixelFormat::Bgra8 => push_unique(&mut out, &[VideoFormat::BGRx, VideoFormat::BGRA]),
            PixelFormat::Rgba8 => push_unique(&mut out, &[VideoFormat::RGBx, VideoFormat::RGBA]),
            PixelFormat::Nv12 => push_unique(&mut out, &[VideoFormat::NV12]),
            PixelFormat::P010 => push_unique(&mut out, &[VideoFormat::P010_10LE]),
            PixelFormat::I420 => push_unique(&mut out, &[VideoFormat::I420]),
        }
    }
    if out.is_empty() {
        out = vec![
            VideoFormat::BGRx,
            VideoFormat::RGBx,
            VideoFormat::BGRA,
            VideoFormat::RGBA,
        ];
    }
    out
}

fn push_unique(out: &mut Vec<VideoFormat>, formats: &[VideoFormat]) {
    for f in formats {
        if !out.contains(f) {
            out.push(*f);
        }
    }
}

/// Translate a fixated SPA video format into our [`NegotiatedFormat`].
fn negotiated_from_info(info: &VideoInfoRaw) -> NegotiatedFormat {
    let modifier = info.modifier();
    let has_modifier = modifier != 0 && modifier != DRM_FORMAT_MOD_INVALID;
    let size = info.size();
    NegotiatedFormat {
        buffer_kind: if has_modifier {
            BufferKind::DmaBuf
        } else {
            BufferKind::SharedMemory
        },
        format: spa_format_to_pixel_format(info.format()),
        resolution: Resolution::new(size.width, size.height),
        modifier: has_modifier.then_some(modifier),
    }
}

fn spa_format_to_pixel_format(f: VideoFormat) -> PixelFormat {
    match f {
        VideoFormat::RGBx | VideoFormat::RGBA => PixelFormat::Rgba8,
        VideoFormat::NV12 => PixelFormat::Nv12,
        VideoFormat::P010_10LE => PixelFormat::P010,
        VideoFormat::I420 => PixelFormat::I420,
        // BGRx/BGRA and anything else map to our packed BGRA representation.
        _ => PixelFormat::Bgra8,
    }
}

/// DRM FourCC for a SPA video format, used when emitting DMA-BUF handles.
#[cfg(unix)]
fn spa_format_to_fourcc(f: VideoFormat) -> u32 {
    use drm_fourcc::DrmFourcc;
    let cc = match f {
        VideoFormat::BGRx => DrmFourcc::Xrgb8888,
        VideoFormat::BGRA => DrmFourcc::Argb8888,
        VideoFormat::RGBx => DrmFourcc::Xbgr8888,
        VideoFormat::RGBA => DrmFourcc::Abgr8888,
        VideoFormat::NV12 => DrmFourcc::Nv12,
        VideoFormat::P010_10LE => DrmFourcc::P010,
        _ => DrmFourcc::Xrgb8888,
    };
    cc as u32
}

/// Build a [`CapturedFrame`] from a dequeued buffer's data planes.
fn build_frame(
    datas: &mut [spa::buffer::Data],
    negotiated: &NegotiatedFormat,
    sequence: u64,
    sink: &FrameSink,
) -> Option<CapturedFrame> {
    if datas.is_empty() {
        return None;
    }
    let chunk = datas[0].chunk();
    if !chunk_has_frame(chunk.size(), chunk.flags().bits()) {
        return None;
    }

    let base = CapturedFrame {
        sequence,
        timestamp: std::time::Instant::now(),
        format: negotiated.format,
        resolution: negotiated.resolution,
        stride: datas[0].chunk().stride().max(0) as u32,
        data: Vec::new(),
        gpu_handle: None,
    };

    match datas[0].type_() {
        #[cfg(unix)]
        DataType::DmaBuf => build_dmabuf_frame(datas, negotiated, base),
        DataType::MemFd | DataType::MemPtr => build_shm_frame(datas, base, sink),
        other => {
            tracing::warn!("PipeWire delivered unsupported buffer type {other:?}");
            None
        }
    }
}

#[cfg(unix)]
fn build_dmabuf_frame(
    datas: &mut [spa::buffer::Data],
    negotiated: &NegotiatedFormat,
    mut base: CapturedFrame,
) -> Option<CapturedFrame> {
    let mut planes = Vec::with_capacity(datas.len());
    for data in datas.iter() {
        let raw_fd = data.as_raw().fd as RawFd;
        if raw_fd < 0 {
            tracing::warn!("DMA-BUF plane has invalid fd; dropping frame");
            return None;
        }
        // Own the fd past PipeWire's buffer recycling.
        let owned = dup_fd(raw_fd).ok()?;
        planes.push(DmaBufPlane {
            fd: Arc::new(owned),
            offset: data.chunk().offset(),
            stride: data.chunk().stride().max(0) as u32,
        });
    }
    if planes.is_empty() {
        return None;
    }

    base.gpu_handle = Some(GpuFrameHandle::DmaBuf(DmaBufHandle {
        planes,
        modifier: negotiated.modifier.unwrap_or(DRM_FORMAT_MOD_INVALID),
        fourcc: spa_format_to_fourcc(pixel_to_spa_format(negotiated.format)),
        width: negotiated.resolution.width,
        height: negotiated.resolution.height,
    }));
    Some(base)
}

fn build_shm_frame(
    datas: &mut [spa::buffer::Data],
    mut base: CapturedFrame,
    sink: &FrameSink,
) -> Option<CapturedFrame> {
    let chunk_size = datas[0].chunk().size() as usize;
    let mapped = datas[0].data()?;
    if chunk_size == 0 || chunk_size > mapped.len() {
        return None;
    }
    let len = chunk_size;
    // PipeWire recycles the mapping, so the copy is required; the allocation
    // is reused from the bridge pool.
    let mut data = sink.take_buffer();
    data.extend_from_slice(&mapped[..len]);
    base.data = data;
    Some(base)
}

/// Inverse of [`spa_format_to_pixel_format`], used only for FourCC selection on
/// the DMA-BUF path (a best-effort representative SPA format).
fn pixel_to_spa_format(p: PixelFormat) -> VideoFormat {
    match p {
        PixelFormat::Bgra8 => VideoFormat::BGRx,
        PixelFormat::Rgba8 => VideoFormat::RGBx,
        PixelFormat::Nv12 => VideoFormat::NV12,
        PixelFormat::P010 => VideoFormat::P010_10LE,
        PixelFormat::I420 => VideoFormat::I420,
    }
}

/// `dup` a borrowed fd into an owned one (close-on-exec), as an `OwnedFd`.
fn dup_fd(fd: RawFd) -> Result<OwnedFd> {
    if fd < 0 {
        return Err(FluxError::Capture("invalid PipeWire fd".into()));
    }
    // SAFETY: we only borrow `fd` for the duration of the dup; the caller
    // retains ownership of the original descriptor.
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    borrowed
        .try_clone_to_owned()
        .map_err(|e| FluxError::Capture(format!("failed to dup PipeWire fd: {e}")))
}

fn pw_err(ctx: &str, e: pw::Error) -> FluxError {
    FluxError::Capture(format!("PipeWire: {ctx}: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preferred_formats_default_to_packed_rgb() {
        let formats = preferred_video_formats(&[]);
        assert!(formats.contains(&VideoFormat::BGRx));
        assert!(formats.contains(&VideoFormat::RGBx));
    }

    #[test]
    fn preferred_formats_follow_prefs_without_duplicates() {
        let formats = preferred_video_formats(&[PixelFormat::Bgra8, PixelFormat::Bgra8]);
        assert_eq!(formats[0], VideoFormat::BGRx);
        // BGRx/BGRA appear once each despite the duplicate input.
        assert_eq!(formats.iter().filter(|f| **f == VideoFormat::BGRx).count(), 1);
    }

    #[test]
    fn build_format_params_serializes_an_enum_format_object() {
        let prefs = FormatPrefs::default();
        let params = build_format_params(&prefs).unwrap();
        assert_eq!(params.len(), 1);
        // The serialized bytes must re-parse as a valid object pod.
        let pod = params[0].as_ref();
        assert!(pod.is_object());
    }

    fn parse_object(pod: &OwnedPod) -> spa::pod::Object {
        use spa::pod::deserialize::PodDeserializer;
        match PodDeserializer::deserialize_from::<Value>(&pod.0).expect("pod deserializes").1 {
            Value::Object(object) => object,
            other => panic!("expected an object pod, got {other:?}"),
        }
    }

    fn property(object: &spa::pod::Object, key: u32) -> Option<&Property> {
        object.properties.iter().find(|p| p.key == key)
    }

    #[test]
    fn exact_size_offers_a_fixed_rectangle_and_max_framerate() {
        use spa::param::format::FormatProperties as Fp;
        use spa::utils::{ChoiceEnum, Fraction, Rectangle};

        let prefs = FormatPrefs {
            resolution: Resolution::new(1280, 720),
            framerate: 60,
            exact_size: true,
            ..FormatPrefs::default()
        };
        let params = build_format_params(&prefs).unwrap();
        assert_eq!(params.len(), 1);
        let object = parse_object(&params[0]);
        assert!(matches!(
            property(&object, Fp::VideoSize.as_raw()).unwrap().value,
            Value::Rectangle(Rectangle { width: 1280, height: 720 })
        ));
        match &property(&object, Fp::VideoMaxFramerate.as_raw()).unwrap().value {
            Value::Choice(spa::pod::ChoiceValue::Fraction(spa::utils::Choice(_, ChoiceEnum::Range { default, min, max }))) => {
                assert_eq!(*default, Fraction { num: 60, denom: 1 });
                assert_eq!(*min, Fraction { num: 1, denom: 1 });
                assert_eq!(*max, Fraction { num: 60, denom: 1 });
            }
            other => panic!("unexpected maxFramerate {other:?}"),
        }

        let ranged = FormatPrefs { exact_size: false, ..prefs };
        let object = parse_object(&build_format_params(&ranged).unwrap()[0]);
        assert!(matches!(
            property(&object, Fp::VideoSize.as_raw()).unwrap().value,
            Value::Choice(spa::pod::ChoiceValue::Rectangle(_))
        ));
        assert!(property(&object, Fp::VideoMaxFramerate.as_raw()).is_some());
    }

    #[test]
    fn dmabuf_modifiers_precede_the_shm_fallback() {
        use spa::param::format::FormatProperties as Fp;
        use spa::utils::ChoiceEnum;

        let prefs = FormatPrefs {
            formats: vec![PixelFormat::Bgra8],
            dmabuf_modifiers: vec![0x0100_0000_0000_0001, 0],
            ..FormatPrefs::default()
        };
        let params = build_format_params(&prefs).unwrap();
        // BGRx and BGRA each get a DMA-BUF object, then one SHM object.
        assert_eq!(params.len(), 3);
        for dmabuf in &params[..2] {
            let object = parse_object(dmabuf);
            let modifier = property(&object, Fp::VideoModifier.as_raw()).expect("modifier property");
            assert!(modifier.flags.contains(PropertyFlags::MANDATORY));
            assert!(modifier.flags.contains(dont_fixate()));
            match &modifier.value {
                Value::Choice(spa::pod::ChoiceValue::Long(spa::utils::Choice(_, ChoiceEnum::Enum { default, alternatives }))) => {
                    assert_eq!(*default, 0x0100_0000_0000_0001);
                    assert_eq!(alternatives, &vec![0x0100_0000_0000_0001, 0]);
                }
                other => panic!("unexpected modifier value {other:?}"),
            }
            assert!(matches!(
                property(&object, Fp::VideoFormat.as_raw()).unwrap().value,
                Value::Id(_)
            ));
        }
        let shm = parse_object(&params[2]);
        assert!(property(&shm, Fp::VideoModifier.as_raw()).is_none());
    }

    #[test]
    fn cursor_meta_param_requests_the_cursor_meta() {
        let object = parse_object(&build_cursor_meta_param().unwrap());
        assert_eq!(object.type_, SpaTypes::ObjectParamMeta.as_raw());
        assert!(matches!(
            property(&object, spa::sys::SPA_PARAM_META_type).unwrap().value,
            Value::Id(Id(id)) if id == spa::sys::SPA_META_Cursor
        ));
        assert!(matches!(
            property(&object, spa::sys::SPA_PARAM_META_size).unwrap().value,
            Value::Int(CURSOR_META_SIZE)
        ));
        let all = build_stream_params(&FormatPrefs::default()).unwrap();
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn metadata_only_chunks_are_not_frames() {
        let corrupted = spa::sys::SPA_CHUNK_FLAG_CORRUPTED as i32;
        assert!(chunk_has_frame(1920 * 1080 * 4, 0));
        assert!(!chunk_has_frame(0, 0));
        assert!(!chunk_has_frame(0, corrupted));
        assert!(!chunk_has_frame(4096, corrupted));
    }

    #[test]
    fn cursor_updates_are_forwarded_only_when_changed() {
        let at = |x, y| CursorMetadata { position: Some((x, y)), hotspot: (0, 0), bitmap: None };
        assert!(cursor_changed(None, &at(1, 1)));
        assert!(!cursor_changed(Some(&at(1, 1)), &at(1, 1)));
        assert!(cursor_changed(Some(&at(1, 1)), &at(2, 1)));
        assert!(cursor_changed(Some(&at(1, 1)), &CursorMetadata::hidden()));
        assert!(!cursor_changed(Some(&CursorMetadata::hidden()), &CursorMetadata::hidden()));
        let mut shaped = at(1, 1);
        shaped.bitmap = Some(flux_core::cursor::CursorBitmap {
            width: 1,
            height: 1,
            stride: 4,
            format: flux_core::cursor::CURSOR_FORMAT_RGBA8888,
            pixels: vec![0; 4],
        });
        assert!(cursor_changed(Some(&at(1, 1)), &shaped));
    }

    #[test]
    fn spa_to_pixel_format_maps_known_formats() {
        assert_eq!(spa_format_to_pixel_format(VideoFormat::BGRx), PixelFormat::Bgra8);
        assert_eq!(spa_format_to_pixel_format(VideoFormat::RGBx), PixelFormat::Rgba8);
        assert_eq!(spa_format_to_pixel_format(VideoFormat::NV12), PixelFormat::Nv12);
        assert_eq!(spa_format_to_pixel_format(VideoFormat::P010_10LE), PixelFormat::P010);
    }

    #[test]
    fn fourcc_is_stable_for_packed_formats() {
        use drm_fourcc::DrmFourcc;
        assert_eq!(spa_format_to_fourcc(VideoFormat::BGRx), DrmFourcc::Xrgb8888 as u32);
        assert_eq!(spa_format_to_fourcc(VideoFormat::RGBA), DrmFourcc::Abgr8888 as u32);
    }

    #[test]
    fn invalid_fd_is_rejected() {
        assert!(dup_fd(-1).is_err());
    }
}
