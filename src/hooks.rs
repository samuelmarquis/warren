//! Claude Code lifecycle-hook integration.
//!
//! `ensure_hooks_json()` writes the settings file each agent gets via
//! `claude --settings`; the hooks run `warren hook <state>`, which connects to
//! the agent's own daemon socket ($WARREN_SOCK, set in the agent's env) and
//! sends one HookState frame. The sidebar updates instantly — no meta files,
//! no polling. Crucially this keeps an agent "working" through a silent tool
//! run that no screen heuristic can see.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::time::Duration;

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
/// MUST exit 0 no matter what: a missing socket, a wedged daemon, or a bad
/// argument may never stall or fail Claude's hook pipeline. Short timeouts
/// guarantee that even a frozen daemon costs at most ~250ms.
pub fn cmd_hook(args: &[String]) -> Result<()> {
    let (Some(state_str), Ok(sock)) = (args.first(), std::env::var("WARREN_SOCK")) else {
        return Ok(()); // claude running outside warren, or no state given
    };
    let Some(state) = HookState::parse(state_str) else {
        return Ok(());
    };
    let _ = try_send(&sock, state); // best-effort by design
    Ok(())
}

fn try_send(sock: &str, state: HookState) -> Result<()> {
    let mut stream = UnixStream::connect(sock)?;
    stream.set_write_timeout(Some(Duration::from_millis(250)))?;
    let frame = proto::encode_frame(&ToDaemon::HookState(state))?;
    stream.write_all(&frame)?;
    Ok(())
}
