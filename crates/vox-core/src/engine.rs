//! The engine: owns the socket, the send/receive threads, and the rings. Returns
//! the [`AudioPorts`] ring seam for a platform audio layer to drive (DESIGN §11).

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc};

use anyhow::{bail, Result};
use ringbuf::traits::{Consumer, Observer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};

use crate::audio::{ring_capacity, CAPTURE_RING_MS};
use crate::control::{EngineControl, Tunable};
use crate::playout::{fade, relaxed, Playout};
use crate::{net, receive, send};

/// How to start an engine. `peer` is required to capture/send; `bind` is required
/// to receive (and is the port the peer targets). Channel counts are the device's
/// (the engine downmixes to mono on the wire and upmixes back).
pub struct EngineConfig {
    pub peer: Option<SocketAddr>,
    /// Local UDP port. `None` → an ephemeral source port (send-only, no listener).
    pub bind: Option<u16>,
    pub capture_channels: Option<u16>,
    pub playback_channels: Option<u16>,
    /// Capture device sample rate (Hz); the send path resamples it up to 48 kHz.
    /// Unused when capture is disabled.
    pub capture_sample_rate: u32,
    /// Playback device sample rate (Hz); the receive path resamples 48 kHz down/up
    /// to it. Unused when playback is disabled.
    pub playback_sample_rate: u32,
    pub jitter_ms: u32,
    pub bitrate: i32,
    /// In-band FEC on the send path (DESIGN §4, §7).
    pub fec: bool,
    /// Expected packet-loss %, tuning FEC (only applied when `fec` is on).
    pub expected_loss: u8,
    /// Discontinuous transmission / silence suppression on the send path.
    pub dtx: bool,
    /// Smooth clock-drift compensation on the receive path (M10b). Off by default;
    /// engages a dynamic resampler that holds the buffer at its target.
    pub drift_correct: bool,
}

/// Non-blocking sink: the platform's record callback pushes device PCM into it.
/// This is the capture ring's producer end (one writer — DESIGN §3).
pub struct CaptureSink {
    producer: HeapProd<f32>,
    /// Callbacks whose audio didn't fit (send thread fell behind) — lost mic audio.
    overruns: Arc<AtomicU64>,
}

