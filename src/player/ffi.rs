//! libmpv, loaded at run time.
//!
//! ZenTorrent does not link libmpv: it opens the shared library when the
//! player is first used (`libmpv.so.2` on Linux; on Windows `libmpv-2.dll`
//! next to the exe, else ZenTorrent's own build fetched on the first Play —
//! see `engine`). Without it the app runs normally and the player says what
//! is missing. This keeps ZenTorrent's own MIT/Apache licence clean (the
//! Rust binding crates are LGPL) and the libmpv build swappable.
//!
//! Only the handful of client-API calls the player uses are bound, with the
//! layouts from mpv's client.h / stream_cb.h (client API 2.x, mpv ≥ 0.35).

use std::ffi::{c_char, c_double, c_int, c_void, CStr, CString};
use std::sync::{Mutex, OnceLock};

#[repr(C)]
pub struct Handle {
    _p: [u8; 0],
}

pub const FORMAT_STRING: c_int = 1;
pub const FORMAT_FLAG: c_int = 3;
pub const FORMAT_INT64: c_int = 4;
pub const FORMAT_DOUBLE: c_int = 5;

pub const EVENT_NONE: c_int = 0;
pub const EVENT_SHUTDOWN: c_int = 1;
pub const EVENT_END_FILE: c_int = 7;
pub const EVENT_FILE_LOADED: c_int = 8;
/// A `script-message` from an input binding (ZenTorrent's own keys in the video window).
pub const EVENT_CLIENT_MESSAGE: c_int = 16;
pub const EVENT_PROPERTY_CHANGE: c_int = 22;

/// mpv_end_file_reason: the file could not be played.
pub const END_ERROR: c_int = 4;

#[repr(C)]
pub struct Event {
    pub event_id: c_int,
    pub error: c_int,
    pub reply_userdata: u64,
    pub data: *mut c_void,
}

#[repr(C)]
pub struct EventProperty {
    pub name: *const c_char,
    pub format: c_int,
    pub data: *mut c_void,
}

#[repr(C)]
pub struct EventEndFile {
    pub reason: c_int,
    pub error: c_int,
    pub playlist_entry_id: i64,
    pub playlist_insert_id: i64,
    pub playlist_insert_num_entries: c_int,
}

#[repr(C)]
pub struct EventClientMessage {
    pub num_args: c_int,
    pub args: *const *const c_char,
}

pub type StreamRead = unsafe extern "C" fn(cookie: *mut c_void, buf: *mut c_char, nbytes: u64) -> i64;
pub type StreamSeek = unsafe extern "C" fn(cookie: *mut c_void, offset: i64) -> i64;
pub type StreamSize = unsafe extern "C" fn(cookie: *mut c_void) -> i64;
pub type StreamClose = unsafe extern "C" fn(cookie: *mut c_void);
pub type StreamCancel = unsafe extern "C" fn(cookie: *mut c_void);

#[repr(C)]
pub struct StreamCbInfo {
    pub cookie: *mut c_void,
    pub read_fn: Option<StreamRead>,
    pub seek_fn: Option<StreamSeek>,
    pub size_fn: Option<StreamSize>,
    pub close_fn: Option<StreamClose>,
    pub cancel_fn: Option<StreamCancel>,
}

pub type StreamOpen = unsafe extern "C" fn(user_data: *mut c_void, uri: *mut c_char, info: *mut StreamCbInfo) -> c_int;

/// mpv error codes used by stream callbacks.
pub const ERROR_GENERIC: i64 = -20;
pub const ERROR_LOADING_FAILED: c_int = -13;
pub const ERROR_UNSUPPORTED: i64 = -18;

/// The libmpv entry points ZenTorrent uses.
pub struct Api {
    _lib: libloading::Library,
    pub create: unsafe extern "C" fn() -> *mut Handle,
    pub initialize: unsafe extern "C" fn(*mut Handle) -> c_int,
    pub terminate_destroy: unsafe extern "C" fn(*mut Handle),
    pub set_option_string: unsafe extern "C" fn(*mut Handle, *const c_char, *const c_char) -> c_int,
    pub set_property_string: unsafe extern "C" fn(*mut Handle, *const c_char, *const c_char) -> c_int,
    pub command: unsafe extern "C" fn(*mut Handle, *mut *const c_char) -> c_int,
    pub observe_property: unsafe extern "C" fn(*mut Handle, u64, *const c_char, c_int) -> c_int,
    pub wait_event: unsafe extern "C" fn(*mut Handle, c_double) -> *mut Event,
    pub wakeup: unsafe extern "C" fn(*mut Handle),
    pub error_string: unsafe extern "C" fn(c_int) -> *const c_char,
    pub stream_cb_add_ro: unsafe extern "C" fn(*mut Handle, *const c_char, *mut c_void, StreamOpen) -> c_int,
    pub client_api_version: unsafe extern "C" fn() -> u64,
}

