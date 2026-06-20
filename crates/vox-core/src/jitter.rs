//! Adaptive jitter-buffer sizing (M10, DESIGN §3): measure network jitter and size
//! the buffer's operating band to it — shallow (low latency) on clean links, deeper
//! (more slack) on bursty ones — so normal arrival jitter stops tripping the recenter
//! drop/hold. Slow clock drift is out of scope here (see PLAN M10b); the band is
//! sized for jitter, and drift is rare enough that a shallow centre still drops
//! seldom.

use std::time::Instant;

use crate::audio::{FRAME, RATE};

/// RFC3550 smoothing weight for the jitter estimate.
const JITTER_GAIN: f64 = 1.0 / 16.0;

/// Adaptive band parameters (ms). The centre is the target depth; the half-band is
/// how far occupancy may swing from it before the recenter corrects. Both grow with
/// the smoothed jitter so the band fits the link. These are the empirical knobs.
const CENTRE_MIN_MS: f64 = 40.0;
const CENTRE_PER_JITTER: f64 = 3.0;
const HALF_BAND_MIN_MS: f64 = 45.0;
const HALF_BAND_PER_JITTER: f64 = 3.0;

/// Watermark glide weights (per packet): quick to *deepen* the band (absorb a jitter
/// burst before it underruns), slow to *shrink* it (don't collapse and start
/// clipping). Fast-attack / slow-release keeps the band from jiggling per packet.
const BAND_GROW: f64 = 0.10;
const BAND_SHRINK: f64 = 0.01;

/// Smoothed inter-arrival jitter (RFC3550), in seconds. Driven by packet arrival
/// times vs the spacing implied by the carried `timestamp` (sample count), so a
/// lost-packet gap is *expected* spacing, not jitter. Late/duplicate arrivals
/// (timestamp went backward) are ignored.
pub(crate) struct JitterEstimator {
    last: Option<(Instant, u32)>,
    jitter: f64,
}

impl JitterEstimator {
    pub(crate) fn new() -> Self {
        JitterEstimator {
            last: None,
            jitter: 0.0,
        }
    }

    /// Fold in an arrival; return the current smoothed jitter (seconds).
    pub(crate) fn update(&mut self, now: Instant, timestamp: u32) -> f64 {
        if let Some((last_now, last_ts)) = self.last {
            let delta_ts = timestamp.wrapping_sub(last_ts);
            // Skip a late/duplicate frame (timestamp at/behind the last): not jitter.
            if delta_ts == 0 || delta_ts >= u32::MAX / 2 {
                return self.jitter;
            }
            let expected = delta_ts as f64 / RATE as f64; // s, from the 48 kHz timestamp
            let actual = now.duration_since(last_now).as_secs_f64();
            let d = (actual - expected).abs();
            self.jitter += JITTER_GAIN * (d - self.jitter);
        }
        self.last = Some((now, timestamp));
        self.jitter
    }
}

/// The recenter watermarks (low, high) in ring samples for a given smoothed jitter:
/// hold below `low`, drop above `high`, free-run between. Centre and band both scale
/// with jitter; clamped to leave one frame of headroom from each ring rail.
pub(crate) fn adaptive_watermarks(
    jitter_secs: f64,
    playback_rate: u32,
    channels: usize,
    capacity: usize,
) -> (usize, usize) {
    let jitter_ms = jitter_secs * 1000.0;
    let centre_ms = CENTRE_MIN_MS + CENTRE_PER_JITTER * jitter_ms;
    let half_band_ms = HALF_BAND_MIN_MS + HALF_BAND_PER_JITTER * jitter_ms;
    let per_ms = playback_rate as f64 / 1000.0 * channels as f64;
    let frame = FRAME * channels; // one interleaved frame = the rail guard

    // Leave a frame at each rail and a frame of band: low ≤ cap−2f, high ≤ cap−f.
    let low_max = capacity.saturating_sub(2 * frame).max(frame);
    let low = (((centre_ms - half_band_ms).max(0.0) * per_ms) as usize).clamp(frame, low_max);
    let high_max = capacity.saturating_sub(frame).max(low + frame);
    let high = (((centre_ms + half_band_ms) * per_ms) as usize).clamp(low + frame, high_max);
    (low, high)
}

