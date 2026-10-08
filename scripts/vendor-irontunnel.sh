#!/usr/bin/env bash
# Re-vendor the IronTunnel v1 client (Noise IK tunnel + SOCKS5 proxy) from the flux
# workspace into src/tunnel/. ZenTorrent cannot take a `path =` dependency on flux (it
# breaks the Windows build and fresh clones), so the four files are copied verbatim with
# only their `crate::` paths rewritten to `super::`. Edit upstream, then re-run this.
set -euo pipefail
SRC=${FLUX_IRONTUNNEL:-/home/storage/deepseek-codewhale/flux/crates/flux-irontunnel}
DST=$(cd "$(dirname "$0")/.." && pwd)/src/tunnel
REV=$(git -C "$SRC" log -1 --format=%h -- .)
for m in secure socks5 relay proxy; do
  {
    echo "// VENDORED from flux crates/flux-irontunnel/src/$m.rs @ $REV."
    echo "// Do not edit here: change it in flux, then run scripts/vendor-irontunnel.sh."
    sed -e 's/use crate::{IronTunnelError, Result};/use super::{IronTunnelError, Result};/' \
        -e 's/use crate::\(secure\|socks5\|relay\|proxy\)::/use super::\1::/g' \
        "$SRC/src/$m.rs"
  } > "$DST/$m.rs"
done
if grep -n 'crate::' "$DST"/{secure,socks5,relay,proxy}.rs; then
  echo "unrewritten crate:: paths above — fix the sed rules" >&2; exit 1
fi
echo "vendored flux-irontunnel @ $REV into $DST"
