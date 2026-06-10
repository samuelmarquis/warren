//! The daemon's embedded terminal: an alacritty_terminal `Term` plus the
//! conversion from its cell grid to wire `Span`s.

use std::cell::RefCell;
use std::rc::Rc;

use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::Dimensions;
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config, Term, TermDamage, TermMode};
use alacritty_terminal::vte::ansi::{Color as AnsiColor, NamedColor, Processor};

use crate::proto::MouseProto;
use crate::spans::{self, Color, LineSpans, Span};

/// Collects Term-emitted events (title changes, pty answer-backs, OSC 52, …)
/// for the daemon loop to drain after each `advance`.
#[derive(Clone, Default)]
pub struct EventProxy(pub Rc<RefCell<Vec<Event>>>);

impl EventListener for EventProxy {
    fn send_event(&self, event: Event) {
        self.0.borrow_mut().push(event);
    }
}

pub struct AgentTerm {
    pub term: Term<EventProxy>,
    pub proxy: EventProxy,
    processor: Processor,
    pub cols: u16,
    pub rows: u16,
}

impl AgentTerm {
    pub fn new(cols: u16, rows: u16, scrollback: usize) -> Self {
        let config = Config { scrolling_history: scrollback, ..Config::default() };
        let proxy = EventProxy::default();
        let size = TermSize::new(cols as usize, rows as usize);
        let term = Term::new(config, &size, proxy.clone());
        AgentTerm { term, proxy, processor: Processor::new(), cols, rows }
    }

    /// Feed raw pty output through the emulator.
    pub fn advance(&mut self, bytes: &[u8]) {
        self.processor.advance(&mut self.term, bytes);
    }

    pub fn resize(&mut self, cols: u16, rows: u16) {
        if cols == 0 || rows == 0 || (cols == self.cols && rows == self.rows) {
            return;
        }
        self.cols = cols;
        self.rows = rows;
        self.term.resize(TermSize::new(cols as usize, rows as usize));
    }

    pub fn alt_screen(&self) -> bool {
        self.term.mode().contains(TermMode::ALT_SCREEN)
    }

    pub fn mouse_proto(&self) -> MouseProto {
        let mode = self.term.mode();
        if mode.contains(TermMode::MOUSE_MOTION) {
            MouseProto::Motion
        } else if mode.contains(TermMode::MOUSE_DRAG) {
            MouseProto::Drag
        } else if mode.contains(TermMode::MOUSE_REPORT_CLICK) {
            MouseProto::Click
        } else {
            MouseProto::None
        }
    }

    pub fn sgr_mouse(&self) -> bool {
        self.term.mode().contains(TermMode::SGR_MOUSE)
    }

    pub fn cursor_visible(&self) -> bool {
        self.term.mode().contains(TermMode::SHOW_CURSOR)
    }

    /// (row, col) of the cursor within the visible screen.
    pub fn cursor(&self) -> (u16, u16) {
        let point = self.term.grid().cursor.point;
        (point.line.0.max(0) as u16, point.column.0 as u16)
    }

    pub fn history_len(&self) -> u32 {
        self.term.history_size() as u32
    }

    /// Visible screen as styled rows.
    pub fn snapshot_screen(&self) -> Vec<LineSpans> {
        (0..self.rows as i32).map(|r| self.line_spans(Line(r))).collect()
    }

    /// Rows changed since the last `reset_damage`, or None for "everything".
    pub fn take_damage(&mut self) -> Option<Vec<(u16, LineSpans)>> {
        let damaged: Option<Vec<usize>> = match self.term.damage() {
            TermDamage::Full => None,
            TermDamage::Partial(iter) => {
                Some(iter.filter(|b| b.is_damaged()).map(|b| b.line).collect())
            }
        };
        self.term.reset_damage();
        damaged.map(|rows| {
            rows.into_iter()
                .filter(|&r| r < self.rows as usize)
                .map(|r| (r as u16, self.line_spans(Line(r as i32))))
                .collect()
        })
    }

