//! The dashboard: a STATELESS viewer over the agent daemons.
//!
//! All durable state (screens, scrollback, names, colors, hook states) lives
//! in the per-agent daemons; this process just connects to every socket in
//! the run dir and renders. Killing it — or the SSH connection under it —
//! loses nothing: rerun `warren` and the same view reassembles.

pub mod conn;
mod input;
mod render;

use std::collections::HashSet;
use std::io::{ErrorKind, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use polling::{Event as PollEvent, Events, PollMode, Poller};

use crate::proto::{self, ToDaemon};
use conn::AgentConn;
use render::SIDEBAR_WIDTH;

const KEY_STDIN: usize = 0;
const KEY_SIGWINCH: usize = 1;
const KEY_FIRST_AGENT: usize = 16;

#[derive(PartialEq)]
pub enum Mode {
    Insert,
    Normal,
}

#[derive(PartialEq)]
pub enum Sub {
    None,
    Cmd,
}

pub struct Dash {
    pub agents: Vec<AgentConn>,
    /// poll key per agent, parallel to `agents`.
    pub keys: Vec<usize>,
    pub focus: usize,
    pub mode: Mode,
    pub sub: Sub,
    pub cmdline: String,
    pub flash: Option<String>,
    pub cols: u16,
    pub rows: u16,
    pub sidebar_dirty: bool,
    pub status_dirty: bool,
    pub full_redraw: bool,
}

impl Dash {
    pub fn focused(&self) -> Option<&AgentConn> {
        self.agents.get(self.focus)
    }

    pub fn focused_mut(&mut self) -> Option<&mut AgentConn> {
        self.agents.get_mut(self.focus)
    }

    fn pane_size(&self) -> (u16, u16) {
        (self.cols.saturating_sub(SIDEBAR_WIDTH).max(2), self.rows.saturating_sub(1).max(1))
    }

    pub fn send_input(&mut self, bytes: &[u8]) {
        if let Some(agent) = self.focused_mut() {
            agent.send(&ToDaemon::Input(proto::b64_encode(bytes)));
        }
    }

    pub fn enter_normal(&mut self) {
        self.mode = Mode::Normal;
        self.sub = Sub::None;
        self.status_dirty = true;
    }

    pub fn enter_insert(&mut self) {
        self.mode = Mode::Insert;
        self.sub = Sub::None;
        self.status_dirty = true;
    }

    fn set_focus(&mut self, idx: usize) {
        if idx < self.agents.len() && idx != self.focus {
            self.focus = idx;
            self.agents[idx].full_dirty = true;
            self.agents[idx].unseen = false; // examined
            self.sidebar_dirty = true;
            self.status_dirty = true;
        }
    }

    pub fn focus_next(&mut self) {
        if !self.agents.is_empty() {
            self.set_focus((self.focus + 1) % self.agents.len());
        }
    }

    pub fn focus_prev(&mut self) {
        if !self.agents.is_empty() {
            self.set_focus((self.focus + self.agents.len() - 1) % self.agents.len());
        }
    }

    pub fn focus_first(&mut self) {
        self.set_focus(0);
    }

    pub fn focus_last(&mut self) {
        if !self.agents.is_empty() {
            self.set_focus(self.agents.len() - 1);
        }
    }

    /// Jump to sidebar row N (1-based; 10 is the '0' key).
    pub fn focus_slot(&mut self, n: u8) {
        self.set_focus(n as usize - 1);
        self.enter_insert();
    }
}

pub fn run() -> Result<()> {
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) };

    let stdin = std::io::stdin();
    let saved = rustix::termios::tcgetattr(&stdin).context("warren must run on a terminal")?;
    let mut raw = saved.clone();
    raw.make_raw();
    rustix::termios::tcsetattr(&stdin, rustix::termios::OptionalActions::Now, &raw)?;
    print!("\x1b[?1049h\x1b[2J");
    let _ = std::io::stdout().flush();

    let result = run_inner(&stdin);

    print!("\x1b[0m\x1b[?25h\x1b[?1049l");
    let _ = std::io::stdout().flush();
    let _ = rustix::termios::tcsetattr(&stdin, rustix::termios::OptionalActions::Now, &saved);

    match result {
        Ok(input::Outcome::QuitKillAll) => println!("warren: killed all agents"),
        Ok(_) => println!("warren: detached — agents keep running ('warren' to return)"),
        Err(ref e) => eprintln!("warren: {e:#}"),
    }
    result.map(|_| ())
}

fn host_size() -> (u16, u16) {
    rustix::termios::tcgetwinsize(std::io::stdout())
        .map(|ws| (ws.ws_col.max(40), ws.ws_row.max(4)))
        .unwrap_or((80, 24))
}

