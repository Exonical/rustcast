use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use std::time::Duration;

use crate::config::{FluxConfig, VirtualDisplayConfig};
use crate::platform::PlatformInfo;
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

#[derive(Debug, Clone, Default)]
pub struct RuntimeStatus {
    pub display_name: Option<String>,
    pub captured_virtual_display: Option<bool>,
    pub encoder_backend: Option<String>,
    pub capture_width: u32,
    pub capture_height: u32,
    pub encode_width: u32,
    pub encode_height: u32,
    pub target_bitrate_kbps: u32,
    /// Whether this host performs OS logins (flux-hostd).
    pub supports_login: bool,
    /// "idle" | "active" | "locked"
    pub login_state: String,
    pub logged_in_user: Option<String>,
    /// Hex SHA-256 of the QUIC listener's certificate, when present.
    pub quic_cert_sha256: Option<String>,
    pub registration_notify: Option<tokio::sync::mpsc::UnboundedSender<()>>,
}

pub struct RegistrationHandle {
    pub stop: oneshot::Sender<()>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
    default_fps: u32,
    target_bitrate_kbps: u32,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    supports_login: bool,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    login_state: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    logged_in_user: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    quic_cert_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    host_dials_out: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    direct_candidates: Vec<DirectCandidate>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct DirectCandidate {
    url: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    cert_hashes: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct DirectStatus {
    #[serde(default)]
    cert_hashes: Vec<String>,
}

pub fn start(
    config: &FluxConfig,
    platform: &PlatformInfo,
    frame_endpoint: String,
    status: Arc<RwLock<RuntimeStatus>>,
    machine_id_path: &Path,
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
    let id = match load_or_create_id(machine_id_path) {
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
        default_fps: config.video.default_fps_cap.min(config.video.max_fps).min(144),
        target_bitrate_kbps: 0,
        supports_login: false,
        login_state: String::new(),
        logged_in_user: None,
        quic_cert_sha256: None,
        host_dials_out: config.relay.host_dials_out,
        direct_candidates: Vec::new(),
    };
    let direct_addrs = config.relay.direct_addrs.clone();
    let direct_status_file = config.relay.direct_status_file.clone();
    let direct_status_warned = Arc::new(AtomicBool::new(false));
    let token = std::env::var("FLUX_RELAY_TOKEN")
        .ok()
        .filter(|value| !value.is_empty())
        .or_else(|| config.relay.token.clone());
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
            if !direct_addrs.is_empty() {
                current.direct_candidates = match build_direct_candidates(&direct_addrs, direct_status_file.as_deref())
                {
                    Ok(candidates) => candidates,
                    Err(error) => {
                        if !direct_status_warned.swap(true, Ordering::Relaxed) {
                            tracing::warn!("Direct candidate status unavailable: {error}");
                        }
                        Vec::new()
                    }
                };
            }
            if let Ok(snapshot) = status.read() {
                current.display_name = snapshot.display_name.clone().unwrap_or_default();
                if let Some(backend) = &snapshot.encoder_backend {
                    current.encoder_backend = backend.clone();
                }
                if let Some(virtual_display) = snapshot.captured_virtual_display {
                    current.virtual_display = virtual_display;
                }
                current.supports_login = snapshot.supports_login;
                current.login_state = snapshot.login_state.clone();
                current.logged_in_user = snapshot.logged_in_user.clone();
                current.quic_cert_sha256 = snapshot.quic_cert_sha256.clone();
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
            let request = client.post(url).json(&current);
            let request = match &token {
                Some(token) => request.bearer_auth(token),
                None => request,
            };
            if let Err(error) = request.send().await {
                tracing::warn!("Relay registration request failed for {url}: {error}");
            }
            tokio::select! {
                _ = interval.tick() => {},
                Some(()) = notify_rx.recv() => {},
                _ = &mut stop_rx => {
                    let request = client.post(&deregister);
                    let request = match &token {
                        Some(token) => request.bearer_auth(token),
                        None => request,
                    };
                    let _ = request.send().await;
                    return;
                }
            }
        }
    });
    Some(RegistrationHandle { stop: stop_tx })
}

