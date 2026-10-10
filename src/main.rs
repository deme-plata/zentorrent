//! ZenTorrent — a small desktop BitTorrent client.
//!
//! egui for the window, librqbit for the BitTorrent engine (DHT, trackers,
//! peers, piece verification), rfd for the native file/folder pickers.
//! The left sidebar (`sidebar.rs`) filters the list by status, tracker and label.

#![cfg_attr(windows, windows_subsystem = "windows")]

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use eframe::egui;
use librqbit::{
    AddTorrent, AddTorrentOptions, AddTorrentResponse, ManagedTorrent, Session, SessionOptions,
};

type ManagedTorrentHandle = Arc<ManagedTorrent>;

/// How soon a feed that failed to load is tried again.
const FEED_RETRY: Duration = Duration::from_secs(60);

mod details;
mod engine;
mod history;
mod info;
mod listview;
mod mcp;
mod meta;
mod moe;
mod player;
mod repair;
mod rss;
mod search;
mod seed;
mod serve;
mod sidebar;
mod tunnel;
mod update;
mod vpn;

fn main() -> eframe::Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    // "Save to" as the user last chose it, else the system's Downloads folder.
    let default_dir = saved_folder()
        .or_else(dirs::download_dir)
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."));

    // Leftovers of the previous update (the replaced program, set aside while it ran).
    update::clean_up();

    // A terminal command on Windows: this is a windowed program, so attach to the
    // terminal it was started from, or its output goes nowhere. Not for `mcp`,
    // whose stdin/stdout are the client's pipes.
    #[cfg(windows)]
    if std::env::args().nth(1).is_some_and(|a| a != "mcp") {
        // SAFETY: plain Win32 call; fails harmlessly when there is no parent console.
        unsafe { windows_sys::Win32::System::Console::AttachConsole(windows_sys::Win32::System::Console::ATTACH_PARENT_PROCESS) };
    }

    // Headless mode: `zentorrent --cli <magnet|url|file.torrent> [folder]`.
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--cli") {
        let Some(link) = args.get(2).cloned() else {
            eprintln!("usage: zentorrent --cli <magnet|url|file.torrent> [folder]");
            std::process::exit(2);
        };
        let dir = args.get(3).map(PathBuf::from).unwrap_or(default_dir);
        let vpn = vpn::Vpn::start(&rt, &vpn::VpnSettings::load());
        if let Err(e) = rt.block_on(cli(link, dir, &vpn)) {
            eprintln!("error: {e:#}");
            std::process::exit(1);
        }
        return Ok(());
    }

    // `zentorrent --update`: check the signed channel and install if newer.
    if args.get(1).map(String::as_str) == Some("--update") {
        println!("ZenTorrent {} ({})", update::VERSION, update::TARGET);
        match rt.block_on(update::check()) {
            Ok(None) => println!("up to date"),
            Ok(Some(m)) => {
                println!("v{} available: {}", m.version, m.notes);
                match rt.block_on(update::install(&m)) {
                    Ok(()) => println!("installed v{} — start ZenTorrent again", m.version),
                    Err(e) => {
                        eprintln!("error: {e}");
                        std::process::exit(1);
                    }
                }
            }
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        return Ok(());
    }

    // `zentorrent --get-player-engine`: what the first Play does on Windows, by hand.
    if args.get(1).map(String::as_str) == Some("--get-player-engine") {
        if player::ffi::api().is_err() && !player::engine::path().exists() {
            let got = Arc::new(std::sync::atomic::AtomicU64::new(0));
            if let Err(e) = rt.block_on(player::engine::fetch(None, got)) {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
            println!("downloaded and verified: {}", player::engine::path().display());
        }
        match player::ffi::api() {
            Ok(_) => println!("player engine loads"),
            Err(e) => {
                eprintln!("error: {e}");
                std::process::exit(1);
            }
        }
        return Ok(());
    }

    // `zentorrent --ask "<question>" [folder] [--cpu]`: one Flux MoE turn in the terminal
    // (the real local model, your feeds and that folder; actions are printed, not done).
    if args.get(1).map(String::as_str) == Some("--ask") {
        let Some(question) = args.get(2) else {
            eprintln!("usage: zentorrent --ask \"<question>\" [folder] [--cpu]");
            std::process::exit(2);
        };
        let folder = args.get(3).filter(|a| !a.starts_with("--")).map(PathBuf::from).unwrap_or(default_dir);
        let lib = moe::skills::Library { folder, torrents: Vec::new(), feeds: history::History::load().entries };
        if let Err(e) = rt.block_on(moe::ask(question, lib, args.iter().any(|a| a == "--cpu"))) {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
        return Ok(());
    }

    // `zentorrent --repair-session <session folder> [save folder]`: show how a damaged
    // torrent list would be rebuilt (writes nothing; the app repairs by itself at start).
    if args.get(1).map(String::as_str) == Some("--repair-session") {
        let Some(dir) = args.get(2).map(PathBuf::from) else {
            eprintln!("usage: zentorrent --repair-session <session folder> [save folder]");
            std::process::exit(2);
        };
        let saves = vec![args.get(3).map(PathBuf::from).unwrap_or(default_dir)];
        let plan = repair::plan(&dir, &saves);
        for t in &plan {
            println!("{}  {}  →  {}", if t.found { "found  " } else { "MISSING" }, t.name, t.folder.display());
        }
        println!("{} torrents, {} found on disk", plan.len(), plan.iter().filter(|t| t.found).count());
        return Ok(());
    }

    // `zentorrent --login <feed url> <user> <password>`: test a tracker login.
    if args.get(1).map(String::as_str) == Some("--login") {
        let (Some(u), Some(n), Some(p)) = (args.get(2), args.get(3), args.get(4)) else {
            eprintln!("usage: zentorrent --login <feed url> <username> <password>");
            std::process::exit(2);
        };
        match rt.block_on(rss::login(u, n, p)) {
            Ok(c) => println!("logged in; session cookie has {} part(s)", c.split("; ").count()),
            Err(e) => {
                eprintln!("error: {e:#}");
                std::process::exit(1);
            }
        }
        return Ok(());
    }

    // `zentorrent --fetch <torrent url> [cookie]`: test a feed download link.
    if args.get(1).map(String::as_str) == Some("--fetch") {
        let Some(url) = args.get(2).cloned() else {
            eprintln!("usage: zentorrent --fetch <torrent url> [cookie]");
            std::process::exit(2);
        };
        let http = rss::http();
        match rt.block_on(rss::fetch_torrent(&http, &url, args.get(3).map(String::as_str))) {
            Ok(b) => println!("ok: valid .torrent, {} bytes", b.len()),
            Err(e) => {
                eprintln!("error: {e:#}");
                std::process::exit(1);
            }
        }
        return Ok(());
    }

    // `zentorrent --feed <url>`: print what ZenTorrent sees in an RSS feed.
    if args.get(1).map(String::as_str) == Some("--feed") {
        let Some(url) = args.get(2).cloned() else {
            eprintln!("usage: zentorrent --feed <rss url>");
            std::process::exit(2);
        };
        let http = rss::http();
        match rt.block_on(rss::fetch(&http, &url)) {
            Ok(items) => {
                println!("{} items from {}", items.len(), rss::display_url(&url));
                for it in items {
                    let sz = it.size.map(human).unwrap_or_default();
                    let cat = if it.category.is_empty() { String::new() } else { format!("{{{}}}  ", it.category) };
                    let n = |v: Option<u32>| v.map(|v| v.to_string()).unwrap_or_else(|| "-".into());
                    let swarm = format!("S{} L{} G{}{}", n(it.seeders), n(it.leechers), n(it.grabs), if it.freeleech { " FREE" } else { "" });
                    let tags = if it.tags.is_empty() { String::new() } else { format!(" #{}", it.tags.join(" #")) };
                    // The link is not printed: private feeds put the passkey in it.
                    println!("  {}  [{sz}]  {cat}{swarm}{tags}", it.title);
                }
            }
            Err(e) => {
                eprintln!("error: {e:#}");
                std::process::exit(1);
            }
        }
        return Ok(());
    }

    // `zentorrent serve [folder] [--port N]`: no window — the engine, the feeds and the
    // MCP server, in a terminal or on a server. Ctrl-C stops it.
    if args.get(1).map(String::as_str) == Some("serve") {
        let folder = args.get(2).filter(|a| !a.starts_with("--")).map(PathBuf::from).unwrap_or(default_dir);
        let port = args.iter().position(|a| a == "--port").and_then(|i| args.get(i + 1)).and_then(|p| p.parse().ok());
        if let Err(e) = serve::run(rt, folder, port) {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
        return Ok(());
    }

    // `zentorrent mcp`: MCP over stdin/stdout, for clients that start a command; it
    // talks to the running ZenTorrent (the window with MCP on, or `serve`).
    if args.get(1).map(String::as_str) == Some("mcp") {
        if let Err(e) = rt.block_on(mcp::stdio_bridge()) {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
        return Ok(());
    }

    // `zentorrent mcp-config`: the commands that connect Claude Code.
    if args.get(1).map(String::as_str) == Some("mcp-config") {
        let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "zentorrent".into());
        println!("{}", mcp::setup_text(&mcp::config(), &exe));
        return Ok(());
    }

    // No screen (an ssh session, a server): say what works instead of a winit error.
    #[cfg(all(unix, not(target_os = "macos")))]
    if std::env::var_os("DISPLAY").is_none() && std::env::var_os("WAYLAND_DISPLAY").is_none() && std::env::var_os("WAYLAND_SOCKET").is_none() {
        eprintln!(
            "ZenTorrent {}: no screen here (neither DISPLAY nor WAYLAND_DISPLAY is set), so no window.\n\n\
             Run it without one:\n  \
             zentorrent serve [folder]           the engine, your RSS feeds and the MCP server (Ctrl-C stops)\n  \
             zentorrent --cli <magnet|url> [dir] download one torrent and seed it\n  \
             zentorrent mcp-config               how to connect Claude Code to `serve`\n  \
             zentorrent --ask \"<question>\"      one Flux MoE question in the terminal",
            update::VERSION
        );
        std::process::exit(2);
    }

    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("ZenTorrent")
            // Room for the sidebar, the list and the player's playlist side by side.
            .with_inner_size([1180.0, 720.0])
            .with_min_inner_size([560.0, 420.0]),
        ..Default::default()
    };
    eframe::run_native(
        "ZenTorrent",
        opts,
        Box::new(move |cc| Ok(Box::new(App::new(cc, rt, default_dir)))),
    )
}

/// Incoming peer connections. Without a listener ZenTorrent can only dial out: two such
/// clients never find each other, and seeding reaches only peers that listen themselves.
/// UPnP asks the router to forward the port. Never used with the VPN on (see `vpn`).
fn listener(port: u16) -> librqbit::ListenerOptions {
    librqbit::ListenerOptions {
        listen_addr: (std::net::Ipv6Addr::UNSPECIFIED, port).into(),
        enable_upnp_port_forwarding: true,
        ..Default::default()
    }
}

/// A port in 20000–59999 picked from the data folder: the same on every start (so a
/// router rule keeps working), different for two installs on one machine.
fn listen_port() -> u16 {
    let h = seed::data_dir()
        .to_string_lossy()
        .bytes()
        .fold(0xcbf2_9ce4_8422_2325_u64, |h, b| (h ^ b as u64).wrapping_mul(0x100_0000_01b3));
    20_000 + (h % 40_000) as u16
}

async fn cli(link: String, dir: PathBuf, vpn: &vpn::Vpn) -> anyhow::Result<()> {
    let Some(session_opts) = vpn::session_options(SessionOptions::default(), vpn) else {
        anyhow::bail!("VPN is on but the tunnel is down, so nothing is downloaded (kill switch): {}", vpn.failure().unwrap_or("?"));
    };
    if let vpn::Vpn::Up { relay, rtt, .. } = vpn {
        println!("VPN on: relay {relay}, handshake {} ms — DHT, local discovery and udp:// trackers off", rtt.as_millis());
    }
    let session = Session::new_with_opts(dir.clone(), session_opts).await?;
    let add = match (std::path::Path::new(&link).is_file(), vpn.is_up()) {
        (true, true) => vpn::tunnel_bytes(std::fs::read(&link)?)?,
        (true, false) => AddTorrent::from_bytes(std::fs::read(&link)?),
        (false, true) => vpn::tunnel_link(&vpn.http(), &link).await?,
        (false, false) => AddTorrent::from_url(link),
    };
    let opts = AddTorrentOptions {
        output_folder: Some(dir.to_string_lossy().into_owned()),
        overwrite: true,
        ..Default::default()
    };
    let handle = match session.add_torrent(add, Some(opts)).await? {
        AddTorrentResponse::Added(_, h) | AddTorrentResponse::AlreadyManaged(_, h) => h,
        AddTorrentResponse::ListOnly(_) => anyhow::bail!("list-only response"),
    };
    println!("downloading {} -> {}", handle.name().unwrap_or_default(), dir.display());
    loop {
        let s = handle.stats();
        let (spd, peers) = s
            .live
            .as_ref()
            .map(|l| (speed(l.download_speed.mbps), l.snapshot.peer_stats.live))
            .unwrap_or_default();
        let tunnel = vpn
            .stats()
            .map(|v| format!("  vpn ↑{} ↓{} conns {}", human(v.bytes_up), human(v.bytes_down), v.connections_active))
            .unwrap_or_default();
        println!(
            "{:5.1} %  {} / {}  ↓ {spd}  peers {peers}  [{}]{tunnel}",
            if s.total_bytes > 0 { s.progress_bytes as f64 * 100.0 / s.total_bytes as f64 } else { 0.0 },
            human(s.progress_bytes),
            human(s.total_bytes),
            s.state
        );
        if s.finished {
            println!("done");
            return Ok(());
        }
        if let Some(e) = s.error {
            anyhow::bail!(e);
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}

/// One row in the transfer list.
struct Transfer {
    name: String,
    folder: PathBuf,
    handle: ManagedTorrentHandle,
}

/// Messages from background tasks back to the UI thread.
#[derive(Default)]
struct Inbox {
    added: Vec<Transfer>,
    errors: Vec<String>,
    /// (feed url, items or error)
    feeds: Vec<(String, Result<Vec<rss::Item>, String>)>,
    update: Option<Upd>,
    /// (feed url, cookie or error) from a Log in click.
    login: Vec<(String, Result<String, String>)>,
    /// (query key, OMDb answer or error)
    meta: Vec<(String, Result<Option<meta::Info>, String>)>,
    /// (imdb id, poster bytes or error)
    posters: Vec<(String, Result<Vec<u8>, String>)>,
    busy: usize,
    /// Result of the VPN window's "Test relay" (handshake + ping, ms).
    vpn_test: Option<Result<u128, String>>,
    /// (info-hash, torrent name, play order + folder, or why not) after a Play click.
    tracks: Vec<(String, String, Result<(Vec<player::playlist::Track>, PathBuf), String>)>,
    /// (info-hash, file index, bytes or why not) for the Details panel's Info tab.
    info: Vec<(String, usize, Result<Vec<u8>, String>)>,
}

#[derive(Clone)]
enum Upd {
    Checking,
    UpToDate,
    Available(update::Manifest),
    Installing(String),
    Installed(String),
    Failed(String),
}

#[derive(PartialEq, Clone, Copy)]
enum View {
    Downloads,
    Seeding,
    Feeds,
    Moe,
}

/// RSS state that lives only in memory (the feed list itself is in `rss::FeedStore`).
#[derive(Default)]
struct FeedView {
    items: HashMap<String, Vec<rss::Item>>,
    status: HashMap<String, String>,
    selected: Option<usize>,
    new_name: String,
    new_url: String,
    /// The search box (see `search.rs` for the syntax).
    query: String,
    sort: search::Sort,
    /// false = what the feeds list now, true = the whole history.
    scope_history: bool,
    /// Indexes into the history, best first, and what they were computed for.
    results: Vec<usize>,
    results_key: Option<(String, search::Sort, bool, u64, Option<String>, u64)>,
    in_flight: usize,
    last_refresh: Option<Instant>,
    /// A feed failed: re-read the failed ones at this time instead of
    /// waiting the whole refresh interval with an empty list.
    retry_at: Option<Instant>,
    login_user: String,
    login_pass: String,
    login_busy: bool,
    /// Feed of the last ⬇ click, to open its login box if the site refuses.
    last_pick: Option<String>,
}

struct App {
    rt: tokio::runtime::Runtime,
    session: Option<Arc<Session>>,
    session_error: Option<String>,
    inbox: Arc<Mutex<Inbox>>,
    transfers: Vec<Transfer>,
    folder: PathBuf,
    link: String,
    errors: Vec<String>,
    view: View,
    store: rss::FeedStore,
    fv: FeedView,
    http: reqwest::Client,
    upd: Upd,
    upd_checked: Option<Instant>,
    ledger: seed::Ledger,
    meta_cache: meta::Cache,
    meta_jobs: tokio::sync::mpsc::UnboundedSender<MetaJob>,
    meta_pending: std::collections::HashSet<String>,
    meta_error: Option<String>,
    meta_error_at: Option<Instant>,
    textures: HashMap<String, Option<egui::TextureHandle>>,
    last_tick: Instant,
    last_ledger_save: Instant,
    sidebar: sidebar::Sidebar,
    /// Per-transfer facts for this frame, same order as `transfers`.
    facts: Vec<sidebar::Facts>,
    /// The open Details panel, if any.
    details: Option<details::Details>,
    /// Per-torrent (down, up) MiB/s, one sample a second, for the Details graph.
    speed_hist: HashMap<String, std::collections::VecDeque<(f32, f32)>>,
    /// Every RSS item seen (the history behind RSS search).
    feed_hist: history::History,
    feed_hist_saved: Instant,
    /// Bumped when OMDb answers arrive: search sees film genres from them.
    meta_version: u64,
    /// The built-in player (libmpv is loaded on the first Play).
    player: Option<player::Player>,
    /// Flux MoE asked to start at this track (matched when the play order arrives).
    play_from: Option<String>,
    /// The ✨ Flux MoE tab.
    moe: moe::Moe,
    pui: player::ui::PlayerUi,
    /// Bytes of the player engine downloaded so far, while the first Play fetches it.
    engine_got: Option<Arc<std::sync::atomic::AtomicU64>>,
    vpn: vpn::Vpn,
    /// Settings the running engine was started with / last saved / being edited.
    vpn_running: vpn::VpnSettings,
    vpn_settings: vpn::VpnSettings,
    vpn_edit: vpn::VpnSettings,
    vpn_open: bool,
    vpn_note: Option<String>,
    vpn_testing: bool,
    /// This install's tunnel public key (hex), read when the VPN window opens.
    vpn_pub: Option<Result<String, String>>,
    /// The MCP server (on while Flux MoE's MCP switch is on).
    mcp: Option<mcp::Server>,
    /// What MCP clients see (refreshed every 2 s while the server runs) and ask for.
    mcp_lib: Arc<Mutex<moe::skills::Library>>,
    mcp_asks: Arc<Mutex<Vec<mcp::Ask>>>,
    mcp_lib_at: Instant,
    /// The Downloads list's layout, order and media filter.
    list: listview::ListView,
    /// What each torrent holds (video / music / other), by info-hash, once its file list is known.
    media: HashMap<String, listview::Media>,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, rt: tokio::runtime::Runtime, folder: PathBuf) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());

        let vpn_settings = vpn::VpnSettings::load();
        // The engine: damaged torrent lists repaired, VPN kill switch, incoming listener.
        let engine::Opened { session, error: session_error, vpn, notes: repaired } = engine::open(&rt, &folder);
        let inbox = Arc::new(Mutex::new(Inbox::default()));
        // Torrents restored from the previous run.
        let ledger = seed::Ledger::load();
        let transfers = session.as_ref().map(|s| engine::restore(s, &ledger, &folder)).unwrap_or_default();

        let meta_jobs = spawn_meta_worker(&rt, inbox.clone(), cc.egui_ctx.clone());

        let app = Self {
            rt,
            session,
            session_error,
            inbox,
            transfers,
            folder,
            link: String::new(),
            errors: repaired,
            view: View::Downloads,
            store: rss::FeedStore::load(),
            fv: FeedView::default(),
            http: vpn.http(),
            upd: Upd::Checking,
            upd_checked: None,
            ledger,
            meta_cache: meta::Cache::load(),
            meta_jobs,
            meta_pending: Default::default(),
            meta_error: None,
            meta_error_at: None,
            textures: HashMap::new(),
            last_tick: Instant::now(),
            last_ledger_save: Instant::now(),
            sidebar: Default::default(),
            facts: Vec::new(),
            details: None,
            speed_hist: HashMap::new(),
            feed_hist: history::History::load(),
            feed_hist_saved: Instant::now(),
            meta_version: 0,
            player: None,
            play_from: None,
            moe: moe::Moe::default(),
            pui: Default::default(),
            engine_got: None,
            vpn,
            vpn_running: vpn_settings.clone(),
            vpn_edit: vpn_settings.clone(),
            vpn_settings,
            vpn_open: false,
            vpn_note: None,
            vpn_testing: false,
            vpn_pub: None,
            mcp: None,
            mcp_lib: Default::default(),
            mcp_asks: Default::default(),
            mcp_lib_at: Instant::now(),
            list: listview::ListView::load(),
            media: HashMap::new(),
        };
        app.apply_global_limits();
        app
    }

    /// The "All torrents" limits from the ledger → the engine.
    fn apply_global_limits(&self) {
        if let Some(s) = &self.session {
            s.ratelimits.set_download_bps(seed::bps(self.ledger.global_down_kib));
            s.ratelimits.set_upload_bps(seed::bps(self.ledger.global_up_kib));
        }
    }

    /// A torrent's own limits from the ledger → its live state. Called every
    /// tick: a resumed torrent gets a fresh limiter, so this re-applies it.
    fn apply_torrent_limits(&self, t: &Transfer) {
        let Some(live) = t.handle.live() else { return };
        let e = self.ledger.entries.get(&t.handle.info_hash().as_string());
        let (down, up) = e.map(|e| (seed::bps(e.down_limit_kib), seed::bps(e.up_limit_kib))).unwrap_or_default();
        let lim = live.ratelimits();
        if lim.get_download_bps() != down {
            lim.set_download_bps(down);
        }
        if lim.get_upload_bps() != up {
            lim.set_upload_bps(up);
        }
    }

    /// Draw the Details panel and carry out what it asks for.
    fn details_ui(&mut self, ctx: &egui::Context) {
        let Some(mut d) = self.details.take() else { return };
        let Some(i) = self.transfers.iter().position(|t| t.handle.info_hash().as_string() == d.hash) else {
            return; // the torrent was removed: the panel closes
        };
        let Some(f) = self.facts.iter().find(|f| f.hash == d.hash).cloned() else {
            self.details = Some(d);
            return;
        };
        let hash = d.hash.clone();
        let actions = details::show(ctx, &mut d, &self.transfers[i], &f, &mut self.ledger, self.speed_hist.get(&hash));
        let mut keep = true;
        for a in actions {
            match a {
                details::Action::Close => keep = false,
                details::Action::Pause(p) => self.set_paused(&d.hash, p),
                details::Action::OpenFolder => {
                    let _ = open_folder(&self.transfers[i].folder);
                }
                details::Action::Label(l, on) => {
                    self.ledger.set_label(&d.hash, &l, on);
                    self.save_ledger();
                }
                details::Action::LedgerChanged => {
                    self.save_ledger();
                    self.apply_global_limits();
                    self.apply_torrent_limits(&self.transfers[i]);
                }
                details::Action::FetchInfo(file) => {
                    let (h, inbox, ctx, hash) = (self.transfers[i].handle.clone(), self.inbox.clone(), ctx.clone(), d.hash.clone());
                    self.rt.spawn(async move {
                        use tokio::io::AsyncReadExt;
                        let read = async {
                            let s = h.stream(file).await.map_err(|e| format!("{e:#}"))?;
                            let mut b = Vec::new();
                            s.take(512 * 1024).read_to_end(&mut b).await.map_err(|e| e.to_string())?;
                            Ok::<_, String>(b)
                        };
                        let got = tokio::time::timeout(Duration::from_secs(30), read)
                            .await
                            .unwrap_or_else(|_| Err("no peer has sent this file yet — try again in a moment".into()));
                        inbox.lock().unwrap().info.push((hash, file, got));
                        ctx.request_repaint();
                    });
                }
                details::Action::SetFiles(only) => {
                    if let Some(sess) = self.session.clone() {
                        let (h, inbox, ctx, name) = (self.transfers[i].handle.clone(), self.inbox.clone(), ctx.clone(), self.transfers[i].name.clone());
                        self.rt.spawn(async move {
                            if let Err(e) = sess.update_only_files(&h, &only).await {
                                inbox.lock().unwrap().errors.push(format!("{name}: could not change the file selection: {e:#}"));
                                ctx.request_repaint();
                            }
                        });
                    }
                }
            }
        }
        if keep {
            self.details = Some(d);
        }
    }

    /// What the sidebar needs to know about every transfer, read once per frame.
    fn collect_facts(&self) -> Vec<sidebar::Facts> {
        self.transfers
            .iter()
            .map(|t| {
                let s = t.handle.stats();
                let hash = t.handle.info_hash().as_string();
                let e = self.ledger.entries.get(&hash);
                let (down, up) = s.live.as_ref().map(|l| (l.download_speed.mbps, l.upload_speed.mbps)).unwrap_or_default();
                // Hosts only: private announce URLs carry the passkey.
                let mut sites: Vec<String> =
                    t.handle.shared().trackers.iter().filter_map(|u| u.host_str().map(sidebar::site)).collect();
                sites.sort();
                sites.dedup();
                // Trailing seconds with nothing arriving, from the per-second history.
                let idle_secs = self
                    .speed_hist
                    .get(&hash)
                    .map(|h| h.iter().rev().take_while(|(d, _)| (*d as f64) <= sidebar::ACTIVE_MIBS).count() as u32)
                    .unwrap_or(0);
                sidebar::Facts {
                    idle_secs,
                    name: t.name.clone(),
                    finished: s.finished,
                    paused: t.handle.is_paused(),
                    error: s.error.is_some(),
                    live: s.live.is_some(),
                    down_mibs: down,
                    up_mibs: up,
                    private: e.is_some_and(|e| e.private),
                    ratio: e.map(|e| e.ratio()).unwrap_or(0.0),
                    uploaded: e.map(|e| e.uploaded).unwrap_or(0),
                    size: s.total_bytes.max(e.map(|e| e.size).unwrap_or(0)),
                    sites,
                    labels: e.map(|e| e.labels.clone()).unwrap_or_default(),
                    hash,
                }
            })
            .collect()
    }

    /// Artwork for a torrent in the thumbnail view: the OMDb poster when ratings are on
    /// and the name looks like a film or an episode (looked up once, then cached).
    fn poster_for(&mut self, name: &str) -> Option<egui::TextureHandle> {
        let q = meta::guess(name, "")?;
        let k = q.key();
        match self.meta_cache.get(&k) {
            Some(Some(info)) => match self.textures.get(&info.imdb_id) {
                Some(t) => t.clone(),
                None => {
                    if let Some(url) = info.poster.clone() {
                        self.textures.insert(info.imdb_id.clone(), None);
                        let _ = self.meta_jobs.send(MetaJob::Poster { imdb_id: info.imdb_id.clone(), url });
                    }
                    None
                }
            },
            Some(None) => None,
            None => {
                let ratings_on = self.store.show_ratings && !self.store.omdb_key.trim().is_empty();
                if ratings_on && self.meta_error.is_none() && !self.meta_pending.contains(&k) && self.meta_cache.spend() {
                    self.meta_pending.insert(k.clone());
                    let _ = self.meta_jobs.send(MetaJob::Lookup { key: k, query: q, api_key: self.store.omdb_key.trim().to_string() });
                    self.meta_cache.save();
                }
                None
            }
        }
    }

    /// Is transfer `i` shown under the sidebar's current filter?
    fn visible(&self, i: usize) -> bool {
        let floor = self.ledger.ratio_floor();
        self.facts.get(i).is_none_or(|f| self.sidebar.filter.matches(f, floor))
    }

    fn sidebar_actions(&mut self, actions: Vec<sidebar::Action>) {
        use sidebar::Action;
        let mut save = false;
        for a in actions {
            match a {
                Action::Show => {
                    if self.view == View::Feeds {
                        self.view = View::Downloads;
                    }
                }
                Action::Pause(hash) => self.set_paused(&hash, true),
                Action::Resume(hash) => self.set_paused(&hash, false),
                Action::Tag(hash, label) => {
                    self.ledger.set_label(&hash, &label, true);
                    save = true;
                }
                Action::NewLabel(label) => {
                    if !self.ledger.labels.contains(&label) {
                        self.ledger.labels.push(label);
                        save = true;
                    }
                }
                Action::DeleteLabel(label) => {
                    self.ledger.delete_label(&label);
                    if self.sidebar.filter.label.as_ref() == Some(&label) {
                        self.sidebar.filter.label = None;
                    }
                    save = true;
                }
            }
        }
        if save {
            self.save_ledger();
        }
    }

    /// Pause or resume one torrent by info-hash (from a sidebar drop).
    fn set_paused(&self, hash: &str, pause: bool) {
        let Some(sess) = self.session.clone() else { return };
        let Some(t) = self.transfers.iter().find(|t| t.handle.info_hash().as_string() == hash) else { return };
        if t.handle.is_paused() == pause {
            return;
        }
        let h = t.handle.clone();
        self.rt.spawn(async move {
            let _ = if pause { sess.pause(&h).await } else { sess.unpause(&h).await };
        });
    }

    /// Once a second: fold upload counters into the ledger, count seed
    /// time, and pause torrents that reached the seed goal.
    fn tick_ledger(&mut self) {
        let dt = self.last_tick.elapsed();
        if dt < Duration::from_secs(1) {
            return;
        }
        self.last_tick = Instant::now();
        let mut to_pause = Vec::new();
        let goals = self.ledger.clone_goals();
        for t in &self.transfers {
            let s = t.handle.stats();
            let hash = t.handle.info_hash().as_string();
            let (down, up) = s.live.as_ref().map(|l| (l.download_speed.mbps as f32, l.upload_speed.mbps as f32)).unwrap_or_default();
            let h = self.speed_hist.entry(hash.clone()).or_default();
            h.push_back((down, up));
            while h.len() > details::HISTORY {
                h.pop_front();
            }
            let private = t.handle.with_metadata(|m| m.info.info().private).unwrap_or(false);
            let e = self.ledger.entries.entry(hash).or_insert_with(|| seed::Entry {
                name: t.name.clone(),
                folder: t.folder.clone(),
                ..Default::default()
            });
            e.private = private;
            if s.total_bytes > 0 {
                e.size = s.total_bytes;
            }
            e.observe_uploaded(s.uploaded_bytes);
            let seeding = s.finished && !t.handle.is_paused() && s.error.is_none();
            if seeding {
                e.seed_secs += dt.as_secs();
            }
            if seeding && !e.goal_reached && goals.goal_met(e) {
                e.goal_reached = true;
                to_pause.push(t.handle.clone());
            }
        }
        for t in &self.transfers {
            self.apply_torrent_limits(t);
        }
        if let Some(sess) = self.session.clone() {
            for h in to_pause {
                let sess = sess.clone();
                self.rt.spawn(async move {
                    let _ = sess.pause(&h).await;
                });
            }
        }
        if self.last_ledger_save.elapsed() >= Duration::from_secs(30) {
            self.save_ledger();
        }
    }

    fn save_ledger(&mut self) {
        self.last_ledger_save = Instant::now();
        if let Err(e) = self.ledger.save() {
            self.errors.push(format!("could not save ratio ledger: {e:#}"));
        }
    }

    fn seeding_ui(&mut self, ui: &mut egui::Ui) {
        let (up, size) = self.ledger.totals();
        ui.horizontal(|ui| {
            ui.heading("Seeding & ratio");
            ui.label(
                egui::RichText::new(format!(
                    "uploaded {} of {}  ·  overall ratio {:.2}",
                    human(up),
                    human(size),
                    if size > 0 { up as f64 / size as f64 } else { 0.0 }
                ))
                .weak(),
            );
        });
        ui.label(
            egui::RichText::new(
                "Ratio = what you have uploaded ÷ the torrent's size. Private trackers ask you to keep it at 1.0 or \
                 more, so leave finished torrents seeding. Counts survive restarts.",
            )
            .weak()
            .small(),
        );
        ui.horizontal(|ui| {
            let mut changed = false;
            ui.label("Stop seeding at ratio");
            let mut r_on = self.ledger.ratio_goal > 0.0;
            changed |= ui.checkbox(&mut r_on, "").changed();
            if !r_on {
                self.ledger.ratio_goal = 0.0;
            } else {
                if self.ledger.ratio_goal == 0.0 {
                    self.ledger.ratio_goal = 1.0;
                }
                changed |= ui.add(egui::DragValue::new(&mut self.ledger.ratio_goal).range(0.1..=50.0).speed(0.05)).changed();
            }
            ui.add_space(12.0);
            ui.label("or after");
            let mut h_on = self.ledger.hours_goal > 0.0;
            changed |= ui.checkbox(&mut h_on, "").changed();
            if !h_on {
                self.ledger.hours_goal = 0.0;
            } else {
                if self.ledger.hours_goal == 0.0 {
                    self.ledger.hours_goal = 72.0;
                }
                changed |= ui.add(egui::DragValue::new(&mut self.ledger.hours_goal).range(1.0..=8760.0).suffix(" h")).changed();
            }
            if !r_on && !h_on {
                ui.label(egui::RichText::new("(off: seed forever)").weak().small());
            }
            if changed {
                self.save_ledger();
            }
        });
        ui.separator();

        if self.transfers.is_empty() {
            ui.label(egui::RichText::new("Nothing is seeding yet. Finished downloads show up here.").weak());
            return;
        }
        let mut toggle = None;
        egui::ScrollArea::both().show(ui, |ui| {
            egui::Grid::new("seedgrid").striped(true).num_columns(8).spacing([10.0, 6.0]).show(ui, |ui| {
                for h in ["Torrent", "", "Size", "Uploaded", "Ratio", "Seeded", "Up now", "State"] {
                    ui.strong(h);
                }
                ui.end_row();
                for (i, t) in self.transfers.iter().enumerate() {
                    if !self.visible(i) {
                        continue;
                    }
                    let s = t.handle.stats();
                    let hash = t.handle.info_hash().as_string();
                    let e = self.ledger.entries.get(&hash).cloned().unwrap_or_default();
                    let paused = t.handle.is_paused();
                    let name: String = t.name.chars().take(34).collect();
                    ui.label(name).on_hover_text(&t.name);
                    ui.label(if e.private { "🔒" } else { "" }).on_hover_text(if e.private {
                        "Private torrent: tracker only, no DHT"
                    } else {
                        "Public torrent"
                    });
                    ui.label(human(s.total_bytes));
                    ui.label(human(e.uploaded));
                    let r = e.ratio();
                    let col = if r >= 1.0 {
                        egui::Color32::from_rgb(90, 200, 120)
                    } else if r >= 0.5 {
                        egui::Color32::from_rgb(230, 190, 80)
                    } else {
                        egui::Color32::from_rgb(230, 110, 100)
                    };
                    ui.colored_label(col, format!("{r:.2}"));
                    ui.label(seed::duration(e.seed_secs));
                    ui.label(s.live.as_ref().map(|l| speed(l.upload_speed.mbps)).unwrap_or_default());
                    let state = if !s.finished {
                        "Downloading"
                    } else if paused && e.goal_reached {
                        "Goal reached"
                    } else if paused {
                        "Paused"
                    } else {
                        "Seeding"
                    };
                    ui.horizontal(|ui| {
                        ui.label(state);
                        if ui.small_button(if paused { "▶" } else { "⏸" }).clicked() {
                            toggle = Some((t.handle.clone(), paused));
                        }
                    });
                    ui.end_row();
                }
            });
        });
        if let (Some((h, paused)), Some(sess)) = (toggle, self.session.clone()) {
            self.rt.spawn(async move {
                let _ = if paused { sess.unpause(&h).await } else { sess.pause(&h).await };
            });
        }
    }

    fn check_update(&mut self, ctx: &egui::Context) {
        self.upd = Upd::Checking;
        self.upd_checked = Some(Instant::now());
        let (inbox, ctx) = (self.inbox.clone(), ctx.clone());
        self.rt.spawn(async move {
            let u = match update::check().await {
                Ok(Some(m)) => Upd::Available(m),
                Ok(None) => Upd::UpToDate,
                Err(e) => Upd::Failed(e),
            };
            inbox.lock().unwrap().update = Some(u);
            ctx.request_repaint();
        });
    }

    fn install_update(&mut self, ctx: &egui::Context, m: update::Manifest) {
        self.upd = Upd::Installing(m.version.clone());
        let (inbox, ctx) = (self.inbox.clone(), ctx.clone());
        self.rt.spawn(async move {
            let u = match update::install(&m).await {
                Ok(()) => Upd::Installed(m.version),
                Err(e) => Upd::Failed(e),
            };
            inbox.lock().unwrap().update = Some(u);
            ctx.request_repaint();
        });
    }

    /// Top-bar VPN state; a click opens the VPN window.
    fn vpn_badge(&mut self, ui: &mut egui::Ui) {
        let green = egui::Color32::from_rgb(90, 200, 120);
        let (text, color, tip) = match &self.vpn {
            vpn::Vpn::Off => (
                "VPN off".to_string(),
                None,
                "Torrent traffic goes out directly from this computer. Click to set up the VPN.".to_string(),
            ),
            vpn::Vpn::Up { relay, rtt, .. } => {
                let s = self.vpn.stats().unwrap_or_else(|| unreachable!());
                (
                    format!("🔒 VPN  ⬆ {}  ⬇ {}", human(s.bytes_up), human(s.bytes_down)),
                    Some(green),
                    format!(
                        "Encrypted tunnel to {relay} (handshake {} ms), {} connections open.\n\
                         DHT, local peer discovery and udp:// trackers are off; no incoming peers.",
                        rtt.as_millis(),
                        s.connections_active
                    ),
                )
            }
            vpn::Vpn::Failed(e) => (
                "VPN DOWN".to_string(),
                Some(egui::Color32::LIGHT_RED),
                format!("Kill switch — the torrent engine is stopped until the tunnel is up.\n{e}"),
            ),
        };
        let mut label = egui::RichText::new(text);
        if let Some(c) = color {
            label = label.color(c);
        }
        if ui.button(label).on_hover_text(tip).clicked() {
            self.vpn_open = true;
            self.vpn_edit = self.vpn_settings.clone();
            self.vpn_note = None;
            self.vpn_pub = Some(vpn::client_key().map(|k| k.public().to_hex()).map_err(|e| format!("{e:#}")));
        }
    }

    fn vpn_window(&mut self, ctx: &egui::Context) {
        if !self.vpn_open {
            return;
        }
        let mut open = true;
        egui::Window::new("VPN — IronTunnel")
            .open(&mut open)
            .resizable(false)
            .default_width(540.0)
            .show(ctx, |ui| {
                ui.label(
                    "Torrent traffic — peers, trackers and tracker websites — goes through an encrypted \
                     tunnel to your relay, so they see the relay's address instead of yours.",
                );
                ui.add_space(6.0);
                ui.checkbox(&mut self.vpn_edit.enabled, "Send torrent traffic through the VPN");
                egui::Grid::new("vpn_grid").num_columns(2).spacing([8.0, 6.0]).show(ui, |ui| {
                    ui.label("Relay");
                    ui.add(egui::TextEdit::singleline(&mut self.vpn_edit.relay).hint_text("vpn.example.org:1195").desired_width(340.0));
                    ui.end_row();
                    ui.label("Relay key");
                    ui.add(
                        egui::TextEdit::singleline(&mut self.vpn_edit.relay_key)
                            .hint_text("64 hex characters from the relay operator")
                            .desired_width(340.0),
                    );
                    ui.end_row();
                    ui.label("Your key");
                    match &self.vpn_pub {
                        Some(Ok(hex)) => {
                            ui.horizontal(|ui| {
                                ui.monospace(format!("{}…{}", &hex[..12], &hex[52..]));
                                if ui
                                    .small_button("Copy")
                                    .on_hover_text("Send this to the relay operator: it goes in the relay's authorized list")
                                    .clicked()
                                {
                                    ui.ctx().copy_text(hex.clone());
                                }
                            });
                        }
                        Some(Err(e)) => {
                            ui.colored_label(egui::Color32::LIGHT_RED, e);
                        }
                        None => {}
                    }
                    ui.end_row();
                });
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    let valid = self.vpn_edit.check().is_ok();
                    if ui.add_enabled(valid && !self.vpn_testing, egui::Button::new("Test relay")).clicked() {
                        self.vpn_testing = true;
                        let (s, inbox, ctx) = (self.vpn_edit.clone(), self.inbox.clone(), ctx.clone());
                        self.rt.spawn(async move {
                            let r = vpn::test(&s).await.map(|d| d.as_millis());
                            inbox.lock().unwrap().vpn_test = Some(r);
                            ctx.request_repaint();
                        });
                    }
                    if self.vpn_testing {
                        ui.spinner();
                    }
                    if ui.button("Save").clicked() {
                        self.vpn_note = Some(match (self.vpn_edit.enabled, self.vpn_edit.check()) {
                            (true, Err(e)) => e,
                            _ => match self.vpn_edit.save() {
                                Ok(()) => {
                                    self.vpn_settings = self.vpn_edit.clone();
                                    "Saved.".to_string()
                                }
                                Err(e) => format!("Could not save: {e:#}"),
                            },
                        });
                    }
                    if self.vpn_settings != self.vpn_running
                        && ui.button("↻ Restart now").on_hover_text("The engine takes the VPN setting at start").clicked()
                    {
                        update::relaunch();
                    }
                });
                if let Some(n) = &self.vpn_note {
                    ui.label(n);
                }
                if self.vpn_settings != self.vpn_running {
                    ui.label(egui::RichText::new("Saved settings apply after a restart.").weak());
                }
                ui.separator();
                ui.label(
                    egui::RichText::new(
                        "While the VPN is on: DHT, local peer discovery and udp:// trackers are off (UDP cannot use \
                         the tunnel), no incoming peers are accepted, and if the relay stops answering nothing is \
                         downloaded at all. Torrents added with the VPN off are kept separately and return when you \
                         turn it off. Ratings (OMDb) and update checks still go out directly.",
                    )
                    .weak()
                    .small(),
                );
            });
        if !open {
            self.vpn_open = false;
        }
    }

    fn update_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            match self.upd.clone() {
                Upd::Available(m) => {
                    let b = egui::Button::new(
                        egui::RichText::new(format!("⬆ Update to v{}", m.version)).strong().color(egui::Color32::BLACK),
                    )
                    .fill(egui::Color32::from_rgb(90, 200, 120));
                    if ui.add(b).on_hover_text(if m.notes.is_empty() { "New version".into() } else { m.notes.clone() }).clicked() {
                        self.install_update(ctx, m);
                    }
                }
                Upd::Installing(v) => {
                    ui.label(format!("Installing v{v}…"));
                    ui.spinner();
                }
                Upd::Installed(v) => {
                    if ui.button(format!("↻ Restart into v{v}")).clicked() {
                        update::relaunch();
                    }
                }
                Upd::Checking => {
                    ui.spinner();
                }
                Upd::UpToDate => {
                    if ui.small_button("Check for updates").on_hover_text("You have the newest version").clicked() {
                        self.check_update(ctx);
                    }
                }
                Upd::Failed(e) => {
                    if ui.small_button("Retry update").on_hover_text(&e).clicked() {
                        self.check_update(ctx);
                    }
                    // Said out loud, not only on hover: "it says retry" told nobody why.
                    let short: String = e.chars().take(90).collect();
                    ui.add(
                        egui::Label::new(egui::RichText::new(format!("⚠ update failed: {short}")).color(egui::Color32::from_rgb(230, 160, 60)).small())
                            .truncate(),
                    )
                    .on_hover_text(&e);
                }
            }
            ui.label(egui::RichText::new(format!("v{}", update::VERSION)).weak());
        });
    }

    /// Re-read every feed (or just one) in the background.
    fn refresh_feeds(&mut self, ctx: &egui::Context, only: Option<usize>) {
        self.fv.last_refresh = Some(Instant::now());
        // A refused key (e.g. not yet activated) gets another try on every refresh.
        self.meta_error = None;
        for (i, f) in self.store.feeds.iter().enumerate() {
            if only.is_some_and(|o| o != i) {
                continue;
            }
            let (url, http, inbox, ctx) = (f.url.clone(), self.http.clone(), self.inbox.clone(), ctx.clone());
            self.fv.in_flight += 1;
            self.fv.status.insert(url.clone(), "refreshing…".into());
            self.rt.spawn(async move {
                let res = rss::fetch(&http, &url).await.map_err(|e| format!("{e:#}"));
                inbox.lock().unwrap().feeds.push((url, res));
                ctx.request_repaint();
            });
        }
    }

    /// Jump to the RSS tab with the right feed's log-in box open.
    fn open_login(&mut self) {
        self.view = View::Feeds;
        let i = self
            .fv
            .last_pick
            .as_ref()
            .and_then(|u| self.store.feeds.iter().position(|f| &f.url == u))
            .or((self.store.feeds.len() == 1).then_some(0));
        if i.is_some() {
            self.fv.selected = i;
        }
    }

    fn save_feed_history(&mut self) {
        self.feed_hist_saved = Instant::now();
        if let Err(e) = self.feed_hist.save() {
            self.errors.push(format!("could not save the RSS history: {e:#}"));
        }
    }

    fn save_feeds(&mut self) {
        if let Err(e) = self.store.save() {
            self.errors.push(format!("could not save feeds: {e:#}"));
        }
    }

    /// Start a torrent in the background. `source` is a magnet link, an
    /// http(s) URL to a .torrent file, or the raw bytes of a .torrent file.
    fn start(&mut self, ctx: &egui::Context, label: String, source: Source) {
        // Never a silent no-op: if the engine did not start, every Download
        // button would otherwise do nothing at all.
        let Some(session) = self.session.clone() else {
            let why = self.session_error.clone().unwrap_or_else(|| "the torrent engine is not running".into());
            self.errors.push(format!("{label}: can't start — {why}"));
            return;
        };
        let folder = self.folder.clone();
        let inbox = self.inbox.clone();
        let ctx = ctx.clone();
        let http = self.http.clone();
        // Through the VPN, every torrent loses its udp:// trackers before librqbit sees
        // it (they would bypass the tunnel), and .torrent URLs are fetched through it.
        let tunnelled = self.vpn.is_up();
        inbox.lock().unwrap().busy += 1;
        self.rt.spawn(async move {
            let res = engine::add(session, http, tunnelled, folder, label, source).await;
            let mut ib = inbox.lock().unwrap();
            ib.busy -= 1;
            match res {
                Ok(t) => ib.added.push(t),
                Err(e) => ib.errors.push(e),
            }
            ctx.request_repaint();
        });
    }

    /// Bytes done and sizes of every file of one torrent (empty before metadata).
    fn file_state(&self, hash: &str) -> (Vec<u64>, Vec<u64>) {
        let Some(t) = self.transfers.iter().find(|t| t.handle.info_hash().as_string() == hash) else { return Default::default() };
        let lens = t.handle.with_metadata(|m| m.file_infos.iter().map(|f| f.len).collect()).unwrap_or_default();
        (t.handle.stats().file_progress, lens)
    }

    /// Play click: build the play order in the background (reading the torrent's
    /// .m3u playlists, through the stream if they aren't downloaded yet).
    fn play_torrent(&mut self, ctx: &egui::Context, hash: String) {
        let Some(t) = self.transfers.iter().find(|t| t.handle.info_hash().as_string() == hash) else { return };
        let (handle, name) = (t.handle.clone(), t.name.clone());
        if self.player.is_none() {
            let c = ctx.clone();
            self.player = Some(player::Player::new(self.rt.handle().clone(), Arc::new(move || c.request_repaint())));
        }
        let p = self.player.as_mut().unwrap();
        p.registry.torrents.lock().unwrap().insert(hash.clone(), handle.clone());
        p.error = Some("preparing the playlist…".into());
        let reg = p.registry;
        // Windows, first Play: fetch the player engine first (through the tunnel when
        // the VPN is up, like everything else the user asked for).
        if self.engine_got.is_some() {
            return; // already fetching; that Play continues by itself
        }
        let got = player::engine::needed().then(|| Arc::new(std::sync::atomic::AtomicU64::new(0)));
        self.engine_got = got.clone();
        let proxy = match &self.vpn {
            vpn::Vpn::Up { proxy, .. } => Some(format!("socks5h://{}", proxy.local_addr())),
            _ => None,
        };
        let (inbox, ctx) = (self.inbox.clone(), ctx.clone());
        self.rt.spawn(async move {
            if let Some(got) = got {
                if let Err(e) = player::engine::fetch(proxy, got).await {
                    inbox.lock().unwrap().tracks.push((hash, name, Err(e)));
                    ctx.request_repaint();
                    return;
                }
            }
            let r = player::load_tracks(handle, reg).await;
            inbox.lock().unwrap().tracks.push((hash, name, r));
            ctx.request_repaint();
        });
    }

    /// Play files on disk (a Flux MoE playlist): a queue with no torrent behind it.
    fn play_files(&mut self, ctx: &egui::Context, name: String, files: Vec<PathBuf>) {
        if self.player.is_none() {
            let c = ctx.clone();
            self.player = Some(player::Player::new(self.rt.handle().clone(), Arc::new(move || c.request_repaint())));
        }
        self.player.as_mut().unwrap().error = Some("preparing the playlist…".into());
        if self.engine_got.is_some() {
            return;
        }
        let tracks: Vec<player::playlist::Track> = files
            .iter()
            .enumerate()
            .filter_map(|(i, f)| {
                let p = f.to_string_lossy();
                Some(player::playlist::Track::file(i, &p, player::playlist::title_from_path(&p), None, player::playlist::kind_of(&p)?))
            })
            .collect();
        let got = player::engine::needed().then(|| Arc::new(std::sync::atomic::AtomicU64::new(0)));
        self.engine_got = got.clone();
        let proxy = match &self.vpn {
            vpn::Vpn::Up { proxy, .. } => Some(format!("socks5h://{}", proxy.local_addr())),
            _ => None,
        };
        let (inbox, ctx) = (self.inbox.clone(), ctx.clone());
        self.rt.spawn(async move {
            let r = match got {
                Some(got) => player::engine::fetch(proxy, got).await.map(|_| (tracks, PathBuf::new())),
                None => Ok((tracks, PathBuf::new())),
            };
            inbox.lock().unwrap().tracks.push((String::new(), name, r));
            ctx.request_repaint();
        });
    }

    /// The MCP server: started and stopped with Flux MoE's MCP switch; its library
    /// snapshot kept fresh; what clients asked for carried out (or shown as a card).
    fn mcp_tick(&mut self, ctx: &egui::Context) {
        match (self.moe.mcp_on(), self.mcp.is_some()) {
            (true, false) => {
                let cfg = mcp::config();
                *self.mcp_lib.lock().unwrap() = self.moe_library();
                self.mcp_lib_at = Instant::now();
                let c = ctx.clone();
                let host = Arc::new(mcp::Relay { lib: self.mcp_lib.clone(), asks: self.mcp_asks.clone(), wake: Box::new(move || c.request_repaint()) });
                match self.rt.block_on(mcp::serve(host, &cfg)) {
                    Ok(srv) => {
                        let cfg = mcp::Config { port: srv.port, ..cfg };
                        let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "zentorrent".into());
                        self.moe.mcp_started(srv.port, mcp::setup_text(&cfg, &exe));
                        self.mcp = Some(srv);
                    }
                    Err(e) => self.moe.mcp_failed(e),
                }
            }
            (false, true) => {
                if let Some(s) = self.mcp.take() {
                    s.stop();
                }
            }
            _ => {}
        }
        if self.mcp.is_none() {
            return;
        }
        if self.mcp_lib_at.elapsed() >= Duration::from_secs(2) {
            *self.mcp_lib.lock().unwrap() = self.moe_library();
            self.mcp_lib_at = Instant::now();
        }
        let asks: Vec<mcp::Ask> = std::mem::take(&mut *self.mcp_asks.lock().unwrap());
        for a in asks {
            match a {
                mcp::Ask::Act(a) if a.needs_ok() => {
                    self.moe.propose(a);
                    self.view = View::Moe;
                }
                mcp::Ask::Act(a) => self.moe_actions(ctx, vec![a]),
                mcp::Ask::Control(mcp::Control::Add(link)) => {
                    let title = if link.starts_with("magnet:") {
                        link.split("dn=").nth(1).and_then(|n| n.split('&').next()).map(|n| n.replace('+', " ")).unwrap_or_else(|| "magnet link".into())
                    } else {
                        link.rsplit('/').next().unwrap_or("torrent").to_string()
                    };
                    self.moe.propose(moe::skills::Action::Download { title, link, feed_key: String::new() });
                    self.view = View::Moe;
                }
                mcp::Ask::Control(mcp::Control::Pause(h)) => self.set_paused(&h, true),
                mcp::Ask::Control(mcp::Control::Resume(h)) => self.set_paused(&h, false),
            }
        }
        ctx.request_repaint_after(Duration::from_secs(2));
    }

    /// What Flux MoE's skills see: the torrents, their files, and the feed items.
    fn moe_library(&self) -> moe::skills::Library {
        let facts = self.collect_facts();
        let torrents = self
            .transfers
            .iter()
            .zip(facts)
            .map(|(t, f)| {
                let s = t.handle.stats();
                let state = if f.error {
                    "error"
                } else if f.paused {
                    "paused"
                } else if f.finished {
                    "complete, seeding"
                } else {
                    "downloading"
                };
                moe::skills::Torrent {
                    hash: f.hash,
                    name: f.name,
                    state: state.into(),
                    progress: if s.total_bytes > 0 { s.progress_bytes as f64 / s.total_bytes as f64 } else { 0.0 },
                    size: f.size,
                    labels: f.labels,
                    sites: f.sites,
                    files: t
                        .handle
                        .with_metadata(|m| m.file_infos.iter().map(|i| (i.relative_filename.to_string_lossy().replace('\\', "/"), i.len)).collect())
                        .unwrap_or_default(),
                    root: t.handle.output_folder().to_path_buf(),
                }
            })
            .collect();
        moe::skills::Library { folder: self.folder.clone(), torrents, feeds: self.feed_hist.entries.clone() }
    }

    /// Carry out what Flux MoE asked for (downloads only after the user's OK in the tab).
    fn moe_actions(&mut self, ctx: &egui::Context, actions: Vec<moe::skills::Action>) {
        use moe::skills::Action;
        for a in actions {
            match a {
                Action::Play { hash, track } => {
                    self.play_from = track;
                    self.play_torrent(ctx, hash);
                }
                Action::PlayFiles { name, files } => self.play_files(ctx, name, files),
                Action::Download { title, link, feed_key } => {
                    let cookie = self
                        .store
                        .feeds
                        .iter()
                        .find(|f| history::feed_key(&f.url) == feed_key)
                        .map(|f| rss::feed_cookie(&f.cookie, &f.url))
                        .unwrap_or_default();
                    self.start(ctx, title, Source::from_feed(link, &cookie));
                }
                // Moves are done by the tab itself (files only, after Apply).
                Action::Moves { .. } => {}
                // Result lists are shown in the tab; their Download buttons send Download.
                Action::Picks(_) => {}
            }
        }
    }

    /// The now-playing bar, the playlist panel and the Sound window.
    fn player_ui(&mut self, root: &mut egui::Ui, ctx: &egui::Context) {
        let Some(hash) = self.player.as_ref().filter(|p| p.active() || p.error.is_some()).map(|p| p.queue.hash.clone()) else { return };
        let (done, len) = self.file_state(&hash);
        let files = player::ui::Files { done: &done, len: &len };
        let p = self.player.as_mut().unwrap();
        p.tick(&|f| files.complete(f));
        if !p.active() {
            // Not playing yet (or the engine is missing): just say why.
            let msg = match &self.engine_got {
                Some(got) => {
                    ctx.request_repaint_after(Duration::from_millis(250));
                    let mb = |b: u64| b as f64 / 1_048_576.0;
                    format!(
                        "Getting the player engine (libmpv, LGPL) — {:.1} of {:.1} MB, only this once…",
                        mb(got.load(std::sync::atomic::Ordering::Relaxed)),
                        mb(player::engine::SIZE)
                    )
                }
                None => p.error.clone().unwrap_or_default(),
            };
            egui::Panel::bottom("player").show(root, |ui| {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(msg).weak());
                    if ui.small_button("x").clicked() {
                        p.error = None;
                    }
                });
            });
            return;
        }
        let pui = &mut self.pui;
        egui::Panel::bottom("player").show(root, |ui| player::ui::bar(ui, p, pui, &files));
        if pui.show_playlist {
            egui::Panel::right("playlist").resizable(true).default_size(300.0).size_range(220.0..=520.0).show(root, |ui| {
                player::ui::playlist(ui, p, &files)
            });
        }
        if pui.show_sound {
            let mut open = true;
            player::ui::sound(ctx, p, &mut open);
            pui.show_sound = open;
        }
    }

    fn drain_inbox(&mut self, ctx: &egui::Context) {
        // Info files fetched for the Details panel (ignored if it was closed meanwhile).
        let infos = std::mem::take(&mut self.inbox.lock().unwrap().info);
        for (hash, file, got) in infos {
            if let Some(d) = self.details.as_mut().filter(|d| d.hash == hash) {
                d.set_info(file, got);
            }
        }
        let ready = std::mem::take(&mut self.inbox.lock().unwrap().tracks);
        if !ready.is_empty() {
            self.engine_got = None; // the engine fetch (if any) is over, either way
        }
        for (hash, name, r) in ready {
            match r {
                Ok((tracks, folder)) => {
                    let (done, len) = self.file_state(&hash);
                    let files = player::ui::Files { done: &done, len: &len };
                    let p = self.player.as_mut().unwrap();
                    let queue = player::audio::Queue {
                        hash,
                        torrent: name,
                        folder,
                        order: (0..tracks.len()).collect(),
                        tracks,
                        pos: None,
                        shuffle: p.queue.shuffle,
                        repeat: p.queue.repeat,
                    };
                    p.error = None;
                    // Flux MoE may have asked for a particular track.
                    let at = self.play_from.take().and_then(|want| {
                        let w = want.to_lowercase();
                        queue.tracks.iter().position(|t| t.title.to_lowercase().contains(&w) || t.path.to_lowercase().contains(&w))
                    });
                    p.start(queue, at.unwrap_or(0), &|f| files.complete(f));
                    self.pui.show_playlist = true;
                }
                Err(e) => {
                    if let Some(p) = self.player.as_mut() {
                        p.error = Some(format!("{name}: {e}"));
                    }
                }
            }
        }
        if let Some(r) = self.inbox.lock().unwrap().vpn_test.take() {
            self.vpn_testing = false;
            self.vpn_note = Some(match r {
                Ok(ms) => format!("Relay answered: authenticated handshake + encrypted ping in {ms} ms."),
                Err(e) => format!("Relay test failed: {e}"),
            });
        }
        let feed_results = std::mem::take(&mut self.inbox.lock().unwrap().feeds);
        let mut dirty = false;
        for (url, res) in feed_results {
            self.fv.in_flight = self.fv.in_flight.saturating_sub(1);
            match res {
                Ok(items) => {
                    let stamp = format!("{} items · {}", items.len(), clock());
                    if let Some(f) = self.store.feeds.iter_mut().find(|f| f.url == url) {
                        let auto: Vec<rss::Item> = f.absorb(&items).into_iter().cloned().collect();
                        dirty = true;
                        let cookie = rss::feed_cookie(&f.cookie, &f.url);
                        for it in auto {
                            self.fv.status.insert(url.clone(), format!("auto-downloading “{}”", it.title));
                            self.start(ctx, it.title.clone(), Source::from_feed(it.link.clone(), &cookie));
                        }
                    }
                    self.fv.status.entry(url.clone()).and_modify(|s| {
                        if !s.starts_with("auto") {
                            *s = stamp.clone();
                        }
                    });
                    if let Some(name) = self.store.feeds.iter().find(|f| f.url == url).map(|f| f.name.clone()) {
                        self.feed_hist.absorb(&name, &url, &items, history::now());
                    }
                    self.fv.items.insert(url, items);
                }
                Err(e) => {
                    // The last good items stay on screen; only the status says it failed.
                    self.fv.status.insert(url, format!("error: {e} — trying again in 1 min"));
                    self.fv.retry_at.get_or_insert(Instant::now() + FEED_RETRY);
                }
            }
        }
        if dirty {
            self.save_feeds();
        }
        // History can be tens of thousands of items: written at most once a minute (and on exit).
        if self.feed_hist.dirty && self.feed_hist_saved.elapsed() >= Duration::from_secs(60) {
            self.save_feed_history();
        }

        let mut ib = self.inbox.lock().unwrap();
        if let Some(u) = ib.update.take() {
            self.upd = u;
        }
        for t in ib.added.drain(..) {
            let id = t.handle.id();
            if !self.transfers.iter().any(|x| x.handle.id() == id) {
                let e = self.ledger.entries.entry(t.handle.info_hash().as_string()).or_default();
                e.name = t.name.clone();
                e.folder = t.folder.clone();
                self.transfers.push(t);
            }
        }
        let mut meta_dirty = false;
        for (key, res) in ib.meta.drain(..) {
            self.meta_pending.remove(&key);
            match res {
                Ok(info) => {
                    self.meta_cache.put(key, info);
                    meta_dirty = true;
                }
                // Bad key / limit / network: show it, do not cache it.
                Err(e) => {
                    self.meta_error = Some(e);
                    self.meta_error_at = Some(Instant::now());
                }
            }
        }
        if meta_dirty {
            self.meta_cache.save();
            self.meta_version += 1;
        }
        for (id, res) in ib.posters.drain(..) {
            let tex = res.ok().and_then(|b| poster_texture(ctx, &id, &b));
            self.textures.insert(id, tex);
        }
        for (url, res) in ib.login.drain(..) {
            self.fv.login_busy = false;
            match res {
                Ok(cookie) => {
                    if let Some(f) = self.store.feeds.iter_mut().find(|f| f.url == url) {
                        f.cookie = cookie;
                    }
                    self.fv.login_pass.clear();
                    self.fv.status.insert(url, "logged in ✔ — downloads will now work".into());
                    let _ = self.store.save();
                }
                Err(e) => {
                    self.fv.status.insert(url, format!("error: {e}"));
                }
            }
        }
        for e in &ib.errors {
            // The site wants a login: open that feed's login box.
            if e.contains("wants you logged in") {
                if let Some(i) = self.fv.last_pick.as_ref().and_then(|u| self.store.feeds.iter().position(|f| &f.url == u)) {
                    self.fv.selected = Some(i);
                    self.view = View::Feeds;
                }
            }
        }
        self.errors.extend(ib.errors.drain(..));
    }
}

