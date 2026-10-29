//! This server's links to other hosts' servers: one ssh connection per target, carrying every session this server's
//! clients have there, kept a while after the last ends, and brought back after a drop with no more than a pause seen.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::process::{Child, ChildStdout, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;
use std::time::{Duration, Instant};

use crate::hosted::{DEAD_AFTER, PING_EVERY, grace, seconds};
use crate::mux::{self, Inbox, Outbox, Sid};
use crate::proto::{self, Hello};
use crate::server::log;
use crate::sock::Stream;
use crate::ssh;

/// how much a client may type ahead of what the other end has acknowledged
const IN_CAP: usize = 1 << 20;

/// what `delish <host>` asks its own server for: ssh's options and host, what runs there to reach that host's
/// server, and the session's spawn
#[derive(Debug, PartialEq, Eq)]
pub struct Request {
    pub args: Vec<String>,
    pub remote: String,
    pub spawn: Vec<u8>,
}

impl Request {
    pub fn encode(&self) -> Vec<u8> {
        let mut b = (self.args.len() as u32).to_le_bytes().to_vec();
        for a in self.args.iter().chain([&self.remote]) {
            b.extend_from_slice(&(a.len() as u32).to_le_bytes());
            b.extend_from_slice(a.as_bytes());
        }
        b.extend_from_slice(&self.spawn);
        b
    }

    pub fn decode(b: &[u8]) -> io::Result<Self> {
        let bad = || io::Error::new(io::ErrorKind::InvalidData, "short remote request");
        let mut at = 0;
        let mut take = |n: usize| -> io::Result<&[u8]> {
            let s = b.get(at..at + n).ok_or_else(bad)?;
            at += n;
            Ok(s)
        };
        let count = u32::from_le_bytes(take(4)?.try_into().unwrap()) as usize;
        let mut strings = Vec::new();
        for _ in 0..=count {
            let len = u32::from_le_bytes(take(4)?.try_into().unwrap()) as usize;
            strings.push(String::from_utf8(take(len)?.to_vec()).map_err(|_| bad())?);
        }
        let remote = strings.pop().unwrap_or_default();
        Ok(Request {
            args: strings,
            remote,
            spawn: b[at..].to_vec(),
        })
    }
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
    host: String,
    target: ssh::Target,
    remote: String,
    state: Mutex<State>,
    changed: Condvar,
}

fn links() -> &'static Mutex<HashMap<String, Arc<Link>>> {
    static L: OnceLock<Mutex<HashMap<String, Arc<Link>>>> = OnceLock::new();
    L.get_or_init(|| Mutex::new(HashMap::new()))
}

fn link_for(target: ssh::Target, remote: String) -> Arc<Link> {
    let key = format!("{}\0{}\0{}", target.options.join("\0"), target.host, remote);
    let mut all = links().lock().unwrap();
    if let Some(l) = all.get(&key)
        && !l.state.lock().unwrap().closing
    {
        return l.clone();
    }
    let l = Arc::new(Link {
        host: target.host.clone(),
        target,
        remote,
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
                Ok((child, stdout, w)) => {
                    backoff = Duration::from_millis(500);
                    down_since = None;
                    self.carry(child, stdout, w);
                }
                Err(e) => {
                    if !self.state.lock().unwrap().up_once {
                        self.fail_all(&format!("{}: {e}", self.host));
                        return;
                    }
                    log(&format!("link to {}: {e}", self.host));
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
                    self.host
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

    /// ssh to the host, to its server, and that server agreeing to a link
    fn connect(&self) -> io::Result<(Child, ChildStdout, Writer)> {
        let program = std::env::var("DELISH_SSH").unwrap_or_else(|_| "ssh".into());
        let mut args = vec!["-o".to_string(), "BatchMode=yes".to_string()];
        args.extend(ssh::argv(&self.target, &self.remote));
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let mut stdout = child.stdout.take().unwrap();
        let mut stderr = child.stderr.take().unwrap();
        let said = Arc::new(Mutex::new(Vec::new()));
        {
            let said = said.clone();
            thread::spawn(move || {
                let mut buf = [0u8; 4096];
                while let Ok(n) = stderr.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    let mut s = said.lock().unwrap();
                    s.extend_from_slice(&buf[..n]);
                    let over = s.len().saturating_sub(4096);
                    s.drain(..over);
                }
            });
        }
        let w: Writer = Arc::new(Mutex::new(Box::new(child.stdin.take().unwrap())));
        let refused = |child: &mut Child, why: String| {
            let _ = child.kill();
            let _ = child.wait();
            thread::sleep(Duration::from_millis(50));
            let ssh_said = String::from_utf8_lossy(&said.lock().unwrap())
                .trim()
                .to_string();
            io::Error::other(if ssh_said.is_empty() {
                why
            } else {
                format!("{why}: {ssh_said}")
            })
        };
        loop {
            match proto::read_frame(&mut stdout) {
                Ok(Some((proto::READY, _))) => {}
                Ok(Some((proto::HELLO, payload))) => {
                    let hello = Hello::decode(&payload)?;
                    if hello.protocol != proto::PROTOCOL {
                        return Err(refused(
                            &mut child,
                            format!(
                                "delishd there is {} and speaks protocol {}, this delish {} protocol {}: update delish on both machines, then restart delishd",
                                hello.version,
                                hello.protocol,
                                proto::VERSION,
                                proto::PROTOCOL
                            ),
                        ));
                    }
                    break;
                }
                Ok(Some((proto::ERROR, msg))) => {
                    return Err(refused(
                        &mut child,
                        String::from_utf8_lossy(&msg).into_owned(),
                    ));
                }
                Ok(_) | Err(_) => {
                    return Err(refused(
                        &mut child,
                        "the connection closed before its server answered".into(),
                    ));
                }
            }
        }
        proto::write_frame(
            &mut *w.lock().unwrap(),
            proto::LINK,
            &proto::PROTOCOL.to_le_bytes(),
        )?;
        match proto::read_frame(&mut stdout) {
            Ok(Some((proto::LINK, _))) => Ok((child, stdout, w)),
            Ok(Some((proto::ERROR, msg))) => Err(refused(
                &mut child,
                String::from_utf8_lossy(&msg).into_owned(),
            )),
            _ => Err(refused(
                &mut child,
                "the server there would not take a link".into(),
            )),
        }
    }

    /// the link's frames until it drops, or closes idle
    fn carry(self: &Arc<Self>, child: Child, mut stdout: ChildStdout, w: Writer) {
        let child = Arc::new(Mutex::new(child));
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
        log(&format!("link to {} up", self.host));

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
                        let _ = child.lock().unwrap().kill();
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
                        self.host,
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
        let mut child = child.lock().unwrap();
        let _ = child.kill();
        let _ = child.wait();
        log(&format!("link to {} down", self.host));
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
    let Some(target) = ssh::parse(&req.args) else {
        return refuse(&mut stream, "no host to run on");
    };
    let link = link_for(target, req.remote);
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
            args: vec!["-p".into(), "2222".into(), "--".into(), "u@glass".into()],
            remote: "delish proxy".into(),
            spawn: vec![1, 2, 3],
        };
        assert_eq!(Request::decode(&r.encode()).unwrap(), r);
    }
}
