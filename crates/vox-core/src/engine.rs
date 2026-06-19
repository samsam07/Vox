//! The engine: owns the socket, the send/receive threads, and the rings. Returns
//! the [`AudioPorts`] ring seam for a platform audio layer to drive (DESIGN §11).

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use anyhow::{bail, Result};
use ringbuf::traits::{Consumer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};

use crate::audio::{ring_capacity, CAPTURE_RING_MS};
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
    pub jitter_ms: u32,
    pub bitrate: i32,
}

/// Non-blocking sink: the platform's record callback pushes device PCM into it.
/// This is the capture ring's producer end (one writer — DESIGN §3).
pub struct CaptureSink {
    producer: HeapProd<f32>,
}

impl CaptureSink {
    /// Push captured interleaved PCM. Non-blocking; a full ring drops the excess
    /// (overrun). Safe to call from a real-time audio callback.
    pub fn push(&mut self, data: &[f32]) {
        self.producer.push_slice(data);
    }
}

/// Non-blocking source: the platform's play callback pulls PCM to play from it,
/// getting silence on underrun. This is the jitter buffer's consumer end (one
/// reader — DESIGN §3).
pub struct PlaybackSource {
    consumer: HeapCons<f32>,
}

impl PlaybackSource {
    /// Fill `data` with interleaved PCM to play, padding with silence on underrun.
    /// Non-blocking. Safe to call from a real-time audio callback.
    pub fn fill(&mut self, data: &mut [f32]) {
        let popped = self.consumer.pop_slice(data);
        if popped < data.len() {
            data[popped..].fill(0.0);
        }
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
    /// Current jitter-buffer occupancy and capacity, in samples.
    pub jitter_fill: u64,
    pub jitter_capacity: u64,
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
                let ring = HeapRb::<f32>::new(ring_capacity(CAPTURE_RING_MS, channels));
                let (producer, consumer) = ring.split();
                let thread = send::spawn(
                    consumer,
                    Arc::clone(&socket),
                    peer,
                    channels as usize,
                    config.bitrate,
                )?;
                (Some(thread), Some(CaptureSink { producer }))
            }
            (Some(_), None) => bail!("a capture role requires a peer to send to"),
            (None, _) => (None, None),
        };

        let (receiver, playback) = match config.playback_channels {
            Some(channels) => {
                let capacity = ring_capacity(config.jitter_ms, channels);
                let ring = HeapRb::<f32>::new(capacity);
                let (mut producer, consumer) = ring.split();
                // Prefill a look-ahead cushion so playback starts smoothly and
                // short-term jitter is absorbed (the buffer FEC relies on at M7).
                producer.push_slice(&vec![0.0f32; capacity / 2]);
                let thread =
                    receive::spawn(producer, Arc::clone(&socket), channels as usize, capacity)?;
                (Some(thread), Some(PlaybackSource { consumer }))
            }
            None => (None, None),
        };

        Ok((
            Engine {
                socket,
                sender,
                receiver,
            },
            AudioPorts { capture, playback },
        ))
    }

    /// The bound local address (for diagnostics).
    pub fn local_addr(&self) -> std::io::Result<SocketAddr> {
        self.socket.local_addr()
    }

    /// A live snapshot of the cumulative counters (non-consuming).
    pub fn stats(&self) -> EngineStats {
        let mut stats = EngineStats::default();
        if let Some(sender) = &self.sender {
            stats.packets_sent = sender.packets.load(Ordering::Relaxed);
            stats.bytes_sent = sender.bytes.load(Ordering::Relaxed);
        }
        if let Some(receiver) = &self.receiver {
            stats.packets_received = receiver.stats.received.load(Ordering::Relaxed);
            stats.bytes_received = receiver.stats.bytes.load(Ordering::Relaxed);
            stats.gap_frames = receiver.stats.gap_frames.load(Ordering::Relaxed);
            stats.dropped_late = receiver.stats.dropped_late.load(Ordering::Relaxed);
            stats.overruns = receiver.stats.overruns.load(Ordering::Relaxed);
            stats.jitter_fill = receiver.stats.jitter_fill.load(Ordering::Relaxed);
            stats.jitter_capacity = receiver.capacity as u64;
        }
        stats
    }

    /// Stop the send/receive threads and return the final stats.
    pub fn stop(self) -> Result<EngineStats> {
        let mut stats = EngineStats::default();
        if let Some(sender) = self.sender {
            let packets = Arc::clone(&sender.packets);
            let bytes = Arc::clone(&sender.bytes);
            sender.stop_and_join()?;
            stats.packets_sent = packets.load(Ordering::Relaxed);
            stats.bytes_sent = bytes.load(Ordering::Relaxed);
        }
        if let Some(receiver) = self.receiver {
            let counters = Arc::clone(&receiver.stats);
            receiver.stop_and_join()?;
            stats.packets_received = counters.received.load(Ordering::Relaxed);
            stats.bytes_received = counters.bytes.load(Ordering::Relaxed);
            stats.gap_frames = counters.gap_frames.load(Ordering::Relaxed);
            stats.dropped_late = counters.dropped_late.load(Ordering::Relaxed);
            stats.overruns = counters.overruns.load(Ordering::Relaxed);
        }
        Ok(stats)
    }
}
