//! flux-hostd — root host daemon in front of the per-user flux-server agent.
//!
//! Owns the relay-facing frame port, performs PAM login over the wire, opens
//! a real logind session, spawns the user's GNOME session + flux-server agent,
//! then proxies the relay connection to the agent. One session per host:
//! disconnect leaves the OS session "locked"; re-login re-attaches.

mod osuser;
mod pam_session;
mod proto;
mod rate;
mod session;

use flux_core::config::FluxConfig;
use flux_core::platform::PlatformInfo;
use flux_core::registration::{self, RuntimeStatus};
use osuser::OsUser;
use pam_session::PamEvent;
use proto::{ClientCommand, LoginFrame, PromptStyle};
use rate::LoginRateLimiter;
use session::{SessionManager, SessionState};
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, RwLock};
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};

const DEFAULT_FRAME_PORT: u16 = 8556;
const AGENT_PORT: u16 = 8557;
const MACHINE_ID_PATH: &str = "/var/lib/flux/machine-id";
const AGENT_START_TIMEOUT: Duration = Duration::from_secs(60);
const USER_MANAGER_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_LOGIN_FAILURES: u32 = 3;
const LOGIN_COOLDOWN: Duration = Duration::from_secs(10);
const USERNAME_PROMPT: &str = "Username";

struct Config {
    frame_addr: SocketAddr,
    agent_addr: SocketAddr,
}

struct Shared {
    cfg: Config,
    manager: SessionManager,
    limiter: Mutex<LoginRateLimiter>,
    status: Arc<RwLock<RuntimeStatus>>,
}

fn env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // reqwest is built with `rustls-no-provider`; install ring like flux-server.
    let _ = rustls::crypto::ring::default_provider().install_default();
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let frame_port: u16 = env("FLUX_FRAME_PORT")
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_FRAME_PORT);
    let idle_logout = Duration::from_secs(
        env("FLUX_SESSION_IDLE_LOGOUT_SECS")
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
    );
    let cfg = Config {
        frame_addr: SocketAddr::from(([0, 0, 0, 0], frame_port)),
        agent_addr: SocketAddr::from(([127, 0, 0, 1], AGENT_PORT)),
    };

    // Relay registration (shared logic from flux-core); the stable machine id
    // lives in /var/lib/flux so host restarts keep the same machine.
    let id_path = PathBuf::from(MACHINE_ID_PATH);
    if let Some(parent) = id_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut config = FluxConfig::default();
    config.name = env("FLUX_NAME").unwrap_or_else(|| "Flux Host".into());
    config.relay.base_url = env("FLUX_RELAY_URL");
    config.relay.advertise_host = env("FLUX_ADVERTISE_HOST").or(Some("127.0.0.1".into()));
    config.relay.token = env("FLUX_RELAY_TOKEN");
    if let Some(size) = env("FLUX_DISPLAY_SIZE") {
        if let Some((w, h)) = size.split_once('x') {
            config.video.max_width = w.parse().unwrap_or(config.video.max_width);
            config.video.max_height = h.parse().unwrap_or(config.video.max_height);
        }
    }
    let status = Arc::new(RwLock::new(RuntimeStatus {
        encoder_backend: Some("pending".into()),
        ..Default::default()
    }));
    let platform = PlatformInfo::detect();
    let advertised = format!(
        "{}:{}",
        config
            .relay
            .advertise_host
            .as_deref()
            .unwrap_or("127.0.0.1"),
        frame_port
    );
    let _registration = registration::start(&config, &platform, advertised, status.clone(), &id_path);

    let listener = TcpListener::bind(cfg.frame_addr).await?;
    tracing::info!("flux-hostd listening on tcp://{}", cfg.frame_addr);

    let shared = Arc::new(Shared {
        cfg,
        manager: SessionManager::new(idle_logout),
        limiter: Mutex::new(LoginRateLimiter::new(MAX_LOGIN_FAILURES, LOGIN_COOLDOWN)),
        status,
    });

    // SIGTERM → log out the OS session (pam_close_session + stop target).
    let shutdown = {
        let shared = shared.clone();
        async move {
            let mut term =
                tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                    .expect("signal handler");
            tokio::select! {
                _ = tokio::signal::ctrl_c() => {}
                _ = term.recv() => {}
            }
            tracing::info!("flux-hostd shutting down; logging out session");
            logout_current(&shared).await;
            shared
        }
    };

    tokio::select! {
        _ = accept_loop(listener, shared.clone()) => {}
        _ = shutdown => {}
    }
    Ok(())
}

