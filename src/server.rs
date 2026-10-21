//! `deskptyd`: started inside the desktop session, it runs each command a client sends on a pty of its own, so the
//! command has the session's context — its env, its window server, its token — whoever asked for it.

use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::proto::{self, Size, Spawn};
use crate::pty;
use crate::sock::{self, Stream};

/// a line in ~/.deskpty/deskptyd.log: the server has no terminal to say it on
pub fn log(msg: &str) {
    let path = sock::dir().join("deskptyd.log");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let _ = writeln!(f, "{now} {msg}");
    }
}

pub fn serve() -> io::Result<()> {
    let listener = sock::listen()?;
    log(&format!(
        "listening on {} (pid {})",
        sock::path().display(),
        std::process::id()
    ));
    for conn in listener.incoming() {
        match conn {
            Ok(stream) => {
                thread::spawn(move || {
                    if let Err(e) = handle(stream) {
                        log(&format!("connection: {e}"));
                    }
                });
            }
            Err(e) => log(&format!("accept: {e}")),
        }
    }
    Ok(())
}

fn handle(mut stream: Stream) -> io::Result<()> {
    let spawn = match proto::read_frame(&mut stream)? {
        Some((proto::SPAWN, payload)) => Spawn::decode(&payload)?,
        // `deskpty status` connects and goes: nothing to run, and nothing wrong
        None => return Ok(()),
        Some(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "first frame is not a spawn",
            ));
        }
    };
    let (mut reader, mut writer, control) = match pty::spawn(&spawn) {
        Ok(p) => p,
        Err(e) => {
            let msg = format!("could not run {:?}: {e}", spawn.argv[0]);
            log(&msg);
            return proto::write_frame(&mut stream, proto::ERROR, msg.as_bytes());
        }
    };
    log(&format!("running {:?}", spawn.argv));
    let control = Arc::new(control);
    let out = Arc::new(Mutex::new(stream.try_clone()?));

    // the terminal's output, as it comes
    let (drained, read_done) = mpsc::channel::<()>();
    let reader_out = out.clone();
    thread::spawn(move || {
        let mut buf = [0u8; 16 * 1024];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    if proto::write_frame(
                        &mut *reader_out.lock().unwrap(),
                        proto::OUTPUT,
                        &buf[..n],
                    )
                    .is_err()
                    {
                        break;
                    }
                }
            }
        }
        let _ = drained.send(());
    });

    // the command's end: what it wrote last goes out before its exit code, and the connection closes after
    let waiter_control = control.clone();
    let waiter_out = out.clone();
    let shutdown = stream.try_clone()?;
    let ended = Arc::new(AtomicBool::new(false));
    let waiter_ended = ended.clone();
    let waiter = thread::spawn(move || {
        let code = waiter_control.wait().unwrap_or(255);
        waiter_ended.store(true, Ordering::SeqCst);
        waiter_control.close();
        // a background process still holding the terminal would keep it open forever: the wait for it is bounded
        let _ = read_done.recv_timeout(Duration::from_secs(2));
        let _ = proto::write_frame(
            &mut *waiter_out.lock().unwrap(),
            proto::EXIT,
            &code.to_le_bytes(),
        );
        let _ = shutdown.shutdown(std::net::Shutdown::Both);
        code
    });

    // the client's keys and resizes. a client that goes away takes its command with it, as a closed terminal does
    loop {
        match proto::read_frame(&mut stream) {
            Ok(Some((proto::INPUT, bytes))) => {
                let _ = writer.write_all(&bytes);
            }
            Ok(Some((proto::RESIZE, payload))) => {
                if let Ok(size) = Size::decode(&payload) {
                    let _ = control.resize(size);
                }
            }
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => break,
        }
    }
    // never once it has been waited on: its pid may be another process's by now
    if !ended.load(Ordering::SeqCst) {
        control.kill();
    }
    let code = waiter.join().unwrap_or(255);
    log(&format!("{:?} exited {code}", spawn.argv));
    Ok(())
}
