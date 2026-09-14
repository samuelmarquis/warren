//! What an agent leaves behind, so a machine that restarts can offer it back.
//!
//! An agent is a daemon and a daemon dies with its machine. The conversation
//! survives — the harness keeps that — but *which tab it was* did not: the
//! name, colour, slot, directory, harness and session id lived only in the
//! daemon's memory, and rebuilding a colony by hand out of the resume picker
//! is the one thing a reboot used to cost.
//!
//! So each daemon writes itself down, and removes the note when it ends on
//! purpose. What is left afterwards is exactly the set of agents that died
//! with the machine — nothing else can leave a note behind, because nothing
//! else skips the cleanup. A running agent never reads these: the daemon is
//! still the agent, and this is only ever consulted once it is gone.

use std::collections::HashSet;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// One agent, as much of it as can outlive the process.
#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct Record {
    pub name: String,
    pub display: String,
    pub color: u8,
    pub pinned: bool,
    pub slot: u8,
    pub cwd: String,
    pub kind: String,
    pub sys: Option<String>,
    pub extra: Option<String>,
    /// The conversation to come back to. An agent with none is not written
    /// down at all: there would be nothing to restore but a name.
    pub session: String,
}

pub fn dir() -> PathBuf {
    crate::paths::home().join("agents")
}

fn path(name: &str) -> PathBuf {
    dir().join(format!("{name}.json"))
}

/// Write the note, replacing any earlier one. Best-effort by design: an
/// agent that cannot be written down still runs, it just cannot be offered
/// back later.
pub fn save(rec: &Record) {
    let dir = dir();
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let Ok(body) = serde_json::to_vec_pretty(rec) else { return };
    // Through a temporary name: a note half-written is a note that cannot be
    // read, and this is rewritten every time a title changes.
    let tmp = dir.join(format!(".{}.tmp", rec.name));
    if std::fs::write(&tmp, &body).is_err() {
        let _ = std::fs::remove_file(&tmp);
        return;
    }
    if std::fs::rename(&tmp, path(&rec.name)).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// The agent ended on purpose; there is nothing to come back to.
pub fn forget(name: &str) {
    let _ = std::fs::remove_file(path(name));
}

/// Every note on this machine, oldest slot first.
pub fn all() -> Vec<Record> {
    let Ok(entries) = std::fs::read_dir(dir()) else { return Vec::new() };
    let mut out: Vec<Record> = entries
        .flatten()
        .filter(|e| e.path().extension().and_then(|x| x.to_str()) == Some("json"))
        .filter_map(|e| std::fs::read(e.path()).ok())
        .filter_map(|body| serde_json::from_slice(&body).ok())
        .collect();
    out.sort_by(|a: &Record, b: &Record| a.slot.cmp(&b.slot).then(a.name.cmp(&b.name)));
    out
}

/// Notes whose agent is not running: what a restore would actually bring
/// back. An agent that is already here needs nothing.
pub fn restorable(live: &HashSet<String>) -> Vec<Record> {
    all().into_iter().filter(|r| !live.contains(&r.name)).collect()
}
