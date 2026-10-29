//! This server's links to other hosts' servers: one TCP connection per host, each side's key proved, carrying every
//! session this server's clients have there, kept a while after the last ends, and brought back after a drop with no
//! more than a pause seen.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use crate::hosted::{DEAD_AFTER, PING_EVERY, grace, seconds};
use crate::keys;
use crate::mux::{self, Inbox, Outbox, Sid};
use crate::noise;
use crate::proto::{self, Hello};
use crate::server::log;
use crate::sock::Stream;

/// how much a client may type ahead of what the other end has acknowledged
const IN_CAP: usize = 1 << 20;

/// what `delish <host>` asks its own server for: the host, by name or address, and the session's spawn
#[derive(Debug, PartialEq, Eq)]
pub struct Request {
    pub host: String,
    pub spawn: Vec<u8>,
}

impl Request {
    pub fn encode(&self) -> Vec<u8> {
        let mut b = (self.host.len() as u32).to_le_bytes().to_vec();
        b.extend_from_slice(self.host.as_bytes());
        b.extend_from_slice(&self.spawn);
        b
    }

    pub fn decode(b: &[u8]) -> io::Result<Self> {
        let bad = || io::Error::new(io::ErrorKind::InvalidData, "short remote request");
        let len = u32::from_le_bytes(b.get(..4).ok_or_else(bad)?.try_into().unwrap()) as usize;
        let host = b.get(4..4 + len).ok_or_else(bad)?;
        Ok(Request {
            host: String::from_utf8(host.to_vec()).map_err(|_| bad())?,
            spawn: b[4 + len..].to_vec(),
        })
    }
}

/// the host a name or an address in `hosts` stands for
pub fn resolve(name: &str) -> Option<keys::Host> {
    let all = keys::hosts();
    all.iter().find(|h| h.name == name).cloned().or_else(|| {
        all.into_iter()
            .find(|h| keys::with_port(&h.address) == keys::with_port(name))
    })
}

type Writer = Arc<Mutex<Box<dyn Write + Send>>>;

struct Client {
    stream: Mutex<Stream>,
    output: Mutex<Inbox>,
    input: Outbox,
    spawn: Vec<u8>,
    /// the other end has answered for it, so a new link takes it back rather than opening it again
    confirmed: AtomicBool,
}

impl Client {
    fn fail(&self, msg: &str) {
        let mut s = self.stream.lock().unwrap();
        let _ = proto::write_frame(&mut *s, proto::ERROR, msg.as_bytes());
        let _ = s.shutdown(std::net::Shutdown::Both);
    }
}

#[derive(Default)]
struct State {
    writer: Option<Writer>,
    generation: u64,
    clients: HashMap<Sid, Arc<Client>>,
    /// sessions whose client went while the link was down: the other end hears of it once it is back
    closed_while_down: Vec<Sid>,
    up_once: bool,
    closing: bool,
}

struct Link {
    host: keys::Host,
    state: Mutex<State>,
    changed: Condvar,
}

fn links() -> &'static Mutex<HashMap<String, Arc<Link>>> {
    static L: OnceLock<Mutex<HashMap<String, Arc<Link>>>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(HashMap::new()))
}

fn link_for(host: keys::Host) -> Arc<Link> {
    let key = host.name.clone();
    let mut all = links().lock().unwrap();
    if let Some(l) = all.get(&key)
        && !l.state.lock().unwrap().closing
    {
        return l.clone();
    }
    let l = Arc::new(Link {
        host,
        state: Mutex::new(State::default()),
        changed: Condvar::new(),
    });
    all.insert(key.clone(), l.clone());
    let run = l.clone();
    thread::spawn(move || {
        run.manage();
        let mut all = links().lock().unwrap();
        if all.get(&key).is_some_and(|x| Arc::ptr_eq(x, &run)) {
            all.remove(&key);
        }
    });
    l
}

fn send(w: &Writer, kind: u8, sid: &Sid, offset: u64, payload: &[u8]) -> io::Result<()> {
    mux::write(&mut *w.lock().unwrap(), kind, sid, offset, payload)
}

