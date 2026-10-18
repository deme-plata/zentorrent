//! .torrent files dragged onto the window from Explorer (or any file manager): any number
//! at once, and the .torrent files right inside a dropped folder.

use std::path::{Path, PathBuf};

/// The .torrent files among `paths` (for a folder: the .torrent files right inside it,
/// sorted), and the names of what was dropped but is not one.
pub fn torrents(paths: &[PathBuf]) -> (Vec<PathBuf>, Vec<String>) {
    let mut found = Vec::new();
    let mut other = Vec::new();
    for p in paths {
        if p.is_dir() {
            let mut inside: Vec<PathBuf> =
                std::fs::read_dir(p).into_iter().flatten().flatten().map(|e| e.path()).filter(|q| is_torrent(q)).collect();
            inside.sort();
            if inside.is_empty() {
                other.push(name(p));
            }
            found.extend(inside);
        } else if is_torrent(p) {
            found.push(p.clone());
        } else {
            other.push(name(p));
        }
    }
    (found, other)
}

fn is_torrent(p: &Path) -> bool {
    p.is_file() && named_torrent(p)
}

/// Named `*.torrent` (any case); says nothing about whether it exists.
pub fn named_torrent(p: &Path) -> bool {
    p.extension().is_some_and(|e| e.eq_ignore_ascii_case("torrent"))
}

fn name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.display().to_string())
}

/// What the window says while `all` files, `torrents` of them .torrent files, are dragged over it.
pub fn hover_text(torrents: usize, all: usize) -> String {
    match (torrents, all) {
        (1, 1) => "Drop to download this torrent".into(),
        (t, a) if t == a => format!("Drop to download these {t} torrents"),
        _ => "Drop .torrent files (or a folder of them) to download them".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn several_torrents_a_folder_of_them_and_what_is_left_out() {
        let d = std::env::temp_dir().join(format!("zt-drop-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(d.join("season")).unwrap();
        std::fs::create_dir_all(d.join("empty")).unwrap();
        for f in ["a.torrent", "B.TORRENT", "notes.txt", "season/e02.torrent", "season/e01.torrent", "season/info.nfo"] {
            std::fs::write(d.join(f), b"d4:infod4:name1:xee").unwrap();
        }
        let dropped: Vec<PathBuf> = ["a.torrent", "B.TORRENT", "notes.txt", "season", "empty"].iter().map(|f| d.join(f)).collect();
        let (found, other) = torrents(&dropped);
        let names: Vec<String> = found.iter().map(|p| name(p)).collect();
        assert_eq!(names, ["a.torrent", "B.TORRENT", "e01.torrent", "e02.torrent"]);
        assert_eq!(other, ["notes.txt", "empty"]);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_hint_counts_what_is_dragged() {
        assert_eq!(hover_text(1, 1), "Drop to download this torrent");
        assert_eq!(hover_text(3, 3), "Drop to download these 3 torrents");
        assert!(hover_text(0, 1).contains("folder"));
    }
}
