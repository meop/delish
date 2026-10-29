//! A link: one connection between two servers that carries many sessions, and outlives a drop of the TCP under it.
//! Each direction of a session is a byte stream of ordinary frames, numbered by offset, kept by its sender until the
//! other side acknowledges it, so a link that drops and comes back resends what did not arrive and nothing twice.
//! A sender whose unacknowledged bytes reach its cap waits: a command writing faster than the link carries it is
//! paused, as a slow terminal would pause it, rather than losing output.

use std::io::{self, Read, Write};
use std::sync::{Condvar, Mutex};
use std::time::Duration;

/// local → remote: start a session; the payload is its spawn
pub const OPEN: u8 = 1;
/// either way: bytes of a session's stream, from offset
pub const DATA: u8 = 2;
/// either way: everything of a session's stream before offset has arrived
pub const ACK: u8 = 3;
/// local → remote, on a new link: take this session back; offset is what of its output has arrived
pub const ATTACH: u8 = 4;
/// remote → local: the session is back; offset is what of its input has arrived
pub const ATTACHED: u8 = 5;
/// remote → local: no such session, or it ended while the link was down; the payload says which
pub const GONE: u8 = 6;
/// local → remote: the session's client went away: end it as a closed terminal ends its command
pub const CLOSE: u8 = 7;
/// either way, no session: a link that hears nothing for a while is taken for dead
pub const PING: u8 = 8;
pub const PONG: u8 = 9;

pub type Sid = [u8; 16];
pub const NO_SID: Sid = [0; 16];

const HEAD: usize = 1 + 16 + 8 + 4;
const MAX_PAYLOAD: u32 = 16 << 20;

#[derive(Debug, PartialEq, Eq)]
pub struct Frame {
    pub kind: u8,
    pub sid: Sid,
    pub offset: u64,
    pub payload: Vec<u8>,
}

pub fn write(
    w: &mut impl Write,
    kind: u8,
    sid: &Sid,
    offset: u64,
    payload: &[u8],
) -> io::Result<()> {
    let mut head = [0u8; HEAD];
    head[0] = kind;
    head[1..17].copy_from_slice(sid);
    head[17..25].copy_from_slice(&offset.to_le_bytes());
    head[25..29].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    w.write_all(&head)?;
    w.write_all(payload)?;
    w.flush()
}

/// the next frame, or None at a clean end of the link
pub fn read(r: &mut impl Read) -> io::Result<Option<Frame>> {
    let mut head = [0u8; HEAD];
    match r.read_exact(&mut head) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(head[25..29].try_into().unwrap());
    if len > MAX_PAYLOAD {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "link frame too large",
        ));
    }
    let mut payload = vec![0u8; len as usize];
    r.read_exact(&mut payload)?;
    Ok(Some(Frame {
        kind: head[0],
        sid: head[1..17].try_into().unwrap(),
        offset: u64::from_le_bytes(head[17..25].try_into().unwrap()),
        payload,
    }))
}

/// a session's id, from the OS's randomness, which std draws on to seed its hash maps
pub fn new_sid() -> Sid {
    use std::hash::{BuildHasher, Hasher};
    let half = || {
        let mut h = std::collections::hash_map::RandomState::new().build_hasher();
        h.write_u64(std::process::id() as u64);
        h.finish()
    };
    let mut sid = [0u8; 16];
    sid[..8].copy_from_slice(&half().to_le_bytes());
    sid[8..].copy_from_slice(&half().to_le_bytes());
    sid
}

struct Kept {
    /// the offset of pending's first byte: everything before it was acknowledged
    acked: u64,
    pending: Vec<u8>,
    /// how far into pending the current link has been sent
    sent: usize,
    closed: bool,
}

/// a stream's sending side: what was sent and not yet acknowledged, kept to send again on a new link
pub struct Outbox {
    kept: Mutex<Kept>,
    changed: Condvar,
    cap: usize,
}

impl Outbox {
    pub fn new(cap: usize) -> Self {
        Outbox {
            kept: Mutex::new(Kept {
                acked: 0,
                pending: Vec::new(),
                sent: 0,
                closed: false,
            }),
            changed: Condvar::new(),
            cap,
        }
    }

    /// appends bytes, waiting while the unacknowledged ones fill the cap; false once the stream is closed
    pub fn push(&self, bytes: &[u8]) -> bool {
        let mut k = self.kept.lock().unwrap();
        while !k.closed && !k.pending.is_empty() && k.pending.len() + bytes.len() > self.cap {
            k = self.changed.wait(k).unwrap();
        }
        if k.closed {
            return false;
        }
        k.pending.extend_from_slice(bytes);
        self.changed.notify_all();
        true
    }

    /// the other side has everything before offset
    pub fn ack(&self, offset: u64) {
        let mut k = self.kept.lock().unwrap();
        if offset > k.acked {
            let n = ((offset - k.acked) as usize).min(k.pending.len());
            k.pending.drain(..n);
            k.acked += n as u64;
            k.sent = k.sent.saturating_sub(n);
            self.changed.notify_all();
        }
    }

    /// a new link: the other side has everything before offset, and the rest goes again
    pub fn rewind(&self, offset: u64) {
        self.ack(offset);
        let mut k = self.kept.lock().unwrap();
        k.sent = 0;
        self.changed.notify_all();
    }

    /// what is yet to be sent on this link, from its offset, waiting up to `wait` for some; None once closed and sent
    pub fn take(&self, max: usize, wait: Duration) -> Option<(u64, Vec<u8>)> {
        let mut k = self.kept.lock().unwrap();
        if k.sent == k.pending.len() && !k.closed {
            k = self.changed.wait_timeout(k, wait).unwrap().0;
        }
        if k.sent == k.pending.len() {
            return if k.closed {
                None
            } else {
                Some((k.acked + k.sent as u64, Vec::new()))
            };
        }
        let end = (k.sent + max).min(k.pending.len());
        let chunk = k.pending[k.sent..end].to_vec();
        let offset = k.acked + k.sent as u64;
        k.sent = end;
        Some((offset, chunk))
    }

