//! Braille plotting, btop-style: one sample per dot column (two per cell), newest
//! at the right edge. A sample keeps its shape as it ages left — nothing is
//! re-sampled — so the graph scrolls instead of reshaping.

use ratatui::buffer::Buffer;
use ratatui::layout::Rect;
use ratatui::style::Color;
use ratatui::symbols::braille;

use super::model::{HISTORY, SAMPLE};

/// A braille plotting surface over `area`: 2×4 dots per cell, each cell remembering
/// which series (bit flags) passed through it.
pub(super) struct BrailleGrid {
    area: Rect,
    bits: Vec<u16>,
    series: Vec<u8>,
}

impl BrailleGrid {
    pub(super) fn new(area: Rect) -> Self {
        let n = area.width as usize * area.height as usize;
        Self {
            area,
            bits: vec![0; n],
            series: vec![0; n],
        }
    }

    fn cols(&self) -> usize {
        self.area.width as usize * 2
    }

    fn rows(&self) -> usize {
        self.area.height as usize * 4
    }

    fn mark(&mut self, sx: usize, sy: usize, flag: u8) {
        let i = (sy / 4) * self.area.width as usize + sx / 2;
        self.bits[i] |= braille::DOTS[sy % 4][sx % 2];
        self.series[i] |= flag;
    }

    /// The sample shown in dot column `sx`: the newest sample sits in the last
    /// column; columns older than the series are empty.
    fn sample(&self, data: &[f64], sx: usize) -> Option<f64> {
        (data.len() + sx).checked_sub(self.cols()).map(|i| data[i])
    }

    fn norm(value: f64, max_y: f64) -> f64 {
        (value / max_y).clamp(0.0, 1.0)
    }

    fn drawable(&self, max_y: f64) -> bool {
        self.cols() > 0 && self.rows() > 0 && max_y > 0.0
    }

    /// A line through every dot column, vertically connected (gap-free), or a sparse
    /// dotted line when `dotted`.
    pub(super) fn line(&mut self, data: &[f64], max_y: f64, flag: u8, dotted: bool) {
        if !self.drawable(max_y) {
            return;
        }
        let rows = self.rows();
        let mut prev_y: Option<usize> = None;
        for sx in 0..self.cols() {
            let Some(value) = self.sample(data, sx) else {
                continue;
            };
            let y = (rows - 1) - (Self::norm(value, max_y) * (rows - 1) as f64).round() as usize;
            if dotted {
                if sx % 3 == 0 {
                    self.mark(sx, y, flag);
                }
                continue;
            }
            let (lo, hi) = prev_y.map_or((y, y), |p| (p.min(y), p.max(y)));
            for yy in lo..=hi {
                self.mark(sx, yy, flag);
            }
            prev_y = Some(y);
        }
    }

    /// A filled area from the bottom edge up, or from the top edge down when `down`.
    pub(super) fn area(&mut self, data: &[f64], max_y: f64, flag: u8, down: bool) {
        if !self.drawable(max_y) {
            return;
        }
        let rows = self.rows();
        for sx in 0..self.cols() {
            let Some(value) = self.sample(data, sx) else {
                continue;
            };
            let n = (Self::norm(value, max_y) * rows as f64).round() as usize;
            for k in 0..n {
                self.mark(sx, if down { k } else { rows - 1 - k }, flag);
            }
        }
    }

    /// The cell column showing sample `i` of a `len`-sample series, if it is visible.
    pub(super) fn cell_of(&self, i: usize, len: usize) -> Option<u16> {
        let sx = (i + self.cols()).checked_sub(len)?;
        Some(self.area.x + (sx / 2) as u16)
    }

    /// Draw the marked cells, each coloured by the series that passed through it.
    pub(super) fn render(&self, buf: &mut Buffer, color: impl Fn(u8) -> Color) {
        self.paint(buf, |i, _| color(self.series[i]));
    }

    /// Draw the marked cells coloured by distance from the baseline (btop's
    /// gradient): `colors[0]` near the baseline, the last colour near the scale top.
    /// The baseline is the bottom edge, or the top edge when `down`.
    pub(super) fn render_gradient(&self, buf: &mut Buffer, colors: &[Color], down: bool) {
        let h = f64::from(self.area.height.max(1));
        self.paint(buf, |_, cy| {
            let frac = if down {
                (cy as f64 + 0.5) / h
            } else {
                (h - cy as f64 - 0.5) / h
            };
            colors[((frac * colors.len() as f64) as usize).min(colors.len() - 1)]
        });
    }

