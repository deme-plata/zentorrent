//! `make_mix`: a playlist ZenTorrent picks itself from a description ("a chill mix for the
//! evening", "workout", "trance for tonight"). A small model could not do it by hand: on
//! 2026-10-18 qwen3:4b, with 1231 files in album folders, had no file ids to give
//! make_playlist, wrote paths that did not exist, then talked in circles.
//!
//! Every audio file in the download folder is known by the words of its path and its tags
//! (genre, artist, album, title). The description's own words count most; mood words
//! (chill, workout, party, focus, sleep…) stand for the genres that fit them. The mix keeps
//! to a few tracks per album, so it is a mix and not one album.

use std::path::{Path, PathBuf};

use crate::player::playlist::{kind_of, Kind};
use crate::search::tokens;

/// One audio file: the words it is known by, and its genre tag's words.
pub struct Track {
    pub path: PathBuf,
    words: Vec<String>,
    genre: Vec<String>,
}

/// Mood words and the genres (and path words) that fit them. English and Danish.
const MOODS: &[(&[&str], &[&str])] = &[
    (
        &["chill", "chillout", "relax", "relaxing", "calm", "mellow", "evening", "lounge", "rolig", "afslappende", "hygge", "hyggelig"],
        &["ambient", "chillout", "chill", "downtempo", "lounge", "lofi", "jazz", "acoustic", "soul", "trip", "chillwave", "deep"],
    ),
    (
        &["workout", "gym", "run", "running", "training", "træning", "traening", "energy", "energetic", "pump", "power", "cardio"],
        &["trance", "techno", "edm", "house", "dnb", "drum", "hardstyle", "dance", "electro", "rock", "metal", "uplifting", "psytrance", "bass"],
    ),
    (&["party", "fest", "club", "dance", "danse"], &["house", "dance", "edm", "disco", "pop", "electro", "club", "funk"]),
    (&["focus", "study", "concentrate", "koncentration", "reading", "læse"], &["ambient", "classical", "instrumental", "piano", "lofi", "minimal", "soundtrack"]),
    (&["sleep", "sleepy", "sove", "night", "nat"], &["ambient", "piano", "classical", "sleep"]),
    (&["happy", "upbeat", "summer", "sommer", "sunny", "glad"], &["pop", "house", "dance", "disco", "funk", "summer"]),
    (&["sad", "melancholy", "melancholic", "trist"], &["acoustic", "piano", "ballad", "indie"]),
];

/// Words that say nothing about which music.
const FILLER: &[&str] = &[
    "mix", "playlist", "playliste", "music", "musik", "songs", "sange", "tracks", "track", "numre", "some", "for", "the", "a", "an",
    "my", "min", "mine", "of", "and", "og", "with", "med", "to", "til", "me", "mig", "please", "play", "spil", "make", "lav", "en", "et",
    "good", "best", "new", "nogle", "noget", "from", "fra", "i", "in", "high", "tonight", "today", "i", "aften", "dag", "list",
];

/// The description's own words, and the genres its moods stand for.
pub fn wanted(describe: &str) -> (Vec<String>, Vec<String>) {
    let (mut own, mut mood) = (Vec::new(), Vec::new());
    for w in tokens(describe) {
        if let Some((_, genres)) = MOODS.iter().find(|(keys, _)| keys.contains(&w.as_str())) {
            mood.extend(genres.iter().map(|g| g.to_string()));
        } else if !FILLER.contains(&w.as_str()) && w.chars().count() > 1 {
            own.push(w);
        }
    }
    mood.sort();
    mood.dedup();
    (own, mood)
}

/// Every audio file under `root`; the tags of the first `read_tags` are read too.
pub fn index(root: &Path, read_tags: usize) -> Vec<Track> {
    let mut out = Vec::new();
    for (_, files) in super::organize::walk(root) {
        for (p, _) in files {
            if kind_of(&p.to_string_lossy()) != Some(Kind::Audio) {
                continue;
            }
            let rel = p.strip_prefix(root).unwrap_or(&p).to_string_lossy().into_owned();
            let mut words = tokens(&rel);
            let mut genre = Vec::new();
            if out.len() < read_tags {
                if let Some((g, rest)) = tags(&p) {
                    genre = tokens(&g);
                    words.extend(tokens(&rest));
                }
            }
            out.push(Track { path: p, words, genre });
        }
    }
    out
}

/// (genre, "artist album title") from the file's tags.
fn tags(p: &Path) -> Option<(String, String)> {
    use lofty::prelude::*;
    let f = lofty::read_from_path(p).ok()?;
    let t = f.primary_tag().or_else(|| f.first_tag())?;
    let s = |v: Option<std::borrow::Cow<'_, str>>| v.map(|v| v.to_string()).unwrap_or_default();
    Some((s(t.genre()), format!("{} {} {}", s(t.artist()), s(t.album()), s(t.title()))))
}

/// Mood words that also mean other things in a title ("Trippie Redd", "Black Is the Soul",
/// "The Deep"): they count only in a genre tag.
const VAGUE: &[&str] = &["deep", "soul", "trip", "bass", "drum", "sleep", "summer", "club", "hard", "uplifting", "chill"];

fn hit(w: &str, toks: &[String]) -> bool {
    toks.iter().any(|t| t == w || (w.chars().count() >= 3 && t.starts_with(w)))
}

