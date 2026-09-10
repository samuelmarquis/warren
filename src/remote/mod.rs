//! Agents on other machines, shown in the same sidebar as the ones here.
//!
//! The dashboard is a viewer over unix sockets, and a socketpair is a unix
//! socket — so a remote agent is the same `AgentConn` as a local one, with an
//! `ssh` in the middle carrying its bytes to a `warren __pipe` over there.
//! Nothing about the daemon protocol, the poll loop, or the daemons knows the
//! difference.
//!
//! Per host that costs one long-lived `ssh … warren __roster` (which says
//! which agents exist) plus one `ssh … warren __pipe <name>` per agent, all
//! sharing a single ssh connection through ControlMaster. A host that stops
//! answering keeps its rows in the sidebar, dimmed, until it comes back.

pub mod wire;

use std::collections::HashMap;
use std::io::Read;
use std::os::fd::{AsFd, OwnedFd};
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::Result;

/// How long to wait before dialling a host that just failed, and the ceiling
/// that backoff climbs to.
const RETRY_MIN: Duration = Duration::from_secs(2);
const RETRY_MAX: Duration = Duration::from_secs(10);

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HostState {
    /// Dialling, or waiting to redial.
    Connecting,
    /// The roster is current.
    Live,
    /// Last thing that went wrong, shown on the host's row.
    Offline(String),
}

/// An agent warren has seen on a host it can no longer reach: enough to keep
/// the row on screen, greyed, without pretending it is still readable.
#[derive(Debug, Clone)]
pub struct Ghost {
    pub name: String,
    pub display: String,
    pub cwd: String,
    pub slot: u8,
    pub created: u64,
}

pub struct Host {
    /// ssh destination, as written in the hosts file.
    pub dest: String,
    /// What the sidebar calls it: the destination without any `user@`.
    pub label: String,
    /// warren on the far side; hosts file column two, else plain `warren`.
    pub remote_bin: String,
    pub state: HostState,
    /// Agents the far side last reported.
    pub roster: Vec<String>,
    /// Rows to keep while the host is unreachable.
    pub ghosts: Vec<Ghost>,
    /// What the far side says it runs, once it has said so.
    pub version: Option<String>,
    /// That machine's home directory, which its roster reports so paths from
    /// it can be read the way it reads them. A warren too old to say leaves
    /// this None and the sidebar guesses from the paths themselves.
    pub home: Option<String>,

    watcher: Option<Child>,
    /// Poll key of the watcher's stdout, while one is registered.
    pub key: Option<usize>,
    /// Partial line buffer and the roster being accumulated.
    buf: String,
    pending: Vec<String>,
    retry_at: Option<Instant>,
    backoff: Duration,
}

impl Host {
    fn new(dest: String, remote_bin: String) -> Host {
        let label = dest.rsplit('@').next().unwrap_or(&dest).to_string();
        Host {
            dest,
            label,
            remote_bin,
            state: HostState::Connecting,
            roster: Vec::new(),
            ghosts: Vec::new(),
            version: None,
            home: None,
            watcher: None,
            key: None,
            buf: String::new(),
            pending: Vec::new(),
            retry_at: None,
            backoff: RETRY_MIN,
        }
    }

    pub fn reachable(&self) -> bool {
        self.state == HostState::Live
    }

