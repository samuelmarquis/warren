//! Matrix chat for agents, so they can talk to you and to each other from
//! any harness.
//!
//! warren is not a chat client. The chat itself is Mentor's matrix-channel,
//! an MCP server that pushes messages into a running session and offers
//! reply/react/… tools; Claude Code takes it as a channel, and OMP through
//! the extension in `omp/warren-matrix.ts`. What warren does is the part only
//! it can do: give each agent its own copy of the channel, started with the
//! agent and resumed with it, and vouch for the agents it made.
//!
//! An agent starts with no account. It picks its own name with the channel's
//! `claim_name` tool, which makes `@<name>.<machine>`; until then it is in no
//! rooms and nobody can reach it. Once it has a name, warren invites it to the
//! roster room — membership there is what lets agents hear one another — and
//! to whatever rooms you `warren join` it to. Only you can reach an agent
//! unless you say otherwise.
//!
//! All of it is off unless `~/.warren/matrix.json` exists. The channel's own
//! state for an agent lives in `~/.warren/matrix/NAME/`.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::kind::Kind;

/// What the channel is called in every agent's MCP config, and so the name a
/// Claude Code agent is told to load as a channel.
pub const SERVER: &str = "warren-matrix";

/// The OMP side, built in so that it always matches this warren.
const OMP_EXTENSION: &str = include_str!("../omp/warren-matrix.ts");

/// `~/.warren/matrix.json`: this machine's half of the setup. Keys mirror the
/// channel's own config where they pass straight through to it.
#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    /// matrix_channel.py.
    pub channel: String,
    /// The python that runs it; any 3.9 or later.
    #[serde(default = "default_python")]
    pub python: String,
    /// The agent's last name: `@ada.<machine>`.
    pub machine: String,
    /// Extra environment for the channel (SSL_CERT_FILE, IAI_HOMESERVER, …).
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub homeserver: Option<String>,
    /// People whose messages reach every agent. Just you, by default.
    pub allowed_users: Vec<String>,
    /// Whose invites an agent accepts: you, and warren's own account.
    pub inviters: Vec<String>,
    /// The room whose members count as agents.
    #[serde(default)]
    pub roster_room: Option<String>,
    /// Names no agent may take.
    #[serde(default)]
    pub reserved: Vec<String>,
    #[serde(default)]
    pub agent_loop_cap: Option<u32>,
    /// Must a message mention the agent to reach it? On by default: in a
    /// room of agents, the alternative is every one of them answering
    /// everything.
    #[serde(default = "yes")]
    pub require_mention: bool,
    /// Do agents made from now on get chat? `warren new --matrix` or
    /// `--no-matrix`, and `:set matrix on|off`, decide for one agent.
    #[serde(default = "yes")]
    pub new_agents: bool,
    /// warren's own account (access_token, homeserver): what invites agents.
    #[serde(default)]
    pub warren_account: Option<String>,
    /// Settings for one agent, by its warren name, over the ones above: a
    /// pair of agents working together in a room of their own needs no
    /// mention, no loop cap, and each other's word.
    #[serde(default)]
    pub agents: BTreeMap<String, AgentOverride>,
}

#[derive(Deserialize, Debug, Clone, Default)]
#[serde(rename_all = "camelCase")]
pub struct AgentOverride {
    /// More people (or agents elsewhere, such as a Hermes) whose messages
    /// reach this agent as from a person: trusted, and never loop-capped.
    #[serde(default)]
    pub allowed_users: Vec<String>,
    /// Per-room options for the channel, e.g. `{"!pair:x": {"requireMention": false}}`.
    #[serde(default)]
    pub rooms: BTreeMap<String, Value>,
    /// 0 turns the loop guard off for this agent.
    #[serde(default)]
    pub agent_loop_cap: Option<u32>,
}

fn default_python() -> String {
    "python3".into()
}

fn yes() -> bool {
    true
}

pub fn config_path() -> PathBuf {
    crate::paths::home().join("matrix.json")
}

/// This machine's setup, or None when chat is not set up here. A file that
/// is there but wrong is an error worth seeing, not a silent off.
pub fn config() -> Result<Option<Config>> {
    let path = config_path();
    let body = match std::fs::read(&path) {
        Ok(body) => body,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e).with_context(|| format!("reading {}", path.display())),
    };
    let cfg =
        serde_json::from_slice(&body).with_context(|| format!("reading {}", path.display()))?;
    Ok(Some(cfg))
}

