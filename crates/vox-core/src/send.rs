//! Send thread: drain the capture ring → downmix → Opus encode → packetize → UDP.
//! Owns the one Opus encoder (DESIGN §2). The capture-ring producer lives in the
//! platform's record callback (a [`crate::CaptureSink`]); this is the consumer end.

use std::net::{SocketAddr, UdpSocket};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{bail, Context, Result};
use log::{debug, warn};
use ringbuf::traits::Consumer;
use ringbuf::HeapCons;

use crate::audio::{FRAME, MAX_PACKET, RATE};
use crate::control::{EncoderControls, Setting, Tunable};
use crate::packet;
use crate::resample::Resampler;

pub(crate) struct SendThread {
    thread: JoinHandle<Result<()>>,
    stop: Arc<AtomicBool>,
    pub(crate) packets: Arc<AtomicU64>,
    pub(crate) bytes: Arc<AtomicU64>,
    /// Shared with the [`crate::CaptureSink`], which increments it (M10c).
    pub(crate) capture_overruns: Arc<AtomicU64>,
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
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct EncoderParams {
    pub(crate) bitrate: i32,
    pub(crate) fec: bool,
    pub(crate) expected_loss: u8,
    pub(crate) dtx: bool,
}

impl EncoderParams {
    /// packet-loss-perc tunes how much FEC redundancy the encoder spends; it only
    /// matters with FEC on, so it is 0 otherwise (cli: "to tune FEC").
    fn loss_perc(&self) -> i32 {
        if self.fec {
            i32::from(self.expected_loss).min(100)
        } else {
            0
        }
    }
}

/// Put every send-path setting on a fresh encoder (start-up).
fn configure(encoder: &mut opus::Encoder, params: &EncoderParams) -> Result<()> {
    encoder
        .set_bitrate(opus::Bitrate::Bits(params.bitrate))
        .context("set opus bitrate")?;
    // In-band FEC carries a redundant low-bitrate copy of the previous frame inside
    // each packet; the receiver reconstructs a single lost frame from it (DESIGN §4).
    encoder
        .set_inband_fec(params.fec)
        .context("set opus inband fec")?;
    encoder
        .set_packet_loss_perc(params.loss_perc())
        .context("set opus packet-loss perc")?;
    // DTX shrinks silence frames to 1-2 byte packets. We still transmit every frame
    // (seq stays contiguous → a receiver gap always means real loss, not silence),
    // which also keeps NAT mappings warm on the symmetric UDP peers (DESIGN §1).
    encoder.set_dtx(params.dtx).context("set opus dtx")?;
    Ok(())
}

/// Apply one live change to the encoder (DESIGN §12). `params` mirrors what the
/// encoder holds; it is updated only when the ctl succeeds.
fn apply(encoder: &mut opus::Encoder, params: &mut EncoderParams, setting: Setting) -> Result<()> {
    let mut next = *params;
    let result = match setting {
        Setting::Bitrate(bits) => {
            next.bitrate = bits;
            encoder.set_bitrate(opus::Bitrate::Bits(bits))
        }
        Setting::Fec(on) => {
            next.fec = on;
            encoder
                .set_inband_fec(on)
                .and_then(|()| encoder.set_packet_loss_perc(next.loss_perc()))
        }
        Setting::ExpectedLoss(perc) => {
            next.expected_loss = perc;
            encoder.set_packet_loss_perc(next.loss_perc())
        }
        Setting::Dtx(on) => {
            next.dtx = on;
            encoder.set_dtx(on)
        }
        // Routed to the receive thread; never queued here.
        Setting::DriftCorrect(_) => return Ok(()),
    };
    result.with_context(|| format!("apply {setting:?}"))?;
    *params = next;
    Ok(())
}

/// Drain queued live changes. Called once per loop pass, so each lands between
/// frames, never mid-encode. A failed change is logged and the previous setting
/// kept: live tuning must never end the call.
fn drain_controls(
    controls: &EncoderControls,
    encoder: &mut opus::Encoder,
    params: &mut EncoderParams,
) {
    while let Ok(setting) = controls.try_recv() {
        match apply(encoder, params, setting) {
            Ok(()) => debug!("send path: applied {setting:?}"),
            Err(e) => warn!("send path: {e:#}; keeping the previous setting"),
        }
    }
}

pub(crate) fn spawn(
    consumer: HeapCons<f32>,
    socket: Arc<UdpSocket>,
    peer: SocketAddr,
    channels: usize,
    capture_rate: u32,
    encoding: Tunable<EncoderParams>,
    capture_overruns: Arc<AtomicU64>,
) -> Result<SendThread> {
    let Tunable {
        start: params,
        controls,
    } = encoding;
    // Resample the downmixed capture-rate mono up to the 48 kHz wire rate before
    // encode (a passthrough when the device already runs at 48 kHz) — DESIGN §4.
    let resampler = Resampler::new(capture_rate, RATE)?;
    let mut encoder = opus::Encoder::new(RATE, opus::Channels::Mono, opus::Application::Voip)
        .context("create opus encoder")?;
    configure(&mut encoder, &params)?;

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
                params,
                controls,
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
        capture_overruns,
    })
}

