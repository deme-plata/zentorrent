//! The player engine on Windows: ZenTorrent's own LGPL build of libmpv-2.dll
//! (mpv 0.39.0 with -Dgpl=false, FFmpeg 7.1 without --enable-gpl, everything
//! linked into the one DLL), fetched on the first Play instead of making every
//! download of ZenTorrent 28 MB bigger.
//!
//! The file's size and BLAKE3 are compiled in here, so the download is trusted
//! exactly as much as this exe: anything else is refused before it is saved. A
//! libmpv-2.dll placed next to ZenTorrent.exe still wins (`ffi::candidates`), so
//! the library stays replaceable, as its licence asks. Sources and the build
//! script: https://quillon.xyz/downloads/zentorrent-libmpv-2-0.39.0-lgpl-src.tar.gz

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

pub const URL: &str = "https://quillon.xyz/downloads/zentorrent-libmpv-2-0.39.0-lgpl.dll";
pub const SIZE: u64 = 28_485_632;
pub const BLAKE3: &str = "015137386d0e79a76136bd9032eb8939831b9dd7b7355e6e0483ad975c670248";

/// Where the downloaded engine lives (machine-local, not roaming).
pub fn path() -> PathBuf {
    dirs::data_local_dir().unwrap_or_else(|| PathBuf::from(".")).join("zentorrent").join("player").join("libmpv-2.dll")
}

/// Only Windows gets the engine this way; Linux has it from the package manager.
pub fn needed() -> bool {
    cfg!(windows) && super::ffi::api().is_err() && !path().exists()
}

/// Download, check and install the engine. `got` counts bytes as they arrive.
/// `proxy` is the VPN's socks5h URL when the tunnel is up.
pub async fn fetch(proxy: Option<String>, got: Arc<AtomicU64>) -> Result<(), String> {
    let mut b = reqwest::Client::builder()
        .user_agent(concat!("ZenTorrent/", env!("CARGO_PKG_VERSION"), " player"))
        .connect_timeout(std::time::Duration::from_secs(15))
        .timeout(std::time::Duration::from_secs(30 * 60));
    if let Some(p) = proxy {
        b = b.proxy(reqwest::Proxy::all(p).map_err(|e| e.to_string())?);
    }
    let http = b.build().map_err(|e| e.to_string())?;
    let mut r = http
        .get(URL)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("could not download the player engine: {e}"))?;
    let mut bytes = Vec::with_capacity(SIZE as usize);
    while let Some(c) = r.chunk().await.map_err(|e| format!("player engine download stopped: {e}"))? {
        bytes.extend_from_slice(&c);
        if bytes.len() as u64 > SIZE {
            return Err("the player engine download is larger than expected — refused".into());
        }
        got.store(bytes.len() as u64, Ordering::Relaxed);
    }
    let a = crate::update::Artifact { url: URL.into(), blake3_hex: BLAKE3.into(), size_bytes: SIZE };
    crate::update::check_artifact(&bytes, &a).map_err(|e| format!("player engine: {e}"))?;
    let dest = path();
    tokio::task::spawn_blocking(move || -> Result<(), String> {
        let dir = dest.parent().ok_or("no folder for the player engine")?;
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let tmp = dest.with_extension("dll.part");
        std::fs::write(&tmp, &bytes).map_err(|e| format!("{}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, &dest).map_err(|e| format!("{}: {e}", dest.display()))
    })
    .await
    .map_err(|e| e.to_string())?
}

#[cfg(test)]
mod tests {
    #[test]
    fn pinned_hash_is_well_formed() {
        assert_eq!(super::BLAKE3.len(), 64);
        assert!(super::BLAKE3.bytes().all(|b| b.is_ascii_hexdigit()));
        assert!(super::URL.starts_with("https://"));
    }
}
