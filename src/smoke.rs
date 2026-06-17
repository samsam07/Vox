//! M1 dual-stream smoke test (DESIGN §2): open a capture stream and a playback
//! stream on two SEPARATE devices at once and confirm both run cleanly for a
//! fixed window. This is throwaway M1 scaffolding — the real capture/playback
//! engine arrives at M2+. The data callbacks here are deliberately minimal to
//! honour the sacred-callback invariant (no alloc/I/O/lock/codec inside them).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{
    BufferSize, Device, SampleFormat, SampleRate, SizedSample, Stream, StreamConfig,
};

/// DESIGN §4: MVP is 48 kHz, mono. We request exactly this from cpal.
const TARGET_RATE: u32 = 48_000;
const TARGET_CHANNELS: u16 = 1;

/// How an opened stream ended up configured, for the smoke report.
struct Opened {
    stream: Stream,
    config: StreamConfig,
    format: SampleFormat,
    /// True if 48 kHz mono was rejected and we fell back to the device default —
    /// the `[VERIFY]` signal that this device would need resampling (Phase 2).
    fell_back: bool,
    samples: Arc<AtomicU64>,
    errors: Arc<AtomicU64>,
}

/// Run the dual-stream smoke test for `secs` seconds. Returns `Ok(true)` if both
/// streams ran cleanly (no callback errors, throughput within tolerance).
pub fn run(capture: &Device, playback: &Device, secs: u64) -> Result<bool> {
    // Invariant (DESIGN §2): capture and playback are always separate devices.
    if let (Ok(a), Ok(b)) = (capture.name(), playback.name()) {
        if a == b {
            bail!("capture and playback resolved to the same device ({a:?}); they must be separate (DESIGN §2)");
        }
    }

    let cap = open_capture(capture).context("open capture stream")?;
    let play = open_playback(playback).context("open playback stream")?;

    cap.stream.play().context("start capture stream")?;
    play.stream.play().context("start playback stream")?;

    println!(
        "running dual-stream smoke for {secs}s (capture {:?} + playback {:?})...",
        capture.name().unwrap_or_default(),
        playback.name().unwrap_or_default()
    );
    std::thread::sleep(Duration::from_secs(secs));

    // Pause (don't move out of the structs) so we can still read final counts;
    // the streams stop for good when `cap`/`play` drop at function end.
    let _ = cap.stream.pause();
    let _ = play.stream.pause();

    println!("results:");
    let cap_ok = report("capture", capture, &cap, secs);
    let play_ok = report("playback", playback, &play, secs);
    Ok(cap_ok && play_ok)
}

fn report(label: &str, device: &Device, opened: &Opened, secs: u64) -> bool {
    let name = device.name().unwrap_or_else(|_| "<unknown>".to_string());
    let channels = opened.config.channels.max(1) as u64;
    let rate = opened.config.sample_rate.0 as u64;
    let frames = opened.samples.load(Ordering::Relaxed) / channels;
    let errors = opened.errors.load(Ordering::Relaxed);
    let expected = rate * secs;
    let ratio = if expected > 0 {
        frames as f64 / expected as f64
    } else {
        0.0
    };
    let fallback_note = if opened.fell_back {
        let mut reasons = Vec::new();
        if opened.config.sample_rate.0 != TARGET_RATE {
            reasons.push("rate != 48 kHz (resampling, Phase 2)");
        }
        if opened.config.channels != TARGET_CHANNELS {
            reasons.push("not mono (channel down/up-mix needed)");
        }
        format!("  (fallback: {})", reasons.join("; "))
    } else {
        String::new()
    };

    println!("  {label}: {name}");
    println!(
        "    config: {} Hz, {} ch, {:?}{fallback_note}",
        opened.config.sample_rate.0, opened.config.channels, opened.format
    );
    println!(
        "    frames: {frames} (~{:.1}% of expected {expected}), errors: {errors}",
        ratio * 100.0
    );

    let ok = errors == 0 && (0.9..=1.1).contains(&ratio);
    println!("    -> {}", if ok { "OK" } else { "FAIL" });
    ok
}