    /// The ssh invocation for one remote command.
    ///
    /// BatchMode is not optional: ssh's stdin is the agent's protocol stream,
    /// so a password prompt would read protocol bytes as a passphrase. Keys
    /// or an agent, or the host stays offline and says why.
    fn ssh(&self, args: &[&str]) -> Command {
        // A test (or a user with their own wrapper) can substitute the
        // transport; then warren passes only destination and command, so the
        // stand-in has nothing to parse.
        if let Ok(custom) = std::env::var("WARREN_SSH") {
            if !custom.is_empty() {
                let mut parts = custom.split_whitespace();
                let mut cmd = Command::new(parts.next().unwrap_or("ssh"));
                // The stand-in execs the command itself, so its arguments
                // arrive as written — quoting them would be quoting twice.
                cmd.args(parts).arg(&self.dest).arg(&self.remote_bin).args(args);
                return cmd;
            }
        }
        // Real ssh joins its command with spaces and the far side's shell
        // splits it again, so anything that could hold one — a title, a
        // system prompt — has to survive the round trip quoted. The binary
        // itself is left alone: a hosts file may well write it with a ~.
        let quoted: Vec<String> = args.iter().map(|a| shell_quote(a)).collect();
        let mut cmd = Command::new("ssh");
        cmd.arg("-T")
            .args(["-o", "BatchMode=yes"])
            .args(["-o", "ControlMaster=auto"])
            .arg("-o")
            .arg(format!("ControlPath={}", control_path().display()))
            .args(["-o", "ControlPersist=30s"])
            .args(["-o", "ServerAliveInterval=15"])
            .args(["-o", "ServerAliveCountMax=3"])
            .arg(&self.dest)
            .arg(&self.remote_bin)
            .args(&quoted);
        cmd
    }

    /// `ssh … warren new …` on that machine: the far side picks the name and
    /// the slot against its own agents, exactly as it would for someone
    /// typing the command over there. Nothing is waited on here — the child
    /// is reaped later, and the agent arrives the way every other one does,
    /// in that machine's next roster.
    pub fn spawn_agent(&self, spec: &crate::cli::NewAgent) -> Result<Child> {
        let mut args: Vec<String> = vec![
            "new".into(),
            spec.base.to_string(),
            spec.dir.to_string(),
            spec.color.to_string(),
            spec.mode.to_string(),
        ];
        // Positional, and the far side reads them in this order; the flags
        // may follow in any.
        if let Some(sid) = spec.sid {
            args.push(sid.to_string());
        }
        args.push(format!("--kind={}", spec.kind.as_str()));
        if let Some(sys) = spec.sys {
            args.push(format!("--sys={sys}"));
        }
        if let Some(extra) = spec.extra {
            args.push(format!("--extra={extra}"));
        }
        let refs: Vec<&str> = args.iter().map(String::as_str).collect();
        let child = self
            .ssh(&refs)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        Ok(child)
    }

    /// `ssh … warren sessions KIND` — the resume picker, one machine away.
    /// The output is the same tab-separated list the command has always
    /// printed, so this asks nothing of the far side that it could not
    /// already do.
    pub fn sessions(&self, kind: crate::kind::Kind) -> Result<Child> {
        let child = self
            .ssh(&["sessions", kind.as_str()])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()?;
        if let Some(out) = child.stdout.as_ref() {
            set_nonblocking(out)?;
        }
        Ok(child)
    }

    /// Start `warren __pipe NAME` over there, with a socketpair for stdio:
    /// our end is an ordinary `UnixStream`, which is all a viewer ever needed.
    pub fn open_agent(&self, name: &str) -> Result<(std::os::unix::net::UnixStream, Child)> {
        let (near, far) = std::os::unix::net::UnixStream::pair()?;
        // One socketpair end serves as both stdin and stdout over there: a
        // unix socket is bidirectional, which is exactly the shape stdio of a
        // piped command wants.
        let (read_end, write_end) = (OwnedFd::from(far.try_clone()?), OwnedFd::from(far));
        let child = self
            .ssh(&["__pipe", name])
            .stdin(Stdio::from(read_end))
            .stdout(Stdio::from(write_end))
            .stderr(Stdio::null())
            .spawn()?;
        near.set_nonblocking(true)?;
        Ok((near, child))
    }

    /// Let go of the watcher — deregistering *before* the descriptor closes,
    /// because the next ssh will very likely be handed the same number and
    /// the poller would refuse to watch it twice.
    fn stop_watcher(&mut self, poller: Option<&polling::Poller>) {
        if let Some(mut child) = self.watcher.take() {
            if let (Some(poller), Some(out)) = (poller, child.stdout.as_ref()) {
                let _ = poller.delete(out);
            }
            let _ = child.kill();
            let _ = child.wait();
        }
        self.key = None;
        self.buf.clear();
        self.pending.clear();
    }

