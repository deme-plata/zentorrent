//! Looking at a folder, and moving things in it — only after the user says yes.
//!
//! `scan` sums a folder up for the model: each sub-folder with media is one
//! entry (usually one album, film or season), with the tags of its first
//! track; loose files at the top are listed one by one. The model answers
//! with moves; `check` validates them (inside the folder, nothing a torrent
//! is still seeding, nothing overwritten) before the user sees a preview, and
//! `apply` performs them and keeps a journal so `undo` can put everything back.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::player::playlist::{kind_of, Kind};

const MAX_FILES: usize = 50_000;
const MAX_ENTRIES: usize = 400;

/// Embedded tags of one audio file (what genre sorting starts from).
fn tags(path: &Path) -> Value {
    use lofty::prelude::*;
    let Ok(f) = lofty::read_from_path(path) else { return Value::Null };
    let Some(t) = f.primary_tag().or_else(|| f.first_tag()) else { return Value::Null };
    let mut o = serde_json::Map::new();
    for (k, v) in [("artist", t.artist()), ("album", t.album()), ("genre", t.genre())] {
        if let Some(v) = v.filter(|v| !v.trim().is_empty()) {
            o.insert(k.into(), json!(v.trim()));
        }
    }
    if let Some(y) = t.year() {
        o.insert("year".into(), json!(y));
    }
    if o.is_empty() { Value::Null } else { Value::Object(o) }
}

fn rel(root: &Path, p: &Path) -> String {
    p.strip_prefix(root).unwrap_or(p).to_string_lossy().replace('\\', "/")
}

/// Every media file under `root` (not following links), grouped by its folder.
fn walk(root: &Path) -> Vec<(PathBuf, Vec<(PathBuf, u64)>)> {
    let mut groups: std::collections::BTreeMap<PathBuf, Vec<(PathBuf, u64)>> = Default::default();
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    let mut seen = 0;
    while let Some((dir, depth)) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            let p = e.path();
            if ft.is_dir() && depth < 6 {
                stack.push((p, depth + 1));
            } else if ft.is_file() && kind_of(&p.to_string_lossy()).is_some() {
                seen += 1;
                if seen > MAX_FILES {
                    break;
                }
                let len = e.metadata().map(|m| m.len()).unwrap_or(0);
                groups.entry(dir.clone()).or_default().push((p, len));
            }
        }
    }
    groups.into_iter().collect()
}

/// A summary of `root` for the model: folders with media, and loose files.
pub fn scan(root: &Path) -> Value {
    let groups = walk(root);
    let mut folders = Vec::new();
    let mut loose = Vec::new();
    let mut total = (0usize, 0u64);
    for (dir, mut files) in groups {
        files.sort();
        total.0 += files.len();
        total.1 += files.iter().map(|f| f.1).sum::<u64>();
        if dir == root {
            for (f, len) in files {
                let mut o = json!({ "file": rel(root, &f), "mb": len >> 20 });
                if kind_of(&f.to_string_lossy()) == Some(Kind::Audio) {
                    o["tags"] = tags(&f);
                }
                loose.push(o);
            }
            continue;
        }
        let audio = files.iter().filter(|f| kind_of(&f.0.to_string_lossy()) == Some(Kind::Audio)).count();
        let first_audio = files.iter().find(|f| kind_of(&f.0.to_string_lossy()) == Some(Kind::Audio));
        folders.push(json!({
            "folder": rel(root, &dir),
            "files": files.len(),
            "kind": if audio * 2 >= files.len() { "audio" } else { "video" },
            "mb": files.iter().map(|f| f.1).sum::<u64>() >> 20,
            "example": files.first().map(|f| f.0.file_name().unwrap_or_default().to_string_lossy().into_owned()),
            "tags": first_audio.map(|f| tags(&f.0)).unwrap_or(Value::Null),
        }));
    }
    let more = folders.len().saturating_sub(MAX_ENTRIES) + loose.len().saturating_sub(MAX_ENTRIES);
    folders.truncate(MAX_ENTRIES);
    loose.truncate(MAX_ENTRIES);
    json!({
        "root": root.to_string_lossy(),
        "media_files": total.0,
        "total_mb": total.1 >> 20,
        "folders": folders,
        "loose_files": loose,
        "not_shown": more,
    })
}

