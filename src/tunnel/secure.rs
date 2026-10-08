// VENDORED from flux crates/flux-irontunnel/src/secure.rs @ 4fb1ad85.
// Do not edit here: change it in flux, then run scripts/vendor-irontunnel.sh.
//! IronTunnel v1 secure channel — the encrypted replacement for the v0.0.2 handshake.
//!
//! Noise `IK` (x25519, ChaCha20-Poly1305, BLAKE2s) through the `snow` crate, so no
//! cryptography is hand-rolled here. What the pattern buys:
//!
//! - The client knows the relay's static public key in advance (pinned, like an SSH
//!   `known_hosts` entry). A relay without the matching private key cannot finish the
//!   handshake, so a man in the middle learns nothing and the client fails closed.
//! - The client's static public key travels encrypted in the first message. The relay
//!   checks it against its allowlist BEFORE answering, so an unknown key gets silence,
//!   not a usable tunnel — the relay is never an open proxy.
//! - After two messages both sides hold fresh per-direction ChaCha20-Poly1305 keys,
//!   mixed from ephemeral keys (forward secrecy for everything after the handshake).
//!
//! Wire: every Noise message is a frame of `u16` big-endian length + bytes (Noise caps a
//! message at 65535). After the handshake each frame carries at most [`MAX_CHUNK`]
//! plaintext bytes plus a 16-byte tag. A frame with an EMPTY plaintext is the
//! end-of-stream marker (a TCP half-close carried through the tunnel).

use super::{IronTunnelError, Result};
use snow::{params::NoiseParams, Builder, StatelessTransportState};
use std::fmt;
use std::path::Path;
use std::sync::Arc;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt, ReadHalf, WriteHalf};
use zeroize::Zeroizing;

pub const NOISE_PARAMS: &str = "Noise_IK_25519_ChaChaPoly_BLAKE2s";
/// Bound into the handshake hash: a v1 peer can never be talked into a different protocol.
pub const PROLOGUE: &[u8] = b"irontunnel/1";
/// Largest plaintext in one transport frame.
pub const MAX_CHUNK: usize = 16 * 1024;
const MAX_NOISE: usize = 65535;
const TAG: usize = 16;

fn params() -> NoiseParams {
    NOISE_PARAMS.parse().expect("NOISE_PARAMS is a valid pattern")
}

fn crypto(msg: impl Into<String>) -> IronTunnelError {
    IronTunnelError::Crypto(msg.into())
}

/// An x25519 static public key — a relay's identity or an authorized client.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct PublicKey(pub [u8; 32]);

impl PublicKey {
    pub fn from_hex(s: &str) -> Result<Self> {
        let bytes = hex::decode(s.trim()).map_err(|e| IronTunnelError::Parse(format!("public key hex: {e}")))?;
        let arr: [u8; 32] = bytes
            .try_into()
            .map_err(|_| IronTunnelError::Parse("public key must be 32 bytes (64 hex chars)".into()))?;
        Ok(Self(arr))
    }

    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }
}

impl fmt::Display for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_hex())
    }
}

impl fmt::Debug for PublicKey {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "PublicKey({}…)", &self.to_hex()[..12])
    }
}

/// An x25519 static keypair. The private half is zeroed when dropped.
pub struct StaticKeypair {
    private: Zeroizing<[u8; 32]>,
    public: PublicKey,
}

impl StaticKeypair {
    pub fn generate() -> Result<Self> {
        let kp = Builder::new(params()).generate_keypair().map_err(|e| crypto(e.to_string()))?;
        let private = Zeroizing::new(<[u8; 32]>::try_from(&kp.private[..]).map_err(|_| crypto("keypair size"))?);
        let public = PublicKey(<[u8; 32]>::try_from(&kp.public[..]).map_err(|_| crypto("keypair size"))?);
        Ok(Self { private, public })
    }

    pub fn from_private_bytes(bytes: [u8; 32]) -> Self {
        let secret = x25519_dalek::StaticSecret::from(bytes);
        let public = PublicKey(x25519_dalek::PublicKey::from(&secret).to_bytes());
        Self { private: Zeroizing::new(bytes), public }
    }

    pub fn from_private_hex(s: &str) -> Result<Self> {
        let bytes = Zeroizing::new(hex::decode(s.trim()).map_err(|_| IronTunnelError::Parse("private key is not hex".into()))?);
        let arr: [u8; 32] = bytes[..]
            .try_into()
            .map_err(|_| IronTunnelError::Parse("private key must be 32 bytes (64 hex chars)".into()))?;
        Ok(Self::from_private_bytes(arr))
    }

