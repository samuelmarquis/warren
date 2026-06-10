//! Dashboard compositor: sidebar + content pane + status bar, emitted as raw
//! ANSI into one buffer per frame (wrapped in synchronized-update marks).
//! Region-level diffing: each frame only repaints what's flagged dirty.

use std::fmt::Write;

use crate::spans::{self, LineSpans, Span};

use super::{Dash, Mode, Sub};

pub const SIDEBAR_WIDTH: u16 = 24;

pub fn paint(dash: &mut Dash) -> String {
    let mut out = String::new();
    let _ = write!(out, "\x1b[?2026h\x1b[?25l"); // sync update, cursor hidden while painting

    if dash.full_redraw {
        let _ = write!(out, "\x1b[0m\x1b[2J");
        dash.sidebar_dirty = true;
        dash.status_dirty = true;
        dash.form_dirty = true;
        if let Some(a) = dash.focused_mut() {
            a.full_dirty = true;
        }
    }

    if dash.sidebar_dirty {
        draw_sidebar(dash, &mut out);
    }
    if dash.editform.is_some() {
        if dash.form_dirty {
            super::forms::draw_edit_form(dash, &mut out);
            dash.form_dirty = false;
        }
    } else if dash.on_newform() {
        if dash.form_dirty {
            super::forms::draw_new_form(dash, &mut out);
            dash.form_dirty = false;
        }
    } else {
        draw_content(dash, &mut out);
    }
    if dash.status_dirty {
        draw_status(dash, &mut out);
    }

    place_cursor(dash, &mut out);
    let _ = write!(out, "\x1b[?2026l");
    dash.full_redraw = false;
    dash.sidebar_dirty = false;
    dash.status_dirty = false;
    out
}

fn draw_sidebar(dash: &mut Dash, out: &mut String) {
    let rows = dash.rows.saturating_sub(1);
    let text_w = (SIDEBAR_WIDTH - 1) as usize;
    for row in 0..rows {
        let _ = write!(out, "\x1b[{};1H", row + 1);
        if row as usize == dash.agents.len() {
            // The pinned "+ new agent" tab.
            let focused = dash.on_newform();
            let style = if focused { "\x1b[0;7m" } else { "\x1b[0;2m" };
            let mut label = "   + new agent".to_string();
            label.truncate(text_w);
            let pad = text_w.saturating_sub(label.chars().count());
            let _ = write!(out, "{style}{label}{}\x1b[0m{SEP_STYLE}│\x1b[0m", " ".repeat(pad));
            continue;
        }
        if let Some(agent) = dash.agents.get(row as usize) {
            let focused = dash.focus == row as usize;
            let number = match row + 1 {
                10 => "0".to_string(),
                n if n < 10 => n.to_string(),
                _ => " ".to_string(),
            };
            // Trailing mark: '!' = blocked on a permission prompt and quiet;
            // '*' = went idle while unfocused, not yet examined.
            let mark = if agent.needs_attention() {
                " !"
            } else if agent.unseen {
                " *"
            } else {
                ""
            };
            let name_w = text_w.saturating_sub(3 + mark.len());
            let name: String = agent.meta.display.chars().take(name_w).collect();
            let label = format!(" {number} {name}{mark}");
            let pad = text_w.saturating_sub(label.chars().count());

            let color = agent.meta.color;
            let mut style = String::from("\x1b[0");
            if agent.busy() {
                style.push_str(";2"); // dim while working
            } else {
                style.push_str(";1"); // bold when ready
            }
            if focused {
                if color != 0 {
                    let (r, g, b) = spans::xterm256_to_rgb(color);
                    let fg = if spans::color_is_dark(r, g, b) { 15 } else { 0 };
                    let _ = write!(style, ";48;5;{color};38;5;{fg}");
                } else {
                    style.push_str(";7");
                }
            } else if color != 0 {
                let _ = write!(style, ";38;5;{color}");
            }
            style.push('m');
            let _ = write!(out, "{style}{label}{}\x1b[0m", " ".repeat(pad));
        } else {
            let _ = write!(out, "\x1b[0m{}", " ".repeat(text_w));
        }
        let _ = write!(out, "{SEP_STYLE}│\x1b[0m");
    }
}

/// Sidebar divider color — matched to the gray Claude uses for its
/// text-entry box borders.
const SEP_STYLE: &str = "\x1b[0;38;5;240m";

/// Horizontal box-drawing leads that should joint into the divider with '├'.
fn joins_divider(c: char) -> bool {
    matches!(c, '\u{2500}' | '\u{2501}' | '\u{254c}' | '\u{2504}' | '\u{2508}' | '\u{2574}' | '\u{2576}')
}

fn draw_content(dash: &mut Dash, out: &mut String) {
    let pane_w = dash.cols.saturating_sub(SIDEBAR_WIDTH) as usize;
    let pane_h = dash.rows.saturating_sub(1);
    let x0 = SIDEBAR_WIDTH + 1; // 1-based ANSI column

    let focus = dash.focus;
    let Some(agent) = dash.agents.get_mut(focus) else {
        return; // + tab focused: the form renderer owns the pane
    };

    if agent.full_dirty {
        for row in 0..pane_h {
            let line = agent.grid.get(row as usize);
            draw_pane_line(out, row, x0, pane_w, line);
        }
        agent.full_dirty = false;
        agent.damage_rows.clear();
    } else {
        let rows: Vec<u16> = agent.damage_rows.drain(..).collect();
        for row in rows {
            if row < pane_h {
                let line = agent.grid.get(row as usize);
                draw_pane_line(out, row, x0, pane_w, line);
            }
        }
    }
}

