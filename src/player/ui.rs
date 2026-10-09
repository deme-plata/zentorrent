//! The player's screens: the now-playing bar (bottom), the playlist panel
//! (right) and the Sound window (equalizer and the other sound settings).

use eframe::egui::{self, Color32, RichText};

use super::audio::{Player, Repeat, BANDS, EQ_RANGE, PRESETS};

#[derive(Default)]
pub struct PlayerUi {
    pub show_playlist: bool,
    pub show_sound: bool,
    /// Seek-slider position while it is being dragged.
    seek_drag: Option<f64>,
}

/// What each file of the playing torrent looks like right now.
pub struct Files<'a> {
    /// Bytes downloaded, per file index.
    pub done: &'a [u64],
    /// File sizes, per file index.
    pub len: &'a [u64],
}

impl Files<'_> {
    pub fn complete(&self, f: usize) -> bool {
        matches!((self.done.get(f), self.len.get(f)), (Some(d), Some(l)) if d >= l)
    }
    fn percent(&self, f: usize) -> Option<u64> {
        match (self.done.get(f), self.len.get(f)) {
            (Some(&d), Some(&l)) if l > 0 && d < l => Some(d * 100 / l),
            _ => None,
        }
    }
}

fn clock(s: f64) -> String {
    let s = s.max(0.0) as u64;
    if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60) } else { format!("{}:{:02}", s / 60, s % 60) }
}

/// The now-playing bar.
pub fn bar(ui: &mut egui::Ui, p: &mut Player, pu: &mut PlayerUi, files: &Files) {
    let complete = |f: usize| files.complete(f);
    let live = p.live.lock().unwrap().clone();
    let Some(track) = p.queue.current().cloned() else { return };
    ui.add_space(4.0);
    ui.horizontal(|ui| {
        if ui.button("⏮").on_hover_text("Previous (or back to the start)").clicked() {
            p.prev(&complete);
        }
        let play = if live.paused || live.idle { "▶" } else { "⏸" };
        if ui.add(egui::Button::new(RichText::new(play).size(18.0))).on_hover_text("Play / pause").clicked() {
            if live.idle {
                if let Some(pos) = p.queue.pos {
                    p.jump(pos, &complete);
                }
            } else {
                p.toggle_pause();
            }
        }
        if ui.button("⏭").on_hover_text("Next").clicked() {
            p.next(&complete);
        }
        ui.add_space(6.0);
        let title_w = (ui.available_width() * 0.28).clamp(120.0, 300.0);
        ui.vertical(|ui| {
            ui.set_width(title_w);
            // While streaming, mpv only knows the zt:// URL, whose last part is the file index.
            let from_url = live.title.contains("zt://") || live.title == track.file.to_string();
            let title = if live.title.is_empty() || from_url { track.title.clone() } else { live.title.clone() };
            ui.add(egui::Label::new(RichText::new(title).strong()).truncate()).on_hover_text(&track.path);
            let mut sub = p.queue.torrent.clone();
            if !live.artist.is_empty() {
                sub = format!("{} · {sub}", live.artist);
            }
            if let Some(pc) = files.percent(track.file) {
                sub = format!("streaming, {pc} % downloaded · {sub}");
            }
            ui.add(egui::Label::new(RichText::new(sub).small().weak()).truncate());
        });
        // Right-hand controls are laid out first (right to left), so they always
        // fit; the seek slider then takes whatever width is left.
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if ui.small_button("x").on_hover_text("Stop").clicked() {
                p.stop();
            }
            if ui.selectable_label(pu.show_playlist, "Playlist").clicked() {
                pu.show_playlist = !pu.show_playlist;
            }
            if ui.selectable_label(pu.show_sound, "Sound").on_hover_text("Equalizer and sound settings").clicked() {
                pu.show_sound = !pu.show_sound;
            }
            ui.spacing_mut().slider_width = 70.0;
            let mut v = p.sound.volume;
            let vr = ui.add(egui::Slider::new(&mut v, 0.0..=130.0).show_value(false)).on_hover_text(format!("Volume {v:.0} %"));
            if vr.changed() {
                p.sound.volume = v;
                p.apply_sound();
            }
            let (glyph, tip, next) = match p.queue.repeat {
                Repeat::Off => ("🔁", "Repeat: off", Repeat::All),
                Repeat::All => ("🔁", "Repeat: all", Repeat::One),
                Repeat::One => ("🔂", "Repeat: this track", Repeat::Off),
            };
            if ui.selectable_label(p.queue.repeat != Repeat::Off, glyph).on_hover_text(tip).clicked() {
                p.queue.repeat = next;
                p.requeue(&complete);
            }
            let shuffle = p.queue.shuffle;
            if ui.selectable_label(shuffle, "🔀").on_hover_text("Shuffle").clicked() {
                let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(1);
                p.queue.set_shuffle(!shuffle, seed);
                p.requeue(&complete);
            }
            // Seek: the slider follows playback; a drag seeks once, when released.
            let dur = if live.duration > 0.0 { live.duration } else { track.duration.unwrap_or(0.0) };
            let mut t = pu.seek_drag.unwrap_or(live.time).min(dur.max(0.0));
            ui.label(RichText::new(clock(dur)).monospace().small());
            ui.spacing_mut().slider_width = (ui.available_width() - 48.0).max(60.0);
            let r = ui.add_enabled(dur > 0.0, egui::Slider::new(&mut t, 0.0..=dur.max(0.001)).show_value(false));
            if r.dragged() || r.changed() {
                pu.seek_drag = Some(t);
            }
            if r.drag_stopped() || (r.changed() && !r.dragged()) {
                p.seek(t);
                pu.seek_drag = None;
            }
            ui.label(RichText::new(clock(t)).monospace().small());
        });
    });
    if let Some(e) = live.error.as_ref().or(p.error.as_ref()) {
        ui.colored_label(Color32::LIGHT_RED, e);
    }
    ui.add_space(4.0);
}

