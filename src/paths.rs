//! Filesystem layout under ~/.warren.
//!
//! v1 owns only `run/` (one unix socket per agent daemon) and `hooks.json`.
//! v0's `agents/ dash/ meta/` directories are never touched, so both versions
//! can coexist during the cutover.

use std::path::PathBuf;

use anyhow::{Context, Result};

/// macOS sun_path is 104 bytes (incl. NUL); Linux is 108. Stay under the lower.
const SUN_PATH_MAX: usize = 103;

pub fn home() -> PathBuf {
    if let Ok(h) = std::env::var("WARREN_HOME") {
        if !h.is_empty() {
            return PathBuf::from(h);
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join(".warren")
}

/// Directory holding one unix socket per live agent daemon.
///
/// Falls back to `$TMPDIR/warren-$UID/run` when `$HOME` is deep enough that
/// socket paths would overflow `sun_path` (sockets need name space for a
/// 32-char agent name plus ".sock").
pub fn run_dir() -> PathBuf {
    let primary = home().join("run");
    let longest = primary.as_os_str().len() + 1 + crate::names::NAME_MAX + ".sock".len();
    if longest <= SUN_PATH_MAX {
        return primary;
    }
    let tmp = std::env::var("TMPDIR").unwrap_or_else(|_| "/tmp".into());
    let uid = unsafe { libc_geteuid() };
    PathBuf::from(tmp).join(format!("warren-{uid}")).join("run")
}

pub fn ensure_run_dir() -> Result<PathBuf> {
    let dir = run_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    restrict_to_owner(&dir)?;
    Ok(dir)
}

pub fn sock_path(name: &str) -> PathBuf {
    run_dir().join(format!("{name}.sock"))
}

/// Where a daemon's stderr goes until its socket is up: a daemon that dies
/// before that leaves its reason here, and nowhere else.
pub fn spawn_err(name: &str) -> PathBuf {
    run_dir().join(format!("{name}.err"))
}

pub fn hooks_json() -> PathBuf {
    home().join("hooks.json")
}

/// Session-scoped settings file passed to `agy --settings`.
pub fn agy_settings_json(autonomous: bool) -> PathBuf {
    if autonomous {
        home().join("agy-settings-autonomous.json")
    } else {
        home().join("agy-settings.json")
    }
}

pub fn claude_projects() -> PathBuf {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR").filter(|d| !d.is_empty()) {
        return PathBuf::from(dir).join("projects");
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join(".claude").join("projects")
}

/// OMP's data directory (`WARREN_OMP_HOME` overrides it, for tests).
pub fn omp_home() -> PathBuf {
    if let Ok(dir) = std::env::var("WARREN_OMP_HOME") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join(".omp")
}

/// One directory per project, each holding `<timestamp>_<uuid>.jsonl`.
pub fn omp_sessions() -> PathBuf {
    omp_home().join("agent").join("sessions")
}

/// One row per tty: the cwd and session file of the OMP running on it.
pub fn omp_terminal_sessions() -> PathBuf {
    omp_home().join("agent").join("terminal-sessions")
}

/// Antigravity CLI's data directory (`WARREN_AGY_HOME` overrides it, for tests).
pub fn agy_home() -> PathBuf {
    if let Ok(dir) = std::env::var("WARREN_AGY_HOME") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join(".gemini").join("antigravity-cli")
}

/// One `<uuid>.db` trajectory database per Antigravity conversation.
pub fn agy_conversations() -> PathBuf {
    agy_home().join("conversations")
}

/// One `<uuid>/` directory per Antigravity conversation (artifacts and JSONL transcripts).
pub fn agy_brain() -> PathBuf {
    agy_home().join("brain")
}

/// Antigravity's global customization directory (`WARREN_GEMINI_CONFIG` overrides it, for tests).
pub fn gemini_config() -> PathBuf {
    if let Ok(dir) = std::env::var("WARREN_GEMINI_CONFIG") {
        if !dir.is_empty() {
            return PathBuf::from(dir);
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".into());
    PathBuf::from(home).join(".gemini").join("config")
}

/// Global Antigravity plugins (`~/.gemini/config/plugins/`).
pub fn agy_plugins_dir() -> PathBuf {
    gemini_config().join("plugins")
}

fn restrict_to_owner(dir: &std::path::Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    let perms = std::fs::Permissions::from_mode(0o700);
    std::fs::set_permissions(dir, perms).with_context(|| format!("chmod 700 {}", dir.display()))?;
    Ok(())
}

// Tiny shim so we don't pull the libc crate for one call before the daemon
// milestone (which brings rustix) lands.
unsafe extern "C" {
    #[link_name = "geteuid"]
    fn libc_geteuid() -> u32;
}
