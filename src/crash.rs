//! When ZenTorrent dies or freezes, it says so: `crash.log` in the data folder, and the next
//! start shows the last one ("ZenTorrent closed unexpectedly last time…") with a button that
//! copies the details. Before this, a tester's "it keeps crashing" could not be checked
//! (2026-10-19): nothing was written down.
//!
//! What goes in:
//! - a panic (an error inside ZenTorrent): message, file and line; on the main thread it ended
//!   the app ("crash"), elsewhere the app kept running ("error, kept running");
//! - on Windows, a crash below Rust (a driver, the video player): the exception code;
//! - the window not answering Windows for 30 s or more ("froze");
//! - a new start that found ZenTorrent frozen and ended it.

use std::path::{Path, PathBuf};

/// The log keeps its newest part only.
const MAX_BYTES: usize = 64 * 1024;
/// What the next start tells the user about (an error that ZenTorrent survived it does not).
const SHOWN: [&str; 4] = ["crash", "native crash", "froze", "ended frozen"];

fn path() -> PathBuf {
    crate::seed::data_dir().join("crash.log")
}

fn seen_path() -> PathBuf {
    crate::seed::data_dir().join("crash.seen")
}

/// Write one entry: `=== <UTC time> · ZenTorrent <version> · <kind>` and the text.
pub fn note(kind: &str, text: &str) {
    note_at(&path(), kind, text, now());
}

fn note_at(path: &Path, kind: &str, text: &str, secs: u64) {
    let mut log = std::fs::read_to_string(path).unwrap_or_default();
    log += &format!("=== {} · ZenTorrent {} · {kind}\n{}\n", utc(secs), crate::update::VERSION, text.trim_end());
    if log.len() > MAX_BYTES {
        let mut cut = log.len() - MAX_BYTES;
        while !log.is_char_boundary(cut) {
            cut += 1;
        }
        // Start at an entry.
        let from = log[cut..].find("=== ").map(|i| cut + i).unwrap_or(cut);
        log = log[from..].to_string();
    }
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let _ = std::fs::write(path, log);
}

fn now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// `2026-10-19 13:28:05 UTC`.
fn utc(secs: u64) -> String {
    let (days, rem) = ((secs / 86_400) as i64, secs % 86_400);
    // Days since 1970-01-01 → civil date (H. Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02} UTC", rem / 3600, rem / 60 % 60, rem % 60)
}

/// Catch panics (and on Windows, crashes below Rust) for the log. Called first in `main`.
pub fn install() {
    let before = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let thread = std::thread::current();
        let name = thread.name().unwrap_or("unnamed").to_string();
        let what = info
            .payload()
            .downcast_ref::<&str>()
            .map(|s| s.to_string())
            .or_else(|| info.payload().downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "(no message)".into());
        let at = info.location().map(|l| format!("{}:{}", l.file(), l.line())).unwrap_or_default();
        let kind = if name == "main" { "crash" } else { "error, kept running" };
        let trace = std::backtrace::Backtrace::force_capture();
        note(kind, &format!("{what}\nat {at} (thread {name})\n{trace}"));
        before(info);
    }));
    #[cfg(windows)]
    unsafe {
        windows_sys::Win32::System::Diagnostics::Debug::SetUnhandledExceptionFilter(Some(on_exception));
    }
}

#[cfg(windows)]
unsafe extern "system" fn on_exception(info: *const windows_sys::Win32::System::Diagnostics::Debug::EXCEPTION_POINTERS) -> i32 {
    // SAFETY: Windows hands a valid record to the filter.
    let (code, addr) = unsafe {
        let rec = (*info).ExceptionRecord;
        if rec.is_null() { (0, std::ptr::null_mut()) } else { ((*rec).ExceptionCode as u32, (*rec).ExceptionAddress) }
    };
    note(
        "native crash",
        &format!("Windows exception 0x{code:08X} at {addr:?}. Windows' Event Viewer (Windows Logs → Application) names the module."),
    );
    0 // EXCEPTION_CONTINUE_SEARCH: Windows' own crash handling goes on
}

/// The last thing worth telling the user about, if they have not seen it yet.
pub struct Last {
    /// `2026-10-19 13:28:05 UTC · ZenTorrent 0.10.18 · crash`
    pub head: String,
    pub kind: String,
    /// The whole entry, for "Copy details".
    pub text: String,
}

impl Last {
    /// Its first line after the header: what went wrong.
    pub fn summary(&self) -> String {
        self.text.lines().nth(1).unwrap_or("").chars().take(160).collect()
    }
}

pub fn last_unseen() -> Option<Last> {
    last_unseen_at(&path(), &seen_path())
}

