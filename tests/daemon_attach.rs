//! Headless integration tests: drive the real `warren` binary, spawning real
//! agent daemons around scripted children (no Claude, no tty on our side).

use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

// The crate isn't a library, so pull the protocol modules in directly
// (their `crate::spans` paths resolve against this test crate's root).
#[allow(dead_code)]
#[path = "../src/spans.rs"]
mod spans;
#[allow(dead_code)]
#[path = "../src/proto.rs"]
mod proto;
use proto::{FrameDecoder, HookState, MouseKind, Power, ToClient, ToDaemon};

const BIN: &str = env!("CARGO_BIN_EXE_warren");

struct TestHome {
    dir: PathBuf,
}

impl TestHome {
    fn new(tag: &str) -> Self {
        // Keep it short: macOS $TMPDIR is long enough to overflow sun_path,
        // which would (correctly) divert sockets to the fallback run dir.
        let dir = PathBuf::from(format!("/tmp/warren-it-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        TestHome { dir }
    }

    fn warren(&self, agent_cmd: &str) -> Command {
        let mut cmd = Command::new(BIN);
        cmd.env("WARREN_HOME", &self.dir)
            .env("WARREN_AGENT_CMD", agent_cmd)
            .env("WARREN_OUT_CAP", "65536");
        cmd
    }

    fn sock(&self, name: &str) -> PathBuf {
        self.dir.join("run").join(format!("{name}.sock"))
    }
}

impl Drop for TestHome {
    fn drop(&mut self) {
        // Kill any daemons still around, then remove the tree.
        if let Ok(entries) = std::fs::read_dir(self.dir.join("run")) {
            for entry in entries.flatten() {
                let _ = kill_via_socket(&entry.path());
            }
        }
        std::thread::sleep(Duration::from_millis(100));
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn kill_via_socket(sock: &Path) -> std::io::Result<()> {
    let mut s = UnixStream::connect(sock)?;
    s.write_all(&proto::encode_frame(&ToDaemon::Kill).unwrap())?;
    Ok(())
}

/// A test viewer: blocking socket + decoder + helpers to await frames.
struct Viewer {
    stream: UnixStream,
    decoder: FrameDecoder,
}

impl Viewer {
    fn connect(sock: &Path) -> Viewer {
        let stream = UnixStream::connect(sock).expect("connect to daemon");
        stream.set_read_timeout(Some(Duration::from_millis(200))).unwrap();
        Viewer { stream, decoder: FrameDecoder::new() }
    }

    fn send(&mut self, msg: &ToDaemon) {
        self.stream.write_all(&proto::encode_frame(msg).unwrap()).unwrap();
    }

    fn attach(sock: &Path, cols: u16, rows: u16) -> (Viewer, ToClient) {
        let mut v = Viewer::connect(sock);
        v.send(&ToDaemon::Attach { cols, rows });
        let snap = v
            .await_frame(5_000, |m| matches!(m, ToClient::Snapshot { .. }))
            .expect("snapshot after attach");
        (v, snap)
    }

    /// Read frames until one matches, with an overall deadline in ms.
    fn await_frame(
        &mut self,
        ms: u64,
        pred: impl Fn(&ToClient) -> bool,
    ) -> Option<ToClient> {
        let deadline = Instant::now() + Duration::from_millis(ms);
        let mut buf = [0u8; 65536];
        loop {
            while let Some(msg) = self.decoder.next::<ToClient>().expect("valid frame") {
                if std::env::var("WARREN_TEST_TRACE").is_ok() {
                    eprintln!("frame: {msg:?}");
                }
                if pred(&msg) {
                    return Some(msg);
                }
            }
            if Instant::now() >= deadline {
                return None;
            }
            match self.stream.read(&mut buf) {
                Ok(0) => return None,
                Ok(n) => {
                    if std::env::var("WARREN_TEST_TRACE").is_ok() {
                        eprintln!("viewer read: {n} bytes");
                    }
                    self.decoder.push(&buf[..n]);
                }
                Err(e) if e.kind() == ErrorKind::WouldBlock || e.kind() == ErrorKind::TimedOut => {
                    if std::env::var("WARREN_TEST_TRACE").is_ok() {
                        eprintln!("viewer read: timeout");
                    }
                }
                Err(e) => panic!("viewer read: {e}"),
            }
        }
    }
}

fn screen_text(screen: &[spans::LineSpans]) -> String {
    screen
        .iter()
        .map(|l| l.0.iter().map(|s| s.text.as_str()).collect::<String>())
        .collect::<Vec<_>>()
        .join("\n")
}

/// `warren new NAME DIR` — the working directory is what groups the sidebar.
fn new_agent_in(home: &TestHome, name: &str, dir: &Path, agent_cmd: &str) {
    std::fs::create_dir_all(dir).unwrap();
    let out = home
        .warren(agent_cmd)
        .args(["new", name, dir.to_str().unwrap()])
        .output()
        .expect("run warren new");
    assert!(out.status.success(), "warren new failed: {}", String::from_utf8_lossy(&out.stderr));
}

fn new_agent(home: &TestHome, name: &str, agent_cmd: &str) {
    let out = home.warren(agent_cmd).args(["new", name]).output().expect("run warren new");
    assert!(
        out.status.success(),
        "warren new failed: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
}

/// ssh, for the purposes of a test: drop the destination and run the rest
/// here against the far machine's home. Everything but the network.
fn fake_ssh(near: &TestHome, far: &TestHome) -> PathBuf {
    let shim = near.dir.join("fake-ssh");
    std::fs::write(
        &shim,
        format!(
            "#!/bin/sh\nshift\nexec env WARREN_HOME={} WARREN_AGENT_CMD='sleep 300' \"$@\"\n",
            far.dir.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();
    shim
}

/// Keep a screen up to date from snapshots and damage, the way a terminal
/// would, so a test can read what the dashboard is showing.
fn apply_frame(grid: &std::cell::RefCell<Vec<String>>, m: &ToClient) {
    let mut g = grid.borrow_mut();
    match m {
        ToClient::Snapshot { screen, .. } => {
            *g = screen.iter().map(|l| l.0.iter().map(|s| s.text.as_str()).collect()).collect();
        }
        ToClient::Damage { lines, .. } => {
            for (row, line) in lines {
                let r = *row as usize;
                if g.len() <= r {
                    g.resize(r + 1, String::new());
                }
                g[r] = line.0.iter().map(|s| s.text.as_str()).collect();
            }
        }
        _ => {}
    }
}

/// The sidebar column only, trimmed.
fn sidebar_of(grid: &std::cell::RefCell<Vec<String>>) -> Vec<String> {
    grid.borrow()
        .iter()
        .map(|r| r.chars().take(23).collect::<String>().trim_end().to_string())
        .collect()
}

// --------------------------------------------------------------------- tests

#[test]
fn attach_snapshot_input_echo_and_kill() {
    let home = TestHome::new("basic");
    new_agent(&home, "echo", "printf 'hello from agent\\n'; cat");
    let sock = home.sock("echo");

    let (mut viewer, snap) = Viewer::attach(&sock, 80, 24);
    let ToClient::Snapshot { cols, rows, screen, .. } = &snap else { unreachable!() };
    assert_eq!((*cols, *rows), (80, 24));
    // The agent races us: its greeting is either already in the snapshot or
    // arrives as the damage right after.
    let greeted = screen_text(screen).contains("hello from agent")
        || viewer
            .await_frame(5_000, |m| match m {
                ToClient::Damage { lines, .. } => lines.iter().any(|(_, l)| {
                    screen_text(std::slice::from_ref(l)).contains("hello from agent")
                }),
                _ => false,
            })
            .is_some();
    assert!(greeted, "the agent's first output reaches the viewer");

    // Typed input reaches the child's pty (echo mode bounces it back).
    viewer.send(&ToDaemon::Input(proto::b64_encode(b"typed-line\r")));
    let damage = viewer.await_frame(5_000, |m| {
        matches!(m, ToClient::Damage { lines, .. }
            if lines.iter().any(|(_, l)| l.0.iter().any(|s| s.text.contains("typed-line"))))
    });
    assert!(damage.is_some(), "echoed input arrives as damage");

    // warren ls sees it.
    let out = home.warren("unused").arg("ls").output().unwrap();
    let ls = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(ls.contains("echo"), "ls lists the agent: {ls}");

    // Kill → Exited frame → socket unlinked.
    let out = home.warren("unused").args(["kill", "echo"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let exited = viewer.await_frame(5_000, |m| matches!(m, ToClient::Exited { .. }));
    assert!(exited.is_some(), "viewer told about exit");
    assert!(!sock.exists(), "socket cleaned up");
}

#[test]
fn resize_reflows_and_resnapshots_all_viewers() {
    let home = TestHome::new("resize");
    new_agent(&home, "rsz", "printf 'resize me\\n'; cat");
    let sock = home.sock("rsz");

    let (mut a, _) = Viewer::attach(&sock, 80, 24);
    let (mut b, _) = Viewer::attach(&sock, 80, 24);

    a.send(&ToDaemon::Resize { cols: 100, rows: 30 });
    // BOTH viewers get the authoritative new snapshot.
    for v in [&mut a, &mut b] {
        let snap = v.await_frame(
            5_000,
            |m| matches!(m, ToClient::Snapshot { cols: 100, rows: 30, .. }),
        );
        assert!(snap.is_some(), "resized snapshot broadcast");
    }
}

#[test]
fn agent_exit_reports_status_and_unlinks() {
    let home = TestHome::new("exit");
    new_agent(&home, "mortal", "sleep 2; exit 7");
    let sock = home.sock("mortal");

    let (mut viewer, _) = Viewer::attach(&sock, 80, 24);
    let exited = viewer.await_frame(10_000, |m| matches!(m, ToClient::Exited { .. }));
    match exited {
        Some(ToClient::Exited { status }) => assert_eq!(status, 7),
        other => panic!("expected Exited, got {other:?}"),
    }
    // Daemon unlinks on the way out.
    let deadline = Instant::now() + Duration::from_secs(3);
    while sock.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(!sock.exists());
}

#[test]
fn hook_state_round_trip() {
    let home = TestHome::new("hook");
    new_agent(&home, "hooked", "cat");
    let sock = home.sock("hooked");

    // Run `warren hook attention` the way Claude's hooks would.
    let out = Command::new(BIN)
        .env("WARREN_SOCK", &sock)
        .args(["hook", "attention"])
        .output()
        .unwrap();
    assert!(out.status.success(), "hook must exit 0");

    let (_, snap) = Viewer::attach(&sock, 80, 24);
    let ToClient::Snapshot { state, .. } = snap else { unreachable!() };
    assert_eq!(state.hook, Some(HookState::Attention));
    // No stdin payload, so no conversation to resume yet.
    assert_eq!(state.session, None);

    // Claude pipes each hook a JSON payload carrying the session id; that is
    // what makes the agent sleepable (there is something to --resume).
    let mut hook = Command::new(BIN)
        .env("WARREN_SOCK", &sock)
        .args(["hook", "waiting"])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    hook.stdin
        .take()
        .unwrap()
        .write_all(br#"{"session_id":"0199b4c1-7f3e-4b2a-9d18-2c6f5a0e77bd","cwd":"/tmp"}"#)
        .unwrap();
    assert!(hook.wait().unwrap().success(), "hook must exit 0");

    let (_, snap) = Viewer::attach(&sock, 80, 24);
    let ToClient::Snapshot { state, .. } = snap else { unreachable!() };
    assert_eq!(state.hook, Some(HookState::Waiting));
    assert_eq!(
        state.session.as_deref(),
        Some("0199b4c1-7f3e-4b2a-9d18-2c6f5a0e77bd"),
        "the daemon learns what to --resume from the hook payload"
    );
    assert!(state.resumable);

    // Without WARREN_SOCK it's silently a no-op (claude outside warren).
    let out = Command::new(BIN).args(["hook", "working"]).output().unwrap();
    assert!(out.status.success());
}

/// Is that pid still around? (The daemon reaps its child, so a slept agent's
/// process is gone, not a zombie.)
fn pid_alive(pid: &str) -> bool {
    Command::new("kill")
        .args(["-0", pid])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

#[test]
fn sleep_stops_the_process_and_keeps_the_agent() {
    let home = TestHome::new("sleep");
    // Announces its own pid, then sits there like an idle claude.
    new_agent(&home, "napper", "printf 'awake as %s\\n' $$; sleep 300");
    let sock = home.sock("napper");

    let (mut viewer, snap) = Viewer::attach(&sock, 80, 24);
    let ToClient::Snapshot { screen, .. } = &snap else { unreachable!() };
    let mut text = screen_text(screen);
    if !text.contains("awake as") {
        let frame = viewer
            .await_frame(5_000, |m| matches!(m, ToClient::Damage { .. }))
            .expect("agent output");
        let ToClient::Damage { lines, .. } = frame else { unreachable!() };
        text = lines.iter().map(|(_, l)| screen_text(std::slice::from_ref(l))).collect();
    }
    let pid: String = text
        .split("awake as ")
        .nth(1)
        .expect("pid on screen")
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    assert!(pid_alive(&pid), "the agent process should be running");

    viewer.send(&ToDaemon::Sleep);
    let asleep = viewer
        .await_frame(10_000, |m| {
            matches!(m, ToClient::StateChanged(s) if s.power == Power::Asleep)
        })
        .expect("agent reports itself asleep");
    let ToClient::StateChanged(state) = asleep else { unreachable!() };
    assert_eq!(state.hook, None, "no process, no lifecycle state");

    // The process is gone — that's the whole point — but the agent is not.
    let deadline = Instant::now() + Duration::from_secs(5);
    while pid_alive(&pid) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!pid_alive(&pid), "sleeping must kill the agent's process group");
    assert!(sock.exists(), "the daemon (and so the tab) outlives its process");

    // A fresh viewer still gets the agent, its metadata and its last screen.
    let (_, snap) = Viewer::attach(&sock, 80, 24);
    let ToClient::Snapshot { screen, state, meta, .. } = &snap else { unreachable!() };
    assert_eq!(meta.name, "napper");
    assert_eq!(state.power, Power::Asleep);
    assert!(screen_text(screen).contains("awake as"), "the last frame stays readable");

    // Waking respawns it: same agent, new process.
    viewer.send(&ToDaemon::Wake);
    let frame = viewer
        .await_frame(10_000, |m| match m {
            ToClient::Damage { lines, .. } => {
                lines.iter().any(|(_, l)| screen_text(std::slice::from_ref(l)).contains("awake as"))
            }
            _ => false,
        })
        .expect("the woken agent paints again");
    let ToClient::Damage { lines, .. } = frame else { unreachable!() };
    let text: String = lines.iter().map(|(_, l)| screen_text(std::slice::from_ref(l))).collect();
    let new_pid: String = text
        .split("awake as ")
        .nth(1)
        .unwrap()
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    assert_ne!(new_pid, pid, "wake starts a new process");
    assert!(pid_alive(&new_pid));
}

#[test]
fn keys_typed_at_a_sleeping_agent_land_in_the_resumed_one() {
    let home = TestHome::new("buffer");
    new_agent(&home, "dozy", "cat");
    let sock = home.sock("dozy");
    let (mut viewer, _) = Viewer::attach(&sock, 80, 24);

    viewer.send(&ToDaemon::Sleep);
    viewer
        .await_frame(10_000, |m| matches!(m, ToClient::StateChanged(s) if s.power == Power::Asleep))
        .expect("asleep");

    // Typing at a sleeping agent: the daemon holds the bytes…
    viewer.send(&ToDaemon::Input(proto::b64_encode(b"knock knock\r")));
    assert!(
        viewer.await_frame(500, |m| matches!(m, ToClient::Damage { .. })).is_none(),
        "a sleeping agent paints nothing"
    );
    // …and hands them to the process the wake starts.
    viewer.send(&ToDaemon::Wake);
    let frame = viewer
        .await_frame(10_000, |m| match m {
            ToClient::Damage { lines, .. } => lines
                .iter()
                .any(|(_, l)| screen_text(std::slice::from_ref(l)).contains("knock knock")),
            _ => false,
        });
    assert!(frame.is_some(), "buffered keystrokes reach the resumed agent");
}

#[test]
fn a_wake_that_fails_leaves_the_agent_asleep() {
    let home = TestHome::new("badwake");
    // Starts fine; refuses to start again once the marker exists — standing
    // in for a `claude --resume` whose session has gone missing.
    let marker = home.dir.join("no-resume");
    let cmd = format!(
        "if [ -f {m} ]; then printf 'cannot resume\\r\\n'; exit 3; fi; printf 'up\\r\\n'; sleep 300",
        m = marker.display()
    );
    new_agent(&home, "doomed", &cmd);
    let sock = home.sock("doomed");
    let (mut viewer, _) = Viewer::attach(&sock, 80, 24);

    viewer.send(&ToDaemon::Sleep);
    viewer
        .await_frame(10_000, |m| matches!(m, ToClient::StateChanged(s) if s.power == Power::Asleep))
        .expect("asleep");

    std::fs::write(&marker, b"").unwrap();
    viewer.send(&ToDaemon::Wake);

    // The agent tries, fails, and settles back to asleep — with the reason on
    // screen. What it must NOT do is take its own tab down.
    let back = viewer.await_frame(10_000, |m| {
        matches!(m, ToClient::StateChanged(s) if s.power == Power::Asleep)
    });
    assert!(back.is_some(), "a failed wake falls back to asleep");
    assert!(sock.exists(), "the agent outlives a resume it could not do");

    let (_, snap) = Viewer::attach(&sock, 80, 24);
    let ToClient::Snapshot { screen, state, .. } = &snap else { unreachable!() };
    assert_eq!(state.power, Power::Asleep);
    assert!(
        screen_text(screen).contains("cannot resume"),
        "claude's own complaint stays readable: {:?}",
        screen_text(screen)
    );

    // And it can still be woken once the problem goes away.
    std::fs::remove_file(&marker).unwrap();
    viewer.send(&ToDaemon::Wake);
    let up = viewer.await_frame(10_000, |m| match m {
        ToClient::Damage { lines, .. } => {
            lines.iter().any(|(_, l)| screen_text(std::slice::from_ref(l)).contains("up"))
        }
        _ => false,
    });
    assert!(up.is_some(), "the agent comes back when the resume can succeed");
}

#[test]
fn closing_an_agent_just_after_waking_it_still_closes_it() {
    let home = TestHome::new("wakekill");
    new_agent(&home, "brief", "printf 'up\\r\\n'; sleep 300");
    let sock = home.sock("brief");
    let (mut viewer, _) = Viewer::attach(&sock, 80, 24);

    viewer.send(&ToDaemon::Sleep);
    viewer
        .await_frame(10_000, |m| matches!(m, ToClient::StateChanged(s) if s.power == Power::Asleep))
        .expect("asleep");
    viewer.send(&ToDaemon::Wake);
    viewer
        .await_frame(10_000, |m| matches!(m, ToClient::Damage { .. }))
        .expect("awake again");

    // Inside the window where a dying child means "the resume failed" — but
    // this death was asked for, and must close the agent, not re-sleep it.
    viewer.send(&ToDaemon::Kill);
    let exited = viewer.await_frame(10_000, |m| matches!(m, ToClient::Exited { .. }));
    assert!(exited.is_some(), "kill right after a wake still ends the agent");
    let deadline = Instant::now() + Duration::from_secs(5);
    while sock.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!sock.exists(), "and unlinks its socket");
}

#[test]
fn closing_a_sleeping_agent_ends_its_daemon() {
    let home = TestHome::new("sleepkill");
    new_agent(&home, "dozer", "sleep 300");
    let sock = home.sock("dozer");
    let (mut viewer, _) = Viewer::attach(&sock, 80, 24);

    viewer.send(&ToDaemon::Sleep);
    viewer
        .await_frame(10_000, |m| matches!(m, ToClient::StateChanged(s) if s.power == Power::Asleep))
        .expect("asleep");

    // No child means no child exit to end the daemon: Kill has to do it.
    viewer.send(&ToDaemon::Kill);
    let deadline = Instant::now() + Duration::from_secs(5);
    while sock.exists() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(!sock.exists(), "closing a sleeping agent must unlink its socket");
}

#[test]
fn stalled_viewer_is_dropped_and_never_blocks_the_daemon() {
    let home = TestHome::new("stall");
    // A chatty agent: continuous output forever.
    new_agent(&home, "noisy", "while :; do printf 'spam %s\\n' $RANDOM; done");
    let sock = home.sock("noisy");

    // Viewer that attaches and then never reads — v0's freeze recipe.
    // (Set the timeout now: macOS rejects setsockopt once the peer drops us.)
    let mut stalled = UnixStream::connect(&sock).unwrap();
    stalled.set_read_timeout(Some(Duration::from_secs(10))).unwrap();
    stalled
        .write_all(&proto::encode_frame(&ToDaemon::Attach { cols: 80, rows: 24 }).unwrap())
        .unwrap();

    // While the stalled viewer's queue fills, the daemon must keep serving:
    // repeated Queries and a live attach all work throughout.
    let busy_until = Instant::now() + Duration::from_secs(3);
    let mut queries = 0;
    while Instant::now() < busy_until {
        let mut v = Viewer::connect(&sock);
        v.send(&ToDaemon::Query);
        let meta = v.await_frame(2_000, |m| matches!(m, ToClient::MetaChanged(_)));
        assert!(meta.is_some(), "daemon answers Query #{queries} while a viewer stalls");
        queries += 1;
        std::thread::sleep(Duration::from_millis(100));
    }
    assert!(queries >= 10, "daemon stayed responsive ({queries} queries)");

    // The stalled connection got dropped at the cap (read returns EOF).
    let mut drain = vec![0u8; 1 << 20];
    let mut got_eof = false;
    loop {
        match stalled.read(&mut drain) {
            Ok(0) => {
                got_eof = true;
                break;
            }
            Ok(_) => {} // the bytes queued before the drop
            Err(_) => break,
        }
    }
    assert!(got_eof, "stalled viewer disconnected by the daemon");

    // A fresh attach still gets a clean snapshot afterwards.
    let (_, snap) = Viewer::attach(&sock, 80, 24);
    assert!(matches!(snap, ToClient::Snapshot { .. }));
}

#[test]
fn title_sync_policy() {
    let home = TestHome::new("title");
    // Claude-style title with a leading spinner glyph; then the idle title.
    new_agent(
        &home,
        "titled",
        "printf '\\033]0;\\xe2\\x9c\\xb3 Fix the bug\\007'; sleep 0.3; printf '\\033]0;Claude Code\\007'; cat",
    );
    let sock = home.sock("titled");

    // The title may land before any viewer attaches, so poll snapshots:
    // spinner glyph stripped, task title mirrored.
    let deadline = Instant::now() + Duration::from_secs(3);
    let mut display = String::new();
    while Instant::now() < deadline {
        let (_, snap) = Viewer::attach(&sock, 80, 24);
        let ToClient::Snapshot { meta, .. } = snap else { unreachable!() };
        display = meta.display;
        if display == "Fix the bug" {
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    assert_eq!(display, "Fix the bug", "title mirrored with spinner stripped");
    // The idle title ("Claude Code", emitted 300ms in) must NOT overwrite it.
    std::thread::sleep(Duration::from_millis(600));
    let (_, snap) = Viewer::attach(&sock, 80, 24);
    let ToClient::Snapshot { meta, .. } = snap else { unreachable!() };
    assert_eq!(meta.display, "Fix the bug", "idle title ignored");
}

#[test]
fn pinned_name_resists_title_sync() {
    let home = TestHome::new("pin");
    new_agent(&home, "pinned", "sleep 0.4; printf '\\033]0;Sneaky title\\007'; cat");
    let sock = home.sock("pinned");

    let (mut viewer, _) = Viewer::attach(&sock, 80, 24);
    viewer.send(&ToDaemon::SetMeta {
        name: Some("my-name".into()),
        color: None,
        pinned: Some(true),
        slot: None,
    });
    let renamed = viewer.await_frame(5_000, |m| {
        matches!(m, ToClient::MetaChanged(meta) if meta.display == "my-name" && meta.pinned)
    });
    assert!(renamed.is_some(), "manual rename lands");

    // Give the title plenty of time to arrive after the rename.
    std::thread::sleep(Duration::from_millis(800));
    let (_, snap) = Viewer::attach(&sock, 80, 24);
    let ToClient::Snapshot { meta, .. } = snap else { unreachable!() };
    assert_eq!(meta.display, "my-name", "pinned name wins over Claude's title");
}

#[test]
fn mouse_forwarded_only_when_subscribed() {
    let home = TestHome::new("mouse");
    // The app subscribes to drag tracking + SGR by WRITING the DECSET (only
    // app output reaches the emulator); cat -v then makes the forwarded
    // mouse bytes visible on screen.
    new_agent(&home, "mousey", "printf '\\033[?1002h\\033[?1006h'; cat -v");
    let sock = home.sock("mousey");
    let (mut viewer, _) = Viewer::attach(&sock, 80, 24);

    // Wait until the daemon reports the subscription, so the click can't race
    // the app's DECSET. (Snapshot may already carry it; ask fresh.)
    let deadline = Instant::now() + Duration::from_secs(3);
    loop {
        let (_, snap) = Viewer::attach(&sock, 80, 24);
        let ToClient::Snapshot { mouse, .. } = snap else { unreachable!() };
        if mouse == proto::MouseProto::Drag {
            break;
        }
        assert!(Instant::now() < deadline, "agent never subscribed to mouse");
        std::thread::sleep(Duration::from_millis(50));
    }
    viewer.send(&ToDaemon::Mouse { kind: proto::MouseKind::Down(0), col: 4, row: 2, mods: 0 });

    // cat -v echoes the click's SGR encoding: ^[[<0;5;3M.
    let damage = viewer.await_frame(5_000, |m| {
        matches!(m, ToClient::Damage { lines, .. }
            if lines.iter().any(|(_, l)| l.0.iter().any(|s| s.text.contains("[<0;5;3M"))))
    });
    assert!(damage.is_some(), "subscribed click reaches the app, SGR-encoded");
}

#[test]
fn unique_names_and_slots() {
    let home = TestHome::new("uniq");
    new_agent(&home, "twin", "cat");
    new_agent(&home, "twin", "cat");
    let out = home.warren("unused").arg("ls").output().unwrap();
    let ls = String::from_utf8_lossy(&out.stdout).to_string();
    assert!(ls.contains("twin"), "{ls}");
    assert!(ls.contains("twin-2"), "auto-suffixed name: {ls}");
    assert!(home.sock("twin").exists() && home.sock("twin-2").exists());
}

#[test]
fn big_frame_tail_flushes_while_daemon_is_idle() {
    // Regression: a Damage frame bigger than the socket buffer (8KiB on
    // macOS) hits WouldBlock mid-write; the tail sits in the conn queue. The
    // daemon must arm writable interest BEFORE sleeping, or the tail waits
    // for the next unrelated event and every viewer renders one redraw
    // behind, "unstuck" by each keypress.
    let home = TestHome::new("bigframe");

    // One burst of 1200 cells, every one a different color, so RLE can't
    // merge spans and the encoded frame is far larger than the socket
    // buffers (yet under the 64KiB test out-cap: overflow must NOT trip).
    let mut burst = String::new();
    for i in 0..1200u32 {
        burst.push_str(&format!("\x1b[38;5;{}mX", i % 256));
    }
    burst.push_str("\x1b[0m");
    let payload = home.dir.join("burst");
    std::fs::write(&payload, burst).unwrap();

    // The delay guarantees the burst lands AFTER the attach snapshot, in one
    // coalesced flush, while the viewer below is deliberately not reading.
    new_agent(&home, "big", &format!("sleep 1; cat {}; sleep 30", payload.display()));

    let (mut viewer, _snap) = Viewer::attach(&home.sock("big"), 120, 30);

    // Don't read while the daemon flushes: the kernel buffers ~16KiB, the
    // daemon queues the rest and goes to sleep — the child stays silent and
    // we send nothing, so writability is the ONLY thing that can wake it.
    std::thread::sleep(Duration::from_millis(2500));

    let seen = std::cell::Cell::new(0usize);
    let done = viewer.await_frame(5_000, |m| {
        let text: String = match m {
            ToClient::Damage { lines, .. } => {
                lines.iter().flat_map(|(_, l)| l.0.iter()).map(|s| s.text.as_str()).collect()
            }
            ToClient::Snapshot { screen, .. } => screen_text(screen),
            _ => return false,
        };
        seen.set(seen.get() + text.matches('X').count());
        seen.get() >= 1200
    });
    assert!(
        done.is_some(),
        "frame tail never arrived (got {}/1200 cells): daemon slept without writable interest",
        seen.get()
    );
}
// Scratch discriminator: is the DAEMON slow to deliver an input-triggered
// burst to a continuously-reading viewer? (The C-g-opens-editor shape.)

#[test]
fn input_triggered_burst_arrives_promptly() {
    let home = TestHome::new("trigburst");
    // Each input line triggers 600 distinct-color cells (~30KB encoded).
    new_agent(
        &home,
        "trig",
        "while read x; do i=0; while [ $i -lt 600 ]; do printf '\\033[38;5;%dmY' $((i%256)); i=$((i+1)); done; printf '\\033[0m\\n'; done",
    );
    let (mut viewer, _snap) = Viewer::attach(&home.sock("trig"), 120, 30);
    std::thread::sleep(Duration::from_millis(300)); // let the shell reach read

    for round in 1..=3 {
        let t0 = Instant::now();
        viewer.send(&ToDaemon::Input(proto::b64_encode(b"\n")));
        let seen = std::cell::Cell::new(0usize);
        let done = viewer.await_frame(4_000, |m| {
            let text: String = match m {
                ToClient::Damage { lines, .. } => {
                    lines.iter().flat_map(|(_, l)| l.0.iter()).map(|s| s.text.as_str()).collect()
                }
                ToClient::Snapshot { screen, .. } => screen_text(screen),
                _ => return false,
            };
            seen.set(seen.get() + text.matches('Y').count());
            seen.get() >= 600
        });
        let ms = t0.elapsed().as_millis();
        assert!(done.is_some(), "round {round}: burst never arrived ({}Y)", seen.get());
        assert!(ms < 1500, "round {round}: burst took {ms}ms — daemon-side stall");
    }
}

/// End-to-end: a REAL dashboard showing agents from two machines at once.
///
/// The far machine is this one wearing a different WARREN_HOME, reached
/// through a stand-in for ssh (WARREN_SSH) that runs the command here. That
/// exercises everything but the network itself: the roster, a `warren __pipe`
/// per agent, the socketpair each becomes, and the sidebar that has to number
/// folders straight through both machines.
#[test]
fn a_second_machine_shares_the_sidebar() {
    let far = TestHome::new("far");
    let near = TestHome::new("near");
    let outer = TestHome::new("nearout");

    // Two directories over there, one here.
    new_agent_in(&far, "spork", &far.dir.join("Games"), "sleep 300");
    new_agent_in(&far, "terrain", &far.dir.join("Games"), "sleep 300");
    new_agent_in(&far, "grader", &far.dir.join("Courses"), "sleep 300");
    new_agent_in(&near, "svm", &near.dir.join("Research"), "sleep 300");

    // The near machine is told where the far one is.
    std::fs::write(near.dir.join("hosts"), format!("smq  {BIN}\n")).unwrap();

    // ssh, for the purposes of this test: drop the destination, run the rest
    // here against the far machine's home.
    let shim = near.dir.join("fake-ssh");
    std::fs::write(
        &shim,
        format!(
            "#!/bin/sh\nshift\nexec env WARREN_HOME={} WARREN_AGENT_CMD='sleep 300' \"$@\"\n",
            far.dir.display()
        ),
    )
    .unwrap();
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(&shim, std::fs::Permissions::from_mode(0o755)).unwrap();

    let dash_cmd = format!(
        "WARREN_HOME={} WARREN_SSH={} {} up",
        near.dir.display(),
        shim.display(),
        BIN
    );
    new_agent(&outer, "dash", &dash_cmd);
    let (mut viewer, snap) = Viewer::attach(&outer.sock("dash"), 100, 22);

    let grid = std::cell::RefCell::new(Vec::<String>::new());
    let apply = |grid: &std::cell::RefCell<Vec<String>>, m: &ToClient| {
        let mut g = grid.borrow_mut();
        match m {
            ToClient::Snapshot { screen, .. } => {
                *g = screen.iter().map(|l| l.0.iter().map(|s| s.text.as_str()).collect()).collect();
            }
            ToClient::Damage { lines, .. } => {
                for (row, line) in lines {
                    let r = *row as usize;
                    if g.len() <= r {
                        g.resize(r + 1, String::new());
                    }
                    g[r] = line.0.iter().map(|s| s.text.as_str()).collect();
                }
            }
            _ => {}
        }
    };
    apply(&grid, &snap);
    fn sidebar(grid: &std::cell::RefCell<Vec<String>>) -> Vec<String> {
        grid.borrow()
            .iter()
            .map(|r| r.chars().take(23).collect::<String>().trim_end().to_string())
            .collect()
    }

    // Everything from both machines, in one list.
    let up = viewer.await_frame(20_000, |m| {
        apply(&grid, m);
        let rows = sidebar(&grid);
        rows.iter().any(|r| r.contains("grader")) && rows.iter().any(|r| r.contains("svm"))
    });
    assert!(up.is_some(), "both machines' agents arrived: {:?}", sidebar(&grid));

    let rows = sidebar(&grid);
    let smq = rows.iter().position(|r| r.trim() == "smq").expect("a heading for the far machine");
    let here = rows.iter().position(|r| r.contains("Research/")).unwrap();
    assert!(here < smq, "this machine comes first: {rows:?}");
    // Folder numbers run straight through, so two digits still reach any of
    // them — 1 is local, 2 and 3 are one machine away.
    assert!(rows.iter().any(|r| r.trim() == "1 Research/"), "{rows:?}");
    assert!(rows.iter().any(|r| r.trim() == "2 Games/"), "{rows:?}");
    assert!(rows.iter().any(|r| r.trim() == "3 Courses/"), "{rows:?}");
    // And the sidebar says nothing about which machine an agent is on beyond
    // the heading it sits under.
    assert!(!rows.iter().any(|r| r.contains("smq:")), "no host tags on rows: {rows:?}");

    // ^Space 2 1 reaches an agent on the other machine.
    viewer.send(&ToDaemon::Input(proto::b64_encode(b"\x0021")));
    let landed = viewer.await_frame(10_000, |m| {
        apply(&grid, m);
        grid.borrow().last().map(|s| s.contains("spork")).unwrap_or(false)
    });
    assert!(landed.is_some(), "two digits crossed the machine: {:?}", grid.borrow().last());
}

/// End-to-end, on a REAL dashboard: agents are grouped into folders by their
/// working directory, `^Space <folder> <agent>` reaches one, and clicking a
/// folder header folds that directory away.
#[test]
fn sidebar_groups_agents_by_directory_and_two_digits_navigate() {
    let inner = TestHome::new("folders");
    let research = inner.dir.join("Research");
    let phylogen = inner.dir.join("Developer").join("Phylogen");
    new_agent_in(&inner, "svm", &research, "sleep 300");
    new_agent_in(&inner, "splitr", &research, "sleep 300");
    new_agent_in(&inner, "phylo", &phylogen, "sleep 300");

    let outer = TestHome::new("foldersout");
    let dash_cmd = format!("WARREN_HOME={} {} up", inner.dir.display(), BIN);
    new_agent(&outer, "dash", &dash_cmd);
    let (mut viewer, snap) = Viewer::attach(&outer.sock("dash"), 100, 20);

    let grid = std::cell::RefCell::new(Vec::<String>::new());
    let apply = |grid: &std::cell::RefCell<Vec<String>>, m: &ToClient| {
        let mut g = grid.borrow_mut();
        match m {
            ToClient::Snapshot { screen, .. } => {
                *g = screen.iter().map(|l| l.0.iter().map(|s| s.text.as_str()).collect()).collect();
            }
            ToClient::Damage { lines, .. } => {
                for (row, line) in lines {
                    let r = *row as usize;
                    if g.len() <= r {
                        g.resize(r + 1, String::new());
                    }
                    g[r] = line.0.iter().map(|s| s.text.as_str()).collect();
                }
            }
            _ => {}
        }
    };
    apply(&grid, &snap);
    /// The sidebar column only, trimmed.
    fn sidebar(grid: &std::cell::RefCell<Vec<String>>) -> Vec<String> {
        grid.borrow()
            .iter()
            .map(|r| r.chars().take(23).collect::<String>().trim_end().to_string())
            .collect()
    }

    let up = viewer.await_frame(15_000, |m| {
        apply(&grid, m);
        sidebar(&grid).iter().any(|r| r.contains("Phylogen/"))
    });
    assert!(up.is_some(), "dashboard came up with folders: {:?}", sidebar(&grid));

    // A folder is the directory itself, named by its own last component —
    // never the path that got you there.
    let rows = sidebar(&grid);
    let research_row = rows.iter().position(|r| r.contains("Research/")).unwrap();
    assert_eq!(rows[research_row].trim(), "1 Research/");
    assert!(rows[research_row + 1].contains("├ 1 svm"), "{rows:?}");
    assert!(rows[research_row + 2].contains("└ 2 splitr"), "{rows:?}");
    let phylo_row = rows.iter().position(|r| r.contains("Phylogen/")).unwrap();
    assert_eq!(rows[phylo_row].trim(), "2 Phylogen/");
    assert!(rows[phylo_row + 1].contains("└ 1 phylo"), "{rows:?}");
    assert!(
        !rows.iter().any(|r| r.contains(&inner.dir.display().to_string())),
        "no full paths in the sidebar: {rows:?}"
    );

    // ^Space 1 2 — folder one, agent two.
    viewer.send(&ToDaemon::Input(proto::b64_encode(b"\x001")));
    let armed = viewer.await_frame(5_000, |m| {
        apply(&grid, m);
        grid.borrow().last().map(|s| s.contains("go to  1 Research/")).unwrap_or(false)
    });
    assert!(armed.is_some(), "first digit arms the folder: {:?}", grid.borrow().last());
    viewer.send(&ToDaemon::Input(proto::b64_encode(b"2")));
    let landed = viewer.await_frame(5_000, |m| {
        apply(&grid, m);
        grid.borrow().last().map(|s| s.contains("splitr")).unwrap_or(false)
    });
    assert!(landed.is_some(), "second digit lands on it: {:?}", grid.borrow().last());

    // Click the folder header: the directory folds away, agents keep running.
    viewer.send(&ToDaemon::Mouse {
        kind: MouseKind::Down(0),
        col: 3,
        row: research_row as u16,
        mods: 0,
    });
    viewer.send(&ToDaemon::Mouse {
        kind: MouseKind::Up(0),
        col: 3,
        row: research_row as u16,
        mods: 0,
    });
    let folded = viewer.await_frame(5_000, |m| {
        apply(&grid, m);
        let rows = sidebar(&grid);
        rows.iter().any(|r| r.contains("Research/") && r.contains('▸'))
            && !rows.iter().any(|r| r.contains("svm"))
    });
    assert!(folded.is_some(), "clicking a folder folds it: {:?}", sidebar(&grid));

    // And opens it again.
    viewer.send(&ToDaemon::Mouse {
        kind: MouseKind::Down(0),
        col: 3,
        row: research_row as u16,
        mods: 0,
    });
    viewer.send(&ToDaemon::Mouse {
        kind: MouseKind::Up(0),
        col: 3,
        row: research_row as u16,
        mods: 0,
    });
    let opened = viewer.await_frame(5_000, |m| {
        apply(&grid, m);
        sidebar(&grid).iter().any(|r| r.contains("svm"))
    });
    assert!(opened.is_some(), "clicking again opens it: {:?}", sidebar(&grid));
}

/// End-to-end: a REAL dashboard (running as an agent under an outer daemon,
/// which provides its pty) viewing an inner agent that bursts a vis-like
/// screen (alt-screen toggle + 600 colored cells) when poked. The burst must
/// render on the dashboard's actual output WITHOUT any further keypress.
#[test]
fn dashboard_paints_triggered_burst_without_extra_keys() {
    let inner = TestHome::new("e2ein");
    new_agent(
        &inner,
        "trig",
        "while read x; do printf '\\033[?1049l\\033[?1049h'; i=0; while [ $i -lt 600 ]; do printf '\\033[38;5;%dmY' $((i%256)); i=$((i+1)); done; printf '\\033[0m'; done",
    );

    let outer = TestHome::new("e2eout");
    let dash_cmd = format!("WARREN_HOME={} {} up", inner.dir.display(), BIN);
    new_agent(&outer, "dash", &dash_cmd);

    let (mut viewer, snap) = Viewer::attach(&outer.sock("dash"), 140, 40);

    // Local model of the dashboard's screen: apply Snapshot/Damage rows.
    let grid = std::cell::RefCell::new(Vec::<String>::new());
    let apply = |grid: &std::cell::RefCell<Vec<String>>, m: &ToClient| {
        let mut g = grid.borrow_mut();
        match m {
            ToClient::Snapshot { screen, .. } => {
                *g = screen.iter().map(|l| l.0.iter().map(|s| s.text.as_str()).collect()).collect();
            }
            ToClient::Damage { lines, .. } => {
                for (row, line) in lines {
                    let r = *row as usize;
                    if g.len() <= r {
                        g.resize(r + 1, String::new());
                    }
                    g[r] = line.0.iter().map(|s| s.text.as_str()).collect();
                }
            }
            _ => {}
        }
    };
    apply(&grid, &snap);

    // Wait for the dashboard to attach the inner agent (sidebar shows it).
    let up = viewer.await_frame(10_000, |m| {
        apply(&grid, m);
        grid.borrow().iter().any(|r| r.contains("trig"))
    });
    assert!(up.is_some(), "dashboard came up showing the inner agent");

    // The C-g moment: one key through the whole stack.
    viewer.send(&ToDaemon::Input(proto::b64_encode(b"\r")));
    let t0 = Instant::now();
    let drawn = viewer.await_frame(5_000, |m| {
        apply(&grid, m);
        grid.borrow().iter().map(|r| r.matches('Y').count()).sum::<usize>() >= 300
    });
    let ms = t0.elapsed().as_millis();
    eprintln!("burst rendered by the dashboard in {ms}ms");
    assert!(drawn.is_some(), "dashboard never painted the burst without another key");
    assert!(ms < 2000, "dashboard took {ms}ms to paint an input-triggered burst");
}

/// A machine with nothing running on it is still a machine that answered.
/// The roster reports an empty list as a list, so the sidebar stops saying
/// "connecting…" about a machine that is connected — which is what it would
/// say if the ssh had never landed at all. A quiet machine then keeps no
/// heading, which is the rule for any machine with nothing on it.
#[test]
fn a_machine_with_no_agents_still_says_it_is_there() {
    let far = TestHome::new("emptyfar");
    let near = TestHome::new("emptynear");
    let outer = TestHome::new("emptyout");

    // Nothing over there at all; one agent here so the sidebar has a shape.
    new_agent_in(&near, "svm", &near.dir.join("Research"), "sleep 300");
    std::fs::create_dir_all(far.dir.join("run")).unwrap();
    std::fs::write(near.dir.join("hosts"), format!("faraway  {BIN}\n")).unwrap();

    let shim = fake_ssh(&near, &far);
    let dash_cmd =
        format!("WARREN_HOME={} WARREN_SSH={} {} up", near.dir.display(), shim.display(), BIN);
    new_agent(&outer, "dash", &dash_cmd);
    let (mut viewer, snap) = Viewer::attach(&outer.sock("dash"), 100, 22);

    let grid = std::cell::RefCell::new(Vec::<String>::new());
    apply_frame(&grid, &snap);
    // Give it well past a roster tick to say "connecting…" if it were going
    // to: the sidebar is up, and what matters is what it settles on.
    let settled = viewer.await_frame(20_000, |m| {
        apply_frame(&grid, m);
        let rows = sidebar_of(&grid);
        rows.iter().any(|r| r.contains("Research/")) && !rows.iter().any(|r| r.contains("faraway"))
    });
    assert!(settled.is_some(), "no word about connecting: {:?}", sidebar_of(&grid));

    // And it stays that way — the roster is current, not merely unheard.
    std::thread::sleep(Duration::from_millis(2_500));
    let _ = viewer.await_frame(500, |m| {
        apply_frame(&grid, m);
        false
    });
    let rows = sidebar_of(&grid);
    assert!(
        !rows.iter().any(|r| r.contains("connecting") || r.contains("reconnecting")),
        "an answering machine says nothing about connecting: {rows:?}"
    );
}

/// The new-agent form's first field is the machine, and picking one sends
/// the whole form there: `warren new` runs on that machine, which names the
/// agent and gives it a slot, and the row arrives in its next roster. The
/// near machine gains nothing at all.
#[test]
fn the_form_creates_an_agent_on_another_machine() {
    let far = TestHome::new("mkfar");
    let near = TestHome::new("mknear");
    let outer = TestHome::new("mkout");

    new_agent_in(&far, "spork", &far.dir.join("Games"), "sleep 300");
    new_agent_in(&near, "svm", &near.dir.join("Research"), "sleep 300");
    let burrow = far.dir.join("Burrow");
    std::fs::create_dir_all(&burrow).unwrap();
    std::fs::write(near.dir.join("hosts"), format!("smq  {BIN}\n")).unwrap();

    let shim = fake_ssh(&near, &far);
    let dash_cmd =
        format!("WARREN_HOME={} WARREN_SSH={} {} up", near.dir.display(), shim.display(), BIN);
    new_agent(&outer, "dash", &dash_cmd);
    let (mut viewer, snap) = Viewer::attach(&outer.sock("dash"), 100, 22);

    let grid = std::cell::RefCell::new(Vec::<String>::new());
    apply_frame(&grid, &snap);
    let up = viewer.await_frame(20_000, |m| {
        apply_frame(&grid, m);
        sidebar_of(&grid).iter().any(|r| r.contains("spork"))
    });
    assert!(up.is_some(), "both machines are up: {:?}", sidebar_of(&grid));

    // ^Space n opens the form; Machine leads it, and `l` moves off "here".
    viewer.send(&ToDaemon::Input(proto::b64_encode(b"\x00n")));
    let form = viewer.await_frame(10_000, |m| {
        apply_frame(&grid, m);
        grid.borrow().iter().any(|r| r.contains("Machine"))
    });
    assert!(form.is_some(), "the form offers a machine: {:?}", grid.borrow().clone());
    viewer.send(&ToDaemon::Input(proto::b64_encode(b"l")));
    let picked = viewer.await_frame(10_000, |m| {
        apply_frame(&grid, m);
        grid.borrow().iter().any(|r| r.contains("new claude agent on smq"))
    });
    assert!(picked.is_some(), "the form says where it will run: {:?}", grid.borrow().clone());

    // Tab to Title, name it, Tab to Root dir, clear the `~` the machine
    // switch left there, and give it one over on the far side.
    let mut keys: Vec<u8> = b"\t\t\tremote-made\t".to_vec();
    keys.extend([0x7f; 4]); // backspace out "~"
    keys.extend(burrow.to_str().unwrap().as_bytes());
    keys.push(b'\r');
    viewer.send(&ToDaemon::Input(proto::b64_encode(&keys)));

    let made = viewer.await_frame(25_000, |m| {
        apply_frame(&grid, m);
        sidebar_of(&grid).iter().any(|r| r.contains("remote-made"))
    });
    assert!(made.is_some(), "the agent arrived from over there: {:?}", sidebar_of(&grid));

    let rows = sidebar_of(&grid);
    let smq = rows.iter().position(|r| r.trim() == "smq").expect("a heading for the far machine");
    let made_at = rows.iter().position(|r| r.contains("remote-made")).unwrap();
    assert!(made_at > smq, "it belongs to the far machine: {rows:?}");
    assert!(rows.iter().any(|r| r.contains("Burrow/")), "in the directory asked for: {rows:?}");

    // And it really is over there: this machine's run dir never saw it.
    let here: Vec<String> = std::fs::read_dir(near.dir.join("run"))
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().to_string())
        .collect();
    assert_eq!(here, vec!["svm.sock".to_string()], "nothing was created here: {here:?}");
    let there = std::fs::read_dir(far.dir.join("run")).unwrap().flatten().count();
    assert_eq!(there, 2, "spork and the new one, over there");
}

/// The new-agent form is as clickable as the sidebar: a chip picks its
/// option, a field row takes the cursor, and neither needs the keyboard to
/// have found it first.
#[test]
fn the_new_agent_form_answers_the_mouse() {
    let far = TestHome::new("mousefar");
    let near = TestHome::new("mousenear");
    let outer = TestHome::new("mouseout");

    new_agent_in(&far, "spork", &far.dir.join("Games"), "sleep 300");
    new_agent_in(&near, "svm", &near.dir.join("Research"), "sleep 300");
    std::fs::write(near.dir.join("hosts"), format!("smq  {BIN}\n")).unwrap();

    let shim = fake_ssh(&near, &far);
    let dash_cmd =
        format!("WARREN_HOME={} WARREN_SSH={} {} up", near.dir.display(), shim.display(), BIN);
    new_agent(&outer, "dash", &dash_cmd);
    let (mut viewer, snap) = Viewer::attach(&outer.sock("dash"), 100, 24);

    let grid = std::cell::RefCell::new(Vec::<String>::new());
    apply_frame(&grid, &snap);
    let up = viewer.await_frame(20_000, |m| {
        apply_frame(&grid, m);
        sidebar_of(&grid).iter().any(|r| r.contains("spork"))
    });
    assert!(up.is_some(), "both machines are up: {:?}", sidebar_of(&grid));

    viewer.send(&ToDaemon::Input(proto::b64_encode(b"\x00n")));
    let form = viewer.await_frame(10_000, |m| {
        apply_frame(&grid, m);
        grid.borrow().iter().any(|r| r.contains("[ smq ]"))
    });
    assert!(form.is_some(), "the machine chips are drawn: {:?}", grid.borrow().clone());

    // Click the far machine's chip, wherever the form happened to draw it.
    let (row, col) = find_in_grid(&grid, "[ smq ]").expect("the smq chip is on screen");
    click(&mut viewer, row + 1, col + 2); // inside the chip, 1-based
    let picked = viewer.await_frame(10_000, |m| {
        apply_frame(&grid, m);
        grid.borrow().iter().any(|r| r.contains("new claude agent on smq"))
    });
    assert!(picked.is_some(), "clicking a chip picks it: {:?}", grid.borrow().clone());

    // Click a text field: the cursor goes there, with no tabbing to reach it.
    let (row, _) = find_in_grid(&grid, "Sys prompt").expect("the field is on screen");
    click(&mut viewer, row + 1, 30);
    let moved = viewer.await_frame(10_000, |m| {
        apply_frame(&grid, m);
        grid.borrow()
            .get(row)
            .map(|r| r.contains("Sys prompt") && r.trim_end().ends_with('\u{2588}'))
            .unwrap_or(false)
    });
    assert!(moved.is_some(), "clicking a field focuses it: {:?}", grid.borrow().get(row));
}

/// Where a string sits on the dashboard's screen, 0-based.
fn find_in_grid(grid: &std::cell::RefCell<Vec<String>>, needle: &str) -> Option<(usize, usize)> {
    grid.borrow()
        .iter()
        .enumerate()
        .find_map(|(r, line)| line.find(needle).map(|c| (r, line[..c].chars().count())))
}

/// One left-button click, as a terminal reports it: SGR, 1-based.
fn click(viewer: &mut Viewer, row: usize, col: usize) {
    let press = format!("\x1b[<0;{col};{row}M");
    let release = format!("\x1b[<0;{col};{row}m");
    viewer.send(&ToDaemon::Input(proto::b64_encode(press.as_bytes())));
    viewer.send(&ToDaemon::Input(proto::b64_encode(release.as_bytes())));
}

/// ^Z is warren's, in either mode. Typed at an agent it would be the tty's
/// own SUSP — the harness stops, the daemon is still holding a process that
/// answers nothing, and the tab is neither awake nor resumable. Sleeping is
/// what someone reaching for ^Z wants anyway, and it can be woken.
#[test]
fn ctrl_z_sleeps_the_agent_instead_of_suspending_it() {
    let home = TestHome::new("ctrlz");
    // Announces its pid, then sits there like an idle harness.
    new_agent_in(&home, "napper", &home.dir.join("Burrow"), "printf 'awake as %s\\n' $$; sleep 300");

    // Sleep needs a session to resume, which is what the hooks report.
    let mut hook = Command::new(BIN)
        .env("WARREN_SOCK", home.sock("napper"))
        .args(["hook", "waiting"])
        .stdin(std::process::Stdio::piped())
        .spawn()
        .unwrap();
    hook.stdin.take().unwrap().write_all(br#"{"session_id":"deadbeef"}"#).unwrap();
    assert!(hook.wait().unwrap().success(), "the hook must exit 0");

    let outer = TestHome::new("ctrlzout");
    let dash_cmd = format!("WARREN_HOME={} {} up", home.dir.display(), BIN);
    new_agent(&outer, "dash", &dash_cmd);
    let (mut viewer, snap) = Viewer::attach(&outer.sock("dash"), 100, 20);

    let grid = std::cell::RefCell::new(Vec::<String>::new());
    apply_frame(&grid, &snap);
    let up = viewer.await_frame(15_000, |m| {
        apply_frame(&grid, m);
        grid.borrow().iter().any(|r| r.contains("awake as"))
    });
    assert!(up.is_some(), "the agent is up and painting: {:?}", grid.borrow().clone());
    let text: String = grid.borrow().concat();
    let pid: String = text
        .split("awake as ")
        .nth(1)
        .expect("pid on screen")
        .chars()
        .take_while(|c| c.is_ascii_digit())
        .collect();
    assert!(pid_alive(&pid), "the agent's process is running");

    // CLAUDE mode, where every other key goes straight to the agent.
    viewer.send(&ToDaemon::Input(proto::b64_encode(b"\x1a")));
    let slept = viewer.await_frame(15_000, |m| {
        apply_frame(&grid, m);
        sidebar_of(&grid).iter().any(|r| r.contains("napper z"))
    });
    assert!(slept.is_some(), "the row says it is asleep: {:?}", sidebar_of(&grid));
    assert!(!pid_alive(&pid), "the process is gone, not stopped");

    // And it wakes the same way, from NORMAL this time.
    viewer.send(&ToDaemon::Input(proto::b64_encode(b"\x00\x1a")));
    let woke = viewer.await_frame(15_000, |m| {
        apply_frame(&grid, m);
        !sidebar_of(&grid).iter().any(|r| r.contains("napper z"))
    });
    assert!(woke.is_some(), "^Z woke it again: {:?}", sidebar_of(&grid));
}

/// What a harness leaves on the primary screen is history, and warren is the
/// terminal it left it in: the wheel reads it back. OMP flushes a finished
/// turn up there in one go, so this is the view that matters — and typing,
/// or the next turn starting, puts you back on the live screen.
#[test]
fn the_wheel_reads_the_scrollback_when_the_turn_is_over() {
    let home = TestHome::new("scroll");
    // Forty lines, a quiet spell, then a second "turn".
    new_agent(
        &home,
        "reader",
        "for i in $(seq 1 40); do echo \"line $i\"; done; sleep 12; echo 'TURN TWO'; sleep 300",
    );
    let (mut viewer, snap) = Viewer::attach(&home.sock("reader"), 60, 10);

    let grid = std::cell::RefCell::new(Vec::<String>::new());
    apply_frame(&grid, &snap);
    // The burst races the attach: it is either already in the snapshot or
    // lands as the damage right after.
    let done = grid.borrow().iter().any(|r| r.contains("line 40"))
        || viewer
            .await_frame(10_000, |m| {
                apply_frame(&grid, m);
                grid.borrow().iter().any(|r| r.contains("line 40"))
            })
            .is_some();
    assert!(done, "the first burst finished: {:?}", grid.borrow().clone());
    // Ten rows: the early lines are long gone from the screen.
    assert!(!grid.borrow().iter().any(|r| r.contains("line 5")), "{:?}", grid.borrow().clone());

    // Wait out the busy heuristic — this is the "turn is over" the rule is
    // about — then wheel up.
    std::thread::sleep(Duration::from_millis(1_800));
    // Three lines a notch, the way a terminal does it, so this is a dozen
    // notches back up a thirty-line history — it clamps at the top.
    for _ in 0..12 {
        viewer.send(&ToDaemon::Mouse { kind: MouseKind::WheelUp, col: 10, row: 5, mods: 0 });
    }
    let back = viewer.await_frame(10_000, |m| {
        apply_frame(&grid, m);
        grid.borrow().iter().any(|r| r.contains("line 5"))
    });
    assert!(back.is_some(), "the wheel reached the scrollback: {:?}", grid.borrow().clone());
    // A viewer reading history is shown no cursor: it is down on the live
    // screen, where this viewer is not looking.
    if let Some(ToClient::Snapshot { cursor_visible, .. }) = back {
        assert!(!cursor_visible, "no cursor while reading history");
    }

    // The next turn starts: no scrolling while it works, so the view comes
    // back down on its own.
    let live = viewer.await_frame(20_000, |m| {
        apply_frame(&grid, m);
        grid.borrow().iter().any(|r| r.contains("TURN TWO"))
    });
    assert!(live.is_some(), "a turn starting ends the scroll: {:?}", grid.borrow().clone());
}

/// No scrolling while it works. Mid-turn a harness repaints a live region,
/// so what went past the top is not in the scrollback yet — the wheel must
/// not freeze the view on a history that has nothing in it.
#[test]
fn the_wheel_does_nothing_while_the_agent_is_working() {
    let home = TestHome::new("scrollbusy");
    // Never stops printing: busy by the same heuristic the sidebar uses.
    new_agent(&home, "chatty", "i=0; while :; do i=$((i+1)); echo \"tick $i\"; sleep 0.2; done");
    let (mut viewer, snap) = Viewer::attach(&home.sock("chatty"), 60, 10);

    let grid = std::cell::RefCell::new(Vec::<String>::new());
    apply_frame(&grid, &snap);
    let running = viewer.await_frame(10_000, |m| {
        apply_frame(&grid, m);
        highest_tick(&grid) >= 8
    });
    assert!(running.is_some(), "it is working: {:?}", grid.borrow().clone());

    let at_wheel = highest_tick(&grid);
    for _ in 0..6 {
        viewer.send(&ToDaemon::Mouse { kind: MouseKind::WheelUp, col: 10, row: 5, mods: 0 });
    }
    // The view stays live: new ticks keep arriving rather than the viewer
    // being parked on a frozen screen.
    let still_live = viewer.await_frame(10_000, |m| {
        apply_frame(&grid, m);
        highest_tick(&grid) > at_wheel + 3
    });
    assert!(
        still_live.is_some(),
        "the wheel left the live view alone: at wheel {at_wheel}, now {}",
        highest_tick(&grid)
    );
}

/// The biggest "tick N" currently on screen.
fn highest_tick(grid: &std::cell::RefCell<Vec<String>>) -> u32 {
    grid.borrow()
        .iter()
        .filter_map(|r| r.split("tick ").nth(1))
        .filter_map(|rest| {
            let n: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            n.parse().ok()
        })
        .max()
        .unwrap_or(0)
}


/// A flick of the wheel goes as far as it was flicked. The answer to a
/// scroll is a snapshot, and a snapshot used to be counted as the agent
/// having produced output — so the first notch made the dashboard believe a
/// turn had started and it refused the rest of the flick for a second and a
/// half. Through a real dashboard, because that is where the gate lives.
#[test]
fn a_flick_of_the_wheel_scrolls_further_than_one_notch() {
    let inner = TestHome::new("flick");
    new_agent_in(
        &inner,
        "pager",
        &inner.dir.join("Burrow"),
        "for i in $(seq 1 200); do echo \"line $i\"; done; sleep 300",
    );

    let outer = TestHome::new("flickout");
    let dash_cmd = format!("WARREN_HOME={} {} up", inner.dir.display(), BIN);
    new_agent(&outer, "dash", &dash_cmd);
    let (mut viewer, snap) = Viewer::attach(&outer.sock("dash"), 100, 24);

    let grid = std::cell::RefCell::new(Vec::<String>::new());
    apply_frame(&grid, &snap);
    let up = grid.borrow().iter().any(|r| r.contains("line 200"))
        || viewer
            .await_frame(15_000, |m| {
                apply_frame(&grid, m);
                grid.borrow().iter().any(|r| r.contains("line 200"))
            })
            .is_some();
    assert!(up, "the agent painted: {:?}", grid.borrow().clone());

    // Past the busy window, so the turn counts as over.
    std::thread::sleep(Duration::from_millis(1_800));
    let before = topmost_line(&grid);
    assert!(before > 0, "a live view to scroll from: {:?}", grid.borrow().clone());

    // Eight notches spread over a flick's worth of time, which is what a
    // trackpad sends — the answer to one notch lands before the next is
    // typed, and that answer must not read as the agent going back to work.
    for _ in 0..8 {
        viewer.send(&ToDaemon::Input(proto::b64_encode(b"\x1b[<64;50;10M")));
        std::thread::sleep(Duration::from_millis(100));
    }
    let moved = viewer.await_frame(10_000, |m| {
        apply_frame(&grid, m);
        let now = topmost_line(&grid);
        now > 0 && before - now >= 12
    });
    assert!(
        moved.is_some(),
        "the whole flick landed: from line {before} to line {} ({:?})",
        topmost_line(&grid),
        sidebar_of(&grid).first()
    );
}

/// The lowest "line N" visible in the pane, 0 if none.
fn topmost_line(grid: &std::cell::RefCell<Vec<String>>) -> u32 {
    grid.borrow()
        .iter()
        .filter_map(|r| r.split("line ").nth(1))
        .filter_map(|rest| {
            let n: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
            n.parse().ok()
        })
        .min()
        .unwrap_or(0)
}
