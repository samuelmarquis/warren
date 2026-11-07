//! Work an agent is still doing after its turn: shells under its harness
//! (background commands, monitor loops) and live Claude subagents. Shared by
//! `warren cycle`, which waits for it to end, and the daemon, which reports it
//! so the sidebar can show it.

use std::path::{Path, PathBuf};
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

/// Whether a Claude session's transcript says its last turn is over: an
/// end-of-turn record (the Stop hook's summary, or the turn's duration)
/// after the last message. A turn stopped on a permission prompt or a
/// question has a tool call with nothing after it, and is not over. No
/// transcript, or one that can't be read, is not evidence either way, so
/// it says no.
pub fn turn_ended(session: &str) -> bool {
    use std::io::{Read, Seek, SeekFrom};
    let Some(path) = transcript(session) else {
        return false;
    };
    let Ok(mut f) = std::fs::File::open(path) else {
        return false;
    };
    let len = f.metadata().map(|m| m.len()).unwrap_or(0);
    let start = len.saturating_sub(256 * 1024);
    let mut tail = Vec::new();
    if f.seek(SeekFrom::Start(start)).is_err() || f.read_to_end(&mut tail).is_err() {
        return false;
    }
    let text = String::from_utf8_lossy(&tail);
    let mut lines = text.lines();
    if start > 0 {
        lines.next(); // cut mid-record
    }
    for line in lines.collect::<Vec<_>>().into_iter().rev() {
        let Ok(rec) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        match (
            rec.get("type").and_then(|v| v.as_str()),
            rec.get("subtype").and_then(|v| v.as_str()),
        ) {
            (Some("system"), Some("stop_hook_summary" | "turn_duration")) => return true,
            (Some("user" | "assistant"), _) => return false,
            _ => continue,
        }
    }
    false
}

