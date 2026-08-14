//! End-to-end compositor latency instrumentation for Tracy builds.
//!
//! A sample follows the newest visual cause through queueing, rendering, KMS submission and the
//! DRM vblank that presents it. The module only exists with `profile-with-tracy`, so production
//! builds do not carry the tracker or per-output sample state.

use std::time::Duration;

const INPUT_TO_COMMIT_MAX_AGE: Duration = Duration::from_millis(750);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LatencySource {
    Keyboard,
    PointerMotion,
    PointerButton,
    PointerAxis,
    Touch,
    Gesture,
    Tablet,
    IpcAction,
    SurfaceCommit,
}

impl LatencySource {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Keyboard => "keyboard",
            Self::PointerMotion => "pointer-motion",
            Self::PointerButton => "pointer-button",
            Self::PointerAxis => "pointer-axis",
            Self::Touch => "touch",
            Self::Gesture => "gesture",
            Self::Tablet => "tablet",
            Self::IpcAction => "ipc-action",
            Self::SurfaceCommit => "surface-commit",
        }
    }
}

#[derive(Debug, Clone, Copy)]
struct LatencyTrigger {
    id: u64,
    source: LatencySource,
    received_at: Duration,
}

#[derive(Debug, Default)]
pub struct PerceptualLatency {
    next_id: u64,
    latest_trigger: Option<LatencyTrigger>,
}

#[derive(Debug, Clone, Copy)]
pub struct FrameLatencySample {
    pub id: u64,
    pub source: LatencySource,
    pub input_received_at: Option<Duration>,
    pub surface_commit_at: Option<Duration>,
    pub queued_at: Option<Duration>,
    pub render_started_at: Option<Duration>,
    pub submitted_at: Option<Duration>,
}

impl PerceptualLatency {
    pub fn note_trigger(&mut self, source: LatencySource, now: Duration) {
        let id = self.next_id();
        self.latest_trigger = Some(LatencyTrigger {
            id,
            source,
            received_at: now,
        });
        plot(tracy_client::plot_name!("latency.triggers"), 1.0);
    }

    pub fn surface_commit_sample(
        &mut self,
        now: Duration,
        last_latched_id: u64,
    ) -> FrameLatencySample {
        let linked = self.latest_trigger.filter(|trigger| {
            trigger.id > last_latched_id
                && now.saturating_sub(trigger.received_at) <= INPUT_TO_COMMIT_MAX_AGE
        });

        match linked {
            Some(trigger) => FrameLatencySample {
                id: trigger.id,
                source: trigger.source,
                input_received_at: Some(trigger.received_at),
                surface_commit_at: Some(now),
                queued_at: None,
                render_started_at: None,
                submitted_at: None,
            },
            None => FrameLatencySample {
                id: self.next_id(),
                source: LatencySource::SurfaceCommit,
                input_received_at: None,
                surface_commit_at: Some(now),
                queued_at: None,
                render_started_at: None,
                submitted_at: None,
            },
        }
    }

    pub fn trigger_sample(
        &self,
        now: Duration,
        last_latched_id: u64,
    ) -> Option<FrameLatencySample> {
        let trigger = self.latest_trigger?;
        if trigger.id <= last_latched_id
            || now.saturating_sub(trigger.received_at) > INPUT_TO_COMMIT_MAX_AGE
        {
            return None;
        }

        Some(FrameLatencySample {
            id: trigger.id,
            source: trigger.source,
            input_received_at: Some(trigger.received_at),
            surface_commit_at: None,
            queued_at: None,
            render_started_at: None,
            submitted_at: None,
        })
    }

    fn next_id(&mut self) -> u64 {
        self.next_id = self.next_id.wrapping_add(1).max(1);
        self.next_id
    }
}

impl FrameLatencySample {
    pub fn mark_queued(&mut self, now: Duration) {
        self.queued_at.get_or_insert(now);
    }

    pub fn mark_render_started(&mut self, now: Duration) {
        self.render_started_at = Some(now);
    }

    pub fn prepare_submission(mut self, now: Duration) -> Self {
        self.submitted_at = Some(now);
        self
    }

