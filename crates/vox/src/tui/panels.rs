//! The Pipeline panels (PLAN M16), in the order the audio flows: header, Status,
//! Throughput (+ live box), Jitter buffer, Quality tiles, footer. Ported from the
//! workbench artifact; each function notes the JS function it mirrors.

use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Flex, Layout, Rect};
use ratatui::style::{Color, Modifier, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Padding, Paragraph, Widget};
use ratatui::Frame;

use super::model::{health, View, EV_DROP, EV_HOLD, EV_UNDERRUN, SAMPLE};
use super::plot::{fmt_span, nice_max, peak, visible, visible_span, BrailleGrid};

pub(super) const ACCENT: Color = Color::Cyan;
pub(super) const DIM: Color = Color::DarkGray;
const TX_COLOR: Color = Color::Green;
const RX_COLOR: Color = Color::Cyan;
const DEPTH_COLOR: Color = Color::LightBlue;
/// btop gradients, baseline → scale top.
const TX_GRAD: [Color; 3] = [Color::Green, Color::LightGreen, Color::LightYellow];
const RX_GRAD: [Color; 3] = [Color::Cyan, Color::LightCyan, Color::LightBlue];
/// Quality severity: none / concealed / audible.
const SEV_COLOR: [Color; 3] = [Color::Green, Color::Yellow, Color::Red];
const SEV_SYMBOL: [&str; 3] = ["●", "▲", "✕"];

/// Throughput needs this much inner width for the live numbers beside the graph.
const LIVE_MIN_WIDTH: u16 = 60;
/// Width of the boxed live inset, and of the bare column when too short for a box.
const LIVE_BOX_WIDTH: u16 = 26;
const LIVE_BARE_WIDTH: u16 = 24;

// ---------- shared helpers ----------

/// A rounded panel with a dim border and an accent title (`box` in the JS).
pub(super) fn panel(title: &str) -> Block<'static> {
    let block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(DIM))
        .padding(Padding::horizontal(1));
    if title.is_empty() {
        block
    } else {
        block.title(Line::from(format!(" {title} ").fg(ACCENT).bold()))
    }
}

/// Draw `line` at (x, y), clipped to `clip` (and to the buffer).
pub(super) fn put(buf: &mut Buffer, x: u16, y: u16, line: &Line, clip: Rect) {
    let clip = clip.intersection(buf.area);
    if y < clip.y || y >= clip.bottom() || x < clip.x || x >= clip.right() {
        return;
    }
    buf.set_line(x, y, line, clip.right() - x);
}

/// Left and right spans, padded apart to `w` (at least one space between).
fn lr(left: Vec<Span<'static>>, right: Vec<Span<'static>>, w: u16) -> Line<'static> {
    let used: usize = left.iter().chain(&right).map(Span::width).sum();
    let gap = (w as usize).saturating_sub(used).max(1);
    let mut spans = left;
    spans.push(" ".repeat(gap).into());
    spans.extend(right);
    Line::from(spans)
}

fn kv(label: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        format!("{label:<9}").fg(DIM),
        value.to_string().into(),
    ])
}

pub(super) fn on_off(enabled: bool) -> &'static str {
    if enabled {
        "on"
    } else {
        "off"
    }
}

/// Human-readable byte count (B / KB / MB / GB).
fn human_bytes(bytes: u64) -> String {
    const K: f64 = 1024.0;
    let b = bytes as f64;
    if b < K {
        format!("{bytes} B")
    } else if b < K * K {
        format!("{:.1} KB", b / K)
    } else if b < K * K * K {
        format!("{:.1} MB", b / (K * K))
    } else {
        format!("{:.1} GB", b / (K * K * K))
    }
}

fn format_duration(secs: u64) -> String {
    format!(
        "{:02}:{:02}:{:02}",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}

/// Bold default foreground for totals, not `Color::White`: bright white vanishes on
/// light terminal palettes (it is the background there).
fn strong(s: String) -> Span<'static> {
    s.bold()
}

// ---------- header + footer ----------

