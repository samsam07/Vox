//! `--output tui`: a live full-screen dashboard (ratatui), styled like btop —
//! rounded panels, a braille tx/rx throughput graph, a jitter-buffer gauge, and
//! loss/quality readouts. Read-only over the engine; never touches the audio path.

use std::io::IsTerminal;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use ratatui::buffer::Buffer;
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Style, Stylize};
use ratatui::symbols::braille;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Gauge, Padding, Paragraph};
use ratatui::{DefaultTerminal, Frame};

use vox_core::{Engine, EngineStats};

use crate::{kbps, SessionInfo};

/// Stats sampling cadence (also the rate-averaging window).
const SAMPLE: Duration = Duration::from_millis(500);
/// Number of rate samples kept for the throughput graph.
const HISTORY: usize = 240;
/// Throughput-graph series colours: tx, rx, and cells where both lines overlap.
const TX_COLOR: Color = Color::Green;
const RX_COLOR: Color = Color::Cyan;
const BOTH_COLOR: Color = Color::Yellow;

/// Run the dashboard until the user quits (q / Esc / Ctrl+C) or `duration` elapses.
pub fn run(engine: &Engine, info: &SessionInfo, duration: Option<u64>) -> Result<()> {
    if !std::io::stdout().is_terminal() {
        bail!("--output tui needs an interactive terminal (use --output plain when redirected)");
    }
    let mut terminal = ratatui::init();
    let result = run_loop(&mut terminal, engine, info, duration);
    ratatui::restore();
    result
}

#[derive(Default, Clone, Copy)]
struct Rates {
    tx_kbps: f64,
    rx_kbps: f64,
    tx_pps: f64,
    rx_pps: f64,
}

fn run_loop(
    terminal: &mut DefaultTerminal,
    engine: &Engine,
    info: &SessionInfo,
    duration: Option<u64>,
) -> Result<()> {
    let start = Instant::now();
    let deadline = duration.map(|secs| start + Duration::from_secs(secs));

    let mut last = engine.stats();
    let mut last_sample = Instant::now();
    let mut rate = Rates::default();
    let mut tx_history = vec![0.0f64; HISTORY];
    let mut rx_history = vec![0.0f64; HISTORY];
    // Buffered-latency history for drift detection (grows to HISTORY, no leading zeros
    // so the trend isn't skewed at startup).
    let mut depth_history: Vec<f64> = Vec::new();
    let mut drift = 0.0f64;

    loop {
        if deadline.is_some_and(|d| Instant::now() >= d) {
            break;
        }
        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press && is_quit(key.code, key.modifiers) {
                    break;
                }
            }
        }
        if last_sample.elapsed() >= SAMPLE {
            let now = engine.stats();
            let secs = last_sample.elapsed().as_secs_f64();
            rate = Rates {
                tx_kbps: kbps(now.bytes_sent - last.bytes_sent, secs),
                rx_kbps: kbps(now.bytes_received - last.bytes_received, secs),
                tx_pps: (now.packets_sent - last.packets_sent) as f64 / secs,
                rx_pps: (now.packets_received - last.packets_received) as f64 / secs,
            };
            push(&mut tx_history, rate.tx_kbps);
            push(&mut rx_history, rate.rx_kbps);
            depth_history.push(now.jitter_fill_ms as f64);
            if depth_history.len() > HISTORY {
                depth_history.remove(0);
            }
            drift = drift_per_min(&depth_history);
            last = now;
            last_sample = Instant::now();
        }

        let stats = engine.stats();
        let uptime = start.elapsed();
        terminal.draw(|frame| {
            draw(
                frame,
                info,
                &stats,
                &rate,
                drift,
                uptime,
                &tx_history,
                &rx_history,
            )
        })?;
    }
    Ok(())
}

/// Trend (slope) of the buffered-latency history in ms/min — positive means the
/// jitter buffer is slowly filling, negative draining: the signature of clock drift,
/// which a steady recenter count alone wouldn't reveal until it hits a rail.
fn drift_per_min(history: &[f64]) -> f64 {
    let n = history.len();
    if n < 20 {
        return 0.0; // ~10 s minimum before estimating
    }
    let half = n / 2;
    let mean = |s: &[f64]| s.iter().sum::<f64>() / s.len() as f64;
    // Rise between the two halves' centroids (n/2 samples × 0.5 s apart) → per minute.
    (mean(&history[half..]) - mean(&history[..half])) * 240.0 / n as f64
}

