//! What the dashboard knows: sampled rates, the scrolling history (one sample per
//! braille dot column), last-30 s deltas, the reset baseline, and the health
//! verdict. Pure over [`EngineStats`]; never touches the engine itself.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use ratatui::style::Color;
use vox_core::EngineStats;

use super::options::Settings;
use crate::{kbps, SessionInfo};

/// Stats sampling cadence (also the rate-averaging window). One sample per braille
/// dot column, so every graph scrolls by one dot per `SAMPLE`.
pub(super) const SAMPLE: Duration = Duration::from_millis(500);
/// Samples kept per series: enough to fill a graph 512 cells wide (~8.5 min), wider
/// than any realistic terminal, so the graph always spans its full width.
pub(super) const HISTORY: usize = 1024;
/// Samples spanned by the "last 30 s" numbers (RECENT × SAMPLE = 30 s).
pub(super) const RECENT: usize = 60;
/// Samples the drift trend is measured over (2 min), independent of graph width.
const DRIFT_WINDOW: usize = 240;

/// Per-sample event bits for the depth graph's markers.
pub(super) const EV_DROP: u8 = 1;
pub(super) const EV_HOLD: u8 = 2;
pub(super) const EV_UNDERRUN: u8 = 4;

#[derive(Default, Clone, Copy)]
pub(super) struct Rates {
    pub(super) tx_kbps: f64,
    pub(super) rx_kbps: f64,
    pub(super) tx_pps: f64,
    pub(super) rx_pps: f64,
}

/// Per-sample series, all aligned and newest last. They grow to [`HISTORY`] with no
/// leading padding, so a graph only draws samples that exist.
#[derive(Clone)]
pub(super) struct History {
    pub(super) tx: Vec<f64>,
    pub(super) rx: Vec<f64>,
    /// Buffered latency and adaptive target, in ms.
    pub(super) depth: Vec<f64>,
    pub(super) target: Vec<f64>,
    /// Recenter / underrun events per sample.
    pub(super) events: Vec<u8>,
    /// Stats snapshots for the last-30 s deltas (front = ~30 s ago).
    snapshots: VecDeque<EngineStats>,
}

impl History {
    pub(super) fn new(first: EngineStats) -> Self {
        let mut snapshots = VecDeque::with_capacity(RECENT + 1);
        snapshots.push_back(first);
        Self {
            tx: Vec::with_capacity(HISTORY + 1),
            rx: Vec::with_capacity(HISTORY + 1),
            depth: Vec::with_capacity(HISTORY + 1),
            target: Vec::with_capacity(HISTORY + 1),
            events: Vec::with_capacity(HISTORY + 1),
            snapshots,
        }
    }

    pub(super) fn record(&mut self, prev: &EngineStats, now: &EngineStats, rate: &Rates) {
        let mut ev = 0;
        if now.recenter_drops > prev.recenter_drops {
            ev |= EV_DROP;
        }
        if now.recenter_inserts > prev.recenter_inserts {
            ev |= EV_HOLD;
        }
        if now.underruns > prev.underruns {
            ev |= EV_UNDERRUN;
        }
        push_capped(&mut self.tx, rate.tx_kbps);
        push_capped(&mut self.rx, rate.rx_kbps);
        push_capped(&mut self.depth, now.jitter_fill_ms as f64);
        push_capped(&mut self.target, now.target_depth_ms as f64);
        push_capped(&mut self.events, ev);
        self.snapshots.push_back(*now);
        if self.snapshots.len() > RECENT + 1 {
            self.snapshots.pop_front();
        }
    }

    /// Buffer drift over the last 2 min, in ms/min.
    pub(super) fn drift(&self) -> f64 {
        drift_per_min(&self.depth[self.depth.len().saturating_sub(DRIFT_WINDOW)..])
    }

    /// The stats ~30 s ago (the start of the "last 30 s" window).
    fn window_start(&self) -> EngineStats {
        self.snapshots.front().copied().unwrap_or_default()
    }
}

