//! The agent harnesses warren knows how to run.
//!
//! Everything harness-specific lives here: the command line for one run,
//! whether lifecycle state can reach the daemon, where resumable sessions are
//! kept, and how a live agent's session is identified so sleep can resume it.
//! Another harness means a new variant and those answers — nothing in the
//! daemon, the dashboard or the protocol distinguishes them, and (by request)
//! neither does the sidebar.

use anyhow::Result;

use crate::sessions::Session;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Kind {
    #[default]
    Claude,
    Omp,
    Agy,
}

impl Kind {
    pub fn parse(s: &str) -> Option<Kind> {
        match s.trim().to_ascii_lowercase().as_str() {
            "claude" => Some(Kind::Claude),
            "omp" => Some(Kind::Omp),
            "agy" | "antigravity" => Some(Kind::Agy),
            _ => None,
        }
    }

    /// Also the program name — every harness is on PATH under its label.
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Claude => "claude",
            Kind::Omp => "omp",
            Kind::Agy => "agy",
        }
    }

    /// The base command for one run: the program plus its mode flags.
    /// Claude and OMP spell `--continue` and `--resume` alike; Antigravity
    /// spells resume by id as `--conversation`.
    pub fn base_command(self, mode: &str, sid: Option<&str>) -> String {
        let bin = self.as_str();
        match (self, mode) {
            (Kind::Agy, "resume") => match sid {
                Some(sid) => format!("{bin} --conversation {sid}"),
                None => format!("{bin} -i /resume"),
            },
            (_, "resume") => match sid {
                Some(sid) => format!("{bin} --resume {sid}"),
                // No id: the harness's own picker beats a wrong guess.
                None => format!("{bin} --resume"),
            },
            // A fork reads the conversation and then writes somewhere else:
            // same history, new session id, so the two never tread on each
            // other's transcript. Only the first run forks — every wake after
            // it resumes the id the fork reported.
            (_, "fork") => match sid {
                Some(sid) => format!("{bin} --resume {sid} --fork-session"),
                None => format!("{bin} --resume --fork-session"),
            },
            (_, "continue") => format!("{bin} --continue"),
            _ => bin.to_string(),
        }
    }

    /// Can this harness open a conversation *twice* from the command line —
    /// take its history and carry on separately? Claude Code has
    /// `--fork-session`; OMP and Antigravity have no CLI fork flag, and
    /// resuming twice means two processes appending to one transcript, so
    /// warren does not offer what it cannot deliver.
    pub fn can_fork(self) -> bool {
        matches!(self, Kind::Claude)
    }

    /// Can this harness take `/rename` at an idle prompt once a conversation
    /// is running? Claude Code and Antigravity both do; OMP has no `/rename`.
    pub fn can_retitle(self) -> bool {
        matches!(self, Kind::Claude | Kind::Agy)
    }

    /// The flag that starts a session already carrying the name it was given
    /// — what `/rename` does once you are inside. Without it Claude Code
    /// titles the session itself a turn or two in, and that title is what
    /// its terminal reports, what the sidebar mirrors and what the resume
    /// picker lists. OMP and Antigravity have no launch-time title flag;
    /// warren pins the row's name there instead (and `/rename`s Antigravity
    /// on explicit renames once it is idle).
    pub fn name_flag(self, title: &str) -> Option<String> {
        match self {
            Kind::Claude => Some(format!("-n '{}'", title.replace('\'', r"'\''"))),
            Kind::Omp | Kind::Agy => None,
        }
    }

    /// Does this harness take a per-agent system prompt on the command line?
    /// Claude Code and OMP spell it `--system-prompt`; Antigravity has none
    /// (`--agent` switches away from the built-in coding agent to a custom
    /// specialist profile with a restricted toolset).
    pub fn has_sys(self) -> bool {
        matches!(self, Kind::Claude | Kind::Omp)
    }

    /// The flag giving one run its per-agent system prompt, for harnesses
    /// that take one.
    pub fn sys_flag(self, sys: &str) -> Option<String> {
        match self {
            Kind::Claude | Kind::Omp => {
                let escaped = sys.replace('\'', r"'\''");
                Some(format!("--system-prompt '{escaped}'"))
            }
            Kind::Agy => None,
        }
    }

    /// The flag wiring warren's lifecycle hooks, for harnesses that have them.
    ///
    /// Claude Code gets a generated settings file whose hooks push exact
    /// states (and the session id). Antigravity gets a session-only
    /// `--settings` file whose `title` command receives live state,
    /// `conversation_id`, and `conversation_title` on stdin. OMP has no
    /// equivalent, so its agents fall back to the output-activity heuristic.
    pub fn hooks_flag(self, extra: Option<&str>) -> Result<Option<String>> {
        match self {
            Kind::Claude => {
                let path = crate::hooks::ensure_hooks_json()?;
                Ok(Some(format!("--settings '{}'", path.display())))
            }
            Kind::Agy => {
                let autonomous = extra.is_some_and(|e| {
                    e.split_whitespace()
                        .any(|t| t == "--dangerously-skip-permissions")
                });
                let path = crate::hooks::ensure_agy_settings_json(autonomous)?;
                Ok(Some(format!("--settings '{}'", path.display())))
            }
            Kind::Omp => Ok(None),
        }
    }

    /// Every resumable session of this harness, newest first.
    pub fn sessions(self) -> Vec<Session> {
        match self {
            Kind::Claude => crate::sessions::scan_claude(&crate::paths::claude_projects()),
            Kind::Omp => crate::sessions::scan_omp(&crate::paths::omp_sessions()),
            Kind::Agy => crate::sessions::scan_agy(&crate::paths::agy_home()),
        }
    }

    /// The session a *running* agent is in, given the name of the tty its
    /// harness is talking to — what sleep mode needs to resume it later.
    ///
    /// Claude Code and Antigravity report their session ids through warren's
    /// hooks, so there is nothing to look up here. OMP writes
    /// `<cwd>\n<session file>\n…` to `~/.omp/agent/terminal-sessions/<tty>`
    /// at startup, and the daemon owns the pty, so the row keyed by its own
    /// slave tty is the agent's own.
    pub fn live_session(
        self,
        tty: &str,
        cwd: &str,
        started: std::time::SystemTime,
    ) -> Option<String> {
        match self {
            Kind::Claude | Kind::Agy => None,
            Kind::Omp => {
                let path = crate::paths::omp_terminal_sessions().join(tty);
                let meta = std::fs::metadata(&path).ok()?;
                if meta.modified().ok()? < started {
                    return None; // a previous tenant of this tty
                }
                let body = std::fs::read_to_string(&path).ok()?;
                let mut lines = body.lines();
                if lines.next()? != cwd {
                    return None; // this tty is someone else's agent
                }
                let session = lines.next()?.trim();
                (!session.is_empty()).then(|| session.to_string())
            }
        }
    }

    /// Does this harness name its session on the tty (rather than telling
    /// the daemon directly)? Only then is there anything to look up.
    pub fn learns_session_from_tty(self) -> bool {
        matches!(self, Kind::Omp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parsing_is_forgiving_but_closed() {
        assert_eq!(Kind::parse("claude"), Some(Kind::Claude));
        assert_eq!(Kind::parse(" OMP "), Some(Kind::Omp));
        assert_eq!(Kind::parse("agy"), Some(Kind::Agy));
        assert_eq!(Kind::parse("Antigravity"), Some(Kind::Agy));
        assert_eq!(Kind::parse("codex"), None);
        assert_eq!(Kind::default(), Kind::Claude);
    }

    #[test]
    fn each_harness_spells_its_modes_its_own_way() {
        assert_eq!(Kind::Claude.base_command("new", None), "claude");
        assert_eq!(Kind::Omp.base_command("new", None), "omp");
        assert_eq!(Kind::Agy.base_command("new", None), "agy");
        assert_eq!(Kind::Omp.base_command("continue", None), "omp --continue");
        assert_eq!(Kind::Agy.base_command("continue", None), "agy --continue");
        assert_eq!(
            Kind::Omp.base_command("resume", Some("abc")),
            "omp --resume abc"
        );
        assert_eq!(
            Kind::Agy.base_command("resume", Some("abc")),
            "agy --conversation abc"
        );
        // No id falls through to the harness's own picker.
        assert_eq!(Kind::Claude.base_command("resume", None), "claude --resume");
        assert_eq!(Kind::Agy.base_command("resume", None), "agy -i /resume");
    }

    #[test]
    fn a_fork_resumes_into_a_new_session_where_the_harness_can() {
        assert_eq!(
            Kind::Claude.base_command("fork", Some("abc")),
            "claude --resume abc --fork-session"
        );
        assert!(Kind::Claude.can_fork());
        // OMP and Antigravity have no CLI fork flag, so nothing may offer one.
        assert!(!Kind::Omp.can_fork());
        assert!(!Kind::Agy.can_fork());
    }

    #[test]
    fn only_claude_and_omp_take_a_cli_system_prompt() {
        assert!(Kind::Claude.has_sys());
        assert!(Kind::Omp.has_sys());
        assert!(!Kind::Agy.has_sys());
        assert_eq!(
            Kind::Claude.sys_flag("be brief").as_deref(),
            Some("--system-prompt 'be brief'")
        );
        assert_eq!(Kind::Agy.sys_flag("be brief"), None);
    }

    #[test]
    fn omp_has_no_hooks_to_wire() {
        assert!(Kind::Omp.hooks_flag(None).unwrap().is_none());
    }

    #[test]
    fn a_live_omp_session_is_read_from_its_own_tty_row() {
        let dir = std::env::temp_dir().join(format!("warren-kind-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let rows = dir.join("agent").join("terminal-sessions");
        std::fs::create_dir_all(&rows).unwrap();
        // SAFETY: single-threaded test, before any daemon reads the env.
        unsafe { std::env::set_var("WARREN_OMP_HOME", &dir) };

        let before = std::time::SystemTime::now() - std::time::Duration::from_secs(60);
        std::fs::write(rows.join("ttys004"), "/w/repo\n/s/2026_abc.jsonl\nfresh\n").unwrap();
        assert_eq!(
            Kind::Omp
                .live_session("ttys004", "/w/repo", before)
                .as_deref(),
            Some("/s/2026_abc.jsonl")
        );
        // Someone else's directory on a recycled tty.
        assert_eq!(Kind::Omp.live_session("ttys004", "/w/other", before), None);
        // Written before this agent started: the tty's previous tenant.
        let after = std::time::SystemTime::now() + std::time::Duration::from_secs(60);
        assert_eq!(Kind::Omp.live_session("ttys004", "/w/repo", after), None);
        // Claude and Antigravity report their sessions through hooks instead.
        assert_eq!(
            Kind::Claude.live_session("ttys004", "/w/repo", before),
            None
        );
        assert_eq!(Kind::Agy.live_session("ttys004", "/w/repo", before), None);

        unsafe { std::env::remove_var("WARREN_OMP_HOME") };
        let _ = std::fs::remove_dir_all(&dir);
    }
}
