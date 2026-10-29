//! A connection between two servers, over TCP: Noise's XX handshake, in which each side proves its key to the other,
//! then every message encrypted, as WireGuard's are, with x25519, ChaCha20-Poly1305 and BLAKE2s. What either side
//! does with the key it was shown — a host it knows, a peer it authorized — is the caller's.

use std::io::{self, Read, Write};
use std::net::TcpStream;
use std::sync::{Arc, Mutex};

use crate::keys::Key;

const PATTERN: &str = "Noise_XX_25519_ChaChaPoly_BLAKE2s";
/// what both sides mix into the handshake: a peer speaking another protocol fails it rather than misreading
fn prologue() -> Vec<u8> {
    let mut p = b"delish".to_vec();
    p.extend_from_slice(&crate::proto::PROTOCOL.to_le_bytes());
    p
}
const MAX_MESSAGE: usize = 65535;
const TAG: usize = 16;

fn bad(e: impl std::fmt::Display) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, e.to_string())
}

fn builder() -> snow::Builder<'static> {
    snow::Builder::new(PATTERN.parse().expect("a valid noise pattern"))
}

/// a new private key and its public half
pub fn generate() -> io::Result<(Key, Key)> {
    let pair = builder().generate_keypair().map_err(bad)?;
    let take =
        |v: &[u8]| -> io::Result<Key> { v.try_into().map_err(|_| bad("a key is not 32 bytes")) };
    Ok((take(&pair.private)?, take(&pair.public)?))
}

/// the public half of a private key
pub fn public_of(private: &Key) -> io::Result<Key> {
    // an x25519 public key is the private one times the base point
    let point = curve25519_dalek::montgomery::MontgomeryPoint::mul_base_clamped(*private);
    Ok(point.to_bytes())
}

fn send(s: &mut TcpStream, msg: &[u8]) -> io::Result<()> {
    s.write_all(&(msg.len() as u16).to_le_bytes())?;
    s.write_all(msg)?;
    s.flush()
}

fn recv(s: &mut TcpStream) -> io::Result<Vec<u8>> {
    let mut len = [0u8; 2];
    s.read_exact(&mut len)?;
    let mut msg = vec![0u8; u16::from_le_bytes(len) as usize];
    s.read_exact(&mut msg)?;
    Ok(msg)
}

fn remote_key(h: &snow::HandshakeState) -> io::Result<Key> {
    h.get_remote_static()
        .and_then(|k| k.try_into().ok())
        .ok_or_else(|| bad("the other side showed no key"))
}

/// the side that connects: it learns the other's key before showing its own, and goes on only if `trusted` takes it
pub fn initiate(
    mut s: TcpStream,
    private: &Key,
    trusted: impl Fn(&Key) -> bool,
) -> io::Result<(Reader, Writer, Key)> {
    let mut h = builder()
        .local_private_key(private)
        .map_err(bad)?
        .prologue(&prologue())
        .map_err(bad)?
        .build_initiator()
        .map_err(bad)?;
    let mut buf = vec![0u8; MAX_MESSAGE];
    let n = h.write_message(&[], &mut buf).map_err(bad)?;
    send(&mut s, &buf[..n])?;
    let msg = recv(&mut s)?;
    h.read_message(&msg, &mut buf)
        .map_err(|_| bad("the handshake failed: another protocol, or not delish"))?;
    let theirs = remote_key(&h)?;
    if !trusted(&theirs) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "it proved the key {}, which is not the one known for it",
                crate::keys::encode(&theirs)
            ),
        ));
    }
    let n = h.write_message(&[], &mut buf).map_err(bad)?;
    send(&mut s, &buf[..n])?;
    split(s, h.into_transport_mode().map_err(bad)?, theirs)
}

/// the side that listens: it goes on only if `authorized` takes the key the other side proved
pub fn respond(
    mut s: TcpStream,
    private: &Key,
    authorized: impl Fn(&Key) -> bool,
) -> io::Result<(Reader, Writer, Key)> {
    let mut h = builder()
        .local_private_key(private)
        .map_err(bad)?
        .prologue(&prologue())
        .map_err(bad)?
        .build_responder()
        .map_err(bad)?;
    let mut buf = vec![0u8; MAX_MESSAGE];
    let msg = recv(&mut s)?;
    h.read_message(&msg, &mut buf).map_err(bad)?;
    let n = h.write_message(&[], &mut buf).map_err(bad)?;
    send(&mut s, &buf[..n])?;
    let msg = recv(&mut s)?;
    h.read_message(&msg, &mut buf).map_err(bad)?;
    let theirs = remote_key(&h)?;
    if !authorized(&theirs) {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            format!(
                "a connection proved the key {}, which is not authorized",
                crate::keys::encode(&theirs)
            ),
        ));
    }
    split(s, h.into_transport_mode().map_err(bad)?, theirs)
}

