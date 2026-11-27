//! Render tests for the Pipeline dashboard, driven by a synthetic session shaped
//! like the workbench's "Clock drift" scenario (buffer creeping toward the ceiling,
//! periodic recenter drops, speech-like VBR).

use std::time::Duration;

use ratatui::backend::TestBackend;
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::Terminal;
use vox_core::EngineStats;

use super::help::{Entry, HELP};
use super::model::{Baseline, History, Rates, Snapshot, View, SAMPLE};
use super::options::{Roles, Settings, COUNT};
use super::{draw, key_action, Action, Overlay, Ui};
use crate::SessionInfo;
use vox_core::Setting;

/// The workbench's four reference sizes.
const SIZES: [(u16, u16); 4] = [(80, 24), (120, 30), (160, 42), (197, 46)];

#[derive(Clone, Copy, PartialEq)]
enum Mode {
    Duplex,
    SendOnly,
    ReceiveOnly,
}

impl Mode {
    fn sends(self) -> bool {
        self != Mode::ReceiveOnly
    }
    fn receives(self) -> bool {
        self != Mode::SendOnly
    }
    fn roles(self) -> Roles {
        Roles {
            sends: self.sends(),
            receives: self.receives(),
        }
    }
    fn label(self) -> &'static str {
        match self {
            Mode::Duplex => "full duplex",
            Mode::SendOnly => "send-only",
            Mode::ReceiveOnly => "receive-only",
        }
    }
}

pub(super) fn info(mode: &'static str) -> SessionInfo {
    SessionInfo {
        mode,
        capture: "default  [48000 Hz, mono]".into(),
        playback: "default  [48000 Hz, mono]".into(),
        peer: Some("192.168.100.29:9680".parse().expect("valid addr")),
        bind: "0.0.0.0:9680".parse().expect("valid addr"),
        bitrate: 24000,
        jitter_ms: 250,
        fec: false,
        expected_loss: 10,
        dtx: false,
        drift_correct: false,
    }
}

/// Deterministic noise in [-1, 1).
struct Noise(u64);

impl Noise {
    fn next(&mut self) -> f64 {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        (self.0 >> 11) as f64 / (1u64 << 52) as f64 - 1.0
    }
}

/// A finished drifting session: final stats, its history, the last rates.
struct Session {
    stats: EngineStats,
    history: History,
    rate: Rates,
    uptime: Duration,
}

/// `samples` × 0.5 s of the "Clock drift" scenario: the buffer creeps up 0.15 ms
/// per sample until it nears the 250 ms ceiling and recenters down by 20 ms.
fn session(mode: Mode, samples: usize) -> Session {
    const CAP_MS: u64 = 250;
    const FRAMES: u64 = 25; // 20 ms frames per 0.5 s sample
    let mut noise = Noise(7);
    let mut s = EngineStats::default();
    if mode.receives() {
        s.jitter_capacity = CAP_MS * 48;
        s.target_depth_ms = 174;
    }
    let mut history = History::new(s);
    let (mut txv, mut rxv, mut base) = (14.0f64, 26.0f64, 150.0f64);
    let mut rate = Rates::default();
    let secs = SAMPLE.as_secs_f64();
    for _ in 0..samples {
        let prev = s;
        txv = (txv + (14.0 - txv) * 0.25 + noise.next() * 2.2).clamp(3.0, 60.0);
        rxv = (rxv + (26.0 - rxv) * 0.25 + noise.next() * 2.6).clamp(8.0, 34.0);
        rate = Rates::default();
        if mode.sends() {
            s.packets_sent += FRAMES;
            s.bytes_sent += (txv * 1000.0 / 8.0 * secs) as u64;
            rate.tx_kbps = txv;
            rate.tx_pps = FRAMES as f64 / secs;
        }
        if mode.receives() {
            s.packets_received += FRAMES;
            s.bytes_received += (rxv * 1000.0 / 8.0 * secs) as u64;
            rate.rx_kbps = rxv;
            rate.rx_pps = FRAMES as f64 / secs;
            base += 0.15;
            if base > (CAP_MS - 14) as f64 {
                base -= 20.0;
                s.recenter_drops += 1;
            }
            let fill = (base + noise.next() * 3.0).clamp(0.0, CAP_MS as f64) as u64;
            s.jitter_fill_ms = fill;
            s.jitter_fill = fill * 48;
        }
        history.record(&prev, &s, &rate);
    }
    Session {
        stats: s,
        history,
        rate,
        uptime: SAMPLE * samples as u32,
    }
}

