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
use proto::{FrameDecoder, HookState, ToClient, ToDaemon};

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

fn new_agent(home: &TestHome, name: &str, agent_cmd: &str) {
    let out = home.warren(agent_cmd).args(["new", name]).output().expect("run warren new");
    assert!(
        out.status.success(),
        "warren new failed: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
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
    assert!(
        screen_text(screen).contains("hello from agent"),
        "snapshot shows initial output: {:?}",
        screen_text(screen)
    );

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
    new_agent(&home, "mortal", "sleep 0.4; exit 7");
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

    // Without WARREN_SOCK it's silently a no-op (claude outside warren).
    let out = Command::new(BIN).args(["hook", "working"]).output().unwrap();
    assert!(out.status.success());
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
