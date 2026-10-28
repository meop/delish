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
