pub const RATE: u32 = 48_000;
pub const SAMPLES: usize = 480;
#[cfg(feature = "noise-cancellation")]
const FILTER_DELAY: std::time::Duration = crate::noise_cancellation::FILTER_DELAY;
#[cfg(not(feature = "noise-cancellation"))]
const FILTER_DELAY: std::time::Duration = std::time::Duration::ZERO;

pub const DELAY_PRESETS: [u32; 9] = [0, 25, 50, 75, 100, 125, 150, 175, 200];
pub const VOLUME_PRESETS: [u32; 9] = [0, 25, 50, 75, 100, 125, 150, 175, 200];
pub type Frame = [f32; 2];

/// Requested total delay, bounded by the measured time needed to get processed
/// audio to the output callback. Low presets must not schedule into the past.
pub fn playback_delay(requested_ms: u32, minimum: std::time::Duration) -> std::time::Duration {
    std::time::Duration::from_millis(requested_ms.into())
        .saturating_sub(FILTER_DELAY)
        .max(minimum)
}

/// Follow the device clock without transferring callback timestamp jitter to
/// individual samples. Slow correction tracks independent input/output clocks.
#[derive(Default)]
pub struct SampleClock(Option<std::time::Duration>);
impl SampleClock {
    pub fn block(
        &mut self,
        observed: std::time::Duration,
        frames: usize,
        rate: u32,
    ) -> std::time::Duration {
        use std::time::Duration;
        let predicted = self.0.unwrap_or(observed);
        let error = observed.abs_diff(predicted);
        let start = if error > Duration::from_millis(20) {
            observed
        } else if observed >= predicted {
            predicted + error / 100
        } else {
            predicted.saturating_sub(error / 100)
        };
        self.0 = Some(start + Duration::from_secs_f64(frames as f64 / rate as f64));
        start
    }
}

/// Filter output is scheduled against WASAPI's shared capture/playback clock.
#[derive(Clone, Copy)]
pub struct TimedFrame {
    pub at: std::time::Duration,
    pub value: Frame,
}

/// Interpolate on the device timeline so small clock corrections do not insert
/// silence or clicks. Expired audio is discarded instead of building latency.
pub fn take_due(
    previous: &mut Option<TimedFrame>,
    pending: &mut Option<TimedFrame>,
    now: std::time::Duration,
    period: std::time::Duration,
    mut pop: impl FnMut() -> Option<TimedFrame>,
) -> (Option<Frame>, u64) {
    use std::time::Duration;
    let mut late = 0;
    loop {
        if pending.is_none() {
            *pending = pop();
        }
        match pending {
            Some(frame) if frame.at <= now => {
                if now.saturating_sub(frame.at) > Duration::from_millis(3) {
                    late += 1;
                }
                *previous = pending.take();
            }
            _ => break,
        }
    }
    let Some(before) = previous else {
        return (None, late);
    };
    if let Some(after) = pending {
        let gap = after.at.saturating_sub(before.at);
        if gap > Duration::from_millis(3) || gap.is_zero() {
            return (None, late);
        }
        let mix = (now.saturating_sub(before.at).as_secs_f64() / gap.as_secs_f64()).clamp(0.0, 1.0)
            as f32;
        return (
            Some(std::array::from_fn(|i| {
                before.value[i] + (after.value[i] - before.value[i]) * mix
            })),
            late,
        );
    }
    if now.saturating_sub(before.at) < period {
        (Some(before.value), late)
    } else {
        (None, late)
    }
}

/// Streaming linear conversion for endpoints with different sample rates.
/// Position is an integer ratio, avoiding cumulative rounding drift.
pub struct Resampler {
    input_rate: u64,
    output_rate: u64,
    position: u64,
    previous: Option<Frame>,
}

impl Resampler {
    pub fn new(input_rate: u32, output_rate: u32) -> Self {
        Self {
            input_rate: input_rate.into(),
            output_rate: output_rate.into(),
            position: input_rate.into(),
            previous: None,
        }
    }

    pub fn push(&mut self, frame: Frame, mut emit: impl FnMut(Frame)) {
        let Some(previous) = self.previous.replace(frame) else {
            emit(frame);
            return;
        };
        while self.position <= self.output_rate {
            let t = self.position as f32 / self.output_rate as f32;
            emit(std::array::from_fn(|c| {
                previous[c] + (frame[c] - previous[c]) * t
            }));
            self.position += self.input_rate;
        }
        self.position -= self.output_rate;
    }
}

/// Muting preserves the selected volume for re-enabling.
pub struct MonitorState {
    pub enabled: bool,
    pub volume_percent: u32,
}

