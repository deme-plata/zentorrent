//! The Details panel: one torrent, up close.
//!
//! A modal over the list with a live speed graph on top and five tabs:
//! Status (the numbers), Details (hash, magnet, pieces, trackers), Files
//! (choose what to download, per-file progress), Peers (who you trade
//! with) and Settings (this torrent's speed limits and seed goal, plus the
//! limits for all torrents).
//!
//! The panel never touches the engine itself: it returns [`Action`]s and
//! edits the ledger, and `main.rs` applies them. Per-torrent speed limits
//! use the one getter ZenTorrent adds to librqbit (see vendor/).

use std::collections::{HashSet, VecDeque};

use eframe::egui::{self, Color32, RichText};

use crate::{human, seed, sidebar, speed, Transfer};

/// Seconds of speed history kept per torrent for the graph.
pub const HISTORY: usize = 300;

const DOWN: Color32 = Color32::from_rgb(70, 130, 220);
const UP: Color32 = Color32::from_rgb(60, 170, 90);

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Tab {
    Status,
    Details,
    Files,
    Peers,
    Settings,
}

/// The open panel: which torrent, which tab.
pub struct Details {
    pub hash: String,
    pub tab: Tab,
}

impl Details {
    pub fn new(hash: String) -> Self {
        Self { hash, tab: Tab::Status }
    }
}

/// What the panel asks the app to do.
#[derive(Debug, PartialEq)]
pub enum Action {
    Close,
    /// Download only these file indexes.
    SetFiles(HashSet<usize>),
    /// true = pause, false = resume.
    Pause(bool),
    OpenFolder,
    /// (label, on)
    Label(String, bool),
    /// Limits or goals changed: save the ledger and apply the limits now.
    LedgerChanged,
}

/// Draw the panel. `f` is this frame's sidebar facts for the torrent.
pub fn show(
    ctx: &egui::Context,
    d: &mut Details,
    t: &Transfer,
    f: &sidebar::Facts,
    ledger: &mut seed::Ledger,
    history: Option<&VecDeque<(f32, f32)>>,
) -> Vec<Action> {
    let mut out = Vec::new();
    let modal = egui::Modal::new(egui::Id::new("zt-details")).show(ctx, |ui| {
        // Fixed size from the window: nothing inside may widen the modal.
        let screen = ctx.content_rect();
        let w = 760.0_f32.min(screen.width() - 40.0);
        ui.set_width(w);
        ui.set_max_width(w);
        let s = t.handle.stats();
        let floor = ledger.ratio_floor();

        // ── header ───────────────────────────────────────────────────
        ui.horizontal(|ui| {
            let st = state_of(f, floor);
            ui.label(RichText::new(format!(" {} ", st.label())).small().strong().color(Color32::BLACK).background_color(st.color()));
            if f.private {
                ui.label(RichText::new(" 🔒 private ").small().color(Color32::BLACK).background_color(Color32::from_gray(170)));
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.small_button("x").on_hover_text("Close (Esc)").clicked() {
                    out.push(Action::Close);
                }
            });
        });
        ui.add_space(2.0);
        ui.label(RichText::new(&t.name).heading().strong());
        let frac = if s.total_bytes > 0 { s.progress_bytes as f32 / s.total_bytes as f32 } else { 0.0 };
        ui.add(
            egui::ProgressBar::new(frac)
                .text(format!("{:.1} %  ·  {} of {}", frac * 100.0, human(s.progress_bytes), human(s.total_bytes)))
                .fill(if s.finished { UP } else { DOWN }),
        );
        ui.add_space(6.0);

        // ── graph ────────────────────────────────────────────────────
        let empty = VecDeque::new();
        graph(ui, history.unwrap_or(&empty));
        ui.add_space(8.0);

        // ── tabs ─────────────────────────────────────────────────────
        ui.horizontal(|ui| {
            for (tab, name) in [
                (Tab::Status, "Status"),
                (Tab::Details, "Details"),
                (Tab::Files, "Files"),
                (Tab::Peers, "Peers"),
                (Tab::Settings, "Settings"),
            ] {
                ui.selectable_value(&mut d.tab, tab, RichText::new(name).strong());
            }
        });
        ui.separator();
        // Header + graph + tabs take ~330 px; the tab body gets the rest of the window.
        // A FIXED height: if it followed each tab's content, the modal would
        // re-center on every tab switch and the tab bar would jump under the cursor.
        let body_h = (screen.height() - 360.0).clamp(140.0, 320.0);
        egui::ScrollArea::vertical()
            .max_height(body_h)
            .min_scrolled_height(body_h)
            .auto_shrink([false, false])
            .show(ui, |ui| match d.tab {
            Tab::Status => status_tab(ui, t, f, &s, ledger),
            Tab::Details => details_tab(ui, t, f, &mut out),
            Tab::Files => files_tab(ui, t, &s, &mut out),
            Tab::Peers => peers_tab(ui, t),
            Tab::Settings => settings_tab(ui, t, f, ledger, &mut out),
        });
    });
    if modal.should_close() && !out.contains(&Action::Close) {
        out.push(Action::Close);
    }
    out
}

