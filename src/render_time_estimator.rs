use std::time::Duration;

/// Exponential moving average with geometric decay.
///
/// Used to estimate render+commit times. A higher weight means faster
/// reaction to new samples (more weight to recent values).
#[derive(Debug)]
struct GeometricDecay {
    weight: f64,
    value: f64,
}

impl GeometricDecay {
    fn new(weight: f64, default_ns: f64) -> Self {
        Self {
            weight,
            // Store the initial value pre-divided so get() returns default_ns.
            value: default_ns / weight,
        }
    }

    fn add(&mut self, sample_ns: f64) {
        self.value = sample_ns + (1.0 - self.weight) * self.value;
    }

    fn get(&self) -> f64 {
        self.weight * self.value
    }
}

/// Estimates the optimal render deadline based on measured render+commit times
/// and frame drop history.
///
/// The estimator tracks:
/// - **render_commit_margin**: measured GPU render + kernel page-flip time
///   (fast-reacting EMA, weight 0.5)
/// - **drop_bump**: extra margin added when frame drops are detected, decays
///   gradually when frames land on time
///
/// The render deadline is `next_vblank - render_commit_margin - drop_bump`.
///
/// `record_render_time()` is called with the elapsed time from the start of
/// `Tty::render()` through successful `queue_frame()`, so it captures both
/// GPU rendering and the kernel commit in a single measurement.
#[derive(Debug)]
pub struct RenderTimeEstimator {
    render_commit_decay: GeometricDecay,
    fixed_margin_ns: Option<u64>,
    /// Extra margin added on frame drops, in nanoseconds.
    drop_bump_ns: f64,
    /// Last DRM sequence we saw (for drop detection).
    last_sequence: Option<u32>,
    /// Refresh interval for capping margins.
    refresh_interval_ns: Option<u64>,
}

impl RenderTimeEstimator {
    /// Default render+commit margin: 5ms (conservative initial estimate).
    const DEFAULT_RENDER_COMMIT_NS: f64 = 5_000_000.0;
    /// Extra slack for timer wakeup jitter and compositor work done before `Tty::render()`.
    const PRE_RENDER_SLACK_NS: f64 = 2_000_000.0;
    /// Ignore tiny presentation timing errors; they are usually just noise.
    const PRESENTATION_LATE_TOLERANCE_NS: f64 = 250_000.0;
    /// Bump added to margin on frame drop: 500µs.
    const DROP_BUMP_NS: f64 = 500_000.0;

    pub fn new() -> Self {
        Self {
            render_commit_decay: GeometricDecay::new(0.5, Self::DEFAULT_RENDER_COMMIT_NS),
            fixed_margin_ns: None,
            drop_bump_ns: 0.0,
            last_sequence: None,
            refresh_interval_ns: None,
        }
    }

    /// Set the refresh interval (used for capping the total margin).
    pub fn set_refresh_interval(&mut self, interval: Duration) {
        self.refresh_interval_ns = Some(interval.as_nanos() as u64);
    }

    pub fn set_fixed_margin(&mut self, margin: Duration) {
        self.fixed_margin_ns = Some(margin.as_nanos() as u64);
    }

    /// Total estimated margin (render+commit EMA + drop bump).
    pub fn total_margin(&self) -> Duration {
        let total_ns = if let Some(fixed_margin_ns) = self.fixed_margin_ns {
            fixed_margin_ns as f64
        } else {
            self.render_commit_decay.get() + self.drop_bump_ns + Self::PRE_RENDER_SLACK_NS
        };

        // Cap at refresh interval if known.
        let capped_ns = if let Some(refresh_ns) = self.refresh_interval_ns {
            total_ns.min(refresh_ns as f64)
        } else {
            total_ns
        };

        Duration::from_nanos(capped_ns as u64)
    }

    /// Calculate the render deadline given the next vblank time.
    ///
    /// Returns `None` if the deadline has already passed or is too close (< 500µs).
    pub fn deadline(&self, next_vblank: Duration, now: Duration) -> Option<Duration> {
        let margin = self.total_margin();
        let deadline = next_vblank.saturating_sub(margin);

        // If the deadline is less than 500µs away, render immediately.
        if deadline <= now + Duration::from_micros(500) {
            None
        } else {
            Some(deadline)
        }
    }

    /// Record a render + commit duration sample.
    ///
    /// This should be the elapsed time from the start of rendering through
    /// successful `queue_frame()`.
    pub fn record_render_time(&mut self, duration: Duration) {
        self.render_commit_decay.add(duration.as_nanos() as f64);
    }

