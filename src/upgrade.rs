//! a delishd that an upgrade left behind says so: it asks the program at the path it was started from (the shim on
//! PATH, a symlink, whatever the service manager runs) for its version, and logs once when that is another version.
//! it keeps running: the move is delish's sync, which lets the new version through the firewall and then restarts
//! delishd, the same on every platform

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const CHECK_EVERY: Duration = Duration::from_secs(300);
const VERSION_TIMEOUT: Duration = Duration::from_secs(10);

/// what moves a running delishd to the installed one, firewall rule and all
pub const MOVE_HINT: &str = "`wut p s delish` moves to it";

/// the path delishd was started from: argv[0] when it names a path, else where PATH finds it
pub fn start_path(arg0: &OsString, cwd: &Path, path_var: Option<OsString>) -> Option<PathBuf> {
    let p = Path::new(arg0);
    if p.components().count() > 1 || p.is_absolute() {
        return Some(cwd.join(p));
    }
    let exe = if cfg!(windows) && p.extension().is_none() {
        p.with_extension("exe")
    } else {
        p.to_path_buf()
    };
    std::env::split_paths(&path_var?)
        .map(|dir| dir.join(&exe))
        .find(|candidate| candidate.is_file())
}

/// the version a `--version` printed: the last word of its first line
pub fn parse_version(out: &str) -> Option<String> {
    out.lines()
        .next()?
        .split_whitespace()
        .last()
        .map(|v| v.trim_start_matches('v').to_string())
}

/// the version to say is installed: one other than ours and other than the one already said
pub fn newly_installed<'a>(
    ours: &str,
    found: Option<&'a str>,
    said: Option<&str>,
) -> Option<&'a str> {
    found.filter(|f| *f != ours && Some(*f) != said)
}

/// the version of the program at path, or none when it does not answer in time
pub fn version_at(path: &Path) -> Option<String> {
    let mut child = Command::new(path)
        .arg("--version")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let end = Instant::now() + VERSION_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(status)) if status.success() => break,
            Ok(Some(_)) | Err(_) => return None,
            Ok(None) if Instant::now() > end => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(50)),
        }
    }
    let mut out = String::new();
    std::io::Read::read_to_string(&mut child.stdout.take()?, &mut out).ok()?;
    parse_version(&out)
}

/// checks every CHECK_EVERY (DELISH_UPGRADE_CHECK seconds, for tests) and logs a newly installed version once
pub fn watch() {
    let arg0 = std::env::args_os().next().unwrap_or_default();
    let cwd = std::env::current_dir().unwrap_or_default();
    let Some(start) = start_path(&arg0, &cwd, std::env::var_os("PATH")) else {
        crate::server::log("upgrade: no path to check, started from neither a path nor PATH");
        return;
    };
    let every = std::env::var("DELISH_UPGRADE_CHECK")
        .ok()
        .and_then(|s| s.parse().ok())
        .map_or(CHECK_EVERY, Duration::from_secs);
    let ours = env!("CARGO_PKG_VERSION");
    let mut said: Option<String> = None;
    loop {
        std::thread::sleep(every);
        let found = version_at(&start);
        if let Some(v) = newly_installed(ours, found.as_deref(), said.as_deref()) {
            crate::server::log(&format!(
                "upgrade: delishd {v} is installed at {}, this is {ours}: {MOVE_HINT}",
                start.display()
            ));
            said = Some(v.to_string());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_start_path_is_argv0_when_it_names_one_and_else_found_on_path() {
        let dir = std::env::temp_dir().join(format!("delish-start-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let name = if cfg!(windows) {
            "delishd.exe"
        } else {
            "delishd"
        };
        std::fs::write(dir.join(name), b"").unwrap();
        let cwd = Path::new("/somewhere");

        assert_eq!(
            start_path(&OsString::from("/opt/bin/delishd"), cwd, None),
            Some(PathBuf::from("/opt/bin/delishd"))
        );
        assert_eq!(
            start_path(&OsString::from("bin/delishd"), cwd, None),
            Some(cwd.join("bin/delishd"))
        );
        let path = std::env::join_paths([Path::new("/no/such/dir"), &dir]).unwrap();
        assert_eq!(
            start_path(&OsString::from("delishd"), cwd, Some(path)),
            Some(dir.join(name))
        );
        assert_eq!(start_path(&OsString::from("delishd"), cwd, None), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_version_is_the_last_word_of_the_first_line() {
        assert_eq!(parse_version("0.6.2\n").as_deref(), Some("0.6.2"));
        assert_eq!(
            parse_version("delishd v0.7.0\nmore").as_deref(),
            Some("0.7.0")
        );
        assert_eq!(parse_version(""), None);
    }

    #[test]
    fn another_installed_version_is_said_once() {
        assert_eq!(newly_installed("0.7.1", Some("0.7.2"), None), Some("0.7.2"));
        assert_eq!(newly_installed("0.7.1", Some("0.7.2"), Some("0.7.2")), None);
        assert_eq!(
            newly_installed("0.7.1", Some("0.7.3"), Some("0.7.2")),
            Some("0.7.3")
        );
        assert_eq!(newly_installed("0.7.1", Some("0.7.1"), None), None);
        assert_eq!(newly_installed("0.7.1", None, None), None);
    }
}
