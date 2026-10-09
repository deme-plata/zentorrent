//! `zt://<info-hash>/<file index>`: mpv reads a file that is still downloading.
//!
//! mpv calls these C callbacks on its own demuxer thread. Each read goes to
//! librqbit's `FileStream`, which asks the swarm for the pieces just ahead of
//! the read position first, so playback can start long before the download
//! finishes. A read that waits for a missing piece is woken by mpv's cancel
//! callback (stop, skip, quit), so the player never hangs on a slow swarm.

use std::collections::HashMap;
use std::ffi::{c_char, c_int, c_void, CStr};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use librqbit::ManagedTorrent;
use tokio::io::{AsyncReadExt, AsyncSeekExt};

use super::ffi::{self, StreamCbInfo};

pub const SCHEME: &str = "zt";

/// The URL mpv is given for one file of one torrent.
pub fn url(info_hash: &str, file_index: usize) -> String {
    format!("{SCHEME}://{info_hash}/{file_index}")
}

/// `zt://<hash>/<idx>` → (hash, idx).
pub fn parse(uri: &str) -> Option<(String, usize)> {
    let rest = uri.strip_prefix(SCHEME)?.strip_prefix("://")?;
    let (hash, idx) = rest.split_once('/')?;
    (!hash.is_empty() && hash.chars().all(|c| c.is_ascii_hexdigit())).then_some(())?;
    Some((hash.to_ascii_lowercase(), idx.parse().ok()?))
}

/// What the open callback needs: the torrents it may stream from, and a runtime.
pub struct Registry {
    pub rt: tokio::runtime::Handle,
    pub torrents: Mutex<HashMap<String, Arc<ManagedTorrent>>>,
}

/// librqbit's `FileStream` is not exported by name; it is used through these traits.
trait ReadSeek: tokio::io::AsyncRead + tokio::io::AsyncSeek + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncSeek + Unpin + Send> ReadSeek for T {}

/// Per open file. Callbacks only ever take `&Cookie` (cancel arrives on another
/// thread while a read waits), so the stream itself sits behind a mutex.
struct Cookie {
    rt: tokio::runtime::Handle,
    stream: Mutex<Box<dyn ReadSeek>>,
    len: u64,
    cancelled: AtomicBool,
    wake: tokio::sync::Notify,
}

/// Register the protocol on an mpv handle. `reg` must outlive the handle.
pub fn register(api: &ffi::Api, h: *mut ffi::Handle, reg: &'static Registry) -> Result<(), String> {
    let scheme = ffi::cstr(SCHEME);
    let rc = unsafe { (api.stream_cb_add_ro)(h, scheme.as_ptr(), reg as *const Registry as *mut c_void, open) };
    if rc < 0 {
        return Err(ffi::err_text(api, rc));
    }
    Ok(())
}

unsafe extern "C" fn open(user: *mut c_void, uri: *mut c_char, info: *mut StreamCbInfo) -> c_int {
    let reg = &*(user as *const Registry);
    let uri = CStr::from_ptr(uri).to_string_lossy();
    let Some((hash, idx)) = parse(&uri) else { return ffi::ERROR_LOADING_FAILED };
    let Some(handle) = reg.torrents.lock().unwrap().get(&hash).cloned() else { return ffi::ERROR_LOADING_FAILED };
    let stream = match reg.rt.block_on(handle.stream(idx)) {
        Ok(s) => s,
        Err(_) => return ffi::ERROR_LOADING_FAILED,
    };
    let cookie = Box::new(Cookie {
        rt: reg.rt.clone(),
        len: stream.len(),
        stream: Mutex::new(Box::new(stream)),
        cancelled: AtomicBool::new(false),
        wake: tokio::sync::Notify::new(),
    });
    *info = StreamCbInfo {
        cookie: Box::into_raw(cookie) as *mut c_void,
        read_fn: Some(read),
        seek_fn: Some(seek),
        size_fn: Some(size),
        close_fn: Some(close),
        cancel_fn: Some(cancel),
    };
    0
}

unsafe extern "C" fn read(cookie: *mut c_void, buf: *mut c_char, n: u64) -> i64 {
    let c = &*(cookie as *const Cookie);
    if c.cancelled.load(Ordering::Acquire) {
        return ffi::ERROR_GENERIC;
    }
    let out = std::slice::from_raw_parts_mut(buf as *mut u8, n.min(1 << 20) as usize);
    let mut stream = c.stream.lock().unwrap();
    let r = c.rt.block_on(async {
        tokio::select! {
            r = stream.read(out) => Some(r),
            // notify_one leaves a permit, so a cancel that lands just before
            // this wait still wakes it.
            _ = c.wake.notified() => None,
        }
    });
    match r {
        Some(Ok(got)) => got as i64, // 0 = end of file
        _ => ffi::ERROR_GENERIC,
    }
}

unsafe extern "C" fn seek(cookie: *mut c_void, offset: i64) -> i64 {
    let c = &*(cookie as *const Cookie);
    if offset < 0 || c.cancelled.load(Ordering::Acquire) {
        return ffi::ERROR_UNSUPPORTED;
    }
    let mut stream = c.stream.lock().unwrap();
    match c.rt.block_on(stream.seek(std::io::SeekFrom::Start(offset as u64))) {
        Ok(p) => p as i64,
        Err(_) => ffi::ERROR_UNSUPPORTED,
    }
}

unsafe extern "C" fn size(cookie: *mut c_void) -> i64 {
    (*(cookie as *const Cookie)).len as i64
}

unsafe extern "C" fn cancel(cookie: *mut c_void) {
    // Any thread, possibly while a read waits for a piece: flag it and wake the read.
    let c = &*(cookie as *const Cookie);
    c.cancelled.store(true, Ordering::Release);
    c.wake.notify_one();
}

unsafe extern "C" fn close(cookie: *mut c_void) {
    drop(Box::from_raw(cookie as *mut Cookie));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urls_round_trip() {
        let u = url("5b1e0d988fc7a0c9e99bd852071681a59974b39f", 3);
        assert_eq!(u, "zt://5b1e0d988fc7a0c9e99bd852071681a59974b39f/3");
        assert_eq!(parse(&u), Some(("5b1e0d988fc7a0c9e99bd852071681a59974b39f".into(), 3)));
        assert_eq!(parse("zt://ABCDEF/0"), Some(("abcdef".into(), 0)));
        assert_eq!(parse("zt://../etc/passwd"), None);
        assert_eq!(parse("http://abc/1"), None);
        assert_eq!(parse("zt://abc/x"), None);
    }
}
