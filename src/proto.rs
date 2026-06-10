//! Daemon <-> viewer wire protocol.
//!
//! Framing: u32 little-endian payload length, then that many bytes of JSON.
//! JSON keeps the protocol debuggable (`nc -U` a socket and read it); damage
//! is coalesced to <=30 fps and span-run-length-encoded, so frames stay small.
//!
//! Cardinal rule (the lesson of v0's freeze): writers NEVER block on a peer.
//! Both sides keep per-connection bounded outbound queues and drop the
//! connection on overflow; a dropped viewer reconnects and gets a fresh
//! Snapshot.

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use serde::de::DeserializeOwned;

use crate::spans::LineSpans;

/// Refuse frames bigger than this — a corrupt length prefix must not make us
/// buffer gigabytes. A full 4k-cell snapshot with worst-case styling is far
/// below this.
pub const MAX_FRAME: usize = 16 * 1024 * 1024;

// ---------------------------------------------------------------- agent state

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum HookState {
    Working,
    Waiting,
    Attention,
    Gone,
}

impl HookState {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "working" => Some(Self::Working),
            "waiting" => Some(Self::Waiting),
            "attention" => Some(Self::Attention),
            "gone" => Some(Self::Gone),
            _ => None,
        }
    }
}

/// Everything the sidebar needs about one agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Meta {
    /// Immutable session name (socket file stem).
    pub name: String,
    /// Sidebar label: manual rename or Claude's title, else `name`.
    pub display: String,
    /// xterm-256 tab color, 0 = none.
    pub color: u8,
    /// Manual rename wins over Claude title sync.
    pub pinned: bool,
    /// Sidebar position, 1-based; assigned lowest-free at spawn.
    pub slot: u8,
    /// Agent working directory.
    pub cwd: String,
    /// Spawn time, unix seconds (orders agents with equal slots).
    pub created: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentState {
    /// Last hook-reported state, if any.
    pub hook: Option<HookState>,
    /// Milliseconds since the pty last produced output. Viewers compute the
    /// busy fallback (working if < 1500ms) on their own render tick.
    pub ms_since_output: u64,
}

// --------------------------------------------------------------------- mouse

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MouseKind {
    /// Button press: 0=left 1=middle 2=right.
    Down(u8),
    Up(u8),
    /// Motion with a button held.
    Drag(u8),
    /// Motion with no button (only forwarded under any-motion tracking).
    Moved,
    WheelUp,
    WheelDown,
}

/// Which mouse reports the application subscribed to (DECSET state),
/// so viewers can route the wheel without guessing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum MouseProto {
    #[default]
    None,
    /// 1000: presses/releases.
    Click,
    /// 1002: + drag motion.
    Drag,
    /// 1003: + all motion.
    Motion,
}

// ------------------------------------------------------------------- messages

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToDaemon {
    /// Become a viewer: receive Snapshot now, Damage/events from then on.
    Attach { cols: u16, rows: u16 },
    /// Keyboard bytes for the application (base64).
    Input(String),
    Resize { cols: u16, rows: u16 },
    Mouse { kind: MouseKind, col: u16, row: u16, mods: u8 },
    SetMeta {
        name: Option<String>,
        color: Option<u8>,
        pinned: Option<bool>,
        slot: Option<u8>,
    },
    /// From `warren hook` (Claude lifecycle hooks).
    HookState(HookState),
    /// One-shot status query (`warren ls`): answered with Snapshot-free
    /// MetaChanged + StateChanged, then the daemon closes the connection.
    Query,
    /// Terminate the agent.
    Kill,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ToClient {
    /// Full authoritative screen. Sent on attach and after any resize.
    /// Scrollback is deliberately absent: agents (Claude's fullscreen TUI)
    /// own their own history; warren is a live view.
    Snapshot {
        cols: u16,
        rows: u16,
        screen: Vec<LineSpans>,
        cursor: (u16, u16),
        cursor_visible: bool,
        alt_screen: bool,
        mouse: MouseProto,
        meta: Meta,
        state: AgentState,
    },
    /// Changed rows since the previous frame (row index, new content).
    Damage {
        lines: Vec<(u16, LineSpans)>,
        cursor: (u16, u16),
        cursor_visible: bool,
    },
    ModeChanged { alt_screen: bool, mouse: MouseProto },
    MetaChanged(Meta),
    StateChanged(AgentState),
    /// OSC 52 payload (already base64), forwarded for the focused agent.
    Clipboard(String),
    /// The application exited; the daemon unlinks its socket and quits.
    Exited { status: i32 },
}

// ------------------------------------------------------------------- framing

pub fn encode_frame<T: Serialize>(msg: &T) -> Result<Vec<u8>> {
    let body = serde_json::to_vec(msg)?;
    if body.len() > MAX_FRAME {
        bail!("frame too large: {} bytes", body.len());
    }
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// Incremental frame decoder: feed arbitrary byte chunks, pull whole messages.
#[derive(Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
}

impl FrameDecoder {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
    }

    /// Next complete message, or Ok(None) if more bytes are needed.
    pub fn next<T: DeserializeOwned>(&mut self) -> Result<Option<T>> {
        if self.buf.len() < 4 {
            return Ok(None);
        }
        let len = u32::from_le_bytes(self.buf[..4].try_into().unwrap()) as usize;
        if len > MAX_FRAME {
            bail!("incoming frame length {len} exceeds limit");
        }
        if self.buf.len() < 4 + len {
            return Ok(None);
        }
        let msg = serde_json::from_slice(&self.buf[4..4 + len])?;
        self.buf.drain(..4 + len);
        Ok(Some(msg))
    }
}