/// The buffer depth to prefill / start at (the zero-jitter centre), in ring samples.
pub(crate) fn initial_depth(playback_rate: u32, channels: usize, capacity: usize) -> usize {
    let (low, high) = adaptive_watermarks(0.0, playback_rate, channels, capacity);
    (low + high) / 2
}

/// Glide a watermark from `current` toward `target` (asymmetric EMA: quick to
/// deepen, slow to shrink), so the band moves smoothly across packets rather than
/// snapping to the noisy per-packet estimate.
pub(crate) fn ease(current: usize, target: usize) -> usize {
    let alpha = if target > current {
        BAND_GROW
    } else {
        BAND_SHRINK
    };
    (current as f64 + alpha * (target as f64 - current as f64)).round() as usize
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const MONO: usize = 1;

    #[test]
    fn steady_arrivals_have_zero_jitter() {
        let mut est = JitterEstimator::new();
        let mut now = Instant::now();
        let mut ts = 0u32;
        let mut j = 0.0;
        for _ in 0..50 {
            j = est.update(now, ts);
            now += Duration::from_millis(20); // exactly one frame apart
            ts = ts.wrapping_add(FRAME as u32);
        }
        assert!(
            j < 1e-6,
            "steady arrivals should have ~zero jitter, got {j}"
        );
    }

    #[test]
    fn uneven_arrivals_raise_jitter() {
        let mut est = JitterEstimator::new();
        let mut now = Instant::now();
        let mut ts = 0u32;
        let mut j = 0.0;
        for i in 0..50 {
            j = est.update(now, ts);
            // Alternate 5 ms / 35 ms spacing around the 20 ms nominal.
            now += Duration::from_millis(if i % 2 == 0 { 5 } else { 35 });
            ts = ts.wrapping_add(FRAME as u32);
        }
        assert!(j > 0.005, "uneven arrivals should raise jitter, got {j}");
    }

    #[test]
    fn late_arrival_is_ignored() {
        let mut est = JitterEstimator::new();
        let now = Instant::now();
        est.update(now, 10 * FRAME as u32);
        let before = est.update(now + Duration::from_millis(20), 11 * FRAME as u32);
        // A frame whose timestamp went backward (late/dup) must not change the estimate.
        let after = est.update(now + Duration::from_millis(40), 3 * FRAME as u32);
        assert_eq!(before, after);
    }

    #[test]
    fn watermarks_grow_with_jitter_and_stay_in_bounds() {
        let cap = 48_000; // 1 s mono ring
        let (lo0, hi0) = adaptive_watermarks(0.0, 48_000, MONO, cap);
        let (lo1, hi1) = adaptive_watermarks(0.020, 48_000, MONO, cap); // 20 ms jitter

        assert!(
            lo0 >= FRAME,
            "low floored at one frame (the underrun cushion)"
        );
        assert!(lo0 < hi0 && hi0 <= cap - FRAME, "ordered and within rails");
        // The band deepens via the high watermark; low stays at the underrun floor.
        assert!(hi1 > hi0 && lo1 >= lo0, "band deepens with jitter");
    }

    #[test]
    fn ease_attacks_fast_releases_slow() {
        let grow = ease(1000, 2000);
        let shrink = ease(2000, 1000);
        assert!(1000 < grow && grow < 2000 && 1000 < shrink && shrink < 2000);
        assert!(grow - 1000 > 2000 - shrink, "attack should outpace release");

        let mut x = 1000;
        for _ in 0..500 {
            x = ease(x, 1800);
        }
        // Converges to within the rounding stall (a few samples ≈ sub-0.1 ms).
        assert!((x as i64 - 1800).abs() <= 5, "easing converges, got {x}");
    }

    #[test]
    fn watermarks_clamp_to_a_small_ring() {
        let cap = 3 * FRAME; // tiny ring
        let (low, high) = adaptive_watermarks(0.050, 48_000, MONO, cap);
        assert!(low >= FRAME && high <= cap - FRAME && low < high);
    }
}