impl CaptureSink {
    /// Push captured interleaved PCM. Non-blocking; a full ring drops the excess
    /// (overrun, counted). Safe to call from a real-time audio callback.
    pub fn push(&mut self, data: &[f32]) {
        if self.producer.push_slice(data) < data.len() {
            self.overruns.fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Non-blocking source: the platform's play callback pulls PCM to play from it.
/// This is the jitter buffer's consumer end (one reader — DESIGN §3). On underrun
/// it fades out and rebuffers to the adaptive target before resuming (M10c).
pub struct PlaybackSource {
    consumer: HeapCons<f32>,
    channels: usize,
    playout: Arc<Playout>,
    /// Callback-local mirrors of the shared fields this callback writes.
    starved: bool,
    period: usize,
}

impl PlaybackSource {
    /// Fill `data` with interleaved PCM to play. While rebuffering, or on underrun,
    /// the remainder is silence (with a short fade at each edge). Non-blocking; only
    /// ring ops and relaxed atomics. Safe to call from a real-time audio callback.
    pub fn fill(&mut self, data: &mut [f32]) {
        if data.len() > self.period {
            self.period = data.len();
            self.playout.period.store(self.period, Ordering::Relaxed);
        }
        if self.starved {
            // Resume only once the ring holds the target AND this whole request, so a
            // resume never underruns inside the same callback.
            let resume_at = relaxed(&self.playout.resume_at).max(data.len());
            if self.consumer.occupied_len() < resume_at {
                data.fill(0.0);
                return;
            }
        }
        let popped = self.consumer.pop_slice(data);
        if self.starved {
            fade(&mut data[..popped], self.channels, true);
            self.set_starved(false);
        }
        if popped < data.len() {
            fade(&mut data[..popped], self.channels, false);
            data[popped..].fill(0.0);
            self.set_starved(true);
            self.playout.underruns.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn set_starved(&mut self, starved: bool) {
        self.starved = starved;
        self.playout.starved.store(starved, Ordering::Relaxed);
    }
}

/// The ring seam handed to the platform audio layer. A role is `None` when that
/// direction is disabled.
pub struct AudioPorts {
    pub capture: Option<CaptureSink>,
    pub playback: Option<PlaybackSource>,
}

/// A running engine. Hold it for the session; call [`Engine::stop`] to tear down.
pub struct Engine {
    socket: Arc<UdpSocket>,
    sender: Option<send::SendThread>,
    receiver: Option<receive::ReceiveThread>,
    /// Live-settings channel senders, one per running thread (DESIGN §12).
    control: EngineControl,
}

/// Cumulative engine counters. Read live via [`Engine::stats`] or final via
/// [`Engine::stop`].
#[derive(Default, Clone, Copy)]
pub struct EngineStats {
    pub packets_sent: u64,
    pub bytes_sent: u64,
    pub packets_received: u64,
    pub bytes_received: u64,
    pub gap_frames: u64,
    pub dropped_late: u64,
    /// Cumulative jitter-buffer overruns (frames dropped because the ring was full).
    pub overruns: u64,
    /// Cumulative recentering corrections against clock drift (DESIGN §3): frames
    /// dropped when the buffer ran high, frames repeated when it ran low.
    pub recenter_drops: u64,
    pub recenter_inserts: u64,
    /// Current jitter-buffer occupancy and capacity, in samples.
    pub jitter_fill: u64,
    pub jitter_capacity: u64,
    /// Live buffered latency (occupancy) in ms.
    pub jitter_fill_ms: u64,
    /// Cumulative playback underruns: each is one audible break, after which the
    /// buffer rebuffers to the target (M10c).
    pub underruns: u64,
    /// Cumulative capture-ring overruns: callbacks whose mic audio was dropped because
    /// the send thread fell behind.
    pub capture_overruns: u64,
    /// Current adaptive target buffer depth, in ms (M10).
    pub target_depth_ms: u64,
}

impl Engine {
    /// Start the engine. Returns it plus the [`AudioPorts`] the platform feeds.
    pub fn start(config: EngineConfig) -> Result<(Engine, AudioPorts)> {
        if config.capture_channels.is_none() && config.playback_channels.is_none() {
            bail!("engine has neither a capture nor a playback role");
        }
        if config.playback_channels.is_some() && config.bind.is_none() {
            bail!("a playback (receive) role requires a bind port for the peer to target");
        }
        // `None` bind → port 0, i.e. an OS-assigned ephemeral source port.
        let socket = net::bind(config.bind.unwrap_or(0))?;

        let (sender, capture) = match (config.capture_channels, config.peer) {
            (Some(channels), Some(peer)) => {
                let ring = HeapRb::<f32>::new(ring_capacity(
                    config.capture_sample_rate,
                    CAPTURE_RING_MS,
                    channels,
                ));
                let (producer, consumer) = ring.split();
                let overruns = Arc::new(AtomicU64::new(0));
                let (control_tx, controls) = mpsc::channel();
                let thread = send::spawn(
                    consumer,
                    Arc::clone(&socket),
                    peer,
                    channels as usize,
                    config.capture_sample_rate,
                    Tunable {
                        start: send::EncoderParams {
                            bitrate: config.bitrate,
                            fec: config.fec,
                            expected_loss: config.expected_loss,
                            dtx: config.dtx,
                        },
                        controls,
                    },
                    Arc::clone(&overruns),
                )?;
                (
                    Some((thread, control_tx)),
                    Some(CaptureSink { producer, overruns }),
                )
            }
            (Some(_), None) => bail!("a capture role requires a peer to send to"),
            (None, _) => (None, None),
        };

        let (receiver, playback) = match config.playback_channels {
            Some(channels) => {
                let capacity =
                    ring_capacity(config.playback_sample_rate, config.jitter_ms, channels);
                let ring = HeapRb::<f32>::new(capacity);
                let (producer, consumer) = ring.split();
                // Playback starts starved: it stays silent until the ring fills to the
                // adaptive initial depth (the zero-jitter band centre), so it starts
                // smoothly and FEC gets its look-ahead (§4). The receive thread then
                // moves that resume point with the band (M10, M10c).
                let initial = crate::jitter::initial_depth(
                    config.playback_sample_rate,
                    channels as usize,
                    capacity,
                );
                let playout = Arc::new(Playout::new(initial));
                let (control_tx, controls) = mpsc::channel();
                let thread = receive::spawn(
                    producer,
                    Arc::clone(&socket),
                    channels as usize,
                    capacity,
                    config.playback_sample_rate,
                    Tunable {
                        start: config.drift_correct,
                        controls,
                    },
                    Arc::clone(&playout),
                )?;
                let source = PlaybackSource {
                    consumer,
                    channels: channels as usize,
                    playout,
                    starved: true,
                    period: 0,
                };
                (Some((thread, control_tx)), Some(source))
            }
            None => (None, None),
        };

        let (sender, send_control) = sender.unzip();
        let (receiver, receive_control) = receiver.unzip();
        Ok((
            Engine {
                socket,
                sender,
                receiver,
                control: EngineControl {
                    send: send_control,
                    receive: receive_control,
                },
            },
            AudioPorts { capture, playback },
        ))
    }

    /// The bound local address (for diagnostics).
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// A handle for changing settings live (DESIGN §12). Changes last for this
    /// session only.
    pub fn control(&self) -> EngineControl {
        self.control.clone()
    }

    /// A live snapshot of the cumulative counters (non-consuming).
    pub fn stats(&self) -> EngineStats {
        let mut stats = EngineStats::default();
        if let Some(sender) = &self.sender {
            stats.packets_sent = sender.packets.load(Ordering::Relaxed);
            stats.bytes_sent = sender.bytes.load(Ordering::Relaxed);
            stats.capture_overruns = sender.capture_overruns.load(Ordering::Relaxed);
        }
        if let Some(receiver) = &self.receiver {
            stats.packets_received = receiver.stats.received.load(Ordering::Relaxed);
            stats.bytes_received = receiver.stats.bytes.load(Ordering::Relaxed);
            stats.gap_frames = receiver.stats.gap_frames.load(Ordering::Relaxed);
            stats.dropped_late = receiver.stats.dropped_late.load(Ordering::Relaxed);
            stats.overruns = receiver.stats.overruns.load(Ordering::Relaxed);
            stats.recenter_drops = receiver.stats.recenter_drops.load(Ordering::Relaxed);
            stats.recenter_inserts = receiver.stats.recenter_inserts.load(Ordering::Relaxed);
            stats.jitter_fill = receiver.stats.jitter_fill.load(Ordering::Relaxed);
            stats.jitter_capacity = receiver.capacity as u64;
            stats.jitter_fill_ms = receiver.stats.jitter_fill_ms.load(Ordering::Relaxed);
            stats.target_depth_ms = receiver.stats.target_depth_ms.load(Ordering::Relaxed);
            stats.underruns = receiver.playout.underruns.load(Ordering::Relaxed);
        }
        stats
    }

    /// Stop the send/receive threads and return the final stats.
    pub fn stop(self) -> Result<EngineStats> {
        let mut stats = EngineStats::default();
        if let Some(sender) = self.sender {
            let packets = Arc::clone(&sender.packets);
            let bytes = Arc::clone(&sender.bytes);
            let capture_overruns = Arc::clone(&sender.capture_overruns);
            sender.stop_and_join()?;
            stats.packets_sent = packets.load(Ordering::Relaxed);
            stats.bytes_sent = bytes.load(Ordering::Relaxed);
            stats.capture_overruns = capture_overruns.load(Ordering::Relaxed);
        }
        if let Some(receiver) = self.receiver {
            let counters = Arc::clone(&receiver.stats);
            let playout = Arc::clone(&receiver.playout);
            receiver.stop_and_join()?;
            stats.underruns = playout.underruns.load(Ordering::Relaxed);
            stats.packets_received = counters.received.load(Ordering::Relaxed);
            stats.bytes_received = counters.bytes.load(Ordering::Relaxed);
            stats.gap_frames = counters.gap_frames.load(Ordering::Relaxed);
            stats.dropped_late = counters.dropped_late.load(Ordering::Relaxed);
            stats.overruns = counters.overruns.load(Ordering::Relaxed);
            stats.recenter_drops = counters.recenter_drops.load(Ordering::Relaxed);
            stats.recenter_inserts = counters.recenter_inserts.load(Ordering::Relaxed);
        }
        Ok(stats)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A mono source over a ring of `capacity`, resuming at `resume_at` samples.
    fn source(capacity: usize, resume_at: usize) -> (HeapProd<f32>, PlaybackSource) {
        let (producer, consumer) = HeapRb::<f32>::new(capacity).split();
        let source = PlaybackSource {
            consumer,
            channels: 1,
            playout: Arc::new(Playout::new(resume_at)),
            starved: true,
            period: 0,
        };
        (producer, source)
    }

    fn underruns(source: &PlaybackSource) -> u64 {
        source.playout.underruns.load(Ordering::Relaxed)
    }

    /// Startup: silent until the ring reaches the resume point, and the initially
    /// empty ring is not counted as an underrun.
    #[test]
    fn starts_silent_until_target_then_fades_in() {
        let (mut producer, mut src) = source(4096, 1000);
        let mut out = vec![1.0f32; 256];

        producer.push_slice(&vec![0.5f32; 900]); // below the resume point
        src.fill(&mut out);
        assert!(out.iter().all(|&s| s == 0.0), "still rebuffering");
        assert_eq!(underruns(&src), 0);

        producer.push_slice(&vec![0.5f32; 200]); // now at/above it
        src.fill(&mut out);
        assert!(out[0] < 0.05, "resume edge is faded in, not a step");
        assert_eq!(out[255], 0.5);
        assert!(!src.playout.starved.load(Ordering::Relaxed));
        assert_eq!(src.playout.period.load(Ordering::Relaxed), 256);
    }

    /// Running dry mid-callback: the tail fades out, the rest is silence, one underrun
    /// is counted, and playback waits for the target again instead of trickling.
    #[test]
    fn underrun_counts_once_and_rebuffers() {
        let (mut producer, mut src) = source(4096, 512);
        let mut out = vec![0.0f32; 256];
        producer.push_slice(&vec![0.5f32; 512 + 100]);
        src.fill(&mut out); // resume, 356 left
        src.fill(&mut out); // 100 left

        src.fill(&mut out); // only 100 of 256 available → underrun
        assert_eq!(underruns(&src), 1);
        assert!(out[99] < 0.05, "tail faded out");
        assert!(out[100..].iter().all(|&s| s == 0.0));

        // Further starved callbacks don't re-count, and a trickle doesn't resume.
        producer.push_slice(&vec![0.5f32; 300]);
        src.fill(&mut out);
        assert!(out.iter().all(|&s| s == 0.0));
        assert_eq!(underruns(&src), 1);

        producer.push_slice(&vec![0.5f32; 300]); // 600 ≥ 512 → resumes
        src.fill(&mut out);
        assert_eq!(out[255], 0.5);
        assert_eq!(underruns(&src), 1);
    }

    /// A resume never starts a callback it can't finish, even when the target is
    /// smaller than the request.
    #[test]
    fn resume_waits_for_a_full_request() {
        let (mut producer, mut src) = source(4096, 10);
        let mut out = vec![0.0f32; 256];
        producer.push_slice(&vec![0.5f32; 100]); // ≥ target, < request
        src.fill(&mut out);
        assert!(out.iter().all(|&s| s == 0.0));
        assert_eq!(underruns(&src), 0);
    }

    /// A full capture ring drops the excess and counts the overrun.
    #[test]
    fn capture_overrun_is_counted() {
        let (producer, _consumer) = HeapRb::<f32>::new(100).split();
        let mut sink = CaptureSink {
            producer,
            overruns: Arc::new(AtomicU64::new(0)),
        };
        sink.push(&[0.0; 80]);
        assert_eq!(sink.overruns.load(Ordering::Relaxed), 0);
        sink.push(&[0.0; 80]);
        assert_eq!(sink.overruns.load(Ordering::Relaxed), 1);
    }
}