fn split(s: TcpStream, t: snow::TransportState, theirs: Key) -> io::Result<(Reader, Writer, Key)> {
    let t = Arc::new(Mutex::new(t));
    let reader = Reader {
        stream: s.try_clone()?,
        state: t.clone(),
        plain: Vec::new(),
        at: 0,
    };
    Ok((
        reader,
        Writer {
            stream: s,
            state: t,
        },
        theirs,
    ))
}

/// the decrypting half
pub struct Reader {
    stream: TcpStream,
    state: Arc<Mutex<snow::TransportState>>,
    plain: Vec<u8>,
    at: usize,
}

impl Read for Reader {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        while self.at == self.plain.len() {
            let mut len = [0u8; 2];
            match self.stream.read_exact(&mut len) {
                Ok(()) => {}
                Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(0),
                Err(e) => return Err(e),
            }
            let mut msg = vec![0u8; u16::from_le_bytes(len) as usize];
            self.stream.read_exact(&mut msg)?;
            let mut plain = vec![0u8; msg.len()];
            let n = self
                .state
                .lock()
                .unwrap()
                .read_message(&msg, &mut plain)
                .map_err(bad)?;
            plain.truncate(n);
            self.plain = plain;
            self.at = 0;
        }
        let n = buf.len().min(self.plain.len() - self.at);
        buf[..n].copy_from_slice(&self.plain[self.at..self.at + n]);
        self.at += n;
        Ok(n)
    }
}

/// the encrypting half
pub struct Writer {
    stream: TcpStream,
    state: Arc<Mutex<snow::TransportState>>,
}

impl Writer {
    /// both halves, at once: what a watchdog does to a link it takes for dead
    pub fn shutdown(&self) {
        let _ = self.stream.shutdown(std::net::Shutdown::Both);
    }
}

impl Write for Writer {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let mut out = vec![0u8; MAX_MESSAGE];
        let mut sent = 0;
        for chunk in buf.chunks(MAX_MESSAGE - TAG) {
            let n = self
                .state
                .lock()
                .unwrap()
                .write_message(chunk, &mut out)
                .map_err(bad)?;
            let mut framed = (n as u16).to_le_bytes().to_vec();
            framed.extend_from_slice(&out[..n]);
            self.stream.write_all(&framed)?;
            sent += chunk.len();
        }
        Ok(sent)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.stream.flush()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::TcpListener;

    fn pair() -> (Key, Key) {
        generate().unwrap()
    }

    type Side = io::Result<(Reader, Writer, Key)>;

    fn connect(
        client: (Key, Key),
        server: (Key, Key),
        client_trusts: Key,
        server_authorizes: Key,
    ) -> (Side, Side) {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = l.local_addr().unwrap();
        let served = std::thread::spawn(move || {
            let (s, _) = l.accept().unwrap();
            respond(s, &server.0, |k| *k == server_authorizes)
        });
        let s = TcpStream::connect(addr).unwrap();
        let ours = initiate(s, &client.0, |k| *k == client_trusts);
        (ours, served.join().unwrap())
    }

    #[test]
    fn the_public_half_is_what_generate_gave() {
        let (private, public) = pair();
        assert_eq!(public_of(&private).unwrap(), public);
    }

    #[test]
    fn each_side_proves_its_key_and_then_talks_encrypted() {
        let (c, s) = (pair(), pair());
        let (ours, theirs) = connect(c, s, s.1, c.1);
        let (mut cr, mut cw, server_key) = ours.unwrap();
        let (mut sr, mut sw, client_key) = theirs.unwrap();
        assert_eq!((server_key, client_key), (s.1, c.1));
        let big = vec![7u8; 200_000];
        cw.write_all(&big).unwrap();
        let mut got = vec![0u8; big.len()];
        sr.read_exact(&mut got).unwrap();
        assert_eq!(got, big);
        sw.write_all(b"back").unwrap();
        let mut back = [0u8; 4];
        cr.read_exact(&mut back).unwrap();
        assert_eq!(&back, b"back");
    }

    #[test]
    fn a_server_that_proves_another_key_is_refused() {
        let (c, s, other) = (pair(), pair(), pair());
        let (ours, _) = connect(c, s, other.1, c.1);
        assert_eq!(ours.err().unwrap().kind(), io::ErrorKind::PermissionDenied);
    }

    #[test]
    fn a_client_whose_key_is_not_authorized_is_refused() {
        let (c, s, other) = (pair(), pair(), pair());
        let (_, theirs) = connect(c, s, s.1, other.1);
        assert_eq!(
            theirs.err().unwrap().kind(),
            io::ErrorKind::PermissionDenied
        );
    }
}
