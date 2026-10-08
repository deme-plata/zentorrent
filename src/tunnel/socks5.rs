// VENDORED from flux crates/flux-irontunnel/src/socks5.rs @ 4fb1ad85.
// Do not edit here: change it in flux, then run scripts/vendor-irontunnel.sh.
//! The local SOCKS5 front door (RFC 1928), CONNECT only, no authentication.
//!
//! No authentication is safe only because the proxy binds to loopback (enforced in
//! `proxy::start_proxy`). BIND and UDP ASSOCIATE are refused with reply 0x07: UDP cannot
//! ride this TCP tunnel, so an app that needs UDP must turn it off rather than have it
//! silently bypass the tunnel.
//!
//! [`TargetAddr`] uses the SOCKS5 address encoding, and the tunnel's CONNECT message
//! reuses it, so a domain name reaches the relay unresolved and DNS never leaves the
//! client in the clear.

use super::{IronTunnelError, Result};
use std::fmt;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub mod reply {
    pub const SUCCEEDED: u8 = 0x00;
    pub const GENERAL_FAILURE: u8 = 0x01;
    pub const NOT_ALLOWED: u8 = 0x02;
    pub const NETWORK_UNREACHABLE: u8 = 0x03;
    pub const HOST_UNREACHABLE: u8 = 0x04;
    pub const CONNECTION_REFUSED: u8 = 0x05;
    pub const TTL_EXPIRED: u8 = 0x06;
    pub const COMMAND_NOT_SUPPORTED: u8 = 0x07;
    pub const ADDRESS_TYPE_NOT_SUPPORTED: u8 = 0x08;
}

const VERSION: u8 = 0x05;
const NO_AUTH: u8 = 0x00;
const NO_ACCEPTABLE_METHOD: u8 = 0xFF;
const CMD_CONNECT: u8 = 0x01;
const ATYP_V4: u8 = 0x01;
const ATYP_DOMAIN: u8 = 0x03;
const ATYP_V6: u8 = 0x04;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TargetAddr {
    Ip(SocketAddr),
    Domain(String, u16),
}

impl TargetAddr {
    pub fn port(&self) -> u16 {
        match self {
            TargetAddr::Ip(a) => a.port(),
            TargetAddr::Domain(_, p) => *p,
        }
    }

    /// SOCKS5 address encoding: ATYP, address, port (big endian).
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(32);
        match self {
            TargetAddr::Ip(SocketAddr::V4(a)) => {
                out.push(ATYP_V4);
                out.extend_from_slice(&a.ip().octets());
            }
            TargetAddr::Ip(SocketAddr::V6(a)) => {
                out.push(ATYP_V6);
                out.extend_from_slice(&a.ip().octets());
            }
            TargetAddr::Domain(host, _) => {
                out.push(ATYP_DOMAIN);
                out.push(host.len() as u8);
                out.extend_from_slice(host.as_bytes());
            }
        }
        out.extend_from_slice(&self.port().to_be_bytes());
        out
    }

    /// Inverse of [`TargetAddr::encode`]; returns the address and the bytes consumed.
    pub fn decode(buf: &[u8]) -> Result<(Self, usize)> {
        let short = || IronTunnelError::Parse("truncated SOCKS5 address".into());
        let (&atyp, rest) = buf.split_first().ok_or_else(short)?;
        let (addr, used) = match atyp {
            ATYP_V4 => {
                let o: [u8; 4] = rest.get(..4).ok_or_else(short)?.try_into().unwrap();
                (Ok(IpAddr::V4(Ipv4Addr::from(o))), 4)
            }
            ATYP_V6 => {
                let o: [u8; 16] = rest.get(..16).ok_or_else(short)?.try_into().unwrap();
                (Ok(IpAddr::V6(Ipv6Addr::from(o))), 16)
            }
            ATYP_DOMAIN => {
                let len = *rest.first().ok_or_else(short)? as usize;
                let name = rest.get(1..1 + len).ok_or_else(short)?;
                let name = std::str::from_utf8(name)
                    .map_err(|_| IronTunnelError::Parse("domain is not UTF-8".into()))?;
                if name.is_empty() {
                    return Err(IronTunnelError::Parse("empty domain".into()));
                }
                (Err(name.to_string()), 1 + len)
            }
            other => return Err(IronTunnelError::Parse(format!("unknown address type {other:#04x}"))),
        };
        let p = rest.get(used..used + 2).ok_or_else(short)?;
        let port = u16::from_be_bytes([p[0], p[1]]);
        let target = match addr {
            Ok(ip) => TargetAddr::Ip(SocketAddr::new(ip, port)),
            Err(host) => TargetAddr::Domain(host, port),
        };
        Ok((target, 1 + used + 2))
    }
}

impl fmt::Display for TargetAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            TargetAddr::Ip(a) => write!(f, "{a}"),
            TargetAddr::Domain(h, p) => write!(f, "{h}:{p}"),
        }
    }
}