/// Options for one rendered frame.
#[derive(Default)]
struct Opts {
    paused: bool,
    reset: bool,
    help: Option<usize>,
    /// Options overlay open with this row selected.
    options: Option<usize>,
    /// Settings changed this session (default: as configured).
    settings: Option<Settings>,
}

/// Render one frame as text (one string per row).
fn render(w: u16, h: u16, mode: Mode, opts: Opts) -> Vec<String> {
    let info = info(mode.label());
    let sess = session(mode, 600);
    let base = opts.reset.then(|| Baseline {
        stats: sess.stats,
        at: sess.uptime.saturating_sub(Duration::from_secs(42)),
    });
    let snap = Snapshot {
        stats: sess.stats,
        rate: sess.rate,
        drift: sess.history.drift(),
        uptime: sess.uptime,
        history: &sess.history,
    };
    let start = Settings::from_info(&info);
    let settings = opts.settings.unwrap_or(start);
    let view = View::new(&info, snap, settings, base.as_ref(), opts.paused);
    let mut scroll = opts.help.unwrap_or(0);
    let mut term = Terminal::new(TestBackend::new(w, h)).expect("test backend");
    term.draw(|f| {
        let overlay = match (opts.help, opts.options) {
            (Some(_), _) => Overlay::Help(&mut scroll),
            (None, Some(sel)) => Overlay::Options {
                sel,
                start: &start,
                roles: mode.roles(),
            },
            (None, None) => Overlay::None,
        };
        draw(f, &view, overlay)
    })
    .expect("draw");
    let buf = term.backend().buffer();
    (0..h)
        .map(|y| (0..w).map(|x| buf[(x, y)].symbol()).collect::<String>())
        .collect()
}

fn text(rows: &[String]) -> String {
    rows.join("\n")
}

/// No panel text runs over a border: every panel row starts and ends with one.
fn assert_borders_intact(rows: &[String], w: u16, h: u16) {
    for (y, row) in rows.iter().enumerate().take(h as usize - 1) {
        let chars: Vec<char> = row.chars().collect();
        let (first, last) = (chars[0], chars[w as usize - 1]);
        assert!(
            matches!(first, '╭' | '│' | '╰'),
            "{w}×{h} row {y} left edge {first:?}:\n{row}"
        );
        assert!(
            matches!(last, '╮' | '│' | '╯'),
            "{w}×{h} row {y} right edge {last:?}:\n{row}"
        );
    }
}

#[test]
fn full_duplex_at_reference_sizes() {
    for (w, h) in SIZES {
        let rows = render(w, h, Mode::Duplex, Opts::default());
        let t = text(&rows);
        for panel in ["Status", "Throughput", "Jitter buffer", "Quality"] {
            assert!(t.contains(panel), "{w}×{h} missing {panel}:\n{t}");
        }
        for tile in ["LOSS", "GAPS", "LATE", "UNDERRUN", "RECENTER", "OVERRUN"] {
            assert!(t.contains(tile), "{w}×{h} missing tile {tile}:\n{t}");
        }
        assert!(t.contains("▲ tx") && t.contains("▼ rx"), "{w}×{h}:\n{t}");
        assert!(
            t.contains("fill ") && t.contains("headroom"),
            "{w}×{h}:\n{t}"
        );
        assert!(t.contains("totals since start"), "{w}×{h}:\n{t}");
        assert!(t.contains("quit") && t.contains("help"), "{w}×{h}:\n{t}");
        assert_borders_intact(&rows, w, h);
    }
}