impl Link {
    fn sender(self: &Arc<Self>, sid: Sid, c: Arc<Client>, generation: u64, w: Writer) {
        let link = self.clone();
        thread::spawn(move || {
            loop {
                if link.state.lock().unwrap().generation != generation {
                    return;
                }
                match c.input.take(64 << 10, Duration::from_millis(200)) {
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

    /// a session on a link that is up: opened, or taken back after a drop
    fn start(self: &Arc<Self>, sid: Sid, c: &Arc<Client>, generation: u64, w: &Writer) {
        if c.confirmed.load(Ordering::SeqCst) {
            let received = c.output.lock().unwrap().received;
            let _ = send(w, mux::ATTACH, &sid, received, &[]);
        } else {
            c.input.rewind(0);
            let _ = send(w, mux::OPEN, &sid, 0, &c.spawn);
            self.sender(sid, c.clone(), generation, w.clone());
        }
    }

    fn manage(self: &Arc<Self>) {
        let mut backoff = Duration::from_millis(500);
        let mut down_since: Option<Instant> = None;
        loop {
            match self.connect() {
                Ok((killer, reader, w)) => {
                    backoff = Duration::from_millis(500);
                    down_since = None;
                    self.carry(killer, reader, w);
                }
                Err(e) => {
                    if !self.state.lock().unwrap().up_once {
                        self.fail_all(&format!("{}: {e}", self.host.name));
                        return;
                    }
                    log(&format!("link to {}: {e}", self.host.name));
                }
            }
            if self.state.lock().unwrap().clients.is_empty() {
                self.state.lock().unwrap().closing = true;
                return;
            }
            let since = *down_since.get_or_insert_with(Instant::now);
            if since.elapsed() > grace() {
                self.fail_all(&format!(
                    "the connection to {} was lost and did not come back",
                    self.host.name
                ));
                return;
            }
            let jitter = Duration::from_millis(u64::from(mux::new_sid()[0]) * 2);
            thread::sleep(backoff + jitter);
            backoff = (backoff * 2).min(Duration::from_secs(30));
        }
    }

    fn fail_all(&self, msg: &str) {
        let mut st = self.state.lock().unwrap();
        st.closing = true;
        for c in st.clients.values() {
            c.fail(msg);
        }
        st.clients.clear();
        self.changed.notify_all();
    }

    /// the host's server over TCP, each side's key proved to the other, and that server agreeing to a link
    fn connect(&self) -> io::Result<(TcpStream, noise::Reader, Writer)> {
        let address = keys::with_port(&self.host.address);
        let addrs: Vec<_> = address.to_socket_addrs()?.collect();
        let mut tcp = Err(io::Error::new(
            io::ErrorKind::NotFound,
            format!("no address for {address}"),
        ));
        for a in &addrs {
            tcp = TcpStream::connect_timeout(a, Duration::from_secs(5));
            if tcp.is_ok() {
                break;
            }
        }
        let tcp =
            tcp.map_err(|e| io::Error::new(e.kind(), format!("cannot reach {address}: {e}")))?;
        let _ = tcp.set_nodelay(true);
        let killer = tcp.try_clone()?;
        let (private, _) = keys::mine()?;
        let (mut reader, mut writer, _) = noise::initiate(tcp, &private, |k| *k == self.host.key)?;
        match proto::read_frame(&mut reader)? {
            Some((proto::HELLO, payload)) => {
                let hello = Hello::decode(&payload)?;
                if hello.protocol != proto::PROTOCOL {
                    return Err(io::Error::other(format!(
                        "delishd there is {} and speaks protocol {}, this delish {} protocol {}: update delish on both machines, then restart delishd",
                        hello.version,
                        hello.protocol,
                        proto::VERSION,
                        proto::PROTOCOL
                    )));
                }
            }
            _ => return Err(io::Error::other("its server did not say hello")),
        }
        proto::write_frame(&mut writer, proto::LINK, &proto::PROTOCOL.to_le_bytes())?;
        match proto::read_frame(&mut reader)? {
            Some((proto::LINK, _)) => {}
            Some((proto::ERROR, msg)) => {
                return Err(io::Error::other(String::from_utf8_lossy(&msg).into_owned()));
            }
            _ => return Err(io::Error::other("the server there would not take a link")),
        }
        let w: Writer = Arc::new(Mutex::new(Box::new(writer)));
        Ok((killer, reader, w))
    }

    /// the link's frames until it drops, or closes idle
    fn carry(self: &Arc<Self>, killer: TcpStream, mut stdout: noise::Reader, w: Writer) {
        let child = Arc::new(Mutex::new(killer));
        let generation = {
            let mut st = self.state.lock().unwrap();
            st.generation += 1;
            st.writer = Some(w.clone());
            st.up_once = true;
            for sid in st.closed_while_down.drain(..) {
                let _ = send(&w, mux::CLOSE, &sid, 0, &[]);
            }
            let g = st.generation;
            for (sid, c) in st.clients.iter() {
                self.start(*sid, c, g, &w);
            }
            g
        };
        log(&format!("link to {} up", self.host.name));

        let heard = Arc::new(Mutex::new(Instant::now()));
        let up = Arc::new(AtomicBool::new(true));
        {
            let (link, w, child, heard, up) = (
                self.clone(),
                w.clone(),
                child.clone(),
                heard.clone(),
                up.clone(),
            );
            thread::spawn(move || {
                let idle = seconds("DELISH_LINK_IDLE", 600);
                let mut pinged = Instant::now();
                let mut empty_since: Option<Instant> = None;
                while up.load(Ordering::SeqCst) {
                    thread::sleep(Duration::from_millis(250));
                    let empty = link.state.lock().unwrap().clients.is_empty();
                    empty_since = if empty {
                        empty_since.or(Some(Instant::now()))
                    } else {
                        None
                    };
                    let idled = empty_since.is_some_and(|t| t.elapsed() > idle);
                    if idled {
                        link.state.lock().unwrap().closing = true;
                    }
                    if idled || heard.lock().unwrap().elapsed() > DEAD_AFTER {
                        let _ = child.lock().unwrap().shutdown(std::net::Shutdown::Both);
                        return;
                    }
                    if pinged.elapsed() > PING_EVERY {
                        pinged = Instant::now();
                        let _ = send(&w, mux::PING, &mux::NO_SID, 0, &[]);
                    }
                }
            });
        }

        while let Ok(Some(f)) = mux::read(&mut stdout) {
            *heard.lock().unwrap() = Instant::now();
            let client = self.state.lock().unwrap().clients.get(&f.sid).cloned();
            match (f.kind, client) {
                (mux::DATA, Some(c)) => {
                    c.confirmed.store(true, Ordering::SeqCst);
                    let received = {
                        let mut inbox = c.output.lock().unwrap();
                        let Ok(new) = inbox.accept(f.offset, &f.payload) else {
                            break;
                        };
                        if c.stream.lock().unwrap().write_all(new).is_err() {
                            continue;
                        }
                        inbox.received
                    };
                    let _ = send(&w, mux::ACK, &f.sid, received, &[]);
                }
                (mux::ACK, Some(c)) => {
                    c.confirmed.store(true, Ordering::SeqCst);
                    c.input.ack(f.offset);
                }
                (mux::ATTACHED, Some(c)) => {
                    c.input.rewind(f.offset);
                    self.sender(f.sid, c, generation, w.clone());
                }
                (mux::GONE, Some(c)) => {
                    self.state.lock().unwrap().clients.remove(&f.sid);
                    c.fail(&format!(
                        "{}: {}",
                        self.host.name,
                        String::from_utf8_lossy(&f.payload)
                    ));
                }
                (mux::PING, _) => {
                    let _ = send(&w, mux::PONG, &mux::NO_SID, 0, &[]);
                }
                _ => {}
            }
        }
        up.store(false, Ordering::SeqCst);
        {
            let mut st = self.state.lock().unwrap();
            st.writer = None;
            st.generation += 1;
        }
        let _ = child.lock().unwrap().shutdown(std::net::Shutdown::Both);
        log(&format!("link to {} down", self.host.name));
    }

    fn add(self: &Arc<Self>, sid: Sid, c: Arc<Client>) {
        let mut st = self.state.lock().unwrap();
        st.clients.insert(sid, c.clone());
        if let Some(w) = st.writer.clone() {
            let g = st.generation;
            drop(st);
            self.start(sid, &c, g, &w);
        }
        self.changed.notify_all();
    }

    fn remove(&self, sid: &Sid) {
        let mut st = self.state.lock().unwrap();
        if let Some(c) = st.clients.remove(sid) {
            c.input.close();
            match st.writer.clone() {
                Some(w) => {
                    let _ = send(&w, mux::CLOSE, sid, 0, &[]);
                }
                None => st.closed_while_down.push(*sid),
            }
        }
    }
}

/// a client's `delish <host>`: its session on the host, over this server's link there
pub fn serve_client(mut stream: Stream, payload: &[u8]) -> io::Result<()> {
    let refuse =
        |stream: &mut Stream, msg: &str| proto::write_frame(stream, proto::ERROR, msg.as_bytes());
    let req = match Request::decode(payload) {
        Ok(r) => r,
        Err(e) => return refuse(&mut stream, &e.to_string()),
    };
    let Some(host) = resolve(&req.host) else {
        return refuse(
            &mut stream,
            &format!(
                "{} is not a known host: add it with delish trust <name> <address[:port]> <its delish key>",
                req.host
            ),
        );
    };
    let link = link_for(host);
    let sid = mux::new_sid();
    let client = Arc::new(Client {
        stream: Mutex::new(stream.try_clone()?),
        output: Mutex::new(Inbox::default()),
        input: Outbox::new(IN_CAP),
        spawn: req.spawn,
        confirmed: AtomicBool::new(false),
    });
    link.add(sid, client.clone());

    // the client's frames, as bytes, on their way to the session; a client that goes ends its session
    let mut buf = [0u8; 16 << 10];
    while let Ok(n) = stream.read(&mut buf) {
        if n == 0 || !client.input.push(&buf[..n]) {
            break;
        }
    }
    link.remove(&sid);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_remote_request_round_trips() {
        let r = Request {
            host: "glass".into(),
            spawn: vec![1, 2, 3],
        };
        assert_eq!(Request::decode(&r.encode()).unwrap(), r);
    }
}
