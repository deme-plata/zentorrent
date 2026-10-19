//! The torrent engine without the window: what the desktop app and the
//! headless `zentorrent serve` share — opening the session (damaged lists
//! repaired, VPN kill switch, incoming listener), restoring the torrents, and
//! adding one (through the VPN when it is on, into its own folder when it has
//! several files).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use librqbit::{AddTorrent, AddTorrentOptions, AddTorrentResponse, Session, SessionOptions};

use crate::{listen_port, listener, repair, rss, seed, torrent_folder, vpn, ManagedTorrentHandle, Source, Transfer};

pub struct Opened {
    pub session: Option<Arc<Session>>,
    /// Why there is no session (VPN kill switch, engine error).
    pub error: Option<String>,
    pub vpn: vpn::Vpn,
    /// What the repair of a damaged torrent list did, for the user.
    pub notes: Vec<String>,
}

/// Start the engine for `folder` ("Save to").
pub fn open(rt: &tokio::runtime::Runtime, folder: &Path) -> Opened {
    // A torrent list damaged by an older version (empty session.json) is rebuilt
    // from the saved .torrent files instead of stopping the engine.
    let saves: Vec<PathBuf> = std::iter::once(folder.to_path_buf()).chain(dirs::download_dir()).collect();
    let notes: Vec<String> = ["session", "session-vpn"].iter().filter_map(|s| repair::session(&seed::data_dir().join(s), &saves)).collect();

    // The VPN comes first: when it is on, the engine may start only once the
    // tunnel is up (kill switch), and with every non-tunnel path switched off.
    let vpn = vpn::Vpn::start(rt, &vpn::VpnSettings::load());
    let (session, error) = start(rt, folder, &vpn);
    Opened { session, error, vpn, notes }
}

/// The engine for this VPN state: none while the kill switch holds (with why).
pub fn start(rt: &tokio::runtime::Runtime, folder: &Path, vpn: &vpn::Vpn) -> (Option<Arc<Session>>, Option<String>) {
    let engine_opts = |listen: Option<librqbit::ListenerOptions>| SessionOptions {
        // Persistence: librqbit remembers every torrent (and where it got
        // to) in the data dir, so a restart resumes downloads and seeding.
        persistence: Some(librqbit::SessionPersistenceConfig::Json { folder: Some(seed::data_dir().join("session")) }),
        fastresume: true,
        listen,
        ..Default::default()
    };
    match vpn::session_options(engine_opts(None), vpn) {
        None => (
            None,
            Some(format!(
                "VPN is on but the tunnel is down, so the torrent engine is stopped (kill switch): {}. \
                 ZenTorrent tries again every 30 s and starts by itself once the tunnel is up; or turn the VPN off.",
                vpn.failure().unwrap_or("?")
            )),
        ),
        // The VPN tunnel only carries outgoing connections, so no listener there.
        Some(opts) if vpn.is_up() => match rt.block_on(Session::new_with_opts(folder.to_path_buf(), opts)) {
            Ok(s) => (Some(s), None),
            Err(e) => (None, Some(format!("could not start the torrent engine: {e:#}"))),
        },
        // Direct: listen on this install's port, else any free port, else not at all.
        Some(_) => {
            let mut last = None;
            let mut started = None;
            for listen in [Some(listener(listen_port())), Some(listener(0)), None] {
                match rt.block_on(Session::new_with_opts(folder.to_path_buf(), engine_opts(listen))) {
                    Ok(s) => {
                        started = Some(s);
                        break;
                    }
                    Err(e) => last = Some(e),
                }
            }
            match started {
                Some(s) => (Some(s), None),
                None => (None, last.map(|e| format!("could not start the torrent engine: {e:#}"))),
            }
        }
    }
}

