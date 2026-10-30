//! Skins and see-through backgrounds (Settings → Look).
//!
//! A skin is a whole egui colour set. "Transparency":
//! * Linux (and macOS): only the backgrounds turn see-through, text and buttons stay solid
//!   (the window asks for an alpha channel; needs a desktop with a compositor).
//! * Windows: the whole window (a layered window's opacity). Measured 2026-10-30 on Viktor's
//!   PC: DirectX 12 through DirectComposition (`DxgiFromVisual`) draws the see-through
//!   backgrounds over the pale surface Windows keeps behind every window — removing that
//!   surface (WS_EX_NOREDIRECTIONBITMAP) is only possible when the window is made, and
//!   egui-winit does not offer it. A layered window over `DxgiFromVisual` does fade.
//!
//! The window must open that way, so turning it on takes one restart; after that the slider
//! works live. Saved in prefs as "look".

use std::sync::OnceLock;

use eframe::egui::{self, Color32, Stroke};
use serde_json::json;

#[derive(Clone, Debug, PartialEq)]
pub struct Look {
    pub skin: String,
    /// How see-through the backgrounds are, 0–70 %.
    pub transparency: u8,
}

/// Only the backgrounds fade (not on Windows: there the whole window does).
pub const PANEL_GLASS: bool = !cfg!(windows);

/// Past half, a whole see-through window is hard to read.
pub const MAX_TRANSPARENCY: u8 = if PANEL_GLASS { 70 } else { 50 };

fn alpha(transparency: u8) -> u8 {
    255 - (transparency.min(MAX_TRANSPARENCY) as u32 * 255 / 100) as u8
}

/// Windows: keep the whole window at the chosen opacity (each frame; cheap when unchanged).
pub fn apply_window_alpha(look: &Look) {
    #[cfg(windows)]
    if see_through_window() {
        crate::tray::set_opacity(alpha(look.transparency));
    }
    #[cfg(not(windows))]
    let _ = look;
}

pub struct Skin {
    pub id: &'static str,
    pub name: &'static str,
    /// Panels, the darkest wells (inputs, code), buttons, buttons under the mouse, the accent, text.
    panel: [u8; 3],
    well: [u8; 3],
    widget: [u8; 3],
    hover: [u8; 3],
    accent: [u8; 3],
    text: [u8; 3],
}

const fn skin(id: &'static str, name: &'static str, panel: [u8; 3], well: [u8; 3], widget: [u8; 3], hover: [u8; 3], accent: [u8; 3], text: [u8; 3]) -> Skin {
    Skin { id, name, panel, well, widget, hover, accent, text }
}

/// Midnight is egui's own dark look, ZenTorrent's look until now.
pub const SKINS: &[Skin] = &[
    skin("midnight", "Midnight", [27, 27, 27], [10, 10, 10], [60, 60, 60], [70, 70, 70], [90, 170, 255], [180, 180, 180]),
    skin("nord", "Nord", [46, 52, 64], [36, 41, 51], [67, 76, 94], [76, 86, 106], [136, 192, 208], [216, 222, 233]),
    skin("dracula", "Dracula", [40, 42, 54], [30, 31, 41], [68, 71, 90], [98, 114, 164], [189, 147, 249], [248, 248, 242]),
    skin("solarized", "Solarized", [0, 43, 54], [0, 33, 43], [7, 54, 66], [38, 74, 84], [38, 139, 210], [147, 161, 161]),
    skin("forest", "Forest", [28, 38, 30], [18, 26, 20], [44, 60, 46], [58, 80, 60], [130, 205, 120], [205, 218, 198]),
    skin("ember", "Ember", [36, 26, 24], [22, 15, 13], [62, 41, 35], [86, 56, 45], [255, 145, 60], [232, 212, 202]),
    skin("synthwave", "Synthwave", [34, 20, 50], [22, 12, 34], [58, 34, 84], [84, 50, 120], [255, 90, 205], [240, 222, 255]),
    skin("oled", "OLED black", [0, 0, 0], [0, 0, 0], [32, 32, 32], [48, 48, 48], [80, 160, 255], [210, 210, 210]),
];

fn c(x: [u8; 3]) -> Color32 {
    Color32::from_rgb(x[0], x[1], x[2])
}

/// `a` moved `t` of the way to `b`.
fn mix(a: [u8; 3], b: [u8; 3], t: f32) -> Color32 {
    let m = |i: usize| (a[i] as f32 + (b[i] as f32 - a[i] as f32) * t).round() as u8;
    Color32::from_rgb(m(0), m(1), m(2))
}

fn with_alpha(col: Color32, a: u8) -> Color32 {
    let [r, g, b, _] = col.to_srgba_unmultiplied();
    Color32::from_rgba_unmultiplied(r, g, b, a)
}

impl Skin {
    /// The skin's own colours for its button in Settings: (background, accent).
    pub fn swatch(&self) -> (Color32, Color32) {
        (c(self.panel), c(self.accent))
    }
}

