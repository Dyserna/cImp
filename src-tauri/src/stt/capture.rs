//! Microphone capture via a `cpal` input stream + `rubato` resampling.
//!
//! `cpal::Stream` is `!Send` (the underlying audio handle is bound to its
//! creating thread), so — exactly like the audio *output* path in
//! `audio/playback.rs` — capture runs on a dedicated OS thread that owns the
//! stream and processes [`CaptureCmd`]s off a `std::sync::mpsc` channel. The
//! stream's data callback (which fires on cpal's own thread) downmixes to
//! mono, appends to an accumulator, and tees a copy into the mic amplitude
//! ring for the recording waveform.
//!
//! On `Stop` the thread tears the stream down, resamples the accumulated
//! native-rate mono buffer to Whisper's 16 kHz, and forwards it to the
//! transcription worker. Batch-on-stop (not streaming) keeps this simple and
//! is fine for non-streaming dictation.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex, RwLock};

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{Sample, SampleFormat};
use tauri::AppHandle;
use tracing::{debug, info, warn};

use crate::audio::amplitude::RingBuffer;
use crate::error::{AppError, AppResult};
use crate::settings::SettingsHandle;
use crate::stt::engine::WHISPER_SAMPLE_RATE;
use crate::stt::{set_state, CaptureCmd, SttState, WorkerMsg};

/// Live capture session state, owned entirely by the capture thread.
struct ActiveCapture {
    /// Held to keep the stream alive; dropped to stop capture. Never moved
    /// off this thread (it is `!Send`).
    _stream: cpal::Stream,
    accumulator: Arc<Mutex<Vec<f32>>>,
    sample_rate: u32,
}

/// Spawn the dedicated capture thread. `jobs_tx` carries finished 16 kHz mono
/// recordings to the transcription worker.
pub(crate) fn spawn_capture_thread(
    app: AppHandle,
    settings: SettingsHandle,
    cmd_rx: Receiver<CaptureCmd>,
    jobs_tx: Sender<WorkerMsg>,
    recording: Arc<AtomicBool>,
    state: Arc<RwLock<SttState>>,
    mic: Arc<RwLock<RingBuffer>>,
) {
    std::thread::Builder::new()
        .name("cimp-stt-capture".into())
        .spawn(move || {
            let mut active: Option<ActiveCapture> = None;
            while let Ok(cmd) = cmd_rx.recv() {
                match cmd {
                    CaptureCmd::Start => {
                        if active.is_some() {
                            debug!(target: "stt", "start ignored: already recording");
                            continue;
                        }
                        match start_capture(&settings, mic.clone(), &app, &state, &recording) {
                            Ok(cap) => {
                                active = Some(cap);
                                recording.store(true, Ordering::SeqCst);
                                set_state(&app, &state, SttState::Recording);
                            }
                            Err(e) => {
                                warn!(target: "stt", error = %e, "failed to start capture");
                                recording.store(false, Ordering::SeqCst);
                                set_state(&app, &state, SttState::Error);
                            }
                        }
                    }
                    CaptureCmd::Stop => {
                        let Some(cap) = active.take() else {
                            debug!(target: "stt", "stop ignored: not recording");
                            continue;
                        };
                        recording.store(false, Ordering::SeqCst);
                        let (samples, rate) = finish(cap);
                        // Clear the mic amplitude ring now that the stream is
                        // torn down, so the previous recording's trailing ~1s
                        // doesn't bleed into the next recording's waveform
                        // (mirrors the playback stop path in audio/amplitude).
                        if let Ok(mut ring) = mic.write() {
                            ring.clear();
                        }
                        // Hand off to the worker, which emits the transcript
                        // (and the idle/error state) once inference completes.
                        set_state(&app, &state, SttState::Transcribing);
                        match resample_to_16k(samples, rate) {
                            Ok(ready) => {
                                let (peak, rms) = peak_rms(&ready);
                                info!(target: "stt", frames_16k = ready.len(), peak, rms, "resampled for whisper");
                                if jobs_tx.send(WorkerMsg::Transcribe(ready)).is_err() {
                                    warn!(target: "stt", "transcription worker gone; dropping recording");
                                    set_state(&app, &state, SttState::Idle);
                                }
                            }
                            Err(e) => {
                                warn!(target: "stt", error = %e, "resample failed");
                                set_state(&app, &state, SttState::Error);
                            }
                        }
                    }
                    CaptureCmd::Cancel => {
                        if active.take().is_some() {
                            recording.store(false, Ordering::SeqCst);
                            // Drop the stale tail so the next recording starts
                            // from a clean waveform (see Stop).
                            if let Ok(mut ring) = mic.write() {
                                ring.clear();
                            }
                            info!(target: "stt", "recording cancelled");
                        }
                        set_state(&app, &state, SttState::Idle);
                    }
                }
            }
            debug!(target: "stt", "capture thread: command channel closed; exiting");
        })
        .expect("spawn stt capture thread");
}