/// The one status word the header shows, most urgent first.
fn state_of(f: &sidebar::Facts, floor: f64) -> sidebar::Status {
    use sidebar::Status::*;
    [Errored, Paused, Stalled, Downloading, NeedsSeeding, Seeding, Completed]
        .into_iter()
        .find(|s| f.is(*s, floor))
        .unwrap_or(All)
}

/// Download (blue) and upload (green) over the last [`HISTORY`] seconds:
/// gradient-filled areas, a speed scale, and a hover readout.
fn graph(ui: &mut egui::Ui, hist: &VecDeque<(f32, f32)>) {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 150.0), egui::Sense::hover());
    let p = ui.painter_at(rect);
    let vis = ui.visuals();
    p.rect_filled(rect, 8.0, vis.extreme_bg_color);
    let plot = egui::Rect::from_min_max(egui::pos2(rect.left() + 64.0, rect.top() + 26.0), egui::pos2(rect.right() - 12.0, rect.bottom() - 20.0));
    let small = egui::TextStyle::Small.resolve(ui.style());
    let weak = vis.weak_text_color();

    let peak = hist.iter().fold(0.0_f32, |m, &(d, u)| m.max(d).max(u));
    let (top, steps) = scale(peak);
    for i in 0..=steps {
        let v = top * i as f32 / steps as f32;
        let y = plot.bottom() - plot.height() * i as f32 / steps as f32;
        p.line_segment([egui::pos2(plot.left(), y), egui::pos2(plot.right(), y)], egui::Stroke::new(1.0, vis.faint_bg_color.gamma_multiply(2.0)));
        // Gridlines speak the scale's unit: "0.5 MB/s", not "512 KB/s", on a MB/s scale.
        let label = if top >= 1.0 { format!("{} MB/s", (v * 10.0).round() / 10.0) } else { speed(v as f64) };
        p.text(egui::pos2(plot.left() - 8.0, y), egui::Align2::RIGHT_CENTER, label, small.clone(), weak);
    }
    for (frac, label) in [(0.0, "-5 min"), (0.5, "-2½ min"), (1.0, "now")] {
        let x = plot.left() + plot.width() * frac;
        let align = if frac == 0.0 { egui::Align2::LEFT_TOP } else if frac == 1.0 { egui::Align2::RIGHT_TOP } else { egui::Align2::CENTER_TOP };
        p.text(egui::pos2(x, plot.bottom() + 4.0), align, label, small.clone(), weak);
    }

    let step = plot.width() / (HISTORY - 1) as f32;
    let x_of = |i: usize| plot.right() - (hist.len() - 1 - i) as f32 * step;
    let y_of = |v: f32| plot.bottom() - plot.height() * (v / top).clamp(0.0, 1.0);
    for (pick, col) in [(1, UP), (0, DOWN)] {
        let pts: Vec<egui::Pos2> = hist
            .iter()
            .enumerate()
            .map(|(i, &(d, u))| egui::pos2(x_of(i), y_of(if pick == 0 { d } else { u })))
            .collect();
        if pts.len() < 2 {
            continue;
        }
        // Area under the line: opaque at the line, fading to nothing at the axis.
        let (hi, lo) = (Color32::from_rgba_unmultiplied(col.r(), col.g(), col.b(), 90), Color32::TRANSPARENT);
        let mut mesh = egui::Mesh::default();
        for w in pts.windows(2) {
            let base = mesh.vertices.len() as u32;
            mesh.colored_vertex(w[0], hi);
            mesh.colored_vertex(w[1], hi);
            mesh.colored_vertex(egui::pos2(w[1].x, plot.bottom()), lo);
            mesh.colored_vertex(egui::pos2(w[0].x, plot.bottom()), lo);
            mesh.add_triangle(base, base + 1, base + 2);
            mesh.add_triangle(base, base + 2, base + 3);
        }
        p.add(egui::Shape::mesh(mesh));
        p.add(egui::Shape::line(pts, egui::Stroke::new(2.0, col)));
    }

    // Legend: current speeds and the peak.
    let (d_now, u_now) = hist.back().copied().unwrap_or_default();
    let body = egui::TextStyle::Body.resolve(ui.style());
    let mut x = rect.left() + 12.0;
    for (txt, col) in [(format!("⬇ {}", speed(d_now as f64)), DOWN), (format!("⬆ {}", speed(u_now as f64)), UP)] {
        let g = p.layout_no_wrap(txt, body.clone(), col);
        let w = g.size().x;
        p.galley(egui::pos2(x, rect.top() + 6.0), g, col);
        x += w + 16.0;
    }
    p.text(egui::pos2(rect.right() - 12.0, rect.top() + 8.0), egui::Align2::RIGHT_TOP, format!("peak {}", speed(peak as f64)), small.clone(), weak);
    if hist.len() < 2 {
        p.text(plot.center(), egui::Align2::CENTER_CENTER, "collecting speed samples…", small, weak);
        return;
    }

    // Hover: a crosshair with both values at that moment.
    if let Some(pos) = resp.hover_pos().filter(|p| plot.contains(*p)) {
        let back = ((plot.right() - pos.x) / step).round() as usize;
        if back < hist.len() {
            let i = hist.len() - 1 - back;
            let (dv, uv) = hist[i];
            let x = x_of(i);
            p.line_segment([egui::pos2(x, plot.top()), egui::pos2(x, plot.bottom())], egui::Stroke::new(1.0, weak));
            p.circle_filled(egui::pos2(x, y_of(dv)), 3.5, DOWN);
            p.circle_filled(egui::pos2(x, y_of(uv)), 3.5, UP);
            let txt = format!("{} s ago\n⬇ {}\n⬆ {}", back, speed(dv as f64), speed(uv as f64));
            let g = p.layout_no_wrap(txt, small, vis.text_color());
            let mut at = egui::pos2(x + 10.0, plot.top() + 4.0);
            if at.x + g.size().x + 12.0 > rect.right() {
                at.x = x - g.size().x - 18.0;
            }
            let bg = egui::Rect::from_min_size(at, g.size() + egui::vec2(10.0, 8.0));
            p.rect_filled(bg, 5.0, vis.window_fill);
            p.rect_stroke(bg, 5.0, egui::Stroke::new(1.0, weak), egui::StrokeKind::Inside);
            p.galley(at + egui::vec2(5.0, 4.0), g, vis.text_color());
        }
    }
}

