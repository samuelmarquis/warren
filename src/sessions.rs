//! List a harness's resumable sessions across ALL projects, newest first.
//!
//! Reads ~/.claude/projects/*/*.jsonl directly (rather than relying on
//! `claude --resume`'s cwd-scoped picker, whose behavior could change).
//! `warren sessions` emits one session per line as TSV:
//!
//!     <sessionId>\t<mtime_epoch>\t<cwd>\t<title>
//!
//! The session id is the file stem; cwd and title are read from the
//! records. The title is the name you gave it with `/rename` (a
//! `custom-title` record), else Claude's own `aiTitle`, else the first user
//! message — and for either kind of title the last one written wins, as it
//! does when Claude reads the file back. Lines are pre-filtered
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
            .ok_or_else(|| anyhow::anyhow!("unknown agent kind '{arg}' (claude, omp, or agy)"))?,
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
    label
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(80)
        .collect()
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
        let Ok(files) = std::fs::read_dir(project.path()) else {
            continue;
        };
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
            let Ok(handle) = std::fs::File::open(&path) else {
                continue;
            };
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
                let Ok(d) = serde_json::from_str::<Value>(&ln) else {
                    continue;
                };
                match d.get("type").and_then(Value::as_str) {
                    Some("session") => {
                        id = d
                            .get("id")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        cwd = d
                            .get("cwd")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                        title = d
                            .get("title")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string();
                    }
                    Some("title") => {
                        pad_title = d
                            .get("title")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .trim()
                            .to_string();
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
            let Ok(modified) = meta.modified() else {
                continue;
            };
            let mtime = modified
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs_f64())
                .unwrap_or(0.0);
            if let Some((cwd, label)) = read_session_file(&path) {
                rows.push(Session {
                    id: stem.to_string(),
                    mtime,
                    cwd,
                    title: label,
                });
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
    let mut custom = String::new();
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
        let want_custom = ln.contains("\"customTitle\"");
        let want_cwd = cwd.is_empty() && ln.contains("\"cwd\"");
        let want_user = first_user.is_empty()
            && title.is_empty()
            && (ln.contains("\"type\":\"user\"") || ln.contains("\"type\": \"user\""));
        if !(want_title || want_custom || want_cwd || want_user) {
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
        // Claude rewrites its aiTitle all session long, right alongside the
        // name you picked; only the name is one you would recognise.
        if want_custom && d.get("type").and_then(Value::as_str) == Some("custom-title") {
            if let Some(t) = d.get("customTitle").and_then(Value::as_str) {
                custom = t.trim().to_string();
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

    let label = if !custom.is_empty() {
        custom
    } else if !title.is_empty() {
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

/// Antigravity indexes top-level conversations in `conversation_summaries.db`
/// (with `title`, `preview`, `last_modified_time`, and `workspace_uris`) and
/// logs prompt history in `history.jsonl`. Prefer the SQLite summary table so
/// `/rename`d and AI-generated titles win; fall back to `history.jsonl` if the
/// database is absent or `sqlite3` is not installed.
pub fn scan_agy(agy_home: &Path) -> Vec<Session> {
    let db = agy_home.join("conversation_summaries.db");
    if db.is_file() {
        if let Some(mut rows) = scan_agy_sqlite(&db) {
            newest_first(&mut rows);
            return rows;
        }
    }
    let mut rows = scan_agy_history(agy_home);
    newest_first(&mut rows);
    rows
}

fn scan_agy_sqlite(db: &Path) -> Option<Vec<Session>> {
    let query = "SELECT conversation_id, title, preview, last_modified_time, workspace_uris \
                 FROM conversation_summaries \
                 WHERE parent_conversation_id = '' AND nesting_depth = 0 \
                 AND killed = 0 AND conversation_id != ''";
    let out = std::process::Command::new("sqlite3")
        .args(["-readonly", "-cmd", ".timeout 1000", "-json"])
        .arg(db)
        .arg(query)
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let trimmed = String::from_utf8_lossy(&out.stdout);
    let trimmed = trimmed.trim();
    if trimmed.is_empty() {
        return Some(Vec::new());
    }
    let items: Vec<Value> = serde_json::from_str(trimmed).ok()?;
    let mut rows = Vec::with_capacity(items.len());
    for item in items {
        let Some(id) = item
            .get("conversation_id")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let mtime = item
            .get("last_modified_time")
            .and_then(Value::as_str)
            .and_then(parse_iso_epoch)
            .unwrap_or(0.0);
        let cwd = item
            .get("workspace_uris")
            .and_then(Value::as_str)
            .and_then(first_workspace_path)
            .unwrap_or_else(|| "?".to_string());
        let title = item
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let preview = item
            .get("preview")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let label = [title, preview]
            .into_iter()
            .find(|s| !s.is_empty())
            .unwrap_or("(untitled)");
        rows.push(Session {
            id: id.to_string(),
            mtime,
            cwd,
            title: tidy(label),
        });
    }
    Some(rows)
}

fn scan_agy_history(agy_home: &Path) -> Vec<Session> {
    let path = agy_home.join("history.jsonl");
    let Ok(file) = std::fs::File::open(&path) else {
        return Vec::new();
    };
    let conv_dir = agy_home.join("conversations");
    let brain_dir = agy_home.join("brain");
    let check_store = conv_dir.is_dir() || brain_dir.is_dir();

    // Preserves insertion order cleanly after sorting by mtime.
    struct Acc {
        mtime: f64,
        cwd: String,
        renamed: String,
        first_prompt: String,
    }
    let mut map: std::collections::HashMap<String, Acc> = std::collections::HashMap::new();
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        if !line.contains("conversationId") {
            continue;
        }
        let Ok(d) = serde_json::from_str::<Value>(&line) else {
            continue;
        };
        let Some(id) = d
            .get("conversationId")
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
        else {
            continue;
        };
        let ts = d.get("timestamp").and_then(Value::as_f64).unwrap_or(0.0);
        let mtime = if ts > 1e11 { ts / 1000.0 } else { ts };
        let ws = d.get("workspace").and_then(Value::as_str).unwrap_or("");
        let display = d
            .get("display")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        let entry = map.entry(id.to_string()).or_insert_with(|| Acc {
            mtime: 0.0,
            cwd: String::new(),
            renamed: String::new(),
            first_prompt: String::new(),
        });
        if mtime > entry.mtime {
            entry.mtime = mtime;
        }
        if !ws.is_empty() {
            entry.cwd = ws.to_string();
        }
        if let Some(rest) = display.strip_prefix("/rename ") {
            let rest = rest.trim();
            if !rest.is_empty() {
                entry.renamed = rest.to_string();
            }
        } else if entry.first_prompt.is_empty() && !display.starts_with('/') && !display.is_empty()
        {
            entry.first_prompt = display.to_string();
        }
    }

    map.into_iter()
        .filter(|(id, _)| {
            !check_store
                || conv_dir.join(format!("{id}.db")).is_file()
                || brain_dir.join(id).is_dir()
        })
        .map(|(id, acc)| {
            let label = if !acc.renamed.is_empty() {
                acc.renamed
            } else if !acc.first_prompt.is_empty() {
                acc.first_prompt
            } else {
                "(untitled)".to_string()
            };
            Session {
                id,
                mtime: acc.mtime,
                cwd: if acc.cwd.is_empty() {
                    "?".to_string()
                } else {
                    acc.cwd
                },
                title: tidy(&label),
            }
        })
        .collect()
}

/// First `file://` URI out of Antigravity's `workspace_uris` JSON column
/// (`["file:///Users/sam/Developer/warren"]`), percent-decoded to a path.
fn first_workspace_path(raw: &str) -> Option<String> {
    let uris: Vec<String> = serde_json::from_str(raw).ok()?;
    let uri = uris.first()?.trim();
    let rest = uri
        .strip_prefix("file://localhost")
        .or_else(|| uri.strip_prefix("file://"))
        .unwrap_or(uri);
    let decoded = percent_decode(rest);
    (!decoded.is_empty()).then_some(decoded)
}

fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            if let (Some(hi), Some(lo)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2])) {
                out.push(hi << 4 | lo);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// Parse an ISO-8601 / SQLite timestamp (`2026-11-17 23:36:54.367012+00:00` or
/// `2026-11-17T18:35:59-05:00`) to Unix epoch seconds.
fn parse_iso_epoch(s: &str) -> Option<f64> {
    let s = s.trim();
    if s.len() < 19 {
        return None;
    }
    let b = s.as_bytes();
    if b[4] != b'-'
        || b[7] != b'-'
        || !matches!(b[10], b'T' | b't' | b' ')
        || b[13] != b':'
        || b[16] != b':'
    {
        return None;
    }
    let y: i64 = s.get(0..4)?.parse().ok()?;
    let m: u32 = s.get(5..7)?.parse().ok()?;
    let d: u32 = s.get(8..10)?.parse().ok()?;
    let hh: u32 = s.get(11..13)?.parse().ok()?;
    let mi: u32 = s.get(14..16)?.parse().ok()?;
    let ss: u32 = s.get(17..19)?.parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || hh > 23 || mi > 59 || ss > 60 {
        return None;
    }

    let mut rest = &s[19..];
    let mut frac = 0.0f64;
    if let Some(after_dot) = rest.strip_prefix('.') {
        let digits = after_dot.bytes().take_while(|c| c.is_ascii_digit()).count();
        if digits > 0 {
            if let Ok(v) = after_dot[..digits].parse::<f64>() {
                frac = v / 10f64.powi(digits as i32);
            }
        }
        rest = &after_dot[digits..];
    }

    let rest = rest.trim();
    let tz_secs: i64 = if rest.is_empty() || matches!(rest, "Z" | "z") {
        0
    } else {
        let sign = match rest.as_bytes().first() {
            Some(b'+') => 1i64,
            Some(b'-') => -1i64,
            _ => return None,
        };
        let off = &rest[1..];
        let (oh, om) = if let Some((h, m)) = off.split_once(':') {
            (h.parse::<i64>().ok()?, m.parse::<i64>().ok()?)
        } else if off.len() >= 4 {
            (
                off[..2].parse::<i64>().ok()?,
                off[2..4].parse::<i64>().ok()?,
            )
        } else if off.len() == 2 {
            (off.parse::<i64>().ok()?, 0)
        } else {
            return None;
        };
        sign * (oh * 3600 + om * 60)
    };

    // Civil date to days since 1970-01-01 (Howard Hinnant).
    let y_adj = if m <= 2 { y - 1 } else { y };
    let era = if y_adj >= 0 { y_adj } else { y_adj - 399 } / 400;
    let yoe = (y_adj - era * 400) as u32;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe as i64 - 719468;
    let epoch = days * 86400 + (hh as i64) * 3600 + (mi as i64) * 60 + (ss as i64) - tz_secs;
    Some(epoch as f64 + frac)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fixture_root(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "warren-sessions-test-{}-{name}",
            std::process::id()
        ));
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
    fn a_session_you_renamed_is_listed_by_the_name_you_gave_it() {
        let root = fixture_root("renamed");
        fs::write(
            root.join("proj/r1.jsonl"),
            concat!(
                r#"{"type":"user","cwd":"/w/Research","message":{"content":"look at TEEs"}}"#,
                "\n",
                r#"{"type":"ai-title","aiTitle":"Explore type systems and TEE partitioning","sessionId":"r1"}"#,
                "\n",
                r#"{"type":"custom-title","customTitle":"SPLITR","sessionId":"r1"}"#,
                "\n",
                r#"{"type":"agent-name","agentName":"SPLITR","sessionId":"r1"}"#,
                "\n",
                // Claude keeps rewriting its own title after the rename.
                r#"{"type":"ai-title","aiTitle":"Explore type systems and TEE partitioning","sessionId":"r1"}"#,
                "\n",
                // And a second rename replaces the first.
                r#"{"type":"custom-title","customTitle":"SPLITR-2","sessionId":"r1"}"#,
                "\n",
            ),
        )
        .unwrap();
        let s = scan_claude(&root);
        assert_eq!(s[0].title, "SPLITR-2");
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
        fs::write(
            root.join("-w-repo/notes.jsonl"),
            "{\"type\":\"model_change\"}\n",
        )
        .unwrap();

        let s = scan_omp(&root);
        assert_eq!(s.len(), 1);
        assert_eq!(s[0].id, "01a08858-abcd");
        assert_eq!(s[0].cwd, "/w/repo");
        assert_eq!(
            s[0].title, "Remove noreply lines",
            "the head title wins, whitespace squashed"
        );
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

    #[test]
    fn iso_timestamps_and_workspace_uris_decode_cleanly() {
        assert_eq!(parse_iso_epoch("1970-01-01T00:00:00Z"), Some(0.0));
        assert_eq!(
            parse_iso_epoch("2026-11-17 23:36:54+00:00"),
            parse_iso_epoch("2026-11-17T18:36:54-05:00")
        );
        let sub = parse_iso_epoch("1970-01-01 00:00:01.250000+00:00").unwrap();
        assert!((sub - 1.25).abs() < 1e-6);

        assert_eq!(
            first_workspace_path(r#"["file:///Users/sam/My%20Code/warren"]"#).as_deref(),
            Some("/Users/sam/My Code/warren")
        );
        assert_eq!(first_workspace_path("[]"), None);
    }

    #[test]
    fn agy_sessions_read_sqlite_summaries_and_skip_subagents() {
        let root = fixture_root("agy_db");
        let db = root.join("conversation_summaries.db");
        let schema_and_rows = "\
            CREATE TABLE conversation_summaries (\
                conversation_id text PRIMARY KEY,\
                title text NOT NULL DEFAULT '',\
                preview text NOT NULL DEFAULT '',\
                last_modified_time datetime NOT NULL,\
                workspace_uris text NOT NULL,\
                parent_conversation_id text NOT NULL DEFAULT '',\
                nesting_depth integer NOT NULL DEFAULT 0,\
                killed numeric NOT NULL DEFAULT 0\
            );\
            INSERT INTO conversation_summaries VALUES \
                ('c-old', '', 'First   prompt preview', '2026-11-17 10:00:00+00:00', '[\"file:///w/one\"]', '', 0, 0),\
                ('c-new', 'Renamed   Title', 'Older preview', '2026-11-17 12:00:00+00:00', '[\"file:///w/two\"]', '', 0, 0),\
                ('c-sub', 'Subagent work', '', '2026-11-17 13:00:00+00:00', '[\"file:///w/two\"]', 'c-new', 1, 0),\
                ('c-dead', 'Killed', '', '2026-11-17 14:00:00+00:00', '[\"file:///w/two\"]', '', 0, 1);";
        let ok = std::process::Command::new("sqlite3")
            .arg(&db)
            .arg(schema_and_rows)
            .status()
            .map(|s| s.success())
            .unwrap_or(false);
        if ok {
            let s = scan_agy(&root);
            assert_eq!(s.len(), 2);
            assert_eq!(s[0].id, "c-new");
            assert_eq!(s[0].cwd, "/w/two");
            assert_eq!(s[0].title, "Renamed Title");
            assert_eq!(s[1].id, "c-old");
            assert_eq!(s[1].cwd, "/w/one");
            assert_eq!(s[1].title, "First prompt preview");
        }
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn agy_history_fallback_aggregates_prompts_and_renames() {
        let root = fixture_root("agy_hist");
        fs::create_dir_all(root.join("conversations")).unwrap();
        fs::write(root.join("conversations/u1.db"), b"").unwrap();
        fs::write(root.join("conversations/u2.db"), b"").unwrap();
        fs::write(
            root.join("history.jsonl"),
            concat!(
                r#"{"conversationId":"u1","timestamp":1700000000000,"workspace":"/w/a","display":"initial   ask"}"#,
                "\n",
                r#"{"conversationId":"u1","timestamp":1700000005000,"workspace":"/w/a","display":"/rename Clean Name"}"#,
                "\n",
                r#"{"conversationId":"u2","timestamp":1700000010000,"workspace":"/w/b","display":"second thread"}"#,
                "\n",
                r#"{"conversationId":"gone","timestamp":1700000020000,"workspace":"/w/c","display":"deleted db"}"#,
                "\n",
            ),
        )
        .unwrap();

        let s = scan_agy(&root);
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].id, "u2");
        assert_eq!(s[0].title, "second thread");
        assert_eq!(s[1].id, "u1");
        assert_eq!(s[1].title, "Clean Name");
        let _ = fs::remove_dir_all(&root);
    }
}