/// The torrents the session remembered from the previous run.
pub fn restore(session: &Arc<Session>, ledger: &seed::Ledger, folder: &Path) -> Vec<Transfer> {
    let mut handles: Vec<ManagedTorrentHandle> = session.with_torrents(|it| it.map(|(_, h)| h.clone()).collect());
    // The session hands them back in no particular order; ids grow with every add,
    // so this is the order they were added in (what "Sort: Added" shows).
    handles.sort_by_key(|h| h.id());
    handles
        .into_iter()
        .map(|h| {
            let hash = h.info_hash().as_string();
            let e = ledger.entries.get(&hash);
            Transfer {
                name: h.name().or_else(|| e.map(|e| e.name.clone())).unwrap_or_else(|| hash.clone()),
                folder: e.map(|e| e.folder.clone()).unwrap_or_else(|| folder.to_path_buf()),
                handle: h,
            }
        })
        .collect()
}

/// A torrent read but not added yet: its files, to choose from (`add_listed`).
pub struct Listed {
    pub label: String,
    pub name: String,
    /// (the file's index in the torrent, its path inside it with `/` between folders, size).
    /// Padding files are left out; the indexes stay the torrent's.
    pub files: Vec<(usize, String, u64)>,
    add: AddTorrent<'static>,
    folder: PathBuf,
    peers: Vec<std::net::SocketAddr>,
}

impl Listed {
    /// Where it will be saved.
    pub fn folder(&self) -> &std::path::Path {
        &self.folder
    }

    #[cfg(test)]
    pub fn for_test(name: &str, files: Vec<(usize, String, u64)>) -> Self {
        Listed {
            label: name.into(),
            name: name.into(),
            files,
            add: AddTorrent::from_bytes(Vec::<u8>::new()),
            folder: PathBuf::from("/dl").join(name),
            peers: Vec::new(),
        }
    }
}

/// Read a torrent (a magnet's metadata comes from its peers) without adding it.
pub enum Listing {
    New(Listed),
    /// It is in the list already.
    Already(Transfer),
}

/// Add a torrent with all its files. Through the VPN (`tunnelled`) every torrent loses its
/// udp:// trackers before librqbit sees it (they would bypass the tunnel), and .torrent
/// URLs are fetched through it with `http`.
pub async fn add(session: Arc<Session>, http: reqwest::Client, tunnelled: bool, folder: PathBuf, label: String, source: Source) -> Result<Transfer, String> {
    match list(&session, http, tunnelled, folder, label, source).await? {
        Listing::Already(t) => Ok(t),
        Listing::New(l) => add_listed(session, l, None).await,
    }
}

/// The first half of `add`: the torrent's files, nothing added yet.
pub async fn list(session: &Arc<Session>, http: reqwest::Client, tunnelled: bool, folder: PathBuf, label: String, source: Source) -> Result<Listing, String> {
    let add = match (source, tunnelled) {
        (Source::Link(s), false) => Ok(AddTorrent::from_url(s)),
        (Source::Link(s), true) => vpn::tunnel_link(&http, &s).await,
        (Source::Bytes(b), false) => Ok(AddTorrent::from_bytes(b)),
        (Source::Bytes(b), true) => vpn::tunnel_bytes(b),
        (Source::Fetch { url, cookie }, t) => match rss::fetch_torrent(&http, &url, Some(&cookie)).await {
            Ok(b) if t => vpn::tunnel_bytes(b),
            Ok(b) => Ok(AddTorrent::from_bytes(b)),
            Err(e) => Err(e),
        },
    }
    .map_err(|e| crate::vpn::explain(format!("{label}: {e:#}"), tunnelled))?;
    // Where it goes: a torrent with several files (an album, a season) gets its own
    // folder named after it, a single file goes straight into the download folder
    // (qBittorrent's default). librqbit only makes that folder itself when no
    // output_folder is given, and ZenTorrent always gives the chosen "Save to". So
    // first list the files, then add from the same bytes (no second metadata fetch).
    match session.add_torrent(add, Some(AddTorrentOptions { list_only: true, ..Default::default() })).await {
        Ok(AddTorrentResponse::ListOnly(l)) => {
            let count = l.info.iter_file_details().count();
            let files: Vec<(usize, String, u64)> = l
                .info
                .iter_file_details()
                .enumerate()
                .filter(|(_, f)| !f.attrs().padding)
                .map(|(i, f)| (i, f.filename.to_vec().join("/"), f.len))
                .collect();
            let name = l.info.name().map(|n| n.into_owned()).unwrap_or_else(|| label.clone());
            let folder = torrent_folder(&folder, count, l.info.name().as_deref());
            Ok(Listing::New(Listed { label, name, files, add: AddTorrent::from_bytes(l.torrent_bytes), folder, peers: l.seen_peers }))
        }
        // Already in the list (or added anyway): nothing more to do.
        Ok(AddTorrentResponse::Added(_, handle)) | Ok(AddTorrentResponse::AlreadyManaged(_, handle)) => {
            let name = handle.name().unwrap_or(label);
            let folder = handle.output_folder().to_path_buf();
            Ok(Listing::Already(Transfer { name, folder, handle }))
        }
        Err(e) => Err(crate::vpn::explain(format!("{label}: {e:#}"), tunnelled)),
    }
}