// The function pointers are plain C entry points; libmpv's client API is thread-safe.
unsafe impl Send for Api {}
unsafe impl Sync for Api {}

/// Library names tried, in order. On Windows the exe's own folder comes first.
fn candidates() -> Vec<std::path::PathBuf> {
    let mut out = Vec::new();
    if let Ok(dir) = std::env::var("ZENTORRENT_LIBMPV") {
        out.push(dir.into());
    }
    #[cfg(windows)]
    {
        if let Some(dir) = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.to_path_buf())) {
            out.push(dir.join("libmpv-2.dll"));
            out.push(dir.join("mpv-2.dll"));
        }
        // ZenTorrent's own build, fetched on the first Play (`engine`).
        out.push(super::engine::path());
        out.push("libmpv-2.dll".into());
        out.push("mpv-2.dll".into());
    }
    #[cfg(not(windows))]
    {
        out.push("libmpv.so.2".into());
        out.push("libmpv.so".into());
    }
    out
}

static API: OnceLock<Api> = OnceLock::new();
static LOADING: Mutex<()> = Mutex::new(());

/// Load libmpv; once it loads, later calls return it. A failure is not remembered,
/// so the engine can be installed while ZenTorrent runs (first Play on Windows).
pub fn api() -> Result<&'static Api, String> {
    if let Some(a) = API.get() {
        return Ok(a);
    }
    let _one_at_a_time = LOADING.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(a) = API.get() {
        return Ok(a);
    }
    let a = load()?;
    Ok(API.get_or_init(|| a))
}

fn load() -> Result<Api, String> {
    let mut tried = Vec::new();
    for path in candidates() {
        // SAFETY: loading libmpv runs its library initialisers, which have no preconditions.
        match unsafe { libloading::Library::new(&path) } {
            Ok(lib) => return bind(lib).map_err(|e| format!("{}: {e}", path.display())),
            Err(e) => tried.push(format!("{} ({e})", path.display())),
        }
    }
    Err(if cfg!(windows) {
        "the player engine (libmpv-2.dll) is not installed".to_string()
    } else {
        "the player engine libmpv is not installed (Debian/Ubuntu: sudo apt install libmpv2)".to_string()
    } + &format!(" — tried {}", tried.join(", ")))
}

fn bind(lib: libloading::Library) -> Result<Api, String> {
    macro_rules! sym {
        ($name:literal) => {
            // SAFETY: the signature matches mpv's client.h for client API 2.x.
            *unsafe { lib.get($name) }.map_err(|e| format!("{}: {e}", String::from_utf8_lossy(&$name[..$name.len() - 1])))?
        };
    }
    let api = Api {
        create: sym!(b"mpv_create\0"),
        initialize: sym!(b"mpv_initialize\0"),
        terminate_destroy: sym!(b"mpv_terminate_destroy\0"),
        set_option_string: sym!(b"mpv_set_option_string\0"),
        set_property_string: sym!(b"mpv_set_property_string\0"),
        command: sym!(b"mpv_command\0"),
        observe_property: sym!(b"mpv_observe_property\0"),
        wait_event: sym!(b"mpv_wait_event\0"),
        wakeup: sym!(b"mpv_wakeup\0"),
        error_string: sym!(b"mpv_error_string\0"),
        stream_cb_add_ro: sym!(b"mpv_stream_cb_add_ro\0"),
        client_api_version: sym!(b"mpv_client_api_version\0"),
        _lib: lib,
    };
    // Client API 2.x is what these layouts are written for.
    let v = unsafe { (api.client_api_version)() };
    if v >> 16 != 2 {
        return Err(format!("libmpv client API {}.{} is not supported (need 2.x)", v >> 16, v & 0xffff));
    }
    Ok(api)
}

/// mpv's text for an error code.
pub fn err_text(api: &Api, code: c_int) -> String {
    let p = unsafe { (api.error_string)(code) };
    if p.is_null() {
        format!("mpv error {code}")
    } else {
        unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
    }
}

/// A Rust string as a C string (interior NULs cannot occur in what we pass, but never panic).
pub fn cstr(s: &str) -> CString {
    CString::new(s.replace('\0', "")).unwrap_or_default()
}