    fn paint(&self, buf: &mut Buffer, color: impl Fn(usize, usize) -> Color) {
        let w = self.area.width as usize;
        for (i, &bits) in self.bits.iter().enumerate() {
            if bits == 0 {
                continue;
            }
            let (cx, cy) = (i % w, i / w);
            let pos = (self.area.x + cx as u16, self.area.y + cy as u16);
            if !buf.area.contains(pos.into()) {
                continue;
            }
            let ch = char::from_u32(u32::from(braille::BLANK | bits)).unwrap_or('\u{2800}');
            buf[pos].set_char(ch).set_fg(color(i, cy));
        }
    }
}

/// Auto-fit axis ceiling: the next step at or above `v` (never below 8).
pub(super) fn nice_max(v: f64) -> f64 {
    const STEPS: [f64; 16] = [
        8.0, 10.0, 12.0, 16.0, 20.0, 24.0, 32.0, 40.0, 48.0, 64.0, 80.0, 96.0, 128.0, 160.0, 200.0,
        256.0,
    ];
    STEPS
        .iter()
        .copied()
        .find(|&s| s >= v)
        .unwrap_or_else(|| (v / 50.0).ceil() * 50.0)
}

/// "45 s", "2 min", "2 min 30 s".
pub(super) fn fmt_span(secs: u64) -> String {
    if secs < 60 {
        return format!("{secs} s");
    }
    let (m, s) = (secs / 60, secs % 60);
    if s == 0 {
        format!("{m} min")
    } else {
        format!("{m} min {s} s")
    }
}

/// The time a graph `cells` wide shows: two samples per cell, capped by the history.
pub(super) fn visible_span(cells: u16) -> String {
    let samples = (cells as u64 * 2).min(HISTORY as u64);
    fmt_span(samples * SAMPLE.as_millis() as u64 / 1000)
}

/// The newest `dots` samples of a series (what a graph `dots` columns wide shows).
pub(super) fn visible(data: &[f64], dots: usize) -> &[f64] {
    &data[data.len().saturating_sub(dots)..]
}

/// Highest value in `data` (0 when empty).
pub(super) fn peak(data: &[f64]) -> f64 {
    data.iter().copied().fold(0.0, f64::max)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nice_max_rounds_up_to_a_step() {
        assert_eq!(nice_max(0.0), 8.0);
        assert_eq!(nice_max(30.0), 32.0);
        assert_eq!(nice_max(300.0), 300.0);
    }

    #[test]
    fn spans_read_naturally() {
        assert_eq!(fmt_span(38), "38 s");
        assert_eq!(fmt_span(120), "2 min");
        assert_eq!(fmt_span(195), "3 min 15 s");
        assert_eq!(visible_span(76), "1 min 16 s");
        assert_eq!(visible_span(5000), "8 min 32 s"); // capped by HISTORY
    }

    /// The column holding a spike, in a one-row grid 10 cells wide.
    fn spike_cell(data: &[f64]) -> (usize, u16) {
        let mut g = BrailleGrid::new(Rect::new(0, 0, 10, 1));
        g.area(data, 1.0, 1, false);
        let i = g.bits.iter().position(|&b| b != 0).expect("spike drawn");
        (i, g.bits[i])
    }

    /// btop scrolling: a sample stays put in its dot column and moves left exactly
    /// one dot per new sample — the graph is never re-sampled to fit.
    #[test]
    fn graph_scrolls_one_dot_per_sample() {
        let mut data = vec![0.0; 40];
        data.push(1.0); // newest → rightmost dot column (cell 9, right half)
        let (cell, bits) = spike_cell(&data);
        assert_eq!(cell, 9);
        assert_eq!(bits & braille::DOTS[3][0], 0, "right column only");

        data.push(0.0); // one older → cell 9, left half
        let (cell, bits) = spike_cell(&data);
        assert_eq!(cell, 9);
        assert_ne!(bits & braille::DOTS[3][0], 0);

        data.extend([0.0; 4]); // five older → cell 7, left half
        assert_eq!(spike_cell(&data).0, 7);
    }

    /// A short series fills from the right; the left stays empty.
    #[test]
    fn short_series_is_right_aligned() {
        let mut g = BrailleGrid::new(Rect::new(0, 0, 10, 1));
        g.area(&[1.0, 1.0], 1.0, 1, false);
        assert!(g.bits[..9].iter().all(|&b| b == 0));
        assert_ne!(g.bits[9], 0);
        assert_eq!(g.cell_of(0, 2), Some(9));
        assert_eq!(g.cell_of(0, 100), None);
    }
}
