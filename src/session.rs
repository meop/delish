//! One command on its terminal, whatever carries its frames: the server's own connection to a client, or a link from
//! another server that carries many.

use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crate::proto::{self, Size, Spawn};
use crate::pty;
use crate::shell;

/// where a session's frames go: OUTPUT and STDERR as they come, EXIT last. an error stops the reader that met it
pub type Sink = Arc<dyn Fn(u8, &[u8]) -> io::Result<()> + Send + Sync>;

pub struct Session {
    writer: Mutex<pty::Writer>,
    control: Arc<pty::Control>,
    ended: Arc<AtomicBool>,
    waiter: Mutex<Option<thread::JoinHandle<i32>>>,
    pub program: String,
}

/// what the log says a command is: its program, never its arguments, which can carry what is not the log's to keep
pub fn program(argv: &[String]) -> String {
    argv.first()
        .map(|p| {
            std::path::Path::new(p)
                .file_name()
                .map_or(p.clone(), |n| n.to_string_lossy().into_owned())
        })
        .unwrap_or_default()
}

impl Session {
    /// the command started, its output on its way to sink; an error is why it could not be run
    pub fn start(mut spawn: Spawn, sink: Sink) -> Result<Session, String> {
        if spawn.login {
            spawn.argv = shell::login_argv(spawn.argv.first().map(String::as_str));
        }
        let name = program(&spawn.argv);
        let (readers, writer, control) =
            pty::spawn(&spawn).map_err(|e| format!("could not run {:?}: {e}", spawn.argv[0]))?;
        let control = Arc::new(control);

        let (drained, read_done) = mpsc::channel::<()>();
        let outputs = readers.len();
        for mut reader in readers {
            let sink = sink.clone();
            let drained = drained.clone();
            let kind = if reader.stderr {
                proto::STDERR
            } else {
                proto::OUTPUT
            };
            thread::spawn(move || {
                let mut buf = [0u8; 16 * 1024];
                while let Ok(n) = reader.read(&mut buf) {
                    if n == 0 || sink(kind, &buf[..n]).is_err() {
                        break;
                    }
                }
                let _ = drained.send(());
            });
        }

        let ended = Arc::new(AtomicBool::new(false));
        let waiter = {
            let (control, ended) = (control.clone(), ended.clone());
            thread::spawn(move || {
                let code = control.wait().unwrap_or(255);
                ended.store(true, Ordering::SeqCst);
                control.close();
                // a background process still holding the terminal would keep it open forever: the wait for it is
                // bounded, and then the readers stop, so the terminal closes under it as a closed window's does
                let until = Instant::now() + Duration::from_secs(2);
                for _ in 0..outputs {
                    let left = until.saturating_duration_since(Instant::now());
                    if read_done.recv_timeout(left).is_err() {
                        break;
                    }
                }
                control.stop();
                let _ = sink(proto::EXIT, &code.to_le_bytes());
                code
            })
        };
        Ok(Session {
            writer: Mutex::new(writer),
            control,
            ended,
            waiter: Mutex::new(Some(waiter)),
            program: name,
        })
    }

    pub fn input(&self, bytes: &[u8]) {
        let _ = self.writer.lock().unwrap().write_all(bytes);
    }

    pub fn eof(&self) {
        self.writer.lock().unwrap().close();
    }

    pub fn resize(&self, size: Size) {
        let _ = self.control.resize(size);
    }

    /// one of the client's frames, as the client sent it
    pub fn apply(&self, kind: u8, payload: &[u8]) {
        match kind {
            proto::INPUT => self.input(payload),
            proto::EOF => self.eof(),
            proto::RESIZE => {
                if let Ok(size) = Size::decode(payload) {
                    self.resize(size);
                }
            }
            _ => {}
        }
    }

    /// what a closed terminal does to the command, unless it has ended: its pid may be another process's by then
    pub fn hang_up(&self) {
        if !self.ended.load(Ordering::SeqCst) {
            self.control.kill();
        }
    }

    pub fn has_ended(&self) -> bool {
        self.ended.load(Ordering::SeqCst)
    }

    /// the command's exit code, once it and its output are done
    pub fn wait(&self) -> i32 {
        match self.waiter.lock().unwrap().take() {
            Some(w) => w.join().unwrap_or(255),
            None => 255,
        }
    }
}
