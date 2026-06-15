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

use crate::error::{FluxError, Result};
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

/// A concrete set of backends chosen for a session by
/// [`PlatformCapabilities::negotiate`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionPlan {
    pub capture: CaptureBackend,
    pub encoder: EncoderBackend,
    /// The codec to encode with — may differ from the request if the requested
    /// one isn't supported by the probed encoder.
    pub codec: VideoCodec,
    /// `None` when input forwarding is disabled or no backend is usable.
    pub input: InputBackendKind,
    /// Whether DMA-BUF zero-copy import is expected to be usable for this plan.
    pub zero_copy: bool,
}

impl PlatformCapabilities {
    /// Whether `codec` can be hardware-encoded according to the encode probe.
    pub fn supports_codec(&self, codec: VideoCodec) -> bool {
        self.encode.supports_codec(codec)
    }

    /// Choose a coherent set of backends for a session from the probed
    /// capabilities, applying the documented fallback order.
    ///
    /// Selection is driven by what was actually probed: a Wayland PipeWire
    /// session is preferred when the portal can grant one; the encoder and
    /// codec honour the encode probe when it ran, otherwise fall back to the
    /// statically advertised backend and the requested codec (the pre-probe
    /// behaviour). Returns an error only when nothing usable exists.
    pub fn negotiate(&self, requested_codec: VideoCodec, enable_input: bool) -> Result<SessionPlan> {
        let capture = self
            .negotiate_capture()
            .ok_or(FluxError::NoCaptureBackend)?;
        let encoder = self
            .negotiate_encoder()
            .ok_or(FluxError::NoEncoderBackend)?;
        let codec = self.negotiate_codec(requested_codec).ok_or_else(|| {
            FluxError::UnsupportedPlatform(format!(
                "no hardware-supported video codec (requested {requested_codec:?})"
            ))
        })?;
        let input = if enable_input {
            self.input_backend
        } else {
            InputBackendKind::None
        };
        let zero_copy = self.has_dmabuf() && capture == CaptureBackend::PipeWire;

        Ok(SessionPlan {
            capture,
            encoder,
            codec,
            input,
            zero_copy,
        })
    }

    /// Prefer a Wayland PipeWire session when the portal can grant one,
    /// otherwise the first advertised capture backend (e.g. DXGI on Windows,
    /// or the PipeWire/DRM default list on Linux).
    fn negotiate_capture(&self) -> Option<CaptureBackend> {
        if self.can_capture_wayland() {
            return Some(CaptureBackend::PipeWire);
        }
        self.capture_backends.first().copied()
    }

    /// Use the encoder the encode probe actually initialized, falling back to
    /// the first statically advertised backend when no probe ran.
    fn negotiate_encoder(&self) -> Option<EncoderBackend> {
        if let Some(backend) = self.encode.backend {
            return Some(backend);
        }
        self.encoder_backends.first().copied()
    }

    /// Resolve the codec to encode with. When the encode probe ran, honour the
    /// request if supported else fall back to the most broadly decodable
    /// supported codec (H.264 → H.265 → AV1). When no probe ran, trust the
    /// request.
    fn negotiate_codec(&self, requested: VideoCodec) -> Option<VideoCodec> {
        if self.encode.backend.is_none() {
            return Some(requested);
        }
        if self.encode.supports_codec(requested) {
            return Some(requested);
        }
        [VideoCodec::H264, VideoCodec::H265, VideoCodec::Av1]
            .into_iter()
            .find(|&c| self.encode.supports_codec(c))
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

    fn linux_caps() -> PlatformCapabilities {
        PlatformCapabilities {
            os: Os::Linux,
            gpu_vendor: GpuVendor::Amd,
            capture_backends: vec![CaptureBackend::PipeWire, CaptureBackend::Drm],
            encoder_backends: vec![EncoderBackend::Vaapi, EncoderBackend::Software],
            portal: PortalCapabilities::default(),
            encode: EncodeCapabilities::default(),
            dmabuf_modifiers: Vec::new(),
            input_backend: InputBackendKind::Portal,
        }
    }

    #[test]
    fn negotiate_without_probe_trusts_request_and_first_backends() {
        let caps = linux_caps();
        let plan = caps.negotiate(VideoCodec::H265, true).unwrap();
        // No encode probe ran: honour the request and the advertised first backend.
        assert_eq!(plan.encoder, EncoderBackend::Vaapi);
        assert_eq!(plan.codec, VideoCodec::H265);
        // No portal probed -> falls back to first capture backend (PipeWire).
        assert_eq!(plan.capture, CaptureBackend::PipeWire);
        assert_eq!(plan.input, InputBackendKind::Portal);
        assert!(!plan.zero_copy, "no dmabuf modifiers probed");
    }

    #[test]
    fn negotiate_prefers_pipewire_and_zero_copy_when_portal_and_dmabuf_present() {
        let mut caps = linux_caps();
        caps.portal.screencast = true;
        caps.dmabuf_modifiers.push(0);
        let plan = caps.negotiate(VideoCodec::H264, true).unwrap();
        assert_eq!(plan.capture, CaptureBackend::PipeWire);
        assert!(plan.zero_copy);
    }

    #[test]
    fn negotiate_uses_probed_encoder_and_falls_back_unsupported_codec() {
        let mut caps = linux_caps();
        // Encode probe ran and only H.264 is drivable.
        caps.encode = EncodeCapabilities {
            backend: Some(EncoderBackend::Vaapi),
            driver: Some("Mesa Gallium".into()),
            h264: true,
            h265: false,
            av1: false,
            hdr10: false,
        };
        let plan = caps.negotiate(VideoCodec::H265, true).unwrap();
        assert_eq!(plan.encoder, EncoderBackend::Vaapi);
        assert_eq!(plan.codec, VideoCodec::H264, "H.265 unsupported -> fall back to H.264");
    }

    #[test]
    fn negotiate_errors_when_probe_supports_no_codec() {
        let mut caps = linux_caps();
        caps.encode.backend = Some(EncoderBackend::Vaapi); // probed, nothing supported
        let err = caps.negotiate(VideoCodec::H264, true).unwrap_err();
        assert!(matches!(err, FluxError::UnsupportedPlatform(_)));
    }

    #[test]
    fn negotiate_omits_input_when_disabled() {
        let caps = linux_caps();
        let plan = caps.negotiate(VideoCodec::H264, false).unwrap();
        assert_eq!(plan.input, InputBackendKind::None);
    }

    #[test]
    fn negotiate_errors_without_any_backend() {
        let mut caps = linux_caps();
        caps.capture_backends.clear();
        assert!(matches!(
            caps.negotiate(VideoCodec::H264, true),
            Err(FluxError::NoCaptureBackend)
        ));

        let mut caps = linux_caps();
        caps.encoder_backends.clear();
        assert!(matches!(
            caps.negotiate(VideoCodec::H264, true),
            Err(FluxError::NoEncoderBackend)
        ));
    }
}