fn push_capped<T>(series: &mut Vec<T>, value: T) {
    series.push(value);
    if series.len() > HISTORY {
        series.remove(0);
    }
}

/// Trend (slope) of the buffered-latency history in ms/min — positive means the
/// jitter buffer is slowly filling, negative draining: the signature of clock drift,
/// which a steady recenter count alone wouldn't reveal until it hits a rail.
pub(super) fn drift_per_min(history: &[f64]) -> f64 {
    let n = history.len();
    if n < 20 {
        return 0.0; // ~10 s minimum before estimating
    }
    let half = n / 2;
    let mean = |s: &[f64]| s.iter().sum::<f64>() / s.len() as f64;
    // Rise between the two halves' centroids (n/2 samples × 0.5 s apart) → per minute.
    (mean(&history[half..]) - mean(&history[..half])) * 240.0 / n as f64
}

/// The totals when the user pressed `r`, and the uptime then. Subtracted in the TUI
/// only; the engine's counters are never reset.
#[derive(Clone, Copy)]
pub(super) struct Baseline {
    pub(super) stats: EngineStats,
    pub(super) at: Duration,
}

/// Turns successive engine snapshots into rates and history. Keeps sampling while
/// the screen is paused, so un-pausing jumps straight back to live.
pub(super) struct Sampler {
    last: EngineStats,
    last_at: Instant,
    rate: Rates,
    history: History,
    drift: f64,
}

impl Sampler {
    pub(super) fn new(first: EngineStats) -> Self {
        Self {
            last: first,
            last_at: Instant::now(),
            rate: Rates::default(),
            history: History::new(first),
            drift: 0.0,
        }
    }

    pub(super) fn due(&self) -> bool {
        self.last_at.elapsed() >= SAMPLE
    }

    pub(super) fn sample(&mut self, now: EngineStats) {
        let secs = self.last_at.elapsed().as_secs_f64();
        let last = &self.last;
        let per_sec = |n: u64| if secs > 0.0 { n as f64 / secs } else { 0.0 };
        self.rate = Rates {
            tx_kbps: kbps(now.bytes_sent.saturating_sub(last.bytes_sent), secs),
            rx_kbps: kbps(now.bytes_received.saturating_sub(last.bytes_received), secs),
            tx_pps: per_sec(now.packets_sent.saturating_sub(last.packets_sent)),
            rx_pps: per_sec(now.packets_received.saturating_sub(last.packets_received)),
        };
        self.history.record(&self.last, &now, &self.rate);
        self.drift = self.history.drift();
        self.last = now;
        self.last_at = Instant::now();
    }

    /// The live picture, with `stats` read fresh for this frame.
    pub(super) fn snapshot(&self, stats: EngineStats, uptime: Duration) -> Snapshot<'_> {
        Snapshot {
            stats,
            rate: self.rate,
            drift: self.drift,
            uptime,
            history: &self.history,
        }
    }

    /// An owned copy of the live picture, for `p` (pause).
    pub(super) fn freeze(&self, stats: EngineStats, uptime: Duration) -> Frozen {
        Frozen {
            stats,
            rate: self.rate,
            drift: self.drift,
            uptime,
            history: self.history.clone(),
        }
    }
}

/// One frame's raw inputs, live or frozen.
pub(super) struct Snapshot<'a> {
    pub(super) stats: EngineStats,
    pub(super) rate: Rates,
    pub(super) drift: f64,
    pub(super) uptime: Duration,
    pub(super) history: &'a History,
}

/// A paused screen: the picture at the moment `p` was pressed.
pub(super) struct Frozen {
    stats: EngineStats,
    rate: Rates,
    drift: f64,
    uptime: Duration,
    history: History,
}

impl Frozen {
    pub(super) fn snapshot(&self) -> Snapshot<'_> {
        Snapshot {
            stats: self.stats,
            rate: self.rate,
            drift: self.drift,
            uptime: self.uptime,
            history: &self.history,
        }
    }
}

