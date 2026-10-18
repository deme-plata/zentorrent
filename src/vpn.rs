//! The VPN switch: ZenTorrent's glue around the vendored IronTunnel client (`tunnel/`).
//!
//! When the VPN is on, every byte of torrent traffic leaves through an encrypted Noise
//! tunnel to the user's relay, and nothing is allowed to go around it:
//!
//! - **peers + HTTP(S) trackers** go through librqbit's SOCKS5 option, pointed at the
//!   local IronTunnel proxy. librqbit never falls back to a direct connection when a
//!   proxy is set, so a dead relay means no traffic, not leaked traffic.
//! - **tracker websites** (feeds, .torrent downloads, log-in) use a client that goes
//!   through the same proxy, with names resolved on the relay (`socks5h`). librqbit's own
//!   tracker client is patched to `socks5h` too (vendor/README-zentorrent.md), so no
//!   tracker name is looked up locally either.
//! - **UDP cannot ride the tunnel**, so everything UDP is switched off: DHT, local peer
//!   discovery, and `udp://` trackers (librqbit would contact those directly, proxy or
//!   not). Every `udp://host:port` tracker in a magnet or .torrent is replaced by its
//!   HTTP twin `http://host:port/announce` before it is added: the big public trackers
//!   (opentrackr and others) answer both on the same port, and HTTP rides the tunnel.
//!   A magnet that listed only udp:// trackers would otherwise find no peers at all.
//! - **no listener** — incoming peers would reach the real address.
//! - **kill switch**: VPN on but the relay not answering = the engine does not start.
//! - torrents remembered from direct mode still carry their `udp://` trackers, so VPN
//!   mode resumes from its own session folder (`session-vpn`); switch the VPN off and
//!   the direct-mode torrents come back.
//!
//! Not covered (said in the UI): OMDb ratings and the update check stay direct.

use crate::tunnel::proxy::{probe, start_proxy, ProxyConfig, ProxyHandle, ProxySnapshot};
use crate::tunnel::secure::{PublicKey, StaticKeypair};
use librqbit::{ConnectionOptions, SessionOptions, SessionPersistenceConfig};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::time::Duration;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VpnSettings {
    #[serde(default)]
    pub enabled: bool,
    /// Relay address, `host:port`.
    #[serde(default)]
    pub relay: String,
    /// The relay's public key (64 hex chars), pinned.
    #[serde(default)]
    pub relay_key: String,
}

fn config_dir() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("zentorrent"))
}

