//! Receive thread: UDP recv → parse → Opus decode → upmix → jitter buffer. Owns
//! the one Opus decoder (DESIGN §2). The jitter-buffer consumer lives in the
//! platform's play callback (a [`crate::PlaybackSource`]); this is the producer end.
//!
//! Jitter buffer (DESIGN §3, §4): sequence numbers drive gap handling — an in-order
//! frame is decoded and enqueued; a gap reconstructs the missing frames (Opus
//! in-band FEC recovers the last lost frame from the redundant copy carried in the
//! just-arrived packet; earlier ones use Opus PLC); a late or duplicate frame is
//! dropped; a large discontinuity (e.g. a restarted peer) resyncs.

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
    /// Cumulative jitter-buffer overruns: 20 ms frame pushes truncated because the
    /// ring was full (decoded audio dropped → glitches).
    pub(crate) overruns: AtomicU64,
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
    decoder: opus::Decoder,
    channels: usize,
    stop: Arc<AtomicBool>,
    stats: Arc<Stats>,
) -> Result<()> {
    let mut buf = vec![0u8; packet::HEADER_LEN + MAX_PACKET];
    let mut receiver = Receiver::new(decoder, channels);

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

        receiver.accept(pkt.seq, pkt.payload, &mut producer, &stats)?;

        stats
            .jitter_fill
            .store(producer.occupied_len() as u64, Ordering::Relaxed);
    }
    Ok(())
}

/// The receive-side decode state machine (DESIGN §3, §4): owns the one Opus decoder
/// and the expected next sequence number, turning sequence-numbered packets into a
/// continuous mono→upmixed PCM stream and concealing loss with FEC + PLC. Decoupled
/// from the socket and the ring so it is unit-testable.
struct Receiver {
    decoder: opus::Decoder,
    channels: usize,
    /// Next sequence number we expect in order; `None` until the first packet.
    expected: Option<u32>,
    /// Mono decode scratch (one 20 ms frame).
    decoded: Vec<f32>,
    /// Interleaved upmix scratch (one frame × `channels`).
    out: Vec<f32>,
}

impl Receiver {
    fn new(decoder: opus::Decoder, channels: usize) -> Self {
        Receiver {
            decoder,
            channels,
            expected: None,
            decoded: vec![0.0f32; FRAME],
            out: Vec::with_capacity(FRAME * channels),
        }
    }

    /// Handle one parsed, in-sequence-numbered packet: drop it if late/duplicate,
    /// else conceal any gap (FEC for the last lost frame, PLC for earlier ones) and
    /// decode this frame, pushing every produced frame into the jitter buffer.
    fn accept(
        &mut self,
        seq: u32,
        payload: &[u8],
        producer: &mut HeapProd<f32>,
        stats: &Stats,
    ) -> Result<()> {
        if let Some(exp) = self.expected {
            let gap = seq.wrapping_sub(exp);
            if gap >= u32::MAX / 2 {
                // seq is behind what we expect: a late or duplicate frame — drop.
                stats.dropped_late.fetch_add(1, Ordering::Relaxed);
                return Ok(());
            }
            if gap > 0 && gap <= MAX_GAP_FRAMES {
                // Conceal the missing frames. Opus in-band FEC only carries the
                // single immediately-preceding frame, so PLC the earlier ones and
                // FEC-reconstruct the last from THIS packet's redundant copy (which
                // falls back to PLC internally if it carries none) — DESIGN §4.
                for _ in 0..gap - 1 {
                    self.decode_plc(producer, stats)?;
                }
                self.decode_fec(payload, producer, stats)?;
                stats.gap_frames.fetch_add(gap as u64, Ordering::Relaxed);
            }
            // gap > MAX_GAP_FRAMES: discontinuity — resync (decode this frame, no fill).
        }

        self.decode_frame(payload, producer, stats)?;
        self.expected = Some(seq.wrapping_add(1));
        Ok(())
    }