/// `header` + `healthPill` + `pausedChip`.
pub(super) fn header(frame: &mut Frame, area: Rect, v: &View) {
    let block = panel("");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let title = Line::from(vec![
        format!("vox {}", env!("CARGO_PKG_VERSION"))
            .fg(ACCENT)
            .bold(),
        "  —  ".fg(DIM),
        v.info.mode.bold(),
    ]);
    frame.render_widget(Paragraph::new(title).centered(), inner);
    let h = health(v);
    let pill = Line::from(format!(" {} ", h.text).fg(Color::Black).bg(h.color).bold());
    frame.render_widget(Paragraph::new(pill).right_aligned(), inner);
    if v.paused {
        let chip = Line::from(
            " ⏸ PAUSED · p resume "
                .fg(Color::Black)
                .bg(Color::Yellow)
                .bold(),
        );
        frame.render_widget(Paragraph::new(chip), inner);
    }
}

/// `footer` (key chips).
pub(super) fn footer(frame: &mut Frame, area: Rect, v: &View) {
    let chip = |k: &str| format!(" {k} ").fg(Color::Black).bg(DIM).bold();
    let mut spans = vec![chip("q"), " ".into(), chip("Esc"), " quit".fg(DIM)];
    let pause = if v.paused { "resume" } else { "pause" };
    for (key, label) in [
        ("p", pause),
        ("r", "reset"),
        ("o", "options"),
        ("?", "help"),
    ] {
        spans.extend(["   ".into(), chip(key), format!(" {label}").fg(DIM)]);
    }
    let left = Line::from(spans);
    let left_w = left.width();
    frame.render_widget(Paragraph::new(left), area);
    let right =
        Line::from(format!("sample {:.1} s · 2 samples per cell ", SAMPLE.as_secs_f64()).fg(DIM));
    // Only when it fits beside the chips (never drawn over them).
    if left_w + 1 + right.width() <= area.width as usize {
        frame.render_widget(Paragraph::new(right).right_aligned(), area);
    }
}

// ---------- Status ----------

/// `statusSession`: session details only, two columns over 3 rows when wide.
pub(super) fn status(frame: &mut Frame, area: Rect, v: &View) {
    let block = panel("Status");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let i = v.info;
    let peer = i.peer.map_or_else(|| "—".to_string(), |p| p.to_string());
    let uptime = format_duration(v.uptime.as_secs());
    let right_w = if inner.width >= 90 { 30 } else { 0 };
    let [left, right] =
        Layout::horizontal([Constraint::Fill(1), Constraint::Length(right_w)]).areas(inner);
    if right_w > 0 {
        let left = Rect {
            width: left.width.saturating_sub(2),
            ..left
        };
        let lines = vec![
            kv("capture", &i.capture),
            kv("playback", &i.playback),
            kv("codec", &codec_text(v)),
        ];
        frame.render_widget(Paragraph::new(lines), left);
        let lines = vec![
            kv("peer", &peer),
            kv("bind", &i.bind.to_string()),
            kv("uptime", &uptime),
        ];
        frame.render_widget(Paragraph::new(lines), right);
    } else {
        let lines = vec![
            kv("capture", &i.capture),
            kv("playback", &i.playback),
            Line::from(vec![
                format!("{:<9}", "peer").fg(DIM),
                peer.into(),
                "  up ".fg(DIM),
                uptime.into(),
            ]),
        ];
        frame.render_widget(Paragraph::new(lines), inner);
    }
}

/// The codec settings line, in plain words.
pub(super) fn codec_text(v: &View) -> String {
    let i = &v.settings;
    format!(
        "{} bps   fec {}   dtx {}   ceiling {} ms   drift correction {}",
        i.bitrate,
        on_off(i.fec),
        on_off(i.dtx),
        i.jitter_ms,
        on_off(i.drift_correct)
    )
}

// ---------- Throughput ----------

/// Where the live tx / rx / total numbers go, by how much room Throughput has.
enum Live {
    /// A boxed inset, centred on the right of the graph.
    Boxed(Rect),
    /// A bare column behind a divider (too short for the box).
    Bare(Rect),
    /// Three text rows above the graph (too narrow for an inset).
    Rows(Rect),
}

