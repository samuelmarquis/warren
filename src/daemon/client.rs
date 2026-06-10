//! Per-connection state on the daemon side.
//!
//! The cardinal rule from v0's freeze: the daemon NEVER blocks on a client.
//! Every connection has a bounded outbound queue; a peer that stops reading
//! gets disconnected at the cap, never waited on.

use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;

use crate::proto::FrameDecoder;

/// Outbound queue cap per connection. A full-screen snapshot is tens of KiB;
/// hitting 4 MiB means the peer stopped draining long ago.
/// WARREN_OUT_CAP overrides it (integration tests use a small cap).
pub fn out_cap() -> usize {
    static CAP: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *CAP.get_or_init(|| {
        std::env::var("WARREN_OUT_CAP")
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(4 * 1024 * 1024)
    })
}

pub struct Conn {
    pub stream: UnixStream,
    pub decoder: FrameDecoder,
    /// Pending outbound bytes (already framed), drained on writability.
    out: Vec<u8>,
    /// Bytes of `out` already written.
    sent: usize,
    /// Receives Snapshot/Damage/events (sent Attach). Query connections don't.
    pub attached: bool,
    /// Close once the outbound queue drains (Query replies, Exited).
    pub close_after_write: bool,
    /// Marked for removal (overflow, IO error, or clean close).
    pub dead: bool,
}

impl Conn {
    pub fn new(stream: UnixStream) -> Self {
        Conn {
            stream,
            decoder: FrameDecoder::new(),
            out: Vec::new(),
            sent: 0,
            attached: false,
            close_after_write: false,
            dead: false,
        }
    }

    /// Queue an already-encoded frame; overflow kills the connection.
    pub fn send(&mut self, frame: &[u8]) {
        if self.dead {
            return;
        }
        if self.out.len() - self.sent + frame.len() > out_cap() {
            self.dead = true;
            return;
        }
        self.out.extend_from_slice(frame);
        // Opportunistic immediate flush keeps latency low without waiting for
        // the next writability event.
        self.flush();
    }

    pub fn wants_write(&self) -> bool {
        !self.dead && self.sent < self.out.len()
    }

    /// Drain as much of the outbound queue as the socket accepts right now.
    pub fn flush(&mut self) {
        while self.sent < self.out.len() {
            match self.stream.write(&self.out[self.sent..]) {
                Ok(0) => {
                    self.dead = true;
                    return;
                }
                Ok(n) => self.sent += n,
                Err(e) if e.kind() == ErrorKind::WouldBlock => break,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(_) => {
                    self.dead = true;
                    return;
                }
            }
        }
        if self.sent == self.out.len() {
            self.out.clear();
            self.sent = 0;
            if self.close_after_write {
                self.dead = true;
            }
        } else if self.sent > out_cap() / 2 {
            self.out.drain(..self.sent);
            self.sent = 0;
        }
    }

    /// Pull readable bytes into the frame decoder. Returns false on EOF/error.
    pub fn fill(&mut self) -> bool {
        let mut buf = [0u8; 16 * 1024];
        loop {
            match self.stream.read(&mut buf) {
                Ok(0) => return false,
                Ok(n) => self.decoder.push(&buf[..n]),
                Err(e) if e.kind() == ErrorKind::WouldBlock => return true,
                Err(e) if e.kind() == ErrorKind::Interrupted => continue,
                Err(_) => return false,
            }
        }
    }
}
