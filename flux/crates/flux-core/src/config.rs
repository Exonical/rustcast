use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{FluxError, Result};
use crate::types::{AudioCodec, ChromaSampling, DynamicRange, RateControlMode, VideoCodec};

/// Top-level Flux configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FluxConfig {
    /// Human-readable name for this host.
    pub name: String,

    /// Network bind address.
    pub bind_address: String,

    /// Server configuration.
    pub server: ServerConfig,

    /// Video stream settings.
    pub video: VideoConfig,

    /// Audio stream settings.
    pub audio: AudioConfig,

    /// Input forwarding settings.
    pub input: InputConfig,

    /// Security / crypto settings.
    pub security: SecurityConfig,
}

impl FluxConfig {
    /// Read configuration from a TOML file.
    pub fn from_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        let contents = std::fs::read_to_string(path.as_ref())?;
        toml::from_str(&contents).map_err(|e| FluxError::Config(format!("parse error: {e}")))
    }

    /// Write configuration to a TOML file.
    pub fn save<P: AsRef<Path>>(&self, path: P) -> Result<()> {
        let contents =
            toml::to_string_pretty(self).map_err(|e| FluxError::Config(format!("serialize error: {e}")))?;
        if let Some(parent) = path.as_ref().parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(path, contents)?;
        Ok(())
    }
}

impl Default for FluxConfig {
    fn default() -> Self {
        Self {
            name: "Flux Host".into(),
            bind_address: "0.0.0.0".into(),
            server: ServerConfig::default(),
            video: VideoConfig::default(),
            audio: AudioConfig::default(),
            input: InputConfig::default(),
            security: SecurityConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    /// HTTPS port for the control API.
    pub https_port: u16,

    /// Session signaling port (RTSP or custom).
    pub signaling_port: u16,

    /// Maximum concurrent sessions.
    pub max_sessions: u32,

    /// Idle timeout in seconds before a session is torn down.
    pub session_timeout_secs: u64,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            https_port: 8443,
            signaling_port: 8554,
            max_sessions: 4,
            session_timeout_secs: 300,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoConfig {
    /// Preferred codec.
    pub codec: VideoCodec,

    /// Maximum resolution (width).
    pub max_width: u32,

    /// Maximum resolution (height).
    pub max_height: u32,

    /// Maximum framerate.
    pub max_fps: u32,

    /// Target bitrate in kbps.
    pub bitrate_kbps: u32,

    /// Rate control mode.
    pub rate_control: RateControlMode,

    /// Dynamic range mode.
    pub dynamic_range: DynamicRange,

    /// Chroma sub-sampling.
    pub chroma_sampling: ChromaSampling,

    /// UDP port for video RTP stream.
    pub rtp_port: u16,

    /// FEC overhead percentage (0-100).
    pub fec_percentage: u8,

    /// Maximum RTP packet size in bytes.
    pub max_packet_size: u16,
}

impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            codec: VideoCodec::H265,
            max_width: 3840,
            max_height: 2160,
            max_fps: 60,
            bitrate_kbps: 20_000,
            rate_control: RateControlMode::Cbr,
            dynamic_range: DynamicRange::Sdr,
            chroma_sampling: ChromaSampling::Yuv420,
            rtp_port: 47998,
            fec_percentage: 20,
            max_packet_size: 1400,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioConfig {
    /// Audio codec.
    pub codec: AudioCodec,

    /// Sample rate in Hz.
    pub sample_rate: u32,

    /// Number of channels.
    pub channels: u16,

    /// Audio bitrate in kbps.
    pub bitrate_kbps: u32,

    /// UDP port for audio RTP stream.
    pub rtp_port: u16,
}

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            codec: AudioCodec::Opus,
            sample_rate: 48_000,
            channels: 2,
            bitrate_kbps: 256,
            rtp_port: 48000,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InputConfig {
    /// Enable keyboard forwarding.
    pub keyboard: bool,

    /// Enable mouse forwarding.
    pub mouse: bool,

    /// Enable gamepad forwarding.
    pub gamepad: bool,

    /// UDP port for input messages.
    pub port: u16,
}

impl Default for InputConfig {
    fn default() -> Self {
        Self {
            keyboard: true,
            mouse: true,
            gamepad: true,
            port: 47999,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SecurityConfig {
    /// Path to TLS certificate PEM file.
    pub cert_path: PathBuf,

    /// Path to TLS private key PEM file.
    pub key_path: PathBuf,

    /// Whether to require client certificate authentication.
    pub require_client_cert: bool,

    /// Whether to enable PIN-based pairing.
    pub pin_pairing: bool,
}

impl Default for SecurityConfig {
    fn default() -> Self {
        Self {
            cert_path: PathBuf::from("~/.config/flux/cert.pem"),
            key_path: PathBuf::from("~/.config/flux/key.pem"),
            require_client_cert: false,
            pin_pairing: true,
        }
    }
}
