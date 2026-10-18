//! ZenTorrent in the system tray (Windows): closing the window keeps it running
//! there, so downloads and seeding carry on. A click on the ⚡ icon brings the window
//! back; its menu has Open and Quit. On unless turned off under Settings.
//!
//! The window is hidden and shown with Win32 `ShowWindow` on its handle, not with
//! egui's `Visible` command: a window egui has hidden gets no frames on Windows, so it
//! could never process the command that shows it again.

use eframe::egui;

/// The setting in prefs.json.
const PREF: &str = "keep_in_tray";

/// The tray exists on Windows only.
pub fn supported() -> bool {
    cfg!(windows)
}

/// What the user chose (on when never chosen).
pub fn wanted() -> bool {
    crate::pref(PREF).and_then(|v| v.as_bool()).unwrap_or(true)
}

pub fn set_wanted(on: bool) {
    crate::set_pref(PREF, serde_json::json!(on));
}

/// "Quit ZenTorrent" was chosen in the tray menu: the next close really quits.
pub static QUIT: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The ⚡ icon: a yellow bolt on a dark rounded square, `n`×`n` RGBA.
#[cfg_attr(not(windows), allow(dead_code))]
pub fn icon_rgba(n: u32) -> Vec<u8> {
    let s = n as f32 / 32.0;
    let bolt: [(f32, f32); 6] = [(19.0, 3.0), (7.0, 18.0), (15.0, 18.0), (12.0, 29.0), (25.0, 13.0), (17.0, 13.0)];
    let bolt: Vec<(f32, f32)> = bolt.iter().map(|&(x, y)| (x * s, y * s)).collect();
    let inside = |px: f32, py: f32| {
        let mut odd = false;
        for i in 0..bolt.len() {
            let (a, b) = (bolt[i], bolt[(i + 1) % bolt.len()]);
            if (a.1 > py) != (b.1 > py) && px < a.0 + (py - a.1) / (b.1 - a.1) * (b.0 - a.0) {
                odd = !odd;
            }
        }
        odd
    };
    let r = 6.0 * s;
    let mut out = Vec::with_capacity((n * n * 4) as usize);
    for y in 0..n {
        for x in 0..n {
            let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
            // Rounded corners: outside the corner circles is transparent.
            let (cx, cy) = (px.clamp(r, n as f32 - r), py.clamp(r, n as f32 - r));
            let corner = (px - cx).powi(2) + (py - cy).powi(2) > r * r;
            let px_rgba = if corner {
                [0, 0, 0, 0]
            } else if inside(px, py) {
                [250, 200, 40, 255]
            } else {
                [28, 30, 36, 255]
            };
            out.extend_from_slice(&px_rgba);
        }
    }
    out
}

#[cfg(windows)]
mod imp {
    use super::*;
    use std::sync::atomic::{AtomicIsize, Ordering};
    use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent};
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        FindWindowW, GetWindowLongPtrW, IsIconic, SetForegroundWindow, SetWindowLongPtrW, ShowWindow, GWLP_HWNDPARENT, SW_HIDE,
        SW_RESTORE, SW_SHOW,
    };

    /// The main window's handle, set on the first frame (the tray handlers need it).
    static HWND: AtomicIsize = AtomicIsize::new(0);

    pub struct Tray {
        _icon: TrayIcon,
    }

    pub fn set_window(hwnd: isize) {
        HWND.store(hwnd, Ordering::Relaxed);
    }

    pub fn create(ctx: &egui::Context) -> Result<Tray, String> {
        let menu = Menu::new();
        let open = MenuItem::new("Open ZenTorrent", true, None);
        let quit = MenuItem::new("Quit ZenTorrent", true, None);
        menu.append_items(&[&open, &PredefinedMenuItem::separator(), &quit]).map_err(|e| e.to_string())?;
        let icon = Icon::from_rgba(icon_rgba(32), 32, 32).map_err(|e| e.to_string())?;
        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_menu_on_left_click(false)
            .with_tooltip("ZenTorrent")
            .with_icon(icon)
            .build()
            .map_err(|e| format!("could not add the tray icon: {e}"))?;

        let c = ctx.clone();
        TrayIconEvent::set_event_handler(Some(move |e: TrayIconEvent| match e {
            TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. }
            | TrayIconEvent::DoubleClick { button: MouseButton::Left, .. } => show(&c),
            _ => {}
        }));
        let (c, open_id, quit_id) = (ctx.clone(), open.id().clone(), quit.id().clone());
        MenuEvent::set_event_handler(Some(move |e: MenuEvent| {
            if e.id == open_id {
                show(&c);
            } else if e.id == quit_id {
                QUIT.store(true, Ordering::Relaxed);
                show(&c);
                c.send_viewport_cmd(egui::ViewportCommand::Close);
                c.request_repaint();
            }
        }));
        Ok(Tray { _icon: tray })
    }

    /// Back on screen, in front.
    pub fn show(ctx: &egui::Context) {
        let h = HWND.load(Ordering::Relaxed);
        if h != 0 {
            // SAFETY: a window handle of this process; Win32 ignores a stale one.
            unsafe {
                ShowWindow(h as _, if IsIconic(h as _) != 0 { SW_RESTORE } else { SW_SHOW });
                SetForegroundWindow(h as _);
            }
        }
        ctx.request_repaint();
    }

    /// The video player's window (mpv's own, found by its `title`) belongs to the main
    /// window: clicking the player's buttons in the main window no longer sends the video
    /// behind it. An owned window stays in front of its owner only, not of other programs,
    /// and is minimised with it. mpv makes a new window for each video, so this is done
    /// whenever the window is there and not ours yet; cheap enough for every frame.
    pub fn own_video_window(title: &str) {
        let owner = HWND.load(Ordering::Relaxed);
        if owner == 0 {
            return;
        }
        let name: Vec<u16> = title.encode_utf16().chain(Some(0)).collect();
        // SAFETY: a NUL-terminated UTF-16 title; a window handle of this process (libmpv
        // runs in it); Win32 ignores a stale one.
        unsafe {
            let video = FindWindowW(std::ptr::null(), name.as_ptr());
            if !video.is_null() && GetWindowLongPtrW(video, GWLP_HWNDPARENT) != owner {
                SetWindowLongPtrW(video, GWLP_HWNDPARENT, owner);
            }
        }
    }

    /// Off the screen and the taskbar; still running, in the tray. False before the
    /// window's handle is known.
    pub fn hide() -> bool {
        let h = HWND.load(Ordering::Relaxed);
        if h == 0 {
            return false;
        }
        // SAFETY: as above.
        unsafe { ShowWindow(h as _, SW_HIDE) };
        true
    }
}

#[cfg(not(windows))]
mod imp {
    use super::*;
    pub struct Tray;
    pub fn create(_: &egui::Context) -> Result<Tray, String> {
        Err("the tray is a Windows feature".into())
    }
    pub fn hide() -> bool {
        false
    }
    pub fn own_video_window(_: &str) {}
}

pub use imp::*;

#[cfg(test)]
mod tests {
    #[test]
    fn icon_is_a_bolt_on_a_rounded_square() {
        let px = super::icon_rgba(32);
        assert_eq!(px.len(), 32 * 32 * 4);
        let at = |x: usize, y: usize| &px[(y * 32 + x) * 4..(y * 32 + x) * 4 + 4];
        assert_eq!(at(0, 0)[3], 0, "corner transparent");
        assert_eq!(at(15, 15), [250, 200, 40, 255], "bolt in the middle");
        assert_eq!(at(3, 28), [28, 30, 36, 255], "background bottom left");
    }
}
