//! Where `delishd` listens: a unix socket in a directory only its user can open. The path is the same from the
//! desktop session and from an ssh login, since both are the user's home. Windows has unix sockets too (10 1803+), as
//! a file in the filesystem, so its access is the profile dir's: the user's own.

use std::io;
use std::path::PathBuf;

#[cfg(unix)]
pub use std::os::unix::net::{UnixListener as Listener, UnixStream as Stream};
#[cfg(windows)]
pub use uds_windows::{UnixListener as Listener, UnixStream as Stream};

pub fn home() -> PathBuf {
    let var = if cfg!(windows) { "USERPROFILE" } else { "HOME" };
    std::env::var_os(var)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

pub fn dir() -> PathBuf {
    home().join(".delish")
}

/// DELISH_SOCKET moves it, for a second server or a test
pub fn path() -> PathBuf {
    std::env::var_os("DELISH_SOCKET")
        .map(PathBuf::from)
        .unwrap_or_else(|| dir().join("delishd.sock"))
}

pub fn connect() -> io::Result<Stream> {
    Stream::connect(path())
}

/// a socket left by a server that is gone is taken over; one a live server answers on is not
pub fn listen() -> io::Result<Listener> {
    listen_at(&path(), &dir())
}

/// only delish's own directory is made its user's alone: one DELISH_SOCKET names is somebody's to keep as it is
#[cfg_attr(windows, allow(unused_variables))]
fn listen_at(path: &std::path::Path, own: &std::path::Path) -> io::Result<Listener> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        #[cfg(unix)]
        if parent == own {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    if Stream::connect(path).is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!("a delishd already listens on {}", path.display()),
        ));
    }
    let _ = std::fs::remove_file(path);
    let listener = Listener::bind(path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(listener)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn mode(p: &std::path::Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    #[test]
    fn only_delishs_own_directory_is_made_private() {
        let root = std::env::temp_dir().join(format!("delish-sock-{}", std::process::id()));
        let (own, shared) = (root.join(".delish"), root.join("shared"));
        for d in [&own, &shared] {
            std::fs::create_dir_all(d).unwrap();
            std::fs::set_permissions(d, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        drop(listen_at(&shared.join("x.sock"), &own).unwrap());
        assert_eq!(
            mode(&shared),
            0o755,
            "a directory DELISH_SOCKET named was changed"
        );
        drop(listen_at(&own.join("delishd.sock"), &own).unwrap());
        assert_eq!(mode(&own), 0o700);
        assert_eq!(mode(&own.join("delishd.sock")), 0o600);
        let _ = std::fs::remove_dir_all(&root);
    }
}