/// `throughputPanel` (btop mode): tx grows up, rx grows down, coloured by height.
pub(super) fn throughput(frame: &mut Frame, area: Rect, v: &View) {
    let block = panel("Throughput");
    let inner = block.inner(area);
    let (plot, live) = if inner.width >= LIVE_MIN_WIDTH && inner.height >= 5 {
        let [p, l] = Layout::horizontal([Constraint::Fill(1), Constraint::Length(LIVE_BOX_WIDTH)])
            .areas(inner);
        (shrink_w(p, 1), Live::Boxed(l))
    } else if inner.width >= LIVE_MIN_WIDTH {
        let [p, l] = Layout::horizontal([Constraint::Fill(1), Constraint::Length(LIVE_BARE_WIDTH)])
            .areas(inner);
        (shrink_w(p, 2), Live::Bare(l))
    } else {
        let [t, p] = Layout::vertical([Constraint::Length(3), Constraint::Fill(1)]).areas(inner);
        (p, Live::Rows(t))
    };

    let h = v.history;
    let dots = plot.width as usize * 2;
    let (tx_peak, rx_peak) = (peak(visible(&h.tx, dots)), peak(visible(&h.rx, dots)));
    let max_y = nice_max((tx_peak.max(rx_peak) * 1.15).max(6.0));
    let legend = Line::from(vec![
        " ▲ tx ".fg(TX_COLOR).bold(),
        "▼ rx ".fg(RX_COLOR).bold(),
        format!("· kbps · {} ", visible_span(plot.width)).fg(DIM),
    ])
    .right_aligned();
    frame.render_widget(block.title_top(legend), area);

    let buf = frame.buffer_mut();
    match live {
        Live::Boxed(r) => live_box(buf, r, v, tx_peak, rx_peak),
        Live::Bare(r) => live_bare(buf, r, v),
        Live::Rows(r) => live_rows(buf, r, v),
    }
    mirrored_graph(buf, plot, v, max_y);
}

fn shrink_w(r: Rect, by: u16) -> Rect {
    Rect {
        width: r.width.saturating_sub(by),
        ..r
    }
}

/// tx area above a dim divider row, rx area below, on one shared scale.
fn mirrored_graph(buf: &mut Buffer, plot: Rect, v: &View, max_y: f64) {
    if plot.width == 0 || plot.height == 0 {
        return;
    }
    let divider = u16::from(plot.height >= 3);
    let half = (plot.height - divider) / 2;
    let [up, div, down] = Layout::vertical([
        Constraint::Length(half),
        Constraint::Length(divider),
        Constraint::Fill(1),
    ])
    .areas(plot);
    let mut g = BrailleGrid::new(up);
    g.area(&v.history.tx, max_y, 1, false);
    g.render_gradient(buf, &TX_GRAD, false);
    let mut g = BrailleGrid::new(down);
    g.area(&v.history.rx, max_y, 1, true);
    g.render_gradient(buf, &RX_GRAD, true);
    if divider > 0 {
        let rule = Line::from("─".repeat(div.width as usize).fg(DIM));
        put(buf, div.x, div.y, &rule, div);
    }
    // y-scale at the outer edges, like btop's "40K".
    let scale = Line::from(format!("{max_y:.0}").fg(DIM));
    put(buf, up.x, up.y, &scale, up);
    if down.height > 0 {
        put(buf, down.x, down.bottom() - 1, &scale, down);
    }
}

/// One direction's rows in the live box: rate, then packets + volume, then peak.
fn live_direction(
    head: &str,
    color: Color,
    (kbps, pps, bytes, peak): (f64, f64, u64, f64),
    rows: u16,
    w: u16,
) -> Vec<Line<'static>> {
    let mut out = vec![lr(
        vec![head.to_string().fg(color).bold()],
        vec![format!("{kbps:>3.0} kbps").fg(color).bold()],
        w,
    )];
    if rows >= 2 {
        out.push(lr(
            vec![format!("  {pps:>3.0} pkt/s").fg(DIM)],
            vec![human_bytes(bytes).fg(color)],
            w,
        ));
    }
    if rows >= 3 {
        out.push(lr(
            vec!["  peak".fg(DIM)],
            vec![format!("{peak:>3.0} kbps").fg(DIM)],
            w,
        ));
    }
    out
}