/// The send thread's owned state (one struct so the worker takes a single arg).
struct SendLoop {
    consumer: HeapCons<f32>,
    socket: Arc<UdpSocket>,
    peer: SocketAddr,
    encoder: opus::Encoder,
    /// What the encoder currently holds (live changes update it — DESIGN §12).
    params: EncoderParams,
    controls: EncoderControls,
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
        mut params,
        controls,
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
        // Top of the pass: between frames, so a change never splits an encode.
        drain_controls(&controls, &mut encoder, &mut params);
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn encoder(params: &EncoderParams) -> opus::Encoder {
        let mut enc =
            opus::Encoder::new(RATE, opus::Channels::Mono, opus::Application::Voip).unwrap();
        configure(&mut enc, params).unwrap();
        enc
    }

    const START: EncoderParams = EncoderParams {
        bitrate: 24_000,
        fec: false,
        expected_loss: 10,
        dtx: false,
    };

    /// Queued changes are all applied by the next drain (the top of the next loop
    /// pass, i.e. before the next frame is encoded) and the encoder reports them.
    #[test]
    fn queued_changes_apply_on_the_next_drain() {
        let (tx, rx) = mpsc::channel();
        let mut params = START;
        let mut enc = encoder(&params);
        tx.send(Setting::Bitrate(12_000)).unwrap();
        tx.send(Setting::Dtx(true)).unwrap();
        // Nothing changes until the thread drains.
        assert_eq!(enc.get_bitrate().unwrap(), opus::Bitrate::Bits(24_000));

        drain_controls(&rx, &mut enc, &mut params);
        assert_eq!(enc.get_bitrate().unwrap(), opus::Bitrate::Bits(12_000));
        assert!(enc.get_dtx().unwrap());
        assert_eq!(params.bitrate, 12_000);
        assert!(params.dtx);
        assert!(rx.try_recv().is_err(), "queue fully drained");
    }

    /// expected_loss follows the start-up rule: it reaches the encoder only while
    /// fec is on, and turning fec on applies the stored value.
    #[test]
    fn expected_loss_only_applies_with_fec_on() {
        let (tx, rx) = mpsc::channel();
        let mut params = START;
        let mut enc = encoder(&params);
        tx.send(Setting::ExpectedLoss(25)).unwrap();
        drain_controls(&rx, &mut enc, &mut params);
        assert_eq!(enc.get_packet_loss_perc().unwrap(), 0);
        assert_eq!(params.expected_loss, 25);

        tx.send(Setting::Fec(true)).unwrap();
        drain_controls(&rx, &mut enc, &mut params);
        assert!(enc.get_inband_fec().unwrap());
        assert_eq!(enc.get_packet_loss_perc().unwrap(), 25);

        tx.send(Setting::Fec(false)).unwrap();
        drain_controls(&rx, &mut enc, &mut params);
        assert_eq!(enc.get_packet_loss_perc().unwrap(), 0);
    }

    /// A ctl the encoder rejects leaves both the encoder and `params` as they were.
    #[test]
    fn rejected_change_keeps_the_previous_setting() {
        let mut params = START;
        let mut enc = encoder(&params);
        assert!(apply(&mut enc, &mut params, Setting::Bitrate(-5)).is_err());
        assert_eq!(params, START);
        assert_eq!(enc.get_bitrate().unwrap(), opus::Bitrate::Bits(24_000));
    }
}