fn run_inner(stdin: &std::io::Stdin) -> Result<input::Outcome> {
    let (cols, rows) = host_size();
    let mut dash = Dash {
        agents: Vec::new(),
        keys: Vec::new(),
        focus: 0,
        mode: Mode::Insert,
        sub: Sub::None,
        cmdline: String::new(),
        flash: None,
        cols,
        rows,
        sidebar_dirty: true,
        status_dirty: true,
        full_redraw: true,
    };

    let poller = Poller::new()?;
    let winch_rx = install_sigwinch()?;
    unsafe {
        poller.add_with_mode(stdin, PollEvent::readable(KEY_STDIN), PollMode::Level)?;
        poller.add_with_mode(&winch_rx, PollEvent::readable(KEY_SIGWINCH), PollMode::Level)?;
    }

    let mut next_key = KEY_FIRST_AGENT;
    discover_new(&mut dash, &poller, &mut next_key);
    if dash.agents.is_empty() {
        dash.mode = Mode::Normal;
    }

    let mut events = Events::new();
    let mut last_scan = Instant::now();
    let outcome = 'main: loop {
        // One frame's worth of paint, then wait for activity.
        sort_agents(&mut dash);
        let frame = render::paint(&mut dash);
        if !frame.is_empty() {
            let stdout = std::io::stdout();
            let mut out = stdout.lock();
            let _ = out.write_all(frame.as_bytes());
            let _ = out.flush();
        }

        events.clear();
        // 250ms tick: busy/idle is time-based (1500ms quiet threshold), so
        // transitions must repaint without any socket activity. v0 polled
        // its meta files at the same cadence.
        match poller.wait(&mut events, Some(Duration::from_millis(250))) {
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }

        for ev in events.iter() {
            match ev.key {
                KEY_STDIN => {
                    let mut buf = [0u8; 4096];
                    let n = match read_nb(stdin.lock(), &mut buf) {
                        Ok(n) => n,
                        Err(_) => 0,
                    };
                    if n > 0 {
                        match input::handle_bytes(&mut dash, &buf[..n]) {
                            input::Outcome::Continue => {}
                            input::Outcome::Quit => break 'main input::Outcome::Quit,
                            input::Outcome::QuitKillAll => {
                                kill_all(&mut dash);
                                break 'main input::Outcome::QuitKillAll;
                            }
                        }
                    }
                }
                KEY_SIGWINCH => {
                    let mut drain = [0u8; 64];
                    let _ = read_nb(&winch_rx, &mut drain);
                    let (cols, rows) = host_size();
                    if (cols, rows) != (dash.cols, dash.rows) {
                        dash.cols = cols;
                        dash.rows = rows;
                        dash.full_redraw = true;
                        let (pw, ph) = dash.pane_size();
                        for agent in &mut dash.agents {
                            agent.send(&ToDaemon::Resize { cols: pw, rows: ph });
                        }
                    }
                }
                key => {
                    if let Some(idx) = dash.keys.iter().position(|&k| k == key) {
                        if ev.readable {
                            dash.agents[idx].pump();
                            if dash.agents[idx].meta_dirty {
                                dash.sidebar_dirty = true;
                                dash.agents[idx].meta_dirty = false;
                            }
                        }
                        if ev.writable {
                            dash.agents[idx].flush();
                        }
                    }
                }
            }
        }

        reap_agents(&mut dash, &poller);
        update_busy_transitions(&mut dash);
        update_write_interest(&dash, &poller);

        if last_scan.elapsed() >= Duration::from_secs(1) {
            last_scan = Instant::now();
            discover_new(&mut dash, &poller, &mut next_key);
        }
    };

    Ok(outcome)
}

/// Connect any run-dir socket we aren't already attached to.
fn discover_new(dash: &mut Dash, poller: &Poller, next_key: &mut usize) {
    let Ok(entries) = std::fs::read_dir(crate::paths::run_dir()) else { return };
    let known: HashSet<_> = dash.agents.iter().map(|a| a.sock.clone()).collect();
    let (pw, ph) = dash.pane_size();
    for entry in entries.flatten() {
        let sock = entry.path();
        if sock.extension().and_then(|e| e.to_str()) != Some("sock") || known.contains(&sock) {
            continue;
        }
        match AgentConn::connect(sock.clone(), pw, ph) {
            Ok(agent) => {
                let key = *next_key;
                *next_key += 1;
                unsafe {
                    if poller
                        .add_with_mode(agent.stream(), PollEvent::readable(key), PollMode::Level)
                        .is_err()
                    {
                        continue;
                    }
                }
                dash.agents.push(agent);
                dash.keys.push(key);
                dash.sidebar_dirty = true;
            }
            Err(_) => {
                // Stale socket from a crashed daemon.
                let _ = std::fs::remove_file(&sock);
            }
        }
    }
}

