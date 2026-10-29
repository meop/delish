//! `delishd`: started inside the desktop session, it runs each command a client sends on a pty of its own, so the
//! command has the session's context — its env, its window server, its token — whoever asked for it.

use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use std::thread;

use crate::proto::{self, Hello, Spawn};
use crate::session::{Session, Sink};
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

pub fn serve() -> io::Result<()> {
    session_check()?;
    let listener = sock::listen()?;
    log(&format!(
        "listening on {} (pid {})",
        sock::path().display(),
        std::process::id()
    ));
    thread::spawn(listen_for_peers);
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
    let spawn = match proto::read_frame(&mut stream)? {
        Some((proto::SPAWN, payload)) => match Spawn::decode(&payload) {
            Ok(s) => s,
            Err(e) => {
                log(&format!("refused: {e}"));
                return proto::write_frame(&mut stream, proto::ERROR, e.to_string().as_bytes());
            }
        },
        Some((proto::REMOTE, payload)) => return crate::link::serve_client(stream, &payload),
        // `delish status` connects and goes: nothing to run, and nothing wrong
        None => return Ok(()),
        Some(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "first frame is not a spawn",
            ));
        }
    };
    let out = Arc::new(Mutex::new(stream.try_clone()?));
    let shutdown = stream.try_clone()?;
    // the command's end: what it wrote last goes out before its exit code, and the connection closes after
    let sink: Sink = Arc::new(move |kind, bytes| {
        proto::write_frame(&mut *out.lock().unwrap(), kind, bytes)?;
        if kind == proto::EXIT {
            let _ = shutdown.shutdown(std::net::Shutdown::Both);
        }
        Ok(())
    });
    let session = match Session::start(spawn, sink) {
        Ok(s) => s,
        Err(msg) => {
            log(&msg);
            return proto::write_frame(&mut stream, proto::ERROR, msg.as_bytes());
        }
    };
    log(&format!("running {}", session.program));

    // the client's keys and resizes. a client that goes away takes its command with it, as a closed terminal does
    while let Ok(Some((kind, payload))) = proto::read_frame(&mut stream) {
        session.apply(kind, &payload);
    }
    session.hang_up();
    let code = session.wait();
    log(&format!("{} exited {code}", session.program));
    Ok(())
}

/// another host's server: its key proved and authorized, then a link to serve
fn handle_peer(tcp: std::net::TcpStream, private: &crate::keys::Key) -> io::Result<()> {
    let peer = tcp.peer_addr().map(|a| a.to_string()).unwrap_or_default();
    let _ = tcp.set_read_timeout(Some(crate::hosted::DEAD_AFTER));
    let authorized = crate::keys::authorized();
    let (mut reader, mut writer, key) =
        crate::noise::respond(tcp, private, |k| authorized.iter().any(|p| &p.key == k))?;
    let name = authorized
        .iter()
        .find(|p| p.key == key)
        .map_or("?", |p| p.name.as_str())
        .to_string();
    proto::write_frame(&mut writer, proto::HELLO, &Hello::ours().encode())?;
    match proto::read_frame(&mut reader)? {
        Some((proto::LINK, _)) => {}
        // `delish ping` proves its key, hears the hello, and goes
        None => return Ok(()),
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "a peer's first frame is not a link",
            ));
        }
    }
    proto::write_frame(&mut writer, proto::LINK, &proto::PROTOCOL.to_le_bytes())?;
    log(&format!("a link from {name} ({peer})"));
    crate::hosted::serve(reader, Arc::new(Mutex::new(Box::new(writer))))
}

/// other hosts' servers, on config's `listen`: a server that cannot listen there still serves this machine
fn listen_for_peers() {
    let address = crate::keys::listen();
    let private = match crate::keys::mine() {
        Ok((private, _)) => private,
        Err(e) => return log(&format!("no key, so no links from other hosts: {e}")),
    };
    let listener = match std::net::TcpListener::bind(&address) {
        Ok(l) => l,
        Err(e) => return log(&format!("cannot listen on {address} for other hosts: {e}")),
    };
    let bound = listener.local_addr().map_or(address, |a| a.to_string());
    log(&format!("listening on {bound} for other hosts"));
    for conn in listener.incoming() {
        match conn {
            Ok(tcp) => {
                thread::spawn(move || {
                    if let Err(e) = handle_peer(tcp, &private) {
                        log(&format!("peer: {e}"));
                    }
                });
            }
            Err(e) => log(&format!("accept: {e}")),
        }
    }
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
        assert_eq!(crate::session::program(&argv), "zsh");
        assert_eq!(crate::session::program(&[]), "");
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
