//! M3 codec loopback: capture callback → capture ring → codec thread (downmix →
//! Opus encode → Opus decode → upmix) → playback ring → playback callback. One
//! machine, no network. Inserting the codec into M2's loopback isolates codec bugs
//! from network bugs and proves the 48 kHz / 20 ms / mono / bitrate config.
//!
//! The codec thread stands in for the future send + receive threads (M4/M5),
//! looping encoded bytes in-process instead of over UDP. It owns the one Opus
//! encoder and the one decoder; neither is ever shared across threads. The sacred
//! callbacks still do only a non-blocking ring push/pop (DESIGN §2, §3, §4).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{BufferSize, Device, SampleRate, StreamConfig};
use ringbuf::traits::{Consumer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};

use crate::device::Role;

const RATE: u32 = 48_000; // DESIGN §4: 48 kHz only.
const FRAME: usize = 960; // 20 ms @ 48 kHz, mono (DESIGN §4).
const MAX_PACKET: usize = 4000; // generous upper bound for one 20 ms Opus packet.

pub struct Params {
    /// Each ring's capacity in milliseconds of audio (also the latency ceiling).
    pub ring_ms: u32,
    /// How long to run before stopping and reporting.
    pub secs: u64,
    /// Opus target bitrate in bits/s.
    pub bitrate: i32,
}

/// Run the codec loopback for `params.secs` seconds.
pub fn run(capture: &Device, playback: &Device, params: Params) -> Result<()> {
    // Invariant (DESIGN §2): capture and playback are always separate devices.
    if let (Ok(a), Ok(b)) = (capture.name(), playback.name()) {
        if a == b {
            bail!("capture and playback are the same device ({a:?}); they must be separate (DESIGN §2)");
        }
    }

    // The codec normalises to mono in the middle, so the two devices need not share
    // a channel count — each opens at mono if it can, else stereo.
    let cap_ch = pick_channels(capture, Role::Capture)?;
    let play_ch = pick_channels(playback, Role::Playback)?;

    // Two SPSC rings: capture cb -> codec thread, codec thread -> playback cb.
    let cap_ring = HeapRb::<f32>::new(ring_capacity(params.ring_ms, cap_ch));
    let (mut cap_prod, cap_cons) = cap_ring.split();
    let play_ring = HeapRb::<f32>::new(ring_capacity(params.ring_ms, play_ch));
    let (mut play_prod, mut play_cons) = play_ring.split();
    // Prefill the playback ring so the playback cb has a cushion while the codec
    // thread warms up instead of starving immediately.
    let cushion = vec![0.0f32; play_ring_cushion(params.ring_ms, play_ch)];
    play_prod.push_slice(&cushion);

    let captured = Arc::new(AtomicU64::new(0));
    let cap_dropped = Arc::new(AtomicU64::new(0)); // capture-ring overrun
    let played = Arc::new(AtomicU64::new(0));
    let play_starved = Arc::new(AtomicU64::new(0)); // playback-ring underrun
    let frames_coded = Arc::new(AtomicU64::new(0));
    let stop = Arc::new(AtomicBool::new(false));

    // One encoder + one decoder, owned by the codec thread (built here so failures
    // surface before we spawn). FEC and DTX stay off until M7 (happy path first).
    let mut encoder = opus::Encoder::new(RATE, opus::Channels::Mono, opus::Application::Voip)
        .context("create opus encoder")?;
    encoder
        .set_bitrate(opus::Bitrate::Bits(params.bitrate))
        .context("set opus bitrate")?;
    let decoder = opus::Decoder::new(RATE, opus::Channels::Mono).context("create opus decoder")?;

    let cap_stream = {
        let captured = Arc::clone(&captured);
        let cap_dropped = Arc::clone(&cap_dropped);
        capture
            .build_input_stream::<f32, _, _>(
                &stream_config(cap_ch),
                // SACRED: non-blocking ring push + atomic counts only.
                move |data: &[f32], _| {
                    let pushed = cap_prod.push_slice(data);
                    captured.fetch_add(data.len() as u64, Ordering::Relaxed);
                    cap_dropped.fetch_add((data.len() - pushed) as u64, Ordering::Relaxed);
                },
                move |err| eprintln!("capture stream error: {err}"),
                None,
            )
            .context("build capture stream")?
    };

    let play_stream = {
        let played = Arc::clone(&played);
        let play_starved = Arc::clone(&play_starved);
        playback
            .build_output_stream::<f32, _, _>(
                &stream_config(play_ch),
                // SACRED: non-blocking ring pop + silence on underrun + counts only.
                move |data: &mut [f32], _| {
                    let popped = play_cons.pop_slice(data);
                    if popped < data.len() {
                        data[popped..].fill(0.0);
                    }
                    played.fetch_add(popped as u64, Ordering::Relaxed);
                    play_starved.fetch_add((data.len() - popped) as u64, Ordering::Relaxed);
                },
                move |err| eprintln!("playback stream error: {err}"),
                None,
            )
            .context("build playback stream")?
    };

    let codec = {
        let stop = Arc::clone(&stop);
        let frames_coded = Arc::clone(&frames_coded);
        thread::spawn(move || -> Result<()> {
            codec_loop(CodecLoop {
                cap_cons,
                play_prod,
                encoder,
                decoder,
                cap_ch,
                play_ch,
                stop,
                frames_coded,
            })
        })
    };

    cap_stream.play().context("start capture stream")?;
    play_stream.play().context("start playback stream")?;

    println!(
        "codec loopback for {}s @ {} bps: {:?} ({} ch) -> opus mono -> {:?} ({} ch)",
        params.secs,
        params.bitrate,
        capture.name().unwrap_or_default(),
        cap_ch,
        playback.name().unwrap_or_default(),
        play_ch,
    );
    println!("speak into the capture device — you should still hear yourself, through Opus.");
    thread::sleep(Duration::from_secs(params.secs));

    stop.store(true, Ordering::Release);
    let _ = cap_stream.pause();
    let _ = play_stream.pause();
    match codec.join() {
        Ok(result) => result.context("codec thread")?,
        Err(_) => bail!("codec thread panicked"),
    }

    println!("results:");
    println!("  frames encoded+decoded: {}", frames_coded.load(Ordering::Relaxed));
    println!(
        "  captured {}, played {} (samples)",
        captured.load(Ordering::Relaxed),
        played.load(Ordering::Relaxed)
    );
    println!(
        "  capture overrun {}, playback underrun {} (samples)",
        cap_dropped.load(Ordering::Relaxed),
        play_starved.load(Ordering::Relaxed)
    );
    Ok(())
}

