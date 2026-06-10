//! Modal input for the dashboard.
//!
//! The dashboard reads RAW bytes from the host terminal and, in INSERT mode,
//! forwards them verbatim to the focused agent — no decode/re-encode, exactly
//! v0 dvtm's approach, so every key claude understands keeps working.
//! Ctrl-Space (NUL) toggles NORMAL mode, which is parsed just enough for the
//! nav keys; `:` opens a one-line command editor in the status bar.
//!
//! When the "+ new agent" tab is focused, INSERT edits the form instead of
//! typing into an agent; NORMAL still navigates the whole dashboard, so the
//! form never traps you.

use super::render::SIDEBAR_WIDTH;
use super::{forms, Dash, Mode, Sub};
use crate::proto::{MouseKind, ToDaemon};

pub enum Outcome {
    Continue,
    /// Detach: leave agents running.
    Quit,
    /// Kill every agent, then exit.
    QuitKillAll,
}

const CTRL_SPACE: u8 = 0x00;
const CTRL_BACKSLASH: u8 = 0x1c;
const ESC: u8 = 0x1b;

pub fn handle_bytes(dash: &mut Dash, bytes: &[u8]) -> Outcome {
    let mut i = 0;
    while i < bytes.len() {
        // SGR mouse reports can interleave with anything; route them first.
        if let Some((mouse, used)) = parse_sgr_mouse(&bytes[i..]) {
            handle_mouse(dash, mouse);
            i += used;
            continue;
        }
        // The edit form captures everything until Enter/Esc (v0's SUB_EDIT).
        if dash.editform.is_some() {
            i += forms::edit_key(dash, &bytes[i..]).max(1);
            continue;
        }
        match dash.mode {
            Mode::Insert if dash.on_newform() => {
                if bytes[i] == CTRL_SPACE {
                    dash.enter_normal();
                    i += 1;
                } else {
                    i += forms::new_key(dash, &bytes[i..]).max(1);
                }
            }
            Mode::Insert => {
                // Forward verbatim up to a control toggle or a mouse report.
                let mut stop = bytes.len();
                for (p, &b) in bytes[i..].iter().enumerate() {
                    if b == CTRL_SPACE
                        || b == CTRL_BACKSLASH
                        || (b == ESC && bytes[i + p..].starts_with(b"\x1b[<"))
                    {
                        stop = i + p;
                        break;
                    }
                }
                if stop > i {
                    dash.send_input(&bytes[i..stop]);
                }
                if stop == bytes.len() {
                    return Outcome::Continue;
                }
                match bytes[stop] {
                    CTRL_SPACE => dash.enter_normal(),
                    CTRL_BACKSLASH => return Outcome::Quit,
                    ESC => {
                        i = stop; // mouse report; reparsed at loop top
                        continue;
                    }
                    _ => unreachable!(),
                }
                i = stop + 1;
            }
            Mode::Normal => {
                let (consumed, outcome) = match dash.sub {
                    Sub::None => normal_key(dash, &bytes[i..]),
                    Sub::Cmd => cmd_key(dash, &bytes[i..]),
                    Sub::Rename => rename_key(dash, &bytes[i..]),
                    Sub::Kill => kill_key(dash, &bytes[i..]),
                };
                if let Some(o) = outcome {
                    return o;
                }
                i += consumed.max(1);
            }
        }
    }
    Outcome::Continue
}

