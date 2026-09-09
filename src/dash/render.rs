//! Dashboard compositor: sidebar + content pane + status bar, emitted as raw
//! ANSI into one buffer per frame (wrapped in synchronized-update marks).
//! Region-level diffing: each frame only repaints what's flagged dirty.

use std::fmt::Write;

use crate::spans::{self, LineSpans, Span};

use crate::proto::Power;

use super::{Dash, Mode, Row, Sub};

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

    // The divider column repaints whenever its inputs may have changed
    // (sidebar/junction rows are derived from the focused agent's content).
    // Decide BEFORE drawing: draw_content consumes the damage flags.
    let divider_due = dash.full_redraw
        || dash.sidebar_dirty
        || dash.form_dirty
        || dash
            .focused()
            .map(|a| a.full_dirty || !a.damage_rows.is_empty())
            .unwrap_or(false);

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
    if divider_due {
        draw_divider(dash, &mut out);
    }

    place_cursor(dash, &mut out);
    let _ = write!(out, "\x1b[?2026l");
    dash.full_redraw = false;
    dash.sidebar_dirty = false;
    dash.status_dirty = false;
    out
}

/// Sidebar row number: 1-9, `0` for ten, blank past that (no key reaches it).
fn row_number(n: usize) -> String {
    match n {
        10 => "0".to_string(),
        n if n < 10 => n.to_string(),
        _ => " ".to_string(),
    }
}

fn draw_sidebar(dash: &mut Dash, out: &mut String) {
    let height = dash.rows.saturating_sub(1);
    let text_w = (SIDEBAR_WIDTH - 1) as usize;
    let folders = dash.folders();
    let layout = dash.rows(&folders);

    for row in 0..height {
        let _ = write!(out, "\x1b[{};1H", row + 1);
        let Some(item) = layout.get(row as usize) else {
            let _ = write!(out, "\x1b[0m{}", " ".repeat(text_w));
            continue;
        };
        match item {
            // The pinned "+ new agent" tab.
            Row::NewAgent => {
                let focused = dash.on_newform();
                let style = if focused { "\x1b[0;7m" } else { "\x1b[0;2m" };
                let mut label = "   + new agent".to_string();
                label.truncate(text_w);
                let pad = text_w.saturating_sub(label.chars().count());
                let _ = write!(out, "{style}{label}{}\x1b[0m", " ".repeat(pad));
            }
            // A working directory. Folded, it says how many it is holding.
            Row::Folder(fi) => {
                let folder = &folders[*fi];
                let holds_focus = folder.agents().contains(&dash.focus);
                let tail =
                    if folder.collapsed { format!("\u{25b8}{} ", folder.len) } else { String::new() };
                let room = text_w.saturating_sub(tail.chars().count());
                let head = format!(" {} {}/", row_number(fi + 1), folder.label);
                let head: String = head.chars().take(room).collect();
                let pad = room.saturating_sub(head.chars().count());
                // Bold while it holds the focused agent — a folded folder is
                // then the only thing on screen saying where you are.
                let style = if holds_focus { "\x1b[0;1m" } else { "\x1b[0;2m" };
                let _ = write!(out, "{style}{head}{}{tail}\x1b[0m", " ".repeat(pad));
            }
            Row::Agent(i) => {
                let folder = folders.iter().find(|f| f.agents().contains(i)).unwrap();
                let agent = &dash.agents[*i];
                let focused = dash.focus == *i;
                let last = *i + 1 == folder.start + folder.len;
                let number = row_number(*i - folder.start + 1);
                // Trailing mark: 'z' = asleep (no process, resumable); '!' =
                // blocked on a permission prompt and quiet; '*' = went idle
                // while unfocused, not yet examined.
                let mark = if agent.asleep() {
                    " z"
                } else if agent.needs_attention() {
                    " !"
                } else if agent.unseen {
                    " *"
                } else {
                    ""
                };
                let branch = if last { '\u{2514}' } else { '\u{251c}' };
                let name_w = text_w.saturating_sub(5 + mark.len());
                let name: String = agent.meta.display.chars().take(name_w).collect();
                let label = format!(" {branch} {number} {name}{mark}");
                let pad = text_w.saturating_sub(label.chars().count());

                let color = agent.meta.color;
                let mut style = String::from("\x1b[0");
                // Busy = plain weight, idle = bold. Never dim: dim fg over a
                // colored background reads as unreadable mid-gray — so a
                // sleeping row is dimmed only when it isn't the focused
                // (color-backed) one.
                if agent.asleep() {
                    if !focused {
                        style.push_str(";2");
                    }
                } else if !agent.busy() {
                    style.push_str(";1");
                }
                if focused {
                    if color != 0 {
                        let (r, g, b) = spans::xterm256_to_rgb(color);
                        let fg = if spans::color_is_dark(r, g, b) { 231 } else { 16 };
                        let _ = write!(style, ";48;5;{color};38;5;{fg}");
                    } else {
                        style.push_str(";7");
                    }
                } else if color != 0 {
                    let _ = write!(style, ";38;5;{color}");
                }
                style.push('m');
                let _ = write!(out, "{style}{label}{}\x1b[0m", " ".repeat(pad));
            }
        }
    }
}

