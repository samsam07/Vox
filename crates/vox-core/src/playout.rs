//! Playout control (M10c, DESIGN §3): underrun handling on the jitter buffer's
//! reader side. The playback callback cannot decode (sacred), so on underrun it goes
//! silent and *rebuffers* — stays silent until the ring refills to the adaptive
//! target — instead of limping along at the floor and breaking again on the next
//! hiccup. Edges are faded (~2 ms) so a break doesn't click.
//!
//! [`Playout`] is the small state the callback and the receive thread share. It is
//! atomics only, each field with exactly one writer, so the callback stays lock-free
//! and allocation-free; the ring itself stays strictly SPSC.

use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

/// Fade length at an underrun / resume edge, in frames (~2 ms at 48 kHz).
const FADE_FRAMES: usize = 96;

/// State shared between the playback callback and the receive thread.
pub(crate) struct Playout {
    /// Occupancy (ring samples) a starved callback waits for before resuming — the
    /// adaptive band centre. Written by the receive thread.
    pub(crate) resume_at: AtomicUsize,
    /// Largest callback request seen (interleaved samples): the playback period the
    /// receive thread floors its low watermark on. Written by the callback.
    pub(crate) period: AtomicUsize,
    /// True while rebuffering; the receive thread doesn't recenter-insert then (a
    /// repeated frame would just play back as a stutter after resume). Written by the
    /// callback.
    pub(crate) starved: AtomicBool,
    /// Underruns: playing → starved transitions (each is one audible break). Written
    /// by the callback.
    pub(crate) underruns: AtomicU64,
}

impl Playout {
    /// Start starved, so playback first fills to `resume_at` (the startup prefill and
    /// FEC look-ahead — §4) and the initial empty ring isn't counted as an underrun.
    pub(crate) fn new(resume_at: usize) -> Self {
        Playout {
            resume_at: AtomicUsize::new(resume_at),
            period: AtomicUsize::new(0),
            starved: AtomicBool::new(true),
            underruns: AtomicU64::new(0),
        }
    }
}

/// Scale the first (`rising`) or last (`!rising`) up-to-[`FADE_FRAMES`] frames of an
/// interleaved buffer by a linear ramp toward/away from silence. Arithmetic only.
pub(crate) fn fade(samples: &mut [f32], channels: usize, rising: bool) {
    let channels = channels.max(1);
    let frames = samples.len() / channels;
    let n = frames.min(FADE_FRAMES);
    let start = if rising { 0 } else { frames - n };
    for i in 0..n {
        let t = (i + 1) as f32 / (n + 1) as f32;
        let gain = if rising { t } else { 1.0 - t };
        let at = (start + i) * channels;
        for sample in &mut samples[at..at + channels] {
            *sample *= gain;
        }
    }
}

/// Load a relaxed atomic (shorthand for the callback/receive-thread handoff fields).
pub(crate) fn relaxed(value: &AtomicUsize) -> usize {
    value.load(Ordering::Relaxed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fade_out_ramps_tail_to_near_silence() {
        let mut buf = vec![1.0f32; 400];
        fade(&mut buf, 2, false);
        // Head untouched, tail ramps down, last frame nearly silent.
        assert_eq!(buf[0], 1.0);
        assert!(buf[398] < 0.02 && buf[399] < 0.02);
        assert!(buf[300] > buf[398]);
        // Both channels of a frame get the same gain.
        assert_eq!(buf[398], buf[399]);
    }

    #[test]
    fn fade_in_ramps_head_from_near_silence() {
        let mut buf = vec![1.0f32; 400];
        fade(&mut buf, 1, true);
        assert!(buf[0] < 0.02);
        assert!(buf[50] > buf[0]);
        assert_eq!(buf[399], 1.0);
    }

    #[test]
    fn fade_handles_short_and_empty_buffers() {
        let mut empty: [f32; 0] = [];
        fade(&mut empty, 2, false);
        let mut short = vec![1.0f32; 6]; // 3 stereo frames < FADE_FRAMES
        fade(&mut short, 2, false);
        assert!(short.iter().all(|s| (0.0..1.0).contains(s)));
    }
}