async fn accept_loop(listener: TcpListener, shared: Arc<Shared>) {
    loop {
        match listener.accept().await {
            Ok((stream, peer)) => {
                let _ = stream.set_nodelay(true);
                let shared = shared.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_connection(stream, peer.ip(), shared).await {
                        tracing::debug!("connection {peer} ended: {e}");
                    }
                });
            }
            Err(e) => {
                tracing::warn!("accept failed: {e}");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

async fn handle_connection(
    mut stream: TcpStream,
    peer: IpAddr,
    shared: Arc<Shared>,
) -> std::io::Result<()> {
    // Cooldown: stalled acceptance for rate-limited peers.
    loop {
        let blocked = shared.limiter.lock().unwrap().is_blocked(peer);
        if !blocked {
            break;
        }
        tracing::info!("login attempt from rate-limited peer {peer}; delaying");
        tokio::time::sleep(LOGIN_COOLDOWN).await;
    }

    let (mut rd, mut wr) = stream.split();

    enum Login<'a> {
        Fresh {
            user: OsUser,
            outcome: Box<pam_session::PamOutcome>,
            claim: session::Claim<'a>,
        },
        Reattach {
            user: String,
        },
    }

    // Outer loop: after a lock command the OS session stays locked and the
    // relay connection returns here for the next login.
    loop {
    let login: Login = loop {
        proto::write_login_frame(
            &mut wr,
            &LoginFrame::Prompt {
                style: PromptStyle::EchoOn,
                message: USERNAME_PROMPT.into(),
            },
        )
        .await?;
        let username = match read_response(&mut rd).await? {
            Some(name) => name,
            None => return Ok(()),
        };
        let Some(username) = sanitize_username(&username) else {
            proto::write_login_frame(
                &mut wr,
                &LoginFrame::Failed {
                    message: "invalid username".into(),
                    user: None,
                },
            )
            .await?;
            continue;
        };

        enum Mode {
            Fresh,
            Reattach,
            Busy(String),
        }
        let (mode, claim) = match shared.manager.state() {
            SessionState::Free => match shared.manager.try_claim(&username) {
                Some(c) => (Mode::Fresh, Some(c)),
                None => (
                    Mode::Busy(match shared.manager.state() {
                        SessionState::Claimed { user }
                        | SessionState::Active { user }
                        | SessionState::Locked { user } => user,
                        SessionState::Free => username.clone(),
                    }),
                    None,
                ),
            },
            SessionState::Claimed { user } | SessionState::Active { user } => {
                (Mode::Busy(user), None)
            }
            SessionState::Locked { user } if user == username => {
                (Mode::Reattach, None)
            }
            SessionState::Locked { user } => (Mode::Busy(user), None),
        };
        let open_session = match mode {
            Mode::Busy(user) => {
                proto::write_login_frame(&mut wr, &LoginFrame::Busy { user }).await?;
                return Ok(());
            }
            Mode::Fresh => true,
            Mode::Reattach => false,
        };

        let (events, responses) = pam_session::start(username.clone(), open_session);
        match drive_pam(events, responses, &mut rd, &mut wr, &username).await? {
            Some(outcome) => {
                shared.limiter.lock().unwrap().record_success(peer);
                match mode {
                    Mode::Reattach => {
                        // Re-check atomically: another connection may have
                        // attached the session while PAM was running.
                        if !shared.manager.try_attach(&username) {
                            let busy = match shared.manager.state() {
                                SessionState::Claimed { user }
                                | SessionState::Active { user }
                                | SessionState::Locked { user } => user,
                                SessionState::Free => username.clone(),
                            };
                            proto::write_login_frame(&mut wr, &LoginFrame::Busy {
                                user: busy,
                            })
                            .await?;
                            return Ok(());
                        }
                        break Login::Reattach {
                            user: username,
                        };
                    }
                    _ => {
                        match osuser::lookup(&outcome.user)? {
                            Some(user) => {
                                break Login::Fresh {
                                    user,
                                    outcome,
                                    claim: claim
                                        .expect("fresh mode always holds the claim"),
                                }
                            }
                            None => {
                                proto::write_login_frame(
                                    &mut wr,
                                    &LoginFrame::Failed {
                                        message: "no local account".into(),
                                        user: Some(username.clone()),
                                    },
                                )
                                .await?;
                                continue;
                            }
                        }
                    }
                }
            }
            None => {
                // drive_pam already sent the failed frame.
                if shared.limiter.lock().unwrap().record_failure(peer) {
                    tracing::warn!("too many login failures from {peer}; closing");
                    return Ok(());
                }
                continue;
            }
        }
    };

    let end = match login {
        Login::Reattach { user } => {
            let os_user = osuser::lookup(&user)?;
            proto::write_login_frame(
                &mut wr,
                &LoginFrame::Ok {
                    user: user.clone(),
                    uid: os_user.as_ref().map(|u| u.uid).unwrap_or(0),
                    gid: os_user.as_ref().map(|u| u.gid).unwrap_or(0),
                    home: os_user
                        .as_ref()
                        .map(|u| u.home.display().to_string())
                        .unwrap_or_default(),
                    reattached: true,
                },
            )
            .await?;
            proxy(&mut rd, &mut wr, &shared).await
        }
        Login::Fresh {
            user,
            outcome,
            claim: _claim,
        } => {
            if let Err(e) = start_user_session(&user, &outcome.env).await {
                tracing::error!("failed to start session for {}: {e}", user.name);
                proto::write_login_frame(
                    &mut wr,
                    &LoginFrame::Failed {
                        message: format!("session start failed: {e}"),
                        user: Some(user.name.clone()),
                    },
                )
                .await?;
                return Ok(());
            }
            let uid = user.uid;
            let gid = user.gid;
            let home = user.home.display().to_string();
            let name = user.name.clone();
            shared
                .manager
                .open(user, outcome.session.expect("fresh login opens pam session"));
            spawn_agent_watcher(uid, name.clone(), shared.clone());
            proto::write_login_frame(
                &mut wr,
                &LoginFrame::Ok {
                    user: name,
                    uid,
                    gid,
                    home,
                    reattached: false,
                },
            )
            .await?;
            proxy(&mut rd, &mut wr, &shared).await
        }
    };

    let end = match end {
        Ok(end) => end,
        Err(e) => {
            shared.manager.lock();
            return Err(e);
        }
    };
    schedule_idle_logout(shared.clone());
    shared.manager.lock();
    match end {
        // Relay dropped: the OS session stays alive but locked.
        ProxyEnd::Disconnected => return Ok(()),
        // Lock command: agent socket dropped; the relay connection goes back
        // to the Username prompt on the same connection.
        ProxyEnd::Locked => continue,
    }
    }
}

/// Read 0x08 responses (dropping other commands) until one arrives or the
/// stream ends.
async fn read_response(
    rd: &mut (impl tokio::io::AsyncReadExt + Unpin),
) -> std::io::Result<Option<String>> {
    loop {
        match proto::read_client_command(rd).await? {
            Some(ClientCommand::Login(resp)) => return Ok(Some(resp)),
            Some(ClientCommand::Dropped(_)) => continue,
            None => return Ok(None),
        }
    }
}

fn sanitize_username(name: &str) -> Option<String> {
    let name = name.trim();
    if name.is_empty()
        || name.len() > 64
        || !name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-' | '@' | '+'))
    {
        return None;
    }
    Some(name.to_string())
}

/// Pump PAM prompts to the wire and responses back to the worker thread.
/// Returns Some(outcome) on success, None on a reported failure.
async fn drive_pam(
    mut events: std::sync::mpsc::Receiver<PamEvent>,
    responses: std::sync::mpsc::Sender<String>,
    rd: &mut (impl tokio::io::AsyncReadExt + Unpin),
    wr: &mut (impl AsyncWriteExt + Unpin),
    attempted_user: &str,
) -> std::io::Result<Option<Box<pam_session::PamOutcome>>> {
    loop {
        let (event, rx) = tokio::task::spawn_blocking(move || (events.recv(), events))
            .await
            .map_err(|e| std::io::Error::other(format!("pam event task: {e}")))?;
        events = rx;
        let event = event.ok();
        match event {
            Some(PamEvent::Prompt { style, message }) => {
                proto::write_login_frame(wr, &LoginFrame::Prompt { style, message }).await?;
                match read_response(rd).await? {
                    Some(resp) => {
                        if responses.send(resp).is_err() {
                            return Ok(None);
                        }
                    }
                    // Client hung up mid-conversation: drop the channel so the
                    // PAM worker fails fast instead of waiting out the timeout.
                    None => {
                        drop(responses);
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::UnexpectedEof,
                            "client disconnected during login",
                        ));
                    }
                }
            }
            Some(PamEvent::Ok(outcome)) => return Ok(Some(outcome)),
            Some(PamEvent::Failed(message)) => {
                proto::write_login_frame(
                    wr,
                    &LoginFrame::Failed {
                        message,
                        user: Some(attempted_user.to_string()),
                    },
                )
                .await?;
                return Ok(None);
            }
            None => {
                proto::write_login_frame(
                    wr,
                    &LoginFrame::Failed {
                        message: "login service unavailable".into(),
                        user: Some(attempted_user.to_string()),
                    },
                )
                .await?;
                return Ok(None);
            }
        }
    }
}