/// Horizontal box-drawing leads that should joint into the divider with '├'.
fn joins_divider(c: char) -> bool {
    matches!(c, '\u{2500}' | '\u{2501}' | '\u{254c}' | '\u{2504}' | '\u{2508}' | '\u{2574}' | '\u{2576}')
}

/// The divider column between sidebar and pane, drawn by ONE owner so
/// junctions can't be clobbered by sidebar repaints. Where a horizontal rule
/// in the focused agent's UI meets the column, the cell joins with '├'; the
/// whole column takes that rule's color (Claude's own divider gray), falling
/// back to 240 when no rule is on screen.
fn draw_divider(dash: &Dash, out: &mut String) {
    let rows = dash.rows.saturating_sub(1);
    let col = SIDEBAR_WIDTH; // 1-based ANSI column

    let agent = (!dash.on_newform() && dash.editform.is_none())
        .then(|| dash.focused())
        .flatten();
    let mut junctions: Vec<Option<spans::Color>> = vec![None; rows as usize];
    let mut rule_color: Option<spans::Color> = None;
    if let Some(agent) = agent {
        for (row, line) in agent.grid.iter().enumerate().take(rows as usize) {
            if let Some(span) = line.0.first() {
                if span.text.chars().next().map(joins_divider).unwrap_or(false) {
                    junctions[row] = Some(span.fg);
                    rule_color.get_or_insert(span.fg);
                }
            }
        }
    }
    let base = rule_color.unwrap_or(spans::Color::Indexed(240));

    for row in 0..rows {
        let (glyph, color) = match junctions[row as usize] {
            Some(fg) => ('\u{251c}', fg), // ├
            None => ('\u{2502}', base),   // │
        };
        let style = spans::sgr_sequence(&Span {
            text: String::new(),
            fg: color,
            bg: spans::Color::Default,
            attrs: 0,
        });
        let _ = write!(out, "\x1b[{};{}H{style}{glyph}\x1b[0m", row + 1, col);
    }
}

fn draw_content(dash: &mut Dash, out: &mut String) {
    let pane_w = dash.cols.saturating_sub(SIDEBAR_WIDTH) as usize;
    let pane_h = dash.rows.saturating_sub(1);
    let x0 = SIDEBAR_WIDTH + 1; // 1-based ANSI column

    // Sheep tick: derived from the clock, not from how often we happen to
    // paint, so the flock keeps its pace whatever else the dashboard is doing.
    let frame = dash.anim_frame();
    let focus = dash.focus;
    let color = dash.focused().map(|a| a.meta.color).unwrap_or(0);
    let Some(agent) = dash.agents.get_mut(focus) else {
        return; // + tab focused: the form renderer owns the pane
    };

    // Asleep: the last frame claude painted, dimmed and frozen. Nothing new
    // can arrive, so the grid is drawn once and only the badge animates.
    let asleep = agent.asleep();
    let repaint = agent.full_dirty;
    if repaint {
        for row in 0..pane_h {
            let line = agent.grid.get(row as usize);
            draw_pane_line(out, row, x0, pane_w, line, asleep);
        }
        agent.full_dirty = false;
        agent.damage_rows.clear();
    } else {
        let rows: Vec<u16> = agent.damage_rows.drain(..).collect();
        for row in rows {
            if row < pane_h {
                let line = agent.grid.get(row as usize);
                draw_pane_line(out, row, x0, pane_w, line, asleep);
            }
        }
    }
    if asleep && (repaint || frame != dash.sheep_frame) {
        dash.sheep_frame = frame;
        draw_sleep_badge(out, x0, pane_w, pane_h, color, frame);
    }
}

