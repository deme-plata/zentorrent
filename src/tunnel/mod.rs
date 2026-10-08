//! IronTunnel v1, vendored from flux `crates/flux-irontunnel` (see
//! `scripts/vendor-irontunnel.sh`): a Noise IK encrypted tunnel to a relay, fronted by
//! a SOCKS5 proxy on loopback. ZenTorrent's own glue lives in `vpn.rs`; nothing in this
//! directory except this file is edited here.
//!
//! `relay` is vendored too: the client needs its wire constants and byte pipe, and the
//! tests run a real relay in-process.

#![allow(dead_code)]

pub mod proxy;
pub mod relay;
pub mod secure;
pub mod socks5;

use std::fmt;

/// The subset of flux-irontunnel's error type the vendored modules use.
#[derive(Debug)]
pub enum IronTunnelError {
    Config(String),
    Network(String),
    Crypto(String),
    Auth(String),
    Parse(String),
    Io(std::io::Error),
}

impl fmt::Display for IronTunnelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(m) => write!(f, "Configuration error: {m}"),
            Self::Network(m) => write!(f, "Network error: {m}"),
            Self::Crypto(m) => write!(f, "Cryptographic error: {m}"),
            Self::Auth(m) => write!(f, "Authentication error: {m}"),
            Self::Parse(m) => write!(f, "Parse error: {m}"),
            Self::Io(e) => write!(f, "IO error: {e}"),
        }
    }
}

impl std::error::Error for IronTunnelError {}

impl From<std::io::Error> for IronTunnelError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e)
    }
}

pub type Result<T> = std::result::Result<T, IronTunnelError>;