/// Fresh login: start the user's flux-session target (waits for the logind
/// user manager) and wait for the agent to listen.
async fn start_user_session(
    user: &OsUser,
    pam_env: &[(std::ffi::OsString, std::ffi::OsString)],
) -> Result<(), String> {
    let pam_env = pam_env.to_vec();
    let user_name = user.name.clone();
    let (uid, gid, home) = (user.uid, user.gid, user.home.clone());
    let shell = user.shell.clone();
    let deadline = std::time::Instant::now() + USER_MANAGER_TIMEOUT;
    let target = tokio::task::spawn_blocking(move || {
        let user = OsUser {
            name: user_name,
            uid,
            gid,
            home,
            shell,
        };
        loop {
            match osuser::run_as_user(
                &user,
                &pam_env,
                "systemctl",
                &["--user", "start", "flux-session.target"],
            ) {
                Ok(mut child) => match child.wait() {
                    Ok(status) if status.success() => return Ok(()),
                    Ok(_) | Err(_) => {}
                },
                Err(_) => {}
            }
            if std::time::Instant::now() > deadline {
                return Err("timed out waiting for user manager".to_string());
            }
            std::thread::sleep(Duration::from_millis(500));
        }
    })
    .await
    .map_err(|e| e.to_string())??;
    let _ = target;

    // Wait for the agent's frame listener on 127.0.0.1:8557.
    let deadline = tokio::time::Instant::now() + AGENT_START_TIMEOUT;
    loop {
        match TcpStream::connect(("127.0.0.1", AGENT_PORT)).await {
            Ok(_) => return Ok(()),
            Err(_) if tokio::time::Instant::now() > deadline => {
                return Err("agent did not start listening".into());
            }
            Err(_) => tokio::time::sleep(Duration::from_millis(250)).await,
        }
    }
}

