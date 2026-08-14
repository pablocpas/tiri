use std::time::Duration;

/// Time-weighted render duration estimator.
///
/// This follows KWin's mature `RenderJournal` model: the prediction is the moving render-time
/// estimate plus twice the observed positive deviation. Slow frames therefore move the deadline
/// earlier immediately, while faster frames pull it back gradually.
#[derive(Debug, Default)]
struct RenderJournal {
    render_time_ns: f64,
    positive_deviation_ns: f64,
    last_presentation: Option<Duration>,
}

impl RenderJournal {
    const VARIANCE_TIME_CONSTANT: Duration = Duration::from_secs(6);
    const RENDER_TIME_CONSTANT: Duration = Duration::from_millis(500);
    const FIRST_SAMPLE_INTERVAL: Duration = Duration::from_secs(10);

    fn mix(sample: f64, previous: f64, ratio: f64) -> f64 {
        sample * ratio + previous * (1.0 - ratio)
    }

    fn ratio(interval: Duration, time_constant: Duration, min: f64, max: f64) -> f64 {
        (interval.as_secs_f64() / time_constant.as_secs_f64()).clamp(min, max)
    }

    fn add(&mut self, render_time: Duration, presentation_time: Duration) {
        let interval = self
            .last_presentation
            .map(|last| presentation_time.saturating_sub(last))
            .unwrap_or(Self::FIRST_SAMPLE_INTERVAL);
        self.last_presentation = Some(presentation_time);

        let render_time_ns = render_time.as_nanos() as f64;
        let variance_ratio = Self::ratio(interval, Self::VARIANCE_TIME_CONSTANT, 0.001, 0.1);
        let positive_difference = (render_time_ns - self.render_time_ns).max(0.0);
        self.positive_deviation_ns = Self::mix(
            positive_difference,
            self.positive_deviation_ns,
            variance_ratio,
        )
        .max(positive_difference);

        let render_ratio = Self::ratio(interval, Self::RENDER_TIME_CONSTANT, 0.01, 1.0);
        self.render_time_ns = Self::mix(render_time_ns, self.render_time_ns, render_ratio);
    }

    fn result_ns(&self) -> f64 {
        self.render_time_ns + self.positive_deviation_ns * 2.0
    }
}

/// Predicts how long before vblank Tiri must start a compositing cycle.
///
/// The primary estimate is the KWin-style render journal. A separate late-presentation penalty
/// accounts for timer, scheduler, driver and GPU completion delays that are not visible in the CPU
/// render span. The penalty grows immediately and only starts decaying after a stable run.
#[derive(Debug, Default)]
pub struct RenderTimeEstimator {
    journal: RenderJournal,
    late_presentation_penalty_ns: f64,
    consecutive_on_time_presentations: u32,
    refresh_interval_ns: Option<u64>,
}

impl RenderTimeEstimator {
    /// KWin reserves one millisecond for timer and scheduler inaccuracies.
    const TIMER_SCHEDULER_SLACK_NS: f64 = 1_000_000.0;
    /// Ignore sub-quarter-millisecond timestamp noise.
    const PRESENTATION_LATE_TOLERANCE_NS: f64 = 250_000.0;
    /// Do not reduce a miss penalty until the output has been stable for a while.
    const STABLE_PRESENTATIONS_BEFORE_DECAY: u32 = 10;
    const PENALTY_DECAY: f64 = 0.95;
    /// Like KWin, assume that the GPU may have entered a low-power state after a long idle.
    const IDLE_REFRESH_INTERVALS: u32 = 100;
    const MIN_TIMER_HORIZON: Duration = Duration::from_micros(500);
    const MIN_VBLANK_HEADROOM: Duration = Duration::from_micros(1);

    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_refresh_interval(&mut self, interval: Duration) {
        self.refresh_interval_ns = Some(interval.as_nanos() as u64);
        self.cap_late_penalty();
    }

