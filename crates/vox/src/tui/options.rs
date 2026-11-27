//! The `o` Options overlay (`optionsOverlay` + `OPTIONS` in the design): change
//! bitrate, fec, expected loss, dtx and drift correction while running, through the
//! engine's control handle (DESIGN §12). The buffer ceiling is shown but needs a
//! restart. Changes last for this session only.

use ratatui::layout::Rect;
use ratatui::style::{Color, Style, Stylize};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Clear, Paragraph};
use ratatui::Frame;

use vox_core::Setting;

use super::help::wrap;
use super::panels::{on_off, panel, ACCENT, DIM};
use crate::SessionInfo;

/// The settings as this session last set them (start: the configured values).
#[derive(Clone, Copy, Debug, PartialEq)]
pub(super) struct Settings {
    pub(super) bitrate: i32,
    pub(super) fec: bool,
    pub(super) expected_loss: u8,
    pub(super) dtx: bool,
    pub(super) drift_correct: bool,
    pub(super) jitter_ms: u32,
}

impl Settings {
    pub(super) fn from_info(info: &SessionInfo) -> Self {
        Self {
            bitrate: info.bitrate,
            fec: info.fec,
            expected_loss: info.expected_loss,
            dtx: info.dtx,
            drift_correct: info.drift_correct,
            jitter_ms: info.jitter_ms,
        }
    }

    /// Record a change the engine accepted.
    pub(super) fn apply(&mut self, setting: Setting) {
        match setting {
            Setting::Bitrate(v) => self.bitrate = v,
            Setting::Fec(v) => self.fec = v,
            Setting::ExpectedLoss(v) => self.expected_loss = v,
            Setting::Dtx(v) => self.dtx = v,
            Setting::DriftCorrect(v) => self.drift_correct = v,
        }
    }
}

/// Which directions are running (only their settings can change).
#[derive(Clone, Copy, Debug)]
pub(super) struct Roles {
    pub(super) sends: bool,
    pub(super) receives: bool,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Item {
    Bitrate,
    Fec,
    ExpectedLoss,
    Dtx,
    DriftCorrect,
    Ceiling,
}

struct Row {
    item: Item,
    label: &'static str,
    group: Option<&'static str>,
    help: &'static str,
}

const ROWS: [Row; 6] = [
    Row {
        item: Item::Bitrate,
        label: "bitrate",
        group: Some("send path · what the peer hears from you"),
        help: "Opus bitrate for your voice. Higher = better quality, more bandwidth. 24000 is plenty for speech.",
    },
    Row {
        item: Item::Fec,
        label: "fec",
        group: None,
        help: "Forward error correction: each packet carries a copy of the previous frame so the peer can rebuild one lost frame. Costs bitrate and makes packets bigger: turn on only for lossy links.",
    },
    Row {
        item: Item::ExpectedLoss,
        label: "expected loss",
        group: None,
        help: "How much loss vox plans for when fec is on. Higher = more redundancy, less bitrate for the voice itself.",
    },
    Row {
        item: Item::Dtx,
        label: "dtx",
        group: None,
        help: "Discontinuous transmission: silent frames are sent tiny. Saves bandwidth when you are quiet. vox still sends every frame, so gaps stay meaningful.",
    },
    Row {
        item: Item::DriftCorrect,
        label: "drift correction",
        group: Some("receive path · what you hear"),
        help: "Adjust playback speed by a tiny, inaudible amount to hold the buffer at its target, instead of drop/hold recenters. Turn on if drift is steady (see the Jitter buffer drift value).",
    },
    Row {
        item: Item::Ceiling,
        label: "buffer ceiling",
        group: None,
        help: "The most audio the buffer can hold. It is set when vox starts, so changing it needs a restart.",
    },
];

/// Number of rows the selection cycles through.
pub(super) const COUNT: usize = ROWS.len();

const BITRATES: [i32; 8] = [12000, 16000, 20000, 24000, 32000, 40000, 48000, 64000];
const LOSSES: [u8; 7] = [0, 5, 10, 15, 20, 25, 30];

/// Why a row can or can't be changed right now.
#[derive(PartialEq, Debug)]
enum Availability {
    Live,
    NeedsFec,
    Restart,
    NotSending,
    NotReceiving,
}

impl Availability {
    fn note(&self) -> &'static str {
        match self {
            Availability::Live => "applies live",
            Availability::NeedsFec => "only with fec on",
            Availability::Restart => "needs restart",
            Availability::NotSending => "not sending",
            Availability::NotReceiving => "not receiving",
        }
    }
}

