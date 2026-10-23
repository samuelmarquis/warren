//! Work an agent is still doing after its turn: shells under its harness
//! (background commands, monitor loops) and live Claude subagents. Shared by
//! `warren cycle`, which waits for it to end, and the daemon, which reports it
//! so the sidebar can show it.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

/// A subagent transcript written to this recently means the subagent is live.
pub const SUBAGENT_QUIET: Duration = Duration::from_secs(90);

/// What is still running under the daemon `root`, for agent session
/// `session`, as a short reason — or None when there is nothing.
pub fn background_work(root: i32, session: Option<&str>) -> Option<String> {
    let shells = background_shells(root, &children, &name);
    if shells > 0 {
        return Some(format!(
            "{shells} background shell{}",
            if shells == 1 { "" } else { "s" }
        ));
    }
    session
        .filter(|s| subagent_live(s))
        .map(|_| "subagent".to_string())
}

/// Shells below the harness. The daemon's own children are the harness (or a
/// shell wrapped around it), so only their descendants count; an MCP server
/// started through `sh -c exec ...` has become its program by then. A shell's
/// own children are not searched: it is counted once.
pub fn background_shells(
    root: i32,
    children: &dyn Fn(i32) -> Vec<i32>,
    name: &dyn Fn(i32) -> String,
) -> usize {
    let mut stack = children(root);
    let mut count = 0;
    while let Some(pid) = stack.pop() {
        for kid in children(pid) {
            if is_shell(&name(kid)) {
                count += 1;
            } else {
                stack.push(kid);
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

#[cfg(target_os = "macos")]
pub fn children(pid: i32) -> Vec<i32> {
    let mut buf = vec![0 as libc::pid_t; 512];
    let bytes = (buf.len() * std::mem::size_of::<libc::pid_t>()) as libc::c_int;
    // Returns how many pids it wrote.
    let n = unsafe { libc::proc_listchildpids(pid, buf.as_mut_ptr().cast(), bytes) };
    buf.truncate(n.clamp(0, buf.len() as i32) as usize);
    buf.retain(|&p| p > 0);
    buf
}

#[cfg(target_os = "macos")]
pub fn name(pid: i32) -> String {
    let mut buf = [0u8; 256];
    let n = unsafe { libc::proc_name(pid, buf.as_mut_ptr().cast(), buf.len() as u32) };
    String::from_utf8_lossy(&buf[..n.clamp(0, buf.len() as i32) as usize]).into_owned()
}

#[cfg(not(target_os = "macos"))]
pub fn children(pid: i32) -> Vec<i32> {
    // Each thread lists the children it forked; most have one thread.
    let Ok(tasks) = std::fs::read_dir(format!("/proc/{pid}/task")) else {
        return Vec::new();
    };
    tasks
        .flatten()
        .filter_map(|t| std::fs::read_to_string(t.path().join("children")).ok())
        .flat_map(|s| {
            s.split_whitespace()
                .filter_map(|p| p.parse().ok())
                .collect::<Vec<_>>()
        })
        .collect()
}

#[cfg(not(target_os = "macos"))]
pub fn name(pid: i32) -> String {
    std::fs::read_to_string(format!("/proc/{pid}/comm"))
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

/// Claude Code writes each subagent's transcript under
/// `projects/<project>/<session>/subagents/` as it goes.
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
    use std::collections::HashMap;

    fn tree(rows: &[(i32, i32, &str)]) -> (impl Fn(i32) -> Vec<i32>, impl Fn(i32) -> String) {
        let mut kids: HashMap<i32, Vec<i32>> = HashMap::new();
        let mut names: HashMap<i32, String> = HashMap::new();
        for &(pid, ppid, comm) in rows {
            kids.entry(ppid).or_default().push(pid);
            names.insert(pid, comm.to_string());
        }
        (
            move |p| kids.get(&p).cloned().unwrap_or_default(),
            move |p| names.get(&p).cloned().unwrap_or_default(),
        )
    }

    #[test]
    fn shells_under_the_harness_count_and_the_harness_does_not() {
        let (kids, names) = tree(&[
            (11, 10, "claude"),
            (12, 11, "/bin/zsh"),
            (13, 12, "sleep"),
            (14, 11, "python3"),
            (15, 14, "sh"),
            (20, 1, "zsh"),
        ]);
        // 12 (a Bash tool shell) and 15 (a shell under an MCP server) count;
        // 20 belongs to someone else.
        assert_eq!(background_shells(10, &kids, &names), 2);
    }

    #[test]
    fn a_harness_started_through_a_shell_is_not_its_own_background_work() {
        let (kids, names) = tree(&[(11, 10, "sh"), (12, 11, "omp"), (13, 12, "node")]);
        assert_eq!(background_shells(10, &kids, &names), 0);
    }

    #[test]
    fn this_process_is_found_among_its_parents_children() {
        let me = std::process::id() as i32;
        let parent = unsafe { libc::getppid() };
        assert!(children(parent).contains(&me), "{:?}", children(parent));
        assert!(!name(me).is_empty());
    }
}