/// `liveBox`: the live numbers in their own box, centred on the right of the graph.
/// 10 rows tall when there is room (with peaks), 8 without peaks, 5 bare rates.
fn live_box(buf: &mut Buffer, r: Rect, v: &View, tx_peak: f64, rx_peak: f64) {
    let (box_h, rows) = match r.height {
        h if h >= 10 => (10, 3),
        h if h >= 8 => (8, 2),
        _ => (5, 1),
    };
    let [b] = Layout::vertical([Constraint::Length(box_h)])
        .flex(Flex::Center)
        .areas(r);
    let block = panel("live");
    let inner = block.inner(b);
    block.render(b, buf);
    let (rate, s, w) = (&v.rate, &v.stats, inner.width);
    let mut lines = live_direction(
        "▲ tx",
        TX_COLOR,
        (rate.tx_kbps, rate.tx_pps, s.bytes_sent, tx_peak),
        rows,
        w,
    );
    lines.extend(live_direction(
        "▼ rx",
        RX_COLOR,
        (rate.rx_kbps, rate.rx_pps, s.bytes_received, rx_peak),
        rows,
        w,
    ));
    lines.push(lr(
        vec![strong("Σ total".into())],
        vec![strong(format!("{:>3.0} kbps", rate.tx_kbps + rate.rx_kbps))],
        w,
    ));
    if rows >= 2 {
        lines.push(lr(
            vec![format!("  {:>3.0} pkt/s", rate.tx_pps + rate.rx_pps).fg(DIM)],
            vec![strong(human_bytes(s.bytes_sent + s.bytes_received))],
            w,
        ));
    }
    for (k, line) in lines.iter().enumerate() {
        put(buf, inner.x, inner.y + k as u16, line, inner);
    }
}

/// Too short for the boxed inset: a bare column of rates behind a divider.
fn live_bare(buf: &mut Buffer, r: Rect, v: &View) {
    if r.x > 0 {
        for y in r.y..r.bottom() {
            put(buf, r.x - 1, y, &Line::from("│".fg(DIM)), buf.area);
        }
    }
    let rate = &v.rate;
    let w = r.width.saturating_sub(1);
    let lines = [
        lr(
            vec!["▲ tx".fg(TX_COLOR).bold()],
            vec![format!("{:>3.0} kbps", rate.tx_kbps).fg(TX_COLOR).bold()],
            w,
        ),
        lr(
            vec!["▼ rx".fg(RX_COLOR).bold()],
            vec![format!("{:>3.0} kbps", rate.rx_kbps).fg(RX_COLOR).bold()],
            w,
        ),
        lr(
            vec![strong("Σ total".into())],
            vec![strong(format!("{:>3.0} kbps", rate.tx_kbps + rate.rx_kbps))],
            w,
        ),
    ];
    let top = r.y + r.height.saturating_sub(3) / 2;
    let clip = Rect::new(r.x + 1, r.y, w, r.height);
    for (k, line) in lines.iter().enumerate() {
        put(buf, r.x + 1, top + k as u16, line, clip);
    }
}

/// Too narrow for an inset: tx / rx / total as text rows above the graph.
fn live_rows(buf: &mut Buffer, r: Rect, v: &View) {
    let (rate, s) = (&v.rate, &v.stats);
    let row = |label: &str, kbps: f64, pps: f64, bytes: u64, color: Color| {
        Line::from(vec![
            format!("{label:<8}").fg(DIM),
            format!("{kbps:>5.0} kbps").fg(color).bold(),
            format!("  {pps:>4.0} pkt/s").fg(DIM),
            format!("   {:>9}", human_bytes(bytes)).fg(color),
        ])
    };
    let lines = [
        row("tx", rate.tx_kbps, rate.tx_pps, s.bytes_sent, TX_COLOR),
        row("rx", rate.rx_kbps, rate.rx_pps, s.bytes_received, RX_COLOR),
        Line::from(vec![
            strong(format!("{:<8}", "total")),
            strong(format!("{:>5.0} kbps", rate.tx_kbps + rate.rx_kbps)),
            strong(format!("  {:>4.0} pkt/s", rate.tx_pps + rate.rx_pps)),
            strong(format!(
                "   {:>9}",
                human_bytes(s.bytes_sent + s.bytes_received)
            )),
        ]),
    ];
    for (k, line) in lines.iter().enumerate() {
        put(buf, r.x, r.y + k as u16, line, r);
    }
}