/// One frame's display inputs, with the reset baseline applied.
pub(super) struct View<'a> {
    pub(super) info: &'a SessionInfo,
    /// The settings as this session last set them (Options overlay).
    pub(super) settings: Settings,
    /// Counters since start (or since `r`); gauges are live.
    pub(super) stats: EngineStats,
    /// Counter deltas over the last 30 s (never reaching back past a reset).
    pub(super) recent: EngineStats,
    pub(super) rate: Rates,
    pub(super) drift: f64,
    pub(super) uptime: Duration,
    pub(super) history: &'a History,
    pub(super) paused: bool,
    /// How long ago the totals were reset, if they were.
    pub(super) reset_ago: Option<Duration>,
}

impl<'a> View<'a> {
    pub(super) fn new(
        info: &'a SessionInfo,
        snap: Snapshot<'a>,
        settings: Settings,
        base: Option<&Baseline>,
        paused: bool,
    ) -> Self {
        let floor = base.map_or_else(EngineStats::default, |b| b.stats);
        let window = zip_counters(&snap.history.window_start(), &floor, u64::max);
        View {
            info,
            settings,
            stats: zip_counters(&snap.stats, &floor, u64::saturating_sub),
            recent: zip_counters(&snap.stats, &window, u64::saturating_sub),
            rate: snap.rate,
            drift: snap.drift,
            uptime: snap.uptime,
            history: snap.history,
            paused,
            reset_ago: base.map(|b| snap.uptime.saturating_sub(b.at)),
        }
    }

    /// Whether this instance plays back (and so has a jitter buffer).
    pub(super) fn receives(&self) -> bool {
        self.stats.jitter_capacity > 0
    }

    pub(super) fn loss(&self) -> f64 {
        loss_pct(self.stats.gap_frames, self.stats.packets_received)
    }

    pub(super) fn recent_loss(&self) -> f64 {
        loss_pct(self.recent.gap_frames, self.recent.packets_received)
    }

    pub(super) fn fill_ratio(&self) -> f64 {
        if self.receives() {
            (self.stats.jitter_fill as f64 / self.stats.jitter_capacity as f64).clamp(0.0, 1.0)
        } else {
            0.0
        }
    }
}

/// Combine the cumulative counters of `a` and `b` field by field; the live gauges
/// (buffer fill, capacity, target) come from `a` unchanged.
fn zip_counters(a: &EngineStats, b: &EngineStats, f: impl Fn(u64, u64) -> u64) -> EngineStats {
    EngineStats {
        packets_sent: f(a.packets_sent, b.packets_sent),
        bytes_sent: f(a.bytes_sent, b.bytes_sent),
        packets_received: f(a.packets_received, b.packets_received),
        bytes_received: f(a.bytes_received, b.bytes_received),
        gap_frames: f(a.gap_frames, b.gap_frames),
        dropped_late: f(a.dropped_late, b.dropped_late),
        overruns: f(a.overruns, b.overruns),
        recenter_drops: f(a.recenter_drops, b.recenter_drops),
        recenter_inserts: f(a.recenter_inserts, b.recenter_inserts),
        underruns: f(a.underruns, b.underruns),
        capture_overruns: f(a.capture_overruns, b.capture_overruns),
        ..*a
    }
}

fn loss_pct(gaps: u64, received: u64) -> f64 {
    let expected = received + gaps;
    if expected > 0 {
        gaps as f64 / expected as f64 * 100.0
    } else {
        0.0
    }
}

/// Plain-language verdict for the header pill, most severe first.
pub(super) struct Health {
    pub(super) color: Color,
    pub(super) text: String,
}

