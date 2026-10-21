//! Run a command inside the user's desktop session from somewhere that is not, like an ssh login, with a real
//! terminal streamed between the two. `deskptyd` runs in the session; `deskpty` is the client, here or through ssh.

pub mod client;
pub mod cmdline;
pub mod proto;
pub mod proxy;
pub mod pty;
pub mod server;
pub mod shell;
pub mod sock;
pub mod ssh;
pub mod term;
