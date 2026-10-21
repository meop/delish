//! Run a command inside the user's desktop session from somewhere that is not, like an ssh login, with a real
//! terminal streamed between the two. `deskptyd` runs in the session; `deskpty run` is the client.

pub mod client;
pub mod proto;
pub mod pty;
pub mod server;
pub mod sock;
pub mod term;
