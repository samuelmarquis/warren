//! List a harness's resumable sessions across ALL projects, newest first.
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

pub fn cmd_sessions(args: &[String]) -> Result<()> {
    let kind = match args.first() {
        Some(arg) => crate::kind::Kind::parse(arg)
            .ok_or_else(|| anyhow::anyhow!("unknown agent kind '{arg}' (claude or omp)"))?,
        None => crate::kind::Kind::default(),
    };
    let sessions = kind.sessions();
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for s in &sessions {
        // `warren sessions | head` closes the pipe on us; that is the reader
        // being done, not an error worth a nonzero exit.
        if let Err(e) = writeln!(out, "{}\t{}\t{}\t{}", s.id, s.mtime as i64, s.cwd, s.title) {
            if e.kind() == std::io::ErrorKind::BrokenPipe {
                return Ok(());
            }
            return Err(e.into());
        }
    }
    Ok(())
}

/// Order a scan's results the way the pickers want them: newest first, with
/// the remaining fields breaking ties so the listing is stable.
fn newest_first(rows: &mut [Session]) {
    rows.sort_by(|a, b| {
        b.mtime
            .partial_cmp(&a.mtime)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| b.id.cmp(&a.id))
            .then_with(|| b.cwd.cmp(&a.cwd))
            .then_with(|| b.title.cmp(&a.title))
    });
}

/// Squash whitespace and cap at 80 chars, as the pickers display them.
fn tidy(label: &str) -> String {
    label.split_whitespace().collect::<Vec<_>>().join(" ").chars().take(80).collect()
}

/// OMP keeps one directory per project, each holding `<timestamp>_<uuid>.jsonl`.
/// The `session` record carries the id, cwd and title; a padded `title` record
/// sits at the head of the file and is rewritten in place as the title
/// changes, so it wins when it is there. Both are near the top, so a scan
/// stops as soon as it has them rather than reading whole transcripts.
pub fn scan_omp(root: &Path) -> Vec<Session> {
    let mut rows: Vec<Session> = Vec::new();
    let Ok(projects) = std::fs::read_dir(root) else {
        return rows;
    };
    for project in projects.flatten() {
        let Ok(files) = std::fs::read_dir(project.path()) else { continue };
        for file in files.flatten() {
            let path = file.path();
            if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                continue;
            }
            let Ok(meta) = file.metadata() else { continue };
            let mtime = meta
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0);
            let Ok(handle) = std::fs::File::open(&path) else { continue };
            let mut reader = BufReader::new(handle);

            let (mut id, mut cwd, mut title, mut pad_title) =
                (String::new(), String::new(), String::new(), String::new());
            let mut raw = Vec::new();
            for _ in 0..64 {
                raw.clear();
                match reader.read_until(b'\n', &mut raw) {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {}
                }
                let ln = String::from_utf8_lossy(&raw);
                if !(ln.contains("\"session\"") || ln.contains("\"title\"")) {
                    continue;
                }
                let Ok(d) = serde_json::from_str::<Value>(&ln) else { continue };
                match d.get("type").and_then(Value::as_str) {
                    Some("session") => {
                        id = d.get("id").and_then(Value::as_str).unwrap_or("").to_string();
                        cwd = d.get("cwd").and_then(Value::as_str).unwrap_or("").to_string();
                        title = d.get("title").and_then(Value::as_str).unwrap_or("").to_string();
                    }
                    Some("title") => {
                        pad_title =
                            d.get("title").and_then(Value::as_str).unwrap_or("").trim().to_string();
                    }
                    _ => {}
                }
                if !id.is_empty() && !pad_title.is_empty() {
                    break;
                }
            }
            if id.is_empty() {
                continue; // not an OMP session file we understand
            }
            let label = [pad_title, title]
                .into_iter()
                .find(|t| !t.is_empty())
                .unwrap_or_else(|| "(untitled)".to_string());
            rows.push(Session {
                id,
                mtime,
                cwd: if cwd.is_empty() { "?".to_string() } else { cwd },
                title: tidy(&label),
            });
        }
    }
    newest_first(&mut rows);
    rows
}

pub fn scan_claude(root: &Path) -> Vec<Session> {
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
    newest_first(&mut rows);
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
    let label = tidy(&label);
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
        let s = scan_claude(&root);
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
        let s = scan_claude(&root);
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
        let s = scan_claude(&root);
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].id, "new");
        assert_eq!(s[1].id, "old");
        assert_eq!(s[0].title, "(untitled)");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn omp_sessions_read_id_cwd_and_the_freshest_title() {
        let root = fixture_root("omp");
        fs::create_dir_all(root.join("-w-repo")).unwrap();
        fs::write(
            root.join("-w-repo/2026-09-09T22-44-32-066Z_01a08858-abcd.jsonl"),
            concat!(
                // The padded head record, rewritten in place as the title changes.
                r#"{"type":"title","title":"Remove   noreply lines","pad":"      ","v":1}"#,
                "\n",
                r#"{"type":"session","id":"01a08858-abcd","cwd":"/w/repo","title":"stale title"}"#,
                "\n",
                r#"{"type":"model_change","id":"f7f4"}"#,
                "\n",
            ),
        )
        .unwrap();
        // A file with no session record is not an OMP session we understand.
        fs::write(root.join("-w-repo/notes.jsonl"), "{\"type\":\"model_change\"}\n").unwrap();

        let s = scan_omp(&root);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].id, "01a08858-abcd");
        assert_eq!(s[0].cwd, "/w/repo");
        assert_eq!(s[0].title, "Remove noreply lines", "the head title wins, whitespace squashed");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn omp_falls_back_to_the_session_records_title() {
        let root = fixture_root("omp2");
        fs::create_dir_all(root.join("proj")).unwrap();
        fs::write(
            root.join("proj/a_b.jsonl"),
            "{\"type\":\"session\",\"id\":\"b\",\"cwd\":\"/x\"}\n",
        )
        .unwrap();
        let s = scan_omp(&root);
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
        let s = scan_claude(&root);
        assert_eq!(s[0].title.chars().count(), 80);
        let _ = fs::remove_dir_all(&root);
    }
}
