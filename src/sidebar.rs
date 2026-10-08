//! The left sidebar: a faceted navigator over the transfer list.
//!
//! Three facets — status, tracker, label — combine with AND, plus a search
//! box. Every entry's count is computed with the *other* facets applied, so
//! a number always says what you would see if you clicked it.
//!
//! Trackers are grouped by site (`tracker.torrentleech.org` and
//! `tleechreload.org` are both TorrentLeech), never by full announce URL:
//! private announce URLs carry the passkey, and the sidebar must not show it.
//!
//! Drag a torrent's name from the list onto an entry to act on it: a label
//! tags it, Paused pauses it, Downloading / Seeding resumes it.

use std::collections::{BTreeMap, VecDeque};
use std::time::Instant;

use eframe::egui::{self, Color32, RichText};

/// Below this (MiB/s) a torrent counts as not moving bytes.
pub const ACTIVE_MIBS: f64 = 1.0 / 1024.0; // 1 KB/s
/// Seconds of nothing arriving before a download counts as Stalled. Shorter
/// would flag every torrent while it verifies its last pieces.
pub const STALL_SECS: u32 = 10;
/// Trackers shown before "Show all".
const SITES_SHOWN: usize = 6;
/// Seconds of history in the speed sparkline.
const SPARK_LEN: usize = 90;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Status {
    All,
    Downloading,
    Seeding,
    Completed,
    Paused,
    Active,
    Stalled,
    Errored,
    /// Private, finished, ratio below the floor: leaving now risks the account.
    NeedsSeeding,
}

impl Status {
    pub const MAIN: [Status; 8] = [
        Status::All,
        Status::Downloading,
        Status::Seeding,
        Status::Completed,
        Status::Paused,
        Status::Active,
        Status::Stalled,
        Status::Errored,
    ];

    pub fn label(self) -> &'static str {
        match self {
            Status::All => "All",
            Status::Downloading => "Downloading",
            Status::Seeding => "Seeding",
            Status::Completed => "Completed",
            Status::Paused => "Paused",
            Status::Active => "Active",
            Status::Stalled => "Stalled",
            Status::Errored => "Errored",
            Status::NeedsSeeding => "Needs seeding",
        }
    }

    fn hint(self) -> &'static str {
        match self {
            Status::All => "Every torrent. Drop a torrent here to resume it.",
            Status::Downloading => "Not finished and not paused. Drop a torrent here to resume it.",
            Status::Seeding => "Finished and uploading to others. Drop a torrent here to resume it.",
            Status::Completed => "Every finished torrent, seeding or paused.",
            Status::Paused => "Stopped by you or by the seed goal. Drop a torrent here to pause it.",
            Status::Active => "Moving bytes right now, up or down.",
            Status::Stalled => "Downloading, but nothing is arriving: no peers have the missing pieces.",
            Status::Errored => "The engine reported an error (disk, tracker refusal, bad data).",
            Status::NeedsSeeding => "",
        }
    }

    pub fn color(self) -> Color32 {
        match self {
            Status::All => Color32::from_gray(150),
            Status::Downloading => Color32::from_rgb(70, 130, 220),
            Status::Seeding => Color32::from_rgb(60, 170, 90),
            Status::Completed => Color32::from_rgb(80, 170, 170),
            Status::Paused => Color32::from_rgb(230, 190, 80),
            Status::Active => Color32::from_rgb(150, 210, 70),
            Status::Stalled => Color32::from_rgb(220, 140, 60),
            Status::Errored => Color32::from_rgb(230, 90, 90),
            Status::NeedsSeeding => Color32::from_rgb(170, 120, 230),
        }
    }
}

/// What the sidebar knows about one torrent, computed once per frame.
#[derive(Clone, Debug, Default)]
pub struct Facts {
    pub hash: String,
    pub name: String,
    pub finished: bool,
    pub paused: bool,
    pub error: bool,
    /// The engine has live peer state (not initialising / paused).
    pub live: bool,
    pub down_mibs: f64,
    pub up_mibs: f64,
    /// Seconds in a row that nothing has arrived (from the speed history).
    pub idle_secs: u32,
    pub private: bool,
    pub ratio: f64,
    pub uploaded: u64,
    pub size: u64,
    /// Tracker sites (see [`site`]), sorted and de-duplicated. Empty = DHT only.
    pub sites: Vec<String>,
    pub labels: Vec<String>,
}