fn availability(item: Item, s: &Settings, roles: Roles) -> Availability {
    match item {
        Item::Ceiling => Availability::Restart,
        Item::DriftCorrect if !roles.receives => Availability::NotReceiving,
        Item::DriftCorrect => Availability::Live,
        _ if !roles.sends => Availability::NotSending,
        Item::ExpectedLoss if !s.fec => Availability::NeedsFec,
        _ => Availability::Live,
    }
}

fn value_text(item: Item, s: &Settings) -> String {
    match item {
        Item::Bitrate => format!("{} bps", s.bitrate),
        Item::Fec => on_off(s.fec).into(),
        Item::ExpectedLoss => format!("{} %", s.expected_loss),
        Item::Dtx => on_off(s.dtx).into(),
        Item::DriftCorrect => on_off(s.drift_correct).into(),
        Item::Ceiling => format!("{} ms", s.jitter_ms),
    }
}

/// The next step from `cur` in direction `up` (a configured value between steps
/// moves to the neighbouring step), or `None` at the end of the range.
fn step<T: Copy + PartialOrd>(steps: &[T], cur: T, up: bool) -> Option<T> {
    if up {
        steps.iter().copied().find(|&v| v > cur)
    } else {
        steps.iter().rev().copied().find(|&v| v < cur)
    }
}

/// The change ←/→ makes on row `sel`, if that row can change and isn't at the end
/// of its range. On/off rows toggle either way.
pub(super) fn change(sel: usize, s: &Settings, roles: Roles, up: bool) -> Option<Setting> {
    let item = ROWS.get(sel)?.item;
    if availability(item, s, roles) != Availability::Live {
        return None;
    }
    match item {
        Item::Bitrate => step(&BITRATES, s.bitrate, up).map(Setting::Bitrate),
        Item::Fec => Some(Setting::Fec(!s.fec)),
        Item::ExpectedLoss => step(&LOSSES, s.expected_loss, up).map(Setting::ExpectedLoss),
        Item::Dtx => Some(Setting::Dtx(!s.dtx)),
        Item::DriftCorrect => Some(Setting::DriftCorrect(!s.drift_correct)),
        Item::Ceiling => None,
    }
}