/// Work for the ratings worker: one request at a time, politely spaced.
enum MetaJob {
    Lookup { key: String, query: meta::Query, api_key: String },
    Poster { imdb_id: String, url: String },
}

/// The single background worker for OMDb lookups and poster downloads.
fn spawn_meta_worker(
    rt: &tokio::runtime::Runtime,
    inbox: Arc<Mutex<Inbox>>,
    ctx: egui::Context,
) -> tokio::sync::mpsc::UnboundedSender<MetaJob> {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<MetaJob>();
    rt.spawn(async move {
        let http = reqwest::Client::builder()
            .user_agent(concat!("ZenTorrent/", env!("CARGO_PKG_VERSION"), " (+ratings via OMDb)"))
            .https_only(true)
            .timeout(Duration::from_secs(20))
            .build()
            .expect("http client");
        while let Some(job) = rx.recv().await {
            match job {
                MetaJob::Lookup { key, query, api_key } => {
                    let r = meta::lookup(&http, &api_key, &query).await.map_err(|e| format!("{e:#}"));
                    inbox.lock().unwrap().meta.push((key, r));
                    ctx.request_repaint();
                    tokio::time::sleep(Duration::from_millis(300)).await;
                }
                MetaJob::Poster { imdb_id, url } => {
                    let r = meta::poster(&http, &imdb_id, &url).await.map_err(|e| format!("{e:#}"));
                    inbox.lock().unwrap().posters.push((imdb_id, r));
                    ctx.request_repaint();
                }
            }
        }
    });
    tx
}

