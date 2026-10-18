//! Choose what to download when adding a torrent with several files: its folders and
//! files as a tree, everything ticked to start with. A folder's box ticks or unticks all
//! inside it (half-ticked when some are).

use std::collections::BTreeMap;

use eframe::egui::{self, RichText};

use crate::engine::Listed;
use crate::player::playlist::kind_of;

/// A torrent waiting for the user's choice.
pub struct Pick {
    pub listed: Listed,
    /// One per `listed.files`.
    pub on: Vec<bool>,
}

impl Pick {
    pub fn new(listed: Listed) -> Self {
        let on = vec![true; listed.files.len()];
        Pick { listed, on }
    }

    /// The files to download (torrent file indexes); None = all of them.
    pub fn only(&self) -> Option<Vec<usize>> {
        if self.on.iter().all(|&b| b) {
            return None;
        }
        Some(self.listed.files.iter().zip(&self.on).filter(|(_, &on)| on).map(|(f, _)| f.0).collect())
    }
}

/// The window. Some(true): download what is ticked; Some(false): don't add it.
/// `ask`: the "ask every time" setting, which the window can turn off.
pub fn show(ctx: &egui::Context, p: &mut Pick, ask: &mut bool, waiting: usize) -> Option<bool> {
    let mut out = None;
    let total: u64 = p.listed.files.iter().map(|f| f.2).sum();
    let (n_on, size_on) = p.listed.files.iter().zip(&p.on).filter(|(_, &on)| on).fold((0, 0u64), |(n, s), (f, _)| (n + 1, s + f.2));
    let modal = egui::Modal::new(egui::Id::new("zt-pick")).show(ctx, |ui| {
        ui.set_width(640.0);
        ui.label(RichText::new("Choose what to download").heading());
        ui.add_space(2.0);
        ui.label(RichText::new(&p.listed.name).strong());
        ui.label(
            RichText::new(format!("{} files · {} · into {}", p.listed.files.len(), crate::human(total), p.listed.folder().display()))
                .weak()
                .small(),
        );
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            if ui.button("All").clicked() {
                p.on.iter_mut().for_each(|b| *b = true);
            }
            if ui.button("None").clicked() {
                p.on.iter_mut().for_each(|b| *b = false);
            }
            let media: Vec<bool> = p.listed.files.iter().map(|f| kind_of(&f.1).is_some()).collect();
            if media.iter().any(|&m| m) && media.iter().any(|&m| !m) && ui.button("Only video & music").on_hover_text("Leaves out .nfo and .txt files, pictures and other files that are not video or music").clicked() {
                p.on = media;
            }
        });
        ui.add_space(4.0);
        egui::ScrollArea::vertical().max_height(380.0).auto_shrink([false, true]).show(ui, |ui| {
            let all: Vec<usize> = (0..p.listed.files.len()).collect();
            let few = p.listed.files.len() <= 40;
            level(ui, &p.listed.files, &mut p.on, &all, 0, egui::Id::new("zt-pick-tree"), few);
        });
        ui.separator();
        ui.horizontal_wrapped(|ui| {
            ui.label(RichText::new(format!("{n_on} of {} files · {}", p.listed.files.len(), crate::human(size_on))).strong());
            if ui.add_enabled(n_on > 0, egui::Button::new(RichText::new("Download").strong())).clicked() {
                out = Some(true);
            }
            if ui.button("Cancel").on_hover_text("Don't add this torrent").clicked() {
                out = Some(false);
            }
            ui.checkbox(ask, "Ask every time").on_hover_text("Off: torrents start with all their files (also under Settings)");
            if waiting > 0 {
                ui.label(RichText::new(format!("{waiting} more waiting")).weak().small());
            }
        });
    });
    if modal.should_close() && out.is_none() {
        out = Some(false);
    }
    out
}

/// One level of the tree: the folders at `depth` (each with its box and its contents), then
/// the files. `members`: positions in `files` under this level.
fn level(ui: &mut egui::Ui, files: &[(usize, String, u64)], on: &mut [bool], members: &[usize], depth: usize, id: egui::Id, open: bool) {
    let mut folders: BTreeMap<String, Vec<usize>> = BTreeMap::new();
    let mut here: Vec<usize> = Vec::new();
    for &k in members {
        let parts: Vec<&str> = files[k].1.split('/').collect();
        if parts.len() > depth + 1 {
            folders.entry(parts[depth].to_string()).or_default().push(k);
        } else {
            here.push(k);
        }
    }
    for (name, inside) in &folders {
        let all = inside.iter().all(|&k| on[k]);
        let any = inside.iter().any(|&k| on[k]);
        let size: u64 = inside.iter().map(|&k| files[k].2).sum();
        let fid = id.with(name);
        let state = egui::collapsing_header::CollapsingState::load_with_default_open(ui.ctx(), fid, open || folders.len() == 1);
        state
            .show_header(ui, |ui| {
                let mut b = all;
                if ui.add(egui::Checkbox::new(&mut b, "").indeterminate(any && !all)).changed() {
                    inside.iter().for_each(|&k| on[k] = b);
                }
                ui.label(RichText::new(format!("{name}/")).strong());
                ui.label(RichText::new(format!("{} files · {}", inside.len(), crate::human(size))).weak().small());
            })
            .body(|ui| level(ui, files, on, inside, depth + 1, fid, open));
    }
    for k in here {
        let name = files[k].1.rsplit('/').next().unwrap_or(&files[k].1).to_string();
        ui.horizontal(|ui| {
            ui.checkbox(&mut on[k], name);
            ui.label(RichText::new(crate::human(files[k].2)).weak().small());
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_ticked_means_everything_and_padding_keeps_its_index() {
        // Torrent index 1 is a padding file: not in the list, but the indexes stay the torrent's.
        let files = vec![(0, "Show/S01E01.mkv".to_string(), 10), (2, "Show/Sample/sample.mkv".to_string(), 1), (3, "Show/info.nfo".to_string(), 1)];
        let mut p = Pick::new(Listed::for_test("Show", files));
        assert_eq!(p.only(), None, "everything ticked: no list");
        p.on[1] = false;
        assert_eq!(p.only(), Some(vec![0, 3]));
    }
}
