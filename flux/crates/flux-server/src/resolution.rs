//! Resolution change requests and the status reported back to clients.

use std::sync::Arc;
use std::time::Duration;

#[derive(Clone, Copy, Debug)]
pub(crate) struct ModeRequest {
    pub width: u32,
    pub height: u32,
}

#[derive(Clone, Debug, serde::Serialize)]
pub(crate) struct ResolutionStatus {
    pub state: &'static str,
    pub width: u32,
    pub height: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_height: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encode_width: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encode_height: Option<u32>,
}

pub(crate) type ResolutionStatusMessage = Arc<(u64, Vec<u8>)>;

pub(crate) fn publish_resolution_status(
    tx: &tokio::sync::broadcast::Sender<ResolutionStatusMessage>,
    status: ResolutionStatus,
) {
    if let Ok(payload) = serde_json::to_vec(&status) {
        let _ = tx.send(Arc::new((0, payload)));
    }
}

#[derive(Default)]
pub(crate) struct ResolutionStatusTracker {
    pending_success: Option<((u32, u32), ResolutionStatus)>,
    latest_started: Option<(u32, u32)>,
    recovering: Option<(u32, u32)>,
}

impl ResolutionStatusTracker {
    pub(crate) fn on_transition_started(&mut self, width: u32, height: u32) {
        self.latest_started = Some((width, height));
        self.pending_success = None;
    }

    pub(crate) fn on_transition_applied(&mut self, status: ResolutionStatus) {
        let requested = (status.width, status.height);
        if self.latest_started == Some(requested) {
            self.pending_success = Some((requested, status));
        }
    }

    pub(crate) fn on_transition_failed(&mut self, width: u32, height: u32) {
        if self.latest_started == Some((width, height)) {
            self.pending_success = None;
        }
    }

    pub(crate) fn on_encoder_failed(&mut self, width: u32, height: u32, retry: Duration) -> ResolutionStatus {
        self.recovering = Some((width, height));
        ResolutionStatus {
            state: "recovering",
            width,
            height,
            previous_width: None,
            previous_height: None,
            error: Some(format!(
                "encoder failed to start at {width}x{height}; retrying in {}s",
                retry.as_secs()
            )),
            encode_width: None,
            encode_height: None,
        }
    }

    pub(crate) fn on_packet_emitted(&mut self, encode_width: u32, encode_height: u32) -> Option<ResolutionStatus> {
        let pending_success = self
            .pending_success
            .take()
            .filter(|(requested, _)| self.latest_started == Some(*requested));
        let mut status = if let Some((_, status)) = pending_success {
            status
        } else {
            let (width, height) = self.recovering.take()?;
            ResolutionStatus {
                state: "succeeded",
                width,
                height,
                previous_width: None,
                previous_height: None,
                error: None,
                encode_width: None,
                encode_height: None,
            }
        };
        status.state = "succeeded";
        status.error = None;
        status.encode_width = Some(encode_width);
        status.encode_height = Some(encode_height);
        self.recovering = None;
        Some(status)
    }
}

pub(crate) fn validate_mode_request(request: ModeRequest) -> Result<ModeRequest, &'static str> {
    if request.width < 640 || request.height < 480 {
        return Err("resolution is implausibly small");
    }
    if request.width > 2560 || request.height > 1440 {
        return Err("resolution exceeds 2560x1440");
    }
    if request.width % 2 != 0 || request.height % 2 != 0 {
        return Err("resolution dimensions must be even");
    }
    Ok(request)
}

pub(crate) fn mode_change_needed(current: ModeRequest, requested: ModeRequest) -> bool {
    current.width != requested.width || current.height != requested.height
}

/// How long a compositor-side mode change may take to show up in the frames.
#[cfg_attr(target_os = "windows", allow(dead_code))]
pub(crate) const MODE_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// A mode change that was handed to the capture backend and is waiting for
/// frames at the requested size.
#[cfg_attr(target_os = "windows", allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PendingMode {
    pub(crate) requested: flux_core::types::Resolution,
    pub(crate) previous: flux_core::types::Resolution,
    deadline: std::time::Instant,
}

#[cfg_attr(target_os = "windows", allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum PendingModeState {
    Waiting,
    Applied,
    TimedOut,
}

#[cfg_attr(target_os = "windows", allow(dead_code))]
impl PendingMode {
    pub(crate) fn new(
        requested: flux_core::types::Resolution,
        previous: flux_core::types::Resolution,
        now: std::time::Instant,
    ) -> Self {
        Self {
            requested,
            previous,
            deadline: now + MODE_REQUEST_TIMEOUT,
        }
    }

    /// Where the request stands given the resolution of the latest frame (if
    /// any arrived) at `now`. A frame at the requested size wins over the deadline.
    pub(crate) fn state(&self, frame: Option<flux_core::types::Resolution>, now: std::time::Instant) -> PendingModeState {
        if frame == Some(self.requested) {
            PendingModeState::Applied
        } else if now >= self.deadline {
            PendingModeState::TimedOut
        } else {
            PendingModeState::Waiting
        }
    }
}

#[cfg(test)]
mod resolution_status_tests {
    use super::*;

