//! ZenTorrent — a small desktop BitTorrent client.
//!
//! egui for the window, librqbit for the BitTorrent engine (DHT, trackers,
//! peers, piece verification), rfd for the native file/folder pickers.
//! A Linux catalog resolves the *current* official release torrents at
//! startup, so the list never goes stale.

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

mod catalog;
mod meta;
mod rss;
mod seed;
mod update;
use catalog::CatalogEntry;

fn main() -> eframe::Result<()> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .expect("tokio runtime");

    let default_dir = dirs::download_dir()
        .or_else(dirs::home_dir)
        .unwrap_or_else(|| PathBuf::from("."));

    // Headless mode: `zentorrent --cli <magnet|url|file.torrent> [folder]`.
    let args: Vec<String> = std::env::args().collect();
    if args.get(1).map(String::as_str) == Some("--cli") {
        let Some(link) = args.get(2).cloned() else {
            eprintln!("usage: zentorrent --cli <magnet|url|file.torrent> [folder]");
            std::process::exit(2);
        };
        let dir = args.get(3).map(PathBuf::from).unwrap_or(default_dir);
        if let Err(e) = rt.block_on(cli(link, dir)) {
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
                    println!("  {}  [{sz}]  {}", it.title, it.link.chars().take(90).collect::<String>());
                }
            }
            Err(e) => {
                eprintln!("error: {e:#}");
                std::process::exit(1);
            }
        }
        return Ok(());
    }

    let opts = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("ZenTorrent")
            .with_inner_size([860.0, 620.0])
            .with_min_inner_size([560.0, 420.0]),
        ..Default::default()
    };
    eframe::run_native(
        "ZenTorrent",
        opts,
        Box::new(move |cc| Ok(Box::new(App::new(cc, rt, default_dir)))),
    )
}