#[allow(clippy::too_many_arguments)] // a dashboard frame; grouping these would not clarify
fn draw(
    frame: &mut Frame,
    info: &SessionInfo,
    stats: &EngineStats,
    rate: &Rates,
    drift: f64,
    uptime: Duration,
    tx_history: &[f64],
    rx_history: &[f64],
) {
    let [header_a, mid_a, chart_a, footer_a] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(13),
        Constraint::Min(6),
        Constraint::Length(1),
    ])
    .areas(frame.area());
    let [status_a, right_a] =
        Layout::horizontal([Constraint::Percentage(56), Constraint::Percentage(44)]).areas(mid_a);
    let [jitter_a, quality_a] =
        Layout::vertical([Constraint::Length(3), Constraint::Min(4)]).areas(right_a);

    // Header.
    let header = Paragraph::new(Line::from(vec![
        format!("vox {}", env!("CARGO_PKG_VERSION")).cyan().bold(),
        "  —  ".dark_gray(),
        info.mode.bold(),
    ]))
    .centered()
    .block(rounded(""));
    frame.render_widget(header, header_a);

    // Status.
    let peer = info.peer.map_or_else(|| "—".to_string(), |p| p.to_string());
    let status = Paragraph::new(vec![
        kv("capture", &info.capture),
        kv("playback", &info.playback),
        kv("peer", &peer),
        kv("bind", &info.bind.to_string()),
        kv("uptime", &format_duration(uptime)),
        kv(
            "codec",
            &format!(
                "{} bps   fec {}   dtx {}   jitter {} ms   drift {}",
                info.bitrate,
                on_off(info.fec),
                on_off(info.dtx),
                info.jitter_ms,
                on_off(info.drift_correct)
            ),
        ),
        Line::from(""),
        throughput_line("tx", rate.tx_kbps, rate.tx_pps, stats.bytes_sent, TX_COLOR),
        throughput_line(
            "rx",
            rate.rx_kbps,
            rate.rx_pps,
            stats.bytes_received,
            RX_COLOR,
        ),
        // White + bold, not BOTH_COLOR: yellow would read as "the yellow graph line
        // is the total", but yellow there means the tx/rx overlap.
        Line::from(vec![
            format!("{:<8}", "total").white().bold(),
            format!("{:>5.0} kbps", rate.tx_kbps + rate.rx_kbps)
                .white()
                .bold(),
            format!("  {:>4.0} pkt/s", rate.tx_pps + rate.rx_pps)
                .white()
                .bold(),
            format!(
                "   {:>9}",
                human_bytes(stats.bytes_sent + stats.bytes_received)
            )
            .white()
            .bold(),
        ]),
    ])
    .block(rounded("Status"));
    frame.render_widget(status, status_a);

    // Jitter-buffer gauge.
    let jratio = if stats.jitter_capacity > 0 {
        (stats.jitter_fill as f64 / stats.jitter_capacity as f64).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let jcolor = if jratio < 0.2 {
        Color::Red
    } else if jratio > 0.8 {
        Color::Yellow
    } else {
        Color::Green
    };
    let jitter = Gauge::default()
        .block(rounded("Jitter buffer"))
        .gauge_style(Style::new().fg(jcolor))
        .ratio(jratio)
        .label(format!(
            "{:.0}%  ·  {} ms",
            jratio * 100.0,
            stats.jitter_fill_ms
        ));
    frame.render_widget(jitter, jitter_a);

    // Quality.
    let expected = stats.packets_received + stats.gap_frames;
    let loss = if expected > 0 {
        stats.gap_frames as f64 / expected as f64 * 100.0
    } else {
        0.0
    };
    let lcolor = if loss == 0.0 {
        Color::Green
    } else if loss < 5.0 {
        Color::Yellow
    } else {
        Color::Red
    };
    let overrun_color = (stats.overruns > 0).then_some(Color::Red);
    let recenter = stats.recenter_drops + stats.recenter_inserts;
    let recenter_color = (recenter > 0).then_some(Color::Yellow);
    let drift_color = (drift.abs() >= 2.0).then_some(Color::Yellow);
    let quality = Paragraph::new(vec![
        qline("loss", format!("{loss:.1}%"), Some(lcolor)),
        qline("gaps", stats.gap_frames.to_string(), None),
        // dropped_late = late/duplicate packets discarded (distinct from a recenter drop).
        qline("late", stats.dropped_late.to_string(), None),
        qline("overrun", stats.overruns.to_string(), overrun_color),
        qline(
            "recenter",
            format!(
                "{recenter}  (drop {} / hold {})",
                stats.recenter_drops, stats.recenter_inserts
            ),
            recenter_color,
        ),
        qline(
            "target",
            format!("{} ms (adaptive)", stats.target_depth_ms),
            None,
        ),
        qline("drift", format!("{drift:+.0} ms/min"), drift_color),
    ])
    .block(rounded("Quality"));
    frame.render_widget(quality, quality_a);

    // Throughput graph: custom braille plot so tx / rx / their overlap colour
    // independently, with a value interpolated at every sub-column (gap-free).
    let max_y = tx_history
        .iter()
        .chain(rx_history)
        .cloned()
        .fold(0.0f64, f64::max)
        .max(48.0)
        * 1.2;
    let graph_block = Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(Color::DarkGray))
        .padding(Padding::horizontal(1))
        .title(Line::from(vec![
            " Throughput kbps   ".cyan().bold(),
            "tx ".fg(TX_COLOR).bold(),
            "rx ".fg(RX_COLOR).bold(),
            "both".fg(BOTH_COLOR).bold(),
            format!("   (0–{max_y:.0}) ").dark_gray(),
        ]));
    let graph_inner = graph_block.inner(chart_a);
    frame.render_widget(graph_block, chart_a);
    braille_graph(
        frame.buffer_mut(),
        graph_inner,
        tx_history,
        rx_history,
        max_y,
    );

    let footer = Paragraph::new("q / Esc / Ctrl+C to quit".dark_gray());
    frame.render_widget(footer, footer_a);
}

