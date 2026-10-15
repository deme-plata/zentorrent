//! How the Downloads list looks and in which order: three layouts (cards with
//! every detail, a compact one-line list, a thumbnail grid), sort orders, and a
//! filter by what the torrent holds (video, music, other). Remembered across
//! starts in prefs.json; the sidebar's status / tracker / label filter still
//! applies on top.

use eframe::egui::{self, Color32, RichText};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Layout {
    #[default]
    Cards,
    Compact,
    Thumbs,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Sort {
    /// The order they were added in.
    #[default]
    Added,
    Name,
    Size,
    Progress,
    /// Download + upload speed right now.
    Speed,
    Ratio,
    Status,
    /// The user's own 1–10 rating.
    Rating,
}

impl Sort {
    const ALL: [Sort; 8] = [Sort::Added, Sort::Name, Sort::Size, Sort::Progress, Sort::Speed, Sort::Ratio, Sort::Status, Sort::Rating];
    fn label(self) -> &'static str {
        match self {
            Sort::Added => "Added",
            Sort::Name => "Name",
            Sort::Size => "Size",
            Sort::Progress => "Progress",
            Sort::Speed => "Speed",
            Sort::Ratio => "Ratio",
            Sort::Status => "Status",
            Sort::Rating => "Rating",
        }
    }
    /// Which way a fresh pick of this order goes: biggest / fastest first, names A→Z.
    fn natural_desc(self) -> bool {
        !matches!(self, Sort::Name | Sort::Added | Sort::Status)
    }
}

/// What a torrent is mostly made of (by bytes).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Media {
    Video,
    Audio,
    #[default]
    Other,
}

impl Media {
    pub fn glyph(self) -> &'static str {
        match self {
            Media::Video => "🎬",
            Media::Audio => "🎵",
            Media::Other => "📦",
        }
    }
}

/// A volume of a RAR set: .rar, .r00–.r99, .s00 (scene releases split films and
/// episodes into these). Not .zip / .tar: those are as often programs.
fn rar_volume(p: &str) -> bool {
    let ext = p.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
    ext == "rar" || (ext.len() == 3 && (ext.starts_with('r') || ext.starts_with('s')) && ext[1..].bytes().all(|b| b.is_ascii_digit()))
}