#[test]
fn live_box_appears_when_tall_enough() {
    // 80×24 is too short for the boxed inset (bare column); the rest get the box.
    assert!(!text(&render(80, 24, Mode::Duplex, Opts::default())).contains(" live "));
    for (w, h) in &SIZES[1..] {
        let t = text(&render(*w, *h, Mode::Duplex, Opts::default()));
        assert!(
            t.contains(" live ") && t.contains("Σ total"),
            "{w}×{h}:\n{t}"
        );
    }
    // Tall enough for peaks.
    assert!(text(&render(160, 42, Mode::Duplex, Opts::default())).contains("peak"));
}

#[test]
fn titles_show_the_visible_span() {
    // 160 cols: plot ≈ 127 cells → 254 samples → 2 min 7 s.
    let t = text(&render(160, 42, Mode::Duplex, Opts::default()));
    assert!(t.contains("· kbps · 2 min"), "{t}");
    assert!(t.contains("(0–250 ms · 2 min 36 s)"), "{t}");
}

#[test]
fn send_only_drops_the_buffer_panel() {
    for (w, h) in SIZES {
        let rows = render(w, h, Mode::SendOnly, Opts::default());
        let t = text(&rows);
        assert!(!t.contains("Jitter buffer"), "{w}×{h}:\n{t}");
        // "not receiving" is cut to fit the narrow tiles at 80 columns.
        assert!(
            t.contains("not r") && t.contains("● sending"),
            "{w}×{h}:\n{t}"
        );
        assert_borders_intact(&rows, w, h);
    }
}

#[test]
fn receive_only_renders() {
    for (w, h) in SIZES {
        let rows = render(w, h, Mode::ReceiveOnly, Opts::default());
        assert!(text(&rows).contains("Jitter buffer"));
        assert_borders_intact(&rows, w, h);
    }
}

#[test]
fn draws_at_tiny_sizes_without_panicking() {
    for (w, h) in [(60, 20), (40, 15), (30, 10), (8, 4), (1, 1)] {
        for mode in [Mode::Duplex, Mode::SendOnly, Mode::ReceiveOnly] {
            render(w, h, mode, Opts::default());
            render(
                w,
                h,
                mode,
                Opts {
                    help: Some(99),
                    paused: true,
                    reset: true,
                    ..Opts::default()
                },
            );
            render(
                w,
                h,
                mode,
                Opts {
                    options: Some(COUNT - 1),
                    ..Opts::default()
                },
            );
        }
    }
}

#[test]
fn paused_and_reset_are_shown() {
    let t = text(&render(
        160,
        42,
        Mode::Duplex,
        Opts {
            paused: true,
            ..Opts::default()
        },
    ));
    assert!(
        t.contains("PAUSED · p resume") && t.contains("resume"),
        "{t}"
    );

    let t = text(&render(
        160,
        42,
        Mode::Duplex,
        Opts {
            reset: true,
            ..Opts::default()
        },
    ));
    assert!(t.contains("totals since reset · 42 s ago"), "{t}");
    assert!(t.contains("drop 0 · hold 0"), "totals zeroed:\n{t}");
}

#[test]
fn help_overlay_scrolls_when_it_does_not_fit() {
    for (w, h) in SIZES {
        let rows = render(
            w,
            h,
            Mode::Duplex,
            Opts {
                help: Some(0),
                ..Opts::default()
            },
        );
        let t = text(&rows);
        assert!(t.contains("Help · what the numbers mean"), "{w}×{h}:\n{t}");
        assert!(t.contains("JITTER BUFFER"), "{w}×{h}:\n{t}");
        assert_borders_intact(&rows, w, h);
    }
    // 80×24 can't fit it all: scroll hint, and scrolling far is clamped to the end.
    let t = text(&render(
        80,
        24,
        Mode::Duplex,
        Opts {
            help: Some(0),
            ..Opts::default()
        },
    ));
    assert!(t.contains("↑↓ scroll"), "{t}");
    let t = text(&render(
        80,
        24,
        Mode::Duplex,
        Opts {
            help: Some(999),
            ..Opts::default()
        },
    ));
    assert!(t.contains("HEALTH"), "{t}");
}

