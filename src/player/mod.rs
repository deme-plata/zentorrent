//! ZenTorrent's built-in player (libmpv, loaded at run time).
//!
//! * `ffi`      — the libmpv entry points, opened when the player is first used
//! * `engine`   — Windows: ZenTorrent's own LGPL libmpv-2.dll, fetched on the first Play
//! * `stream`   — `zt://` streaming: mpv reads files that are still downloading
//! * `playlist` — a torrent's files (and its .m3u playlists) → what plays, in order
//! * `audio`    — the engine: queue, gapless playback, sound settings
//! * `ui`       — the now-playing bar, the playlist panel and the Sound window

pub mod audio;
pub mod engine;
pub mod ffi;
pub mod playlist;
pub mod stream;
pub mod ui;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use librqbit::ManagedTorrent;
use tokio::io::AsyncReadExt;

pub use audio::Player;
use playlist::Track;

/// Does this torrent have anything to play? (Needs its metadata.)
pub fn has_media(handle: &ManagedTorrent) -> bool {
    handle
        .with_metadata(|m| m.file_infos.iter().any(|f| playlist::kind_of(&f.relative_filename.to_string_lossy()).is_some()))
        .unwrap_or(false)
}

/// An m3u from a Windows ripper is often not UTF-8: read it as Latin-1 then.
fn text_of(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => bytes.iter().map(|&b| b as char).collect(),
    }
}

/// Read the torrent's file list and playlists, and build the play order.
/// Playlists still downloading are read through the stream (they are small).
pub async fn load_tracks(handle: Arc<ManagedTorrent>) -> Result<(Vec<Track>, PathBuf), String> {
    let files: Vec<(String, usize, u64)> = handle
        .with_metadata(|m| {
            m.file_infos
                .iter()
                .enumerate()
                .map(|(i, f)| (f.relative_filename.to_string_lossy().replace('\\', "/"), i, f.len))
                .collect()
        })
        .map_err(|_| "the torrent's file list hasn't arrived from peers yet".to_string())?;
    let progress = handle.stats().file_progress;
    let folder = handle.output_folder().to_path_buf();
    let mut m3us = Vec::new();
    for (path, i, len) in files.iter().filter(|(p, _, _)| playlist::is_m3u(p)) {
        if *len > 512 * 1024 {
            continue; // not a playlist, whatever its name says
        }
        let bytes = if progress.get(*i) == Some(len) {
            tokio::fs::read(folder.join(path)).await.ok()
        } else {
            let h = handle.clone();
            tokio::time::timeout(Duration::from_secs(20), async move {
                let mut s = h.stream(*i).await.ok()?;
                let mut b = Vec::new();
                s.read_to_end(&mut b).await.ok()?;
                Some(b)
            })
            .await
            .ok()
            .flatten()
        };
        if let Some(b) = bytes {
            m3us.push((path.clone(), text_of(&b)));
        }
    }
    let list: Vec<(String, usize)> = files.iter().map(|(p, i, _)| (p.clone(), *i)).collect();
    let tracks = playlist::build(&list, &m3us);
    if tracks.is_empty() {
        return Err("there are no audio or video files in this torrent".into());
    }
    Ok((tracks, folder))
}

#[cfg(test)]
mod tests {
    #[test]
    fn latin1_playlists_are_read() {
        assert_eq!(super::text_of(b"Bj\xf6rk - J\xf3ga.mp3"), "Björk - Jóga.mp3");
        assert_eq!(super::text_of("Björk".as_bytes()), "Björk");
    }
}