/// The graph's top (MiB/s) and gridline count for a peak speed, so every
/// gridline lands on a round number in the unit it is shown in: KB/s below
/// 1 MB/s (50 KB/s at least), MB/s above. 1, 2.5 and 5 split into 5 steps, 2 into 4.
pub fn scale(peak_mib: f32) -> (f32, usize) {
    let kib = peak_mib * 1024.0 * 1.15;
    let (top_in_unit, unit) = if kib < 1000.0 { (nice_ceiling(kib.max(50.0)), 1.0 / 1024.0) } else { (nice_ceiling(peak_mib * 1.15), 1.0) };
    let lead = (top_in_unit / 10f32.powf(top_in_unit.log10().floor())).round() as u32;
    (top_in_unit * unit, if lead == 2 { 4 } else { 5 })
}

/// 1, 2, 2.5 or 5 × a power of ten, at or above `v`.
pub fn nice_ceiling(v: f32) -> f32 {
    let mag = 10f32.powf(v.log10().floor());
    [1.0, 2.0, 2.5, 5.0, 10.0].into_iter().map(|m| m * mag).find(|c| *c >= v * 0.999).unwrap_or(10.0 * mag)
}

/// A small boxed figure for the Status tab: caption, big value, small note,
/// stacked. `width` is the outer width of the card.
fn card(ui: &mut egui::Ui, width: f32, label: &str, value: RichText, sub: &str) {
    egui::Frame::new()
        .fill(ui.visuals().faint_bg_color)
        .corner_radius(8.0)
        .inner_margin(egui::Margin::same(10))
        .show(ui, |ui| {
            ui.set_width(width - 20.0);
            ui.vertical(|ui| {
                ui.label(RichText::new(label).small().color(ui.visuals().weak_text_color()));
                ui.label(value.size(18.0).strong());
                ui.label(RichText::new(if sub.is_empty() { " " } else { sub }).small().color(ui.visuals().weak_text_color()));
            });
        });
}

