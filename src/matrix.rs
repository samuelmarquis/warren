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
use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{ChildStdin, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::kind::Kind;
use crate::proto::{self, ToDaemon};

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
    /// Talkback, where agents leave asks for you and your replies come back:
    /// a second channel beside the chat one, for Claude Code agents.
    #[serde(default)]
    pub talkback: Option<Talkback>,
    /// More channels like talkback (marginalia, …), by their MCP server name:
    /// each one an `--agent NAME` plugin beside chat, for Claude Code agents
    /// under its dirs.
    #[serde(default)]
    pub sidecars: BTreeMap<String, Talkback>,
}

impl Config {
    /// Talkback and the other sidecars, under their server names, in order.
    fn all_sidecars(&self) -> Vec<(&str, &Talkback)> {
        let mut out: Vec<(&str, &Talkback)> = self.talkback.iter().map(|t| (TALKBACK, t)).collect();
        out.extend(
            self.sidecars
                .iter()
                .filter(|(k, _)| {
                    k.as_str() != TALKBACK && k.as_str() != SERVER && valid_server_name(k)
                })
                .map(|(k, v)| (k.as_str(), v)),
        );
        out
    }
}

/// An MCP server name that can sit on the channels flag and in a file name.
fn valid_server_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The `talkback` key of `~/.warren/matrix.json`.
#[derive(Deserialize, Debug, Clone)]
#[serde(rename_all = "camelCase")]
pub struct Talkback {
    /// talkback_channel.py. It runs on the chat channel's python.
    pub channel: String,
    /// Its environment: TALKBACK_URL, TALKBACK_TOKEN_FILE, SSL_CERT_FILE, …
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Only agents working in one of these directories (or below) get it;
    /// with none listed, every agent with chat does.
    #[serde(default)]
    pub dirs: Vec<String>,
}

/// What talkback calls this agent: the name on your cards, and what your
/// replies are routed by. A pinned display name (kiln, nullset) is the one the
/// room knows; an unpinned one follows the session's title, so it would move.
fn talkback_name(name: &str) -> String {
    match crate::record::get(name) {
        Some(rec) if rec.pinned && !rec.display.trim().is_empty() => rec.display,
        _ => name.to_string(),
    }
}