async fn cli(link: String, dir: PathBuf) -> anyhow::Result<()> {
    let session = Session::new_with_opts(dir.clone(), SessionOptions::default()).await?;
    let add = if std::path::Path::new(&link).is_file() {
        AddTorrent::from_bytes(std::fs::read(&link)?)
    } else {
        AddTorrent::from_url(link)
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
        println!(
            "{:5.1} %  {} / {}  ↓ {spd}  peers {peers}  [{}]",
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
    catalog: Option<Vec<CatalogEntry>>,
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
}

/// RSS state that lives only in memory (the feed list itself is in `rss::FeedStore`).
#[derive(Default)]
struct FeedView {
    items: HashMap<String, Vec<rss::Item>>,
    status: HashMap<String, String>,
    selected: Option<usize>,
    new_name: String,
    new_url: String,
    filter: String,
    in_flight: usize,
    last_refresh: Option<Instant>,
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
    catalog: Vec<CatalogEntry>,
    catalog_loading: bool,
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
    textures: HashMap<String, Option<egui::TextureHandle>>,
    last_tick: Instant,
    last_ledger_save: Instant,
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, rt: tokio::runtime::Runtime, folder: PathBuf) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());

        let (session, session_error) =
            // Persistence: librqbit remembers every torrent (and where it got
            // to) in the data dir, so a restart resumes downloads and seeding.
            match rt.block_on(Session::new_with_opts(
                folder.clone(),
                SessionOptions {
                    persistence: Some(librqbit::SessionPersistenceConfig::Json {
                        folder: Some(seed::data_dir().join("session")),
                    }),
                    fastresume: true,
                    ..Default::default()
                },
            )) {
                Ok(s) => (Some(s), None),
                Err(e) => (None, Some(format!("could not start the torrent engine: {e:#}"))),
            };

        let inbox = Arc::new(Mutex::new(Inbox::default()));
        {
            let inbox = inbox.clone();
            let ctx = cc.egui_ctx.clone();
            rt.spawn(async move {
                let entries = catalog::resolve_all().await;
                inbox.lock().unwrap().catalog = Some(entries);
                ctx.request_repaint();
            });
        }

        // Torrents restored from the previous run.
        let ledger = seed::Ledger::load();
        let mut transfers = Vec::new();
        if let Some(s) = &session {
            let handles: Vec<ManagedTorrentHandle> = s.with_torrents(|it| it.map(|(_, h)| h.clone()).collect());
            {
                for h in &handles {
                    let hash = h.info_hash().as_string();
                    let e = ledger.entries.get(&hash);
                    transfers.push(Transfer {
                        name: h.name().or_else(|| e.map(|e| e.name.clone())).unwrap_or_else(|| hash.clone()),
                        folder: e.map(|e| e.folder.clone()).unwrap_or_else(|| folder.clone()),
                        handle: h.clone(),
                    });
                }
            }
        }

        let meta_jobs = spawn_meta_worker(&rt, inbox.clone(), cc.egui_ctx.clone());

        Self {
            rt,
            session,
            session_error,
            inbox,
            transfers,
            catalog: Vec::new(),
            catalog_loading: true,
            folder,
            link: String::new(),
            errors: Vec::new(),
            view: View::Downloads,
            store: rss::FeedStore::load(),
            fv: FeedView::default(),
            http: rss::http(),
            upd: Upd::Checking,
            upd_checked: None,
            ledger,
            meta_cache: meta::Cache::load(),
            meta_jobs,
            meta_pending: Default::default(),
            meta_error: None,
            textures: HashMap::new(),
            last_tick: Instant::now(),
            last_ledger_save: Instant::now(),
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
                for t in &self.transfers {
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
                    if ui.small_button("Retry update check").on_hover_text(e).clicked() {
                        self.check_update(ctx);
                    }
                }
            }
            ui.label(egui::RichText::new(format!("v{}", update::VERSION)).weak());
        });
    }

    /// Re-read every feed (or just one) in the background.
    fn refresh_feeds(&mut self, ctx: &egui::Context, only: Option<usize>) {
        self.fv.last_refresh = Some(Instant::now());
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

    fn save_feeds(&mut self) {
        if let Err(e) = self.store.save() {
            self.errors.push(format!("could not save feeds: {e:#}"));
        }
    }

    /// Start a torrent in the background. `source` is a magnet link, an
    /// http(s) URL to a .torrent file, or the raw bytes of a .torrent file.
    fn start(&self, ctx: &egui::Context, label: String, source: Source) {
        let Some(session) = self.session.clone() else { return };
        let folder = self.folder.clone();
        let inbox = self.inbox.clone();
        let ctx = ctx.clone();
        let http = self.http.clone();
        inbox.lock().unwrap().busy += 1;
        self.rt.spawn(async move {
            let add = match source {
                Source::Link(s) => AddTorrent::from_url(s),
                Source::Bytes(b) => AddTorrent::from_bytes(b),
                Source::Fetch { url, cookie } => match rss::fetch_torrent(&http, &url, Some(&cookie)).await {
                    Ok(b) => AddTorrent::from_bytes(b),
                    Err(e) => {
                        let mut ib = inbox.lock().unwrap();
                        ib.busy -= 1;
                        ib.errors.push(format!("{label}: {e:#}"));
                        ctx.request_repaint();
                        return;
                    }
                },
            };
            let opts = AddTorrentOptions {
                output_folder: Some(folder.to_string_lossy().into_owned()),
                overwrite: true,
                ..Default::default()
            };
            let res = session.add_torrent(add, Some(opts)).await;
            let mut ib = inbox.lock().unwrap();
            ib.busy -= 1;
            match res {
                Ok(AddTorrentResponse::Added(_, handle))
                | Ok(AddTorrentResponse::AlreadyManaged(_, handle)) => {
                    let name = handle.name().unwrap_or(label);
                    ib.added.push(Transfer { name, folder, handle });
                }
                Ok(AddTorrentResponse::ListOnly(_)) => {}
                Err(e) => ib.errors.push(format!("{label}: {e:#}")),
            }
            ctx.request_repaint();
        });
    }

    fn drain_inbox(&mut self, ctx: &egui::Context) {
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
                        let cookie = f.cookie.clone();
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
                    self.fv.items.insert(url, items);
                }
                Err(e) => {
                    self.fv.status.insert(url, format!("error: {e}"));
                }
            }
        }
        if dirty {
            self.save_feeds();
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
                Err(e) => self.meta_error = Some(e),
            }
        }
        if meta_dirty {
            self.meta_cache.save();
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
        if let Some(c) = ib.catalog.take() {
            self.catalog = c;
            self.catalog_loading = false;
        }
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
        let due = match self.fv.last_refresh {
            None => true,
            Some(t) => t.elapsed() >= Duration::from_secs(self.store.refresh_minutes.max(1) * 60),
        };
        if due && !self.store.feeds.is_empty() {
            self.refresh_feeds(ctx, None);
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

        egui::Panel::left("catalog").resizable(false).exact_size(250.0).show(root, |ui| {
            ui.add_space(6.0);
            ui.strong("🐧 Linux downloads");
            ui.label(egui::RichText::new("Official release torrents, resolved live").weak().small());
            ui.separator();
            if self.catalog_loading {
                ui.horizontal(|ui| {
                    ui.spinner();
                    ui.label("Looking up current releases…");
                });
            }
            let mut pick = None;
            for e in &self.catalog {
                ui.group(|ui| {
                    ui.set_width(ui.available_width());
                    ui.strong(&e.distro);
                    ui.label(egui::RichText::new(&e.version).small());
                    if ui.button("⬇ Download").clicked() {
                        pick = Some(e.clone());
                    }
                });
            }
            if let Some(e) = pick {
                self.start(ctx, format!("{} {}", e.distro, e.version), Source::Link(e.link));
            }
        });

        self.tick_ledger();
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
                    ui.label("Pick a Linux release on the left, paste a magnet link, or open a .torrent file.");
                });
            }

            let mut remove = None;
            egui::ScrollArea::vertical().show(ui, |ui| {
                for (i, t) in self.transfers.iter().enumerate() {
                    transfer_row(ui, &self.rt, self.session.as_ref(), t, || remove = Some(i));
                    ui.add_space(4.0);
                }
            });
            if let Some(i) = remove {
                let t = self.transfers.remove(i);
                self.ledger.entries.remove(&t.handle.info_hash().as_string());
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
                if let Some(e) = &self.meta_error {
                    ui.colored_label(egui::Color32::LIGHT_RED, e);
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

        // ── items ───────────────────────────────────────────────────
        ui.horizontal(|ui| {
            ui.label("Filter:");
            ui.add(egui::TextEdit::singleline(&mut self.fv.filter).hint_text("words in the title").desired_width(240.0));
            ui.label(egui::RichText::new(format!("saving to {}", self.folder.display())).weak().small());
        });
        let needle = self.fv.filter.to_lowercase();
        let feeds: Vec<(String, String, String)> = match self.fv.selected.and_then(|i| self.store.feeds.get(i)) {
            Some(f) => vec![(f.name.clone(), f.url.clone(), f.cookie.clone())],
            None => self.store.feeds.iter().map(|f| (f.name.clone(), f.url.clone(), f.cookie.clone())).collect(),
        };
        let mut pick: Option<(rss::Item, String, String)> = None;
        // Cached ratings always show; NEW lookups stop while the key is refused.
        let ratings_on = self.store.show_ratings && !self.store.omdb_key.trim().is_empty();
        let can_lookup = ratings_on && self.meta_error.is_none();
        let mut to_lookup: Vec<(String, meta::Query)> = Vec::new();
        let mut to_poster: Vec<(String, String)> = Vec::new();
        egui::ScrollArea::vertical().show(ui, |ui| {
            let mut shown = 0;
            for (name, url, cookie) in &feeds {
                let Some(items) = self.fv.items.get(url) else { continue };
                for it in items.iter().filter(|it| needle.is_empty() || it.title.to_lowercase().contains(&needle)) {
                    shown += 1;
                    let mut sub = String::new();
                    if let Some(sz) = it.size {
                        sub += &human(sz);
                    }
                    if !it.date.is_empty() {
                        sub += &format!("  ·  {}", it.date);
                    }
                    if feeds.len() > 1 {
                        sub += &format!("  ·  {name}");
                    }
                    // Ratings: cached answer, or queue a lookup.
                    let info = match (ratings_on, meta::guess(&it.title, &it.category)) {
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
                        ui.horizontal(|ui| {
                            if ui.small_button("⬇").on_hover_text("Download").clicked() {
                                pick = Some((it.clone(), cookie.clone(), url.clone()));
                            }
                            ui.label(&it.title);
                            ui.label(egui::RichText::new(&sub).weak().small());
                        });
                        continue;
                    };
                    ui.horizontal(|ui| {
                        if ui.small_button("⬇").on_hover_text("Download").clicked() {
                            pick = Some((it.clone(), cookie.clone(), url.clone()));
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
                                if !info.genre.is_empty() {
                                    ui.label(egui::RichText::new(&info.genre).weak().small());
                                }
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
                            });
                            ui.label(egui::RichText::new(format!("{}   {sub}", it.title)).weak().small())
                                .on_hover_text(if info.plot.is_empty() { "—".to_string() } else { info.plot.clone() });
                        });
                    });
                    ui.add_space(2.0);
                }
            }
            if shown == 0 {
                ui.label(egui::RichText::new(if self.fv.in_flight > 0 { "Reading feeds…" } else { "No items." }).weak());
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
        if let Some((it, cookie, url)) = pick {
            self.fv.last_pick = Some(url);
            self.fv.status.insert(self.fv.last_pick.clone().unwrap(), format!("starting “{}” — see Downloads", it.title));
            self.start(ctx, it.title.clone(), Source::from_feed(it.link, &cookie));
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

fn transfer_row(
    ui: &mut egui::Ui,
    rt: &tokio::runtime::Runtime,
    session: Option<&Arc<Session>>,
    t: &Transfer,
    mut on_remove: impl FnMut(),
) {
    let s = t.handle.stats();
    let frac = if s.total_bytes > 0 { s.progress_bytes as f32 / s.total_bytes as f32 } else { 0.0 };
    let paused = t.handle.is_paused();

    ui.group(|ui| {
        ui.set_width(ui.available_width());
        ui.horizontal(|ui| {
            ui.strong(&t.name);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("Remove").on_hover_text("Stop and remove from list (keeps files)").clicked() {
                    on_remove();
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
