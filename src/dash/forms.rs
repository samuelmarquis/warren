//! The new-agent form (the "+ new agent" tab's content), the edit form
//! (title + color of a running agent), and the shared 16x16 color palette.
//!
//! v0's modal model is preserved: the form is just what the + tab shows.
//! INSERT edits it, NORMAL navigates the dashboard as usual, so you can
//! always leave without creating anything.

use std::fmt::Write;

use crate::proto::ToDaemon;
use crate::sessions::Session;
use crate::spans;

use super::render::SIDEBAR_WIDTH;
use super::{Dash, Mode};

#[derive(Clone, Copy, PartialEq)]
pub enum NField {
    Mode,
    Title,
    Root,
    List,
    Color,
}

pub const MODE_NEW: u8 = 0;
pub const MODE_RESUME: u8 = 1;
pub const MODE_CONTINUE: u8 = 2;

pub struct NewForm {
    pub mode: u8,
    pub field: usize,
    pub title: String,
    pub root: String,
    pub sessions: Option<Vec<Session>>,
    pub sess_sel: usize,
    pub color: u16, // 0 = none, 1..=255 xterm index
}

impl NewForm {
    pub fn reset() -> NewForm {
        NewForm {
            mode: MODE_NEW,
            field: 0,
            title: String::new(),
            root: std::env::var("HOME").unwrap_or_else(|_| "/".into()),
            sessions: None,
            sess_sel: 0,
            color: 0,
        }
    }

    pub fn fields(&self) -> &'static [NField] {
        match self.mode {
            MODE_RESUME => &[NField::Mode, NField::List, NField::Color],
            MODE_CONTINUE => &[NField::Mode, NField::Root, NField::Color],
            _ => &[NField::Mode, NField::Title, NField::Root, NField::Color],
        }
    }

    pub fn active(&self) -> NField {
        self.fields()[self.field.min(self.fields().len() - 1)]
    }

    fn ensure_sessions(&mut self) {
        if self.sessions.is_none() {
            self.sessions = Some(crate::sessions::scan(&crate::paths::claude_projects()));
        }
    }
}

pub struct EditForm {
    pub field: usize, // 0 = title, 1 = color
    pub title: String,
    pub color: u16,
}

// ------------------------------------------------------------------ new form

/// Handle one INSERT-mode chunk on the new-agent form. Returns consumed bytes.
pub fn new_key(dash: &mut Dash, bytes: &[u8]) -> usize {
    dash.form_dirty = true;
    let (key, consumed) = decode_key(bytes);
    let form = &mut dash.newform;
    let nfields = form.fields().len();
    match key {
        Key::Tab => form.field = (form.field + 1) % nfields,
        Key::ShiftTab | Key::Up if form.active() != NField::List => {
            form.field = (form.field + nfields - 1) % nfields;
        }
        Key::Down if form.active() != NField::List && form.active() != NField::Color => {
            form.field = (form.field + 1) % nfields;
        }
        Key::Esc => {
            dash.enter_normal();
        }
        Key::Enter => match form.active() {
            NField::Title => form.field += 1,
            _ => submit_new(dash),
        },
        key => match form.active() {
            NField::Mode => {
                if matches!(key, Key::Char(b'l') | Key::Right) {
                    form.mode = (form.mode + 1) % 3;
                    form.field = 0;
                } else if matches!(key, Key::Char(b'h') | Key::Left) {
                    form.mode = (form.mode + 2) % 3;
                    form.field = 0;
                }
                if form.mode == MODE_RESUME {
                    form.ensure_sessions();
                }
            }
            NField::Title => line_edit(&mut form.title, key),
            NField::Root => line_edit(&mut form.root, key),
            NField::List => {
                form.ensure_sessions();
                let len = form.sessions.as_ref().map(Vec::len).unwrap_or(0);
                match key {
                    Key::Char(b'j') | Key::Down => {
                        form.sess_sel = (form.sess_sel + 1).min(len.saturating_sub(1));
                    }
                    Key::Char(b'k') | Key::Up => form.sess_sel = form.sess_sel.saturating_sub(1),
                    _ => {}
                }
            }
            NField::Color => palette_key(&mut form.color, key),
        },
    }
    consumed
}

