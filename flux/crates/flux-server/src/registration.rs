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
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Registration {
    id: String,
    name: String,
    frame_endpoint: String,
    os: String,
    gpu_vendor: String,
    encoder_backend: String,
    virtual_display: bool,
    width: u32,
    height: u32,
    target_fps: u32,
}

pub fn start(
    config: &FluxConfig,
    platform: &PlatformInfo,
    frame_endpoint: String,
    status: Arc<RwLock<RuntimeStatus>>,
) -> Option<oneshot::Sender<()>> {
    let base_url = config.relay.base_url.clone()?;
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
        target_fps: config.video.max_fps.min(60),
    };
    let (stop_tx, mut stop_rx) = oneshot::channel();
    tokio::spawn(async move {
        let client = reqwest::Client::new();
        let endpoint = format!("{}/api/machines/register", base_url.trim_end_matches('/'));
        let heartbeat = format!(
            "{}/api/machines/{}/heartbeat",
            base_url.trim_end_matches('/'),
            registration.id
        );
        let deregister = format!(
            "{}/api/machines/{}/deregister",
            base_url.trim_end_matches('/'),
            registration.id
        );
        let mut interval = tokio::time::interval(Duration::from_secs(5));
        let mut first = true;
        loop {
            let mut current = registration.clone();
            if let Ok(snapshot) = status.read() {
                if let Some(display) = &snapshot.display_name {
                    current.name = format!("{} ({display})", registration.name);
                }
                if let Some(backend) = &snapshot.encoder_backend {
                    current.encoder_backend = backend.clone();
                }
                if snapshot.encode_width != 0 {
                    current.width = snapshot.encode_width;
                    current.height = snapshot.encode_height;
                }
            }
            let url = if first {
                &endpoint
            } else {
                &heartbeat
            };
            first = false;
            if let Err(error) = client.post(url).json(&current).send().await {
                tracing::warn!("Relay registration request failed: {error}");
            }
            tokio::select! {
                _ = interval.tick() => {},
                _ = &mut stop_rx => {
                    let _ = client.post(&deregister).send().await;
                    return;
                }
            }
        }
    });
    Some(stop_tx)
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
