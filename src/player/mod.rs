//! ZenTorrent's built-in player (libmpv, loaded at run time).
//!
//! * `ffi`      — the libmpv entry points, opened when the player is first used
//! * `engine`   — Windows: ZenTorrent's own LGPL libmpv-2.dll, fetched on the first Play
//! * `stream`   — `zt://` streaming: mpv reads files that are still downloading
//! * `archive`  — films inside RAR/ZIP/TAR archives and .001 splits, streamed too
//! * `playlist` — a torrent's files (and its .m3u playlists) → what plays, in order
//! * `audio`    — the engine: queue, gapless playback, sound settings
//! * `ui`       — the now-playing bar, the playlist panel and the Sound window

pub mod archive;
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

/// Does this torrent have anything to play, loose or in an archive? (Needs its metadata.)
pub fn has_media(handle: &ManagedTorrent) -> bool {
    handle
        .with_metadata(|m| {
            m.file_infos.iter().any(|f| {
                let p = f.relative_filename.to_string_lossy();
                playlist::kind_of(&p).is_some() || archive::may_hold_media(&p)
            })
        })
        .unwrap_or(false)
}

/// An m3u from a Windows ripper is often not UTF-8: read it as Latin-1 then.
fn text_of(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => bytes.iter().map(|&b| b as char).collect(),
    }
}

/// Read the torrent's file list, playlists and archives, and build the play order.
/// Playlists and archive headers still downloading are read through the stream
/// (they are small; their pieces are fetched first). Indexed archives go into
/// `reg`, where the stream finds them.
pub async fn load_tracks(handle: Arc<ManagedTorrent>, reg: &'static stream::Registry) -> Result<(Vec<Track>, PathBuf), String> {
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
    let mut tracks = playlist::build(&list, &m3us);

    // Films and episodes packed in archives (scene RAR sets, ZIP, TAR, .001 splits).
    let hash = handle.info_hash().as_string();
    let src = stream::TorrentSource::new(handle.clone());
    let mut packed = Vec::new();
    let mut why_not = Vec::new();
    for set in archive::find_sets(&files) {
        let first = set.volumes[0].clone();
        let shown = first.path.rsplit('/').next().unwrap_or(&first.path).to_string();
        let a = match tokio::time::timeout(Duration::from_secs(60), archive::index(&src, &set)).await {
            Ok(Ok(a)) => a,
            Ok(Err(e)) => {
                why_not.push(format!("{shown}: {e}"));
                continue;
            }
            Err(_) => {
                why_not.push(format!("{shown}: its header hasn't arrived from peers yet — try again in a moment"));
                continue;
            }
        };
        let dir = first.path.rsplit_once('/').map(|(d, _)| format!("{d}/")).unwrap_or_default();
        let parts: Vec<usize> = set.volumes.iter().map(|v| v.file).collect();
        for (n, m) in a.members.iter().enumerate() {
            let Some(kind) = playlist::kind_of(&m.name) else { continue };
            if let Some(why) = &m.why_not {
                why_not.push(format!("{}: {why}", m.name));
                continue;
            }
            packed.push(Track {
                file: first.file,
                path: format!("{dir}{}", m.name),
                title: playlist::title_from_path(&m.name),
                duration: None,
                kind,
                archive: Some((first.file, n)),
                parts: parts.clone(),
            });
        }
        reg.archives.lock().unwrap().insert((hash.clone(), first.file), Arc::new(a));
    }
    // An m3u's order stands; otherwise loose and packed files share one natural order.
    tracks.extend(packed);
    if m3us.is_empty() {
        playlist::order(&mut tracks);
    }
    if tracks.is_empty() {
        return Err(match why_not.first() {
            Some(w) => format!("nothing here can be played while downloading — {w}"),
            None => "there are no audio or video files in this torrent".into(),
        });
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