fn draw_pane_line(
    out: &mut String,
    row: u16,
    x0: u16,
    width: usize,
    line: Option<&LineSpans>,
    dim: bool,
) {
    let _ = write!(out, "\x1b[{};{}H\x1b[0m\x1b[K", row + 1, x0);
    let Some(line) = line else { return };
    let mut budget = width;
    for span in &line.0 {
        if budget == 0 {
            break;
        }
        let text: String = span.text.chars().take(budget).collect();
        budget -= text.chars().count();
        if dim {
            // Bold and dim fight; the frozen screen is background now, so dim
            // wins and the badge on top is the only bright thing in the pane.
            let faded = Span {
                text: String::new(),
                fg: span.fg,
                bg: span.bg,
                attrs: (span.attrs & !spans::attr::BOLD) | spans::attr::DIM,
            };
            let _ = write!(out, "{}{}", spans::sgr_sequence(&faded), text);
        } else {
            let _ = write!(out, "{}{}", spans::sgr_sequence(span), text);
        }
    }
    let _ = write!(out, "\x1b[0m");
}

// ------------------------------------------------------------------- asleep
//
// The badge over a sleeping agent's frozen screen: a fence, a ground line,
// and a sheep that trots across and hops the fence, one cell per tick.

/// The sheep: wool, face, legs, five cells wide. Drawn standing on the ground
/// line, or one row higher mid-hop.
const SHEEP_W: i32 = 5;
const SHEEP_WOOL: &str = " ⌒⌒⌒";
const SHEEP_FACE: &str = "(o.o)";
/// Legs alternate as it runs; tucked up while it's over the fence.
const SHEEP_LEGS: [&str; 3] = ["  \" \"", " \"  \"", "  ~~ "];

/// Box height: sky, three sheep rows, ground, caption, two borders.
const BADGE_H: usize = 8;

fn draw_sleep_badge(out: &mut String, x0: u16, pane_w: usize, pane_h: u16, color: u8, frame: u64) {
    let hint = "^Space z  ·  wake";
    // Too small for a meadow: one honest line, centered.
    if pane_w < 34 || (pane_h as usize) < BADGE_H + 2 {
        let text = format!("asleep  ·  {hint}");
        let len = text.chars().count();
        if pane_h >= 1 && pane_w >= len + 2 {
            let col = x0 as usize + (pane_w - len) / 2;
            let row = (pane_h / 2).max(1);
            let _ = write!(out, "\x1b[{row};{col}H\x1b[0;1masleep\x1b[0;2m  ·  {hint}\x1b[0m");
        }
        return;
    }

    let bw = pane_w.min(40); // box width, borders included
    let iw = bw - 2; // interior width
    let left = x0 as usize + (pane_w - bw) / 2;
    let top = ((pane_h as usize) - BADGE_H) / 2 + 1; // 1-based screen row

    // The border wears the tab's own color; everything inside is moonlight.
    let border = match color {
        0 => "\x1b[0;38;5;240m".to_string(),
        c => format!("\x1b[0;38;5;{c}m"),
    };
    let sky_style = "\x1b[0;38;5;244m";
    let sheep_style = "\x1b[0;38;5;255m";
    let ground_style = "\x1b[0;38;5;238m";
    let caption_style = "\x1b[0;2m";

    // The meadow: the sheep walks in off the left edge and out off the right,
    // hopping the fence in the middle. One cell per tick.
    let fence = (iw / 2) as i32;
    let period = iw as i64 + SHEEP_W as i64 + 8;
    let x = -SHEEP_W + (frame as i64 % period) as i32;
    // Airborne from a cell before the fence meets its nose to a cell after
    // it clears its tail — so it never walks through the post.
    let hop = x + SHEEP_W >= fence && x <= fence + 1;
    let legs = if hop { 2 } else { (frame % 2) as usize };
    let sprite_top = if hop { 0 } else { 1 }; // interior row of the wool

    // …and a slow breath of z's over it.
    let zs: String = (0..3)
        .map(|i| if i < frame % 4 { "z " } else { "  " })
        .collect();

    // Interior rows 0..3: sky and the three sheep rows.
    let mut rows: Vec<String> = Vec::with_capacity(BADGE_H);
    rows.push(format!("{border}╭{}╮\x1b[0m", "─".repeat(iw)));
    for r in 0..4usize {
        let mut pieces: Vec<(i32, &str)> = Vec::new();
        if r == 0 {
            pieces.push((2, zs.as_str()));
        }
        match r as i32 - sprite_top {
            0 => pieces.push((x, SHEEP_WOOL)),
            1 => pieces.push((x, SHEEP_FACE)),
            2 => pieces.push((x, SHEEP_LEGS[legs])),
            _ => {}
        }
        let has_sheep = r as i32 >= sprite_top && r as i32 <= sprite_top + 2;
        let style = if has_sheep { sheep_style } else { sky_style };
        rows.push(format!("{border}│{style}{}{border}│\x1b[0m", cell_row(iw, &pieces)));
    }
    // The ground, with the fence post standing on it.
    let mut ground: Vec<char> = std::iter::repeat_n('▁', iw).collect();
    if let Some(cell) = ground.get_mut(fence as usize) {
        *cell = '╥';
    }
    let ground: String = ground.into_iter().collect();
    rows.push(format!("{border}│{ground_style}{ground}{border}│\x1b[0m"));
    rows.push(format!(
        "{border}│{caption_style}{}{border}│\x1b[0m",
        center(iw, &format!("asleep  ·  {hint}"))
    ));
    rows.push(format!("{border}╰{}╯\x1b[0m", "─".repeat(iw)));

    for (i, row) in rows.iter().enumerate() {
        let _ = write!(out, "\x1b[{};{}H{row}", top + i, left);
    }
}