/// Every UI string is plain user language: no config keys, flag names, or
/// implementation words.
#[test]
fn ui_text_is_plain_language() {
    const BANNED: [&str; 9] = [
        "ring",
        "thread",
        "toml",
        "jitter_ms",
        "drift_correct",
        "expected_loss",
        "role",
        "callback",
        "resampler",
    ];
    let mut corpus = Vec::new();
    for (w, h) in SIZES {
        for mode in [Mode::Duplex, Mode::SendOnly, Mode::ReceiveOnly] {
            corpus.push(text(&render(
                w,
                h,
                mode,
                Opts {
                    paused: true,
                    reset: true,
                    ..Opts::default()
                },
            )));
        }
    }
    // The Options overlay with each row selected (so every row's explanation shows).
    for sel in 0..COUNT {
        corpus.push(text(&render(
            120,
            30,
            Mode::Duplex,
            Opts {
                options: Some(sel),
                ..Opts::default()
            },
        )));
    }
    for entry in HELP {
        match entry {
            Entry::Section(t, sub) => {
                corpus.push(t.to_string());
                corpus.extend(sub.map(str::to_string));
            }
            Entry::Term(t, d) => corpus.push(format!("{t} {d}")),
        }
    }
    for chunk in &corpus {
        assert!(!chunk.contains("--"), "flag-like text in:\n{chunk}");
        for word in chunk.split(|c: char| !(c.is_alphanumeric() || c == '_')) {
            let word = word.to_lowercase();
            assert!(!BANNED.contains(&word.as_str()), "{word:?} in:\n{chunk}");
        }
    }
}

fn ui() -> Ui {
    Ui::new(
        Settings::from_info(&info("full duplex")),
        Mode::Duplex.roles(),
    )
}

fn press(ui: &mut Ui, code: KeyCode) -> Action {
    key_action(ui, KeyEvent::new(code, KeyModifiers::NONE))
}

#[test]
fn esc_closes_help_before_it_quits() {
    let mut ui = ui();
    assert_eq!(press(&mut ui, KeyCode::Char('?')), Action::None);
    assert!(ui.help);
    assert_eq!(press(&mut ui, KeyCode::Esc), Action::None);
    assert!(!ui.help);
    assert_eq!(press(&mut ui, KeyCode::Esc), Action::Quit);
}

#[test]
fn keys_map_to_actions() {
    let mut ui = ui();
    assert_eq!(press(&mut ui, KeyCode::Char('p')), Action::TogglePause);
    assert_eq!(press(&mut ui, KeyCode::Char('r')), Action::Reset);
    assert_eq!(press(&mut ui, KeyCode::Char('q')), Action::Quit);
    let ctrl_c = KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert_eq!(key_action(&mut ui, ctrl_c), Action::Quit);

    // In help, arrows scroll (never below 0) and p / r do nothing.
    press(&mut ui, KeyCode::Char('?'));
    press(&mut ui, KeyCode::Up);
    assert_eq!(ui.help_scroll, 0);
    press(&mut ui, KeyCode::Down);
    press(&mut ui, KeyCode::Char('j'));
    assert_eq!(ui.help_scroll, 2);
    assert_eq!(press(&mut ui, KeyCode::Char('p')), Action::None);
    assert_eq!(press(&mut ui, KeyCode::Char('r')), Action::None);
    // Reopening help starts at the top.
    press(&mut ui, KeyCode::Char('?'));
    press(&mut ui, KeyCode::Char('?'));
    assert_eq!(ui.help_scroll, 0);
}

#[test]
fn options_overlay_at_reference_sizes() {
    for (w, h) in SIZES {
        let rows = render(
            w,
            h,
            Mode::Duplex,
            Opts {
                options: Some(0),
                ..Opts::default()
            },
        );
        let t = text(&rows);
        for want in [
            " Options ",
            "SEND PATH",
            "RECEIVE PATH",
            "◂ 24000 bps ▸",
            "applies live",
            "only with fec on",
            "needs restart",
            "buffer ceiling",
            "Opus bitrate for your voice",
        ] {
            assert!(t.contains(want), "{w}×{h} missing {want:?}:\n{t}");
        }
        assert_borders_intact(&rows, w, h);
    }
    // Send-only: the receive-path row says why it can't change.
    let t = text(&render(
        120,
        30,
        Mode::SendOnly,
        Opts {
            options: Some(4),
            ..Opts::default()
        },
    ));
    assert!(t.contains("not receiving"), "{t}");
}

