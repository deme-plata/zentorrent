// VENDORED from flux crates/flux-irontunnel/src/relay.rs @ 4fb1ad85.
// Do not edit here: change it in flux, then run scripts/vendor-irontunnel.sh.
//! The relay: the far end of the tunnel, where traffic leaves for the internet.
//!
//! Per TCP connection: Noise handshake (only allowlisted client keys get one) → one
//! encrypted control message → either PING/PONG (a liveness check) or CONNECT to a
//! target → then a byte pipe until both sides have closed.
//!
//! The egress guard matters as much as the crypto. Without it, any authorized client
//! could reach the relay host's own loopback services through the tunnel — on Epsilon
//! that would be the Quillon API on 127.0.0.1:8080 and SIGIL on :18181. Every resolved
//! address is checked BEFORE connecting, and the checked address is the one dialed, so
//! a DNS answer cannot point the relay back at itself (rebinding).

use super::secure::{server_handshake, PublicKey, SecureReader, SecureWriter, StaticKeypair, MAX_CHUNK};
use super::socks5::{reply, TargetAddr};
use super::{IronTunnelError, Result};
use std::collections::HashSet;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Semaphore;
use tokio::task::JoinHandle;

/// Control messages, the first plaintext inside a fresh channel.
pub const MSG_CONNECT: u8 = 0x01;
pub const MSG_CONNECT_REPLY: u8 = 0x02;
pub const MSG_PING: u8 = 0x03;
pub const MSG_PONG: u8 = 0x04;

/// Where the relay is willing to connect on a client's behalf.
#[derive(Debug, Clone)]
pub struct EgressPolicy {
    /// Allow loopback/private/link-local targets. Tests only.
    pub allow_private: bool,
    /// Ports never dialed (default: 25, so the relay cannot be used to send spam).
    pub deny_ports: Vec<u16>,
}

impl Default for EgressPolicy {
    fn default() -> Self {
        Self { allow_private: false, deny_ports: vec![25] }
    }
}

impl EgressPolicy {
    pub fn permits(&self, addr: SocketAddr) -> bool {
        if self.deny_ports.contains(&addr.port()) || addr.port() == 0 {
            return false;
        }
        let ip = addr.ip();
        if ip.is_unspecified() || ip.is_multicast() {
            return false;
        }
        self.allow_private || is_global(ip)
    }
}

/// Conservative "is this a public internet address".
pub fn is_global(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(a) => is_global_v4(a),
        IpAddr::V6(a) => is_global_v6(a),
    }
}

fn is_global_v4(a: Ipv4Addr) -> bool {
    let o = a.octets();
    !(a.is_loopback()
        || a.is_private()
        || a.is_link_local()
        || a.is_unspecified()
        || a.is_broadcast()
        || a.is_multicast()
        || a.is_documentation()
        || o[0] == 0
        || o[0] >= 240
        || (o[0] == 100 && (o[1] & 0xC0) == 64) // 100.64.0.0/10 carrier-grade NAT
        || (o[0] == 192 && o[1] == 0 && o[2] == 0) // 192.0.0.0/24 protocol assignments
        || (o[0] == 198 && (o[1] & 0xFE) == 18)) // 198.18.0.0/15 benchmarking
}

fn is_global_v6(a: Ipv6Addr) -> bool {
    if let Some(v4) = a.to_ipv4_mapped() {
        return is_global_v4(v4);
    }
    let s = a.segments();
    // NAT64 (64:ff9b::/96) and 6to4 (2002::/16) embed an IPv4 address: judge that.
    if s[0] == 0x64 && s[1] == 0xff9b && s[2..6] == [0, 0, 0, 0] {
        return is_global_v4(Ipv4Addr::new((s[6] >> 8) as u8, s[6] as u8, (s[7] >> 8) as u8, s[7] as u8));
    }
    if s[0] == 0x2002 {
        return is_global_v4(Ipv4Addr::new((s[1] >> 8) as u8, s[1] as u8, (s[2] >> 8) as u8, s[2] as u8));
    }
    !(a.is_loopback()
        || a.is_unspecified()
        || a.is_multicast()
        || (s[0] & 0xFE00) == 0xFC00 // fc00::/7 unique local
        || (s[0] & 0xFFC0) == 0xFE80 // fe80::/10 link local
        || (s[0] == 0x2001 && s[1] == 0x0DB8)) // documentation
}

pub struct RelayConfig {
    pub listen: SocketAddr,
    pub keypair: Arc<StaticKeypair>,
    pub authorized: Arc<HashSet<PublicKey>>,
    pub egress: EgressPolicy,
    pub handshake_timeout: Duration,
    pub connect_timeout: Duration,
    pub max_connections: usize,
}