fn status_tab(ui: &mut egui::Ui, t: &Transfer, f: &sidebar::Facts, s: &librqbit::TorrentStats, ledger: &seed::Ledger) {
    let e = ledger.entries.get(&f.hash).cloned().unwrap_or_default();
    let live = s.live.as_ref();
    let pieces = t.handle.with_metadata(|m| m.info.lengths().total_pieces()).ok();
    let dash = || "—".to_string();
    let r = e.ratio();
    let eta = match (s.finished, live.and_then(|l| l.time_remaining.as_ref())) {
        (true, _) => "done".to_string(),
        (false, Some(t)) => t.to_string(),
        (false, None) => dash(),
    };
    let cards: Vec<(&str, RichText, String)> = vec![
        ("Downloaded", RichText::new(human(s.progress_bytes)), format!("of {}", human(s.total_bytes))),
        (
            "Uploaded (all runs)",
            RichText::new(human(e.uploaded)),
            format!("ratio {r:.2}{}", if f.private { " · private tracker" } else { "" }),
        ),
        (
            "Speed",
            RichText::new(format!("⬇ {}", live.map(|l| speed(l.download_speed.mbps)).unwrap_or_else(dash))).color(DOWN),
            format!("⬆ {}", live.map(|l| speed(l.upload_speed.mbps)).unwrap_or_else(dash)),
        ),
        ("Time left", RichText::new(eta), String::new()),
        (
            "Peers connected",
            RichText::new(live.map(|l| l.snapshot.peer_stats.live.to_string()).unwrap_or_else(|| "0".into())),
            (if f.sites.is_empty() { "found through DHT" } else { "from trackers + DHT" }).into(),
        ),
        (
            "Seeding time",
            RichText::new(seed::duration(e.seed_secs)),
            (if s.finished { "while complete and running" } else { "starts when complete" }).into(),
        ),
        (
            "Pieces verified this run",
            RichText::new(live.map(|l| l.snapshot.downloaded_and_checked_pieces.to_string()).unwrap_or_else(dash)),
            pieces.map(|p| format!("of {p} pieces")).unwrap_or_default(),
        ),
        (
            "Avg. piece time",
            RichText::new(live.and_then(|l| l.average_piece_download_time).map(|d| format!("{:.2} s", d.as_secs_f64())).unwrap_or_else(dash)),
            String::new(),
        ),
        ("Engine state", RichText::new(s.state.to_string()), String::new()),
    ];
    let gap = 10.0;
    let width = ((ui.available_width() - 2.0 * gap) / 3.0).floor();
    egui::Grid::new("zt-status-cards").num_columns(3).spacing([gap, gap]).show(ui, |ui| {
        for (i, (label, value, sub)) in cards.into_iter().enumerate() {
            card(ui, width, label, value, &sub);
            if i % 3 == 2 {
                ui.end_row();
            }
        }
    });
    if let Some(err) = &s.error {
        ui.add_space(6.0);
        ui.colored_label(Color32::LIGHT_RED, format!("Error: {err}"));
    }
}

/// `udp://tracker.example.org:1337/announce?passkey=…` → `udp://tracker.example.org:1337`.
/// Private announce URLs carry the passkey in the path or query; it is never shown.
pub fn tracker_display(u: &url::Url) -> String {
    match (u.host_str(), u.port()) {
        (Some(h), Some(p)) => format!("{}://{h}:{p}", u.scheme()),
        (Some(h), None) => format!("{}://{h}", u.scheme()),
        _ => u.scheme().to_string(),
    }
}

/// A magnet link with only the hash and the name (never a private tracker URL).
pub fn magnet(hash: &str, name: &str) -> String {
    let dn: String = name
        .bytes()
        .map(|b| if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) { (b as char).to_string() } else { format!("%{b:02X}") })
        .collect();
    format!("magnet:?xt=urn:btih:{hash}&dn={dn}")
}