pub fn b64_encode(data: &[u8]) -> String {
    // Tiny standalone base64 (standard alphabet, padded) — not worth a crate.
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for chunk in data.chunks(3) {
        let b = [chunk[0], *chunk.get(1).unwrap_or(&0), *chunk.get(2).unwrap_or(&0)];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        out.push(TABLE[(n >> 18) as usize & 63] as char);
        out.push(TABLE[(n >> 12) as usize & 63] as char);
        out.push(if chunk.len() > 1 { TABLE[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if chunk.len() > 2 { TABLE[n as usize & 63] as char } else { '=' });
    }
    out
}

pub fn b64_decode(s: &str) -> Result<Vec<u8>> {
    fn val(c: u8) -> Result<u32> {
        Ok(match c {
            b'A'..=b'Z' => (c - b'A') as u32,
            b'a'..=b'z' => (c - b'a' + 26) as u32,
            b'0'..=b'9' => (c - b'0' + 52) as u32,
            b'+' => 62,
            b'/' => 63,
            _ => bail!("invalid base64 byte {c}"),
        })
    }
    let s = s.trim_end_matches('=').as_bytes();
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    for chunk in s.chunks(4) {
        if chunk.len() == 1 {
            bail!("truncated base64");
        }
        let mut n = 0u32;
        for &c in chunk {
            n = n << 6 | val(c)?;
        }
        n <<= 6 * (4 - chunk.len()) as u32;
        out.push((n >> 16) as u8);
        if chunk.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if chunk.len() > 3 {
            out.push(n as u8);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spans::{Color, Span};

    fn sample_meta() -> Meta {
        Meta {
            name: "Phylogen".into(),
            display: "Build the grove".into(),
            color: 213,
            pinned: false,
            slot: 3,
            cwd: "/Users/x/Developer/Phylogen".into(),
            created: 1781125027,
        }
    }

    #[test]
    fn roundtrip_to_daemon() {
        let msgs = vec![
            ToDaemon::Attach { cols: 120, rows: 40 },
            ToDaemon::Input(b64_encode(b"hello\x1b[A")),
            ToDaemon::Resize { cols: 80, rows: 24 },
            ToDaemon::Mouse { kind: MouseKind::Drag(0), col: 5, row: 9, mods: 0 },
            ToDaemon::SetMeta { name: Some("x".into()), color: None, pinned: Some(true), slot: None },
            ToDaemon::HookState(HookState::Attention),
            ToDaemon::Query,
            ToDaemon::Kill,
        ];
        let mut dec = FrameDecoder::new();
        for m in &msgs {
            dec.push(&encode_frame(m).unwrap());
        }
        for m in &msgs {
            let got: ToDaemon = dec.next().unwrap().unwrap();
            assert_eq!(&got, m);
        }
        assert!(dec.next::<ToDaemon>().unwrap().is_none());
    }

    #[test]
    fn roundtrip_to_client() {
        let line = LineSpans(vec![
            Span { text: "warren".into(), fg: Color::Indexed(213), bg: Color::Default, attrs: 1 },
            Span::plain(" v1"),
        ]);
        let msgs = vec![
            ToClient::Snapshot {
                cols: 80,
                rows: 2,
                screen: vec![line.clone(), LineSpans::default()],
                cursor: (0, 7),
                cursor_visible: true,
                alt_screen: false,
                mouse: MouseProto::None,
                meta: sample_meta(),
                state: AgentState { hook: Some(HookState::Working), ms_since_output: 12 },
            },
            ToClient::Damage {
                lines: vec![(1, line)],
                cursor: (1, 0),
                cursor_visible: false,
            },
            ToClient::ModeChanged { alt_screen: true, mouse: MouseProto::Motion },
            ToClient::MetaChanged(sample_meta()),
            ToClient::StateChanged(AgentState { hook: None, ms_since_output: 99 }),
            ToClient::Clipboard("aGVsbG8=".into()),
            ToClient::Exited { status: 0 },
        ];
        let mut dec = FrameDecoder::new();
        for m in &msgs {
            dec.push(&encode_frame(m).unwrap());
        }
        for m in &msgs {
            let got: ToClient = dec.next().unwrap().unwrap();
            assert_eq!(&got, m);
        }
    }

    #[test]
    fn partial_frames_wait_for_more_bytes() {
        let frame = encode_frame(&ToDaemon::Query).unwrap();
        let mut dec = FrameDecoder::new();
        for i in 0..frame.len() - 1 {
            dec.push(&frame[i..i + 1]);
            assert!(dec.next::<ToDaemon>().unwrap().is_none(), "byte {i}");
        }
        dec.push(&frame[frame.len() - 1..]);
        assert_eq!(dec.next::<ToDaemon>().unwrap(), Some(ToDaemon::Query));
    }

    #[test]
    fn oversized_length_is_an_error() {
        let mut dec = FrameDecoder::new();
        dec.push(&(u32::MAX).to_le_bytes());
        assert!(dec.next::<ToDaemon>().is_err());
    }

    #[test]
    fn base64_roundtrip() {
        for case in [&b""[..], b"a", b"ab", b"abc", b"abcd", b"\x00\xff\x1b[200~"] {
            assert_eq!(b64_decode(&b64_encode(case)).unwrap(), case);
        }
        assert_eq!(b64_encode(b"hello"), "aGVsbG8=");
        assert!(b64_decode("!!!").is_err());
    }
}