/// Open the chosen (or default) input device and start an f32-accumulating
/// input stream.
fn start_capture(
    settings: &SettingsHandle,
    mic: Arc<RwLock<RingBuffer>>,
    app: &AppHandle,
    state: &Arc<RwLock<SttState>>,
    recording: &Arc<AtomicBool>,
) -> AppResult<ActiveCapture> {
    let host = cpal::default_host();
    let wanted = settings.current().stt.input_device;
    let device = resolve_input_device(&host, &wanted)?;

    let supported = device
        .default_input_config()
        .map_err(|e| AppError::Stt(format!("default input config: {e}")))?;
    let sample_format = supported.sample_format();
    let sample_rate = supported.sample_rate().0;
    let channels = supported.channels() as usize;
    // `data.chunks(channels)` in the callback panics on a zero chunk size, and
    // a 0-channel config is meaningless for capture — reject it up front.
    if channels == 0 {
        return Err(AppError::Stt("input device reports zero channels".into()));
    }
    let config: cpal::StreamConfig = supported.into();

    info!(
        target: "stt",
        device = device.name().unwrap_or_else(|_| "<unknown>".into()),
        sample_rate,
        channels,
        format = ?sample_format,
        "capture started"
    );

    let accumulator = Arc::new(Mutex::new(Vec::<f32>::new()));
    // A mid-capture stream error (e.g. the device is unplugged) fires on cpal's
    // thread. Surface it as SttState::Error and clear `recording` — otherwise
    // Stop later yields a silent empty transcript and the user thinks they
    // recorded successfully.
    let err_app = app.clone();
    let err_state = state.clone();
    let err_recording = recording.clone();
    let err_fn = move |e| {
        warn!(target: "stt", error = %e, "input stream error");
        err_recording.store(false, Ordering::SeqCst);
        set_state(&err_app, &err_state, SttState::Error);
    };

    let stream = match sample_format {
        SampleFormat::F32 => {
            build_stream::<f32>(&device, &config, channels, accumulator.clone(), mic, err_fn)
        }
        SampleFormat::I16 => {
            build_stream::<i16>(&device, &config, channels, accumulator.clone(), mic, err_fn)
        }
        SampleFormat::U16 => {
            build_stream::<u16>(&device, &config, channels, accumulator.clone(), mic, err_fn)
        }
        other => {
            return Err(AppError::Stt(format!(
                "unsupported input sample format: {other:?}"
            )))
        }
    }?;

    stream
        .play()
        .map_err(|e| AppError::Stt(format!("stream play: {e}")))?;

    Ok(ActiveCapture {
        _stream: stream,
        accumulator,
        sample_rate,
    })
}

/// Resolve `wanted` (empty = system default) to a concrete input device,
/// falling back to the default device if the named one is gone.
fn resolve_input_device(host: &cpal::Host, wanted: &str) -> AppResult<cpal::Device> {
    if !wanted.is_empty() {
        if let Ok(devices) = host.input_devices() {
            for d in devices {
                if d.name().map(|n| n == wanted).unwrap_or(false) {
                    return Ok(d);
                }
            }
        }
        warn!(target: "stt", device = %wanted, "input device not found; using system default");
    }
    host.default_input_device()
        .ok_or_else(|| AppError::Stt("no input device available".into()))
}

