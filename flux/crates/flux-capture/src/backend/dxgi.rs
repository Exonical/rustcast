//! Windows Desktop Duplication API (DXGI) screen capture backend.
//!
//! Uses the IDXGIOutputDuplication interface to capture frames directly from
//! the GPU. Provides both a GPU zero-copy path (shared texture handle for
//! hardware encoders) and a CPU fallback path (staging texture map/read).

use flux_core::error::{FluxError, Result};
use flux_core::frame::CapturedFrame;
use flux_core::types::{PixelFormat, Resolution};

use crate::traits::{CaptureSession, DisplayInfo, ScreenCapture};

use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::core::Interface;

/// DXGI Desktop Duplication capture backend.
pub struct DxgiCapture {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    adapter: IDXGIAdapter1,
}

impl DxgiCapture {
    pub fn new() -> Result<Self> {
        tracing::info!("Initializing DXGI Desktop Duplication capture");

        unsafe {
            // Create D3D11 device with BGRA support (needed for Desktop Duplication)
            let mut device = None;
            let mut context = None;

            D3D11CreateDevice(
                None,                          // default adapter
                D3D_DRIVER_TYPE_HARDWARE,
                HMODULE::default(),            // no software rasterizer
                D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                None,                          // default feature levels
                D3D11_SDK_VERSION,
                Some(&mut device),
                None,                          // don't need selected feature level
                Some(&mut context),
            )
            .map_err(|e| FluxError::Capture(format!("D3D11CreateDevice failed: {}", e)))?;

            let device = device.ok_or_else(|| FluxError::Capture("D3D11 device is null".into()))?;
            let context =
                context.ok_or_else(|| FluxError::Capture("D3D11 context is null".into()))?;

            // Get the DXGI adapter from the device
            let dxgi_device: IDXGIDevice = device.cast()
                .map_err(|e| FluxError::Capture(format!("QueryInterface IDXGIDevice: {}", e)))?;
            let adapter: IDXGIAdapter = dxgi_device.GetAdapter()
                .map_err(|e| FluxError::Capture(format!("GetAdapter: {}", e)))?;
            let adapter: IDXGIAdapter1 = adapter.cast()
                .map_err(|e| FluxError::Capture(format!("QueryInterface IDXGIAdapter1: {}", e)))?;

            let desc = adapter.GetDesc1()
                .map_err(|e| FluxError::Capture(format!("GetDesc1: {}", e)))?;
            let name_len = desc.Description.iter().position(|&c| c == 0).unwrap_or(128);
            let adapter_name = String::from_utf16_lossy(&desc.Description[..name_len]);
            tracing::info!("DXGI capture using adapter: {}", adapter_name);

            Ok(Self { device, context, adapter })
        }
    }
}

impl ScreenCapture for DxgiCapture {
    fn name(&self) -> &'static str {
        "DXGI Desktop Duplication"
    }

    fn enumerate_displays(&self) -> Result<Vec<DisplayInfo>> {
        let mut displays = Vec::new();
        let mut output_index: u32 = 0;

        unsafe {
            loop {
                let output = match self.adapter.EnumOutputs(output_index) {
                    Ok(o) => o,
                    Err(_) => break, // DXGI_ERROR_NOT_FOUND — no more outputs
                };

                let desc = output.GetDesc()
                    .map_err(|e| FluxError::Capture(format!("GetDesc: {}", e)))?;

                if !desc.AttachedToDesktop.as_bool() {
                    output_index += 1;
                    continue;
                }

                let rect = desc.DesktopCoordinates;
                let width = (rect.right - rect.left) as u32;
                let height = (rect.bottom - rect.top) as u32;

                let name_len = desc.DeviceName.iter().position(|&c| c == 0).unwrap_or(32);
                let name = String::from_utf16_lossy(&desc.DeviceName[..name_len]);

                tracing::info!(
                    "Display {}: {} ({}x{}, primary={})",
                    output_index, name, width, height, output_index == 0
                );

                displays.push(DisplayInfo {
                    id: output_index,
                    name,
                    native_resolution: Resolution::new(width, height),
                    primary: output_index == 0,
                });

                output_index += 1;
            }
        }

        if displays.is_empty() {
            return Err(FluxError::Capture("No attached displays found".into()));
        }

        Ok(displays)
    }

    fn start_capture(
        &self,
        display_id: Option<u32>,
        resolution: Resolution,
        framerate: u32,
    ) -> Result<Box<dyn CaptureSession>> {
        let display_id = display_id.unwrap_or(0);
        tracing::info!(
            "Starting DXGI capture on display {} at {}@{}fps",
            display_id,
            resolution,
            framerate
        );

        Ok(Box::new(DxgiCaptureSession::new(
            &self.device,
            &self.context,
            &self.adapter,
            display_id,
            resolution,
            framerate,
        )?))
    }
}

/// An active DXGI duplication session.
struct DxgiCaptureSession {
    _device: ID3D11Device,
    context: ID3D11DeviceContext,
    duplication: IDXGIOutputDuplication,
    shared_texture: ID3D11Texture2D,
    shared_handle: u64,
    display_id: u32,
    resolution: Resolution,
    frame_interval: std::time::Duration,
    frame_sequence: u64,
    running: bool,
    last_frame_time: std::time::Instant,
}

