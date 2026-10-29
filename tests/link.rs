#![cfg(unix)]

use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::{Child, Command, Output, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// two servers, as two hosts: `a` is this one, which links to `b` over TCP through a relay of the test's own, which
/// counts the connections it carries and can cut them or refuse more
struct Hosts {
    root: PathBuf,
    a: Child,
    b: Child,
    relay: Arc<Relay>,
}

#[derive(Default)]
struct Relay {
    carried: Mutex<Vec<TcpStream>>,
    launches: AtomicUsize,
    blocked: AtomicBool,
}

impl Relay {
    fn start(to: SocketAddr) -> (Arc<Relay>, SocketAddr) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let at = l.local_addr().unwrap();
        let relay = Arc::new(Relay::default());
        let r = relay.clone();
        std::thread::spawn(move || {
            for c in l.incoming().flatten() {
                if r.blocked.load(Ordering::SeqCst) {
                    continue;
                }
                let Ok(b) = TcpStream::connect(to) else {
                    continue;
                };
                r.launches.fetch_add(1, Ordering::SeqCst);
                r.carried
                    .lock()
                    .unwrap()
                    .extend([c.try_clone().unwrap(), b.try_clone().unwrap()]);
                for (mut from, mut into) in
                    [(c.try_clone().unwrap(), b.try_clone().unwrap()), (b, c)]
                {
                    std::thread::spawn(move || {
                        let _ = std::io::copy(&mut from, &mut into);
                        let _ = into.shutdown(Shutdown::Both);
                    });
                }
            }
        });
        (relay, at)
    }

    /// every connection it carries, gone at once, as a dropped network would take them
    fn cut(&self) {
        for s in self.carried.lock().unwrap().drain(..) {
            let _ = s.shutdown(Shutdown::Both);
        }
    }

    fn launches(&self) -> usize {
        self.launches.load(Ordering::SeqCst)
    }
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

/// where a server says it listens for other hosts: on port 0, whichever the system gave it
fn listening(root: &std::path::Path, d: &str) -> Option<SocketAddr> {
    let log = std::fs::read_to_string(root.join(d).join(".delish").join("delishd.log")).ok()?;
    log.lines()
        .find_map(|l| {
            l.split("listening on ")
                .nth(1)?
                .strip_suffix(" for other hosts")
        })?
        .parse()
        .ok()
}

/// `delish` with `d`'s home
fn delish(root: &std::path::Path, d: &str, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_delish"))
        .args(args)
        .env("HOME", root.join(d))
        .env("DELISH_SOCKET", root.join(d).join("s.sock"))
        .output()
        .unwrap()
}

fn key_of(root: &std::path::Path, d: &str) -> String {
    let out = delish(root, d, &["key"]);
    assert!(out.status.success());
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

/// `a` trusts `b` at the relay, and `b` authorizes `a`, each with the other's key, as an operator would
fn hosts(name: &str, env: &[(&str, &str)]) -> Hosts {
    let root = std::env::temp_dir().join(format!("delish-link-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    for d in ["a", "b"] {
        std::fs::create_dir_all(root.join(d).join(".delish")).unwrap();
        std::fs::write(
            root.join(d).join(".delish").join("config"),
            "listen 127.0.0.1:0\n",
        )
        .unwrap();
    }
    let (a_key, b_key) = (key_of(&root, "a"), key_of(&root, "b"));
    assert!(
        delish(&root, "b", &["authorize", &a_key, "a.example"])
            .status
            .success()
    );
    let server = |d: &str| {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_delishd"));
        cmd.env("HOME", root.join(d))
            .env("DELISH_SOCKET", root.join(d).join("s.sock"))
            .envs(env.iter().copied());
        cmd.spawn().unwrap()
    };
    let (a, b) = (server("a"), server("b"));
    for d in ["a", "b"] {
        assert!(wait_for(|| root.join(d).join("s.sock").exists(), 5));
    }
    assert!(wait_for(|| listening(&root, "b").is_some(), 5));
    let (relay, relay_at) = Relay::start(listening(&root, "b").unwrap());
    assert!(
        delish(
            &root,
            "a",
            &["trust", "b.example", &relay_at.to_string(), &b_key]
        )
        .status
        .success()
    );
    Hosts { root, a, b, relay }
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

    fn launches(&self) -> usize {
        self.relay.launches()
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
        h.launches(),
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
    assert!(wait_for(|| h.launches() > 0, 5));
    std::thread::sleep(Duration::from_millis(600));
    h.relay.cut();
    let out = client.wait_with_output().unwrap();
    let want: String = (1..=40).map(|i| format!("{i}\n")).collect();
    assert_eq!(String::from_utf8_lossy(&out.stdout), want);
    assert_eq!(out.status.code(), Some(3));
    assert_eq!(h.launches(), 2, "the link did not come back once");
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
    assert!(wait_for(|| h.launches() > 0, 5));
    std::thread::sleep(Duration::from_millis(300));
    h.relay.blocked.store(true, Ordering::SeqCst);
    h.relay.cut();
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
    assert!(
        wait_for(
            || h.relay
                .carried
                .lock()
                .unwrap()
                .iter()
                .all(|s| s.peek(&mut [0]).is_ok_and(|n| n == 0)),
            10
        ),
        "an idle link stayed open"
    );
    assert_eq!(h.run("true").status.code(), Some(0));
    assert_eq!(h.launches(), 2);
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

#[test]
fn a_host_that_proves_another_key_is_refused_with_why() {
    let h = hosts("wrongkey", &[]);
    std::fs::create_dir_all(h.root.join("c")).unwrap();
    let wrong = key_of(&h.root, "c");
    let known = std::fs::read_to_string(h.root.join("a").join(".delish").join("hosts")).unwrap();
    let address = known.split_whitespace().nth(1).unwrap().to_string();
    assert!(
        delish(&h.root, "a", &["trust", "b.example", &address, &wrong])
            .status
            .success()
    );
    let out = h.run("true");
    assert_eq!(out.status.code(), Some(255));
    let said = String::from_utf8_lossy(&out.stderr);
    assert!(said.contains("not the one known for it"), "{said}");
    let log = std::fs::read_to_string(h.root.join("b").join(".delish").join("delishd.log"))
        .unwrap_or_default();
    assert!(!log.contains("a link from"), "{log}");
}

#[test]
fn a_client_its_host_does_not_authorize_is_refused() {
    let h = hosts("unauthorized", &[]);
    assert!(
        delish(&h.root, "b", &["revoke", "a.example"])
            .status
            .success()
    );
    let out = h.run("true");
    assert_eq!(out.status.code(), Some(255));
    assert!(wait_for(
        || std::fs::read_to_string(h.root.join("b").join(".delish").join("delishd.log"))
            .is_ok_and(|l| l.contains("not authorized")),
        10
    ));
}

#[test]
fn ping_reports_the_host_and_an_unknown_one_is_named() {
    let h = hosts("ping", &[]);
    let out = delish(&h.root, "a", &["ping", "b.example"]);
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let said = String::from_utf8_lossy(&out.stdout);
    assert!(said.starts_with("b.example answered: delishd "), "{said}");
    let out = h.run("true");
    assert_eq!(out.status.code(), Some(0));
    let out = delish(&h.root, "a", &["nowhere.example", "true"]);
    assert_eq!(out.status.code(), Some(255));
    assert!(String::from_utf8_lossy(&out.stderr).contains("not a known host"));
}