    fn fail(&mut self, poller: Option<&polling::Poller>, why: String) {
        self.stop_watcher(poller);
        self.state = HostState::Offline(why);
        self.retry_at = Some(Instant::now() + self.backoff);
        self.backoff = (self.backoff * 2).min(RETRY_MAX);
    }
}

/// The machines this dashboard watches, in the order the file lists them.
#[derive(Default)]
pub struct Hosts {
    pub hosts: Vec<Host>,
    /// Modification time of the hosts file when it was last read.
    stamp: Option<std::time::SystemTime>,
}

impl Hosts {
    /// Read `~/.warren/hosts` if it changed: one ssh destination per line,
    /// `#` comments, and an optional second column giving warren's path on
    /// that machine (an ssh command runs without your login shell's PATH, so
    /// `~/.local/bin` is usually not on it). Returns true if the list moved.
    pub fn reload(&mut self, poller: &polling::Poller) -> bool {
        let path = hosts_file();
        let stamp = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        if stamp == self.stamp && !self.hosts.is_empty() {
            return false;
        }
        let Ok(body) = std::fs::read_to_string(&path) else {
            self.stamp = stamp;
            let had = !self.hosts.is_empty();
            for host in &mut self.hosts {
                host.stop_watcher(Some(poller));
            }
            self.hosts.clear();
            return had;
        };
        self.stamp = stamp;

        let mut wanted: Vec<(String, String)> = Vec::new();
        for line in body.lines() {
            let line = line.split('#').next().unwrap_or("").trim();
            if line.is_empty() {
                continue;
            }
            let mut cols = line.split_whitespace();
            let Some(dest) = cols.next() else { continue };
            let bin = cols.next().unwrap_or("warren").to_string();
            wanted.push((dest.to_string(), bin));
        }
        if wanted.iter().map(|(d, _)| d.as_str()).eq(self.hosts.iter().map(|h| h.dest.as_str())) {
            return false;
        }
        // Keep the hosts that stayed, in the file's order, so a reload does
        // not drop connections that are working.
        let mut kept: HashMap<String, Host> =
            self.hosts.drain(..).map(|h| (h.dest.clone(), h)).collect();
        for (dest, bin) in wanted {
            match kept.remove(&dest) {
                Some(mut host) => {
                    host.remote_bin = bin;
                    self.hosts.push(host);
                }
                None => self.hosts.push(Host::new(dest, bin)),
            }
        }
        for (_, mut gone) in kept {
            gone.stop_watcher(Some(poller));
        }
        true
    }

    /// Dial anything that is not connected and whose backoff has expired.
    /// Returns the poll keys newly registered.
    pub fn dial(&mut self, poller: &polling::Poller, next_key: &mut usize) -> Vec<usize> {
        let mut added = Vec::new();
        for host in &mut self.hosts {
            if host.watcher.is_some() {
                continue;
            }
            if let Some(at) = host.retry_at {
                if Instant::now() < at {
                    continue;
                }
            }
            host.retry_at = None;
            let spawned = host
                .ssh(&["__roster"])
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn();
            match spawned {
                Ok(mut child) => {
                    let Some(out) = child.stdout.take() else {
                        let _ = child.kill();
                        host.fail(Some(poller), "could not read from ssh".into());
                        continue;
                    };
                    if set_nonblocking(&out).is_err() {
                        let _ = child.kill();
                        host.fail(Some(poller), "could not set up the ssh pipe".into());
                        continue;
                    }
                    let key = *next_key;
                    *next_key += 1;
                    let registered = unsafe {
                        poller.add_with_mode(
                            &out,
                            polling::Event::readable(key),
                            polling::PollMode::Level,
                        )
                    };
                    if registered.is_err() {
                        let _ = child.kill();
                        host.fail(Some(poller), "could not watch the ssh pipe".into());
                        continue;
                    }
                    child.stdout = Some(out);
                    host.watcher = Some(child);
                    host.key = Some(key);
                    host.state = HostState::Connecting;
                    added.push(key);
                }
                Err(e) => host.fail(Some(poller), format!("{e}")),
            }
        }
        added
    }

