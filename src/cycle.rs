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
//!
//! An agent counts as at rest when none of that is true for SETTLE straight,
//! which also covers the moment between a turn ending and the background
//! result that starts the next one.

use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};

use crate::cli::{agent_busy, daemon_pid, query_agent};

const SETTLE: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_secs(2);
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
        let mut waiting: BTreeMap<String, String> = BTreeMap::new();
        for name in pending.clone() {
            let done = match at_rest(&name, renew) {
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

fn at_rest(name: &str, renew: bool) -> Rest {
    let sock = crate::paths::sock_path(name);
    let Some((_, state)) = query_agent(&sock) else {
        return Rest::Gone;
    };
    if renew && state.daemon_started > 0 && state.daemon_started >= binary_built() {
        return Rest::Skip("already on this warren".into());
    }
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
    match crate::procs::background_work(daemon, state.session.as_deref()) {
        Some(why) => Rest::Busy(why),
        None => Rest::Calm,
    }
}

/// When the warren binary running this was last replaced, unix seconds.
fn binary_built() -> u64 {
    std::env::current_exe()
        .and_then(std::fs::metadata)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(u64::MAX, |d| d.as_secs())
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