/// Where a torrent's folder goes when `pairs` move (from → its whole new path): None when
/// none of its files move, or they would be split up.
pub fn moved_output(t: &Transfer, pairs: &[(PathBuf, PathBuf)]) -> Option<PathBuf> {
    let out = t.handle.output_folder().to_path_buf();
    // The folder it is in moves (its own folder, or one above it).
    for (from, to) in pairs {
        if let Ok(rest) = out.strip_prefix(from) {
            return Some(to.join(rest));
        }
    }
    // Its files sit in a shared folder (a single file, or a few top-level entries): it moves
    // when all of them move to the same new folder.
    let tops: std::collections::BTreeSet<PathBuf> = t
        .handle
        .with_metadata(|m| {
            m.file_infos
                .iter()
                .filter(|f| !f.attrs.padding)
                .filter_map(|f| f.relative_filename.components().next().map(|c| out.join(c)))
                .collect()
        })
        .unwrap_or_default();
    let mut parent: Option<PathBuf> = None;
    for top in &tops {
        let (_, to) = pairs.iter().find(|(from, _)| from == top)?;
        let p = to.parent()?.to_path_buf();
        if parent.get_or_insert_with(|| p.clone()) != &p {
            return None;
        }
    }
    parent
}

/// A torrent taken out of the engine to be moved, and what it needs to come back.
pub struct Moving {
    pub name: String,
    torrent: Vec<u8>,
    only: Option<Vec<usize>>,
    paused: bool,
    /// One of its files, relative to its folder: is it in the new place?
    probe: Option<PathBuf>,
    pub old: PathBuf,
    pub new: PathBuf,
}

/// Take a torrent out of the engine (its files close: Windows can't move open files), to be
/// added back with `put_back` once its folder has moved.
pub async fn take_out(session: &Arc<Session>, t: Transfer, new: PathBuf) -> Moving {
    let h = &t.handle;
    let torrent = h.with_metadata(|m| m.torrent_bytes.to_vec()).unwrap_or_default();
    let probe = h.with_metadata(|m| m.file_infos.iter().find(|f| !f.attrs.padding).map(|f| f.relative_filename.clone())).unwrap_or_default();
    let m = Moving { name: t.name.clone(), torrent, only: h.only_files(), paused: h.is_paused(), probe, old: h.output_folder().to_path_buf(), new };
    let _ = session.delete(h.id().into(), false).await;
    m
}

/// Add a taken-out torrent back: at its new folder if its files are there now, else where it
/// was. It checks its files and seeds on, as it was (paused or not, the same files ticked).
pub async fn put_back(session: &Arc<Session>, m: Moving) -> Result<Transfer, String> {
    let moved = m.probe.as_ref().is_some_and(|p| m.new.join(p).exists());
    let folder = if moved { m.new } else { m.old };
    let opts = AddTorrentOptions {
        output_folder: Some(folder.to_string_lossy().into_owned()),
        overwrite: true,
        only_files: m.only,
        paused: m.paused,
        ..Default::default()
    };
    match session.add_torrent(AddTorrent::from_bytes(m.torrent), Some(opts)).await {
        Ok(AddTorrentResponse::Added(_, handle)) | Ok(AddTorrentResponse::AlreadyManaged(_, handle)) => {
            Ok(Transfer { name: handle.name().unwrap_or(m.name), folder, handle })
        }
        Ok(AddTorrentResponse::ListOnly(_)) => Err(format!("{}: the engine only listed it", m.name)),
        Err(e) => Err(format!("{}: {e:#}", m.name)),
    }
}