fn submit_new(dash: &mut Dash) {
    let form = &mut dash.newform;
    let (mode_str, sid, dir, fallback_name) = match form.mode {
        MODE_RESUME => {
            form.ensure_sessions();
            let Some(sess) = form.sessions.as_ref().and_then(|s| s.get(form.sess_sel)) else {
                dash.flash = Some("no session selected".into());
                dash.status_dirty = true;
                return;
            };
            ("resume", Some(sess.id.clone()), sess.cwd.clone(), sess.title.clone())
        }
        MODE_CONTINUE => ("continue", None, form.root.clone(), "agent".to_string()),
        _ => ("new", None, form.root.clone(), String::new()),
    };
    let raw_name = if form.title.is_empty() { fallback_name } else { form.title.clone() };
    let base = crate::names::sanitize(raw_name.trim());
    if base.is_empty() {
        dash.flash = Some("give the agent a title".into());
        dash.status_dirty = true;
        return;
    }
    let dir = crate::cli::expand_dir(Some(&dir));
    let color = form.color.min(255) as u8;
    let live: Vec<(String, u8)> =
        dash.agents.iter().map(|a| (a.meta.name.clone(), a.meta.slot)).collect();
    match crate::cli::launch_agent(&base, &dir, color, mode_str, sid.as_deref(), &live) {
        Ok(name) => {
            dash.flash = Some(format!("creating agent '{name}'…"));
            dash.pending_focus = Some(name);
            dash.newform = NewForm::reset();
        }
        Err(e) => dash.flash = Some(format!("create failed: {e}")),
    }
    dash.status_dirty = true;
}

pub fn draw_new_form(dash: &mut Dash, out: &mut String) {
    let x0 = SIDEBAR_WIDTH + 1;
    let pane_w = dash.cols.saturating_sub(SIDEBAR_WIDTH) as usize;
    let pane_h = dash.rows.saturating_sub(1);
    let form = &mut dash.newform;
    let active = form.fields()[form.field.min(form.fields().len() - 1)];
    let insert = dash.mode == Mode::Insert;

    for row in 0..pane_h {
        let _ = write!(out, "\x1b[{};{}H\x1b[0m\x1b[K", row + 1, x0);
    }
    let _ = write!(out, "\x1b[2;{}H\x1b[1m+ new claude agent\x1b[0m", x0 + 2);

    // Mode selector.
    let _ = write!(out, "\x1b[4;{}H", x0 + 2);
    let _ = write!(out, "{}Mode     \x1b[0m  ", field_label(active == NField::Mode));
    for (i, label) in ["new", "resume", "continue"].iter().enumerate() {
        if form.mode == i as u8 {
            let _ = write!(out, "\x1b[7m[ {label} ]\x1b[0m ");
        } else {
            let _ = write!(out, "\x1b[2m[ {label} ]\x1b[0m ");
        }
    }

    let mut row = 6u16;
    match form.mode {
        MODE_RESUME => {
            form.ensure_sessions();
            let sessions = form.sessions.as_deref().unwrap_or(&[]);
            let _ = write!(
                out,
                "\x1b[{};{}H{}Session  \x1b[0m",
                row,
                x0 + 2,
                field_label(active == NField::List)
            );
            row += 1;
            let visible = (pane_h.saturating_sub(row + 4) as usize).max(3).min(12);
            let first = form.sess_sel.saturating_sub(visible - 1);
            for (i, sess) in sessions.iter().enumerate().skip(first).take(visible) {
                let marker = if i == form.sess_sel { ">" } else { " " };
                let style = if i == form.sess_sel { "\x1b[1m" } else { "\x1b[2m" };
                let mut label = format!("{marker} {} \u{b7} {}", short_path(&sess.cwd), sess.title);
                label.truncate(pane_w.saturating_sub(4));
                let _ = write!(out, "\x1b[{};{}H{style}{label}\x1b[0m", row, x0 + 4);
                row += 1;
            }
            let more = sessions.len().saturating_sub(first + visible);
            if more > 0 {
                let _ = write!(out, "\x1b[{};{}H\x1b[2m… {more} more\x1b[0m", row, x0 + 4);
            }
            if sessions.is_empty() {
                let _ = write!(out, "\x1b[{};{}H\x1b[2m(no sessions found)\x1b[0m", row, x0 + 4);
            }
            row += 2;
        }
        MODE_CONTINUE => {
            row = draw_text_field(out, row, x0, "Root dir", &form.root, active == NField::Root, insert);
        }
        _ => {
            row = draw_text_field(out, row, x0, "Title", &form.title, active == NField::Title, insert);
            row = draw_text_field(out, row, x0, "Root dir", &form.root, active == NField::Root, insert);
        }
    }

    dash.palette_origin = draw_color_field(out, row, x0, form.color, active == NField::Color);
}