/// Does an agent working in `cwd` get talkback?
fn talkback_covers(tb: &Talkback, cwd: &str) -> bool {
    // Resolved, where they can be: the same directory can be spelled two
    // ways (on macOS, /tmp is /private/tmp).
    let real = |p: &Path| std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let cwd = real(Path::new(cwd));
    tb.dirs.is_empty()
        || tb
            .dirs
            .iter()
            .any(|d| cwd.starts_with(real(&expand_home(d))))
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
pub fn launch_flags(kind: Kind, name: &str, cwd: &str) -> Result<String> {
    let cfg =
        config()?.context("chat is on for this agent, but ~/.warren/matrix.json is missing")?;
    // Both harnesses get them in the MCP file: Claude Code as channels, OMP
    // through warren's extension, which relays what they push.
    let sidecars: Vec<(&str, &Talkback)> = cfg
        .all_sidecars()
        .into_iter()
        .filter(|(_, tb)| talkback_covers(tb, cwd))
        .collect();
    let dir = write_agent_files(&cfg, name, &sidecars)?;
    let mcp = dir.join("mcp.json");
    Ok(match kind {
        Kind::Claude => format!(
            "--mcp-config {} --dangerously-load-development-channels server:{SERVER}{}",
            quote(&mcp),
            sidecars
                .iter()
                .map(|(n, _)| format!(" server:{n}"))
                .collect::<String>()
        ),
        Kind::Omp => format!("-e {}", quote(&omp_extension()?)),
        Kind::Agy => {
            set_agy_enabled(name, true)?;
            ensure_agy_plugin(&cfg)?;
            String::new()
        }
    })
}

/// Talkback's name in the MCP config, and on the channels flag.
pub const TALKBACK: &str = "talkback";

fn quote(path: &Path) -> String {
    format!("'{}'", path.display().to_string().replace('\'', r"'\''"))
}

/// The channel config and the MCP config for one agent.
fn write_agent_files(cfg: &Config, name: &str, sidecars: &[(&str, &Talkback)]) -> Result<PathBuf> {
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
    let mut mcp = json!({
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
    for (server, tb) in sidecars {
        // Its log beside the chat channel's; its own state (a reply cursor)
        // stays where the plugin keeps it, under the agent's name there.
        mcp["mcpServers"][*server] = json!({
            "command": "/bin/sh",
            "args": [
                "-c", r#"exec "$0" -I "$1" --agent "$2" 2>>"$3""#,
                cfg.python, tb.channel, talkback_name(name),
                dir.join(format!("{server}.log")).display().to_string(),
            ],
            "env": tb.env,
        });
    }
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

// --------------------------------------------------------------- Antigravity
//
// Antigravity loads MCP servers and lifecycle hooks from plugins under
// `~/.gemini/config/plugins/`. One shared `warren-matrix` plugin serves every
// `agy` session on the machine: its stdio server is `warren __mcp <server>`,
// which inspects `$WARREN_AGENT` (inherited from the daemon's pty) to route
// each agent to its own `~/.warren/matrix/<profile>/mcp.json` — and becomes a
// zero-tool no-op outside warren or when `:set matrix off` is in effect.
//
// Because `warren __mcp` proxies the stdio stream rather than `exec`ing away,
// it intercepts `notifications/claude/channel` pushes from `matrix_channel.py`
// (and `talkback` / `sidecars`) and forwards them as `ToDaemon::ChannelPush`
// over `$WARREN_SOCK`: an idle `agy` prompt is woken immediately via bracketed
// paste, while mid-turn pushes are drained between steps by `PostInvocation`
// and `Stop` hooks.

fn agy_marker(name: &str) -> PathBuf {
    agent_dir(name).join("agy.on")
}

/// Turn Antigravity's chat bridge on or off for one agent.
pub fn set_agy_enabled(name: &str, on: bool) -> Result<()> {
    let marker = agy_marker(name);
    if on {
        if let Some(parent) = marker.parent() {
            std::fs::create_dir_all(parent)?;
            private(parent, 0o700)?;
        }
        write_private(&marker, b"1\n")?;
    } else {
        let _ = std::fs::remove_file(marker);
    }
    Ok(())
}

pub fn is_agy_enabled(name: &str) -> bool {
    agy_marker(name).is_file()
}

/// Write or refresh `~/.gemini/config/plugins/warren-matrix/` so `agy`
/// discovers the MCP bridge and the channel-drain hooks.
fn ensure_agy_plugin(cfg: &Config) -> Result<PathBuf> {
    let exe = std::env::current_exe().context("resolving warren binary path")?;
    ensure_agy_plugin_in(&crate::paths::agy_plugins_dir(), &exe, cfg)
}

fn ensure_agy_plugin_in(plugins_root: &Path, exe: &Path, cfg: &Config) -> Result<PathBuf> {
    let dir = plugins_root.join(SERVER);
    std::fs::create_dir_all(&dir).with_context(|| format!("creating {}", dir.display()))?;

    let manifest = serde_json::to_vec_pretty(&json!({ "name": SERVER }))?;
    write_if_changed(&dir.join("plugin.json"), &manifest)?;

    let warren_home = std::env::var("WARREN_HOME").ok().filter(|h| !h.is_empty());
    let server_entry = |name: &str| {
        let mut spec = json!({
            "command": exe.display().to_string(),
            "args": ["__mcp", name],
        });
        if let Some(h) = &warren_home {
            spec["env"] = json!({ "WARREN_HOME": h });
        }
        spec
    };
    let mut servers = serde_json::Map::new();
    servers.insert(SERVER.to_string(), server_entry(SERVER));
    for (sidecar, _) in cfg.all_sidecars() {
        servers.insert(sidecar.to_string(), server_entry(sidecar));
    }
    let mcp_cfg = serde_json::to_vec_pretty(&json!({ "mcpServers": servers }))?;
    write_if_changed(&dir.join("mcp_config.json"), &mcp_cfg)?;

    let bin = crate::hooks::shell_exe(exe);
    let hooks = serde_json::to_vec_pretty(&json!({
        SERVER: {
            "SessionStart":   [{ "type": "command", "command": format!("{bin} hook agy-session"), "timeout": 5 }],
            "PreInvocation":  [{ "type": "command", "command": format!("{bin} hook agy-pre"),     "timeout": 5 }],
            "PostInvocation": [{ "type": "command", "command": format!("{bin} hook agy-post"),    "timeout": 5 }],
            "Stop":           [{ "type": "command", "command": format!("{bin} hook agy-stop"),    "timeout": 5 }],
        }
    }))?;
    write_if_changed(&dir.join("hooks.json"), &hooks)?;
    Ok(dir)
}

fn write_if_changed(path: &Path, body: &[u8]) -> Result<()> {
    if std::fs::read(path).ok().as_deref() == Some(body) {
        return Ok(());
    }
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, body).with_context(|| format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).with_context(|| format!("writing {}", path.display()))
}

/// Channel instructions captured during MCP `initialize`, returned at most
/// once per conversation (`SessionStart` or first `PreInvocation`, since
/// injected `systemMessage` steps persist in Antigravity's trajectory).
pub fn take_agy_instructions(agent: &str, session: Option<&str>) -> Option<String> {
    let dir = agent_dir(agent);
    let sid_clean: Option<String> = session
        .filter(|s| !s.is_empty())
        .map(|s| crate::names::sanitize(s))
        .filter(|s| !s.is_empty());
    if let Some(sid) = &sid_clean {
        if dir.join(format!("instructions.{sid}")).exists() {
            return None;
        }
    }
    let entries = std::fs::read_dir(&dir).ok()?;
    let mut main_note = String::new();
    let mut side_notes: Vec<(String, String)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("instructions") {
            continue;
        }
        let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let text = text.trim();
        if text.is_empty() {
            continue;
        }
        if stem == SERVER {
            main_note = text.to_string();
        } else {
            side_notes.push((stem.to_string(), text.to_string()));
        }
    }
    if main_note.is_empty() && side_notes.is_empty() {
        return None;
    }
    side_notes.sort_by(|a, b| a.0.cmp(&b.0));

    let mut out = String::new();
    if !main_note.is_empty() {
        out.push_str("# Matrix chat\n");
        out.push_str(&main_note);
        out.push('\n');
    }
    for (name, note) in side_notes {
        out.push_str(&format!("\n# {name}\n{note}\n"));
    }
    if let Some(sid) = sid_clean {
        let _ = write_private(&dir.join(format!("instructions.{sid}")), b"1\n");
    }
    Some(out)
}

#[derive(Deserialize, Clone)]
struct McpSpec {
    command: String,
    #[serde(default)]
    args: Vec<String>,
    #[serde(default)]
    env: BTreeMap<String, String>,
}

fn read_mcp_spec(agent: &str, server: &str) -> Option<McpSpec> {
    if !is_agy_enabled(agent) {
        return None;
    }
    let body = std::fs::read(agent_dir(agent).join("mcp.json")).ok()?;
    let v: Value = serde_json::from_slice(&body).ok()?;
    serde_json::from_value(v.get("mcpServers")?.get(server)?.clone()).ok()
}

/// A pushed notification formatted identically to Claude Code and OMP's
/// `<channel source="…">` events, so channel instructions read the same way
/// across all three harnesses.
pub(crate) fn as_channel_event(source: &str, params: &Value) -> String {
    let esc = |s: &str| s.replace('&', "&amp;").replace('"', "&quot;");
    let mut attrs = String::new();
    if let Some(meta) = params.get("meta").and_then(Value::as_object) {
        for (k, v) in meta {
            let raw = match v {
                Value::String(s) => s.clone(),
                Value::Null => String::new(),
                other => other.to_string(),
            };
            attrs.push_str(&format!(" {k}=\"{}\"", esc(&raw)));
        }
    }
    let content = params
        .get("content")
        .and_then(Value::as_str)
        .unwrap_or_default();
    format!(
        "<channel source=\"{}\"{attrs}>{content}</channel>",
        esc(source)
    )
}

fn send_channel_push(sock: &str, xml: String) {
    let Ok(mut stream) = UnixStream::connect(sock) else {
        return;
    };
    let _ = stream.set_write_timeout(Some(Duration::from_millis(250)));
    if let Ok(frame) = proto::encode_frame(&ToDaemon::ChannelPush(xml)) {
        let _ = stream.write_all(&frame);
    }
}

/// `warren __mcp <server>`: per-agent MCP stdio proxy and push-to-wake bridge.
pub fn cmd_mcp(args: &[String]) -> Result<()> {
    let server = args.first().map(String::as_str).unwrap_or(SERVER);
    let agent = std::env::var("WARREN_AGENT").ok().filter(|a| !a.is_empty());
    let spec = agent.as_deref().and_then(|a| read_mcp_spec(a, server));
    match (agent, spec) {
        (Some(agent), Some(spec)) => bridge_mcp(&agent, server, spec),
        _ => noop_mcp(),
    }
}

/// Outside warren (or with `:set matrix off`, or for a sidecar whose `dirs` do
/// not cover this agent), answer MCP discovery with zero tools and sit idle
/// until stdin closes.
fn noop_mcp() -> Result<()> {
    let stdin = std::io::stdin();
    let stdout = std::io::stdout();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(msg) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(id) = msg.get("id").cloned().filter(|v| !v.is_null()) else {
            continue; // notification
        };
        let reply = match msg.get("method").and_then(Value::as_str) {
            Some("initialize") => {
                let pv = msg
                    .pointer("/params/protocolVersion")
                    .and_then(Value::as_str)
                    .unwrap_or("2025-06-18");
                json!({
                    "jsonrpc": "2.0",
                    "id": id,
                    "result": {
                        "protocolVersion": pv,
                        "capabilities": { "tools": {} },
                        "serverInfo": { "name": "warren-noop", "version": env!("CARGO_PKG_VERSION") },
                    }
                })
            }
            Some("tools/list") => json!({ "jsonrpc": "2.0", "id": id, "result": { "tools": [] } }),
            Some("resources/list") => {
                json!({ "jsonrpc": "2.0", "id": id, "result": { "resources": [] } })
            }
            Some("prompts/list") => {
                json!({ "jsonrpc": "2.0", "id": id, "result": { "prompts": [] } })
            }
            _ => json!({ "jsonrpc": "2.0", "id": id, "result": {} }),
        };
        let mut out = stdout.lock();
        writeln!(out, "{reply}")?;
        out.flush()?;
    }
    Ok(())
}

struct BridgeState {
    child_in: Option<ChildStdin>,
    child_pid: Option<u32>,
    init_params: Option<Value>,
    stopping: bool,
}

const REINIT_ID: &str = "__warren_reinit__";

fn bridge_mcp(agent: &str, server: &str, spec: McpSpec) -> Result<()> {
    let sock = std::env::var("WARREN_SOCK").ok().filter(|s| !s.is_empty());
    let instr_path = agent_dir(agent).join(format!("{server}.instructions"));
    let state = Arc::new(Mutex::new(BridgeState {
        child_in: None,
        child_pid: None,
        init_params: None,
        stopping: false,
    }));

    let state_sup = Arc::clone(&state);
    let server_name = server.to_string();
    std::thread::spawn(move || {
        let mut delay = Duration::from_secs(1);
        let mut first = true;
        loop {
            if state_sup.lock().unwrap().stopping {
                return;
            }
            let started = Instant::now();
            let spawned = Command::new(&spec.command)
                .args(&spec.args)
                .envs(&spec.env)
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn();
            let Ok(mut child) = spawned else {
                std::thread::sleep(delay);
                delay = (delay * 2).min(Duration::from_secs(60));
                continue;
            };
            let child_out = child.stdout.take();
            {
                let mut st = state_sup.lock().unwrap();
                if st.stopping {
                    let _ = child.kill();
                    let _ = child.wait();
                    return;
                }
                st.child_pid = Some(child.id());
                let mut cin = child.stdin.take();
                // On a respawn, replay the MCP initialize handshake ourselves
                // so `matrix_channel.py` resumes syncing and pushing messages
                // without waiting for another client initialize (which never comes).
                if !first {
                    if let (Some(w), Some(params)) = (cin.as_mut(), st.init_params.as_ref()) {
                        let req = json!({
                            "jsonrpc": "2.0",
                            "id": REINIT_ID,
                            "method": "initialize",
                            "params": params,
                        });
                        let _ = writeln!(w, "{req}");
                        let _ = w.flush();
                    }
                }
                st.child_in = cin;
            }
            first = false;

            if let Some(out) = child_out {
                let reader = BufReader::new(out);
                for line in reader.lines() {
                    let Ok(line) = line else { break };
                    let trimmed = line.trim();
                    if trimmed.is_empty() {
                        continue;
                    }
                    if let Ok(msg) = serde_json::from_str::<Value>(trimmed) {
                        // Internal re-init handshake after a channel restart.
                        if msg.get("id").and_then(Value::as_str) == Some(REINIT_ID) {
                            let mut st = state_sup.lock().unwrap();
                            if let Some(w) = st.child_in.as_mut() {
                                let note = json!({
                                    "jsonrpc": "2.0",
                                    "method": "notifications/initialized",
                                });
                                let _ = writeln!(w, "{note}");
                                let _ = w.flush();
                            }
                            continue;
                        }
                        // Capture server instructions from `initialize` response.
                        if let Some(instr) = msg
                            .pointer("/result/instructions")
                            .and_then(Value::as_str)
                            .filter(|s| !s.trim().is_empty())
                        {
                            let _ = write_private(&instr_path, instr.as_bytes());
                        }
                        // Intercept pushed Matrix / sidecar messages and hand
                        // them to the agent's daemon for wake / hook delivery.
                        if msg.get("method").and_then(Value::as_str)
                            == Some("notifications/claude/channel")
                        {
                            if let Some(sock) = &sock {
                                let params = msg.get("params").cloned().unwrap_or(Value::Null);
                                send_channel_push(sock, as_channel_event(&server_name, &params));
                            }
                            continue;
                        }
                        // Answer server-initiated requests (ping) directly.
                        if msg.get("id").is_some_and(|v| !v.is_null())
                            && msg.get("method").is_some_and(Value::is_string)
                        {
                            let pong = json!({
                                "jsonrpc": "2.0",
                                "id": msg["id"],
                                "result": {},
                            });
                            let mut st = state_sup.lock().unwrap();
                            if let Some(w) = st.child_in.as_mut() {
                                let _ = writeln!(w, "{pong}");
                                let _ = w.flush();
                            }
                            continue;
                        }
                    }
                    let stdout = std::io::stdout();
                    let mut out_lock = stdout.lock();
                    if writeln!(out_lock, "{trimmed}").is_err() || out_lock.flush().is_err() {
                        break;
                    }
                }
            }
            {
                let mut st = state_sup.lock().unwrap();
                st.child_in = None;
                st.child_pid = None;
                if st.stopping {
                    let _ = child.kill();
                    let _ = child.wait();
                    return;
                }
            }
            let _ = child.wait();
            delay = if started.elapsed() > Duration::from_secs(60) {
                Duration::from_secs(1)
            } else {
                (delay * 2).min(Duration::from_secs(60))
            };
            std::thread::sleep(delay);
        }
    });

    let stdin = std::io::stdin();
    for line in stdin.lock().lines() {
        let Ok(line) = line else { break };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Ok(msg) = serde_json::from_str::<Value>(trimmed) {
            if msg.get("method").and_then(Value::as_str) == Some("initialize") {
                state.lock().unwrap().init_params = msg.get("params").cloned();
            }
        }
        // Wait briefly on startup if the supervisor is still spawning the child.
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let mut st = state.lock().unwrap();
            if let Some(w) = st.child_in.as_mut() {
                let _ = writeln!(w, "{trimmed}");
                let _ = w.flush();
                break;
            }
            drop(st);
            if Instant::now() >= deadline {
                break;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    let pid = {
        let mut st = state.lock().unwrap();
        st.stopping = true;
        st.child_in = None;
        st.child_pid.take()
    };
    if let Some(pid) = pid {
        unsafe { libc::kill(pid as i32, libc::SIGTERM) };
    }
    Ok(())
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
        assert!(cfg.talkback.is_none(), "talkback is opt-in");
    }

    #[test]
    fn talkback_reaches_only_agents_under_its_dirs() {
        let tb: Talkback =
            serde_json::from_str(r#"{"channel":"/t.py","dirs":["/w/open-audio"]}"#).unwrap();
        assert!(talkback_covers(&tb, "/w/open-audio"));
        assert!(talkback_covers(&tb, "/w/open-audio/open-eq"));
        assert!(
            !talkback_covers(&tb, "/w/open-audio-out"),
            "a sibling with the same prefix"
        );
        assert!(!talkback_covers(&tb, "/w/pancake"));
        let everyone: Talkback = serde_json::from_str(r#"{"channel":"/t.py"}"#).unwrap();
        assert!(talkback_covers(&everyone, "/anywhere"));
    }

    #[test]
    fn channel_events_match_claude_and_omp_xml_wire_format() {
        let params = json!({
            "content": "check \\frac{1}{2} & run tests",
            "meta": {
                "room": "!abc:example.org",
                "sender": "@sam:example.org",
                "quote": "a \"quoted\" & <raw>",
            }
        });
        let xml = as_channel_event(SERVER, &params);
        assert!(xml.starts_with("<channel source=\"warren-matrix\" "));
        assert!(xml.contains("room=\"!abc:example.org\""));
        assert!(xml.contains("sender=\"@sam:example.org\""));
        assert!(xml.contains("quote=\"a &quot;quoted&quot; &amp; <raw>\""));
        // Content is left verbatim so TeX backslashes and angle brackets arrive intact.
        assert!(xml.ends_with(">check \\frac{1}{2} & run tests</channel>"));
    }

    #[test]
    fn agy_plugin_bundles_mcp_servers_and_lifecycle_hooks() {
        let tmp = std::env::temp_dir().join(format!("warren-agy-plug-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&tmp);
        let cfg: Config = serde_json::from_str(
            r#"{
                "channel": "/c.py",
                "machine": "smq",
                "allowedUsers": ["@sam:x"],
                "inviters": ["@sam:x"],
                "talkback": { "channel": "/t.py" },
                "sidecars": { "marginalia": { "channel": "/m.py" } }
            }"#,
        )
        .unwrap();
        let dir = ensure_agy_plugin_in(&tmp, Path::new("/usr/local/bin/warren"), &cfg).unwrap();
        let mcp: Value =
            serde_json::from_slice(&std::fs::read(dir.join("mcp_config.json")).unwrap()).unwrap();
        assert_eq!(
            mcp["mcpServers"]["warren-matrix"]["args"],
            json!(["__mcp", "warren-matrix"])
        );
        assert_eq!(
            mcp["mcpServers"]["talkback"]["args"],
            json!(["__mcp", "talkback"])
        );
        assert_eq!(
            mcp["mcpServers"]["marginalia"]["args"],
            json!(["__mcp", "marginalia"])
        );

        let hooks: Value =
            serde_json::from_slice(&std::fs::read(dir.join("hooks.json")).unwrap()).unwrap();
        assert!(hooks["warren-matrix"]["SessionStart"].is_array());
        assert!(hooks["warren-matrix"]["PreInvocation"].is_array());
        assert!(hooks["warren-matrix"]["PostInvocation"].is_array());
        assert!(hooks["warren-matrix"]["Stop"].is_array());
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
