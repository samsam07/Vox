//! Send thread: drain the capture ring → downmix → Opus encode → packetize → UDP.
//! Owns the one Opus encoder (DESIGN §2). The capture-ring producer lives in the
//! platform's record callback (a [`crate::CaptureSink`]); this is the consumer end.

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use log::debug;
use ringbuf::traits::Consumer;
use ringbuf::HeapCons;

use crate::audio::{FRAME, MAX_PACKET, RATE};
use crate::packet;
use crate::resample::Resampler;

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

/// Encoder-only / send-path codec settings (DESIGN §4, §6). `expected_loss` tunes
/// FEC and so only applies when `fec` is on.
pub(crate) struct EncoderParams {
    pub(crate) bitrate: i32,
    pub(crate) fec: bool,
    pub(crate) expected_loss: u8,
    pub(crate) dtx: bool,
}

pub(crate) fn spawn(
    consumer: HeapCons<f32>,
    socket: Arc<UdpSocket>,
    peer: SocketAddr,
    channels: usize,
    capture_rate: u32,
    params: EncoderParams,
) -> Result<SendThread> {
    // Resample the downmixed capture-rate mono up to the 48 kHz wire rate before
    // encode (a passthrough when the device already runs at 48 kHz) — DESIGN §4.
    let resampler = Resampler::new(capture_rate, RATE)?;
    let mut encoder = opus::Encoder::new(RATE, opus::Channels::Mono, opus::Application::Voip)
        .context("create opus encoder")?;
    encoder
        .set_bitrate(opus::Bitrate::Bits(params.bitrate))
        .context("set opus bitrate")?;
    // In-band FEC carries a redundant low-bitrate copy of the previous frame inside
    // each packet; the receiver reconstructs a single lost frame from it (DESIGN §4).
    encoder
        .set_inband_fec(params.fec)
        .context("set opus inband fec")?;
    // packet-loss-perc tunes how much FEC redundancy the encoder spends; it only
    // matters with FEC on, so leave it at 0 otherwise (cli: "to tune FEC").
    let loss = if params.fec {
        (params.expected_loss as i32).min(100)
    } else {
        0
    };
    encoder
        .set_packet_loss_perc(loss)
        .context("set opus packet-loss perc")?;
    // DTX shrinks silence frames to 1-2 byte packets. We still transmit every frame
    // (seq stays contiguous → a receiver gap always means real loss, not silence),
    // which also keeps NAT mappings warm on the symmetric UDP peers (DESIGN §1).
    encoder.set_dtx(params.dtx).context("set opus dtx")?;

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
                resampler,
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
    resampler: Resampler,
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
        mut resampler,
        channels,
        stop,
        packets,
        bytes: sent_bytes,
    } = ctx;
    let mut read = vec![0.0f32; 4096];
    let mut interleaved: Vec<f32> = Vec::with_capacity(8192); // < channels leftover
    let mut mono: Vec<f32> = Vec::with_capacity(FRAME * 4); // capture-rate downmix
    let mut wire: Vec<f32> = Vec::with_capacity(FRAME * 4); // 48 kHz, encoded by frame
    let mut datagram = vec![0u8; packet::HEADER_LEN + MAX_PACKET];
    let mut seq: u32 = 0;
    let mut timestamp: u32 = 0;
    // Edge-trigger for send-failure logging (peer down): log on enter/recover only.
    let mut send_failing = false;

    while !stop.load(Ordering::Acquire) {
        let n = consumer.pop_slice(&mut read);
        if n == 0 {
            thread::sleep(Duration::from_millis(3));
            continue;
        }

        // Downmix each complete interleaved frame to one capture-rate mono sample.
        interleaved.extend_from_slice(&read[..n]);
        let complete = interleaved.len() / channels;
        mono.clear();
        for i in 0..complete {
            let frame = &interleaved[i * channels..(i + 1) * channels];
            mono.push(frame.iter().sum::<f32>() / channels as f32);
        }
        interleaved.drain(..complete * channels);

        // Resample capture-rate mono → 48 kHz wire samples (the resampler buffers
        // sub-chunk remainders internally), then encode whole 20 ms frames.
        resampler.process(&mono, &mut wire)?;
        while wire.len() >= FRAME {
            let bytes = encoder
                .encode_float(&wire[..FRAME], &mut datagram[packet::HEADER_LEN..])
                .context("opus encode")?;
            packet::write_header(seq, timestamp, &mut datagram[..packet::HEADER_LEN]);
            match socket.send_to(&datagram[..packet::HEADER_LEN + bytes], peer) {
                Ok(_) => {
                    if send_failing {
                        debug!("udp send recovered");
                        send_failing = false;
                    }
                    packets.fetch_add(1, Ordering::Relaxed);
                    sent_bytes.fetch_add((packet::HEADER_LEN + bytes) as u64, Ordering::Relaxed);
                }
                // The peer being down/restarting draws ICMP that surfaces as a send
                // error (e.g. ConnectionReset on Windows). Drop this frame but keep
                // the thread alive so the stream resumes when the peer returns — UDP
                // is lossy and a restarted peer resyncs from our later packets. Don't
                // count it as sent.
                Err(e) => {
                    if !send_failing {
                        debug!("udp send failing (peer unreachable?): {e}");
                        send_failing = true;
                    }
                }
            }

            seq = seq.wrapping_add(1);
            timestamp = timestamp.wrapping_add(FRAME as u32);
            wire.drain(..FRAME);
        }
    }
    Ok(())
}
