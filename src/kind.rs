//! The agent harnesses warren knows how to run.
//!
//! Everything harness-specific lives here: the command line for one run,
//! whether lifecycle state can reach the daemon, where resumable sessions are
//! kept, and how a live agent's session is identified so sleep can resume it.
//! A third harness should mean a new variant and those four answers — nothing
//! in the daemon, the dashboard or the protocol distinguishes them, and (by
//! request) neither does the sidebar.

use anyhow::Result;

use crate::sessions::Session;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Kind {
    #[default]
    Claude,
    Omp,
}

impl Kind {
    pub fn parse(s: &str) -> Option<Kind> {
        match s.trim().to_ascii_lowercase().as_str() {
            "claude" => Some(Kind::Claude),
            "omp" => Some(Kind::Omp),
            _ => None,
        }
    }

    /// Also the program name — both harnesses are on PATH under their label.
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Claude => "claude",
            Kind::Omp => "omp",
        }
    }

    /// The base command for one run: the program plus its mode flags. The two
    /// harnesses spell `--continue`, `--resume` and `--system-prompt`
    /// identically, so only the program name and the hooks differ.
    pub fn base_command(self, mode: &str, sid: Option<&str>) -> String {
        let bin = self.as_str();
        match mode {
            "resume" => match sid {
                Some(sid) => format!("{bin} --resume {sid}"),
                // No id: the harness's own picker beats a wrong guess.
                None => format!("{bin} --resume"),
            },
            // A fork reads the conversation and then writes somewhere else:
            // same history, new session id, so the two never tread on each
            // other's transcript. Only the first run forks — every wake after
            // it resumes the id the fork reported.
            "fork" => match sid {
                Some(sid) => format!("{bin} --resume {sid} --fork-session"),
                None => format!("{bin} --resume --fork-session"),
            },
            "continue" => format!("{bin} --continue"),
            _ => bin.to_string(),
        }
    }

    /// Can this harness open a conversation *twice* — take its history and
    /// carry on separately? Claude Code has `--fork-session`; OMP has no
    /// equivalent, and resuming it twice means two processes appending to one
    /// transcript, so warren does not offer what it cannot deliver.
    pub fn can_fork(self) -> bool {
        matches!(self, Kind::Claude)
    }

    /// The flag wiring warren's lifecycle hooks, for harnesses that have them.
    ///
    /// Claude Code gets a generated settings file whose hooks push exact
    /// states (and the session id). OMP has no equivalent, so its agents fall
    /// back to the output-activity heuristic every viewer already computes:
    /// they read working and idle correctly, but never `!` for a permission
    /// prompt, because nothing tells us about one.
    pub fn hooks_flag(self) -> Result<Option<String>> {
        match self {
            Kind::Claude => {
                let path = crate::hooks::ensure_hooks_json()?;
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
        }
    }

    /// The session a *running* agent is in, given the name of the tty its
    /// harness is talking to — what sleep mode needs to resume it later.
    ///
    /// Claude Code reports its session id through the hooks, so there is
    /// nothing to look up here. OMP writes `<cwd>\n<session file>\n…` to
    /// `~/.omp/agent/terminal-sessions/<tty>` at startup, and the daemon owns
    /// the pty, so the row keyed by its own slave tty is the agent's own.
    /// Tty names are recycled, so a stale row is rejected on either count:
    /// written before this process started, or for another directory.
    pub fn live_session(self, tty: &str, cwd: &str, started: std::time::SystemTime) -> Option<String> {
        match self {
            Kind::Claude => None,
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
        assert_eq!(Kind::parse("codex"), None);
        assert_eq!(Kind::default(), Kind::Claude);
    }

    #[test]
    fn both_harnesses_spell_the_modes_the_same_way() {
        assert_eq!(Kind::Claude.base_command("new", None), "claude");
        assert_eq!(Kind::Omp.base_command("new", None), "omp");
        assert_eq!(Kind::Omp.base_command("continue", None), "omp --continue");
        assert_eq!(Kind::Omp.base_command("resume", Some("abc")), "omp --resume abc");
        // No id falls through to the harness's own picker.
        assert_eq!(Kind::Claude.base_command("resume", None), "claude --resume");
    }

    #[test]
    fn a_fork_resumes_into_a_new_session_where_the_harness_can() {
        assert_eq!(
            Kind::Claude.base_command("fork", Some("abc")),
            "claude --resume abc --fork-session"
        );
        assert!(Kind::Claude.can_fork());
        // OMP has no fork flag, so nothing may offer it one.
        assert!(!Kind::Omp.can_fork());
    }

    #[test]
    fn only_claude_has_hooks_to_wire() {
        assert!(Kind::Omp.hooks_flag().unwrap().is_none());
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
            Kind::Omp.live_session("ttys004", "/w/repo", before).as_deref(),
            Some("/s/2026_abc.jsonl")
        );
        // Someone else's directory on a recycled tty.
        assert_eq!(Kind::Omp.live_session("ttys004", "/w/other", before), None);
        // Written before this agent started: the tty's previous tenant.
        let after = std::time::SystemTime::now() + std::time::Duration::from_secs(60);
        assert_eq!(Kind::Omp.live_session("ttys004", "/w/repo", after), None);
        // Claude reports its session through the hooks instead.
        assert_eq!(Kind::Claude.live_session("ttys004", "/w/repo", before), None);

        unsafe { std::env::remove_var("WARREN_OMP_HOME") };
        let _ = std::fs::remove_dir_all(&dir);
    }
}