/// Draw the overlay centred over `area`.
pub(super) fn overlay(
    frame: &mut Frame,
    area: Rect,
    sel: usize,
    now: &Settings,
    start: &Settings,
    roles: Roles,
) {
    let w = area.width.saturating_sub(4).min(92);
    if w < 24 || area.height < 5 {
        return;
    }
    let hi = Style::new().fg(Color::Black).bg(ACCENT).bold();
    let mut rows: Vec<Line> = Vec::new();
    for (k, row) in ROWS.iter().enumerate() {
        if let Some(group) = row.group {
            if k > 0 {
                rows.push(Line::from(""));
            }
            rows.push(Line::from(group.to_uppercase().fg(ACCENT).bold()));
        }
        let avail = availability(row.item, now, roles);
        let fixed = avail != Availability::Live;
        let changed = value_text(row.item, now) != value_text(row.item, start);
        let selected = k == sel;
        let value = if row.item == Item::Ceiling || fixed {
            format!("  {}  ", value_text(row.item, now))
        } else {
            format!("◂ {} ▸", value_text(row.item, now))
        };
        let pick = |normal: Style| if selected { hi } else { normal };
        let label_style = pick(if fixed {
            DIM.into()
        } else {
            Style::new().bold()
        });
        let value_style = pick(if changed {
            Style::new().fg(Color::Yellow).bold()
        } else if fixed {
            DIM.into()
        } else {
            Style::new()
        });
        rows.push(Line::from(vec![
            Span::styled(if selected { "▸ " } else { "  " }, pick(Style::new())),
            Span::styled(format!("{:<18}", row.label), label_style),
            Span::styled(format!("{value:<16}"), value_style),
            if changed {
                " changed".yellow()
            } else {
                "        ".into()
            },
            format!("   {}", avail.note()).fg(DIM),
        ]));
    }
    rows.push(Line::from(""));
    let help = ROWS.get(sel).map_or("", |r| r.help);
    rows.extend(
        wrap(help, (w as usize).saturating_sub(4))
            .into_iter()
            .map(Line::from),
    );

    let h = (area.height - 2).min(rows.len() as u16 + 2);
    let r = Rect::new(
        area.x + (area.width - w) / 2,
        area.y + ((area.height - h) / 2).max(1),
        w,
        h,
    );
    frame.render_widget(Clear, r);
    let block = panel("Options")
        .title_top(Line::from(" ↑↓ select · ←→ change · o / Esc close ".fg(DIM)).right_aligned());
    let inner = block.inner(r);
    frame.render_widget(block, r);
    frame.render_widget(Paragraph::new(rows), inner);
}

#[cfg(test)]
mod tests {
    use super::*;

    const BOTH: Roles = Roles {
        sends: true,
        receives: true,
    };

    fn settings() -> Settings {
        Settings {
            bitrate: 24000,
            fec: false,
            expected_loss: 10,
            dtx: false,
            drift_correct: false,
            jitter_ms: 250,
        }
    }

    fn row(item: Item) -> usize {
        ROWS.iter().position(|r| r.item == item).unwrap()
    }

    #[test]
    fn bitrate_steps_and_stops_at_the_ends() {
        let mut s = settings();
        let up = change(row(Item::Bitrate), &s, BOTH, true);
        assert_eq!(up, Some(Setting::Bitrate(32000)));
        s.bitrate = 30000; // configured between steps
        assert_eq!(
            change(row(Item::Bitrate), &s, BOTH, false),
            Some(Setting::Bitrate(24000))
        );
        s.bitrate = 64000;
        assert_eq!(change(row(Item::Bitrate), &s, BOTH, true), None);
    }

    #[test]
    fn toggles_and_restrictions() {
        let mut s = settings();
        assert_eq!(
            change(row(Item::Fec), &s, BOTH, false),
            Some(Setting::Fec(true))
        );
        assert_eq!(
            change(row(Item::ExpectedLoss), &s, BOTH, true),
            None,
            "needs fec"
        );
        s.fec = true;
        assert_eq!(
            change(row(Item::ExpectedLoss), &s, BOTH, true),
            Some(Setting::ExpectedLoss(15))
        );
        assert_eq!(
            change(row(Item::Ceiling), &s, BOTH, true),
            None,
            "needs restart"
        );
        let send_only = Roles {
            sends: true,
            receives: false,
        };
        assert_eq!(change(row(Item::DriftCorrect), &s, send_only, true), None);
        let receive_only = Roles {
            sends: false,
            receives: true,
        };
        assert_eq!(change(row(Item::Dtx), &s, receive_only, true), None);
        assert_eq!(
            change(row(Item::DriftCorrect), &s, receive_only, true),
            Some(Setting::DriftCorrect(true))
        );
    }

    #[test]
    fn settings_record_accepted_changes() {
        let mut s = settings();
        s.apply(Setting::Bitrate(16000));
        s.apply(Setting::DriftCorrect(true));
        assert_eq!((s.bitrate, s.drift_correct), (16000, true));
    }
}