/// One NORMAL-mode keypress; returns (bytes consumed, outcome).
fn normal_key(dash: &mut Dash, bytes: &[u8]) -> (usize, Option<Outcome>) {
    // Arrow keys arrive as CSI sequences.
    if bytes.len() >= 3 && bytes[0] == ESC && bytes[1] == b'[' {
        match bytes[2] {
            b'A' => dash.focus_prev(),
            b'B' => dash.focus_next(),
            b'C' => dash.enter_insert(),
            b'D' => {}
            _ => {}
        }
        return (3, None);
    }
    match bytes[0] {
        CTRL_SPACE | b'i' | b'a' | b'\r' | b'l' | ESC => dash.enter_insert(),
        CTRL_BACKSLASH => return (1, Some(Outcome::Quit)),
        b'j' => dash.focus_next(),
        b'k' => dash.focus_prev(),
        b'g' => dash.focus_first(),
        b'G' => dash.focus_last(),
        b'1'..=b'9' => dash.focus_slot(bytes[0] - b'0'),
        b'0' => dash.focus_slot(10),
        // Shift+digit: swap the focused agent with sidebar row N.
        b'!' => dash.swap_with_row(1),
        b'@' => dash.swap_with_row(2),
        b'#' => dash.swap_with_row(3),
        b'$' => dash.swap_with_row(4),
        b'%' => dash.swap_with_row(5),
        b'^' => dash.swap_with_row(6),
        b'&' => dash.swap_with_row(7),
        b'*' => dash.swap_with_row(8),
        b'(' => dash.swap_with_row(9),
        b')' => dash.swap_with_row(10),
        b'n' => dash.open_new_form(),
        b'r' => {
            if let Some(agent) = dash.focused() {
                dash.cmdline = agent.meta.display.clone();
                dash.sub = Sub::Rename;
                dash.status_dirty = true;
            }
        }
        b'e' | b'c' => {
            if let Some(agent) = dash.focused() {
                dash.editform = Some(forms::EditForm {
                    field: if bytes[0] == b'c' { 1 } else { 0 },
                    title: agent.meta.display.clone(),
                    color: agent.meta.color as u16,
                });
                dash.form_dirty = true;
                dash.status_dirty = true; // mode chip flips to EDIT
            }
        }
        b'x' => {
            if dash.focused().is_some() {
                dash.sub = Sub::Kill;
                dash.status_dirty = true;
            }
        }
        b':' => {
            dash.sub = Sub::Cmd;
            dash.cmdline.clear();
            dash.status_dirty = true;
        }
        _ => {}
    }
    (1, None)
}

/// `:` command-line editing in the status bar.
fn cmd_key(dash: &mut Dash, bytes: &[u8]) -> (usize, Option<Outcome>) {
    dash.status_dirty = true;
    match bytes[0] {
        b'\r' => {
            let cmd = std::mem::take(&mut dash.cmdline);
            dash.sub = Sub::None;
            let outcome = run_command(dash, cmd.trim());
            (1, outcome)
        }
        ESC => {
            dash.cmdline.clear();
            dash.sub = Sub::None;
            (1, None)
        }
        0x7f | 0x08 => {
            if dash.cmdline.pop().is_none() {
                dash.sub = Sub::None; // backspace on empty closes, like v0
            }
            (1, None)
        }
        b @ 0x20..=0x7e => {
            dash.cmdline.push(b as char);
            (1, None)
        }
        _ => (1, None),
    }
}

fn rename_key(dash: &mut Dash, bytes: &[u8]) -> (usize, Option<Outcome>) {
    dash.status_dirty = true;
    match bytes[0] {
        b'\r' => {
            let name = std::mem::take(&mut dash.cmdline);
            dash.sub = Sub::None;
            let name = name.trim().to_string();
            if !name.is_empty() {
                if let Some(agent) = dash.focused_mut() {
                    // A manual rename pins the name against title auto-sync.
                    agent.send(&ToDaemon::SetMeta {
                        name: Some(name),
                        color: None,
                        pinned: Some(true),
                        slot: None,
                    });
                }
            }
        }
        ESC => {
            dash.cmdline.clear();
            dash.sub = Sub::None;
        }
        0x7f | 0x08 => {
            dash.cmdline.pop();
        }
        b @ 0x20..=0x7e => dash.cmdline.push(b as char),
        _ => {}
    }
    (1, None)
}

fn kill_key(dash: &mut Dash, bytes: &[u8]) -> (usize, Option<Outcome>) {
    dash.status_dirty = true;
    dash.sub = Sub::None;
    if matches!(bytes[0], b'y' | b'Y') {
        if let Some(agent) = dash.focused_mut() {
            agent.send(&ToDaemon::Kill);
        }
    }
    (1, None)
}