/// Video / music / other by where the bytes are. RAR sets count as video: that
/// is what they almost always are.
pub fn media_of<'a>(files: impl IntoIterator<Item = (&'a str, u64)>) -> Media {
    let (mut v, mut a, mut o) = (0u64, 0u64, 0u64);
    for (p, len) in files {
        match crate::player::playlist::kind_of(p) {
            Some(crate::player::playlist::Kind::Video) => v += len,
            Some(crate::player::playlist::Kind::Audio) => a += len,
            None if rar_volume(p) => v += len,
            None => o += len,
        }
    }
    if v == 0 && a == 0 {
        Media::Other
    } else if v >= a && v >= o / 4 {
        Media::Video
    } else if a > v && a >= o / 4 {
        Media::Audio
    } else {
        Media::Other
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Show {
    #[default]
    All,
    Only(Media),
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct ListView {
    #[serde(default)]
    pub layout: Layout,
    #[serde(default)]
    pub sort: Sort,
    #[serde(default)]
    pub desc: bool,
    #[serde(default)]
    pub show: Show,
}

/// What sorting and filtering look at, one per torrent.
pub struct Key<'a> {
    pub name: &'a str,
    pub size: u64,
    pub progress: f64,
    pub speed: f64,
    pub ratio: f64,
    /// Lower = needs attention first: error, downloading, paused, seeding, done.
    pub status: u8,
    pub media: Media,
    /// The user's rating, 0–10.
    pub rating: u8,
}

impl ListView {
    pub fn load() -> Self {
        serde_json::from_value(crate::prefs()["list"].clone()).unwrap_or_default()
    }

    fn save(&self) {
        let mut v = crate::prefs();
        v["list"] = serde_json::to_value(self).unwrap_or_default();
        crate::save_prefs(&v);
    }

    /// Indexes of the torrents to show, in order (`keys[i]` belongs to torrent `i`;
    /// `None` = hidden by the sidebar).
    pub fn arrange(&self, keys: &[Option<Key>]) -> Vec<usize> {
        let mut out: Vec<usize> = keys
            .iter()
            .enumerate()
            .filter_map(|(i, k)| k.as_ref().filter(|k| self.show == Show::All || self.show == Show::Only(k.media)).map(|_| i))
            .collect();
        let k = |i: usize| keys[i].as_ref().unwrap();
        let by = |a: &usize, b: &usize| -> std::cmp::Ordering {
            let (x, y) = (k(*a), k(*b));
            match self.sort {
                Sort::Added => a.cmp(b),
                Sort::Name => x.name.to_lowercase().cmp(&y.name.to_lowercase()),
                Sort::Size => x.size.cmp(&y.size),
                Sort::Progress => x.progress.total_cmp(&y.progress),
                Sort::Speed => x.speed.total_cmp(&y.speed),
                Sort::Ratio => x.ratio.total_cmp(&y.ratio),
                Sort::Status => x.status.cmp(&y.status),
                Sort::Rating => x.rating.cmp(&y.rating),
            }
            // Ties keep the order they were added in.
            .then_with(|| a.cmp(b))
        };
        out.sort_by(|a, b| if self.desc { by(b, a) } else { by(a, b) });
        out
    }

    /// The bar above the list. `counts` = torrents per media kind (sidebar filter applied).
    pub fn toolbar(&mut self, ui: &mut egui::Ui, counts: [usize; 3]) {
        let before = self.clone();
        ui.horizontal(|ui| {
            // What to show.
            let all = counts.iter().sum::<usize>();
            let pill = |ui: &mut egui::Ui, on: bool, text: String| ui.selectable_label(on, RichText::new(text).size(12.5));
            if pill(ui, self.show == Show::All, format!("All {all}")).clicked() {
                self.show = Show::All;
            }
            for (m, name, n) in [(Media::Video, "Video", counts[0]), (Media::Audio, "Music", counts[1]), (Media::Other, "Other", counts[2])] {
                if pill(ui, self.show == Show::Only(m), format!("{} {name} {n}", m.glyph())).clicked() {
                    self.show = if self.show == Show::Only(m) { Show::All } else { Show::Only(m) };
                }
            }

            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                // Layout switch, rightmost.
                for (l, tip) in [
                    (Layout::Thumbs, "Thumbnails — artwork, a progress ring, the name"),
                    (Layout::Compact, "Compact list — one line each; right-click for actions"),
                    (Layout::Cards, "Cards — every detail and button"),
                ] {
                    if layout_button(ui, self.layout == l, l).on_hover_text(tip).clicked() {
                        self.layout = l;
                    }
                }
                ui.separator();
                let arrow = if self.desc { "⬇" } else { "⬆" };
                let tip = if self.desc { "Descending — click for ascending" } else { "Ascending — click for descending" };
                if ui.small_button(arrow).on_hover_text(tip).clicked() {
                    self.desc = !self.desc;
                }
                egui::ComboBox::from_id_salt("zt-sort").selected_text(format!("Sort: {}", self.sort.label())).width(132.0).show_ui(ui, |ui| {
                    for s in Sort::ALL {
                        if ui.selectable_label(self.sort == s, s.label()).clicked() && self.sort != s {
                            self.sort = s;
                            self.desc = s.natural_desc();
                        }
                    }
                });
            });
        });
        if *self != before {
            self.save();
        }
    }
}