    /// Record the whole compositing critical path, ending at successful `queue_frame()`.
    ///
    /// The presentation timestamp makes adaptation independent of refresh rate, matching KWin's
    /// time-weighted journal rather than changing by an arbitrary amount per frame.
    pub fn record_render_time(&mut self, duration: Duration, presentation_time: Duration) {
        self.journal.add(duration, presentation_time);
    }

    /// Feed back whether the submitted frame reached the vblank it targeted.
    pub fn record_presentation_timing(&mut self, target: Duration, actual: Duration) {
        let late_ns = actual.saturating_sub(target).as_nanos() as f64;
        if late_ns > Self::PRESENTATION_LATE_TOLERANCE_NS {
            self.late_presentation_penalty_ns = self
                .late_presentation_penalty_ns
                .max(late_ns + Self::PRESENTATION_LATE_TOLERANCE_NS);
            self.consecutive_on_time_presentations = 0;
            self.cap_late_penalty();
            return;
        }

        self.consecutive_on_time_presentations =
            self.consecutive_on_time_presentations.saturating_add(1);
        if self.consecutive_on_time_presentations >= Self::STABLE_PRESENTATIONS_BEFORE_DECAY {
            self.late_presentation_penalty_ns *= Self::PENALTY_DECAY;
            if self.late_presentation_penalty_ns < 1_000.0 {
                self.late_presentation_penalty_ns = 0.0;
            }
        }
    }

    pub fn predicted_render_time(&self) -> Duration {
        Duration::from_nanos(self.journal.result_ns() as u64)
    }

    pub fn late_presentation_penalty(&self) -> Duration {
        Duration::from_nanos(self.late_presentation_penalty_ns as u64)
    }

    /// Adaptive margin for the current output state.
    pub fn adaptive_margin(&self, now: Duration, last_presentation: Option<Duration>) -> Duration {
        let mut total_ns = self.journal.result_ns()
            + self.late_presentation_penalty_ns
            + Self::TIMER_SCHEDULER_SLACK_NS;

        if let Some(refresh_ns) = self.refresh_interval_ns {
            let refresh = Duration::from_nanos(refresh_ns);
            let long_idle = last_presentation.is_some_and(|last| {
                now.saturating_sub(last) >= refresh.saturating_mul(Self::IDLE_REFRESH_INTERVALS)
            });
            let maximum_margin = refresh.saturating_sub(Self::MIN_VBLANK_HEADROOM);

            if long_idle {
                total_ns = total_ns.max(maximum_margin.as_nanos() as f64);
            }
            // KWin may extend this to two intervals by switching to triple buffering. Tiri keeps
            // one frame in flight, so an estimate at least as large as one interval means "start
            // now" instead.
            total_ns = total_ns.min(maximum_margin.as_nanos() as f64);
        }

        Duration::from_nanos(total_ns as u64)
    }

    /// Return a future render deadline, or `None` when rendering must start immediately.
    pub fn deadline(next_vblank: Duration, now: Duration, margin: Duration) -> Option<Duration> {
        let deadline = next_vblank.saturating_sub(margin);
        if deadline <= now + Self::MIN_TIMER_HORIZON {
            None
        } else {
            Some(deadline)
        }
    }