enum ProxyEnd {
    /// Relay side went away.
    Disconnected,
    /// Relay sent the lock command (0x09).
    Locked,
}

/// Proxy bytes both ways. Relay→agent is parsed per command so the lock
/// command (0x09) can be intercepted; everything else is forwarded verbatim.
/// Agent→relay is a byte copy, passing the agent's 0x01-0x05 frames through.
async fn proxy(
    rd: &mut (impl tokio::io::AsyncReadExt + Unpin),
    wr: &mut (impl AsyncWriteExt + Unpin),
    shared: &Shared,
) -> std::io::Result<ProxyEnd> {
    let mut agent = TcpStream::connect(shared.cfg.agent_addr).await?;
    let (mut agent_rd, mut agent_wr) = agent.split();

    let to_relay = async {
        let r = tokio::io::copy(&mut agent_rd, wr).await;
        let _ = wr.shutdown().await;
        r
    };
    let from_relay = async {
        let mut total = 0u64;
        loop {
            match proto::read_raw_command(rd).await? {
                None => {
                    tracing::info!("proxy ended ({total} bytes relay→agent)");
                    return Ok(ProxyEnd::Disconnected);
                }
                Some(cmd) if cmd.ty == proto::CMD_LOCK => {
                    tracing::info!("lock command received; locking session");
                    return Ok(ProxyEnd::Locked);
                }
                Some(cmd) => {
                    agent_wr.write_all(&cmd.bytes).await?;
                    total += cmd.bytes.len() as u64;
                }
            }
        }
    };

    tokio::select! {
        r = to_relay => {
            match r {
                Ok(n) => tracing::info!("proxy ended (agent closed; {n} bytes agent→relay)"),
                Err(e) => tracing::info!("proxy ended (agent error: {e})"),
            }
            Ok(ProxyEnd::Disconnected)
        }
        r = from_relay => r,
    }
}

