//! Linux PipeWire audio capture.
//!
//! Captures audio from a PipeWire sink's monitor source. With a dedicated null
//! sink (`flux-sink`), the remote user's audio is isolated from the host's
//! speakers. The real backend lives behind the `audio-pipewire` feature; the
//! crate compiles to an inert stub without it.

#[cfg(not(feature = "audio-pipewire"))]
mod imp {
    use flux_core::error::{FluxError, Result};

    use crate::traits::{AudioCaptureSession, AudioDeviceInfo, AudioSamples};

    /// Stub capture used when the `audio-pipewire` feature is off.
    pub struct PipeWireAudioCapture;

    impl PipeWireAudioCapture {
        pub fn new() -> Result<Self> {
            Ok(Self)
        }

        pub fn set_sink(&mut self, _sink_name: &str) {}
    }

    impl AudioCaptureSession for PipeWireAudioCapture {
        fn enumerate_devices(&self) -> Result<Vec<AudioDeviceInfo>> {
            Ok(vec![])
        }

        fn start(&mut self, _device_id: Option<&str>) -> Result<()> {
            Err(FluxError::AudioCapture(
                "audio capture not available: build without feature 'audio-pipewire'".into(),
            ))
        }

        fn next_samples(&mut self) -> Result<AudioSamples> {
            Err(FluxError::AudioCapture("not started".into()))
        }

        fn stop(&mut self) -> Result<()> {
            Ok(())
        }
    }
}

#[cfg(feature = "audio-pipewire")]
mod imp {
    use std::thread::JoinHandle;
    use std::time::Instant;

    use flux_core::error::{FluxError, Result};
    use pipewire as pw;
    use pw::spa;
    use spa::param::format::{MediaSubtype, MediaType};
    use spa::param::format_utils::parse_format;
    use spa::pod::{Object, Pod, Property, Value};
    use spa::utils::{Direction, Id, SpaTypes};

    use crate::traits::{AudioCaptureSession, AudioDeviceInfo, AudioSamples};

    const SAMPLE_RATE: u32 = 48_000;
    const CHANNELS: u16 = 2;
    const QUEUE_CAP: usize = 64;

    /// PipeWire audio capture session: a stream thread feeding a bounded
    /// channel drained by `next_samples()`.
    pub struct PipeWireAudioCapture {
        sink_name: Option<String>,
        thread: Option<ThreadHandle>,
        samples_rx: Option<crossbeam_channel::Receiver<AudioSamples>>,
    }

    struct ThreadHandle {
        cmd: pw::channel::Sender<StreamCmd>,
        join: JoinHandle<()>,
    }

    enum StreamCmd {
        Quit,
    }

    fn pw_err(context: &str, e: impl std::fmt::Display) -> FluxError {
        FluxError::AudioCapture(format!("{context}: {e}"))
    }

    impl PipeWireAudioCapture {
        pub fn new() -> Result<Self> {
            tracing::info!("Initializing PipeWire audio capture");
            Ok(Self {
                sink_name: None,
                thread: None,
                samples_rx: None,
            })
        }

        /// Set a specific PipeWire sink whose monitor source to capture from.
        pub fn set_sink(&mut self, sink_name: &str) {
            self.sink_name = Some(sink_name.to_string());
        }
    }

    impl AudioCaptureSession for PipeWireAudioCapture {
        fn enumerate_devices(&self) -> Result<Vec<AudioDeviceInfo>> {
            Ok(vec![AudioDeviceInfo {
                id: self.sink_name.clone().unwrap_or_else(|| "flux-sink".into()),
                name: "Flux Remote Audio".into(),
                sample_rate: SAMPLE_RATE,
                channels: CHANNELS,
                is_default: true,
            }])
        }

