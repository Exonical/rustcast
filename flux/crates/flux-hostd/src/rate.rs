//! Per-peer failed-login rate limiting: after `max_failures` failures the
//! connection is closed and further attempts from that peer are delayed by
//! `cooldown`.

use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

#[derive(Debug)]
struct PeerState {
    failures: u32,
    blocked_until: Option<Instant>,
}

#[derive(Debug)]
pub struct LoginRateLimiter {
    max_failures: u32,
    cooldown: Duration,
    peers: HashMap<IpAddr, PeerState>,
}

impl LoginRateLimiter {
    pub fn new(max_failures: u32, cooldown: Duration) -> Self {
        Self {
            max_failures,
            cooldown,
            peers: HashMap::new(),
        }
    }

    /// True when the peer is currently inside its cooldown window and the
    /// connection should be refused.
    pub fn is_blocked(&mut self, peer: IpAddr) -> bool {
        match self.peers.get(&peer).and_then(|s| s.blocked_until) {
            Some(until) if until > Instant::now() => true,
            _ => false,
        }
    }

    /// Record a failed login. Returns true when this failure tripped the
    /// limit and the connection should be closed.
    pub fn record_failure(&mut self, peer: IpAddr) -> bool {
        let state = self.peers.entry(peer).or_insert(PeerState {
            failures: 0,
            blocked_until: None,
        });
        state.failures += 1;
        if state.failures >= self.max_failures {
            state.failures = 0;
            state.blocked_until = Some(Instant::now() + self.cooldown);
            return true;
        }
        false
    }

    /// Successful login clears the failure counter.
    pub fn record_success(&mut self, peer: IpAddr) {
        if let Some(state) = self.peers.get_mut(&peer) {
            state.failures = 0;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::Ipv4Addr;

    #[test]
    fn blocks_after_max_failures() {
        let peer = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let mut limiter = LoginRateLimiter::new(3, Duration::from_secs(10));
        assert!(!limiter.record_failure(peer));
        assert!(!limiter.record_failure(peer));
        assert!(limiter.record_failure(peer));
        assert!(limiter.is_blocked(peer));
    }

    #[test]
    fn success_resets_failures() {
        let peer = IpAddr::V4(Ipv4Addr::LOCALHOST);
        let mut limiter = LoginRateLimiter::new(3, Duration::from_secs(10));
        limiter.record_failure(peer);
        limiter.record_failure(peer);
        limiter.record_success(peer);
        assert!(!limiter.record_failure(peer));
        assert!(!limiter.record_failure(peer));
        assert!(limiter.record_failure(peer));
    }

    #[test]
    fn peers_are_independent() {
        let a = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1));
        let b = IpAddr::V4(Ipv4Addr::new(10, 0, 0, 2));
        let mut limiter = LoginRateLimiter::new(2, Duration::from_secs(10));
        limiter.record_failure(a);
        assert!(limiter.record_failure(a));
        assert!(limiter.is_blocked(a));
        assert!(!limiter.is_blocked(b));
        assert!(!limiter.record_failure(b));
    }
}
