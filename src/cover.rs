//! The picture that comes inside a torrent (an album's cover.jpg or folder.jpg, a
//! film's poster.jpg): chosen from the file list, read once that file has
//! downloaded, and shown next to the torrent in the Downloads list and as its
//! thumbnail.

use eframe::egui;
use std::path::{Path, PathBuf};

const PICTURE: [&str; 5] = ["jpg", "jpeg", "png", "webp", "jfif"];
/// File names that mean "the front", best first.
const FRONT: [&str; 7] = ["cover", "folder", "front", "albumart", "albumartlarge", "album", "poster"];
/// Words that mark a scan or a screenshot rather than the front.
const NOT_FRONT: [&str; 20] = [
    "back", "rear", "cd", "disc", "disk", "inlay", "inside", "interior", "tray", "booklet", "spine", "matrix", "label",
    "obi", "insert", "sample", "screen", "proof", "thumb", "small",
];

/// Where a torrent's picture is.
pub enum State {
    /// The torrent has no picture.
    None,
    /// This file of it, read once it has downloaded.
    Waiting { file: usize, path: PathBuf, len: u64 },
    Loading,
    /// Shown small in the lists; `path` is the file, for the large banner in Details.
    Ready { tex: egui::TextureHandle, path: PathBuf },
}

/// The best picture among a torrent's files (paths relative to the torrent, sizes in
/// bytes): a front cover by name, else, for music, the biggest picture nearest the
/// top. `None` when there is nothing fit to show.
pub fn pick(files: &[(String, u64)], audio: bool) -> Option<usize> {
    files
        .iter()
        .enumerate()
        .filter_map(|(i, (path, len))| {
            let path = path.replace('\\', "/").to_lowercase();
            let (stem, ext) = path.rsplit('/').next()?.rsplit_once('.')?;
            if !PICTURE.contains(&ext) || !(1024..=25 << 20).contains(len) {
                return None;
            }
            let words: Vec<&str> = stem.split(|c: char| !c.is_ascii_alphanumeric()).filter(|w| !w.is_empty()).collect();
            if words.iter().any(|w| NOT_FRONT.iter().any(|n| w.starts_with(n))) {
                return None;
            }
            let score = match FRONT.iter().position(|f| *f == stem) {
                Some(rank) => 300 - rank as i64 * 10,
                None if words.iter().any(|w| FRONT.contains(w)) => 200,
                None if audio => 100,
                None => return None,
            };
            let depth = path.matches('/').count() as i64;
            Some(((score - depth * 5, *len), i))
        })
        .max()
        .map(|(_, i)| i)
}

/// Read a picture into an image for the screen, at most `max` pixels wide and high;
/// `None` when it is not one.
pub fn decode(path: &Path, max: u32) -> Option<egui::ColorImage> {
    let bytes = std::fs::read(path).ok()?;
    let img = image::load_from_memory(&bytes).ok()?.thumbnail(max, max).to_rgba8();
    Some(egui::ColorImage::from_rgba_unmultiplied([img.width() as usize, img.height() as usize], img.as_raw()))
}

/// Texture coordinates that fill a box of this shape with the picture, cutting off
/// what sticks out on the long side.
pub fn fill_uv(size: [usize; 2], frame: egui::Vec2) -> egui::Rect {
    let (pic, want) = (size[0] as f32 / size[1].max(1) as f32, frame.x / frame.y.max(1.0));
    let (u, v) = if pic > want { (want / pic, 1.0) } else { (1.0, pic / want) };
    egui::Rect::from_min_max(egui::pos2((1.0 - u) / 2.0, (1.0 - v) / 2.0), egui::pos2((1.0 + u) / 2.0, (1.0 + v) / 2.0))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(list: &[(&str, u64)]) -> Vec<(String, u64)> {
        list.iter().map(|(p, l)| (p.to_string(), *l)).collect()
    }

    #[test]
    fn the_front_cover_wins_over_scans() {
        let f = files(&[
            ("01 - Intro.flac", 30 << 20),
            ("Scans/Back.jpg", 900_000),
            ("Scans/CD1.jpg", 800_000),
            ("Scans/Front.jpg", 700_000),
            ("folder.jpg", 90_000),
        ]);
        assert_eq!(pick(&f, true), Some(4), "folder.jpg at the top beats Scans/Front.jpg");
        let f = files(&[("Scans/Back.jpg", 900_000), ("Scans/Front.jpg", 700_000), ("Scans/Inlay.jpg", 950_000)]);
        assert_eq!(pick(&f, true), Some(1));
        let f = files(&[("Artist - Album (2026) - Cover.JPG", 300_000), ("big-photo.png", 5 << 20)]);
        assert_eq!(pick(&f, true), Some(0));
    }

    #[test]
    fn music_takes_any_picture_films_only_a_named_one() {
        let f = files(&[("01.flac", 30 << 20), ("artwork/a.jpg", 50_000), ("b.png", 40_000)]);
        assert_eq!(pick(&f, true), Some(2), "nearest the top");
        let f = files(&[("Show.S01E01.mkv", 900 << 20), ("Screens/screen01.jpg", 200_000), ("b.png", 40_000)]);
        assert_eq!(pick(&f, false), None, "screenshots and unnamed pictures are not a film's cover");
        let f = files(&[("Film.2026.mkv", 4 << 30), ("poster.jpg", 200_000), ("Screens/screen01.jpg", 300_000)]);
        assert_eq!(pick(&f, false), Some(1));
    }

    #[test]
    fn junk_is_not_a_picture() {
        let f = files(&[("cover.jpg", 200), ("cover.txt", 5_000), ("folder.gif", 9_000), ("x.nfo", 3_000)]);
        assert_eq!(pick(&f, true), None, "too small, not a picture, a format not decoded");
        assert_eq!(pick(&[], true), None);
        let f = files(&[(r"Album\Scans\front.jpg", 70_000)]);
        assert_eq!(pick(&f, true), Some(0), "Windows separators");
    }

    #[test]
    fn fill_cuts_the_long_side() {
        let sq = fill_uv([600, 600], egui::vec2(172.0, 128.0));
        assert!((sq.min.x - 0.0).abs() < 1e-6 && (sq.height() - 128.0 / 172.0).abs() < 1e-5);
        let wide = fill_uv([1000, 500], egui::vec2(1.0, 1.0));
        assert!((wide.width() - 0.5).abs() < 1e-6 && (wide.min.x - 0.25).abs() < 1e-6 && wide.height() == 1.0);
        assert_eq!(fill_uv([300, 300], egui::vec2(1.0, 1.0)), egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)));
    }
}
