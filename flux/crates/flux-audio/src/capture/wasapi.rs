//! Windows WASAPI loopback audio capture (not implemented).
//!
//! Will use the Windows Audio Session API in loopback mode to capture the audio
//! output of the desktop (or a specific application). This is the standard
//! approach used by Sunshine, Parsec, and Windows RDP. Until it exists,
//! construction fails so the host runs without audio and says so, instead of
//! reporting a running capture that never yields a sample.

use flux_core::error::{FluxError, Result};

use crate::traits::{AudioCaptureSession, AudioDeviceInfo, AudioSamples};

const NOT_IMPLEMENTED: &str = "WASAPI loopback capture not implemented";

/// WASAPI loopback capture session — not implemented.
pub struct WasapiCapture {
    _private: (),
    // TODO: COM objects:
    //   - IMMDeviceEnumerator
    //   - IMMDevice (render endpoint)
    //   - IAudioClient (initialized in loopback mode)
    //   - IAudioCaptureClient (for reading packets)
    //   - WAVEFORMATEX (negotiated format)
}

impl WasapiCapture {
    pub fn new() -> Result<Self> {
        // TODO: Initialization:
        //   1. CoInitializeEx(COINIT_MULTITHREADED)
        //   2. CoCreateInstance(CLSID_MMDeviceEnumerator) → IMMDeviceEnumerator
        //   3. Enumerator ready for use

        Err(FluxError::AudioCapture(NOT_IMPLEMENTED.into()))
    }
}

impl AudioCaptureSession for WasapiCapture {
    fn enumerate_devices(&self) -> Result<Vec<AudioDeviceInfo>> {
        // TODO: Real enumeration:
        //   enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE)
        //   For each endpoint: GetId(), OpenPropertyStore → PKEY_Device_FriendlyName

        Err(FluxError::AudioCapture(NOT_IMPLEMENTED.into()))
    }

    fn start(&mut self, _device_id: Option<&str>) -> Result<()> {
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

        Err(FluxError::AudioCapture(NOT_IMPLEMENTED.into()))
    }

    fn next_samples(&mut self) -> Result<AudioSamples> {
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

        Err(FluxError::AudioCapture(NOT_IMPLEMENTED.into()))
    }

    fn stop(&mut self) -> Result<()> {
        // TODO: audio_client.Stop(), release COM objects
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn construction_fails_instead_of_faking_success() {
        assert!(matches!(WasapiCapture::new(), Err(FluxError::AudioCapture(_))));
        assert!(matches!(crate::create_audio_capture(), Err(FluxError::AudioCapture(_))));
    }
}
