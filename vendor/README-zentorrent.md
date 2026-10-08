# Vendored librqbit 9.0.1

An unmodified copy of librqbit 9.0.1 from crates.io (MIT OR Apache-2.0, see its LICENSE files),
plus ONE addition in `librqbit/src/torrent_state/live/mod.rs`:

```rust
pub fn ratelimits(&self) -> &Limits { &self.ratelimits }
```

Upstream sets a torrent's download/upload limits only when the torrent is added; the limiter
itself already supports changing them at runtime (`Limits::set_download_bps`). Exposing it lets
ZenTorrent's Details panel change one torrent's speed limits instantly, without removing and
re-adding (and re-hashing) the torrent.

To upgrade librqbit: copy the new version here, re-apply the getter (search "ZenTorrent patch"),
and bump the version in Cargo.toml.
