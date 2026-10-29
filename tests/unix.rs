#![cfg(unix)]

use std::io::Write;
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

fn run(s: &Server, input: Option<&[u8]>, argv: &[&str]) -> (Output, Duration) {
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
    let (out, took) = run(&s, Some(b"one\ntwo"), &["cat"]);
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
    let (out, _) = run(&s, None, &["sh", "-c", "printf 'x\\ny\\n\\377'; exit 9"]);
    assert_eq!(out.status.code(), Some(9));
    assert_eq!(out.stdout, b"x\ny\n\xff");
}

#[test]
fn a_background_job_holding_the_terminal_neither_keeps_the_session_nor_survives_it() {
    let s = server("background");
    let pid_file = s.home.join("bg.pid");
    let script = format!(
        "sleep 60 2>&1 & echo $! > {}; echo started",
        pid_file.display()
    );
    let (out, took) = run(&s, None, &["sh", "-c", &script]);
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
