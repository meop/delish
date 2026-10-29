//! The wire format between `delish` and `delishd`: frames of a kind byte, a little-endian u32 length, then the
//! payload. One connection runs one command. The server's first frame is a HELLO and the client's a SPAWN, each
//! starting with the protocol they speak, so a client and a server that differ say so rather than misread each other.
//! READY and ERROR from `delish proxy` come before any of it, and keep their shape in every protocol.

use std::io::{self, Read, Write};

/// client → server: the command to run, in the first frame
pub const SPAWN: u8 = 1;
/// client → server: bytes for the command's terminal
pub const INPUT: u8 = 2;
/// client → server: the terminal's new size
pub const RESIZE: u8 = 3;
/// client → server: the client's stdin ended; sent only when it is not a terminal
pub const EOF: u8 = 4;
/// server → client: bytes the command's terminal wrote
pub const OUTPUT: u8 = 10;
/// server → client: the command's exit code, last
pub const EXIT: u8 = 11;
/// server → client: why the command could not be run, last
pub const ERROR: u8 = 12;
/// proxy → client, before the server's frames: `delish proxy` on the host has reached its server
pub const READY: u8 = 13;
/// server → client, first: the protocol it speaks and its version
pub const HELLO: u8 = 14;

/// the frames and their payloads; a change to either is a new protocol
pub const PROTOCOL: u32 = 2;
pub const VERSION: &str = env!("CARGO_PKG_VERSION");

const MAX_FRAME: u32 = 16 << 20;

pub fn write_frame(w: &mut impl Write, kind: u8, payload: &[u8]) -> io::Result<()> {
    let mut head = [0u8; 5];
    head[0] = kind;
    head[1..].copy_from_slice(&(payload.len() as u32).to_le_bytes());
    w.write_all(&head)?;
    w.write_all(payload)?;
    w.flush()
}

/// the next frame, or None at a clean end of the stream
pub fn read_frame(r: &mut impl Read) -> io::Result<Option<(u8, Vec<u8>)>> {
    let mut head = [0u8; 5];
    match r.read_exact(&mut head) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes([head[1], head[2], head[3], head[4]]);
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame too large",
        ));
    }
    let mut payload = vec![0u8; len as usize];
    r.read_exact(&mut payload)?;
    Ok(Some((head[0], payload)))
}

/// what a server says first
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Hello {
    pub protocol: u32,
    pub version: String,
}

impl Hello {
    pub fn ours() -> Self {
        Hello {
            protocol: PROTOCOL,
            version: VERSION.into(),
        }
    }

    pub fn encode(&self) -> Vec<u8> {
        let mut b = self.protocol.to_le_bytes().to_vec();
        put_str(&mut b, &self.version);
        b
    }

