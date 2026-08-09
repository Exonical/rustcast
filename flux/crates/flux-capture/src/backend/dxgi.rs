//! Windows Desktop Duplication API (DXGI) screen capture backend.
//!
//! Uses the IDXGIOutputDuplication interface to capture frames directly from
//! the GPU. Provides both a GPU zero-copy path (shared texture handle for
//! hardware encoders) and a CPU fallback path (staging texture map/read).

use flux_core::error::{FluxError, Result};
use flux_core::frame::CapturedFrame;
use flux_core::types::{PixelFormat, Resolution};

use crate::traits::{CaptureSession, DisplayInfo, ScreenCapture};

use std::collections::HashMap;
use std::sync::Mutex;

use windows::Win32::Foundation::HMODULE;
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_UNKNOWN;
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::UI::HiDpi::{
    SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
};
use windows::core::Interface;

/// DXGI Desktop Duplication capture backend.
pub struct DxgiCapture {
    state: Mutex<DxgiState>,
}

struct DxgiState {
    /// Held to keep the factory that produced `targets` alive for as long as
    /// those adapter-derived objects are cached.
    #[allow(dead_code)]
    factory: IDXGIFactory1,
    inventory_key: Vec<String>,
    displays: Vec<DisplayInfo>,
    targets: HashMap<u32, DxgiDisplayTarget>,
}

/// The complete adapter/output/device tuple selected during enumeration.
///
/// Keeping these COM objects together prevents `start_capture` from
/// re-resolving an output on a different adapter than the one tested during
/// enumeration.
struct DxgiDisplayTarget {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    output: IDXGIOutput,
    capture_supported: bool,
}