        fn start(&mut self, device_id: Option<&str>) -> Result<()> {
            if self.thread.is_some() {
                return Err(FluxError::AudioCapture("capture already running".into()));
            }
            let target = device_id
                .map(str::to_string)
                .or_else(|| self.sink_name.clone())
                .unwrap_or_else(|| "flux-sink".to_string());
            tracing::info!("Starting PipeWire audio capture (target={target})");

            let (tx, rx) = crossbeam_channel::bounded(QUEUE_CAP);
            self.samples_rx = Some(rx.clone());

            let (cmd_tx, cmd_rx) = pw::channel::channel::<StreamCmd>();
            let join = std::thread::Builder::new()
                .name("flux-audio-pw".into())
                .spawn(move || {
                    if let Err(e) = run_loop(&target, tx, rx, cmd_rx) {
                        tracing::error!("PipeWire audio thread exited with error: {e}");
                    }
                })
                .map_err(|e| pw_err("failed to spawn PipeWire audio thread", e))?;
            self.thread = Some(ThreadHandle { cmd: cmd_tx, join });
            Ok(())
        }

        fn next_samples(&mut self) -> Result<AudioSamples> {
            let rx = self
                .samples_rx
                .as_ref()
                .ok_or_else(|| FluxError::AudioCapture("not started".into()))?;
            rx.recv()
                .map_err(|_| FluxError::AudioCapture("PipeWire audio stream ended".into()))
        }

        fn stop(&mut self) -> Result<()> {
            tracing::info!("Stopping PipeWire audio capture");
            if let Some(handle) = self.thread.take() {
                let _ = handle.cmd.send(StreamCmd::Quit);
                let _ = handle.join.join();
            }
            self.samples_rx = None;
            Ok(())
        }
    }

    impl Drop for PipeWireAudioCapture {
        fn drop(&mut self) {
            let _ = self.stop();
        }
    }

    /// Body of the dedicated PipeWire thread: main loop, stream, process
    /// callback pushing interleaved f32 samples into the bounded channel.
    fn run_loop(
        target: &str,
        tx: crossbeam_channel::Sender<AudioSamples>,
        rx_drain: crossbeam_channel::Receiver<AudioSamples>,
        cmd_rx: pw::channel::Receiver<StreamCmd>,
    ) -> Result<()> {
        pw::init();

        let mainloop = pw::main_loop::MainLoopRc::new(None).map_err(|e| pw_err("create main loop", e))?;
        let context = pw::context::ContextRc::new(&mainloop, None).map_err(|e| pw_err("create context", e))?;
        let core = context
            .connect_rc(None)
            .map_err(|e| pw_err("connect to PipeWire daemon", e))?;

        let stream = pw::stream::StreamRc::new(
            core,
            "flux-audio",
            pw::properties::properties! {
                *pw::keys::MEDIA_TYPE => "Audio",
                *pw::keys::MEDIA_CATEGORY => "Capture",
                *pw::keys::MEDIA_ROLE => "Music",
                *pw::keys::STREAM_CAPTURE_SINK => "true",
                "target.object" => target,
                *pw::keys::NODE_NAME => "flux-audio",
            },
        )
        .map_err(|e| pw_err("create audio stream", e))?;

        let mut sequence = 0u64;

        let _listener = stream
            .add_local_listener::<()>()
            .state_changed(|_stream, _ud, old, new| {
                tracing::debug!("PipeWire audio stream state: {old:?} -> {new:?}");
            })
            .param_changed(|_stream, _ud, id, param| {
                let Some(param) = param else { return };
                if id != spa::param::ParamType::Format.as_raw() {
                    return;
                }
                if let Ok((MediaType::Audio, MediaSubtype::Raw)) = parse_format(param) {
                    tracing::info!("PipeWire audio format negotiated (raw)");
                }
            })
            .process(move |stream, _ud| {
                if let Some(samples) = read_buffer(stream, sequence + 1) {
                    sequence += 1;
                    // Drop-oldest when the consumer is behind.
                    let mut pending = samples;
                    loop {
                        match tx.try_send(pending) {
                            Ok(()) => break,
                            Err(crossbeam_channel::TrySendError::Full(s)) => {
                                if rx_drain.try_recv().is_err() {
                                    break;
                                }
                                pending = s;
                            }
                            Err(crossbeam_channel::TrySendError::Disconnected(_)) => break,
                        }
                    }
                }
            })
            .register()
            .map_err(|e| pw_err("register audio listener", e))?;

        let params = [build_format_param()?];
        let mut param_refs: Vec<&Pod> = params.iter().map(|p| p.as_ref()).collect();
        stream
            .connect(
                Direction::Input,
                None,
                pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
                &mut param_refs,
            )
            .map_err(|e| pw_err("connect audio stream", e))?;

        let ml = mainloop.clone();
        let _cmd = cmd_rx.attach(mainloop.loop_(), move |cmd| match cmd {
            StreamCmd::Quit => ml.quit(),
        });

        mainloop.run();
        Ok(())
    }

