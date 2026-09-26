//! ZenTorrent — a small desktop BitTorrent client.
//!
//! egui for the window, librqbit for the BitTorrent engine (DHT, trackers,
//! peers, piece verification), rfd for the native file/folder pickers.
//! A Linux catalog resolves the *current* official release torrents at
//! startup, so the list never goes stale.

#![cfg_attr(windows, windows_subsystem = "windows")]

use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
    time::Duration,
};

use eframe::egui;
use librqbit::{
    AddTorrent, AddTorrentOptions, AddTorrentResponse, ManagedTorrent, Session, SessionOptions,
};

type ManagedTorrentHandle = Arc<ManagedTorrent>;

mod catalog;
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
    busy: usize,
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

    fn drain_inbox(&mut self) {
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
        self.drain_inbox();
        let busy = self.inbox.lock().unwrap().busy;

        egui::Panel::top("top").show(root, |ui| {
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.heading("⚡ ZenTorrent");
                ui.label(egui::RichText::new("egui · librqbit").weak());
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
