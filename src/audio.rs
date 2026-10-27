use crate::dsp::{RATE, Resampler, SAMPLES, SampleClock, TimedFrame, playback_delay, take_due};
#[cfg(feature = "noise-cancellation")]
use crate::noise_cancellation::Helper;
use anyhow::{Result, bail};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SampleFormat, SizedSample, Stream, StreamConfig};
use crossbeam_queue::ArrayQueue;
use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering},
};

#[derive(Default)]
pub struct Counters {
    pub captured: AtomicU64,
    pub rendered: AtomicU64,
    pub underruns: AtomicU64,
    pub overruns: AtomicU64,
    pub processed: AtomicU64,
    pub processing_max_us: AtomicU64,
    pub late: AtomicU64,
    pub scheduled: AtomicU64,
}

use std::{
    thread::JoinHandle,
    time::{Duration, Instant},
};

#[derive(Clone, Copy)]
struct Block {
    at: Duration,
    samples: [f32; SAMPLES],
}

struct Worker {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}
impl Drop for Worker {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

#[derive(Clone)]
struct Devices {
    input: cpal::Device,
    output: cpal::Device,
}
impl Devices {
    fn current() -> Option<Self> {
        let host = cpal::default_host();
        Some(Self {
            input: host.default_input_device()?,
            output: host.default_output_device()?,
        })
    }
}
impl PartialEq for Devices {
    fn eq(&self, other: &Self) -> bool {
        // WASAPI Device equality compares IMMDevice endpoint IDs, not names or
        // COM pointer identities. Only WASAPI is enabled in this Windows app.
        fn same(a: &cpal::Device, b: &cpal::Device) -> bool {
            match (a.as_inner(), b.as_inner()) {
                (
                    cpal::platform::DeviceInner::Wasapi(a),
                    cpal::platform::DeviceInner::Wasapi(b),
                ) => a == b,
            }
        }
        same(&self.input, &other.input) && same(&self.output, &other.output)
    }
}

pub enum Event {
    Ready(Arc<Counters>),
    Waiting,
}

/// Own all device/COM objects on one supervisor thread. The tray remains
/// responsive while Windows enumerates endpoints or the helper is starting.
pub struct Controller {
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}
impl Drop for Controller {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            thread.thread().unpark();
            let _ = thread.join();
        }
    }
}
impl Controller {
    pub fn start(
        gain: Arc<AtomicU32>,
        delay_ms: Arc<AtomicU32>,
        on_event: Arc<dyn Fn(Event) + Send + Sync>,
        recovery_test: bool,
    ) -> Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let shutdown = stop.clone();
        let thread = std::thread::Builder::new()
            .name("audio-devices".into())
            .spawn(move || {
                let mut active: Option<(Devices, Audio, std::sync::mpsc::Receiver<String>)> = None;
                let mut next_attempt = Instant::now();
                let mut retry_delay = Duration::from_millis(500);
                let mut progress_at = Instant::now();
                let mut connected_at = Instant::now();
                let mut progress = [0; 3];
                let mut previous_devices = None;
                let mut test_started = None;
                let mut injected_failure = false;
                while !shutdown.load(Ordering::Relaxed) {
                    let now = Instant::now();
                    // Exercise the real lifecycle without altering Windows settings.
                    let test_elapsed = test_started.map(|start: Instant| start.elapsed());
                    let simulated_missing = recovery_test
                        && test_elapsed.is_some_and(|t| (3..5).contains(&t.as_secs()));
                    let devices = if simulated_missing {
                        None
                    } else {
                        Devices::current()
                    };
                    let devices_changed = devices != previous_devices;
                    if devices_changed {
                        next_attempt = now + Duration::from_millis(250);
                        retry_delay = Duration::from_millis(500);
                        previous_devices = devices.clone();
                    }
                    let mut failure = None;
                    if active.is_some()
                        && now.duration_since(connected_at) >= Duration::from_secs(5)
                    {
                        retry_delay = Duration::from_millis(500);
                    }
                    if let Some((selected, audio, errors)) = &active {
                        if devices.as_ref() != Some(selected) {
                            failure =
                                Some("Default audio endpoints changed or disconnected".to_owned());
                        } else if let Ok(error) = errors.try_recv() {
                            failure = Some(error);
                        } else if recovery_test
                            && !injected_failure
                            && test_elapsed.is_some_and(|t| t.as_secs() >= 8)
                        {
                            injected_failure = true;
                            audio._worker.stop.store(true, Ordering::Relaxed);
                            // Let the normal stalled-callback watchdog discover it.
                        }
                        if now.duration_since(progress_at) >= Duration::from_secs(2) {
                            let current = audio.counters.progress();
                            if stalled(progress, current) {
                                failure = Some("Audio callbacks or processing stalled".to_owned());
                            }
                            progress = current;
                            progress_at = now;
                        }
                    }
                    if let Some(reason) = failure {
                        crate::log(&format!("Audio reconnecting: {reason}"));
                        on_event(Event::Waiting);
                        // Drop old streams, helper and all buffered audio before opening new ones.
                        active = None;
                        crate::log("Previous audio pipeline closed");
                        next_attempt = Instant::now()
                            + if devices_changed {
                                Duration::from_millis(250)
                            } else {
                                retry_delay
                            };
                        retry_delay = (retry_delay * 2).min(Duration::from_secs(5));
                    }
                    if active.is_none()
                        && now >= next_attempt
                        && let Some(selected) = devices
                    {
                        let (tx, rx) = std::sync::mpsc::channel();
                        // Each generation gets its own error channel. A late callback
                        // from a retired stream cannot stop its replacement.
                        let on_error = Arc::new(move |message| {
                            let _ = tx.send(message);
                        });
                        match Audio::start(
                            &selected,
                            gain.clone(),
                            delay_ms.clone(),
                            on_error,
                            shutdown.clone(),
                        ) {
                            Ok(audio) => {
                                if shutdown.load(Ordering::Relaxed) {
                                    break;
                                }
                                // Defaults can change during slow initialization. Never
                                // activate the obsolete selection, even briefly.
                                if Devices::current().as_ref() != Some(&selected) {
                                    continue;
                                }
                                audio.ready.store(true, Ordering::Relaxed);
                                progress = audio.counters.progress();
                                progress_at = Instant::now();
                                connected_at = progress_at;
                                test_started.get_or_insert(progress_at);
                                on_event(Event::Ready(audio.counters.clone()));
                                crate::log("Audio connected to current default devices");
                                active = Some((selected, audio, rx));
                            }
                            Err(error) => {
                                if shutdown.load(Ordering::Relaxed) {
                                    break;
                                }
                                crate::log(&format!("Audio reconnect attempt failed: {error:#}"));
                                on_event(Event::Waiting);
                                next_attempt = Instant::now() + retry_delay;
                                retry_delay = (retry_delay * 2).min(Duration::from_secs(5));
                            }
                        }
                    }
                    std::thread::park_timeout(Duration::from_millis(250));
                }
                drop(active);
            })?;
        Ok(Self {
            stop,
            thread: Some(thread),
        })
    }
}