// ---------- Jitter buffer ----------

/// The Jitter buffer panel: gauge with target tick + scale, the summary row, and
/// the depth history, all on the same 0–ceiling scale.
pub(super) fn jitter(frame: &mut Frame, area: Rect, v: &View) {
    let block = panel("Jitter buffer");
    let inner = block.inner(area);
    let legend = Line::from(vec![
        " depth ".fg(DEPTH_COLOR).bold(),
        "target ".fg(DIM),
        "▾▴ recenter ".yellow(),
        "✕ underrun ".red(),
        format!(
            "(0–{} ms · {}) ",
            v.info.jitter_ms,
            visible_span(inner.width)
        )
        .fg(DIM),
    ])
    .right_aligned();
    frame.render_widget(block.title_top(legend), area);
    let buf = frame.buffer_mut();
    gauge(
        buf,
        Rect {
            height: inner.height.min(2),
            ..inner
        },
        v,
    );
    if inner.height >= 3 {
        buffer_summary(buf, Rect::new(inner.x, inner.y + 2, inner.width, 1), v);
    }
    if inner.height > 3 {
        let rest = Rect::new(inner.x, inner.y + 3, inner.width, inner.height - 3);
        depth_graph(buf, rest, v);
    }
}

/// Fill colour by distance from the adaptive target (not by fraction of capacity).
fn target_color(v: &View) -> Color {
    let fill = v.stats.jitter_fill_ms as f64;
    let target = v.stats.target_depth_ms as f64;
    let cap = f64::from(v.info.jitter_ms);
    if fill < target * 0.3 {
        Color::Red
    } else if fill > target + (cap - target) * 0.6 {
        Color::Yellow
    } else {
        Color::Green
    }
}

/// `gaugeTarget`: a bar coloured against the target, a target tick, and a
/// 0 / target / ceiling scale line.
fn gauge(buf: &mut Buffer, r: Rect, v: &View) {
    let r = r.intersection(buf.area);
    if r.width < 4 || r.height == 0 {
        return;
    }
    let s = &v.stats;
    let cap_ms = u64::from(v.info.jitter_ms).max(1);
    let ratio = v.fill_ratio();
    let color = target_color(v);

    // Bar row (same cell treatment as ratatui's Gauge: filled cells swap fg/bg).
    let (x0, y0, w) = (r.x, r.y, r.width);
    let filled = (f64::from(w) * ratio).round() as u16;
    for x in x0..x0 + w {
        let cell = &mut buf[(x, y0)];
        cell.set_symbol(" ");
        if x < x0 + filled {
            cell.set_fg(Color::Reset).set_bg(color);
        } else {
            cell.set_fg(color).set_bg(Color::Reset);
        }
    }
    let label = format!("{} ms · {:.0}%", s.jitter_fill_ms, ratio * 100.0);
    let lx = x0 + w.saturating_sub(label.chars().count() as u16) / 2;
    put(buf, lx, y0, &Line::from(label), r);
    let tick_x = x0
        + ((f64::from(w - 1) * s.target_depth_ms as f64 / cap_ms as f64).round() as u16).min(w - 1);
    if buf[(tick_x, y0)].symbol() == " " {
        let tick_fg = if tick_x < x0 + filled {
            Color::Black
        } else {
            DIM
        };
        buf[(tick_x, y0)].set_symbol("┃").set_fg(tick_fg);
    }

    // Scale row.
    if r.height < 2 {
        return;
    }
    let y1 = y0 + 1;
    let cap_label = format!("{cap_ms} ms");
    let cap_w = cap_label.chars().count() as u16;
    put(buf, x0, y1, &Line::from("0".fg(DIM)), r);
    put(
        buf,
        x0 + w.saturating_sub(cap_w),
        y1,
        &Line::from(cap_label.fg(DIM)),
        r,
    );
    let target_label = format!("▲ target {}", s.target_depth_ms);
    let tl_w = target_label.chars().count() as u16;
    if tick_x + tl_w + 1 < x0 + w.saturating_sub(cap_w) {
        put(buf, tick_x, y1, &Line::from(target_label), r);
    } else {
        // Not enough room to the right: put the words left of the marker.
        let words = format!("target {} ", s.target_depth_ms);
        let wx = tick_x
            .saturating_sub(words.chars().count() as u16)
            .max(x0 + 2);
        put(buf, wx, y1, &Line::from(words), r);
        put(buf, tick_x, y1, &Line::from("▲"), r);
    }
}

