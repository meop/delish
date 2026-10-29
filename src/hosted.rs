//! The sessions a server runs for links from other servers. A session belongs to the server, not to the link that
//! opened it: when the link drops, the session waits, detached, for a link to take it back, and only a grace period
//! without one ends it, as a closed terminal would.

use std::collections::HashMap;
use std::io;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use crate::mux::{self, Frames, Inbox, Outbox, Sid};
use crate::proto::{self, Spawn};
use crate::server::log;
use crate::session::{Session, Sink};

/// how many unacknowledged bytes of a session's output it may get ahead by before it is paused
pub const OUT_CAP: usize = 4 << 20;
pub const PING_EVERY: Duration = Duration::from_secs(15);
pub const DEAD_AFTER: Duration = Duration::from_secs(45);

/// how long a session, or a link, outlasts what it depends on: DELISH_LINK_GRACE, DELISH_LINK_IDLE, in seconds
pub fn seconds(var: &str, default: u64) -> Duration {
    Duration::from_secs(
        std::env::var(var)
            .ok()
            .and_then(|v| v.parse().ok())
            .unwrap_or(default),
    )
}

pub fn grace() -> Duration {
    seconds("DELISH_LINK_GRACE", 600)
}

pub type Writer = Arc<Mutex<Box<dyn io::Write + Send>>>;

struct Hosted {
    session: OnceLock<Session>,
    out: Arc<Outbox>,
    input: Mutex<(Inbox, Frames)>,
    /// the link it is attached to, by number, with that link's writer; none while detached
    link: Mutex<Option<(u64, Writer)>>,
    detached_at: Mutex<Option<Instant>>,
    finished: AtomicBool,
}

fn hosted() -> &'static Mutex<HashMap<Sid, Arc<Hosted>>> {
    static H: OnceLock<Mutex<HashMap<Sid, Arc<Hosted>>>> = OnceLock::new();
    H.get_or_init(|| {
        thread::spawn(reap);
        Mutex::new(HashMap::new())
    })
}

/// a session detached past its grace is hung up, and one that ended with everything acknowledged is forgotten
fn reap() {
    loop {
        thread::sleep(Duration::from_secs(1));
        let grace = grace();
        let mut gone = Vec::new();
        for (sid, h) in hosted().lock().unwrap().iter() {
            let expired = h
                .detached_at
                .lock()
                .unwrap()
                .is_some_and(|t| t.elapsed() > grace);
            if expired || (h.finished.load(Ordering::SeqCst) && h.out.drained()) {
                gone.push((*sid, expired));
            }
        }
        for (sid, expired) in gone {
            if let Some(h) = hosted().lock().unwrap().remove(&sid) {
                if expired {
                    log("a detached session's grace ran out");
                    if let Some(s) = h.session.get() {
                        s.hang_up();
                    }
                }
                h.out.close();
            }
        }
    }
}

fn send(w: &Writer, kind: u8, sid: &Sid, offset: u64, payload: &[u8]) -> io::Result<()> {
    mux::write(&mut *w.lock().unwrap(), kind, sid, offset, payload)
}

/// a session's output to its link while it is attached to that one
fn sender(h: Arc<Hosted>, sid: Sid, link: u64, w: Writer) {
    thread::spawn(move || {
        loop {
            if h.link.lock().unwrap().as_ref().map(|(n, _)| *n) != Some(link) {
                return;
            }
            match h.out.take(64 << 10, Duration::from_millis(200)) {
                None => return,
                Some((_, bytes)) if bytes.is_empty() => {}
                Some((offset, bytes)) => {
                    if send(&w, mux::DATA, &sid, offset, &bytes).is_err() {
                        return;
                    }
                }
            }
        }
    });
}

fn attach(h: &Arc<Hosted>, sid: Sid, link: u64, w: &Writer) {
    *h.link.lock().unwrap() = Some((link, w.clone()));
    *h.detached_at.lock().unwrap() = None;
    sender(h.clone(), sid, link, w.clone());
}