impl Counters {
    fn progress(&self) -> [u64; 3] {
        [
            self.captured.load(Ordering::Relaxed),
            self.rendered.load(Ordering::Relaxed),
            self.processed.load(Ordering::Relaxed),
        ]
    }
}
fn stalled(previous: [u64; 3], current: [u64; 3]) -> bool {
    previous
        .iter()
        .zip(current)
        .any(|(previous, current)| *previous == current)
}

struct Audio {
    _input: Stream,
    _output: Stream,
    _worker: Worker,
    ready: Arc<AtomicBool>,
    counters: Arc<Counters>,
}

impl Audio {
    fn start(
        devices: &Devices,
        gain: Arc<AtomicU32>,
        delay_ms: Arc<AtomicU32>,
        on_error: Arc<dyn Fn(String) + Send + Sync>,
        _shutdown: Arc<AtomicBool>,
    ) -> Result<Self> {
        let input = &devices.input;
        let output = &devices.output;
        let output_config = output.default_output_config()?;
        let output_rate = output_config.sample_rate().0;
        // Use the device's current shared-mode format. Enumerating every
        // supported rate can block some USB drivers for seconds on each reopen;
        // the streaming resampler already handles conversion to the internal 48 kHz format.
        let input_config = input.default_input_config()?;
        crate::log(&format!(
            "Input: {} / {} Hz / {} channels / {:?}; output: {} / {} Hz / {} channels / {:?}; noise cancellation: {}; target delay: {} ms including filter/transport",
            input.name()?,
            input_config.sample_rate().0,
            input_config.channels(),
            input_config.sample_format(),
            output.name()?,
            output_rate,
            output_config.channels(),
            output_config.sample_format(),
            cfg!(feature = "noise-cancellation"),
            delay_ms.load(Ordering::Relaxed)
        ));
        #[cfg(feature = "noise-cancellation")]
        let mut helper = Helper::start(Some(&_shutdown))?;
        let input_queue = Arc::new(ArrayQueue::<Block>::new(8));
        // Cover the configured delay and every tray preset, plus 50 ms of headroom.
        let buffered_ms = u64::from(delay_ms.load(Ordering::Relaxed).max(200)) + 50;
        let buffered_frames = (u64::from(output_rate) * buffered_ms).div_ceil(1000) as usize;
        let queue = Arc::new(ArrayQueue::new(buffered_frames.max(1)));
        let origin = Arc::new(OnceLock::<cpal::StreamInstant>::new());
        let counters = Arc::new(Counters::default());
        let stop = Arc::new(AtomicBool::new(false));
        let ready = Arc::new(AtomicBool::new(false));
        let on_error: Arc<dyn Fn(String) + Send + Sync> = {
            let stop = stop.clone();
            Arc::new(move |error| {
                // Device callbacks must silence the pipeline immediately, even
                // while the supervisor or the tray is busy.
                if !stop.swap(true, Ordering::Relaxed) {
                    on_error(error);
                }
            })
        };
        let worker = {
            let input = input_queue.clone();
            let output = queue.clone();
            let stop_flag = stop.clone();
            let counters = counters.clone();
            let on_error = on_error.clone();
            let thread = std::thread::Builder::new()
                .name("audio-processing".into())
                .spawn(move || {
                    let mut resampler = Resampler::new(RATE, output_rate);
                    while !stop_flag.load(Ordering::Relaxed) {
                        let Some(block) = input.pop() else {
                            std::thread::sleep(Duration::from_millis(1));
                            continue;
                        };
                        let started = Instant::now();
                        #[cfg(feature = "noise-cancellation")]
                        let samples = match helper.process(&block.samples) {
                            Ok(samples) => samples,
                            Err(error) => {
                                on_error(format!("Noise cancellation: {error:#}"));
                                break;
                            }
                        };
                        #[cfg(not(feature = "noise-cancellation"))]
                        let samples = block.samples;
                        counters
                            .processing_max_us
                            .fetch_max(started.elapsed().as_micros() as u64, Ordering::Relaxed);
                        if counters.processed.fetch_add(1, Ordering::Relaxed) == 0 {
                            crate::log("Live audio processing started");
                        }
                        let mut emitted = 0;
                        let mut overflow = false;
                        for sample in samples {
                            resampler.push([sample; 2], |value| {
                                let at = block.at
                                    + Duration::from_secs_f64(emitted as f64 / output_rate as f64);
                                emitted += 1;
                                if output.push(TimedFrame { at, value }).is_err() {
                                    overflow = true;
                                }
                            });
                        }
                        if overflow {
                            on_error("Audio playback queue overflow".into());
                            break;
                        }
                    }
                })?;
            Worker {
                stop: stop.clone(),
                thread: Some(thread),
            }
        };
        macro_rules! input_stream {
            ($t:ty) => {
                build_input::<$t>(
                    input,
                    &input_config.config(),
                    input_queue.clone(),
                    origin.clone(),
                    stop.clone(),
                    counters.clone(),
                    on_error.clone(),
                )?
            };
        }
        let input_stream = match input_config.sample_format() {
            SampleFormat::F32 => input_stream!(f32),
            SampleFormat::F64 => input_stream!(f64),
            SampleFormat::I8 => input_stream!(i8),
            SampleFormat::I16 => input_stream!(i16),
            SampleFormat::I32 => input_stream!(i32),
            SampleFormat::I64 => input_stream!(i64),
            SampleFormat::U8 => input_stream!(u8),
            SampleFormat::U16 => input_stream!(u16),
            SampleFormat::U32 => input_stream!(u32),
            SampleFormat::U64 => input_stream!(u64),
            format => bail!("Unsupported microphone sample format: {format:?}"),
        };
        macro_rules! output_stream {
            ($t:ty) => {
                build_output::<$t>(
                    output,
                    &output_config.config(),
                    queue.clone(),
                    origin.clone(),
                    stop.clone(),
                    gain.clone(),
                    delay_ms.clone(),
                    ready.clone(),
                    counters.clone(),
                    on_error.clone(),
                )?
            };
        }
        let output_stream = match output_config.sample_format() {
            SampleFormat::F32 => output_stream!(f32),
            SampleFormat::F64 => output_stream!(f64),
            SampleFormat::I8 => output_stream!(i8),
            SampleFormat::I16 => output_stream!(i16),
            SampleFormat::I32 => output_stream!(i32),
            SampleFormat::I64 => output_stream!(i64),
            SampleFormat::U8 => output_stream!(u8),
            SampleFormat::U16 => output_stream!(u16),
            SampleFormat::U32 => output_stream!(u32),
            SampleFormat::U64 => output_stream!(u64),
            format => bail!("Unsupported output sample format: {format:?}"),
        };
        input_stream.play()?;
        output_stream.play()?;
        Ok(Self {
            _input: input_stream,
            _output: output_stream,
            _worker: worker,
            ready,
            counters,
        })
    }
}

