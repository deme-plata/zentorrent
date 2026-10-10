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

Third (2026-10-10), in `librqbit/src/session_persistence/json.rs`: every file the session store
writes (`session.json` via its `.tmp`, the `.bitv` progress files, the stored `.torrent` copies) is
flushed and `sync_all`ed before it is renamed into place or dropped. Upstream calls `write_all` on a
tokio `File` and renames straight after; tokio only *queues* that write, so an exit right after a
save (closing ZenTorrent) left `session.json` EMPTY — and the engine then refuses to start
("error deserializing session database: EOF while parsing"). Search "ZenTorrent patch".