/// Where "Save to" is remembered between runs.
fn prefs_path() -> PathBuf {
    seed::data_dir().join("prefs.json")
}

/// The "Save to" folder the user last chose, if it still exists.
fn saved_folder() -> Option<PathBuf> {
    let v: serde_json::Value = serde_json::from_slice(&std::fs::read(prefs_path()).ok()?).ok()?;
    Some(PathBuf::from(v["save_to"].as_str()?)).filter(|p| p.is_dir())
}

fn remember_folder(dir: &std::path::Path) {
    let path = prefs_path();
    let mut v: serde_json::Value = std::fs::read(&path).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_else(|| serde_json::json!({}));
    v["save_to"] = serde_json::json!(dir.to_string_lossy());
    let _ = std::fs::create_dir_all(seed::data_dir());
    let tmp = path.with_extension("json.tmp");
    // Written whole, then renamed: never a half-written prefs file.
    if std::fs::write(&tmp, serde_json::to_vec_pretty(&v).unwrap_or_default()).is_ok() {
        let _ = std::fs::rename(&tmp, &path);
    }
}

/// The folder a torrent's files go in: its own folder (named after the
/// torrent) when it has more than one file, else the download folder itself.
fn torrent_folder(base: &std::path::Path, files: usize, name: Option<&str>) -> PathBuf {
    match name.map(safe_folder_name) {
        Some(n) if files > 1 && !n.is_empty() => base.join(n),
        _ => base.to_path_buf(),
    }
}

