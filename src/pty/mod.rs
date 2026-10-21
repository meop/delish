//! A command on a pseudo terminal: openpty on unix, the console's own ConPTY on windows. What the server needs of it
//! is a reader and a writer for the terminal, and a control to resize, wait on, kill and close it.

use std::io;

use crate::proto::Spawn;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
use unix as os;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
use windows as os;

pub use os::{Control, Reader, Writer};

pub fn spawn(s: &Spawn) -> io::Result<(Reader, Writer, Control)> {
    os::spawn(s)
}
