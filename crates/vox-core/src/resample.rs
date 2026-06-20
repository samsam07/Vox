//! Sample-rate conversion between a device rate and the 48 kHz wire rate (DESIGN
//! §4). Mono only: resampling sits at the device edge — on the send path between
//! downmix and encode (capture rate → 48 kHz), on the receive path between decode
//! and upmix (48 kHz → playback rate). The codec and wire stay 48 kHz mono.
//!
//! A device already at 48 kHz uses [`Resampler::Passthrough`] — no rubato, the
//! 48 kHz path is unchanged. `[PHASE-2]` M10 reuses this seam for drift correction.

use anyhow::{anyhow, Result};
use rubato::{FftFixedIn, Resampler as _};

/// Fixed input chunk fed to rubato per FFT (~21 ms at 48 kHz) — a latency/overhead
/// balance; non-48 kHz only, so it never touches the common path.
const CHUNK: usize = 1024;

/// Converts a mono f32 stream from one rate to another, buffering internally so the
/// caller can push arbitrary-length input and pull whatever output is ready.
pub(crate) enum Resampler {
    /// Rates match — hand samples straight through.
    Passthrough,
    Convert {
        // Boxed: `FftFixedIn` is large and `Passthrough` carries nothing, so an
        // unboxed variant bloats every `Resampler` (clippy::large_enum_variant). Off
        // the audio callback, so the indirection is free.
        inner: Box<FftFixedIn<f32>>,
        /// Input not yet consumed (rubato needs a full chunk to process).
        pending: Vec<f32>,
    },
}

impl Resampler {
    /// A converter from `in_rate` to `out_rate` (a passthrough when they match).
    pub(crate) fn new(in_rate: u32, out_rate: u32) -> Result<Self> {
        if in_rate == out_rate {
            return Ok(Resampler::Passthrough);
        }
        let inner = FftFixedIn::<f32>::new(in_rate as usize, out_rate as usize, CHUNK, 1, 1)
            .map_err(|e| anyhow!("create resampler {in_rate}->{out_rate} Hz: {e}"))?;
        Ok(Resampler::Convert {
            inner: Box::new(inner),
            pending: Vec::with_capacity(CHUNK * 2),
        })
    }

    /// Push mono `input`, appending all resampled output produced so far to `out`.
    /// Output is delayed by the resampler's filter and emitted a chunk at a time, so
    /// a single call may append more or fewer samples than it was given.
    pub(crate) fn process(&mut self, input: &[f32], out: &mut Vec<f32>) -> Result<()> {
        match self {
            Resampler::Passthrough => out.extend_from_slice(input),
            Resampler::Convert { inner, pending } => {
                pending.extend_from_slice(input);
                while pending.len() >= inner.input_frames_next() {
                    let need = inner.input_frames_next();
                    let resampled = inner
                        .process(&[&pending[..need]], None)
                        .map_err(|e| anyhow!("resample: {e}"))?;
                    out.extend_from_slice(&resampled[0]);
                    pending.drain(..need);
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 440 Hz mono tone of `n` samples at `rate`.
    fn tone(rate: u32, n: usize) -> Vec<f32> {
        let step = 2.0 * std::f32::consts::PI * 440.0 / rate as f32;
        (0..n).map(|i| (step * i as f32).sin() * 0.5).collect()
    }

    #[test]
    fn passthrough_is_identity() {
        let mut r = Resampler::new(48_000, 48_000).unwrap();
        let input = tone(48_000, 1000);
        let mut out = Vec::new();
        r.process(&input, &mut out).unwrap();
        assert_eq!(out, input);
    }

    #[test]
    fn upsample_preserves_rate_ratio() {
        let mut r = Resampler::new(44_100, 48_000).unwrap();
        let input = tone(44_100, 44_100); // 1 s
        let mut out = Vec::new();
        r.process(&input, &mut out).unwrap();
        // ~1 s at 48 kHz out; allow a chunk of slack for filter delay / buffering.
        let expected = 48_000i64;
        assert!(
            (out.len() as i64 - expected).abs() < 2 * CHUNK as i64,
            "got {} samples, expected ~{expected}",
            out.len()
        );
        assert!(out.iter().all(|s| s.is_finite()));
    }

    #[test]
    fn downsample_preserves_rate_ratio() {
        let mut r = Resampler::new(48_000, 44_100).unwrap();
        let input = tone(48_000, 48_000); // 1 s
        let mut out = Vec::new();
        r.process(&input, &mut out).unwrap();
        let expected = 44_100i64;
        assert!(
            (out.len() as i64 - expected).abs() < 2 * CHUNK as i64,
            "got {} samples, expected ~{expected}",
            out.len()
        );
        // A resampled tone is still real audio, not silence.
        let rms = (out.iter().map(|s| s * s).sum::<f32>() / out.len() as f32).sqrt();
        assert!(rms > 0.1, "resampled tone rms {rms} too low");
    }

    #[test]
    fn output_accumulates_across_small_pushes() {
        // Feeding many sub-chunk pushes yields the same total as one big push.
        let mut a = Resampler::new(44_100, 48_000).unwrap();
        let mut b = Resampler::new(44_100, 48_000).unwrap();
        let input = tone(44_100, 10_000);

        let mut out_a = Vec::new();
        a.process(&input, &mut out_a).unwrap();

        let mut out_b = Vec::new();
        for chunk in input.chunks(137) {
            b.process(chunk, &mut out_b).unwrap();
        }
        assert_eq!(out_a.len(), out_b.len());
        assert_eq!(out_a, out_b);
    }
}