fn build_input<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    queue: Arc<ArrayQueue<Block>>,
    origin: Arc<OnceLock<cpal::StreamInstant>>,
    stop: Arc<AtomicBool>,
    counters: Arc<Counters>,
    on_error: Arc<dyn Fn(String) + Send + Sync>,
) -> Result<Stream>
where
    T: SizedSample,
    f32: FromSample<T>,
{
    let channels = config.channels as usize;
    let config_rate = config.sample_rate.0;
    let mut resampler = Resampler::new(config_rate, RATE);
    let mut block = Block {
        at: Duration::ZERO,
        samples: [0.0; SAMPLES],
    };
    let mut filled = 0;
    let mut clock = SampleClock::default();
    let callback_error = on_error.clone();
    Ok(device.build_input_stream(
        config,
        move |data: &[T], info| {
            if stop.load(Ordering::Relaxed) {
                return;
            }
            let capture = info.timestamp().capture;
            let base = capture
                .duration_since(origin.get_or_init(|| capture))
                .unwrap_or_default();
            let base = clock.block(base, data.len() / channels, config_rate);
            let mut emitted = 0;
            let mut overflow = false;
            for frame in data.chunks_exact(channels) {
                let mono =
                    frame.iter().map(|s| f32::from_sample(*s)).sum::<f32>() / channels as f32;
                resampler.push([mono; 2], |sample| {
                    if filled == 0 {
                        block.at = base + Duration::from_secs_f64(emitted as f64 / RATE as f64);
                    }
                    emitted += 1;
                    block.samples[filled] = sample[0];
                    filled += 1;
                    if filled == SAMPLES {
                        if queue.push(block).is_err() {
                            overflow = true;
                        }
                        filled = 0;
                    }
                });
            }
            counters
                .captured
                .fetch_add((data.len() / channels) as u64, Ordering::Relaxed);
            if overflow {
                counters.overruns.fetch_add(1, Ordering::Relaxed);
                callback_error("Audio capture queue overflow".into());
            }
        },
        move |error| on_error(format!("Microphone: {error}")),
        None,
    )?)
}

