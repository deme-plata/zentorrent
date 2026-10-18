//! ZenTorrent's own data in one flux-db store, `<data dir>/zentorrent-db/`: settings,
//! the ratio ledger (with ratings), RSS feeds and history, Flux MoE and player settings,
//! the OMDb cache. Each used to be a JSON file: the first time a key is asked for and the
//! store does not have it yet, that file is imported and renamed `<file>.migrated` (kept
//! as a backup). The values are the same JSON as before; flux-db holds them.
//!
//! Kept as files on purpose: the VPN settings and key and `mcp.json` (other ZenTorrent
//! processes read them while the app runs), the organize undo journals (they belong next
//! to the moved files), poster images, and the torrent engine's own list.
//!
//! One process owns the store: the first ZenTorrent to open it holds `db.lock` (an OS file
//! lock, released if the process dies). Another one (a second start handing a .torrent
//! over, `--ask` beside the app) leaves the store alone: it reads nothing and saves nothing.
//! The window's process waits for it (`claim`): right after an update's restart, or a Quit
//! and start again, the old ZenTorrent may still be closing and holding it.

use std::fs::File;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

pub struct Store {
    db: flux_db::Database,
    /// Held for the life of the process: the store is ours.
    _lock: File,
}

impl Store {
    /// Open (or create) the store in `dir`; None when another process holds it.
    pub fn open(dir: &Path) -> Option<Store> {
        std::fs::create_dir_all(dir).ok()?;
        let lock = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(dir.join("db.lock")).ok()?;
        if lock.try_lock().is_err() {
            return None;
        }
        let path = dir.join("zentorrent-db");
        let fresh = !path.exists();
        // A small write-ahead log: it is turned into a compacted file every 8 MB.
        let db = flux_db::Database::open(&path).ok()?.with_max_wal_bytes(8 << 20);
        #[cfg(unix)]
        if fresh {
            // RSS feed links carry tracker passkeys: as private as feeds.json was (0600).
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o700));
        }
        let _ = fresh;
        Some(Store { db, _lock: lock })
    }

    /// The bytes saved under `key`; if there are none yet, the old file `legacy` is
    /// imported (and renamed `<file>.migrated`).
    pub fn load(&self, key: &str, legacy: Option<&Path>) -> Option<Vec<u8>> {
        if let Ok(Some(v)) = self.db.get(key.as_bytes()) {
            return Some(v);
        }
        let file = legacy?;
        let bytes = std::fs::read(file).ok()?;
        if self.save(key, &bytes).is_ok() {
            let mut kept = file.as_os_str().to_owned();
            kept.push(".migrated");
            let _ = std::fs::rename(file, PathBuf::from(kept));
        }
        Some(bytes)
    }

    /// Save `bytes` under `key`, on disk before this returns.
    pub fn save(&self, key: &str, bytes: &[u8]) -> Result<(), String> {
        self.db.put(key.as_bytes(), bytes)?;
        self.db.sync_wal()
    }
}

/// `Store::open`, trying again for up to `wait` while another process holds the store.
fn open_waiting(dir: &Path, wait: Duration) -> Option<Store> {
    let until = Instant::now() + wait;
    loop {
        if let Some(s) = Store::open(dir) {
            return Some(s);
        }
        if Instant::now() >= until {
            return None;
        }
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Whether this process has the store. Decided once, at the first use: a process that did
/// not get it never takes it later, or it would save the empty lists it started with over
/// the real ones.
static STORE: OnceLock<Option<Store>> = OnceLock::new();

fn store() -> Option<&'static Store> {
    STORE.get_or_init(|| Store::open(&crate::seed::data_dir())).as_ref()
}

/// For the window's process, before anything is loaded: open the store, waiting up to
/// `wait` for a ZenTorrent that is still closing to let go of it. True if it is ours.
pub fn claim(wait: Duration) -> bool {
    STORE.get_or_init(|| open_waiting(&crate::seed::data_dir(), wait)).is_some()
}

/// Whether this process has the saved data (false: it shows none of it and saves nothing).
pub fn owned() -> bool {
    STORE.get().is_some_and(|s| s.is_some())
}

/// What was saved under `key` (importing the old JSON file `legacy` the first time).
/// A process that does not own the store gets nothing.
pub fn load(key: &str, legacy: Option<&Path>) -> Option<Vec<u8>> {
    store()?.load(key, legacy)
}

/// Save under `key`. A process that does not own the store cannot, and says so.
pub fn save(key: &str, bytes: &[u8]) -> Result<(), String> {
    store().ok_or_else(|| "another ZenTorrent has its data open; not saved".to_string())?.save(key, bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("zt-db-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn an_old_json_file_is_imported_once_and_kept_as_a_backup() {
        let d = dir("import");
        std::fs::create_dir_all(&d).unwrap();
        let old = d.join("ratio.json");
        std::fs::write(&old, br#"{"entries":{"ab":{"rating":8}}}"#).unwrap();
        {
            let s = Store::open(&d).unwrap();
            assert_eq!(s.load("ledger", Some(&old)).unwrap(), br#"{"entries":{"ab":{"rating":8}}}"#);
            assert!(!old.exists() && d.join("ratio.json.migrated").is_file(), "renamed, kept");
            s.save("ledger", br#"{"entries":{"ab":{"rating":9}}}"#).unwrap();
        }
        // Next start: from the store, the backup is not read again.
        std::fs::write(d.join("ratio.json.migrated"), b"stale").unwrap();
        let s = Store::open(&d).unwrap();
        assert_eq!(s.load("ledger", Some(&old)).unwrap(), br#"{"entries":{"ab":{"rating":9}}}"#);
        assert_eq!(s.load("missing", None), None);
        drop(s);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn one_owner_at_a_time() {
        let d = dir("lock");
        let first = Store::open(&d).unwrap();
        first.save("prefs", b"{}").unwrap();
        assert!(Store::open(&d).is_none(), "a second opener stays out");
        drop(first);
        let again = Store::open(&d).expect("free once the owner is gone");
        assert_eq!(again.load("prefs", None).unwrap(), b"{}");
        drop(again);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_restart_waits_for_the_old_one_to_close() {
        let d = dir("wait");
        let old = Store::open(&d).unwrap();
        old.save("feeds", b"[1]").unwrap();
        assert!(open_waiting(&d, Duration::from_millis(200)).is_none(), "still held: no store");
        // The old ZenTorrent closes a moment after the new one started.
        let closing = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(400));
            drop(old);
        });
        let new = open_waiting(&d, Duration::from_secs(10)).expect("taken once the old one is gone");
        assert_eq!(new.load("feeds", None).unwrap(), b"[1]", "with the data, not empty");
        closing.join().unwrap();
        drop(new);
        let _ = std::fs::remove_dir_all(&d);
    }
}
