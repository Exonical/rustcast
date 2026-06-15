use crate::types::{CaptureBackend, EncoderBackend, GpuVendor};

/// Runtime platform information detected at startup.
#[derive(Debug, Clone)]
pub struct PlatformInfo {
    pub os: Os,
    pub gpu_vendor: GpuVendor,
    pub available_capture_backends: Vec<CaptureBackend>,
    pub available_encoder_backends: Vec<EncoderBackend>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Windows,
    Linux,
}

impl PlatformInfo {
    /// Probe the current system and return detected capabilities.
    pub fn detect() -> Self {
        let os = Self::detect_os();
        let gpu_vendor = Self::detect_gpu_vendor();
        let available_capture_backends = Self::detect_capture_backends(os);
        let available_encoder_backends = Self::detect_encoder_backends(os, gpu_vendor);

        let info = Self {
            os,
            gpu_vendor,
            available_capture_backends,
            available_encoder_backends,
        };

        tracing::info!("Platform detected: {:?}", info);
        info
    }

    fn detect_os() -> Os {
        if cfg!(target_os = "windows") {
            Os::Windows
        } else {
            Os::Linux
        }
    }

    fn detect_gpu_vendor() -> GpuVendor {
        #[cfg(target_os = "windows")]
        {
            match Self::detect_gpu_vendor_dxgi() {
                Ok(vendor) => vendor,
                Err(e) => {
                    tracing::warn!("DXGI GPU detection failed: {}, falling back to Unknown", e);
                    GpuVendor::Unknown
                }
            }
        }
        #[cfg(not(target_os = "windows"))]
        {
            match Self::detect_gpu_vendor_linux() {
                Ok(vendor) => vendor,
                Err(e) => {
                    tracing::warn!("Linux GPU detection failed: {}, falling back to Unknown", e);
                    GpuVendor::Unknown
                }
            }
        }
    }

    /// Detect GPU vendor on Windows via DXGI adapter enumeration.
    #[cfg(target_os = "windows")]
    fn detect_gpu_vendor_dxgi() -> Result<GpuVendor, String> {
        use windows::Win32::Graphics::Dxgi::{CreateDXGIFactory1, IDXGIFactory1, DXGI_ADAPTER_FLAG_SOFTWARE};

        const VENDOR_AMD: u32 = 0x1002;
        const VENDOR_NVIDIA: u32 = 0x10DE;
        const VENDOR_INTEL: u32 = 0x8086;

        unsafe {
            let factory: IDXGIFactory1 = CreateDXGIFactory1()
                .map_err(|e| format!("CreateDXGIFactory1 failed: {}", e))?;

            let mut best_vendor = GpuVendor::Unknown;
            let mut best_vram: usize = 0;
            let mut adapter_index: u32 = 0;

            while let Ok(adapter) = factory.EnumAdapters1(adapter_index) {
                let desc = adapter.GetDesc1()
                    .map_err(|e| format!("GetDesc1 failed: {}", e))?;

                // Skip software adapters (Microsoft Basic Render Driver)
                if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 != 0 {
                    adapter_index += 1;
                    continue;
                }

                let vendor = match desc.VendorId {
                    VENDOR_AMD => GpuVendor::Amd,
                    VENDOR_NVIDIA => GpuVendor::Nvidia,
                    VENDOR_INTEL => GpuVendor::Intel,
                    _ => GpuVendor::Unknown,
                };

                let desc_len = desc.Description.iter().position(|&c| c == 0).unwrap_or(128);
                let desc_str = String::from_utf16_lossy(&desc.Description[..desc_len]);

                tracing::info!(
                    "DXGI Adapter {}: {} (VendorID: 0x{:04X}, VRAM: {} MB)",
                    adapter_index,
                    desc_str,
                    desc.VendorId,
                    desc.DedicatedVideoMemory / (1024 * 1024),
                );

                if desc.DedicatedVideoMemory > best_vram {
                    best_vram = desc.DedicatedVideoMemory;
                    best_vendor = vendor;
                }

                adapter_index += 1;
            }

            if best_vendor == GpuVendor::Unknown && adapter_index > 0 {
                tracing::warn!("Found {} DXGI adapter(s) but could not identify vendor", adapter_index);
            }

            Ok(best_vendor)
        }
    }

    /// Detect GPU vendor on Linux via /sys/class/drm.
    #[cfg(not(target_os = "windows"))]
    fn detect_gpu_vendor_linux() -> Result<GpuVendor, String> {
        // Check the primary GPU via /sys/class/drm/card0/device/vendor
        let vendor_path = std::path::Path::new("/sys/class/drm/card0/device/vendor");
        if let Ok(contents) = std::fs::read_to_string(vendor_path) {
            let vendor_str = contents.trim().trim_start_matches("0x");
            if let Ok(vendor_id) = u32::from_str_radix(vendor_str, 16) {
                let vendor = match vendor_id {
                    0x1002 => GpuVendor::Amd,
                    0x10DE => GpuVendor::Nvidia,
                    0x8086 => GpuVendor::Intel,
                    _ => GpuVendor::Unknown,
                };
                tracing::info!("Linux DRM GPU vendor: 0x{:04X} -> {:?}", vendor_id, vendor);
                return Ok(vendor);
            }
        }
        Err("Could not read /sys/class/drm/card0/device/vendor".into())
    }

    fn detect_capture_backends(os: Os) -> Vec<CaptureBackend> {
        match os {
            Os::Windows => vec![CaptureBackend::Dxgi],
            Os::Linux => {
                let mut backends = Vec::new();
                // PipeWire is preferred when available.
                backends.push(CaptureBackend::PipeWire);
                // DRM/KMS is the fallback for headless or Wayland-less setups.
                backends.push(CaptureBackend::Drm);
                backends
            }
        }
    }

    fn detect_encoder_backends(os: Os, vendor: GpuVendor) -> Vec<EncoderBackend> {
        let mut backends = Vec::new();

        match (os, vendor) {
            (Os::Windows, GpuVendor::Nvidia) => {
                backends.push(EncoderBackend::Nvenc);
                backends.push(EncoderBackend::VulkanVideo);
            }
            (Os::Windows, GpuVendor::Amd) => {
                backends.push(EncoderBackend::Amf);
                backends.push(EncoderBackend::VulkanVideo);
            }
            (Os::Windows, GpuVendor::Intel) => {
                backends.push(EncoderBackend::VulkanVideo);
            }
            (Os::Linux, GpuVendor::Nvidia) => {
                backends.push(EncoderBackend::Nvenc);
                backends.push(EncoderBackend::VulkanVideo);
            }
            (Os::Linux, GpuVendor::Amd | GpuVendor::Intel) => {
                backends.push(EncoderBackend::Vaapi);
                backends.push(EncoderBackend::VulkanVideo);
            }
            _ => {
                backends.push(EncoderBackend::VulkanVideo);
            }
        }

        // Software fallback is always available.
        backends.push(EncoderBackend::Software);
        backends
    }
}
