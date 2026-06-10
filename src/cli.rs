//! Short-lived CLI roles: new / ls / kill / attach.
//! Each is a small unix-socket client of one or more agent daemons.

use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};

use crate::proto::{self, AgentState, FrameDecoder, Meta, ToClient, ToDaemon};

// ------------------------------------------------------------------ discovery

pub struct Agent {
    pub meta: Meta,
    pub state: AgentState,
}

/// Query every socket in the run dir; unlink stale ones (nothing listening).
pub fn discover() -> Vec<Agent> {
    let mut agents = Vec::new();
    let Ok(entries) = std::fs::read_dir(crate::paths::run_dir()) else {
        return agents;
    };
    for entry in entries.flatten() {
        let sock = entry.path();
        if sock.extension().and_then(|e| e.to_str()) != Some("sock") {
            continue;
        }
        match query_agent(&sock) {
            Some((meta, state)) => agents.push(Agent { meta, state }),
            None => {
                // Stale socket from a crashed daemon — clean it up.
                let _ = std::fs::remove_file(&sock);
            }
        }
    }
    agents.sort_by_key(|a| (a.meta.slot, a.meta.created));
    agents
}

/// One-shot Query against a daemon socket.
fn query_agent(sock: &Path) -> Option<(Meta, AgentState)> {
    let mut stream = UnixStream::connect(sock).ok()?;
    stream.set_read_timeout(Some(Duration::from_millis(500))).ok()?;
    stream.set_write_timeout(Some(Duration::from_millis(500))).ok()?;
    let frame = proto::encode_frame(&ToDaemon::Query).ok()?;
    stream.write_all(&frame).ok()?;

    let mut decoder = FrameDecoder::new();
    let mut buf = [0u8; 4096];
    let mut meta: Option<Meta> = None;
    let mut state: Option<AgentState> = None;
    loop {
        if let (Some(m), Some(s)) = (&meta, &state) {
            return Some((m.clone(), *s));
        }
        match decoder.next::<ToClient>() {
            Ok(Some(ToClient::MetaChanged(m))) => {
                meta = Some(m);
                continue;
            }
            Ok(Some(ToClient::StateChanged(s))) => {
                state = Some(s);
                continue;
            }
            Ok(Some(_)) => continue,
            Ok(None) => {}
            Err(_) => return None,
        }
        match stream.read(&mut buf) {
            Ok(0) => return None,
            Ok(n) => decoder.push(&buf[..n]),
            Err(_) => return None,
        }
    }
}

// ------------------------------------------------------------------------ new