    /// Dequeue one buffer and copy its interleaved f32 samples.
    fn read_buffer(stream: &pw::stream::Stream, sequence: u64) -> Option<AudioSamples> {
        let raw = unsafe { stream.dequeue_raw_buffer() };
        if raw.is_null() {
            return None;
        }
        struct Requeue<'s> {
            stream: &'s pw::stream::Stream,
            raw: *mut pw::sys::pw_buffer,
        }
        impl Drop for Requeue<'_> {
            fn drop(&mut self) {
                unsafe { self.stream.queue_raw_buffer(self.raw) }
            }
        }
        let _guard = Requeue { stream, raw };

        let buffer = unsafe { (*raw).buffer.as_ref() }?;
        if buffer.datas.is_null() || buffer.n_datas == 0 {
            return None;
        }
        let data = unsafe { &*buffer.datas };
        let chunk = unsafe { &*data.chunk };
        if chunk.size == 0 || chunk.flags & spa::sys::SPA_CHUNK_FLAG_CORRUPTED as i32 != 0 || data.data.is_null() {
            return None;
        }
        let bytes = unsafe { std::slice::from_raw_parts(data.data as *const u8, (chunk.offset + chunk.size) as usize) };
        let slice = &bytes[chunk.offset as usize..(chunk.offset + chunk.size) as usize];
        // Whole stereo frames only: truncate a partial trailing frame.
        let samples = slice.len() / 4;
        let samples = samples - samples % CHANNELS as usize;
        let data_out: Vec<f32> = slice[..samples * 4]
            .chunks_exact(4)
            .map(|b| f32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            .collect();
        Some(AudioSamples {
            data: data_out,
            sample_rate: SAMPLE_RATE,
            channels: CHANNELS,
            sequence,
            timestamp: Instant::now(),
        })
    }

    /// Owned, serialized SPA pod backing a `&Pod` handed to `Stream::connect`.
    struct OwnedPod(Vec<u8>);

    impl OwnedPod {
        fn as_ref(&self) -> &Pod {
            Pod::from_bytes(&self.0).expect("serialized pod is valid")
        }
    }

    /// `EnumFormat` offering F32LE / 48 kHz / stereo.
    fn build_format_param() -> Result<OwnedPod> {
        use spa::pod::serialize::PodSerializer;

        let object = Object {
            type_: SpaTypes::ObjectParamFormat.as_raw(),
            id: spa::param::ParamType::EnumFormat.as_raw(),
            properties: vec![
                Property::new(spa::sys::SPA_FORMAT_mediaType, Value::Id(Id(MediaType::Audio.as_raw()))),
                Property::new(
                    spa::sys::SPA_FORMAT_mediaSubtype,
                    Value::Id(Id(MediaSubtype::Raw.as_raw())),
                ),
                Property::new(
                    spa::sys::SPA_FORMAT_AUDIO_format,
                    Value::Id(Id(spa::sys::SPA_AUDIO_FORMAT_F32_LE)),
                ),
                Property::new(spa::sys::SPA_FORMAT_AUDIO_rate, Value::Int(SAMPLE_RATE as i32)),
                Property::new(spa::sys::SPA_FORMAT_AUDIO_channels, Value::Int(CHANNELS as i32)),
            ],
        };
        let bytes = PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Object(object))
            .map_err(|e| pw_err("failed to serialize audio format pod", e))?
            .0
            .into_inner();
        Ok(OwnedPod(bytes))
    }
}

pub use imp::PipeWireAudioCapture;
