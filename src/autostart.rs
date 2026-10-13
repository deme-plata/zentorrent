//! Start ZenTorrent when Windows starts (on unless turned off under Settings), so
//! downloads continue and seeding keeps going after a restart. It is one value under
//! HKCU\Software\Microsoft\Windows\CurrentVersion\Run: no admin rights, and it is
//! the same list Task Manager's "Startup apps" shows. The value points at this exe,
//! which the updater replaces in place, so it stays right across updates.

/// Given when Windows starts ZenTorrent: the window starts minimized.
pub const FLAG: &str = "--autostart";
/// The setting in prefs.json.
const PREF: &str = "start_with_windows";

/// Starting with the computer exists on Windows only.
pub fn supported() -> bool {
    cfg!(windows)
}

/// What the user chose (on when never chosen).
pub fn wanted() -> bool {
    crate::pref(PREF).and_then(|v| v.as_bool()).unwrap_or(true)
}

/// Remember the choice and register or unregister now.
pub fn set_wanted(on: bool) -> Result<(), String> {
    crate::set_pref(PREF, serde_json::json!(on));
    apply(on)
}

/// Make Windows' list match the choice, pointing at this exe.
pub fn apply(on: bool) -> Result<(), String> {
    if !supported() {
        return Ok(());
    }
    if on {
        let exe = std::env::current_exe().map_err(|e| format!("where is ZenTorrent: {e}"))?;
        let cmd = format!("\"{}\" {FLAG}", exe.display());
        if registered().as_deref() != Some(cmd.as_str()) {
            reg::set(&cmd)?;
        }
        Ok(())
    } else {
        reg::remove()
    }
}

/// The command Windows runs at start-up, if any.
pub fn registered() -> Option<String> {
    reg::get()
}

#[cfg(windows)]
mod reg {
    use windows_sys::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows_sys::Win32::System::Registry::{
        RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ,
    };

    const RUN: &str = r"Software\Microsoft\Windows\CurrentVersion\Run";
    const NAME: &str = "ZenTorrent";

    fn wide(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(Some(0)).collect()
    }

    pub fn set(cmd: &str) -> Result<(), String> {
        let (key, name, data) = (wide(RUN), wide(NAME), wide(cmd));
        // SAFETY: NUL-terminated UTF-16 strings that outlive the call; size in bytes.
        let r = unsafe {
            RegSetKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr(), REG_SZ, data.as_ptr().cast(), (data.len() * 2) as u32)
        };
        if r == ERROR_SUCCESS {
            Ok(())
        } else {
            Err(format!("could not add ZenTorrent to Windows start-up (error {r})"))
        }
    }

    pub fn remove() -> Result<(), String> {
        let (key, name) = (wide(RUN), wide(NAME));
        // SAFETY: as above.
        let r = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr()) };
        if r == ERROR_SUCCESS || r == ERROR_FILE_NOT_FOUND {
            Ok(())
        } else {
            Err(format!("could not remove ZenTorrent from Windows start-up (error {r})"))
        }
    }

    pub fn get() -> Option<String> {
        let (key, name) = (wide(RUN), wide(NAME));
        let mut buf = vec![0u16; 2048];
        let mut bytes = (buf.len() * 2) as u32;
        // SAFETY: `buf` holds `bytes` bytes; the call writes at most that many.
        let r = unsafe {
            RegGetValueW(HKEY_CURRENT_USER, key.as_ptr(), name.as_ptr(), RRF_RT_REG_SZ, std::ptr::null_mut(), buf.as_mut_ptr().cast(), &mut bytes)
        };
        (r == ERROR_SUCCESS).then(|| String::from_utf16_lossy(&buf[..(bytes as usize / 2).saturating_sub(1)]))
    }
}

#[cfg(not(windows))]
mod reg {
    pub fn set(_: &str) -> Result<(), String> {
        Ok(())
    }
    pub fn remove() -> Result<(), String> {
        Ok(())
    }
    pub fn get() -> Option<String> {
        None
    }
}
