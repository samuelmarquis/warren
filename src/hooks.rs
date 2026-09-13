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
    let payload = read_payload();
    let session = payload.as_ref().and_then(session_of);
    // Notification is fired for anything Claude wants to say, only some of
    // which is "I am stuck": the payload says which.
    let state = match state {
        HookState::Attention => {
            let kind = payload
                .as_ref()
                .and_then(|p| p.get("notification_type"))
                .and_then(|v| v.as_str());
            blocked_on_you(kind)
        }
        other => Some(other),
    };
    let _ = try_send(&sock, state, session); // best-effort by design
    Ok(())
}

/// Which of Claude Code's notifications actually mean it is waiting on you.
///
/// `Notification` covers eleven different things, and a permission prompt is
/// only one of them: an idle nudge a minute after your turn started, an auth
/// success, a background agent finishing. The rest say nothing about whether
/// this agent is blocked, so they leave its state exactly as it was — an
/// agent that is merely waiting for you to think of the next thing has been
/// wearing `!` (blocked) when it should wear `*` (finished while you were
/// looking elsewhere).
///
/// An event with no type is an older Claude, or another harness, and gets
/// the benefit of the doubt: a block nobody notices is the thing `!` exists
/// to prevent.
fn blocked_on_you(kind: Option<&str>) -> Option<HookState> {
    match kind {
        None => Some(HookState::Attention),
        Some("permission_prompt" | "worker_permission_prompt") => Some(HookState::Attention),
        Some("elicitation_dialog" | "agent_needs_input") => Some(HookState::Attention),
        // Names warren has not seen but which can only mean one thing. The
        // sibling elicitation_* events are answers, not questions, so that
        // one is spelled out above rather than matched loosely.
        Some(k) if k.contains("permission") || k.contains("needs_input") => {
            Some(HookState::Attention)
        }
        Some(_) => None,
    }
}

fn try_send(sock: &str, state: Option<HookState>, session: Option<String>) -> Result<()> {
    if state.is_none() && session.is_none() {
        return Ok(());
    }
    let mut stream = UnixStream::connect(sock)?;
    stream.set_write_timeout(Some(Duration::from_millis(250)))?;
    // State first: a pre-sleep-mode daemon applies this one and only then
    // fails on the Session frame it has never heard of.
    if let Some(state) = state {
        stream.write_all(&proto::encode_frame(&ToDaemon::HookState(state))?)?;
    }
    if let Some(sid) = session {
        stream.write_all(&proto::encode_frame(&ToDaemon::Session(sid))?)?;
    }
    Ok(())
}

/// The session id an event's payload carries, if it carries one.
fn session_of(payload: &serde_json::Value) -> Option<String> {
    let sid = payload.get("session_id")?.as_str()?;
    (!sid.is_empty()).then(|| sid.to_string())
}

/// The hook's stdin payload, if it arrives promptly.
///
/// Claude writes one small JSON object and closes, but we never assume that:
/// every read is poll-gated against a 200ms budget, so a hook whose stdin is
/// a terminal (someone running `warren hook` by hand) returns immediately
/// instead of blocking Claude's pipeline forever.
fn read_payload() -> Option<serde_json::Value> {
    fn parse(buf: &[u8]) -> Option<serde_json::Value> {
        let value: serde_json::Value = serde_json::from_slice(buf).ok()?;
        value.is_object().then_some(value)
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
                if let Some(value) = parse(&buf) {
                    return Some(value);
                }
                if buf.len() > 1 << 20 {
                    break;
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(_) => break,
        }
    }
    parse(&buf)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_notifications_that_block_raise_a_flag() {
        let blocks = |k: Option<&str>| blocked_on_you(k) == Some(HookState::Attention);
        // Claude is stuck and cannot go on without you.
        assert!(blocks(Some("permission_prompt")));
        assert!(blocks(Some("worker_permission_prompt")));
        assert!(blocks(Some("elicitation_dialog")));
        assert!(blocks(Some("agent_needs_input")));
        // Claude is talking, not waiting. The idle nudge a minute into your
        // turn is the one that had every quiet agent wearing `!`.
        assert_eq!(blocked_on_you(Some("idle_prompt")), None);
        assert_eq!(blocked_on_you(Some("auth_success")), None);
        assert_eq!(blocked_on_you(Some("agent_completed")), None);
        assert_eq!(blocked_on_you(Some("elicitation_response")), None);
        assert_eq!(blocked_on_you(Some("elicitation_complete")), None);
        assert_eq!(blocked_on_you(Some("computer_use_enter")), None);
        // A name we have not met that can only mean one thing.
        assert!(blocks(Some("tool_permission_prompt")));
        assert!(blocks(Some("subagent_needs_input")));
        // No type at all: an older Claude, or another harness. A block
        // nobody notices is the thing the flag exists to prevent.
        assert!(blocks(None));
    }
}