    pub fn public(&self) -> &PublicKey {
        &self.public
    }

    /// Read a key file written by [`StaticKeypair::save`] (64 hex chars).
    pub fn load(path: &Path) -> Result<Self> {
        let text = Zeroizing::new(std::fs::read_to_string(path)?);
        Self::from_private_hex(&text)
    }

    /// Write the private key as hex. Refuses to overwrite; mode 0600 on Unix.
    pub fn save(&self, path: &Path) -> Result<()> {
        use std::io::Write;
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(0o600);
        }
        let mut f = opts.open(path)?;
        let hex = Zeroizing::new(hex::encode(&self.private[..]));
        f.write_all(hex.as_bytes())?;
        f.write_all(b"\n")?;
        Ok(())
    }
}

async fn write_frame<W: AsyncWrite + Unpin>(w: &mut W, data: &[u8]) -> Result<()> {
    let len = u16::try_from(data.len()).map_err(|_| crypto("noise message over 65535 bytes"))?;
    w.write_all(&len.to_be_bytes()).await?;
    w.write_all(data).await?;
    w.flush().await?;
    Ok(())
}

/// `Ok(None)` when the peer closed cleanly at a frame boundary.
async fn read_frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<Option<Vec<u8>>> {
    let mut len = [0u8; 2];
    match r.read_exact(&mut len).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let mut buf = vec![0u8; u16::from_be_bytes(len) as usize];
    r.read_exact(&mut buf).await?;
    Ok(Some(buf))
}

/// The receiving half of an established channel.
pub struct SecureReader<R> {
    inner: R,
    state: Arc<StatelessTransportState>,
    nonce: u64,
}

/// The sending half of an established channel.
pub struct SecureWriter<W> {
    inner: W,
    state: Arc<StatelessTransportState>,
    nonce: u64,
}

impl<R: AsyncRead + Unpin> SecureReader<R> {
    /// Next decrypted message. `Some(empty)` is the peer's end-of-stream marker;
    /// `None` means the underlying connection closed without one.
    pub async fn recv(&mut self) -> Result<Option<Vec<u8>>> {
        let Some(ct) = read_frame(&mut self.inner).await? else { return Ok(None) };
        if ct.len() < TAG {
            return Err(crypto("frame shorter than its authentication tag"));
        }
        let mut out = vec![0u8; ct.len()];
        let n = self
            .state
            .read_message(self.nonce, &ct, &mut out)
            .map_err(|_| crypto("frame failed authentication (tampered, replayed or reordered)"))?;
        self.nonce += 1;
        out.truncate(n);
        Ok(Some(out))
    }
}

impl<W: AsyncWrite + Unpin> SecureWriter<W> {
    /// Encrypt and send `data`, split into [`MAX_CHUNK`] frames. Empty input sends nothing.
    pub async fn send(&mut self, data: &[u8]) -> Result<()> {
        for chunk in data.chunks(MAX_CHUNK) {
            self.send_frame(chunk).await?;
        }
        Ok(())
    }

    /// Tell the peer this direction is finished (half-close).
    pub async fn send_eof(&mut self) -> Result<()> {
        self.send_frame(&[]).await
    }

    async fn send_frame(&mut self, chunk: &[u8]) -> Result<()> {
        let mut out = vec![0u8; chunk.len() + TAG];
        let n = self
            .state
            .write_message(self.nonce, chunk, &mut out)
            .map_err(|e| crypto(e.to_string()))?;
        self.nonce += 1;
        write_frame(&mut self.inner, &out[..n]).await
    }

    pub async fn shutdown(&mut self) -> Result<()> {
        self.inner.shutdown().await?;
        Ok(())
    }
}

pub type Channel<S> = (SecureReader<ReadHalf<S>>, SecureWriter<WriteHalf<S>>);

fn into_channel<S: AsyncRead + AsyncWrite>(
    hs: snow::HandshakeState,
    r: ReadHalf<S>,
    w: WriteHalf<S>,
) -> Result<Channel<S>> {
    let state = Arc::new(hs.into_stateless_transport_mode().map_err(|e| crypto(e.to_string()))?);
    Ok((
        SecureReader { inner: r, state: state.clone(), nonce: 0 },
        SecureWriter { inner: w, state, nonce: 0 },
    ))
}

