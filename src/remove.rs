//! Removing a torrent with its files: the torrent's own files go to the Recycle Bin
//! (the trash on Linux), so a wrong click can be undone; then the folders they leave
//! empty are removed, but only inside the torrent's own folder, and never the Save to
//! folder, Downloads or home.

use std::path::{Path, PathBuf};

/// What the user knows the place as.
pub const BIN: &str = if cfg!(windows) { "Recycle Bin" } else { "trash" };

/// Move the files (those that exist) to the Recycle Bin, then remove the folders left
/// empty under `root` (the torrent's output folder; `root` itself only when it is the
/// torrent's own folder). Returns how many files went.
pub fn trash_files(files: &[PathBuf], root: &Path, own_root: bool, protected: &[PathBuf]) -> Result<usize, String> {
    let there: Vec<&PathBuf> = files.iter().filter(|f| f.is_file()).collect();
    if !there.is_empty() {
        trash::delete_all(&there).map_err(|e| format!("could not move the files to the {BIN}: {e}"))?;
    }
    remove_empty_dirs(files, root, own_root, protected);
    Ok(there.len())
}

/// The folders the files were in, deepest first, that may go once empty.
fn candidates(files: &[PathBuf], root: &Path, own_root: bool, protected: &[PathBuf]) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = files
        .iter()
        .flat_map(|f| f.ancestors().skip(1).map(Path::to_path_buf))
        .filter(|d| d.starts_with(root) && (own_root || d != root))
        // A protected folder, or one that holds one, stays.
        .filter(|d| !protected.iter().any(|p| p.starts_with(d)))
        .collect();
    dirs.sort_by_key(|d| std::cmp::Reverse(d.components().count()));
    dirs.dedup();
    dirs
}

fn remove_empty_dirs(files: &[PathBuf], root: &Path, own_root: bool, protected: &[PathBuf]) {
    for d in candidates(files, root, own_root, protected) {
        // Fails (and is left alone) when something else is still in it.
        let _ = std::fs::remove_dir(&d);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_torrents_own_empty_folders_go() {
        let t = std::env::temp_dir().join(format!("zt-remove-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&t);
        let save = t.join("Downloads");
        let album = save.join("Album");
        std::fs::create_dir_all(album.join("Scans")).unwrap();
        std::fs::create_dir_all(save.join("Other")).unwrap();
        let files = vec![album.join("01.flac"), album.join("Scans/Back.jpg")];
        // The files are gone (as after the Recycle Bin): Scans and Album go, Downloads stays.
        remove_empty_dirs(&files, &album, true, &[save.clone()]);
        assert!(!album.exists());
        assert!(save.join("Other").exists() && save.exists());

        // A folder with something else in it stays.
        std::fs::create_dir_all(album.join("Scans")).unwrap();
        std::fs::write(album.join("extracted.mkv"), b"x").unwrap();
        remove_empty_dirs(&files, &album, true, &[save.clone()]);
        assert!(!album.join("Scans").exists() && album.join("extracted.mkv").exists());
        let _ = std::fs::remove_dir_all(&t);
    }

    #[test]
    fn a_single_file_never_takes_the_save_folder() {
        let save = PathBuf::from("/d/Downloads");
        let files = vec![save.join("film.mkv")];
        assert!(candidates(&files, &save, false, &[]).is_empty(), "the output folder is the shared save folder");
        // Old torrents saved straight into a protected folder: it and what holds it stay.
        let files = vec![save.join("Album/01.flac")];
        assert_eq!(candidates(&files, &save, true, &[save.clone()]), vec![save.join("Album")]);
        assert!(candidates(&[PathBuf::from("/d/x/01.flac")], Path::new("/d"), true, &[PathBuf::from("/d/x/keep")]).is_empty());
    }
}
