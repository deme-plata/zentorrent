# Vendored librqbit 9.0.1

An unmodified copy of librqbit 9.0.1 from crates.io (MIT OR Apache-2.0, see its LICENSE files),
plus TWO changes. First, in `librqbit/src/torrent_state/live/mod.rs`:

```rust
pub fn ratelimits(&self) -> &Limits { &self.ratelimits }
```

Upstream sets a torrent's download/upload limits only when the torrent is added; the limiter
itself already supports changing them at runtime (`Limits::set_download_bps`). Exposing it lets
ZenTorrent's Details panel change one torrent's speed limits instantly, without removing and
re-adding (and re-hashing) the torrent.

To upgrade librqbit: copy the new version here, re-apply the getter (search "ZenTorrent patch"),
and bump the version in Cargo.toml.

Second (VPN, `src/vpn.rs`), in `librqbit/src/session.rs` where the HTTP client gets its proxy:
`socks5://` is rewritten to `socks5h://`, so tracker host names are resolved by the proxy (the
IronTunnel relay) instead of locally. Measured before the patch: one DNS query per tracker name
from the ZenTorrent process to the local resolver while every byte of traffic was tunnelled.
Only runs when a proxy is configured, i.e. only with the VPN on.