impl VpnSettings {
    pub fn load() -> Self {
        config_dir()
            .and_then(|d| std::fs::read(d.join("vpn.json")).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default()
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let dir = config_dir().ok_or_else(|| anyhow::anyhow!("no config dir"))?;
        std::fs::create_dir_all(&dir)?;
        let tmp = dir.join("vpn.json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(tmp, dir.join("vpn.json"))?;
        Ok(())
    }

    /// The pinned relay key, or a sentence saying what is wrong with the settings.
    pub fn check(&self) -> Result<PublicKey, String> {
        let relay = self.relay.trim();
        let port_ok = relay.rsplit_once(':').is_some_and(|(h, p)| !h.is_empty() && p.parse::<u16>().is_ok_and(|p| p > 0));
        if !port_ok {
            return Err("relay must be host:port, e.g. vpn.example.org:1195".into());
        }
        PublicKey::from_hex(&self.relay_key).map_err(|_| "relay key must be 64 hex characters".to_string())
    }
}

/// This install's tunnel identity: created on first use, private half stored 0600 next
/// to vpn.json. Its PUBLIC key is what the relay operator adds to `authorized`.
pub fn client_key() -> anyhow::Result<StaticKeypair> {
    let dir = config_dir().ok_or_else(|| anyhow::anyhow!("no config dir"))?;
    let path = dir.join("irontunnel-client.key");
    if path.exists() {
        return Ok(StaticKeypair::load(&path)?);
    }
    std::fs::create_dir_all(&dir)?;
    let kp = StaticKeypair::generate()?;
    kp.save(&path)?;
    Ok(kp)
}

pub enum Vpn {
    Off,
    Up { proxy: ProxyHandle, relay: String, rtt: Duration },
    /// VPN wanted, tunnel not up: the engine stays off (kill switch).
    Failed(String),
}

/// How often the start-up test of the tunnel is tried before the kill switch holds.
const START_TRIES: u64 = 3;

impl Vpn {
    /// Bring the tunnel up before the engine starts. Blocks for at most the probe
    /// timeout (~10 s) when the relay does not answer.
    pub fn start(rt: &tokio::runtime::Runtime, s: &VpnSettings) -> Self {
        if !s.enabled {
            return Vpn::Off;
        }
        match client_key() {
            Ok(key) => rt.block_on(Self::start_with(s, key)),
            Err(e) => Vpn::Failed(format!("tunnel key: {e:#}")),
        }
    }

    pub async fn start_with(s: &VpnSettings, key: StaticKeypair) -> Self {
        let relay_key = match s.check() {
            Ok(k) => k,
            Err(e) => return Vpn::Failed(e),
        };
        let cfg = ProxyConfig::new(s.relay.trim(), key, relay_key);
        // One failed try is not a relay that is down: a PC waking up or joining Wi-Fi,
        // or a busy relay, drops a first connection now and then (Viktor, 2026-10-12:
        // "os error 10054" at start-up, relay fine a minute later). Fast failures are
        // tried again; a timeout has already waited long enough.
        let t0 = std::time::Instant::now();
        let mut tries = 0u64;
        let rtt = loop {
            tries += 1;
            match probe(&cfg).await {
                Ok(rtt) => break rtt,
                Err(_) if tries < START_TRIES && t0.elapsed() < Duration::from_secs(8) => {
                    tokio::time::sleep(Duration::from_secs(tries)).await
                }
                Err(e) => return Vpn::Failed(format!("relay {}: {e}", s.relay.trim())),
            }
        };
        match start_proxy(cfg).await {
            Ok(proxy) => Vpn::Up { proxy, relay: s.relay.trim().to_string(), rtt },
            Err(e) => Vpn::Failed(format!("local proxy: {e}")),
        }
    }

    pub fn is_up(&self) -> bool {
        matches!(self, Vpn::Up { .. })
    }

    pub fn failure(&self) -> Option<&str> {
        match self {
            Vpn::Failed(e) => Some(e),
            _ => None,
        }
    }

    pub fn stats(&self) -> Option<ProxySnapshot> {
        match self {
            Vpn::Up { proxy, .. } => Some(proxy.stats()),
            _ => None,
        }
    }

    /// Client for tracker websites: through the tunnel when it is up, names resolved on
    /// the relay. Same User-Agent and timeouts as `rss::http()`.
    pub fn http(&self) -> reqwest::Client {
        let Vpn::Up { proxy, .. } = self else { return crate::rss::http() };
        reqwest::Client::builder()
            .user_agent(crate::rss::UA)
            .connect_timeout(Duration::from_secs(10))
            .timeout(Duration::from_secs(30))
            .proxy(reqwest::Proxy::all(format!("socks5h://{}", proxy.local_addr())).expect("socks5h url"))
            .build()
            .expect("http client")
    }
}

/// The "Test relay" button: full handshake + encrypted ping with these settings.
pub async fn test(s: &VpnSettings) -> Result<Duration, String> {
    let relay_key = s.check()?;
    let key = client_key().map_err(|e| format!("{e:#}"))?;
    probe(&ProxyConfig::new(s.relay.trim(), key, relay_key)).await.map_err(|e| e.to_string())
}

/// Engine options for this VPN state; `None` is the kill switch.
pub fn session_options(mut o: SessionOptions, vpn: &Vpn) -> Option<SessionOptions> {
    match vpn {
        Vpn::Off => Some(o),
        Vpn::Failed(_) => None,
        Vpn::Up { proxy, .. } => {
            o.connect = Some(ConnectionOptions { proxy_url: Some(proxy.socks5_url()), ..Default::default() });
            o.dht = None;
            o.listen = None;
            o.disable_local_service_discovery = true;
            if let Some(SessionPersistenceConfig::Json { folder: Some(f) }) = &mut o.persistence {
                *f = f.with_file_name("session-vpn");
            }
            Some(o)
        }
    }
}

/// A typed link (magnet or .torrent URL) made safe for the tunnel: magnets get HTTP
/// trackers for their `udp://` ones; a .torrent URL is fetched by ZenTorrent through
/// `http` (the tunnel client) and changed the same way, instead of letting librqbit fetch it.
pub async fn tunnel_link(http: &reqwest::Client, link: &str) -> anyhow::Result<librqbit::AddTorrent<'static>> {
    let l = link.trim();
    if l.len() > 7 && l[..7].eq_ignore_ascii_case("magnet:") {
        return Ok(librqbit::AddTorrent::from_url(tunnel_magnet(l).0));
    }
    if l.starts_with("http://") || l.starts_with("https://") {
        let bytes = crate::rss::fetch_torrent(http, l, None).await?;
        return tunnel_bytes(bytes);
    }
    Ok(librqbit::AddTorrent::from_url(l.to_string()))
}

/// librqbit's "no known way to resolve peers", said plainly: behind the VPN it means the link
/// has no tracker that can go through the tunnel.
pub fn explain(e: String, tunnelled: bool) -> String {
    if tunnelled && e.contains("no known way to resolve peers") {
        format!(
            "{e} — with the VPN on, DHT and udp:// trackers are off (UDP can't go through the tunnel),              and this link has no tracker that works over HTTP. Turn the VPN off for it, or use its .torrent file"
        )
    } else {
        e
    }
}

/// What to call a magnet link in messages: its name (`dn=`), else the link.
pub fn link_label(link: &str) -> String {
    link.split_once('?')
        .and_then(|(_, q)| q.split('&').find_map(|p| p.strip_prefix("dn=")))
        .map(|n| pct_decode(&n.replace('+', " ")))
        .filter(|n| !n.trim().is_empty())
        .unwrap_or_else(|| link.to_string())
}

/// .torrent bytes made safe for the tunnel.
pub fn tunnel_bytes(bytes: Vec<u8>) -> anyhow::Result<librqbit::AddTorrent<'static>> {
    Ok(librqbit::AddTorrent::from_bytes(tunnel_torrent(&bytes)?.0))
}

