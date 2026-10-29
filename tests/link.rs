#![cfg(unix)]

use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

/// two servers, as two hosts: `a` is this one, which links to `b` through a stand-in for ssh that counts its starts
struct Hosts {
    root: PathBuf,
    a: Child,
    b: Child,
}

impl Drop for Hosts {
    fn drop(&mut self) {
        for c in [&mut self.a, &mut self.b] {
            let _ = c.kill();
            let _ = c.wait();
        }
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn wait_for(what: impl Fn() -> bool, secs: u64) -> bool {
    let end = Instant::now() + Duration::from_secs(secs);
    while Instant::now() < end {
        if what() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    false
}

fn hosts(name: &str, env: &[(&str, &str)]) -> Hosts {
    let root = std::env::temp_dir().join(format!("delish-link-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    for d in ["a", "b"] {
        std::fs::create_dir_all(root.join(d)).unwrap();
    }
    let ssh = root.join("ssh");
    std::fs::write(
        &ssh,
        format!(
            "#!/bin/sh\n[ -e {block} ] && exit 255\necho $$ >> {launches}\nexec env DELISH_SOCKET={b} {delish} proxy\n",
            block = root.join("block").display(),
            launches = root.join("launches").display(),
            b = root.join("b").join("s.sock").display(),
            delish = env!("CARGO_BIN_EXE_delish"),
        ),
    )
    .unwrap();
    std::fs::set_permissions(&ssh, std::fs::Permissions::from_mode(0o755)).unwrap();
    let server = |d: &str| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_delishd"));
        cmd.env("HOME", root.join(d))
            .env("DELISH_SOCKET", root.join(d).join("s.sock"))
            .env("DELISH_SSH", &ssh)
            .envs(env.iter().copied());
        cmd.spawn().unwrap()
    };
    let (a, b) = (server("a"), server("b"));
    for d in ["a", "b"] {
        assert!(wait_for(|| root.join(d).join("s.sock").exists(), 5));
    }
    Hosts { root, a, b }
}

impl Hosts {
    fn client(&self, command: &str) -> Command {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_delish"));
        cmd.args(["b.example", command])
            .env("HOME", self.root.join("a"))
            .env("DELISH_SOCKET", self.root.join("a").join("s.sock"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        cmd
    }

    fn run(&self, command: &str) -> Output {
        self.client(command).output().unwrap()
    }

    fn launches(&self) -> Vec<i32> {
        std::fs::read_to_string(self.root.join("launches"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.trim().parse().ok())
            .collect()
    }
}

fn alive(pid: i32) -> bool {
    unsafe { libc::kill(pid, 0) == 0 }
}

#[test]
fn sessions_to_a_host_share_one_link() {
    let h = hosts("share", &[]);
    let busy: Vec<Child> = (0..3)
        .map(|_| h.client("sleep 2").spawn().unwrap())
        .collect();
    for n in 1..=3 {
        let out = h.run(&format!("echo $(({n} * 7)); exit {n}"));
        assert_eq!(out.status.code(), Some(n));
        assert_eq!(String::from_utf8_lossy(&out.stdout), format!("{}\n", n * 7));
    }
    for c in busy {
        assert_eq!(c.wait_with_output().unwrap().status.code(), Some(0));
    }
    assert_eq!(
        h.launches().len(),
        1,
        "sessions to one host opened more than one link"
    );
}

#[test]
fn a_link_that_drops_comes_back_and_its_session_misses_nothing() {
    let h = hosts("drop", &[]);
    let client = h
        .client("i=0; while [ $i -lt 40 ]; do i=$((i+1)); echo $i; sleep 0.05; done; exit 3")
        .spawn()
        .unwrap();
    assert!(wait_for(|| !h.launches().is_empty(), 5));
    std::thread::sleep(Duration::from_millis(600));
    unsafe { libc::kill(h.launches()[0], libc::SIGKILL) };
    let out = client.wait_with_output().unwrap();
    let want: String = (1..=40).map(|i| format!("{i}\n")).collect();
    assert_eq!(String::from_utf8_lossy(&out.stdout), want);
    assert_eq!(out.status.code(), Some(3));
    assert_eq!(h.launches().len(), 2, "the link did not come back once");
}

#[test]
fn a_client_that_goes_ends_its_session_on_the_host() {
    let h = hosts("gone", &[]);
    let pid_file = h.root.join("b").join("sleep.pid");
    let mut client = h
        .client(&format!("echo $$ > {}; exec sleep 300", pid_file.display()))
        .spawn()
        .unwrap();
    assert!(wait_for(
        || std::fs::read_to_string(&pid_file).is_ok_and(|s| !s.is_empty()),
        10
    ));
    let pid: i32 = std::fs::read_to_string(&pid_file)
        .unwrap()
        .trim()
        .parse()
        .unwrap();
    client.kill().unwrap();
    client.wait().unwrap();
    assert!(
        wait_for(|| !alive(pid), 10),
        "the host's command outlived its client"
    );
}

#[test]
fn a_link_down_past_its_grace_ends_its_sessions_with_why() {
    let h = hosts("grace", &[("DELISH_LINK_GRACE", "2")]);
    let client = h.client("sleep 60").spawn().unwrap();
    assert!(wait_for(|| !h.launches().is_empty(), 5));
    std::thread::sleep(Duration::from_millis(300));
    std::fs::write(h.root.join("block"), "").unwrap();
    unsafe { libc::kill(h.launches()[0], libc::SIGKILL) };
    let started = Instant::now();
    let out = client.wait_with_output().unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(20),
        "the session outwaited its grace"
    );
    assert_eq!(out.status.code(), Some(255));
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("lost"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn an_idle_link_closes_and_the_next_session_opens_another() {
    let h = hosts("idle", &[("DELISH_LINK_IDLE", "1")]);
    assert_eq!(h.run("true").status.code(), Some(0));
    let first = h.launches()[0];
    assert!(wait_for(|| !alive(first), 10), "an idle link stayed open");
    assert_eq!(h.run("true").status.code(), Some(0));
    assert_eq!(h.launches().len(), 2);
}

#[test]
fn output_past_what_a_session_may_keep_unacknowledged_arrives_whole() {
    let h = hosts("large", &[]);
    let file = h.root.join("b").join("blob");
    let made = Command::new("sh")
        .args([
            "-c",
            &format!("head -c 6291456 /dev/urandom > {}", file.display()),
        ])
        .status()
        .unwrap();
    assert!(made.success());
    let out = h.run(&format!("cat {}", file.display()));
    assert_eq!(out.status.code(), Some(0));
    assert_eq!(out.stdout.len(), 6291456);
    assert!(
        out.stdout == std::fs::read(&file).unwrap(),
        "the bytes that arrived are not the ones sent"
    );
}
