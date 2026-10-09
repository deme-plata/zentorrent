//! `zt://<info-hash>/<file index>`: mpv reads a file that is still downloading.
//! `zt://<info-hash>/a/<archive>/<member>`: the same for a film inside an archive
//! (see `archive`), read across the archive's volume files.
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

use super::archive;
use super::ffi::{self, StreamCbInfo};

pub const SCHEME: &str = "zt";

/// The URL mpv is given for one file of one torrent.
pub fn url(info_hash: &str, file_index: usize) -> String {
    format!("{SCHEME}://{info_hash}/{file_index}")
}

/// The URL mpv is given for a member of an archive. `archive` = the index of the
/// archive's first volume file, `member` = its place in the archive's file list.
pub fn member_url(info_hash: &str, archive: usize, member: usize) -> String {
    format!("{SCHEME}://{info_hash}/a/{archive}/{member}")
}

#[derive(Debug, PartialEq, Eq)]
pub enum Target {
    File { hash: String, file: usize },
    Member { hash: String, archive: usize, member: usize },
}

/// `zt://<hash>/<idx>` or `zt://<hash>/a/<archive>/<member>`.
pub fn parse(uri: &str) -> Option<Target> {
    let rest = uri.strip_prefix(SCHEME)?.strip_prefix("://")?;
    let (hash, path) = rest.split_once('/')?;
    (!hash.is_empty() && hash.chars().all(|c| c.is_ascii_hexdigit())).then_some(())?;
    let hash = hash.to_ascii_lowercase();
    match path.strip_prefix("a/") {
        Some(m) => {
            let (a, m) = m.split_once('/')?;
            Some(Target::Member { hash, archive: a.parse().ok()?, member: m.parse().ok()? })
        }
        None => Some(Target::File { hash, file: path.parse().ok()? }),
    }
}

/// What the open callback needs: the torrents it may stream from, their indexed
/// archives (by info hash + first volume), and a runtime.
pub struct Registry {
    pub rt: tokio::runtime::Handle,
    pub torrents: Mutex<HashMap<String, Arc<ManagedTorrent>>>,
    pub archives: Mutex<HashMap<(String, usize), Arc<archive::Indexed>>>,
}

/// librqbit's `FileStream` is not exported by name; it is used through these traits.
trait ReadSeek: tokio::io::AsyncRead + tokio::io::AsyncSeek + Unpin + Send {}
impl<T: tokio::io::AsyncRead + tokio::io::AsyncSeek + Unpin + Send> ReadSeek for T {}

/// The torrent's files as an `archive::Source`. It keeps the last file's stream
/// open, so reading a volume front to back is one stream (librqbit's read-ahead
/// keeps fetching the pieces just after the read position).
pub struct TorrentSource {
    handle: Arc<ManagedTorrent>,
    open: tokio::sync::Mutex<Option<(usize, Box<dyn ReadSeek>, u64)>>,
}

impl TorrentSource {
    pub fn new(handle: Arc<ManagedTorrent>) -> Self {
        TorrentSource { handle, open: tokio::sync::Mutex::new(None) }
    }
}

impl archive::Source for TorrentSource {
    fn read_into<'a>(&'a self, file: usize, off: u64, buf: &'a mut [u8]) -> impl std::future::Future<Output = std::io::Result<usize>> + Send + 'a {
        async move {
            let mut open = self.open.lock().await;
            if !matches!(&*open, Some((f, _, _)) if *f == file) {
                let s = self.handle.clone().stream(file).await.map_err(|e| std::io::Error::other(e.to_string()))?;
                *open = Some((file, Box::new(s), 0));
            }
            let (_, s, at) = open.as_mut().unwrap();
            if *at != off {
                s.seek(std::io::SeekFrom::Start(off)).await?;
                *at = off;
            }
            let n = s.read(buf).await?;
            *at += n as u64;
            Ok(n)
        }
    }
}

enum Reader {
    File(Box<dyn ReadSeek>),
    Member(archive::Cursor<TorrentSource>),
}

/// Per open file. Callbacks only ever take `&Cookie` (cancel arrives on another
/// thread while a read waits), so the reader itself sits behind a mutex.
struct Cookie {
    rt: tokio::runtime::Handle,
    stream: Mutex<Reader>,
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
    let Some(target) = parse(&uri) else { return ffi::ERROR_LOADING_FAILED };
    let hash = match &target {
        Target::File { hash, .. } | Target::Member { hash, .. } => hash.clone(),
    };
    let Some(handle) = reg.torrents.lock().unwrap().get(&hash).cloned() else { return ffi::ERROR_LOADING_FAILED };
    let (reader, len) = match target {
        Target::File { file, .. } => match reg.rt.block_on(handle.stream(file)) {
            Ok(s) => {
                let len = s.len();
                (Reader::File(Box::new(s)), len)
            }
            Err(_) => return ffi::ERROR_LOADING_FAILED,
        },
        Target::Member { archive, member, .. } => {
            let Some(a) = reg.archives.lock().unwrap().get(&(hash, archive)).cloned() else { return ffi::ERROR_LOADING_FAILED };
            if a.members.get(member).is_none_or(|m| m.why_not.is_some()) {
                return ffi::ERROR_LOADING_FAILED;
            }
            let c = archive::Cursor { src: TorrentSource::new(handle), archive: a, member, pos: 0 };
            let len = c.len();
            (Reader::Member(c), len)
        }
    };
    let cookie = Box::new(Cookie {
        rt: reg.rt.clone(),
        len,
        stream: Mutex::new(reader),
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
    let mut reader = c.stream.lock().unwrap();
    let r = c.rt.block_on(async {
        let read = async {
            match &mut *reader {
                Reader::File(s) => s.read(out).await,
                Reader::Member(m) => m.read(out).await,
            }
        };
        tokio::select! {
            r = read => Some(r),
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
    let mut reader = c.stream.lock().unwrap();
    match &mut *reader {
        Reader::File(s) => match c.rt.block_on(s.seek(std::io::SeekFrom::Start(offset as u64))) {
            Ok(p) => p as i64,
            Err(_) => ffi::ERROR_UNSUPPORTED,
        },
        // Positions are mapped onto the volumes at the next read.
        Reader::Member(m) if offset as u64 <= c.len => {
            m.pos = offset as u64;
            offset
        }
        Reader::Member(_) => ffi::ERROR_UNSUPPORTED,
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
        let h = "5b1e0d988fc7a0c9e99bd852071681a59974b39f";
        let u = url(h, 3);
        assert_eq!(u, "zt://5b1e0d988fc7a0c9e99bd852071681a59974b39f/3");
        assert_eq!(parse(&u), Some(Target::File { hash: h.into(), file: 3 }));
        assert_eq!(parse(&member_url(h, 12, 1)), Some(Target::Member { hash: h.into(), archive: 12, member: 1 }));
        assert_eq!(parse("zt://ABCDEF/0"), Some(Target::File { hash: "abcdef".into(), file: 0 }));
        assert_eq!(parse("zt://../etc/passwd"), None);
        assert_eq!(parse("http://abc/1"), None);
        assert_eq!(parse("zt://abc/x"), None);
        assert_eq!(parse("zt://abc/a/1"), None);
    }
}