fn row(ui: &mut egui::Ui, label: &str, add: impl FnOnce(&mut egui::Ui)) {
    ui.label(RichText::new(label).color(ui.visuals().weak_text_color()));
    ui.horizontal(add);
    ui.end_row();
}

fn details_tab(ui: &mut egui::Ui, t: &Transfer, f: &sidebar::Facts, out: &mut Vec<Action>) {
    let meta = t
        .handle
        .with_metadata(|m| (m.info.lengths().total_pieces(), m.info.lengths().default_piece_length(), m.file_infos.len()))
        .ok();
    egui::Grid::new("zt-details-grid").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
        row(ui, "Name", |ui| {
            ui.label(&t.name);
        });
        row(ui, "Info hash", |ui| {
            ui.monospace(&f.hash);
            if ui.small_button("Copy").clicked() {
                ui.ctx().copy_text(f.hash.clone());
            }
        });
        row(ui, "Magnet link", |ui| {
            let m = magnet(&f.hash, &t.name);
            ui.monospace(format!("{}…", m.chars().take(46).collect::<String>()));
            if ui.small_button("Copy").on_hover_text(if f.private {
                "Hash + name only. A private torrent's tracker (with your passkey) is never put in a magnet."
            } else {
                "Hash + name only"
            })
            .clicked()
            {
                ui.ctx().copy_text(m);
            }
        });
        row(ui, "Saved in", |ui| {
            ui.monospace(t.folder.display().to_string());
            if ui.small_button("Open").clicked() {
                out.push(Action::OpenFolder);
            }
        });
        row(ui, "Size", |ui| {
            ui.label(human(f.size));
        });
        if let Some((pieces, plen, files)) = meta {
            row(ui, "Pieces", |ui| {
                ui.label(format!("{pieces} × {}", human(plen as u64)));
            });
            row(ui, "Files", |ui| {
                ui.label(files.to_string());
            });
        } else {
            row(ui, "Pieces", |ui| {
                ui.label(RichText::new("waiting for metadata from peers…").weak());
            });
        }
        row(ui, "Private", |ui| {
            ui.label(if f.private { "yes: trackers only, no DHT or peer exchange" } else { "no" });
        });
    });
    ui.add_space(8.0);
    ui.label(RichText::new("Trackers").strong());
    let mut trackers: Vec<String> = t.handle.shared().trackers.iter().map(tracker_display).collect();
    trackers.sort();
    trackers.dedup();
    if trackers.is_empty() {
        ui.label(RichText::new("none: peers come from DHT").weak());
    }
    for tr in trackers {
        let site = url::Url::parse(&tr).ok().and_then(|u| u.host_str().map(sidebar::site)).unwrap_or_default();
        ui.horizontal(|ui| {
            ui.label(RichText::new("■").color(sidebar::tint(&site)));
            ui.label(sidebar::pretty(&site));
            ui.monospace(RichText::new(tr).weak());
        });
    }
}

fn files_tab(ui: &mut egui::Ui, t: &Transfer, s: &librqbit::TorrentStats, out: &mut Vec<Action>) {
    let files: Vec<(String, u64)> = match t
        .handle
        .with_metadata(|m| m.file_infos.iter().map(|fi| (fi.relative_filename.display().to_string(), fi.len)).collect())
    {
        Ok(v) => v,
        Err(_) => {
            ui.label(RichText::new("The file list arrives with the metadata from peers…").weak());
            return;
        }
    };
    let only: HashSet<usize> = t.handle.only_files().map(|v| v.into_iter().collect()).unwrap_or_else(|| (0..files.len()).collect());
    let mut next = only.clone();
    ui.horizontal(|ui| {
        ui.label(format!("{} of {} files selected", only.len(), files.len()));
        if ui.small_button("All").clicked() {
            next = (0..files.len()).collect();
        }
        if ui.small_button("Largest only").on_hover_text("Just the biggest file: usually the film or the ISO").clicked() {
            if let Some((i, _)) = files.iter().enumerate().max_by_key(|(_, f)| f.1) {
                next = [i].into();
            }
        }
    });
    ui.add_space(4.0);
    egui::Grid::new("zt-files").num_columns(4).striped(true).spacing([12.0, 6.0]).show(ui, |ui| {
        for (i, (name, len)) in files.iter().enumerate() {
            let mut on = only.contains(&i);
            if ui.checkbox(&mut on, "").on_hover_text("Download this file").changed() {
                if on {
                    next.insert(i);
                } else {
                    next.remove(&i);
                }
            }
            let short: String = if name.chars().count() > 54 { format!("…{}", name.chars().rev().take(53).collect::<Vec<_>>().into_iter().rev().collect::<String>()) } else { name.clone() };
            ui.label(short).on_hover_text(name);
            ui.label(human(*len));
            let done = s.file_progress.get(i).copied().unwrap_or(0);
            let frac = if *len > 0 { done as f32 / *len as f32 } else { 1.0 };
            ui.add(egui::ProgressBar::new(frac).desired_width(130.0).text(format!("{:.0} %", frac * 100.0)).fill(if frac >= 1.0 { UP } else { DOWN }));
            ui.end_row();
        }
    });
    if next != only {
        if next.is_empty() {
            ui.colored_label(Color32::LIGHT_RED, "Keep at least one file. To stop the whole torrent, pause it.");
        } else {
            out.push(Action::SetFiles(next));
        }
    }
}

