//! `deskpty proxy`: what `deskpty <host>` runs on the host over ssh. It joins its stdin and stdout to the user's
//! `deskptyd` byte for byte, after one frame of its own: READY once it has the server, or ERROR when there is none.

use std::io::{self, Read, Write};
use std::net::Shutdown;
use std::thread;

use crate::proto;
use crate::sock;

pub fn run() -> i32 {
    let mut stdout = io::stdout().lock();
    let stream = match sock::connect() {
        Ok(s) => s,
        Err(e) => {
            let msg = format!(
                "no deskptyd in this user's desktop session ({}): {e}",
                sock::path().display()
            );
            let _ = proto::write_frame(&mut stdout, proto::ERROR, msg.as_bytes());
            return 1;
        }
    };
    if proto::write_frame(&mut stdout, proto::READY, &[]).is_err() {
        return 1;
    }

    // the client's frames in. when ssh's side ends, the server sees its client go, and ends the command
    let mut up = match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return 1,
    };
    thread::spawn(move || {
        let _ = io::copy(&mut io::stdin().lock(), &mut up);
        let _ = up.shutdown(Shutdown::Write);
    });

    // the server's frames out, as they come: stdout is line buffered, so each read is flushed
    let mut down = stream;
    let mut buf = [0u8; 16 * 1024];
    loop {
        match down.read(&mut buf) {
            Ok(0) | Err(_) => break,
            Ok(n) => {
                if stdout
                    .write_all(&buf[..n])
                    .and_then(|_| stdout.flush())
                    .is_err()
                {
                    break;
                }
            }
        }
    }
    0
}