/// `bufferSummary`: fill · target · headroom · drift on one row.
fn buffer_summary(buf: &mut Buffer, r: Rect, v: &View) {
    let s = &v.stats;
    let headroom = i64::from(v.info.jitter_ms) - s.jitter_fill_ms as i64;
    let drift = v.drift.round() as i64;
    let line = Line::from(vec![
        "fill ".fg(DIM),
        format!("{} ms", s.jitter_fill_ms)
            .fg(target_color(v))
            .bold(),
        "   target ".fg(DIM),
        format!("{} ms", s.target_depth_ms).into(),
        "   headroom ".fg(DIM),
        if headroom < 25 {
            format!("{headroom} ms").yellow().bold()
        } else {
            format!("{headroom} ms").into()
        },
        "   drift ".fg(DIM),
        if drift.abs() >= 2 {
            format!("{drift:+} ms/min").yellow().bold()
        } else {
            format!("{drift:+} ms/min").green()
        },
    ]);
    put(buf, r.x, r.y, &line, r);
}

/// `depthGraph`: fill vs the adaptive target (dotted), with recenter (▾ drop /
/// ▴ hold) and underrun (✕) markers in the sample's column.
fn depth_graph(buf: &mut Buffer, r: Rect, v: &View) {
    let r = r.intersection(buf.area);
    if r.width == 0 || r.height == 0 {
        return;
    }
    const TARGET: u8 = 1;
    const DEPTH: u8 = 2;
    let h = v.history;
    let cap_ms = f64::from(v.info.jitter_ms.max(1));
    let mut g = BrailleGrid::new(r);
    g.line(&h.target, cap_ms, TARGET, true);
    g.line(&h.depth, cap_ms, DEPTH, false);
    g.render(buf, |series| {
        if series & DEPTH != 0 {
            DEPTH_COLOR
        } else {
            DIM
        }
    });
    let bold = Modifier::BOLD;
    for (i, &ev) in h.events.iter().enumerate() {
        if ev == 0 {
            continue;
        }
        let Some(cx) = g.cell_of(i, h.events.len()) else {
            continue;
        };
        let (y, symbol, color) = if ev & EV_UNDERRUN != 0 {
            (r.bottom() - 1, "✕", Color::Red)
        } else if ev & EV_DROP != 0 {
            (r.y, "▾", Color::Yellow)
        } else if ev & EV_HOLD != 0 {
            (r.bottom() - 1, "▴", Color::Yellow)
        } else {
            continue;
        };
        buf[(cx, y)]
            .set_symbol(symbol)
            .set_style(Style::new().fg(color).add_modifier(bold));
    }
}

// ---------- Quality ----------

/// One Quality tile: stripe + verdict, the last-30 s number, the total.
struct Tile {
    label: &'static str,
    value: String,
    unit: &'static str,
    color: Color,
    symbol: &'static str,
    word: &'static str,
    total: String,
}