/// One move: `from` (a file or folder) goes INTO the folder `to`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Move {
    pub from: PathBuf,
    pub to: PathBuf,
}

impl Move {
    pub fn dest(&self) -> PathBuf {
        self.to.join(self.from.file_name().unwrap_or_default())
    }
}

/// Resolve a path the model wrote (relative to `root`, or absolute) and keep it inside `root`.
fn inside(root: &Path, p: &str) -> Result<PathBuf, String> {
    let p = p.trim().trim_matches('"').replace('\\', "/");
    let mut out = if Path::new(&p).is_absolute() { PathBuf::new() } else { root.to_path_buf() };
    for c in Path::new(&p).components() {
        use std::path::Component::*;
        match c {
            ParentDir => return Err(format!("\"{p}\": no '..' allowed")),
            CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    if !out.starts_with(root) {
        return Err(format!("\"{p}\" is outside {}", root.display()));
    }
    Ok(out)
}

/// Validate the model's moves. `busy` = folders/files torrents are still using.
/// Returns the good moves and one line per refused move.
pub fn check(root: &Path, raw: &[(String, String)], busy: &[PathBuf]) -> (Vec<Move>, Vec<String>) {
    let mut ok: Vec<Move> = Vec::new();
    let mut refused = Vec::new();
    for (from, to) in raw {
        let m = match (inside(root, from), inside(root, to)) {
            (Ok(f), Ok(t)) => Move { from: f, to: t },
            (Err(e), _) | (_, Err(e)) => {
                refused.push(e);
                continue;
            }
        };
        let why = if !m.from.exists() {
            Some("does not exist".to_string())
        } else if m.from == root || m.to.starts_with(&m.from) {
            Some("can't be moved into itself".into())
        } else if m.from.parent() == Some(m.to.as_path()) {
            Some("is already there".into())
        } else if m.dest().exists() || ok.iter().any(|o| o.dest() == m.dest()) {
            Some("something with that name is already in the destination".into())
        } else if busy.iter().any(|b| b.starts_with(&m.from) || m.from.starts_with(b)) {
            Some("ZenTorrent is still seeding it — remove the torrent first, or it can't find its files".into())
        } else {
            None
        };
        match why {
            Some(w) => refused.push(format!("{}: {w}", rel(root, &m.from))),
            None => ok.push(m),
        }
    }
    (ok, refused)
}

#[derive(Serialize, Deserialize, Default)]
struct Journal {
    /// (where it went, where it was), in the order they were done.
    done: Vec<(PathBuf, PathBuf)>,
}

fn journal_path() -> PathBuf {
    crate::seed::data_dir().join("organize-undo.json")
}

/// Perform the moves. Returns (moved, problems). The journal replaces the last one.
pub fn apply(moves: &[Move]) -> (usize, Vec<String>) {
    apply_at(moves, &journal_path())
}

fn apply_at(moves: &[Move], journal: &Path) -> (usize, Vec<String>) {
    let mut j = Journal::default();
    let mut problems = Vec::new();
    for m in moves {
        let dest = m.dest();
        let r = std::fs::create_dir_all(&m.to).and_then(|_| {
            if dest.exists() {
                Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, "already exists"))
            } else {
                std::fs::rename(&m.from, &dest)
            }
        });
        match r {
            Ok(()) => j.done.push((dest, m.from.clone())),
            Err(e) => problems.push(format!("{}: {e}", m.from.display())),
        }
    }
    let _ = std::fs::write(journal, serde_json::to_vec_pretty(&j).unwrap_or_default());
    (j.done.len(), problems)
}