/// Run the SOCKS5 greeting and read one request. Returns the CONNECT target; anything
/// else is answered with the proper error reply and returned as an error.
pub async fn read_connect_request<S: AsyncRead + AsyncWrite + Unpin>(s: &mut S) -> Result<TargetAddr> {
    let mut head = [0u8; 2];
    s.read_exact(&mut head).await?;
    if head[0] != VERSION {
        return Err(IronTunnelError::Parse(format!("not SOCKS5 (version byte {:#04x})", head[0])));
    }
    let mut methods = vec![0u8; head[1] as usize];
    s.read_exact(&mut methods).await?;
    if !methods.contains(&NO_AUTH) {
        s.write_all(&[VERSION, NO_ACCEPTABLE_METHOD]).await?;
        return Err(IronTunnelError::Auth("SOCKS5 client offered no 'no authentication' method".into()));
    }
    s.write_all(&[VERSION, NO_AUTH]).await?;

    let mut req = [0u8; 4];
    s.read_exact(&mut req).await?;
    if req[0] != VERSION {
        return Err(IronTunnelError::Parse("bad SOCKS5 request version".into()));
    }
    let mut addr = vec![req[3]];
    match req[3] {
        ATYP_V4 => addr.resize(1 + 4 + 2, 0),
        ATYP_V6 => addr.resize(1 + 16 + 2, 0),
        ATYP_DOMAIN => {
            let mut len = [0u8; 1];
            s.read_exact(&mut len).await?;
            addr.push(len[0]);
            addr.resize(2 + len[0] as usize + 2, 0);
        }
        _ => {
            send_reply(s, reply::ADDRESS_TYPE_NOT_SUPPORTED).await?;
            return Err(IronTunnelError::Parse("unsupported SOCKS5 address type".into()));
        }
    }
    let start = if req[3] == ATYP_DOMAIN { 2 } else { 1 };
    s.read_exact(&mut addr[start..]).await?;
    if req[1] != CMD_CONNECT {
        send_reply(s, reply::COMMAND_NOT_SUPPORTED).await?;
        return Err(IronTunnelError::Parse(format!(
            "SOCKS5 command {:#04x} refused: only CONNECT is tunnelled (UDP cannot ride this tunnel)",
            req[1]
        )));
    }
    match TargetAddr::decode(&addr) {
        Ok((t, _)) => Ok(t),
        Err(e) => {
            send_reply(s, reply::GENERAL_FAILURE).await?;
            Err(e)
        }
    }
}

/// Final SOCKS5 reply. The bound address is reported as 0.0.0.0:0 — the real egress
/// address belongs to the relay, and clients (librqbit, curl) do not use it.
pub async fn send_reply<S: AsyncWrite + Unpin>(s: &mut S, code: u8) -> Result<()> {
    s.write_all(&[VERSION, code, 0x00, ATYP_V4, 0, 0, 0, 0, 0, 0]).await?;
    s.flush().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    #[test]
    fn address_round_trips() {
        for t in [
            TargetAddr::Ip("93.184.216.34:443".parse().unwrap()),
            TargetAddr::Ip("[2606:2800:220:1::248]:6881".parse().unwrap()),
            TargetAddr::Domain("tracker.example.org".into(), 6969),
        ] {
            let enc = t.encode();
            assert_eq!(TargetAddr::decode(&enc).unwrap(), (t, enc.len()));
        }
        assert!(TargetAddr::decode(&[ATYP_V4, 1, 2]).is_err());
        assert!(TargetAddr::decode(&[0x09, 0, 0]).is_err());
    }

    async fn client_says(bytes: Vec<u8>) -> (Result<TargetAddr>, Vec<u8>) {
        let (mut c, mut s) = duplex(4096);
        c.write_all(&bytes).await.unwrap();
        let res = read_connect_request(&mut s).await;
        drop(s);
        let mut answer = Vec::new();
        c.read_to_end(&mut answer).await.unwrap();
        (res, answer)
    }

    #[tokio::test]
    async fn connect_by_domain() {
        let mut b = vec![5, 1, 0, 5, 1, 0, 3, 11];
        b.extend_from_slice(b"example.org");
        b.extend_from_slice(&443u16.to_be_bytes());
        let (res, answer) = client_says(b).await;
        assert_eq!(res.unwrap(), TargetAddr::Domain("example.org".into(), 443));
        assert_eq!(answer, vec![5, 0], "method selection only; the final reply is the caller's");
    }

    #[tokio::test]
    async fn connect_by_ipv4() {
        let (res, _) = client_says(vec![5, 1, 0, 5, 1, 0, 1, 1, 2, 3, 4, 0x1a, 0xe1]).await;
        assert_eq!(res.unwrap(), TargetAddr::Ip("1.2.3.4:6881".parse().unwrap()));
    }

    #[tokio::test]
    async fn udp_associate_is_refused() {
        let (res, answer) = client_says(vec![5, 1, 0, 5, 3, 0, 1, 0, 0, 0, 0, 0, 0]).await;
        assert!(res.is_err());
        assert_eq!(&answer[..4], &[5, 0, 5, reply::COMMAND_NOT_SUPPORTED]);
    }

    #[tokio::test]
    async fn password_only_client_is_refused() {
        let (res, answer) = client_says(vec![5, 1, 2]).await;
        assert!(res.is_err());
        assert_eq!(answer, vec![5, 0xFF]);
    }
}