/// A torrent name as a folder name that is safe on Windows and Linux: no path
/// separators or `..`, no characters Windows forbids, no trailing dots/spaces,
/// not a reserved device name, and not absurdly long.
fn safe_folder_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .map(|c| if c.is_control() || r#"<>:"/\|?*"#.contains(c) { '_' } else { c })
        .take(150)
        .collect();
    let trimmed = cleaned.trim().trim_end_matches(['.', ' ']).to_string();
    let upper = trimmed.split('.').next().unwrap_or("").to_ascii_uppercase();
    const RESERVED: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8", "COM9", "LPT1", "LPT2",
        "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];
    if trimmed.is_empty() || trimmed.chars().all(|c| c == '.' || c == '_') {
        String::new()
    } else if RESERVED.contains(&upper.as_str()) {
        format!("_{trimmed}")
    } else {
        trimmed
    }
}

#[cfg(test)]
mod folder_tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn albums_get_their_own_folder_single_files_do_not() {
        let base = Path::new("/home/v/Downloads");
        assert_eq!(torrent_folder(base, 12, Some("Some Artist - Album (2024) [FLAC]")), base.join("Some Artist - Album (2024) [FLAC]"));
        assert_eq!(torrent_folder(base, 1, Some("debian-13.7.0-amd64-netinst.iso")), base, "one file: straight into Downloads");
        assert_eq!(torrent_folder(base, 5, None), base, "no name: fall back to the download folder");
    }

    #[test]
    fn names_are_safe_folder_names() {
        assert_eq!(safe_folder_name("A/B\\C:D*E?F\"G<H>I|J"), "A_B_C_D_E_F_G_H_I_J");
        assert_eq!(safe_folder_name("../../etc"), ".._.._etc", "no path traversal: separators are gone");
        assert_eq!(safe_folder_name("Album.  "), "Album", "Windows strips trailing dots/spaces");
        assert_eq!(safe_folder_name(".."), "");
        assert_eq!(safe_folder_name("con"), "_con", "reserved device name");
        assert_eq!(safe_folder_name("CON.flac"), "_CON.flac");
        assert_eq!(safe_folder_name(&"x".repeat(400)).len(), 150);
        assert_eq!(torrent_folder(Path::new("/d"), 3, Some("..")), Path::new("/d"), "an unusable name stays in the base");
    }
}