pub fn cmd_new(args: &[String]) -> Result<()> {
    let Some(raw) = args.first() else {
        bail!("usage: warren new NAME [DIR] [COLOR 0-255] [new|resume|continue] [session-id]");
    };
    let base = crate::names::sanitize(raw);
    if base.is_empty() {
        bail!("empty name");
    }
    let dir = expand_dir(args.get(1).map(String::as_str));
    let color: u8 = args.get(2).map(|c| c.parse()).transpose().context("COLOR must be 0-255")?.unwrap_or(0);
    let mode = args.get(3).cloned().unwrap_or_else(|| "new".to_string());
    if !matches!(mode.as_str(), "new" | "resume" | "continue") {
        bail!("mode must be new, resume, or continue");
    }
    let sid = args.get(4).cloned();

    // Live agents give us both name collisions and used slots; discover()
    // also garbage-collects stale sockets so dead names become reusable.
    let live: Vec<(String, u8)> =
        discover().into_iter().map(|a| (a.meta.name, a.meta.slot)).collect();
    let name = launch_agent(&base, &dir, color, &mode, sid.as_deref(), &live)?;

    // Wait for the daemon's socket to come up so failures surface here.
    let sock = crate::paths::sock_path(&name);
    let deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < deadline {
        if query_agent(&sock).is_some() {
            println!("warren: created agent '{name}'");
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    bail!("agent '{name}' did not come up (set WARREN_LOG=/tmp/warren.log and retry to debug)");
}

/// Pick a unique name and free slot against `live` (name, slot) pairs and
/// spawn the daemon. Shared by the CLI and the dashboard's new-agent form.
pub fn launch_agent(
    base: &str,
    dir: &str,
    color: u8,
    mode: &str,
    sid: Option<&str>,
    live: &[(String, u8)],
) -> Result<String> {
    let name = crate::names::unique(base, live.iter().map(|(n, _)| n.as_str()));
    let slot = (1..=u8::MAX).find(|s| !live.iter().any(|(_, used)| used == s)).unwrap_or(0);
    spawn_daemon(&name, slot, color, mode, dir, sid)?;
    Ok(name)
}

/// Spawn `warren __daemon` fully detached: new session, no stdio, no cwd tie.
/// We exit immediately after, so it reparents to init (ppid 1).
fn spawn_daemon(
    name: &str,
    slot: u8,
    color: u8,
    mode: &str,
    dir: &str,
    sid: Option<&str>,
) -> Result<()> {
    let exe = std::env::current_exe()?;
    let mut cmd = std::process::Command::new(exe);
    cmd.arg("__daemon")
        .arg(name)
        .arg(slot.to_string())
        .arg(color.to_string())
        .arg(mode)
        .arg(dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .current_dir("/");
    if let Some(sid) = sid {
        cmd.arg(sid);
    }
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    cmd.spawn().context("spawning agent daemon")?;
    Ok(())
}

pub fn expand_dir(arg: Option<&str>) -> String {
    let home = std::env::var("HOME").unwrap_or_else(|_| "/".into());
    match arg {
        None | Some("") => std::env::var("PWD").unwrap_or(home),
        Some("~") => home,
        Some(d) if d.starts_with("~/") => format!("{home}/{}", &d[2..]),
        Some(d) => d.to_string(),
    }
}

// ------------------------------------------------------------------------- ls

pub fn cmd_ls() -> Result<()> {
    let agents = discover();
    if agents.is_empty() {
        println!("no agents running (in {})", crate::paths::run_dir().display());
        return Ok(());
    }
    println!("agents (in {}):", crate::paths::run_dir().display());
    for a in &agents {
        let state = match a.state.hook {
            Some(h) => format!("{h:?}").to_lowercase(),
            None if a.state.ms_since_output < 1500 => "working".to_string(),
            None => "idle".to_string(),
        };
        let label = if a.meta.display != a.meta.name {
            format!("{} ({})", a.meta.name, a.meta.display)
        } else {
            a.meta.name.clone()
        };
        println!("{:>3}  {:<32}  {:<9}  {}", a.meta.slot, label, state, a.meta.cwd);
    }
    Ok(())
}

// ----------------------------------------------------------------------- kill

pub fn cmd_kill(args: &[String]) -> Result<()> {
    let Some(raw) = args.first() else {
        bail!("usage: warren kill NAME");
    };
    let name = crate::names::sanitize(raw);
    let sock = crate::paths::sock_path(&name);
    let mut stream =
        UnixStream::connect(&sock).with_context(|| format!("no live agent named '{name}'"))?;
    stream.set_write_timeout(Some(Duration::from_millis(500)))?;
    stream.write_all(&proto::encode_frame(&ToDaemon::Kill)?)?;
    drop(stream);

    // The daemon unlinks its socket on the way out.
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if !sock.exists() {
            println!("warren: killed agent '{name}'");
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    println!("warren: kill sent to '{name}' (still shutting down)");
    Ok(())
}

// --------------------------------------------------------------------- attach
// Raw single-agent viewer: no sidebar, full terminal. Primarily a debugging
// and testing tool; Ctrl-\ detaches. The dashboard supersedes it for daily use.

pub fn cmd_attach(args: &[String]) -> Result<()> {
    let Some(raw) = args.first() else {
        bail!("usage: warren attach NAME");
    };
    let name = crate::names::sanitize(raw);
    let sock = crate::paths::sock_path(&name);
    let mut stream =
        UnixStream::connect(&sock).with_context(|| format!("no live agent named '{name}'"))?;
    stream.set_nonblocking(true)?;

    let stdin = std::io::stdin();
    let winsize = rustix::termios::tcgetwinsize(std::io::stdout())?;
    let saved = rustix::termios::tcgetattr(&stdin)?;
    let mut raw_tio = saved.clone();
    raw_tio.make_raw();
    rustix::termios::tcsetattr(&stdin, rustix::termios::OptionalActions::Now, &raw_tio)?;
    // Alternate screen + reset, restored on the way out.
    print!("\x1b[?1049h\x1b[2J");
    let _ = std::io::stdout().flush();

    let result = attach_loop(&mut stream, winsize.ws_col, winsize.ws_row);

    print!("\x1b[0m\x1b[?1049l");
    let _ = std::io::stdout().flush();
    rustix::termios::tcsetattr(&stdin, rustix::termios::OptionalActions::Now, &saved)?;
    match result {
        Ok(reason) => {
            println!("warren: {reason}");
            Ok(())
        }
        Err(e) => Err(e),
    }
}

fn attach_loop(stream: &mut UnixStream, cols: u16, rows: u16) -> Result<String> {
    use polling::{Event as PollEvent, Events, PollMode, Poller};

    let mut out_q: Vec<u8> = proto::encode_frame(&ToDaemon::Attach { cols, rows })?;
    let stdin = std::io::stdin();

    let poller = Poller::new()?;
    unsafe {
        poller.add_with_mode(&stdin, PollEvent::readable(0), PollMode::Level)?;
        poller.add_with_mode(&*stream, PollEvent::all(1), PollMode::Level)?;
    }

    let mut decoder = FrameDecoder::new();
    let mut events = Events::new();
    let stdout = std::io::stdout();

    loop {
        events.clear();
        poller.wait(&mut events, None)?;
        for ev in events.iter() {
            if ev.key == 0 && ev.readable {
                let mut buf = [0u8; 4096];
                let n = read_nb(&stdin, &mut buf)?;
                if buf[..n].contains(&0x1c) {
                    return Ok("detached (agent keeps running)".into());
                }
                if n > 0 {
                    let msg = ToDaemon::Input(proto::b64_encode(&buf[..n]));
                    out_q.extend_from_slice(&proto::encode_frame(&msg)?);
                }
            }
            if ev.key == 1 {
                if ev.readable {
                    let mut buf = [0u8; 65536];
                    let n = read_nb(&*stream, &mut buf)?;
                    if n == 0 {
                        return Ok("daemon closed the connection".into());
                    }
                    decoder.push(&buf[..n]);
                }
            }
        }

        // Drain pending writes (input) without ever blocking hard.
        while !out_q.is_empty() {
            match (&*stream).write(&out_q) {
                Ok(0) => return Ok("daemon closed the connection".into()),
                Ok(n) => {
                    out_q.drain(..n);
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(e) => return Err(e.into()),
            }
        }

        let mut painter = stdout.lock();
        while let Some(msg) = decoder.next::<ToClient>()? {
            match msg {
                ToClient::Snapshot { screen, cursor, cursor_visible, .. } => {
                    write!(painter, "\x1b[2J")?;
                    for (row, line) in screen.iter().enumerate() {
                        draw_line(&mut painter, row as u16, line)?;
                    }
                    place_cursor(&mut painter, cursor, cursor_visible)?;
                }
                ToClient::Damage { lines, cursor, cursor_visible, .. } => {
                    for (row, line) in &lines {
                        draw_line(&mut painter, *row, line)?;
                    }
                    place_cursor(&mut painter, cursor, cursor_visible)?;
                }
                ToClient::Exited { status } => {
                    return Ok(format!("agent exited with status {status}"));
                }
                _ => {}
            }
        }
        painter.flush()?;
    }
}

fn read_nb(mut src: impl Read, buf: &mut [u8]) -> Result<usize> {
    match src.read(buf) {
        Ok(n) => Ok(n),
        Err(e) if e.kind() == ErrorKind::WouldBlock => Ok(0),
        Err(e) => Err(e.into()),
    }
}

fn draw_line(out: &mut impl Write, row: u16, line: &crate::spans::LineSpans) -> Result<()> {
    write!(out, "\x1b[{};1H\x1b[2K", row + 1)?;
    for span in &line.0 {
        write!(out, "{}{}", crate::spans::sgr_sequence(span), span.text)?;
    }
    write!(out, "\x1b[0m")?;
    Ok(())
}

fn place_cursor(out: &mut impl Write, cursor: (u16, u16), visible: bool) -> Result<()> {
    write!(out, "\x1b[{};{}H", cursor.0 + 1, cursor.1 + 1)?;
    write!(out, "{}", if visible { "\x1b[?25h" } else { "\x1b[?25l" })?;
    Ok(())
}
