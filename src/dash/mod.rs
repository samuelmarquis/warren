//! The dashboard: a STATELESS viewer over the agent daemons.
//!
//! All durable state (screens, scrollback, names, colors, hook states) lives
//! in the per-agent daemons; this process just connects to every socket in
//! the run dir and renders. Killing it — or the SSH connection under it —
//! loses nothing: rerun `warren` and the same view reassembles.

pub mod conn;
mod forms;
mod input;
mod render;
mod tree;

use std::collections::HashSet;
use std::collections::HashMap;
use std::io::{ErrorKind, Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use polling::{Event as PollEvent, Events, PollMode, Poller};

use crate::proto::{self, Power, ToDaemon};
use conn::AgentConn;
use render::SIDEBAR_WIDTH;

/// One sheep-step per dashboard tick (the poll timeout below), which is a
/// sleepy four frames a second.
const SHEEP_TICK_MS: u128 = 250;

const KEY_STDIN: usize = 0;
const KEY_SIGWINCH: usize = 1;
const KEY_FIRST_AGENT: usize = 16;

#[derive(PartialEq)]
pub enum Mode {
    Insert,
    Normal,
}

#[derive(PartialEq, Clone, Copy)]
pub enum Sub {
    None,
    Cmd,
    Rename,
    Kill,
    /// A folder digit is in hand, waiting for the agent digit (`^Space 1 2`).
    Goto(u8),
}

pub struct Dash {
    pub agents: Vec<AgentConn>,
    /// poll key per agent, parallel to `agents`.
    pub keys: Vec<usize>,
    /// 0..agents.len() = an agent; agents.len() = the "+ new agent" tab.
    pub focus: usize,
    pub mode: Mode,
    pub sub: Sub,
    pub cmdline: String,
    pub flash: Option<String>,
    pub newform: forms::NewForm,
    pub editform: Option<forms::EditForm>,
    /// Agent name to focus once discovery sees its socket (form submission).
    /// The agent the form just asked for — (machine, name) — to focus the
    /// moment it shows up. A name alone would be ambiguous now that two
    /// machines can each have one.
    pub pending_focus: Option<(Option<String>, String)>,
    /// A `warren new` running on another machine, waiting to be reaped.
    pub spawn_job: Option<forms::Job>,
    /// Folders folded away, by working directory. Viewer-local, like every
    /// other thing the dashboard knows: fold state is not worth a protocol.
    pub collapsed: HashSet<String>,
    /// Other machines whose agents share this sidebar.
    pub hosts: crate::remote::Hosts,
    /// What to call this machine once there is another one to tell it from.
    pub here: String,
    /// 0-based (row, col, box_w, box_h) of the color grid, when on screen.
    pub palette_geom: Option<(u16, u16, u16, u16)>,
    /// Dashboard start, the clock the sleeping-sheep animation runs on.
    pub started: Instant,
    /// Animation frame the sleep badge was last drawn at.
    pub sheep_frame: u64,
    pub cols: u16,
    pub rows: u16,
    pub sidebar_dirty: bool,
    pub status_dirty: bool,
    pub form_dirty: bool,
    pub full_redraw: bool,
}

impl Dash {
    pub fn focused(&self) -> Option<&AgentConn> {
        self.agents.get(self.focus)
    }

    pub fn focused_mut(&mut self) -> Option<&mut AgentConn> {
        let focus = self.focus;
        self.agents.get_mut(focus)
    }

    /// Is the "+ new agent" tab focused (so the pane shows the form)?
    pub fn on_newform(&self) -> bool {
        self.focus >= self.agents.len()
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
        let idx = idx.min(self.agents.len()); // last position = the + tab
        if idx == self.focus {
            return;
        }
        self.focus = idx;
        if let Some(agent) = self.agents.get_mut(idx) {
            agent.full_dirty = true;
            agent.unseen = false; // examined
        } else {
            // Arriving on the + tab gets a fresh form (v0's nf_reset).
            self.newform = forms::NewForm::reset();
            self.form_dirty = true;
        }
        self.sidebar_dirty = true;
        self.status_dirty = true;
    }

    // ----------------------------------------------------------- folders

    /// The whole sidebar: machines, their folders, and what is in them.
    pub fn sections(&self) -> Vec<tree::Section> {
        let entries: Vec<tree::Entry> = self
            .agents
            .iter()
            .enumerate()
            .map(|(index, a)| tree::Entry {
                index,
                host: a.host.as_deref(),
                cwd: a.meta.cwd.as_str(),
            })
            .collect();
        let hosts: Vec<tree::HostView> = self
            .hosts
            .hosts
            .iter()
            .map(|h| tree::HostView {
                dest: &h.dest,
                label: &h.label,
                status: match &h.state {
                    crate::remote::HostState::Live => None,
                    crate::remote::HostState::Connecting => Some("connecting…".to_string()),
                    crate::remote::HostState::Offline(_) => Some("reconnecting…".to_string()),
                },
                ghosts: &h.ghosts,
            })
            .collect();
        let mut sections = tree::build(&entries, &hosts, &self.collapsed, &self.here);
        tree::disambiguate(&mut sections);
        sections
    }

    /// Is this agent's row on screen (its folder open, its machine answering)?
    fn shown(&self, idx: usize) -> bool {
        self.agents.get(idx).map(|a| !self.collapsed.contains(&a.meta.cwd)).unwrap_or(true)
    }

    /// Fold a folder away, or open it. Its agents keep running either way.
    pub fn toggle_folder(&mut self, cwd: &str) {
        if !self.collapsed.remove(cwd) {
            self.collapsed.insert(cwd.to_string());
        }
        self.sidebar_dirty = true;
        self.status_dirty = true;
    }

    /// First digit of `^Space <folder> <agent>`: hold it, and show which
    /// folder is pending in the status bar.
    pub fn begin_goto(&mut self, folder: u8) {
        self.sub = Sub::Goto(folder);
        self.status_dirty = true;
    }

    /// `^Space <folder> <agent>`: both 1-based, 10 being the `0` key. Folder
    /// numbers run through the whole sidebar, so this reaches another machine
    /// without a third digit.
    pub fn goto(&mut self, folder: u8, agent: u8) {
        let sections = self.sections();
        match tree::locate(&sections, folder as usize, agent as usize) {
            Ok(index) => {
                // Jumping into a folded folder opens it — you asked to go there.
                let cwd = self.agents[index].meta.cwd.clone();
                drop(sections);
                if self.collapsed.remove(&cwd) {
                    self.sidebar_dirty = true;
                }
                self.set_focus(index);
                self.enter_insert();
            }
            Err(why) => {
                self.flash = Some(why);
                self.status_dirty = true;
            }
        }
    }

    // -------------------------------------------------------------- focus

    /// Next agent in sidebar order, skipping anything folded away; the + tab
    /// is always reachable.
    pub fn focus_next(&mut self) {
        let total = self.agents.len() + 1;
        let mut i = self.focus;
        for _ in 0..total {
            i = (i + 1) % total;
            if i == self.agents.len() || self.shown(i) {
                self.set_focus(i);
                return;
            }
        }
    }

    pub fn focus_prev(&mut self) {
        let total = self.agents.len() + 1;
        let mut i = self.focus;
        for _ in 0..total {
            i = (i + total - 1) % total;
            if i == self.agents.len() || self.shown(i) {
                self.set_focus(i);
                return;
            }
        }
    }

    pub fn focus_first(&mut self) {
        if let Some(i) = (0..self.agents.len()).find(|i| self.shown(*i)) {
            self.set_focus(i);
        }
    }

    pub fn focus_last(&mut self) {
        if let Some(i) = (0..self.agents.len()).rev().find(|i| self.shown(*i)) {
            self.set_focus(i);
        }
    }

    /// Jump to the + tab ready to type (NORMAL `n`).
    pub fn open_new_form(&mut self) {
        self.set_focus(self.agents.len());
        self.enter_insert();
    }

    /// Frames elapsed at the sleeping-sheep rate — driven by the clock, not
    /// by how often the dashboard happens to paint, so the flock keeps its
    /// pace no matter what else is going on.
    pub fn anim_frame(&self) -> u64 {
        (self.started.elapsed().as_millis() / SHEEP_TICK_MS) as u64
    }

    /// NORMAL `z`: sleep the focused agent, or wake it if it's already down.
    ///
    /// Sleeping kills claude to give its memory back and keeps the tab; the
    /// two refusals are the ones you can't undo — mid-turn (the turn would be
    /// lost) and before the agent's first hook has told us its session id
    /// (there would be nothing to resume).
    pub fn toggle_sleep(&mut self) {
        let Some(agent) = self.focused() else { return };
        let refusal = if agent.asleep() {
            None // waking is always allowed
        } else if agent.busy() {
            Some("AGENT BUSY")
        } else if !agent.resumable() {
            Some("NO SESSION YET")
        } else {
            None
        };
        if let Some(msg) = refusal {
            self.flash = Some(msg.to_string());
            self.status_dirty = true;
            return;
        }
        let msg = if agent.asleep() { ToDaemon::Wake } else { ToDaemon::Sleep };
        if let Some(agent) = self.focused_mut() {
            agent.send(&msg);
        }
    }

    /// Typing at a sleeping agent wakes it; the daemon holds the keystrokes
    /// until claude is back and then feeds them to the resumed prompt.
    pub fn wake_on_input(&mut self) {
        if self.focused().map(|a| a.power()) != Some(Power::Asleep) {
            return;
        }
        if let Some(agent) = self.focused_mut() {
            agent.send(&ToDaemon::Wake);
        }
    }

    /// Shift+digit: swap the focused agent with position N *of its own
    /// folder*. Folders come from working directories, so renumbering moves
    /// an agent within its folder and can never smuggle it into another.
    ///
    /// Slots live in daemon meta, so the swap is two SetMeta messages; the
    /// MetaChanged broadcasts resort the sidebar (and any other viewer's).
    pub fn swap_with_row(&mut self, n: u8) {
        if self.on_newform() {
            return;
        }
        let focus = self.focus;
        let sections = self.sections();
        let Some(folder) = sections
            .iter()
            .flat_map(|s| s.folders.iter())
            .find(|f| f.items.contains(&tree::Item::Live(focus)))
        else {
            return;
        };
        let target = match folder.items.get(n as usize - 1) {
            Some(tree::Item::Live(index)) => *index,
            _ => return,
        };
        if target == self.focus {
            return;
        }
        drop(sections);
        let a_slot = self.agents[self.focus].meta.slot;
        let b_slot = self.agents[target].meta.slot;
        self.agents[target].send(&ToDaemon::SetMeta {
            name: None,
            color: None,
            pinned: None,
            slot: Some(a_slot),
        });
        let focused = &mut self.agents[self.focus];
        focused.send(&ToDaemon::SetMeta {
            name: None,
            color: None,
            pinned: None,
            slot: Some(b_slot),
        });
    }
}

pub fn run() -> Result<()> {
    unsafe { libc::signal(libc::SIGPIPE, libc::SIG_IGN) };

    let stdin = std::io::stdin();
    let saved = rustix::termios::tcgetattr(&stdin).context("warren must run on a terminal")?;
    let mut raw = saved.clone();
    raw.make_raw();
    rustix::termios::tcsetattr(&stdin, rustix::termios::OptionalActions::Now, &raw)?;
    // Altscreen + SGR mouse (button-drag tracking): sidebar clicks/wheel for
    // us, everything over the pane forwarded to the focused agent.
    print!("\x1b[?1049h\x1b[2J\x1b[?1002h\x1b[?1006h");
    let _ = std::io::stdout().flush();

    let result = run_inner(&stdin);

    print!("\x1b[?1002l\x1b[?1006l\x1b[0m\x1b[?25h\x1b[?1049l");
    let _ = std::io::stdout().flush();
    let _ = rustix::termios::tcsetattr(&stdin, rustix::termios::OptionalActions::Now, &saved);

    match result {
        Ok(input::Outcome::QuitKillAll) => println!("warren: killed all agents"),
        Ok(_) => println!("warren: detached — agents keep running ('warren' to return)"),
        Err(ref e) => eprintln!("warren: {e:#}"),
    }
    result.map(|_| ())
}

/// The new-agent form offers the machines the hosts file names, and only
/// grows the field once there is a second machine to mean anything.
fn sync_form_machines(dash: &mut Dash) {
    let dests: Vec<String> = dash.hosts.hosts.iter().map(|h| h.dest.clone()).collect();
    if dash.newform.machines != dests {
        dash.newform.sync_machines(&dests);
        dash.form_dirty = true;
    }
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
        newform: forms::NewForm::reset(),
        editform: None,
        pending_focus: None,
        spawn_job: None,
        collapsed: HashSet::new(),
        hosts: crate::remote::Hosts::default(),
        here: local_hostname(),
        palette_geom: None,
        started: Instant::now(),
        sheep_frame: u64::MAX,
        cols,
        rows,
        sidebar_dirty: true,
        status_dirty: true,
        form_dirty: true,
        full_redraw: true,
    };

    let poller = Poller::new()?;
    let winch_rx = install_sigwinch()?;
    unsafe {
        poller.add_with_mode(stdin, PollEvent::readable(KEY_STDIN), PollMode::Level)?;
        poller.add_with_mode(&winch_rx, PollEvent::readable(KEY_SIGWINCH), PollMode::Level)?;
    }

    let mut next_key = KEY_FIRST_AGENT;
    dash.hosts.reload(&poller);
    sync_form_machines(&mut dash);
    dash.hosts.dial(&poller, &mut next_key);
    discover_new(&mut dash, &poller, &mut next_key);
    // Initial focus: the first agent (discover_new's stay-on-form rule is for
    // MID-SESSION arrivals; before discovery "empty == on the + tab" lies).
    dash.focus = 0;
    if dash.agents.is_empty() {
        // Fresh dashboard lands on the new-agent form, ready to navigate.
        dash.mode = Mode::Normal;
    }

    // Opt-in field diagnostics, same file the daemons use (WARREN_LOG).
    let mut log = std::env::var("WARREN_LOG").ok().and_then(|p| {
        std::fs::OpenOptions::new().create(true).append(true).open(p).ok()
    });
    let log_t0 = Instant::now();
    macro_rules! dlog {
        ($($arg:tt)*) => {
            if let Some(f) = &mut log {
                use std::io::Write;
                let _ = writeln!(
                    f,
                    "[dash] {:>9.3}ms {}",
                    log_t0.elapsed().as_secs_f64() * 1000.0,
                    format!($($arg)*)
                );
            }
        };
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
        // The empty-diff frame is ~14 bytes of sync-update bracketing.
        if frame.len() > 20 {
            dlog!("paint: {} bytes", frame.len());
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
        if log.is_some() && events.iter().next().is_some() {
            let keys: Vec<String> = events
                .iter()
                .map(|e| {
                    format!(
                        "{}{}{}",
                        e.key,
                        if e.readable { "r" } else { "" },
                        if e.writable { "w" } else { "" }
                    )
                })
                .collect();
            dlog!("wake: [{}]", keys.join(","));
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
                key if dash.hosts.hosts.iter().any(|h| h.key == Some(key)) => {
                    // A machine talking: the roster it just printed, or the
                    // silence of an ssh that ended.
                    if dash.hosts.read_watcher(key, &poller) {
                        discover_remote(&mut dash, &poller, &mut next_key);
                        dash.sidebar_dirty = true;
                        dash.status_dirty = true;
                    }
                }
                key => {
                    if let Some(idx) = dash.keys.iter().position(|&k| k == key) {
                        if ev.readable {
                            dash.agents[idx].pump();
                            if log.is_some() {
                                let a = &dash.agents[idx];
                                dlog!(
                                    "pump {}: rows={} full={} dead={}",
                                    a.meta.name,
                                    a.damage_rows.len(),
                                    a.full_dirty,
                                    a.dead
                                );
                            }
                            if dash.agents[idx].meta_dirty {
                                dash.sidebar_dirty = true;
                                dash.agents[idx].meta_dirty = false;
                            }
                            // OSC 52: only the FOCUSED agent may write the
                            // host clipboard (a backgrounded agent can't
                            // silently clobber it — v0 rule).
                            let payload = dash.agents[idx].clipboard_pending.take();
                            if let Some(b64) = payload {
                                if idx == dash.focus {
                                    let stdout = std::io::stdout();
                                    let mut out = stdout.lock();
                                    let _ = write!(out, "\x1b]52;c;{b64}\x07");
                                    let _ = out.flush();
                                }
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
        forms::poll_jobs(&mut dash);
        update_busy_transitions(&mut dash);
        update_write_interest(&dash, &poller);

        if last_scan.elapsed() >= Duration::from_secs(1) {
            last_scan = Instant::now();
            discover_new(&mut dash, &poller, &mut next_key);
            if dash.hosts.reload(&poller) {
                dash.sidebar_dirty = true;
            }
            sync_form_machines(&mut dash);
            let dialled = dash.hosts.dial(&poller, &mut next_key);
            discover_remote(&mut dash, &poller, &mut next_key);
            park_unreachable(&mut dash, &poller);
            if log.is_some() {
                let states: Vec<String> = dash
                    .hosts
                    .hosts
                    .iter()
                    .map(|h| {
                        format!("{}={:?} key={:?} roster={}", h.label, h.state, h.key, h.roster.len())
                    })
                    .collect();
                dlog!("hosts: [{}] dialled={dialled:?}", states.join(" | "));
            }
        }
    };

    Ok(outcome)
}

/// Focus an agent the new-agent form just asked for, whenever it turns up.
/// A local spawn is often attached before the far side of this call, while a
/// remote one is still a row its machine has not reported yet — so this both
/// focuses what is here and remembers what is not.
pub fn focus_when_it_arrives(dash: &mut Dash, host: Option<String>, name: String) {
    let here = dash.agents.iter().position(|a| a.ident() == (host.as_deref(), name.as_str()));
    match here {
        Some(idx) => {
            dash.pending_focus = None;
            dash.focus = idx;
            dash.agents[idx].full_dirty = true;
            dash.enter_insert();
        }
        None => dash.pending_focus = Some((host, name)),
    }
}

impl Dash {
    /// Was this agent the one the form was waiting for? Clears the wait.
    fn claim_pending(&mut self, host: Option<&str>, name: &str) -> bool {
        match &self.pending_focus {
            Some((h, n)) if h.as_deref() == host && n == name => {
                self.pending_focus = None;
                true
            }
            _ => false,
        }
    }
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
                let name = agent.meta.name.clone();
                // Was the + tab focused? Keep it so unless this is the agent
                // the form just created — that one steals focus (v0 parity).
                let was_on_form = dash.on_newform();
                dash.agents.push(agent);
                dash.keys.push(key);
                if dash.claim_pending(None, &name) {
                    dash.focus = dash.agents.len() - 1;
                    dash.agents.last_mut().unwrap().full_dirty = true;
                    dash.enter_insert();
                } else if was_on_form {
                    dash.focus = dash.agents.len(); // stay on the + tab
                }
                dash.sidebar_dirty = true;
            }
            Err(_) => {
                // Stale socket from a crashed daemon.
                let _ = std::fs::remove_file(&sock);
            }
        }
    }
}

/// Attach anything a machine reports that we are not already showing.
fn discover_remote(dash: &mut Dash, poller: &Poller, next_key: &mut usize) {
    let (pw, ph) = dash.pane_size();
    let mut wanted: Vec<(String, String)> = Vec::new();
    for host in &dash.hosts.hosts {
        if !host.reachable() {
            continue;
        }
        for name in &host.roster {
            let known = dash
                .agents
                .iter()
                .any(|a| a.ident() == (Some(host.dest.as_str()), name.as_str()));
            if !known {
                wanted.push((host.dest.clone(), name.clone()));
            }
        }
    }
    for (dest, name) in wanted {
        let Some(host) = dash.hosts.get(&dest) else { continue };
        // One `ssh … warren __pipe` per agent, all sharing the machine's
        // single ssh connection. What comes back is an ordinary UnixStream.
        let agent = match AgentConn::attach_remote(host, &name, pw, ph) {
            Ok(agent) => agent,
            Err(_) => continue, // the watcher will report why, if it is fatal
        };
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
        let was_on_form = dash.on_newform();
        dash.agents.push(agent);
        dash.keys.push(key);
        if dash.claim_pending(Some(&dest), &name) {
            dash.focus = dash.agents.len() - 1;
            dash.agents.last_mut().unwrap().full_dirty = true;
            dash.enter_insert();
        } else if was_on_form {
            dash.focus = dash.agents.len(); // stay on the + tab
        }
        dash.sidebar_dirty = true;
    }
}

/// A machine stopped answering: keep its rows as ghosts and let go of the
/// connections, rather than showing tabs that cannot come back until it does.
fn park_unreachable(dash: &mut Dash, poller: &Poller) {
    let offline: HashSet<String> = dash
        .hosts
        .hosts
        .iter()
        .filter(|h| !h.reachable())
        .map(|h| h.dest.clone())
        .collect();
    if offline.is_empty() {
        return;
    }
    let mut parked: HashMap<String, Vec<crate::remote::Ghost>> = HashMap::new();
    let mut i = 0;
    while i < dash.agents.len() {
        match dash.agents[i].host.clone() {
            Some(dest) if offline.contains(&dest) => {
                let agent = dash.agents.remove(i);
                dash.keys.remove(i);
                let _ = poller.delete(agent.stream());
                parked.entry(dest).or_default().push(agent.ghost());
                if dash.focus >= i && dash.focus > 0 {
                    dash.focus -= 1;
                }
                dash.sidebar_dirty = true;
                dash.full_redraw = true;
            }
            _ => i += 1,
        }
    }
    for (dest, ghosts) in parked {
        dash.hosts.park(&dest, ghosts);
    }
}

/// Sidebar order: folders by their earliest member, agents by (slot, created)
/// within a folder — so a working directory's agents are always contiguous,
/// which is what lets a folder be a range. Follows the focused agent across
/// sorts.
fn sort_agents(dash: &mut Dash) {
    if dash.agents.len() < 2 {
        return;
    }
    // A folder ranks by its earliest agent, so swapping two agents inside one
    // can never reshuffle the folders around it. This runs every frame, so
    // the already-sorted path (nearly all of them) allocates nothing.
    fn order<'a>(
        agent: &'a AgentConn,
        rank: &HashMap<(Option<&str>, &str), (u8, u64)>,
        hosts: &[String],
    ) -> (usize, (u8, u64), &'a str, u8, u64) {
        let key = (agent.host.as_deref(), agent.meta.cwd.as_str());
        let folder = rank.get(&key).copied().unwrap_or((u8::MAX, u64::MAX));
        // This machine first, then the others in the order the file lists.
        let machine = match &agent.host {
            None => 0,
            Some(dest) => hosts.iter().position(|h| h == dest).map(|i| i + 1).unwrap_or(usize::MAX),
        };
        (machine, folder, agent.meta.cwd.as_str(), agent.meta.slot, agent.meta.created)
    }
    let hosts: Vec<String> = dash.hosts.hosts.iter().map(|h| h.dest.clone()).collect();
    let mut rank: HashMap<(Option<&str>, &str), (u8, u64)> = HashMap::new();
    for agent in &dash.agents {
        let key = (agent.meta.slot, agent.meta.created);
        rank.entry((agent.host.as_deref(), agent.meta.cwd.as_str()))
            .and_modify(|r| *r = (*r).min(key))
            .or_insert(key);
    }
    if dash.agents.windows(2).all(|w| order(&w[0], &rank, &hosts) <= order(&w[1], &rank, &hosts)) {
        return;
    }
    // Owned keys from here: the sort has to outlive the agents' current home.
    let rank: HashMap<(Option<String>, String), (u8, u64)> =
        rank.into_iter().map(|((h, c), v)| ((h.map(String::from), c.to_string()), v)).collect();
    let owned = |a: &AgentConn| {
        let key = (a.host.clone(), a.meta.cwd.clone());
        let folder = rank.get(&key).copied().unwrap_or((u8::MAX, u64::MAX));
        let machine = match &a.host {
            None => 0,
            Some(dest) => hosts.iter().position(|h| h == dest).map(|i| i + 1).unwrap_or(usize::MAX),
        };
        (machine, folder, a.meta.cwd.clone(), a.meta.slot, a.meta.created)
    };
    let focused_name = dash.focused().map(|a| a.meta.name.clone());
    let mut zipped: Vec<(AgentConn, usize)> =
        dash.agents.drain(..).zip(dash.keys.drain(..)).collect();
    zipped.sort_by_key(|(a, _)| owned(a));
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
            // Keep what a remote agent looked like. If its machine is simply
            // gone, that row stays on screen dimmed; if the agent really did
            // end, the machine's next roster (within the second) clears it.
            if let Some(dest) = agent.host.clone() {
                dash.hosts.park(&dest, vec![agent.ghost()]);
            }
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

/// This machine's short name, for the sidebar heading once there is another
/// machine to tell it apart from.
fn local_hostname() -> String {
    let mut buf = [0u8; 256];
    // SAFETY: a plain gethostname into a buffer we own; the result is
    // NUL-terminated within it or truncated, and we only read up to the NUL.
    let ok = unsafe { libc::gethostname(buf.as_mut_ptr().cast(), buf.len()) } == 0;
    if !ok {
        return "here".to_string();
    }
    let end = buf.iter().position(|&b| b == 0).unwrap_or(buf.len());
    String::from_utf8_lossy(&buf[..end])
        .split('.')
        .next()
        .filter(|s| !s.is_empty())
        .unwrap_or("here")
        .to_string()
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
