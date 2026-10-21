//! The wire format between `deskpty` and `deskptyd`: frames of a kind byte, a little-endian u32 length, then the
//! payload. One connection runs one command.

use std::io::{self, Read, Write};

/// client → server: the command to run, in the first frame
pub const SPAWN: u8 = 1;
/// client → server: bytes for the command's terminal
pub const INPUT: u8 = 2;
/// client → server: the terminal's new size
pub const RESIZE: u8 = 3;
/// server → client: bytes the command's terminal wrote
pub const OUTPUT: u8 = 10;
/// server → client: the command's exit code, last
pub const EXIT: u8 = 11;
/// server → client: why the command could not be run, last
pub const ERROR: u8 = 12;

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
    pub cwd: String,
    pub argv: Vec<String>,
    pub env: Vec<(String, String)>,
}

impl Spawn {
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&self.size.encode());
        b.push(self.login as u8);
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

    pub fn decode(b: &[u8]) -> io::Result<Self> {
        let size = Size::decode(b)?;
        let login = *b.get(4).ok_or_else(|| bad("short frame"))? != 0;
        let mut at = 5;
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
            cwd: "/home/x".into(),
            argv: vec!["nu".into(), "-c".into(), "print ünï".into()],
            env: vec![("TERM".into(), "xterm-256color".into())],
        };
        assert_eq!(Spawn::decode(&s.encode()).unwrap(), s);
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
            cwd: String::new(),
            argv,
            env: vec![],
        };
        assert!(Spawn::decode(&shell(vec![]).encode()).is_ok());
        assert!(Spawn::decode(&shell(vec!["wut p s".into()]).encode()).is_ok());
        assert!(Spawn::decode(&shell(vec!["a".into(), "b".into()]).encode()).is_err());
    }
}
