//! The per-agent daemon: owns the pty running Claude, the embedded terminal
//! (screen + scrollback), agent metadata, and a unix socket serving viewers.
//!
//! Spawned detached by `warren new` (ppid 1); fully independent of the
//! dashboard and of every other agent. Single-threaded `polling` loop over
//! the pty, a SIGCHLD pipe, the listener, and client sockets — and no write
//! anywhere that can block on a peer.

mod client;
mod term;

use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use alacritty_terminal::event::{Event, OnResize, WindowSize};
use alacritty_terminal::tty::{self, ChildEvent, EventedPty, EventedReadWrite, Options, Shell};
use anyhow::{Context, Result, bail};
use polling::{Event as PollEvent, Events, PollMode, Poller};

use crate::proto::{
    self, AgentState, HookState, Meta, MouseKind, MouseProto, ToClient, ToDaemon,
};
use client::Conn;

/// Keys alacritty's Pty hardcodes when registering itself (file, SIGCHLD pipe).
const KEY_PTY: usize = 0;
const KEY_CHILD: usize = 1;
const KEY_LISTENER: usize = 2;
const KEY_FIRST_CLIENT: usize = 3;

/// Damage coalescing: at most ~30 frames/sec to viewers.
const FLUSH_INTERVAL: Duration = Duration::from_millis(33);

/// No daemon-side scrollback: agents (Claude's fullscreen TUI) own their own
/// history; warren serves a live view only.
const SCROLLBACK: usize = 0;

pub struct DaemonArgs {
    pub name: String,
    pub slot: u8,
    pub color: u8,
    pub mode: String,
    pub dir: String,
    pub sid: Option<String>,
    /// `--system-prompt` for claude (replaces the default).
    pub sys: Option<String>,
    /// Raw extra args appended to the claude command line.
    pub extra: Option<String>,
}

impl DaemonArgs {
    pub fn parse(rest: &[String]) -> Result<Self> {
        let sys = rest.iter().find_map(|a| a.strip_prefix("--sys=")).map(String::from);
        let extra = rest.iter().find_map(|a| a.strip_prefix("--extra=")).map(String::from);
        let pos: Vec<&String> = rest.iter().filter(|a| !a.starts_with("--")).collect();
        if pos.len() < 5 {
            bail!("usage: warren __daemon NAME SLOT COLOR MODE DIR [SESSION-ID] [--sys=…] [--extra=…]");
        }
        Ok(DaemonArgs {
            name: pos[0].clone(),
            slot: pos[1].parse().context("slot")?,
            color: pos[2].parse().context("color")?,
            mode: pos[3].clone(),
            dir: pos[4].clone(),
            sid: pos.get(5).map(|s| s.to_string()),
            sys,
            extra,
        })
    }
}

struct Daemon {
    term: term::AgentTerm,
    pty: tty::Pty,
    listener: UnixListener,
    poller: Arc<Poller>,
    conns: HashMap<usize, Conn>,
    next_key: usize,
    meta: Meta,
    hook: Option<HookState>,
    last_output: Instant,
    /// Bytes queued for the application (keyboard input, answer-backs).
    pty_in: Vec<u8>,
    pty_wants_write: bool,
    /// Mode state last broadcast, to detect changes.
    sent_alt: bool,
    sent_mouse: MouseProto,
    dirty: bool,
    log: Option<std::fs::File>,
    started: Instant,
}

