#![cfg(unix)]

use std::fs::File;
use std::io::Write;
use std::os::fd::FromRawFd;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

struct Server {
    child: Child,
    home: PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.home);
    }
}

fn server(name: &str) -> Server {
    let home = std::env::temp_dir().join(format!("delish-e2e-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    std::fs::create_dir_all(&home).unwrap();
    let child = Command::new(env!("CARGO_BIN_EXE_delishd"))
        .env("HOME", &home)
        .env("DELISH_SOCKET", home.join("s.sock"))
        .spawn()
        .unwrap();
    let end = Instant::now() + Duration::from_secs(5);
    while !home.join("s.sock").exists() && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(20));
    }
    Server { child, home }
}

/// a terminal, for a client stream that is one
fn terminal() -> (File, File) {
    let (mut m, mut s) = (-1, -1);
    let made = unsafe {
        libc::openpty(
            &mut m,
            &mut s,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
    };
    assert_eq!(made, 0);
    unsafe { (File::from_raw_fd(m), File::from_raw_fd(s)) }
}

fn run(s: &Server, input: Option<&[u8]>, stderr: Stdio, argv: &[&str]) -> (Output, Duration) {
    let started = Instant::now();
    let mut child = Command::new(env!("CARGO_BIN_EXE_delish"))
        .arg("run")
        .arg("--")
        .args(argv)
        .env("HOME", &s.home)
        .env("DELISH_SOCKET", s.home.join("s.sock"))
        .stdin(if input.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(stderr)
        .spawn()
        .unwrap();
    if let Some(bytes) = input {
        child.stdin.take().unwrap().write_all(bytes).unwrap();
    }
    let out = child.wait_with_output().unwrap();
    (out, started.elapsed())
}

#[test]
fn piped_stdin_reaches_its_end_and_is_not_echoed() {
    let s = server("stdin");
    let (out, took) = run(&s, Some(b"one\ntwo"), Stdio::piped(), &["cat"]);
    assert!(
        took < Duration::from_secs(10),
        "the command never saw the end of its input"
    );
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(out.stdout, b"one\ntwo");
}

#[test]
fn output_to_a_pipe_is_byte_for_byte_and_the_exit_code_comes_back() {
    let s = server("stdout");
    let (out, _) = run(
        &s,
        None,
        Stdio::piped(),
        &["sh", "-c", "printf 'x\\ny\\n\\377'; exit 9"],
    );
    assert_eq!(out.status.code(), Some(9));
    assert_eq!(out.stdout, b"x\ny\n\xff");
}

#[test]
fn stderr_comes_back_on_the_clients_stderr() {
    let s = server("stderr");
    let (out, _) = run(
        &s,
        None,
        Stdio::piped(),
        &["sh", "-c", "echo out; echo err >&2; exit 4"],
    );
    assert_eq!(out.status.code(), Some(4));
    assert_eq!(out.stdout, b"out\n");
    assert_eq!(out.stderr, b"err\n");
}

#[test]
fn a_client_with_no_terminal_gives_the_command_none() {
    let s = server("noterminal");
    let (out, _) = run(
        &s,
        None,
        Stdio::piped(),
        &[
            "sh",
            "-c",
            "[ -t 0 ] || [ -t 1 ] || [ -t 2 ] || echo none; tty",
        ],
    );
    assert_eq!(out.stdout, b"none\nnot a tty\n");
}

#[test]
fn a_stream_on_a_terminal_puts_the_command_on_one_and_the_rest_on_pipes() {
    let s = server("someterminal");
    let (_master, slave) = terminal();
    let (out, _) = run(
        &s,
        None,
        Stdio::from(slave),
        &[
            "sh",
            "-c",
            "[ -t 2 ] && [ ! -t 1 ] && echo stderr-on-a-terminal",
        ],
    );
    assert_eq!(out.stdout, b"stderr-on-a-terminal\n");
}

#[test]
fn a_background_job_holding_the_terminal_neither_keeps_the_session_nor_survives_it() {
    let s = server("background");
    let pid_file = s.home.join("bg.pid");
    let script = format!(
        "sleep 60 2>&1 & echo $! > {}; echo started",
        pid_file.display()
    );
    let (_master, slave) = terminal();
    let (out, took) = run(&s, None, Stdio::from(slave), &["sh", "-c", &script]);
    assert_eq!(out.stdout, b"started\n");
    assert!(
        took < Duration::from_secs(8),
        "the session waited {took:?} on a background job"
    );
    let pid: i32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    let end = Instant::now() + Duration::from_secs(5);
    while unsafe { libc::kill(pid, 0) } == 0 && Instant::now() < end {
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        unsafe { libc::kill(pid, 0) } != 0,
        "the background job outlived its terminal"
    );
}

#[test]
fn with_stdout_redirected_the_terminals_bytes_go_to_the_clients_stderr() {
    let s = server("redirected");
    let (mut master, slave) = terminal();
    // read while the client runs: macOS drops what a pty holds once its last user closes it
    let seen = std::thread::spawn(move || {
        use std::io::Read;
        let mut seen = Vec::new();
        let mut buf = [0u8; 64];
        while let Ok(n) = master.read(&mut buf) {
            if n == 0 {
                break;
            }
            seen.extend_from_slice(&buf[..n]);
            if String::from_utf8_lossy(&seen).contains("err") {
                break;
            }
        }
        seen
    });
    let (out, _) = run(
        &s,
        None,
        Stdio::from(slave),
        &["sh", "-c", "echo out; echo err >&2"],
    );
    assert_eq!(out.stdout, b"out\n");
    assert!(
        String::from_utf8_lossy(&seen.join().unwrap()).contains("err"),
        "stderr did not reach the client's terminal"
    );
}