/// Lay `pieces` (column, text) into a row of `width` cells, clipping anything
/// that runs off either edge.
fn cell_row(width: usize, pieces: &[(i32, &str)]) -> String {
    let mut cells: Vec<char> = vec![' '; width];
    for (start, text) in pieces {
        for (i, ch) in text.chars().enumerate() {
            let col = start + i as i32;
            if col >= 0 && (col as usize) < width {
                cells[col as usize] = ch;
            }
        }
    }
    cells.into_iter().collect()
}

fn center(width: usize, text: &str) -> String {
    let len = text.chars().count();
    if len >= width {
        return text.chars().take(width).collect();
    }
    let pad = (width - len) / 2;
    format!("{}{}{}", " ".repeat(pad), text, " ".repeat(width - len - pad))
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
    } else if let Sub::Goto(folder) = dash.sub {
        let folders = dash.folders();
        match folders.get(folder as usize - 1) {
            Some(f) => format!(
                " go to  {folder} {}/  \u{2588}   agent 1-{} · Esc cancel",
                f.label, f.len
            ),
            None => format!(" go to  {folder} \u{2588}   (no folder {folder})"),
        }
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
                " j/k move · N N folder/agent · i claude · r rename · e edit · z sleep · x close · : cmd"
                    .to_string()
            }
            Mode::Insert => {
                let name = dash
                    .focused()
                    .map(|a| a.meta.display.clone())
                    .unwrap_or_else(|| "—".to_string());
                match dash.focused().map(|a| a.power()) {
                    Some(Power::Asleep) => {
                        format!(" {name}  ·  asleep · type or ^Space z to wake and resume")
                    }
                    Some(Power::Sleeping) => format!(" {name}  ·  sleeping…"),
                    Some(Power::Waking) => format!(" {name}  ·  waking — resuming the session…"),
                    _ => format!(" {name}  ·  ^Space normal mode · ^\\ detach"),
                }
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
        // A sleeping agent has no process to own the cursor; leaving one
        // blinking on the frozen screen would look live.
        if agent.cursor_visible && agent.exited.is_none() && !agent.asleep() {
            let (row, col) = agent.cursor;
            let _ = write!(out, "\x1b[{};{}H\x1b[?25h", row + 1, col + 1 + SIDEBAR_WIDTH);
            return;
        }
    }
    // NORMAL mode / no agent: cursor stays hidden.
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Replay the badge's ANSI onto a blank canvas: a tiny terminal, so the
    /// tests (and a human with --nocapture) see what the pane would show.
    /// The canvas is deliberately bigger than the pane, so anything drawn
    /// out of bounds shows up instead of being clipped away.
    struct Canvas {
        cells: Vec<Vec<char>>,
    }

    impl Canvas {
        fn paint(ansi: &str, w: usize, h: usize) -> Canvas {
            let mut cells = vec![vec![' '; w]; h];
            let (mut row, mut col) = (0usize, 0usize);
            let mut rest = ansi;
            while !rest.is_empty() {
                if let Some(after) = rest.strip_prefix('\x1b') {
                    let body = after.strip_prefix('[').unwrap_or(after);
                    let end = body.find(|c: char| c.is_ascii_alphabetic()).unwrap_or(0);
                    if body.as_bytes().get(end) == Some(&b'H') {
                        let mut parts = body[..end].split(';');
                        let mut next = || {
                            parts.next().and_then(|v| v.parse::<usize>().ok()).unwrap_or(1) - 1
                        };
                        row = next();
                        col = next();
                    }
                    rest = &body[(end + 1).min(body.len())..];
                } else {
                    let upto = rest.find('\x1b').unwrap_or(rest.len());
                    for ch in rest[..upto].chars() {
                        if let Some(cell) = cells.get_mut(row).and_then(|r| r.get_mut(col)) {
                            *cell = ch;
                        }
                        col += 1;
                    }
                    rest = &rest[upto..];
                }
            }
            Canvas { cells }
        }

        fn rows(&self) -> Vec<String> {
            self.cells.iter().map(|r| r.iter().collect::<String>()).collect()
        }

        /// (row, col) of every painted cell outside the given rectangle.
        fn outside(&self, rows: usize, cols: std::ops::Range<usize>) -> Vec<(usize, usize)> {
            let mut stray = Vec::new();
            for (r, line) in self.cells.iter().enumerate() {
                for (c, ch) in line.iter().enumerate() {
                    if *ch != ' ' && (r >= rows || !cols.contains(&c)) {
                        stray.push((r, c));
                    }
                }
            }
            stray
        }
    }

    fn badge(pane_w: usize, pane_h: u16, frame: u64) -> Canvas {
        let mut ansi = String::new();
        draw_sleep_badge(&mut ansi, 1, pane_w, pane_h, 0, frame);
        Canvas::paint(&ansi, pane_w + 20, pane_h as usize + 4)
    }

    /// Column (in cells, not bytes — these rows are full of box drawing).
    fn cell_col(line: &str, needle: &str) -> Option<usize> {
        line.find(needle).map(|byte| line[..byte].chars().count())
    }

    #[test]
    fn sheep_crosses_the_meadow_and_hops_the_fence() {
        let mut columns: Vec<i32> = Vec::new();
        let mut hops = 0;
        for frame in 0..80 {
            let rows = badge(46, 14, frame).rows();
            let fence = rows.iter().find(|r| r.contains('╥')).expect("a fence to jump");
            let fence_col = cell_col(fence, "╥").unwrap() as i32;
            let ground = rows.iter().position(|r| r.contains('▁')).unwrap();
            let Some(face) = rows.iter().position(|r| r.contains("(o.o)")) else { continue };
            let col = cell_col(&rows[face], "(o.o)").unwrap() as i32;
            columns.push(col);
            // Grounded, the face sits two rows above the ground; airborne,
            // three.
            let airborne = ground - face == 3;
            if airborne {
                hops += 1;
            }
            // The invariant that makes it a fence and not a decoration.
            let over_the_post = (col..col + SHEEP_W).contains(&fence_col);
            assert!(
                !over_the_post || airborne,
                "frame {frame}: the sheep walks through the fence \
                 (sheep at {col}, post at {fence_col})"
            );
        }
        assert!(columns.len() > 20, "the sheep is on screen most of the time");
        let forward = columns.windows(2).filter(|w| w[1] > w[0]).count();
        assert!(forward >= columns.len() - 2, "the sheep runs one way: {columns:?}");
        assert!(hops >= 5, "it clears the fence in an arc, not a twitch");
    }

    #[test]
    fn badge_stays_inside_every_pane_a_terminal_can_give_us() {
        for w in 0..48usize {
            for h in 0..14u16 {
                let canvas = badge(w, h, 7);
                let stray = canvas.outside(h as usize, 0..w);
                assert!(stray.is_empty(), "badge escapes a {w}x{h} pane at {stray:?}");
            }
        }
    }

    /// `cargo test sheep -- --nocapture` to watch the flock.
    #[test]
    fn sheep_frames() {
        for frame in 17..27 {
            for line in badge(44, 12, frame).rows() {
                println!("|{}|", line.trim_end());
            }
            println!();
        }
    }
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