fn draw_pane_line(out: &mut String, row: u16, x0: u16, width: usize, line: Option<&LineSpans>) {
    // The divider cell to our left: '├' (in the line's own color) where a
    // horizontal rule in the agent's UI meets the sidebar, plain '│' elsewhere.
    let first_span = line.and_then(|l| l.0.first());
    let joins = first_span
        .and_then(|s| s.text.chars().next())
        .map(joins_divider)
        .unwrap_or(false);
    if joins {
        let rule = first_span.unwrap();
        let style = spans::sgr_sequence(&Span { text: String::new(), ..rule.clone() });
        let _ = write!(out, "\x1b[{};{}H{style}├\x1b[0m", row + 1, x0 - 1);
    } else {
        let _ = write!(out, "\x1b[{};{}H{SEP_STYLE}│\x1b[0m", row + 1, x0 - 1);
    }

    let _ = write!(out, "\x1b[{};{}H\x1b[0m\x1b[K", row + 1, x0);
    let Some(line) = line else { return };
    let mut budget = width;
    for span in &line.0 {
        if budget == 0 {
            break;
        }
        let text: String = span.text.chars().take(budget).collect();
        budget -= text.chars().count();
        let _ = write!(out, "{}{}", spans::sgr_sequence(span), text);
    }
    let _ = write!(out, "\x1b[0m");
}

// Mode-chip styles: CLAUDE = Anthropic orange (#D97757), NORMAL = green,
// EDIT = purple; the rest of the bar is dark gray with light text.
const CHIP_CLAUDE: &str = "\x1b[0;48;2;217;119;87;38;5;16;1m";
const CHIP_NORMAL: &str = "\x1b[0;48;5;34;38;5;16;1m";
const CHIP_EDIT: &str = "\x1b[0;48;5;93;38;5;231;1m";
const BAR_BODY: &str = "\x1b[0;48;5;236;38;5;252m";

fn draw_status(dash: &mut Dash, out: &mut String) {
    let row = dash.rows;
    let width = dash.cols as usize;

    // Editing — an agent's metadata or the new-agent form — is EDIT mode;
    // typing into Claude is CLAUDE mode; navigating is NORMAL.
    let editing = dash.editform.is_some() || (dash.on_newform() && dash.mode == Mode::Insert);
    let (chip, chip_style) = if editing {
        ("EDIT", CHIP_EDIT)
    } else if dash.mode == Mode::Insert {
        ("CLAUDE", CHIP_CLAUDE)
    } else {
        ("NORMAL", CHIP_NORMAL)
    };

    let body = if let Some(flash) = dash.flash.take() {
        format!(" {flash}")
    } else if matches!(dash.sub, Sub::Cmd) {
        format!(
            " :{}\u{2588}   q detach · q! quit+kill all · color #hex/index",
            dash.cmdline
        )
    } else if matches!(dash.sub, Sub::Rename) {
        format!(" rename> {}\u{2588}   Enter save · Esc cancel", dash.cmdline)
    } else if matches!(dash.sub, Sub::Kill) {
        let name = dash.focused().map(|a| a.meta.display.clone()).unwrap_or_default();
        format!(" close agent '{name}'?  [y] yes · [n] no")
    } else if dash.editform.is_some() {
        " Tab field · type / arrows · Enter save · Esc cancel".to_string()
    } else if dash.on_newform() {
        match dash.mode {
            Mode::Insert => " Tab field · h/l mode · type · Enter create · Esc done".to_string(),
            Mode::Normal => " l/i/Enter edit form · j/k move · n new · :q quit".to_string(),
        }
    } else {
        match dash.mode {
            Mode::Normal => {
                " j/k move · 1-9 jump · i claude · r rename · e edit · x close · : cmd".to_string()
            }
            Mode::Insert => {
                let name = dash
                    .focused()
                    .map(|a| a.meta.display.clone())
                    .unwrap_or_else(|| "—".to_string());
                format!(" {name}  ·  ^Space normal mode · ^\\ detach")
            }
        }
    };

    let chip_text = format!(" {chip} ");
    let body: String = body.chars().take(width.saturating_sub(chip_text.len())).collect();
    let pad = width.saturating_sub(chip_text.len() + body.chars().count());
    let _ = write!(
        out,
        "\x1b[{row};1H{chip_style}{chip_text}{BAR_BODY}{body}{}\x1b[0m",
        " ".repeat(pad)
    );
}

fn place_cursor(dash: &Dash, out: &mut String) {
    if dash.editform.is_some() || dash.on_newform() {
        return; // forms draw their own block cursor glyph
    }
    if let (Mode::Insert, Some(agent)) = (&dash.mode, dash.focused()) {
        if agent.cursor_visible && agent.exited.is_none() {
            let (row, col) = agent.cursor;
            let _ = write!(out, "\x1b[{};{}H\x1b[?25h", row + 1, col + 1 + SIDEBAR_WIDTH);
            return;
        }
    }
    // NORMAL mode / no agent: cursor stays hidden.
}

/// Convenience used by tests: render one span row to a plain string.
#[allow(dead_code)]
pub fn line_text(line: &LineSpans) -> String {
    line.0.iter().map(|s| s.text.as_str()).collect()
}

#[allow(dead_code)]
pub fn plain_line(text: &str) -> LineSpans {
    LineSpans(vec![Span::plain(text)])
}