// ----------------------------------------------------------------- edit form

pub fn edit_key(dash: &mut Dash, bytes: &[u8]) -> usize {
    dash.form_dirty = true;
    let (key, consumed) = decode_key(bytes);
    let Some(form) = dash.editform.as_mut() else { return consumed };
    match key {
        Key::Tab | Key::ShiftTab | Key::Up | Key::Down => form.field ^= 1,
        Key::Esc => {
            dash.editform = None;
            dash.enter_normal();
            if let Some(agent) = dash.focused_mut() {
                agent.full_dirty = true;
            }
        }
        Key::Enter => {
            let (title, color) = (form.title.clone(), form.color.min(255) as u8);
            dash.editform = None;
            if let Some(agent) = dash.focused_mut() {
                let rename = !title.trim().is_empty() && title != agent.meta.display;
                agent.send(&ToDaemon::SetMeta {
                    name: rename.then(|| title.trim().to_string()),
                    color: Some(color),
                    pinned: rename.then_some(true),
                    slot: None,
                });
                agent.full_dirty = true;
            }
            dash.enter_insert();
        }
        key => match form.field {
            0 => line_edit(&mut form.title, key),
            _ => palette_key(&mut form.color, key),
        },
    }
    consumed
}

pub fn draw_edit_form(dash: &mut Dash, out: &mut String) {
    let x0 = SIDEBAR_WIDTH + 1;
    let pane_h = dash.rows.saturating_sub(1);
    let Some(form) = dash.editform.as_ref() else { return };

    for row in 0..pane_h {
        let _ = write!(out, "\x1b[{};{}H\x1b[0m\x1b[K", row + 1, x0);
    }
    let _ = write!(out, "\x1b[2;{}H\x1b[1medit agent\x1b[0m", x0 + 2);
    let row = draw_text_field(out, 4, x0, "Title", &form.title, form.field == 0, true);
    dash.palette_origin = draw_color_field(out, row, x0, form.color, form.field == 1);
}

// ------------------------------------------------------------ shared pieces

pub enum Key {
    Char(u8),
    Up,
    Down,
    Left,
    Right,
    Tab,
    ShiftTab,
    Enter,
    Esc,
    Backspace,
    Other,
}

/// Decode one keypress from raw bytes (CSI arrows, tab, etc).
pub fn decode_key(bytes: &[u8]) -> (Key, usize) {
    if bytes.is_empty() {
        return (Key::Other, 1);
    }
    if bytes[0] == 0x1b {
        if bytes.len() >= 3 && bytes[1] == b'[' {
            let key = match bytes[2] {
                b'A' => Key::Up,
                b'B' => Key::Down,
                b'C' => Key::Right,
                b'D' => Key::Left,
                b'Z' => Key::ShiftTab,
                _ => Key::Other,
            };
            return (key, 3);
        }
        return (Key::Esc, 1);
    }
    let key = match bytes[0] {
        b'\t' => Key::Tab,
        b'\r' | b'\n' => Key::Enter,
        0x7f | 0x08 => Key::Backspace,
        b => Key::Char(b),
    };
    (key, 1)
}

