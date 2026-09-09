//! Claude Code lifecycle-hook integration.
//!
//! `ensure_hooks_json()` writes the settings file each agent gets via
//! `claude --settings`; the hooks run `warren hook <state>`, which connects to
//! the agent's own daemon socket ($WARREN_SOCK, set in the agent's env) and
//! sends one HookState frame. The sidebar updates instantly — no meta files,
//! no polling. Crucially this keeps an agent "working" through a silent tool
//! run that no screen heuristic can see.

use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};

use crate::proto::{self, HookState, ToDaemon};

pub fn ensure_hooks_json() -> Result<PathBuf> {
    let path = crate::paths::hooks_json();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let exe = std::env::current_exe().context("resolving warren binary path")?;
    let hook = format!("{} hook", exe.display());
    let json = format!(
        r#"{{
  "hooks": {{
    "SessionStart":     [{{ "hooks": [{{ "type": "command", "command": "{hook} waiting" }}] }}],
    "UserPromptSubmit": [{{ "hooks": [{{ "type": "command", "command": "{hook} working" }}] }}],
    "PreToolUse":       [{{ "matcher": "*", "hooks": [{{ "type": "command", "command": "{hook} working" }}] }}],
    "PostToolUse":      [{{ "matcher": "*", "hooks": [{{ "type": "command", "command": "{hook} working" }}] }}],
    "Notification":     [{{ "hooks": [{{ "type": "command", "command": "{hook} attention" }}] }}],
    "Stop":             [{{ "hooks": [{{ "type": "command", "command": "{hook} waiting" }}] }}],
    "SessionEnd":       [{{ "hooks": [{{ "type": "command", "command": "{hook} gone" }}] }}]
  }}
}}
"#
    );
    std::fs::write(&path, json).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// `warren hook <state>` — runs inside the agent on Claude's lifecycle hooks.
///
/// MUST exit 0 no matter what: a missing socket, a wedged daemon, a silent
/// stdin, or a bad argument may never stall or fail Claude's hook pipeline.
/// Short timeouts guarantee that even a frozen peer costs at most ~450ms.
pub fn cmd_hook(args: &[String]) -> Result<()> {
    let (Some(state_str), Ok(sock)) = (args.first(), std::env::var("WARREN_SOCK")) else {
        return Ok(()); // claude running outside warren, or no state given
    };
    let Some(state) = HookState::parse(state_str) else {
        return Ok(());
    };
    // Every hook event's stdin payload carries the session id; that's how the
    // daemon learns what to `claude --resume` after a sleep. Best-effort: a
    // hook with no readable stdin still reports its state.
    let session = read_session_id();
    let _ = try_send(&sock, state, session); // best-effort by design
    Ok(())
}

fn try_send(sock: &str, state: HookState, session: Option<String>) -> Result<()> {
    let mut stream = UnixStream::connect(sock)?;
    stream.set_write_timeout(Some(Duration::from_millis(250)))?;
    // State first: a pre-sleep-mode daemon applies this one and only then
    // fails on the Session frame it has never heard of.
    stream.write_all(&proto::encode_frame(&ToDaemon::HookState(state))?)?;
    if let Some(sid) = session {
        stream.write_all(&proto::encode_frame(&ToDaemon::Session(sid))?)?;
    }
    Ok(())
}

/// `session_id` out of the hook's stdin JSON, if it arrives promptly.
///
/// Claude writes one small JSON object and closes, but we never assume that:
/// every read is poll-gated against a 200ms budget, so a hook whose stdin is
/// a terminal (someone running `warren hook` by hand) returns immediately
/// instead of blocking Claude's pipeline forever.
fn read_session_id() -> Option<String> {
    fn session_of(buf: &[u8]) -> Option<String> {
        let value: serde_json::Value = serde_json::from_slice(buf).ok()?;
        let sid = value.get("session_id")?.as_str()?;
        (!sid.is_empty()).then(|| sid.to_string())
    }

    let stdin = std::io::stdin();
    let fd = stdin.as_raw_fd();
    let deadline = Instant::now() + Duration::from_millis(200);
    let mut buf = Vec::with_capacity(1024);
    let mut chunk = [0u8; 4096];
    loop {
        let left = deadline.saturating_duration_since(Instant::now()).as_millis() as i32;
        if left <= 0 {
            break;
        }
        let mut pfd = libc::pollfd { fd, events: libc::POLLIN, revents: 0 };
        match unsafe { libc::poll(&mut pfd, 1, left) } {
            1 => {}
            _ => break, // timeout, or a stdin we can't poll
        }
        match stdin.lock().read(&mut chunk) {
            Ok(0) => break, // EOF: the whole payload is in hand
            Ok(n) => {
                buf.extend_from_slice(&chunk[..n]);
                // Return on a complete object rather than on EOF: this runs
                // on every tool call, and must never pay the timeout just
                // because the writer keeps the pipe open.
                if let Some(sid) = session_of(&buf) {
                    return Some(sid);
                }
                if buf.len() > 1 << 20 {
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    session_of(&buf)
}