/// Rows drawn at most; past that the list says how many more matched.
const MAX_ROWS: usize = 300;

const SEARCH_HELP: &str = "Search your feeds:\n\
    trance live         both words (title counts most)\n\
    tran                beginnings of words: trance, transmission…\n\
    tarnce              one typo is forgiven\n\
    \"group therapy\"     exact phrase\n\
    -remix              leave out\n\
    genre:trance        genre or tag   (also cat:music, feed:torrentleech)\n\
    seeders>10  size<2gb  grabs>=100\n\
    free                freeleech only\n\
    Sort by Most active = downloads per hour since ZenTorrent first saw it.";

/// Swarm numbers after a feed item: seeders (coloured by health), leechers,
/// grabs, activity and freeleech. Only what the feed actually reports.
fn swarm(ui: &mut egui::Ui, e: &history::Entry, now: u64) {
    if let Some(s) = e.seeders {
        let col = if s >= 10 {
            egui::Color32::from_rgb(90, 200, 120)
        } else if s >= 1 {
            egui::Color32::from_rgb(230, 190, 80)
        } else {
            egui::Color32::from_rgb(230, 110, 100)
        };
        ui.label(egui::RichText::new(format!("{s} seeders")).small().strong().color(col)).on_hover_text(format!(
            "{s} seeders now{}",
            if e.max_seeders > s { format!(" · peak {}", e.max_seeders) } else { String::new() }
        ));
    }
    if let Some(l) = e.leechers {
        ui.label(egui::RichText::new(format!("{l} leechers")).small().weak());
    }
    if let Some(g) = e.grabs {
        ui.label(egui::RichText::new(format!("{g} grabs")).small().weak()).on_hover_text("times downloaded (grabs / snatched)");
    }
    if let Some(a) = e.activity(now).filter(|a| *a >= 0.5) {
        ui.label(egui::RichText::new(format!("+{a:.0}/h")).small().strong().color(egui::Color32::from_rgb(240, 150, 60)))
            .on_hover_text("downloads per hour since ZenTorrent first saw it");
    }
    if e.freeleech {
        ui.label(egui::RichText::new(" FREE ").small().strong().color(egui::Color32::BLACK).background_color(egui::Color32::from_rgb(90, 200, 120)))
            .on_hover_text("freeleech: downloading it doesn't count against your ratio");
    }
}

/// Small chips after a feed item's title: the tracker's category
/// ("Movies/HD", "Music/MP3") and, once OMDb knows the title, its genres
/// ("Action, Sci-Fi" → two chips).
fn tags(ui: &mut egui::Ui, category: &str, genre: &str) {
    let chip = |ui: &mut egui::Ui, text: &str, bg: egui::Color32, fg: egui::Color32| {
        ui.label(egui::RichText::new(format!(" {text} ")).small().color(fg).background_color(bg))
    };
    let dark = ui.visuals().dark_mode;
    if !category.is_empty() {
        let (bg, fg) = if dark {
            (egui::Color32::from_gray(60), egui::Color32::from_gray(220))
        } else {
            (egui::Color32::from_gray(222), egui::Color32::from_gray(40))
        };
        chip(ui, category, bg, fg).on_hover_text("tracker category");
    }
    for g in genre.split(',').map(str::trim).filter(|g| !g.is_empty()) {
        let (bg, fg) = if dark {
            (egui::Color32::from_rgb(38, 70, 110), egui::Color32::from_rgb(200, 225, 255))
        } else {
            (egui::Color32::from_rgb(214, 232, 252), egui::Color32::from_rgb(20, 60, 110))
        };
        chip(ui, g, bg, fg).on_hover_text("genre or tag (from the feed, or IMDb via OMDb)");
    }
}

/// Decode a poster into a small texture.
fn poster_texture(ctx: &egui::Context, id: &str, bytes: &[u8]) -> Option<egui::TextureHandle> {
    let img = image::load_from_memory(bytes).ok()?.thumbnail(120, 180).to_rgba8();
    let size = [img.width() as usize, img.height() as usize];
    let ci = egui::ColorImage::from_rgba_unmultiplied(size, img.as_raw());
    Some(ctx.load_texture(format!("poster-{id}"), ci, egui::TextureOptions::LINEAR))
}

enum Source {
    Link(String),
    Bytes(Vec<u8>),
    /// A .torrent URL from a feed: fetched by ZenTorrent (User-Agent +
    /// the feed's login cookie) and checked before it reaches librqbit.
    Fetch { url: String, cookie: String },
}

impl Source {
    fn from_feed(link: String, cookie: &str) -> Self {
        if link.starts_with("http://") || link.starts_with("https://") {
            Source::Fetch { url: link, cookie: rss::clean_cookie(cookie) }
        } else {
            Source::Link(link)
        }
    }
}

impl eframe::App for App {
    fn ui(&mut self, root: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = &root.ctx().clone();
        self.drain_inbox(ctx);
        self.mcp_tick(ctx);
        self.vpn_window(ctx);
        let due = match self.fv.last_refresh {
            None => true,
            Some(t) => t.elapsed() >= Duration::from_secs(self.store.refresh_minutes.max(1) * 60),
        };
        if due && !self.store.feeds.is_empty() {
            self.fv.retry_at = None;
            self.refresh_feeds(ctx, None);
        } else if self.fv.retry_at.is_some_and(|t| Instant::now() >= t) && self.fv.in_flight == 0 {
            self.fv.retry_at = None;
            let failed: Vec<usize> = (0..self.store.feeds.len())
                .filter(|&i| self.fv.status.get(&self.store.feeds[i].url).is_some_and(|s| s.starts_with("error")))
                .collect();
            for i in failed {
                self.refresh_feeds(ctx, Some(i));
            }
        }
        // Update check at start-up and every 6 hours (never during an install).
        let upd_due = self.upd_checked.is_none_or(|t| t.elapsed() >= Duration::from_secs(6 * 3600));
        if upd_due && !matches!(self.upd, Upd::Installing(_) | Upd::Installed(_) | Upd::Available(_)) {
            self.check_update(ctx);
        }
        let busy = self.inbox.lock().unwrap().busy;

        egui::Panel::top("top").show(root, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.heading("⚡ ZenTorrent");
                ui.label(egui::RichText::new("egui · librqbit").weak());
                ui.add_space(16.0);
                ui.selectable_value(&mut self.view, View::Downloads, format!("Downloads ({})", self.transfers.len()));
                ui.selectable_value(&mut self.view, View::Seeding, "Seeding & ratio");
                ui.selectable_value(&mut self.view, View::Feeds, format!("RSS feeds ({})", self.store.feeds.len()));
                ui.selectable_value(&mut self.view, View::Moe, "✨ Flux MoE").on_hover_text("Your media assistant: overview, feed search, play, playlists, tidy folders — runs on this computer");
                ui.add_space(8.0);
                self.vpn_badge(ui);
                self.update_ui(ui, ctx);
            });
            ui.add_space(4.0);

            // ── download folder ──────────────────────────────────────
            ui.horizontal(|ui| {
                ui.label("Save to:");
                ui.monospace(self.folder.display().to_string());
                if ui.button("Choose folder…").clicked() {
                    if let Some(dir) = rfd::FileDialog::new()
                        .set_directory(&self.folder)
                        .pick_folder()
                    {
                        remember_folder(&dir);
                        self.folder = dir;
                    }
                }
            });