/// How well a track fits: its genre counts most, then its path and tags.
fn score(t: &Track, own: &[String], mood: &[String]) -> f64 {
    let mut s = 0.0;
    for w in own {
        s += if hit(w, &t.genre) { 4.0 } else if hit(w, &t.words) { 2.0 } else { 0.0 };
    }
    // A mood's genres: in the genre tag, or as a whole word of the name (not a prefix).
    for w in mood {
        s += if hit(w, &t.genre) {
            3.0
        } else if !VAGUE.contains(&w.as_str()) && t.words.iter().any(|x| x == w) {
            1.0
        } else {
            0.0
        };
    }
    s
}

/// A small, seeded random source (a mix should not be the same every time).
struct Rng(u64);
impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// Up to `n` tracks for the description (indexes into `tracks`, in play order), and how
/// many fitted it at all. Nothing fits: empty — unless it named nothing, then everything
/// counts (a varied mix).
pub fn pick(tracks: &[Track], own: &[String], mood: &[String], n: usize, seed: u64) -> (Vec<usize>, usize) {
    let mut rng = Rng(seed | 1);
    let any = own.is_empty() && mood.is_empty();
    let mut fits: Vec<(usize, f64)> = tracks
        .iter()
        .enumerate()
        .map(|(i, t)| (i, score(t, own, mood)))
        .filter(|&(_, s)| any || s > 0.0)
        .map(|(i, s)| (i, s + rng.unit() * 1.5))
        .collect();
    let matching = fits.len();
    fits.sort_by(|a, b| b.1.total_cmp(&a.1));
    // A few tracks per album (folder), so it is a mix; fewer albums than that: more each.
    let folder = |i: usize| tracks[i].path.parent().map(Path::to_path_buf);
    let mut per: std::collections::HashMap<Option<PathBuf>, usize> = Default::default();
    let mut out = Vec::new();
    for cap in [2usize.max(n / 8), usize::MAX] {
        for &(i, _) in &fits {
            if out.len() >= n {
                break;
            }
            if out.contains(&i) {
                continue;
            }
            let c = per.entry(folder(i)).or_default();
            if *c < cap {
                *c += 1;
                out.push(i);
            }
        }
    }
    // Shuffled, so one album's tracks are not next to each other.
    for k in (1..out.len()).rev() {
        let j = (rng.next() % (k as u64 + 1)) as usize;
        out.swap(k, j);
    }
    (out, matching)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lib(files: &[&str]) -> (PathBuf, Vec<Track>) {
        let d = std::env::temp_dir().join(format!("zt-mix-{}-{}", std::process::id(), files.len()));
        let _ = std::fs::remove_dir_all(&d);
        for f in files {
            let p = d.join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, b"not really audio").unwrap();
        }
        let t = index(&d, 100);
        (d, t)
    }

    #[test]
    fn the_description_splits_into_words_and_moods() {
        let (own, mood) = wanted("A chill mix for the evening");
        assert!(own.is_empty(), "{own:?}");
        assert!(mood.contains(&"ambient".to_string()) && mood.contains(&"downtempo".to_string()));
        let (own, mood) = wanted("workout playlist with high-energy tracks");
        assert!(own.is_empty() && mood.contains(&"trance".to_string()), "{own:?}");
        let (own, _) = wanted("Armin trance til i aften");
        assert_eq!(own, ["armin", "trance"]);
    }

    #[test]
    fn a_mix_fits_the_mood_and_spreads_over_albums() {
        let mut files: Vec<String> = Vec::new();
        for a in 0..6 {
            for t in 0..10 {
                files.push(format!("Trance/Album {a}/{t:02} Uplifting {a}-{t}.mp3"));
            }
        }
        files.extend((0..10).map(|t| format!("Jazz Night/{t:02} Blue {t}.flac")));
        files.extend((0..5).map(|t| format!("Downtempo Sessions/{t:02} Calm {t}.mp3")));
        files.push("readme.txt".into());
        let refs: Vec<&str> = files.iter().map(String::as_str).collect();
        let (d, tracks) = lib(&refs);
        assert_eq!(tracks.len(), 75, "audio only");

        let (own, mood) = wanted("workout");
        let (got, matching) = pick(&tracks, &own, &mood, 20, 42);
        assert_eq!((got.len(), matching), (20, 60));
        assert!(got.iter().all(|&i| tracks[i].path.to_string_lossy().contains("Trance")));
        let albums: std::collections::HashSet<_> = got.iter().map(|&i| tracks[i].path.parent().unwrap().to_path_buf()).collect();
        assert_eq!(albums.len(), 6, "all six albums, not one");

        let (own, mood) = wanted("chill evening");
        let (got, matching) = pick(&tracks, &own, &mood, 25, 7);
        assert_eq!(matching, 15, "jazz + downtempo");
        assert_eq!(got.len(), 15, "more than there is: all of it");

        let (got, matching) = pick(&tracks, &["polka".into()], &[], 10, 1);
        assert!(got.is_empty() && matching == 0, "named something that is not there");
        // Words that mean other things in a title do not make a chill track.
        let (_, odd) = lib(&["Rap/034 - Trippie Redd - Deep Soul.mp3"]);
        assert_eq!(pick(&odd, &[], &wanted("chill evening").1, 5, 1).1, 0);
        let (got, _) = pick(&tracks, &[], &[], 10, 1);
        assert_eq!(got.len(), 10, "named nothing: a varied mix of everything");
        let _ = std::fs::remove_dir_all(&d);
    }
}
