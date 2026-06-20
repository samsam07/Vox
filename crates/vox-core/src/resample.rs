//! Sample-rate conversion between a device rate and the 48 kHz wire rate (DESIGN
//! §4). Mono only: resampling sits at the device edge — on the send path between
//! downmix and encode (capture rate → 48 kHz), on the receive path between decode
//! and upmix (48 kHz → playback rate). The codec and wire stay 48 kHz mono.
//!
//! A device already at 48 kHz uses [`Resampler::Passthrough`] — no rubato, the
//! 48 kHz path is unchanged. The opt-in [`DriftResampler`] (M10b, `--drift-correct`)
//! is the dynamic-ratio variant used for smooth clock-drift correction.

use anyhow::{anyhow, Result};
use rubato::{
    FftFixedIn, Resampler as _, SincFixedIn, SincInterpolationParameters, SincInterpolationType,
    WindowFunction,
};

/// Fixed input chunk fed to rubato per FFT (~21 ms at 48 kHz) — a latency/overhead
/// balance; non-48 kHz only, so it never touches the common path.
const CHUNK: usize = 1024;

/// Input chunk for the drift resampler (~5 ms at 48 kHz). Smaller than [`CHUNK`]
/// because it's on the receive path whenever drift correction is on.
const DRIFT_CHUNK: usize = 256;

/// Bound on the drift trim (± relative ratio). Clock drift is tens of ppm, so this is
/// ample authority while keeping any transient pitch shift inaudible (~8 cents).
const MAX_TRIM: f64 = 0.005;

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

/// A receive-side resampler whose output rate can be nudged for smooth clock-drift
/// correction (M10b, opt-in via `--drift-correct`). Always active when used — even at
/// 48 kHz, ratio ~1.0 — because drift exists between two nominally-equal but
/// physically different clocks, and only an interpolating resampler corrects it
/// smoothly (vs the coarse recenter drop/hold).
pub(crate) struct DriftResampler {
    // Boxed: `SincFixedIn` is large; off the audio callback, so the indirection is free.
    inner: Box<SincFixedIn<f32>>,
    pending: Vec<f32>,
}

impl DriftResampler {
    /// An always-active sinc resampler from `in_rate` to `out_rate`.
    pub(crate) fn new(in_rate: u32, out_rate: u32) -> Result<Self> {
        let params = SincInterpolationParameters {
            sinc_len: 128,
            f_cutoff: 0.95,
            oversampling_factor: 256,
            interpolation: SincInterpolationType::Linear,
            window: WindowFunction::BlackmanHarris2,
        };
        let ratio = out_rate as f64 / in_rate as f64;
        // rubato's relative-ratio bound is [1/max, max], so a touch wider than MAX_TRIM
        // is needed for `1 - MAX_TRIM` to be in range (1/1.005 > 0.995).
        let max_relative = 1.0 + 2.0 * MAX_TRIM;
        let inner = SincFixedIn::<f32>::new(ratio, max_relative, params, DRIFT_CHUNK, 1)
            .map_err(|e| anyhow!("create drift resampler {in_rate}->{out_rate} Hz: {e}"))?;
        Ok(DriftResampler {
            inner: Box::new(inner),
            pending: Vec::with_capacity(DRIFT_CHUNK * 2),
        })
    }

    /// Trim the output rate to `1.0 + trim` of nominal (clamped to ±[`MAX_TRIM`]).
    /// `trim < 0` produces fewer samples (drains a high buffer); `trim > 0` more.
    pub(crate) fn set_trim(&mut self, trim: f64) -> Result<()> {
        let rel = 1.0 + trim.clamp(-MAX_TRIM, MAX_TRIM);
        self.inner
            .set_resample_ratio_relative(rel, true)
            .map_err(|e| anyhow!("set drift ratio: {e}"))?;
        Ok(())
    }

    /// Push mono `input`, appending resampled output to `out` (see [`Resampler::process`]).
    pub(crate) fn process(&mut self, input: &[f32], out: &mut Vec<f32>) -> Result<()> {
        self.pending.extend_from_slice(input);
        while self.pending.len() >= self.inner.input_frames_next() {
            let need = self.inner.input_frames_next();
            let resampled = self
                .inner
                .process(&[&self.pending[..need]], None)
                .map_err(|e| anyhow!("drift resample: {e}"))?;
            out.extend_from_slice(&resampled[0]);
            self.pending.drain(..need);
        }
        Ok(())
    }
}

/// The receive-path resampler: the static rate converter (default — passthrough at
/// 48 kHz, FFT for non-48 kHz), or the dynamic-ratio drift resampler when
/// `--drift-correct` is on (M10b). One seam so the receiver doesn't branch per frame.
pub(crate) enum ReceiveResampler {
    Static(Resampler),
    Drift(DriftResampler),
}

impl ReceiveResampler {
    pub(crate) fn new(in_rate: u32, out_rate: u32, drift_correct: bool) -> Result<Self> {
        Ok(if drift_correct {
            ReceiveResampler::Drift(DriftResampler::new(in_rate, out_rate)?)
        } else {
            ReceiveResampler::Static(Resampler::new(in_rate, out_rate)?)
        })
    }

    pub(crate) fn process(&mut self, input: &[f32], out: &mut Vec<f32>) -> Result<()> {
        match self {
            ReceiveResampler::Static(r) => r.process(input, out),
            ReceiveResampler::Drift(r) => r.process(input, out),
        }
    }

    /// Apply a drift trim (no-op for the static resampler).
    pub(crate) fn set_trim(&mut self, trim: f64) -> Result<()> {
        match self {
            ReceiveResampler::Static(_) => Ok(()),
            ReceiveResampler::Drift(r) => r.set_trim(trim),
        }
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

    /// The drift resampler at nominal ratio (no trim) is near rate-preserving and
    /// outputs real audio.
    #[test]
    fn drift_resampler_unity_is_near_identity() {
        let mut r = DriftResampler::new(48_000, 48_000).unwrap();
        let input = tone(48_000, 48_000); // 1 s
        let mut out = Vec::new();
        r.process(&input, &mut out).unwrap();
        assert!(
            (out.len() as i64 - 48_000).abs() < 2 * DRIFT_CHUNK as i64,
            "got {} samples, expected ~48000",
            out.len()
        );
        assert!(out.iter().all(|s| s.is_finite()));
        let rms = (out.iter().map(|s| s * s).sum::<f32>() / out.len() as f32).sqrt();
        assert!(rms > 0.1, "drift-resampled tone rms {rms} too low");
    }

    /// A positive trim slows output (more samples), negative speeds it (fewer) — the
    /// lever the drift controller pulls to refill / drain the buffer.
    #[test]
    fn trim_sign_changes_output_count() {
        let input = tone(48_000, 48_000);

        let mut up = DriftResampler::new(48_000, 48_000).unwrap();
        up.set_trim(MAX_TRIM).unwrap();
        let mut out_up = Vec::new();
        up.process(&input, &mut out_up).unwrap();

        let mut down = DriftResampler::new(48_000, 48_000).unwrap();
        down.set_trim(-MAX_TRIM).unwrap();
        let mut out_down = Vec::new();
        down.process(&input, &mut out_down).unwrap();

        assert!(
            out_up.len() > out_down.len(),
            "trim up {} should exceed trim down {}",
            out_up.len(),
            out_down.len()
        );
    }
}
