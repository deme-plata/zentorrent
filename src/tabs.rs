//! The main tabs, each with an icon drawn in code that comes alive with what it stands
//! for: the Downloads arrow drops into its tray while something downloads, seeding sends
//! sparks up while you upload, the RSS waves pulse while feeds are read, Flux MoE's
//! sparkle spins while it answers, the Settings gear turns under the mouse. Every icon
//! also moves while hovered; idle and unhovered they stand still (no redraws).

use std::f32::consts::{PI, TAU};

use eframe::egui::{self, Color32, Pos2, Stroke, Vec2};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    Downloads,
    Seeding,
    Feeds,
    Moe,
    Settings,
}

impl Icon {
    /// Its own colour, used when it is busy or selected.
    fn accent(self) -> Color32 {
        match self {
            Icon::Downloads => Color32::from_rgb(90, 150, 240),
            Icon::Seeding => Color32::from_rgb(80, 200, 120),
            Icon::Feeds => Color32::from_rgb(245, 160, 60),
            Icon::Moe => Color32::from_rgb(190, 140, 255),
            Icon::Settings => Color32::from_rgb(200, 210, 225),
        }
    }
}

/// One tab: icon and label. `busy` = what the icon stands for is happening now.
pub fn tab(ui: &mut egui::Ui, selected: bool, icon: Icon, label: &str, busy: bool) -> egui::Response {
    let font = egui::TextStyle::Button.resolve(ui.style());
    let galley = ui.painter().layout_no_wrap(label.to_string(), font, Color32::PLACEHOLDER);
    let (pad, icon_w, gap) = (9.0, 18.0, 6.0);
    let size = egui::vec2(pad + icon_w + gap + galley.size().x + pad, 26.0);
    let (rect, resp) = ui.allocate_exact_size(size, egui::Sense::click());
    if !ui.is_rect_visible(rect) {
        return resp;
    }
    let hovered = resp.hovered();
    let v = ui.visuals();
    let accent = icon.accent();
    let p = ui.painter();
    if selected {
        // A soft glow in the tab's own colour behind the selection.
        p.rect_filled(rect.expand(2.0), 9.0, accent.gamma_multiply(0.12));
        p.rect_filled(rect, 7.0, v.selection.bg_fill);
    } else if hovered {
        p.rect_filled(rect, 7.0, v.widgets.hovered.weak_bg_fill);
    }
    let text = if selected { v.selection.stroke.color } else if hovered { v.strong_text_color() } else { v.text_color() };
    let col = if busy || selected || hovered { accent } else { v.text_color() };
    let moving = busy || hovered;
    let t = if moving { ui.input(|i| i.time) as f32 } else { 0.0 };
    let c = egui::pos2(rect.left() + pad + icon_w / 2.0, rect.center().y);
    p.extend(shapes(icon, c, col, t, moving, busy));
    p.galley(egui::pos2(rect.left() + pad + icon_w + gap, rect.center().y - galley.size().y / 2.0), galley, text);
    if moving {
        ui.ctx().request_repaint_after(std::time::Duration::from_millis(33));
    }
    resp
}

