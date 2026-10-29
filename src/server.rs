//! `delishd`: started inside the desktop session, it runs each command a client sends on a pty of its own, so the
//! command has the session's context — its env, its window server, its token — whoever asked for it.

use std::io::{self, Read, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::proto::{self, Hello, Size, Spawn};
use crate::pty;
use crate::shell;
use crate::sock::{self, Stream};

/// a line in ~/.delish/delishd.log: the server has no terminal to say it on
pub fn log(msg: &str) {
    let path = sock::dir().join("delishd.log");
    rotate(&path, LOG_LIMIT);
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

const LOG_LIMIT: u64 = 1 << 20;

/// a log past limit starts again, the one before kept beside it as .1
fn rotate(path: &std::path::Path, limit: u64) {
    if std::fs::metadata(path).is_ok_and(|m| m.len() > limit) {
        let _ = std::fs::rename(path, path.with_extension("log.1"));
    }
}

/// what the log says a command is: its program, never its arguments, which can carry what is not the log's to keep
fn program(argv: &[String]) -> String {
    argv.first()
        .map(|p| {
            std::path::Path::new(p)
                .file_name()
                .map_or(p.clone(), |n| n.to_string_lossy().into_owned())
        })
        .unwrap_or_default()
}

pub fn serve() -> io::Result<()> {
    session_check()?;
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
    proto::write_frame(&mut stream, proto::HELLO, &Hello::ours().encode())?;
    let mut spawn = match proto::read_frame(&mut stream)? {
        Some((proto::SPAWN, payload)) => match Spawn::decode(&payload) {
            Ok(s) => s,
            Err(e) => {
                log(&format!("refused: {e}"));
                return proto::write_frame(&mut stream, proto::ERROR, e.to_string().as_bytes());
            }
        },
        // `delish status` connects and goes: nothing to run, and nothing wrong
        None => return Ok(()),
        Some(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "first frame is not a spawn",
            ));
        }
    };
    if spawn.login {
        spawn.argv = shell::login_argv(spawn.argv.first().map(String::as_str));
    }
    let (readers, mut writer, control) = match pty::spawn(&spawn) {
        Ok(p) => p,
        Err(e) => {
            let msg = format!("could not run {:?}: {e}", spawn.argv[0]);
            log(&format!("could not run {}: {e}", program(&spawn.argv)));
            return proto::write_frame(&mut stream, proto::ERROR, msg.as_bytes());
        }
    };
    log(&format!("running {}", program(&spawn.argv)));
    let control = Arc::new(control);
    let out = Arc::new(Mutex::new(stream.try_clone()?));

    // the command's output, as it comes, from the terminal and from stdout when that is a pipe
    let (drained, read_done) = mpsc::channel::<()>();
    let outputs = readers.len();
    for mut reader in readers {
        let reader_out = out.clone();
        let drained = drained.clone();
        let kind = if reader.stderr {
            proto::STDERR
        } else {
            proto::OUTPUT
        };
        thread::spawn(move || {
            let mut buf = [0u8; 16 * 1024];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        if proto::write_frame(&mut *reader_out.lock().unwrap(), kind, &buf[..n])
                            .is_err()
                        {
                            break;
                        }
                    }
                }
            }
            let _ = drained.send(());
        });
    }

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
        // a background process still holding the terminal would keep it open forever: the wait for it is bounded,
        // and then the readers stop, so the terminal closes under it as a closed window's does
        let until = std::time::Instant::now() + Duration::from_secs(2);
        for _ in 0..outputs {
            let left = until.saturating_duration_since(std::time::Instant::now());
            if read_done.recv_timeout(left).is_err() {
                break;
            }
        }
        waiter_control.stop();
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
            Ok(Some((proto::EOF, _))) => writer.close(),
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
    log(&format!("{} exited {code}", program(&spawn.argv)));
    Ok(())
}

/// what it runs is run as it is: a server started elevated, or as root, would hand that to every command, and one
/// started outside the desktop session (a windows ssh login is session 0) would hand them none of the session
#[cfg(unix)]
fn session_check() -> io::Result<()> {
    if unsafe { libc::geteuid() } == 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "delishd runs as root: start it as the user, in the desktop session",
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn session_check() -> io::Result<()> {
    use windows_sys::Win32::Foundation::{CloseHandle, HANDLE};
    use windows_sys::Win32::Security::{
        GetTokenInformation, TOKEN_ELEVATION, TOKEN_QUERY, TokenElevation,
    };
    use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, GetCurrentProcessId, OpenProcessToken,
    };

    let mut session = 0u32;
    if unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) } == 0 {
        return Err(io::Error::last_os_error());
    }
    if session == 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "delishd is in session 0, not a desktop session: start it from the desktop, as a scheduled task that runs only when the user is logged on",
        ));
    }
    let mut token: HANDLE = std::ptr::null_mut();
    if unsafe { OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token) } == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut elevation = TOKEN_ELEVATION { TokenIsElevated: 0 };
    let mut len = 0u32;
    let ok = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            (&mut elevation as *mut TOKEN_ELEVATION).cast(),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut len,
        )
    };
    unsafe { CloseHandle(token) };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    if elevation.TokenIsElevated != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "delishd is elevated: start it with the user's own rights, not as administrator",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_log_names_a_program_and_none_of_its_arguments() {
        let argv = ["/bin/zsh", "-l", "-c", "curl -H 'token: secret'"].map(String::from);
        assert_eq!(program(&argv), "zsh");
        assert_eq!(program(&[]), "");
    }

    #[test]
    fn a_log_past_its_limit_starts_again_with_the_last_one_kept() {
        let dir = std::env::temp_dir().join(format!("delish-log-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("delishd.log");
        std::fs::write(&path, "x".repeat(11)).unwrap();
        rotate(&path, 20);
        assert!(path.exists(), "a log under its limit was rotated");
        rotate(&path, 10);
        assert!(!path.exists());
        assert_eq!(
            std::fs::read_to_string(dir.join("delishd.log.1"))
                .unwrap()
                .len(),
            11
        );
        let _ = std::fs::remove_dir_all(&dir);
    }
}