pub fn visuals(look: &Look) -> egui::Visuals {
    let mut v = egui::Visuals::dark();
    if let Some(s) = SKINS.iter().find(|s| s.id == look.skin && s.id != "midnight") {
        v.panel_fill = c(s.panel);
        v.window_fill = c(s.panel);
        v.extreme_bg_color = c(s.well);
        v.code_bg_color = c(s.well);
        v.faint_bg_color = mix(s.panel, s.widget, 0.3);
        v.window_stroke = Stroke::new(1.0, c(s.widget));
        v.hyperlink_color = c(s.accent);
        v.selection.bg_fill = mix(s.panel, s.accent, 0.45);
        v.selection.stroke = Stroke::new(1.0, c(s.text));
        let w = &mut v.widgets;
        w.noninteractive.bg_fill = c(s.panel);
        w.noninteractive.weak_bg_fill = c(s.panel);
        w.noninteractive.bg_stroke = Stroke::new(1.0, c(s.widget));
        w.noninteractive.fg_stroke = Stroke::new(1.0, mix(s.panel, s.text, 0.8));
        w.inactive.bg_fill = c(s.widget);
        w.inactive.weak_bg_fill = c(s.widget);
        w.inactive.fg_stroke = Stroke::new(1.0, c(s.text));
        w.hovered.bg_fill = c(s.hover);
        w.hovered.weak_bg_fill = c(s.hover);
        w.hovered.bg_stroke = Stroke::new(1.0, c(s.accent));
        w.hovered.fg_stroke = Stroke::new(1.5, mix(s.text, [255, 255, 255], 0.5));
        w.active.bg_fill = mix(s.hover, s.accent, 0.5);
        w.active.weak_bg_fill = mix(s.hover, s.accent, 0.5);
        w.active.bg_stroke = Stroke::new(1.0, c(s.accent));
        w.active.fg_stroke = Stroke::new(2.0, Color32::WHITE);
        w.open.bg_fill = c(s.hover);
        w.open.weak_bg_fill = c(s.hover);
        w.open.fg_stroke = Stroke::new(1.0, c(s.text));
    }
    if look.transparency > 0 && PANEL_GLASS {
        let a = alpha(look.transparency);
        v.panel_fill = with_alpha(v.panel_fill, a);
        v.faint_bg_color = with_alpha(v.faint_bg_color, a);
        // Text boxes a little more solid; menus and dialogs stay readable over anything.
        v.extreme_bg_color = with_alpha(v.extreme_bg_color, a.saturating_add(40));
        v.window_fill = with_alpha(v.window_fill, a.max(225));
    }
    v
}

pub fn load() -> Look {
    let v = crate::pref("look").unwrap_or_default();
    let skin = v["skin"].as_str().filter(|s| SKINS.iter().any(|k| k.id == *s)).unwrap_or("midnight");
    Look { skin: skin.into(), transparency: v["transparency"].as_u64().unwrap_or(0).min(MAX_TRANSPARENCY as u64) as u8 }
}

pub fn save(look: &Look) {
    crate::set_pref("look", json!({ "skin": look.skin, "transparency": look.transparency }));
}

static SEE_THROUGH: OnceLock<bool> = OnceLock::new();

/// At start: does the window open see-through (Transparency above 0)?
pub fn open_see_through() -> bool {
    *SEE_THROUGH.get_or_init(|| load().transparency > 0)
}

/// Is the window that opened see-through?
pub fn see_through_window() -> bool {
    SEE_THROUGH.get().copied().unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn midnight_is_the_old_look_and_skins_differ() {
        let midnight = visuals(&Look { skin: "midnight".into(), transparency: 0 });
        assert_eq!(midnight.panel_fill, egui::Visuals::dark().panel_fill);
        let nord = visuals(&Look { skin: "nord".into(), transparency: 0 });
        assert_eq!(nord.panel_fill, Color32::from_rgb(46, 52, 64));
        assert_eq!(nord.hyperlink_color, Color32::from_rgb(136, 192, 208));
        let ids: std::collections::HashSet<_> = SKINS.iter().map(|s| s.id).collect();
        assert_eq!(ids.len(), SKINS.len(), "ids are unique");
    }

    #[test]
    fn transparency_touches_backgrounds_only() {
        if !PANEL_GLASS {
            return; // Windows fades the whole window instead
        }
        let solid = visuals(&Look { skin: "dracula".into(), transparency: 0 });
        let glass = visuals(&Look { skin: "dracula".into(), transparency: 40 });
        assert_eq!(solid.panel_fill.a(), 255);
        assert_eq!(glass.panel_fill.to_srgba_unmultiplied()[3], 153, "40 % see-through");
        assert!(glass.window_fill.a() >= 225, "menus stay readable");
        assert_eq!(glass.widgets.inactive.fg_stroke, solid.widgets.inactive.fg_stroke, "text stays solid");
        let most = visuals(&Look { skin: "dracula".into(), transparency: 100 });
        assert_eq!(most.panel_fill.to_srgba_unmultiplied()[3], 77, "capped at 70 %");
    }
}