impl DxgiCaptureSession {
    fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        adapter: &IDXGIAdapter1,
        display_id: u32,
        _resolution: Resolution,
        framerate: u32,
    ) -> Result<Self> {
        unsafe {
            // Get the output for this display
            let output: IDXGIOutput = adapter.EnumOutputs(display_id)
                .map_err(|e| FluxError::Capture(format!("EnumOutputs({}): {}", display_id, e)))?;

            // Need IDXGIOutput1 for DuplicateOutput
            let output1: IDXGIOutput1 = output.cast()
                .map_err(|e| FluxError::Capture(format!("Cast to IDXGIOutput1: {}", e)))?;

            // Create the output duplication
            let duplication = output1.DuplicateOutput(device)
                .map_err(|e| FluxError::Capture(format!("DuplicateOutput: {}", e)))?;

            // Get the output description to know the actual size
            let dup_desc = duplication.GetDesc();

            tracing::info!(
                "Desktop Duplication created: {}x{} format={:?}",
                dup_desc.ModeDesc.Width,
                dup_desc.ModeDesc.Height,
                dup_desc.ModeDesc.Format
            );

            // Create a shared texture for GPU zero-copy access
            let tex_desc = D3D11_TEXTURE2D_DESC {
                Width: dup_desc.ModeDesc.Width,
                Height: dup_desc.ModeDesc.Height,
                MipLevels: 1,
                ArraySize: 1,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Usage: D3D11_USAGE_DEFAULT,
                BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                CPUAccessFlags: 0,
                MiscFlags: D3D11_RESOURCE_MISC_SHARED.0 as u32,
            };

            let mut shared_texture = None;
            device.CreateTexture2D(&tex_desc, None, Some(&mut shared_texture))
                .map_err(|e| FluxError::Capture(format!("CreateTexture2D shared: {}", e)))?;
            let shared_texture = shared_texture
                .ok_or_else(|| FluxError::Capture("Shared texture is null".into()))?;

            // Cache the shared handle — it's the same for the lifetime of this texture
            let shared_resource: IDXGIResource = shared_texture.cast()
                .map_err(|e| FluxError::Capture(format!("Cast shared texture to IDXGIResource: {}", e)))?;
            let shared_handle = shared_resource.GetSharedHandle()
                .map_err(|e| FluxError::Capture(format!("GetSharedHandle: {}", e)))?;
            let shared_handle_val = shared_handle.0 as u64;
            tracing::info!("Shared texture handle: 0x{:x}", shared_handle_val);

            let frame_interval = std::time::Duration::from_micros(1_000_000 / framerate as u64);

            Ok(Self {
                _device: device.clone(),
                context: context.clone(),
                duplication,
                shared_texture,
                shared_handle: shared_handle_val,
                display_id,
                resolution: Resolution::new(dup_desc.ModeDesc.Width, dup_desc.ModeDesc.Height),
                frame_interval,
                frame_sequence: 0,
                running: true,
                last_frame_time: std::time::Instant::now(),
            })
        }
    }

    fn acquire_frame(&mut self, timeout_ms: u32) -> Result<Option<CapturedFrame>> {
        unsafe {
            let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
            let mut resource: Option<IDXGIResource> = None;

            let hr = self.duplication.AcquireNextFrame(
                timeout_ms,
                &mut frame_info,
                &mut resource,
            );

            match hr {
                Ok(()) => {}
                Err(e) => {
                    // DXGI_ERROR_WAIT_TIMEOUT — no new frame available
                    let code = e.code().0 as u32;
                    if code == 0x887A0027 {
                        return Ok(None);
                    }
                    // DXGI_ERROR_ACCESS_LOST — need to recreate duplication
                    if code == 0x887A0026 {
                        return Err(FluxError::Capture("Desktop Duplication access lost — display mode changed".into()));
                    }
                    return Err(FluxError::Capture(format!("AcquireNextFrame: {}", e)));
                }
            }

            let resource = resource.ok_or_else(|| FluxError::Capture("Frame resource is null".into()))?;

            // Get the desktop texture
            let desktop_texture: ID3D11Texture2D = resource.cast()
                .map_err(|e| FluxError::Capture(format!("Cast to ID3D11Texture2D: {}", e)))?;

            // Copy to shared texture (GPU copy)
            self.context.CopyResource(&self.shared_texture, &desktop_texture);

            // Flush to ensure the GPU copy is submitted before the encoder
            // reads from this texture on a different D3D11 device.
            self.context.Flush();

            let _ = self.duplication.ReleaseFrame();

            self.frame_sequence += 1;

            Ok(Some(CapturedFrame {
                sequence: self.frame_sequence,
                timestamp: std::time::Instant::now(),
                format: PixelFormat::Bgra8,
                resolution: self.resolution,
                stride: 0, // Not relevant for GPU frames
                data: Vec::new(), // No CPU data
                gpu_handle: Some(flux_core::frame::GpuFrameHandle::DxgiSharedTexture(
                    flux_core::frame::DxgiTextureHandle {
                        handle: self.shared_handle,
                        width: self.resolution.width,
                        height: self.resolution.height,
                    }
                )),
            }))
        }
    }
}

impl CaptureSession for DxgiCaptureSession {
    fn next_frame(&mut self) -> Result<CapturedFrame> {
        if !self.running {
            return Err(FluxError::Capture("session stopped".into()));
        }

        // Rate limit to target framerate
        let elapsed = self.last_frame_time.elapsed();
        if elapsed < self.frame_interval {
            std::thread::sleep(self.frame_interval - elapsed);
        }

        // Try with a generous timeout
        loop {
            match self.acquire_frame(100)? {
                Some(frame) => {
                    self.last_frame_time = std::time::Instant::now();
                    return Ok(frame);
                }
                None => continue, // timeout, try again
            }
        }
    }

    fn try_next_frame(&mut self) -> Result<Option<CapturedFrame>> {
        if !self.running {
            return Ok(None);
        }
        self.acquire_frame(0)
    }

    fn stop(&mut self) -> Result<()> {
        tracing::info!("Stopping DXGI capture session on display {}", self.display_id);
        self.running = false;
        Ok(())
    }
}