    /// Feed a readable watcher; true if this host's roster changed.
    pub fn read_watcher(&mut self, key: usize, poller: &polling::Poller) -> bool {
        let Some(host) = self.hosts.iter_mut().find(|h| h.key == Some(key)) else {
            return false;
        };
        let mut buf = [0u8; 8192];
        let mut closed = false;
        loop {
            let Some(child) = host.watcher.as_mut() else { break };
            let Some(out) = child.stdout.as_mut() else { break };
            match out.read(&mut buf) {
                Ok(0) => {
                    closed = true;
                    break;
                }
                Ok(n) => host.buf.push_str(&String::from_utf8_lossy(&buf[..n])),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Err(_) => {
                    closed = true;
                    break;
                }
            }
        }

        let mut changed = false;
        while let Some(nl) = host.buf.find('\n') {
            let line: String = host.buf.drain(..=nl).collect();
            let line = line.trim_end().to_string();
            if let Some(rest) = line.strip_prefix("warren ") {
                // "<version> <wire>", and since this warren, " <home>".
                let mut fields = rest.splitn(3, ' ');
                let vers: String =
                    [fields.next().unwrap_or(""), fields.next().unwrap_or("")].join(" ");
                host.version = Some(vers.trim().to_string());
                host.home = fields.next().map(str::to_string).filter(|h| !h.is_empty());
                // Answering at all is enough to stop saying "connecting…",
                // but only when there is nothing on screen to contradict:
                // with rows already known, the block that follows owns them,
                // and promoting early would show a stale roster beside its
                // own ghosts for a tick. An older warren over there never
                // reports an empty roster, so this is also what keeps a
                // machine with no agents from looking unreachable forever.
                if host.state != HostState::Live && host.roster.is_empty() && host.ghosts.is_empty()
                {
                    host.state = HostState::Live;
                    host.backoff = RETRY_MIN;
                    changed = true;
                }
                continue;
            }
            if line.is_empty() {
                // Blank line ends a roster block.
                let fresh = std::mem::take(&mut host.pending);
                if fresh != host.roster || host.state != HostState::Live {
                    host.roster = fresh;
                    changed = true;
                }
                host.state = HostState::Live;
                host.backoff = RETRY_MIN;
                host.ghosts.clear();
                continue;
            }
            host.pending.push(line);
        }

        if closed {
            // ssh gave up, or the far side did.
            let why = match host.watcher.as_mut().and_then(|c| c.wait().ok()) {
                Some(status) if status.code() == Some(127) => {
                    format!("no '{}' on that machine", host.remote_bin)
                }
                Some(status) => format!("ssh exited {}", status.code().unwrap_or(-1)),
                None => "ssh ended".to_string(),
            };
            host.fail(Some(poller), why);
            changed = true;
        }
        changed
    }

    /// Remember what an agent looked like, for as long as its machine is not
    /// answering for it. Merged by name, and kept in sidebar order so the
    /// dimmed rows sit where the live ones did.
    pub fn park(&mut self, dest: &str, ghosts: Vec<Ghost>) {
        let Some(host) = self.hosts.iter_mut().find(|h| h.dest == dest) else { return };
        for ghost in ghosts {
            match host.ghosts.iter_mut().find(|g| g.name == ghost.name) {
                Some(slot) => *slot = ghost,
                None => host.ghosts.push(ghost),
            }
        }
        // Folders first appearance wins, exactly as for live agents.
        let mut rank: HashMap<String, (u8, u64)> = HashMap::new();
        for g in &host.ghosts {
            let key = (g.slot, g.created);
            rank.entry(g.cwd.clone()).and_modify(|r| *r = (*r).min(key)).or_insert(key);
        }
        host.ghosts.sort_by(|a, b| {
            let ra = rank.get(&a.cwd).copied().unwrap_or((u8::MAX, u64::MAX));
            let rb = rank.get(&b.cwd).copied().unwrap_or((u8::MAX, u64::MAX));
            (ra, &a.cwd, a.slot, a.created).cmp(&(rb, &b.cwd, b.slot, b.created))
        });
    }

