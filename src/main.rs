//! ZenTorrent — a small desktop BitTorrent client.
//!
//! egui for the window, librqbit for the BitTorrent engine (DHT, trackers,
//! peers, piece verification), rfd for the native file/folder pickers.
//! The left sidebar (`sidebar.rs`) filters the list by status, tracker and label.

#![cfg_attr(windows, windows_subsystem = "windows")]

use std::{
    collections::{HashMap, HashSet},
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
/// A feed download the site was too overloaded to give: tried again this often, this
/// many times (two hours).
const SITE_RETRY: Duration = Duration::from_secs(3 * 60);
const SITE_TRIES: u32 = 40;

mod assoc;
mod autodl;
mod autostart;
mod cover;
mod crash;
mod db;
mod details;
mod diskguard;
mod drop;
mod engine;
mod history;
mod info;
mod instance;
mod listview;
mod mcp;
mod meta;
mod moe;
mod pick;
mod player;
mod remove;
mod repair;
mod rss;
mod search;
mod seed;
mod serve;
mod sidebar;
mod tabs;
mod tray;
mod tunnel;
mod update;
mod vpn;

fn main() -> eframe::Result<()> {
    // Errors and crashes go to crash.log; the next start tells about the last one.
    crash::install();
    // Started by an update's (or the VPN's) restart: the old ZenTorrent may still be
    // closing, holding the saved data and the one-ZenTorrent port. Wait for it first, or
    // the new one would start with none of it. Not passed on to programs started from here.
    if std::env::var_os(update::RESTARTED).is_some() {
        std::env::remove_var(update::RESTARTED);
        db::claim(std::time::Duration::from_secs(30));
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    // "Save to" as the user last chose it, else the system's Downloads folder. Asked only
    // where it is needed: a second start that hands over to the running one never opens
    // the saved data.
    let default_dir =
        || saved_folder().or_else(dirs::download_dir).or_else(dirs::home_dir).unwrap_or_else(|| PathBuf::from("."));

    // Leftovers of the previous update (the replaced program, set aside while it ran).
    update::clean_up();

    // A terminal command on Windows: this is a windowed program, so attach to the
    // terminal it was started from, or its output goes nowhere. Not for `mcp`,
    // whose stdin/stdout are the client's pipes.
    #[cfg(windows)]
    if std::env::args().nth(1).is_some_and(|a| a != "mcp" && a != autostart::FLAG) {
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
        let dir = args.get(3).map(PathBuf::from).unwrap_or_else(default_dir);
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

    // `zentorrent --associate [on|off]`: open .torrent files and magnet links with ZenTorrent.
    if args.get(1).map(String::as_str) == Some("--associate") {
        if !assoc::supported() {
            println!("opening .torrent files with ZenTorrent is a Windows setting");
            return Ok(());
        }
        match args.get(2).map(String::as_str) {
            Some(v @ ("on" | "off")) => {
                if let Err(e) = assoc::set_wanted(v == "on") {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            }
            None => {}
            Some(_) => {
                eprintln!("usage: zentorrent --associate [on|off]");
                std::process::exit(2);
            }
        }
        println!("open .torrent files and magnet links with ZenTorrent: {}", if assoc::wanted() { "on" } else { "off" });
        if let Some(other) = assoc::other_choice() {
            println!("but Windows is set to open .torrent files with {other} (Settings → Apps → Default apps)");
        }
        return Ok(());
    }

    // `zentorrent --start-with-windows [on|off]`: the Settings switch, from a terminal.
    if args.get(1).map(String::as_str) == Some("--start-with-windows") {
        if !autostart::supported() {
            println!("starting with the computer is a Windows setting");
            return Ok(());
        }
        match args.get(2).map(String::as_str) {
            Some(v @ ("on" | "off")) => {
                if let Err(e) = autostart::set_wanted(v == "on") {
                    eprintln!("error: {e}");
                    std::process::exit(1);
                }
            }
            None => {}
            Some(_) => {
                eprintln!("usage: zentorrent --start-with-windows [on|off]");
                std::process::exit(2);
            }
        }
        println!("start with Windows: {}", if autostart::wanted() { "on" } else { "off" });
        println!("registered: {}", autostart::registered().unwrap_or_else(|| "(nothing)".into()));
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
        let folder = args.get(3).filter(|a| !a.starts_with("--")).map(PathBuf::from).unwrap_or_else(default_dir);
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
        let saves = vec![args.get(3).map(PathBuf::from).unwrap_or_else(default_dir)];
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
        let folder = args.get(2).filter(|a| !a.starts_with("--")).map(PathBuf::from).unwrap_or_else(default_dir);
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

    // One ZenTorrent: a second start (a double-clicked .torrent or magnet link, the Start
    // menu) hands over to the running one and ends here; so does Windows' start-up.
    let autostarted = args.iter().any(|a| a == autostart::FLAG);
    let open = instance::to_open(&args);
    let listener = match instance::start(&open, !autostarted) {
        instance::Start::Primary(l) => Some(l),
        instance::Start::HandedOff => return Ok(()),
        instance::Start::Alone => None,
        // The running one is frozen: offer to end it, then be the running one.
        instance::Start::Hung(pid) => {
            if !crash::ask_to_end_frozen(pid) {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(1500));
            match instance::start(&open, !autostarted) {
                instance::Start::Primary(l) => Some(l),
                instance::Start::HandedOff => return Ok(()),
                _ => None,
            }
        }
    };

    // This window owns the saved data. A ZenTorrent that was just closed may still hold it
    // for a moment: wait for it rather than start without your feeds, ratios and settings.
    db::claim(std::time::Duration::from_secs(10));
    let default_dir = default_dir();

    // Windows' start-up list and the .torrent/magnet registration follow Settings and this exe's path.
    if let Err(e) = autostart::apply(autostart::wanted()) {
        eprintln!("{e}");
    }
    if let Err(e) = assoc::apply(assoc::wanted()) {
        eprintln!("{e}");
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
        Box::new(move |cc| Ok(Box::new(App::new(cc, rt, default_dir, listener, open)))),
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
    /// .torrent files and magnet links from outside (Windows, a second start).
    opens: Vec<String>,
    /// (feed url, items or error)
    feeds: Vec<(String, Result<Vec<rss::Item>, String>)>,
    update: Option<Upd>,
    /// (feed url, cookie or error) from a Log in click.
    login: Vec<(String, Result<String, String>)>,
    /// (query key, OMDb answer or error)
    meta: Vec<(String, Result<Option<meta::Info>, String>)>,
    /// (imdb id, poster bytes or error)
    posters: Vec<(String, Result<Vec<u8>, String>)>,
    /// Pictures read from inside torrents: (info-hash, file, image; `None`: not a picture after all).
    covers: Vec<(String, PathBuf, Option<egui::ColorImage>)>,
    /// The same picture read large, for the Details banner.
    cover_large: Vec<(String, Option<egui::ColorImage>)>,
    /// (label, .torrent URL, cookie) of feed downloads the site was too overloaded to give.
    overloaded: Vec<(String, String, String)>,
    /// Torrents read, waiting for the user to choose their files.
    listed: Vec<engine::Listed>,
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
    Settings,
}

/// Where people reach the people behind ZenTorrent (shown under Settings → About).
const CONTACT: &str = "bitknight.dipper688@passmail.net";

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
    /// Measured height of each result row (0 = not drawn yet), and the width they were measured at.
    row_h: Vec<f32>,
    row_w: f32,
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
    /// The auto-download rule being made or changed.
    rule_edit: Option<RuleEdit>,
}

/// The auto-download rule editor.
struct RuleEdit {
    rule: autodl::Rule,
    /// The rule it changes; None = a new one.
    index: Option<usize>,
    /// The name follows what is typed until the user names it.
    auto_name: bool,
    /// What it would have caught (history indexes, newest first), and what that was worked out for.
    preview: Result<Vec<usize>, String>,
    preview_key: Option<(String, autodl::Quality, String, bool, u64)>,
    /// Also download the matching items the feeds list right now.
    also_now: bool,
}

/// How often a tunnel that is down is tried again (kill switch on).
const VPN_RETRY: Duration = Duration::from_secs(30);

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
    /// The picture that came inside each torrent, by info-hash.
    covers: HashMap<String, cover::State>,
    /// Started by Windows at log-in: to the tray (or minimized) on the first frame.
    start_minimized: bool,
    /// Remove was clicked for these torrents (info-hashes): ask whether their files go too.
    confirm_remove: Vec<String>,
    /// Torrents selected in Downloads (info-hashes), and the one a Shift-click counts from.
    sel: HashSet<String>,
    sel_anchor: Option<String>,
    /// The last crash or freeze (crash.log) not yet shown to the user.
    last_crash: Option<crash::Last>,
    /// Torrents the user added, waiting in the "Choose what to download" window.
    picks: Vec<pick::Pick>,
    /// Where each Downloads row was drawn last frame (clicks and the selection box use it).
    row_rects: HashMap<String, egui::Rect>,
    /// A selection box being drawn: where it started, and the selection it adds to (Ctrl).
    band: Option<(egui::Pos2, HashSet<String>)>,
    /// The tray icon while "keep running in the tray" is on (Windows).
    tray: Option<tray::Tray>,
    /// The tray knows the window's handle (set on the first frame).
    hwnd_known: bool,
    /// The torrent picture read large for the open Details panel (info-hash, texture once read).
    cover_large: Option<(String, Option<egui::TextureHandle>)>,
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
    /// Disk guard (see diskguard.rs): when it last looked, the free space it saw, and the
    /// downloads it paused (info-hashes) so it resumes exactly those.
    disk_at: Instant,
    disk_free: Option<f64>,
    space_paused: Vec<String>,
    /// "Continue anyway" was pressed: downloads stop only at `diskguard::FLOOR_GB`
    /// (until there is room again, then the normal guard is back).
    space_anyway: bool,
    /// Kill switch on: the next try of the tunnel, and the one running.
    vpn_retry_at: Instant,
    vpn_retry: Option<tokio::task::JoinHandle<vpn::Vpn>>,
    /// Downloads asked for while the kill switch held; started once the engine is up.
    waiting: Vec<(String, Source)>,
    /// Feed downloads an overloaded site refused: (label, URL, cookie, when to try again).
    site_retry: Vec<(String, String, String, Instant)>,
    /// How often each of those has been refused (by URL).
    site_tries: std::collections::HashMap<String, u32>,
    /// The Downloads list's layout, order and media filter.
    list: listview::ListView,
    /// What each torrent holds (video / music / other), by info-hash, once its file list is known.
    media: HashMap<String, listview::Media>,
}

impl App {
    fn new(
        cc: &eframe::CreationContext<'_>,
        rt: tokio::runtime::Runtime,
        folder: PathBuf,
        listener: Option<std::net::TcpListener>,
        open: Vec<String>,
    ) -> Self {
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
            covers: HashMap::new(),
            cover_large: None,
            start_minimized: std::env::args().any(|a| a == autostart::FLAG),
            confirm_remove: Vec::new(),
            sel: HashSet::new(),
            sel_anchor: None,
            picks: Vec::new(),
            last_crash: crash::last_unseen(),
            row_rects: HashMap::new(),
            band: None,
            tray: (tray::supported() && tray::wanted())
                .then(|| tray::create(&cc.egui_ctx).map_err(|e| eprintln!("{e}")).ok())
                .flatten(),
            hwnd_known: false,
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
            disk_at: Instant::now() - Duration::from_secs(60),
            disk_free: None,
            space_paused: Vec::new(),
            space_anyway: false,
            vpn_retry_at: Instant::now() + VPN_RETRY,
            vpn_retry: None,
            waiting: Vec::new(),
            site_retry: Vec::new(),
            site_tries: Default::default(),
            list: listview::ListView::load(),
            media: HashMap::new(),
        };
        app.apply_global_limits();
        // What this start was given, and what later starts hand over.
        app.inbox.lock().unwrap().opens.extend(open);
        crash::watchdog(tray::responding);
        if let Some(l) = listener {
            let (inbox, ctx) = (app.inbox.clone(), cc.egui_ctx.clone());
            instance::listen(l, || tray::responding() != Some(false), move |req| {
                inbox.lock().unwrap().opens.extend(req.open);
                if req.show {
                    bring_forward(&ctx);
                }
                ctx.request_repaint();
            });
        }
        app
    }

    /// Remove: from the list only, or with the torrent's files (to the Recycle Bin).
    fn remove_dialog(&mut self, ctx: &egui::Context) {
        if self.confirm_remove.is_empty() {
            return;
        }
        let picked: Vec<&Transfer> =
            self.confirm_remove.iter().filter_map(|h| self.transfers.iter().find(|t| t.handle.info_hash().as_string() == *h)).collect();
        if picked.is_empty() {
            self.confirm_remove.clear();
            return;
        }
        let n = picked.len();
        let names: Vec<String> = picked.iter().map(|t| t.name.clone()).collect();
        let got: u64 = picked.iter().map(|t| t.handle.stats().progress_bytes).sum();
        let where_ = if n == 1 {
            format!("{} downloaded, in {}", human(got), picked[0].handle.output_folder().display())
        } else {
            format!("{} downloaded in all", human(got))
        };
        let has_files = picked.iter().any(|t| t.handle.with_metadata(|m| !m.file_infos.is_empty()).unwrap_or(false));
        let mut choice: Option<Option<bool>> = None;
        let modal = egui::Modal::new(egui::Id::new("zt-remove")).show(ctx, |ui| {
            ui.set_width(460.0);
            ui.label(egui::RichText::new(if n == 1 { "Remove this torrent?".to_string() } else { format!("Remove these {n} torrents?") }).heading());
            ui.add_space(4.0);
            if n == 1 {
                ui.label(egui::RichText::new(&names[0]).strong());
            } else {
                egui::ScrollArea::vertical().max_height(180.0).show(ui, |ui| {
                    for name in &names {
                        ui.label(egui::RichText::new(name).small());
                    }
                });
            }
            ui.label(egui::RichText::new(where_).weak().small());
            ui.add_space(10.0);
            ui.horizontal(|ui| {
                if ui
                    .button("Remove from list")
                    .on_hover_text("Stops it and takes it off the list. The downloaded files stay where they are.")
                    .clicked()
                {
                    choice = Some(Some(false));
                }
                let delete = egui::Button::new(egui::RichText::new("🗑 Delete the files too").color(egui::Color32::from_rgb(235, 110, 100)));
                if ui
                    .add_enabled(has_files, delete)
                    .on_hover_text(format!(
                        "Also moves this torrent's downloaded files to the {} (only its own files; you can restore them from there).",
                        remove::BIN
                    ))
                    .clicked()
                {
                    choice = Some(Some(true));
                }
                if ui.button("Cancel").clicked() {
                    choice = Some(None);
                }
            });
        });
        if modal.should_close() && choice.is_none() {
            choice = Some(None);
        }
        match choice {
            Some(Some(delete)) => {
                for h in std::mem::take(&mut self.confirm_remove) {
                    self.remove_torrent(ctx, &h, delete);
                    self.sel.remove(&h);
                }
            }
            Some(None) => self.confirm_remove.clear(),
            None => {}
        }
    }

    /// Take a torrent off the list; with `delete`, its files go to the Recycle Bin too.
    fn remove_torrent(&mut self, ctx: &egui::Context, hash: &str, delete: bool) {
        let Some(i) = self.transfers.iter().position(|t| t.handle.info_hash().as_string() == hash) else { return };
        let t = self.transfers.remove(i);
        self.ledger.entries.remove(hash);
        self.speed_hist.remove(hash);
        self.covers.remove(hash);
        self.save_ledger();
        let root = t.handle.output_folder().to_path_buf();
        let files: Vec<PathBuf> = if delete {
            t.handle
                .with_metadata(|m| m.file_infos.iter().filter(|f| !f.attrs.padding).map(|f| root.join(&f.relative_filename)).collect())
                .unwrap_or_default()
        } else {
            Vec::new()
        };
        // A torrent with several files has its own folder (it may go once empty); a single
        // file sits in the shared save folder.
        let own_root = files.len() > 1;
        let protected: Vec<PathBuf> = [Some(self.folder.clone()), dirs::download_dir(), dirs::home_dir()].into_iter().flatten().collect();
        let (session, inbox, ctx, name) = (self.session.clone(), self.inbox.clone(), ctx.clone(), t.name.clone());
        self.rt.spawn(async move {
            // Stopped first: Windows will not move a file the engine still has open.
            if let Some(s) = session {
                let _ = s.delete(t.handle.id().into(), false).await;
            }
            if !delete {
                return;
            }
            let done = tokio::task::spawn_blocking(move || remove::trash_files(&files, &root, own_root, &protected)).await;
            if let Ok(Err(e)) = done {
                inbox.lock().unwrap().errors.push(format!("{name}: {e}"));
            }
            ctx.request_repaint();
        });
    }

    /// While files are dragged over the window: say what dropping them does.
    fn drop_hint(&self, ctx: &egui::Context) {
        let hovered: Vec<Option<PathBuf>> = ctx.input(|i| i.raw.hovered_files.iter().map(|f| f.path.clone()).collect());
        if hovered.is_empty() {
            return;
        }
        let torrents = hovered.iter().flatten().filter(|p| drop::named_torrent(p)).count();
        let rect = ctx.content_rect();
        let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("drop-hint")));
        painter.rect_filled(rect, 0.0, egui::Color32::from_black_alpha(170));
        painter.rect_stroke(
            rect.shrink(14.0),
            14.0,
            egui::Stroke::new(2.5, egui::Color32::from_rgb(90, 200, 120)),
            egui::StrokeKind::Inside,
        );
        painter.text(
            rect.center(),
            egui::Align2::CENTER_CENTER,
            format!("⬇  {}", drop::hover_text(torrents, hovered.len())),
            egui::FontId::proportional(26.0),
            egui::Color32::WHITE,
        );
        ctx.request_repaint();
    }

    /// A .torrent file (path) or a magnet link from outside: a double-click in Windows,
    /// a second start, the command line, a drop from Explorer.
    fn open_item(&mut self, ctx: &egui::Context, item: String) {
        self.view = View::Downloads;
        if item.starts_with("magnet:") {
            self.start_asking(ctx, vpn::link_label(&item), Source::Link(item));
            return;
        }
        let path = PathBuf::from(&item);
        match std::fs::read(&path) {
            Ok(bytes) => {
                let label = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
                self.start_asking(ctx, label, Source::Bytes(bytes));
            }
            Err(e) => self.errors.push(format!("{}: {e}", path.display())),
        }
    }

    /// "Choose folder…": where new downloads go (remembered for the next start).
    fn choose_folder(&mut self) {
        if let Some(dir) = rfd::FileDialog::new().set_directory(&self.folder).pick_folder() {
            remember_folder(&dir);
            self.folder = dir;
        }
    }

    /// The Settings tab: what is otherwise spread over the app, in one place, and About.
    fn settings_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        egui::ScrollArea::vertical().show(ui, |ui| {
            ui.set_max_width(760.0);
            let section = |ui: &mut egui::Ui, title: &str| {
                ui.add_space(14.0);
                ui.label(egui::RichText::new(title).heading());
                ui.separator();
            };

            section(ui, "Downloads");
            let mut limits_changed = false;
            egui::Grid::new("zt-settings-downloads").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
                ui.label("Save to");
                ui.horizontal(|ui| {
                    ui.monospace(self.folder.display().to_string());
                    if ui.button("Choose folder…").clicked() {
                        self.choose_folder();
                    }
                    if ui.button("Open").clicked() {
                        let _ = open_folder(&self.folder);
                    }
                });
                ui.end_row();
                ui.label("Total download limit");
                ui.horizontal(|ui| limits_changed |= details::limit(ui, &mut self.ledger.global_down_kib));
                ui.end_row();
                ui.label("Total upload limit");
                ui.horizontal(|ui| limits_changed |= details::limit(ui, &mut self.ledger.global_up_kib));
                ui.end_row();
            });
            if limits_changed {
                self.save_ledger();
                self.apply_global_limits();
            }
            ui.label(
                egui::RichText::new(format!(
                    "Limits apply at once. When the drive you save to has less than {} GB free, downloads pause by themselves and continue at {} GB.",
                    diskguard::LOW_GB,
                    diskguard::OK_GB
                ))
                .weak()
                .small(),
            );

            if autostart::supported() {
                section(ui, "Windows");
                let mut on = autostart::wanted();
                if ui
                    .checkbox(&mut on, "Start ZenTorrent when Windows starts, so downloads and seeding carry on")
                    .changed()
                {
                    if let Err(e) = autostart::set_wanted(on) {
                        self.errors.push(e);
                    }
                }
            }
            if tray::supported() {
                let mut on = self.tray.is_some();
                if ui
                    .checkbox(&mut on, "Keep running in the system tray when the window is closed (Quit from the tray icon's menu)")
                    .changed()
                {
                    tray::set_wanted(on);
                    self.tray = if on {
                        tray::create(ctx).map_err(|e| self.errors.push(e)).ok()
                    } else {
                        None
                    };
                }
            }
            let mut ask = ask_files();
            if ui
                .checkbox(&mut ask, "Choose which files to download when adding a torrent")
                .on_hover_text(
                    "A torrent with several files shows its folders and files first, all ticked: untick what you \
                     don't want. Auto-download rules always take everything.",
                )
                .changed()
            {
                set_pref("ask_files", serde_json::json!(ask));
            }
            if assoc::supported() {
                let mut on = assoc::wanted();
                if ui.checkbox(&mut on, "Open .torrent files and magnet links with ZenTorrent").changed() {
                    if let Err(e) = assoc::set_wanted(on) {
                        self.errors.push(e);
                    }
                }
                if let (true, Some(other)) = (on, assoc::other_choice()) {
                    ui.horizontal_wrapped(|ui| {
                        ui.label(
                            egui::RichText::new(format!(
                                "Windows is set to open .torrent files with {other}: a choice only you can change, in Windows' Default apps (pick ZenTorrent for .torrent)."
                            ))
                            .small()
                            .color(egui::Color32::from_rgb(240, 170, 60)),
                        );
                        if ui.small_button("Open Default apps").clicked() {
                            let _ = std::process::Command::new("explorer").arg("ms-settings:defaultapps").spawn();
                        }
                    });
                }
            }

            section(ui, "VPN");
            ui.horizontal(|ui| {
                let (state, color) = match &self.vpn {
                    vpn::Vpn::Off => ("Off: torrent traffic goes out directly from this computer.".to_string(), ui.visuals().text_color()),
                    vpn::Vpn::Up { relay, .. } => (format!("On: encrypted tunnel to {relay}."), egui::Color32::from_rgb(90, 200, 120)),
                    vpn::Vpn::Failed(_) => ("On, but the tunnel is down: downloads wait until it is up (tried every 30 s).".to_string(), egui::Color32::LIGHT_RED),
                };
                ui.colored_label(color, state);
                if ui.button("VPN settings…").clicked() {
                    self.vpn_open = true;
                    self.vpn_edit = self.vpn_settings.clone();
                    self.vpn_note = None;
                    self.vpn_pub = Some(vpn::client_key().map(|k| k.public().to_hex()).map_err(|e| format!("{e:#}")));
                }
            });

            section(ui, "Posters & ratings");
            ui.horizontal_wrapped(|ui| {
                let mut changed = ui.checkbox(&mut self.store.show_ratings, "Show film posters and ratings (IMDb, Rotten Tomatoes, Metascore)").changed();
                if self.store.show_ratings {
                    ui.label("OMDb key:");
                    let r = ui.add(egui::TextEdit::singleline(&mut self.store.omdb_key).password(true).hint_text("free key").desired_width(110.0));
                    if r.changed() {
                        self.meta_error = None;
                    }
                    changed |= r.lost_focus();
                    if self.store.omdb_key.trim().is_empty() {
                        ui.hyperlink_to("get a free key (1,000/day)", "https://www.omdbapi.com/apikey.aspx");
                    }
                }
                if changed {
                    if let Err(e) = self.store.save() {
                        self.errors.push(format!("could not save settings: {e:#}"));
                    }
                }
            });

            section(ui, "Updates");
            ui.horizontal(|ui| {
                ui.label(format!("This is ZenTorrent {}.", env!("CARGO_PKG_VERSION")));
                match self.upd.clone() {
                    Upd::Checking => {
                        ui.spinner();
                        ui.label("checking…");
                    }
                    Upd::Available(m) => {
                        if ui.button(egui::RichText::new(format!("⬆ Update to v{}", m.version)).strong()).clicked() {
                            self.install_update(ctx, m);
                        }
                    }
                    Upd::Installing(v) => {
                        ui.spinner();
                        ui.label(format!("installing v{v}…"));
                    }
                    Upd::Installed(v) => {
                        ui.label(format!("v{v} is installed: start ZenTorrent again to use it."));
                    }
                    Upd::UpToDate | Upd::Failed(_) => {
                        if let Upd::Failed(e) = &self.upd {
                            ui.colored_label(egui::Color32::LIGHT_RED, e);
                        }
                        if ui.button("Check for updates").clicked() {
                            self.check_update(ctx);
                        }
                    }
                }
            });
            ui.label(egui::RichText::new("ZenTorrent looks for a new version at start and every 6 hours; updates are signed and checked before they install.").weak().small());

            section(ui, "About");
            ui.label(egui::RichText::new(format!("⚡ ZenTorrent {}", env!("CARGO_PKG_VERSION"))).strong());
            ui.label("A fast, private BitTorrent client with a built-in player, RSS feeds, an optional VPN, and Flux MoE, a media assistant that runs on this computer.");
            ui.add_space(6.0);
            egui::Grid::new("zt-settings-about").num_columns(2).spacing([16.0, 6.0]).show(ui, |ui| {
                ui.label("Contact");
                ui.horizontal(|ui| {
                    ui.hyperlink_to(CONTACT, format!("mailto:{CONTACT}")).on_hover_text("Questions, ideas, bug reports");
                    if ui.small_button("Copy").clicked() {
                        ctx.copy_text(CONTACT.to_string());
                    }
                });
                ui.end_row();
                ui.label("Website");
                ui.hyperlink_to("quillon.xyz", "https://quillon.xyz");
                ui.end_row();
                ui.label("Source & releases");
                ui.hyperlink_to("github.com/deme-plata/zentorrent", "https://github.com/deme-plata/zentorrent");
                ui.end_row();
                ui.label("Licence");
                ui.label("MIT or Apache-2.0");
                ui.end_row();
                ui.label("Built with");
                ui.label("librqbit (BitTorrent engine), egui (window), libmpv (player, LGPL)");
                ui.end_row();
                ui.label("Your data");
                ui.horizontal(|ui| {
                    let dir = seed::data_dir();
                    ui.monospace(dir.display().to_string());
                    if ui.small_button("Open").clicked() {
                        let _ = open_folder(&dir);
                    }
                });
                ui.end_row();
            });
        });
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
    /// The torrent's own picture for the Details banner: read large in the background
    /// the first time, the list's small copy until then.
    fn banner_for(&mut self, ctx: &egui::Context, hash: &str) -> Option<egui::TextureHandle> {
        let Some(cover::State::Ready { tex, path }) = self.covers.get(hash) else { return None };
        match &self.cover_large {
            Some((h, large)) if h == hash => Some(large.clone().unwrap_or_else(|| tex.clone())),
            _ => {
                self.cover_large = Some((hash.to_string(), None));
                let (path, inbox, ctx, key) = (path.clone(), self.inbox.clone(), ctx.clone(), hash.to_string());
                self.rt.spawn_blocking(move || {
                    let img = cover::decode(&path, 1200);
                    inbox.lock().unwrap().cover_large.push((key, img));
                    ctx.request_repaint();
                });
                Some(tex.clone())
            }
        }
    }

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
        let banner = self.banner_for(ctx, &hash);
        let actions = details::show(ctx, &mut d, &self.transfers[i], &f, &mut self.ledger, self.speed_hist.get(&hash), banner.as_ref());
        let mut keep = true;
        for a in actions {
            match a {
                details::Action::Close => keep = false,
                details::Action::Pause(p) => self.set_paused(&d.hash, p),
                details::Action::OpenFolder => {
                    let _ = open_folder(&self.transfers[i].folder);
                }
                details::Action::OpenPicture => {
                    if let Some(cover::State::Ready { path, .. }) = self.covers.get(&d.hash) {
                        let _ = open_folder(path);
                    }
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
                    rating: e.map(|e| e.rating).unwrap_or(0),
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
                Action::Pause(hash) => self.with_selection(&hash).iter().for_each(|h| self.set_paused(h, true)),
                Action::Resume(hash) => self.with_selection(&hash).iter().for_each(|h| self.set_paused(h, false)),
                Action::Tag(hash, label) => {
                    for h in self.with_selection(&hash) {
                        self.ledger.set_label(&h, &label, true);
                    }
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

    /// A torrent dragged onto the sidebar: the whole selection when it is part of it.
    fn with_selection(&self, hash: &str) -> Vec<String> {
        if self.sel.contains(hash) { self.sel.iter().cloned().collect() } else { vec![hash.to_string()] }
    }

    /// The selected torrents, in the order the list shows them.
    fn selected_in(&self, shown: &[String]) -> Vec<String> {
        shown.iter().filter(|h| self.sel.contains(*h)).cloned().collect()
    }

    /// A click on a torrent in Downloads: it alone; Ctrl: in or out; Shift: the range from
    /// the last one clicked.
    fn select_click(&mut self, hash: &str, shown: &[String], ctx: &egui::Context) {
        let m = ctx.input(|i| i.modifiers);
        if m.shift {
            let at = |h: &str| shown.iter().position(|x| x == h);
            if let (Some(a), Some(b)) = (self.sel_anchor.as_deref().and_then(at), at(hash)) {
                if !m.command {
                    self.sel.clear();
                }
                self.sel.extend(shown[a.min(b)..=a.max(b)].iter().cloned());
                return;
            }
        }
        if m.command {
            if !self.sel.remove(hash) {
                self.sel.insert(hash.to_string());
            }
        } else {
            self.sel.clear();
            self.sel.insert(hash.to_string());
        }
        self.sel_anchor = Some(hash.to_string());
    }

    /// What can be done with the selected torrents, above the list.
    fn selection_bar(&mut self, ui: &mut egui::Ui, shown: &[String]) {
        let picked = self.selected_in(shown);
        let n = picked.len();
        let mut save = false;
        egui::Frame::new()
            .fill(egui::Color32::from_rgb(30, 46, 68))
            .corner_radius(6.0)
            .inner_margin(egui::Margin::symmetric(8, 4))
            .show(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    ui.label(egui::RichText::new(format!("{n} selected")).strong());
                    if ui.button("Pause").clicked() {
                        picked.iter().for_each(|h| self.set_paused(h, true));
                    }
                    if ui.button("Resume").clicked() {
                        picked.iter().for_each(|h| self.set_paused(h, false));
                    }
                    ui.menu_button("Labels", |ui| {
                        if self.ledger.labels.is_empty() {
                            ui.label(egui::RichText::new("Make a label in the sidebar first").weak());
                        }
                        for l in self.ledger.labels.clone() {
                            let have = picked.iter().filter(|h| self.ledger.entries.get(*h).is_some_and(|e| e.labels.contains(&l))).count();
                            let mut on = have == n;
                            let text = if have > 0 && have < n { format!("{l}  ({have} of {n})") } else { l.clone() };
                            if ui.checkbox(&mut on, text).clicked() {
                                picked.iter().for_each(|h| self.ledger.set_label(h, &l, on));
                                save = true;
                            }
                        }
                    });
                    ui.menu_button("Rating", |ui| {
                        for r in (1..=10u8).rev() {
                            if ui.button(format!("{r} ★")).clicked() {
                                picked.iter().for_each(|h| self.ledger.set_rating(h, r));
                                save = true;
                                ui.close();
                            }
                        }
                        if ui.button("No rating").clicked() {
                            picked.iter().for_each(|h| self.ledger.set_rating(h, 0));
                            save = true;
                            ui.close();
                        }
                    });
                    if ui.button(egui::RichText::new("Remove…").color(egui::Color32::from_rgb(235, 110, 100))).on_hover_text("Del").clicked() {
                        self.confirm_remove = picked.clone();
                    }
                    if ui.button("Select all").on_hover_text("Ctrl+A").clicked() {
                        self.sel = shown.iter().cloned().collect();
                    }
                    if ui.button("Clear").on_hover_text("Esc").clicked() {
                        self.sel.clear();
                    }
                    ui.label(
                        egui::RichText::new("Ctrl+click adds · Shift+click: a range · drag a box on empty space · drop on a label: all of them")
                            .weak()
                            .small(),
                    );
                });
            });
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

    /// Every 10 s: the disk guard (diskguard.rs). Pauses running downloads when the drive
    /// they go to is nearly full, resumes exactly those when there is room again.
    fn guard_disk(&mut self) {
        if self.disk_at.elapsed() < Duration::from_secs(10) {
            return;
        }
        self.disk_at = Instant::now();
        self.disk_free = moe::ollama::free_disk_gb(&self.folder);
        // Room again: the normal guard is back for the next time the drive fills up.
        if self.disk_free.is_some_and(|f| f >= diskguard::OK_GB) {
            self.space_anyway = false;
        }
        match diskguard::step(self.disk_free, !self.space_paused.is_empty(), self.space_anyway) {
            diskguard::Step::Pause => {
                let running: Vec<String> = self
                    .transfers
                    .iter()
                    .filter(|t| !t.handle.is_paused() && !t.handle.stats().finished)
                    .map(|t| t.handle.info_hash().as_string())
                    .collect();
                for h in running {
                    self.set_paused(&h, true);
                    if !self.space_paused.contains(&h) {
                        self.space_paused.push(h);
                    }
                }
            }
            diskguard::Step::Resume => {
                for h in std::mem::take(&mut self.space_paused) {
                    self.set_paused(&h, false);
                }
            }
            diskguard::Step::Nothing => {}
        }
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
                        self.restart();
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
                        self.restart();
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

    /// Kill switch on: try the tunnel again every `VPN_RETRY`, and the moment it is
    /// up start the engine, the saved torrents and the downloads that waited.
    fn retry_vpn(&mut self, ctx: &egui::Context) {
        if self.session.is_some() || !matches!(self.vpn, vpn::Vpn::Failed(_)) {
            return;
        }
        if let Some(h) = &self.vpn_retry {
            if !h.is_finished() {
                return;
            }
            let h = self.vpn_retry.take().unwrap();
            self.vpn_retry_at = Instant::now() + VPN_RETRY;
            let Ok(v) = self.rt.block_on(h) else { return };
            if !v.is_up() {
                self.vpn = v;
                return;
            }
            let (session, error) = engine::start(&self.rt, &self.folder, &v);
            self.http = v.http();
            self.vpn = v;
            self.session = session;
            self.session_error = error;
            let Some(s) = self.session.clone() else { return };
            self.transfers = engine::restore(&s, &self.ledger, &self.folder);
            self.apply_global_limits();
            self.errors.retain(|e| !e.contains("waiting for the VPN tunnel"));
            for (label, source) in std::mem::take(&mut self.waiting) {
                self.start(ctx, label, source);
            }
            return;
        }
        if Instant::now() >= self.vpn_retry_at {
            let s = self.vpn_running.clone();
            let ctx = ctx.clone();
            self.vpn_retry = Some(self.rt.spawn(async move {
                let v = match vpn::client_key() {
                    Ok(key) => vpn::Vpn::start_with(&s, key).await,
                    Err(e) => vpn::Vpn::Failed(format!("tunnel key: {e:#}")),
                };
                ctx.request_repaint();
                v
            }));
        }
    }

    /// A torrent the user added: with several files, they choose which first (all ticked;
    /// see `pick`). Off under Settings, or no engine: as `start`.
    fn start_asking(&mut self, ctx: &egui::Context, label: String, source: Source) {
        let Some(session) = self.session.clone().filter(|_| ask_files()) else {
            return self.start(ctx, label, source);
        };
        let (folder, inbox, ctx, http, tunnelled) = (self.folder.clone(), self.inbox.clone(), ctx.clone(), self.http.clone(), self.vpn.is_up());
        let again = match &source {
            Source::Fetch { url, cookie } => Some((label.clone(), url.clone(), cookie.clone())),
            _ => None,
        };
        inbox.lock().unwrap().busy += 1;
        self.rt.spawn(async move {
            let res = match engine::list(&session, http, tunnelled, folder, label, source).await {
                Ok(engine::Listing::Already(t)) => Ok(Some(t)),
                Ok(engine::Listing::New(l)) if l.files.len() > 1 => {
                    inbox.lock().unwrap().listed.push(l);
                    Ok(None)
                }
                // One file: nothing to choose.
                Ok(engine::Listing::New(l)) => engine::add_listed(session, l, None).await.map(Some),
                Err(e) => Err(e),
            };
            let mut ib = inbox.lock().unwrap();
            ib.busy -= 1;
            match res {
                Ok(Some(t)) => ib.added.push(t),
                Ok(None) => {}
                Err(e) => match again.filter(|_| e.contains(rss::OVERLOADED)) {
                    Some(a) => {
                        ib.errors.push(format!("{e} — ZenTorrent tries again by itself every {} min", SITE_RETRY.as_secs() / 60));
                        ib.overloaded.push(a);
                    }
                    None => ib.errors.push(e),
                },
            }
            ctx.request_repaint();
        });
    }

    /// The "Choose what to download" window, one torrent at a time.
    fn pick_dialog(&mut self, ctx: &egui::Context) {
        let waiting = self.picks.len().saturating_sub(1);
        let Some(p) = self.picks.first_mut() else { return };
        let mut ask = ask_files();
        let choice = pick::show(ctx, p, &mut ask, waiting);
        if ask != ask_files() {
            set_pref("ask_files", serde_json::json!(ask));
        }
        match choice {
            Some(true) => {
                let p = self.picks.remove(0);
                let only = p.only();
                let Some(session) = self.session.clone() else { return };
                let (inbox, ctx) = (self.inbox.clone(), ctx.clone());
                inbox.lock().unwrap().busy += 1;
                self.rt.spawn(async move {
                    let res = engine::add_listed(session, p.listed, only).await;
                    let mut ib = inbox.lock().unwrap();
                    ib.busy -= 1;
                    match res {
                        Ok(t) => ib.added.push(t),
                        Err(e) => ib.errors.push(e),
                    }
                    ctx.request_repaint();
                });
                self.view = View::Downloads;
            }
            Some(false) => {
                self.picks.remove(0);
            }
            None => {}
        }
    }

    /// Start a torrent in the background. `source` is a magnet link, an
    /// http(s) URL to a .torrent file, or the raw bytes of a .torrent file.
    fn start(&mut self, ctx: &egui::Context, label: String, source: Source) {
        // Never a silent no-op: if the engine did not start, every Download
        // button would otherwise do nothing at all.
        let Some(session) = self.session.clone() else {
            if matches!(self.vpn, vpn::Vpn::Failed(_)) {
                self.errors.push(format!("{label}: waiting for the VPN tunnel; it starts by itself once the tunnel is up."));
                self.waiting.push((label, source));
                return;
            }
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
        // A feed download the site is too overloaded to give is kept and tried again.
        let again = match &source {
            Source::Fetch { url, cookie } => Some((label.clone(), url.clone(), cookie.clone())),
            _ => None,
        };
        inbox.lock().unwrap().busy += 1;
        self.rt.spawn(async move {
            let res = engine::add(session, http, tunnelled, folder, label, source).await;
            let mut ib = inbox.lock().unwrap();
            ib.busy -= 1;
            match res {
                Ok(t) => ib.added.push(t),
                Err(e) => match again.filter(|_| e.contains(rss::OVERLOADED)) {
                    Some(a) => {
                        ib.errors.push(format!("{e} — ZenTorrent tries again by itself every {} min", SITE_RETRY.as_secs() / 60));
                        ib.overloaded.push(a);
                    }
                    None => ib.errors.push(e),
                },
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
                // Flux MoE's auto-download rule, after Add rule.
                Action::Rule { rule, .. } => {
                    self.store.rules.push(rule);
                    self.save_feeds();
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
        // The music visualizer: the playing track's analysis (a downloaded audio file).
        let track = p.queue.current().cloned();
        let audio = track.as_ref().is_some_and(|t| matches!(t.kind, player::playlist::Kind::Audio) && t.archive.is_none());
        let local = track.as_ref().filter(|t| audio && files.complete(t.file)).map(|t| p.queue.folder.join(&t.path));
        let (time, playing) = p.live.lock().map(|l| (l.time, !l.paused)).unwrap_or((0.0, false));
        let bands = pui.vis.bands(local.as_deref(), time, playing);
        egui::Panel::bottom("player").show(root, |ui| player::ui::bar(ui, p, pui, &files));
        if audio && !pui.hide_vis {
            let note = local.is_none().then_some("The visualizer starts once this track has downloaded.");
            egui::Panel::bottom("visualizer")
                .resizable(true)
                .default_size(220.0)
                .size_range(120.0..=520.0)
                .show(root, |ui| player::vis::show(ui, &bands, note));
            ctx.request_repaint_after(Duration::from_millis(33));
        }
        if pui.show_playlist {
            egui::Panel::right("playlist").resizable(true).default_size(300.0).size_range(220.0..=520.0).show(root, |ui| {
                player::ui::playlist(ui, p, &files)
            });
        }
        if pui.show_sound {
            let mut open = true;
            player::ui::sound(ctx, p, &mut open, audio.then_some(&bands));
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
                        let fresh = f.fresh(&items, history::now());
                        dirty = true;
                        let cookie = rss::feed_cookie(&f.cookie, &f.url);
                        // The auto-download rules: what they want of the new items.
                        for (ri, e) in autodl::pick(&mut self.store.rules, &fresh, history::now()) {
                            let rule = self.store.rules[ri].name.clone();
                            self.fv.status.insert(url.clone(), format!("auto-downloading “{}” (rule {rule})", e.title));
                            self.start(ctx, e.title.clone(), Source::from_feed(e.link.clone(), &cookie));
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
        for l in ib.listed.drain(..) {
            self.picks.push(pick::Pick::new(l));
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
        for (hash, path, img) in ib.covers.drain(..) {
            let state = match img {
                Some(img) => cover::State::Ready { tex: ctx.load_texture(format!("cover-{hash}"), img, egui::TextureOptions::LINEAR), path },
                None => cover::State::None,
            };
            self.covers.insert(hash, state);
        }
        for (hash, img) in ib.cover_large.drain(..) {
            if let Some((open, tex)) = self.cover_large.as_mut().filter(|(h, _)| *h == hash) {
                *tex = img.map(|img| ctx.load_texture(format!("cover-large-{open}"), img, egui::TextureOptions::LINEAR));
            }
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
        let overloaded: Vec<_> = ib.overloaded.drain(..).collect();
        for (label, url, cookie) in overloaded {
            // Only the newest note about this download stays.
            let note = |x: &String| x.starts_with(&format!("{label}: ")) && x.contains(rss::OVERLOADED);
            self.errors.retain(|x| !note(x));
            let tries = self.site_tries.entry(url.clone()).or_insert(0);
            *tries += 1;
            if *tries >= SITE_TRIES {
                self.site_tries.remove(&url);
                ib.errors.retain(|x| !note(x));
                let hours = SITE_RETRY.as_secs() * SITE_TRIES as u64 / 3600;
                ib.errors.push(format!("{label}: the site was still overloaded after {hours} hours; click it again later"));
            } else if !self.site_retry.iter().any(|r| r.1 == url) {
                self.site_retry.push((label, url, cookie, Instant::now() + SITE_RETRY));
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

/// All preferences, one JSON object in the store ("Save to", the list layout, the
/// Windows switches…); prefs.json before 0.10.10.
fn prefs() -> serde_json::Value {
    db::load("prefs", Some(&seed::data_dir().join("prefs.json")))
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_else(|| serde_json::json!({}))
}

fn save_prefs(v: &serde_json::Value) {
    if let Err(e) = db::save("prefs", &serde_json::to_vec_pretty(v).unwrap_or_default()) {
        eprintln!("{e}");
    }
}

/// The "Save to" folder the user last chose, if it still exists.
fn saved_folder() -> Option<PathBuf> {
    Some(PathBuf::from(prefs()["save_to"].as_str()?)).filter(|p| p.is_dir())
}

fn remember_folder(dir: &std::path::Path) {
    set_pref("save_to", serde_json::json!(dir.to_string_lossy()));
}

/// Whether adding a torrent with several files asks which to download (on unless turned off).
fn ask_files() -> bool {
    pref("ask_files").and_then(|v| v.as_bool()).unwrap_or(true)
}

/// One setting.
fn pref(key: &str) -> Option<serde_json::Value> {
    prefs().get(key).cloned()
}

/// Change one setting, keeping the others.
fn set_pref(key: &str, value: serde_json::Value) {
    let mut v = prefs();
    v[key] = value;
    save_prefs(&v);
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

/// Height of an RSS result row not drawn yet (one line; rows with a poster are taller).
const ROW_EST: f32 = 24.0;

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
    fn ui(&mut self, root: &mut egui::Ui, frame: &mut eframe::Frame) {
        let ctx = &root.ctx().clone();
        // Tests only (debug builds): freeze the window once, to try the hang check.
        #[cfg(debug_assertions)]
        if let Some(secs) = std::env::var("ZENTORRENT_TEST_FREEZE").ok().and_then(|s| s.parse::<u64>().ok()) {
            static FROZE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
            if ctx.cumulative_pass_nr() > 60 && !FROZE.swap(true, std::sync::atomic::Ordering::Relaxed) {
                std::thread::sleep(Duration::from_secs(secs));
            }
        }
        #[cfg(windows)]
        if !self.hwnd_known {
            use raw_window_handle::{HasWindowHandle, RawWindowHandle};
            if let Ok(h) = frame.window_handle() {
                if let RawWindowHandle::Win32(w) = h.as_raw() {
                    tray::set_window(w.hwnd.get());
                    self.hwnd_known = true;
                }
            }
        }
        #[cfg(not(windows))]
        let _ = (frame, self.hwnd_known);
        // Started by Windows at log-in: in the tray when it is on, else minimized. Not on
        // the first frame: eframe shows the window after that one, undoing the hiding.
        if self.start_minimized && ctx.cumulative_frame_nr() >= 3 {
            self.start_minimized = false;
            if !(self.tray.is_some() && tray::hide()) {
                ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(true));
            }
        }
        if self.start_minimized {
            ctx.request_repaint();
        }
        // Closing the window keeps ZenTorrent running in the tray (Quit is in its menu).
        if ctx.input(|i| i.viewport().close_requested())
            && self.tray.is_some()
            && !tray::QUIT.load(std::sync::atomic::Ordering::Relaxed)
            && tray::hide()
        {
            ctx.send_viewport_cmd(egui::ViewportCommand::CancelClose);
        }
        self.drain_inbox(ctx);
        let opens = std::mem::take(&mut self.inbox.lock().unwrap().opens);
        for item in opens {
            self.open_item(ctx, item);
        }
        // .torrent files dragged in from Explorer: any number at once, or a folder of them.
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().filter_map(|f| f.path.clone()).collect());
        if !dropped.is_empty() {
            let (found, other) = drop::torrents(&dropped);
            if !other.is_empty() {
                self.errors.push(format!("not a .torrent file, left out: {}", other.join(", ")));
            }
            for p in found {
                self.open_item(ctx, p.to_string_lossy().into_owned());
            }
        }
        self.drop_hint(ctx);
        // A video's window stays in front of this one while its buttons are clicked.
        if self.player.is_some() {
            tray::own_video_window(player::audio::WINDOW_TITLE);
        }
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
        // Feed downloads an overloaded site refused: again when their time comes.
        let now = Instant::now();
        if self.site_retry.iter().any(|r| r.3 <= now) {
            let (due, later): (Vec<_>, Vec<_>) = std::mem::take(&mut self.site_retry).into_iter().partition(|r| r.3 <= now);
            self.site_retry = later;
            for (label, url, cookie, _) in due {
                self.errors.retain(|x| !(x.starts_with(&format!("{label}: ")) && x.contains(rss::OVERLOADED)));
                self.start(ctx, label, Source::Fetch { url, cookie });
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
                ui.add_space(16.0);
                // Each tab's icon comes alive with what it stands for (see `tabs`).
                let downloading = self.facts.iter().any(|f| !f.finished && f.down_mibs > 0.01);
                let uploading = self.facts.iter().any(|f| f.up_mibs > 0.01);
                let main_tabs = [
                    (View::Downloads, tabs::Icon::Downloads, format!("Downloads ({})", self.transfers.len()), downloading, ""),
                    (View::Seeding, tabs::Icon::Seeding, "Seeding & ratio".to_string(), uploading, "Upload, ratio and seed goals"),
                    (View::Feeds, tabs::Icon::Feeds, format!("RSS feeds ({})", self.store.feeds.len()), self.fv.in_flight > 0, ""),
                    (
                        View::Moe,
                        tabs::Icon::Moe,
                        "Flux MoE".to_string(),
                        self.moe.busy(),
                        "Your media assistant: overview, feed search, play, playlists, tidy folders — runs on this computer",
                    ),
                    (View::Settings, tabs::Icon::Settings, "Settings".to_string(), false, "Save folder, speed limits, VPN, updates, and About"),
                ];
                for (view, icon, label, busy, tip) in main_tabs {
                    let mut r = tabs::tab(ui, self.view == view, icon, &label, busy);
                    if !tip.is_empty() {
                        r = r.on_hover_text(tip);
                    }
                    if r.clicked() {
                        self.view = view;
                    }
                }
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
                    self.choose_folder();
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
                    self.start_asking(ctx, vpn::link_label(&link), Source::Link(link));
                    self.link.clear();
                }
                if ui
                    .button("Open .torrent…")
                    .on_hover_text("Pick one or several .torrent files, or drag them from Explorer onto the window")
                    .clicked()
                {
                    for path in rfd::FileDialog::new().add_filter("Torrent", &["torrent"]).pick_files().unwrap_or_default() {
                        match std::fs::read(&path) {
                            Ok(bytes) => {
                                let label = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
                                self.start_asking(ctx, label, Source::Bytes(bytes));
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
        self.guard_disk();
        self.retry_vpn(ctx);
        // Drawn before the central panels; as a modal it sits above all of them.
        self.details_ui(ctx);
        self.remove_dialog(ctx);
        self.pick_dialog(ctx);
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
        if self.view == View::Settings {
            egui::CentralPanel::default().show(root, |ui| self.settings_ui(ui, ctx));
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
            if let Some(c) = &self.last_crash {
                let (mut copy, mut dismiss) = (false, false);
                ui.horizontal_wrapped(|ui| {
                    let what = match c.kind.as_str() {
                        "froze" => "ZenTorrent stopped responding last time",
                        "ended frozen" => "ZenTorrent was not responding last time and was closed",
                        _ => "ZenTorrent closed unexpectedly last time",
                    };
                    ui.colored_label(egui::Color32::from_rgb(240, 170, 60), format!("⚠ {what} ({}): {}", c.head, c.summary()));
                    copy = ui.small_button("Copy details").on_hover_text("To send to the people behind ZenTorrent (Settings → About)").clicked();
                    if ui.small_button("Open folder").on_hover_text("crash.log is in ZenTorrent's data folder").clicked() {
                        let _ = open_folder(&seed::data_dir());
                    }
                    dismiss = ui.small_button("Dismiss").clicked();
                });
                if copy {
                    ui.ctx().copy_text(c.text.clone());
                }
                if dismiss {
                    crash::mark_seen(c);
                    self.last_crash = None;
                }
            }
            if !db::owned() {
                let mut restart = false;
                ui.horizontal_wrapped(|ui| {
                    ui.colored_label(
                        egui::Color32::LIGHT_RED,
                        "⚠ Your saved data (RSS feeds and history, ratios and ratings, settings) is open in another \
                         ZenTorrent, so this one shows none of it and saves nothing. Close the other ZenTorrent, then restart.",
                    );
                    restart = ui.button("↻ Restart").clicked();
                });
                if restart {
                    self.restart();
                }
            }
            let orange = egui::Color32::from_rgb(240, 170, 60);
            let free = self.disk_free.map(|f| format!("{f:.1} GB")).unwrap_or_else(|| "almost nothing".into());
            if !self.space_paused.is_empty() && self.space_anyway {
                ui.colored_label(
                    orange,
                    format!(
                        "⚠ The drive your downloads go to is full ({free} free). ZenTorrent paused {} download(s) at {} GB, also after Continue anyway, so Windows and your other programs keep working. Free some space, or choose another Save to folder; they continue by themselves once {} GB are free.",
                        self.space_paused.len(),
                        diskguard::FLOOR_GB,
                        diskguard::OK_GB
                    ),
                );
            } else if !self.space_paused.is_empty() {
                let mut go = false;
                ui.horizontal_wrapped(|ui| {
                    ui.colored_label(
                        orange,
                        format!(
                            "⚠ The drive your downloads go to is nearly full ({free} free). ZenTorrent paused {} download(s) so Windows and your other programs keep working; they continue by themselves once {} GB are free. Free some space, or choose another Save to folder.",
                            self.space_paused.len(),
                            diskguard::OK_GB
                        ),
                    );
                    go = ui
                        .button("Continue anyway")
                        .on_hover_text(format!(
                            "Resume now and keep downloading until only {} GB are left. ZenTorrent stops there: at 0 bytes Windows and other programs stop working properly.",
                            diskguard::FLOOR_GB
                        ))
                        .clicked();
                });
                if go {
                    self.space_anyway = true;
                    for h in std::mem::take(&mut self.space_paused) {
                        self.set_paused(&h, false);
                    }
                }
            } else if self.space_anyway && self.disk_free.is_some_and(|f| f < diskguard::LOW_GB) {
                ui.label(
                    egui::RichText::new(format!(
                        "Continuing with little space: {free} free on the drive your downloads go to. ZenTorrent stops at {} GB.",
                        diskguard::FLOOR_GB
                    ))
                    .small()
                    .color(orange),
                );
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
            // The picture inside each torrent (album covers): chosen once its file list
            // is known, read in the background once that file has downloaded.
            for (t, &media) in self.transfers.iter().zip(&medias) {
                let hash = t.handle.info_hash().as_string();
                match self.covers.get(&hash) {
                    None => {
                        let files: Vec<(String, u64)> = t
                            .handle
                            .with_metadata(|m| m.file_infos.iter().map(|f| (f.relative_filename.to_string_lossy().into_owned(), f.len)).collect())
                            .unwrap_or_default();
                        if files.is_empty() {
                            continue;
                        }
                        let state = match cover::pick(&files, media == listview::Media::Audio) {
                            Some(i) => cover::State::Waiting { file: i, path: t.handle.output_folder().join(&files[i].0), len: files[i].1 },
                            None => cover::State::None,
                        };
                        self.covers.insert(hash, state);
                    }
                    Some(cover::State::Waiting { file, path, len }) => {
                        if !t.handle.stats().file_progress.get(*file).is_some_and(|d| d >= len) {
                            continue;
                        }
                        let (path, inbox, ctx, key) = (path.clone(), self.inbox.clone(), ctx.clone(), hash.clone());
                        self.rt.spawn_blocking(move || {
                            let img = cover::decode(&path, 320);
                            inbox.lock().unwrap().covers.push((key, path, img));
                            ctx.request_repaint();
                        });
                        self.covers.insert(hash, cover::State::Loading);
                    }
                    Some(_) => {}
                }
            }
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
                            rating: f.rating,
                        })
                    })
                    .collect();
                self.list.arrange(&keys)
            };
            // Artwork: a film's OMDb poster in the thumbnails (when ratings are on), else
            // the picture that came inside the torrent.
            let mut art: HashMap<usize, egui::TextureHandle> = if self.list.layout == listview::Layout::Thumbs {
                let names: Vec<(usize, String)> =
                    order.iter().filter(|&&i| medias[i] == listview::Media::Video).map(|&i| (i, self.transfers[i].name.clone())).collect();
                names.into_iter().filter_map(|(i, n)| self.poster_for(&n).map(|t| (i, t))).collect()
            } else {
                HashMap::new()
            };
            for &i in &order {
                if let (false, Some(cover::State::Ready { tex, .. })) = (art.contains_key(&i), self.covers.get(&self.transfers[i].handle.info_hash().as_string())) {
                    art.insert(i, tex.clone());
                }
            }

            // ── several at once: keys, and a bar for what is selected ──
            let shown_hashes: Vec<String> = order.iter().map(|&i| self.transfers[i].handle.info_hash().as_string()).collect();
            self.sel.retain(|h| shown_hashes.contains(h));
            if !ctx.egui_wants_keyboard_input() && self.confirm_remove.is_empty() {
                let (all, esc, del) = ctx.input(|i| {
                    (i.modifiers.command && i.key_pressed(egui::Key::A), i.key_pressed(egui::Key::Escape), i.key_pressed(egui::Key::Delete))
                });
                if all {
                    self.sel = shown_hashes.iter().cloned().collect();
                }
                if esc {
                    self.sel.clear();
                }
                if del && !self.sel.is_empty() {
                    self.confirm_remove = self.selected_in(&shown_hashes);
                }
            }
            if !self.sel.is_empty() {
                self.selection_bar(ui, &shown_hashes);
                ui.add_space(2.0);
            }

            let mut remove = None;
            let mut relabel: Vec<(String, String, bool)> = Vec::new();
            let mut open_details: Option<String> = None;
            let mut play_req: Option<String> = None;
            let mut rate_req: Option<(String, u8)> = None;
            let mut row_click: Option<String> = None;
            let mut under_click: Option<String> = None;
            let mut bg: Option<egui::Response> = None;
            let prev_rects = std::mem::take(&mut self.row_rects);
            let mut rects: HashMap<String, egui::Rect> = HashMap::new();
            let sel = self.sel.clone();
            let labels = self.ledger.labels.clone();
            let layout = self.list.layout;
            // A mouse drag does not scroll here (only touch does): on empty space it draws a selection box.
            egui::ScrollArea::vertical().show(ui, |ui| {
                // Behind every row: a click on empty space clears the selection, a drag there draws a box.
                bg = Some(ui.interact(ui.clip_rect(), egui::Id::new("dl-bg"), egui::Sense::click_and_drag()));
                let mut out = RowOut {
                    relabel: &mut relabel,
                    open_details: &mut open_details,
                    play: &mut play_req,
                    remove: &mut remove,
                    rate: &mut rate_req,
                    select: &mut row_click,
                };
                match layout {
                    listview::Layout::Cards => {
                        for &i in &order {
                            let (t, Some(f)) = (&self.transfers[i], self.facts.get(i)) else { continue };
                            let mut gone = false;
                            sel_row(ui, &f.hash, sel.contains(&f.hash), prev_rects.get(&f.hash).copied(), &mut under_click, &mut rects, |ui| {
                                transfer_row(ui, &self.rt, self.session.as_ref(), t, f, art.get(&i), &labels, out.relabel, out.open_details, out.play, out.rate, || gone = true)
                            });
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
                            sel_row(ui, &f.hash, sel.contains(&f.hash), prev_rects.get(&f.hash).copied(), &mut under_click, &mut rects, |ui| {
                                compact_row(ui, &self.rt, self.session.as_ref(), t, f, medias[i], art.get(&i), n % 2 == 1, &labels, i, &mut out)
                            });
                        }
                    }
                    listview::Layout::Thumbs => {
                        ui.horizontal_wrapped(|ui| {
                            ui.spacing_mut().item_spacing = egui::vec2(12.0, 12.0);
                            for &i in &order {
                                let (t, Some(f)) = (&self.transfers[i], self.facts.get(i)) else { continue };
                                sel_row(ui, &f.hash, sel.contains(&f.hash), prev_rects.get(&f.hash).copied(), &mut under_click, &mut rects, |ui| {
                                    thumb_tile(ui, &self.rt, self.session.as_ref(), t, f, medias[i], art.get(&i), &labels, i, &mut out)
                                });
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
            self.row_rects = rects;
            if let Some(h) = row_click.or(under_click) {
                self.select_click(&h, &shown_hashes, ctx);
            }
            if let Some(bg) = bg {
                let pos = ctx.input(|i| i.pointer.interact_pos());
                if bg.drag_started() {
                    // From where the button went down (a drag is only noticed a few pixels later).
                    if let Some(p) = ctx.input(|i| i.pointer.press_origin()).or(pos) {
                        let keep = if ctx.input(|i| i.modifiers.command) { self.sel.clone() } else { HashSet::new() };
                        self.band = Some((p, keep));
                    }
                }
                if let (Some((start, keep)), Some(p)) = (&self.band, pos) {
                    let r = egui::Rect::from_two_pos(*start, p);
                    let mut s = keep.clone();
                    s.extend(self.row_rects.iter().filter(|(_, rr)| rr.intersects(r)).map(|(h, _)| h.clone()));
                    self.sel = s;
                    let painter = ctx.layer_painter(egui::LayerId::new(egui::Order::Foreground, egui::Id::new("dl-band"))).with_clip_rect(bg.rect);
                    painter.rect(
                        r,
                        2.0,
                        egui::Color32::from_rgba_unmultiplied(90, 160, 255, 36),
                        egui::Stroke::new(1.0, egui::Color32::from_rgb(90, 160, 255)),
                        egui::StrokeKind::Inside,
                    );
                }
                if bg.drag_stopped() || !ctx.input(|i| i.pointer.primary_down()) {
                    self.band = None;
                }
                if bg.clicked() && !ctx.input(|i| i.modifiers.command || i.modifiers.shift) {
                    self.sel.clear();
                }
            }
            if !relabel.is_empty() {
                for (hash, label, on) in relabel {
                    self.ledger.set_label(&hash, &label, on);
                }
                self.save_ledger();
            }
            if let Some((hash, r)) = rate_req {
                self.ledger.set_rating(&hash, r);
                self.save_ledger();
            }
            if let Some(hash) = play_req {
                self.play_torrent(ctx, hash);
            }
            if let Some(hash) = open_details {
                self.details = Some(details::Details::new(hash));
            }
            if let Some(i) = remove {
                self.confirm_remove = vec![self.transfers[i].handle.info_hash().as_string()];
            }
        });

        // Live progress: redraw twice a second while anything is running.
        ctx.request_repaint_after(Duration::from_millis(500));
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        self.save_all();
    }
}

impl App {
    /// The auto-download rules in the RSS tab: one line each, and the editor.
    fn rules_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let n = self.store.rules.len();
        let on = self.store.rules.iter().filter(|r| r.enabled).count();
        let head = if n == 0 { "⚡ Auto-download".to_string() } else { format!("⚡ Auto-download  ·  {on} of {n} rule{} on", if n == 1 { "" } else { "s" }) };
        let names: HashMap<String, String> = self.store.feeds.iter().map(|f| (history::feed_key(&f.url), f.name.clone())).collect();
        let now = history::now();
        let (mut changed, mut edit, mut remove, mut new) = (false, None, None, false);
        egui::CollapsingHeader::new(egui::RichText::new(head).strong()).id_salt("autodl-rules").default_open(true).show(ui, |ui| {
            if n == 0 {
                ui.label(
                    egui::RichText::new(
                        "New items in your feeds can download by themselves: the next episodes of a show, a season, an \
                         artist. Make a rule here, right-click an item below and pick “Auto-download more like this…”, or ask Flux MoE.",
                    )
                    .weak()
                    .small(),
                );
            }
            for (i, r) in self.store.rules.iter_mut().enumerate() {
                ui.horizontal_wrapped(|ui| {
                    changed |= ui.checkbox(&mut r.enabled, "").on_hover_text("On / off").changed();
                    let name = egui::RichText::new(&r.name).strong();
                    ui.label(if r.enabled { name } else { name.weak() });
                    let what = if r.regex { format!("pattern  {}", r.query) } else { autodl::plan(&r.query, r.quality).describe() };
                    ui.label(egui::RichText::new(what).small().color(egui::Color32::from_rgb(140, 170, 200)));
                    let feed = if r.feed_key.is_empty() { "all feeds".to_string() } else { names.get(&r.feed_key).cloned().unwrap_or_else(|| "a removed feed".into()) };
                    ui.label(egui::RichText::new(feed).weak().small());
                    let stats = if r.hits == 0 {
                        "nothing yet".to_string()
                    } else {
                        format!("{} downloaded · last {} ago", r.hits, history::ago(now.saturating_sub(r.last_at)))
                    };
                    ui.label(egui::RichText::new(stats).weak().small()).on_hover_text(if r.last.is_empty() { "—" } else { r.last.as_str() });
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("Remove").on_hover_text("Remove this rule").clicked() {
                            remove = Some(i);
                        }
                        if ui.small_button("Edit").clicked() {
                            edit = Some(i);
                        }
                    });
                });
            }
            ui.horizontal_wrapped(|ui| {
                new = ui.button("+ New rule…").clicked();
                ui.label(egui::RichText::new("or right-click an item below: “Auto-download more like this…”").weak().small());
            });
        });
        if changed {
            self.save_feeds();
        }
        if let Some(i) = remove {
            self.store.rules.remove(i);
            self.save_feeds();
        }
        if let Some(i) = edit {
            let r = self.store.rules[i].clone();
            self.edit_rule(r, Some(i));
        }
        if new {
            self.edit_rule(autodl::new_rule(), None);
        }
        self.rule_editor(ctx);
    }

    /// Open the rule editor on `rule` (`index`: the rule it changes).
    fn edit_rule(&mut self, rule: autodl::Rule, index: Option<usize>) {
        self.view = View::Feeds;
        self.fv.rule_edit = Some(RuleEdit { rule, index, auto_name: index.is_none(), preview: Ok(Vec::new()), preview_key: None, also_now: false });
    }

    /// The rule editor window: what to download, quality, feed, and what it would catch.
    fn rule_editor(&mut self, ctx: &egui::Context) {
        let Some(ed) = self.fv.rule_edit.as_mut() else { return };
        let entries = &self.feed_hist.entries;
        let key = (ed.rule.query.clone(), ed.rule.quality, ed.rule.feed_key.clone(), ed.rule.regex, self.feed_hist.version);
        if ed.preview_key.as_ref() != Some(&key) {
            ed.preview = autodl::preview(&ed.rule, entries);
            ed.preview_key = Some(key);
        }
        let feeds: Vec<(String, String)> = self.store.feeds.iter().map(|f| (history::feed_key(&f.url), f.name.clone())).collect();
        let (mut open, mut save, mut cancel) = (true, false, false);
        let title = if ed.index.is_some() { "Edit auto-download rule" } else { "New auto-download rule" };
        egui::Window::new(title)
            .open(&mut open)
            .collapsible(false)
            .resizable(true)
            .default_width(560.0)
            .default_pos(ctx.content_rect().center_top() + egui::vec2(-280.0, 110.0))
            .show(ctx, |ui| {
            ui.label(egui::RichText::new("What to download").strong());
            let hint = if ed.rule.regex {
                "a regular expression on the title, e.g.  debian.*netinst"
            } else {
                "e.g.  the bear s04 1080p   ·   severance s02e05+   ·   above beyond flac -remix"
            };
            let q = ui.add(egui::TextEdit::singleline(&mut ed.rule.query).hint_text(hint).desired_width(f32::INFINITY)).on_hover_text(autodl::HELP);
            let mut renamed = q.changed();
            ui.horizontal_wrapped(|ui| {
                ui.add_enabled_ui(!ed.rule.regex, |ui| {
                    egui::ComboBox::from_id_salt("rule-quality").selected_text(ed.rule.quality.label()).show_ui(ui, |ui| {
                        for qual in autodl::Quality::ALL {
                            renamed |= ui.selectable_value(&mut ed.rule.quality, qual, qual.label()).changed();
                        }
                    });
                });
                let feed_name = if ed.rule.feed_key.is_empty() {
                    "All feeds".to_string()
                } else {
                    feeds.iter().find(|(k, _)| *k == ed.rule.feed_key).map(|(_, n)| n.clone()).unwrap_or_else(|| "a removed feed".into())
                };
                egui::ComboBox::from_id_salt("rule-feed").selected_text(feed_name).show_ui(ui, |ui| {
                    ui.selectable_value(&mut ed.rule.feed_key, String::new(), "All feeds");
                    for (k, n) in &feeds {
                        ui.selectable_value(&mut ed.rule.feed_key, k.clone(), n);
                    }
                });
                ui.checkbox(&mut ed.rule.once_per_episode, "Each episode once").on_hover_text(
                    "When a 720p and a 1080p release of an episode both turn up, only the first one is downloaded \
                     (also when another rule got it)",
                );
            });
            if renamed && ed.auto_name && !ed.rule.regex {
                ed.rule.name = autodl::name_for(&ed.rule.query, ed.rule.quality);
            }
            ui.horizontal(|ui| {
                ui.label("Name");
                if ui.add(egui::TextEdit::singleline(&mut ed.rule.name).desired_width(260.0)).changed() {
                    ed.auto_name = false;
                }
                ui.checkbox(&mut ed.rule.enabled, "On");
            });
            ui.collapsing("Advanced", |ui| {
                ui.checkbox(&mut ed.rule.regex, "A regular expression instead (on the title, any case)");
            });
            ui.separator();
            match &ed.preview {
                // Nothing typed yet: a hint, not an error.
                Err(_) if ed.rule.query.trim().is_empty() => {
                    ui.label(egui::RichText::new("Type a show, an artist or other words from the title; the matches show here.").weak());
                }
                Err(e) => {
                    ui.colored_label(egui::Color32::LIGHT_RED, e);
                }
                Ok(hits) => {
                    if !ed.rule.regex {
                        let plan = autodl::plan(&ed.rule.query, ed.rule.quality);
                        ui.label(egui::RichText::new(format!("Understood as:  {}", plan.describe())).color(egui::Color32::from_rgb(140, 170, 200)));
                    }
                    if hits.is_empty() {
                        ui.label(egui::RichText::new("Nothing ZenTorrent has seen in your feeds matches yet; it will catch new items that do.").weak());
                    } else {
                        ui.label(format!("{} of the {} items seen in your feeds match — newest:", hits.len(), entries.len()));
                        egui::ScrollArea::vertical().id_salt("rule-preview").max_height(180.0).show(ui, |ui| {
                            for &i in hits.iter().take(40) {
                                let e = &entries[i];
                                let live = if e.in_feed { "in the feed now" } else { "history" };
                                ui.label(egui::RichText::new(format!("{}    · {} · {live}", e.title, e.feed)).small());
                            }
                            if hits.len() > 40 {
                                ui.label(egui::RichText::new(format!("…and {} more", hits.len() - 40)).weak().small());
                            }
                        });
                        let live = hits.iter().filter(|&&i| entries[i].in_feed).count();
                        if live > 0 {
                            ui.checkbox(&mut ed.also_now, format!("Also download the {live} in the feeds right now"));
                        }
                    }
                }
            }
            ui.separator();
            ui.horizontal(|ui| {
                let ok = ed.preview.is_ok() && !ed.rule.name.trim().is_empty();
                save = ui.add_enabled(ok, egui::Button::new(egui::RichText::new("Save rule").strong())).clicked();
                cancel = ui.button("Cancel").clicked();
                ui.label(egui::RichText::new("Only new items download by themselves.").weak().small());
            });
        });
        if cancel || !open {
            self.fv.rule_edit = None;
            return;
        }
        if !save {
            return;
        }
        let ed = self.fv.rule_edit.take().unwrap();
        let idx = match ed.index.filter(|&i| i < self.store.rules.len()) {
            Some(i) => {
                self.store.rules[i] = ed.rule;
                i
            }
            None => {
                self.store.rules.push(ed.rule);
                self.store.rules.len() - 1
            }
        };
        if ed.also_now {
            let live: Vec<history::Entry> =
                ed.preview.unwrap_or_default().iter().map(|&i| &self.feed_hist.entries[i]).filter(|e| e.in_feed).cloned().collect();
            for (_, e) in autodl::pick(&mut self.store.rules[idx..=idx], &live, history::now()) {
                let cookie = self
                    .store
                    .feeds
                    .iter()
                    .find(|f| history::feed_key(&f.url) == e.feed_key)
                    .map(|f| rss::feed_cookie(&f.cookie, &f.url))
                    .unwrap_or_default();
                self.start(ctx, e.title.clone(), Source::from_feed(e.link.clone(), &cookie));
            }
        }
        self.save_feeds();
    }

    /// What is kept on quitting: bookmarks, the ratio ledger, RSS history.
    fn save_all(&mut self) {
        if let Some(p) = self.player.as_mut() {
            p.marks.save();
        }
        self.tick_ledger();
        let _ = self.ledger.save();
        if self.feed_hist.dirty {
            let _ = self.feed_hist.save();
        }
    }

    /// Save, then start ZenTorrent again (`process::exit` skips `on_exit`).
    fn restart(&mut self) -> ! {
        self.save_all();
        update::relaunch()
    }

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
        self.rules_ui(ui, ctx);
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
            self.fv.row_h = vec![0.0; self.fv.results.len()];
        }
        let total = self.fv.results.len();
        ui.label(egui::RichText::new(format!("{total} result{}", if total == 1 { "" } else { "s" })).weak().small());

        let many_feeds = self.store.feeds.len() > 1 || self.fv.scope_history;
        let mut pick: Option<usize> = None;
        // Right-click on an item's title → "Auto-download more like this".
        let mut like: Option<usize> = None;
        // Cached ratings always show; NEW lookups stop while the key is refused.
        let ratings_on = self.store.show_ratings && !self.store.omdb_key.trim().is_empty();
        let can_lookup = ratings_on && self.meta_error.is_none();
        let mut to_lookup: Vec<(String, meta::Query)> = Vec::new();
        let mut to_poster: Vec<(String, String)> = Vec::new();
        // Only the rows on screen are laid out (a history holds thousands): each row's
        // height is measured when it is drawn and kept; rows not drawn yet are estimated.
        let width = ui.available_width();
        if (width - self.fv.row_w).abs() > 0.5 {
            self.fv.row_w = width;
            self.fv.row_h.iter_mut().for_each(|h| *h = 0.0);
        }
        let heights: Vec<f32> = self.fv.row_h.iter().map(|&h| if h > 0.0 { h } else { ROW_EST }).collect();
        let total_h: f32 = heights.iter().sum();
        let mut measured: Vec<(usize, f32)> = Vec::new();
        egui::ScrollArea::vertical().auto_shrink([false, false]).show_viewport(ui, |ui, view| {
            ui.set_height(total_h.max(1.0));
            // The first row reaching into view, and where it starts.
            let (mut first, mut top) = (0, 0.0);
            while first < heights.len() && top + heights[first] < view.min.y {
                top += heights[first];
                first += 1;
            }
            let x = ui.max_rect().x_range();
            let rect = egui::Rect::from_x_y_ranges(x, ui.max_rect().top() + top..=ui.max_rect().top() + view.max.y + ROW_EST);
            ui.scope_builder(egui::UiBuilder::new().max_rect(rect), |ui| {
                ui.skip_ahead_auto_ids(first);
                let start = ui.cursor().top();
                for (n, &i) in self.fv.results.iter().enumerate().skip(first) {
                    if ui.cursor().top() - start + top > view.max.y {
                        break;
                    }
                    let row_top = ui.cursor().top();
                    'row: {
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
                            let t = ui
                                .add(egui::Label::new(&e.title).sense(egui::Sense::click()))
                                .on_hover_text(if e.description.is_empty() { "—" } else { e.description.as_str() });
                            match item_menu(&t, &e.title) {
                                Some(ItemAct::Download) => pick = Some(i),
                                Some(ItemAct::Like) => like = Some(i),
                                None => {}
                            }
                            tags(ui, &e.category, &genre);
                            swarm(ui, e, now);
                            ui.label(egui::RichText::new(&sub).weak().small());
                        });
                        break 'row;
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
                                let t = ui.add(
                                    egui::Label::new(egui::RichText::new(format!("{} ({})", info.title, info.year)).strong())
                                        .sense(egui::Sense::click()),
                                );
                                match item_menu(&t, &e.title) {
                                    Some(ItemAct::Download) => pick = Some(i),
                                    Some(ItemAct::Like) => like = Some(i),
                                    None => {}
                                }
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
                    measured.push((n, ui.cursor().top() - row_top));
                }
            });
            if total == 0 {
                let msg = if self.fv.in_flight > 0 && live_n == 0 {
                    "Reading feeds…".to_string()
                } else if !self.fv.query.trim().is_empty() {
                    format!("Nothing matches “{}”.", self.fv.query.trim())
                } else if self.fv.scope_history && !self.feed_hist.enabled {
                    "History is off: tick “Remember every item” to start collecting.".to_string()
                } else if !self.fv.scope_history && self.fv.status.values().any(|s| s.starts_with("error")) {
                    format!(
                        "The feed isn't answering right now (the red note at the feed says why); ZenTorrent keeps trying. \
                         Your {} earlier items are under History.",
                        self.feed_hist.entries.len()
                    )
                } else {
                    "No items.".to_string()
                };
                ui.label(egui::RichText::new(msg).weak());
            }
        });
        for (n, h) in measured {
            if let Some(x) = self.fv.row_h.get_mut(n) {
                *x = h;
            }
        }
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
        if let Some(i) = like {
            let rule = autodl::like(&self.feed_hist.entries[i]);
            self.edit_rule(rule, None);
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
            self.start_asking(ctx, title, Source::from_feed(link, &cookie));
        }
    }
}

/// What the right-click menu on a feed item's title asks for.
enum ItemAct {
    Download,
    Like,
}

/// The right-click menu on a feed item's title.
fn item_menu(r: &egui::Response, title: &str) -> Option<ItemAct> {
    let mut act = None;
    r.context_menu(|ui| {
        if ui.button("⬇  Download").clicked() {
            act = Some(ItemAct::Download);
            ui.close();
        }
        if ui
            .button("⚡  Auto-download more like this…")
            .on_hover_text("A rule that downloads new episodes (or releases) like this one by themselves")
            .clicked()
        {
            act = Some(ItemAct::Like);
            ui.close();
        }
        if ui.button("Copy title").clicked() {
            ui.ctx().copy_text(title.to_string());
            ui.close();
        }
    });
    act
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
    art: Option<&egui::TextureHandle>,
    all_labels: &[String],
    relabel: &mut Vec<(String, String, bool)>,
    open_details: &mut Option<String>,
    play: &mut Option<String>,
    rate: &mut Option<(String, u8)>,
    mut on_remove: impl FnMut(),
) {
    let s = t.handle.stats();
    let frac = if s.total_bytes > 0 { s.progress_bytes as f32 / s.total_bytes as f32 } else { 0.0 };
    let paused = t.handle.is_paused();

    ui.group(|ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            if let Some(tex) = art {
                let side = egui::vec2(58.0, 58.0);
                ui.add(egui::Image::new((tex.id(), side)).uv(cover::fill_uv(tex.size(), side)).corner_radius(6));
            }
            ui.vertical(|ui| {
                ui.horizontal(|ui| {
                    // Buttons are laid out first (right to left) so they always fit; the
                    // name and chips then get the width that's left, truncated if need be.
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("Remove").on_hover_text("Take it off the list; you choose whether its files go too").clicked() {
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
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new(line).small().monospace());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if let Some(r) = listview::stars(ui, f.rating) {
                            *rate = Some((f.hash.clone(), r));
                        }
                    });
                });
            });
        });
    });
}

/// What a click in the list asks the app for (carried out after the list is drawn).
struct RowOut<'a> {
    relabel: &'a mut Vec<(String, String, bool)>,
    open_details: &'a mut Option<String>,
    play: &'a mut Option<String>,
    remove: &'a mut Option<usize>,
    /// (info-hash, new rating 0–10)
    rate: &'a mut Option<(String, u8)>,
    /// A click that selects (see `App::select_click`).
    select: &'a mut Option<String>,
}

/// Around one torrent in Downloads: its selection highlight and its click target, both under
/// the row so its own buttons keep working (where it was last frame), and where it is now.
fn sel_row(
    ui: &mut egui::Ui,
    hash: &str,
    selected: bool,
    prev: Option<egui::Rect>,
    clicked: &mut Option<String>,
    rects: &mut HashMap<String, egui::Rect>,
    draw: impl FnOnce(&mut egui::Ui),
) {
    let bg = ui.painter().add(egui::Shape::Noop);
    if let Some(r) = prev {
        if ui.interact(r, egui::Id::new(("dl-row", hash)), egui::Sense::click()).clicked() {
            *clicked = Some(hash.to_string());
        }
    }
    let rect = ui.scope(draw).response.rect;
    if selected {
        ui.painter().set(
            bg,
            egui::epaint::RectShape::new(
                rect.expand(3.0),
                7.0,
                egui::Color32::from_rgba_unmultiplied(70, 130, 220, 70),
                egui::Stroke::new(1.5, egui::Color32::from_rgb(90, 160, 255)),
                egui::StrokeKind::Outside,
            ),
        );
    }
    rects.insert(hash.to_string(), rect);
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
    let rate_label = if f.rating > 0 { format!("Rating: {}/10", f.rating) } else { "Rate…".to_string() };
    ui.menu_button(rate_label, |ui| {
        if let Some(r) = listview::stars(ui, f.rating) {
            *out.rate = Some((f.hash.clone(), r));
            ui.close();
        }
    });
    ui.separator();
    if ui.button("Remove…").on_hover_text("Take it off the list; you choose whether its files go too").clicked() {
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
    art: Option<&egui::TextureHandle>,
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
    match art {
        Some(tex) => {
            let r = egui::Rect::from_center_size(egui::pos2(rect.left() + 29.0, mid), egui::vec2(20.0, 20.0));
            p.image(tex.id(), r, cover::fill_uv(tex.size(), r.size()), egui::Color32::WHITE);
        }
        None => {
            p.text(egui::pos2(rect.left() + 20.0, mid), egui::Align2::LEFT_CENTER, media.glyph(), egui::FontId::proportional(12.0), text);
        }
    }

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
    let name_left = rect.left() + 44.0;
    // The user's rating, just left of the bar.
    let rated = f.rating > 0;
    if rated {
        p.text(egui::pos2(bar.left() - 10.0, mid), egui::Align2::RIGHT_CENTER, format!("★{}", f.rating), egui::FontId::proportional(11.5), listview::GOLD);
    }
    let name_w = (bar.left() - if rated { 50.0 } else { 14.0 } - name_left).max(40.0);
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
    } else if resp.clicked() {
        *out.select = Some(f.hash.clone());
    }
    let resp = resp.on_hover_text(format!(
        "{}\n\nClick: select (Ctrl / Shift: more) · double-click: details · right-click: actions · drag onto a label",
        t.name
    ));
    resp.context_menu(|ui| actions_menu(ui, rt, session, t, f, all_labels, i, out));
}

/// A tile: artwork (the film's poster or the torrent's own picture, else a colour and the kind), a progress ring,
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
    let wide = poster.filter(|tex| tex.size()[0] as f32 >= 0.8 * tex.size()[1] as f32);
    if let Some(tex) = wide {
        egui::Image::new((tex.id(), art.size())).uv(cover::fill_uv(tex.size(), art.size())).corner_radius(round_top).paint_at(ui, art);
    }
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
        Some(_) if wide.is_some() => {}
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
    if f.rating > 0 {
        let badge = egui::Rect::from_min_size(art.min + egui::vec2(6.0, 6.0), egui::vec2(40.0, 18.0));
        p.rect_filled(badge, 9.0, egui::Color32::from_black_alpha(170));
        p.text(badge.center(), egui::Align2::CENTER_CENTER, format!("★ {}", f.rating), egui::FontId::proportional(11.5), listview::GOLD);
    }
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
        if ui.input(|i| i.modifiers.command || i.modifiers.shift) {
            *out.select = Some(f.hash.clone());
        } else if over_play {
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

/// A second start asked for the window: back from the tray (or the taskbar), in front.
fn bring_forward(ctx: &egui::Context) {
    #[cfg(windows)]
    tray::show(ctx);
    #[cfg(not(windows))]
    {
        ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(false));
        ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        ctx.request_repaint();
    }
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
