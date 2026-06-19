//! `--output tui`: a live full-screen dashboard (ratatui), styled like btop —
//! rounded panels, a braille tx/rx throughput graph, a jitter-buffer gauge, and
//! loss/quality readouts. Read-only over the engine; never touches the audio path.

use std::io::IsTerminal;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Style, Stylize};
use ratatui::symbols::Marker;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Axis, Block, BorderType, Chart, Dataset, Gauge, GraphType, Paragraph};
use ratatui::{DefaultTerminal, Frame};

use vox_core::{Engine, EngineStats};

use crate::{kbps, SessionInfo};

/// Stats sampling cadence (also the rate-averaging window).
const SAMPLE: Duration = Duration::from_millis(500);
/// Number of rate samples kept for the throughput graph.
const HISTORY: usize = 240;

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
            last = now;
            last_sample = Instant::now();
        }

        let stats = engine.stats();
        let uptime = start.elapsed();
        terminal
            .draw(|frame| draw(frame, info, &stats, &rate, uptime, &tx_history, &rx_history))?;
    }
    Ok(())
}

fn draw(
    frame: &mut Frame,
    info: &SessionInfo,
    stats: &EngineStats,
    rate: &Rates,
    uptime: Duration,
    tx_history: &[f64],
    rx_history: &[f64],
) {
    let [header_a, mid_a, chart_a, footer_a] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(10),
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
        Line::from(""),
        rate_line("tx", rate.tx_kbps, rate.tx_pps, Color::Green),
        rate_line("rx", rate.rx_kbps, rate.rx_pps, Color::Cyan),
    ])
    .block(rounded("status"));
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
        .block(rounded("jitter buffer"))
        .gauge_style(Style::new().fg(jcolor))
        .ratio(jratio)
        .label(format!("{:.0}%", jratio * 100.0));
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
    let quality = Paragraph::new(vec![
        Line::from(vec![
            "loss   ".dark_gray(),
            format!("{loss:.1}%").fg(lcolor).bold(),
        ]),
        Line::from(vec![
            "gaps   ".dark_gray(),
            stats.gap_frames.to_string().into(),
        ]),
        Line::from(vec![
            "drops  ".dark_gray(),
            stats.dropped_late.to_string().into(),
        ]),
    ])
    .block(rounded("quality"));
    frame.render_widget(quality, quality_a);

    // Throughput graph (braille line, tx green + rx cyan).
    let tx_points = points(tx_history);
    let rx_points = points(rx_history);
    let max_y = tx_history
        .iter()
        .chain(rx_history)
        .cloned()
        .fold(0.0f64, f64::max)
        .max(48.0)
        * 1.2;
    let datasets = vec![
        Dataset::default()
            .name("tx")
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::new().green())
            .data(&tx_points),
        Dataset::default()
            .name("rx")
            .marker(Marker::Braille)
            .graph_type(GraphType::Line)
            .style(Style::new().cyan())
            .data(&rx_points),
    ];
    let chart = Chart::new(datasets)
        .block(rounded("throughput kbps"))
        .x_axis(Axis::default().bounds([0.0, (HISTORY - 1) as f64]))
        .y_axis(
            Axis::default()
                .bounds([0.0, max_y])
                .labels(vec![Span::raw("0"), Span::raw(format!("{max_y:.0}"))]),
        );
    frame.render_widget(chart, chart_a);

    let footer = Paragraph::new("q / Esc / Ctrl+C to quit".dark_gray());
    frame.render_widget(footer, footer_a);
}

fn rounded(title: &str) -> Block<'static> {
    Block::bordered()
        .border_type(BorderType::Rounded)
        .border_style(Style::new().fg(Color::DarkGray))
        .title(Span::from(format!(" {title} ")).cyan().bold())
}

fn kv(label: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        format!("{label:<9}").dark_gray(),
        value.to_string().into(),
    ])
}

fn rate_line(label: &str, kbps_val: f64, pps: f64, color: Color) -> Line<'static> {
    Line::from(vec![
        format!("{label:<9}").dark_gray(),
        format!("{kbps_val:>5.0} kbps").fg(color).bold(),
        format!("   {pps:>4.0} pkt/s").dark_gray(),
    ])
}

fn points(history: &[f64]) -> Vec<(f64, f64)> {
    history
        .iter()
        .enumerate()
        .map(|(i, &v)| (i as f64, v))
        .collect()
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
