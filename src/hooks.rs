//! Claude Code and Antigravity lifecycle-hook integration.
//!
//! `ensure_hooks_json()` writes the settings file each Claude agent gets via
//! `claude --settings`; `ensure_agy_settings_json()` writes the session-only
//! settings file each Antigravity agent gets via `agy --settings` (binding its
//! `title` command to `warren hook agy`). Both connect to the agent's own
//! daemon socket ($WARREN_SOCK, set in the agent's env) and push `HookState`
//! and `Session` frames. The sidebar updates instantly — no meta files, no
//! polling.
//!
//! For Antigravity agents with Matrix chat on, the `warren-matrix` plugin's
//! `SessionStart`, `PreInvocation`, `PostInvocation`, and `Stop` hooks also
//! run `warren hook agy-*`, atomically draining any `<channel …>` messages the
//! daemon buffered mid-turn so pushed messages land between steps just as they
//! do in Claude Code and OMP.

use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use serde_json::{Value, json};

use crate::proto::{self, FrameDecoder, HookState, ToClient, ToDaemon};

/// Format a binary path for `sh -c`: bare when every byte is shell-safe, else
/// single-quoted.
pub(crate) fn shell_exe(exe: &std::path::Path) -> String {
    let s = exe.display().to_string();
    let safe = !s.is_empty()
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'/' | b'.' | b'_' | b'-'));
    if safe {
        s
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

pub fn ensure_hooks_json() -> Result<PathBuf> {
    let path = crate::paths::hooks_json();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let exe = std::env::current_exe().context("resolving warren binary path")?;
    let hook = format!("{} hook", shell_exe(&exe));
    let body = serde_json::to_vec_pretty(&json!({
        "hooks": {
            "SessionStart":     [{ "hooks": [{ "type": "command", "command": format!("{hook} waiting") }] }],
            "UserPromptSubmit": [{ "hooks": [{ "type": "command", "command": format!("{hook} working") }] }],
            "PreToolUse":       [{ "matcher": "*", "hooks": [{ "type": "command", "command": format!("{hook} working") }] }],
            "PostToolUse":      [{ "matcher": "*", "hooks": [{ "type": "command", "command": format!("{hook} working") }] }],
            "Notification":     [{ "hooks": [{ "type": "command", "command": format!("{hook} attention") }] }],
            "Stop":             [{ "hooks": [{ "type": "command", "command": format!("{hook} waiting") }] }],
            "SessionEnd":       [{ "hooks": [{ "type": "command", "command": format!("{hook} gone") }] }],
        }
    }))?;
    std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

/// Session-only settings file passed via `agy --settings`: binds `title` to
/// `warren hook agy`, which receives Antigravity's JSON state telemetry on
/// stdin (`agent_state`, `conversation_id`, `conversation_title`) and echoes
/// `conversation_title` to stdout so `agy` updates the terminal title.
///
/// When `autonomous` is true (`--dangerously-skip-permissions` in `--extra`),
/// also disables the macOS/Linux terminal sandbox and enables non-workspace
/// file access + `always-proceed` tool permissions so headless Matrix-driven
/// agents never hit sandbox-exec shell errors or hang on TUI prompts.
pub fn ensure_agy_settings_json(autonomous: bool) -> Result<PathBuf> {
    let path = crate::paths::agy_settings_json(autonomous);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let exe = std::env::current_exe().context("resolving warren binary path")?;
    let body = serde_json::to_vec_pretty(&agy_settings_value(&exe, autonomous))?;
    std::fs::write(&path, body).with_context(|| format!("writing {}", path.display()))?;
    Ok(path)
}

fn agy_settings_value(exe: &std::path::Path, autonomous: bool) -> Value {
    let mut cfg = json!({
        "title": {
            "type": "command",
            "command": format!("{} hook agy", shell_exe(exe)),
            "enabled": true,
        }
    });
    if autonomous {
        cfg["toolPermission"] = json!("always-proceed");
        cfg["enableTerminalSandbox"] = json!(false);
        cfg["allowNonWorkspaceAccess"] = json!(true);
    }
    cfg
}

/// `warren hook <mode>` — runs inside the agent on Claude or Antigravity hooks.
///
/// MUST exit 0 no matter what: a missing socket, a wedged daemon, a silent
/// stdin, or a bad argument may never stall or fail a harness's hook pipeline.
/// Short timeouts guarantee that even a frozen peer costs at most ~450ms.
pub fn cmd_hook(args: &[String]) -> Result<()> {
    let Some(mode) = args.first().map(String::as_str) else {
        return Ok(());
    };
    match mode {
        "agy" => return cmd_hook_agy_title(),
        "agy-session" | "agy-pre" | "agy-post" | "agy-stop" => {
            return cmd_hook_agy_lifecycle(mode);
        }
        _ => {}
    }

    let Ok(sock) = std::env::var("WARREN_SOCK") else {
        return Ok(()); // claude running outside warren
    };
    let Some(state) = HookState::parse(mode) else {
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

/// `warren hook agy`: Antigravity's `title` command. Reads the telemetry JSON
/// on stdin, prints `conversation_title` to stdout (so `agy` sets the terminal
/// title and the daemon mirrors it to the sidebar), and pushes the mapped
/// `HookState` + `conversation_id` to the daemon.
fn cmd_hook_agy_title() -> Result<()> {
    let payload = read_payload();
    if let Some(title) = payload.as_ref().and_then(agy_title_of) {
        println!("{title}");
    }
    if let Ok(sock) = std::env::var("WARREN_SOCK") {
        let session = payload.as_ref().and_then(session_of);
        let raw_state = payload
            .as_ref()
            .and_then(|p| p.get("agent_state"))
            .and_then(Value::as_str);
        let state = agy_state(raw_state);
        let _ = try_send(&sock, state, session);
    }
    Ok(())
}

/// `warren hook agy-{session,pre,post,stop}`: Antigravity's `hooks.json`
/// handlers from the `warren-matrix` plugin. Always prints a JSON object to
/// stdout as Antigravity's hook contract requires, and exits 0.
fn cmd_hook_agy_lifecycle(mode: &str) -> Result<()> {
    let payload = read_payload();
    let session = payload.as_ref().and_then(session_of);
    let sock = std::env::var("WARREN_SOCK").ok().filter(|s| !s.is_empty());
    let agent = std::env::var("WARREN_AGENT").ok().filter(|s| !s.is_empty());
    let chat_on = agent.as_deref().is_some_and(crate::matrix::is_agy_enabled);

    let drained = match (&sock, chat_on) {
        (Some(s), true) => drain_channel(s).unwrap_or_default(),
        _ => Vec::new(),
    };

    let out = match mode {
        "agy-stop" => {
            if !drained.is_empty() {
                if let Some(s) = &sock {
                    let _ = try_send(s, Some(HookState::Working), session);
                }
                json!({
                    "decision": "continue",
                    "reason": drained.join("\n\n"),
                })
            } else {
                if let Some(s) = &sock {
                    let _ = try_send(s, Some(HookState::Waiting), session);
                }
                json!({ "decision": "stop" })
            }
        }
        _ => {
            let state = if mode == "agy-session" {
                HookState::Waiting
            } else {
                HookState::Working
            };
            if let Some(s) = &sock {
                let _ = try_send(s, Some(state), session.clone());
            }
            let mut steps: Vec<Value> = Vec::new();
            if chat_on && matches!(mode, "agy-session" | "agy-pre") {
                if let Some(agent) = agent.as_deref() {
                    if let Some(note) =
                        crate::matrix::take_agy_instructions(agent, session.as_deref())
                    {
                        steps.push(json!({
                            "systemMessage": { "systemMessage": note }
                        }));
                    }
                }
            }
            for msg in drained {
                steps.push(json!({ "userMessage": msg }));
            }
            if steps.is_empty() {
                json!({})
            } else if mode == "agy-post" {
                json!({
                    "injectSteps": steps,
                    "terminationBehavior": "force_continue",
                })
            } else {
                json!({ "injectSteps": steps })
            }
        }
    };
    println!("{out}");
    Ok(())
}

/// Map Antigravity's `agent_state` telemetry strings onto warren's `HookState`.
fn agy_state(state: Option<&str>) -> Option<HookState> {
    match state.map(str::trim) {
        Some("idle" | "error" | "failed") => Some(HookState::Waiting),
        Some("waiting" | "approval" | "permission" | "authenticating") => {
            Some(HookState::Attention)
        }
        Some(s)
            if s.contains("permission") || s.contains("approval") || s.contains("needs_input") =>
        {
            Some(HookState::Attention)
        }
        Some("working" | "thinking" | "tool_use" | "reviewing" | "initializing" | "running") => {
            Some(HookState::Working)
        }
        Some(s) if s.starts_with("tool") || s.starts_with("calling") => Some(HookState::Working),
        _ => None,
    }
}

fn agy_title_of(payload: &Value) -> Option<String> {
    let t = payload.get("conversation_title")?.as_str()?.trim();
    (!t.is_empty()).then(|| t.to_string())
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

/// Atomically drain queued `<channel …>` pushes from the agent's daemon.
fn drain_channel(sock: &str) -> Result<Vec<String>> {
    let mut stream = UnixStream::connect(sock)?;
    stream.set_read_timeout(Some(Duration::from_millis(250)))?;
    stream.set_write_timeout(Some(Duration::from_millis(250)))?;
    stream.write_all(&proto::encode_frame(&ToDaemon::DrainChannel)?)?;

    let mut decoder = FrameDecoder::new();
    let mut buf = [0u8; 4096];
    loop {
        if let Some(msg) = decoder.next::<ToClient>()? {
            if let ToClient::ChannelBatch(batch) = msg {
                return Ok(batch);
            }
        }
        match stream.read(&mut buf) {
            Ok(0) => return Ok(Vec::new()),
            Ok(n) => decoder.push(&buf[..n]),
            Err(_) => return Ok(Vec::new()),
        }
    }
}

/// The session id an event's payload carries, if it carries one. Claude spells
/// it `session_id`; Antigravity spells it `conversation_id` in telemetry and
/// `conversationId` in `hooks.json` payloads.
fn session_of(payload: &serde_json::Value) -> Option<String> {
    for key in ["session_id", "conversation_id", "conversationId"] {
        if let Some(sid) = payload.get(key).and_then(Value::as_str) {
            if !sid.is_empty() {
                return Some(sid.to_string());
            }
        }
    }
    None
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
        let left = deadline
            .saturating_duration_since(Instant::now())
            .as_millis() as i32;
        if left <= 0 {
            break;
        }
        let mut pfd = libc::pollfd {
            fd,
            events: libc::POLLIN,
            revents: 0,
        };
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

    #[test]
    fn agy_telemetry_maps_states_ids_and_titles() {
        assert_eq!(agy_state(Some("idle")), Some(HookState::Waiting));
        assert_eq!(agy_state(Some("error")), Some(HookState::Waiting));
        assert_eq!(agy_state(Some("working")), Some(HookState::Working));
        assert_eq!(agy_state(Some("thinking")), Some(HookState::Working));
        assert_eq!(agy_state(Some("tool_use")), Some(HookState::Working));
        assert_eq!(agy_state(Some("waiting")), Some(HookState::Attention));
        assert_eq!(agy_state(Some("approval")), Some(HookState::Attention));
        assert_eq!(agy_state(None), None);

        let p = json!({
            "conversation_id": "21bc5b13-c8b7-4333-943e-e6fa66b9c21f",
            "conversation_title": "Wire up Antigravity",
        });
        assert_eq!(
            session_of(&p).as_deref(),
            Some("21bc5b13-c8b7-4333-943e-e6fa66b9c21f")
        );
        assert_eq!(agy_title_of(&p).as_deref(), Some("Wire up Antigravity"));

        let hook_p = json!({ "conversationId": "ec33ebf9-0cba", "conversation_title": "   " });
        assert_eq!(session_of(&hook_p).as_deref(), Some("ec33ebf9-0cba"));
        assert_eq!(agy_title_of(&hook_p), None);

        let default_cfg = agy_settings_value(std::path::Path::new("/bin/warren"), false);
        assert!(default_cfg.get("enableTerminalSandbox").is_none());
        let auto_cfg = agy_settings_value(std::path::Path::new("/bin/warren"), true);
        assert_eq!(auto_cfg["toolPermission"], "always-proceed");
        assert_eq!(auto_cfg["enableTerminalSandbox"], false);
        assert_eq!(auto_cfg["allowNonWorkspaceAccess"], true);
    }
}