impl DxgiCapture {
    pub fn new() -> Result<Self> {
        tracing::info!("Initializing DXGI Desktop Duplication capture");

        unsafe {
            // Declare per-monitor DPI awareness so display enumeration reports
            // native pixel resolutions. Without this, Windows virtualizes
            // DesktopCoordinates by the scale factor (e.g. a 2560x1600 panel at
            // 150% shows up as 1707x1067) and the stream is silently captured
            // at reduced resolution. Fails harmlessly if awareness was already
            // set (e.g. via manifest).
            if let Err(e) = SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) {
                tracing::debug!("SetProcessDpiAwarenessContext: {} (already set?)", e);
            }

            let factory: IDXGIFactory1 = CreateDXGIFactory1()
                .map_err(|e| FluxError::Capture(format!("CreateDXGIFactory1 failed: {}", e)))?;
            let capture = Self {
                state: Mutex::new(DxgiState {
                    factory,
                    inventory_key: Vec::new(),
                    displays: Vec::new(),
                    targets: HashMap::new(),
                }),
            };

            // Emit the complete inventory once for each backend instance.
            // The virtual-display probe creates one instance before plug-in
            // and the capture loop creates another after plug-in, without
            // making the 100 ms identity polling loop noisy.
            let _ = capture.enumerate_displays_internal(true)?;
            Ok(capture)
        }
    }

    fn inventory_key(factory: &IDXGIFactory1) -> Result<Vec<String>> {
        let mut key = Vec::new();
        let mut adapter_index = 0u32;

        unsafe {
            while let Ok(adapter) = factory.EnumAdapters1(adapter_index) {
                let desc = adapter
                    .GetDesc1()
                    .map_err(|e| FluxError::Capture(format!("GetDesc1 failed: {}", e)))?;
                let adapter_luid =
                    u64::from(desc.AdapterLuid.LowPart) | ((desc.AdapterLuid.HighPart as i64 as u64) << 32);
                let adapter_name_len =
                    desc.Description.iter().position(|&c| c == 0).unwrap_or(128);
                let adapter_name =
                    String::from_utf16_lossy(&desc.Description[..adapter_name_len]);
                let software = desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0;
                let mut adapter_key = format!(
                    "adapter:{}:{}:{}:{}:{}:{}:{}",
                    adapter_index,
                    adapter_luid,
                    adapter_name,
                    desc.VendorId,
                    desc.DeviceId,
                    desc.Flags,
                    desc.DedicatedVideoMemory,
                );

                if !software {
                    let mut output_index = 0u32;
                    while let Ok(output) = adapter.EnumOutputs(output_index) {
                        let output_desc = output
                            .GetDesc()
                            .map_err(|e| FluxError::Capture(format!("GetDesc failed: {}", e)))?;
                        let name_len =
                            output_desc.DeviceName.iter().position(|&c| c == 0).unwrap_or(32);
                        let name =
                            String::from_utf16_lossy(&output_desc.DeviceName[..name_len]);
                        let rect = output_desc.DesktopCoordinates;
                        adapter_key.push_str(&format!(
                            "|output:{}:{}:{}:{}:{}:{}:{}:{}:{}:{}",
                            output_index,
                            name,
                            (rect.right - rect.left).max(0),
                            (rect.bottom - rect.top).max(0),
                            output_desc.AttachedToDesktop.as_bool(),
                            output_desc.Rotation.0,
                            rect.left,
                            rect.top,
                            rect.right,
                            rect.bottom,
                        ));
                        output_index += 1;
                    }
                }
                key.push(adapter_key);
                adapter_index += 1;
            }
        }

        Ok(key)
    }

    fn enumerate_displays_internal(&self, diagnostics: bool) -> Result<Vec<DisplayInfo>> {
        // DXGI factories snapshot the adapter/output set at creation. A new
        // factory is therefore required while polling for a hot-plugged
        // indirect display; the cached COM generation is replaced below only
        // when the inventory key changes.
        let fresh_factory: IDXGIFactory1 = unsafe { CreateDXGIFactory1() }
            .map_err(|e| FluxError::Capture(format!("CreateDXGIFactory1 failed: {}", e)))?;
        let fresh_key = Self::inventory_key(&fresh_factory)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| FluxError::Capture("DXGI state cache poisoned".into()))?;
        if state.inventory_key == fresh_key {
            return Ok(state.displays.clone());
        }
        let log_inventory = diagnostics || !state.inventory_key.is_empty();
        let factory = &fresh_factory;
        let mut displays = Vec::new();
        let mut targets = HashMap::new();
        let mut adapter_index = 0u32;
        let mut attached_display_count = 0u32;

        unsafe {
            while let Ok(adapter) = factory.EnumAdapters1(adapter_index) {
                let desc = adapter
                    .GetDesc1()
                    .map_err(|e| FluxError::Capture(format!("GetDesc1 failed: {}", e)))?;
                let adapter_luid =
                    u64::from(desc.AdapterLuid.LowPart) | ((desc.AdapterLuid.HighPart as i64 as u64) << 32);
                let adapter_name_len =
                    desc.Description.iter().position(|&c| c == 0).unwrap_or(128);
                let adapter_name =
                    String::from_utf16_lossy(&desc.Description[..adapter_name_len]);

                if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
                    if log_inventory {
                        tracing::info!(
                            "DXGI Adapter {}: {} (LUID=0x{:016X}, VendorID=0x{:04X}, DeviceID=0x{:04X}, software=true; skipped)",
                            adapter_index,
                            adapter_name,
                            adapter_luid,
                            desc.VendorId,
                            desc.DeviceId,
                        );
                    }
                    adapter_index += 1;
                    continue;
                }

                let mut device = None;
                let mut context = None;
                // Create a device explicitly from each adapter. Do not rely
                // on DXGI's default/hybrid-GPU preference: output reparenting
                // can place an indirect-display output on a different adapter
                // than the one selected by D3D11CreateDevice(None, ...).
                let device_result = D3D11CreateDevice(
                    &adapter,
                    D3D_DRIVER_TYPE_UNKNOWN,
                    HMODULE::default(),
                    D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                    None,
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    Some(&mut context),
                );
                if let Err(e) = device_result {
                    tracing::warn!(
                        "DXGI Adapter {} ({}): D3D11 device creation failed; outputs will be diagnostic-only: {}",
                        adapter_index,
                        adapter_name,
                        e,
                    );
                }
                let device = device;
                let context = context;

                if log_inventory {
                    tracing::info!(
                        "DXGI Adapter {}: {} (LUID=0x{:016X}, VendorID=0x{:04X}, DeviceID=0x{:04X}, VRAM={} MB)",
                        adapter_index,
                        adapter_name,
                        adapter_luid,
                        desc.VendorId,
                        desc.DeviceId,
                        desc.DedicatedVideoMemory / (1024 * 1024),
                    );
                }

                let mut output_index = 0u32;
                while let Ok(output) = adapter.EnumOutputs(output_index) {
                    let output_desc = output
                        .GetDesc()
                        .map_err(|e| FluxError::Capture(format!("GetDesc failed: {}", e)))?;
                    let name_len =
                        output_desc.DeviceName.iter().position(|&c| c == 0).unwrap_or(32);
                    let name =
                        String::from_utf16_lossy(&output_desc.DeviceName[..name_len]);
                    let rect = output_desc.DesktopCoordinates;
                    let width = (rect.right - rect.left).max(0) as u32;
                    let height = (rect.bottom - rect.top).max(0) as u32;

                    let (capture_supported, duplication_error) = match (&device, &context) {
                        (Some(device), Some(_context)) => match output.cast::<IDXGIOutput1>() {
                            Ok(output1) => match output1.DuplicateOutput(device) {
                                Ok(duplication) => {
                                    // Release the test duplication before
                                    // storing the output. DXGI permits only a
                                    // limited number of duplications per
                                    // output, so never retain this probe.
                                    drop(duplication);
                                    (true, None)
                                }
                                Err(e) => {
                                    let error = e.to_string();
                                    tracing::debug!(
                                        "DXGI Adapter {} output {} ({}): DuplicateOutput test failed: {}",
                                        adapter_index,
                                        output_index,
                                        name,
                                        error,
                                    );
                                    (false, Some(error))
                                }
                            },
                            Err(e) => {
                                let error = e.to_string();
                                tracing::debug!(
                                    "DXGI Adapter {} output {} ({}): IDXGIOutput1 unavailable: {}",
                                    adapter_index,
                                    output_index,
                                    name,
                                    error,
                                );
                                (false, Some(error))
                            }
                        },
                        _ => (false, Some("D3D11 device unavailable".into())),
                    };

                    if log_inventory {
                        match duplication_error.as_deref() {
                            Some(error) => tracing::info!(
                                "DXGI Adapter {} Output {}: {} ({}x{}, attached={}, rotation={:?}, desktop=({},{})-({},{}), duplicate_output=false, error={})",
                                adapter_index,
                                output_index,
                                name,
                                width,
                                height,
                                output_desc.AttachedToDesktop.as_bool(),
                                output_desc.Rotation,
                                rect.left,
                                rect.top,
                                rect.right,
                                rect.bottom,
                                error,
                            ),
                            None => tracing::info!(
                                "DXGI Adapter {} Output {}: {} ({}x{}, attached={}, rotation={:?}, desktop=({},{})-({},{}), duplicate_output=true)",
                                adapter_index,
                                output_index,
                                name,
                                width,
                                height,
                                output_desc.AttachedToDesktop.as_bool(),
                                output_desc.Rotation,
                                rect.left,
                                rect.top,
                                rect.right,
                                rect.bottom,
                            ),
                        }
                    }

                    if output_desc.AttachedToDesktop.as_bool() {
                        let display_id = (adapter_index << 16) | output_index;
                        displays.push(DisplayInfo {
                            id: display_id,
                            adapter_luid: Some(adapter_luid),
                            name,
                            native_resolution: Resolution::new(width, height),
                            primary: attached_display_count == 0,
                            capture_supported,
                        });
                        attached_display_count += 1;

                        if let (Some(device), Some(context)) = (device.clone(), context.clone()) {
                            targets.insert(
                                display_id,
                                DxgiDisplayTarget {
                                    device,
                                    context,
                                    output: output.clone(),
                                    capture_supported,
                                },
                            );
                        }
                    }

                    output_index += 1;
                }

                adapter_index += 1;
            }
        }

        state.factory = fresh_factory;
        state.inventory_key = fresh_key;
        state.displays = displays.clone();
        state.targets = targets;
        Ok(displays)
    }
}

