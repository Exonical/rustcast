//! PAM authentication driven from a blocking worker thread. Prompts are
//! forwarded to the async connection task as login frames; responses come
//! back over a channel with a per-prompt timeout.

use crate::proto::PromptStyle;
use pam_client::{Context, ConversationHandler, ErrorCode, Flag};
use std::ffi::{CStr, CString, OsString};
use std::sync::mpsc;
use std::time::Duration;

const PROMPT_TIMEOUT: Duration = Duration::from_secs(120);
const PAM_SERVICE: &str = "flux";

/// Events emitted by the PAM worker thread.
pub enum PamEvent {
    /// PAM asked a question (echo_on/echo_off) or emitted info/error text.
    Prompt { style: PromptStyle, message: String },
    /// Authentication (and session open, when requested) succeeded.
    Ok(Box<PamOutcome>),
    /// Authentication or session setup failed.
    Failed(String),
}

pub struct PamOutcome {
    /// Resulting (possibly canonicalized) user name.
    pub user: String,
    /// pam_getenvlist entries for the session environment.
    pub env: Vec<(OsString, OsString)>,
    /// Kept alive for the OS session's lifetime; drop it to run
    /// `pam_close_session` + `pam_end`.
    pub session: Option<PamSessionGuard>,
}

/// Handle keeping the PAM session open. The worker thread parks until the
/// guard is dropped, then closes the session.
pub struct PamSessionGuard {
    close_tx: mpsc::Sender<()>,
}

impl Drop for PamSessionGuard {
    fn drop(&mut self) {
        let _ = self.close_tx.send(());
    }
}

struct WireConversation {
    events: mpsc::Sender<PamEvent>,
    responses: mpsc::Receiver<String>,
}

impl WireConversation {
    fn ask(&mut self, style: PromptStyle, msg: &CStr) -> Result<CString, ErrorCode> {
        let _ = self.events.send(PamEvent::Prompt {
            style,
            message: msg.to_string_lossy().into_owned(),
        });
        let response = self
            .responses
            .recv_timeout(PROMPT_TIMEOUT)
            .map_err(|_| ErrorCode::CONV_ERR)?;
        CString::new(response.trim_end_matches(['\n', '\r'])).map_err(|_| ErrorCode::CONV_ERR)
    }
}

impl ConversationHandler for WireConversation {
    fn prompt_echo_on(&mut self, prompt: &CStr) -> Result<CString, ErrorCode> {
        self.ask(PromptStyle::EchoOn, prompt)
    }

    fn prompt_echo_off(&mut self, prompt: &CStr) -> Result<CString, ErrorCode> {
        self.ask(PromptStyle::EchoOff, prompt)
    }

    fn text_info(&mut self, msg: &CStr) {
        let _ = self.events.send(PamEvent::Prompt {
            style: PromptStyle::Info,
            message: msg.to_string_lossy().into_owned(),
        });
    }

    fn error_msg(&mut self, msg: &CStr) {
        let _ = self.events.send(PamEvent::Prompt {
            style: PromptStyle::Error,
            message: msg.to_string_lossy().into_owned(),
        });
    }
}

/// Start a PAM login for `user` on a dedicated OS thread. When
/// `open_session` is true the worker also runs `pam_open_session` and parks
/// holding the session until the returned guard is dropped.
///
/// Returns (event receiver, response sender). The event receiver closes
/// when the thread exits.
pub fn start(
    user: String,
    open_session: bool,
) -> (
    mpsc::Receiver<PamEvent>,
    mpsc::Sender<String>,
) {
    let (events_tx, events_rx) = mpsc::channel();
    let (responses_tx, responses_rx) = mpsc::channel();
    std::thread::Builder::new()
        .name(format!("flux-pam-{user}"))
        .spawn(move || pam_worker(user, open_session, events_tx, responses_rx))
        .expect("spawn PAM worker");
    (events_rx, responses_tx)
}

fn pam_worker(
    user: String,
    open_session: bool,
    events: mpsc::Sender<PamEvent>,
    responses: mpsc::Receiver<String>,
) {
    macro_rules! fail {
        ($fmt:literal $($arg:tt)*) => {{
            let _ = events.send(PamEvent::Failed(format!($fmt $($arg)*)));
            return;
        }};
    }

    let conv = WireConversation {
        events: events.clone(),
        responses,
    };
    let mut ctx = match Context::new(PAM_SERVICE, Some(&user), conv) {
        Ok(ctx) => ctx,
        Err(e) => fail!("PAM initialization failed: {e}"),
    };

    if let Err(e) = ctx.authenticate(Flag::NONE) {
        fail!("authentication failed: {e}");
    }
    if let Err(e) = ctx.acct_mgmt(Flag::NONE) {
        fail!("account validation failed: {e}");
    }

    let user = ctx
        .user()
        .unwrap_or(user);

    if !open_session {
        let _ = events.send(PamEvent::Ok(Box::new(PamOutcome {
            user,
            env: Vec::new(),
            session: None,
        })));
        return;
    }

    let session = match ctx.open_session(Flag::NONE) {
        Ok(session) => session,
        Err(e) => fail!("session open failed: {e}"),
    };
    let env = session
        .envlist()
        .iter_tuples()
        .map(|(k, v)| (k.to_os_string(), v.to_os_string()))
        .collect();

    let (close_tx, close_rx) = mpsc::channel::<()>();
    if events
        .send(PamEvent::Ok(Box::new(PamOutcome {
            user,
            env,
            session: Some(PamSessionGuard { close_tx }),
        })))
        .is_err()
    {
        return;
    }

    // Hold the PAM handle until the guard is dropped, then close cleanly.
    let _ = close_rx.recv();
    if let Err(e) = session.close(Flag::NONE) {
        tracing::warn!("pam_close_session failed: {e}");
    }
}
