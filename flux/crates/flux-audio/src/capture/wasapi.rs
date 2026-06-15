//! Windows WASAPI loopback audio capture.
//!
//! Uses the Windows Audio Session API in loopback mode to capture the audio
//! output of the desktop (or a specific application). This is the standard
//! approach used by Sunshine, Parsec, and Windows RDP.

use flux_core::error::{FluxError, Result};

use crate::traits::{AudioCaptureSession, AudioDeviceInfo, AudioSamples};

/// WASAPI loopback capture session.
pub struct WasapiCapture {
    running: bool,
    sequence: u64,
    // TODO: COM objects:
    //   - IMMDeviceEnumerator
    //   - IMMDevice (render endpoint)
    //   - IAudioClient (initialized in loopback mode)
    //   - IAudioCaptureClient (for reading packets)
    //   - WAVEFORMATEX (negotiated format)
}

impl WasapiCapture {
    pub fn new() -> Result<Self> {
        tracing::info!("Initializing WASAPI audio capture");

        // TODO: Initialization:
        //   1. CoInitializeEx(COINIT_MULTITHREADED)
        //   2. CoCreateInstance(CLSID_MMDeviceEnumerator) → IMMDeviceEnumerator
        //   3. Enumerator ready for use

        Ok(Self {
            running: false,
            sequence: 0,
        })
    }
}

impl AudioCaptureSession for WasapiCapture {
    fn enumerate_devices(&self) -> Result<Vec<AudioDeviceInfo>> {
        // TODO: Real enumeration:
        //   enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)
        //   For each endpoint: GetId(), OpenPropertyStore → PKEY_Device_FriendlyName

        tracing::debug!("Enumerating WASAPI audio devices (stub)");
        Ok(vec![AudioDeviceInfo {
            id: "default".into(),
            name: "Default Audio Output".into(),
            sample_rate: 48000,
            channels: 2,
            is_default: true,
        }])
    }

    fn start(&mut self, device_id: Option<&str>) -> Result<()> {
        tracing::info!("Starting WASAPI loopback capture (device={:?})", device_id);

        // TODO: Full capture start:
        //
        //   1. Get device:
        //      - If device_id is Some: enumerator.GetDevice(id)
        //      - If None: enumerator.GetDefaultAudioEndpoint(eRender, eConsole)
        //
        //   2. device.Activate(IAudioClient) → audio_client
        //
        //   3. audio_client.GetMixFormat() → WAVEFORMATEX (get the native format)
        //
        //   4. audio_client.Initialize(
        //        AUDCLNT_SHAREMODE_SHARED,
        //        AUDCLNT_STREAMFLAGS_LOOPBACK | AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
        //        buffer_duration,  // e.g. 10ms = 100000 (100ns units)
        //        0,                // periodicity (0 for shared mode)
        //        &wave_format,
        //        NULL
        //      )
        //
        //   5. audio_client.GetService(IAudioCaptureClient) → capture_client
        //
        //   6. Create event handle for buffer-ready notifications
        //      audio_client.SetEventHandle(event)
        //
        //   7. audio_client.Start()

        self.running = true;
        Ok(())
    }

    fn next_samples(&mut self) -> Result<AudioSamples> {
        if !self.running {
            return Err(FluxError::AudioCapture("not started".into()));
        }

        self.sequence += 1;

        // TODO: Read from WASAPI capture client:
        //
        //   1. WaitForSingleObject(event, timeout) — wait for buffer
        //   2. capture_client.GetNextPacketSize() → frames_available
        //   3. While frames_available > 0:
        //      a. capture_client.GetBuffer(&data, &frames, &flags, &position, &qpc)
        //      b. If flags & AUDCLNT_BUFFERFLAGS_SILENT → fill with zeros
        //      c. Copy / convert data to f32 interleaved
        //      d. capture_client.ReleaseBuffer(frames)
        //      e. GetNextPacketSize again

        Ok(AudioSamples {
            data: Vec::new(),
            sample_rate: 48000,
            channels: 2,
            sequence: self.sequence,
            timestamp: std::time::Instant::now(),
        })
    }

    fn stop(&mut self) -> Result<()> {
        tracing::info!("Stopping WASAPI capture");
        // TODO: audio_client.Stop(), release COM objects
        self.running = false;
        Ok(())
    }
}