/// `qualityTiles`. `w` is the text width of the narrowest tile, for the overrun
/// total's short form.
fn quality_tiles(v: &View, w: usize) -> [Tile; 6] {
    let (s, rc) = (&v.stats, &v.recent);
    let count = |label, n: u64, total: String, audible: bool, words: [&'static str; 2]| {
        let level = match (n > 0, audible) {
            (false, _) => 0,
            (true, false) => 1,
            (true, true) => 2,
        };
        Tile {
            label,
            value: if n > 0 { format!("+{n}") } else { "0".into() },
            unit: "in 30 s",
            color: SEV_COLOR[level],
            symbol: SEV_SYMBOL[level],
            word: words[usize::from(n > 0)],
            total,
        }
    };
    let recent_loss = v.recent_loss();
    let loss_level = if recent_loss == 0.0 {
        0
    } else if recent_loss < 5.0 {
        1
    } else {
        2
    };
    let overrun_total = {
        let long = format!("buffer {} · capture {}", s.overruns, s.capture_overruns);
        if long.chars().count() > w {
            format!("buf {} · cap {}", s.overruns, s.capture_overruns)
        } else {
            long
        }
    };
    let mut tiles = [
        Tile {
            label: "LOSS",
            value: format!("{recent_loss:.1}%"),
            unit: "last 30 s",
            color: SEV_COLOR[loss_level],
            symbol: SEV_SYMBOL[loss_level],
            word: ["clean", "light", "heavy"][loss_level],
            total: format!("total {:.2}%", v.loss()),
        },
        count(
            "GAPS",
            rc.gap_frames,
            format!("total {}", s.gap_frames),
            false,
            ["none", "concealed"],
        ),
        count(
            "LATE",
            rc.dropped_late,
            format!("total {}", s.dropped_late),
            false,
            ["none", "discarded"],
        ),
        count(
            "UNDERRUN",
            rc.underruns,
            format!("total {}", s.underruns),
            true,
            ["none", "audible"],
        ),
        count(
            "RECENTER",
            rc.recenter_drops + rc.recenter_inserts,
            format!("drop {} · hold {}", s.recenter_drops, s.recenter_inserts),
            false,
            ["none", "nudged"],
        ),
        count(
            "OVERRUN",
            rc.overruns + rc.capture_overruns,
            overrun_total,
            true,
            ["none", "dropped"],
        ),
    ];
    if !v.receives() {
        for t in &mut tiles[..5] {
            t.value = "—".into();
            t.unit = "not receiving";
            t.color = DIM;
            t.symbol = "·";
            t.word = "n/a";
        }
    }
    tiles
}

/// `tile`: a 3-line tile with a coloured stripe on its left edge.
fn tile(buf: &mut Buffer, r: Rect, t: &Tile) {
    let stripe = Line::from("▎".fg(t.color));
    for y in r.y..r.bottom() {
        put(buf, r.x, y, &stripe, r);
    }
    if r.width < 5 {
        return;
    }
    let (x, w) = (r.x + 2, r.width - 4);
    let clip = Rect::new(x, r.y, w, r.height);
    let full = format!("{} {}", t.symbol, t.word);
    let verdict = if t.label.len() + full.chars().count() < w as usize {
        full
    } else {
        t.symbol.to_string()
    };
    let head = lr(vec![t.label.bold()], vec![verdict.fg(t.color).bold()], w);
    put(buf, x, r.y, &head, clip);
    let value = Line::from(vec![
        t.value.clone().fg(t.color).bold(),
        format!("  {}", t.unit).fg(DIM),
    ]);
    put(buf, x, r.y + 1, &value, clip);
    put(buf, x, r.y + 2, &Line::from(t.total.clone().fg(DIM)), clip);
}

/// `qualityPanel`: six tiles side by side; the title says since when totals count.
pub(super) fn quality(frame: &mut Frame, area: Rect, v: &View) {
    let since = match v.reset_ago {
        Some(ago) => format!(" totals since reset · {} ago ", fmt_span(ago.as_secs()))
            .fg(ACCENT)
            .bold(),
        None => " totals since start ".fg(DIM),
    };
    let block = panel("Quality").title_top(Line::from(since).right_aligned());
    let inner = block.inner(area);
    frame.render_widget(block, area);
    // The tiles run one column into the right padding, as in the design.
    let strip = Rect {
        width: inner.width + 1,
        height: inner.height.min(3),
        ..inner
    }
    .intersection(area);
    if strip.is_empty() {
        return;
    }
    let cols: [Rect; 6] = even_split(strip);
    let w = cols
        .iter()
        .map(|c| c.width)
        .max()
        .unwrap_or(0)
        .saturating_sub(4);
    let tiles = quality_tiles(v, w as usize);
    let buf = frame.buffer_mut();
    for (c, t) in cols.iter().zip(&tiles) {
        tile(buf, *c, t);
    }
}

/// `N` equal columns; the last takes the remainder (matches the design's split).
fn even_split<const N: usize>(r: Rect) -> [Rect; N] {
    let base = r.width / N as u16;
    std::array::from_fn(|k| {
        let x = r.x + base * k as u16;
        let width = if k == N - 1 { r.right() - x } else { base };
        Rect::new(x, r.y, width, r.height)
    })
}
