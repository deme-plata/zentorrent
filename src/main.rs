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
mod rss;
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

    // `zentorrent --feed <url>`: print what ZenTorrent sees in an RSS feed.
    if args.get(1).map(String::as_str) == Some("--feed") {
        let Some(url) = args.get(2).cloned() else {
            eprintln!("usage: zentorrent --feed <rss url>");
            std::process::exit(2);
        };
        let http = reqwest::Client::builder().user_agent("ZenTorrent/0.2").build().expect("http");
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
    busy: usize,
}

#[derive(PartialEq, Clone, Copy)]
enum View {
    Downloads,
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
}

impl App {
    fn new(cc: &eframe::CreationContext<'_>, rt: tokio::runtime::Runtime, folder: PathBuf) -> Self {
        cc.egui_ctx.set_visuals(egui::Visuals::dark());

        let (session, session_error) =
            match rt.block_on(Session::new_with_opts(folder.clone(), SessionOptions::default())) {
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

        Self {
            rt,
            session,
            session_error,
            inbox,
            transfers: Vec::new(),
            catalog: Vec::new(),
            catalog_loading: true,
            folder,
            link: String::new(),
            errors: Vec::new(),
            view: View::Downloads,
            store: rss::FeedStore::load(),
            fv: FeedView::default(),
            http: reqwest::Client::builder()
                .user_agent("ZenTorrent/0.2")
                .timeout(Duration::from_secs(30))
                .build()
                .expect("http client"),
        }
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
        inbox.lock().unwrap().busy += 1;
        self.rt.spawn(async move {
            let add = match &source {
                Source::Link(s) => AddTorrent::from_url(s.clone()),
                Source::Bytes(b) => AddTorrent::from_bytes(b.clone()),
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
                        for it in auto {
                            self.fv.status.insert(url.clone(), format!("auto-downloading “{}”", it.title));
                            self.start(ctx, it.title.clone(), Source::Link(it.link.clone()));
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
        for t in ib.added.drain(..) {
            let id = t.handle.id();
            if !self.transfers.iter().any(|x| x.handle.id() == id) {
                self.transfers.push(t);
            }
        }
        self.errors.extend(ib.errors.drain(..));
        if let Some(c) = ib.catalog.take() {
            self.catalog = c;
            self.catalog_loading = false;
        }
    }
}

enum Source {
    Link(String),
    Bytes(Vec<u8>),
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
        let busy = self.inbox.lock().unwrap().busy;

        egui::Panel::top("top").show(root, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.heading("⚡ ZenTorrent");
                ui.label(egui::RichText::new("egui · librqbit").weak());
                ui.add_space(16.0);
                ui.selectable_value(&mut self.view, View::Downloads, format!("Downloads ({})", self.transfers.len()));
                ui.selectable_value(&mut self.view, View::Feeds, format!("RSS feeds ({})", self.store.feeds.len()));
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
                ui.horizontal(|ui| {
                    ui.colored_label(egui::Color32::LIGHT_RED, self.errors.last().unwrap());
                    clear = ui.small_button("x").clicked();
                });
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
}

impl App {
    fn feeds_ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        if !self.errors.is_empty() {
            let mut clear = false;
            ui.horizontal(|ui| {
                ui.colored_label(egui::Color32::LIGHT_RED, self.errors.last().unwrap());
                clear = ui.small_button("x").clicked();
            });
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
            }
        }
        if changed {
            self.save_feeds();
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
        let feeds: Vec<(String, String)> = match self.fv.selected.and_then(|i| self.store.feeds.get(i)) {
            Some(f) => vec![(f.name.clone(), f.url.clone())],
            None => self.store.feeds.iter().map(|f| (f.name.clone(), f.url.clone())).collect(),
        };
        let mut pick: Option<rss::Item> = None;
        egui::ScrollArea::vertical().show(ui, |ui| {
            let mut shown = 0;
            for (name, url) in &feeds {
                let Some(items) = self.fv.items.get(url) else { continue };
                for it in items.iter().filter(|it| needle.is_empty() || it.title.to_lowercase().contains(&needle)) {
                    shown += 1;
                    ui.horizontal(|ui| {
                        if ui.small_button("⬇").on_hover_text("Download").clicked() {
                            pick = Some(it.clone());
                        }
                        ui.label(&it.title);
                        let mut meta = String::new();
                        if let Some(sz) = it.size {
                            meta += &human(sz);
                        }
                        if !it.date.is_empty() {
                            meta += &format!("  ·  {}", it.date);
                        }
                        if feeds.len() > 1 {
                            meta += &format!("  ·  {name}");
                        }
                        ui.label(egui::RichText::new(meta).weak().small());
                    });
                }
            }
            if shown == 0 {
                ui.label(egui::RichText::new(if self.fv.in_flight > 0 { "Reading feeds…" } else { "No items." }).weak());
            }
        });
        if let Some(it) = pick {
            self.start(ctx, it.title.clone(), Source::Link(it.link));
            self.view = View::Downloads;
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