impl ScreenCapture for DxgiCapture {
    fn name(&self) -> &'static str {
        "DXGI Desktop Duplication"
    }

    fn enumerate_displays(&self) -> Result<Vec<DisplayInfo>> {
        let displays = self.enumerate_displays_internal(false)?;
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
        let state = self
            .state
            .lock()
            .map_err(|_| FluxError::Capture("DXGI state cache poisoned".into()))?;
        let target = state
            .targets
            .get(&display_id)
            .ok_or_else(|| FluxError::Capture(format!("DXGI display {} was not enumerated", display_id)))?;
        if !target.capture_supported {
            return Err(FluxError::Capture(format!(
                "DXGI display {} failed the DuplicateOutput capability test",
                display_id
            )));
        }
        tracing::info!(
            "Starting DXGI capture on display {} at {}@{}fps (adapter/output pair from enumeration)",
            display_id,
            resolution,
            framerate
        );

        Ok(Box::new(DxgiCaptureSession::new(
            &target.device,
            &target.context,
            &target.output,
            display_id,
            resolution,
            framerate,
        )?))
    }
}

/// GPU downscaler: a D3D11 video processor that blits the desktop-sized
/// capture into a smaller shared texture, so hardware encoders with a lower
/// maximum coded size (e.g. 4096x4096 for H.264 on AMD VCN) can consume the
/// frame without any CPU copy.
struct GpuScaler {
    video_device: ID3D11VideoDevice,
    video_context: ID3D11VideoContext,
    processor: ID3D11VideoProcessor,
    enumerator: ID3D11VideoProcessorEnumerator,
    output_view: ID3D11VideoProcessorOutputView,
    /// Lazily-created desktop-sized intermediate, used only if an input view
    /// can't be created directly on the acquired duplication texture.
    fallback: std::cell::RefCell<Option<(ID3D11Texture2D, ID3D11VideoProcessorInputView)>>,
    input: Resolution,
}

