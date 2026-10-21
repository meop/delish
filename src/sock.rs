//! Where `deskptyd` listens: a unix socket in a directory only its user can open. The path is the same from the
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
    home().join(".deskpty")
}

/// DESKPTY_SOCKET moves it, for a second server or a test
pub fn path() -> PathBuf {
    std::env::var_os("DESKPTY_SOCKET")
        .map(PathBuf::from)
        .unwrap_or_else(|| dir().join("deskptyd.sock"))
}

pub fn connect() -> io::Result<Stream> {
    Stream::connect(path())
}

/// a socket left by a server that is gone is taken over; one a live server answers on is not
pub fn listen() -> io::Result<Listener> {
    let path = path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(parent, std::fs::Permissions::from_mode(0o700))?;
        }
    }
    if Stream::connect(&path).is_ok() {
        return Err(io::Error::new(
            io::ErrorKind::AddrInUse,
            format!("a deskptyd already listens on {}", path.display()),
        ));
    }
    let _ = std::fs::remove_file(&path);
    let listener = Listener::bind(&path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(listener)
}
