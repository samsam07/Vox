//! Receive thread: UDP recv → parse → Opus decode → upmix → jitter buffer. Owns
//! the one Opus decoder (DESIGN §2). The jitter-buffer consumer lives in the
//! platform's play callback (a [`crate::PlaybackSource`]); this is the producer end.
//!
//! Jitter buffer (DESIGN §3): sequence numbers drive gap handling — an in-order
//! frame is decoded and enqueued; a gap fills the missing frames with silence
//! (FEC/PLC is M7); a late or duplicate frame is dropped; a large discontinuity
//! (e.g. a restarted peer) resyncs.

use std::io::ErrorKind;
use std::net::UdpSocket;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};

use anyhow::{bail, Context, Result};
use ringbuf::traits::{Observer, Producer};
use ringbuf::HeapProd;

use crate::audio::{FRAME, MAX_PACKET};
use crate::packet;

/// Frames of gap beyond which we assume a discontinuity (peer restart) and resync
/// instead of filling silence — ~1 s at 20 ms/frame.
const MAX_GAP_FRAMES: u32 = 50;

#[derive(Default)]
pub(crate) struct Stats {
    pub(crate) received: AtomicU64,
    pub(crate) bytes: AtomicU64,
    pub(crate) gap_frames: AtomicU64,
    pub(crate) dropped_late: AtomicU64,
    /// Current jitter-buffer occupancy in samples (instantaneous, not cumulative).
    pub(crate) jitter_fill: AtomicU64,
}

pub(crate) struct ReceiveThread {
    thread: JoinHandle<Result<()>>,
    stop: Arc<AtomicBool>,
    pub(crate) stats: Arc<Stats>,
    /// Jitter-buffer capacity in samples (for reporting fill as a fraction).
    pub(crate) capacity: usize,
}

impl ReceiveThread {
    pub(crate) fn stop_and_join(self) -> Result<()> {
        self.stop.store(true, Ordering::Release);
        match self.thread.join() {
            Ok(result) => result,
            Err(_) => bail!("receive thread panicked"),
        }
    }
}

pub(crate) fn spawn(
    producer: HeapProd<f32>,
    socket: Arc<UdpSocket>,
    channels: usize,
    capacity: usize,
) -> Result<ReceiveThread> {
    let decoder = opus::Decoder::new(crate::audio::RATE, opus::Channels::Mono)
        .context("create opus decoder")?;
    let stop = Arc::new(AtomicBool::new(false));
    let stats = Arc::new(Stats::default());
    let thread = {
        let stop = Arc::clone(&stop);
        let stats = Arc::clone(&stats);
        thread::spawn(move || recv_loop(producer, socket, decoder, channels, stop, stats))
    };
    Ok(ReceiveThread {
        thread,
        stop,
        stats,
        capacity,
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
        stats.bytes.fetch_add(n as u64, Ordering::Relaxed);

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

        push_frame(
            &mut decoder,
            pkt.payload,
            &mut decoded,
            channels,
            &mut out,
            &mut producer,
        )?;
        expected = Some(pkt.seq.wrapping_add(1));
        stats
            .jitter_fill
            .store(producer.occupied_len() as u64, Ordering::Relaxed);
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
