//! M2 loopback engine: capture callback → SPSC ring → playback callback. Raw PCM,
//! one machine, no network, no codec. Validates the sacred-callback discipline and
//! the lock-free SPSC ring in isolation (DESIGN §2, §3).
//!
//! The ring carries interleaved f32 at a channel count both devices share (mono on
//! real hardware, stereo on VB-Cable). Downmix-to-mono is intentionally absent: per
//! DESIGN §4 it belongs "before encode" on the send thread (M4+), not in a sacred
//! callback, so M2 negotiates a common channel count rather than mixing here.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{BufferSize, Device, SampleRate, StreamConfig};
use ringbuf::traits::{Consumer, Producer, Split};
use ringbuf::HeapRb;

use crate::device::Role;

/// DESIGN §4: MVP is 48 kHz only.
const RATE: u32 = 48_000;

pub struct Params {
    /// Ring capacity expressed in milliseconds of audio; also the latency ceiling.
    pub ring_ms: u32,
    /// How long to run the loopback before stopping and reporting.
    pub secs: u64,
}

/// Run the loopback for `params.secs` seconds.
pub fn run(capture: &Device, playback: &Device, params: Params) -> Result<()> {
    // Invariant (DESIGN §2): capture and playback are always separate devices.
    if let (Ok(a), Ok(b)) = (capture.name(), playback.name()) {
        if a == b {
            bail!("capture and playback are the same device ({a:?}); they must be separate (DESIGN §2)");
        }
    }

    // Negotiate a channel count both devices offer at 48 kHz / f32, preferring mono.
    let channels = negotiate_channels(capture, playback)?;
    let config = StreamConfig {
        channels,
        sample_rate: SampleRate(RATE),
        buffer_size: BufferSize::Default,
    };

    // SPSC ring sized to `ring_ms` of audio, prefilled half-full as a latency
    // cushion so playback starts smoothly instead of starving immediately.
    let capacity = ((RATE as usize * params.ring_ms as usize / 1000) * channels as usize)
        .max(channels as usize * 64);
    let ring = HeapRb::<f32>::new(capacity);
    let (mut producer, mut consumer) = ring.split();
    let cushion = vec![0.0f32; capacity / 2];
    producer.push_slice(&cushion);

    let captured = Arc::new(AtomicU64::new(0));
    let dropped = Arc::new(AtomicU64::new(0)); // overrun: ring full on push
    let played = Arc::new(AtomicU64::new(0));
    let starved = Arc::new(AtomicU64::new(0)); // underrun: ring empty on pop

    let cap_stream = {
        let captured = Arc::clone(&captured);
        let dropped = Arc::clone(&dropped);
        capture
            .build_input_stream::<f32, _, _>(
                &config,
                // SACRED: non-blocking ring push + atomic counts only.
                move |data: &[f32], _| {
                    let pushed = producer.push_slice(data);
                    captured.fetch_add(data.len() as u64, Ordering::Relaxed);
                    dropped.fetch_add((data.len() - pushed) as u64, Ordering::Relaxed);
                },
                move |err| eprintln!("capture stream error: {err}"),
                None,
            )
            .context("build capture stream")?
    };

    let play_stream = {
        let played = Arc::clone(&played);
        let starved = Arc::clone(&starved);
        playback
            .build_output_stream::<f32, _, _>(
                &config,
                // SACRED: non-blocking ring pop + silence on underrun + counts only.
                move |data: &mut [f32], _| {
                    let popped = consumer.pop_slice(data);
                    if popped < data.len() {
                        data[popped..].fill(0.0);
                    }
                    played.fetch_add(popped as u64, Ordering::Relaxed);
                    starved.fetch_add((data.len() - popped) as u64, Ordering::Relaxed);
                },
                move |err| eprintln!("playback stream error: {err}"),
                None,
            )
            .context("build playback stream")?
    };

    cap_stream.play().context("start capture stream")?;
    play_stream.play().context("start playback stream")?;

    println!(
        "loopback for {}s: {:?} -> ring({} ms, {} ch) -> {:?}",
        params.secs,
        capture.name().unwrap_or_default(),
        params.ring_ms,
        channels,
        playback.name().unwrap_or_default(),
    );
    println!("speak into the capture device — you should hear yourself on the playback device.");
    std::thread::sleep(Duration::from_secs(params.secs));

    let _ = cap_stream.pause();
    let _ = play_stream.pause();

    report(
        captured.load(Ordering::Relaxed),
        played.load(Ordering::Relaxed),
        dropped.load(Ordering::Relaxed),
        starved.load(Ordering::Relaxed),
    );
    Ok(())
}

fn report(captured: u64, played: u64, dropped: u64, starved: u64) {
    println!("results (samples):");
    println!("  captured {captured}, played {played}");
    println!("  dropped (overrun) {dropped}, starved (underrun) {starved}");
    // A small startup starve (before the first capture buffers arrive) is expected;
    // sustained drops/starves indicate clock drift outgrowing the ring (DESIGN §3).
}

/// Pick a channel count both devices can actually open at 48 kHz / f32, preferring
/// mono. WASAPI's advertised `supported_*_configs` under-reports configs the device
/// will happily open in shared mode (M1 showed a "mono-only" mic and "stereo-only"
/// speakers both opening mono), so we probe by building a throwaway stream rather
/// than trusting the enumeration.
fn negotiate_channels(capture: &Device, playback: &Device) -> Result<u16> {
    for channels in [1u16, 2] {
        if can_build(capture, Role::Capture, channels)
            && can_build(playback, Role::Playback, channels)
        {
            return Ok(channels);
        }
    }
    bail!("no common 48 kHz f32 channel count (mono or stereo) for capture and playback")
}

fn can_build(device: &Device, role: Role, channels: u16) -> bool {
    let config = StreamConfig {
        channels,
        sample_rate: SampleRate(RATE),
        buffer_size: BufferSize::Default,
    };
    // The probe stream is never played, just built and dropped.
    match role {
        Role::Capture => device
            .build_input_stream::<f32, _, _>(
                &config,
                |_: &[f32], _: &cpal::InputCallbackInfo| {},
                |_| {},
                None,
            )
            .is_ok(),
        Role::Playback => device
            .build_output_stream::<f32, _, _>(
                &config,
                |_: &mut [f32], _: &cpal::OutputCallbackInfo| {},
                |_| {},
                None,
            )
            .is_ok(),
    }
}
