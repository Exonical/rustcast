use std::time::{Duration, Instant};

pub struct CapturePacer {
    interval: Duration,
    next_due: Option<Instant>,
}

impl CapturePacer {
    pub fn new(interval: Duration) -> Self {
        Self {
            interval,
            next_due: None,
        }
    }

    fn tolerance(&self) -> Duration {
        self.interval / 10
    }

    pub fn is_due(&self, now: Instant) -> bool {
        match self.next_due {
            Some(next_due) => now + self.tolerance() >= next_due,
            None => true,
        }
    }

    pub fn time_until_due(&self, now: Instant) -> Duration {
        if self.is_due(now) {
            return Duration::ZERO;
        }
        let Some(next_due) = self.next_due else {
            return Duration::ZERO;
        };
        next_due - self.tolerance() - now
    }

    pub fn mark_delivered(&mut self, now: Instant) {
        let grid = self.next_due.unwrap_or(now) + self.interval;
        self.next_due = Some(if grid <= now {
            now + self.interval
        } else {
            grid.max(now + self.interval / 2)
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_frame_is_due_immediately() {
        let base = Instant::now();
        let pacer = CapturePacer::new(Duration::from_millis(16));

        assert!(pacer.is_due(base));
    }

    #[test]
    fn allows_due_present_with_tolerance() {
        let base = Instant::now();
        let interval = Duration::from_micros(1_000_000 / 60);
        let mut pacer = CapturePacer::new(interval);
        pacer.mark_delivered(base);

        assert!(!pacer.is_due(base + Duration::from_millis(10)));
        assert!(pacer.is_due(base + Duration::from_millis(16)));
    }

    #[test]
    fn presents_at_144_hz_yield_target_delivery_rate() {
        let base = Instant::now();
        let mut pacer = CapturePacer::new(Duration::from_micros(1_000_000 / 60));
        let present_interval = Duration::from_micros(1_000_000 / 144);
        let mut deliveries = 0;

        for present in 0..=144 {
            let now = base + present_interval * present;
            if pacer.is_due(now) {
                deliveries += 1;
                pacer.mark_delivered(now);
            }
        }

        assert!((59..=61).contains(&deliveries), "delivered {deliveries} frames");
    }

    #[test]
    fn consistently_late_presents_preserve_target_delivery_rate() {
        let base = Instant::now();
        let interval = Duration::from_micros(1_000_000 / 60);
        let late = Duration::from_millis(6);
        let mut pacer = CapturePacer::new(interval);
        let mut deliveries = 0;

        for frame in 0..60 {
            let now = base + interval * frame + late;
            if pacer.is_due(now) {
                deliveries += 1;
                pacer.mark_delivered(now);
            }
        }

        assert!((59..=61).contains(&deliveries), "delivered {deliveries} frames");
    }

    #[test]
    fn stall_does_not_cause_a_burst() {
        let base = Instant::now();
        let interval = Duration::from_micros(1_000_000 / 60);
        let mut pacer = CapturePacer::new(interval);
        pacer.mark_delivered(base);

        let first_after_stall = base + Duration::from_millis(500);
        assert!(pacer.is_due(first_after_stall));
        pacer.mark_delivered(first_after_stall);

        let second_after_stall = first_after_stall + pacer.time_until_due(first_after_stall);
        assert!(pacer.is_due(second_after_stall));
        assert!(second_after_stall - first_after_stall >= interval * 8 / 10);
    }

    #[test]
    fn time_until_due_is_zero_when_due_and_exact_otherwise() {
        let base = Instant::now();
        let interval = Duration::from_micros(1_000_000 / 60);
        let mut pacer = CapturePacer::new(interval);
        pacer.mark_delivered(base);

        assert_eq!(pacer.time_until_due(base + Duration::from_millis(16)), Duration::ZERO);
        assert_eq!(
            pacer.time_until_due(base + Duration::from_millis(10)),
            interval - interval / 10 - Duration::from_millis(10)
        );
    }
}