impl GpuScaler {
    fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        shared_texture: &ID3D11Texture2D,
        input: Resolution,
        output: Resolution,
    ) -> Result<Self> {
        unsafe {
            let video_device: ID3D11VideoDevice = device.cast()
                .map_err(|e| FluxError::Capture(format!("Cast to ID3D11VideoDevice: {}", e)))?;
            let video_context: ID3D11VideoContext = context.cast()
                .map_err(|e| FluxError::Capture(format!("Cast to ID3D11VideoContext: {}", e)))?;

            let content_desc = D3D11_VIDEO_PROCESSOR_CONTENT_DESC {
                InputFrameFormat: D3D11_VIDEO_FRAME_FORMAT_PROGRESSIVE,
                InputFrameRate: DXGI_RATIONAL { Numerator: 60, Denominator: 1 },
                InputWidth: input.width,
                InputHeight: input.height,
                OutputFrameRate: DXGI_RATIONAL { Numerator: 60, Denominator: 1 },
                OutputWidth: output.width,
                OutputHeight: output.height,
                Usage: D3D11_VIDEO_USAGE_PLAYBACK_NORMAL,
            };
            let enumerator = video_device.CreateVideoProcessorEnumerator(&content_desc)
                .map_err(|e| FluxError::Capture(format!("CreateVideoProcessorEnumerator: {}", e)))?;
            let processor = video_device.CreateVideoProcessor(&enumerator, 0)
                .map_err(|e| FluxError::Capture(format!("CreateVideoProcessor: {}", e)))?;

            let output_view_desc = D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC {
                ViewDimension: D3D11_VPOV_DIMENSION_TEXTURE2D,
                Anonymous: D3D11_VIDEO_PROCESSOR_OUTPUT_VIEW_DESC_0 {
                    Texture2D: D3D11_TEX2D_VPOV { MipSlice: 0 },
                },
            };
            let mut output_view = None;
            video_device.CreateVideoProcessorOutputView(shared_texture, &enumerator, &output_view_desc, Some(&mut output_view))
                .map_err(|e| FluxError::Capture(format!("CreateVideoProcessorOutputView: {}", e)))?;
            let output_view = output_view
                .ok_or_else(|| FluxError::Capture("Scaler output view is null".into()))?;

            tracing::info!("GPU scaler created: {} → {} (D3D11 video processor)", input, output);

            Ok(Self {
                video_device,
                video_context,
                processor,
                enumerator,
                output_view,
                fallback: std::cell::RefCell::new(None),
                input,
            })
        }
    }

    /// Blit the acquired desktop texture, scaled, into the shared output
    /// texture. Prefers a zero-copy blit straight from the duplication
    /// texture; falls back to copying through a desktop-sized intermediate
    /// if the driver refuses an input view on it.
    fn scale(&self, device: &ID3D11Device, context: &ID3D11DeviceContext, desktop_texture: &ID3D11Texture2D) -> Result<()> {
        let input_view_desc = D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC {
            FourCC: 0,
            ViewDimension: D3D11_VPIV_DIMENSION_TEXTURE2D,
            Anonymous: D3D11_VIDEO_PROCESSOR_INPUT_VIEW_DESC_0 {
                Texture2D: D3D11_TEX2D_VPIV { MipSlice: 0, ArraySlice: 0 },
            },
        };

        unsafe {
            let mut direct_view = None;
            let input_view = match self.video_device.CreateVideoProcessorInputView(
                desktop_texture,
                &self.enumerator,
                &input_view_desc,
                Some(&mut direct_view),
            ) {
                Ok(()) => direct_view
                    .ok_or_else(|| FluxError::Capture("Scaler input view is null".into()))?,
                Err(e) => {
                    // Copy through the lazily-created intermediate instead.
                    let mut fallback = self.fallback.borrow_mut();
                    if fallback.is_none() {
                        tracing::info!(
                            "Direct input view on duplication texture unavailable ({}); using intermediate copy",
                            e
                        );
                        let input_desc = D3D11_TEXTURE2D_DESC {
                            Width: self.input.width,
                            Height: self.input.height,
                            MipLevels: 1,
                            ArraySize: 1,
                            Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                            Usage: D3D11_USAGE_DEFAULT,
                            BindFlags: (D3D11_BIND_RENDER_TARGET.0 | D3D11_BIND_SHADER_RESOURCE.0) as u32,
                            CPUAccessFlags: 0,
                            MiscFlags: 0,
                        };
                        let mut input_texture = None;
                        device.CreateTexture2D(&input_desc, None, Some(&mut input_texture))
                            .map_err(|e| FluxError::Capture(format!("CreateTexture2D scaler input: {}", e)))?;
                        let input_texture = input_texture
                            .ok_or_else(|| FluxError::Capture("Scaler input texture is null".into()))?;
                        let mut view = None;
                        self.video_device.CreateVideoProcessorInputView(&input_texture, &self.enumerator, &input_view_desc, Some(&mut view))
                            .map_err(|e| FluxError::Capture(format!("CreateVideoProcessorInputView: {}", e)))?;
                        let view = view
                            .ok_or_else(|| FluxError::Capture("Scaler input view is null".into()))?;
                        *fallback = Some((input_texture, view));
                    }
                    let (input_texture, view) = fallback.as_ref().unwrap().clone();
                    context.CopyResource(&input_texture, desktop_texture);
                    view
                }
            };

            let stream = D3D11_VIDEO_PROCESSOR_STREAM {
                Enable: true.into(),
                OutputIndex: 0,
                InputFrameOrField: 0,
                PastFrames: 0,
                FutureFrames: 0,
                ppPastSurfaces: std::ptr::null_mut(),
                pInputSurface: std::mem::ManuallyDrop::new(Some(input_view)),
                ppFutureSurfaces: std::ptr::null_mut(),
                ppPastSurfacesRight: std::ptr::null_mut(),
                pInputSurfaceRight: std::mem::ManuallyDrop::new(None),
                ppFutureSurfacesRight: std::ptr::null_mut(),
            };
            self.video_context.VideoProcessorBlt(&self.processor, &self.output_view, 0, &[stream])
                .map_err(|e| FluxError::Capture(format!("VideoProcessorBlt: {}", e)))
        }
    }
}

