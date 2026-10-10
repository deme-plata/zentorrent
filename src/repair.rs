//! Rebuilding a lost torrent list.
//!
//! The engine keeps its list of torrents in `<session folder>/session.json`,
//! and each torrent's metadata in `<info hash>.torrent` next to it. When
//! `session.json` is empty or unreadable — ZenTorrent ≤ 0.9.0 could leave it
//! empty when it closed mid-save — the engine refuses to start. Instead, the
//! damaged file is set aside and the list is rebuilt from the `.torrent` files:
//! each torrent goes back to the folder its files are found in on disk.

use std::path::{Path, PathBuf};

use serde_json::{json, Map, Value};

/// Repair `<dir>/session.json` if it is damaged. `saves` = folders torrents may
/// have been saved to (the current "Save to" first). Returns what was done.
pub fn session(dir: &Path, saves: &[PathBuf]) -> Option<String> {
    let db = dir.join("session.json");
    let bytes = std::fs::read(&db).ok()?;
    if serde_json::from_slice::<Value>(&bytes).is_ok_and(|v| v["torrents"].is_object()) {
        return None; // healthy
    }
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let aside = dir.join(format!("session.json.damaged-{stamp}"));
    if let Err(e) = std::fs::rename(&db, &aside) {
        return Some(format!("the torrent list {} is damaged and could not be set aside: {e}", db.display()));
    }
    let mut torrents = Map::new();
    let mut found = 0usize;
    for t in plan(dir, saves) {
        found += t.found as usize;
        torrents.insert(torrents.len().to_string(), t.entry);
    }
    let n = torrents.len();
    let text = serde_json::to_vec(&json!({ "torrents": torrents })).unwrap_or_default();
    let tmp = dir.join("session.json.tmp");
    if let Err(e) = std::fs::write(&tmp, &text).and_then(|_| std::fs::rename(&tmp, &db)) {
        return Some(format!("could not rebuild the torrent list: {e}"));
    }
    Some(format!(
        "The saved torrent list was damaged, so it was rebuilt from the {n} saved torrents ({found} found on disk, \
         {} start from scratch). The damaged file is kept as {}.",
        n - found,
        aside.display()
    ))
}

/// One torrent of a rebuilt list.
pub struct Planned {
    pub name: String,
    pub folder: PathBuf,
    /// Its files were found there (else it starts from scratch in `folder`).
    pub found: bool,
    entry: Value,
}

/// What a rebuild would do, without writing anything (`--repair-session`).
pub fn plan(dir: &Path, saves: &[PathBuf]) -> Vec<Planned> {
    // Torrents were saved to "Save to" or a folder in it (it is not remembered between runs).
    let mut bases: Vec<PathBuf> = Vec::new();
    for s in saves {
        bases.push(s.clone());
        if let Ok(rd) = std::fs::read_dir(s) {
            let mut subs: Vec<PathBuf> = rd.flatten().filter(|e| e.file_type().is_ok_and(|t| t.is_dir())).map(|e| e.path()).collect();
            subs.sort();
            bases.extend(subs);
        }
    }
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| rd.flatten().map(|e| e.path()).filter(|p| p.extension().is_some_and(|x| x == "torrent")).collect())
        .unwrap_or_default();
    entries.sort();
    entries.iter().filter_map(|p| rebuild_one(p, &bases)).collect()
}

fn rebuild_one(path: &Path, saves: &[PathBuf]) -> Option<Planned> {
    let bytes = std::fs::read(path).ok()?;
    let t = librqbit::torrent_from_bytes(&bytes).ok()?;
    let hash = t.info_hash.as_string();
    let info = t.info.data.validate().ok()?;
    let name = info.name().map(|n| n.to_string());
    let files: Vec<PathBuf> = info.iter_file_details().map(|f| f.filename.to_pathbuf()).collect();
    let first = files.first()?;
    // Where the files are: a torrent's own folder (0.8.2+), or loose in the save folder (older).
    let candidates: Vec<PathBuf> = saves
        .iter()
        .flat_map(|s| [crate::torrent_folder(s, files.len(), name.as_deref()), s.clone()])
        .collect();
    let hit = candidates.iter().find(|c| c.join(first).exists()).cloned();
    let folder = hit.clone().unwrap_or_else(|| candidates.first().cloned().unwrap_or_default());
    Some(Planned {
        name: name.unwrap_or_else(|| hash.clone()),
        entry: json!({ "info_hash": hash, "trackers": [], "output_folder": folder, "only_files": null, "is_paused": false }),
        folder,
        found: hit.is_some(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn benc(v: &Value) -> Vec<u8> {
        match v {
            Value::Number(n) => format!("i{}e", n).into_bytes(),
            Value::String(s) => [format!("{}:", s.len()).into_bytes(), s.as_bytes().to_vec()].concat(),
            Value::Array(a) => [b"l".to_vec(), a.iter().flat_map(benc).collect(), b"e".to_vec()].concat(),
            Value::Object(o) => {
                let mut keys: Vec<&String> = o.keys().collect();
                keys.sort();
                [b"d".to_vec(), keys.iter().flat_map(|k| [benc(&json!(k)), benc(&o[*k])].concat()).collect(), b"e".to_vec()].concat()
            }
            _ => unreachable!(),
        }
    }

    #[test]
    fn an_empty_list_is_rebuilt_from_the_torrent_files() {
        let root = std::env::temp_dir().join(format!("zt-repair-{}", std::process::id()));
        let (dir, save) = (root.join("session"), root.join("Downloads"));
        std::fs::create_dir_all(&dir).unwrap();
        // An album already downloaded into its own folder, and a single file not downloaded yet.
        let album = json!({"announce": "http://t/a", "info": {"name": "Artist - Album", "piece length": 16384, "pieces": "01234567890123456789",
            "files": [{"length": 1, "path": ["01.flac"]}, {"length": 1, "path": ["02.flac"]}]}});
        let single = json!({"announce": "http://t/a", "info": {"name": "film.mkv", "piece length": 16384, "pieces": "01234567890123456789", "length": 5}});
        for (n, t) in [("a", &album), ("b", &single)] {
            std::fs::write(dir.join(format!("{n}.torrent")), benc(t)).unwrap();
        }
        std::fs::create_dir_all(save.join("Artist - Album")).unwrap();
        std::fs::write(save.join("Artist - Album/01.flac"), b"x").unwrap();
        std::fs::write(dir.join("session.json"), b"").unwrap();

        let msg = session(&dir, &[save.clone()]).expect("repaired");
        assert!(msg.contains("2 saved torrents") && msg.contains("1 found"), "{msg}");
        let db: Value = serde_json::from_slice(&std::fs::read(dir.join("session.json")).unwrap()).unwrap();
        let folders: Vec<&str> = db["torrents"].as_object().unwrap().values().map(|t| t["output_folder"].as_str().unwrap()).collect();
        assert!(folders.contains(&save.join("Artist - Album").to_str().unwrap()));
        assert!(folders.contains(&save.to_str().unwrap()), "a single file goes in the save folder");
        assert!(std::fs::read_dir(&dir).unwrap().flatten().any(|e| e.file_name().to_string_lossy().starts_with("session.json.damaged-")));
        assert_eq!(session(&dir, &[save]), None, "a healthy list is left alone");
        std::fs::remove_dir_all(root).unwrap();
    }
}