/// Does a new agent get chat, when nobody said? Whatever the setup says, and
/// no when there is no setup.
pub fn on_by_default() -> bool {
    matches!(config(), Ok(Some(c)) if c.new_agents)
}

/// The channel's name for an agent's state: its profile. The channel takes
/// only lowercase letters, digits, `.`, `_` and `-`, starting with a letter or
/// digit, and agent names are not so tame (DESIGN, Mentor). The OMP extension
/// works this out the same way.
pub fn profile(name: &str) -> String {
    let mut out: String = name
        .chars()
        .map(|c| c.to_ascii_lowercase())
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '-'
            }
        })
        .collect();
    if !out.starts_with(|c: char| c.is_ascii_alphanumeric()) {
        out.insert(0, 'w');
    }
    out
}

/// One agent's channel state: its config, sync token, identity, log.
pub fn agent_dir(name: &str) -> PathBuf {
    crate::paths::home().join("matrix").join(profile(name))
}

/// The command-line flags that give this agent its channel, writing the
/// files they point at. Called on every start, so a change to the setup
/// reaches an agent the next time it wakes.
pub fn launch_flags(kind: Kind, name: &str) -> Result<String> {
    let cfg =
        config()?.context("chat is on for this agent, but ~/.warren/matrix.json is missing")?;
    let dir = write_agent_files(&cfg, name)?;
    let mcp = dir.join("mcp.json");
    Ok(match kind {
        Kind::Claude => format!(
            "--mcp-config {} --dangerously-load-development-channels server:{SERVER}",
            quote(&mcp)
        ),
        Kind::Omp => format!("-e {}", quote(&omp_extension()?)),
    })
}

fn quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', r"'\''"))
}

/// The channel config and the MCP config for one agent.
fn write_agent_files(cfg: &Config, name: &str) -> Result<PathBuf> {
    let dir = agent_dir(name);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;
    private(&dir, 0o700)?;

    // Approvers are people named outright; a pattern can never approve, and
    // nor can anyone an override adds.
    let approvers: Vec<&String> = cfg
        .allowed_users
        .iter()
        .filter(|u| !u.contains('*'))
        .collect();
    let extra = cfg.agents.get(name).cloned().unwrap_or_default();
    let mut allowed = cfg.allowed_users.clone();
    allowed.extend(
        extra
            .allowed_users
            .iter()
            .filter(|u| !cfg.allowed_users.contains(u))
            .cloned(),
    );
    let mut channel = json!({
        "allowedUsers": allowed,
        "approvers": approvers,
        "inviters": cfg.inviters,
        "claim": { "machine": cfg.machine, "reserved": cfg.reserved },
        "roomDefaults": { "requireMention": cfg.require_mention },
        "rooms": extra.rooms,
        "relayPermissions": false,
    });
    if let Some(roster) = &cfg.roster_room {
        channel["rosterRoom"] = json!(roster);
    }
    if let Some(cap) = extra.agent_loop_cap.or(cfg.agent_loop_cap) {
        channel["agentLoopCap"] = json!(cap);
    }
    if let Some(hs) = &cfg.homeserver {
        channel["homeserver"] = json!(hs);
    }
    write_private(
        &dir.join("config.json"),
        &serde_json::to_vec_pretty(&channel)?,
    )?;

    // Through sh, only so the channel's stderr lands in a file: Claude Code
    // keeps none of it, and it is the only place the channel says what went
    // wrong.
    let mut env: BTreeMap<String, String> = cfg.env.clone();
    env.insert(
        "MATRIX_CHANNEL_DIR".into(),
        crate::paths::home().join("matrix").display().to_string(),
    );
    let mcp = json!({
        "mcpServers": {
            SERVER: {
                "command": "/bin/sh",
                "args": [
                    "-c", r#"exec "$0" -I "$1" --profile "$2" 2>>"$3""#,
                    cfg.python, cfg.channel, profile(name),
                    dir.join("channel.log").display().to_string(),
                ],
                "env": env,
            }
        }
    });
    write_private(&dir.join("mcp.json"), &serde_json::to_vec_pretty(&mcp)?)?;
    Ok(dir)
}