/// The second half of `add`: start it, with only the files in `only` (None: all of them).
pub async fn add_listed(session: Arc<Session>, l: Listed, only: Option<Vec<usize>>) -> Result<Transfer, String> {
    let Listed { label, add, folder, peers, .. } = l;
    let opts = AddTorrentOptions {
        output_folder: Some(folder.to_string_lossy().into_owned()),
        overwrite: true,
        initial_peers: (!peers.is_empty()).then_some(peers),
        only_files: only,
        ..Default::default()
    };
    match session.add_torrent(add, Some(opts)).await {
        Ok(AddTorrentResponse::Added(_, handle)) | Ok(AddTorrentResponse::AlreadyManaged(_, handle)) => {
            Ok(Transfer { name: handle.name().unwrap_or(label), folder, handle })
        }
        Ok(AddTorrentResponse::ListOnly(_)) => Err(format!("{label}: the engine only listed it")),
        Err(e) => Err(format!("{label}: {e:#}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// Flux MoE's organize on a torrent ZenTorrent seeds: taken out, its folder moved, put back
    /// at the new place, and it finds every piece there (no download again).
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_seeding_torrent_moves_with_its_folder() {
        let d = std::env::temp_dir().join(format!("zt-move-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let album = d.join("Album");
        std::fs::create_dir_all(&album).unwrap();
        for (i, f) in ["01 One.flac", "02 Two.flac"].iter().enumerate() {
            std::fs::write(album.join(f), vec![i as u8 + 1; 50_000]).unwrap();
        }
        let spawner = librqbit::spawn_utils::BlockingSpawner::new(2);
        let opts = librqbit::CreateTorrentOptions { name: Some("Album"), trackers: vec![], piece_length: Some(16384) };
        let bytes = librqbit::create_torrent(&album, opts, &spawner).await.unwrap().as_bytes().unwrap().to_vec();
        let session = Session::new_with_opts(
            d.join("engine"),
            librqbit::SessionOptions { dht: None, persistence: None, listen: None, fastresume: false, ..Default::default() },
        )
        .await
        .unwrap();
        let add = AddTorrentOptions { output_folder: Some(album.to_string_lossy().into_owned()), overwrite: true, ..Default::default() };
        let handle = match session.add_torrent(AddTorrent::from_bytes(bytes), Some(add)).await.unwrap() {
            AddTorrentResponse::Added(_, h) => h,
            _ => panic!("not added"),
        };
        tokio::time::timeout(Duration::from_secs(30), handle.wait_until_completed()).await.unwrap().unwrap();
        let t = Transfer { name: "Album".into(), folder: album.clone(), handle };

        let dest = d.join("Music").join("Trance").join("Album");
        assert!(moved_output(&t, &[(d.join("Other"), d.join("X").join("Other"))]).is_none(), "not this one");
        let new = moved_output(&t, &[(album.clone(), dest.clone())]).unwrap();
        assert_eq!(new, dest);
        let m = take_out(&session, t, new).await;
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::rename(&album, &dest).unwrap();
        let back = put_back(&session, m).await.unwrap();
        assert_eq!(back.handle.output_folder(), dest.as_path());
        tokio::time::timeout(Duration::from_secs(30), back.handle.wait_until_completed()).await.unwrap().unwrap();
        let s = back.handle.stats();
        assert!(s.finished && s.progress_bytes == s.total_bytes && s.total_bytes == 100_000, "every piece found in the new place");
        let _ = std::fs::remove_dir_all(&d);
    }
}