fn peers_tab(ui: &mut egui::Ui, t: &Transfer) {
    let Some(live) = t.handle.live() else {
        ui.label(RichText::new("No peers while the torrent is paused or starting.").weak());
        return;
    };
    let snap = live.per_peer_stats_snapshot(Default::default());
    let mut peers: Vec<_> = snap.peers.iter().collect();
    peers.sort_by_key(|(_, p)| std::cmp::Reverse(p.counters.fetched_bytes + p.counters.uploaded_bytes));
    ui.label(format!("{} connected peers", peers.len()));
    ui.add_space(4.0);
    egui::Grid::new("zt-peers").num_columns(5).striped(true).spacing([14.0, 5.0]).show(ui, |ui| {
        for h in ["Address", "Client", "Link", "⬇ got from them", "⬆ sent to them"] {
            ui.strong(h);
        }
        ui.end_row();
        for (addr, p) in peers.into_iter().take(200) {
            ui.monospace(addr);
            ui.label(p.client_name.clone().unwrap_or_else(|| "—".into()));
            ui.label(p.conn_kind.map(|k| format!("{k:?}").to_lowercase()).unwrap_or_default());
            ui.label(human(p.counters.fetched_bytes));
            ui.label(human(p.counters.uploaded_bytes));
            ui.end_row();
        }
    });
}

/// A speed limit: one-click presets, plus a field for an exact value
/// (drag it, or click and type). 0 = unlimited.
fn limit(ui: &mut egui::Ui, v: &mut u32) -> bool {
    let mut changed = false;
    for (label, kib) in [("unlimited", 0), ("1 MB/s", 1024), ("5 MB/s", 5 * 1024), ("10 MB/s", 10 * 1024), ("50 MB/s", 50 * 1024)] {
        if ui.selectable_label(*v == kib, label).clicked() && *v != kib {
            *v = kib;
            changed = true;
        }
    }
    changed |= ui.add(egui::DragValue::new(v).range(0..=10_000_000).speed(16.0).suffix(" KiB/s")).on_hover_text("Exact value: drag, or click and type").changed();
    changed
}

