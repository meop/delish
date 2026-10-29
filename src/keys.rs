//! What a server proves and trusts, in `~/.delish`, the same on every platform: `key`, this machine's private key,
//! made on first use and never sent anywhere; `authorized`, the keys that may run sessions as this user here; `hosts`,
//! where each other host is and the key it must prove; `config`, where this server listens. Keys are 32 bytes,
//! written in base64, as WireGuard writes them.

use std::io;
use std::path::PathBuf;

use crate::sock;

pub const DEFAULT_PORT: u16 = 7397;
pub const KEY_LEN: usize = 32;

pub type Key = [u8; KEY_LEN];

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn encode(bytes: &[u8]) -> String {
    let mut out = String::new();
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (b[0] as u32) << 16 | (b[1] as u32) << 8 | b[2] as u32;
        for i in 0..4 {
            if i <= chunk.len() {
                out.push(B64[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

pub fn decode(s: &str) -> Option<Vec<u8>> {
    let s = s.trim().trim_end_matches('=');
    let mut out = Vec::new();
    let (mut acc, mut bits) = (0u32, 0);
    for c in s.bytes() {
        let v = B64.iter().position(|&b| b == c)? as u32;
        acc = acc << 6 | v;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

/// a key as it is written: 44 base64 characters
pub fn parse_key(s: &str) -> Option<Key> {
    decode(s)?.try_into().ok()
}

fn path(name: &str) -> PathBuf {
    sock::dir().join(name)
}

fn private(file: &std::path::Path) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o600))?;
    }
    let _ = file;
    Ok(())
}

/// this machine's private key and its public half, made the first time it is asked for
pub fn mine() -> io::Result<(Key, Key)> {
    let file = path("key");
    let private_key = match std::fs::read_to_string(&file) {
        Ok(s) => parse_key(&s).ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                format!("{} is not a key", file.display()),
            )
        })?,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            let pair = crate::noise::generate()?;
            std::fs::create_dir_all(sock::dir())?;
            std::fs::write(&file, encode(&pair.0) + "\n")?;
            private(&file)?;
            pair.0
        }
        Err(e) => return Err(e),
    };
    Ok((private_key, crate::noise::public_of(&private_key)?))
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Host {
    pub name: String,
    pub address: String,
    pub key: Key,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Peer {
    pub key: Key,
    pub name: String,
}

/// a file of whitespace separated fields, one entry a line; `#` starts a comment
fn lines(name: &str) -> Vec<Vec<String>> {
    std::fs::read_to_string(path(name))
        .unwrap_or_default()
        .lines()
        .map(|l| l.split('#').next().unwrap_or(""))
        .map(|l| l.split_whitespace().map(String::from).collect::<Vec<_>>())
        .filter(|f| !f.is_empty())
        .collect()
}

fn write_lines(name: &str, rows: impl Iterator<Item = String>) -> io::Result<()> {
    std::fs::create_dir_all(sock::dir())?;
    let file = path(name);
    let tmp = file.with_extension("new");
    std::fs::write(&tmp, rows.map(|r| r + "\n").collect::<String>())?;
    std::fs::rename(&tmp, &file)
}

/// `<name> <address[:port]> <key>`
pub fn hosts() -> Vec<Host> {
    lines("hosts")
        .into_iter()
        .filter_map(|f| match f.as_slice() {
            [name, address, key] => Some(Host {
                name: name.clone(),
                address: address.clone(),
                key: parse_key(key)?,
            }),
            _ => None,
        })
        .collect()
}

pub fn write_hosts(hosts: &[Host]) -> io::Result<()> {
    write_lines(
        "hosts",
        hosts
            .iter()
            .map(|h| format!("{} {} {}", h.name, h.address, encode(&h.key))),
    )
}

/// `<key> <name>`
pub fn authorized() -> Vec<Peer> {
    lines("authorized")
        .into_iter()
        .filter_map(|f| match f.as_slice() {
            [key, name, ..] => Some(Peer {
                key: parse_key(key)?,
                name: name.clone(),
            }),
            _ => None,
        })
        .collect()
}

pub fn write_authorized(peers: &[Peer]) -> io::Result<()> {
    write_lines(
        "authorized",
        peers
            .iter()
            .map(|p| format!("{} {}", encode(&p.key), p.name)),
    )
}

/// where this server listens for other hosts: config's `listen`, or every address on the default port
pub fn listen() -> String {
    lines("config")
        .into_iter()
        .find_map(|f| match f.as_slice() {
            [k, v] if k == "listen" => Some(v.clone()),
            _ => None,
        })
        .unwrap_or_else(|| format!("0.0.0.0:{DEFAULT_PORT}"))
}

/// a host's address with its port, the default one when it names none
pub fn with_port(address: &str) -> String {
    let bare_v6 = address.matches(':').count() > 1 && !address.starts_with('[');
    if bare_v6
        || !address
            .rsplit_once(':')
            .is_some_and(|(_, p)| p.parse::<u16>().is_ok())
    {
        format!("{address}:{DEFAULT_PORT}")
    } else {
        address.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trips_as_wireguard_writes_a_key() {
        for n in 0..=33 {
            let bytes: Vec<u8> = (0..n).map(|i| (i * 37 + 11) as u8).collect();
            assert_eq!(decode(&encode(&bytes)).unwrap(), bytes);
        }
        assert_eq!(
            encode(&[0u8; 32]),
            "AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA="
        );
        assert_eq!(encode(b"delish"), "ZGVsaXNo");
        assert!(parse_key("not a key").is_none());
        assert!(parse_key("ZGVsaXNo").is_none(), "a key is 32 bytes");
    }

    #[test]
    fn an_address_gets_the_default_port_when_it_names_none() {
        assert_eq!(with_port("glass.lan"), "glass.lan:7397");
        assert_eq!(with_port("metal.lan:7392"), "metal.lan:7392");
        assert_eq!(with_port("[::1]:9"), "[::1]:9");
        assert_eq!(with_port("192.168.0.30"), "192.168.0.30:7397");
    }
}
