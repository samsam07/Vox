//! The `?` help overlay (`helpOverlay` + `HELP` in the design): what each number
//! means, in plain words. Scrolls with ↑↓ / j k when it doesn't fit.

use ratatui::layout::Rect;
use ratatui::style::Stylize;
use ratatui::text::Line;
use ratatui::widgets::{Clear, Paragraph};
use ratatui::Frame;

use super::panels::{panel, put, ACCENT, DIM};

pub(super) enum Entry {
    /// A section heading with an optional dim subtitle.
    Section(&'static str, Option<&'static str>),
    /// A term and its explanation.
    Term(&'static str, &'static str),
}

use Entry::{Section, Term};

pub(super) const HELP: &[Entry] = &[
    Section("Link", None),
    Term("tx / rx", "Audio you send to the peer / receive from it. One packet is one 20 ms Opus frame, so about 50 pkt/s each way."),
    Term("peak", "Highest rate in the time shown on the graph. The graph scale follows it."),
    Section("Jitter buffer", Some("receive side: network → buffer → speakers")),
    Term("fill", "Audio queued for playback, in ms. This is the delay vox adds on your end."),
    Term("target", "The depth vox aims for (▲ on the gauge). It adapts to measured network jitter: shallow on a clean link, deeper on a bursty one."),
    Term("ceiling", "The most the buffer can hold (250 ms by default). The right end of the gauge."),
    Term("drift", "How fast fill is moving, in ms per minute. A steady climb or fall means the two machines' audio clocks run at slightly different speeds. Turn on drift correction in Options (o) to fix it smoothly."),
    Term("recenter", "Fill hit the edge of its band and vox pushed it back. drop = one 20 ms frame discarded (too full), hold = one frame repeated (too empty). Each is a tiny cut; an occasional one under drift is normal."),
    Term("underrun", "The buffer ran dry: you hear a break. vox then stays silent until it refills to the target, so it doesn't break again straight away."),
    Section("Quality", Some("big number = last 30 s · stripe colour = status: green none, yellow concealed, red audible")),
    Term("loss", "Share of frames that never arrived: gaps ÷ (received + gaps)."),
    Term("gaps", "Missing frames that vox filled in (FEC when it is on, otherwise Opus concealment)."),
    Term("late", "Packets that arrived after their slot had already played. They are discarded."),
    Term("overrun", "The jitter buffer was full and frames were dropped. capture = mic audio dropped because vox fell behind sending it."),
    Section("Keys", None),
    Term("p", "Pause the screen to read a spike. vox keeps running and counting; p again jumps back to live."),
    Term("r", "Reset the totals to zero, e.g. before testing a fix. The 30 s numbers keep working."),
    Term(
        "o",
        "Options: change bitrate, fec, expected loss, dtx and drift correction while running. Changes last until vox exits.",
    ),
    Section("Health", None),
    Term("● ▲ ✕", "ok · warning · problem, judged on the last 30 s: no audio from the peer, underruns, loss, drift, buffer near full."),
];

/// Width of the term column (plus two spaces before the explanation).
const TERM_W: usize = 10;

/// Greedy word wrap to `width` characters.
pub(super) fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    for word in text.split(' ') {
        if !cur.is_empty() && cur.chars().count() + 1 + word.chars().count() > width {
            out.push(std::mem::take(&mut cur));
        }
        if !cur.is_empty() {
            cur.push(' ');
        }
        cur.push_str(word);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// The help text laid out for an explanation column `dw` characters wide.
fn rows(dw: usize) -> Vec<Line<'static>> {
    let mut rows = Vec::new();
    for (k, entry) in HELP.iter().enumerate() {
        match entry {
            Section(title, sub) => {
                if k > 0 {
                    rows.push(Line::from(""));
                }
                let mut spans = vec![title.to_uppercase().fg(ACCENT).bold()];
                if let Some(sub) = sub {
                    spans.push(format!("   {sub}").fg(DIM));
                }
                rows.push(Line::from(spans));
            }
            Term(term, text) => {
                for (j, line) in wrap(text, dw).into_iter().enumerate() {
                    let term = if j == 0 { *term } else { "" };
                    rows.push(Line::from(vec![
                        format!("{term:<w$}", w = TERM_W + 2).bold(),
                        line.into(),
                    ]));
                }
            }
        }
    }
    rows
}

/// Draw the overlay centred over `area`. `scroll` is clamped to what fits.
pub(super) fn overlay(frame: &mut Frame, area: Rect, scroll: &mut usize) {
    let w = area.width.saturating_sub(4).min(100);
    if w < 24 || area.height < 5 {
        return;
    }
    let dw = (w as usize).saturating_sub(4 + TERM_W + 2).max(1);
    let rows = rows(dw);
    let h = (area.height - 2).min(rows.len() as u16 + 2);
    let r = Rect::new(
        area.x + (area.width - w) / 2,
        area.y + ((area.height - h) / 2).max(1),
        w,
        h,
    );
    let visible = (h - 2) as usize;
    let max_scroll = rows.len().saturating_sub(visible);
    *scroll = (*scroll).min(max_scroll);
    let sc = *scroll;
    let hint = if max_scroll > 0 {
        format!(
            " ↑↓ scroll {}/{} · ? / Esc close ",
            sc + visible,
            rows.len()
        )
    } else {
        " ? / Esc close ".to_string()
    };
    frame.render_widget(Clear, r);
    let block =
        panel("Help · what the numbers mean").title_top(Line::from(hint.fg(DIM)).right_aligned());
    let inner = block.inner(r);
    frame.render_widget(block, r);
    frame.render_widget(Paragraph::new(rows[sc..].to_vec()), inner);
    let buf = frame.buffer_mut();
    let arrow_x = r.right().saturating_sub(3);
    if sc < max_scroll {
        put(
            buf,
            arrow_x,
            r.bottom() - 1,
            &Line::from("▼".fg(ACCENT).bold()),
            r,
        );
    }
    if sc > 0 {
        put(buf, arrow_x, r.y, &Line::from("▲".fg(ACCENT).bold()), r);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wrap_keeps_words_whole() {
        assert_eq!(wrap("aa bb cc", 5), ["aa bb", "cc"]);
        assert_eq!(wrap("toolongword x", 4), ["toolongword", "x"]);
        assert!(wrap("", 10).is_empty());
    }
}