/// Keep sidebar order = (slot, created); follow the focused agent across sorts.
fn sort_agents(dash: &mut Dash) {
    if dash.agents.len() < 2 {
        return;
    }
    let sorted = dash
        .agents
        .windows(2)
        .all(|w| (w[0].meta.slot, w[0].meta.created) <= (w[1].meta.slot, w[1].meta.created));
    if sorted {
        return;
    }
    let focused_name = dash.focused().map(|a| a.meta.name.clone());
    let mut zipped: Vec<(AgentConn, usize)> =
        dash.agents.drain(..).zip(dash.keys.drain(..)).collect();
    zipped.sort_by_key(|(a, _)| (a.meta.slot, a.meta.created));
    for (agent, key) in zipped {
        dash.agents.push(agent);
        dash.keys.push(key);
    }
    if let Some(name) = focused_name {
        if let Some(idx) = dash.agents.iter().position(|a| a.meta.name == name) {
            dash.focus = idx;
        }
    }
    dash.sidebar_dirty = true;
}

fn reap_agents(dash: &mut Dash, poller: &Poller) {
    let mut removed = false;
    let mut i = 0;
    while i < dash.agents.len() {
        if dash.agents[i].dead || dash.agents[i].exited.is_some() {
            let agent = dash.agents.remove(i);
            dash.keys.remove(i);
            let _ = poller.delete(agent.stream());
            if dash.focus >= i && dash.focus > 0 {
                dash.focus -= 1;
            }
            removed = true;
        } else {
            i += 1;
        }
    }
    if removed {
        dash.sidebar_dirty = true;
        dash.status_dirty = true;
        dash.full_redraw = true; // pane content belongs to a new focus now
        if dash.agents.is_empty() {
            dash.mode = Mode::Normal;
        }
    }
}

/// Repaint the sidebar on busy/idle edges; flag agents that went idle while
/// unfocused (the '*' mark, cleared when examined).
fn update_busy_transitions(dash: &mut Dash) {
    for i in 0..dash.agents.len() {
        let busy = dash.agents[i].busy();
        if dash.agents[i].last_busy != busy {
            dash.agents[i].last_busy = busy;
            dash.sidebar_dirty = true;
            if !busy && i != dash.focus {
                dash.agents[i].unseen = true;
            }
        }
    }
}

fn update_write_interest(dash: &Dash, poller: &Poller) {
    for (agent, &key) in dash.agents.iter().zip(&dash.keys) {
        let ev = PollEvent::new(key, true, agent.wants_write());
        let _ = poller.modify_with_mode(agent.stream(), ev, PollMode::Level);
    }
}

fn kill_all(dash: &mut Dash) {
    for agent in &mut dash.agents {
        agent.send(&ToDaemon::Kill);
        agent.flush();
    }
    // Give the Kill frames a beat to leave our socket buffers.
    std::thread::sleep(Duration::from_millis(150));
    for agent in &mut dash.agents {
        agent.flush();
    }
}

fn read_nb(mut src: impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    match src.read(buf) {
        Ok(n) => Ok(n),
        Err(e) if e.kind() == ErrorKind::WouldBlock => Ok(0),
        Err(e) => Err(e),
    }
}

/// Self-pipe for SIGWINCH so terminal resizes wake the poll loop.
fn install_sigwinch() -> Result<UnixStream> {
    use std::sync::atomic::{AtomicI32, Ordering};
    static WINCH_FD: AtomicI32 = AtomicI32::new(-1);

    extern "C" fn on_winch(_: libc::c_int) {
        let fd = WINCH_FD.load(Ordering::Relaxed);
        if fd >= 0 {
            unsafe { libc::write(fd, b"w".as_ptr().cast(), 1) };
        }
    }

    let (tx, rx) = UnixStream::pair()?;
    tx.set_nonblocking(true)?;
    rx.set_nonblocking(true)?;
    WINCH_FD.store(tx.as_raw_fd(), Ordering::Relaxed);
    std::mem::forget(tx); // lives for the process; the handler owns it now
    unsafe {
        let mut sa: libc::sigaction = std::mem::zeroed();
        sa.sa_sigaction = on_winch as extern "C" fn(libc::c_int) as usize;
        sa.sa_flags = libc::SA_RESTART;
        libc::sigaction(libc::SIGWINCH, &sa, std::ptr::null_mut());
    }
    Ok(rx)
}
