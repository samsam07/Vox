//! `--output tui`: the live full-screen dashboard (ratatui), "Pipeline" layout
//! (PLAN M16). Panels follow the path the audio takes: session (Status) → network
//! (Throughput) → Jitter buffer → Quality. Read-only over the engine; never
//! touches the audio path except through the engine's control handle, which the
//! `o` Options overlay uses to change settings live (DESIGN §12). Keys: `p` freezes
//! the screen (vox keeps counting), `r` resets the totals (in the TUI only), `o`
//! options, `?` help, `q` / Esc / Ctrl+C quit.

mod help;
mod model;
mod options;
mod panels;
mod plot;
#[cfg(test)]
mod tests;

use std::io::IsTerminal;
use std::time::{Duration, Instant};

use anyhow::{bail, Result};
use ratatui::crossterm::event::{self, Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use ratatui::layout::{Constraint, Layout};
use ratatui::{DefaultTerminal, Frame};

use vox_core::{Engine, Setting};

use crate::SessionInfo;
use model::{Baseline, Frozen, Sampler, View};
use options::{Roles, Settings};

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

/// What the keys control.
struct Ui {
    help: bool,
    help_scroll: usize,
    options: bool,
    /// Selected Options row.
    option_sel: usize,
    /// The settings now, and as configured at start (to mark what changed).
    settings: Settings,
    start: Settings,
    roles: Roles,
    /// `Some` while the screen is frozen by `p`.
    paused: Option<Frozen>,
    /// `Some` once the totals have been reset by `r`.
    base: Option<Baseline>,
}

impl Ui {
    fn new(settings: Settings, roles: Roles) -> Self {
        Self {
            help: false,
            help_scroll: 0,
            options: false,
            option_sel: 0,
            settings,
            start: settings,
            roles,
            paused: None,
            base: None,
        }
    }
}

/// Which overlay to draw over the dashboard.
enum Overlay<'a> {
    None,
    Help(&'a mut usize),
    Options {
        sel: usize,
        start: &'a Settings,
        roles: Roles,
    },
}

/// A key's effect beyond the overlay state `key_action` changes itself.
#[derive(Debug, PartialEq)]
enum Action {
    None,
    Quit,
    TogglePause,
    Reset,
    /// A live setting change for the engine (from the Options overlay).
    Change(Setting),
}

/// Map a key press to its effect. Esc closes an open overlay before it quits.
fn key_action(ui: &mut Ui, key: KeyEvent) -> Action {
    if key.code == KeyCode::Char('c') && key.modifiers.contains(KeyModifiers::CONTROL) {
        return Action::Quit;
    }
    if ui.help {
        match key.code {
            KeyCode::Esc | KeyCode::Char('?') => ui.help = false,
            KeyCode::Down | KeyCode::Char('j') => ui.help_scroll += 1,
            KeyCode::Up | KeyCode::Char('k') => {
                ui.help_scroll = ui.help_scroll.saturating_sub(1);
            }
            KeyCode::Char('q') => return Action::Quit,
            _ => {}
        }
        return Action::None;
    }
    if ui.options {
        let change = |ui: &Ui, up: bool| {
            options::change(ui.option_sel, &ui.settings, ui.roles, up)
                .map_or(Action::None, Action::Change)
        };
        return match key.code {
            KeyCode::Esc | KeyCode::Char('o') => {
                ui.options = false;
                Action::None
            }
            KeyCode::Char('?') => {
                ui.options = false;
                ui.help = true;
                ui.help_scroll = 0;
                Action::None
            }
            KeyCode::Down | KeyCode::Char('j') => {
                ui.option_sel = (ui.option_sel + 1) % options::COUNT;
                Action::None
            }
            KeyCode::Up | KeyCode::Char('k') => {
                ui.option_sel = (ui.option_sel + options::COUNT - 1) % options::COUNT;
                Action::None
            }
            KeyCode::Left | KeyCode::Char('h') => change(ui, false),
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Enter => change(ui, true),
            KeyCode::Char('q') => Action::Quit,
            _ => Action::None,
        };
    }
    match key.code {
        KeyCode::Char('q') | KeyCode::Esc => Action::Quit,
        KeyCode::Char('?') => {
            ui.help = true;
            ui.help_scroll = 0;
            Action::None
        }
        KeyCode::Char('o') => {
            ui.options = true;
            Action::None
        }
        KeyCode::Char('p') => Action::TogglePause,
        KeyCode::Char('r') => Action::Reset,
        _ => Action::None,
    }
}

fn run_loop(
    terminal: &mut DefaultTerminal,
    engine: &Engine,
    info: &SessionInfo,
    duration: Option<u64>,
) -> Result<()> {
    let start = Instant::now();
    let deadline = duration.map(|secs| start + Duration::from_secs(secs));
    let mut sampler = Sampler::new(engine.stats());
    let control = engine.control();
    let roles = Roles {
        sends: control.sends(),
        receives: control.receives(),
    };
    let mut ui = Ui::new(Settings::from_info(info), roles);

    loop {
        if deadline.is_some_and(|d| Instant::now() >= d) {
            break;
        }
        if event::poll(Duration::from_millis(100))? {
            if let Event::Key(key) = event::read()? {
                if key.kind == KeyEventKind::Press {
                    match key_action(&mut ui, key) {
                        Action::Quit => break,
                        Action::TogglePause => {
                            ui.paused = match ui.paused.take() {
                                Some(_) => None,
                                None => Some(sampler.freeze(engine.stats(), start.elapsed())),
                            };
                        }
                        Action::Reset => {
                            ui.base = Some(Baseline {
                                stats: engine.stats(),
                                at: start.elapsed(),
                            });
                            ui.paused = None;
                        }
                        Action::Change(setting) => {
                            // Show it only once the engine has taken it; a refusal
                            // (thread gone) leaves the displayed setting as it was.
                            if control.apply(setting).is_ok() {
                                ui.settings.apply(setting);
                                ui.paused = None;
                            }
                        }
                        Action::None => {}
                    }
                }
            }
        }
        // Keep sampling while paused, so un-pausing jumps straight back to live.
        if sampler.due() {
            sampler.sample(engine.stats());
        }

        let snap = match &ui.paused {
            Some(frozen) => frozen.snapshot(),
            None => sampler.snapshot(engine.stats(), start.elapsed()),
        };
        let view = View::new(
            info,
            snap,
            ui.settings,
            ui.base.as_ref(),
            ui.paused.is_some(),
        );
        let overlay = if ui.help {
            Overlay::Help(&mut ui.help_scroll)
        } else if ui.options {
            Overlay::Options {
                sel: ui.option_sel,
                start: &ui.start,
                roles: ui.roles,
            }
        } else {
            Overlay::None
        };
        terminal.draw(|frame| draw(frame, &view, overlay))?;
    }
    Ok(())
}

/// `layoutPipeline`: header, Status, Throughput, Jitter buffer, Quality, footer.
/// With no playback role the buffer row goes and Throughput takes it.
fn draw(frame: &mut Frame, v: &View, overlay: Overlay) {
    let area = frame.area();
    let jitter_row = if v.receives() {
        Constraint::Fill(1)
    } else {
        Constraint::Length(0)
    };
    let [header_a, status_a, throughput_a, jitter_a, quality_a, footer_a] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Length(5),
        Constraint::Fill(1),
        jitter_row,
        Constraint::Length(5),
        Constraint::Length(1),
    ])
    .areas(area);
    panels::header(frame, header_a, v);
    panels::status(frame, status_a, v);
    panels::throughput(frame, throughput_a, v);
    if v.receives() {
        panels::jitter(frame, jitter_a, v);
    }
    panels::quality(frame, quality_a, v);
    panels::footer(frame, footer_a, v);
    match overlay {
        Overlay::None => {}
        Overlay::Help(scroll) => help::overlay(frame, area, scroll),
        Overlay::Options { sel, start, roles } => {
            options::overlay(frame, area, sel, &v.settings, start, roles)
        }
    }
}