fn build_direct_candidates(addrs: &[String], status_file: Option<&Path>) -> Result<Vec<DirectCandidate>, String> {
    if addrs.is_empty() {
        return Ok(Vec::new());
    }
    let status_file = status_file.ok_or_else(|| "FLUX_DIRECT_STATUS_FILE is not set".to_string())?;
    let status_json =
        std::fs::read_to_string(status_file).map_err(|error| format!("read {}: {error}", status_file.display()))?;
    let status: DirectStatus =
        serde_json::from_str(&status_json).map_err(|error| format!("parse {}: {error}", status_file.display()))?;
    let cert_hashes = status.cert_hashes;
    Ok(addrs
        .iter()
        .filter_map(|addr| normalize_direct_addr(addr))
        .map(|addr| DirectCandidate {
            url: format!("https://{addr}/wt"),
            cert_hashes: cert_hashes.clone(),
        })
        .collect())
}

fn normalize_direct_addr(addr: &str) -> Option<String> {
    let addr = addr.trim();
    if addr.is_empty() {
        return None;
    }
    if let Ok(ip) = addr.parse::<std::net::Ipv6Addr>() {
        return Some(format!("[{ip}]:8443"));
    }
    if let Ok(ip) = addr.parse::<std::net::Ipv4Addr>() {
        return Some(format!("{ip}:8443"));
    }
    if addr.starts_with('[') {
        if addr.ends_with(']') {
            return Some(format!("{addr}:8443"));
        }
        if addr
            .rsplit_once("]:")
            .is_some_and(|(host, port)| host.starts_with('[') && port.parse::<u16>().is_ok())
        {
            return Some(addr.to_string());
        }
        return None;
    }
    if addr.matches(':').count() == 0 {
        return Some(format!("{addr}:8443"));
    }
    if addr.parse::<std::net::SocketAddr>().is_ok() {
        return Some(addr.to_string());
    }
    if addr
        .rsplit_once(':')
        .is_some_and(|(host, port)| !host.is_empty() && port.parse::<u16>().is_ok())
    {
        return Some(addr.to_string());
    }
    None
}

pub fn load_or_create_id(path: &Path) -> std::io::Result<String> {
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

#[cfg(test)]
mod tests {
    use super::{build_direct_candidates, Registration};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn test_dir() -> std::path::PathBuf {
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
        let path = std::env::temp_dir().join(format!("flux-registration-{}-{stamp}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn direct_candidates_read_status_and_normalize_addresses() {
        let dir = test_dir();
        let status_file = dir.join("direct.json");
        std::fs::write(&status_file, r#"{"cert_hashes":["abc","def"]}"#).unwrap();
        let addrs = vec![
            "host.example.test".to_string(),
            "192.0.2.10:9443".to_string(),
            "2001:db8::1".to_string(),
            "[2001:db8::2]:9443".to_string(),
        ];
        let candidates = build_direct_candidates(&addrs, Some(&status_file)).unwrap();
        assert_eq!(
            candidates
                .iter()
                .map(|candidate| candidate.url.as_str())
                .collect::<Vec<_>>(),
            [
                "https://host.example.test:8443/wt",
                "https://192.0.2.10:9443/wt",
                "https://[2001:db8::1]:8443/wt",
                "https://[2001:db8::2]:9443/wt",
            ]
        );
        assert!(candidates
            .iter()
            .all(|candidate| candidate.cert_hashes == ["abc", "def"]));
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn missing_direct_status_omits_candidates_from_registration() {
        let dir = test_dir();
        let missing = dir.join("missing.json");
        assert!(build_direct_candidates(&["host.example.test".into()], Some(missing.as_path())).is_err());
        let registration = Registration::default();
        let value = serde_json::to_value(registration).unwrap();
        assert!(value.get("direct_candidates").is_none());
        std::fs::remove_dir_all(dir).unwrap();
    }
}