/// The OMP extension, on disk where `omp -e` can load it.
fn omp_extension() -> Result<PathBuf> {
    let path = crate::paths::home().join("omp-warren-matrix.ts");
    if std::fs::read_to_string(&path).ok().as_deref() != Some(OMP_EXTENSION) {
        write_private(&path, OMP_EXTENSION.as_bytes())?;
    }
    Ok(path)
}

fn private(path: &Path, mode: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .with_context(|| format!("restricting {}", path.display()))
}

/// Through a temporary name and with the mode set before a byte is written:
/// these hold who may talk to the agent.
fn write_private(path: &Path, body: &[u8]) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let tmp = path.with_extension("tmp");
    let mut f = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(&tmp)
        .with_context(|| format!("writing {}", tmp.display()))?;
    f.write_all(body)?;
    drop(f);
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
}

/// The account an agent claimed, once it has.
pub fn identity(name: &str) -> Option<String> {
    let body = std::fs::read(agent_dir(name).join("identity.json")).ok()?;
    let v: Value = serde_json::from_slice(&body).ok()?;
    v.get("user_id")?.as_str().map(str::to_string)
}

/// The agent is over. Its account is not deleted — a name once used stays
/// taken on Matrix whatever happens — but its channel state goes, so a new
/// agent that happens to get the same warren name starts as nobody.
pub fn retire(name: &str) {
    let dir = agent_dir(name);
    if !dir.exists() {
        return;
    }
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let ended = crate::paths::home().join("matrix").join(".ended");
    if std::fs::create_dir_all(&ended).is_ok() {
        let _ = std::fs::rename(&dir, ended.join(format!("{}-{stamp}", profile(name))));
    }
}

/// Has this agent been put in the roster room yet? Kept so a restart does not
/// invite it again.
fn rostered_marker(name: &str) -> PathBuf {
    agent_dir(name).join("rostered")
}

/// Wait for the agent to claim a name, then put it in the roster room. Runs
/// on its own thread for as long as the daemon does, and does nothing at all
/// when there is no roster or no account to invite from.
pub fn watch_for_claim(name: String) {
    let Ok(Some(cfg)) = config() else { return };
    let (Some(roster), Some(_)) = (cfg.roster_room.clone(), cfg.warren_account.clone()) else {
        return;
    };
    if rostered_marker(&name).exists() {
        return;
    }
    std::thread::spawn(move || {
        let mut wait = std::time::Duration::from_secs(5);
        loop {
            if !agent_dir(&name).exists() {
                return; // retired, or chat turned off and cleaned up
            }
            if let Some(user) = identity(&name) {
                match invite(&cfg, &user, &roster) {
                    Ok(()) => {
                        let _ = std::fs::write(rostered_marker(&name), user);
                        return;
                    }
                    // Back off: the homeserver may be away, and this thread
                    // has all the time in the world.
                    Err(_) => wait = (wait * 2).min(std::time::Duration::from_secs(300)),
                }
            }
            std::thread::sleep(wait);
        }
    });
}

/// `warren join NAME ROOM`: invite the agent, as warren, to a room you name.
/// The agent accepts because warren is one of its inviters.
pub fn join(name: &str, room: &str) -> Result<String> {
    let cfg = config()?.context("chat is not set up on this machine (no ~/.warren/matrix.json)")?;
    let Some(user) = identity(name) else {
        bail!("'{name}' has no chat name yet — it claims one with its claim_name tool");
    };
    invite(&cfg, &user, room)?;
    Ok(format!("invited {user} to {room}"))
}

