//! Session registry: one OS login session per host, active or locked, plus a
//! claimed slot for a login in progress.

use crate::osuser::OsUser;
use crate::pam_session::PamSessionGuard;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

pub enum SessionState {
    /// No OS session.
    Free,
    /// A fresh login is running PAM (no OS session yet).
    Claimed { user: String },
    /// OS session exists and a relay connection is attached.
    Active { user: String },
    /// OS session exists but the relay disconnected; next login must
    /// re-authenticate (same user reattaches, others get `busy`).
    Locked { user: String },
}

pub struct ActiveSession {
    pub user: OsUser,
    /// Holds the PAM session open; dropping runs pam_close_session.
    pub pam_guard: Option<PamSessionGuard>,
    pub locked_since: Option<Instant>,
}

/// Guard for a Claimed slot; dropping it frees the slot.
pub struct Claim<'a> {
    mgr: &'a SessionManager,
    done: bool,
}
impl<'a> Drop for Claim<'a> {
    /// Releasing the claim just frees the slot; `open` already cleared it on
    /// success so this is a no-op there.
    fn drop(&mut self) {
        if !self.done {
            *self.mgr.lock_claim() = None;
        }
    }
}

pub struct SessionManager {
    inner: Mutex<Option<ActiveSession>>,
    claim: Mutex<Option<String>>,
    /// Bumped on every lock/attach/take so stale idle timers can be told apart.
    generation: AtomicU64,
    pub idle_logout: Duration,
}

impl SessionManager {
    pub fn new(idle_logout: Duration) -> Self {
        Self {
            inner: Mutex::new(None),
            claim: Mutex::new(None),
            generation: AtomicU64::new(0),
            idle_logout,
        }
    }

    // The critical sections below are plain Option/String mutations that
    // leave the state valid even if a holder panics, so recover from poison
    // instead of wedging every later login (same idiom as flux-input sink.rs).
    fn lock_inner(&self) -> MutexGuard<'_, Option<ActiveSession>> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn lock_claim(&self) -> MutexGuard<'_, Option<String>> {
        self.claim.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Monotonic counter of lock/attach/logout events.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// Current occupancy for the login check.
    pub fn state(&self) -> SessionState {
        if let Some(user) = self.lock_claim().clone() {
            return SessionState::Claimed { user };
        }
        let guard = self.lock_inner();
        match guard.as_ref() {
            None => SessionState::Free,
            Some(s) if s.locked_since.is_some() => SessionState::Locked {
                user: s.user.name.clone(),
            },
            Some(s) => SessionState::Active {
                user: s.user.name.clone(),
            },
        }
    }

    /// Atomically claim the session slot for a fresh login. Returns None if a
    /// session or another claim already exists.
    pub fn try_claim(&self, user: &str) -> Option<Claim<'_>> {
        if self.lock_inner().is_some() {
            return None;
        }
        let mut claim = self.lock_claim();
        if claim.is_some() {
            return None;
        }
        *claim = Some(user.to_string());
        Some(Claim { mgr: self, done: false })
    }

    /// Install a freshly created session as active (consumes the claim).
    pub fn open(&self, user: OsUser, pam_guard: PamSessionGuard) {
        *self.lock_claim() = None;
        *self.lock_inner() = Some(ActiveSession {
            user,
            pam_guard: Some(pam_guard),
            locked_since: None,
        });
    }

    /// Atomically re-attach a locked session for `user`; false when the
    /// session isn't locked by that user (already taken by another login).
    pub fn try_attach(&self, user: &str) -> bool {
        if let Some(s) = self.lock_inner().as_mut() {
            if s.locked_since.is_some() && s.user.name == user {
                s.locked_since = None;
                self.generation.fetch_add(1, Ordering::SeqCst);
                return true;
            }
        }
        false
    }

    /// Mark the current session locked (relay dropped / lock command).
    /// Returns the user.
    pub fn lock(&self) -> Option<String> {
        if let Some(s) = self.lock_inner().as_mut() {
            s.locked_since = Some(Instant::now());
            self.generation.fetch_add(1, Ordering::SeqCst);
            return Some(s.user.name.clone());
        }
        None
    }

    /// Take the session out (for logout). Caller runs cleanup.
    pub fn take(&self) -> Option<ActiveSession> {
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.lock_inner().take()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn user(name: &str) -> OsUser {
        OsUser {
            name: name.into(),
            uid: 2101,
            gid: 8001,
            home: PathBuf::from("/home/u"),
            shell: PathBuf::from("/bin/bash"),
        }
    }

    #[test]
    fn second_claim_rejected() {
        let mgr = SessionManager::new(Duration::ZERO);
        let c1 = mgr.try_claim("a").unwrap();
        assert!(mgr.try_claim("b").is_none());
        assert!(matches!(mgr.state(), SessionState::Claimed { .. }));
        drop(c1);
        assert!(matches!(mgr.state(), SessionState::Free));
        assert!(mgr.try_claim("b").is_some());
    }

    #[test]
    fn claim_released_on_drop() {
        let mgr = SessionManager::new(Duration::ZERO);
        drop(mgr.try_claim("a"));
        assert!(mgr.try_claim("b").is_some());
    }

    #[test]
    fn attach_only_for_locked_same_user() {
        let mgr = SessionManager::new(Duration::ZERO);
        // Simulate an installed session without a pam guard by poking state.
        *mgr.lock_inner() = Some(ActiveSession {
            user: user("a"),
            pam_guard: None,
            locked_since: Some(Instant::now()),
        });
        assert!(!mgr.try_attach("b"));
        assert!(mgr.try_attach("a"));
        // Second attach for the same user now fails (already active).
        assert!(!mgr.try_attach("a"));
        assert!(matches!(mgr.state(), SessionState::Active { .. }));
    }

    #[test]
    fn poisoned_locks_do_not_wedge_logins() {
        let mgr = SessionManager::new(Duration::ZERO);
        std::thread::scope(|s| {
            let _ = s
                .spawn(|| {
                    let _claim = mgr.claim.lock().unwrap();
                    let _inner = mgr.inner.lock().unwrap();
                    panic!("poison both locks");
                })
                .join();
        });
        assert!(mgr.claim.is_poisoned() && mgr.inner.is_poisoned());
        assert!(matches!(mgr.state(), SessionState::Free));
        let claim = mgr.try_claim("a").unwrap();
        assert!(mgr.try_claim("b").is_none());
        drop(claim);
        assert!(mgr.try_claim("b").is_some());
        assert!(mgr.lock().is_none());
        assert!(mgr.take().is_none());
    }
}
