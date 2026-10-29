//! Run a command inside the user's desktop session from somewhere that is not, like an ssh login, with a real
//! terminal streamed between the two. `delishd` runs in the session; `delish` is the client. Servers on different
//! hosts link to each other directly, each proving its key.

pub mod client;
pub mod cmdline;
pub mod hosted;
pub mod keys;
pub mod link;
pub mod manage;
pub mod mux;
pub mod noise;
pub mod proto;
pub mod pty;
pub mod server;
pub mod session;
pub mod shell;
pub mod sock;
pub mod term;

pub const USAGE: &str = "usage: delish <host> [command]
       delish run [--] <command> [args...]
       delish shell [-c <command>]
       delish status
       delish --version
       delish key
       delish hosts
       delish trust <name> <address[:port]> <key>
       delish forget <name>
       delish peers
       delish authorize <key> <name>
       delish revoke <name or key>
       delish ping <host>";
