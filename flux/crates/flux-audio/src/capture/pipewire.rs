//! Linux PipeWire / PulseAudio audio capture.
//!
//! Captures audio from a PipeWire or PulseAudio monitor source. When used with
//! a dedicated null-sink (like Moonshine does), this isolates the remote user's
//! audio from the host's speakers.

use flux_core::error::{FluxError, Result};

use crate::traits::{AudioCaptureSession, AudioDeviceInfo, AudioSamples};

/// PipeWire audio capture session.
pub struct PipeWireAudioCapture {
    running: bool,
    sequence: u64,
    sink_name: Option<String>,
    // TODO: PipeWire / PulseAudio handles:
    //   - pw_main_loop / pa_simple
    //   - pw_stream (connected to monitor source)
    //   - spa_audio_info_raw (negotiated format)
}

impl PipeWireAudioCapture {
    pub fn new() -> Result<Self> {
        tracing::info!("Initializing PipeWire audio capture");

        // TODO: Initialization options (in order of preference):
        //
        //   Option A — PipeWire native:
        //     1. pw_init()
        //     2. Create pw_main_loop + pw_context + pw_core
        //     3. Query available sinks via registry
        //
        //   Option B — PulseAudio simple API (simpler, works with PipeWire's pulse compat):
        //     1. Connect via pa_simple_new with PA_STREAM_RECORD to a .monitor source

        Ok(Self {
            running: false,
            sequence: 0,
            sink_name: None,
        })
    }

    /// Set a specific PulseAudio/PipeWire sink to capture from (monitor source).
    pub fn set_sink(&mut self, sink_name: &str) {
        self.sink_name = Some(sink_name.to_string());
    }
}

impl AudioCaptureSession for PipeWireAudioCapture {
    fn enumerate_devices(&self) -> Result<Vec<AudioDeviceInfo>> {
        // TODO: Query via `pactl list sinks` or PipeWire registry
        tracing::debug!("Enumerating PipeWire audio devices (stub)");
        Ok(vec![AudioDeviceInfo {
            id: "default".into(),
            name: "Default Audio Output".into(),
            sample_rate: 48000,
            channels: 2,
            is_default: true,
        }])
    }

    fn start(&mut self, device_id: Option<&str>) -> Result<()> {
        let source = device_id
            .map(|id| format!("{}.monitor", id))
            .or_else(|| self.sink_name.as_ref().map(|s| format!("{}.monitor", s)));

        tracing::info!("Starting PipeWire audio capture (source={:?})", source);

        // TODO: PulseAudio simple API path:
        //
        //   let spec = pa_sample_spec {
        //       format: PA_SAMPLE_FLOAT32LE,
        //       rate: 48000,
        //       channels: 2,
        //   };
        //
        //   pa_simple_new(
        //       NULL,           // server
        //       "flux",         // app name
        //       PA_STREAM_RECORD,
        //       source,         // device (e.g. "flux-sink.monitor")
        //       "capture",      // stream name
        //       &spec,
        //       NULL,           // channel map
        //       NULL,           // buffer attr
        //       &error
        //   )

        self.running = true;
        Ok(())
    }

    fn next_samples(&mut self) -> Result<AudioSamples> {
        if !self.running {
            return Err(FluxError::AudioCapture("not started".into()));
        }

        self.sequence += 1;

        // TODO: pa_simple_read(handle, buffer, size, &error)
        //   or pw_stream dequeue buffer → read samples → queue buffer

        Ok(AudioSamples {
            data: Vec::new(),
            sample_rate: 48000,
            channels: 2,
            sequence: self.sequence,
            timestamp: std::time::Instant::now(),
        })
    }

    fn stop(&mut self) -> Result<()> {
        tracing::info!("Stopping PipeWire audio capture");
        // TODO: pa_simple_free / pw_stream_disconnect
        self.running = false;
        Ok(())
    }
}