fn line_edit(buf: &mut String, key: Key) {
    match key {
        Key::Backspace => {
            buf.pop();
        }
        Key::Char(b @ 0x20..=0x7e) => buf.push(b as char),
        _ => {}
    }
}

/// hjkl/arrows walk the 16x16 grid; '0' clears to none.
fn palette_key(color: &mut u16, key: Key) {
    let c = *color as i32;
    let next = match key {
        Key::Char(b'h') | Key::Left => c - 1,
        Key::Char(b'l') | Key::Right => c + 1,
        Key::Char(b'k') | Key::Up => c - 16,
        Key::Char(b'j') | Key::Down => c + 16,
        Key::Char(b'0') => 0,
        _ => c,
    };
    *color = next.clamp(0, 255) as u16;
}

fn field_label(active: bool) -> &'static str {
    if active { "\x1b[1;7m" } else { "\x1b[2m" }
}

fn draw_text_field(
    out: &mut String,
    row: u16,
    x0: u16,
    label: &str,
    value: &str,
    active: bool,
    insert: bool,
) -> u16 {
    let cursor = if active && insert { "\u{2588}" } else { "" };
    let _ = write!(
        out,
        "\x1b[{};{}H{}{:<9}\x1b[0m  {value}{cursor}",
        row,
        x0 + 2,
        field_label(active),
        label
    );
    row + 2
}

/// Click on a palette swatch (0-based screen cell). Routes to whichever form
/// is showing; the swatch grid origin is recorded at draw time.
pub fn palette_click(dash: &mut Dash, row: u16, col: u16) {
    let Some((row0, col0)) = dash.palette_origin else { return };
    if row < row0 || col < col0 {
        return;
    }
    let (gr, gc) = ((row - row0) as u16, (col - col0) / 2);
    if gr >= 16 || gc >= 16 {
        return;
    }
    let idx = gr * 16 + gc;
    if let Some(form) = dash.editform.as_mut() {
        form.field = 1;
        form.color = idx;
    } else if dash.on_newform() {
        let nfields = dash.newform.fields().len();
        dash.newform.field = nfields - 1; // Color is always last
        dash.newform.color = idx;
    }
    dash.form_dirty = true;
}

/// Returns the 0-based (row, col) of the swatch grid's origin when drawn.
fn draw_color_field(
    out: &mut String,
    row: u16,
    x0: u16,
    color: u16,
    active: bool,
) -> Option<(u16, u16)> {
    let _ = write!(out, "\x1b[{};{}H{}Color    \x1b[0m  ", row, x0 + 2, field_label(active));
    if color == 0 {
        let _ = write!(out, "\x1b[2mnone\x1b[0m");
    } else {
        let (r, g, b) = spans::xterm256_to_rgb(color as u8);
        let _ = write!(out, "\x1b[48;5;{color}m   \x1b[0m  #{r:02x}{g:02x}{b:02x} \u{b7} {color}");
    }
    if !active {
        return None;
    }
    // 16x16 palette grid; swatch 0 is "none".
    for grid_row in 0..16u16 {
        let _ = write!(out, "\x1b[{};{}H", row + 2 + grid_row, x0 + 4);
        for grid_col in 0..16u16 {
            let idx = grid_row * 16 + grid_col;
            let here = idx == color;
            if idx == 0 {
                let _ = write!(out, "\x1b[0;2m{}\x1b[0m", if here { "[]" } else { "--" });
            } else if here {
                let _ = write!(out, "\x1b[48;5;{idx}m\x1b[1m[]\x1b[0m");
            } else {
                let _ = write!(out, "\x1b[48;5;{idx}m  \x1b[0m");
            }
        }
    }
    let _ = write!(
        out,
        "\x1b[{};{}H\x1b[2mhjkl/arrows/click pick \u{b7} 0 none\x1b[0m",
        row + 2 + 16,
        x0 + 4
    );
    Some((row + 2 - 1, x0 + 4 - 1)) // 1-based draw coords -> 0-based cells
}

fn short_path(path: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    if !home.is_empty() && path.starts_with(&home) {
        format!("~{}", &path[home.len()..])
    } else {
        path.to_string()
    }
}
