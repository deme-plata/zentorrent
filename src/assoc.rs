//! .torrent files and magnet links open with ZenTorrent (Windows; on unless turned off
//! under Settings). Registered for this user only (HKCU\Software\Classes, no admin
//! rights) and re-applied at every start, so it follows the exe the updater replaces.
//!
//! Windows keeps a choice the user made before (say qBittorrent for .torrent) above
//! any program's registration and lets no program change it; Settings then says so and
//! opens Windows' Default apps, where ZenTorrent is listed.

const PREF: &str = "open_torrent_files";
const PROGID: &str = "ZenTorrent.torrent";

pub fn supported() -> bool {
    cfg!(windows)
}

/// What the user chose (on when never chosen).
pub fn wanted() -> bool {
    crate::pref(PREF).and_then(|v| v.as_bool()).unwrap_or(true)
}

pub fn set_wanted(on: bool) -> Result<(), String> {
    crate::set_pref(PREF, serde_json::json!(on));
    apply(on)
}

/// The program Windows opens .torrent files with because the user chose it there,
/// when that is not ZenTorrent.
pub fn other_choice() -> Option<String> {
    imp::user_choice().filter(|p| p != PROGID)
}

/// Register (or unregister) ZenTorrent for .torrent files and magnet links.
pub fn apply(on: bool) -> Result<(), String> {
    if !supported() {
        return Ok(());
    }
    let exe = std::env::current_exe().map_err(|e| format!("where is ZenTorrent: {e}"))?;
    imp::apply(on, &exe.display().to_string())
}

#[cfg(windows)]
mod imp {
    use super::PROGID;
    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows_sys::Win32::System::Registry::{
        RegDeleteKeyValueW, RegDeleteTreeW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ,
    };
    use windows_sys::Win32::UI::Shell::{SHChangeNotify, SHCNE_ASSOCCHANGED, SHCNF_IDLIST};

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    /// `name` None = the key's default value.
    fn set(key: &str, name: Option<&str>, value: &str) -> Result<(), String> {
        let (k, n, v) = (wide(key), name.map(wide), wide(value));
        let n = n.as_ref().map_or(std::ptr::null(), |n| n.as_ptr());
        // SAFETY: NUL-terminated UTF-16 strings that outlive the call; size in bytes.
        let r = unsafe { RegSetKeyValueW(HKEY_CURRENT_USER, k.as_ptr(), n, REG_SZ, v.as_ptr().cast(), (v.len() * 2) as u32) };
        if r == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(format!("could not register ZenTorrent for .torrent files (error {r} at {key})"))
        }
    }

    fn get(key: &str, name: Option<&str>) -> Option<String> {
        let (k, n) = (wide(key), name.map(wide));
        let n = n.as_ref().map_or(std::ptr::null(), |n| n.as_ptr());
        let mut buf = vec![0u16; 2048];
        let mut bytes = (buf.len() * 2) as u32;
        // SAFETY: `buf` holds `bytes` bytes; the call writes at most that many.
        let r = unsafe {
            RegGetValueW(HKEY_CURRENT_USER, k.as_ptr(), n, RRF_RT_REG_SZ, std::ptr::null_mut(), buf.as_mut_ptr().cast(), &mut bytes)
        };
        (r == ERROR_SUCCESS).then(|| String::from_utf16_lossy(&buf[..(bytes as usize / 2).saturating_sub(1)]))
    }

    fn delete_value(key: &str, name: Option<&str>) {
        let (k, n) = (wide(key), name.map(wide));
        let n = n.as_ref().map_or(std::ptr::null(), |n| n.as_ptr());
        // SAFETY: as above.
        unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, k.as_ptr(), n) };
    }

    fn delete_tree(key: &str) -> Result<(), String> {
        let k = wide(key);
        // SAFETY: as above.
        let r = unsafe { RegDeleteTreeW(HKEY_CURRENT_USER, k.as_ptr()) };
        if r == ERROR_SUCCESS || r == ERROR_FILE_NOT_FOUND {
            Ok(())
        } else {
            Err(format!("could not unregister ZenTorrent (error {r} at {key})"))
        }
    }

    pub fn user_choice() -> Option<String> {
        get(r"Software\Microsoft\Windows\CurrentVersion\Explorer\FileExts\.torrent\UserChoice", Some("ProgId"))
    }

    pub fn apply(on: bool, exe: &str) -> Result<(), String> {
        let cls = r"Software\Classes";
        let open = format!("\"{exe}\" \"%1\"");
        let icon = format!("\"{exe}\",0");
        let ours = |key: &str| get(&format!(r"{cls}\{key}\shell\open\command"), None).is_some_and(|c| c == open);
        if on {
            // Only written when different: no registry churn on every start.
            if !ours(PROGID) || get(&format!(r"{cls}\.torrent"), None).as_deref() != Some(PROGID) || !ours("magnet") {
                set(&format!(r"{cls}\{PROGID}"), None, "Torrent file")?;
                set(&format!(r"{cls}\{PROGID}\DefaultIcon"), None, &icon)?;
                set(&format!(r"{cls}\{PROGID}\shell\open\command"), None, &open)?;
                set(&format!(r"{cls}\.torrent"), None, PROGID)?;
                set(&format!(r"{cls}\.torrent"), Some("Content Type"), "application/x-bittorrent")?;
                set(&format!(r"{cls}\.torrent\OpenWithProgids"), Some(PROGID), "")?;
                set(&format!(r"{cls}\magnet"), None, "URL:Magnet link")?;
                set(&format!(r"{cls}\magnet"), Some("URL Protocol"), "")?;
                set(&format!(r"{cls}\magnet\DefaultIcon"), None, &icon)?;
                set(&format!(r"{cls}\magnet\shell\open\command"), None, &open)?;
                // SAFETY: documented call with no pointers.
                unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED as i32, SHCNF_IDLIST, std::ptr::null(), std::ptr::null()) };
            }
        } else {
            // Only what is ours: another program's registration stays.
            if ours("magnet") {
                delete_tree(&format!(r"{cls}\magnet"))?;
            }
            if get(&format!(r"{cls}\.torrent"), None).as_deref() == Some(PROGID) {
                delete_value(&format!(r"{cls}\.torrent"), None);
            }
            delete_value(&format!(r"{cls}\.torrent\OpenWithProgids"), Some(PROGID));
            delete_tree(&format!(r"{cls}\{PROGID}"))?;
            // SAFETY: as above.
            unsafe { SHChangeNotify(SHCNE_ASSOCCHANGED as i32, SHCNF_IDLIST, std::ptr::null(), std::ptr::null()) };
        }
        Ok(())
    }
}

#[cfg(not(windows))]
mod imp {
    pub fn user_choice() -> Option<String> {
        None
    }
    pub fn apply(_: bool, _: &str) -> Result<(), String> {
        Ok(())
    }
}
