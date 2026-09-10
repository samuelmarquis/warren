//! The two commands warren runs *on the far machine*, over ssh.
//!
//! Both are deliberately dumb: `__roster` says which agents exist, `__pipe`
//! carries one agent's socket on its stdio. Neither understands the daemon
//! protocol — the dashboard on the near machine speaks that, straight through,
//! exactly as it does to a local socket. Nothing here holds state, so a
//! dropped ssh loses nothing but the connection.

use std::io::{ErrorKind, Write};
use std::os::fd::{AsFd, BorrowedFd};
use std::os::unix::net::UnixStream;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use polling::{Event as PollEvent, Events, PollMode, Poller};

/// Bytes queued in one direction before we call the far side wedged. The
/// daemons already drop viewers that stop reading; this is the same rule one
/// hop out, so a stalled ssh can never grow without bound.
const PIPE_CAP: usize = 4 * 1024 * 1024;

/// How often `__roster` re-reads the run directory.
const ROSTER_TICK: Duration = Duration::from_millis(750);

/// `warren __roster` — one line per live agent, reprinted whenever the set
/// changes, until stdout closes. The first line names the protocol build, so
/// the near side can say "that machine runs an older warren" instead of
/// discovering it as a corrupt frame later.
pub fn cmd_roster() -> Result<()> {
    println!("warren {} {}", env!("CARGO_PKG_VERSION"), crate::proto::WIRE_VERSION);
    flush()?;

    let mut last: Vec<String> = Vec::new();
    loop {
        let mut names: Vec<String> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(crate::paths::run_dir()) {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("sock") {
                    continue;
                }
                // Answering is what makes an agent live; a stale socket from a
                // crashed daemon is not this command's to clean up.
                if UnixStream::connect(&path).is_err() {
                    continue;
                }
                if let Some(stem) = path.file_stem().and_then(|s| s.to_str()) {
                    names.push(stem.to_string());
                }
            }
        }
        names.sort();
        if names != last {
            // One roster per block, ended by a blank line: a reader that
            // arrives mid-write still gets whole lists.
            for name in &names {
                println!("{name}");
            }
            println!();
            flush()?;
            last = names;
        }
        std::thread::sleep(ROSTER_TICK);
    }
}

fn flush() -> Result<()> {
    // A closed pipe means the dashboard let go: leave quietly.
    match std::io::stdout().flush() {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::BrokenPipe => std::process::exit(0),
        Err(e) => Err(e.into()),
    }
}

