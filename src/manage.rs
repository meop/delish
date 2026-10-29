//! `delish key`, `hosts`, `trust`, `forget`, `peers`, `authorize`, `revoke` and `ping`: what is in `~/.delish`, kept
//! by hand, or by whatever distributes it.

use std::io;
use std::net::{TcpStream, ToSocketAddrs};
use std::time::{Duration, Instant};

use crate::keys::{self, Host, Key, Peer};
use crate::proto::{self, Hello};

fn word(what: &str, s: &str) -> Result<String, String> {
    if s.is_empty() || s.chars().any(|c| c.is_whitespace() || c == '#') {
        Err(format!("{what} {s:?} cannot be empty or hold spaces or #"))
    } else {
        Ok(s.to_string())
    }
}

fn key(s: &str) -> Result<Key, String> {
    keys::parse_key(s)
        .ok_or_else(|| format!("{s:?} is not a delish key: `delish key` on its host prints one"))
}

/// the hosts with this one added, or put in place of the one by its name
pub fn trusted(
    mut hosts: Vec<Host>,
    name: &str,
    address: &str,
    k: &str,
) -> Result<Vec<Host>, String> {
    let host = Host {
        name: word("a name", name)?,
        address: word("an address", address)?,
        key: key(k)?,
    };
    match hosts.iter_mut().find(|h| h.name == host.name) {
        Some(h) => *h = host,
        None => hosts.push(host),
    }
    Ok(hosts)
}

pub fn forgotten(mut hosts: Vec<Host>, name: &str) -> Result<Vec<Host>, String> {
    let before = hosts.len();
    hosts.retain(|h| h.name != name);
    if hosts.len() == before {
        return Err(format!("no host is named {name}"));
    }
    Ok(hosts)
}

/// the peers with this key added under its name; a key is authorized once, under its latest name
pub fn authorized(mut peers: Vec<Peer>, k: &str, name: &str) -> Result<Vec<Peer>, String> {
    let peer = Peer {
        key: key(k)?,
        name: word("a name", name)?,
    };
    peers.retain(|p| p.key != peer.key);
    peers.push(peer);
    Ok(peers)
}

/// every key under that name, or the one key it names
pub fn revoked(mut peers: Vec<Peer>, which: &str) -> Result<Vec<Peer>, String> {
    let k = keys::parse_key(which);
    let before = peers.len();
    peers.retain(|p| p.name != which && Some(p.key) != k);
    if peers.len() == before {
        return Err(format!("no authorized key is named {which}"));
    }
    Ok(peers)
}

fn done(r: Result<io::Result<()>, String>) -> i32 {
    match r {
        Ok(Ok(())) => 0,
        Ok(Err(e)) => {
            eprintln!("delish: {e}");
            1
        }
        Err(e) => {
            eprintln!("delish: {e}");
            2
        }
    }
}

pub fn run(command: &str, args: &[String]) -> i32 {
    let a: Vec<&str> = args.iter().map(String::as_str).collect();
    match (command, a.as_slice()) {
        ("key", []) => match keys::mine() {
            Ok((_, public)) => {
                println!("{}", keys::encode(&public));
                0
            }
            Err(e) => {
                eprintln!("delish: {e}");
                1
            }
        },
        ("hosts", []) => {
            for h in keys::hosts() {
                println!("{} {} {}", h.name, h.address, keys::encode(&h.key));
            }
            0
        }
        ("peers", []) => {
            for p in keys::authorized() {
                println!("{} {}", keys::encode(&p.key), p.name);
            }
            0
        }
        ("trust", [name, address, k]) => {
            done(trusted(keys::hosts(), name, address, k).map(|h| keys::write_hosts(&h)))
        }
        ("forget", [name]) => done(forgotten(keys::hosts(), name).map(|h| keys::write_hosts(&h))),
        ("authorize", [k, name]) => {
            done(authorized(keys::authorized(), k, name).map(|p| keys::write_authorized(&p)))
        }
        ("revoke", [which]) => {
            done(revoked(keys::authorized(), which).map(|p| keys::write_authorized(&p)))
        }
        ("ping", [host]) => ping(host),
        _ => {
            eprintln!("{}", crate::USAGE);
            2
        }
    }
}

/// connect to a host's server, each side proving its key, and say what answered and how quickly
fn ping(name: &str) -> i32 {
    let Some(host) = crate::link::resolve(name) else {
        eprintln!("delish: {name} is not a known host");
        return 2;
    };
    match reach(&host) {
        Ok((hello, took)) => {
            println!(
                "{} answered: delishd {}, protocol {}, in {} ms",
                host.name,
                hello.version,
                hello.protocol,
                took.as_millis()
            );
            0
        }
        Err(e) => {
            eprintln!("delish: {}: {e}", host.name);
            1
        }
    }
}

fn reach(host: &Host) -> io::Result<(Hello, Duration)> {
    let address = keys::with_port(&host.address);
    let started = Instant::now();
    let a = address.to_socket_addrs()?.next().ok_or_else(|| {
        io::Error::new(io::ErrorKind::NotFound, format!("no address for {address}"))
    })?;
    let tcp = TcpStream::connect_timeout(&a, Duration::from_secs(5))?;
    tcp.set_read_timeout(Some(Duration::from_secs(10)))?;
    let (private, _) = keys::mine()?;
    let (mut reader, _writer, _) = crate::noise::initiate(tcp, &private, |k| *k == host.key)?;
    match proto::read_frame(&mut reader)? {
        Some((proto::HELLO, payload)) => Ok((Hello::decode(&payload)?, started.elapsed())),
        _ => Err(io::Error::other("its server did not say hello")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn some_key(n: u8) -> String {
        keys::encode(&[n; 32])
    }

    #[test]
    fn trusting_a_name_again_replaces_its_address_and_key() {
        let h = trusted(vec![], "glass", "glass.lan", &some_key(1)).unwrap();
        let h = trusted(h, "felt", "metal.lan:7392", &some_key(2)).unwrap();
        let h = trusted(h, "glass", "192.168.0.30", &some_key(3)).unwrap();
        assert_eq!(h.len(), 2);
        assert_eq!((h[0].address.as_str(), h[0].key), ("192.168.0.30", [3; 32]));
        assert_eq!(forgotten(h, "glass").unwrap().len(), 1);
    }

    #[test]
    fn what_cannot_be_written_back_is_refused() {
        assert!(trusted(vec![], "my host", "glass.lan", &some_key(1)).is_err());
        assert!(trusted(vec![], "glass", "glass.lan#x", &some_key(1)).is_err());
        assert!(trusted(vec![], "glass", "glass.lan", "not-a-key").is_err());
        assert!(forgotten(vec![], "glass").is_err());
        assert!(revoked(vec![], "glass").is_err());
    }

    #[test]
    fn a_key_is_authorized_once_and_revoked_by_name_or_key() {
        let p = authorized(vec![], &some_key(1), "stone").unwrap();
        let p = authorized(p, &some_key(1), "stone-new").unwrap();
        let p = authorized(p, &some_key(2), "glass").unwrap();
        assert_eq!(p.len(), 2);
        assert_eq!(p[0].name, "stone-new");
        let p = revoked(p, &some_key(2)).unwrap();
        assert_eq!(revoked(p, "stone-new").unwrap(), vec![]);
    }
}
