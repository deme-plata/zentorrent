//! Bookmarks: where you were in a film, an episode or a long mix (a podcast, an audiobook,
//! a DJ set: more than ten minutes), so playing it again carries on there. Ordinary songs
//! get none: an album should start each song from the top. Kept in the store (key
//! "bookmarks"), one per file: the torrent and the file's path in it (or, for a file
//! outside any torrent, its full path).

use std::collections::HashMap;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// Audio shorter than this gets no bookmark.
pub const MIN_AUDIO_SECS: f64 = 600.0;
/// Nothing is kept in the first half minute, and the last minute counts as finished.
const HEAD_SECS: f64 = 30.0;
const TAIL_SECS: f64 = 60.0;
/// The oldest go once there are this many.
const MAX_MARKS: usize = 1000;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Mark {
    pub pos: f64,
    pub duration: f64,
    /// Unix seconds it was last noted.
    pub when: u64,
}

#[derive(Default, Serialize, Deserialize)]
pub struct Bookmarks {
    at: HashMap<String, Mark>,
    #[serde(skip)]
    dirty: bool,
    #[serde(skip)]
    saved: Option<Instant>,
}

/// The bookmark's name for a file of a torrent (`hash` empty: a file outside any torrent).
pub fn key(hash: &str, path: &str) -> String {
    format!("{hash}|{path}")
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

impl Bookmarks {
    pub fn load() -> Self {
        crate::db::load("bookmarks", None).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    /// Write the bookmarks to the store if any changed.
    pub fn save(&mut self) {
        if self.dirty {
            let _ = crate::db::save("bookmarks", &serde_json::to_vec(self).unwrap_or_default());
            self.dirty = false;
        }
        self.saved = Some(Instant::now());
    }

    /// While playing: saved at most every 15 seconds.
    pub fn maybe_save(&mut self) {
        if self.dirty && self.saved.is_none_or(|t| t.elapsed() > Duration::from_secs(15)) {
            self.save();
        }
    }

    /// Note that `key` is at `pos` of `duration` seconds. Only films/episodes (`video`) and
    /// long audio keep one; barely started or finished, the file has none.
    pub fn note(&mut self, key: &str, pos: f64, duration: f64, video: bool) {
        if duration <= 0.0 || !(video || duration >= MIN_AUDIO_SECS) {
            return;
        }
        if pos < HEAD_SECS || pos > duration - TAIL_SECS {
            if self.at.remove(key).is_some() {
                self.dirty = true;
            }
            return;
        }
        let m = self.at.entry(key.to_string()).or_insert(Mark { pos: -1.0, duration, when: 0 });
        if (m.pos - pos).abs() >= 1.0 {
            *m = Mark { pos, duration, when: now() };
            self.dirty = true;
        }
        if self.at.len() > MAX_MARKS {
            if let Some(oldest) = self.at.iter().min_by_key(|(_, m)| m.when).map(|(k, _)| k.clone()) {
                self.at.remove(&oldest);
            }
        }
    }

    /// Where `key` was left, if it has a bookmark.
    pub fn get(&self, key: &str) -> Option<f64> {
        self.at.get(key).map(|m| m.pos)
    }

    /// "Start over": the file's bookmark goes.
    pub fn forget(&mut self, key: &str) {
        if self.at.remove(key).is_some() {
            self.dirty = true;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn films_and_long_mixes_keep_their_place_songs_do_not() {
        let mut b = Bookmarks::default();
        b.note("h|film.mkv", 1421.0, 5400.0, true);
        assert_eq!(b.get("h|film.mkv"), Some(1421.0));
        b.note("h|song.flac", 120.0, 240.0, false);
        assert_eq!(b.get("h|song.flac"), None, "a 4-minute song starts from the top");
        b.note("h|asot-1298.mp3", 3600.0, 7200.0, false);
        assert_eq!(b.get("h|asot-1298.mp3"), Some(3600.0), "a two-hour mix keeps its place");
        b.note("h|ep.mkv", 95.0, 120.0, true);
        assert_eq!(b.get("h|ep.mkv"), None, "short video: past its last minute at once");
    }

    #[test]
    fn the_start_and_the_end_clear_it() {
        let mut b = Bookmarks::default();
        b.note("k", 600.0, 3000.0, true);
        b.note("k", 10.0, 3000.0, true);
        assert_eq!(b.get("k"), None, "watched from the start again");
        b.note("k", 600.0, 3000.0, true);
        b.note("k", 2970.0, 3000.0, true);
        assert_eq!(b.get("k"), None, "finished");
        b.note("k", 600.0, 3000.0, true);
        b.forget("k");
        assert_eq!(b.get("k"), None, "Start over");
    }

    #[test]
    fn it_survives_a_round_trip_and_only_writes_when_changed() {
        let mut b = Bookmarks::default();
        b.note("k", 600.0, 3000.0, true);
        assert!(b.dirty);
        let back: Bookmarks = serde_json::from_slice(&serde_json::to_vec(&b).unwrap()).unwrap();
        assert_eq!(back.get("k"), Some(600.0));
        b.dirty = false;
        b.note("k", 600.4, 3000.0, true);
        assert!(!b.dirty, "under a second moved: nothing to write");
        assert_eq!(key("abc", "Show/S01E01.mkv"), "abc|Show/S01E01.mkv");
    }
}
