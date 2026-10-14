//! One ZenTorrent per user. Double-clicking a .torrent file or a magnet link while
//! ZenTorrent runs (often only in the tray) hands it to the running one, which shows
//! its window: a second engine would fight the first over the same downloads. A plain
//! second start (Start menu, Windows start-up) just brings the running one forward.
//!
//! The running one listens on 127.0.0.1, on a port fixed per data folder, and only
//! hears a request that carries the secret in `instance.key` (in the data folder,
//! 0600 on Unix), so other programs cannot slip torrents in.

use std::io::{BufRead, BufReader, Write};
use std::net::{Ipv4Addr, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::time::Duration;

/// How this start went.
pub enum Start {
    /// No ZenTorrent was running: this is it now (listen on this).
    Primary(TcpListener),
    /// The running ZenTorrent took over: this process can exit.
    HandedOff,
    /// The port is taken by something else: run without the hand-over.
    Alone,
}

/// What a second start asks the running one to do.
#[derive(Debug, Default, PartialEq)]
pub struct Request {
    /// .torrent files (absolute paths) and magnet links.
    pub open: Vec<String>,
    /// Bring the window forward.
    pub show: bool,
}

/// FNV-1a: the port must be the same for every ZenTorrent version (std's hasher is not).
fn fnv(bytes: &[u8]) -> u64 {
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |h, &b| (h ^ b as u64).wrapping_mul(0x100_0000_01b3))
}

fn addr() -> SocketAddr {
    let dir = crate::seed::data_dir();
    let port = 40_000 + (fnv(dir.to_string_lossy().as_bytes()) % 20_000) as u16;
    SocketAddr::from((Ipv4Addr::LOCALHOST, port))
}

fn key_path() -> PathBuf {
    crate::seed::data_dir().join("instance.key")
}

/// The shared secret, made on first use.
fn key() -> String {
    if let Ok(k) = std::fs::read_to_string(key_path()) {
        if k.trim().len() == 32 {
            return k.trim().to_string();
        }
    }
    use std::hash::BuildHasher;
    let r = || std::collections::hash_map::RandomState::new().hash_one(std::time::SystemTime::now());
    let k = format!("{:016x}{:016x}", r(), r());
    let _ = std::fs::create_dir_all(crate::seed::data_dir());
    let _ = std::fs::write(key_path(), &k);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(key_path(), std::fs::Permissions::from_mode(0o600));
    }
    k
}

/// The .torrent files and magnet links on the command line (as Windows passes a
/// double-clicked file), files made absolute for the running ZenTorrent.
pub fn to_open(args: &[String]) -> Vec<String> {
    args.iter()
        .skip(1)
        .filter_map(|a| {
            if a.starts_with("magnet:") {
                Some(a.clone())
            } else if a.to_ascii_lowercase().ends_with(".torrent") {
                std::path::absolute(a).ok().map(|p| p.to_string_lossy().into_owned())
            } else {
                None
            }
        })
        .collect()
}

/// At start: become the running ZenTorrent, or hand `open` to the one running.
pub fn start(open: &[String], show: bool) -> Start {
    let addr = addr();
    if let Ok(l) = TcpListener::bind(addr) {
        key();
        return Start::Primary(l);
    }
    // The port is taken: the running ZenTorrent (perhaps still starting), else something else.
    let req = Request { open: open.to_vec(), show };
    for _ in 0..10 {
        if send(addr, &req).is_ok() {
            return Start::HandedOff;
        }
        std::thread::sleep(Duration::from_millis(500));
    }
    Start::Alone
}

fn encode(key: &str, req: &Request) -> String {
    let mut msg = format!("{key}\n");
    for o in &req.open {
        msg += &format!("open\t{o}\n");
    }
    if req.show {
        msg += "show\n";
    }
    msg + ".\n"
}

/// The request in a message, if its first line is the key.
fn decode(key: &str, r: impl BufRead) -> Option<Request> {
    let mut lines = r.lines();
    if lines.next()?.ok()?.trim() != key {
        return None;
    }
    let mut req = Request::default();
    for line in lines {
        let line = line.ok()?;
        if line == "." {
            return Some(req);
        }
        if let Some(o) = line.strip_prefix("open\t") {
            req.open.push(o.to_string());
        } else if line == "show" {
            req.show = true;
        }
    }
    None
}

fn send(addr: SocketAddr, req: &Request) -> std::io::Result<()> {
    let mut s = TcpStream::connect_timeout(&addr, Duration::from_secs(2))?;
    s.set_read_timeout(Some(Duration::from_secs(3)))?;
    s.write_all(encode(&key(), req).as_bytes())?;
    let mut ack = String::new();
    BufReader::new(&s).read_line(&mut ack)?;
    if ack.trim() == "ok" {
        Ok(())
    } else {
        Err(std::io::Error::other("not a ZenTorrent"))
    }
}

/// The running ZenTorrent: hears second starts on its own thread.
pub fn listen(l: TcpListener, on: impl Fn(Request) + Send + 'static) {
    let key = key();
    std::thread::spawn(move || {
        for conn in l.incoming() {
            let Ok(s) = conn else { continue };
            let _ = s.set_read_timeout(Some(Duration::from_secs(2)));
            if let Some(req) = decode(&key, BufReader::new(&s)) {
                let _ = (&s).write_all(b"ok\n");
                on(req);
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_request_needs_the_key() {
        let req = Request { open: vec!["C:\\Users\\x\\Downloads\\a.torrent".into(), "magnet:?xt=urn:btih:ab&dn=x".into()], show: true };
        let msg = encode("k1", &req);
        assert_eq!(decode("k1", msg.as_bytes()), Some(req));
        assert_eq!(decode("other", msg.as_bytes()), None, "wrong key");
        assert_eq!(decode("k1", "k1\nopen\tx\n".as_bytes()), None, "cut off before the end mark");
    }

    #[test]
    fn what_to_open_from_the_command_line() {
        let args: Vec<String> = ["zentorrent.exe", "--autostart", "magnet:?xt=urn:btih:ab", "a.TORRENT", "notes.txt"].map(String::from).into();
        let got = to_open(&args);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0], "magnet:?xt=urn:btih:ab");
        assert!(std::path::Path::new(&got[1]).is_absolute() && got[1].ends_with("a.TORRENT"));
    }

    #[test]
    fn the_port_is_the_same_for_every_version() {
        assert_eq!(fnv(b""), 0xcbf2_9ce4_8422_2325);
        assert_eq!(fnv(b"a"), 0xaf63_dc4c_8601_ec8c);
    }
}
