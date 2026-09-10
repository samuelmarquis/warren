//! The new-agent form (the "+ new agent" tab's content), the edit form
//! (title + color of a running agent), and the shared 16x16 color palette.
//!
//! v0's modal model is preserved: the form is just what the + tab shows.
//! INSERT edits it, NORMAL navigates the dashboard as usual, so you can
//! always leave without creating anything.

use std::fmt::Write;
use std::io::Read;
use std::process::Child;
use std::time::{Duration, Instant};

use crate::kind::Kind;
use crate::proto::ToDaemon;
use crate::sessions::Session;
use crate::spans;

use super::render::SIDEBAR_WIDTH;
use super::{Dash, Mode, focus_when_it_arrives};

#[derive(Clone, Copy, PartialEq, Debug)]
pub enum NField {
    /// Which machine this agent will run on. Absent unless there is more
    /// than one to choose between.
    Machine,
    Kind,
    Mode,
    Title,
    Root,
    List,
    Sys,
    Extra,
    Color,
}

pub const MODE_NEW: u8 = 0;
pub const MODE_RESUME: u8 = 1;
pub const MODE_CONTINUE: u8 = 2;

/// The harnesses the form offers, in the order it cycles them.
pub const KINDS: [Kind; 2] = [Kind::Claude, Kind::Omp];

/// A machine one hop away, answering a question the form asked it. Both
/// kinds of job are polled, never waited on: the dashboard does not block on
/// a peer, and the far side of an ssh is a peer like any other.
pub struct Job {
    pub child: Child,
    pub dest: String,
    pub buf: String,
    pub started: Instant,
}

impl Job {
    fn new(child: Child, dest: String) -> Job {
        Job { child, dest, buf: String::new(), started: Instant::now() }
    }

    /// Long enough that a slow machine still answers, short enough that a
    /// wedged one gives the field back.
    fn expired(&self) -> bool {
        self.started.elapsed() > Duration::from_secs(20)
    }

