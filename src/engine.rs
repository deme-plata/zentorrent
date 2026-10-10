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
    let engine_opts = |listen: Option<librqbit::ListenerOptions>| SessionOptions {
        // Persistence: librqbit remembers every torrent (and where it got
        // to) in the data dir, so a restart resumes downloads and seeding.
        persistence: Some(librqbit::SessionPersistenceConfig::Json { folder: Some(seed::data_dir().join("session")) }),
        fastresume: true,
        listen,
        ..Default::default()
    };
    let (session, error) = match vpn::session_options(engine_opts(None), &vpn) {
        None => (
            None,
            Some(format!(
                "VPN is on but the tunnel is down, so the torrent engine is stopped (kill switch): {}. \
                 Fix the relay or turn the VPN off.",
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
    };
    Opened { session, error, vpn, notes }
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

/// Add a torrent. Through the VPN (`tunnelled`) every torrent loses its udp://
/// trackers before librqbit sees it (they would bypass the tunnel), and .torrent
/// URLs are fetched through it with `http`.
pub async fn add(session: Arc<Session>, http: reqwest::Client, tunnelled: bool, folder: PathBuf, label: String, source: Source) -> Result<Transfer, String> {
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
    .map_err(|e| format!("{label}: {e:#}"))?;
    // Where it goes: a torrent with several files (an album, a season) gets its own
    // folder named after it, a single file goes straight into the download folder
    // (qBittorrent's default). librqbit only makes that folder itself when no
    // output_folder is given, and ZenTorrent always gives the chosen "Save to". So
    // first list the files, then add from the same bytes (no second metadata fetch).
    let listed = session.add_torrent(add, Some(AddTorrentOptions { list_only: true, ..Default::default() })).await;
    let (add, folder, peers) = match listed {
        Ok(AddTorrentResponse::ListOnly(l)) => {
            let files = l.info.iter_file_details().count();
            let dest = torrent_folder(&folder, files, l.info.name().as_deref());
            (AddTorrent::from_bytes(l.torrent_bytes), dest, l.seen_peers)
        }
        // Already in the list (or added anyway): nothing more to do.
        Ok(AddTorrentResponse::Added(_, handle)) | Ok(AddTorrentResponse::AlreadyManaged(_, handle)) => {
            let name = handle.name().unwrap_or(label);
            let folder = handle.output_folder().to_path_buf();
            return Ok(Transfer { name, folder, handle });
        }
        Err(e) => return Err(format!("{label}: {e:#}")),
    };
    let opts = AddTorrentOptions {
        output_folder: Some(folder.to_string_lossy().into_owned()),
        overwrite: true,
        initial_peers: (!peers.is_empty()).then_some(peers),
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