fn open_capture(device: &Device) -> Result<Opened> {
    let default = device
        .default_input_config()
        .context("query default input config")?;
    let samples = Arc::new(AtomicU64::new(0));
    let errors = Arc::new(AtomicU64::new(0));

    let desired = target_config();
    match build_capture(device, &desired, default.sample_format(), &samples, &errors) {
        Ok(stream) => Ok(Opened {
            stream,
            config: desired,
            format: default.sample_format(),
            fell_back: false,
            samples,
            errors,
        }),
        Err(err) => {
            eprintln!("  capture: 48 kHz mono rejected ({err}); falling back to device default");
            let fallback = default.config();
            let stream = build_capture(device, &fallback, default.sample_format(), &samples, &errors)
                .context("build capture stream (fallback)")?;
            Ok(Opened {
                stream,
                config: fallback,
                format: default.sample_format(),
                fell_back: true,
                samples,
                errors,
            })
        }
    }
}

fn open_playback(device: &Device) -> Result<Opened> {
    let default = device
        .default_output_config()
        .context("query default output config")?;
    let samples = Arc::new(AtomicU64::new(0));
    let errors = Arc::new(AtomicU64::new(0));

    let desired = target_config();
    match build_playback(device, &desired, default.sample_format(), &samples, &errors) {
        Ok(stream) => Ok(Opened {
            stream,
            config: desired,
            format: default.sample_format(),
            fell_back: false,
            samples,
            errors,
        }),
        Err(err) => {
            eprintln!("  playback: 48 kHz mono rejected ({err}); falling back to device default");
            let fallback = default.config();
            let stream =
                build_playback(device, &fallback, default.sample_format(), &samples, &errors)
                    .context("build playback stream (fallback)")?;
            Ok(Opened {
                stream,
                config: fallback,
                format: default.sample_format(),
                fell_back: true,
                samples,
                errors,
            })
        }
    }
}

fn target_config() -> StreamConfig {
    StreamConfig {
        channels: TARGET_CHANNELS,
        sample_rate: SampleRate(TARGET_RATE),
        buffer_size: BufferSize::Default,
    }
}

fn build_capture(
    device: &Device,
    config: &StreamConfig,
    format: SampleFormat,
    samples: &Arc<AtomicU64>,
    errors: &Arc<AtomicU64>,
) -> Result<Stream> {
    match format {
        SampleFormat::F32 => capture_stream::<f32>(device, config, samples, errors),
        SampleFormat::I16 => capture_stream::<i16>(device, config, samples, errors),
        SampleFormat::U16 => capture_stream::<u16>(device, config, samples, errors),
        other => bail!("unsupported capture sample format: {other:?}"),
    }
}

fn build_playback(
    device: &Device,
    config: &StreamConfig,
    format: SampleFormat,
    samples: &Arc<AtomicU64>,
    errors: &Arc<AtomicU64>,
) -> Result<Stream> {
    match format {
        SampleFormat::F32 => playback_stream::<f32>(device, config, samples, errors),
        SampleFormat::I16 => playback_stream::<i16>(device, config, samples, errors),
        SampleFormat::U16 => playback_stream::<u16>(device, config, samples, errors),
        other => bail!("unsupported playback sample format: {other:?}"),
    }
}

fn capture_stream<T: SizedSample + Send + 'static>(
    device: &Device,
    config: &StreamConfig,
    samples: &Arc<AtomicU64>,
    errors: &Arc<AtomicU64>,
) -> Result<Stream> {
    let samples = Arc::clone(samples);
    let errors = Arc::clone(errors);
    let stream = device.build_input_stream::<T, _, _>(
        config,
        // SACRED: count only. No alloc/I/O/lock/codec.
        move |data: &[T], _| {
            samples.fetch_add(data.len() as u64, Ordering::Relaxed);
        },
        move |err| {
            errors.fetch_add(1, Ordering::Relaxed);
            eprintln!("capture stream error: {err}");
        },
        None,
    )?;
    Ok(stream)
}

fn playback_stream<T: SizedSample + Send + 'static>(
    device: &Device,
    config: &StreamConfig,
    samples: &Arc<AtomicU64>,
    errors: &Arc<AtomicU64>,
) -> Result<Stream> {
    let samples = Arc::clone(samples);
    let errors = Arc::clone(errors);
    let stream = device.build_output_stream::<T, _, _>(
        config,
        // SACRED: write silence + count only. No alloc/I/O/lock/codec.
        move |data: &mut [T], _| {
            data.fill(T::EQUILIBRIUM);
            samples.fetch_add(data.len() as u64, Ordering::Relaxed);
        },
        move |err| {
            errors.fetch_add(1, Ordering::Relaxed);
            eprintln!("playback stream error: {err}");
        },
        None,
    )?;
    Ok(stream)
}
