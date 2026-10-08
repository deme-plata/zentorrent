// VENDORED from flux crates/flux-irontunnel/src/proxy.rs @ 60dc7146.
// Do not edit here: change it in flux, then run scripts/vendor-irontunnel.sh.
//! The client side: a SOCKS5 proxy on loopback whose every connection leaves through
//! an encrypted tunnel to the relay. This is the surface an app (ZenTorrent) embeds:
//!
//! ```ignore
//! let handle = start_proxy(cfg).await?;          // binds 127.0.0.1:<port>
//! session_opts.proxy_url = Some(handle.socks5_url()); // librqbit: peers + HTTP trackers
//! let up = probe(&cfg_for_probe).await;           // "is the tunnel up" for a status light
//! ```
//!
//! Each SOCKS connection gets its own TCP connection and Noise handshake to the relay —
//! no multiplexing, so one stalled peer cannot block another.

use super::relay::{pipe, MSG_CONNECT, MSG_CONNECT_REPLY, MSG_PING, MSG_PONG};
use super::secure::{client_handshake, Channel, PublicKey, StaticKeypair};
use super::socks5::{read_connect_request, reply, send_reply};
use super::{IronTunnelError, Result};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::net::{TcpListener, TcpStream};
use tokio::task::JoinHandle;

#[derive(Clone)]
pub struct ProxyConfig {
    /// Where the SOCKS5 listener binds. Must be loopback: SOCKS5 here has no password.
    pub listen: SocketAddr,
    /// Relay address, `host:port`.
    pub relay: String,
    pub client_key: Arc<StaticKeypair>,
    /// The relay's pinned public key.
    pub relay_key: PublicKey,
    pub connect_timeout: Duration,
}

impl ProxyConfig {
    pub fn new(relay: impl Into<String>, client_key: StaticKeypair, relay_key: PublicKey) -> Self {
        Self {
            listen: SocketAddr::from(([127, 0, 0, 1], 0)),
            relay: relay.into(),
            client_key: Arc::new(client_key),
            relay_key,
            connect_timeout: Duration::from_secs(10),
        }
    }
}

#[derive(Debug, Default)]
pub struct ProxyStats {
    pub connections_total: AtomicU64,
    pub connections_active: AtomicU64,
    pub failures: AtomicU64,
    pub bytes_up: AtomicU64,
    pub bytes_down: AtomicU64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProxySnapshot {
    pub connections_total: u64,
    pub connections_active: u64,
    pub failures: u64,
    pub bytes_up: u64,
    pub bytes_down: u64,
}

pub struct ProxyHandle {
    local_addr: SocketAddr,
    stats: Arc<ProxyStats>,
    task: JoinHandle<()>,
}

impl ProxyHandle {
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// `socks5://127.0.0.1:<port>`, ready for librqbit's `connect.proxy_url`.
    pub fn socks5_url(&self) -> String {
        format!("socks5://{}", self.local_addr)
    }

    pub fn stats(&self) -> ProxySnapshot {
        let s = &self.stats;
        ProxySnapshot {
            connections_total: s.connections_total.load(Ordering::Relaxed),
            connections_active: s.connections_active.load(Ordering::Relaxed),
            failures: s.failures.load(Ordering::Relaxed),
            bytes_up: s.bytes_up.load(Ordering::Relaxed),
            bytes_down: s.bytes_down.load(Ordering::Relaxed),
        }
    }

    /// Stop accepting. Connections already open finish on their own.
    pub fn shutdown(self) {
        self.task.abort();
    }
}

impl Drop for ProxyHandle {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub async fn start_proxy(cfg: ProxyConfig) -> Result<ProxyHandle> {
    if !cfg.listen.ip().is_loopback() {
        return Err(IronTunnelError::Config(format!(
            "refusing to bind the SOCKS5 proxy on {}: it has no password, so it must stay on loopback",
            cfg.listen
        )));
    }
    let listener = TcpListener::bind(cfg.listen).await?;
    let local_addr = listener.local_addr()?;
    let stats = Arc::new(ProxyStats::default());
    let (cfg, st) = (Arc::new(cfg), stats.clone());
    let task = tokio::spawn(async move {
        loop {
            let Ok((sock, _)) = listener.accept().await else {
                tokio::time::sleep(Duration::from_millis(50)).await;
                continue;
            };
            let (cfg, st) = (cfg.clone(), st.clone());
            tokio::spawn(async move {
                st.connections_total.fetch_add(1, Ordering::Relaxed);
                st.connections_active.fetch_add(1, Ordering::Relaxed);
                if let Err(e) = serve(sock, &cfg, &st).await {
                    st.failures.fetch_add(1, Ordering::Relaxed);
                    tracing::debug!("socks connection ended: {e}");
                }
                st.connections_active.fetch_sub(1, Ordering::Relaxed);
            });
        }
    });
    Ok(ProxyHandle { local_addr, stats, task })
}

async fn open_channel(cfg: &ProxyConfig) -> Result<Channel<TcpStream>> {
    let t = cfg.connect_timeout;
    let tcp = tokio::time::timeout(t, TcpStream::connect(&cfg.relay))
        .await
        .map_err(|_| IronTunnelError::Network(format!("relay {} did not answer in {t:?}", cfg.relay)))??;
    tcp.set_nodelay(true).ok();
    tokio::time::timeout(t, client_handshake(tcp, &cfg.client_key, &cfg.relay_key))
        .await
        .map_err(|_| IronTunnelError::Network("relay handshake timed out".into()))?
}

async fn serve(mut sock: TcpStream, cfg: &ProxyConfig, st: &ProxyStats) -> Result<()> {
    sock.set_nodelay(true).ok();
    let target = read_connect_request(&mut sock).await?;
    let (mut rd, mut wr) = match open_channel(cfg).await {
        Ok(ch) => ch,
        Err(e) => {
            send_reply(&mut sock, reply::GENERAL_FAILURE).await.ok();
            return Err(e);
        }
    };
    let mut req = vec![MSG_CONNECT];
    req.extend(target.encode());
    wr.send(&req).await?;
    let answer = tokio::time::timeout(cfg.connect_timeout * 2, rd.recv())
        .await
        .map_err(|_| IronTunnelError::Network("relay did not answer CONNECT".into()))??;
    let code = match answer.as_deref() {
        Some([MSG_CONNECT_REPLY, code]) => *code,
        _ => reply::GENERAL_FAILURE,
    };
    send_reply(&mut sock, code).await?;
    if code != reply::SUCCEEDED {
        return Err(IronTunnelError::Network(format!("relay refused {target}: SOCKS code {code:#04x}")));
    }
    pipe(sock, rd, wr, &st.bytes_up, &st.bytes_down).await
}

/// One full handshake plus an encrypted PING/PONG. `Ok(rtt)` means the relay is up, it
/// holds the pinned key, and this client key is authorized there.
pub async fn probe(cfg: &ProxyConfig) -> Result<Duration> {
    let t0 = Instant::now();
    let (mut rd, mut wr) = open_channel(cfg).await?;
    wr.send(&[MSG_PING]).await?;
    match tokio::time::timeout(cfg.connect_timeout, rd.recv()).await {
        Ok(Ok(Some(m))) if m == [MSG_PONG] => Ok(t0.elapsed()),
        Ok(Ok(other)) => Err(IronTunnelError::Network(format!("unexpected probe answer {other:?}"))),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(IronTunnelError::Network("probe timed out".into())),
    }
}
