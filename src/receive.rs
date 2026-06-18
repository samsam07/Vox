//! Receive path: receive thread (UDP recv → parse → Opus decode → upmix → jitter
//! buffer) → playback callback. The receive thread solely owns the one Opus
//! decoder (DESIGN §2). The playback callback is sacred: a non-blocking ring pop
//! (silence on underrun) only.
//!
//! Jitter buffer (DESIGN §3): a fixed PCM ring, prefilled as a look-ahead cushion.
//! Sequence numbers drive gap handling — an in-order frame is decoded and enqueued;
//! a gap fills the missing frames with silence (FEC/PLC is M7); a late or duplicate
//! frame is dropped; a large discontinuity (e.g. a restarted peer) resyncs.

use std::io::ErrorKind;
use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use anyhow::{bail, Context, Result};
use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{Device, Stream};
use ringbuf::traits::{Consumer, Producer, Split};
use ringbuf::{HeapProd, HeapRb};

use crate::audio::{self, FRAME, MAX_PACKET, RATE};
use crate::device::Role;
use crate::packet;

/// Frames of gap beyond which we assume a discontinuity (peer restart) and resync
/// instead of filling silence — ~1 s at 20 ms/frame.
const MAX_GAP_FRAMES: u32 = 50;

/// Live receive path. Holds the playback stream (must stay on the main thread) and
/// the receive thread; dropping/stopping it tears both down.
pub struct Receiver {
    stream: Stream,
    thread: Option<JoinHandle<Result<()>>>,
    stop: Arc<AtomicBool>,
    stats: Arc<Stats>,
}

#[derive(Default)]
struct Stats {
    received: AtomicU64,
    gap_frames: AtomicU64,
    dropped_late: AtomicU64,
}

impl Receiver {
    /// Signal the receive thread to stop, join it, and report.
    pub fn stop_and_join(mut self) -> Result<()> {
        self.stop.store(true, Ordering::Release);
        let _ = self.stream.pause();
        let result = match self.thread.take() {
            Some(handle) => match handle.join() {
                Ok(result) => result,
                Err(_) => bail!("receive thread panicked"),
            },
            None => Ok(()),
        };
        println!(
            "  recv: {} packets, {} gap frames (silence), {} late/dup dropped",
            self.stats.received.load(Ordering::Relaxed),
            self.stats.gap_frames.load(Ordering::Relaxed),
            self.stats.dropped_late.load(Ordering::Relaxed),
        );
        result
    }
}

/// Start receiving on `socket`, decoding, and playing to `device`.
pub fn start(socket: Arc<UdpSocket>, device: &Device, jitter_ms: u32) -> Result<Receiver> {
    let channels = audio::pick_channels(device, Role::Playback)?;
    let capacity = audio::ring_capacity(jitter_ms, channels);
    let ring = HeapRb::<f32>::new(capacity);
    let (mut producer, mut consumer) = ring.split();
    // Prefill a look-ahead cushion so playback starts smoothly and short-term
    // jitter is absorbed (this is the buffer FEC will rely on at M7).
    producer.push_slice(&vec![0.0f32; capacity / 2]);

    let decoder = opus::Decoder::new(RATE, opus::Channels::Mono).context("create opus decoder")?;

    let stream = device
        .build_output_stream::<f32, _, _>(
            &audio::stream_config(channels),
            // SACRED: non-blocking ring pop + silence on underrun only.
            move |data: &mut [f32], _| {
                let popped = consumer.pop_slice(data);
                if popped < data.len() {
                    data[popped..].fill(0.0);
                }
            },
            move |err| eprintln!("playback stream error: {err}"),
            None,
        )
        .context("build playback stream")?;

    let stop = Arc::new(AtomicBool::new(false));
    let stats = Arc::new(Stats::default());
    let thread = {
        let stop = Arc::clone(&stop);
        let stats = Arc::clone(&stats);
        thread::spawn(move || recv_loop(producer, socket, decoder, channels as usize, stop, stats))
    };
    stream.play().context("start playback stream")?;

    Ok(Receiver {
        stream,
        thread: Some(thread),
        stop,
        stats,
    })
}

fn recv_loop(
    mut producer: HeapProd<f32>,
    socket: Arc<UdpSocket>,
    mut decoder: opus::Decoder,
    channels: usize,
    stop: Arc<AtomicBool>,
    stats: Arc<Stats>,
) -> Result<()> {
    let mut buf = vec![0u8; packet::HEADER_LEN + MAX_PACKET];
    let mut decoded = vec![0.0f32; FRAME];
    let mut out: Vec<f32> = Vec::with_capacity(FRAME * channels);
    let mut expected: Option<u32> = None;

    while !stop.load(Ordering::Acquire) {
        let n = match socket.recv_from(&mut buf) {
            Ok((n, _from)) => n,
            // Read timeout fired — loop back to re-check the stop flag.
            Err(e) if matches!(e.kind(), ErrorKind::WouldBlock | ErrorKind::TimedOut) => continue,
            Err(e) => return Err(e).context("udp recv"),
        };
        let pkt = match packet::parse(&buf[..n]) {
            Some(pkt) => pkt,
            None => continue, // too short to be ours
        };
        stats.received.fetch_add(1, Ordering::Relaxed);

        if let Some(exp) = expected {
            let gap = pkt.seq.wrapping_sub(exp);
            if gap >= u32::MAX / 2 {
                // seq is behind what we expect: a late or duplicate frame — drop.
                stats.dropped_late.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            if gap > 0 && gap <= MAX_GAP_FRAMES {
                for _ in 0..gap {
                    push_silence(channels, &mut out, &mut producer);
                }
                stats.gap_frames.fetch_add(gap as u64, Ordering::Relaxed);
            }
            // gap > MAX_GAP_FRAMES: discontinuity — resync (decode this frame, no fill).
        }

        push_frame(&mut decoder, pkt.payload, &mut decoded, channels, &mut out, &mut producer)?;
        expected = Some(pkt.seq.wrapping_add(1));
    }
    Ok(())
}

/// Decode one Opus frame to mono, upmix to `channels`, and enqueue it.
fn push_frame(
    decoder: &mut opus::Decoder,
    payload: &[u8],
    decoded: &mut [f32],
    channels: usize,
    out: &mut Vec<f32>,
    producer: &mut HeapProd<f32>,
) -> Result<()> {
    let samples = decoder
        .decode_float(payload, decoded, false)
        .context("opus decode")?;
    out.clear();
    for &sample in &decoded[..samples] {
        for _ in 0..channels {
            out.push(sample);
        }
    }
    producer.push_slice(out); // jitter-buffer overrun -> drop remainder (DESIGN §3)
    Ok(())
}

/// Enqueue one frame of silence (a lost frame, pre-FEC).
fn push_silence(channels: usize, out: &mut Vec<f32>, producer: &mut HeapProd<f32>) {
    out.clear();
    out.resize(FRAME * channels, 0.0);
    producer.push_slice(out);
}
