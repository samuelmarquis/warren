//! `warren cycle [NAME...] [--renew]`: restart agents once each is at rest,
//! so they pick up a new harness setting, chat tools, or (with `--renew`) a
//! new warren binary.
//!
//! "At rest" is more than "not mid-turn". An agent whose turn has ended can
//! still be waiting on work it started in the background, and sleeping it
//! would kill that work:
//! - a background shell (a Bash tool command, a Monitor loop) is a shell
//!   process somewhere under the harness;
//! - a Claude subagent runs inside the harness, but writes its transcript
//!   under `<session>/subagents/` as it goes.
//! An agent counts as at rest when none of that is true for SETTLE straight,
//! which also covers the moment between a turn ending and the background
//! result that starts the next one.

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Result, bail};

use crate::cli::{agent_busy, daemon_pid, query_agent};

const SETTLE: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_secs(2);
/// A subagent transcript written to this recently means the subagent is live.
const SUBAGENT_QUIET: Duration = Duration::from_secs(90);
const TRIES: u32 = 3;

pub fn cmd_cycle(args: &[String]) -> Result<()> {
    let renew = args.iter().any(|a| a == "--renew");
    if let Some(bad) = args.iter().find(|a| a.starts_with("--") && *a != "--renew") {
        bail!("usage: warren cycle [NAME...] [--renew] (unknown option {bad})");
    }
    let named: Vec<String> = args
        .iter()
        .filter(|a| !a.starts_with("--"))
        .cloned()
        .collect();
    for n in &named {
        if !crate::names::valid(n) {
            bail!("'{n}' is not an agent name");
        }
    }
    // Everyone but the agent running this, which can't wait for itself.
    let me = std::env::var("WARREN_AGENT").ok();
    let mut pending: Vec<String> = if named.is_empty() {
        crate::cli::discover()
            .into_iter()
            .map(|a| a.meta.name)
            .filter(|n| Some(n) != me.as_ref())
            .collect()
    } else {
        named
    };
    if pending.is_empty() {
        println!("warren: no agents to cycle");
        return Ok(());
    }

    let mut calm_since: HashMap<String, Instant> = HashMap::new();
    let mut tries: HashMap<String, u32> = HashMap::new();
    let mut last_report = String::new();
    loop {
        let procs = process_table();
        let mut waiting: BTreeMap<String, String> = BTreeMap::new();
        for name in pending.clone() {
            let done = match at_rest(&name, renew, &procs) {
                Rest::Gone => {
                    println!("warren: '{name}' went away");
                    true
                }
                Rest::Skip(why) => {
                    println!("warren: skipping '{name}': {why}");
                    true
                }
                Rest::Busy(why) => {
                    calm_since.remove(&name);
                    waiting.insert(name.clone(), why);
                    false
                }
                Rest::Calm => {
                    let since = *calm_since.entry(name.clone()).or_insert_with(Instant::now);
                    if since.elapsed() < SETTLE {
                        waiting.insert(name.clone(), "settling".into());
                        false
                    } else {
                        match restart(&name, renew) {
                            Ok(()) => true,
                            Err(e) => {
                                let n = tries.entry(name.clone()).or_default();
                                *n += 1;
                                calm_since.remove(&name);
                                if *n >= TRIES {
                                    println!("warren: giving up on '{name}': {e:#}");
                                    true
                                } else {
                                    waiting.insert(name.clone(), format!("retrying: {e:#}"));
                                    false
                                }
                            }
                        }
                    }
                }
            };
            if done {
                pending.retain(|p| p != &name);
            }
        }
        if pending.is_empty() {
            break;
        }
        let report = waiting
            .iter()
            .filter(|(_, why)| why.as_str() != "settling")
            .map(|(n, why)| format!("{n} ({why})"))
            .collect::<Vec<_>>()
            .join(", ");
        if !report.is_empty() && report != last_report {
            println!("warren: waiting on {report}");
            last_report = report;
        }
        std::thread::sleep(POLL);
    }
    Ok(())
}

enum Rest {
    Calm,
    Busy(String),
    Skip(String),
    Gone,
}

