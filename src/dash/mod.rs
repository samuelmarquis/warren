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

/// One sidebar folder: the run of agents sharing a working directory,
/// labelled by that directory's own name — `~/Developer/Phylogen` reads
/// `Phylogen/`, never the path that got you there.
pub struct Folder {
    pub label: String,
    pub cwd: String,
    /// First agent index, and how many; members are contiguous by sort order.
    pub start: usize,
    pub len: usize,
    pub collapsed: bool,
}

impl Folder {
    pub fn agents(&self) -> std::ops::Range<usize> {
        self.start..self.start + self.len
    }
}

/// What occupies one sidebar line.
pub enum Row {
    /// Index into `folders()`.
    Folder(usize),
    /// Index into `agents`.
    Agent(usize),
    /// The pinned "+ new agent" tab.
    NewAgent,
}

/// Group agents into folders by working directory, in the order given.
///
/// Agents sharing a directory are contiguous (see `sort_agents`), so a folder
/// is just a range. Labels that would collide take one parent component to
/// tell them apart — still a name, not a path.
pub fn group_folders<'a>(
    cwds: impl Iterator<Item = &'a str>,
    collapsed: &HashSet<String>,
) -> Vec<Folder> {
    let mut out: Vec<Folder> = Vec::new();
    for (i, cwd) in cwds.enumerate() {
        match out.last_mut() {
            Some(f) if f.cwd == cwd => f.len += 1,
            _ => out.push(Folder {
                label: folder_label(cwd),
                cwd: cwd.to_string(),
                start: i,
                len: 1,
                collapsed: collapsed.contains(cwd),
            }),
        }
    }
    let mut seen: HashMap<&str, usize> = HashMap::new();
    for f in &out {
        *seen.entry(f.label.as_str()).or_insert(0) += 1;
    }
    let ambiguous: HashSet<String> =
        seen.iter().filter(|(_, n)| **n > 1).map(|(l, _)| l.to_string()).collect();
    for f in &mut out {
        if !ambiguous.contains(&f.label) {
            continue;
        }
        // One step up, and only the step: "src/" becoming "phylo/src/" is a
        // disambiguation, "…/Developer/phylo/src/" would be a path.
        let trimmed = f.cwd.trim_end_matches('/');
        let above = trimmed[..trimmed.len() - f.label.len().min(trimmed.len())]
            .trim_end_matches('/')
            .rsplit('/')
            .next()
            .unwrap_or("");
        if !above.is_empty() {
            f.label = format!("{above}/{}", f.label);
        }
    }
    out
}

/// Folder headers, the agents of the folders that are open, and the + tab.
pub fn layout_rows(folders: &[Folder]) -> Vec<Row> {
    let mut rows = Vec::new();
    for (i, folder) in folders.iter().enumerate() {
        rows.push(Row::Folder(i));
        if !folder.collapsed {
            rows.extend(folder.agents().map(Row::Agent));
        }
    }
    rows.push(Row::NewAgent);
    rows
}