/// Client side: prove we hold `local`, and require the relay to hold `relay_key`.
pub async fn client_handshake<S: AsyncRead + AsyncWrite>(
    stream: S,
    local: &StaticKeypair,
    relay_key: &PublicKey,
) -> Result<Channel<S>> {
    let mut hs = Builder::new(params())
        .local_private_key(&local.private[..])
        .remote_public_key(&relay_key.0)
        .prologue(PROLOGUE)
        .build_initiator()
        .map_err(|e| crypto(e.to_string()))?;
    let (mut r, mut w) = tokio::io::split(stream);
    let mut buf = vec![0u8; MAX_NOISE];
    let n = hs.write_message(&[], &mut buf).map_err(|e| crypto(e.to_string()))?;
    write_frame(&mut w, &buf[..n]).await?;
    let reply = read_frame(&mut r).await?.ok_or_else(|| {
        IronTunnelError::Auth(
            "relay closed the handshake: this client key is not authorized there, or the relay key is wrong".into(),
        )
    })?;
    hs.read_message(&reply, &mut buf)
        .map_err(|_| crypto("relay reply did not verify — wrong relay key or tampered message"))?;
    into_channel(hs, r, w)
}

/// Relay side: complete the handshake only for a client key `authorized` accepts.
/// An unauthorized or undecryptable first message gets no reply at all.
pub async fn server_handshake<S: AsyncRead + AsyncWrite>(
    stream: S,
    local: &StaticKeypair,
    authorized: impl Fn(&PublicKey) -> bool,
) -> Result<(PublicKey, Channel<S>)> {
    let mut hs = Builder::new(params())
        .local_private_key(&local.private[..])
        .prologue(PROLOGUE)
        .build_responder()
        .map_err(|e| crypto(e.to_string()))?;
    let (mut r, mut w) = tokio::io::split(stream);
    let first = read_frame(&mut r)
        .await?
        .ok_or_else(|| IronTunnelError::Network("client closed before the handshake".into()))?;
    let mut buf = vec![0u8; MAX_NOISE];
    hs.read_message(&first, &mut buf)
        .map_err(|_| crypto("client handshake did not verify (wrong relay key on the client side?)"))?;
    let client = hs
        .get_remote_static()
        .and_then(|k| <[u8; 32]>::try_from(k).ok())
        .map(PublicKey)
        .ok_or_else(|| crypto("client sent no static key"))?;
    if !authorized(&client) {
        return Err(IronTunnelError::Auth(format!("client key {client:?} is not authorized")));
    }
    let n = hs.write_message(&[], &mut buf).map_err(|e| crypto(e.to_string()))?;
    write_frame(&mut w, &buf[..n]).await?;
    Ok((client, into_channel(hs, r, w)?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    fn pair() -> (StaticKeypair, StaticKeypair) {
        (StaticKeypair::generate().unwrap(), StaticKeypair::generate().unwrap())
    }

    #[tokio::test]
    async fn round_trip_both_directions_and_eof() {
        let (relay, client) = pair();
        let (relay_pub, client_pub) = (*relay.public(), *client.public());
        let (a, b) = duplex(1 << 20);
        let srv = tokio::spawn(async move {
            let (who, (mut r, mut w)) = server_handshake(b, &relay, |k| *k == client_pub).await.unwrap();
            assert_eq!(who, client_pub);
            let got = r.recv().await.unwrap().unwrap();
            w.send(&got).await.unwrap();
            assert_eq!(r.recv().await.unwrap().unwrap(), Vec::<u8>::new(), "EOF marker");
            w.send_eof().await.unwrap();
        });
        let (mut r, mut w) = client_handshake(a, &client, &relay_pub).await.unwrap();
        w.send(b"hello through the tunnel").await.unwrap();
        assert_eq!(r.recv().await.unwrap().unwrap(), b"hello through the tunnel");
        w.send_eof().await.unwrap();
        assert!(r.recv().await.unwrap().unwrap().is_empty());
        srv.await.unwrap();
    }

    #[tokio::test]
    async fn large_payload_is_chunked_and_reassembled() {
        let (relay, client) = pair();
        let relay_pub = *relay.public();
        let (a, b) = duplex(1 << 20);
        let data: Vec<u8> = (0..200_000u32).map(|i| (i * 7 + 3) as u8).collect();
        let expect = data.clone();
        let srv = tokio::spawn(async move {
            let (_, (mut r, _w)) = server_handshake(b, &relay, |_| true).await.unwrap();
            let mut got = Vec::new();
            while got.len() < expect.len() {
                let m = r.recv().await.unwrap().unwrap();
                assert!(m.len() <= MAX_CHUNK);
                got.extend(m);
            }
            assert_eq!(got, expect);
        });
        let (_r, mut w) = client_handshake(a, &client, &relay_pub).await.unwrap();
        w.send(&data).await.unwrap();
        srv.await.unwrap();
    }

    #[tokio::test]
    async fn unauthorized_client_gets_no_tunnel() {
        let (relay, client) = pair();
        let relay_pub = *relay.public();
        let (a, b) = duplex(1 << 16);
        let srv = tokio::spawn(async move { server_handshake(b, &relay, |_| false).await.map(|_| ()) });
        let err = client_handshake(a, &client, &relay_pub).await.err().expect("must fail");
        assert!(matches!(err, IronTunnelError::Auth(_)), "{err}");
        assert!(matches!(srv.await.unwrap(), Err(IronTunnelError::Auth(_))));
    }

    #[tokio::test]
    async fn wrong_relay_key_fails_closed() {
        let (relay, client) = pair();
        let impostor = StaticKeypair::generate().unwrap();
        let (a, b) = duplex(1 << 16);
        // The client pins `impostor`'s key but talks to `relay`.
        let srv = tokio::spawn(async move { server_handshake(b, &relay, |_| true).await.map(|_| ()) });
        assert!(client_handshake(a, &client, impostor.public()).await.is_err());
        assert!(srv.await.unwrap().is_err(), "relay must not accept a message keyed for someone else");
    }

    #[tokio::test]
    async fn tampered_frame_is_rejected() {
        let (relay, client) = pair();
        let relay_pub = *relay.public();
        let (a, b) = duplex(1 << 16);
        let srv = tokio::spawn(async move { server_handshake(b, &relay, |_| true).await.unwrap().1 });
        let (_r, w) = client_handshake(a, &client, &relay_pub).await.unwrap();
        let (mut sr, _sw) = srv.await.unwrap();
        // Forge a frame on the raw stream: right length, garbage body.
        let SecureWriter { mut inner, .. } = w;
        write_frame(&mut inner, &[0x42; 40]).await.unwrap();
        let err = sr.recv().await.unwrap_err();
        assert!(err.to_string().contains("authentication"), "{err}");
    }

    #[tokio::test]
    async fn plaintext_never_appears_on_the_wire() {
        let (relay, client) = pair();
        let relay_pub = *relay.public();
        // client <-> tap <-> relay, the tap records client->relay bytes.
        let (a, tap_client_side) = duplex(1 << 16);
        let (tap_relay_side, b) = duplex(1 << 16);
        let tap = tokio::spawn(async move {
            let (mut cr, mut cw) = tokio::io::split(tap_client_side);
            let (mut rr, mut rw) = tokio::io::split(tap_relay_side);
            let up = async move {
                let mut seen = Vec::new();
                let mut buf = [0u8; 4096];
                loop {
                    let n = cr.read(&mut buf).await.unwrap();
                    if n == 0 { break; }
                    seen.extend_from_slice(&buf[..n]);
                    rw.write_all(&buf[..n]).await.unwrap();
                }
                seen
            };
            let down = async move { let _ = tokio::io::copy(&mut rr, &mut cw).await; };
            tokio::join!(up, down).0
        });
        let srv = tokio::spawn(async move {
            let (_, (mut r, _w)) = server_handshake(b, &relay, |_| true).await.unwrap();
            r.recv().await.unwrap().unwrap()
        });
        let secret = b"GET /announce?info_hash=VERY-SECRET-TORRENT HTTP/1.1";
        let (r, mut w) = client_handshake(a, &client, &relay_pub).await.unwrap();
        w.send(secret).await.unwrap();
        assert_eq!(srv.await.unwrap(), secret);
        drop((r, w));
        let wire = tap.await.unwrap();
        assert!(wire.len() > secret.len());
        assert!(!wire.windows(12).any(|win| win == b"VERY-SECRET-"), "plaintext leaked onto the wire");
    }

    #[test]
    fn key_hex_round_trip_and_derivation() {
        let kp = StaticKeypair::generate().unwrap();
        let again = StaticKeypair::from_private_hex(&hex::encode(&kp.private[..])).unwrap();
        assert_eq!(again.public(), kp.public(), "public key derived from the private key matches snow's");
        assert_eq!(PublicKey::from_hex(&kp.public().to_hex()).unwrap(), *kp.public());
        assert!(PublicKey::from_hex("abcd").is_err());
    }
}