/// Everything the codec thread owns. Bundled so the worker has a single argument.
struct CodecLoop {
    cap_cons: HeapCons<f32>,
    play_prod: HeapProd<f32>,
    encoder: opus::Encoder,
    decoder: opus::Decoder,
    cap_ch: u16,
    play_ch: u16,
    stop: Arc<AtomicBool>,
    frames_coded: Arc<AtomicU64>,
}

/// Drain the capture ring, downmix to mono, encode and decode in 20 ms frames,
/// upmix to the playback channel count, and feed the playback ring. Runs off the
/// sacred callbacks, so it may allocate (once, up front) and sleep.
fn codec_loop(mut ctx: CodecLoop) -> Result<()> {
    let cap_ch = ctx.cap_ch as usize;
    let play_ch = ctx.play_ch as usize;

    let mut read = vec![0.0f32; 4096];
    let mut interleaved: Vec<f32> = Vec::with_capacity(8192); // < cap_ch leftover
    let mut mono: Vec<f32> = Vec::with_capacity(FRAME * 4); // < FRAME leftover
    let mut packet = vec![0u8; MAX_PACKET];
    let mut decoded = vec![0.0f32; FRAME];
    let mut out: Vec<f32> = Vec::with_capacity(FRAME * play_ch);

    while !ctx.stop.load(Ordering::Acquire) {
        let n = ctx.cap_cons.pop_slice(&mut read);
        if n == 0 {
            thread::sleep(Duration::from_millis(3));
            continue;
        }

        // Downmix each complete interleaved frame to one mono sample.
        interleaved.extend_from_slice(&read[..n]);
        let complete = interleaved.len() / cap_ch;
        for i in 0..complete {
            let frame = &interleaved[i * cap_ch..(i + 1) * cap_ch];
            mono.push(frame.iter().sum::<f32>() / cap_ch as f32);
        }
        interleaved.drain(..complete * cap_ch);

        // Encode + decode whole 20 ms mono frames; upmix and hand to playback.
        while mono.len() >= FRAME {
            let bytes = ctx
                .encoder
                .encode_float(&mono[..FRAME], &mut packet)
                .context("opus encode")?;
            let samples = ctx
                .decoder
                .decode_float(&packet[..bytes], &mut decoded, false)
                .context("opus decode")?;

            out.clear();
            for &sample in &decoded[..samples] {
                for _ in 0..play_ch {
                    out.push(sample);
                }
            }
            ctx.play_prod.push_slice(&out); // playback-ring overrun -> drop remainder
            ctx.frames_coded.fetch_add(1, Ordering::Relaxed);
            mono.drain(..FRAME);
        }
    }
    Ok(())
}

fn stream_config(channels: u16) -> StreamConfig {
    StreamConfig {
        channels,
        sample_rate: SampleRate(RATE),
        buffer_size: BufferSize::Default,
    }
}

fn ring_capacity(ms: u32, channels: u16) -> usize {
    ((RATE as usize * ms as usize / 1000) * channels as usize).max(channels as usize * FRAME)
}

fn play_ring_cushion(ms: u32, channels: u16) -> usize {
    ring_capacity(ms, channels) / 2
}

/// Pick mono if the device can open it at 48 kHz / f32, else stereo (DESIGN §4).
fn pick_channels(device: &Device, role: Role) -> Result<u16> {
    for channels in [1u16, 2] {
        if can_build(device, role, channels) {
            return Ok(channels);
        }
    }
    bail!(
        "{} device offers no 48 kHz f32 mono or stereo config",
        role.label()
    )
}

fn can_build(device: &Device, role: Role, channels: u16) -> bool {
    // Probe by building a throwaway stream — WASAPI under-reports supported configs.
    let config = stream_config(channels);
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