    pub fn decode(b: &[u8]) -> io::Result<Self> {
        let mut at = 0;
        let protocol = take_u32(b, &mut at)?;
        let version = take_str(b, &mut at)?;
        Ok(Hello { protocol, version })
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Size {
    pub rows: u16,
    pub cols: u16,
}

impl Size {
    pub fn encode(self) -> [u8; 4] {
        let mut b = [0u8; 4];
        b[..2].copy_from_slice(&self.rows.to_le_bytes());
        b[2..].copy_from_slice(&self.cols.to_le_bytes());
        b
    }

    pub fn decode(b: &[u8]) -> io::Result<Self> {
        if b.len() < 4 {
            return Err(bad("short size"));
        }
        Ok(Size {
            rows: u16::from_le_bytes([b[0], b[1]]),
            cols: u16::from_le_bytes([b[2], b[3]]),
        })
    }
}

/// what to run: argv, where, extra env on top of the server's own, and the terminal's size. with `login` the server
/// runs the user's login shell instead, and argv is empty or the one command for it to run
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spawn {
    pub size: Size,
    pub login: bool,
    /// whether the client's stdin and stdout are terminals: one that is not is a pipe for the command too
    pub stdin_tty: bool,
    pub stdout_tty: bool,
    pub cwd: String,
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
}

impl Spawn {
    pub fn encode(&self) -> Vec<u8> {
        let mut b = PROTOCOL.to_le_bytes().to_vec();
        b.extend_from_slice(&self.size.encode());
        b.push(self.login as u8);
        b.push(self.stdin_tty as u8 | (self.stdout_tty as u8) << 1);
        put_str(&mut b, &self.cwd);
        b.extend_from_slice(&(self.argv.len() as u32).to_le_bytes());
        for a in &self.argv {
            put_str(&mut b, a);
        }
        b.extend_from_slice(&(self.env.len() as u32).to_le_bytes());
        for (k, v) in &self.env {
            put_str(&mut b, k);
            put_str(&mut b, v);
        }
        b
    }

    /// the protocol is read first, and a client that speaks another is told which this server speaks
    pub fn decode(b: &[u8]) -> io::Result<Self> {
        let mut at = 0;
        let protocol = take_u32(b, &mut at)?;
        if protocol != PROTOCOL {
            // a client older than the protocol number starts its spawn with the terminal's size instead
            let client = if protocol > 0xffff {
                "the client is older and says no protocol".to_string()
            } else {
                format!("the client protocol {protocol}")
            };
            return Err(bad(&format!(
                "this delishd is {VERSION} and speaks protocol {PROTOCOL}, {client}: update delish on both machines, then restart delishd"
            )));
        }
        let size = Size::decode(&b[at..])?;
        at += 4;
        let login = *b.get(at).ok_or_else(|| bad("short frame"))? != 0;
        at += 1;
        let ttys = *b.get(at).ok_or_else(|| bad("short frame"))?;
        at += 1;
        let cwd = take_str(b, &mut at)?;
        let argc = take_u32(b, &mut at)?;
        let mut argv = Vec::new();
        for _ in 0..argc {
            argv.push(take_str(b, &mut at)?);
        }
        let envc = take_u32(b, &mut at)?;
        let mut env = Vec::new();
        for _ in 0..envc {
            let k = take_str(b, &mut at)?;
            let v = take_str(b, &mut at)?;
            env.push((k, v));
        }
        if (login && argv.len() > 1) || (!login && argv.is_empty()) {
            return Err(bad("no command"));
        }
        Ok(Spawn {
            size,
            login,
            stdin_tty: ttys & 1 != 0,
            stdout_tty: ttys & 2 != 0,
            cwd,
            argv,
            env,
        })
    }
}

fn put_str(b: &mut Vec<u8>, s: &str) {
    b.extend_from_slice(&(s.len() as u32).to_le_bytes());
    b.extend_from_slice(s.as_bytes());
}

fn take_u32(b: &[u8], at: &mut usize) -> io::Result<u32> {
    let end = *at + 4;
    let s = b.get(*at..end).ok_or_else(|| bad("short frame"))?;
    *at = end;
    Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn take_str(b: &[u8], at: &mut usize) -> io::Result<String> {
    let len = take_u32(b, at)? as usize;
    let end = *at + len;
    let s = b.get(*at..end).ok_or_else(|| bad("short frame"))?;
    *at = end;
    String::from_utf8(s.to_vec()).map_err(|_| bad("not utf-8"))
}

fn bad(msg: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_round_trips() {
        let s = Spawn {
            size: Size {
                rows: 40,
                cols: 120,
            },
            login: false,
            stdin_tty: true,
            stdout_tty: true,
            cwd: "/home/x".into(),
            argv: vec!["nu".into(), "-c".into(), "print ünï".into()],
            env: vec![("TERM".into(), "xterm-256color".into())],
        };
        assert_eq!(Spawn::decode(&s.encode()).unwrap(), s);
    }

    #[test]
    fn whether_stdin_and_stdout_are_terminals_round_trips() {
        for (stdin_tty, stdout_tty) in [(true, true), (false, true), (true, false), (false, false)]
        {
            let s = Spawn {
                size: Size { rows: 1, cols: 1 },
                login: true,
                stdin_tty,
                stdout_tty,
                cwd: String::new(),
                argv: vec![],
                env: vec![],
            };
            assert_eq!(Spawn::decode(&s.encode()).unwrap(), s);
        }
    }

    #[test]
    fn hello_round_trips() {
        assert_eq!(
            Hello::decode(&Hello::ours().encode()).unwrap(),
            Hello::ours()
        );
    }

    #[test]
    fn a_spawn_in_another_protocol_is_refused_with_this_one() {
        let s = Spawn {
            size: Size { rows: 1, cols: 1 },
            login: true,
            stdin_tty: true,
            stdout_tty: true,
            cwd: String::new(),
            argv: vec![],
            env: vec![],
        };
        let mut b = s.encode();
        b[..4].copy_from_slice(&(PROTOCOL + 1).to_le_bytes());
        let e = Spawn::decode(&b).unwrap_err().to_string();
        assert!(e.contains(&format!("speaks protocol {PROTOCOL}")), "{e}");
    }

    #[test]
    fn frames_round_trip_and_end_cleanly() {
        let mut buf = Vec::new();
        write_frame(&mut buf, OUTPUT, b"hi").unwrap();
        write_frame(&mut buf, EXIT, &3i32.to_le_bytes()).unwrap();
        let mut r = &buf[..];
        assert_eq!(read_frame(&mut r).unwrap(), Some((OUTPUT, b"hi".to_vec())));
        assert_eq!(
            read_frame(&mut r).unwrap(),
            Some((EXIT, 3i32.to_le_bytes().to_vec()))
        );
        assert_eq!(read_frame(&mut r).unwrap(), None);
    }

    #[test]
    fn a_spawn_without_a_command_is_refused() {
        let s = Spawn {
            size: Size { rows: 1, cols: 1 },
            login: false,
            stdin_tty: true,
            stdout_tty: true,
            cwd: String::new(),
            argv: vec![],
            env: vec![],
        };
        assert!(Spawn::decode(&s.encode()).is_err());
    }

    #[test]
    fn a_login_shell_takes_no_command_or_one() {
        let shell = |argv: Vec<String>| Spawn {
            size: Size { rows: 1, cols: 1 },
            login: true,
            stdin_tty: true,
            stdout_tty: true,
            cwd: String::new(),
            argv,
            env: vec![],
        };
        assert!(Spawn::decode(&shell(vec![]).encode()).is_ok());
        assert!(Spawn::decode(&shell(vec!["git status".into()]).encode()).is_ok());
        assert!(Spawn::decode(&shell(vec!["a".into(), "b".into()]).encode()).is_err());
    }
}