pub fn run(args: DaemonArgs) -> Result<()> {
    // A dead viewer's socket must never kill us with SIGPIPE.
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) };

    let run_dir = crate::paths::ensure_run_dir()?;
    let sock = run_dir.join(format!("{}.sock", args.name));
    let listener = bind_or_replace(&sock)
        .with_context(|| format!("binding {}", sock.display()))?;
    listener.set_nonblocking(true)?;

    let log = std::env::var("WARREN_LOG").ok().map(|p| {
        std::fs::OpenOptions::new().create(true).append(true).open(p)
    }).and_then(Result::ok);

    // Build the agent command. WARREN_AGENT_CMD overrides for tests/tools;
    // --settings (Claude lifecycle hooks) is only wired onto the default.
    let cmd = match std::env::var("WARREN_AGENT_CMD") {
        Ok(custom) if !custom.is_empty() => custom,
        _ => {
            let hooks = crate::hooks::ensure_hooks_json()?;
            let mut cmd = match args.mode.as_str() {
                "resume" => match &args.sid {
                    Some(sid) => format!("claude --resume {sid}"),
                    None => "claude --resume".to_string(),
                },
                "continue" => "claude --continue".to_string(),
                _ => "claude".to_string(),
            };
            if let Some(sys) = &args.sys {
                // Single-quote for the shell, escaping embedded quotes.
                let escaped = sys.replace('\'', r"'\''");
                cmd.push_str(&format!(" --system-prompt '{escaped}'"));
            }
            if let Some(extra) = &args.extra {
                // User-authored args, passed through verbatim.
                cmd.push_str(&format!(" {extra}"));
            }
            cmd.push_str(&format!(" --settings '{}'", hooks.display()));
            cmd
        }
    };

    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
    let mut env = std::collections::HashMap::new();
    // TERM must be xterm-ish, NOT screen-*: Claude detects "screen" and falls
    // back to its alternate-screen renderer (no scrollback at all).
    env.insert("TERM".into(), std::env::var("DVTM_TERM").unwrap_or_else(|_| "xterm-256color".into()));
    env.insert("COLORTERM".into(), "truecolor".into());
    env.insert("WARREN_AGENT".into(), args.name.clone());
    env.insert("WARREN_SOCK".into(), sock.display().to_string());

    let window_size = WindowSize { num_lines: 24, num_cols: 80, cell_width: 8, cell_height: 16 };
    let options = Options {
        shell: Some(Shell::new(shell, vec!["-c".into(), cmd])),
        working_directory: Some(args.dir.clone().into()),
        drain_on_exit: false,
        env,
    };
    let mut pty = tty::new(&options, window_size, 0).context("spawning agent pty")?;

    let poller = Arc::new(Poller::new()?);
    unsafe {
        // Registers the pty fd as KEY_PTY and its SIGCHLD pipe as KEY_CHILD.
        pty.register(&poller, PollEvent::readable(KEY_PTY), PollMode::Level)?;
        poller.add_with_mode(&listener, PollEvent::readable(KEY_LISTENER), PollMode::Level)?;
    }

    let created = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let mut daemon = Daemon {
        term: term::AgentTerm::new(80, 24, SCROLLBACK),
        pty,
        listener,
        poller,
        conns: HashMap::new(),
        next_key: KEY_FIRST_CLIENT,
        meta: Meta {
            name: args.name.clone(),
            display: args.name.clone(),
            color: args.color,
            pinned: false,
            slot: args.slot,
            cwd: args.dir,
            created,
        },
        hook: None,
        last_output: Instant::now(),
        pty_in: Vec::new(),
        pty_wants_write: false,
        sent_alt: false,
        sent_mouse: MouseProto::None,
        dirty: false,
        log,
        started: Instant::now(),
    };

    let status = daemon.event_loop()?;
    daemon.logf(&format!("agent exited with status {status}"));

    // Drain any final output, tell viewers, give their queues a moment.
    daemon.read_pty();
    daemon.flush_frame();
    daemon.broadcast(&ToClient::Exited { status });
    let deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < deadline && daemon.conns.values().any(|c| c.wants_write()) {
        for conn in daemon.conns.values_mut() {
            conn.flush();
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    let _ = std::fs::remove_file(&sock);
    Ok(())
}

/// Bind the agent socket; replace it only if it's stale (nothing listening).
fn bind_or_replace(sock: &std::path::Path) -> Result<UnixListener> {
    match UnixListener::bind(sock) {
        Ok(l) => Ok(l),
        Err(e) if e.kind() == ErrorKind::AddrInUse => {
            if UnixStream::connect(sock).is_ok() {
                bail!("an agent is already running on {}", sock.display());
            }
            std::fs::remove_file(sock)?;
            Ok(UnixListener::bind(sock)?)
        }
        Err(e) => Err(e.into()),
    }
}

impl Daemon {
    fn logf(&mut self, msg: &str) {
        if let Some(f) = &mut self.log {
            let t = self.started.elapsed();
            let _ = writeln!(f, "[{}] {:>9.3}ms {}", self.meta.name, t.as_secs_f64() * 1000.0, msg);
        }
    }

    /// Runs until the agent process exits; returns its exit status.
    fn event_loop(&mut self) -> Result<i32> {
        let mut events = Events::new();
        let mut flush_at: Option<Instant> = None;

        loop {
            let timeout = flush_at.map(|at| at.saturating_duration_since(Instant::now()));
            events.clear();
            match self.poller.wait(&mut events, timeout) {
                Ok(_) => {}
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            }

            // Timeout-only wakes are logged when a flush is due (the damage
            // timer firing); event wakes always.
            if self.log.is_some() && (events.iter().next().is_some() || self.dirty) {
                let keys: Vec<String> = events
                    .iter()
                    .map(|e| format!("{}{}{}", e.key, if e.readable { "r" } else { "" }, if e.writable { "w" } else { "" }))
                    .collect();
                let msg = format!("wake: [{}] dirty={} flush_at={:?}", keys.join(","), self.dirty, flush_at.map(|a| a.saturating_duration_since(Instant::now())));
                self.logf(&msg);
            }

            let mut exited: Option<i32> = None;
            for ev in events.iter() {
                match ev.key {
                    KEY_PTY => {
                        if ev.readable {
                            self.read_pty();
                        }
                        if ev.writable {
                            self.write_pty();
                        }
                    }
                    KEY_CHILD => {
                        while let Some(ChildEvent::Exited(status)) = self.pty.next_child_event() {
                            exited = Some(status.and_then(|s| s.code()).unwrap_or(-1));
                        }
                    }
                    KEY_LISTENER => self.accept_clients(),
                    key => {
                        if ev.readable {
                            self.read_client(key);
                        }
                        if ev.writable {
                            if let Some(conn) = self.conns.get_mut(&key) {
                                conn.flush();
                            }
                        }
                    }
                }
            }

            self.drain_term_events();
            self.write_pty();
            self.update_pty_interest()?;

            // Damage coalescing: first dirtying event arms the timer; the
            // frame goes out when it expires.
            match (self.dirty, flush_at) {
                (true, None) => flush_at = Some(Instant::now() + FLUSH_INTERVAL),
                (true, Some(at)) if Instant::now() >= at => {
                    self.flush_frame();
                    flush_at = None;
                }
                (false, Some(_)) => flush_at = None,
                _ => {}
            }

            // Reap + reflect write interest LAST, after anything above may
            // have queued outbound bytes. A frame bigger than the socket
            // buffer (8KiB on macOS) leaves a tail in the conn queue; if the
            // poller isn't watching writability when we go to sleep, that
            // tail waits for the next unrelated event — the dashboard renders
            // one redraw behind, "unstuck" by each keypress.
            self.reap_dead_conns();

            if let Some(status) = exited {
                return Ok(status);
            }
        }
    }

    fn read_pty(&mut self) {
        let mut buf = [0u8; 65536];
        let mut total = 0;
        let log_total = self.log.is_some();
        let mut preview = String::new();
        loop {
            match self.pty.reader().read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if log_total && preview.len() < 160 {
                        preview.push_str(&escape_bytes(&buf[..n.min(160)]));
                    }
                    self.term.advance(&buf[..n]);
                    self.last_output = Instant::now();
                    self.dirty = true;
                    total += n;
                    // Bound one loop iteration so client traffic stays live
                    // under a firehose of output.
                    if total >= 1 << 20 {
                        break;
                    }
                }
                // A dead pty master reads 0 on macOS and EIO on Linux; both
                // just mean "no more output" — SIGCHLD owns exit detection.
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        if log_total {
            let msg = format!("read_pty: {total} bytes  «{preview}»");
            self.logf(&msg);
        }
    }

    fn write_pty(&mut self) {
        if self.log.is_some() && !self.pty_in.is_empty() {
            let msg = format!("write_pty: {} bytes  «{}»", self.pty_in.len(), escape_bytes(&self.pty_in[..self.pty_in.len().min(160)]));
            self.logf(&msg);
        }
        while !self.pty_in.is_empty() {
            match self.pty.writer().write(&self.pty_in) {
                Ok(0) => break,
                Ok(n) => {
                    self.pty_in.drain(..n);
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(_) => {
                    self.pty_in.clear();
                    break;
                }
            }
        }
    }

    /// Re-register the pty for writability only while input is queued.
    fn update_pty_interest(&mut self) -> Result<()> {
        let want = !self.pty_in.is_empty();
        if want != self.pty_wants_write {
            let ev = PollEvent::new(KEY_PTY, true, want);
            self.pty.reregister(&self.poller, ev, PollMode::Level)?;
            self.pty_wants_write = want;
        }
        Ok(())
    }

    fn accept_clients(&mut self) {
        loop {
            match self.listener.accept() {
                Ok((stream, _)) => {
                    if stream.set_nonblocking(true).is_err() {
                        continue;
                    }
                    let key = self.next_key;
                    self.next_key += 1;
                    unsafe {
                        if self
                            .poller
                            .add_with_mode(&stream, PollEvent::readable(key), PollMode::Level)
                            .is_err()
                        {
                            continue;
                        }
                    }
                    self.conns.insert(key, Conn::new(stream));
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(_) => break,
            }
        }
    }

    fn read_client(&mut self, key: usize) {
        let Some(conn) = self.conns.get_mut(&key) else { return };
        // On EOF, still process what's buffered first: one-shot peers
        // (hooks, Query scripts) write their frame and close immediately.
        let eof = !conn.fill();
        loop {
            let msg = match self.conns.get_mut(&key).unwrap().decoder.next::<ToDaemon>() {
                Ok(Some(m)) => m,
                Ok(None) => break,
                Err(_) => {
                    self.conns.get_mut(&key).unwrap().dead = true;
                    break;
                }
            };
            self.handle_msg(key, msg);
        }
        if eof {
            if let Some(conn) = self.conns.get_mut(&key) {
                conn.dead = true;
            }
        }
    }

    fn handle_msg(&mut self, key: usize, msg: ToDaemon) {
        match msg {
            ToDaemon::Attach { cols, rows } => {
                // Settle pending damage for current viewers, then hand the
                // newcomer an authoritative snapshot and reset the damage
                // state so the next output flushes as a partial frame.
                self.flush_frame();
                self.resize_to(cols, rows);
                let snapshot = self.snapshot();
                self.term.term.reset_damage();
                self.dirty = false;
                let conn = self.conns.get_mut(&key).unwrap();
                conn.attached = true;
                send_to(conn, &snapshot);
            }
            ToDaemon::Input(b64) => {
                if let Ok(bytes) = proto::b64_decode(&b64) {
                    self.pty_in.extend_from_slice(&bytes);
                }
            }
            ToDaemon::Resize { cols, rows } => self.resize_to(cols, rows),
            ToDaemon::Mouse { kind, col, row, mods } => {
                if let Some(bytes) = self.encode_mouse(kind, col, row, mods) {
                    self.pty_in.extend_from_slice(&bytes);
                }
            }
            ToDaemon::SetMeta { name, color, pinned, slot } => {
                if let Some(n) = name {
                    self.meta.display = n;
                }
                if let Some(c) = color {
                    self.meta.color = c;
                }
                if let Some(p) = pinned {
                    self.meta.pinned = p;
                }
                if let Some(s) = slot {
                    self.meta.slot = s;
                }
                let update = ToClient::MetaChanged(self.meta.clone());
                self.broadcast(&update);
            }
            ToDaemon::HookState(state) => {
                self.hook = Some(state);
                let update = ToClient::StateChanged(self.state());
                self.broadcast(&update);
            }
            ToDaemon::Query => {
                let meta = ToClient::MetaChanged(self.meta.clone());
                let state = ToClient::StateChanged(self.state());
                let conn = self.conns.get_mut(&key).unwrap();
                send_to(conn, &meta);
                send_to(conn, &state);
                conn.close_after_write = true;
                conn.flush();
            }
            ToDaemon::Kill => {
                // SIGHUP the agent's process group (it ran setsid, so its pid
                // leads the group); exit follows via SIGCHLD.
                let pid = self.pty.child().id() as i32;
                unsafe { libc::kill(-pid, libc::SIGHUP) };
            }
        }
    }

    fn state(&self) -> AgentState {
        AgentState {
            hook: self.hook,
            ms_since_output: self.last_output.elapsed().as_millis() as u64,
        }
    }

    fn snapshot(&self) -> ToClient {
        ToClient::Snapshot {
            cols: self.term.cols,
            rows: self.term.rows,
            screen: self.term.snapshot_screen(),
            cursor: self.term.cursor(),
            cursor_visible: self.term.cursor_visible(),
            alt_screen: self.term.alt_screen(),
            mouse: self.term.mouse_proto(),
            meta: self.meta.clone(),
            state: self.state(),
        }
    }

    /// Last-writer-wins resize: reflow the Term, set the pty winsize (kernel
    /// delivers SIGWINCH), and re-snapshot every attached viewer.
    fn resize_to(&mut self, cols: u16, rows: u16) {
        if cols == 0 || rows == 0 || (cols == self.term.cols && rows == self.term.rows) {
            return;
        }
        self.term.resize(cols, rows);
        self.pty.on_resize(WindowSize {
            num_lines: rows,
            num_cols: cols,
            cell_width: 8,
            cell_height: 16,
        });
        let snapshot = self.snapshot();
        self.broadcast(&snapshot);
        self.term.term.reset_damage();
        self.dirty = false;
    }

    /// Send coalesced damage to all attached viewers.
    fn flush_frame(&mut self) {
        if !self.dirty {
            return;
        }
        self.dirty = false;

        let alt = self.term.alt_screen();
        let mouse = self.term.mouse_proto();
        if alt != self.sent_alt || mouse != self.sent_mouse {
            self.sent_alt = alt;
            self.sent_mouse = mouse;
            let update = ToClient::ModeChanged { alt_screen: alt, mouse };
            self.broadcast(&update);
        }

        let frame = match self.term.take_damage() {
            Some(lines) => ToClient::Damage {
                lines,
                cursor: self.term.cursor(),
                cursor_visible: self.term.cursor_visible(),
            },
            None => self.snapshot(),
        };
        let encoded = match proto::encode_frame(&frame) {
            Ok(e) => e,
            Err(_) => return,
        };
        if self.log.is_some() {
            let msg = format!("flush_frame: {} bytes", encoded.len());
            self.logf(&msg);
        }
        for conn in self.conns.values_mut() {
            if conn.attached {
                conn.send(&encoded);
            }
        }
    }

    fn broadcast(&mut self, msg: &ToClient) {
        let Ok(encoded) = proto::encode_frame(msg) else { return };
        for conn in self.conns.values_mut() {
            if conn.attached {
                conn.send(&encoded);
            }
        }
    }

    /// Handle events the emulator emitted (titles, answer-backs, OSC 52).
    fn drain_term_events(&mut self) {
        let events: Vec<Event> = self.term.proxy.0.borrow_mut().drain(..).collect();
        for event in events {
            match event {
                Event::Title(title) => self.sync_title(&title),
                Event::ResetTitle => {
                    if !self.meta.pinned && self.meta.display != self.meta.name {
                        self.meta.display = self.meta.name.clone();
                        let update = ToClient::MetaChanged(self.meta.clone());
                        self.broadcast(&update);
                    }
                }
                Event::PtyWrite(text) => self.pty_in.extend_from_slice(text.as_bytes()),
                Event::ClipboardStore(_, data) => {
                    let msg = ToClient::Clipboard(proto::b64_encode(data.as_bytes()));
                    self.broadcast(&msg);
                }
                Event::ClipboardLoad(_, formatter) => {
                    // No host clipboard here; answer with empty content.
                    self.pty_in.extend_from_slice(formatter("").as_bytes());
                }
                Event::ColorRequest(index, formatter) => {
                    let rgb = self.term.term.colors()[index].unwrap_or_else(|| {
                        let (r, g, b) = if index < 256 {
                            crate::spans::xterm256_to_rgb(index as u8)
                        } else if index == 257 {
                            (0, 0, 0) // background
                        } else {
                            (229, 229, 229) // foreground-ish
                        };
                        alacritty_terminal::vte::ansi::Rgb { r, g, b }
                    });
                    self.pty_in.extend_from_slice(formatter(rgb).as_bytes());
                }
                Event::TextAreaSizeRequest(formatter) => {
                    let ws = WindowSize {
                        num_lines: self.term.rows,
                        num_cols: self.term.cols,
                        cell_width: 8,
                        cell_height: 16,
                    };
                    self.pty_in.extend_from_slice(formatter(ws).as_bytes());
                }
                _ => {}
            }
        }
    }

    /// v0 dvtm's warren_sync_name: mirror Claude's terminal title into the
    /// sidebar name. Skip leading control/space/non-ASCII bytes (spinner
    /// glyphs); manual renames are pinned; the idle title is noise.
    fn sync_title(&mut self, title: &str) {
        let stripped = title
            .as_bytes()
            .iter()
            .position(|&b| (0x21..0x80).contains(&b))
            .map(|i| &title[i..])
            .unwrap_or("");
        if self.meta.pinned
            || stripped.is_empty()
            || stripped == "Claude Code"
            || stripped == self.meta.display
        {
            return;
        }
        self.meta.display = stripped.to_string();
        let update = ToClient::MetaChanged(self.meta.clone());
        self.broadcast(&update);
    }

    /// Encode a semantic mouse event for the application, iff it subscribed
    /// (and in the encoding it asked for). col/row are 0-based cells.
    fn encode_mouse(&self, kind: MouseKind, col: u16, row: u16, mods: u8) -> Option<Vec<u8>> {
        let proto = self.term.mouse_proto();
        let button = match (kind, proto) {
            (_, MouseProto::None) => return None,
            (MouseKind::Down(b), _) => b as u32,
            (MouseKind::Up(b), _) => b as u32,
            (MouseKind::Drag(b), MouseProto::Drag | MouseProto::Motion) => 32 + b as u32,
            (MouseKind::Drag(_), _) => return None,
            (MouseKind::Moved, MouseProto::Motion) => 32 + 3,
            (MouseKind::Moved, _) => return None,
            (MouseKind::WheelUp, _) => 64,
            (MouseKind::WheelDown, _) => 65,
        };
        let button = button + mods as u32 % 32; // shift=4 alt=8 ctrl=16
        let release = matches!(kind, MouseKind::Up(_));
        if self.term.sgr_mouse() {
            let suffix = if release { 'm' } else { 'M' };
            Some(format!("\x1b[<{};{};{}{}", button, col + 1, row + 1, suffix).into_bytes())
        } else {
            // Legacy X10: printable range only.
            let b = if release { 3 } else { button } as u8;
            let cx = (col + 1).min(222) as u8 + 32;
            let cy = (row + 1).min(222) as u8 + 32;
            Some(vec![0x1b, b'[', b'M', 32 + b, cx, cy])
        }
    }

    fn reap_dead_conns(&mut self) {
        let dead: Vec<usize> = self
            .conns
            .iter()
            .filter(|(_, c)| c.dead)
            .map(|(k, _)| *k)
            .collect();
        for key in dead {
            if let Some(conn) = self.conns.remove(&key) {
                let _ = self.poller.delete(&conn.stream);
            }
        }
        // Reflect outbound-queue interest for the living.
        let mut queued: Vec<String> = Vec::new();
        for (key, conn) in &self.conns {
            let ev = PollEvent::new(*key, true, conn.wants_write());
            let r = self.poller.modify_with_mode(&conn.stream, ev, PollMode::Level);
            if self.log.is_some() && (conn.wants_write() || r.is_err()) {
                queued.push(format!(
                    "{key}:pending={}{}",
                    conn.pending(),
                    if r.is_err() { ",modify-ERR" } else { "" }
                ));
            }
        }
        if !queued.is_empty() {
            let msg = format!("  conns awaiting writability: [{}]", queued.join(" "));
            self.logf(&msg);
        }
    }
}

fn send_to(conn: &mut Conn, msg: &ToClient) {
    if let Ok(encoded) = proto::encode_frame(msg) {
        conn.send(&encoded);
    }
}

/// Printable-escape bytes for WARREN_LOG diagnostics.
fn escape_bytes(bytes: &[u8]) -> String {
    bytes
        .iter()
        .map(|&b| match b {
            0x20..=0x7e => (b as char).to_string(),
            0x1b => "\\e".to_string(),
            b'\r' => "\\r".to_string(),
            b'\n' => "\\n".to_string(),
            _ => format!("\\x{b:02x}"),
        })
        .collect()
}