/// The shapes of `icon` centred on `c` (about 16 px), at time `t` seconds.
fn shapes(icon: Icon, c: Pos2, col: Color32, t: f32, moving: bool, busy: bool) -> Vec<egui::Shape> {
    let w = 1.7;
    let mut out: Vec<egui::Shape> = Vec::new();
    let at = |x: f32, y: f32| c + egui::vec2(x, y);
    let tray = |out: &mut Vec<egui::Shape>| out.push(egui::Shape::line(vec![at(-7.0, 3.0), at(-7.0, 7.0), at(7.0, 7.0), at(7.0, 3.0)], Stroke::new(w, col)));
    let arrow = |out: &mut Vec<egui::Shape>, y0: f32, y1: f32, tip: f32, col: Color32| {
        out.push(egui::Shape::line_segment([at(0.0, y0), at(0.0, y1)], Stroke::new(w, col)));
        let back = if tip > y0 { tip - 3.5 } else { tip + 3.5 };
        out.push(egui::Shape::line(vec![at(-3.5, back), at(0.0, tip), at(3.5, back)], Stroke::new(w, col)));
    };
    match icon {
        Icon::Downloads => {
            tray(&mut out);
            // The arrow drops into the tray, over and over, while moving.
            let phase = if moving { (t * if busy { 1.4 } else { 0.8 }).fract() } else { 0.55 };
            let y = -6.5 + 8.5 * phase;
            let alpha = if moving { (1.0 - (phase - 0.75).max(0.0) * 4.0).clamp(0.0, 1.0) } else { 1.0 };
            arrow(&mut out, y - 5.0, y + 2.0, y + 2.0, col.gamma_multiply(alpha));
            if busy {
                // The tray fills a little with each drop.
                out.push(egui::Shape::rect_filled(egui::Rect::from_min_max(at(-5.5, 5.0), at(5.5, 6.2)), 1.0, col.gamma_multiply(0.35 + 0.4 * phase)));
            }
        }
        Icon::Seeding => {
            // An up arrow out of the tray; while moving, sparks rise from it.
            tray(&mut out);
            arrow(&mut out, 4.0, -5.0, -5.0, col);
            if moving {
                for k in 0..3 {
                    let ph = (t * 0.9 + k as f32 / 3.0).fract();
                    if ph > 0.97 {
                        continue; // faded out
                    }
                    let x = [-4.5, 0.0, 4.5][k] + (ph * TAU + k as f32).sin() * 1.2;
                    out.push(egui::Shape::circle_filled(at(x, -5.0 - ph * 5.0), 1.4 * (1.0 - ph) + 0.4, col.gamma_multiply(1.0 - ph)));
                }
            }
        }
        Icon::Feeds => {
            // A dot and two waves; while moving, the waves pulse outwards one after the other.
            let o = at(-5.0, 5.0);
            out.push(egui::Shape::circle_filled(o, 2.0, col));
            for (k, r) in [6.0f32, 10.5].into_iter().enumerate() {
                let alpha = if moving { 0.25 + 0.75 * ((t * 4.0 - k as f32 * 1.3).sin() * 0.5 + 0.5) } else { 1.0 };
                let pts: Vec<Pos2> = (0..=12)
                    .map(|i| {
                        let a = -PI / 2.0 + (PI / 2.0) * i as f32 / 12.0;
                        o + egui::vec2(a.cos() * r, a.sin() * r)
                    })
                    .collect();
                out.push(egui::Shape::line(pts, Stroke::new(w, col.gamma_multiply(alpha))));
            }
        }
        Icon::Moe => {
            // A four-pointed sparkle that turns (fast while answering) and breathes, with a
            // small twinkling companion.
            let spin = if moving { t * if busy { 2.4 } else { 0.9 } } else { 0.0 };
            let pulse = if moving { 1.0 + 0.1 * (t * 6.0).sin() } else { 1.0 };
            star(&mut out, at(-1.0, 1.0), 7.0 * pulse, 2.0, spin, col);
            let tw = if moving { 0.4 + 0.6 * ((t * 5.0 + 1.0).sin() * 0.5 + 0.5) } else { 0.8 };
            star(&mut out, at(6.0, -6.0), 3.0 * tw, 1.0, -spin * 1.5, col.gamma_multiply(tw));
        }
        Icon::Settings => {
            // A gear: eight teeth around a ring, turning while moving.
            let spin = if moving { t * 1.6 } else { 0.0 };
            for k in 0..8 {
                let d = Vec2::angled(spin + k as f32 * TAU / 8.0);
                out.push(egui::Shape::line_segment([c + d * 5.0, c + d * 7.6], Stroke::new(2.6, col)));
            }
            out.push(egui::Shape::circle_stroke(c, 5.2, Stroke::new(w + 0.3, col)));
            out.push(egui::Shape::circle_stroke(c, 1.9, Stroke::new(w, col)));
        }
    }
    out
}

/// A four-pointed star: points `r` out, waist `waist`, turned by `angle`. Not convex, so
/// drawn as four kites (centre, waist, point, waist), each of them convex.
fn star(out: &mut Vec<egui::Shape>, c: Pos2, r: f32, waist: f32, angle: f32, col: Color32) {
    for k in 0..4 {
        let a = angle + k as f32 * TAU / 4.0;
        let kite = vec![c, c + Vec2::angled(a - TAU / 8.0) * waist, c + Vec2::angled(a) * r, c + Vec2::angled(a + TAU / 8.0) * waist];
        out.push(egui::Shape::convex_polygon(kite, col, Stroke::NONE));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every icon draws something, and only inside its own box, still or moving, busy
    /// or not, at any time (the arrow, the sparks and the sparkles never fly off).
    #[test]
    fn every_icon_stays_in_its_box_in_every_state() {
        let c = egui::pos2(50.0, 50.0);
        let bx = egui::Rect::from_center_size(c, egui::vec2(26.0, 26.0));
        for icon in [Icon::Downloads, Icon::Seeding, Icon::Feeds, Icon::Moe, Icon::Settings] {
            for (moving, busy) in [(false, false), (true, false), (true, true)] {
                for t in [0.0, 0.37, 0.74, 0.99, 12.9] {
                    let s = shapes(icon, c, Color32::WHITE, t, moving, busy);
                    // A fully faded shape (the falling arrow at the bottom) has no visible bounds.
                    let seen: Vec<egui::Rect> = s.iter().map(|sh| sh.visual_bounding_rect()).filter(|r| r.is_positive()).collect();
                    assert!(!seen.is_empty(), "{icon:?} drew nothing visible");
                    for r in seen {
                        assert!(bx.contains_rect(r), "{icon:?} moving={moving} busy={busy} t={t}: {r:?}");
                    }
                }
            }
        }
    }

    #[test]
    fn only_busy_or_hovered_icons_move() {
        let c = egui::pos2(0.0, 0.0);
        for icon in [Icon::Downloads, Icon::Seeding, Icon::Feeds, Icon::Moe, Icon::Settings] {
            let still = |t| format!("{:?}", shapes(icon, c, Color32::WHITE, t, false, false));
            assert_eq!(still(0.0), still(3.3), "{icon:?} moves while idle");
            let moving = |t| format!("{:?}", shapes(icon, c, Color32::WHITE, t, true, true));
            assert_ne!(moving(0.1), moving(0.4), "{icon:?} does not move while busy");
        }
    }
}
