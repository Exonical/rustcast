//! Runtime capability detection.
//!
//! [`PlatformInfo`](crate::platform::PlatformInfo) answers "what *kind* of
//! machine is this?" by inspecting the OS and PCI vendor id. That is enough to
//! *pick* a default backend, but on Linux/Wayland it is not enough to know
//! whether a backend will actually *work*: the XDG desktop portal may be
//! absent, the VA-API driver may lack an HEVC encode entrypoint, or DMA-BUF
//! import may be unavailable.
//!
//! [`PlatformCapabilities`] captures the richer, *probed* answer. The
//! [`CapabilityProbe`] trait lets each subsystem (portal, encoder, input)
//! contribute to that answer; [`BaseCapabilityProbe`] fills in everything that
//! can be determined cheaply and without side effects, leaving the
//! backend-specific fields at their conservative defaults until a real probe
//! runs.

use crate::platform::{Os, PlatformInfo};
use crate::types::{CaptureBackend, EncoderBackend, GpuVendor, VideoCodec};

/// Which Wayland portal interfaces are reachable on the session bus.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PortalCapabilities {
    /// `org.freedesktop.portal.ScreenCast` is available.
    pub screencast: bool,
    /// `org.freedesktop.portal.RemoteDesktop` is available.
    pub remote_desktop: bool,
    /// Advertised `ScreenCast` interface version, if known.
    pub screencast_version: Option<u32>,
    /// Advertised `RemoteDesktop` interface version, if known.
    pub remote_desktop_version: Option<u32>,
}

/// Encode capabilities discovered by initializing a hardware encoder backend
/// (e.g. by querying VA-API config entrypoints). Defaults are all-false /
/// "unknown" until a real probe populates them.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct EncodeCapabilities {
    /// The backend these capabilities describe, if one was probed.
    pub backend: Option<EncoderBackend>,
    /// Driver/vendor string (e.g. VA-API `vaQueryVendorString`).
    pub driver: Option<String>,
    pub h264: bool,
    pub h265: bool,
    pub av1: bool,
    /// 10-bit / HDR10 (e.g. `VAProfileHEVCMain10`) encode is available.
    pub hdr10: bool,
}

impl EncodeCapabilities {
    /// Whether the probed backend can encode `codec`.
    pub fn supports_codec(&self, codec: VideoCodec) -> bool {
        match codec {
            VideoCodec::H264 => self.h264,
            VideoCodec::H265 => self.h265,
            VideoCodec::Av1 => self.av1,
        }
    }
}

/// The input-injection backend selected for this host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InputBackendKind {
    /// `xdg-desktop-portal` `RemoteDesktop` (Wayland, preferred).
    Portal,
    /// libei / EIS input emulation (Wayland).
    LibEi,
    /// Kernel `/dev/uinput` (compositor-agnostic, needs privileges).
    Uinput,
    /// Windows `SendInput`.
    Windows,
    /// No usable input backend.
    None,
}

/// Aggregated, *probed* capabilities for the current host.
#[derive(Debug, Clone)]
pub struct PlatformCapabilities {
    pub os: Os,
    pub gpu_vendor: GpuVendor,
    pub capture_backends: Vec<CaptureBackend>,
    pub encoder_backends: Vec<EncoderBackend>,
    pub portal: PortalCapabilities,
    pub encode: EncodeCapabilities,
    /// DRM format modifiers the GPU/driver can import for zero-copy encode.
    pub dmabuf_modifiers: Vec<u64>,
    pub input_backend: InputBackendKind,
}

impl PlatformCapabilities {
    /// Whether `codec` can be hardware-encoded according to the encode probe.
    pub fn supports_codec(&self, codec: VideoCodec) -> bool {
        self.encode.supports_codec(codec)
    }

    /// Whether a Wayland capture session can be negotiated (both the portal
    /// ScreenCast interface and a PipeWire-capable capture backend present).
    pub fn can_capture_wayland(&self) -> bool {
        self.portal.screencast && self.capture_backends.contains(&CaptureBackend::PipeWire)
    }