fn at_rest(name: &str, renew: bool, procs: &[Proc]) -> Rest {
    let sock = crate::paths::sock_path(name);
    let Some((_, state)) = query_agent(&sock) else {
        return Rest::Gone;
    };
    if state.power.is_down() {
        // A sleeping agent loads everything fresh when it wakes anyway; only
        // a new binary needs its daemon replaced.
        return if renew {
            Rest::Calm
        } else {
            Rest::Skip("asleep; it picks the change up when it wakes".into())
        };
    }
    if !state.resumable {
        return Rest::Skip("no session id yet, so nothing to come back on".into());
    }
    if agent_busy(&state) {
        return Rest::Busy("mid-turn".into());
    }
    let Some(daemon) = daemon_pid(&sock) else {
        return Rest::Gone;
    };
    let shells = background_shells(daemon, procs);
    if shells > 0 {
        return Rest::Busy(format!(
            "{shells} background shell{}",
            if shells == 1 { "" } else { "s" }
        ));
    }
    if let Some(sid) = &state.session {
        if subagent_live(sid) {
            return Rest::Busy("subagent".into());
        }
    }
    Rest::Calm
}

fn restart(name: &str, renew: bool) -> Result<()> {
    let args = [name.to_string()];
    if renew {
        crate::cli::cmd_renew(&args)
    } else {
        crate::cli::cmd_power(&args, false)?;
        crate::cli::cmd_power(&args, true)
    }
}

#[derive(Debug, Clone)]
pub struct Proc {
    pub pid: i32,
    pub ppid: i32,
    pub comm: String,
}

fn process_table() -> Vec<Proc> {
    let Ok(out) = std::process::Command::new("ps")
        .args(["-A", "-o", "pid=,ppid=,comm="])
        .output()
    else {
        return Vec::new();
    };
    parse_ps(&String::from_utf8_lossy(&out.stdout))
}

pub fn parse_ps(text: &str) -> Vec<Proc> {
    text.lines()
        .filter_map(|line| {
            let mut it = line.split_whitespace();
            let pid = it.next()?.parse().ok()?;
            let ppid = it.next()?.parse().ok()?;
            let comm = it.collect::<Vec<_>>().join(" ");
            Some(Proc { pid, ppid, comm })
        })
        .collect()
}

/// Shells running below the harness: the daemon's children are the harness
/// (or a wrapper around it), so only their descendants count. MCP servers
/// started through `sh -c exec ...` have become their program by then.
pub fn background_shells(daemon: i32, procs: &[Proc]) -> usize {
    let mut children: HashMap<i32, Vec<&Proc>> = HashMap::new();
    for p in procs {
        children.entry(p.ppid).or_default().push(p);
    }
    let mut stack: Vec<i32> = children
        .get(&daemon)
        .map(|c| c.iter().map(|p| p.pid).collect())
        .unwrap_or_default();
    let mut count = 0;
    while let Some(pid) = stack.pop() {
        for kid in children.get(&pid).into_iter().flatten() {
            if is_shell(&kid.comm) {
                count += 1;
            } else {
                stack.push(kid.pid);
            }
        }
    }
    count
}

fn is_shell(comm: &str) -> bool {
    let base = comm
        .rsplit('/')
        .next()
        .unwrap_or(comm)
        .trim_start_matches('-');
    matches!(base, "sh" | "bash" | "zsh" | "dash" | "fish" | "ksh")
}

fn subagent_live(session: &str) -> bool {
    let root = std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".claude"))
        .join("projects");
    let Ok(projects) = std::fs::read_dir(root) else {
        return false;
    };
    let now = SystemTime::now();
    for project in projects.flatten() {
        let Ok(files) = std::fs::read_dir(project.path().join(session).join("subagents")) else {
            continue;
        };
        for f in files.flatten() {
            let fresh = f
                .metadata()
                .and_then(|m| m.modified())
                .ok()
                .and_then(|t| now.duration_since(t).ok())
                .is_some_and(|age| age < SUBAGENT_QUIET);
            if fresh && f.path().extension().is_some_and(|e| e == "jsonl") {
                return true;
            }
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shells_under_the_harness_count_and_the_harness_does_not() {
        let ps = "\
  10     1 /usr/local/bin/warren
  11    10 claude
  12    11 /bin/zsh
  13    12 sleep
  14    11 /usr/bin/python3
  15    14 /bin/sh
  20     1 /bin/zsh
";
        let procs = parse_ps(ps);
        // 12 (a Bash tool shell) and 15 (a shell under an MCP server) count;
        // 20 belongs to someone else.
        assert_eq!(background_shells(10, &procs), 2);
    }

    #[test]
    fn a_harness_started_through_a_shell_is_not_its_own_background_work() {
        let procs = parse_ps("  10 1 warren\n  11 10 /bin/sh\n  12 11 omp\n  13 12 node\n");
        assert_eq!(background_shells(10, &procs), 0);
    }
}