fn is_udp(url: &[u8]) -> bool {
    let l = url.to_ascii_lowercase();
    l.starts_with(b"udp:") || l.starts_with(b"udp%3a")
}

/// The HTTP twin of a `udp://host:port[/…]` tracker: `http://host:port/announce`. Many
/// public trackers answer both on the same port (opentrackr, tracker.dler.org,
/// internetwarriors: checked 2026-10-18); one that speaks UDP only just never answers.
fn http_twin(udp: &str) -> Option<String> {
    let rest = udp.get(..6).filter(|s| s.eq_ignore_ascii_case("udp://")).map(|_| &udp[6..])?;
    let (host, port) = rest.split(['/', '?', '#']).next()?.rsplit_once(':')?;
    let host_ok = !host.is_empty() && host.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '-');
    (host_ok && port.parse::<u16>().is_ok_and(|p| p > 0)).then(|| format!("http://{}:{port}/announce", host.to_ascii_lowercase()))
}

/// `%3A` → `:` (magnet values are percent-encoded, or not).
fn pct_decode(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        let hex = |c: u8| (c as char).to_digit(16);
        match (b[i], b.get(i + 1).and_then(|&c| hex(c)), b.get(i + 2).and_then(|&c| hex(c))) {
            (b'%', Some(h), Some(l)) => {
                out.push((h * 16 + l) as u8);
                i += 3;
            }
            (c, _, _) => {
                out.push(c);
                i += 1;
            }
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn is_tracker_key(k: &str) -> bool {
    k.eq_ignore_ascii_case("tr") || k.to_ascii_lowercase().starts_with("tr.")
}

/// A magnet link for the tunnel: each `tr=udp://…` becomes its HTTP twin (once; not when the
/// link already has it). Returns the link and how many udp:// trackers it had.
pub fn tunnel_magnet(link: &str) -> (String, usize) {
    let Some((head, query)) = link.split_once('?') else { return (link.to_string(), 0) };
    let mut seen: std::collections::HashSet<String> = query
        .split('&')
        .filter_map(|p| p.split_once('='))
        .filter(|(k, v)| is_tracker_key(k) && !is_udp(v.as_bytes()))
        .map(|(_, v)| pct_decode(v).to_ascii_lowercase())
        .collect();
    let mut udp = 0;
    let mut out: Vec<String> = Vec::new();
    for p in query.split('&') {
        let (k, v) = p.split_once('=').unwrap_or((p, ""));
        if is_tracker_key(k) && is_udp(v.as_bytes()) {
            udp += 1;
            if let Some(t) = http_twin(&pct_decode(v)).filter(|t| seen.insert(t.clone())) {
                out.push(format!("tr={}", t.replace(':', "%3A").replace('/', "%2F")));
            }
        } else {
            out.push(p.to_string());
        }
    }
    (format!("{head}?{}", out.join("&")), udp)
}

const MAX_DEPTH: usize = 64;

/// End of the bencoded value starting at `i`.
fn skip(b: &[u8], i: usize, depth: usize) -> anyhow::Result<usize> {
    anyhow::ensure!(depth <= MAX_DEPTH, "torrent nested too deeply");
    match b.get(i) {
        Some(b'i') => Ok(i + 1 + b[i + 1..].iter().position(|&c| c == b'e').ok_or_else(|| anyhow::anyhow!("bad integer"))? + 1),
        Some(b'l') | Some(b'd') => {
            let mut j = i + 1;
            while b.get(j) != Some(&b'e') {
                anyhow::ensure!(j < b.len(), "unterminated list");
                j = skip(b, j, depth + 1)?;
            }
            Ok(j + 1)
        }
        Some(c) if c.is_ascii_digit() => Ok(string_at(b, i)?.1),
        _ => anyhow::bail!("not a bencoded torrent"),
    }
}

/// The byte string at `i` and the index just past it.
fn string_at(b: &[u8], i: usize) -> anyhow::Result<(&[u8], usize)> {
    let colon = i + b[i..].iter().position(|&c| c == b':').ok_or_else(|| anyhow::anyhow!("bad string"))?;
    let len: usize = std::str::from_utf8(&b[i..colon])?.parse()?;
    let end = colon + 1 + len;
    anyhow::ensure!(end <= b.len(), "string runs past the end");
    Ok((&b[colon + 1..end], end))
}

fn put_string(out: &mut Vec<u8>, s: &[u8]) {
    out.extend_from_slice(format!("{}:", s.len()).as_bytes());
    out.extend_from_slice(s);
}

/// A .torrent for the tunnel: each `udp://` tracker in `announce` and `announce-list` becomes
/// its HTTP twin (dropped when it has none). Every other key — above all `info`, whose bytes
/// ARE the info-hash — is copied byte for byte. Returns the rewritten torrent and how many
/// udp:// trackers it had.
pub fn tunnel_torrent(b: &[u8]) -> anyhow::Result<(Vec<u8>, usize)> {
    anyhow::ensure!(b.first() == Some(&b'd'), "not a .torrent file");
    let mut out = Vec::with_capacity(b.len());
    out.push(b'd');
    let mut removed = 0;
    let mut i = 1;
    while b.get(i) != Some(&b'e') {
        anyhow::ensure!(i < b.len(), "unterminated torrent");
        let (key, kend) = string_at(b, i)?;
        let vend = skip(b, kend, 1)?;
        match key {
            b"announce" if b.get(kend).is_some_and(u8::is_ascii_digit) && is_udp(string_at(b, kend)?.0) => {
                removed += 1;
                if let Some(t) = http_twin(&String::from_utf8_lossy(string_at(b, kend)?.0)) {
                    put_string(&mut out, key);
                    put_string(&mut out, t.as_bytes());
                }
            }
            b"announce-list" if b.get(kend) == Some(&b'l') => {
                let mut tiers = Vec::new();
                let mut j = kend + 1;
                while b.get(j) != Some(&b'e') {
                    let tend = skip(b, j, 2)?;
                    if b[j] == b'l' {
                        let mut urls: Vec<Vec<u8>> = Vec::new();
                        let mut k = j + 1;
                        while b.get(k) != Some(&b'e') {
                            let uend = skip(b, k, 3)?;
                            match b[k].is_ascii_digit().then(|| string_at(b, k)).transpose()? {
                                Some((u, _)) if is_udp(u) => {
                                    removed += 1;
                                    if let Some(t) = http_twin(&String::from_utf8_lossy(u)) {
                                        let mut s = Vec::new();
                                        put_string(&mut s, t.as_bytes());
                                        urls.push(s);
                                    }
                                }
                                _ => urls.push(b[k..uend].to_vec()),
                            }
                            k = uend;
                        }
                        if !urls.is_empty() {
                            let mut t = vec![b'l'];
                            urls.iter().for_each(|u| t.extend_from_slice(u));
                            t.push(b'e');
                            tiers.push(t);
                        }
                    }
                    j = tend;
                }
                if !tiers.is_empty() {
                    put_string(&mut out, key);
                    out.push(b'l');
                    tiers.iter().for_each(|t| out.extend_from_slice(t));
                    out.push(b'e');
                }
            }
            _ => out.extend_from_slice(&b[i..vend]),
        }
        i = vend;
    }
    out.extend_from_slice(&b[i..]);
    Ok((out, removed))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn torrent(announce: &str, tiers: &[&[&str]]) -> Vec<u8> {
        let mut b = b"d".to_vec();
        put_string(&mut b, b"announce");
        put_string(&mut b, announce.as_bytes());
        put_string(&mut b, b"announce-list");
        b.push(b'l');
        for t in tiers {
            b.push(b'l');
            t.iter().for_each(|u| put_string(&mut b, u.as_bytes()));
            b.push(b'e');
        }
        b.push(b'e');
        put_string(&mut b, b"info");
        b.extend_from_slice(b"d6:lengthi1048576e4:name8:test.iso12:piece lengthi262144e6:pieces20:AAAAAAAAAAAAAAAAAAAAe");
        b.push(b'e');
        b
    }

    fn info(b: &[u8]) -> &[u8] {
        let at = b.windows(6).position(|w| w == b"4:info").unwrap() + 6;
        &b[at..skip(b, at, 1).unwrap()]
    }

    #[test]
    fn udp_trackers_become_http_and_info_is_untouched() {
        let t = torrent(
            "udp://tracker.opentrackr.org:1337/announce",
            &[&["udp://open.demonii.com:1337"], &["http://bttracker.debian.org:6969/announce", "UDP://x:1", "udp://[::1]:80"]],
        );
        let (out, udp) = tunnel_torrent(&t).unwrap();
        assert_eq!(udp, 4);
        assert_eq!(info(&out), info(&t), "info dict bytes (the info-hash) must not change");
        let s = String::from_utf8_lossy(&out);
        assert!(!s.to_ascii_lowercase().contains("udp://"), "{s}");
        let l = |u: &str| format!("{}:{u}", u.len());
        assert!(s.contains(&format!("8:announce{}", l("http://tracker.opentrackr.org:1337/announce"))), "{s}");
        let tiers = format!(
            "13:announce-listll{}el{}{}ee",
            l("http://open.demonii.com:1337/announce"),
            l("http://bttracker.debian.org:6969/announce"),
            l("http://x:1/announce")
        );
        assert!(s.contains(&tiers), "{s}");
    }

    #[test]
    fn http_only_torrent_is_byte_identical() {
        let t = torrent("http://bttracker.debian.org:6969/announce", &[&["https://t.example/announce"]]);
        assert_eq!(tunnel_torrent(&t).unwrap(), (t.clone(), 0));
    }

    #[test]
    fn junk_and_hostile_input_is_refused_not_panicking() {
        assert!(tunnel_torrent(b"<html>registered users only</html>").is_err());
        assert!(tunnel_torrent(b"d8:announce999:short").is_err());
        let deep = [vec![b'd', b'1', b':', b'x'], vec![b'l'; 10_000]].concat();
        assert!(tunnel_torrent(&deep).is_err());
    }

    #[test]
    fn magnet_udp_trackers_become_their_http_twins() {
        let m = "magnet:?xt=urn:btih:abc&dn=debian&tr=udp%3A%2F%2Fopen.demonii.com%3A1337&tr=http%3A%2F%2Fbttracker.debian.org%3A6969%2Fannounce&tr.1=udp://x:1&tr=udp://bttracker.debian.org:6969";
        let (out, udp) = tunnel_magnet(m);
        assert_eq!(udp, 3);
        assert_eq!(
            out,
            "magnet:?xt=urn:btih:abc&dn=debian&tr=http%3A%2F%2Fopen.demonii.com%3A1337%2Fannounce\
             &tr=http%3A%2F%2Fbttracker.debian.org%3A6969%2Fannounce&tr=http%3A%2F%2Fx%3A1%2Fannounce",
            "in place, and the debian twin not twice"
        );
    }

    #[test]
    fn a_udp_only_magnet_still_has_trackers_behind_the_vpn() {
        // Viktor, 2026-10-18: "no known way to resolve peers (no DHT, no trackers, no initial_peers)".
        let m = "magnet:?xt=urn:btih:D098998D4519F241B6BB8478E8D4F87E4062A8A3&dn=VA%20-%20100%20Club%20Hits\
                 &tr=udp%3A%2F%2Ftracker.opentrackr.org%3A1337&tr=udp%3A%2F%2Fopen.stealth.si%3A80%2Fannounce\
                 &tr=udp%3A%2F%2Ftracker.internetwarriors.net%3A1337";
        let (out, udp) = tunnel_magnet(m);
        assert_eq!(udp, 3);
        assert!(!out.to_ascii_lowercase().contains("udp"), "{out}");
        assert!(out.contains("&tr=http%3A%2F%2Ftracker.opentrackr.org%3A1337%2Fannounce"), "{out}");
        assert!(out.starts_with("magnet:?xt=urn:btih:D098998D4519F241B6BB8478E8D4F87E4062A8A3&dn=VA%20-%20100%20Club%20Hits"));
        assert_eq!(link_label(m), "VA - 100 Club Hits");
        assert_eq!(link_label("magnet:?xt=urn:btih:ab"), "magnet:?xt=urn:btih:ab");
        let e = explain("x: no known way to resolve peers (no DHT, no trackers, no initial_peers)".into(), true);
        assert!(e.contains("Turn the VPN off for it"), "{e}");
        assert_eq!(explain("x: other".into(), true), "x: other");
        assert_eq!(http_twin("udp://[::1]:80"), None);
        assert_eq!(http_twin("udp://host:0"), None);
        assert_eq!(http_twin("UDP://Tracker.Example:6969/announce?x=1").as_deref(), Some("http://tracker.example:6969/announce"));
    }

    #[test]
    fn settings_check() {
        let k = StaticKeypair::generate().unwrap();
        let mut s = VpnSettings { enabled: true, relay: "vpn.example.org:1195".into(), relay_key: k.public().to_hex() };
        assert_eq!(s.check().unwrap(), *k.public());
        s.relay = "vpn.example.org".into();
        assert!(s.check().is_err());
        s.relay = "vpn.example.org:1195".into();
        s.relay_key = "beef".into();
        assert!(s.check().is_err());
    }

    #[tokio::test]
    async fn kill_switch_and_leak_free_engine_options() {
        let base = SessionOptions {
            persistence: Some(SessionPersistenceConfig::Json { folder: Some(PathBuf::from("/data/zentorrent/session")) }),
            ..Default::default()
        };
        assert!(session_options(SessionOptions::default(), &Vpn::Failed("down".into())).is_none());
        let off = session_options(SessionOptions::default(), &Vpn::Off).unwrap();
        assert!(off.dht.is_some() && off.connect.is_none(), "direct mode unchanged");

        // A real tunnel up against an in-process relay.
        use crate::tunnel::relay::{start_relay, RelayConfig};
        let (relay_key, client) = (StaticKeypair::generate().unwrap(), StaticKeypair::generate().unwrap());
        let s = VpnSettings { enabled: true, relay: String::new(), relay_key: relay_key.public().to_hex() };
        let relay = start_relay(RelayConfig::new(
            "127.0.0.1:0".parse().unwrap(),
            relay_key,
            [*client.public()].into_iter().collect(),
        ))
        .await
        .unwrap();
        let s = VpnSettings { relay: relay.local_addr.to_string(), ..s };
        let vpn = Vpn::start_with(&s, client).await;
        let Vpn::Up { proxy, .. } = &vpn else { panic!("tunnel should be up") };
        let o = session_options(base, &vpn).unwrap();
        assert_eq!(o.connect.unwrap().proxy_url.unwrap(), proxy.socks5_url());
        assert!(o.dht.is_none() && o.listen.is_none() && o.disable_local_service_discovery);
        match o.persistence {
            Some(SessionPersistenceConfig::Json { folder: Some(f) }) => assert_eq!(f, PathBuf::from("/data/zentorrent/session-vpn")),
            _ => panic!("persistence lost"),
        }

        // Wrong relay key → Failed, i.e. the kill switch.
        let other = StaticKeypair::generate().unwrap();
        let bad = VpnSettings { relay_key: other.public().to_hex(), ..s };
        assert!(matches!(Vpn::start_with(&bad, StaticKeypair::generate().unwrap()).await, Vpn::Failed(_)));
    }
}