    #[test]
    fn transition_success_waits_for_encoded_packet_and_records_encode_size() {
        let mut tracker = ResolutionStatusTracker::default();
        tracker.on_transition_started(2560, 1440);
        tracker.on_transition_applied(ResolutionStatus {
            state: "succeeded",
            width: 2560,
            height: 1440,
            previous_width: Some(1920),
            previous_height: Some(1080),
            error: None,
            encode_width: None,
            encode_height: None,
        });

        let status = tracker.on_packet_emitted(1920, 1080).unwrap();
        assert_eq!(status.state, "succeeded");
        assert_eq!((status.width, status.height), (2560, 1440));
        assert_eq!((status.encode_width, status.encode_height), (Some(1920), Some(1080)));
        assert_eq!(
            (status.previous_width, status.previous_height),
            (Some(1920), Some(1080))
        );
        assert!(tracker.on_packet_emitted(1920, 1080).is_none());
    }

    #[test]
    fn encoder_recovery_publishes_recovering_then_succeeded() {
        let mut tracker = ResolutionStatusTracker::default();
        let recovering = tracker.on_encoder_failed(1280, 720, Duration::from_secs(4));
        assert_eq!(recovering.state, "recovering");
        assert_eq!(
            recovering.error.as_deref(),
            Some("encoder failed to start at 1280x720; retrying in 4s")
        );

        let succeeded = tracker.on_packet_emitted(1280, 720).unwrap();
        assert_eq!(succeeded.state, "succeeded");
        assert_eq!((succeeded.width, succeeded.height), (1280, 720));
        assert_eq!(
            (succeeded.encode_width, succeeded.encode_height),
            (Some(1280), Some(720))
        );
        assert!(succeeded.error.is_none());
    }

    #[test]
    fn pending_transition_success_takes_precedence_over_recovery_clear() {
        let mut tracker = ResolutionStatusTracker::default();
        tracker.on_transition_started(1600, 900);
        tracker.on_transition_applied(ResolutionStatus {
            state: "succeeded",
            width: 1600,
            height: 900,
            previous_width: Some(1920),
            previous_height: Some(1080),
            error: None,
            encode_width: None,
            encode_height: None,
        });
        tracker.on_encoder_failed(1600, 900, Duration::from_secs(2));

        let succeeded = tracker.on_packet_emitted(1600, 900).unwrap();
        assert_eq!((succeeded.width, succeeded.height), (1600, 900));
        assert_eq!(
            (succeeded.previous_width, succeeded.previous_height),
            (Some(1920), Some(1080))
        );
    }

    #[test]
    fn newer_failed_transition_discards_older_pending_success() {
        let mut tracker = ResolutionStatusTracker::default();
        tracker.on_transition_started(1280, 720);
        tracker.on_transition_applied(ResolutionStatus {
            state: "succeeded",
            width: 1280,
            height: 720,
            previous_width: Some(1920),
            previous_height: Some(1080),
            error: None,
            encode_width: None,
            encode_height: None,
        });
        tracker.on_transition_started(1600, 900);
        tracker.on_transition_failed(1600, 900);

        assert!(tracker.on_packet_emitted(1280, 720).is_none());
    }
}

#[cfg(test)]
mod mode_request_tests {
    use super::*;

    #[test]
    fn rejects_invalid_mode_requests() {
        assert!(validate_mode_request(ModeRequest { width: 2561, height: 1440 }).is_err());
        assert!(validate_mode_request(ModeRequest { width: 1921, height: 1080 }).is_err());
        assert!(validate_mode_request(ModeRequest { width: 320, height: 240 }).is_err());
    }

    #[test]
    fn accepts_and_preserves_valid_mode_requests() {
        let request = ModeRequest { width: 2560, height: 1080 };
        assert_eq!(validate_mode_request(request).unwrap().width, 2560);
        assert!(!mode_change_needed(request, request));
        assert!(mode_change_needed(request, ModeRequest { width: 1920, height: 1080 }));
    }
}

#[cfg(test)]
mod pending_mode_tests {
    use super::*;
    use flux_core::types::Resolution;
    use std::time::{Duration, Instant};

    fn pending(now: Instant) -> PendingMode {
        PendingMode::new(Resolution::new(1280, 720), Resolution::new(1920, 1080), now)
    }

    #[test]
    fn waits_while_frames_keep_the_old_size() {
        let now = Instant::now();
        let request = pending(now);
        assert_eq!(request.state(None, now), PendingModeState::Waiting);
        assert_eq!(
            request.state(Some(Resolution::new(1920, 1080)), now + Duration::from_secs(1)),
            PendingModeState::Waiting
        );
    }

    #[test]
    fn applies_on_the_first_frame_at_the_requested_size() {
        let now = Instant::now();
        assert_eq!(
            pending(now).state(Some(Resolution::new(1280, 720)), now + Duration::from_millis(300)),
            PendingModeState::Applied
        );
    }

    #[test]
    fn times_out_after_the_deadline_without_a_matching_frame() {
        let now = Instant::now();
        let request = pending(now);
        assert_eq!(request.state(None, now + MODE_REQUEST_TIMEOUT), PendingModeState::TimedOut);
        assert_eq!(
            request.state(Some(Resolution::new(1920, 1080)), now + MODE_REQUEST_TIMEOUT + Duration::from_millis(1)),
            PendingModeState::TimedOut
        );
    }

    #[test]
    fn a_matching_frame_at_the_deadline_still_counts_as_applied() {
        let now = Instant::now();
        assert_eq!(
            pending(now).state(Some(Resolution::new(1280, 720)), now + MODE_REQUEST_TIMEOUT),
            PendingModeState::Applied
        );
    }
}