/// A working directory's own name: the last component, `~` for the home
/// directory itself, and never a path.
fn folder_label(cwd: &str) -> String {
    if cwd.is_empty() {
        return "…".to_string(); // meta hasn't landed yet
    }
    if let Ok(home) = std::env::var("HOME") {
        if !home.is_empty() && cwd == home {
            return "~".to_string();
        }
    }
    match cwd.trim_end_matches('/').rsplit('/').next() {
        Some(name) if !name.is_empty() => name.to_string(),
        _ => "/".to_string(),
    }
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
    pub pending_focus: Option<String>,
    /// Folders folded away, by working directory. Viewer-local, like every
    /// other thing the dashboard knows: fold state is not worth a protocol.
    pub collapsed: HashSet<String>,
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

    /// The sidebar's folders, in display order.
    pub fn folders(&self) -> Vec<Folder> {
        group_folders(self.agents.iter().map(|a| a.meta.cwd.as_str()), &self.collapsed)
    }

    /// The sidebar, line by line.
    pub fn rows(&self, folders: &[Folder]) -> Vec<Row> {
        layout_rows(folders)
    }

    /// Is this agent's row on screen (its folder open)?
    fn shown(&self, idx: usize) -> bool {
        self.agents.get(idx).map(|a| !self.collapsed.contains(&a.meta.cwd)).unwrap_or(true)
    }

    /// Fold a folder away, or open it. Its agents keep running either way.
    pub fn toggle_folder(&mut self, folder: usize) {
        let Some(f) = self.folders().into_iter().nth(folder) else { return };
        if !self.collapsed.remove(&f.cwd) {
            self.collapsed.insert(f.cwd);
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

    /// `^Space <folder> <agent>`: both 1-based, 10 being the `0` key.
    pub fn goto(&mut self, folder: u8, agent: u8) {
        let folders = self.folders();
        let Some(f) = folders.get(folder as usize - 1) else {
            self.flash = Some(format!("no folder {folder}"));
            self.status_dirty = true;
            return;
        };
        let Some(idx) = (agent as usize)
            .checked_sub(1)
            .map(|n| f.start + n)
            .filter(|i| *i < f.start + f.len)
        else {
            self.flash = Some(format!("{}/ has no agent {agent}", f.label));
            self.status_dirty = true;
            return;
        };
        // Jumping into a folded folder opens it — you asked to go there.
        let cwd = f.cwd.clone();
        drop(folders);
        if self.collapsed.remove(&cwd) {
            self.sidebar_dirty = true;
        }
        self.set_focus(idx);
        self.enter_insert();
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

    /// NORMAL `Z`: sleep the focused agent, or wake it if it's already down.
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
        let Some(folder) = self.folders().into_iter().find(|f| f.agents().contains(&focus)) else {
            return;
        };
        let target = folder.start + n as usize - 1;
        if target >= folder.start + folder.len || target == self.focus {
            return;
        }
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
        collapsed: HashSet::new(),
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
                let name = agent.meta.name.clone();
                // Was the + tab focused? Keep it so unless this is the agent
                // the form just created — that one steals focus (v0 parity).
                let was_on_form = dash.on_newform();
                dash.agents.push(agent);
                dash.keys.push(key);
                if dash.pending_focus.as_deref() == Some(name.as_str()) {
                    dash.pending_focus = None;
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
        rank: &HashMap<&str, (u8, u64)>,
    ) -> ((u8, u64), &'a str, u8, u64) {
        let folder = rank.get(agent.meta.cwd.as_str()).copied().unwrap_or((u8::MAX, u64::MAX));
        (folder, agent.meta.cwd.as_str(), agent.meta.slot, agent.meta.created)
    }
    let mut rank: HashMap<&str, (u8, u64)> = HashMap::new();
    for agent in &dash.agents {
        let key = (agent.meta.slot, agent.meta.created);
        rank.entry(agent.meta.cwd.as_str())
            .and_modify(|r| *r = (*r).min(key))
            .or_insert(key);
    }
    if dash.agents.windows(2).all(|w| order(&w[0], &rank) <= order(&w[1], &rank)) {
        return;
    }
    // Owned keys from here: the sort has to outlive the agents' current home.
    let rank: HashMap<String, (u8, u64)> =
        rank.into_iter().map(|(k, v)| (k.to_string(), v)).collect();
    let owned = |a: &AgentConn| {
        let folder = rank.get(&a.meta.cwd).copied().unwrap_or((u8::MAX, u64::MAX));
        (folder, a.meta.cwd.clone(), a.meta.slot, a.meta.created)
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

#[cfg(test)]
mod tests {
    use super::*;

    fn folders(cwds: &[&str], folded: &[&str]) -> Vec<Folder> {
        let collapsed: HashSet<String> = folded.iter().map(|s| s.to_string()).collect();
        group_folders(cwds.iter().copied(), &collapsed)
    }

    fn labels(f: &[Folder]) -> Vec<String> {
        f.iter().map(|f| format!("{}/{}", f.label, f.len)).collect()
    }

    #[test]
    fn a_folder_is_a_directory_named_by_itself() {
        let f = folders(
            &[
                "/Users/x/Research",
                "/Users/x/Research",
                "/Users/x/Developer/Phylogen",
                "/Users/x/Developer",
            ],
            &[],
        );
        // ~/Developer/Phylogen reads Phylogen/, never the path to it.
        assert_eq!(labels(&f), ["Research/2", "Phylogen/1", "Developer/1"]);
        assert_eq!(f[0].agents(), 0..2);
        assert_eq!(f[1].agents(), 2..3);
    }

    #[test]
    fn colliding_names_take_one_parent_and_no_more() {
        let f = folders(&["/Users/x/phylo/src", "/Users/x/warren/src", "/Users/x/lone"], &[]);
        assert_eq!(labels(&f), ["phylo/src/1", "warren/src/1", "lone/1"]);
    }

    #[test]
    fn odd_directories_still_get_a_name() {
        let home = std::env::var("HOME").unwrap_or_default();
        if !home.is_empty() {
            assert_eq!(folder_label(&home), "~");
        }
        assert_eq!(folder_label("/"), "/");
        assert_eq!(folder_label("/opt/tools/"), "tools");
        assert_eq!(folder_label(""), "…"); // meta not in yet
    }

    #[test]
    fn folding_hides_a_folders_agents_but_never_the_new_tab() {
        let f = folders(&["/a/one", "/a/one", "/b/two"], &["/a/one"]);
        let rows = layout_rows(&f);
        let shape: Vec<String> = rows
            .iter()
            .map(|r| match r {
                Row::Folder(i) => format!("[{}]", f[*i].label),
                Row::Agent(i) => format!("{i}"),
                Row::NewAgent => "+".to_string(),
            })
            .collect();
        assert_eq!(shape, ["[one]", "[two]", "2", "+"]);

        let open = layout_rows(&folders(&["/a/one", "/a/one", "/b/two"], &[]));
        assert_eq!(open.len(), 6, "two headers, three agents, the + tab");
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