            // ── add by link or file ──────────────────────────────────
            ui.horizontal(|ui| {
                ui.label("Magnet / URL:");
                let edit = ui.add(
                    egui::TextEdit::singleline(&mut self.link)
                        .hint_text("magnet:?xt=urn:btih:…  or  https://…/file.torrent")
                        .desired_width(ui.available_width() - 260.0),
                );
                let go = ui.button("⬇ Download").clicked()
                    || (edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
                if go && !self.link.trim().is_empty() {
                    let link = self.link.trim().to_string();
                    self.start(ctx, link.clone(), Source::Link(link));
                    self.link.clear();
                }
                if ui.button("Open .torrent…").clicked() {
                    if let Some(path) = rfd::FileDialog::new()
                        .add_filter("Torrent", &["torrent"])
                        .pick_file()
                    {
                        match std::fs::read(&path) {
                            Ok(bytes) => {
                                let label = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
                                self.start(ctx, label, Source::Bytes(bytes));
                            }
                            Err(e) => self.errors.push(format!("{}: {e}", path.display())),
                        }
                    }
                }
            });
            ui.add_space(6.0);
        });

        if ctx.input(|i| i.modifiers.command && i.key_pressed(egui::Key::F)) {
            self.sidebar.focus_search = true;
        }
        self.facts = self.collect_facts();
        let mut actions = Vec::new();
        egui::Panel::left("sidebar").resizable(true).default_size(250.0).size_range(210.0..=400.0).show(root, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                let floor = self.ledger.ratio_floor();
                actions = self.sidebar.ui(ui, &self.facts, &self.ledger.labels, floor);
            });
        });
        self.sidebar_actions(actions);
        self.player_ui(root, ctx);

        self.tick_ledger();
        // Drawn before the central panels; as a modal it sits above all of them.
        self.details_ui(ctx);
        if self.view == View::Seeding {
            egui::CentralPanel::default().show(root, |ui| self.seeding_ui(ui));
            ctx.request_repaint_after(Duration::from_millis(1000));
            return;
        }
        if self.view == View::Feeds {
            egui::CentralPanel::default().show(root, |ui| self.feeds_ui(ui, ctx));
            ctx.request_repaint_after(Duration::from_millis(500));
            return;
        }
        if self.view == View::Moe {
            let mut actions = Vec::new();
            egui::CentralPanel::default().show(root, |ui| actions = self.moe.ui(ui, ctx, &self.rt));
            if let Some(text) = self.moe.take_pending() {
                let lib = self.moe_library();
                self.moe.send(text, &self.rt, ctx, lib);
            }
            self.moe_actions(ctx, actions);
            return;
        }

        egui::CentralPanel::default().show(root, |ui| {
            if let Some(err) = &self.session_error {
                ui.colored_label(egui::Color32::LIGHT_RED, err);
            }
            if busy > 0 {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label(format!("Fetching torrent metadata ({busy})…"));
                });
            }
            if !self.errors.is_empty() {
                let mut clear = false;
                let mut to_login = false;
                ui.horizontal_wrapped(|ui| {
                    let last = self.errors.last().unwrap();
                    ui.colored_label(egui::Color32::LIGHT_RED, last);
                    if last.contains("wants you logged in") {
                        to_login = ui.button("Log in to tracker…").clicked();
                    }
                    clear = ui.small_button("x").clicked();
                });
                if to_login {
                    self.open_login();
                }
                if clear {
                    self.errors.clear();
                }
            }
            if self.transfers.is_empty() && busy == 0 {
                ui.add_space(40.0);
                ui.vertical_centered(|ui| {
                    ui.label(egui::RichText::new("No downloads yet").size(18.0));
                    ui.label("Paste a magnet link above, open a .torrent file, or add an RSS feed.");
                });
            }

            let shown = (0..self.transfers.len()).filter(|&i| self.visible(i)).count();
            if self.sidebar.filter.is_active() && !self.transfers.is_empty() {
                let mut clear = false;
                ui.horizontal(|ui| {
                    ui.label(
                        egui::RichText::new(format!(
                            "Showing {shown} of {}  ·  {}",
                            self.transfers.len(),
                            self.sidebar.filter.describe()
                        ))
                        .weak(),
                    );
                    clear = ui.small_button("Show all").clicked();
                });
                if clear {
                    self.sidebar.filter = Default::default();
                }
            }
            // What each torrent holds, once its file list is known (kept: it doesn't change).
            for t in &self.transfers {
                let hash = t.handle.info_hash().as_string();
                if self.media.contains_key(&hash) {
                    continue;
                }
                let files: Vec<(String, u64)> = t
                    .handle
                    .with_metadata(|m| m.file_infos.iter().map(|f| (f.relative_filename.to_string_lossy().into_owned(), f.len)).collect())
                    .unwrap_or_default();
                if !files.is_empty() {
                    self.media.insert(hash, listview::media_of(files.iter().map(|(p, l)| (p.as_str(), *l))));
                }
            }
            let medias: Vec<listview::Media> =
                self.transfers.iter().map(|t| self.media.get(&t.handle.info_hash().as_string()).copied().unwrap_or_default()).collect();
            let mut counts = [0usize; 3];
            for i in (0..self.transfers.len()).filter(|&i| self.visible(i)) {
                counts[medias[i] as usize] += 1;
            }
            if !self.transfers.is_empty() {
                self.list.toolbar(ui, counts);
                ui.add_space(2.0);
            }
            let order = {
                let keys: Vec<Option<listview::Key>> = (0..self.transfers.len())
                    .map(|i| {
                        let f = self.facts.get(i).filter(|_| self.visible(i))?;
                        let s = self.transfers[i].handle.stats();
                        Some(listview::Key {
                            name: &f.name,
                            size: f.size,
                            progress: if s.total_bytes > 0 { s.progress_bytes as f64 / s.total_bytes as f64 } else { 0.0 },
                            speed: f.down_mibs + f.up_mibs,
                            ratio: f.ratio,
                            status: if f.error { 0 } else if !f.finished && !f.paused { 1 } else if f.paused { 2 } else if f.live { 3 } else { 4 },
                            media: medias[i],
                        })
                    })
                    .collect();
                self.list.arrange(&keys)
            };
            // Artwork for the thumbnails (films and episodes, when ratings are on).
            let posters: HashMap<usize, egui::TextureHandle> = if self.list.layout == listview::Layout::Thumbs {
                let names: Vec<(usize, String)> =
                    order.iter().filter(|&&i| medias[i] == listview::Media::Video).map(|&i| (i, self.transfers[i].name.clone())).collect();
                names.into_iter().filter_map(|(i, n)| self.poster_for(&n).map(|t| (i, t))).collect()
            } else {
                HashMap::new()
            };

            let mut remove = None;
            let mut relabel: Vec<(String, String, bool)> = Vec::new();
            let mut open_details: Option<String> = None;
            let mut play_req: Option<String> = None;
            let labels = self.ledger.labels.clone();
            let layout = self.list.layout;
            egui::ScrollArea::vertical().show(ui, |ui| {
                let mut out = RowOut { relabel: &mut relabel, open_details: &mut open_details, play: &mut play_req, remove: &mut remove };
                match layout {
                    listview::Layout::Cards => {
                        for &i in &order {
                            let (t, Some(f)) = (&self.transfers[i], self.facts.get(i)) else { continue };
                            let mut gone = false;
                            transfer_row(ui, &self.rt, self.session.as_ref(), t, f, &labels, out.relabel, out.open_details, out.play, || gone = true);
                            if gone {
                                *out.remove = Some(i);
                            }
                            ui.add_space(4.0);
                        }
                    }
                    listview::Layout::Compact => {
                        ui.spacing_mut().item_spacing.y = 1.0;
                        for (n, &i) in order.iter().enumerate() {
                            let (t, Some(f)) = (&self.transfers[i], self.facts.get(i)) else { continue };
                            compact_row(ui, &self.rt, self.session.as_ref(), t, f, medias[i], n % 2 == 1, &labels, i, &mut out);
                        }
                    }
                    listview::Layout::Thumbs => {
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing = egui::vec2(12.0, 12.0);
                            for &i in &order {
                                let (t, Some(f)) = (&self.transfers[i], self.facts.get(i)) else { continue };
                                thumb_tile(ui, &self.rt, self.session.as_ref(), t, f, medias[i], posters.get(&i), &labels, i, &mut out);
                            }
                        });
                    }
                }
                if order.is_empty() && !self.transfers.is_empty() {
                    ui.add_space(30.0);
                    ui.vertical_centered(|ui| {
                        ui.label(egui::RichText::new("Nothing matches this filter").size(16.0));
                        if shown > 0 {
                            ui.label(egui::RichText::new("(the type filter above hides the rest)").weak());
                        }
                    });
                }
            });
            if !relabel.is_empty() {
                for (hash, label, on) in relabel {
                    self.ledger.set_label(&hash, &label, on);
                }
                self.save_ledger();
            }
            if let Some(hash) = play_req {
                self.play_torrent(ctx, hash);
            }
            if let Some(hash) = open_details {
                self.details = Some(details::Details::new(hash));
            }
            if let Some(i) = remove {
                let t = self.transfers.remove(i);
                self.ledger.entries.remove(&t.handle.info_hash().as_string());
                self.speed_hist.remove(&t.handle.info_hash().as_string());
                self.save_ledger();
                if let Some(s) = self.session.clone() {
                    self.rt.spawn(async move {
                        let _ = s.delete(t.handle.id().into(), false).await;
                    });
                }
            }
        });

        // Live progress: redraw twice a second while anything is running.
        ctx.request_repaint_after(Duration::from_millis(500));
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.tick_ledger();
        let _ = self.ledger.save();
        if self.feed_hist.dirty {
            let _ = self.feed_hist.save();
        }
    }
}

impl App {
    fn feeds_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        if !self.errors.is_empty() {
            let mut clear = false;
            let mut to_login = false;
            ui.horizontal_wrapped(|ui| {
                let last = self.errors.last().unwrap();
                ui.colored_label(egui::Color32::LIGHT_RED, last);
                if last.contains("wants you logged in") {
                    to_login = ui.button("Log in to tracker…").clicked();
                }
                clear = ui.small_button("x").clicked();
            });
            if to_login {
                self.open_login();
            }
            if clear {
                self.errors.clear();
            }
        }

        // ── add a feed ───────────────────────────────────────────────
        ui.horizontal(|ui| {
            ui.label("Name:");
            ui.add(egui::TextEdit::singleline(&mut self.fv.new_name).hint_text("My tracker").desired_width(120.0));
            ui.label("Feed URL:");
            ui.add(
                egui::TextEdit::singleline(&mut self.fv.new_url)
                    .hint_text("https://…/rss.php?passkey=…")
                    .password(true)
                    .desired_width(ui.available_width() - 90.0),
            );
            let url = self.fv.new_url.trim().to_string();
            let ok = url.starts_with("http://") || url.starts_with("https://");
            if ui.add_enabled(ok, egui::Button::new("+ Add feed")).clicked() {
                if self.store.feeds.iter().any(|f| f.url == url) {
                    self.errors.push("that feed is already in the list".into());
                } else {
                    let name = match self.fv.new_name.trim() {
                        "" => rss::display_url(&url).split_whitespace().next().unwrap_or("feed").to_string(),
                        n => n.to_string(),
                    };
                    self.store.feeds.push(rss::Feed { name, url, ..Default::default() });
                    self.save_feeds();
                    let i = self.store.feeds.len() - 1;
                    self.fv.selected = Some(i);
                    self.refresh_feeds(ctx, Some(i));
                    self.fv.new_name.clear();
                    self.fv.new_url.clear();
                }
            }
        });
        ui.label(
            egui::RichText::new(
                "Feed links from private trackers contain your passkey. ZenTorrent hides it on screen and stores it only in your own config folder.",
            )
            .weak()
            .small(),
        );
        ui.horizontal(|ui| {
            ui.label("Check feeds every");
            let mut m = self.store.refresh_minutes;
            if ui.add(egui::DragValue::new(&mut m).range(5..=1440).suffix(" min")).changed() {
                self.store.refresh_minutes = m;
                self.save_feeds();
            }
            if ui.add_enabled(self.fv.in_flight == 0, egui::Button::new("⟳ Refresh all")).clicked() {
                self.refresh_feeds(ctx, None);
            }
            if self.fv.in_flight > 0 {
                ui.spinner();
            }
        });
        ui.horizontal_wrapped(|ui| {
            let mut changed = ui.checkbox(&mut self.store.show_ratings, "Posters & ratings").changed();
            if self.store.show_ratings {
                ui.label("OMDb key:");
                let r = ui.add(
                    egui::TextEdit::singleline(&mut self.store.omdb_key)
                        .password(true)
                        .hint_text("free key")
                        .desired_width(110.0),
                );
                if r.changed() {
                    self.meta_error = None;
                }
                changed |= r.lost_focus();
                if self.store.omdb_key.trim().is_empty() {
                    ui.hyperlink_to("get a free key (1,000/day)", "https://www.omdbapi.com/apikey.aspx");
                } else {
                    ui.label(
                        egui::RichText::new(format!(
                            "IMDb · Rotten Tomatoes · Metascore via OMDb (HTTPS) · {}/{} lookups today",
                            self.meta_cache.used_today(),
                            meta::DAILY_BUDGET
                        ))
                        .weak()
                        .small(),
                    );
                }
                // Retry by itself 5 minutes after a refusal (key just activated, network back).
                if self.meta_error_at.is_some_and(|t| t.elapsed() >= Duration::from_secs(300)) {
                    self.meta_error = None;
                    self.meta_error_at = None;
                }
                let mut retry = false;
                if let Some(e) = &self.meta_error {
                    ui.colored_label(egui::Color32::LIGHT_RED, e);
                    if e.contains("Invalid API key") {
                        ui.label(egui::RichText::new("new key? open the activation link in OMDb's email").weak().small());
                    }
                    retry = ui.small_button("Retry").clicked();
                }
                if retry {
                    self.meta_error = None;
                }
            }
            if changed {
                self.save_feeds();
            }
        });
        ui.separator();

        if self.store.feeds.is_empty() {
            ui.add_space(30.0);
            ui.vertical_centered(|ui| {
                ui.label(egui::RichText::new("No feeds yet").size(18.0));
                ui.label("Paste an RSS link above. Torrent-site feeds, distro release feeds and Torznab all work.");
            });
            return;
        }