/// One invite, from warren's account.
fn invite(cfg: &Config, user: &str, room: &str) -> Result<()> {
    let account = cfg
        .warren_account
        .as_deref()
        .context("no warrenAccount in ~/.warren/matrix.json: nothing to invite from")?;
    let account = expand_home(account);
    let creds: Value = serde_json::from_slice(
        &std::fs::read(&account).with_context(|| format!("reading {}", account.display()))?,
    )?;
    let token = creds["access_token"]
        .as_str()
        .context("warren's account has no access_token")?;
    let hs = creds["homeserver"]
        .as_str()
        .or(cfg.homeserver.as_deref())
        .context("warren's account names no homeserver")?
        .trim_end_matches('/');
    let api = format!("{hs}/_matrix/client/v3");

    let room_id = if room.starts_with('#') {
        let (status, body) = curl(
            token,
            "GET",
            &format!("{api}/directory/room/{}", encode(room)),
            None,
        )?;
        if status != 200 {
            bail!("no room {room} ({})", matrix_error(&body));
        }
        body["room_id"]
            .as_str()
            .context("the homeserver gave no room id")?
            .to_string()
    } else {
        room.to_string()
    };
    let (status, body) = curl(
        token,
        "POST",
        &format!("{api}/rooms/{}/invite", encode(&room_id)),
        Some(json!({ "user_id": user })),
    )?;
    match status {
        200 => Ok(()),
        // Asked twice is not a failure: the agent is where it was meant to be.
        _ if matrix_error(&body).contains("already") => Ok(()),
        403 => bail!(
            "warren may not invite to {room} ({}) — invite warren's account there first",
            matrix_error(&body)
        ),
        _ => bail!("inviting {user} to {room}: {}", matrix_error(&body)),
    }
}

fn matrix_error(body: &Value) -> String {
    match (body["errcode"].as_str(), body["error"].as_str()) {
        (Some(code), Some(msg)) => format!("{code}: {msg}"),
        (Some(code), None) => code.to_string(),
        _ => body.to_string(),
    }
}

fn expand_home(path: &str) -> PathBuf {
    match path.strip_prefix("~/") {
        Some(rest) => PathBuf::from(std::env::var("HOME").unwrap_or_default()).join(rest),
        None => PathBuf::from(path),
    }
}

/// A Matrix client-server call through curl, which every machine warren runs
/// on already has. The token goes in on stdin, never on a command line that
/// `ps` would show.
fn curl(token: &str, method: &str, url: &str, body: Option<Value>) -> Result<(u16, Value)> {
    let mut cmd = Command::new("curl");
    cmd.args([
        "-sS",
        "-m",
        "30",
        "--retry",
        "4",
        "--retry-all-errors",
        "-K",
        "-",
        "-X",
        method,
    ])
    .args([
        "-w",
        "\n%{http_code}",
        "-H",
        "Content-Type: application/json",
    ]);
    if let Some(body) = &body {
        cmd.args(["-d", &body.to_string()]);
    }
    cmd.arg(url)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = cmd.spawn().context("running curl")?;
    child
        .stdin
        .take()
        .context("curl's stdin")?
        .write_all(format!("header = \"Authorization: Bearer {token}\"\n").as_bytes())?;
    let out = child.wait_with_output()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let (body, status) = text.rsplit_once('\n').unwrap_or(("", &text));
    let status: u16 = status.trim().parse().unwrap_or(0);
    if status == 0 {
        let why = String::from_utf8_lossy(&out.stderr);
        bail!("could not reach the homeserver: {}", why.trim());
    }
    Ok((status, serde_json::from_str(body).unwrap_or(Value::Null)))
}

/// Percent-encode one path segment: room ids and aliases carry `!`, `#`, `:`.
fn encode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn room_ids_and_aliases_are_one_path_segment() {
        assert_eq!(
            encode("!abc:immanence.institute"),
            "%21abc%3Aimmanence.institute"
        );
        assert_eq!(encode("#open-plugins:x"), "%23open-plugins%3Ax");
    }

    #[test]
    fn every_agent_name_makes_a_profile_the_channel_takes() {
        assert_eq!(profile("DESIGN"), "design");
        assert_eq!(profile("SPORK-Terrain"), "spork-terrain");
        assert_eq!(profile("open-eq"), "open-eq");
        assert_eq!(profile("_x"), "w_x");
    }

    #[test]
    fn a_minimal_setup_reads_with_safe_defaults() {
        let cfg: Config = serde_json::from_str(
            r#"{"channel":"/c.py","machine":"smq","allowedUsers":["@sam:x"],"inviters":["@sam:x"]}"#,
        )
        .unwrap();
        assert!(
            cfg.require_mention,
            "rooms of agents need a mention by default"
        );
        assert!(cfg.new_agents);
        assert_eq!(cfg.python, "python3");
        assert!(cfg.roster_room.is_none() && cfg.warren_account.is_none());
    }
}