    fn kill(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

pub struct NewForm {
    /// 0 is this machine; 1.. index `machines`. Kept in sync with the hosts
    /// file by destination, not by position, so editing that file cannot
    /// silently retarget a form someone is in the middle of filling in.
    pub machine: usize,
    /// ssh destinations, in the order the hosts file lists them.
    pub machines: Vec<String>,
    /// Index into KINDS. Which harness this agent will run — the only thing
    /// on this form the sidebar will never show.
    pub kind: usize,
    pub mode: u8,
    pub field: usize,
    pub title: String,
    pub root: String,
    pub sessions: Option<Vec<Session>>,
    /// The resume picker's list being fetched from another machine.
    pub sess_job: Option<Job>,
    pub sess_sel: usize,
    /// `--system-prompt` value; empty = omit the flag.
    pub sys: String,
    /// Raw extra CLI args appended to the claude command line.
    pub extra: String,
    pub color: u16, // 0 = none, 1..=255 xterm index
}

impl NewForm {
    pub fn reset() -> NewForm {
        NewForm {
            machine: 0,
            machines: Vec::new(),
            kind: 0,
            mode: MODE_NEW,
            field: 0,
            title: String::new(),
            root: std::env::var("HOME").unwrap_or_else(|_| "/".into()),
            sessions: None,
            sess_job: None,
            sess_sel: 0,
            sys: String::new(),
            extra: String::new(),
            color: 0,
        }
    }

    /// Machine leads, when there is one to pick: it decides what every field
    /// under it means. With no hosts configured the form is what it was.
    pub fn fields(&self) -> &'static [NField] {
        match (self.machines.is_empty(), self.mode) {
            (true, MODE_RESUME) => &[
                NField::Kind,
                NField::Mode,
                NField::List,
                NField::Sys,
                NField::Extra,
                NField::Color,
            ],
            (false, MODE_RESUME) => &[
                NField::Machine,
                NField::Kind,
                NField::Mode,
                NField::List,
                NField::Sys,
                NField::Extra,
                NField::Color,
            ],
            (true, MODE_CONTINUE) => &[
                NField::Kind,
                NField::Mode,
                NField::Root,
                NField::Sys,
                NField::Extra,
                NField::Color,
            ],
            (false, MODE_CONTINUE) => &[
                NField::Machine,
                NField::Kind,
                NField::Mode,
                NField::Root,
                NField::Sys,
                NField::Extra,
                NField::Color,
            ],
            (true, _) => &[
                NField::Kind,
                NField::Mode,
                NField::Title,
                NField::Root,
                NField::Sys,
                NField::Extra,
                NField::Color,
            ],
            (false, _) => &[
                NField::Machine,
                NField::Kind,
                NField::Mode,
                NField::Title,
                NField::Root,
                NField::Sys,
                NField::Extra,
                NField::Color,
            ],
        }
    }

    /// Where this agent will run: None is here, Some is an ssh destination.
    pub fn dest(&self) -> Option<&str> {
        match self.machine {
            0 => None,
            i => self.machines.get(i - 1).map(String::as_str),
        }
    }

    /// Take the hosts file's list, keeping the selection by name. A machine
    /// that left the file falls back to here rather than to whichever host
    /// happens to hold its old index.
    pub fn sync_machines(&mut self, dests: &[String]) {
        if self.machines == dests {
            return;
        }
        let chosen = self.dest().map(str::to_string);
        self.machines = dests.to_vec();
        self.machine = match chosen {
            Some(dest) => {
                self.machines.iter().position(|d| *d == dest).map(|i| i + 1).unwrap_or(0)
            }
            None => 0,
        };
    }

    /// The chips the Machine row offers: here, then the hosts file's order.
    fn machine_labels(&self) -> Vec<&str> {
        let mut labels = vec!["here"];
        labels.extend(self.machines.iter().map(|d| {
            // The sidebar's name for a machine is the destination without a
            // user@; the form says the same thing it does.
            d.rsplit('@').next().unwrap_or(d)
        }));
        labels
    }

    /// Root dir means one thing here and another there, and the default is
    /// the only part warren owns: an untouched `$HOME` follows the machine.
    fn follow_machine(&mut self) {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
        match self.machine {
            0 if self.root == "~" => self.root = home,
            0 => {}
            // `~` is left for the far side's own expand_dir to resolve; this
            // machine has no business guessing another one's home.
            _ if self.root == home => self.root = "~".into(),
            _ => {}
        }
    }

    pub fn agent_kind(&self) -> Kind {
        KINDS[self.kind.min(KINDS.len() - 1)]
    }

    pub fn active(&self) -> NField {
        self.fields()[self.field.min(self.fields().len() - 1)]
    }

    /// Drop the cached list: a different harness, or a different machine,
    /// has different sessions.
    fn forget_sessions(&mut self) {
        self.sessions = None;
        self.sess_sel = 0;
        if let Some(mut job) = self.sess_job.take() {
            job.kill();
        }
    }
}

/// Fill the resume picker for whatever machine is selected. Here that is a
/// directory scan and is done by the time it returns; anywhere else it is an
/// `ssh … warren sessions`, started now and collected in `poll_jobs`.
fn ensure_sessions(dash: &mut Dash) {
    let form = &mut dash.newform;
    if form.sessions.is_some() || form.sess_job.is_some() {
        return;
    }
    let kind = form.agent_kind();
    let Some(dest) = form.dest().map(str::to_string) else {
        form.sessions = Some(kind.sessions());
        form.sess_sel = 0;
        return;
    };
    let Some(host) = dash.hosts.get(&dest) else {
        dash.newform.sessions = Some(Vec::new());
        return;
    };
    if !host.reachable() {
        dash.newform.sessions = Some(Vec::new());
        dash.flash = Some(format!("{} is not answering", host.label));
        dash.status_dirty = true;
        return;
    }
    match host.sessions(kind) {
        Ok(child) => dash.newform.sess_job = Some(Job::new(child, dest)),
        Err(e) => {
            dash.newform.sessions = Some(Vec::new());
            dash.flash = Some(format!("sessions on {dest}: {e}"));
            dash.status_dirty = true;
        }
    }
}

/// One tab-separated line of `warren sessions`: id, mtime, cwd, title.
fn parse_sessions(out: &str) -> Vec<Session> {
    out.lines()
        .filter_map(|line| {
            let mut cols = line.split('\t');
            let id = cols.next()?;
            let mtime = cols.next()?;
            let cwd = cols.next()?;
            let title = cols.next().unwrap_or("");
            if id.is_empty() {
                return None;
            }
            Some(Session {
                id: id.to_string(),
                mtime: mtime.parse::<f64>().unwrap_or(0.0),
                cwd: cwd.to_string(),
                title: title.to_string(),
            })
        })
        .collect()
}

/// Collect anything a machine owes us: the resume picker's list, and the
/// result of a `warren new` sent one hop away. Called every turn of the poll
/// loop, and never blocks.
pub fn poll_jobs(dash: &mut Dash) {
    poll_session_job(dash);
    poll_spawn_job(dash);
}

fn poll_session_job(dash: &mut Dash) {
    let Some(job) = dash.newform.sess_job.as_mut() else { return };
    if job.expired() {
        let dest = job.dest.clone();
        job.kill();
        dash.newform.sess_job = None;
        dash.newform.sessions = Some(Vec::new());
        dash.flash = Some(format!("{dest} did not answer with its sessions"));
        dash.status_dirty = true;
        dash.form_dirty = true;
        return;
    }
    // Drain rather than wait for exit: a machine with a few hundred sessions
    // fills the pipe, and a child blocked on a full pipe never exits.
    let mut buf = [0u8; 8192];
    let done = loop {
        let Some(out) = job.child.stdout.as_mut() else { break true };
        match out.read(&mut buf) {
            Ok(0) => break true,
            Ok(n) => job.buf.push_str(&String::from_utf8_lossy(&buf[..n])),
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break false,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break true,
        }
    };
    if !done {
        return;
    }
    let mut job = dash.newform.sess_job.take().expect("just had one");
    let _ = job.child.wait();
    dash.newform.sessions = Some(parse_sessions(&job.buf));
    dash.newform.sess_sel = 0;
    dash.form_dirty = true;
}

fn poll_spawn_job(dash: &mut Dash) {
    let Some(job) = dash.spawn_job.as_mut() else { return };
    if job.expired() {
        let dest = job.dest.clone();
        job.kill();
        dash.spawn_job = None;
        dash.flash = Some(format!("{dest} did not answer"));
        dash.status_dirty = true;
        return;
    }
    match job.child.try_wait() {
        Ok(Some(_)) => {}
        Ok(None) => return,
        Err(_) => {
            dash.spawn_job = None;
            return;
        }
    }
    // One line of stdout, or a short complaint on stderr; nothing that can
    // fill a pipe, so taking it in one go is safe here.
    let job = dash.spawn_job.take().expect("just had one");
    let dest = job.dest.clone();
    let Ok(out) = job.child.wait_with_output() else { return };
    if out.status.success() {
        // "warren: created agent 'NAME'" — the far side had the last word on
        // the name, and that is the one to focus when its row shows up.
        let said = String::from_utf8_lossy(&out.stdout);
        if let Some(name) = said.split('\'').nth(1) {
            focus_when_it_arrives(dash, Some(dest), name.to_string());
        }
    } else {
        let why = String::from_utf8_lossy(&out.stderr);
        let why = why.lines().last().unwrap_or("could not create it").trim();
        dash.flash = Some(format!("{dest}: {why}"));
        dash.status_dirty = true;
    }
}

/// Where the new-agent form's fields landed on screen, recorded as it is
/// drawn so a click can be turned back into a field. 0-based cells, like the
/// mouse reports them.
#[derive(Default)]
pub struct FormGeom {
    /// Row, the field drawn on it, and — for a row of chips — the column
    /// span of each option.
    pub fields: Vec<(u16, NField, Vec<(u16, u16)>)>,
    /// Row, and which session it offers.
    pub sessions: Vec<(u16, usize)>,
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
    // Filling the picker can mean asking another machine, which needs the
    // whole dashboard, not just the form — so it happens once the key is
    // handled and this borrow is done with.
    let mut want_sessions = false;
    match key {
        Key::Tab => form.field = (form.field + 1) % nfields,
        Key::ShiftTab => form.field = (form.field + nfields - 1) % nfields,
        // In the palette, Up/Down walk the grid; they only leave the field
        // at its edges (top row exits up, bottom row exits down).
        Key::Up if form.active() == NField::Color && form.color >= 16 => {
            form.color -= 16;
        }
        Key::Down if form.active() == NField::Color && form.color + 16 <= 255 => {
            form.color += 16;
        }
        Key::Up if form.active() != NField::List => {
            form.field = (form.field + nfields - 1) % nfields;
        }
        Key::Down if form.active() != NField::List => {
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
            NField::Machine => {
                let choices = form.machines.len() + 1;
                let step = match key {
                    Key::Char(b'l') | Key::Right | Key::Char(b' ') => 1,
                    Key::Char(b'h') | Key::Left => choices - 1,
                    _ => 0,
                };
                if step != 0 {
                    form.machine = (form.machine + step) % choices;
                    form.follow_machine();
                    form.forget_sessions(); // another machine, other sessions
                    want_sessions = form.mode == MODE_RESUME;
                    form.field = 0;
                }
            }
            NField::Kind => {
                let step = match key {
                    Key::Char(b'l') | Key::Right | Key::Char(b' ') => 1,
                    Key::Char(b'h') | Key::Left => KINDS.len() - 1,
                    _ => 0,
                };
                if step != 0 {
                    form.kind = (form.kind + step) % KINDS.len();
                    form.forget_sessions(); // a different harness, different sessions
                    want_sessions = form.mode == MODE_RESUME;
                    form.field = 0;
                }
            }
            NField::Mode => {
                if matches!(key, Key::Char(b'l') | Key::Right) {
                    form.mode = (form.mode + 1) % 3;
                    form.field = 0;
                } else if matches!(key, Key::Char(b'h') | Key::Left) {
                    form.mode = (form.mode + 2) % 3;
                    form.field = 0;
                }
                want_sessions = form.mode == MODE_RESUME;
            }
            NField::Title => line_edit(&mut form.title, key),
            NField::Root => line_edit(&mut form.root, key),
            NField::Sys => line_edit(&mut form.sys, key),
            NField::Extra => line_edit(&mut form.extra, key),
            NField::List => {
                want_sessions = true;
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
    if want_sessions {
        ensure_sessions(dash);
    }
    consumed
}

fn submit_new(dash: &mut Dash) {
    if dash.newform.mode == MODE_RESUME {
        ensure_sessions(dash);
    }
    let form = &mut dash.newform;
    let (mode_str, sid, dir, fallback_name) = match form.mode {
        MODE_RESUME => {
            let Some(sess) = form.sessions.as_ref().and_then(|s| s.get(form.sess_sel)) else {
                let waiting = form.sess_job.is_some();
                dash.flash =
                    Some(if waiting { "still asking…" } else { "no session selected" }.into());
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
    let dest = form.dest().map(str::to_string);
    // A path means whatever it means on the machine that will open it: only
    // this one's is ours to expand.
    let dir = match dest {
        None => crate::cli::expand_dir(Some(&dir)),
        Some(_) => dir,
    };
    let color = form.color.min(255) as u8;
    let kind = form.agent_kind();
    let sys = form.sys.trim().to_string();
    let extra = form.extra.trim().to_string();
    let spec = crate::cli::NewAgent {
        base: &base,
        dir: &dir,
        color,
        kind,
        mode: mode_str,
        sid: sid.as_deref(),
        sys: (!sys.is_empty()).then_some(sys.as_str()),
        extra: (!extra.is_empty()).then_some(extra.as_str()),
    };

    let outcome = match &dest {
        // Here: spawn the daemon ourselves, against the names and slots this
        // machine is already using.
        None => {
            let live: Vec<(String, u8)> = dash
                .agents
                .iter()
                .filter(|a| a.ident().0.is_none())
                .map(|a| (a.meta.name.clone(), a.meta.slot))
                .collect();
            crate::cli::launch_agent(&spec, &live).map(|name| {
                focus_when_it_arrives(dash, None, name.clone());
                format!("creating agent '{name}'…")
            })
        }
        // There: `warren new` over that machine's ssh connection, which picks
        // the name and the slot against its own agents. The row arrives in
        // its next roster like any other.
        Some(dest) => {
            let started = match dash.hosts.get(dest) {
                None => Err(anyhow::anyhow!("{dest} is not in the hosts file")),
                Some(host) if !host.reachable() => {
                    Err(anyhow::anyhow!("{} is not answering", host.label))
                }
                Some(host) => host.spawn_agent(&spec).map(|child| (child, host.label.clone())),
            };
            started.map(|(child, label)| {
                dash.spawn_job = Some(Job::new(child, dest.clone()));
                format!("creating agent '{base}' on {label}…")
            })
        }
    };

    match outcome {
        Ok(said) => {
            dash.flash = Some(said);
            // Keep the machine: a colony over there is usually built more
            // than one agent at a time. Anything the old form still had in
            // flight goes first, so no ssh is dropped without being reaped.
            dash.newform.forget_sessions();
            let (machine, machines) = (dash.newform.machine, dash.newform.machines.clone());
            dash.newform = NewForm::reset();
            dash.newform.machine = machine;
            dash.newform.machines = machines;
            dash.newform.follow_machine();
        }
        Err(e) => dash.flash = Some(format!("create failed: {e}")),
    }
    dash.status_dirty = true;
}

pub fn draw_new_form(dash: &mut Dash, out: &mut String) {
    let x0 = SIDEBAR_WIDTH + 1;
    let pane_w = dash.cols.saturating_sub(SIDEBAR_WIDTH) as usize;
    let pane_h = dash.rows.saturating_sub(1);
    if dash.newform.mode == MODE_RESUME {
        ensure_sessions(dash);
    }
    let form = &mut dash.newform;
    let active = form.fields()[form.field.min(form.fields().len() - 1)];
    let insert = dash.mode == Mode::Insert;

    for row in 0..pane_h {
        let _ = write!(out, "\x1b[{};{}H\x1b[0m\x1b[K", row + 1, x0);
    }
    let where_ = match form.dest() {
        Some(dest) => format!(" on {}", dest.rsplit('@').next().unwrap_or(dest)),
        None => String::new(),
    };
    let _ = write!(
        out,
        "\x1b[2;{}H\x1b[1m+ new {} agent{where_}\x1b[0m",
        x0 + 2,
        form.agent_kind().as_str()
    );

    // Where it runs decides what everything under it means, so it leads.
    let mut geom = FormGeom::default();
    let mut row = 4u16;
    if !form.machines.is_empty() {
        let machines = form.machine_labels();
        let chips =
            draw_choice(out, row, x0, "Machine", &machines, form.machine, active == NField::Machine);
        geom.fields.push((row - 1, NField::Machine, chips));
        row += 2;
    }
    // Which harness, then how to start it.
    let kinds: Vec<&str> = KINDS.iter().map(|k| k.as_str()).collect();
    let chips = draw_choice(out, row, x0, "Agent", &kinds, form.kind, active == NField::Kind);
    geom.fields.push((row - 1, NField::Kind, chips));
    row += 2;
    let chips = draw_choice(
        out,
        row,
        x0,
        "Mode",
        &["new", "resume", "continue"],
        form.mode as usize,
        active == NField::Mode,
    );
    geom.fields.push((row - 1, NField::Mode, chips));
    row += 2;

    match form.mode {
        MODE_RESUME => {
            let sessions = form.sessions.as_deref().unwrap_or(&[]);
            let _ = write!(
                out,
                "\x1b[{};{}H{}Session   \x1b[0m",
                row,
                x0 + 2,
                field_label(active == NField::List)
            );
            row += 1;
            // What still has to fit below: the "… N more" line, then Sys,
            // Extra and Color at two rows each.
            const BELOW_LIST: usize = 1 + 2 + 2 + 2;
            let visible =
                (pane_h as usize).saturating_sub(row as usize + BELOW_LIST).clamp(1, 12);
            let first = form.sess_sel.saturating_sub(visible - 1);
            for (i, sess) in sessions.iter().enumerate().skip(first).take(visible) {
                let marker = if i == form.sess_sel { ">" } else { " " };
                let style = if i == form.sess_sel { "\x1b[1m" } else { "\x1b[2m" };
                let mut label = format!("{marker} {} \u{b7} {}", short_path(&sess.cwd), sess.title);
                label.truncate(pane_w.saturating_sub(4));
                let _ = write!(out, "\x1b[{};{}H{style}{label}\x1b[0m", row, x0 + 4);
                geom.sessions.push((row - 1, i));
                row += 1;
            }
            let more = sessions.len().saturating_sub(first + visible);
            if more > 0 {
                let _ = write!(out, "\x1b[{};{}H\x1b[2m… {more} more\x1b[0m", row, x0 + 4);
            }
            if sessions.is_empty() {
                let waiting = match form.sess_job.as_ref() {
                    Some(job) => format!("(asking {}…)", job.dest),
                    None => "(no sessions found)".to_string(),
                };
                let _ = write!(out, "\x1b[{};{}H\x1b[2m{waiting}\x1b[0m", row, x0 + 4);
            }
            row += 2;
        }
        MODE_CONTINUE => {
            geom.fields.push((row - 1, NField::Root, Vec::new()));
            row = draw_text_field(out, row, x0, pane_h, "Root dir", &form.root, active == NField::Root, insert);
        }
        _ => {
            geom.fields.push((row - 1, NField::Title, Vec::new()));
            row = draw_text_field(out, row, x0, pane_h, "Title", &form.title, active == NField::Title, insert);
            geom.fields.push((row - 1, NField::Root, Vec::new()));
            row = draw_text_field(out, row, x0, pane_h, "Root dir", &form.root, active == NField::Root, insert);
        }
    }
    geom.fields.push((row - 1, NField::Sys, Vec::new()));
    row = draw_text_field(out, row, x0, pane_h, "Sys prompt", &form.sys, active == NField::Sys, insert);
    geom.fields.push((row - 1, NField::Extra, Vec::new()));
    row = draw_text_field(out, row, x0, pane_h, "Extra args", &form.extra, active == NField::Extra, insert);
    geom.fields.push((row - 1, NField::Color, Vec::new()));

    dash.palette_geom =
        draw_color_field(out, row, x0, pane_w, pane_h, form.color, active == NField::Color);
    dash.form_geom = geom;
}

// ----------------------------------------------------------------- edit form

/// One row of `[ option ]` chips, the form's h/l selector. Returns where
/// each chip landed, 0-based and inclusive, so it can also be clicked.
fn draw_choice(
    out: &mut String,
    row: u16,
    x0: u16,
    label: &str,
    options: &[&str],
    selected: usize,
    active: bool,
) -> Vec<(u16, u16)> {
    let _ = write!(out, "\x1b[{row};{}H{}{label:<10}\x1b[0m  ", x0 + 2, field_label(active));
    // The label is padded to ten, then two spaces: chips start after that.
    let mut at = x0 + 2 + 10 + 2;
    let mut spans = Vec::with_capacity(options.len());
    for (i, option) in options.iter().enumerate() {
        let style = if i == selected { "\x1b[7m" } else { "\x1b[2m" };
        let _ = write!(out, "{style}[ {option} ]\x1b[0m ");
        let width = option.chars().count() as u16 + 4; // "[ " + option + " ]"
        spans.push((at - 1, at + width - 2)); // 1-based draw -> 0-based cells
        at += width + 1;
    }
    spans
}

pub fn edit_key(dash: &mut Dash, bytes: &[u8]) -> usize {
    dash.form_dirty = true;
    let (key, consumed) = decode_key(bytes);
    let Some(form) = dash.editform.as_mut() else { return consumed };
    match key {
        Key::Up if form.field == 1 && form.color >= 16 => form.color -= 16,
        Key::Down if form.field == 1 && form.color + 16 <= 255 => form.color += 16,
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
    let pane_w = dash.cols.saturating_sub(SIDEBAR_WIDTH) as usize;
    let pane_h = dash.rows.saturating_sub(1);
    let Some(form) = dash.editform.as_ref() else { return };

    for row in 0..pane_h {
        let _ = write!(out, "\x1b[{};{}H\x1b[0m\x1b[K", row + 1, x0);
    }
    let _ = write!(out, "\x1b[2;{}H\x1b[1medit agent\x1b[0m", x0 + 2);
    let row = draw_text_field(out, 4, x0, pane_h, "Title", &form.title, form.field == 0, true);
    dash.palette_geom =
        draw_color_field(out, row, x0, pane_w, pane_h, form.color, form.field == 1);
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
    pane_h: u16,
    label: &str,
    value: &str,
    active: bool,
    insert: bool,
) -> u16 {
    // The status bar owns the row below the pane: a form too tall for the
    // window loses its last fields rather than painting over it.
    if row > pane_h {
        return row + 2;
    }
    let cursor = if active && insert { "\u{2588}" } else { "" };
    let _ = write!(
        out,
        "\x1b[{};{}H{}{:<10}\x1b[0m  {value}{cursor}",
        row,
        x0 + 2,
        field_label(active),
        label
    );
    row + 2
}

/// A click anywhere on a form: the field it landed on, the chip under the
/// pointer if that field offers any, or a session in the resume picker —
/// and the colour palette, which was always clickable. Mouse and keyboard
/// reach the same places, which is the rule the sidebar already follows.
pub fn form_click(dash: &mut Dash, row: u16, col: u16) {
    if dash.editform.is_some() || !dash.on_newform() {
        palette_click(dash, row, col);
        return;
    }
    if let Some(&(_, idx)) = dash.form_geom.sessions.iter().find(|(r, _)| *r == row) {
        dash.newform.sess_sel = idx;
        focus_field(dash, NField::List);
        dash.form_dirty = true;
        dash.enter_insert();
        return;
    }
    let hit = dash
        .form_geom
        .fields
        .iter()
        .find(|(r, ..)| *r == row)
        .map(|(_, field, chips)| (*field, chips.iter().position(|(a, b)| col >= *a && col <= *b)));
    let Some((field, chip)) = hit else {
        palette_click(dash, row, col);
        return;
    };
    // The chip first: choosing a mode changes which fields there are, and
    // the field to land on has to be found in the list that results.
    if let Some(option) = chip {
        choose(dash, field, option);
    }
    focus_field(dash, field);
    dash.form_dirty = true;
    dash.enter_insert();
}

/// Wheel over the form: the resume picker is the one list here to scroll.
pub fn form_wheel(dash: &mut Dash, down: bool) {
    if !dash.on_newform() || dash.newform.mode != MODE_RESUME {
        return;
    }
    let len = dash.newform.sessions.as_ref().map(Vec::len).unwrap_or(0);
    if len == 0 {
        return;
    }
    let sel = dash.newform.sess_sel;
    dash.newform.sess_sel = if down { (sel + 1).min(len - 1) } else { sel.saturating_sub(1) };
    focus_field(dash, NField::List);
    dash.form_dirty = true;
}

fn focus_field(dash: &mut Dash, field: NField) {
    if let Some(pos) = dash.newform.fields().iter().position(|f| *f == field) {
        dash.newform.field = pos;
    }
}

/// Pick one option of a chip row, exactly as `h`/`l` would.
fn choose(dash: &mut Dash, field: NField, option: usize) {
    let form = &mut dash.newform;
    match field {
        NField::Machine if option <= form.machines.len() && option != form.machine => {
            form.machine = option;
            form.follow_machine();
            form.forget_sessions();
        }
        NField::Kind if option < KINDS.len() && option != form.kind => {
            form.kind = option;
            form.forget_sessions();
        }
        NField::Mode if option < 3 => form.mode = option as u8,
        _ => return,
    }
    if dash.newform.mode == MODE_RESUME {
        ensure_sessions(dash);
    }
}

/// Click on a palette swatch (0-based screen cell). Routes to whichever form
/// is showing; the swatch grid geometry is recorded at draw time.
pub fn palette_click(dash: &mut Dash, row: u16, col: u16) {
    let Some((row0, col0, bw, bh)) = dash.palette_geom else { return };
    if row < row0 || col < col0 {
        return;
    }
    let (gr, gc) = ((row - row0) / bh, (col - col0) / bw);
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

/// Draw the color field; when active, fill the remaining pane with the 16x16
/// swatch grid, scaling each box to the available space (the v0 big picker).
/// Returns the grid geometry as 0-based (row, col, box_w, box_h).
fn draw_color_field(
    out: &mut String,
    row: u16,
    x0: u16,
    pane_w: usize,
    pane_h: u16,
    color: u16,
    active: bool,
) -> Option<(u16, u16, u16, u16)> {
    if row > pane_h {
        return None;
    }
    let _ = write!(out, "\x1b[{};{}H{}Color     \x1b[0m  ", row, x0 + 2, field_label(active));
    if color == 0 {
        let _ = write!(out, "\x1b[2mnone\x1b[0m");
    } else {
        let (r, g, b) = spans::xterm256_to_rgb(color as u8);
        let _ = write!(out, "\x1b[48;5;{color}m   \x1b[0m  #{r:02x}{g:02x}{b:02x} \u{b7} {color}");
    }
    if !active {
        return None;
    }

    let grid_top = row + 2;
    let avail_rows = pane_h.saturating_sub(grid_top).saturating_sub(1); // 1 for the hint
    let box_h = (avail_rows / 16).clamp(1, 3);
    let box_w = ((pane_w.saturating_sub(6)) as u16 / 16).clamp(2, 7);

    for gr in 0..16u16 {
        for sub in 0..box_h {
            let _ = write!(out, "\x1b[{};{}H", grid_top + gr * box_h + sub, x0 + 4);
            for gc in 0..16u16 {
                let idx = gr * 16 + gc;
                let here = idx == color;
                // Cursor brackets span the swatch's full height.
                let body: String = if here && box_w >= 2 {
                    format!("[{}]", " ".repeat(box_w as usize - 2))
                } else {
                    " ".repeat(box_w as usize)
                };
                if idx == 0 {
                    let dashes = if here {
                        body
                    } else {
                        "\u{2500}".repeat(box_w as usize)
                    };
                    let _ = write!(out, "\x1b[0;2m{dashes}\x1b[0m");
                } else if here {
                    let (r, g, b) = spans::xterm256_to_rgb(idx as u8);
                    let fg = if spans::color_is_dark(r, g, b) { 15 } else { 0 };
                    let _ = write!(out, "\x1b[48;5;{idx};38;5;{fg};1m{body}\x1b[0m");
                } else {
                    let _ = write!(out, "\x1b[48;5;{idx}m{body}\x1b[0m");
                }
            }
        }
    }
    let _ = write!(
        out,
        "\x1b[{};{}H\x1b[2mhjkl/arrows/click pick \u{b7} 0 none\x1b[0m",
        grid_top + 16 * box_h,
        x0 + 4
    );
    Some((grid_top - 1, x0 + 4 - 1, box_w, box_h)) // 1-based draw -> 0-based cells
}

fn short_path(path: &str) -> String {
    let home = std::env::var("HOME").unwrap_or_default();
    if !home.is_empty() && path.starts_with(&home) {
        format!("~{}", &path[home.len()..])
    } else {
        path.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sessions_come_back_as_the_far_side_printed_them() {
        let out = "abc123\t1700000000\t/w/one\ta title\n\
                   def456\t1700000001\t/w/two\t\n\
                   \n\
                   junk-without-columns\n";
        let got = parse_sessions(out);
        assert_eq!(got.len(), 2, "{got:?}");
        assert_eq!(got[0].id, "abc123");
        assert_eq!(got[0].cwd, "/w/one");
        assert_eq!(got[0].title, "a title");
        // A session no one has titled is still a session.
        assert_eq!(got[1].title, "");
        assert_eq!(got[1].mtime as i64, 1_700_000_001);
    }

    #[test]
    fn the_chosen_machine_follows_its_name_through_a_hosts_file_edit() {
        let mut form = NewForm::reset();
        form.sync_machines(&["smq".into(), "servo".into()]);
        assert_eq!(form.fields()[0], NField::Machine, "the field appears with hosts to offer");
        form.machine = 2;
        assert_eq!(form.dest(), Some("servo"));

        // A machine added above it must not retarget the form.
        form.sync_machines(&["mini".into(), "smq".into(), "servo".into()]);
        assert_eq!(form.dest(), Some("servo"));

        // And one that leaves the file falls back to here, never to whoever
        // inherited its position.
        form.sync_machines(&["mini".into(), "smq".into()]);
        assert_eq!(form.dest(), None);
        assert_eq!(form.machine, 0);

        // With no hosts at all the form is what it always was.
        form.sync_machines(&[]);
        assert_eq!(form.fields()[0], NField::Kind);
    }

    #[test]
    fn an_untouched_root_dir_follows_the_machine() {
        let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
        let mut form = NewForm::reset();
        form.sync_machines(&["servo".into()]);
        assert_eq!(form.root, home);

        // Nobody here knows another machine's home, so `~` goes over as `~`.
        form.machine = 1;
        form.follow_machine();
        assert_eq!(form.root, "~");

        form.machine = 0;
        form.follow_machine();
        assert_eq!(form.root, home);

        // A path someone typed is theirs, and stays put.
        form.root = "/w/somewhere".into();
        form.machine = 1;
        form.follow_machine();
        assert_eq!(form.root, "/w/somewhere");
    }
}