    /// Try this machine again on the next pass, whatever its backoff said.
    pub fn retry_now(&mut self, dest: &str) {
        if let Some(host) = self.hosts.iter_mut().find(|h| h.dest == dest) {
            host.retry_at = None;
            host.backoff = RETRY_MIN;
        }
    }

    pub fn get(&self, dest: &str) -> Option<&Host> {
        self.hosts.iter().find(|h| h.dest == dest)
    }
}

impl Drop for Hosts {
    fn drop(&mut self) {
        for host in &mut self.hosts {
            host.stop_watcher(None); // the process is going away with them
        }
    }
}

pub fn hosts_file() -> PathBuf {
    crate::paths::home().join("hosts")
}

/// Where ssh keeps its shared connection sockets. Short on purpose: these are
/// unix sockets too, and `sun_path` is 104 bytes on macOS.
fn control_path() -> PathBuf {
    let dir = crate::paths::home().join("ssh");
    let _ = std::fs::create_dir_all(&dir);
    dir.join("%C")
}

/// One argument, safe for a shell that will split on whitespace. Single
/// quotes take everything literally, and the only thing they cannot hold is
/// a single quote — which leaves and comes back escaped.
fn shell_quote(arg: &str) -> String {
    format!("'{}'", arg.replace('\'', r"'\''"))
}

pub(crate) fn set_nonblocking(fd: &impl AsFd) -> Result<()> {
    let flags = rustix::fs::fcntl_getfl(fd.as_fd())?;
    rustix::fs::fcntl_setfl(fd.as_fd(), flags | rustix::fs::OFlags::NONBLOCK)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_argument_survives_the_far_side_s_shell() {
        // ssh hands the far side a string, not an argv: a title with a space
        // in it has to come back as one argument.
        assert_eq!(shell_quote("__pipe"), "'__pipe'");
        assert_eq!(shell_quote("--sys=be terse"), "'--sys=be terse'");
        assert_eq!(shell_quote("~/Developer/warren"), "'~/Developer/warren'");
        // The one character single quotes cannot hold, leaving and coming
        // back: don't  ->  'don'\''t'
        assert_eq!(shell_quote("don't"), r"'don'\''t'");
    }

    #[test]
    fn hosts_file_columns_and_comments() {
        let dir = std::env::temp_dir().join(format!("warren-hosts-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        // SAFETY: single-threaded test; nothing else reads WARREN_HOME here.
        unsafe { std::env::set_var("WARREN_HOME", &dir) };
        std::fs::write(
            dir.join("hosts"),
            "# machines\nsmq\nservo@mini.local  /opt/warren   # custom path\n\n",
        )
        .unwrap();

        let poller = polling::Poller::new().unwrap();
        let mut hosts = Hosts::default();
        hosts.reload(&poller);
        assert_eq!(hosts.hosts.len(), 2);
        assert_eq!(hosts.hosts[0].dest, "smq");
        assert_eq!(hosts.hosts[0].remote_bin, "warren");
        assert_eq!(hosts.hosts[1].dest, "servo@mini.local");
        assert_eq!(hosts.hosts[1].remote_bin, "/opt/warren");
        // The sidebar drops the user@, which is noise once it is a header.
        assert_eq!(hosts.hosts[1].label, "mini.local");
        assert!(hosts.hosts.iter().all(|h| !h.reachable()));

        unsafe { std::env::remove_var("WARREN_HOME") };
        let _ = std::fs::remove_dir_all(&dir);
    }
}