    /// Scrollback lines [start, start+count); 0 = oldest retained line.
    /// Clamped to what exists right now.
    pub fn history_lines(&self, start: u32, count: u16) -> Vec<LineSpans> {
        let len = self.history_len();
        let start = start.min(len);
        let end = (start + count as u32).min(len);
        (start..end)
            .map(|h| self.line_spans(Line(h as i32 - len as i32)))
            .collect()
    }

    fn line_spans(&self, line: Line) -> LineSpans {
        let grid = self.term.grid();
        let cols = self.cols as usize;
        let row = &grid[line];

        // Find the last cell that isn't a bare default-styled space so we can
        // skip encoding the (typically long) blank tail of each row.
        let mut last = 0usize;
        for i in 0..cols {
            let cell = &row[Column(i)];
            let blank = cell.c == ' '
                && cell.bg == AnsiColor::Named(NamedColor::Background)
                && !cell.flags.intersects(Flags::INVERSE | Flags::ALL_UNDERLINES | Flags::STRIKEOUT);
            if !blank {
                last = i + 1;
            }
        }

        let mut spans: Vec<Span> = Vec::new();
        for i in 0..last {
            let cell = &row[Column(i)];
            if cell.flags.intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER) {
                continue;
            }
            let fg = map_color(cell.fg);
            let bg = map_color(cell.bg);
            let attrs = map_attrs(cell.flags);
            match spans.last_mut() {
                Some(prev) if prev.fg == fg && prev.bg == bg && prev.attrs == attrs => {
                    prev.text.push(cell.c);
                }
                _ => spans.push(Span { text: cell.c.to_string(), fg, bg, attrs }),
            }
        }
        LineSpans(spans)
    }
}

fn map_color(c: AnsiColor) -> Color {
    match c {
        AnsiColor::Named(n) => match n {
            NamedColor::Foreground
            | NamedColor::Background
            | NamedColor::Cursor
            | NamedColor::BrightForeground
            | NamedColor::DimForeground => Color::Default,
            NamedColor::DimBlack => Color::Indexed(0),
            NamedColor::DimRed => Color::Indexed(1),
            NamedColor::DimGreen => Color::Indexed(2),
            NamedColor::DimYellow => Color::Indexed(3),
            NamedColor::DimBlue => Color::Indexed(4),
            NamedColor::DimMagenta => Color::Indexed(5),
            NamedColor::DimCyan => Color::Indexed(6),
            NamedColor::DimWhite => Color::Indexed(7),
            base => Color::Indexed(base as u8),
        },
        AnsiColor::Spec(rgb) => Color::Rgb(rgb.r, rgb.g, rgb.b),
        AnsiColor::Indexed(i) => Color::Indexed(i),
    }
}