/// The layout switch's icons, drawn (the UI font has no reliable glyphs for them).
fn layout_button(ui: &mut egui::Ui, on: bool, l: Layout) -> egui::Response {
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(28.0, 22.0), egui::Sense::click());
    let v = ui.style().interact_selectable(&resp, on);
    let p = ui.painter();
    if on || resp.hovered() {
        p.rect_filled(rect, 4.0, if on { v.bg_fill } else { v.weak_bg_fill });
    }
    let c = v.fg_stroke.color;
    let r = egui::Rect::from_center_size(rect.center(), egui::vec2(14.0, 11.0));
    match l {
        Layout::Cards => {
            let h = (r.height() - 2.0) / 2.0;
            for k in 0..2 {
                let top = r.top() + k as f32 * (h + 2.0);
                p.rect_stroke(egui::Rect::from_min_size(egui::pos2(r.left(), top), egui::vec2(r.width(), h)), 1.5, egui::Stroke::new(1.2, c), egui::StrokeKind::Inside);
            }
        }
        Layout::Compact => {
            for k in 0..4 {
                let y = r.top() + 0.5 + k as f32 * (r.height() - 1.0) / 3.0;
                p.line_segment([egui::pos2(r.left(), y), egui::pos2(r.right(), y)], egui::Stroke::new(1.4, c));
            }
        }
        Layout::Thumbs => {
            let (w, h) = ((r.width() - 2.0) / 2.0, (r.height() - 2.0) / 2.0);
            for (dx, dy) in [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0)] {
                let min = egui::pos2(r.left() + dx * (w + 2.0), r.top() + dy * (h + 2.0));
                p.rect_filled(egui::Rect::from_min_size(min, egui::vec2(w, h)), 1.5, c);
            }
        }
    }
    resp
}

/// Gold, for ratings.
pub const GOLD: Color32 = Color32::from_rgb(245, 190, 40);

/// Ten stars: the user's rating of a torrent, 1–10 (0 = not rated). Hovering shows what a
/// click gives; returns the new rating on a click (clicking the current one clears it).
pub fn stars(ui: &mut egui::Ui, rating: u8) -> Option<u8> {
    let (size, gap) = (11.0, 1.5);
    let (rect, resp) = ui.allocate_exact_size(egui::vec2(10.0 * (size + gap), size + 3.0), egui::Sense::click());
    let hover = resp.hover_pos().map(|p| (((p.x - rect.left()) / (size + gap)).floor() as i32 + 1).clamp(1, 10) as u8);
    let shown = hover.unwrap_or(rating);
    let dim = ui.visuals().weak_text_color().gamma_multiply(0.45);
    for i in 1..=10u8 {
        let c = egui::pos2(rect.left() + (i as f32 - 0.5) * (size + gap), rect.center().y);
        let col = if i > shown {
            dim
        } else if hover.is_some() {
            Color32::from_rgb(255, 220, 110)
        } else {
            GOLD
        };
        ui.painter().text(c, egui::Align2::CENTER_CENTER, "★", egui::FontId::proportional(size), col);
    }
    let tip = match hover {
        Some(h) if h == rating => format!("Your rating: {h}/10 (click to clear)"),
        Some(h) => format!("Rate it {h}/10"),
        None if rating > 0 => format!("Your rating: {rating}/10"),
        None => "Rate it 1–10".to_string(),
    };
    let clicked = resp.on_hover_text(tip).clicked();
    let h = hover?;
    clicked.then_some(if h == rating { 0 } else { h })
}

/// A colour per name, for artwork without a poster.
pub fn tint(name: &str) -> Color32 {
    let h = name.bytes().fold(2166136261u32, |h, b| (h ^ b as u32).wrapping_mul(16777619));
    let hue = (h % 360) as f32 / 360.0;
    egui::ecolor::Hsva::new(hue, 0.55, 0.16, 1.0).into()
}