/// Watch the agent's status file and feed RuntimeStatus for registration.
fn spawn_agent_watcher(uid: u32, user: String, shared: Arc<Shared>) {
    tokio::spawn(async move {
        let path = format!("/run/user/{uid}/flux-agent.json");
        let mut last = String::new();
        loop {
            match shared.manager.state() {
                SessionState::Free => break,
                SessionState::Active { user: u } | SessionState::Locked { user: u }
                    if u == user => {}
                _ => break,
            }
            if let Ok(text) = std::fs::read_to_string(&path) {
                if text != last {
                    last = text.clone();
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
                        if let Ok(mut s) = shared.status.write() {
                            if let Some(b) = v["encoder_backend"].as_str() {
                                s.encoder_backend = Some(b.to_string());
                            }
                            s.display_name = v["display_name"]
                                .as_str()
                                .map(|s| s.to_string());
                            if let Some(w) = v["width"].as_u64() {
                                s.encode_width = w as u32;
                            }
                            if let Some(h) = v["height"].as_u64() {
                                s.encode_height = h as u32;
                            }
                            s.captured_virtual_display = v["virtual_display"].as_bool();
                            if let Some(notify) = &s.registration_notify {
                                let _ = notify.send(());
                            }
                        }
                    }
                }
            }
            tokio::time::sleep(Duration::from_secs(2)).await;
        }
    });
}

/// After `idle_logout`, a still-locked session is torn down. No-op when the
/// configured idle timeout is 0.
fn schedule_idle_logout(shared: Arc<Shared>) {
    let timeout = shared.manager.idle_logout;
    if timeout.is_zero() {
        return;
    }
    tokio::spawn(async move {
        tokio::time::sleep(timeout).await;
        if matches!(shared.manager.state(), SessionState::Locked { .. }) {
            tracing::info!("idle logout timeout reached; logging out");
            logout_current(&shared).await;
        }
    });
}

/// Stop the user session target and close the PAM session.
async fn logout_current(shared: &Shared) {
    if let Some(mut sess) = shared.manager.take() {
        let user = std::mem::replace(
            &mut sess.user,
            OsUser {
                name: String::new(),
                uid: 0,
                gid: 0,
                home: PathBuf::new(),
                shell: PathBuf::new(),
            },
        );
        let name = user.name.clone();
        let _ = tokio::task::spawn_blocking(move || {
            let _ = osuser::run_as_user(
                &user,
                &[],
                "systemctl",
                &["--user", "stop", "flux-session.target"],
            )
            .and_then(|mut c| c.wait());
        })
        .await;
        drop(sess.pam_guard.take());
        tracing::info!("logged out {name}");
    }
}
