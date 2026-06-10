//! List Claude Code sessions across ALL projects, most-recent first.
//!
//! Reads ~/.claude/projects/*/*.jsonl directly (rather than relying on
//! `claude --resume`'s cwd-scoped picker, whose behavior could change).
//! `warren sessions` emits one session per line as TSV:
//!
//!     <sessionId>\t<mtime_epoch>\t<cwd>\t<title>
//!
//! The session id is the file stem; cwd and title (aiTitle, falling back to
//! the first user message) are read from the records. Lines are pre-filtered
//! by a cheap substring test so large assistant records are not JSON-parsed.
//! Semantics ported verbatim from v0's bin/warren-sessions (Python).

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::time::UNIX_EPOCH;

use anyhow::Result;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct Session {
    pub id: String,
    pub mtime: f64,
    pub cwd: String,
    pub title: String,
}

pub fn cmd_sessions() -> Result<()> {
    let sessions = scan(&crate::paths::claude_projects());
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for s in &sessions {
        writeln!(out, "{}\t{}\t{}\t{}", s.id, s.mtime as i64, s.cwd, s.title)?;
    }
    Ok(())
}

pub fn scan(root: &Path) -> Vec<Session> {
    let mut rows: Vec<Session> = Vec::new();
    let Ok(projects) = std::fs::read_dir(root) else {
        return rows;
    };
    for project in projects.flatten() {
        let Ok(files) = std::fs::read_dir(project.path()) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let Ok(meta) = file.metadata() else { continue };
            let Ok(modified) = meta.modified() else { continue };
            let mtime = modified
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0);
            if let Some((cwd, label)) = read_session_file(&path) {
                rows.push(Session { id: stem.to_string(), mtime, cwd, title: label });
            }
        }
    }
    // Python sorts the (mtime, sid, cwd, label) tuple descending.
    rows.sort_by(|a, b| {
        b.mtime
            .partial_cmp(&a.mtime)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.id.cmp(&a.id))
            .then_with(|| b.cwd.cmp(&a.cwd))
            .then_with(|| b.title.cmp(&a.title))
    });
    rows
}

/// Returns (cwd-or-"?", display label). None only if the file can't be opened.
fn read_session_file(path: &Path) -> Option<(String, String)> {
    let file = std::fs::File::open(path).ok()?;
    let mut reader = BufReader::new(file);

    let mut cwd = String::new();
    let mut title = String::new();
    let mut first_user = String::new();

    let mut raw = Vec::new();
    loop {
        raw.clear();
        match reader.read_until(b'\n', &mut raw) {
            Ok(0) => break,
            Ok(_) => {}
            Err(_) => break,
        }
        let ln = String::from_utf8_lossy(&raw);

        let want_title = ln.contains("\"aiTitle\"");
        let want_cwd = cwd.is_empty() && ln.contains("\"cwd\"");
        let want_user = first_user.is_empty()
            && title.is_empty()
            && (ln.contains("\"type\":\"user\"") || ln.contains("\"type\": \"user\""));
        if !(want_title || want_cwd || want_user) {
            continue;
        }
        let Ok(d) = serde_json::from_str::<Value>(&ln) else {
            continue;
        };
        if want_cwd {
            if let Some(c) = d.get("cwd").and_then(Value::as_str) {
                if !c.is_empty() {
                    cwd = c.to_string();
                }
            }
        }
        if want_title {
            if let Some(t) = d.get("aiTitle").and_then(Value::as_str) {
                if !t.is_empty() {
                    title = t.to_string();
                }
            }
        }
        if want_user
            && first_user.is_empty()
            && d.get("type").and_then(Value::as_str) == Some("user")
        {
            if let Some(content) = d.get("message").and_then(|m| m.get("content")) {
                match content {
                    Value::String(s) => first_user = s.clone(),
                    Value::Array(parts) => {
                        for part in parts {
                            if part.get("type").and_then(Value::as_str) == Some("text") {
                                first_user = part
                                    .get("text")
                                    .and_then(Value::as_str)
                                    .unwrap_or("")
                                    .to_string();
                                break;
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
    }

    let label = if !title.is_empty() {
        title
    } else if !first_user.is_empty() {
        first_user
    } else {
        "(untitled)".to_string()
    };
    // Collapse whitespace runs, cap at 80 chars (Python: " ".join(label.split())[:80]).
    let label: String = label
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(80)
        .collect();
    let cwd = if cwd.is_empty() { "?".to_string() } else { cwd };
    Some((cwd, label))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fixture_root(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("warren-sessions-test-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("proj")).unwrap();
        dir
    }

    #[test]
    fn ai_title_wins_and_is_squashed() {
        let root = fixture_root("title");
        fs::write(
            root.join("proj/abc-123.jsonl"),
            concat!(
                r#"{"type":"user","cwd":"/tmp/x","message":{"content":"hello   there"}}"#,
                "\n",
                r#"{"aiTitle":"Fix   the\tbug"}"#,
                "\n",
            ),
        )
        .unwrap();
        let s = scan(&root);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].id, "abc-123");
        assert_eq!(s[0].cwd, "/tmp/x");
        assert_eq!(s[0].title, "Fix the bug");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn falls_back_to_first_user_text_part() {
        let root = fixture_root("user");
        fs::write(
            root.join("proj/s1.jsonl"),
            concat!(
                r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"x"},{"type":"text","text":"do the thing"}]}}"#,
                "\n",
            ),
        )
        .unwrap();
        let s = scan(&root);
        assert_eq!(s[0].title, "do the thing");
        assert_eq!(s[0].cwd, "?");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn untitled_and_sorting_newest_first() {
        let root = fixture_root("sort");
        let old = root.join("proj/old.jsonl");
        let new = root.join("proj/new.jsonl");
        fs::write(&old, "{}\n").unwrap();
        std::thread::sleep(std::time::Duration::from_millis(20));
        fs::write(&new, "not json\n").unwrap();
        let s = scan(&root);
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].id, "new");
        assert_eq!(s[1].id, "old");
        assert_eq!(s[0].title, "(untitled)");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn label_capped_at_80_chars() {
        let root = fixture_root("cap");
        let long = "y".repeat(200);
        fs::write(
            root.join("proj/cap.jsonl"),
            format!("{{\"type\":\"user\",\"message\":{{\"content\":\"{long}\"}}}}\n"),
        )
        .unwrap();
        let s = scan(&root);
        assert_eq!(s[0].title.chars().count(), 80);
        let _ = fs::remove_dir_all(&root);
    }
}