        // ── feed list ───────────────────────────────────────────────
        let mut remove = None;
        let mut refresh_one = None;
        let mut changed = false;
        let mut login_req: Option<(String, String, String)> = None;
        for (i, f) in self.store.feeds.iter_mut().enumerate() {
            let sel = self.fv.selected == Some(i);
            ui.horizontal(|ui| {
                if ui.selectable_label(sel, egui::RichText::new(&f.name).strong()).clicked() {
                    self.fv.selected = if sel { None } else { Some(i) };
                }
                ui.label(egui::RichText::new(rss::display_url(&f.url)).weak().small());
                let st = self.fv.status.get(&f.url).cloned().unwrap_or_default();
                let col = if st.starts_with("error") { egui::Color32::LIGHT_RED } else { ui.visuals().weak_text_color() };
                ui.label(egui::RichText::new(st).small().color(col));
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("Remove").clicked() {
                        remove = Some(i);
                    }
                    if ui.small_button("⟳").on_hover_text("Refresh this feed").clicked() {
                        refresh_one = Some(i);
                    }
                });
            });
            if sel {
                ui.horizontal(|ui| {
                    changed |= ui.checkbox(&mut f.auto_enabled, "Auto-download new items matching").changed();
                    let r = ui.add(
                        egui::TextEdit::singleline(&mut f.auto_regex)
                            .hint_text("e.g.  debian.*netinst|ubuntu")
                            .desired_width(260.0),
                    );
                    changed |= r.lost_focus();
                    match rss::rule(f) {
                        Some(Err(_)) => {
                            ui.colored_label(egui::Color32::LIGHT_RED, "invalid pattern");
                        }
                        Some(Ok(_)) => {
                            ui.label(egui::RichText::new("only items that appear after now").weak().small());
                        }
                        None => {}
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Log in to the tracker:").on_hover_text(
                        "Needed when the site says \"registered users only\". ZenTorrent logs in once and keeps \
                         only the session cookie. Your password is not saved.",
                    );
                    ui.add(egui::TextEdit::singleline(&mut self.fv.login_user).hint_text("username").desired_width(120.0));
                    let pw = ui.add(
                        egui::TextEdit::singleline(&mut self.fv.login_pass).password(true).hint_text("password").desired_width(120.0),
                    );
                    let can = !self.fv.login_busy && !self.fv.login_user.trim().is_empty() && !self.fv.login_pass.is_empty();
                    let go = ui.add_enabled(can, egui::Button::new("Log in")).clicked()
                        || (can && pw.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
                    if go {
                        login_req = Some((f.url.clone(), self.fv.login_user.trim().to_string(), self.fv.login_pass.clone()));
                    }
                    if self.fv.login_busy {
                        ui.spinner();
                    }
                    ui.label(
                        egui::RichText::new(if f.cookie.trim().is_empty() { "not logged in" } else { "logged in ✔" })
                            .weak()
                            .small(),
                    );
                });
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("…or paste a browser cookie:").weak().small()).on_hover_text(
                        "In your browser, logged in to the tracker: F12 → Network → reload → click any request → \
                         Request Headers → copy the value of Cookie (looks like  uid=…; pass=…).",
                    );
                    let r = ui.add(
                        egui::TextEdit::singleline(&mut f.cookie)
                            .password(true)
                            .hint_text("uid=…; pass=…")
                            .desired_width(260.0),
                    );
                    if r.lost_focus() {
                        f.cookie = rss::clean_cookie(&f.cookie);
                        changed = true;
                    }
                });
            }
        }
        if changed {
            self.save_feeds();
        }
        if let Some((url, user, pass)) = login_req {
            self.fv.login_busy = true;
            self.fv.status.insert(url.clone(), "logging in…".into());
            let (inbox, ctx) = (self.inbox.clone(), ctx.clone());
            self.rt.spawn(async move {
                let res = rss::login(&url, &user, &pass).await.map_err(|e| format!("{e:#}"));
                inbox.lock().unwrap().login.push((url, res));
                ctx.request_repaint();
            });
        }
        if let Some(i) = refresh_one {
            self.refresh_feeds(ctx, Some(i));
        }
        if let Some(i) = remove {
            let f = self.store.feeds.remove(i);
            self.fv.items.remove(&f.url);
            self.fv.status.remove(&f.url);
            self.fv.selected = None;
            self.save_feeds();
        }
        ui.separator();

        // ── items: search over what the feeds list now, or the whole history ─
        let live_n = self.feed_hist.entries.iter().filter(|e| e.in_feed).count();
        let hist_n = self.feed_hist.entries.len();
        let now = history::now();
        ui.horizontal_wrapped(|ui| {
            ui.selectable_value(&mut self.fv.scope_history, false, format!("In the feeds ({live_n})"))
                .on_hover_text("What your feeds list right now");
            ui.selectable_value(&mut self.fv.scope_history, true, format!("History ({hist_n})"))
                .on_hover_text("Everything ZenTorrent has seen in your feeds, also after it scrolled out of them");
            ui.add_space(8.0);
            ui.add(
                egui::TextEdit::singleline(&mut self.fv.query)
                    .hint_text("search: trance   genre:trance   seeders>10   size<2gb   -remix   free")
                    // Leave room for the sort menu in a narrow window.
                    .desired_width((ui.available_width() - 150.0).clamp(160.0, 420.0)),
            )
            .on_hover_text(SEARCH_HELP);
            egui::ComboBox::from_id_salt("rss-sort").selected_text(self.fv.sort.label()).show_ui(ui, |ui| {
                for s in search::Sort::ALL {
                    ui.selectable_value(&mut self.fv.sort, s, s.label());
                }
            });
        });
        ui.horizontal_wrapped(|ui| {
            let mut on = self.feed_hist.enabled;
            if ui
                .checkbox(&mut on, "Remember every item (history)")
                .on_hover_text(
                    "Feeds only list their newest items. With this on, ZenTorrent keeps every item it has seen, \
                     with its seeders and downloads over time, so search reaches back to the day you switched it on. \
                     Stored only on this computer.",
                )
                .changed()
            {
                self.feed_hist.set_enabled(on);
                self.save_feed_history();
            }
            if self.feed_hist.enabled {
                if let Some(since) = self.feed_hist.since() {
                    ui.label(
                        egui::RichText::new(format!("{hist_n} items, collected over {}", history::ago(now.saturating_sub(since))))
                            .weak()
                            .small(),
                    );
                }
                if hist_n > live_n && ui.small_button("Clear history").on_hover_text("Forget everything not in a feed right now").clicked() {
                    self.feed_hist.clear();
                    self.save_feed_history();
                }
            }
            ui.label(egui::RichText::new(format!("saving to {}", self.folder.display())).weak().small());
        });

        // Search only runs again when something it depends on changed.
        let sel_key = self.fv.selected.and_then(|i| self.store.feeds.get(i)).map(|f| history::feed_key(&f.url));
        let key = (self.fv.query.clone(), self.fv.sort, self.fv.scope_history, self.feed_hist.version, sel_key.clone(), self.meta_version);
        if self.fv.results_key.as_ref() != Some(&key) {
            let (scope_hist, meta_cache) = (self.fv.scope_history, &self.meta_cache);
            self.fv.results = search::run(
                &self.feed_hist.entries,
                |e| (scope_hist || e.in_feed) && sel_key.as_ref().is_none_or(|k| &e.feed_key == k),
                &search::parse(&self.fv.query),
                self.fv.sort,
                // Films: OMDb's genre (once looked up) is searchable too.
                |e| meta::guess(&e.title, &e.category).and_then(|q| meta_cache.get(&q.key()).flatten()).map(|i| i.genre).unwrap_or_default(),
                now,
            );
            self.fv.results_key = Some(key);
        }
        let total = self.fv.results.len();
        ui.label(
            egui::RichText::new(if total > MAX_ROWS {
                format!("{total} results · showing the first {MAX_ROWS}, narrow the search to see the rest")
            } else {
                format!("{total} result{}", if total == 1 { "" } else { "s" })
            })
            .weak()
            .small(),
        );

        let many_feeds = self.store.feeds.len() > 1 || self.fv.scope_history;
        let mut pick: Option<usize> = None;
        // Cached ratings always show; NEW lookups stop while the key is refused.
        let ratings_on = self.store.show_ratings && !self.store.omdb_key.trim().is_empty();
        let can_lookup = ratings_on && self.meta_error.is_none();
        let mut to_lookup: Vec<(String, meta::Query)> = Vec::new();
        let mut to_poster: Vec<(String, String)> = Vec::new();
        egui::ScrollArea::vertical().show(ui, |ui| {
            for &i in self.fv.results.iter().take(MAX_ROWS) {
                let e = &self.feed_hist.entries[i];
                // Genre is only known once OMDb has answered for this title; feed tags show next to it.
                let omdb_genre = meta::guess(&e.title, &e.category)
                    .and_then(|q| self.meta_cache.get(&q.key()).flatten())
                    .map(|i| i.genre)
                    .unwrap_or_default();
                let genre = [omdb_genre, e.tags.iter().take(4).cloned().collect::<Vec<_>>().join(", ")]
                    .into_iter()
                    .filter(|s| !s.is_empty())
                    .collect::<Vec<_>>()
                    .join(", ");
                let mut sub = String::new();
                if let Some(sz) = e.size {
                    sub += &human(sz);
                }
                if !e.date.is_empty() {
                    sub += &format!("  ·  {}", e.date);
                } else {
                    sub += &format!("  ·  seen {} ago", history::ago(now.saturating_sub(e.first_seen)));
                }
                if many_feeds {
                    sub += &format!("  ·  {}", e.feed);
                }
                // Ratings: cached answer, or queue a lookup.
                let info = match (ratings_on, meta::guess(&e.title, &e.category)) {
                    (true, Some(q)) => {
                        let k = q.key();
                        match self.meta_cache.get(&k) {
                            Some(found) => found,
                            None => {
                                if can_lookup && !self.meta_pending.contains(&k) && to_lookup.len() < 40 {
                                    to_lookup.push((k, q));
                                }
                                None
                            }
                        }
                    }
                    _ => None,
                };
                let Some(info) = info else {
                    ui.horizontal_wrapped(|ui| {
                        if ui.small_button("⬇").on_hover_text("Download").clicked() {
                            pick = Some(i);
                        }
                        ui.label(&e.title).on_hover_text(if e.description.is_empty() { "—" } else { e.description.as_str() });
                        tags(ui, &e.category, &genre);
                        swarm(ui, e, now);
                        ui.label(egui::RichText::new(&sub).weak().small());
                    });
                    continue;
                };
                ui.horizontal(|ui| {
                    if ui.small_button("⬇").on_hover_text("Download").clicked() {
                        pick = Some(i);
                    }
                    let (w, h) = (46.0, 68.0);
                    match self.textures.get(&info.imdb_id) {
                        Some(Some(t)) => {
                            ui.add(egui::Image::new(t).fit_to_exact_size(egui::vec2(w, h)).corner_radius(3.0));
                        }
                        other => {
                            let (r, _) = ui.allocate_exact_size(egui::vec2(w, h), egui::Sense::hover());
                            ui.painter().rect_filled(r, 3.0, ui.visuals().faint_bg_color);
                            if other.is_none() {
                                if let Some(p) = &info.poster {
                                    to_poster.push((info.imdb_id.clone(), p.clone()));
                                }
                            }
                        }
                    }
                    ui.vertical(|ui| {
                        ui.horizontal_wrapped(|ui| {
                            ui.strong(format!("{} ({})", info.title, info.year));
                            tags(ui, &e.category, &genre);
                        });
                        ui.horizontal_wrapped(|ui| {
                            if let Some(r) = info.imdb_rating {
                                ui.label(
                                    egui::RichText::new(format!(" IMDb {r:.1} "))
                                        .strong()
                                        .color(egui::Color32::BLACK)
                                        .background_color(egui::Color32::from_rgb(245, 197, 24)),
                                )
                                .on_hover_text(format!("{} votes", info.imdb_votes));
                            }
                            if let Some(t) = info.rotten {
                                let (bg, word) = if t >= 60 {
                                    (egui::Color32::from_rgb(250, 80, 50), "Fresh")
                                } else {
                                    (egui::Color32::from_rgb(110, 170, 60), "Rotten")
                                };
                                ui.label(
                                    egui::RichText::new(format!(" Rotten Tomatoes {t}% "))
                                        .strong()
                                        .color(egui::Color32::WHITE)
                                        .background_color(bg),
                                )
                                .on_hover_text(format!("Tomatometer: {word}"));
                            }
                            if let Some(m) = info.metascore {
                                ui.label(egui::RichText::new(format!("Metascore {m}")).small());
                            }
                            ui.hyperlink_to("IMDb page", info.imdb_url());
                            swarm(ui, e, now);
                        });
                        ui.label(egui::RichText::new(format!("{}   {sub}", e.title)).weak().small())
                            .on_hover_text(if info.plot.is_empty() { "—".to_string() } else { info.plot.clone() });
                    });
                });
                ui.add_space(2.0);
            }
            if total == 0 {
                let msg = if self.fv.in_flight > 0 && live_n == 0 {
                    "Reading feeds…".to_string()
                } else if !self.fv.query.trim().is_empty() {
                    format!("Nothing matches “{}”.", self.fv.query.trim())
                } else if self.fv.scope_history && !self.feed_hist.enabled {
                    "History is off: tick “Remember every item” to start collecting.".to_string()
                } else {
                    "No items.".to_string()
                };
                ui.label(egui::RichText::new(msg).weak());
            }
        });
        let mut spent = false;
        for (key, query) in to_lookup {
            if !self.meta_cache.spend() {
                self.meta_error = Some(format!("daily limit of {} lookups reached — more tomorrow", meta::DAILY_BUDGET));
                break;
            }
            self.meta_pending.insert(key.clone());
            let _ = self.meta_jobs.send(MetaJob::Lookup { key, query, api_key: self.store.omdb_key.trim().to_string() });
            spent = true;
        }
        if spent {
            self.meta_cache.save(); // keep the daily count honest across restarts
        }
        for (imdb_id, url) in to_poster {
            if self.textures.contains_key(&imdb_id) {
                continue;
            }
            self.textures.insert(imdb_id.clone(), None);
            let _ = self.meta_jobs.send(MetaJob::Poster { imdb_id, url });
        }
        if let Some(i) = pick {
            // The feed's log-in cookie; a feed removed since has none.
            let e = &self.feed_hist.entries[i];
            let feed = self.store.feeds.iter().find(|f| history::feed_key(&f.url) == e.feed_key);
            let cookie = feed.map(|f| rss::feed_cookie(&f.cookie, &f.url)).unwrap_or_default();
            let feed_url = feed.map(|f| f.url.clone());
            let (title, link) = (e.title.clone(), e.link.clone());
            if let Some(u) = feed_url {
                self.fv.status.insert(u.clone(), format!("starting “{title}” — see Downloads"));
                self.fv.last_pick = Some(u);
            }
            self.start(ctx, title, Source::from_feed(link, &cookie));
        }
    }
}

/// Current UTC time as HH:MM, for "last read" stamps.
fn clock() -> String {
    let s = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{:02}:{:02} UTC", s / 3600 % 24, s / 60 % 60)
}

/// A small coloured chip (label or tracker) after a torrent's name.
fn chip(ui: &mut egui::Ui, text: &str, col: egui::Color32) -> egui::Response {
    ui.label(egui::RichText::new(format!(" {text} ")).small().color(egui::Color32::BLACK).background_color(col))
}

#[allow(clippy::too_many_arguments)]
fn transfer_row(
    ui: &mut egui::Ui,
    rt: &tokio::runtime::Runtime,
    session: Option<&Arc<Session>>,
    t: &Transfer,
    f: &sidebar::Facts,
    all_labels: &[String],
    relabel: &mut Vec<(String, String, bool)>,
    open_details: &mut Option<String>,
    play: &mut Option<String>,
    mut on_remove: impl FnMut(),
) {
    let s = t.handle.stats();
    let frac = if s.total_bytes > 0 { s.progress_bytes as f32 / s.total_bytes as f32 } else { 0.0 };
    let paused = t.handle.is_paused();

    ui.group(|ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            // Buttons are laid out first (right to left) so they always fit; the
            // name and chips then get the width that's left, truncated if need be.
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("Remove").on_hover_text("Stop and remove from list (keeps files)").clicked() {
                    on_remove();
                }
                ui.menu_button("Labels", |ui| {
                    if all_labels.is_empty() {
                        ui.label(egui::RichText::new("Make one in the sidebar under Labels").weak());
                    }
                    for l in all_labels {
                        let mut on = f.labels.contains(l);
                        if ui.checkbox(&mut on, l.as_str()).changed() {
                            relabel.push((f.hash.clone(), l.clone(), on));
                        }
                    }
                });
                if ui
                    .small_button("Details")
                    .on_hover_text("Speed graph, files, peers and this torrent's own settings")
                    .clicked()
                {
                    *open_details = Some(f.hash.clone());
                }
                if player::has_media(&t.handle)
                    && ui
                        .small_button("▶ Play")
                        .on_hover_text("Auto-play: starts right away and streams what hasn't downloaded yet — also films packed in RAR/ZIP/TAR archives (stored, as scene releases are). Uses the torrent's .m3u playlist if it has one.")
                        .clicked()
                {
                    *play = Some(f.hash.clone());
                }
                if ui.small_button("Open folder").clicked() {
                    let _ = open_folder(&t.folder);
                }
                if !s.finished {
                    if let Some(sess) = session {
                        let label = if paused { "▶ Resume" } else { "⏸ Pause" };
                        if ui.small_button(label).clicked() {
                            let (sess, h) = (sess.clone(), t.handle.clone());
                            rt.spawn(async move {
                                let _ = if paused { sess.unpause(&h).await } else { sess.pause(&h).await };
                            });
                        }
                    }
                }
                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                    // The name is the drag handle: drop it on a sidebar entry.
                    ui.dnd_drag_source(egui::Id::new(("zt-drag", &f.hash)), sidebar::Dragged(f.hash.clone()), |ui| {
                        ui.add(egui::Label::new(egui::RichText::new(&t.name).strong()).truncate());
                    })
                    .response
                    .on_hover_text(format!("{}\n\nDrag onto a label to tag it, or onto Paused / Downloading to pause or resume", t.name));
                    for l in &f.labels {
                        chip(ui, l, sidebar::tint(l)).on_hover_text("label");
                    }
                    // Private trackers are the ones worth naming on the row.
                    if f.private {
                        for site in &f.sites {
                            chip(ui, &format!("{} 🔒", sidebar::pretty(site)), egui::Color32::from_gray(170)).on_hover_text("private tracker");
                        }
                    }
                });
            });
        });

        let bar_text = if s.finished {
            "✔ Complete".to_string()
        } else {
            format!("{:.1} %", frac * 100.0)
        };
        let fill = if s.finished {
            egui::Color32::from_rgb(60, 170, 90)
        } else {
            egui::Color32::from_rgb(70, 130, 220)
        };
        ui.add(egui::ProgressBar::new(frac).text(bar_text).fill(fill));

        let mut line = format!("{} / {}", human(s.progress_bytes), human(s.total_bytes));
        if let Some(live) = &s.live {
            if !s.finished {
                line += &format!("  ·  ↓ {}", speed(live.download_speed.mbps));
            }
            line += &format!("  ·  ↑ {}", speed(live.upload_speed.mbps));
            line += &format!("  ·  peers {}", live.snapshot.peer_stats.live);
            if let (false, Some(eta)) = (s.finished, &live.time_remaining) {
                line += &format!("  ·  ETA {eta}");
            }
        } else {
            line += &format!("  ·  {}", s.state);
        }
        if let Some(e) = &s.error {
            line += &format!("  ·  error: {e}");
        }
        ui.label(egui::RichText::new(line).small().monospace());
    });
}