impl RelayConfig {
    pub fn new(listen: SocketAddr, keypair: StaticKeypair, authorized: HashSet<PublicKey>) -> Self {
        Self {
            listen,
            keypair: Arc::new(keypair),
            authorized: Arc::new(authorized),
            egress: EgressPolicy::default(),
            handshake_timeout: Duration::from_secs(10),
            connect_timeout: Duration::from_secs(10),
            max_connections: 1024,
        }
    }
}

/// Parse an authorized-clients file: one hex public key per line, `#` comments.
pub fn parse_authorized(text: &str) -> Result<HashSet<PublicKey>> {
    text.lines()
        .map(|l| l.split('#').next().unwrap_or("").trim())
        .filter(|l| !l.is_empty())
        .map(PublicKey::from_hex)
        .collect()
}

#[derive(Debug, Default)]
pub struct RelayStats {
    pub handshakes_ok: AtomicU64,
    pub handshakes_refused: AtomicU64,
    pub connects_ok: AtomicU64,
    pub connects_denied: AtomicU64,
    pub connects_failed: AtomicU64,
    pub bytes_to_targets: AtomicU64,
    pub bytes_from_targets: AtomicU64,
}

pub struct RelayHandle {
    pub local_addr: SocketAddr,
    pub stats: Arc<RelayStats>,
    task: JoinHandle<()>,
}

impl RelayHandle {
    pub fn shutdown(self) {
        self.task.abort();
    }
}

