#!/usr/bin/env bash
# ZenTorrent release: the ONLY way an update reaches installed copies.
# Usage: scripts/release.sh <version> "<one-line note>"
# Builds linux-x64 + windows-x64 with fluxc, publishes them to quillon.xyz/downloads,
# writes zentorrent-latest.json, signs it with the pinned Ed25519 release key, then
# re-fetches the LIVE manifest + signature and verifies them before declaring success.
set -euo pipefail
VER="${1:?usage: release.sh <version> \"<note>\"}"; NOTE="${2:-}"
REPO="$(cd "$(dirname "$0")/.." && pwd)"; cd "$REPO"
SEED="${ZENTORRENT_RELEASE_SEED:-/root/.config/zentorrent-release/release-sign.seed}"
PUB=e64325a53443b3e49292bd034e3d71e93b054a7c1ee1e222b58bbaf16c2b8e9a
DL=/home/orobit/q-narwhalknight/dist-final/downloads
BASE=https://quillon.xyz/downloads
[ -f "$SEED" ] || { echo "✗ release seed missing: $SEED" >&2; exit 1; }

LIVE=$(curl -fsS "$BASE/zentorrent-latest.json" 2>/dev/null | python3 -c 'import json,sys;print(json.load(sys.stdin)["version"])' 2>/dev/null || echo 0)
python3 - "$VER" "$LIVE" <<'PY' || { echo "✗ $VER is not newer than live $LIVE" >&2; exit 1; }
import sys; p=lambda v:[int(x) for x in v.split('.')]; sys.exit(0 if p(sys.argv[1])>p(sys.argv[2]) else 1)
PY

# Build dirs: a release from a second checkout (worktree) MUST use its own dirs. On
# 2026-10-08 two releases shared /home/storage/zentorrent-target and the 0.8.0 Linux
# artifact was the other checkout's 0.7.1 binary, copied 26 s after it landed.
TGT="${ZT_TARGET:-/home/storage/zentorrent-target}"
TGTW="${ZT_TARGET_WIN:-/home/storage/zentorrent-target-win}"

sed -i "0,/^version = \".*\"/s//version = \"$VER\"/" Cargo.toml
echo "▸ building $VER  (targets: $TGT, $TGTW)"
CARGO_TARGET_DIR="$TGT" nice -n10 fluxc build --release
CARGO_TARGET_DIR="$TGTW" nice -n10 fluxc build --release --target x86_64-pc-windows-gnu

TMP=$(mktemp -d /home/storage/sigil-scratch/zt-release.XXXX); trap 'rm -rf "$TMP"' EXIT
L=zentorrent-$VER-linux-x64; W=zentorrent-$VER-windows-x64.exe
# Copy out FIRST, then check the copies: nothing can change them between the check and
# the publish. A binary that does not embed this version is refused, never published.
cp "$TGT/release/zentorrent" "$TMP/$L"
cp "$TGTW/x86_64-pc-windows-gnu/release/zentorrent.exe" "$TMP/$W"
for f in "$L" "$W"; do
  grep -aq "ZenTorrent/$VER updater" "$TMP/$f" \
    || { echo "✗ $f does not embed version $VER (overwritten by another build?) — refusing to publish" >&2; exit 1; }
done
echo "✓ both binaries embed version $VER"
cp "$TMP/$L" "$DL/$L"
cp "$TMP/$W" "$DL/$W"
git archive --prefix="zentorrent-$VER/" -o "$DL/zentorrent-$VER-src.tar.gz" HEAD
python3 - "$VER" "$NOTE" "$DL/$L" "$DL/$W" "$BASE" "$TMP/m.json" <<'PY'
import json,sys,os,subprocess
ver,note,l,w,base,out=sys.argv[1:]
def art(p): return {"url":f"{base}/{os.path.basename(p)}","blake3_hex":subprocess.check_output(["b3sum","--no-names",p]).decode().strip(),"size_bytes":os.path.getsize(p)}
m={"product":"zentorrent","version":ver,"notes":note,"targets":{"linux-x64":art(l),"windows-x64":art(w)}}
open(out,"w").write(json.dumps(m,indent=2)+"\n")
PY
python3 - "$TMP/m.json" "$SEED" <<'PY'
import sys
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
m,s=sys.argv[1:]; open(m+".sig","w").write(Ed25519PrivateKey.from_private_bytes(open(s,"rb").read()).sign(open(m,"rb").read()).hex())
PY
cp "$TMP/m.json" "$DL/zentorrent-latest.json.new"; cp "$TMP/m.json.sig" "$DL/zentorrent-latest.json.sig.new"
mv "$DL/zentorrent-latest.json.new" "$DL/zentorrent-latest.json"; mv "$DL/zentorrent-latest.json.sig.new" "$DL/zentorrent-latest.json.sig"

echo "▸ verifying the LIVE channel"
curl -fsS "$BASE/zentorrent-latest.json" -o "$TMP/live.json"; curl -fsS "$BASE/zentorrent-latest.json.sig" -o "$TMP/live.sig"
python3 - "$TMP/live.json" "$TMP/live.sig" "$PUB" "$VER" <<'PY'
import sys,json
from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PublicKey
b=open(sys.argv[1],"rb").read(); Ed25519PublicKey.from_public_bytes(bytes.fromhex(sys.argv[3])).verify(bytes.fromhex(open(sys.argv[2]).read().strip()),b)
assert json.loads(b)["version"]==sys.argv[4]; print("✓ live manifest signature verifies, version", sys.argv[4])
PY
for f in "$L" "$W"; do curl -fsS -o /dev/null -w "✓ HTTP %{http_code} %{size_download}B $f\n" "$BASE/$f"; done

git add Cargo.toml Cargo.lock
git -c commit.gpgsign=false commit -qm "release v$VER: $NOTE" || true
git tag -f "v$VER" >/dev/null
echo "✓ ZenTorrent $VER released"