/// The first letters of a release name's words ("ZT.Test.Show.S01E01" → "ZTS").
pub fn initials(name: &str) -> String {
    name.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().next().is_some_and(|c| c.is_alphabetic()))
        .take(3)
        .filter_map(|w| w.chars().next())
        .flat_map(|c| c.to_uppercase())
        .collect()
}

/// A progress ring: `frac` of a circle in `col` over a faint track.
pub fn ring(p: &egui::Painter, centre: egui::Pos2, r: f32, frac: f32, col: Color32) {
    p.circle_filled(centre, r + 3.0, Color32::from_black_alpha(170));
    p.circle_stroke(centre, r, egui::Stroke::new(3.0, Color32::from_white_alpha(40)));
    let n = (64.0 * frac.clamp(0.0, 1.0)).ceil() as usize;
    if n > 0 {
        let pts: Vec<egui::Pos2> = (0..=n)
            .map(|i| {
                let a = -std::f32::consts::FRAC_PI_2 + std::f32::consts::TAU * frac * i as f32 / n as f32;
                centre + egui::vec2(a.cos(), a.sin()) * r
            })
            .collect();
        p.add(egui::Shape::line(pts, egui::Stroke::new(3.0, col)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(name: &str, size: u64, speed: f64, media: Media) -> Option<Key<'_>> {
        Some(Key { name, size, progress: 0.5, speed, ratio: 1.0, status: 1, media, rating: (size % 11) as u8 })
    }

    #[test]
    fn sort_by_rating_best_first() {
        // key() rates by size % 11: 8, 3, 0 (not rated), 10.
        let keys = [key("a", 8, 0.0, Media::Audio), key("b", 3, 0.0, Media::Audio), key("c", 11, 0.0, Media::Audio), key("d", 10, 0.0, Media::Audio)];
        let mut v = ListView::default();
        v.sort = Sort::Rating;
        v.desc = Sort::Rating.natural_desc();
        assert_eq!(v.arrange(&keys), vec![3, 0, 1, 2], "10, 8, 3, then the unrated one");
    }

    #[test]
    fn sort_filter_and_ties() {
        let keys = vec![key("b-film", 900, 0.0, Media::Video), None, key("A-album", 300, 2.0, Media::Audio), key("c-show", 900, 1.0, Media::Video)];
        let mut v = ListView::default();
        assert_eq!(v.arrange(&keys), [0, 2, 3], "added order; the sidebar-hidden one stays hidden");
        v.sort = Sort::Name;
        assert_eq!(v.arrange(&keys), [2, 0, 3], "names ignore case");
        v.sort = Sort::Size;
        v.desc = true;
        assert_eq!(v.arrange(&keys), [3, 0, 2], "biggest first; equal sizes newest first when descending");
        v.sort = Sort::Speed;
        assert_eq!(v.arrange(&keys), [2, 3, 0]);
        v.show = Show::Only(Media::Video);
        assert_eq!(v.arrange(&keys), [3, 0]);
    }

    #[test]
    fn what_a_torrent_holds() {
        assert_eq!(media_of([("Show/S01E01.mkv", 900), ("Show/sample.txt", 1)]), Media::Video);
        assert_eq!(media_of([("Album/01.flac", 30), ("Album/02.flac", 30), ("Album/cover.jpg", 2)]), Media::Audio);
        assert_eq!(media_of([("Movie/movie.part01.rar", 500), ("Movie/movie.part02.rar", 500), ("Movie/movie.nfo", 1)]), Media::Video);
        assert_eq!(media_of([("Show/show.rar", 50), ("Show/show.r00", 50), ("Show/show.r01", 50), ("Show/show.sfv", 1)]), Media::Video, "old-style volumes");
        assert_eq!(media_of([("debian.iso", 4000)]), Media::Other);
        assert_eq!(media_of([("tool.zip", 4000), ("readme.mp3", 1)]), Media::Other, "a jingle doesn't make a program music");
        assert_eq!(initials("ZT.Test.Show.S01E01.720p-ZEN"), "ZTS");
    }
}