/// Build a typed input stream that downmixes to mono f32, appending to the
/// accumulator and teeing into the mic amplitude ring.
fn build_stream<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    channels: usize,
    accumulator: Arc<Mutex<Vec<f32>>>,
    mic: Arc<RwLock<RingBuffer>>,
    err_fn: impl FnMut(cpal::StreamError) + Send + 'static,
) -> AppResult<cpal::Stream>
where
    T: cpal::SizedSample,
    f32: cpal::FromSample<T>,
{
    // Reusable downmix scratch so the realtime callback doesn't allocate per
    // call after warm-up.
    let mut scratch: Vec<f32> = Vec::new();
    device
        .build_input_stream(
            config,
            move |data: &[T], _: &cpal::InputCallbackInfo| {
                // Downmix to mono FIRST, holding no lock. Then take each lock
                // briefly and separately. The original held the `accumulator`
                // mutex AND the `mic` write lock across the whole frame loop;
                // because `recent_samples()` reads `mic` every ~16ms, that let a
                // reader stall the audio driver callback (xruns / dropped input)
                // and nested two locks unnecessarily.
                let inv = 1.0 / channels.max(1) as f32;
                scratch.clear();
                scratch.reserve(data.len() / channels.max(1) + 1);
                for frame in data.chunks(channels) {
                    let mut sum = 0.0f32;
                    for &s in frame {
                        sum += f32::from_sample(s);
                    }
                    // Clamp non-finite samples to silence at the source so a
                    // misbehaving device can't poison Whisper input, the RMS
                    // meter, or the amplitude IPC (NaN isn't valid JSON).
                    let mono = sum * inv;
                    scratch.push(if mono.is_finite() { mono } else { 0.0 });
                }
                if let Ok(mut buf) = accumulator.lock() {
                    // Hard cap (~10 min at 48 kHz) so a session left recording
                    // can't grow the accumulator unbounded (~11 MB/min). Past
                    // the cap we drop further audio rather than risk OOM; a very
                    // long dictation is truncated at the ceiling.
                    const MAX_ACCUM_SAMPLES: usize = 28_800_000;
                    let remaining = MAX_ACCUM_SAMPLES.saturating_sub(buf.len());
                    if remaining > 0 {
                        let take = scratch.len().min(remaining);
                        buf.extend_from_slice(&scratch[..take]);
                    }
                }
                if let Ok(mut ring) = mic.write() {
                    for &m in &scratch {
                        ring.push(m);
                    }
                }
            },
            err_fn,
            None,
        )
        .map_err(|e| AppError::Stt(format!("build input stream: {e}")))
}

/// Stop the stream and take the accumulated mono buffer + its native rate.
fn finish(cap: ActiveCapture) -> (Vec<f32>, u32) {
    let rate = cap.sample_rate;
    // Pause first, then drop: on WASAPI a bare drop isn't guaranteed to tear
    // the callback down synchronously, so an in-flight callback could still be
    // appending when we take the buffer below. Pausing quiesces the device
    // before we read, so trailing frames aren't lost to that race.
    use cpal::traits::StreamTrait;
    let _ = cap._stream.pause();
    drop(cap._stream);
    let samples = cap
        .accumulator
        .lock()
        .map(|mut b| std::mem::take(&mut *b))
        .unwrap_or_default();
    let (peak, rms) = peak_rms(&samples);
    // INFO so it lands in the default-level log: a near-zero peak/rms means the
    // device delivered silence (wrong device, muted, or denied mic permission)
    // rather than a capture/resample bug downstream.
    info!(
        target: "stt",
        frames = samples.len(),
        rate,
        secs = samples.len() as f32 / rate.max(1) as f32,
        peak,
        rms,
        "capture finished"
    );
    (samples, rate)
}

/// Peak (max |sample|) and RMS of a buffer, for diagnosing silent captures.
fn peak_rms(samples: &[f32]) -> (f32, f32) {
    if samples.is_empty() {
        return (0.0, 0.0);
    }
    let mut peak = 0.0f32;
    let mut sumsq = 0.0f64;
    for &s in samples {
        peak = peak.max(s.abs());
        sumsq += (s as f64) * (s as f64);
    }
    (peak, (sumsq / samples.len() as f64).sqrt() as f32)
}

