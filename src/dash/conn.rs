//! Dashboard-side connection to one agent daemon: bounded non-blocking I/O
//! plus the agent's cached screen (the "dumb grid" — spans in, spans out,
//! no escape parsing here).

use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Instant;

use crate::proto::{self, AgentState, FrameDecoder, Meta, MouseProto, ToClient, ToDaemon};
use crate::spans::LineSpans;

/// Outbound cap towards a daemon. Input is tiny; hitting this means the
/// daemon is wedged — drop it rather than ever blocking the dashboard.
const OUT_CAP: usize = 1024 * 1024;

pub struct AgentConn {
    pub sock: PathBuf,
    stream: UnixStream,
    decoder: FrameDecoder,
    out: Vec<u8>,
    sent: usize,
    pub dead: bool,

    pub meta: Meta,
    pub have_meta: bool,
    pub state: AgentState,
    /// Local clock when `state.ms_since_output` was measured.
    pub state_rx: Instant,
    /// Local clock of the last output-bearing frame (busy heuristic).
    pub output_rx: Instant,

    pub grid: Vec<LineSpans>,
    pub cols: u16,
    pub rows: u16,
    pub cursor: (u16, u16),
    pub cursor_visible: bool,
    pub alt_screen: bool,
    pub mouse: MouseProto,

    pub exited: Option<i32>,
    /// Rows repainted since the dashboard last drew this agent.
    pub damage_rows: Vec<u16>,
    pub full_dirty: bool,
    pub meta_dirty: bool,
    /// Went idle while unfocused and not yet examined (sidebar '*').
    pub unseen: bool,
    /// busy() as of the last sidebar paint, for transition detection.
    pub last_busy: bool,
}

impl AgentConn {
    /// Connect and attach at the given pane size.
    pub fn connect(sock: PathBuf, cols: u16, rows: u16) -> std::io::Result<AgentConn> {
        let stream = UnixStream::connect(&sock)?;
        stream.set_nonblocking(true)?;
        let name = sock
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("?")
            .to_string();
        let mut conn = AgentConn {
            sock,
            stream,
            decoder: FrameDecoder::new(),
            out: Vec::new(),
            sent: 0,
            dead: false,
            meta: Meta {
                name: name.clone(),
                display: name,
                color: 0,
                pinned: false,
                slot: u8::MAX,
                cwd: String::new(),
                created: u64::MAX,
            },
            have_meta: false,
            state: AgentState { hook: None, ms_since_output: u64::MAX },
            state_rx: Instant::now(),
            output_rx: Instant::now(),
            grid: Vec::new(),
            cols: 0,
            rows: 0,
            cursor: (0, 0),
            cursor_visible: true,
            alt_screen: false,
            mouse: MouseProto::None,
            exited: None,
            damage_rows: Vec::new(),
            full_dirty: true,
            meta_dirty: true,
            unseen: false,
            last_busy: true,
        };
        conn.send(&ToDaemon::Attach { cols, rows });
        Ok(conn)
    }

    pub fn stream(&self) -> &UnixStream {
        &self.stream
    }

    pub fn send(&mut self, msg: &ToDaemon) {
        if self.dead {
            return;
        }
        let Ok(frame) = proto::encode_frame(msg) else { return };
        if self.out.len() - self.sent + frame.len() > OUT_CAP {
            self.dead = true;
            return;
        }
        self.out.extend_from_slice(&frame);
        self.flush();
    }

    pub fn wants_write(&self) -> bool {
        !self.dead && self.sent < self.out.len()
    }

    pub fn flush(&mut self) {
        while self.sent < self.out.len() {
            match self.stream.write(&self.out[self.sent..]) {
                Ok(0) => {
                    self.dead = true;
                    return;
                }
                Ok(n) => self.sent += n,
                Err(e) if e.kind() == ErrorKind::WouldBlock => return,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(_) => {
                    self.dead = true;
                    return;
                }
            }
        }
        self.out.clear();
        self.sent = 0;
    }

    /// Read whatever the daemon sent and fold it into the cached state.
    pub fn pump(&mut self) {
        let mut buf = [0u8; 65536];
        loop {
            match self.stream.read(&mut buf) {
                Ok(0) => {
                    self.dead = true;
                    break;
                }
                Ok(n) => self.decoder.push(&buf[..n]),
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(_) => {
                    self.dead = true;
                    break;
                }
            }
        }
        loop {
            match self.decoder.next::<ToClient>() {
                Ok(Some(msg)) => self.apply(msg),
                Ok(None) => break,
                Err(_) => {
                    self.dead = true;
                    break;
                }
            }
        }
    }

    fn apply(&mut self, msg: ToClient) {
        match msg {
            ToClient::Snapshot {
                cols,
                rows,
                screen,
                cursor,
                cursor_visible,
                alt_screen,
                mouse,
                meta,
                state,
            } => {
                self.cols = cols;
                self.rows = rows;
                self.grid = screen;
                self.cursor = cursor;
                self.cursor_visible = cursor_visible;
                self.alt_screen = alt_screen;
                self.mouse = mouse;
                self.meta = meta;
                self.have_meta = true;
                self.state = state;
                self.state_rx = Instant::now();
                self.output_rx = Instant::now();
                self.full_dirty = true;
                self.meta_dirty = true;
            }
            ToClient::Damage { lines, cursor, cursor_visible } => {
                for (row, line) in lines {
                    let row_us = row as usize;
                    if row_us >= self.grid.len() {
                        self.grid.resize_with(row_us + 1, LineSpans::default);
                    }
                    self.grid[row_us] = line;
                    if !self.damage_rows.contains(&row) {
                        self.damage_rows.push(row);
                    }
                }
                self.cursor = cursor;
                self.cursor_visible = cursor_visible;
                self.output_rx = Instant::now();
            }
            ToClient::ModeChanged { alt_screen, mouse } => {
                self.alt_screen = alt_screen;
                self.mouse = mouse;
            }
            ToClient::MetaChanged(meta) => {
                self.meta = meta;
                self.have_meta = true;
                self.meta_dirty = true;
            }
            ToClient::StateChanged(state) => {
                self.state = state;
                self.state_rx = Instant::now();
                self.meta_dirty = true;
            }
            ToClient::Exited { status } => {
                self.exited = Some(status);
            }
            // Clipboard forwarding lands in M8.
            ToClient::Clipboard(_) => {}
        }
    }

    /// The v0 busy heuristic: hook state wins; otherwise "output within the
    /// last 1500ms" (Claude's status line repaints about once a second).
    /// `attention` stays activity-based: mid-tool it reads as working.
    pub fn busy(&self) -> bool {
        match self.state.hook {
            Some(crate::proto::HookState::Working) => true,
            Some(crate::proto::HookState::Waiting) => false,
            _ => self.output_rx.elapsed().as_millis() < 1500,
        }
    }

    /// Blocked on a permission prompt and quiet: the sidebar '!' condition.
    pub fn needs_attention(&self) -> bool {
        self.state.hook == Some(crate::proto::HookState::Attention) && !self.busy()
    }
}