#[allow(clippy::too_many_arguments)]
fn build_output<T>(
    device: &cpal::Device,
    config: &StreamConfig,
    queue: Arc<ArrayQueue<TimedFrame>>,
    origin: Arc<OnceLock<cpal::StreamInstant>>,
    stop: Arc<AtomicBool>,
    gain: Arc<AtomicU32>,
    delay_ms: Arc<AtomicU32>,
    ready: Arc<AtomicBool>,
    counters: Arc<Counters>,
    on_error: Arc<dyn Fn(String) + Send + Sync>,
) -> Result<Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = config.channels as usize;
    let rate = config.sample_rate.0;
    let period = Duration::from_secs_f64(1.0 / rate as f64);
    let mut pending = None;
    let mut previous = None;
    let mut minimum_delay = None;
    let mut selected_delay = delay_ms.load(Ordering::Relaxed);
    let mut clock = SampleClock::default();
    let mut envelope = 0.0f32;
    let ramp_step = 1.0 / (rate as f32 * 0.020);
    Ok(device.build_output_stream(
        config,
        move |data: &mut [T], info| {
            let requested = delay_ms.load(Ordering::Relaxed);
            if requested != selected_delay {
                selected_delay = requested;
                // Do not repeat a previously played sample while filling a longer delay.
                previous = None;
                envelope = 0.0;
            }
            let stopped = stop.load(Ordering::Relaxed);
            let volume = f32::from_bits(gain.load(Ordering::Relaxed));
            let audible = ready.load(Ordering::Relaxed) && volume != 0.0 && !stopped;
            let base = origin
                .get()
                .and_then(|origin| info.timestamp().playback.duration_since(origin))
                .map(|time| clock.block(time, data.len() / channels, rate));
            for (i, frame) in data.chunks_exact_mut(channels).enumerate() {
                let mut sample_available = false;
                let value = if let Some(base) = base {
                    let now = base + Duration::from_secs_f64(i as f64 / rate as f64);
                    if pending.is_none() {
                        pending = queue.pop();
                    }
                    if minimum_delay.is_none()
                        && let Some(first) = pending
                    {
                        // One extra audio block absorbs normal callback/worker jitter.
                        minimum_delay =
                            Some(now.saturating_sub(first.at) + Duration::from_millis(10));
                    }
                    let delay = playback_delay(selected_delay, minimum_delay.unwrap_or_default());
                    let (value, late) = match now.checked_sub(delay) {
                        Some(due) => {
                            take_due(&mut previous, &mut pending, due, period, || queue.pop())
                        }
                        None => (None, 0),
                    };
                    counters.late.fetch_add(late, Ordering::Relaxed);
                    if pending.is_none() && queue.is_empty() {
                        counters.underruns.fetch_add(1, Ordering::Relaxed);
                    }
                    if value.is_some() {
                        sample_available = true;
                        counters.scheduled.fetch_add(1, Ordering::Relaxed);
                    }
                    value.unwrap_or([0.0; 2])
                } else {
                    [0.0; 2]
                };
                if stopped || !sample_available {
                    envelope = 0.0;
                }
                envelope = if audible && sample_available {
                    (envelope + ramp_step).min(1.0)
                } else {
                    (envelope - ramp_step).max(0.0)
                };
                for (channel, sample) in frame.iter_mut().enumerate() {
                    let value = if channel < 2 { value[0] } else { 0.0 };
                    *sample = T::from_sample((value * volume * envelope).clamp(-1.0, 1.0));
                }
            }
            counters
                .rendered
                .fetch_add((data.len() / channels) as u64, Ordering::Relaxed);
        },
        move |error| on_error(format!("Output: {error}")),
        None,
    )?)
}

#[cfg(test)]
mod tests {
    use super::stalled;
    #[test]
    fn watchdog_requires_capture_playback_and_filter_progress() {
        assert!(!stalled([10, 20, 30], [11, 21, 31]));
        assert!(stalled([10, 20, 30], [10, 21, 31]));
        assert!(stalled([10, 20, 30], [11, 20, 31]));
        assert!(stalled([10, 20, 30], [11, 21, 30]));
    }
}
