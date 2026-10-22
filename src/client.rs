//! `deskpty run`, `shell` and `<host>`: asks a `deskptyd` to run a command in its desktop session, and is that
//! command's terminal here — keys and resizes go there, its output comes back, and its exit code is this one's. The
//! server is this user's own, on its socket, or a host's, through ssh and `deskpty proxy` there.

use std::io::{self, Read, Write};
use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::proto::{self, Size, Spawn};
use crate::sock;
use crate::ssh;
use crate::term;

const FALLBACK: Size = Size { rows: 24, cols: 80 };

/// the subcommand the server is handed for `deskpty <host>`: ssh from where it runs, never handed on again
pub const HOP: &str = "hop";
/// `deskpty hop`'s first option: the remote command, as `deskpty <host>` found it
pub const REMOTE_COMMAND_OPTION: &str = "--remote-command";

/// how the frames ended: the command's code, or the connection's end before it
enum End {
    Exit(i32),
    Closed,
}

type Sink = Arc<Mutex<Box<dyn Write + Send>>>;

/// `login` runs the user's login shell, interactive or with the one command in argv
pub fn run(argv: Vec<String>, login: bool) -> i32 {
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
    let cwd = std::env::current_dir()
        .map(|d| d.to_string_lossy().into_owned())
        .unwrap_or_default();
    let ended = stream
        .try_clone()
        .and_then(|up| relay(stream, Box::new(up), spawn(argv, login, cwd), false, ""));
    match ended {
        Ok(End::Exit(code)) => code,
        Ok(End::Closed) => {
            eprintln!("deskpty: deskptyd closed the connection before the command exited");
            255
        }
        Err(e) => {
            eprintln!("deskpty: {e}");
            255
        }
    }
}

/// the host's login shell, run by the host's server: interactive, or running the command. the cwd here means nothing
/// there, so the command starts in the user's home, as an ssh login does. ssh always starts from the desktop session:
/// through this user's own server when one is running, wherever this was typed, so the hop has that session's ssh
/// agent, keys and PATH; without one, from here
pub fn remote(target: ssh::Target) -> i32 {
    if sock::connect().is_ok() {
        return from_desktop();
    }
    if !ssh_can_pipe() {
        eprintln!(
            "deskpty: deskpty <host> cannot run from a windows ssh login, where windows' ssh.exe does not read a pipe, and there is no deskptyd in this user's desktop session ({}) to run it from instead",
            sock::path().display()
        );
        return 255;
    }
    hop(target, remote_command())
}

/// what runs on the host: read where `deskpty <host>` was typed, and handed to the server with the rest
pub fn remote_command() -> String {
    std::env::var("DESKPTY_REMOTE_COMMAND").unwrap_or_else(|_| ssh::REMOTE_COMMAND.into())
}

/// ssh, from here: what the server runs for `remote`
pub fn hop(target: ssh::Target, remote: String) -> i32 {
    if !ssh_can_pipe() {
        eprintln!(
            "deskpty: windows' ssh.exe does not read a pipe in a windows ssh login (session 0)"
        );
        return 255;
    }
    let mut child = match Command::new("ssh")
        .args(ssh::argv(&target, &remote))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => {
            eprintln!("deskpty: could not run ssh: {e}");
            return 255;
        }
    };
    let (Some(up), Some(down)) = (child.stdin.take(), child.stdout.take()) else {
        return 255;
    };
    let argv = target.command.into_iter().collect();
    let who = format!("{}: ", target.host);
    let ended = relay(
        down,
        Box::new(up),
        spawn(argv, true, String::new()),
        true,
        &who,
    );
    // ssh ends once the host's command has, and has said itself why it failed, when it has
    let ssh_code = child.wait().ok().and_then(|s| s.code()).unwrap_or(255);
    match ended {
        Ok(End::Exit(code)) => code,
        Ok(End::Closed) if ssh_code != 0 => ssh_code,
        Ok(End::Closed) => {
            eprintln!(
                "deskpty: the connection to {} closed before the command exited",
                target.host
            );
            255
        }
        Err(e) => {
            eprintln!("deskpty: {e}");
            255
        }
    }
}

/// windows' ssh.exe never reads a pipe for its stdin in session 0, where a windows ssh login is (the same pipe works
/// from the desktop): through it, the spawn would never arrive and the client would wait forever
#[cfg(windows)]
fn ssh_can_pipe() -> bool {
    use windows_sys::Win32::System::RemoteDesktop::ProcessIdToSessionId;
    use windows_sys::Win32::System::Threading::GetCurrentProcessId;

    let mut session = 0u32;
    let known = unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut session) } != 0;
    !known || session != 0
}

#[cfg(unix)]
fn ssh_can_pipe() -> bool {
    true
}

/// this same command, run by this user's server as `deskpty hop <host>`: this terminal stays its terminal. the
/// server's environment is not this one, so the remote command goes along as an argument
fn from_desktop() -> i32 {
    let exe = match std::env::current_exe() {
        Ok(p) => p.to_string_lossy().into_owned(),
        Err(e) => {
            eprintln!("deskpty: {e}");
            return 255;
        }
    };
    run(
        [
            exe,
            HOP.into(),
            REMOTE_COMMAND_OPTION.into(),
            remote_command(),
        ]
        .into_iter()
        .chain(std::env::args().skip(1))
        .collect(),
        false,
    )
}

fn spawn(argv: Vec<String>, login: bool, cwd: String) -> Spawn {
    let env = ["TERM", "COLORTERM"]
        .iter()
        .filter_map(|k| std::env::var(k).ok().map(|v| (k.to_string(), v)))
        .collect();
    Spawn {
        size: term::size().unwrap_or(FALLBACK),
        login,
        cwd,
        argv,
        env,
    }
}

/// the spawn goes first. the terminal is taken over — raw, its keys read — only once the server is there: through
/// ssh, which asks for a password or a host key on this same terminal, that is when the proxy says READY. `who` names
/// the server's host in what it says
fn relay(
    mut down: impl Read,
    up: Box<dyn Write + Send>,
    spawn: Spawn,
    wait_ready: bool,
    who: &str,
) -> io::Result<End> {
    let size = spawn.size;
    let up: Sink = Arc::new(Mutex::new(up));
    proto::write_frame(&mut *up.lock().unwrap(), proto::SPAWN, &spawn.encode())?;

    let mut started = !wait_ready;
    let mut raw = if started { take_over(&up, size) } else { None };
    let mut stdout = io::stdout().lock();
    let end = loop {
        match proto::read_frame(&mut down)? {
            Some((proto::READY, _)) if !started => {
                started = true;
                raw = take_over(&up, size);
            }
            Some((proto::OUTPUT, bytes)) => {
                stdout.write_all(&bytes)?;
                stdout.flush()?;
            }
            Some((proto::EXIT, payload)) if payload.len() >= 4 => {
                break End::Exit(i32::from_le_bytes([
                    payload[0], payload[1], payload[2], payload[3],
                ]));
            }
            Some((proto::ERROR, msg)) => {
                drop(raw.take());
                eprintln!("deskpty: {who}{}", String::from_utf8_lossy(&msg));
                break End::Exit(255);
            }
            Some(_) => {}
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