fn last_unseen_at(path: &Path, seen: &Path) -> Option<Last> {
    let log = std::fs::read_to_string(path).ok()?;
    let start = log.rfind("=== ")?;
    let entry = log[start..].trim_end().to_string();
    let head = entry.lines().next()?.trim_start_matches("=== ").to_string();
    let kind = head.rsplit(" · ").next()?.to_string();
    if !SHOWN.contains(&kind.as_str()) || std::fs::read_to_string(seen).ok().is_some_and(|s| s.trim() == head) {
        return None;
    }
    Some(Last { head, kind, text: entry })
}

/// The user has seen it: not shown again.
pub fn mark_seen(last: &Last) {
    let _ = std::fs::write(seen_path(), &last.head);
}

/// A thread that notes when the window has not answered Windows for 30 s or more (once per
/// freeze). `responding`: None while that cannot be known.
pub fn watchdog(responding: fn() -> Option<bool>) {
    std::thread::Builder::new()
        .name("zt-watchdog".into())
        .spawn(move || {
            let (mut stuck, mut noted) = (0u32, false);
            loop {
                std::thread::sleep(std::time::Duration::from_secs(10));
                match responding() {
                    Some(false) => {
                        stuck += 1;
                        if stuck >= 3 && !noted {
                            noted = true;
                            note("froze", "The window stopped answering Windows for 30 seconds or more (ZenTorrent was still running).");
                        }
                    }
                    _ => (stuck, noted) = (0, false),
                }
            }
        })
        .ok();
}

/// A new start found the running ZenTorrent (`pid`) frozen: ask whether to end it.
/// True when it was ended (this start then carries on).
pub fn ask_to_end_frozen(pid: u32) -> bool {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{OpenProcess, TerminateProcess, PROCESS_TERMINATE};
        use windows_sys::Win32::UI::WindowsAndMessaging::{MessageBoxW, IDYES, MB_ICONWARNING, MB_YESNO};
        let w = |s: &str| s.encode_utf16().chain(Some(0)).collect::<Vec<u16>>();
        let text = w("ZenTorrent is already running, but it is not responding.\n\n\
                      Close it and start ZenTorrent again?\n\n\
                      (Your downloads, feeds and settings are kept.)");
        let title = w("ZenTorrent is not responding");
        // SAFETY: NUL-terminated UTF-16 strings; a process handle we opened and close.
        unsafe {
            if MessageBoxW(std::ptr::null_mut(), text.as_ptr(), title.as_ptr(), MB_YESNO | MB_ICONWARNING) != IDYES {
                return false;
            }
            let h = OpenProcess(PROCESS_TERMINATE, 0, pid);
            if h.is_null() {
                return false;
            }
            let ok = TerminateProcess(h, 1) != 0;
            CloseHandle(h);
            if ok {
                note("ended frozen", &format!("A new start found ZenTorrent (process {pid}) not responding; the user chose to close it."));
            }
            ok
        }
    }
    #[cfg(not(windows))]
    {
        eprintln!("ZenTorrent is already running (process {pid}) but not responding. End it (kill {pid}) and start again.");
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_read_like_dates() {
        assert_eq!(utc(0), "1970-01-01 00:00:00 UTC");
        assert_eq!(utc(1_760_000_000), "2025-10-09 08:53:20 UTC");
        assert_eq!(utc(951_782_400), "2000-02-29 00:00:00 UTC", "leap day");
    }

    #[test]
    fn the_last_crash_is_told_once_and_survived_errors_not_at_all() {
        let d = std::env::temp_dir().join(format!("zt-crash-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        let (log, seen) = (d.join("crash.log"), d.join("crash.seen"));
        assert!(last_unseen_at(&log, &seen).is_none(), "no log");
        note_at(&log, "error, kept running", "a feed parser hiccup\nat src/rss.rs:1", 100);
        assert!(last_unseen_at(&log, &seen).is_none(), "it kept running: nothing to tell");
        note_at(&log, "crash", "index out of bounds\nat src/main.rs:42 (thread main)", 200);
        let last = last_unseen_at(&log, &seen).unwrap();
        assert_eq!(last.kind, "crash");
        assert_eq!(last.summary(), "index out of bounds");
        assert!(last.head.starts_with("1970-01-01 00:03:20 UTC · ZenTorrent "));
        std::fs::write(&seen, &last.head).unwrap();
        assert!(last_unseen_at(&log, &seen).is_none(), "seen: not again");
        note_at(&log, "froze", "The window stopped answering", 300);
        assert_eq!(last_unseen_at(&log, &seen).unwrap().kind, "froze", "a new one is told");
        // It stays small, starting at an entry.
        for i in 0..2000 {
            note_at(&log, "error, kept running", &format!("filler {i} {}", "x".repeat(40)), 400 + i);
        }
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(text.len() <= MAX_BYTES && text.starts_with("=== "));
        let _ = std::fs::remove_dir_all(&d);
    }
}