    /// Decode the in-order packet normally.
    fn decode_frame(
        &mut self,
        payload: &[u8],
        producer: &mut HeapProd<f32>,
        stats: &Stats,
    ) -> Result<()> {
        let samples = self
            .decoder
            .decode_float(payload, &mut self.decoded, false)
            .context("opus decode")?;
        self.push(samples, producer, stats);
        Ok(())
    }

    /// Reconstruct the lost frame immediately before `payload` from its in-band FEC.
    fn decode_fec(
        &mut self,
        payload: &[u8],
        producer: &mut HeapProd<f32>,
        stats: &Stats,
    ) -> Result<()> {
        let samples = self
            .decoder
            .decode_float(payload, &mut self.decoded, true)
            .context("opus fec decode")?;
        self.push(samples, producer, stats);
        Ok(())
    }

    /// Conceal one lost frame with Opus PLC (decode with an empty packet).
    fn decode_plc(&mut self, producer: &mut HeapProd<f32>, stats: &Stats) -> Result<()> {
        let samples = self
            .decoder
            .decode_float(&[], &mut self.decoded, false)
            .context("opus plc decode")?;
        self.push(samples, producer, stats);
        Ok(())
    }

    /// Upmix the freshly decoded mono frame to `channels` and enqueue it; a full
    /// jitter buffer drops the excess (overrun → glitch, DESIGN §3).
    fn push(&mut self, samples: usize, producer: &mut HeapProd<f32>, stats: &Stats) {
        self.out.clear();
        for &sample in &self.decoded[..samples] {
            for _ in 0..self.channels {
                self.out.push(sample);
            }
        }
        let pushed = producer.push_slice(&self.out);
        if pushed < self.out.len() {
            stats.overruns.fetch_add(1, Ordering::Relaxed);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{Receiver, Stats, MAX_GAP_FRAMES};
    use crate::audio::{FRAME, MAX_PACKET, RATE};
    use std::collections::HashSet;
    use std::sync::atomic::Ordering;

    use ringbuf::traits::{Consumer, Observer, Split};
    use ringbuf::HeapRb;

    /// `n` mono frames of a 440 Hz tone (something with structure for FEC/PLC to
    /// reconstruct, not silence).
    fn sine_frames(n: usize) -> Vec<Vec<f32>> {
        let step = 2.0 * std::f32::consts::PI * 440.0 / RATE as f32;
        let mut phase = 0.0f32;
        (0..n)
            .map(|_| {
                (0..FRAME)
                    .map(|_| {
                        let s = phase.sin() * 0.5;
                        phase += step;
                        s
                    })
                    .collect()
            })
            .collect()
    }

    /// Encode each mono frame to an Opus packet payload, FEC on/off as the sender.
    fn encode(fec: bool, frames: &[Vec<f32>]) -> Vec<Vec<u8>> {
        let mut enc =
            opus::Encoder::new(RATE, opus::Channels::Mono, opus::Application::Voip).unwrap();
        enc.set_bitrate(opus::Bitrate::Bits(24_000)).unwrap();
        enc.set_inband_fec(fec).unwrap();
        enc.set_packet_loss_perc(if fec { 10 } else { 0 }).unwrap();
        let mut buf = vec![0u8; MAX_PACKET];
        frames
            .iter()
            .map(|f| {
                enc.encode_float(f, &mut buf)
                    .map(|n| buf[..n].to_vec())
                    .unwrap()
            })
            .collect()
    }

    /// Feed sequence-numbered packets to a fresh `Receiver` and return the decoded
    /// interleaved PCM plus the resulting stats. The ring is oversized so nothing is
    /// dropped to overrun (we are testing reconstruction, not the buffer's overrun).
    fn run(channels: usize, packets: &[(u32, &[u8])]) -> (Vec<f32>, Stats) {
        let capacity = (packets.len() + MAX_GAP_FRAMES as usize + 8) * FRAME * channels;
        let (mut producer, mut consumer) = HeapRb::<f32>::new(capacity).split();
        let decoder = opus::Decoder::new(RATE, opus::Channels::Mono).unwrap();
        let mut receiver = Receiver::new(decoder, channels);
        let stats = Stats::default();
        for &(seq, payload) in packets {
            receiver
                .accept(seq, payload, &mut producer, &stats)
                .unwrap();
        }
        let mut out = vec![0.0f32; consumer.occupied_len()];
        consumer.pop_slice(&mut out);
        (out, stats)
    }

    fn rms(frame: &[f32]) -> f32 {
        (frame.iter().map(|s| s * s).sum::<f32>() / frame.len() as f32).sqrt()
    }

    /// Interior single-frame drops are each concealed by exactly one frame, so the
    /// stream stays continuous (one decoded frame per source index) and finite.
    #[test]
    fn reconstruction_keeps_stream_continuous() {
        let frames = sine_frames(40);
        let dropped: HashSet<usize> = [5, 12, 20, 31].into_iter().collect();
        for channels in [1usize, 2] {
            let payloads = encode(true, &frames);
            let packets: Vec<(u32, &[u8])> = (0..frames.len())
                .filter(|i| !dropped.contains(i))
                .map(|i| (i as u32, payloads[i].as_slice()))
                .collect();

            let (out, stats) = run(channels, &packets);

            assert_eq!(out.len(), frames.len() * FRAME * channels);
            assert_eq!(
                stats.gap_frames.load(Ordering::Relaxed),
                dropped.len() as u64
            );
            assert_eq!(stats.overruns.load(Ordering::Relaxed), 0);
            assert!(out.iter().all(|s| s.is_finite()));
        }
    }

    /// A concealed (FEC/PLC) frame carries reconstructed audio, not a silent hole —
    /// the M7 "graceful, not glitchy" property (DESIGN §4).
    #[test]
    fn concealed_frame_is_audio_not_silence() {
        let frames = sine_frames(20);
        let payloads = encode(true, &frames);
        let drop = 9usize;
        let packets: Vec<(u32, &[u8])> = (0..frames.len())
            .filter(|i| *i != drop)
            .map(|i| (i as u32, payloads[i].as_slice()))
            .collect();

        let (out, _stats) = run(1, &packets);

        // The reconstructed frame sits at the dropped index (concealment is pushed
        // before the packet that triggered it).
        let concealed = &out[drop * FRAME..(drop + 1) * FRAME];
        assert!(
            rms(concealed) > 0.05,
            "concealed frame rms {} is ~silent",
            rms(concealed)
        );
    }

    /// A late or duplicate sequence number is dropped, emitting nothing.
    #[test]
    fn late_or_duplicate_is_dropped() {
        let frames = sine_frames(3);
        let p = encode(false, &frames);
        let packets = [
            (0u32, p[0].as_slice()),
            (1, p[1].as_slice()),
            (2, p[2].as_slice()),
            (1, p[1].as_slice()), // late/duplicate
        ];

        let (out, stats) = run(1, &packets);

        assert_eq!(stats.dropped_late.load(Ordering::Relaxed), 1);
        assert_eq!(stats.gap_frames.load(Ordering::Relaxed), 0);
        assert_eq!(out.len(), 3 * FRAME); // only the three distinct frames decoded
    }

    /// A jump larger than MAX_GAP_FRAMES is a discontinuity (peer restart): resync
    /// onto the new frame without concealing the (meaningless) gap.
    #[test]
    fn large_gap_resyncs_without_fill() {
        let frames = sine_frames(2);
        let p = encode(false, &frames);
        let packets = [
            (0u32, p[0].as_slice()),
            (MAX_GAP_FRAMES + 5, p[1].as_slice()),
        ];

        let (out, stats) = run(1, &packets);

        assert_eq!(stats.gap_frames.load(Ordering::Relaxed), 0);
        assert_eq!(out.len(), 2 * FRAME); // both real frames, no fill
    }
}