impl Drop for RelayHandle {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Bind and serve in the background.
pub async fn start_relay(cfg: RelayConfig) -> Result<RelayHandle> {
    let listener = TcpListener::bind(cfg.listen).await?;
    let local_addr = listener.local_addr()?;
    let stats = Arc::new(RelayStats::default());
    let cfg = Arc::new(cfg);
    let limit = Arc::new(Semaphore::new(cfg.max_connections));
    let st = stats.clone();
    let task = tokio::spawn(async move {
        loop {
            let Ok((sock, peer)) = listener.accept().await else {
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            };
            let Ok(permit) = limit.clone().try_acquire_owned() else {
                tracing::warn!("relay at {} connections, dropping {peer}", cfg.max_connections);
                continue;
            };
            let (cfg, st) = (cfg.clone(), st.clone());
            tokio::spawn(async move {
                let _permit = permit;
                if let Err(e) = serve(sock, &cfg, &st).await {
                    // Debug level: a VPN relay should not keep a log of who went where.
                    tracing::debug!("relay connection from {peer} ended: {e}");
                }
            });
        }
    });
    Ok(RelayHandle { local_addr, stats, task })
}

async fn serve(sock: TcpStream, cfg: &RelayConfig, st: &RelayStats) -> Result<()> {
    sock.set_nodelay(true).ok();
    let hs = server_handshake(sock, &cfg.keypair, |k| cfg.authorized.contains(k));
    let (_client, (mut rd, mut wr)) = match tokio::time::timeout(cfg.handshake_timeout, hs).await {
        Ok(Ok(ok)) => ok,
        Ok(Err(e)) => {
            st.handshakes_refused.fetch_add(1, Ordering::Relaxed);
            return Err(e);
        }
        Err(_) => {
            st.handshakes_refused.fetch_add(1, Ordering::Relaxed);
            return Err(IronTunnelError::Network("handshake timed out".into()));
        }
    };
    st.handshakes_ok.fetch_add(1, Ordering::Relaxed);

    let first = tokio::time::timeout(cfg.handshake_timeout, rd.recv())
        .await
        .map_err(|_| IronTunnelError::Network("no request after handshake".into()))??
        .ok_or_else(|| IronTunnelError::Network("client left after handshake".into()))?;
    match first.split_first() {
        Some((&MSG_PING, _)) => wr.send(&[MSG_PONG]).await,
        Some((&MSG_CONNECT, rest)) => {
            let (target, _) = TargetAddr::decode(rest)?;
            match dial(&target, cfg).await {
                Ok(tcp) => {
                    st.connects_ok.fetch_add(1, Ordering::Relaxed);
                    wr.send(&[MSG_CONNECT_REPLY, reply::SUCCEEDED]).await?;
                    pipe(tcp, rd, wr, &st.bytes_from_targets, &st.bytes_to_targets).await
                }
                Err(code) => {
                    let counter = if code == reply::NOT_ALLOWED { &st.connects_denied } else { &st.connects_failed };
                    counter.fetch_add(1, Ordering::Relaxed);
                    wr.send(&[MSG_CONNECT_REPLY, code]).await
                }
            }
        }
        _ => Err(IronTunnelError::Parse("unknown tunnel request".into())),
    }
}

/// Resolve, filter through the egress policy, then dial — the filtered address is the
/// one connected to. Errors are SOCKS5 reply codes.
async fn dial(target: &TargetAddr, cfg: &RelayConfig) -> std::result::Result<TcpStream, u8> {
    let candidates: Vec<SocketAddr> = match target {
        TargetAddr::Ip(a) => vec![*a],
        TargetAddr::Domain(host, port) => {
            match tokio::time::timeout(cfg.connect_timeout, tokio::net::lookup_host((host.as_str(), *port))).await {
                Ok(Ok(addrs)) => addrs.collect(),
                Ok(Err(_)) => return Err(reply::HOST_UNREACHABLE),
                Err(_) => return Err(reply::TTL_EXPIRED),
            }
        }
    };
    let allowed: Vec<SocketAddr> = candidates.iter().copied().filter(|a| cfg.egress.permits(*a)).collect();
    if allowed.is_empty() {
        return Err(if candidates.is_empty() { reply::HOST_UNREACHABLE } else { reply::NOT_ALLOWED });
    }
    let mut last = reply::HOST_UNREACHABLE;
    for addr in allowed {
        match tokio::time::timeout(cfg.connect_timeout, TcpStream::connect(addr)).await {
            Ok(Ok(s)) => {
                s.set_nodelay(true).ok();
                return Ok(s);
            }
            Ok(Err(e)) => {
                last = match e.kind() {
                    std::io::ErrorKind::ConnectionRefused => reply::CONNECTION_REFUSED,
                    std::io::ErrorKind::NetworkUnreachable => reply::NETWORK_UNREACHABLE,
                    _ => reply::HOST_UNREACHABLE,
                }
            }
            Err(_) => last = reply::TTL_EXPIRED,
        }
    }
    Err(last)
}

/// Shuttle bytes between a plain TCP socket and a tunnel channel until both directions
/// are done. `out_count` = bytes read from `tcp` and sent into the tunnel.
/// A tunnel that drops without an end-of-stream marker ends both directions at once.
pub(crate) async fn pipe<R, W>(
    tcp: TcpStream,
    mut rd: SecureReader<R>,
    mut wr: SecureWriter<W>,
    out_count: &AtomicU64,
    in_count: &AtomicU64,
) -> Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let (mut tr, mut tw) = tcp.into_split();
    let outbound = async {
        let mut buf = vec![0u8; MAX_CHUNK];
        loop {
            let n = tr.read(&mut buf).await?;
            if n == 0 {
                wr.send_eof().await?;
                return Ok::<_, IronTunnelError>(());
            }
            wr.send(&buf[..n]).await?;
            out_count.fetch_add(n as u64, Ordering::Relaxed);
        }
    };
    let inbound = async {
        loop {
            match rd.recv().await? {
                Some(d) if d.is_empty() => {
                    tw.shutdown().await.ok();
                    return Ok::<_, IronTunnelError>(());
                }
                Some(d) => {
                    tw.write_all(&d).await?;
                    in_count.fetch_add(d.len() as u64, Ordering::Relaxed);
                }
                None => return Err(IronTunnelError::Network("tunnel closed mid-stream".into())),
            }
        }
    };
    tokio::try_join!(outbound, inbound).map(|_| ())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sa(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn egress_refuses_the_relay_hosts_own_services() {
        let p = EgressPolicy::default();
        for denied in [
            "127.0.0.1:8080",   // Quillon API on Epsilon
            "127.0.0.1:18181",  // SIGIL API on Epsilon
            "10.77.0.5:18181",  // private mesh
            "192.168.1.1:80",
            "172.16.0.1:80",
            "169.254.169.254:80", // cloud metadata
            "100.64.0.1:80",
            "0.0.0.0:80",
            "[::1]:8080",
            "[fd00::1]:80",
            "[fe80::1]:80",
            "[::ffff:127.0.0.1]:8080",
            "[64:ff9b::7f00:1]:8080", // NAT64 → 127.0.0.1
            "[2002:7f00:1::]:8080",   // 6to4 → 127.0.0.1
            "8.8.8.8:25",             // SMTP
            "8.8.8.8:0",
        ] {
            assert!(!p.permits(sa(denied)), "{denied} must be refused");
        }
        for allowed in ["93.184.216.34:443", "1.1.1.1:6881", "[2606:4700::1111]:443"] {
            assert!(p.permits(sa(allowed)), "{allowed} must be allowed");
        }
        let test = EgressPolicy { allow_private: true, ..Default::default() };
        assert!(test.permits(sa("127.0.0.1:9000")));
        assert!(!test.permits(sa("127.0.0.1:25")));
    }

    #[test]
    fn authorized_file_format() {
        let k = StaticKeypair::generate().unwrap();
        let text = format!("# zentorrent laptop\n{}  # trailing comment\n\n", k.public().to_hex());
        let set = parse_authorized(&text).unwrap();
        assert!(set.contains(k.public()) && set.len() == 1);
        assert!(parse_authorized("not-hex").is_err());
    }
}
