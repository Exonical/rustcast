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
//! shared-memory buffers fall back to a CPU copy.

use std::os::fd::{BorrowedFd, OwnedFd, RawFd};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use flux_core::error::{FluxError, Result};
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
use crate::session::{BufferKind, FormatPrefs, NegotiatedFormat, PipewireFrameSource};

/// DRM format modifier sentinel meaning "no/invalid modifier" (linear or
/// unspecified). Matches `DRM_FORMAT_MOD_INVALID`.
const DRM_FORMAT_MOD_INVALID: u64 = 0x00ff_ffff_ffff_ffff;

/// Shared, lock-protected view of the format the stream fixated.
type SharedFormat = Arc<Mutex<Option<NegotiatedFormat>>>;

/// A live PipeWire capture stream feeding a [`FrameBridge`].
pub struct PipewireStreamSource {
    source: Option<FrameSource>,
    format: SharedFormat,
    thread: Option<ThreadHandle>,
}

struct ThreadHandle {
    /// Sends a quit signal into the PipeWire loop thread.
    quit: pw::channel::Sender<()>,
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
        }
    }
}

impl PipewireFrameSource for PipewireStreamSource {
    fn connect(&mut self, pipewire_fd: RawFd, node_id: u32, prefs: FormatPrefs) -> Result<()> {
        if self.thread.is_some() {
            return Err(FluxError::Capture("PipeWire stream already connected".into()));
        }

        // The portal owns the fd it handed us; `dup` it so this stream owns an
        // independent descriptor for `Context::connect_fd` (which takes/closes
        // an `OwnedFd`).
        let owned = dup_fd(pipewire_fd)?;

        let (sink, source) = FrameBridge::new();
        self.source = Some(source);
        let format = Arc::clone(&self.format);

        let (quit_tx, quit_rx) = pw::channel::channel::<()>();
        let join = std::thread::Builder::new()
            .name("flux-pipewire".into())
            .spawn(move || {
                if let Err(e) = run_stream(owned, node_id, prefs, sink.clone(), format, quit_rx) {
                    tracing::error!("PipeWire capture thread exited with error: {e}");
                }
                // Make sure a consumer blocked in `recv` wakes up on exit.
                sink.close();
            })
            .map_err(|e| FluxError::Capture(format!("failed to spawn PipeWire thread: {e}")))?;

        self.thread = Some(ThreadHandle { quit: quit_tx, join });
        Ok(())
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

    fn disconnect(&mut self) -> Result<()> {
        if let Some(handle) = self.thread.take() {
            // Best-effort: signal the loop to quit and join the thread.
            let _ = handle.quit.send(());
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
    fd: OwnedFd,
    node_id: u32,
    prefs: FormatPrefs,
    sink: FrameSink,
    format: SharedFormat,
    quit_rx: pw::channel::Receiver<()>,
) -> Result<()> {
    pw::init();

    let mainloop = pw::main_loop::MainLoop::new(None).map_err(|e| pw_err("create main loop", e))?;
    let context = pw::context::Context::new(&mainloop).map_err(|e| pw_err("create context", e))?;
    let core = context
        .connect_fd(fd, None)
        .map_err(|e| pw_err("connect to PipeWire fd", e))?;

    let stream = pw::stream::Stream::new(
        &core,
        "flux-capture",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )
    .map_err(|e| pw_err("create stream", e))?;

    // Per-frame sequence counter, owned by the process callback.
    let seq = Arc::new(Mutex::new(0u64));

    let format_cb = Arc::clone(&format);
    let format_proc = Arc::clone(&format);
    let sink_proc = sink.clone();
    let seq_proc = Arc::clone(&seq);

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
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let negotiated = format_proc.lock().unwrap().clone();
            let Some(negotiated) = negotiated else {
                return;
            };
            let mut seq = seq_proc.lock().unwrap();
            *seq += 1;
            if let Some(frame) = build_frame(buffer.datas_mut(), &negotiated, *seq) {
                sink_proc.push(frame);
            }
        })
        .register()
        .map_err(|e| pw_err("register listener", e))?;

    let params = build_format_params(&prefs)?;
    let mut param_refs: Vec<&Pod> = params.iter().map(|p| p.as_ref()).collect();
    stream
        .connect(
            Direction::Input,
            Some(node_id),
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut param_refs,
        )
        .map_err(|e| pw_err("connect stream", e))?;

    // Quit the loop when the source is dropped / disconnect is requested.
    let ml = mainloop.clone();
    let _quit = quit_rx.attach(mainloop.loop_(), move |_| ml.quit());

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

/// Build the `EnumFormat` parameter list offered to the server.
///
/// We advertise packed 32-bit RGB formats (the encoder's CPU-upload and
/// DMA-BUF paths both handle these) as a choice, plus size/framerate ranges
/// hinted from [`FormatPrefs`]. The server fixates a concrete format and
/// chooses DMA-BUF vs shared memory based on what both ends support.
fn build_format_params(prefs: &FormatPrefs) -> Result<Vec<OwnedPod>> {
    use spa::pod::{serialize::PodSerializer, Object};
    use spa::utils::{Choice, ChoiceEnum, ChoiceFlags, Fraction, Rectangle};

    let formats = preferred_video_formats(&prefs.formats);
    let default_format = formats[0];

    let width = prefs.resolution.width.max(1);
    let height = prefs.resolution.height.max(1);
    let fps = prefs.framerate.max(1);

    let format_choice = Value::Choice(spa::pod::ChoiceValue::Id(Choice(
        ChoiceFlags::empty(),
        ChoiceEnum::Enum {
            default: Id(default_format.as_raw()),
            alternatives: formats.iter().map(|f| Id(f.as_raw())).collect(),
        },
    )));

    let size_choice = Value::Choice(spa::pod::ChoiceValue::Rectangle(Choice(
        ChoiceFlags::empty(),
        ChoiceEnum::Range {
            default: Rectangle { width, height },
            min: Rectangle { width: 1, height: 1 },
            max: Rectangle {
                width: 8192,
                height: 8192,
            },
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

    let object = Object {
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
            Property::new(
                spa::param::format::FormatProperties::VideoFormat.as_raw(),
                format_choice,
            ),
            Property::new(spa::param::format::FormatProperties::VideoSize.as_raw(), size_choice),
            Property {
                key: spa::param::format::FormatProperties::VideoFramerate.as_raw(),
                flags: PropertyFlags::empty(),
                value: framerate_choice,
            },
        ],
    };

    let bytes = PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Object(object))
        .map_err(|e| FluxError::Capture(format!("failed to serialize format pod: {e}")))?
        .0
        .into_inner();

    Ok(vec![OwnedPod(bytes)])
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
fn build_frame(datas: &mut [spa::buffer::Data], negotiated: &NegotiatedFormat, sequence: u64) -> Option<CapturedFrame> {
    if datas.is_empty() {
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
        DataType::MemFd | DataType::MemPtr => build_shm_frame(datas, base),
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

fn build_shm_frame(datas: &mut [spa::buffer::Data], mut base: CapturedFrame) -> Option<CapturedFrame> {
    let chunk_size = datas[0].chunk().size() as usize;
    let mapped = datas[0].data()?;
    let len = if chunk_size > 0 && chunk_size <= mapped.len() {
        chunk_size
    } else {
        mapped.len()
    };
    if len == 0 {
        return None;
    }
    base.data = mapped[..len].to_vec();
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