/// Resample mono f32 from `in_rate` to 16 kHz using rubato's FFT resampler.
/// Short-circuits when already at 16 kHz or empty.
fn resample_to_16k(input: Vec<f32>, in_rate: u32) -> AppResult<Vec<f32>> {
    if input.is_empty() || in_rate == WHISPER_SAMPLE_RATE {
        return Ok(input);
    }

    use rubato::{FftFixedIn, Resampler};
    const CHUNK: usize = 1024;
    let mut resampler =
        FftFixedIn::<f32>::new(in_rate as usize, WHISPER_SAMPLE_RATE as usize, CHUNK, 2, 1)
            .map_err(|e| AppError::Stt(format!("build resampler: {e}")))?;

    // Expected output length, used to trim resampler tail padding so the
    // result is exactly proportional to the input duration.
    let expected = (input.len() as u64 * WHISPER_SAMPLE_RATE as u64 / in_rate as u64) as usize;

    let mut out: Vec<f32> = Vec::with_capacity(expected + CHUNK);
    let mut pos = 0usize;
    loop {
        let need = resampler.input_frames_next();
        if pos + need > input.len() {
            break;
        }
        let produced = resampler
            .process(&[&input[pos..pos + need]], None)
            .map_err(|e| AppError::Stt(format!("resample: {e}")))?;
        out.extend_from_slice(&produced[0]);
        pos += need;
    }
    // Feed the remainder (zero-padded internally). This is also the ONLY
    // path taken for a sub-chunk recording (`input.len() < CHUNK`), where the
    // main loop breaks immediately with `pos == 0` — e.g. a brief press-to-talk
    // on a 44.1/48 kHz mic produces well under 1024 native samples.
    if pos < input.len() {
        let produced = resampler
            .process_partial(Some(&[&input[pos..]]), None)
            .map_err(|e| AppError::Stt(format!("resample tail: {e}")))?;
        out.extend_from_slice(&produced[0]);
    }

    // Drain the resampler's internal delay line. `FftFixedIn` buffers up to
    // one chunk of latency, so for a short clip (and for the tail of any clip)
    // some output is still held inside after the input is consumed. A single
    // `process_partial` doesn't recover it; flush with empty calls until we've
    // produced the expected frame count or the resampler stops emitting.
    // Without this, short dictations were silently truncated or dropped (the
    // worker's ~150 ms minimum-sample guard would then reject a real clip).
    while out.len() < expected {
        let produced = resampler
            .process_partial::<&[f32]>(None, None)
            .map_err(|e| AppError::Stt(format!("resample flush: {e}")))?;
        if produced[0].is_empty() {
            break;
        }
        out.extend_from_slice(&produced[0]);
    }

    out.truncate(expected.min(out.len()));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 48 kHz sine resampled to 16 kHz should shrink by ~3× in length and
    /// preserve its dominant frequency (checked via zero-crossing count).
    #[test]
    fn resample_preserves_length_and_tone() {
        let in_rate = 48_000u32;
        let freq = 440.0f32;
        let secs = 1.0f32;
        let n = (in_rate as f32 * secs) as usize;
        let input: Vec<f32> = (0..n)
            .map(|i| (2.0 * std::f32::consts::PI * freq * i as f32 / in_rate as f32).sin())
            .collect();

        let out = resample_to_16k(input, in_rate).expect("resample ok");
        let expected = n * 16_000 / 48_000;
        // Length: within one FFT output block of proportional. The FFT
        // resampler trims a partial trailing block, so allow a small margin.
        let tol = expected / 50; // within 2%
        assert!(
            (out.len() as i64 - expected as i64).unsigned_abs() as usize <= tol,
            "len {} not within {} of {}",
            out.len(),
            tol,
            expected
        );

        // Energy: a unit-amplitude sine has RMS ≈ 0.707; resampling preserves
        // it (no large gain change or silence).
        let rms = (out.iter().map(|s| s * s).sum::<f32>() / out.len() as f32).sqrt();
        assert!((rms - 0.707).abs() < 0.07, "RMS {rms} not ~0.707");

        // Dominant frequency: count oscillation cycles with hysteresis so FFT
        // ripple near zero doesn't inflate the count. One cycle = a swing above
        // +0.5 after dipping below -0.5. A clean 440 Hz tone over 1 s ≈ 440.
        let mut cycles = 0usize;
        let mut armed = true;
        for &s in &out {
            if armed && s > 0.5 {
                cycles += 1;
                armed = false;
            } else if s < -0.5 {
                armed = true;
            }
        }
        let expected_cycles = (freq * secs) as usize;
        assert!(
            (cycles as i64 - expected_cycles as i64).unsigned_abs() < 10,
            "dominant frequency not preserved: {cycles} cycles vs ~{expected_cycles}"
        );
    }

    #[test]
    fn resample_noop_at_16k() {
        let input = vec![0.1, -0.2, 0.3];
        let out = resample_to_16k(input.clone(), 16_000).unwrap();
        assert_eq!(out, input);
    }

    #[test]
    fn resample_short_subchunk_input_is_not_dropped() {
        // Fewer than one FFT chunk (1024) of native samples — a very short
        // press-to-talk on a 48 kHz mic. Regression: this used to fall solely
        // to a single `process_partial` that returned far fewer than the
        // expected frames, mangling/dropping the clip. The drain loop must now
        // recover ~proportional output.
        let in_rate = 48_000u32;
        let n = 480usize; // 10 ms at 48 kHz, < CHUNK
        let input: Vec<f32> = (0..n)
            .map(|i| (2.0 * std::f32::consts::PI * 440.0 * i as f32 / in_rate as f32).sin())
            .collect();
        let expected = n * 16_000 / 48_000; // 160
        let out = resample_to_16k(input, in_rate).expect("resample ok");
        // Should be close to the proportional length, not near-zero. Allow a
        // generous lower bound (resampler latency can shave a little).
        assert!(
            out.len() >= expected * 3 / 4,
            "short clip resampled to {} frames, expected ~{}",
            out.len(),
            expected
        );
    }
}