/// `warren __pipe NAME` — carry one agent's unix socket on stdin/stdout.
///
/// Two independent directions, both non-blocking, neither allowed to stall
/// the other: whichever side is ready moves. Ends when either side closes.
pub fn cmd_pipe(args: &[String]) -> Result<()> {
    let Some(raw) = args.first() else {
        bail!("usage: warren __pipe NAME");
    };
    let name = crate::names::sanitize(raw);
    let sock = crate::paths::sock_path(&name);
    let stream = UnixStream::connect(&sock)
        .with_context(|| format!("no live agent named '{name}' on this machine"))?;
    stream.set_nonblocking(true)?;

    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    set_nonblocking(&stdin)?;
    set_nonblocking(&stdout)?;

    const KEY_IN: usize = 0;
    const KEY_SOCK: usize = 1;
    let poller = Poller::new()?;
    unsafe {
        poller.add_with_mode(&stdin, PollEvent::readable(KEY_IN), PollMode::Level)?;
        poller.add_with_mode(&stream, PollEvent::readable(KEY_SOCK), PollMode::Level)?;
    }

    // to_agent: read from stdin, write to the socket. to_ssh: the reverse.
    let mut to_agent: Vec<u8> = Vec::new();
    let mut to_ssh: Vec<u8> = Vec::new();
    let (mut stdin_open, mut sock_open) = (true, true);
    let mut events = Events::new();
    let trace = std::env::var("WARREN_LOG").is_ok();

    while sock_open && (stdin_open || !to_agent.is_empty()) {
        // Interest follows the queues: read a side only while there is room
        // for what it might give us, and watch for writability only while
        // something is waiting to go out.
        let _ = poller.modify_with_mode(
            &stdin,
            PollEvent::new(KEY_IN, stdin_open && to_agent.len() < PIPE_CAP, false),
            PollMode::Level,
        );
        let _ = poller.modify_with_mode(
            &stream,
            PollEvent::new(KEY_SOCK, to_ssh.len() < PIPE_CAP, !to_agent.is_empty()),
            PollMode::Level,
        );

        events.clear();
        match poller.wait(&mut events, None) {
            Ok(_) => {}
            Err(e) if e.kind() == ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
        if trace {
            let keys: Vec<String> = events
                .iter()
                .map(|e| format!("{}{}{}", e.key, if e.readable { "r" } else { "" }, if e.writable { "w" } else { "" }))
                .collect();
            eprintln!(
                "[pipe] wake [{}] to_agent={} to_ssh={}",
                keys.join(","),
                to_agent.len(),
                to_ssh.len()
            );
        }

        for ev in events.iter() {
            match ev.key {
                KEY_IN if ev.readable => match read_some(stdin.as_fd(), &mut to_agent) {
                    Ok(0) => stdin_open = false, // the dashboard hung up
                    Ok(_) => {}
                    Err(_) => stdin_open = false,
                },
                KEY_SOCK => {
                    if ev.readable {
                        match read_some(stream.as_fd(), &mut to_ssh) {
                            Ok(0) => sock_open = false, // the agent went away
                            Ok(_) => {}
                            Err(_) => sock_open = false,
                        }
                    }
                    if ev.writable {
                        if write_some(stream.as_fd(), &mut to_agent).is_err() {
                            sock_open = false;
                        }
                    }
                }
                _ => {}
            }
        }

        // stdout is a pipe ssh owns; a short write just leaves the tail for
        // the next pass rather than blocking the agent's direction.
        if !to_ssh.is_empty() && write_some(stdout.as_fd(), &mut to_ssh).is_err() {
            break;
        }
        if to_agent.len() >= PIPE_CAP || to_ssh.len() >= PIPE_CAP {
            break; // wedged peer: drop the connection, the viewer reattaches
        }
    }
    // Last word to the dashboard, if any is still queued and it will take it.
    let deadline = Instant::now() + Duration::from_millis(200);
    while !to_ssh.is_empty() && Instant::now() < deadline {
        if write_some(stdout.as_fd(), &mut to_ssh).is_err() {
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    Ok(())
}

fn set_nonblocking(fd: &impl AsFd) -> Result<()> {
    let flags = rustix::fs::fcntl_getfl(fd.as_fd())?;
    rustix::fs::fcntl_setfl(fd.as_fd(), flags | rustix::fs::OFlags::NONBLOCK)?;
    Ok(())
}

// Raw descriptors throughout, never std's wrappers: `Stdout` is line
// buffered, and protocol frames are binary with no newlines in them — they
// would sit in that buffer until something happened to overflow it. Reading
// through `Stdin`'s buffer is the same trap in reverse: bytes parked in user
// space that the poller, watching the descriptor, would never report.

/// Read what's there. Ok(0) means the far end closed for good.
fn read_some(fd: BorrowedFd, into: &mut Vec<u8>) -> std::io::Result<usize> {
    let mut buf = [0u8; 65536];
    match rustix::io::read(fd, &mut buf[..]) {
        Ok(0) => Ok(0),
        Ok(n) => {
            into.extend_from_slice(&buf[..n]);
            Ok(n)
        }
        Err(rustix::io::Errno::AGAIN) | Err(rustix::io::Errno::INTR) => Ok(1), // still open
        Err(e) => Err(e.into()),
    }
}

/// Write what will go now; anything left waits for the next pass.
fn write_some(fd: BorrowedFd, from: &mut Vec<u8>) -> std::io::Result<()> {
    while !from.is_empty() {
        match rustix::io::write(fd, from) {
            Ok(0) => return Err(std::io::Error::from(ErrorKind::WriteZero)),
            Ok(n) => {
                from.drain(..n);
            }
            Err(rustix::io::Errno::AGAIN) => return Ok(()),
            Err(rustix::io::Errno::INTR) => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(())
}