/// An active DXGI duplication session.
struct DxgiCaptureSession {
    device: ID3D11Device,
    context: ID3D11DeviceContext,
    duplication: IDXGIOutputDuplication,
    shared_texture: ID3D11Texture2D,
    shared_handle: u64,
    display_id: u32,
    resolution: Resolution,
    scaler: Option<GpuScaler>,
    frame_interval: std::time::Duration,
    frame_sequence: u64,
    running: bool,
    last_frame_time: std::time::Instant,
    last_delivery: std::time::Instant,
}

struct AcquiredFrame {
    duplication: IDXGIOutputDuplication,
    released: bool,
}

impl AcquiredFrame {
    fn new(duplication: &IDXGIOutputDuplication) -> Self {
        Self {
            duplication: duplication.clone(),
            released: false,
        }
    }

    fn release(mut self) -> Result<()> {
        let result = unsafe { self.duplication.ReleaseFrame() };
        self.released = true;
        match result {
            Ok(()) => Ok(()),
            Err(error) => {
                let code = error.code().0 as u32;
                if code == 0x887A0026 {
                    return Err(FluxError::CaptureSessionLost(
                        "Desktop Duplication access lost while releasing frame".into(),
                    ));
                }
                tracing::warn!(
                    "DXGI ReleaseFrame failed (0x{code:08X}): {error}"
                );
                Err(FluxError::Capture(format!("ReleaseFrame: {error}")))
            }
        }
    }
}