fn open(sid: Sid, payload: &[u8], link: u64, w: &Writer) {
    let out = Arc::new(Outbox::new(OUT_CAP));
    let h = Arc::new(Hosted {
        session: OnceLock::new(),
        out: out.clone(),
        input: Mutex::new((Inbox::default(), Frames::default())),
        link: Mutex::new(None),
        detached_at: Mutex::new(None),
        finished: AtomicBool::new(false),
    });
    let sink: Sink = {
        let out = out.clone();
        Arc::new(move |kind, bytes| {
            let mut frame = Vec::with_capacity(bytes.len() + 5);
            proto::write_frame(&mut frame, kind, bytes)?;
            if !out.push(&frame) {
                return Err(io::ErrorKind::BrokenPipe.into());
            }
            if kind == proto::EXIT {
                out.close();
            }
            Ok(())
        })
    };
    let started = Spawn::decode(payload)
        .map_err(|e| e.to_string())
        .and_then(|spawn| Session::start(spawn, sink.clone()));
    match started {
        Ok(session) => {
            log(&format!("running {} for a link", session.program));
            let _ = h.session.set(session);
            let watched = h.clone();
            thread::spawn(move || {
                if let Some(s) = watched.session.get() {
                    let code = s.wait();
                    log(&format!("{} exited {code} for a link", s.program));
                }
                watched.finished.store(true, Ordering::SeqCst);
            });
        }
        Err(msg) => {
            log(&msg);
            let _ = sink(proto::ERROR, msg.as_bytes());
            out.close();
            h.finished.store(true, Ordering::SeqCst);
        }
    }
    hosted().lock().unwrap().insert(sid, h.clone());
    attach(&h, sid, link, w);
}

/// a link from another server: its sessions' frames until it ends, when they are detached, not ended
/// the caller has the reader time out after DEAD_AFTER without a frame
pub fn serve(mut reader: impl io::Read, w: Writer) -> io::Result<()> {
    static LINKS: AtomicU64 = AtomicU64::new(1);
    let link = LINKS.fetch_add(1, Ordering::SeqCst);
    let alive = Arc::new(AtomicBool::new(true));
    {
        let (w, alive) = (w.clone(), alive.clone());
        thread::spawn(move || {
            while alive.load(Ordering::SeqCst) {
                thread::sleep(PING_EVERY);
                if send(&w, mux::PING, &mux::NO_SID, 0, &[]).is_err() {
                    return;
                }
            }
        });
    }
    log("a link opened");
    while let Ok(Some(f)) = mux::read(&mut reader) {
        let found = hosted().lock().unwrap().get(&f.sid).cloned();
        match (f.kind, found) {
            (mux::OPEN, None) => open(f.sid, &f.payload, link, &w),
            (mux::DATA, Some(h)) => {
                let (frames, received) = {
                    let mut input = h.input.lock().unwrap();
                    let (inbox, assembler) = &mut *input;
                    let Ok(new) = inbox.accept(f.offset, &f.payload) else {
                        break;
                    };
                    let new = new.to_vec();
                    (assembler.push(&new), inbox.received)
                };
                if let Some(s) = h.session.get() {
                    for (kind, payload) in frames {
                        s.apply(kind, &payload);
                    }
                }
                let _ = send(&w, mux::ACK, &f.sid, received, &[]);
            }
            (mux::ACK, Some(h)) => h.out.ack(f.offset),
            (mux::ATTACH, Some(h)) => {
                h.out.rewind(f.offset);
                let received = h.input.lock().unwrap().0.received;
                attach(&h, f.sid, link, &w);
                let _ = send(&w, mux::ATTACHED, &f.sid, received, &[]);
            }
            (mux::ATTACH, None) => {
                let _ = send(
                    &w,
                    mux::GONE,
                    &f.sid,
                    0,
                    b"the session ended while its link was down",
                );
            }
            (mux::CLOSE, Some(h)) => {
                hosted().lock().unwrap().remove(&f.sid);
                if let Some(s) = h.session.get() {
                    s.hang_up();
                }
                h.out.close();
            }
            (mux::PING, _) => {
                let _ = send(&w, mux::PONG, &mux::NO_SID, 0, &[]);
            }
            _ => {}
        }
    }
    alive.store(false, Ordering::SeqCst);
    for h in hosted().lock().unwrap().values() {
        let mut attached = h.link.lock().unwrap();
        if attached.as_ref().map(|(n, _)| *n) == Some(link) {
            *attached = None;
            *h.detached_at.lock().unwrap() = Some(Instant::now());
        }
    }
    log("a link closed; its sessions wait for another");
    Ok(())
}