    /// Record that the frame presented later than predicted.
    pub fn record_late_presentation(&mut self, late_by: Duration) {
        let late_ns = late_by.as_nanos() as f64;
        if late_ns <= Self::PRESENTATION_LATE_TOLERANCE_NS {
            return;
        }

        self.drop_bump_ns = self
            .drop_bump_ns
            .max(late_ns + Self::PRESENTATION_LATE_TOLERANCE_NS);

        if let Some(refresh_ns) = self.refresh_interval_ns {
            self.drop_bump_ns = self.drop_bump_ns.min(refresh_ns as f64);
        }
    }

    /// Called on vblank with the DRM sequence number to detect frame drops.
    pub fn on_vblank(&mut self, sequence: u32) {
        if let Some(last) = self.last_sequence {
            let delta = sequence.wrapping_sub(last);
            if delta > 1 {
                // Frame(s) were dropped — bump the margin.
                self.drop_bump_ns += Self::DROP_BUMP_NS * (delta - 1) as f64;

                // Cap bump at refresh interval.
                if let Some(refresh_ns) = self.refresh_interval_ns {
                    self.drop_bump_ns = self.drop_bump_ns.min(refresh_ns as f64);
                }
            } else {
                // No drop — let bump decay gradually.
                self.drop_bump_ns *= 0.95;
                if self.drop_bump_ns < 1000.0 {
                    self.drop_bump_ns = 0.0;
                }
            }
        }
        self.last_sequence = Some(sequence);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_margin() {
        let estimator = RenderTimeEstimator::new();
        let margin = estimator.total_margin();
        // Default: 5ms render+commit + 2ms pre-render slack
        assert!(
            margin.as_micros() >= 6900 && margin.as_micros() <= 7100,
            "expected ~7ms, got {:?}",
            margin
        );
    }

    #[test]
    fn deadline_in_future() {
        let estimator = RenderTimeEstimator::new();
        let now = Duration::from_millis(100);
        let next_vblank = Duration::from_millis(116); // 16ms in future

        let deadline = estimator.deadline(next_vblank, now);
        assert!(deadline.is_some());
        let d = deadline.unwrap();
        // Should be ~109ms (116 - 7)
        assert!(d > now + Duration::from_micros(500));
        assert!(d < next_vblank);
    }

    #[test]
    fn deadline_too_close_renders_immediately() {
        let estimator = RenderTimeEstimator::new();
        let now = Duration::from_millis(112);
        let next_vblank = Duration::from_millis(116); // Only 4ms away, margin is 7ms

        let deadline = estimator.deadline(next_vblank, now);
        assert!(deadline.is_none());
    }

    #[test]
    fn render_time_updates_margin() {
        let mut estimator = RenderTimeEstimator::new();

        // Feed several large render times.
        for _ in 0..20 {
            estimator.record_render_time(Duration::from_millis(8));
        }

        let margin = estimator.total_margin();
        // Should approach 8ms.
        assert!(
            margin > Duration::from_micros(7900),
            "margin should be > ~8ms, got {:?}",
            margin
        );
    }

    #[test]
    fn render_time_converges_down() {
        let mut estimator = RenderTimeEstimator::new();

        // Feed small render times — margin should drop below the 7ms default.
        for _ in 0..100 {
            estimator.record_render_time(Duration::from_micros(500));
        }

        let margin = estimator.total_margin();
        assert!(
            margin < Duration::from_millis(3),
            "margin should converge to ~2.5ms, got {:?}",
            margin
        );
    }

    #[test]
    fn frame_drop_bumps_margin() {
        let mut estimator = RenderTimeEstimator::new();
        let initial_margin = estimator.total_margin();

        // Simulate first vblank.
        estimator.on_vblank(1);
        // Simulate a dropped frame (sequence jumps by 2).
        estimator.on_vblank(3);

        let after_drop = estimator.total_margin();
        assert!(
            after_drop > initial_margin,
            "margin should increase after drop: {:?} vs {:?}",
            after_drop,
            initial_margin
        );
    }

    #[test]
    fn late_presentation_bumps_margin() {
        let mut estimator = RenderTimeEstimator::new();
        let initial_margin = estimator.total_margin();

        estimator.record_late_presentation(Duration::from_micros(800));

        let bumped = estimator.total_margin();
        assert!(
            bumped > initial_margin,
            "margin should increase after late presentation: {:?} vs {:?}",
            bumped,
            initial_margin
        );
    }

    #[test]
    fn tiny_presentation_error_is_ignored() {
        let mut estimator = RenderTimeEstimator::new();
        let initial_margin = estimator.total_margin();

        estimator.record_late_presentation(Duration::from_micros(100));

        let bumped = estimator.total_margin();
        assert_eq!(bumped, initial_margin);
    }

    #[test]
    fn fixed_margin_overrides_adaptive_estimate() {
        let mut estimator = RenderTimeEstimator::new();
        estimator.set_fixed_margin(Duration::from_millis(3));

        for _ in 0..20 {
            estimator.record_render_time(Duration::from_millis(8));
        }
        estimator.record_late_presentation(Duration::from_millis(4));
        estimator.on_vblank(1);
        estimator.on_vblank(3);

        assert_eq!(estimator.total_margin(), Duration::from_millis(3));
    }

    #[test]
    fn frame_drop_bump_decays() {
        let mut estimator = RenderTimeEstimator::new();

        estimator.on_vblank(1);
        estimator.on_vblank(3); // drop

        let after_drop = estimator.total_margin();

        // Simulate many good frames.
        for i in 4..104 {
            estimator.on_vblank(i);
        }

        let after_recovery = estimator.total_margin();
        assert!(
            after_recovery < after_drop,
            "margin should decrease after recovery: {:?} vs {:?}",
            after_recovery,
            after_drop
        );
    }

    #[test]
    fn margin_capped_at_refresh_interval() {
        let mut estimator = RenderTimeEstimator::new();
        estimator.set_refresh_interval(Duration::from_millis(4)); // Very short refresh

        let margin = estimator.total_margin();
        assert!(
            margin <= Duration::from_millis(4),
            "margin should be capped at 4ms, got {:?}",
            margin
        );
    }

    #[test]
    fn geometric_decay_converges() {
        let mut decay = GeometricDecay::new(0.5, 4_000_000.0);

        // Feed constant 2ms samples.
        for _ in 0..100 {
            decay.add(2_000_000.0);
        }

        let value = decay.get();
        // Should converge close to 2ms.
        assert!(
            (value - 2_000_000.0).abs() < 100_000.0,
            "expected ~2ms, got {:.0}ns",
            value
        );
    }

    #[test]
    fn refresh_interval_update_changes_cap() {
        let mut estimator = RenderTimeEstimator::new();

        // Default margin is ~5ms. Set refresh to 6.9ms (144Hz).
        estimator.set_refresh_interval(Duration::from_nanos(6_944_444));
        let margin_144 = estimator.total_margin();
        assert!(
            margin_144 <= Duration::from_nanos(6_944_444),
            "margin should be capped at 144Hz refresh: {:?}",
            margin_144
        );

        // Switch to 60Hz (16.67ms) — margin should no longer be capped.
        estimator.set_refresh_interval(Duration::from_nanos(16_666_667));
        let margin_60 = estimator.total_margin();
        assert!(
            margin_60 >= Duration::from_micros(4900),
            "margin should not be capped at 60Hz: {:?}",
            margin_60
        );
    }

    /// Simulates a full frame scheduling cycle:
    /// input arrives → estimator computes deadline → render happens near deadline →
    /// frame is submitted → vblank arrives → next cycle.
    #[test]
    fn simulated_frame_scheduling_cycle() {
        let mut estimator = RenderTimeEstimator::new();
        estimator.set_refresh_interval(Duration::from_nanos(16_666_667)); // 60Hz

        let mut vblank_time = Duration::from_millis(100);
        let refresh = Duration::from_nanos(16_666_667);

        // Simulate 60 frames.
        for seq in 1..=60u32 {
            let next_vblank = vblank_time + refresh;

            // Input arrives some time after vblank.
            let input_time = vblank_time + Duration::from_millis(2);

            // Estimator computes deadline.
            let deadline = estimator.deadline(next_vblank, input_time);

            if let Some(d) = deadline {
                // Deadline should be between input_time and next_vblank.
                assert!(
                    d > input_time,
                    "frame {seq}: deadline {d:?} should be after input {input_time:?}"
                );
                assert!(
                    d < next_vblank,
                    "frame {seq}: deadline {d:?} should be before vblank {next_vblank:?}"
                );

                // Simulate render starting at deadline, taking ~0.5ms.
                let render_duration = Duration::from_micros(500);
                let render_end = d + render_duration;

                // Render should finish before vblank.
                assert!(render_end <= next_vblank,
                    "frame {seq}: render end {render_end:?} should be before vblank {next_vblank:?}");

                estimator.record_render_time(render_duration);
            }

            // Vblank arrives.
            estimator.on_vblank(seq);
            vblank_time = next_vblank;
        }

        // After 60 frames of 0.5ms renders, margin should have converged down.
        let final_margin = estimator.total_margin();
        assert!(
            final_margin < Duration::from_millis(3),
            "margin should converge to ~2.5ms after 60 frames, got {:?}",
            final_margin
        );
    }
}