/// A changed setting is marked in the overlay and the Status codec line follows it.
#[test]
fn changed_settings_show_in_overlay_and_status() {
    let mut settings = Settings::from_info(&info("full duplex"));
    settings.apply(Setting::Bitrate(12000));
    settings.apply(Setting::DriftCorrect(true));
    let t = text(&render(
        160,
        42,
        Mode::Duplex,
        Opts {
            options: Some(0),
            settings: Some(settings),
            ..Opts::default()
        },
    ));
    assert!(t.contains("◂ 12000 bps ▸") && t.contains(" changed"), "{t}");
    let t = text(&render(
        160,
        42,
        Mode::Duplex,
        Opts {
            settings: Some(settings),
            ..Opts::default()
        },
    ));
    assert!(
        t.contains("12000 bps") && t.contains("drift correction on"),
        "Status codec line follows the change:\n{t}"
    );
}

#[test]
fn options_keys_send_changes() {
    let mut ui = ui();
    assert_eq!(press(&mut ui, KeyCode::Char('o')), Action::None);
    assert!(ui.options);
    // bitrate is row 0: → steps up, ← steps down.
    assert_eq!(
        press(&mut ui, KeyCode::Right),
        Action::Change(Setting::Bitrate(32000))
    );
    assert_eq!(
        press(&mut ui, KeyCode::Left),
        Action::Change(Setting::Bitrate(20000))
    );
    // ↓ to fec, toggle; ↓ to expected loss: refused while fec is off.
    press(&mut ui, KeyCode::Down);
    assert_eq!(
        press(&mut ui, KeyCode::Enter),
        Action::Change(Setting::Fec(true))
    );
    press(&mut ui, KeyCode::Char('j'));
    assert_eq!(press(&mut ui, KeyCode::Right), Action::None);
    ui.settings.apply(Setting::Fec(true));
    assert_eq!(
        press(&mut ui, KeyCode::Right),
        Action::Change(Setting::ExpectedLoss(15))
    );
    // ↑ from the top wraps to the read-only ceiling row.
    ui.option_sel = 0;
    press(&mut ui, KeyCode::Up);
    assert_eq!(ui.option_sel, COUNT - 1);
    assert_eq!(press(&mut ui, KeyCode::Right), Action::None);
    // p / r do nothing while Options is open; Esc closes it before it quits.
    assert_eq!(press(&mut ui, KeyCode::Char('p')), Action::None);
    assert_eq!(press(&mut ui, KeyCode::Esc), Action::None);
    assert!(!ui.options);
    assert_eq!(press(&mut ui, KeyCode::Esc), Action::Quit);
}

#[test]
fn question_mark_switches_from_options_to_help() {
    let mut ui = ui();
    press(&mut ui, KeyCode::Char('o'));
    press(&mut ui, KeyCode::Char('?'));
    assert!(ui.help && !ui.options);
    press(&mut ui, KeyCode::Esc);
    assert!(!ui.help && !ui.options);
}

#[test]
#[ignore = "prints frames for eyeballing: cargo test -p vox print_frame -- --ignored --nocapture"]
fn print_frame() {
    for (w, h) in SIZES {
        println!("── {w}×{h} ──");
        println!("{}", text(&render(w, h, Mode::Duplex, Opts::default())));
    }
    println!("── 120×30 send-only ──");
    println!(
        "{}",
        text(&render(120, 30, Mode::SendOnly, Opts::default()))
    );
    println!("── 120×30 options ──");
    println!(
        "{}",
        text(&render(
            120,
            30,
            Mode::Duplex,
            Opts {
                options: Some(1),
                ..Opts::default()
            }
        ))
    );
    println!("── 120×30 help ──");
    println!(
        "{}",
        text(&render(
            120,
            30,
            Mode::Duplex,
            Opts {
                help: Some(0),
                ..Opts::default()
            }
        ))
    );
}