    /// Whether DMA-BUF based zero-copy import is expected to work.
    pub fn has_dmabuf(&self) -> bool {
        !self.dmabuf_modifiers.is_empty()
    }
}

/// A source of probed platform capabilities.
pub trait CapabilityProbe {
    fn probe(&self) -> PlatformCapabilities;
}

/// The default probe: derives everything it can from [`PlatformInfo`] without
/// touching the GPU or D-Bus. Backend-specific fields (`portal`, `encode`,
/// `dmabuf_modifiers`) are left at conservative defaults for a real
/// subsystem probe to fill in.
pub struct BaseCapabilityProbe;

impl CapabilityProbe for BaseCapabilityProbe {
    fn probe(&self) -> PlatformCapabilities {
        let info = PlatformInfo::detect();
        Self::from_platform_info(&info)
    }
}

impl BaseCapabilityProbe {
    /// Build base capabilities from an already-detected [`PlatformInfo`]
    /// (separated out so it can be unit-tested with synthetic input).
    pub fn from_platform_info(info: &PlatformInfo) -> PlatformCapabilities {
        PlatformCapabilities {
            os: info.os,
            gpu_vendor: info.gpu_vendor,
            capture_backends: info.available_capture_backends.clone(),
            encoder_backends: info.available_encoder_backends.clone(),
            portal: PortalCapabilities::default(),
            encode: EncodeCapabilities::default(),
            dmabuf_modifiers: Vec::new(),
            input_backend: default_input_backend(info.os),
        }
    }
}

/// The input backend we default to before a richer probe runs.
pub fn default_input_backend(os: Os) -> InputBackendKind {
    match os {
        Os::Windows => InputBackendKind::Windows,
        // On Linux the portal RemoteDesktop API is the production-ready
        // default; a real probe may downgrade to LibEi/Uinput.
        Os::Linux => InputBackendKind::Portal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_caps_codec_lookup() {
        let caps = EncodeCapabilities {
            backend: Some(EncoderBackend::Vaapi),
            driver: Some("Mesa Gallium".into()),
            h264: true,
            h265: true,
            av1: false,
            hdr10: true,
        };
        assert!(caps.supports_codec(VideoCodec::H264));
        assert!(caps.supports_codec(VideoCodec::H265));
        assert!(!caps.supports_codec(VideoCodec::Av1));
    }

    #[test]
    fn default_encode_caps_support_nothing() {
        let caps = EncodeCapabilities::default();
        assert!(!caps.supports_codec(VideoCodec::H264));
        assert!(!caps.supports_codec(VideoCodec::H265));
        assert!(!caps.supports_codec(VideoCodec::Av1));
        assert!(caps.backend.is_none());
    }

    #[test]
    fn default_input_backend_per_os() {
        assert_eq!(default_input_backend(Os::Windows), InputBackendKind::Windows);
        assert_eq!(default_input_backend(Os::Linux), InputBackendKind::Portal);
    }

    #[test]
    fn capabilities_helpers() {
        let mut caps = PlatformCapabilities {
            os: Os::Linux,
            gpu_vendor: GpuVendor::Amd,
            capture_backends: vec![CaptureBackend::PipeWire, CaptureBackend::Drm],
            encoder_backends: vec![EncoderBackend::Vaapi],
            portal: PortalCapabilities::default(),
            encode: EncodeCapabilities::default(),
            dmabuf_modifiers: Vec::new(),
            input_backend: InputBackendKind::Portal,
        };
        assert!(!caps.can_capture_wayland(), "no portal -> cannot capture");
        assert!(!caps.has_dmabuf());

        caps.portal.screencast = true;
        caps.dmabuf_modifiers.push(0);
        assert!(caps.can_capture_wayland());
        assert!(caps.has_dmabuf());
    }
}
