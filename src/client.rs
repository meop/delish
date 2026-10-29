//! `delish run`, `shell` and `<host>`: asks a `delishd` to run a command in its desktop session, and is that
//! command's terminal here — keys and resizes go there, its output comes back, and its exit code is this one's. The
//! server is always this user's own, on its socket; another host's sessions go over its links.

use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::proto::{self, Hello, Size, Spawn};
use crate::sock;
use crate::term;

const FALLBACK: Size = Size { rows: 24, cols: 80 };

/// how the frames ended: the command's code, or the connection's end before it, or before the server's hello
enum End {
    Exit(i32),
    Closed,
    NoHello,
}

/// what a server older than the hello does with this protocol's spawn: it cannot read it, and hangs up
fn older_server(who: &str) -> i32 {
    eprintln!(
        "delish: {who}delishd hung up without saying its version, so it is older than this delish {}: update delish there, then restart delishd",
        proto::VERSION
    );
    255
}

type Sink = Arc<Mutex<Box<dyn Write + Send>>>;

/// `login` runs the user's login shell, interactive or with the one command in argv
pub fn run(argv: Vec<String>, login: bool) -> i32 {
    let stream = match sock::connect() {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "delish: no delishd in this user's desktop session ({}): {e}",
                sock::path().display()
            );
            return 255;
        }
    };
    let cwd = std::env::current_dir()
        .map(|d| d.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ended = stream.try_clone().and_then(|up| {
        let s = spawn(argv, login, cwd);
        relay(stream, Box::new(up), (proto::SPAWN, s.encode()), s.size, "")
    });
    match ended {
        Ok(End::Exit(code)) => code,
        Ok(End::Closed) => {
            eprintln!("delish: delishd closed the connection before the command exited");
            255
        }
        Ok(End::NoHello) => older_server(""),
        Err(e) => {
            eprintln!("delish: {e}");
            255
        }
    }
}

/// whether this user's server is listening, and is this delish's version: 0 when it is, 1 with no server, 3 with
/// another version, so a script can tell that a restart is due
pub fn status() -> i32 {
    let path = sock::path();
    let mut stream = match sock::connect() {
        Ok(s) => s,
        Err(e) => {
            println!("no delishd on {}: {e}", path.display());
            return 1;
        }
    };
    // a server older than the hello says nothing until it is sent a spawn
    let _ = stream.set_read_timeout(Some(Duration::from_secs(2)));
    let hello = match proto::read_frame(&mut stream) {
        Ok(Some((proto::HELLO, payload))) => Hello::decode(&payload).ok(),
        _ => None,
    };
    match hello {
        Some(h) if h.version == proto::VERSION => {
            println!("delishd {} is listening on {}", h.version, path.display());
            0
        }
        Some(h) => {
            println!(
                "delishd {} is listening on {}, and this delish is {}: restart delishd to run this one",
                h.version,
                path.display(),
                proto::VERSION
            );
            3
        }
        None => {
            println!(
                "a delishd older than this delish {} is listening on {}: restart delishd to run this one",
                proto::VERSION,
                path.display()
            );
            3
        }
    }
}

/// the host's login shell, run by the host's server: interactive, or running the command. the cwd here means nothing
/// there, so the command starts in the user's home. it goes over this user's own server's link to the host, which
/// holds this machine's key
pub fn remote(host: String, command: Option<String>) -> i32 {
    let stream = match sock::connect() {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "delish: no delishd in this user's desktop session ({}) to reach {host} through: {e}",
                sock::path().display()
            );
            return 255;
        }
    };
    let s = spawn(command.into_iter().collect(), true, String::new());
    let req = crate::link::Request {
        host: host.clone(),
        spawn: s.encode(),
    };
    let ended = stream.try_clone().and_then(|up| {
        relay(
            stream,
            Box::new(up),
            (proto::REMOTE, req.encode()),
            s.size,
            "",
        )
    });
    match ended {
        Ok(End::Exit(code)) => code,
        Ok(End::Closed) => {
            eprintln!("delish: the session on {host} ended before its command exited");
            255
        }
        Ok(End::NoHello) => older_server(""),
        Err(e) => {
            eprintln!("delish: {e}");
            255
        }
    }
}