fn run_command(dash: &mut Dash, cmd: &str) -> Option<Outcome> {
    match cmd {
        "q" | "qa" | "wq" => return Some(Outcome::Quit),
        "q!" | "qa!" => return Some(Outcome::QuitKillAll),
        "" => return None,
        _ => {}
    }
    if let Some(arg) = cmd.strip_prefix("color ").or_else(|| cmd.strip_prefix("c ")) {
        match parse_color(arg.trim()) {
            Some(color) => {
                if let Some(agent) = dash.focused_mut() {
                    agent.send(&ToDaemon::SetMeta {
                        name: None,
                        color: Some(color),
                        pinned: None,
                        slot: None,
                    });
                }
            }
            None => dash.flash = Some(format!("bad color '{}' (index 0-255 or #rrggbb)", arg)),
        }
        return None;
    }
    dash.flash = Some(format!("unknown command :{cmd}"));
    None
}

// ---------------------------------------------------------------------- mouse

pub struct MouseReport {
    pub code: u32,
    /// 0-based screen cell.
    pub col: u16,
    pub row: u16,
    pub press: bool,
}

/// Parse one SGR mouse report: ESC [ < code ; col ; row (M|m).
fn parse_sgr_mouse(bytes: &[u8]) -> Option<(MouseReport, usize)> {
    let rest = bytes.strip_prefix(b"\x1b[<")?;
    let end = rest.iter().position(|&b| b == b'M' || b == b'm')?;
    let body = std::str::from_utf8(&rest[..end]).ok()?;
    let mut parts = body.split(';');
    let code: u32 = parts.next()?.parse().ok()?;
    let col: u16 = parts.next()?.parse().ok()?;
    let row: u16 = parts.next()?.parse().ok()?;
    Some((
        MouseReport {
            code,
            col: col.saturating_sub(1),
            row: row.saturating_sub(1),
            press: rest[end] == b'M',
        },
        3 + end + 1,
    ))
}

fn handle_mouse(dash: &mut Dash, ev: MouseReport) {
    let wheel = ev.code & 64 != 0;
    let motion = ev.code & 32 != 0;
    let button = (ev.code & 3) as u8;
    let mods = (ev.code & 0b11100) as u8;

    // Over the sidebar: wheel cycles focus, click focuses a row.
    if ev.col < SIDEBAR_WIDTH {
        if wheel && ev.press {
            if ev.code & 1 == 0 {
                dash.focus_prev();
            } else {
                dash.focus_next();
            }
        } else if ev.press && !motion && button == 0 {
            let row = ev.row as usize;
            if row <= dash.agents.len() {
                dash.set_focus(row);
            }
        }
        return;
    }

    // Over a form: palette swatch clicks.
    if dash.editform.is_some() || dash.on_newform() {
        if ev.press && !motion && !wheel && button == 0 {
            forms::palette_click(dash, ev.row, ev.col);
        }
        return;
    }

    // Over the content pane: forward semantically; the daemon encodes it
    // only if the app subscribed (fullscreen Claude scrolls itself this way).
    let kind = if wheel {
        if !ev.press {
            return;
        }
        if ev.code & 1 == 0 { MouseKind::WheelUp } else { MouseKind::WheelDown }
    } else if motion {
        if button == 3 { MouseKind::Moved } else { MouseKind::Drag(button) }
    } else if ev.press {
        MouseKind::Down(button)
    } else {
        MouseKind::Up(button)
    };
    let col = ev.col - SIDEBAR_WIDTH;
    let row = ev.row;
    if let Some(agent) = dash.focused_mut() {
        agent.send(&ToDaemon::Mouse { kind, col, row, mods });
    }
}

/// `:color` argument: an xterm-256 index or `#rrggbb` (quantized to 256).
pub fn parse_color(arg: &str) -> Option<u8> {
    if let Some(hex) = arg.strip_prefix('#') {
        if hex.len() != 6 {
            return None;
        }
        let n = u32::from_str_radix(hex, 16).ok()?;
        let (r, g, b) = ((n >> 16) as u8, (n >> 8) as u8, n as u8);
        return Some(crate::spans::nearest_xterm256(r, g, b));
    }
    arg.parse().ok()
}