impl Facts {
    pub fn is(&self, s: Status, ratio_floor: f64) -> bool {
        let running = !self.paused && !self.error;
        match s {
            Status::All => true,
            Status::Downloading => !self.finished && running,
            Status::Seeding => self.finished && running,
            Status::Completed => self.finished,
            Status::Paused => self.paused && !self.error,
            Status::Active => running && self.down_mibs + self.up_mibs > ACTIVE_MIBS,
            Status::Stalled => {
                !self.finished && running && self.live && self.down_mibs <= ACTIVE_MIBS && self.idle_secs >= STALL_SECS
            }
            Status::Errored => self.error,
            Status::NeedsSeeding => self.private && self.finished && self.ratio < ratio_floor,
        }
    }

    /// `site == ""` means "no tracker at all".
    fn on_site(&self, site: &str) -> bool {
        if site.is_empty() { self.sites.is_empty() } else { self.sites.iter().any(|s| s == site) }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Facet {
    None,
    Status,
    Site,
    Label,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Filter {
    pub status: Status,
    /// `Some("")` = torrents with no tracker.
    pub site: Option<String>,
    pub label: Option<String>,
    pub search: String,
}

impl Default for Filter {
    fn default() -> Self {
        Self { status: Status::All, site: None, label: None, search: String::new() }
    }
}

impl Filter {
    pub fn is_active(&self) -> bool {
        *self != Filter::default()
    }

    pub fn matches(&self, f: &Facts, floor: f64) -> bool {
        self.pass(f, floor, Facet::None)
    }

    fn pass(&self, f: &Facts, floor: f64, skip: Facet) -> bool {
        (skip == Facet::Status || f.is(self.status, floor))
            && (skip == Facet::Site || self.site.as_deref().is_none_or(|s| f.on_site(s)))
            && (skip == Facet::Label || self.label.as_ref().is_none_or(|l| f.labels.contains(l)))
            && self.search_hits(f)
    }

    fn search_hits(&self, f: &Facts) -> bool {
        let needle = self.search.trim().to_lowercase();
        needle.is_empty()
            || f.name.to_lowercase().contains(&needle)
            || f.labels.iter().any(|l| l.to_lowercase().contains(&needle))
            || f.sites.iter().any(|s| s.contains(&needle) || pretty(s).to_lowercase().contains(&needle))
    }

    /// One line for the list header: "Seeding · TorrentBytes · “film”".
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        if self.status != Status::All {
            parts.push(self.status.label().to_string());
        }
        if let Some(s) = &self.site {
            parts.push(pretty(s));
        }
        if let Some(l) = &self.label {
            parts.push(format!("label {l}"));
        }
        if !self.search.trim().is_empty() {
            parts.push(format!("“{}”", self.search.trim()));
        }
        parts.join(" · ")
    }

    pub fn count_status(&self, facts: &[Facts], s: Status, floor: f64) -> usize {
        facts.iter().filter(|f| self.pass(f, floor, Facet::Status) && f.is(s, floor)).count()
    }

    pub fn count_site(&self, facts: &[Facts], site: &str, floor: f64) -> usize {
        facts.iter().filter(|f| self.pass(f, floor, Facet::Site) && f.on_site(site)).count()
    }

    pub fn count_label(&self, facts: &[Facts], label: &str, floor: f64) -> usize {
        facts.iter().filter(|f| self.pass(f, floor, Facet::Label) && f.labels.iter().any(|l| l == label)).count()
    }
}

/// Second-level public suffixes where the site name is three labels deep.
const SECOND_LEVEL: &[&str] = &[
    "co.uk", "org.uk", "ac.uk", "me.uk", "com.au", "net.au", "org.au", "co.nz", "com.br", "co.jp", "co.za",
    "com.tr", "com.cn", "com.mx", "co.in",
];

/// Different hostnames that are the same tracker.
const ALIASES: &[(&str, &str)] = &[("tleechreload.org", "torrentleech.org"), ("tleechreload.net", "torrentleech.org")];

/// The site a tracker host belongs to: `tracker.torrentleech.org` →
/// `torrentleech.org`, `udp://tracker.opentrackr.org` → `opentrackr.org`.
/// IP addresses stay as they are.
pub fn site(host: &str) -> String {
    let h = host.trim_matches(|c| c == '[' || c == ']').trim_end_matches('.').to_ascii_lowercase();
    if h.parse::<std::net::IpAddr>().is_ok() {
        return h;
    }
    let parts: Vec<&str> = h.split('.').filter(|p| !p.is_empty()).collect();
    let n = parts.len();
    let keep = if n >= 3 && SECOND_LEVEL.contains(&format!("{}.{}", parts[n - 2], parts[n - 1]).as_str()) { 3 } else { 2 };
    let s = parts[n.saturating_sub(keep)..].join(".");
    ALIASES.iter().find(|(a, _)| *a == s).map(|(_, c)| c.to_string()).unwrap_or(s)
}

/// A friendly name for a site; unknown sites show as their domain.
pub fn pretty(site: &str) -> String {
    const KNOWN: &[(&str, &str)] = &[
        ("torrentbytes.net", "TorrentBytes"),
        ("torrentleech.org", "TorrentLeech"),
        ("opentrackr.org", "OpenTrackr"),
        ("openbittorrent.com", "OpenBitTorrent"),
        ("linuxtracker.org", "LinuxTracker"),
        ("debian.org", "Debian"),
        ("ubuntu.com", "Ubuntu"),
        ("archlinux.org", "Arch Linux"),
        ("fedoraproject.org", "Fedora"),
        ("archive.org", "Internet Archive"),
    ];
    if site.is_empty() {
        return "No tracker (DHT)".into();
    }
    KNOWN.iter().find(|(s, _)| *s == site).map(|(_, n)| n.to_string()).unwrap_or_else(|| site.to_string())
}

/// A stable colour per name, so a label or tracker keeps its colour.
pub fn tint(name: &str) -> Color32 {
    const P: [(u8, u8, u8); 8] = [
        (90, 160, 240),
        (240, 140, 90),
        (120, 200, 120),
        (200, 120, 220),
        (230, 200, 80),
        (80, 200, 200),
        (240, 110, 150),
        (160, 160, 240),
    ];
    // FNV-1a: stable across runs and platforms, unlike std's hasher.
    let h = name.bytes().fold(0x811c_9dc5_u32, |h, b| (h ^ b as u32).wrapping_mul(0x0100_0193));
    let (r, g, b) = P[h as usize % P.len()];
    Color32::from_rgb(r, g, b)
}

/// Drag-and-drop payload: the info-hash of the torrent being dragged.
pub struct Dragged(pub String);

/// What the sidebar asks the app to do.
#[derive(Debug, PartialEq)]
pub enum Action {
    /// The filter changed: show the transfer list.
    Show,
    Pause(String),
    Resume(String),
    /// (info-hash, label)
    Tag(String, String),
    NewLabel(String),
    DeleteLabel(String),
}

#[derive(Default)]
pub struct Sidebar {
    pub filter: Filter,
    /// Set by Ctrl+F; the search box takes focus on the next frame.
    pub focus_search: bool,
    new_label: String,
    all_sites: bool,
    spark: VecDeque<(f32, f32)>,
    last_sample: Option<Instant>,
}

struct SiteRow {
    site: String,
    total: usize,
    private: bool,
    uploaded: u64,
    size: u64,
    up_mibs: f64,
}

impl Sidebar {
    pub fn ui(&mut self, ui: &mut egui::Ui, facts: &[Facts], labels: &[String], floor: f64) -> Vec<Action> {
        let mut out = Vec::new();
        let (down, up) = facts.iter().fold((0.0, 0.0), |(d, u), f| (d + f.down_mibs, u + f.up_mibs));
        if self.last_sample.is_none_or(|t| t.elapsed().as_secs() >= 1) {
            self.last_sample = Some(Instant::now());
            self.spark.push_back((down as f32, up as f32));
            while self.spark.len() > SPARK_LEN {
                self.spark.pop_front();
            }
        }

        // ── live speed + sparkline ───────────────────────────────────
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            ui.label(RichText::new(format!("⬇ {}", crate::speed(down))).strong().color(Status::Downloading.color()));
            ui.add_space(6.0);
            ui.label(RichText::new(format!("⬆ {}", crate::speed(up))).strong().color(Status::Seeding.color()));
        });
        self.sparkline(ui);
        ui.add_space(4.0);

        let r = ui.add(
            egui::TextEdit::singleline(&mut self.filter.search)
                .hint_text("Search  (Ctrl+F)")
                .desired_width(f32::INFINITY),
        );
        if std::mem::take(&mut self.focus_search) {
            r.request_focus();
        }
        if r.changed() {
            out.push(Action::Show);
        }
        if self.filter.is_active() && ui.small_button("Clear filters").clicked() {
            self.filter = Filter::default();
            out.push(Action::Show);
        }

        // ── status ───────────────────────────────────────────────────
        heading(ui, "STATUS");
        for s in Status::MAIN {
            let n = self.filter.count_status(facts, s, floor);
            let sel = self.filter.status == s;
            let resp = row(ui, sel, s.color(), s.label(), None, n, true);
            if let Some(d) = resp.dnd_release_payload::<Dragged>() {
                match s {
                    Status::Paused => out.push(Action::Pause(d.0.clone())),
                    Status::All | Status::Downloading | Status::Seeding | Status::Active => {
                        out.push(Action::Resume(d.0.clone()))
                    }
                    _ => {}
                }
            }
            if resp.on_hover_text(s.hint()).clicked() {
                self.filter.status = if sel { Status::All } else { s };
                out.push(Action::Show);
            }
        }

        // ── smart views: only when there is something private to protect ─
        if facts.iter().any(|f| f.private) {
            heading(ui, "SMART");
            let s = Status::NeedsSeeding;
            let n = self.filter.count_status(facts, s, floor);
            let sel = self.filter.status == s;
            let resp = row(ui, sel, s.color(), "Needs seeding 🔒", None, n, true).on_hover_text(format!(
                "Private torrents that are finished but below ratio {floor:.2}.\nKeep these seeding: \
                 leaving early counts against your tracker account."
            ));
            if resp.clicked() {
                self.filter.status = if sel { Status::All } else { s };
                out.push(Action::Show);
            }
        }

        // ── trackers ─────────────────────────────────────────────────
        let mut sites: BTreeMap<String, SiteRow> = BTreeMap::new();
        for f in facts {
            let keys: Vec<String> = if f.sites.is_empty() { vec![String::new()] } else { f.sites.clone() };
            for k in keys {
                let r = sites.entry(k.clone()).or_insert_with(|| SiteRow {
                    site: k,
                    total: 0,
                    private: false,
                    uploaded: 0,
                    size: 0,
                    up_mibs: 0.0,
                });
                r.total += 1;
                r.private |= f.private;
                r.uploaded += f.uploaded;
                r.size += f.size;
                r.up_mibs += f.up_mibs;
            }
        }
        if !sites.is_empty() {
            heading(ui, "TRACKERS");
            let mut rows: Vec<SiteRow> = sites.into_values().collect();
            // Private trackers first (their ratio matters), then the busiest.
            rows.sort_by(|a, b| {
                b.private.cmp(&a.private).then(b.total.cmp(&a.total)).then_with(|| pretty(&a.site).cmp(&pretty(&b.site)))
            });
            let n_rows = rows.len();
            for (i, r) in rows.into_iter().enumerate() {
                let sel = self.filter.site.as_deref() == Some(r.site.as_str());
                if i >= SITES_SHOWN && !self.all_sites && !sel {
                    continue;
                }
                let n = self.filter.count_site(facts, &r.site, floor);
                let ratio = if r.size > 0 { r.uploaded as f64 / r.size as f64 } else { 0.0 };
                let name = if r.private { format!("{} 🔒", pretty(&r.site)) } else { pretty(&r.site) };
                let extra = r.private.then(|| (format!("{ratio:.2}"), ratio_color(ratio)));
                let dot = if r.site.is_empty() { Color32::from_gray(110) } else { tint(&r.site) };
                let mut tip = format!(
                    "{}\n{} torrent(s) · uploaded {} of {}",
                    if r.site.is_empty() { "Found peers through DHT only" } else { &r.site },
                    r.total,
                    crate::human(r.uploaded),
                    crate::human(r.size),
                );
                if r.private {
                    tip += &format!(" · ratio {ratio:.2} on this tracker");
                }
                if r.up_mibs > ACTIVE_MIBS {
                    tip += &format!("\nuploading {} now", crate::speed(r.up_mibs));
                }
                if row(ui, sel, dot, &name, extra, n, false).on_hover_text(tip).clicked() {
                    self.filter.site = if sel { None } else { Some(r.site) };
                    out.push(Action::Show);
                }
            }
            if n_rows > SITES_SHOWN {
                let t = if self.all_sites { "Show fewer".to_string() } else { format!("Show all {n_rows} trackers") };
                if ui.small_button(t).clicked() {
                    self.all_sites = !self.all_sites;
                }
            }
        }

        // ── labels ───────────────────────────────────────────────────
        heading(ui, "LABELS");
        let mut all: Vec<String> = labels.to_vec();
        for f in facts {
            for l in &f.labels {
                if !all.contains(l) {
                    all.push(l.clone());
                }
            }
        }
        if all.is_empty() {
            ui.label(RichText::new("Make a label, then drag torrents onto it.").weak().small());
        }
        for l in &all {
            let n = self.filter.count_label(facts, l, floor);
            let sel = self.filter.label.as_ref() == Some(l);
            let resp = row(ui, sel, tint(l), l, None, n, false);
            if let Some(d) = resp.dnd_release_payload::<Dragged>() {
                out.push(Action::Tag(d.0.clone(), l.clone()));
            }
            resp.context_menu(|ui| {
                if ui.button("Delete label").clicked() {
                    out.push(Action::DeleteLabel(l.clone()));
                    ui.close();
                }
            });
            if resp.on_hover_text("Click to filter · drop a torrent here to tag it · right-click to delete").clicked() {
                self.filter.label = if sel { None } else { Some(l.clone()) };
                out.push(Action::Show);
            }
        }
        ui.horizontal(|ui| {
            let r = ui.add(egui::TextEdit::singleline(&mut self.new_label).hint_text("new label").desired_width(ui.available_width() - 34.0));
            let name = self.new_label.trim().to_string();
            let ok = !name.is_empty() && !all.contains(&name);
            let go = ui.add_enabled(ok, egui::Button::new("+")).clicked()
                || (ok && r.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)));
            if go {
                out.push(Action::NewLabel(name));
                self.new_label.clear();
            }
        });
        out
    }

    fn sparkline(&self, ui: &mut egui::Ui) {
        let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 26.0), egui::Sense::hover());
        let p = ui.painter();
        p.rect_filled(rect, 3.0, ui.visuals().extreme_bg_color);
        // While a torrent is dragged, this box says where it can go. It must
        // not be a new line: anything that shifts the layout mid-drag moves
        // the drop targets out from under the cursor.
        if egui::DragAndDrop::has_payload_of_type::<Dragged>(ui.ctx()) {
            let font = egui::TextStyle::Small.resolve(ui.style());
            let col = Status::Paused.color();
            let c = rect.center();
            p.text(egui::pos2(c.x, c.y - 6.0), egui::Align2::CENTER_CENTER, "Drop on a label to tag it", font.clone(), col);
            p.text(egui::pos2(c.x, c.y + 6.0), egui::Align2::CENTER_CENTER, "Paused = pause · Seeding = resume", font, col);
            return;
        }
        let max = self.spark.iter().fold(0.01_f32, |m, &(d, u)| m.max(d).max(u));
        let step = rect.width() / (SPARK_LEN - 1) as f32;
        let x0 = rect.right() - step * (self.spark.len().saturating_sub(1)) as f32;
        for (pick, col) in [(0, Status::Downloading.color()), (1, Status::Seeding.color())] {
            let pts: Vec<egui::Pos2> = self
                .spark
                .iter()
                .enumerate()
                .map(|(i, &(d, u))| {
                    let v = if pick == 0 { d } else { u };
                    egui::pos2(x0 + step * i as f32, rect.bottom() - 2.0 - (rect.height() - 4.0) * v / max)
                })
                .collect();
            if pts.len() >= 2 {
                p.add(egui::Shape::line(pts, egui::Stroke::new(1.5, col)));
            }
        }
    }
}