impl MonitorState {
    pub fn new() -> Self {
        Self {
            enabled: true,
            volume_percent: 100,
        }
    }
    pub fn toggle(&mut self) {
        self.enabled = !self.enabled;
    }
    pub fn gain(&self) -> f32 {
        if self.enabled {
            self.volume_percent as f32 / 100.0
        } else {
            0.0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn delay_presets_respect_filter_and_device_latency() {
        use std::time::Duration;
        let minimum = Duration::from_millis(30);
        for requested in DELAY_PRESETS {
            let delay = playback_delay(requested, minimum);
            assert!(delay >= minimum);
            assert!(delay + FILTER_DELAY >= Duration::from_millis(requested.into()));
        }
        assert_eq!(playback_delay(0, minimum), minimum);
        assert_eq!(playback_delay(25, minimum), minimum);
        #[cfg(not(feature = "noise-cancellation"))]
        assert_eq!(playback_delay(100, minimum), Duration::from_millis(100));
        #[cfg(feature = "noise-cancellation")]
        assert_eq!(
            playback_delay(100, minimum),
            Duration::from_nanos(73_312_500)
        );
        assert_eq!(
            playback_delay(100, minimum) + FILTER_DELAY,
            Duration::from_millis(100)
        );
        assert_eq!(
            playback_delay(200, minimum) + FILTER_DELAY,
            Duration::from_millis(200)
        );
    }

    #[test]
    fn changing_delay_waits_for_longer_target_and_skips_old_audio_for_shorter_target() {
        use std::time::Duration;
        let period = Duration::from_millis(1);
        let mut samples = (0..250).map(|ms| TimedFrame {
            at: Duration::from_millis(ms),
            value: [ms as f32; 2],
        });
        let (mut previous, mut pending) = (None, None);
        assert_eq!(
            take_due(
                &mut previous,
                &mut pending,
                Duration::from_millis(50),
                period,
                || samples.next()
            )
            .0,
            Some([50.0; 2])
        );
        // Increasing the delay moves the capture-time cursor backward. Clear
        // the played sample but retain the pending one until its new due time.
        previous = None;
        assert_eq!(
            take_due(
                &mut previous,
                &mut pending,
                Duration::from_millis(20),
                period,
                || samples.next()
            )
            .0,
            None
        );
        assert_eq!(
            take_due(
                &mut previous,
                &mut pending,
                Duration::from_millis(51),
                period,
                || samples.next()
            )
            .0,
            Some([51.0; 2])
        );
        previous = None;
        assert_eq!(
            take_due(
                &mut previous,
                &mut pending,
                Duration::from_millis(200),
                period,
                || samples.next()
            )
            .0,
            Some([200.0; 2])
        );
    }

    #[test]
    fn sample_clock_smooths_jitter_but_recovers_after_a_stall() {
        use std::time::Duration;
        let mut clock = SampleClock::default();
        assert_eq!(clock.block(Duration::ZERO, 480, 48_000), Duration::ZERO);
        assert_eq!(
            clock.block(Duration::from_millis(11), 480, 48_000),
            Duration::from_micros(10_010)
        );
        assert_eq!(
            clock.block(Duration::from_secs(1), 480, 48_000),
            Duration::from_secs(1)
        );
    }

    #[test]
    fn scheduled_audio_preserves_timing_and_interpolates_clock_drift() {
        use std::time::Duration;
        let period = Duration::from_micros(20);
        let due = Duration::from_millis(100) - FILTER_DELAY;
        let mut samples = vec![
            TimedFrame {
                at: due,
                value: [1.0; 2],
            },
            TimedFrame {
                at: due + period,
                value: [2.0; 2],
            },
            TimedFrame {
                at: due + period * 2,
                value: [3.0; 2],
            },
        ]
        .into_iter();
        let (mut previous, mut pending) = (None, None);
        assert_eq!(
            take_due(&mut previous, &mut pending, due - period, period, || {
                samples.next()
            }),
            (None, 0)
        );
        assert_eq!(
            take_due(&mut previous, &mut pending, due, period, || samples.next()),
            (Some([1.0; 2]), 0)
        );
        assert_eq!(
            take_due(
                &mut previous,
                &mut pending,
                due + period / 2,
                period,
                || samples.next()
            ),
            (Some([1.5; 2]), 0)
        );
        // A stalled helper cannot create a growing delayed backlog.
        assert_eq!(
            take_due(
                &mut previous,
                &mut pending,
                due + Duration::from_millis(10),
                period,
                || samples.next()
            ),
            (None, 2)
        );
        assert_eq!(due + FILTER_DELAY, Duration::from_millis(100));
    }

    #[test]
    fn resampling_preserves_duration_and_stereo() {
        for (input_rate, output_rate) in [(44_100, 48_000), (48_000, 44_100), (48_000, 48_000)] {
            let mut resampler = Resampler::new(input_rate, output_rate);
            let mut count = 0;
            for i in 0..=input_rate {
                resampler.push([i as f32, -(i as f32)], |frame| {
                    assert!(
                        (frame[0] - count as f32 * input_rate as f32 / output_rate as f32).abs()
                            < 0.01
                    );
                    assert_eq!(frame[0], -frame[1]);
                    count += 1;
                });
            }
            assert_eq!(count, output_rate + 1);
        }
    }

    #[test]
    fn volume_is_preserved_across_toggles_and_changes_while_muted() {
        let mut state = MonitorState::new();
        assert!(state.enabled);
        assert_eq!(state.gain(), 1.0);
        state.toggle();
        assert!(!state.enabled);
        assert_eq!(state.gain(), 0.0);
        state.toggle();
        assert!(state.enabled);
        assert_eq!(state.gain(), 1.0);
        for percent in VOLUME_PRESETS {
            state.toggle();
            state.volume_percent = percent;
            assert_eq!(state.gain(), 0.0);
            state.toggle();
            assert_eq!(state.gain(), percent as f32 / 100.0);
        }
    }
}
