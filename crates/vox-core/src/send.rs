//! Send thread: drain the capture ring → downmix → Opus encode → packetize → UDP.
//! Owns the one Opus encoder (DESIGN §2). The capture-ring producer lives in the
//! platform's record callback (a [`crate::CaptureSink`]); this is the consumer end.

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use ringbuf::traits::Consumer;
use ringbuf::HeapCons;

use crate::audio::{FRAME, MAX_PACKET, RATE};
use crate::packet;

pub(crate) struct SendThread {
    thread: JoinHandle<Result<()>>,
    stop: Arc<AtomicBool>,
    pub(crate) packets: Arc<AtomicU64>,
    pub(crate) bytes: Arc<AtomicU64>,
}

impl SendThread {
    pub(crate) fn stop_and_join(self) -> Result<()> {
        self.stop.store(true, Ordering::Release);
        match self.thread.join() {
            Ok(result) => result,
            Err(_) => bail!("send thread panicked"),
        }
    }
}

pub(crate) fn spawn(
    consumer: HeapCons<f32>,
    socket: Arc<UdpSocket>,
    peer: SocketAddr,
    channels: usize,
    bitrate: i32,
) -> Result<SendThread> {
    let mut encoder = opus::Encoder::new(RATE, opus::Channels::Mono, opus::Application::Voip)
        .context("create opus encoder")?;
    encoder
        .set_bitrate(opus::Bitrate::Bits(bitrate))
        .context("set opus bitrate")?;

    let stop = Arc::new(AtomicBool::new(false));
    let packets = Arc::new(AtomicU64::new(0));
    let bytes = Arc::new(AtomicU64::new(0));
    let thread = {
        let stop = Arc::clone(&stop);
        let packets = Arc::clone(&packets);
        let bytes = Arc::clone(&bytes);
        thread::spawn(move || {
            send_loop(SendLoop {
                consumer,
                socket,
                peer,
                encoder,
                channels,
                stop,
                packets,
                bytes,
            })
        })
    };
    Ok(SendThread {
        thread,
        stop,
        packets,
        bytes,
    })
}

/// The send thread's owned state (one struct so the worker takes a single arg).
struct SendLoop {
    consumer: HeapCons<f32>,
    socket: Arc<UdpSocket>,
    peer: SocketAddr,
    encoder: opus::Encoder,
    channels: usize,
    stop: Arc<AtomicBool>,
    packets: Arc<AtomicU64>,
    bytes: Arc<AtomicU64>,
}

fn send_loop(ctx: SendLoop) -> Result<()> {
    let SendLoop {
        mut consumer,
        socket,
        peer,
        mut encoder,
        channels,
        stop,
        packets,
        bytes: sent_bytes,
    } = ctx;
    let mut read = vec![0.0f32; 4096];
    let mut interleaved: Vec<f32> = Vec::with_capacity(8192); // < channels leftover
    let mut mono: Vec<f32> = Vec::with_capacity(FRAME * 4); // < FRAME leftover
    let mut datagram = vec![0u8; packet::HEADER_LEN + MAX_PACKET];
    let mut seq: u32 = 0;
    let mut timestamp: u32 = 0;

    while !stop.load(Ordering::Acquire) {
        let n = consumer.pop_slice(&mut read);
        if n == 0 {
            thread::sleep(Duration::from_millis(3));
            continue;
        }

        // Downmix each complete interleaved frame to one mono sample.
        interleaved.extend_from_slice(&read[..n]);
        let complete = interleaved.len() / channels;
        for i in 0..complete {
            let frame = &interleaved[i * channels..(i + 1) * channels];
            mono.push(frame.iter().sum::<f32>() / channels as f32);
        }
        interleaved.drain(..complete * channels);

        // Encode and send whole 20 ms mono frames.
        while mono.len() >= FRAME {
            let bytes = encoder
                .encode_float(&mono[..FRAME], &mut datagram[packet::HEADER_LEN..])
                .context("opus encode")?;
            packet::write_header(seq, timestamp, &mut datagram[..packet::HEADER_LEN]);
            socket
                .send_to(&datagram[..packet::HEADER_LEN + bytes], peer)
                .context("udp send")?;

            seq = seq.wrapping_add(1);
            timestamp = timestamp.wrapping_add(FRAME as u32);
            packets.fetch_add(1, Ordering::Relaxed);
            sent_bytes.fetch_add((packet::HEADER_LEN + bytes) as u64, Ordering::Relaxed);
            mono.drain(..FRAME);
        }
    }
    Ok(())
}