/// The playlist panel: click to play, drag to reorder.
pub fn playlist(ui: &mut egui::Ui, p: &mut Player, files: &Files) {
    let complete = |f: usize| files.complete(f);
    let total: f64 = p.queue.order.iter().filter_map(|&i| p.queue.tracks[i].duration).sum();
    ui.add_space(6.0);
    ui.strong("Playlist");
    ui.label(
        RichText::new(format!(
            "{} · {} tracks{}",
            p.queue.torrent,
            p.queue.order.len(),
            if total > 0.0 { format!(" · {}", clock(total)) } else { String::new() }
        ))
        .small()
        .weak(),
    );
    ui.separator();
    let mut play = None;
    let mut moved: Option<(usize, usize)> = None;
    let mut remove = None;
    egui::ScrollArea::vertical().show(ui, |ui| {
        for (pos, &ti) in p.queue.order.iter().enumerate() {
            let t = &p.queue.tracks[ti];
            let current = p.queue.pos == Some(pos);
            let id = egui::Id::new(("zt-pl", pos));
            let row = ui.dnd_drag_source(id, pos, |ui| {
                ui.horizontal(|ui| {
                    ui.set_width(ui.available_width());
                    let n = RichText::new(format!("{:>2}", pos + 1)).monospace().weak();
                    ui.label(if current { RichText::new("▶").color(Color32::from_rgb(90, 200, 120)) } else { n });
                    let title = if current { RichText::new(&t.title).strong() } else { RichText::new(&t.title) };
                    ui.label(title).on_hover_text(&t.path);
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if !current && ui.small_button("x").on_hover_text("Remove from the playlist").clicked() {
                            remove = Some(pos);
                        }
                        if let Some(pc) = files.percent(t.file) {
                            ui.label(RichText::new(format!("{pc} %")).small().color(Color32::from_rgb(230, 190, 80)))
                                .on_hover_text("still downloading: plays as a stream");
                        }
                        if let Some(d) = t.duration {
                            ui.label(RichText::new(clock(d)).small().weak().monospace());
                        }
                    });
                });
            });
            let resp = row.response.interact(egui::Sense::click());
            if current {
                ui.painter().rect_stroke(resp.rect, 4.0, egui::Stroke::new(1.0, Color32::from_rgb(90, 200, 120)), egui::StrokeKind::Inside);
            }
            if resp.double_clicked() || resp.clicked() {
                play = Some(pos);
            }
            if let Some(from) = resp.dnd_release_payload::<usize>() {
                moved = Some((*from, pos));
            }
        }
    });
    if let Some(at) = remove {
        p.queue.remove(at);
        p.requeue(&complete);
    } else if let Some((from, to)) = moved {
        p.queue.move_item(from, to);
        p.requeue(&complete);
    } else if let Some(pos) = play {
        p.jump(pos, &complete);
    }
}

