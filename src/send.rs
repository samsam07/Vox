//! Send path: capture callback → capture ring → send thread (downmix → Opus
//! encode → packetize → UDP). The send thread solely owns the one Opus encoder
//! (DESIGN §2). The capture callback is sacred: a non-blocking ring push only.

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use cpal::traits::{DeviceTrait, StreamTrait};
use cpal::{Device, Stream};
use ringbuf::traits::{Consumer, Producer, Split};
use ringbuf::{HeapCons, HeapRb};

use crate::audio::{self, FRAME, MAX_PACKET, RATE};
use crate::device::Role;
use crate::packet;

/// Live send path. Holds the capture stream (must stay on the main thread) and the
/// send thread; dropping/stopping it tears both down.
pub struct Sender {
    stream: Stream,
    thread: Option<JoinHandle<Result<()>>>,
    stop: Arc<AtomicBool>,
    packets: Arc<AtomicU64>,
}

impl Sender {
    /// Signal the send thread to stop, join it, and report.
    pub fn stop_and_join(mut self) -> Result<()> {
        self.stop.store(true, Ordering::Release);
        let _ = self.stream.pause();
        let result = match self.thread.take() {
            Some(handle) => match handle.join() {
                Ok(result) => result,
                Err(_) => bail!("send thread panicked"),
            },
            None => Ok(()),
        };
        println!("  send: {} packets sent", self.packets.load(Ordering::Relaxed));
        result
    }
}

/// Start capturing from `device`, encoding, and sending frames to `peer`.
pub fn start(
    socket: Arc<UdpSocket>,
    peer: SocketAddr,
    device: &Device,
    ring_ms: u32,
    bitrate: i32,
) -> Result<Sender> {
    let channels = audio::pick_channels(device, Role::Capture)?;
    let ring = HeapRb::<f32>::new(audio::ring_capacity(ring_ms, channels));
    let (mut producer, consumer) = ring.split();

    let mut encoder = opus::Encoder::new(RATE, opus::Channels::Mono, opus::Application::Voip)
        .context("create opus encoder")?;
    encoder
        .set_bitrate(opus::Bitrate::Bits(bitrate))
        .context("set opus bitrate")?;

    let stream = device
        .build_input_stream::<f32, _, _>(
            &audio::stream_config(channels),
            // SACRED: non-blocking ring push only.
            move |data: &[f32], _| {
                producer.push_slice(data);
            },
            move |err| eprintln!("capture stream error: {err}"),
            None,
        )
        .context("build capture stream")?;

    let stop = Arc::new(AtomicBool::new(false));
    let packets = Arc::new(AtomicU64::new(0));
    let thread = {
        let stop = Arc::clone(&stop);
        let packets = Arc::clone(&packets);
        thread::spawn(move || {
            send_loop(consumer, socket, peer, encoder, channels as usize, stop, packets)
        })
    };
    stream.play().context("start capture stream")?;

    Ok(Sender {
        stream,
        thread: Some(thread),
        stop,
        packets,
    })
}

fn send_loop(
    mut consumer: HeapCons<f32>,
    socket: Arc<UdpSocket>,
    peer: SocketAddr,
    mut encoder: opus::Encoder,
    channels: usize,
    stop: Arc<AtomicBool>,
    packets: Arc<AtomicU64>,
) -> Result<()> {
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
            mono.drain(..FRAME);
        }
    }
    Ok(())
}