fn claude_projects() -> PathBuf {
    std::env::var_os("CLAUDE_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(".claude"))
        .join("projects")
}

pub(crate) fn transcript(session: &str) -> Option<PathBuf> {
    std::fs::read_dir(claude_projects())
        .ok()?
        .flatten()
        .map(|p| p.path().join(format!("{session}.jsonl")))
        .find(|p| p.is_file())
}

/// The folder Claude Code files a directory's conversations and memory under:
/// the path with every character but a letter or digit made a `-`.
fn project_key(cwd: &Path) -> String {
    cwd.to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Move a conversation's transcript (and its folder of subagents and tool
/// results) from wherever it is filed to `to`'s project folder, and copy
/// `from`'s memories into its memory folder (see `merge_memories`). Says what it did.
pub(crate) fn move_conversation(session: &str, from: &Path, to: &Path) -> anyhow::Result<String> {
    move_conversation_in(&claude_projects(), session, from, to)
}

fn move_conversation_in(
    projects: &Path,
    session: &str,
    from: &Path,
    to: &Path,
) -> anyhow::Result<String> {
    use anyhow::{Context, bail};
    let src = std::fs::read_dir(projects)
        .into_iter()
        .flatten()
        .flatten()
        .map(|p| p.path().join(format!("{session}.jsonl")))
        .find(|p| p.is_file())
        .with_context(|| format!("no transcript for session {session}"))?;
    let project_dir = |d: &Path| projects.join(project_key(d));
    let dst_dir = project_dir(to);
    std::fs::create_dir_all(&dst_dir)?;
    let dst = dst_dir.join(format!("{session}.jsonl"));
    let mut said = Vec::new();
    if src != dst {
        if dst.exists() {
            bail!("{} already exists", dst.display());
        }
        std::fs::rename(&src, &dst).with_context(|| format!("moving {}", src.display()))?;
        let side = src.with_extension("");
        if side.is_dir() {
            std::fs::rename(&side, dst_dir.join(session))?;
        }
        said.push(format!("moved its conversation to {}", dst_dir.display()));
    }
    // The old directory's memories join the new folder's once, with the
    // conversation: a file already there under its name, with other content,
    // comes in renamed, and the index gains the lines it lacks.
    let (old_mem, new_mem) = (project_dir(from).join("memory"), dst_dir.join("memory"));
    if said.len() == 1 && old_mem.is_dir() && old_mem != new_mem {
        let tag = from
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        let n = merge_memories(&old_mem, &new_mem, &tag)?;
        if n > 0 {
            said.push(format!("merged {n} of {}'s memories", from.display()));
        }
    }
    Ok(if said.is_empty() {
        "nothing to move".into()
    } else {
        said.join("; ")
    })
}

/// Bring `old`'s memory files into `new`, keeping everything already in `new`.
/// A name clash with different content comes in as `<stem>-<tag>.<ext>`;
/// MEMORY.md, the index, gains the lines it lacks, with links to renamed files
/// following them. Returns how many files or index lines came in.
fn merge_memories(old: &Path, new: &Path, tag: &str) -> std::io::Result<usize> {
    std::fs::create_dir_all(new)?;
    // A folder made as a copy of `old`, and maybe pruned since, already has
    // what it wants from it: merging would bring back what was pruned.
    let mut notes = std::fs::read_dir(new)?
        .flatten()
        .filter(|e| e.file_name() != "MEMORY.md" && e.path().is_file())
        .peekable();
    if notes.peek().is_some()
        && notes
            .all(|e| std::fs::read(e.path()).ok() == std::fs::read(old.join(e.file_name())).ok())
    {
        return Ok(0);
    }
    let (mut count, mut renamed) = (0, Vec::new());
    for e in std::fs::read_dir(old)?.flatten() {
        let (p, name) = (e.path(), e.file_name().to_string_lossy().into_owned());
        if name == "MEMORY.md" || !p.is_file() {
            continue;
        }
        let q = new.join(&name);
        if !q.exists() {
            std::fs::copy(&p, &q)?;
            count += 1;
        } else if std::fs::read(&p)? != std::fs::read(&q)? {
            let path = Path::new(&name);
            let stem = path
                .file_stem()
                .map_or(name.clone(), |s| s.to_string_lossy().into_owned());
            let ext = path
                .extension()
                .map(|x| format!(".{}", x.to_string_lossy()))
                .unwrap_or_default();
            let mut alt = format!("{stem}-{tag}{ext}");
            let mut i = 2;
            while new.join(&alt).exists() {
                alt = format!("{stem}-{tag}-{i}{ext}");
                i += 1;
            }
            std::fs::copy(&p, new.join(&alt))?;
            renamed.push((name, alt));
            count += 1;
        }
    }
    let Ok(index) = std::fs::read_to_string(old.join("MEMORY.md")) else {
        return Ok(count);
    };
    let have = std::fs::read_to_string(new.join("MEMORY.md")).unwrap_or_default();
    let known: std::collections::HashSet<&str> = have.lines().collect();
    let mut add = String::new();
    for line in index.lines().filter(|l| !l.trim().is_empty()) {
        let mut line = line.to_string();
        for (from, to) in &renamed {
            line = line.replace(&format!("({from})"), &format!("({to})"));
        }
        if !known.contains(line.as_str()) {
            add.push_str(&line);
            add.push('\n');
            count += 1;
        }
    }
    if !add.is_empty() {
        let mut out = have;
        if !out.is_empty() && !out.ends_with('\n') {
            out.push('\n');
        }
        out.push_str(&add);
        std::fs::write(new.join("MEMORY.md"), out)?;
    }
    Ok(count)
}

/// Claude Code writes each subagent's transcript under
/// `projects/<project>/<session>/subagents/` as it goes.
fn subagent_live(session: &str) -> bool {
    let Ok(projects) = std::fs::read_dir(claude_projects()) else {
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

    #[test]
    fn a_conversation_moves_with_its_folder_and_the_old_memories_merge_in() {
        let tmp = std::env::temp_dir().join(format!("warren-move-{}", std::process::id()));
        let (from, to) = (Path::new("/r/open-comp"), Path::new("/r/agents/my.celium"));
        assert_eq!(project_key(to), "-r-agents-my-celium");
        let (old, new) = (tmp.join("-r-open-comp"), tmp.join("-r-agents-my-celium"));
        std::fs::create_dir_all(old.join("s1/subagents")).unwrap();
        std::fs::create_dir_all(old.join("memory")).unwrap();
        std::fs::create_dir_all(new.join("memory")).unwrap();
        std::fs::write(old.join("s1.jsonl"), "{}").unwrap();
        let w = |d: &Path, f: &str, t: &str| std::fs::write(d.join("memory").join(f), t).unwrap();
        w(
            &old,
            "MEMORY.md",
            "- [Farm](farm.md) — x\n- [Mine](mine.md) — y\n- [Same](same.md) — z\n",
        );
        w(&old, "farm.md", "comp's farm note");
        w(&old, "mine.md", "only comp has this");
        w(&old, "same.md", "shared");
        // The pool copied in already: its own farm.md, and same.md alike.
        w(
            &new,
            "MEMORY.md",
            "- [Farm](farm.md) — x\n- [Same](same.md) — z\n",
        );
        w(&new, "farm.md", "the pool's farm note");
        w(&new, "same.md", "shared");
        let said = move_conversation_in(&tmp, "s1", from, to).unwrap();
        assert!(
            new.join("s1.jsonl").is_file() && new.join("s1/subagents").is_dir(),
            "{said}"
        );
        assert!(!old.join("s1.jsonl").exists() && !old.join("s1").exists());
        let r = |f: &str| std::fs::read_to_string(new.join("memory").join(f)).unwrap();
        assert_eq!(r("farm.md"), "the pool's farm note");
        assert_eq!(r("farm-open-comp.md"), "comp's farm note");
        assert_eq!(r("mine.md"), "only comp has this");
        assert_eq!(
            r("MEMORY.md"),
            "- [Farm](farm.md) — x\n- [Same](same.md) — z\n- [Farm](farm-open-comp.md) — x\n- [Mine](mine.md) — y\n"
        );
        assert!(old.join("memory/farm.md").is_file(), "the old folder stays");
        // A folder copied from the old one and pruned since stays pruned.
        let pool = tmp.join("-r-pool");
        std::fs::create_dir_all(pool.join("memory")).unwrap();
        w(&pool, "MEMORY.md", "- [A](a.md)\n- [B](b.md)\n");
        w(&pool, "a.md", "a");
        w(&pool, "b.md", "b");
        let pruned = tmp.join("pruned");
        std::fs::create_dir_all(&pruned).unwrap();
        std::fs::write(pruned.join("MEMORY.md"), "- [A](a.md)\n").unwrap();
        std::fs::write(pruned.join("a.md"), "a").unwrap();
        assert_eq!(
            merge_memories(&pool.join("memory"), &pruned, "pool").unwrap(),
            0
        );
        assert!(!pruned.join("b.md").exists());
        // Again: nothing left to move, and a pruned folder stays pruned.
        std::fs::remove_file(new.join("memory/mine.md")).unwrap();
        assert_eq!(
            move_conversation_in(&tmp, "s1", from, to).unwrap(),
            "nothing to move"
        );
        assert!(!new.join("memory/mine.md").exists());
        std::fs::remove_dir_all(&tmp).unwrap();
    }

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