/// The Sound window: 10-band equalizer with presets, and the other sound settings.
pub fn sound(ctx: &egui::Context, p: &mut Player, open: &mut bool) {
    let mut apply = false;
    egui::Window::new("Sound").open(open).resizable(false).default_width(560.0).show(ctx, |ui| {
        ui.horizontal(|ui| {
            ui.label("Preset");
            egui::ComboBox::from_id_salt("zt-eq-preset").selected_text(p.sound.preset.clone()).show_ui(ui, |ui| {
                for (name, gains) in PRESETS {
                    if ui.selectable_label(p.sound.preset == name, name).clicked() {
                        p.sound.preset = name.to_string();
                        p.sound.eq = gains;
                        apply = true;
                    }
                }
            });
            if ui.small_button("Reset").clicked() {
                p.sound.eq = [0.0; 10];
                p.sound.preset = "Flat".into();
                apply = true;
            }
        });
        ui.add_space(6.0);
        ui.horizontal(|ui| {
            for (i, hz) in BANDS.iter().enumerate() {
                ui.vertical(|ui| {
                    ui.set_width(44.0);
                    let g = &mut p.sound.eq[i];
                    ui.label(RichText::new(format!("{:+.0}", *g)).small().monospace());
                    ui.spacing_mut().slider_width = 140.0;
                    let r = ui.add(egui::Slider::new(g, -EQ_RANGE..=EQ_RANGE).vertical().show_value(false).step_by(0.5));
                    // Re-building mpv's filter chain on every pixel of a drag would stutter: apply on release.
                    if r.drag_stopped() || (r.changed() && !r.dragged()) {
                        p.sound.preset = "Custom".into();
                        apply = true;
                    }
                    let label = if *hz >= 1000 { format!("{}k", hz / 1000) } else { hz.to_string() };
                    ui.label(RichText::new(label).small().weak());
                });
            }
        });
        ui.separator();
        egui::Grid::new("zt-sound").num_columns(2).spacing([16.0, 8.0]).show(ui, |ui| {
            ui.label("Even out loudness");
            apply |= ui.checkbox(&mut p.sound.normalize, "loud and quiet tracks play at the same level").changed();
            ui.end_row();
            ui.label("ReplayGain");
            ui.horizontal(|ui| {
                for (v, l) in [("no", "off"), ("track", "per track"), ("album", "per album")] {
                    if ui.selectable_label(p.sound.replaygain == v, l).clicked() {
                        p.sound.replaygain = v.into();
                        apply = true;
                    }
                }
            });
            ui.end_row();
            ui.label("Headphones");
            apply |= ui.checkbox(&mut p.sound.crossfeed, "crossfeed (less tiring hard-panned stereo)").changed();
            ui.end_row();
            ui.label("Stereo width");
            let r = ui.add(egui::Slider::new(&mut p.sound.width, 0.0..=2.5).step_by(0.05).custom_formatter(|v, _| {
                if (v - 1.0).abs() < 0.01 { "as recorded".into() } else if v < 0.01 { "mono".into() } else { format!("{v:.2}×") }
            }));
            apply |= r.drag_stopped() || (r.changed() && !r.dragged());
            ui.end_row();
            ui.label("Speed");
            let r = ui.add(egui::Slider::new(&mut p.sound.speed, 0.5..=2.0).step_by(0.05).suffix("×").text("pitch stays the same"));
            apply |= r.drag_stopped() || (r.changed() && !r.dragged());
            ui.end_row();
        });
    });
    if apply {
        p.apply_sound();
    }
}
