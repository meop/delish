//! `deskpty run`: asks the user's `deskptyd` to run a command in the desktop session, and is that command's terminal
//! here — keys and resizes go there, its output comes back, and its exit code is this one's.

use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::proto::{self, Size, Spawn};
use crate::sock;
use crate::term;

const FALLBACK: Size = Size { rows: 24, cols: 80 };

pub fn run(argv: Vec<String>) -> i32 {
    let stream = match sock::connect() {
        Ok(s) => s,
        Err(e) => {
            eprintln!(
                "deskpty: no deskptyd in this user's desktop session ({}): {e}",
                sock::path().display()
            );
            return 255;
        }
    };
    match relay(stream, argv) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("deskpty: {e}");
            255
        }
    }
}

fn relay(mut stream: sock::Stream, argv: Vec<String>) -> io::Result<i32> {
    let tty = term::is_tty();
    let size = term::size().unwrap_or(FALLBACK);
    let env = ["TERM", "COLORTERM"]
        .iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v)))
        .collect();
    let cwd = std::env::current_dir()
        .map(|d| d.to_string_lossy().into_owned())
        .unwrap_or_default();
    proto::write_frame(
        &mut stream,
        proto::SPAWN,
        &Spawn {
            size,
            cwd,
            argv,
            env,
        }
        .encode(),
    )?;

    let raw = if tty { term::Raw::enter() } else { None };
    let to_server = Arc::new(Mutex::new(stream.try_clone()?));

    let keys = to_server.clone();
    thread::spawn(move || {
        let mut stdin = io::stdin().lock();
        let mut buf = [0u8; 4096];
        loop {
            match stdin.read(&mut buf) {
                Ok(0) | Err(_) => break,
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
        let resizes = to_server.clone();
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

    let mut stdout = io::stdout().lock();
    let code = loop {
        match proto::read_frame(&mut stream)? {
            Some((proto::OUTPUT, bytes)) => {
                stdout.write_all(&bytes)?;
                stdout.flush()?;
            }
            Some((proto::EXIT, payload)) if payload.len() >= 4 => {
                break i32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]);
            }
            Some((proto::ERROR, msg)) => {
                drop(raw);
                eprintln!("deskpty: {}", String::from_utf8_lossy(&msg));
                return Ok(255);
            }
            Some(_) => {}
            None => {
                drop(raw);
                eprintln!("deskpty: deskptyd closed the connection before the command exited");
                return Ok(255);
            }
        }
    };
    drop(raw);
    Ok(code)
}