    pub fn emit_presented(
        self,
        backend: &'static str,
        output: &str,
        presented_at: Duration,
        target_presentation_time: Duration,
        refresh_interval: Option<Duration>,
        sequence: u32,
    ) {
        let input_ms = self
            .input_received_at
            .map(|time| duration_ms(presented_at.saturating_sub(time)));
        let commit_ms = self
            .surface_commit_at
            .map(|time| duration_ms(presented_at.saturating_sub(time)));
        let queue_ms = self
            .queued_at
            .map(|time| duration_ms(presented_at.saturating_sub(time)));
        let render_ms = self
            .render_started_at
            .map(|time| duration_ms(presented_at.saturating_sub(time)));
        let submit_ms = self
            .submitted_at
            .map(|time| duration_ms(presented_at.saturating_sub(time)));
        let late_ms = signed_duration_ms(presented_at, target_presentation_time);
        let refresh_ms = refresh_interval.map(duration_ms);

        if let Some(value) = input_ms {
            plot(
                tracy_client::plot_name!("latency.input_to_present_ms"),
                value,
            );
            if let Some(refresh_ms) = refresh_ms.filter(|value| *value > 0.0) {
                plot(
                    tracy_client::plot_name!("latency.input_to_present_frames"),
                    value / refresh_ms,
                );
            }
        }
        if let Some(value) = commit_ms {
            plot(
                tracy_client::plot_name!("latency.commit_to_present_ms"),
                value,
            );
        }
        if let Some(value) = queue_ms {
            plot(
                tracy_client::plot_name!("latency.queue_to_present_ms"),
                value,
            );
        }
        if let Some(value) = render_ms {
            plot(
                tracy_client::plot_name!("latency.render_to_present_ms"),
                value,
            );
        }
        if let Some(value) = submit_ms {
            plot(
                tracy_client::plot_name!("latency.submit_to_present_ms"),
                value,
            );
        }
        plot(tracy_client::plot_name!("latency.present_late_ms"), late_ms);
        plot(tracy_client::plot_name!("latency.samples_presented"), 1.0);

        if let Some(client) = tracy_client::Client::running() {
            client.message(
                &format!(
                    "latency.present id={} backend={} output={} source={} sequence={} input_ms={} \
                     commit_ms={} queue_ms={} render_ms={} submit_ms={} late_ms={:.6} refresh_ms={}",
                    self.id,
                    backend,
                    output,
                    self.source.as_str(),
                    sequence,
                    optional_ms(input_ms),
                    optional_ms(commit_ms),
                    optional_ms(queue_ms),
                    optional_ms(render_ms),
                    optional_ms(submit_ms),
                    late_ms,
                    optional_ms(refresh_ms),
                ),
                0,
            );
        }
    }
}

pub fn emit_coalesced_sample() {
    plot(tracy_client::plot_name!("latency.samples_coalesced"), 1.0);
}

pub fn emit_no_damage_sample(sample: FrameLatencySample) {
    plot(tracy_client::plot_name!("latency.samples_no_damage"), 1.0);
    if let Some(client) = tracy_client::Client::running() {
        client.message(
            &format!(
                "latency.no_damage id={} source={}",
                sample.id,
                sample.source.as_str()
            ),
            0,
        );
    }
}

fn duration_ms(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}

fn signed_duration_ms(value: Duration, reference: Duration) -> f64 {
    if value >= reference {
        duration_ms(value - reference)
    } else {
        -duration_ms(reference - value)
    }
}

fn optional_ms(value: Option<f64>) -> String {
    value.map_or_else(|| "-".to_owned(), |value| format!("{value:.6}"))
}

fn plot(name: tracy_client::PlotName, value: f64) {
    if let Some(client) = tracy_client::Client::running() {
        client.plot(name, value);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn surface_commit_links_recent_input() {
        let mut tracker = PerceptualLatency::default();
        tracker.note_trigger(LatencySource::Keyboard, Duration::from_millis(100));

        let sample = tracker.surface_commit_sample(Duration::from_millis(120), 0);
        assert_eq!(sample.source, LatencySource::Keyboard);
        assert_eq!(sample.input_received_at, Some(Duration::from_millis(100)));
        assert_eq!(sample.surface_commit_at, Some(Duration::from_millis(120)));
    }

    #[test]
    fn surface_commit_does_not_link_stale_input() {
        let mut tracker = PerceptualLatency::default();
        tracker.note_trigger(LatencySource::PointerButton, Duration::from_millis(100));

        let sample = tracker.surface_commit_sample(Duration::from_secs(1), 0);
        assert_eq!(sample.source, LatencySource::SurfaceCommit);
        assert_eq!(sample.input_received_at, None);
    }

    #[test]
    fn latched_trigger_is_not_reused() {
        let mut tracker = PerceptualLatency::default();
        tracker.note_trigger(LatencySource::IpcAction, Duration::from_millis(100));
        let sample = tracker
            .trigger_sample(Duration::from_millis(110), 0)
            .unwrap();

        assert!(tracker
            .trigger_sample(Duration::from_millis(120), sample.id)
            .is_none());
    }
}