pub fn can_undo() -> bool {
    can_undo_at(&journal_path())
}

fn can_undo_at(journal: &Path) -> bool {
    std::fs::read(journal).ok().and_then(|b| serde_json::from_slice::<Journal>(&b).ok()).is_some_and(|j| !j.done.is_empty())
}

/// Put the last organize back, newest move first. Returns (restored, problems).
pub fn undo() -> (usize, Vec<String>) {
    undo_at(&journal_path())
}

fn undo_at(journal: &Path) -> (usize, Vec<String>) {
    let Some(j) = std::fs::read(journal).ok().and_then(|b| serde_json::from_slice::<Journal>(&b).ok()) else {
        return (0, vec!["nothing to undo".into()]);
    };
    let (mut n, mut problems) = (0, Vec::new());
    for (now, was) in j.done.iter().rev() {
        let r = if was.exists() {
            Err(std::io::Error::new(std::io::ErrorKind::AlreadyExists, "something new is in the old place"))
        } else {
            was.parent().map(std::fs::create_dir_all).unwrap_or(Ok(())).and_then(|_| std::fs::rename(now, was))
        };
        match r {
            Ok(()) => n += 1,
            Err(e) => problems.push(format!("{}: {e}", now.display())),
        }
    }
    let _ = std::fs::remove_file(journal);
    (n, problems)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tree() -> PathBuf {
        let root = std::env::temp_dir().join(format!("zt-organize-{}-{}", std::process::id(), rand_suffix()));
        for f in ["Artist A - Album (2020)/01 - One.mp3", "Artist A - Album (2020)/02 - Two.mp3", "Film (2021)/film.mkv", "loose.flac", "notes.txt"] {
            let p = root.join(f);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(&p, b"x").unwrap();
        }
        root
    }
    fn rand_suffix() -> u64 {
        std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos() as u64 % 1_000_000_007
    }

    #[test]
    fn scan_groups_folders_and_lists_loose_files() {
        let root = tree();
        let s = scan(&root);
        assert_eq!(s["media_files"], 4);
        let names: Vec<&str> = s["folders"].as_array().unwrap().iter().map(|f| f["folder"].as_str().unwrap()).collect();
        assert_eq!(names, ["Artist A - Album (2020)", "Film (2021)"]);
        assert_eq!(s["folders"][0]["kind"], "audio");
        assert_eq!(s["folders"][1]["kind"], "video");
        assert_eq!(s["loose_files"][0]["file"], "loose.flac");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn moves_are_checked_applied_and_undone() {
        let root = tree();
        let seeding = vec![root.join("Film (2021)")];
        let raw = vec![
            ("Artist A - Album (2020)".to_string(), "Music/Trance".to_string()),
            ("loose.flac".into(), "Music/Ambient".into()),
            ("Film (2021)".into(), "Movies".into()),
            ("../etc".into(), "Music".into()),
            ("missing.mp3".into(), "Music".into()),
        ];
        let (ok, refused) = check(&root, &raw, &seeding);
        assert_eq!(ok.len(), 2, "{refused:?}");
        assert_eq!(refused.len(), 3);
        assert!(refused.iter().any(|r| r.contains("seeding")));
        let journal = root.join("journal.json");
        let (n, problems) = apply_at(&ok, &journal);
        assert_eq!((n, problems.len()), (2, 0));
        assert!(root.join("Music/Trance/Artist A - Album (2020)/01 - One.mp3").exists());
        assert!(root.join("Music/Ambient/loose.flac").exists());
        assert!(can_undo_at(&journal));
        let (n, problems) = undo_at(&journal);
        assert_eq!((n, problems.len()), (2, 0));
        assert!(root.join("Artist A - Album (2020)/02 - Two.mp3").exists());
        assert!(root.join("loose.flac").exists());
        std::fs::remove_dir_all(root).unwrap();
    }
}