fn map_attrs(flags: Flags) -> u8 {
    let mut attrs = 0;
    if flags.contains(Flags::BOLD) {
        attrs |= spans::attr::BOLD;
    }
    if flags.contains(Flags::DIM) {
        attrs |= spans::attr::DIM;
    }
    if flags.contains(Flags::ITALIC) {
        attrs |= spans::attr::ITALIC;
    }
    if flags.intersects(Flags::ALL_UNDERLINES) {
        attrs |= spans::attr::UNDERLINE;
    }
    if flags.contains(Flags::INVERSE) {
        attrs |= spans::attr::INVERSE;
    }
    if flags.contains(Flags::STRIKEOUT) {
        attrs |= spans::attr::STRIKEOUT;
    }
    if flags.contains(Flags::HIDDEN) {
        attrs |= spans::attr::HIDDEN;
    }
    attrs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_becomes_one_span() {
        let mut at = AgentTerm::new(20, 4, 100);
        at.advance(b"hello world");
        let snap = at.snapshot_screen();
        assert_eq!(snap.len(), 4);
        assert_eq!(snap[0].0.len(), 1);
        assert_eq!(snap[0].0[0].text, "hello world");
        assert_eq!(snap[1].0.len(), 0); // blank rows encode empty
        assert_eq!(at.cursor(), (0, 11));
    }

    #[test]
    fn styled_runs_split_spans() {
        let mut at = AgentTerm::new(40, 3, 100);
        at.advance(b"a\x1b[1;31mred\x1b[0mb");
        let snap = at.snapshot_screen();
        let line = &snap[0].0;
        assert_eq!(line.len(), 3);
        assert_eq!(line[0].text, "a");
        assert_eq!(line[1].text, "red");
        assert_eq!(line[1].fg, Color::Indexed(1));
        assert_eq!(line[1].attrs, spans::attr::BOLD);
        assert_eq!(line[2].text, "b");
    }

    #[test]
    fn truecolor_passthrough() {
        let mut at = AgentTerm::new(20, 2, 0);
        at.advance(b"\x1b[38;2;10;20;30mX");
        let snap = at.snapshot_screen();
        assert_eq!(snap[0].0[0].fg, Color::Rgb(10, 20, 30));
    }

    #[test]
    fn damage_tracks_changed_rows() {
        let mut at = AgentTerm::new(20, 4, 100);
        at.advance(b"line0");
        let _ = at.take_damage(); // consume initial damage
        at.advance(b"\x1b[3;1Hrow2");
        let damage = at.take_damage().expect("partial damage expected");
        let rows: Vec<u16> = damage.iter().map(|(r, _)| *r).collect();
        assert!(rows.contains(&2), "row 2 damaged: {rows:?}");
        assert!(!rows.contains(&1), "row 1 untouched: {rows:?}");
        assert_eq!(damage.iter().find(|(r, _)| *r == 2).unwrap().1.0[0].text, "row2");
    }

    #[test]
    fn scrollback_and_history_lines() {
        let mut at = AgentTerm::new(10, 3, 100);
        for i in 0..6 {
            at.advance(format!("l{i}\r\n").as_bytes());
        }
        // 6 lines + trailing newline on a 3-row screen: 4 lines scrolled off.
        assert_eq!(at.history_len(), 4);
        let hist = at.history_lines(0, 10);
        assert_eq!(hist.len(), 4);
        assert_eq!(hist[0].0[0].text, "l0");
        assert_eq!(hist[3].0[0].text, "l3");
        // Window into the middle.
        let mid = at.history_lines(1, 2);
        assert_eq!(mid[0].0[0].text, "l1");
        assert_eq!(mid.len(), 2);
    }

    #[test]
    fn resize_reflows_and_alt_screen_flag() {
        let mut at = AgentTerm::new(20, 5, 100);
        assert!(!at.alt_screen());
        at.advance(b"\x1b[?1049h");
        assert!(at.alt_screen());
        at.advance(b"\x1b[?1049l");
        at.resize(10, 4);
        assert_eq!((at.cols, at.rows), (10, 4));
        assert_eq!(at.snapshot_screen().len(), 4);
    }

    #[test]
    fn title_event_is_captured() {
        let mut at = AgentTerm::new(20, 2, 0);
        at.advance(b"\x1b]0;my title\x07");
        let events = at.proxy.0.borrow();
        assert!(events.iter().any(|e| matches!(e, Event::Title(t) if t == "my title")));
    }

    #[test]
    fn mouse_proto_tracking() {
        let mut at = AgentTerm::new(20, 2, 0);
        assert_eq!(at.mouse_proto(), MouseProto::None);
        at.advance(b"\x1b[?1002h\x1b[?1006h");
        assert_eq!(at.mouse_proto(), MouseProto::Drag);
        assert!(at.sgr_mouse());
        at.advance(b"\x1b[?1003h");
        assert_eq!(at.mouse_proto(), MouseProto::Motion);
    }
}