pub(super) fn health(v: &View) -> Health {
    let bad = |text: String| Health {
        color: Color::Red,
        text: format!("✕ {text}"),
    };
    let warn = |text: String| Health {
        color: Color::Yellow,
        text: format!("▲ {text}"),
    };
    let recent_loss = v.recent_loss();
    let underruns = v.recent.underruns;
    if v.receives() && v.rate.rx_pps == 0.0 {
        return bad("no audio from peer".into());
    }
    if underruns > 0 {
        let s = if underruns == 1 { "" } else { "s" };
        return bad(format!("{underruns} underrun{s} in 30 s"));
    }
    if recent_loss >= 5.0 {
        return bad(format!("loss {recent_loss:.1}%"));
    }
    if recent_loss > 0.0 {
        return warn(format!("loss {recent_loss:.1}%"));
    }
    if v.drift.abs() >= 2.0 {
        return warn(format!("drift {:+} ms/min", v.drift.round() as i64));
    }
    if v.receives() && v.fill_ratio() > 0.9 {
        return warn("buffer near full".into());
    }
    let ok = if v.receives() { "ok" } else { "sending" };
    Health {
        color: Color::Green,
        text: format!("● {ok}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats(gaps: u64, received: u64) -> EngineStats {
        EngineStats {
            gap_frames: gaps,
            packets_received: received,
            ..EngineStats::default()
        }
    }

    #[test]
    fn recent_deltas_span_the_window() {
        let mut h = History::new(EngineStats::default());
        let mut s = EngineStats::default();
        for _ in 0..(RECENT + 10) {
            let prev = s;
            s.gap_frames += 1;
            s.recenter_drops += 1;
            h.record(&prev, &s, &Rates::default());
        }
        let recent = zip_counters(&s, &h.window_start(), u64::saturating_sub);
        assert_eq!(recent.gap_frames, RECENT as u64);
        assert_eq!(h.events.last().copied(), Some(EV_DROP));
    }

    #[test]
    fn history_is_capped_and_aligned() {
        let mut h = History::new(EngineStats::default());
        let s = EngineStats::default();
        for i in 0..(HISTORY + 50) {
            h.record(
                &s,
                &s,
                &Rates {
                    tx_kbps: i as f64,
                    ..Rates::default()
                },
            );
        }
        assert_eq!(h.tx.len(), HISTORY);
        assert_eq!(h.depth.len(), HISTORY);
        assert_eq!(h.events.len(), HISTORY);
        assert_eq!(h.tx.last().copied(), Some((HISTORY + 49) as f64));
    }

    /// `r`: totals restart from zero, and the last-30 s numbers never reach back
    /// past the reset even while the window still holds older snapshots.
    #[test]
    fn baseline_resets_totals_and_clips_the_window() {
        let info = crate::tui::tests::info("full duplex");
        let mut h = History::new(stats(0, 0));
        let mut s = stats(0, 0);
        for _ in 0..10 {
            let prev = s;
            s.gap_frames += 2;
            s.packets_received += 25;
            h.record(&prev, &s, &Rates::default());
        }
        let base = Baseline {
            stats: s,
            at: Duration::from_secs(5),
        };
        s.gap_frames += 1;
        s.packets_received += 24;
        let snap = Snapshot {
            stats: s,
            rate: Rates::default(),
            drift: 0.0,
            uptime: Duration::from_secs(9),
            history: &h,
        };
        let settings = Settings::from_info(&info);
        let v = View::new(&info, snap, settings, Some(&base), false);
        assert_eq!(v.stats.gap_frames, 1);
        assert_eq!(v.stats.packets_received, 24);
        assert_eq!(v.recent.gap_frames, 1);
        assert_eq!(v.reset_ago, Some(Duration::from_secs(4)));
        assert!((v.loss() - 4.0).abs() < 1e-9);
    }

    #[test]
    fn drift_needs_ten_seconds_and_tracks_slope() {
        assert_eq!(drift_per_min(&[100.0; 10]), 0.0);
        // +0.15 ms per 0.5 s sample = +18 ms/min.
        let ramp: Vec<f64> = (0..240).map(|i| 100.0 + 0.15 * i as f64).collect();
        assert!((drift_per_min(&ramp) - 18.0).abs() < 0.1);
    }
}