impl Drop for AcquiredFrame {
    fn drop(&mut self) {
        if self.released {
            return;
        }
        if let Err(error) = unsafe { self.duplication.ReleaseFrame() } {
            tracing::error!("DXGI ReleaseFrame during frame-guard drop failed: {error}");
        }
    }
}

impl DxgiCaptureSession {
    fn new(
        device: &ID3D11Device,
        context: &ID3D11DeviceContext,
        output: &IDXGIOutput,
        display_id: u32,
        requested_resolution: Resolution,
        framerate: u32,
    ) -> Result<Self> {
        unsafe {
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

            // Output at the requested resolution when it's smaller than the
            // desktop (GPU downscale for encoders with a lower maximum coded
            // size); otherwise pass the desktop through at native size.
            let desktop = Resolution::new(dup_desc.ModeDesc.Width, dup_desc.ModeDesc.Height);
            let output = if requested_resolution.width > 0
                && requested_resolution.height > 0
                && (requested_resolution.width < desktop.width || requested_resolution.height < desktop.height)
            {
                requested_resolution
            } else {
                desktop
            };

            // Create a shared texture for GPU zero-copy access
            let tex_desc = D3D11_TEXTURE2D_DESC {
                Width: output.width,
                Height: output.height,
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

            let scaler = if output != desktop {
                Some(GpuScaler::new(device, context, &shared_texture, desktop, output)?)
            } else {
                None
            };

            let frame_interval = std::time::Duration::from_micros(1_000_000 / framerate as u64);

            Ok(Self {
                device: device.clone(),
                context: context.clone(),
                duplication,
                shared_texture,
                shared_handle: shared_handle_val,
                display_id,
                resolution: output,
                scaler,
                frame_interval,
                frame_sequence: 0,
                running: true,
                last_frame_time: std::time::Instant::now(),
                last_delivery: std::time::Instant::now(),
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
                        return Err(FluxError::CaptureSessionLost(
                            "Desktop Duplication access lost — display mode changed".into(),
                        ));
                    }
                    // DXGI_ERROR_INVALID_CALL — the previous frame was not released.
                    if code == 0x887A0001 {
                        return Err(FluxError::CaptureSessionLost(
                            "Desktop Duplication frame was not released before the next acquisition".into(),
                        ));
                    }
                    return Err(FluxError::Capture(format!("AcquireNextFrame: {}", e)));
                }
            }

            let frame_guard = AcquiredFrame::new(&self.duplication);
            let resource = resource.ok_or_else(|| FluxError::Capture("Frame resource is null".into()))?;

            // LastPresentTime == 0 means the desktop image was unchanged since the last acquisition.
            if frame_info.LastPresentTime == 0
                && self.last_delivery.elapsed() < std::time::Duration::from_secs(1)
            {
                frame_guard.release()?;
                return Ok(None);
            }

            // Get the desktop texture
            let desktop_texture: ID3D11Texture2D = resource.cast()
                .map_err(|e| FluxError::Capture(format!("Cast to ID3D11Texture2D: {}", e)))?;

            // Copy to shared texture: direct GPU copy at native size, or a
            // video-processor blit when downscaling.
            match &self.scaler {
                Some(scaler) => {
                    if let Err(e) = scaler.scale(&self.device, &self.context, &desktop_texture) {
                        if let Err(release_error) = frame_guard.release() {
                            tracing::warn!("GPU scale failed before release error: {e}");
                            return Err(release_error);
                        }
                        return Err(e);
                    }
                }
                None => self.context.CopyResource(&self.shared_texture, &desktop_texture),
            }

            // Flush to ensure the GPU copy is submitted before the encoder
            // reads from this texture on a different D3D11 device.
            self.context.Flush();

            frame_guard.release()?;

            self.frame_sequence += 1;
            self.last_delivery = std::time::Instant::now();

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