fn spawn(argv: Vec<String>, login: bool, cwd: String) -> Spawn {
    let env = ["TERM", "COLORTERM"]
        .iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v)))
        .collect();
    Spawn {
        size: term::size().unwrap_or(FALLBACK),
        login,
        stdin_tty: term::is_tty(),
        stdout_tty: term::stdout_is_tty(),
        stderr_tty: term::stderr_is_tty(),
        cwd,
        argv,
        env,
    }
}

/// the spawn goes first. the terminal is taken over — raw, its keys read — only once the server has said HELLO in
/// this protocol. `who` names the server's host in what it says
fn relay(
    mut down: impl Read,
    up: Box<dyn Write + Send>,
    first: (u8, Vec<u8>),
    size: Size,
    who: &str,
) -> io::Result<End> {
    let up: Sink = Arc::new(Mutex::new(up));
    proto::write_frame(&mut *up.lock().unwrap(), first.0, &first.1)?;

    let mut started = false;
    let mut raw = None;
    let mut stdout = io::stdout().lock();
    let end = loop {
        match proto::read_frame(&mut down)? {
            Some((proto::HELLO, payload)) if !started => {
                let hello = Hello::decode(&payload)?;
                if hello.protocol != proto::PROTOCOL {
                    eprintln!(
                        "delish: {who}delishd is {} and speaks protocol {}, this delish {} protocol {}: update delish on both machines, then restart delishd",
                        hello.version,
                        hello.protocol,
                        proto::VERSION,
                        proto::PROTOCOL
                    );
                    break End::Exit(255);
                }
                started = true;
                raw = take_over(&up, size);
            }
            Some((proto::ERROR, msg)) => {
                drop(raw.take());
                eprintln!("delish: {who}{}", String::from_utf8_lossy(&msg));
                break End::Exit(255);
            }
            // a server older than the hello runs the command and answers with its output
            Some(_) if !started => {
                eprintln!(
                    "delish: {who}delishd is older than this delish {} and says no version: update delish there, then restart delishd",
                    proto::VERSION
                );
                break End::Exit(255);
            }
            Some((proto::OUTPUT, bytes)) => {
                stdout.write_all(&bytes)?;
                stdout.flush()?;
            }
            Some((proto::STDERR, bytes)) => {
                let mut stderr = io::stderr().lock();
                stderr.write_all(&bytes)?;
                stderr.flush()?;
            }
            Some((proto::EXIT, payload)) if payload.len() >= 4 => {
                break End::Exit(i32::from_le_bytes([
                    payload[0], payload[1], payload[2], payload[3],
                ]));
            }
            Some(_) => {}
            None if !started => break End::NoHello,
            None => break End::Closed,
        }
    };
    drop(raw);
    Ok(end)
}

/// raw mode when stdin is a terminal, and the threads that send its keys and its resizes. the guard is Some only for
/// a terminal; for anything else there is nothing to put back
fn take_over(up: &Sink, size: Size) -> Option<term::Raw> {
    let tty = term::is_tty();
    let raw = if tty { term::Raw::enter() } else { None };

    let keys = up.clone();
    thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        let mut buf = [0u8; 4096];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => {
                    if !tty {
                        let _ = proto::write_frame(&mut *keys.lock().unwrap(), proto::EOF, &[]);
                    }
                    break;
                }
                Ok(n) => {
                    if proto::write_frame(&mut *keys.lock().unwrap(), proto::INPUT, &buf[..n])
                        .is_err()
                    {
                        break;
                    }
                }
            }
        }
    });

    // a resize has no portable signal to wait on, so the size is looked at a few times a second
    if tty {
        let resizes = up.clone();
        thread::spawn(move || {
            let mut last = size;
            loop {
                thread::sleep(Duration::from_millis(250));
                if let Some(now) = term::size()
                    && now != last
                {
                    last = now;
                    if proto::write_frame(
                        &mut *resizes.lock().unwrap(),
                        proto::RESIZE,
                        &now.encode(),
                    )
                    .is_err()
                    {
                        break;
                    }
                }
            }
        });
    }
    raw
}
