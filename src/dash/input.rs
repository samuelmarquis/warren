//! Modal input for the dashboard.
//!
//! The dashboard reads RAW bytes from the host terminal and, in INSERT mode,
//! forwards them verbatim to the focused agent — no decode/re-encode, exactly
//! v0 dvtm's approach, so every key claude understands keeps working.
//! Ctrl-Space (NUL) toggles NORMAL mode, which is parsed just enough for the
//! nav keys; `:` opens a one-line command editor in the status bar.

use super::{Dash, Mode, Sub};

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
        match dash.mode {
            Mode::Insert => {
                // Forward verbatim up to a control toggle.
                let stop = bytes[i..]
                    .iter()
                    .position(|&b| b == CTRL_SPACE || b == CTRL_BACKSLASH)
                    .map(|p| i + p)
                    .unwrap_or(bytes.len());
                if stop > i {
                    dash.send_input(&bytes[i..stop]);
                }
                if stop == bytes.len() {
                    return Outcome::Continue;
                }
                match bytes[stop] {
                    CTRL_SPACE => dash.enter_normal(),
                    CTRL_BACKSLASH => return Outcome::Quit,
                    _ => unreachable!(),
                }
                i = stop + 1;
            }
            Mode::Normal => {
                let (consumed, outcome) = match dash.sub {
                    Sub::None => normal_key(dash, &bytes[i..]),
                    Sub::Cmd => cmd_key(dash, &bytes[i..]),
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
        CTRL_SPACE | b'i' | b'a' | b'\r' | ESC => dash.enter_insert(),
        CTRL_BACKSLASH => return (1, Some(Outcome::Quit)),
        b'j' => dash.focus_next(),
        b'k' => dash.focus_prev(),
        b'g' => dash.focus_first(),
        b'G' => dash.focus_last(),
        b'1'..=b'9' => dash.focus_slot(bytes[0] - b'0'),
        b'0' => dash.focus_slot(10),
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

fn run_command(dash: &mut Dash, cmd: &str) -> Option<Outcome> {
    match cmd {
        "q" | "qa" | "wq" => Some(Outcome::Quit),
        "q!" | "qa!" => Some(Outcome::QuitKillAll),
        "" => None,
        other => {
            dash.flash = Some(format!("unknown command :{other}"));
            None
        }
    }
}