    /// nothing more will be pushed: a pusher waiting gives up, and take ends once the rest is sent
    pub fn close(&self) {
        self.kept.lock().unwrap().closed = true;
        self.changed.notify_all();
    }

    /// everything pushed has been acknowledged
    pub fn drained(&self) -> bool {
        self.kept.lock().unwrap().pending.is_empty()
    }
}

/// a stream's receiving side: how much has arrived, so a resend after a new link yields only what is new
#[derive(Default)]
pub struct Inbox {
    pub received: u64,
}

impl Inbox {
    /// the part of bytes, sent from offset, not already received; an error is a gap, bytes that never arrived
    pub fn accept<'a>(&mut self, offset: u64, bytes: &'a [u8]) -> io::Result<&'a [u8]> {
        if offset > self.received {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "a link stream has a gap",
            ));
        }
        let skip = ((self.received - offset) as usize).min(bytes.len());
        self.received += (bytes.len() - skip) as u64;
        Ok(&bytes[skip..])
    }
}

/// whole frames of the ordinary protocol out of a stream that arrives in pieces
#[derive(Default)]
pub struct Frames {
    buf: Vec<u8>,
}

impl Frames {
    pub fn push(&mut self, bytes: &[u8]) -> Vec<(u8, Vec<u8>)> {
        self.buf.extend_from_slice(bytes);
        let mut frames = Vec::new();
        let mut at = 0;
        while self.buf.len() - at >= 5 {
            let len = u32::from_le_bytes(self.buf[at + 1..at + 5].try_into().unwrap()) as usize;
            if self.buf.len() - at < 5 + len {
                break;
            }
            frames.push((self.buf[at], self.buf[at + 5..at + 5 + len].to_vec()));
            at += 5 + len;
        }
        self.buf.drain(..at);
        frames
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    #[test]
    fn session_ids_differ() {
        assert_ne!(new_sid(), new_sid());
        assert_ne!(new_sid(), NO_SID);
    }

    #[test]
    fn a_link_frame_round_trips() {
        let mut buf = Vec::new();
        write(&mut buf, DATA, &[7; 16], 42, b"hi").unwrap();
        let mut r = &buf[..];
        assert_eq!(
            read(&mut r).unwrap(),
            Some(Frame {
                kind: DATA,
                sid: [7; 16],
                offset: 42,
                payload: b"hi".to_vec()
            })
        );
        assert_eq!(read(&mut r).unwrap(), None);
    }

    #[test]
    fn what_was_sent_and_not_acknowledged_goes_again_on_a_new_link() {
        let out = Outbox::new(1024);
        out.push(b"abcdef");
        assert_eq!(out.take(4, Duration::ZERO), Some((0, b"abcd".to_vec())));
        assert_eq!(out.take(4, Duration::ZERO), Some((4, b"ef".to_vec())));
        out.ack(2);
        // the link dropped; the other side says it has the first three bytes
        out.rewind(3);
        assert_eq!(out.take(10, Duration::ZERO), Some((3, b"def".to_vec())));
        out.ack(6);
        assert!(out.drained());
    }

    #[test]
    fn a_full_outbox_holds_its_pusher_until_the_other_side_acknowledges() {
        let out = Arc::new(Outbox::new(4));
        out.push(b"abcd");
        let pusher = {
            let out = out.clone();
            std::thread::spawn(move || out.push(b"ef"))
        };
        std::thread::sleep(Duration::from_millis(100));
        assert!(!pusher.is_finished(), "a full outbox took more");
        out.take(4, Duration::ZERO);
        out.ack(4);
        assert!(pusher.join().unwrap());
        assert_eq!(out.take(10, Duration::ZERO), Some((4, b"ef".to_vec())));
    }

    #[test]
    fn a_closed_outbox_lets_a_waiting_pusher_go_and_ends_once_sent() {
        let out = Arc::new(Outbox::new(1));
        out.push(b"a");
        let pusher = {
            let out = out.clone();
            std::thread::spawn(move || out.push(b"b"))
        };
        std::thread::sleep(Duration::from_millis(50));
        out.close();
        assert!(!pusher.join().unwrap());
        assert_eq!(out.take(10, Duration::ZERO), Some((0, b"a".to_vec())));
        assert_eq!(out.take(10, Duration::ZERO), None);
    }

    #[test]
    fn an_inbox_yields_only_what_is_new_and_refuses_a_gap() {
        let mut inbox = Inbox::default();
        assert_eq!(inbox.accept(0, b"abc").unwrap(), b"abc");
        assert_eq!(inbox.accept(1, b"bcde").unwrap(), b"de");
        assert_eq!(inbox.accept(0, b"abcde").unwrap(), b"");
        assert!(inbox.accept(9, b"x").is_err());
        assert_eq!(inbox.received, 5);
    }

    #[test]
    fn frames_come_whole_however_the_stream_is_cut() {
        let mut stream = Vec::new();
        crate::proto::write_frame(&mut stream, crate::proto::INPUT, b"ls\r").unwrap();
        crate::proto::write_frame(&mut stream, crate::proto::EOF, &[]).unwrap();
        let mut frames = Frames::default();
        let mut got = Vec::new();
        for b in stream.chunks(3) {
            got.extend(frames.push(b));
        }
        assert_eq!(
            got,
            vec![
                (crate::proto::INPUT, b"ls\r".to_vec()),
                (crate::proto::EOF, vec![])
            ]
        );
    }
}