fn heading(ui: &mut egui::Ui, text: &str) {
    ui.add_space(8.0);
    ui.label(RichText::new(text).small().strong().color(ui.visuals().weak_text_color()));
}

fn ratio_color(r: f64) -> Color32 {
    if r >= 1.0 {
        Color32::from_rgb(90, 200, 120)
    } else if r >= 0.5 {
        Color32::from_rgb(230, 190, 80)
    } else {
        Color32::from_rgb(230, 110, 100)
    }
}

/// One clickable sidebar entry: dot, name, optional small extra, count.
/// Lights up while a dragged torrent hovers over it.
fn row(
    ui: &mut egui::Ui,
    selected: bool,
    dot: Color32,
    text: &str,
    extra: Option<(String, Color32)>,
    count: usize,
    round_dot: bool,
) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(ui.available_width(), 22.0), egui::Sense::click());
    let vis = ui.visuals().clone();
    let drop_hover = egui::DragAndDrop::has_payload_of_type::<Dragged>(ui.ctx()) && resp.contains_pointer();
    let p = ui.painter();
    if drop_hover {
        p.rect_filled(rect, 4.0, vis.selection.bg_fill.gamma_multiply(0.55));
        p.rect_stroke(rect, 4.0, egui::Stroke::new(1.0, vis.selection.stroke.color), egui::StrokeKind::Inside);
    } else if selected {
        p.rect_filled(rect, 4.0, vis.selection.bg_fill);
    } else if resp.hovered() {
        p.rect_filled(rect, 4.0, vis.widgets.hovered.weak_bg_fill);
    }
    let dim = count == 0 && !selected;
    let fg = if selected {
        vis.selection.stroke.color
    } else if dim {
        vis.weak_text_color()
    } else {
        vis.text_color()
    };
    let c = egui::pos2(rect.left() + 11.0, rect.center().y);
    let dot = if dim { dot.gamma_multiply(0.45) } else { dot };
    if round_dot {
        p.circle_filled(c, 4.0, dot);
    } else {
        p.rect_filled(egui::Rect::from_center_size(c, egui::vec2(8.0, 8.0)), 2.0, dot);
    }
    let body = egui::TextStyle::Body.resolve(ui.style());
    let small = egui::TextStyle::Small.resolve(ui.style());
    let right = rect.right() - 8.0;
    p.text(egui::pos2(right, rect.center().y), egui::Align2::RIGHT_CENTER, count.to_string(), body.clone(), fg);
    let mut text_max = rect.width() - 60.0;
    if let Some((e, col)) = extra {
        p.text(egui::pos2(right - 28.0, rect.center().y), egui::Align2::RIGHT_CENTER, e, small, if dim { col.gamma_multiply(0.5) } else { col });
        text_max -= 34.0;
    }
    // Rough cut so long names never run into the count.
    let max_chars = (text_max / 7.5).max(4.0) as usize;
    let shown: String = if text.chars().count() > max_chars {
        text.chars().take(max_chars.saturating_sub(1)).collect::<String>() + "…"
    } else {
        text.to_string()
    };
    p.text(egui::pos2(rect.left() + 22.0, rect.center().y), egui::Align2::LEFT_CENTER, shown, body, fg);
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    fn t(name: &str) -> Facts {
        Facts { hash: name.into(), name: name.into(), live: true, ..Default::default() }
    }

    #[test]
    fn tracker_hosts_group_by_site() {
        assert_eq!(site("tracker.torrentleech.org"), "torrentleech.org");
        assert_eq!(site("tracker.tleechreload.org"), "torrentleech.org");
        assert_eq!(site("www.torrentbytes.net"), "torrentbytes.net");
        assert_eq!(site("tracker.opentrackr.org"), "opentrackr.org");
        assert_eq!(site("bttracker.debian.org"), "debian.org");
        assert_eq!(site("tracker.example.co.uk"), "example.co.uk");
        assert_eq!(site("Tracker.OpenTrackr.org."), "opentrackr.org");
        assert_eq!(site("93.158.213.92"), "93.158.213.92");
        assert_eq!(site("[2001:db8::1]"), "2001:db8::1");
        assert_eq!(pretty("torrentleech.org"), "TorrentLeech");
        assert_eq!(pretty(""), "No tracker (DHT)");
        assert_eq!(pretty("unknown.tld"), "unknown.tld");
    }

    #[test]
    fn statuses_classify_each_torrent() {
        let floor = 1.0;
        let mut dl = t("dl");
        dl.down_mibs = 2.0;
        let mut stalled = t("stalled");
        stalled.down_mibs = 0.0;
        stalled.idle_secs = 30;
        // Finishing: nothing arriving for a moment while the last pieces verify.
        let finishing = Facts { idle_secs: 3, ..t("finishing") };
        assert!(!finishing.is(Status::Stalled, floor), "a short pause is not a stall");
        let seed = Facts { finished: true, up_mibs: 0.5, ..t("seed") };
        let paused = Facts { finished: true, paused: true, ..t("paused") };
        let err = Facts { error: true, ..t("err") };
        let debt = Facts { finished: true, private: true, ratio: 0.4, ..t("debt") };

        assert!(dl.is(Status::Downloading, floor) && dl.is(Status::Active, floor) && !dl.is(Status::Stalled, floor));
        assert!(stalled.is(Status::Stalled, floor) && !stalled.is(Status::Active, floor));
        assert!(seed.is(Status::Seeding, floor) && seed.is(Status::Completed, floor) && seed.is(Status::Active, floor));
        assert!(paused.is(Status::Paused, floor) && paused.is(Status::Completed, floor) && !paused.is(Status::Seeding, floor));
        assert!(err.is(Status::Errored, floor) && !err.is(Status::Downloading, floor) && !err.is(Status::Paused, floor));
        assert!(debt.is(Status::NeedsSeeding, floor));
        assert!(!debt.is(Status::NeedsSeeding, 0.3), "ratio above the floor is fine");
        assert!(!seed.is(Status::NeedsSeeding, floor), "public torrents never owe ratio");
    }

    #[test]
    fn facets_combine_and_counts_ignore_their_own_facet() {
        let floor = 1.0;
        let tl = |n: &str, fin: bool| Facts { finished: fin, sites: vec!["torrentleech.org".into()], ..t(n) };
        let tb = |n: &str, fin: bool| Facts { finished: fin, sites: vec!["torrentbytes.net".into()], ..t(n) };
        let mut film = tl("Some Film", true);
        film.labels = vec!["film".into()];
        let facts = vec![film, tl("a", false), tb("b", true), tb("c", false), Facts { ..t("dht only") }];

        let mut f = Filter { site: Some("torrentleech.org".into()), ..Default::default() };
        // Status counts are restricted to the chosen tracker…
        assert_eq!(f.count_status(&facts, Status::All, floor), 2);
        assert_eq!(f.count_status(&facts, Status::Seeding, floor), 1);
        // …but tracker counts ignore the tracker facet itself.
        assert_eq!(f.count_site(&facts, "torrentbytes.net", floor), 2);
        assert_eq!(f.count_site(&facts, "", floor), 1, "trackerless group");

        f.status = Status::Seeding;
        let shown: Vec<_> = facts.iter().filter(|x| f.matches(x, floor)).map(|x| x.name.as_str()).collect();
        assert_eq!(shown, ["Some Film"]);
        assert_eq!(f.count_site(&facts, "torrentbytes.net", floor), 1, "only b is seeding there");

        f.label = Some("film".into());
        assert_eq!(f.count_label(&facts, "film", floor), 1);
        assert_eq!(f.describe(), "Seeding · TorrentLeech · label film");

        let s = Filter { search: "LEECH".into(), ..Default::default() };
        assert_eq!(facts.iter().filter(|x| s.matches(x, floor)).count(), 2, "search hits the tracker name too");
        assert!(!Filter::default().is_active() && s.is_active());
    }

    #[test]
    fn tint_is_stable() {
        assert_eq!(tint("film"), tint("film"));
        assert_eq!(tint("torrentleech.org"), tint("torrentleech.org"));
    }
}