fn rounded(title: &str) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(Color::DarkGray))
        .padding(Padding::horizontal(1))
        .title(Span::from(format!(" {title} ")).cyan().bold())
}

fn kv(label: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        format!("{label:<9}").dark_gray(),
        value.to_string().into(),
    ])
}

fn on_off(enabled: bool) -> &'static str {
    if enabled {
        "on"
    } else {
        "off"
    }
}

fn qline(label: &str, value: String, color: Option<Color>) -> Line<'static> {
    let value = match color {
        Some(c) => value.fg(c).bold(),
        None => value.into(),
    };
    Line::from(vec![format!("{label:<9}").dark_gray(), value])
}

/// A throughput row: live rate (kbps + pkt/s) plus the cumulative volume since
/// start (tx = sent, rx = received, total = both).
fn throughput_line(label: &str, kbps: f64, pps: f64, bytes: u64, color: Color) -> Line<'static> {
    Line::from(vec![
        format!("{label:<8}").dark_gray(),
        format!("{kbps:>5.0} kbps").fg(color).bold(),
        format!("  {pps:>4.0} pkt/s").dark_gray(),
        format!("   {:>9}", human_bytes(bytes)).fg(color),
    ])
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

/// One braille cell's state: dot bits, and which series passed through it.
#[derive(Clone, Copy, Default)]
struct GraphCell {
    bits: u16,
    tx: bool,
    rx: bool,
}

/// Render tx and rx as continuous braille lines into `area`, colouring each cell by
/// which series cross it (tx, rx, or both). Braille gives 2x4 sub-pixels per cell;
/// a value is interpolated at every sub-column and vertically connected to the
/// previous one, so the line has no gaps.
fn braille_graph(buf: &mut Buffer, area: Rect, tx: &[f64], rx: &[f64], max_y: f64) {
    let (w, h) = (area.width as usize, area.height as usize);
    if w == 0 || h == 0 || max_y <= 0.0 {
        return;
    }
    let (cols, rows) = (w * 2, h * 4);
    let mut cells = vec![GraphCell::default(); w * h];

    plot(&mut cells, w, cols, rows, tx, max_y, true);
    plot(&mut cells, w, cols, rows, rx, max_y, false);

    for cy in 0..h {
        for cx in 0..w {
            let cell = cells[cy * w + cx];
            if cell.bits == 0 {
                continue;
            }
            let ch = char::from_u32(braille::BLANK as u32 | cell.bits as u32).unwrap_or('\u{2800}');
            let color = match (cell.tx, cell.rx) {
                (true, true) => BOTH_COLOR,
                (true, false) => TX_COLOR,
                (false, true) => RX_COLOR,
                (false, false) => continue,
            };
            let target = &mut buf[(area.x + cx as u16, area.y + cy as u16)];
            target.set_char(ch);
            target.set_fg(color);
        }
    }
}

/// Plot one series into the braille cell grid, connecting consecutive sub-columns.
fn plot(
    cells: &mut [GraphCell],
    w: usize,
    cols: usize,
    rows: usize,
    data: &[f64],
    max_y: f64,
    is_tx: bool,
) {
    if data.len() < 2 {
        return;
    }
    let mut prev_y: Option<usize> = None;
    for sx in 0..cols {
        // Interpolate the value at this sub-column from the history.
        let f = sx as f64 / (cols - 1) as f64 * (data.len() - 1) as f64;
        let i0 = f.floor() as usize;
        let i1 = (i0 + 1).min(data.len() - 1);
        let value = data[i0] + (data[i1] - data[i0]) * (f - i0 as f64);
        // Map value to a sub-row (row 0 = top, so higher value = smaller index).
        let norm = (value / max_y).clamp(0.0, 1.0);
        let y = (rows - 1) - (norm * (rows - 1) as f64).round() as usize;
        // Connect from the previous sub-column's height to this one.
        let (lo, hi) = prev_y.map_or((y, y), |p| (p.min(y), p.max(y)));
        for yy in lo..=hi {
            let cell = &mut cells[(yy / 4) * w + sx / 2];
            cell.bits |= braille::DOTS[yy % 4][sx % 2];
            if is_tx {
                cell.tx = true;
            } else {
                cell.rx = true;
            }
        }
        prev_y = Some(y);
    }
}

fn push(history: &mut Vec<f64>, value: f64) {
    history.remove(0);
    history.push(value);
}

fn is_quit(code: KeyCode, modifiers: KeyModifiers) -> bool {
    matches!(code, KeyCode::Char('q') | KeyCode::Esc)
        || (code == KeyCode::Char('c') && modifiers.contains(KeyModifiers::CONTROL))
}

fn format_duration(d: Duration) -> String {
    let secs = d.as_secs();
    format!(
        "{:02}:{:02}:{:02}",
        secs / 3600,
        (secs % 3600) / 60,
        secs % 60
    )
}