    fn cap_late_penalty(&mut self) {
        if let Some(refresh_ns) = self.refresh_interval_ns {
            self.late_presentation_penalty_ns = self
                .late_presentation_penalty_ns
                .min(refresh_ns.saturating_sub(1) as f64);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REFRESH_60_HZ: Duration = Duration::from_nanos(16_666_667);

    #[test]
    fn first_sample_is_deliberately_conservative() {
        let mut estimator = RenderTimeEstimator::new();
        estimator.record_render_time(Duration::from_millis(1), Duration::from_secs(1));

        // KWin's journal starts at render + 2 * first positive deviation.
        assert_eq!(estimator.predicted_render_time(), Duration::from_millis(3));
        assert_eq!(
            estimator.adaptive_margin(Duration::from_secs(1), Some(Duration::from_secs(1))),
            Duration::from_millis(4)
        );
    }

    #[test]
    fn slower_frame_moves_prediction_up_immediately() {
        let mut estimator = RenderTimeEstimator::new();
        estimator.record_render_time(Duration::from_millis(1), Duration::from_secs(1));
        let before = estimator.predicted_render_time();

        estimator.record_render_time(
            Duration::from_millis(4),
            Duration::from_secs(1) + REFRESH_60_HZ,
        );

        assert!(estimator.predicted_render_time() > before);
        assert!(estimator.predicted_render_time() >= Duration::from_millis(6));
    }

    #[test]
    fn faster_frames_reduce_prediction_gradually() {
        let mut estimator = RenderTimeEstimator::new();
        estimator.record_render_time(Duration::from_millis(5), Duration::from_secs(1));
        let high = estimator.predicted_render_time();

        let mut presentation = Duration::from_secs(1);
        for _ in 0..120 {
            presentation += REFRESH_60_HZ;
            estimator.record_render_time(Duration::from_micros(500), presentation);
        }

        let recovered = estimator.predicted_render_time();
        assert!(recovered < high);
        assert!(recovered > Duration::from_micros(500));
    }

    #[test]
    fn late_presentation_grows_margin_immediately() {
        let mut estimator = RenderTimeEstimator::new();
        estimator.set_refresh_interval(REFRESH_60_HZ);
        let before = estimator.adaptive_margin(Duration::from_secs(1), None);

        estimator.record_presentation_timing(
            Duration::from_secs(1),
            Duration::from_secs(1) + Duration::from_millis(2),
        );

        assert!(estimator.adaptive_margin(Duration::from_secs(1), None) > before);
        assert!(estimator.late_presentation_penalty() >= Duration::from_millis(2));
    }

    #[test]
    fn late_penalty_waits_for_stability_before_decaying() {
        let mut estimator = RenderTimeEstimator::new();
        estimator.set_refresh_interval(REFRESH_60_HZ);
        estimator.record_presentation_timing(
            Duration::from_secs(1),
            Duration::from_secs(1) + Duration::from_millis(2),
        );
        let penalty = estimator.late_presentation_penalty();

        for _ in 0..RenderTimeEstimator::STABLE_PRESENTATIONS_BEFORE_DECAY - 1 {
            estimator.record_presentation_timing(Duration::from_secs(2), Duration::from_secs(2));
        }
        assert_eq!(estimator.late_presentation_penalty(), penalty);

        estimator.record_presentation_timing(Duration::from_secs(2), Duration::from_secs(2));
        assert!(estimator.late_presentation_penalty() < penalty);
    }

    #[test]
    fn long_idle_forces_an_early_start() {
        let mut estimator = RenderTimeEstimator::new();
        estimator.set_refresh_interval(REFRESH_60_HZ);
        let last = Duration::from_secs(1);
        let now = last + REFRESH_60_HZ.saturating_mul(101);

        let margin = estimator.adaptive_margin(now, Some(last));
        assert!(margin >= REFRESH_60_HZ - Duration::from_micros(2));
    }

    #[test]
    fn adaptive_margin_is_capped_below_refresh_interval() {
        let mut estimator = RenderTimeEstimator::new();
        estimator.set_refresh_interval(Duration::from_millis(4));
        estimator.record_presentation_timing(
            Duration::from_secs(1),
            Duration::from_secs(1) + Duration::from_millis(20),
        );

        assert!(estimator.adaptive_margin(Duration::from_secs(1), None) < Duration::from_millis(4));
    }

    #[test]
    fn future_deadline_is_scheduled() {
        let deadline = RenderTimeEstimator::deadline(
            Duration::from_millis(116),
            Duration::from_millis(100),
            Duration::from_millis(4),
        );
        assert_eq!(deadline, Some(Duration::from_millis(112)));
    }

    #[test]
    fn elapsed_deadline_renders_immediately() {
        let deadline = RenderTimeEstimator::deadline(
            Duration::from_millis(116),
            Duration::from_millis(113),
            Duration::from_millis(4),
        );
        assert_eq!(deadline, None);
    }
}
