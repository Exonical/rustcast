use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use flux_core::config::{FluxConfig, VirtualDisplayConfig};
use flux_core::platform::PlatformInfo;
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

#[derive(Debug, Clone, Default)]
pub struct RuntimeStatus {
    pub display_name: Option<String>,
    pub encoder_backend: Option<String>,
    pub capture_width: u32,
    pub capture_height: u32,
    pub encode_width: u32,
    pub encode_height: u32,
    pub target_bitrate_kbps: u32,
    pub registration_notify: Option<tokio::sync::mpsc::UnboundedSender<()>>,
}

pub struct RegistrationHandle {
    pub stop: oneshot::Sender<()>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Registration {
    id: String,
    name: String,
    display_name: String,
    frame_endpoint: String,
    os: String,
    gpu_vendor: String,
    encoder_backend: String,
    virtual_display: bool,
    width: u32,
    height: u32,
    target_fps: u32,
    target_bitrate_kbps: u32,
}

pub fn start(
    config: &FluxConfig,
    platform: &PlatformInfo,
    frame_endpoint: String,
    status: Arc<RwLock<RuntimeStatus>>,
) -> Option<RegistrationHandle> {
    let base_url = config.relay.base_url.clone()?;
    let parsed_base_url = match reqwest::Url::parse(&base_url) {
        Ok(url)
            if matches!(url.scheme(), "http" | "https")
                && url.host_str().is_some() =>
        {
            url
        }
        Ok(url) => {
            tracing::warn!(
                "Relay registration disabled: invalid base_url {:?}: expected http:// or https:// URL with a host (parsed scheme {:?})",
                base_url,
                url.scheme()
            );
            return None;
        }
        Err(error) => {
            tracing::warn!(
                "Relay registration disabled: invalid base_url {:?}: {}",
                base_url,
                error
            );
            return None;
        }
    };
    let id_path = config_path(config).with_file_name("flux-machine-id");
    let id = match load_or_create_id(&id_path) {
        Ok(id) => id,
        Err(error) => {
            tracing::warn!("Relay registration disabled: cannot persist machine ID: {error}");
            return None;
        }
    };
    let registration = Registration {
        id,
        name: config.name.clone(),
        display_name: String::new(),
        frame_endpoint,
        os: format!("{:?}", platform.os),
        gpu_vendor: format!("{:?}", platform.gpu_vendor),
        encoder_backend: config
            .video
            .encoder
            .map(|backend| format!("{backend:?}"))
            .unwrap_or_else(|| "unknown".into()),
        virtual_display: config.video.virtual_display.is_some(),
        width: config
            .video
            .virtual_display
            .map(|display: VirtualDisplayConfig| display.width)
            .unwrap_or(config.video.max_width),
        height: config
            .video
            .virtual_display
            .map(|display: VirtualDisplayConfig| display.height)
            .unwrap_or(config.video.max_height),
        target_fps: config.video.max_fps.min(144),
        target_bitrate_kbps: 0,
    };
    let (stop_tx, mut stop_rx) = oneshot::channel();
    let (notify_tx, mut notify_rx) = tokio::sync::mpsc::unbounded_channel();
    if let Ok(mut snapshot) = status.write() {
        snapshot.registration_notify = Some(notify_tx.clone());
    }
    tokio::spawn(async move {
        let client = reqwest::Client::new();
        let base_url = parsed_base_url.as_str().trim_end_matches('/').to_string();
        let endpoint = format!("{base_url}/api/machines/register");
        let heartbeat = format!(
            "{base_url}/api/machines/{}/heartbeat",
            registration.id
        );
        let deregister = format!(
            "{base_url}/api/machines/{}/deregister",
            registration.id
        );
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        let mut first = true;
        loop {
            let mut current = registration.clone();
            if let Ok(snapshot) = status.read() {
                current.display_name = snapshot.display_name.clone().unwrap_or_default();
                if let Some(backend) = &snapshot.encoder_backend {
                    current.encoder_backend = backend.clone();
                }
                if snapshot.encode_width != 0 {
                    current.width = snapshot.encode_width;
                    current.height = snapshot.encode_height;
                }
                current.target_bitrate_kbps = snapshot.target_bitrate_kbps;
            }
            let url = if first {
                &endpoint
            } else {
                &heartbeat
            };
            first = false;
            if let Err(error) = client.post(url).json(&current).send().await {
                tracing::warn!("Relay registration request failed for {url}: {error}");
            }
            tokio::select! {
                _ = interval.tick() => {},
                Some(()) = notify_rx.recv() => {},
                _ = &mut stop_rx => {
                    let _ = client.post(&deregister).send().await;
                    return;
                }
            }
        }
    });
    Some(RegistrationHandle { stop: stop_tx })
}

fn config_path(config: &FluxConfig) -> PathBuf {
    PathBuf::from(&config.security.cert_path)
}

fn load_or_create_id(path: &Path) -> std::io::Result<String> {
    if let Ok(id) = std::fs::read_to_string(path) {
        let id = id.trim();
        if !id.is_empty() {
            return Ok(id.to_string());
        }
    }
    let id = uuid::Uuid::new_v4().to_string();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, &id)?;
    Ok(id)
}