/// What a click in the list asks the app for (carried out after the list is drawn).
struct RowOut<'a> {
    relabel: &'a mut Vec<(String, String, bool)>,
    open_details: &'a mut Option<String>,
    play: &'a mut Option<String>,
    remove: &'a mut Option<usize>,
}

/// Status of one torrent in a word, with its colour.
fn status_word(f: &sidebar::Facts, s: &librqbit::TorrentStats) -> (String, egui::Color32) {
    if f.error {
        ("error".into(), egui::Color32::from_rgb(220, 90, 80))
    } else if f.paused {
        ("paused".into(), egui::Color32::from_gray(140))
    } else if f.finished {
        (if f.live { "seeding" } else { "done" }.into(), egui::Color32::from_rgb(60, 170, 90))
    } else {
        let eta = s.live.as_ref().and_then(|l| l.time_remaining.as_ref()).map(|e| format!(" · {e}")).unwrap_or_default();
        (format!("{:.0} %{eta}", if s.total_bytes > 0 { s.progress_bytes as f64 * 100.0 / s.total_bytes as f64 } else { 0.0 }), egui::Color32::from_rgb(70, 130, 220))
    }
}

/// Right-click menu of the compact list and the thumbnails: what the card's buttons do.
#[allow(clippy::too_many_arguments)]
fn actions_menu(
    ui: &mut egui::Ui,
    rt: &tokio::runtime::Runtime,
    session: Option<&Arc<Session>>,
    t: &Transfer,
    f: &sidebar::Facts,
    all_labels: &[String],
    i: usize,
    out: &mut RowOut,
) {
    ui.label(egui::RichText::new(&t.name).strong().small());
    ui.separator();
    if player::has_media(&t.handle) && ui.button("▶ Play").clicked() {
        *out.play = Some(f.hash.clone());
        ui.close();
    }
    if !f.finished {
        if let Some(sess) = session {
            let paused = t.handle.is_paused();
            if ui.button(if paused { "▶ Resume" } else { "⏸ Pause" }).clicked() {
                let (sess, h) = (sess.clone(), t.handle.clone());
                rt.spawn(async move {
                    let _ = if paused { sess.unpause(&h).await } else { sess.pause(&h).await };
                });
                ui.close();
            }
        }
    }
    if ui.button("Details").clicked() {
        *out.open_details = Some(f.hash.clone());
        ui.close();
    }
    if ui.button("Open folder").clicked() {
        let _ = open_folder(&t.folder);
        ui.close();
    }
    ui.menu_button("Labels", |ui| {
        if all_labels.is_empty() {
            ui.label(egui::RichText::new("Make one in the sidebar under Labels").weak());
        }
        for l in all_labels {
            let mut on = f.labels.contains(l);
            if ui.checkbox(&mut on, l.as_str()).changed() {
                out.relabel.push((f.hash.clone(), l.clone(), on));
            }
        }
    });
    ui.separator();
    if ui.button("Remove").on_hover_text("Stop and remove from list (keeps files)").clicked() {
        *out.remove = Some(i);
        ui.close();
    }
}

/// One line per torrent: status dot, kind, name — then a thin bar, status, size and speeds
/// in fixed columns. Double-click opens Details; right-click has the rest; drag to label.
#[allow(clippy::too_many_arguments)]
fn compact_row(
    ui: &mut egui::Ui,
    rt: &tokio::runtime::Runtime,
    session: Option<&Arc<Session>>,
    t: &Transfer,
    f: &sidebar::Facts,
    media: listview::Media,
    stripe: bool,
    all_labels: &[String],
    i: usize,
    out: &mut RowOut,
) {
    let s = t.handle.stats();
    let frac = if s.total_bytes > 0 { s.progress_bytes as f32 / s.total_bytes as f32 } else { 0.0 };
    let (word, col) = status_word(f, &s);
    let h = 24.0;
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), h), egui::Sense::click_and_drag());
    let p = ui.painter_at(rect);
    let visuals = ui.visuals();
    if resp.hovered() {
        p.rect_filled(rect, 4.0, visuals.widgets.hovered.weak_bg_fill);
    } else if stripe {
        p.rect_filled(rect, 4.0, visuals.faint_bg_color);
    }
    let text = visuals.text_color();
    let weak = visuals.weak_text_color();
    let mid = rect.center().y;
    p.circle_filled(egui::pos2(rect.left() + 10.0, mid), 4.0, col);
    p.text(egui::pos2(rect.left() + 20.0, mid), egui::Align2::LEFT_CENTER, media.glyph(), egui::FontId::proportional(12.0), text);

    // Fixed columns from the right: speeds, size, status, bar.
    let speeds = match &s.live {
        Some(l) if !s.finished => format!("↓ {}  ↑ {}", speed(l.download_speed.mbps), speed(l.upload_speed.mbps)),
        Some(l) => format!("↑ {}", speed(l.upload_speed.mbps)),
        None => String::new(),
    };
    let mono = egui::FontId::monospace(11.0);
    let mut x = rect.right() - 8.0;
    p.text(egui::pos2(x, mid), egui::Align2::RIGHT_CENTER, speeds, mono.clone(), weak);
    x -= 150.0;
    p.text(egui::pos2(x, mid), egui::Align2::RIGHT_CENTER, human(f.size), mono.clone(), weak);
    x -= 78.0;
    p.text(egui::pos2(x, mid), egui::Align2::RIGHT_CENTER, &word, egui::FontId::proportional(11.5), col);
    x -= 104.0;
    let bar = egui::Rect::from_min_size(egui::pos2(x - 90.0, mid - 3.0), egui::vec2(90.0, 6.0));
    p.rect_filled(bar, 3.0, egui::Color32::from_white_alpha(22));
    p.rect_filled(egui::Rect::from_min_size(bar.min, egui::vec2(90.0 * frac.clamp(0.0, 1.0), 6.0)), 3.0, col);

    // The name gets what is left, cut with an ellipsis.
    let name_left = rect.left() + 40.0;
    let name_w = (bar.left() - 14.0 - name_left).max(40.0);
    let mut job = egui::text::LayoutJob::single_section(t.name.clone(), egui::TextFormat::simple(egui::FontId::proportional(13.0), text));
    job.wrap = egui::text::TextWrapping::truncate_at_width(name_w);
    let galley = ui.fonts_mut(|fo| fo.layout_job(job));
    p.galley(egui::pos2(name_left, mid - galley.size().y / 2.0), galley, text);
    let mut chip_x = name_left + name_w.min(ui.fonts_mut(|fo| fo.layout_no_wrap(t.name.clone(), egui::FontId::proportional(13.0), text).size().x)) + 8.0;
    for l in &f.labels {
        let g = ui.fonts_mut(|fo| fo.layout_no_wrap(l.clone(), egui::FontId::proportional(10.5), egui::Color32::BLACK));
        let r = egui::Rect::from_min_size(egui::pos2(chip_x, mid - 7.0), egui::vec2(g.size().x + 8.0, 14.0));
        if r.right() > bar.left() - 8.0 {
            break;
        }
        p.rect_filled(r, 7.0, sidebar::tint(l));
        p.galley(egui::pos2(r.left() + 4.0, mid - g.size().y / 2.0), g, egui::Color32::BLACK);
        chip_x = r.right() + 4.0;
    }

    if resp.drag_started() {
        egui::DragAndDrop::set_payload(ui.ctx(), sidebar::Dragged(f.hash.clone()));
    }
    if resp.double_clicked() {
        *out.open_details = Some(f.hash.clone());
    }
    let resp = resp.on_hover_text(format!("{}\n\nDouble-click: details · right-click: actions · drag onto a label", t.name));
    resp.context_menu(|ui| actions_menu(ui, rt, session, t, f, all_labels, i, out));
}

/// A tile: artwork (the film's poster, else a colour and the kind), a progress ring,
/// a ▶ on hover for media; the name and a status line under it.
#[allow(clippy::too_many_arguments)]
fn thumb_tile(
    ui: &mut egui::Ui,
    rt: &tokio::runtime::Runtime,
    session: Option<&Arc<Session>>,
    t: &Transfer,
    f: &sidebar::Facts,
    media: listview::Media,
    poster: Option<&egui::TextureHandle>,
    all_labels: &[String],
    i: usize,
    out: &mut RowOut,
) {
    let s = t.handle.stats();
    let frac = if s.total_bytes > 0 { s.progress_bytes as f32 / s.total_bytes as f32 } else { 0.0 };
    let (word, col) = status_word(f, &s);
    let (w, art_h, h) = (172.0, 128.0, 186.0);
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(w, h), egui::Sense::click_and_drag());
    if !ui.is_rect_visible(rect) {
        return;
    }
    let p = ui.painter_at(rect);
    let visuals = ui.visuals();
    let hovered = resp.hovered();
    p.rect_filled(rect, 10.0, if hovered { visuals.widgets.hovered.weak_bg_fill } else { visuals.faint_bg_color });
    let art = egui::Rect::from_min_size(rect.min, egui::vec2(w, art_h));
    let round_top = egui::CornerRadius { nw: 10, ne: 10, sw: 0, se: 0 };
    let tint = listview::tint(&t.name);
    p.rect_filled(art, round_top, tint);
    // A soft fade to dark at the bottom of the artwork, under the ring.
    let mut mesh = egui::Mesh::default();
    let fade = egui::Rect::from_min_max(egui::pos2(art.left(), art.center().y), art.max);
    mesh.colored_vertex(fade.left_top(), egui::Color32::TRANSPARENT);
    mesh.colored_vertex(fade.right_top(), egui::Color32::TRANSPARENT);
    mesh.colored_vertex(fade.right_bottom(), egui::Color32::from_black_alpha(150));
    mesh.colored_vertex(fade.left_bottom(), egui::Color32::from_black_alpha(150));
    mesh.add_triangle(0, 1, 2);
    mesh.add_triangle(0, 2, 3);
    p.add(mesh);
    match poster {
        Some(tex) => {
            let ph = art_h - 12.0;
            let pr = egui::Rect::from_center_size(art.center(), egui::vec2(ph * 2.0 / 3.0, ph));
            p.image(tex.id(), pr, egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)), egui::Color32::WHITE);
        }
        None => {
            p.text(art.center() - egui::vec2(0.0, 10.0), egui::Align2::CENTER_CENTER, media.glyph(), egui::FontId::proportional(38.0), egui::Color32::WHITE);
            p.text(
                art.center() + egui::vec2(0.0, 26.0),
                egui::Align2::CENTER_CENTER,
                listview::initials(&t.name),
                egui::FontId::proportional(13.0),
                egui::Color32::from_white_alpha(170),
            );
        }
    }
    // Progress ring, bottom right of the artwork.
    let rc = egui::pos2(art.right() - 22.0, art.bottom() - 22.0);
    listview::ring(&p, rc, 14.0, frac, col);
    let inner = if s.finished { "✔".to_string() } else { format!("{:.0}", frac * 100.0) };
    p.text(rc, egui::Align2::CENTER_CENTER, inner, egui::FontId::proportional(10.5), egui::Color32::WHITE);
    // ▶ on hover, for torrents that can play.
    let can_play = player::has_media(&t.handle);
    let play_spot = egui::Rect::from_center_size(art.center(), egui::vec2(52.0, 52.0));
    let over_play = can_play && hovered && resp.hover_pos().is_some_and(|q| play_spot.contains(q));
    if can_play && hovered {
        p.circle_filled(art.center(), 24.0, egui::Color32::from_black_alpha(if over_play { 210 } else { 150 }));
        p.text(art.center() + egui::vec2(2.0, 0.0), egui::Align2::CENTER_CENTER, "▶", egui::FontId::proportional(22.0), egui::Color32::WHITE);
    }

    // Name (two lines at most) and status.
    let text = visuals.text_color();
    let mut job = egui::text::LayoutJob::single_section(t.name.clone(), egui::TextFormat::simple(egui::FontId::proportional(12.5), text));
    job.wrap = egui::text::TextWrapping { max_width: w - 16.0, max_rows: 2, break_anywhere: false, overflow_character: Some('…') };
    let galley = ui.fonts_mut(|fo| fo.layout_job(job));
    p.galley(egui::pos2(rect.left() + 8.0, art.bottom() + 6.0), galley, text);
    p.text(
        egui::pos2(rect.left() + 8.0, rect.bottom() - 9.0),
        egui::Align2::LEFT_CENTER,
        format!("{}  ·  {word}", human(f.size)),
        egui::FontId::proportional(11.0),
        col,
    );
    if let Some(l) = f.labels.first() {
        p.circle_filled(egui::pos2(rect.right() - 10.0, rect.bottom() - 9.0), 4.0, sidebar::tint(l));
    }

    if resp.drag_started() {
        egui::DragAndDrop::set_payload(ui.ctx(), sidebar::Dragged(f.hash.clone()));
    }
    if resp.clicked() {
        if over_play {
            *out.play = Some(f.hash.clone());
        } else {
            *out.open_details = Some(f.hash.clone());
        }
    }
    let resp = resp.on_hover_text(format!("{}\n\nClick: details{} · right-click: actions · drag onto a label", t.name, if can_play { " · ▶: play" } else { "" }));
    resp.context_menu(|ui| actions_menu(ui, rt, session, t, f, all_labels, i, out));
}

fn human(b: u64) -> String {
    const U: [&str; 5] = ["B", "KB", "MB", "GB", "TB"];
    let mut v = b as f64;
    let mut i = 0;
    while v >= 1024.0 && i < U.len() - 1 {
        v /= 1024.0;
        i += 1;
    }
    if i == 0 { format!("{b} B") } else { format!("{v:.1} {}", U[i]) }
}

/// librqbit reports speed in MiB/s under the name `mbps`.
fn speed(mib_s: f64) -> String {
    if mib_s >= 1.0 { format!("{mib_s:.1} MB/s") } else { format!("{:.0} KB/s", mib_s * 1024.0) }
}

fn open_folder(p: &std::path::Path) -> std::io::Result<()> {
    #[cfg(windows)]
    let cmd = "explorer";
    #[cfg(target_os = "macos")]
    let cmd = "open";
    #[cfg(all(unix, not(target_os = "macos")))]
    let cmd = "xdg-open";
    std::process::Command::new(cmd).arg(p).spawn().map(|_| ())
}