fn settings_tab(ui: &mut egui::Ui, t: &Transfer, f: &sidebar::Facts, ledger: &mut seed::Ledger, out: &mut Vec<Action>) {
    let (g_ratio, g_hours) = (ledger.ratio_goal, ledger.hours_goal);
    let labels = ledger.labels.clone();
    let mut changed = false;
    ui.label(RichText::new("This torrent").strong());
    {
        let e = ledger.entries.entry(f.hash.clone()).or_default();
        egui::Grid::new("zt-settings-one").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
            ui.label("Download limit");
            ui.horizontal(|ui| changed |= limit(ui, &mut e.down_limit_kib));
            ui.end_row();
            ui.label("Upload limit");
            ui.horizontal(|ui| changed |= limit(ui, &mut e.up_limit_kib));
            ui.end_row();
            ui.label("Stop seeding at ratio");
            ui.horizontal(|ui| {
                changed |= ui.add(egui::DragValue::new(&mut e.goal_ratio).range(0.0..=50.0).speed(0.05).max_decimals(2)).changed();
                ui.label(
                    RichText::new(if e.goal_ratio > 0.0 {
                        "this torrent's own goal".to_string()
                    } else if g_ratio > 0.0 {
                        format!("0 = global goal ({g_ratio:.2})")
                    } else {
                        "0 = global goal (off)".to_string()
                    })
                    .weak()
                    .small(),
                );
            });
            ui.end_row();
            ui.label("…or after seeding");
            ui.horizontal(|ui| {
                changed |= ui.add(egui::DragValue::new(&mut e.goal_hours).range(0.0..=8760.0).suffix(" h")).changed();
                ui.label(
                    RichText::new(if e.goal_hours > 0.0 {
                        "this torrent's own goal".to_string()
                    } else if g_hours > 0.0 {
                        format!("0 = global goal ({g_hours:.0} h)")
                    } else {
                        "0 = global goal (off)".to_string()
                    })
                    .weak()
                    .small(),
                );
            });
            ui.end_row();
        });
    }
    ui.add_space(4.0);
    ui.horizontal_wrapped(|ui| {
        ui.label("Labels:");
        if labels.is_empty() {
            ui.label(RichText::new("make one in the sidebar").weak());
        }
        for l in &labels {
            let mut on = f.labels.contains(l);
            if ui.checkbox(&mut on, RichText::new(l).color(sidebar::tint(l))).changed() {
                out.push(Action::Label(l.clone(), on));
            }
        }
    });
    ui.horizontal(|ui| {
        let paused = t.handle.is_paused();
        if ui.button(if paused { "▶ Resume" } else { "⏸ Pause" }).clicked() {
            out.push(Action::Pause(!paused));
        }
        if ui.button("Open folder").clicked() {
            out.push(Action::OpenFolder);
        }
    });
    ui.add_space(10.0);
    ui.separator();
    ui.label(RichText::new("All torrents").strong());
    egui::Grid::new("zt-settings-all").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
        ui.label("Total download limit");
        ui.horizontal(|ui| changed |= limit(ui, &mut ledger.global_down_kib));
        ui.end_row();
        ui.label("Total upload limit");
        ui.horizontal(|ui| changed |= limit(ui, &mut ledger.global_up_kib));
        ui.end_row();
    });
    ui.label(
        RichText::new("Limits apply instantly and are remembered across restarts. On private trackers, keep upload unlimited: it's your ratio.")
            .weak()
            .small(),
    );
    if changed {
        out.push(Action::LedgerChanged);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tracker_urls_lose_their_passkey() {
        let u = url::Url::parse("https://tracker.torrentleech.org/a/0123456789abcdef/announce").unwrap();
        assert_eq!(tracker_display(&u), "https://tracker.torrentleech.org");
        let u = url::Url::parse("http://www.torrentbytes.net:8080/announce.php?passkey=deadbeef").unwrap();
        assert_eq!(tracker_display(&u), "http://www.torrentbytes.net:8080");
        let u = url::Url::parse("udp://tracker.opentrackr.org:1337/announce").unwrap();
        assert_eq!(tracker_display(&u), "udp://tracker.opentrackr.org:1337");
    }

    #[test]
    fn magnet_has_hash_and_escaped_name_only() {
        let m = magnet("ab12", "Some Film (2019) [1080p]");
        assert_eq!(m, "magnet:?xt=urn:btih:ab12&dn=Some%20Film%20%282019%29%20%5B1080p%5D");
        assert!(!m.contains("tr="));
    }

    #[test]
    fn graph_scale_is_round() {
        // Powers of ten in f32 are not exact (10^-2 * 5 ≠ 0.05 bit for bit): compare with a tolerance.
        for (v, want) in [(0.05, 0.05), (0.07, 0.1), (3.2, 5.0), (12.0, 20.0), (21.0, 25.0), (100.0, 100.0), (0.3, 0.5)] {
            let got = nice_ceiling(v);
            assert!((got - want).abs() <= want * 1e-5, "nice_ceiling({v}) = {got}, want {want}");
        }
        // Idle: 50 KB/s top in 5 steps of 10 KB/s.
        let (top, steps) = scale(0.0);
        assert!(((top * 1024.0) - 50.0).abs() < 1e-3 && steps == 5);
        // 150 KB/s peak → 200 KB/s top, 4 steps of 50.
        let (top, steps) = scale(150.0 / 1024.0);
        assert!(((top * 1024.0) - 200.0).abs() < 1e-3 && steps == 4);
        // 18 MB/s peak → 25 MB/s top (18 × 1.15 = 20.7 → 25), 5 steps of 5.
        let (top, steps) = scale(18.0);
        assert!((top - 25.0).abs() < 1e-4 && steps == 5);
    }
}
