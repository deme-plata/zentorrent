//! RSS / Atom torrent feeds.
//!
//! A feed is a URL that returns RSS 2.0 or Atom XML whose items point at
//! a .torrent file or a magnet link (tracker "direct download" feeds,
//! distro release feeds, Torznab). Feeds are saved in the user's config
//! dir (`zentorrent/feeds.json`, mode 0600 on Unix) because private-tracker
//! feed URLs carry a passkey, which is a credential.
//!
//! Each feed can have an auto-download rule: a case-insensitive regex; any
//! NEW item whose title matches is started automatically. Items already
//! seen are remembered by link so a restart never re-downloads them.

use std::{collections::HashSet, path::PathBuf};

use quick_xml::{events::Event, Reader};
use serde::{Deserialize, Serialize};

/// One User-Agent for everything that talks to a tracker (feed, log-in,
/// .torrent download). Some trackers tie the session to the User-Agent, so
/// a cookie obtained under one name and used under another is refused.
pub const UA: &str = "Mozilla/5.0 (X11; Linux x86_64; rv:128.0) Gecko/20100101 Firefox/128.0 ZenTorrent";

/// A shared client for tracker traffic.
pub fn http() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(UA)
        // A dead connection is given up on fast; `fetch` then retries.
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(30))
        .build()
        .expect("http client")
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct Feed {
    pub name: String,
    pub url: String,
    /// Case-insensitive regex; empty = no auto-download.
    #[serde(default)]
    pub auto_regex: String,
    #[serde(default)]
    pub auto_enabled: bool,
    /// Browser login cookie (`uid=…; pass=…`), sent when downloading the
    /// .torrent files. Some trackers (TorrentBytes, most TBDev sites) put
    /// the passkey only on the feed URL and require a login for downloads.
    #[serde(default)]
    pub cookie: String,
    /// Links already present when the feed was last read. Auto-download
    /// only fires for links NOT in this set.
    #[serde(default)]
    pub seen: HashSet<String>,
    /// False until the first successful read. The first read only records
    /// what is already there, so adding a feed never downloads its backlog.
    #[serde(default)]
    pub primed: bool,
}

impl Feed {
    /// Record a fresh read of the feed and return the items that should be
    /// auto-downloaded (new since the last read AND matching the rule).
    pub fn absorb<'a>(&mut self, items: &'a [Item]) -> Vec<&'a Item> {
        let re = match rule(self) {
            Some(Ok(re)) if self.primed => Some(re),
            _ => None,
        };
        let fresh: Vec<&Item> = items
            .iter()
            .filter(|i| !self.seen.contains(&i.link))
            .filter(|i| re.as_ref().is_some_and(|re| re.is_match(&i.title)))
            .collect();
        if self.seen.len() > 5000 {
            self.seen.clear();
        }
        self.seen.extend(items.iter().map(|i| i.link.clone()));
        self.primed = true;
        fresh
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct FeedStore {
    #[serde(default = "default_interval")]
    pub refresh_minutes: u64,
    #[serde(default)]
    pub feeds: Vec<Feed>,
    /// Free OMDb API key for posters + IMDb / Rotten Tomatoes / Metascore.
    #[serde(default)]
    pub omdb_key: String,
    #[serde(default = "yes")]
    pub show_ratings: bool,
}

fn yes() -> bool {
    true
}

fn default_interval() -> u64 {
    15
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Item {
    pub title: String,
    /// Magnet link or http(s) URL of the .torrent.
    pub link: String,
    pub size: Option<u64>,
    pub date: String,
    /// Tracker category ("Movies/HD", "TV/HD", "Music"), used to skip rating lookups.
    pub category: String,
    /// Swarm numbers when the feed reports them (Torznab attrs, ezRSS/Nyaa
    /// elements, or "Seeders: 12" in a TBDev description).
    pub seeders: Option<u32>,
    pub leechers: Option<u32>,
    /// Times downloaded ("snatched", "grabs", "completed").
    pub grabs: Option<u32>,
    /// Download doesn't count against your ratio (Torznab downloadvolumefactor 0).
    pub freeleech: bool,
    pub infohash: String,
    /// Every extra category, genre and tag the feed gives ("Trance", "FLAC").
    pub tags: Vec<String>,
    /// The item's description as plain text, shortened.
    pub description: String,
}

pub fn store_path() -> Option<PathBuf> {
    dirs::config_dir().map(|d| d.join("zentorrent").join("feeds.json"))
}

impl FeedStore {
    pub fn load() -> Self {
        let mut s: FeedStore = store_path()
            .and_then(|p| std::fs::read(p).ok())
            .and_then(|b| serde_json::from_slice(&b).ok())
            .unwrap_or_default();
        if s.refresh_minutes == 0 {
            s.refresh_minutes = default_interval();
        }
        s
    }

    pub fn save(&self) -> anyhow::Result<()> {
        let path = store_path().ok_or_else(|| anyhow::anyhow!("no config dir"))?;
        std::fs::create_dir_all(path.parent().unwrap())?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_vec_pretty(self)?)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
        }
        std::fs::rename(tmp, path)?;
        Ok(())
    }
}

/// What to show instead of the URL: scheme + host only. Feed URLs from
/// private trackers carry the passkey in the query string.
pub fn display_url(url: &str) -> String {
    let rest = url.split_once("://").map(|(_, r)| r).unwrap_or(url);
    let host = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    let hidden = url.contains('?') || rest.contains('@');
    if hidden { format!("{host}  (link hidden)") } else { host.to_string() }
}

/// Split a uTorrent-style feed link `https://…/rss.php?…:COOKIE:uid=1;pass=x`
/// into the real URL and the cookie it carries (empty if none).
pub fn split_cookie(url: &str) -> (&str, &str) {
    match url.split_once(":COOKIE:") {
        Some((u, c)) => (u, c),
        None => (url, ""),
    }
}

/// The cookie to send for a feed: the one saved with it, else the one
/// embedded in its link (`uid=1;pass=x` → `uid=1; pass=x`).
pub fn feed_cookie(saved: &str, url: &str) -> String {
    if !saved.trim().is_empty() {
        return saved.to_string();
    }
    split_cookie(url).1.split(';').map(str::trim).filter(|kv| kv.contains('=')).collect::<Vec<_>>().join("; ")
}

/// Pauses before the 2nd and 3rd attempt. A home connection that drops a
/// packet (or a tracker that stalls once) must not leave the feed empty.
const RETRY: [u64; 2] = [3, 8];

pub async fn fetch(client: &reqwest::Client, url: &str) -> anyhow::Result<Vec<Item>> {
    let mut attempt = 0;
    loop {
        match fetch_once(client, url).await {
            Ok(items) => return Ok(items),
            Err(Fail::Final(e)) => return Err(e),
            Err(Fail::Transient(_)) if attempt < RETRY.len() => {
                tokio::time::sleep(std::time::Duration::from_secs(RETRY[attempt])).await;
                attempt += 1;
            }
            Err(Fail::Transient(e)) => anyhow::bail!("{e} (tried {} times)", attempt + 1),
        }
    }
}

enum Fail {
    /// Timeout, connection refused/reset, 5xx: worth another try.
    Transient(anyhow::Error),
    Final(anyhow::Error),
}

/// reqwest errors print the URL, and a feed URL carries the passkey.
fn net(e: reqwest::Error) -> Fail {
    let transient = e.is_timeout() || e.is_connect() || e.is_request() || e.is_body();
    let e = anyhow::anyhow!("{}", e.without_url());
    if transient { Fail::Transient(e) } else { Fail::Final(e) }
}

async fn fetch_once(client: &reqwest::Client, link: &str) -> Result<Vec<Item>, Fail> {
    let mut req = client.get(split_cookie(link).0);
    let cookie = feed_cookie("", link);
    if !cookie.is_empty() {
        req = req.header(reqwest::header::COOKIE, cookie);
    }
    let resp = req.send().await.map_err(net)?;
    let status = resp.status();
    let body = resp.text().await.map_err(net)?;
    if status.is_server_error() {
        return Err(Fail::Transient(anyhow::anyhow!("HTTP {status}")));
    }
    if !status.is_success() {
        return Err(Fail::Final(anyhow::anyhow!("HTTP {status}")));
    }
    parse_feed(&body).map_err(Fail::Final)
}

fn parse_feed(body: &str) -> anyhow::Result<Vec<Item>> {
    let items = parse(&body)?;
    if items.is_empty() && !body.contains("<item") && !body.contains("<entry") {
        // A login page or error page, not a feed. Do not echo the body.
        anyhow::bail!("the server did not return a feed (check the link / passkey)");
    }
    Ok(items)
}

/// Tolerant RSS 2.0 + Atom parser. Picks, per item, the first torrent-ish
/// link among: enclosure url, torznab/magnet attrs, <link>, <guid>.
pub fn parse(xml: &str) -> anyhow::Result<Vec<Item>> {
    let mut r = Reader::from_str(xml);
    // No trim_text: it would trim each fragment around `&amp;` and glue words.
    r.config_mut().trim_text(false);

    let mut items = Vec::new();
    let mut cur: Option<Cand> = None;
    let mut field: Option<String> = None;
    let mut text = String::new();

    loop {
        match r.read_event()? {
            Event::Eof => break,
            Event::Start(e) | Event::Empty(e) => {
                let name = lname(e.local_name().as_ref());
                if name == "item" || name == "entry" {
                    cur = Some(Cand::default());
                    continue;
                }
                let Some(c) = cur.as_mut() else { continue };
                let attr = |k: &str| {
                    e.attributes()
                        .flatten()
                        .find(|a| lname(a.key.local_name().as_ref()) == k)
                        .and_then(|a| a.normalized_value(quick_xml::XmlVersion::Implicit1_0).ok().map(|v| v.into_owned()))
                };
                match name.as_str() {
                    "enclosure" => {
                        if let Some(u) = attr("url") {
                            c.enclosure.get_or_insert(u);
                        }
                        if let Some(n) = attr("length").and_then(|v| v.parse().ok()) {
                            c.size.get_or_insert(n);
                        }
                    }
                    // Atom: <link href=".." rel="enclosure"/>
                    "link" if attr("href").is_some() => {
                        let href = attr("href").unwrap();
                        if attr("rel").as_deref() == Some("enclosure") || is_torrentish(&href) {
                            c.enclosure.get_or_insert(href);
                        } else {
                            c.link.get_or_insert(href);
                        }
                    }
                    // Torznab: <torznab:attr name="magneturl" value=".."/>
                    "attr" => match (attr("name").map(|n| n.to_ascii_lowercase()).as_deref(), attr("value")) {
                        (Some("magneturl"), Some(v)) => {
                            c.magnet.get_or_insert(v);
                        }
                        (Some("size"), Some(v)) => {
                            if let Ok(n) = v.parse() {
                                c.size.get_or_insert(n);
                            }
                        }
                        (Some("seeders"), Some(v)) => c.seeders = c.seeders.or(v.trim().parse().ok()),
                        (Some("leechers"), Some(v)) => c.leechers = c.leechers.or(v.trim().parse().ok()),
                        // Torznab "peers" = seeders + leechers.
                        (Some("peers"), Some(v)) => c.peers = c.peers.or(v.trim().parse().ok()),
                        (Some("grabs"), Some(v)) => c.grabs = c.grabs.or(v.trim().parse().ok()),
                        (Some("infohash"), Some(v)) => {
                            c.infohash.get_or_insert(v.to_ascii_lowercase());
                        }
                        (Some("downloadvolumefactor"), Some(v)) => {
                            if v.trim().parse::<f64>().is_ok_and(|f| f == 0.0) {
                                c.freeleech = true;
                            }
                        }
                        (Some("genre" | "tag" | "tags"), Some(v)) => c.add_tags(&v),
                        _ => {}
                    },
                    _ => {}
                }
                field = Some(name);
                text.clear();
            }
            Event::Text(t) => text.push_str(&t.decode()?),
            Event::CData(t) => text.push_str(&t.decode()?),
            Event::GeneralRef(g) => text.push_str(&entity(&g.decode()?)),
            Event::End(e) => {
                let name = lname(e.local_name().as_ref());
                if name == "item" || name == "entry" {
                    if let Some(c) = cur.take() {
                        if let Some(it) = c.finish() {
                            items.push(it);
                        }
                    }
                    continue;
                }
                if let (Some(c), Some(f)) = (cur.as_mut(), field.take()) {
                    let v = text.trim().to_string();
                    if !v.is_empty() {
                        match f.as_str() {
                            "title" => {
                                c.title.get_or_insert(v);
                            }
                            "link" => {
                                c.link.get_or_insert(v);
                            }
                            "guid" | "id" => {
                                c.guid.get_or_insert(v);
                            }
                            "magnetURI" | "magneturi" => {
                                c.magnet.get_or_insert(v);
                            }
                            "category" => {
                                if c.category.is_none() {
                                    c.category = Some(v);
                                } else {
                                    c.add_tags(&v);
                                }
                            }
                            "pubDate" | "published" | "updated" | "date" => {
                                c.date.get_or_insert(v);
                            }
                            // <size>123</size>, or Nyaa's human-readable <nyaa:size>1.2 GiB</nyaa:size>
                            "size" | "contentLength" | "contentlength" => {
                                if let Some(n) = v.parse().ok().or_else(|| parse_size(&v)) {
                                    c.size.get_or_insert(n);
                                }
                            }
                            // ezRSS <torrent:seeds>, Nyaa <nyaa:seeders>, plain <seeders>
                            "seeders" | "seeds" => c.seeders = c.seeders.or(v.parse().ok()),
                            // ezRSS <torrent:peers> counts the leechers.
                            "leechers" | "peers" => c.leechers = c.leechers.or(v.parse().ok()),
                            "downloads" | "grabs" | "snatched" | "completed" => c.grabs = c.grabs.or(v.parse().ok()),
                            "infoHash" | "infohash" => {
                                c.infohash.get_or_insert(v.to_ascii_lowercase());
                            }
                            "genre" | "tags" | "tag" | "keywords" => c.add_tags(&v),
                            "description" | "summary" | "content" => {
                                c.description.get_or_insert(v);
                            }
                            _ => {}
                        }
                    }
                }
                text.clear();
            }
            _ => {}
        }
    }
    Ok(items)
}

#[derive(Default)]
struct Cand {
    title: Option<String>,
    enclosure: Option<String>,
    magnet: Option<String>,
    link: Option<String>,
    guid: Option<String>,
    size: Option<u64>,
    date: Option<String>,
    category: Option<String>,
    seeders: Option<u32>,
    leechers: Option<u32>,
    /// Torznab "peers" (seeders + leechers); turned into leechers at the end.
    peers: Option<u32>,
    grabs: Option<u32>,
    freeleech: bool,
    infohash: Option<String>,
    tags: Vec<String>,
    description: Option<String>,
}

/// How much of a description is kept (search text, not an archive).
const DESCRIPTION_CHARS: usize = 400;

impl Cand {
    /// "Trance, Progressive" or "Trance|Uplifting" → separate tags, no duplicates.
    fn add_tags(&mut self, v: &str) {
        for t in v.split([',', '|', ';', '/']).map(str::trim).filter(|t| !t.is_empty() && t.len() <= 40) {
            if !self.tags.iter().any(|x| x.eq_ignore_ascii_case(t)) {
                self.tags.push(t.to_string());
            }
        }
    }

    fn finish(mut self) -> Option<Item> {
        let link = [&self.magnet, &self.enclosure, &self.link, &self.guid]
            .into_iter()
            .flatten()
            .find(|l| is_torrentish(l))
            .or(self.enclosure.as_ref())
            .or(self.link.as_ref())?
            .clone();
        let description = self.description.as_deref().map(plain_text).unwrap_or_default();
        // TBDev-style feeds put the numbers in the description text.
        if self.seeders.is_none() {
            self.seeders = number_after(&description, r"seed(?:er)?s?");
        }
        if self.leechers.is_none() {
            self.leechers = number_after(&description, r"leech(?:er)?s?");
        }
        if self.grabs.is_none() {
            self.grabs = number_after(&description, r"(?:snatche?d?|completed|grabs|downloaded)");
        }
        if self.size.is_none() {
            self.size = regex::Regex::new(r"(?i)\bsize\s*[:=]\s*([\d.,]+\s*[kmgt]?i?b)\b")
                .ok()
                .and_then(|re| re.captures(&description))
                .and_then(|c| parse_size(c.get(1)?.as_str()));
        }
        if self.leechers.is_none() {
            if let (Some(p), Some(s)) = (self.peers, self.seeders) {
                self.leechers = Some(p.saturating_sub(s));
            }
        }
        let title = self.title.unwrap_or_else(|| link.clone());
        // A tag that repeats the main category ("Audio" from "Audio/Lossless") adds nothing.
        if let Some(cat) = &self.category {
            self.tags.retain(|t| !t.eq_ignore_ascii_case(cat));
        }
        let lower = format!("{title} {description}").to_lowercase();
        let freeleech = self.freeleech || lower.contains("freeleech") || lower.contains("free leech");
        Some(Item {
            title,
            link,
            size: self.size,
            date: self.date.unwrap_or_default(),
            category: self.category.unwrap_or_default(),
            seeders: self.seeders,
            leechers: self.leechers,
            grabs: self.grabs,
            freeleech,
            infohash: self.infohash.unwrap_or_default(),
            tags: self.tags,
            description: description.chars().take(DESCRIPTION_CHARS).collect(),
        })
    }
}

/// HTML in a description → one line of plain text.
fn plain_text(html: &str) -> String {
    let t = regex::Regex::new(r"(?s)<(script|style)[^>]*>.*?</(script|style)>|<[^>]*>").unwrap().replace_all(html, " ");
    t.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// The number after a label: "Seeders: 12", "seeds=3", "Snatched 40 times".
fn number_after(text: &str, label: &str) -> Option<u32> {
    let re = regex::Regex::new(&format!(r"(?i)\b{label}\s*[:=]?\s*(\d{{1,9}})\b")).ok()?;
    re.captures(text)?.get(1)?.as_str().parse().ok()
}

/// "1.2 GiB", "700 MB", "4.5GB" → bytes (binary units, as trackers mean them).
pub fn parse_size(s: &str) -> Option<u64> {
    let s = s.trim().to_ascii_lowercase().replace(',', ".");
    // A bare number ("12") is bytes: no unit to split off.
    let split = s.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(s.len());
    let (num, unit) = s.split_at(split);
    let n: f64 = num.parse().ok()?;
    let mul = match unit.trim().trim_end_matches('b').trim_end_matches('i') {
        "" => 1.0,
        "k" => 1024.0,
        "m" => 1024.0 * 1024.0,
        "g" => 1024.0 * 1024.0 * 1024.0,
        "t" => 1024.0 * 1024.0 * 1024.0 * 1024.0,
        _ => return None,
    };
    Some((n * mul) as u64)
}

fn is_torrentish(s: &str) -> bool {
    let l = s.to_ascii_lowercase();
    l.starts_with("magnet:") || l.contains(".torrent") || l.contains("download")
}

fn lname(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

fn entity(name: &str) -> String {
    match name {
        "amp" => "&".into(),
        "lt" => "<".into(),
        "gt" => ">".into(),
        "quot" => "\"".into(),
        "apos" => "'".into(),
        n if n.starts_with("#x") || n.starts_with("#X") => u32::from_str_radix(&n[2..], 16)
            .ok()
            .and_then(char::from_u32)
            .map(String::from)
            .unwrap_or_default(),
        n if n.starts_with('#') => n[1..]
            .parse()
            .ok()
            .and_then(char::from_u32)
            .map(String::from)
            .unwrap_or_default(),
        n => format!("&{n};"),
    }
}

/// Clean up a pasted cookie: drop a leading "Cookie:", join lines.
pub fn clean_cookie(raw: &str) -> String {
    let s = raw.trim();
    let s = s.strip_prefix("Cookie:").or_else(|| s.strip_prefix("cookie:")).unwrap_or(s);
    s.split(['\n', '\r']).map(str::trim).filter(|l| !l.is_empty()).collect::<Vec<_>>().join("; ")
}

/// Log in to a TBDev-style tracker (TorrentBytes and most classic private
/// trackers: `POST /takelogin.php` with `username` + `password`) and return
/// the session cookie. The password is used for this one request and is
/// never stored.
pub async fn login(feed_url: &str, username: &str, password: &str) -> anyhow::Result<String> {
    let base = site_root(feed_url).ok_or_else(|| anyhow::anyhow!("feed link has no host"))?;
    let client = reqwest::Client::builder()
        .user_agent(UA)
        .redirect(reqwest::redirect::Policy::none())
        .timeout(std::time::Duration::from_secs(30))
        .build()?;
    // 1. The login page hands out the pre-login session cookie the form needs.
    let pre = client.get(format!("{base}/login.php")).send().await?;
    let mut jar = cookies_from(pre.headers());
    let _ = pre.bytes().await;
    // 2. Post the form with that cookie.
    let body = format!("username={}&password={}", urlenc(username), urlenc(password));
    let mut req = client
        .post(format!("{base}/takelogin.php"))
        .header(reqwest::header::CONTENT_TYPE, "application/x-www-form-urlencoded")
        .header(reqwest::header::REFERER, format!("{base}/login.php"))
        .body(body);
    if !jar.is_empty() {
        req = req.header(reqwest::header::COOKIE, &jar);
    }
    let resp = req.send().await?;
    let status = resp.status();
    let new = cookies_from(resp.headers());
    let text = resp.text().await.unwrap_or_default();
    jar = merge_cookies(&jar, &new);
    if new.is_empty() {
        let said = describe(text.as_bytes());
        anyhow::bail!("login refused (HTTP {status}): {said} — check username and password");
    }
    // 3. Proof: the first .torrent in the feed must now download.
    let http = http();
    if let Ok(items) = fetch(&http, feed_url).await {
        if let Some(it) = items.iter().find(|i| i.link.starts_with("http")) {
            fetch_torrent(&http, &it.link, Some(&jar))
                .await
                .map_err(|e| anyhow::anyhow!("logged in, but downloads still fail: {e:#}"))?;
        }
    }
    Ok(jar)
}

/// Later cookies replace earlier ones with the same name.
fn merge_cookies(old: &str, new: &str) -> String {
    let mut out: Vec<String> = Vec::new();
    for kv in old.split("; ").chain(new.split("; ")).filter(|k| k.contains('=')) {
        let k = kv.split('=').next().unwrap();
        out.retain(|x| x.split('=').next() != Some(k));
        out.push(kv.to_string());
    }
    out.join("; ")
}

fn site_root(url: &str) -> Option<String> {
    let (scheme, rest) = url.split_once("://")?;
    let host = rest.split(['/', '?', '#']).next()?;
    (!host.is_empty()).then(|| format!("{scheme}://{host}"))
}

fn cookies_from(h: &reqwest::header::HeaderMap) -> String {
    let mut out: Vec<String> = Vec::new();
    for v in h.get_all(reqwest::header::SET_COOKIE) {
        let Ok(v) = v.to_str() else { continue };
        let kv = v.split(';').next().unwrap_or("").trim();
        let Some((k, val)) = kv.split_once('=') else { continue };
        if val.is_empty() || val == "deleted" {
            continue;
        }
        out.retain(|x| !x.starts_with(&format!("{k}=")));
        out.push(kv.to_string());
    }
    out.join("; ")
}

fn urlenc(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

/// Download a .torrent file ourselves (User-Agent + optional cookie) and
/// check it really is one, so the error names what the site sent instead.
pub async fn fetch_torrent(
    client: &reqwest::Client,
    url: &str,
    cookie: Option<&str>,
) -> anyhow::Result<Vec<u8>> {
    let mut req = client.get(url);
    if let Some(c) = cookie.map(str::trim).filter(|c| !c.is_empty()) {
        req = req.header(reqwest::header::COOKIE, c);
    }
    let resp = req.send().await?;
    let status = resp.status();
    let body = resp.bytes().await?.to_vec();
    if !status.is_success() {
        anyhow::bail!("HTTP {status}: {}", describe(&body));
    }
    check_torrent(&body)?;
    Ok(body)
}

/// A .torrent is a bencoded dictionary: it starts with `d`.
pub fn check_torrent(body: &[u8]) -> anyhow::Result<()> {
    if body.first() == Some(&b'd') {
        return Ok(());
    }
    if body.starts_with(&[0x1f, 0x8b]) {
        anyhow::bail!("the site sent a gzip-compressed file, not a .torrent");
    }
    let said = describe(body);
    let hint = if said.to_lowercase().contains("registered") || said.to_lowercase().contains("login") {
        " — this tracker wants you logged in: RSS feeds → click the feed name → Log in"
    } else {
        ""
    };
    anyhow::bail!("not a .torrent file; the site answered: \"{said}\"{hint}")
}

/// Short, readable version of a non-torrent reply (HTML tags stripped).
fn describe(body: &[u8]) -> String {
    let text = String::from_utf8_lossy(&body[..body.len().min(4096)]);
    let text = regex::Regex::new(r"(?s)<(script|style)[^>]*>.*?</(script|style)>|<[^>]*>")
        .unwrap()
        .replace_all(&text, " ");
    let mut out: String = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if out.chars().count() > 140 {
        out = out.chars().take(140).collect::<String>() + "…";
    }
    if out.is_empty() { "(empty reply)".into() } else { out }
}

/// Compile a feed's auto-download rule. `None` = rule off or empty.
pub fn rule(feed: &Feed) -> Option<Result<regex::Regex, regex::Error>> {
    let pat = feed.auto_regex.trim();
    if !feed.auto_enabled || pat.is_empty() {
        return None;
    }
    Some(regex::RegexBuilder::new(pat).case_insensitive(true).build())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rss_enclosure_and_entities() {
        let x = r#"<?xml version="1.0"?><rss><channel><title>t</title>
          <item><title>Debian 13 &amp; friends</title>
            <link>https://example.org/details?id=1</link>
            <enclosure url="https://example.org/dl.php?id=1&amp;passkey=abc" length="123" type="application/x-bittorrent"/>
            <pubDate>Sat, 26 Sep 2026 10:00:00 +0000</pubDate></item>
          <item><title><![CDATA[Arch <2026>]]></title><link>magnet:?xt=urn:btih:abc</link></item>
        </channel></rss>"#;
        let v = parse(x).unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[0].title, "Debian 13 & friends");
        assert_eq!(v[0].link, "https://example.org/dl.php?id=1&passkey=abc");
        assert_eq!(v[0].size, Some(123));
        assert_eq!(v[0].category, "");
        assert_eq!(v[1].title, "Arch <2026>");
        assert_eq!(v[1].link, "magnet:?xt=urn:btih:abc");
    }

    #[test]
    fn direct_link_feed_without_enclosure() {
        let x = "<rss><channel><item><title>A</title><link>https://t.example/download.php/5/a.torrent?passkey=k</link></item></channel></rss>";
        let v = parse(x).unwrap();
        assert_eq!(v[0].link, "https://t.example/download.php/5/a.torrent?passkey=k");
    }

    #[test]
    fn atom_and_torznab() {
        let x = r#"<feed xmlns="http://www.w3.org/2005/Atom"><entry><title>X</title>
            <link href="https://e.org/page"/><link rel="enclosure" href="https://e.org/x.torrent"/>
            <updated>2026-09-27</updated></entry></feed>"#;
        assert_eq!(parse(x).unwrap()[0].link, "https://e.org/x.torrent");
        let t = r#"<rss xmlns:torznab="http://torznab.com/schemas/2015/feed"><channel><item><title>Y</title>
            <link>https://idx/details/1</link>
            <torznab:attr name="size" value="999"/><torznab:attr name="magneturl" value="magnet:?xt=urn:btih:ff"/>
            </item></channel></rss>"#;
        let v = parse(t).unwrap();
        assert_eq!(v[0].link, "magnet:?xt=urn:btih:ff");
        assert_eq!(v[0].size, Some(999));
    }

    #[test]
    fn non_torrent_replies_are_named() {
        assert!(check_torrent(b"d8:announce3:abce").is_ok());
        let e = check_torrent(b"This torrent is for registered users only.").unwrap_err().to_string();
        assert!(e.contains("registered users only") && e.contains("Log in"), "{e}");
        let e = check_torrent(b"<html><head><title>x</title><style>p{}</style></head><body><p>Not found</p></body></html>")
            .unwrap_err()
            .to_string();
        assert!(e.contains("x Not found") && !e.contains("<p>"), "{e}");
    }

    #[test]
    fn cookie_paste_and_login_helpers() {
        assert_eq!(clean_cookie("Cookie: uid=1; pass=ab\n"), "uid=1; pass=ab");
        assert_eq!(clean_cookie("uid=1\npass=ab"), "uid=1; pass=ab");
        assert_eq!(urlenc("a b&ø"), "a%20b%26%C3%B8");
        assert_eq!(site_root("https://www.t.net/rss.php?passkey=x").unwrap(), "https://www.t.net");
        let mut h = reqwest::header::HeaderMap::new();
        for v in ["uid=7; expires=x; path=/", "pass=abc; path=/", "hashv=deleted", "uid=8"] {
            h.append(reqwest::header::SET_COOKIE, v.parse().unwrap());
        }
        assert_eq!(cookies_from(&h), "pass=abc; uid=8");
        assert_eq!(merge_cookies("PHPSESSID=a; checksum=b", "uid=1; checksum=c"), "PHPSESSID=a; uid=1; checksum=c");
    }

    #[test]
    fn passkey_is_never_displayed() {
        let u = "https://www.tracker.example/rss.php?passkey=SECRET&username=me";
        let d = display_url(u);
        assert!(!d.contains("SECRET") && !d.contains("me&"));
        assert!(d.starts_with("www.tracker.example"));
    }

    #[test]
    fn utorrent_cookie_suffix_is_split_off_the_link() {
        let u = "https://www.t.net/rss.php?passkey=k&4&9&direct=1:COOKIE:pass=abc;uid=7";
        assert_eq!(split_cookie(u), ("https://www.t.net/rss.php?passkey=k&4&9&direct=1", "pass=abc;uid=7"));
        assert_eq!(feed_cookie("", u), "pass=abc; uid=7");
        assert_eq!(feed_cookie("uid=1; pass=z", u), "uid=1; pass=z", "a saved cookie wins");
        assert_eq!(split_cookie("https://x/rss"), ("https://x/rss", ""));
        assert_eq!(feed_cookie("", "https://x/rss"), "");
        assert_eq!(display_url(u), "www.t.net  (link hidden)");
    }

    #[tokio::test]
    async fn fetch_errors_never_print_the_passkey() {
        // Nothing listens on port 9; the connection is refused at once.
        let c = reqwest::Client::builder().connect_timeout(std::time::Duration::from_secs(2)).build().unwrap();
        let e = fetch_once(&c, "http://127.0.0.1:9/rss.php?passkey=SECRET:COOKIE:pass=SECRET2").await;
        let Err(Fail::Transient(e)) = e else { panic!("refused connection must be retried") };
        let s = format!("{e:#}");
        assert!(!s.contains("SECRET"), "{s}");
    }

    #[test]
    fn absorb_primes_then_fires_once_per_new_match() {
        let it = |t: &str, l: &str| Item { title: t.into(), link: l.into(), ..Default::default() };
        let mut f = Feed { auto_regex: "debian".into(), auto_enabled: true, ..Default::default() };
        // First read: backlog is recorded, nothing downloads.
        assert!(f.absorb(&[it("Debian 12", "a")]).is_empty());
        // New matching item fires; non-matching and already-seen do not.
        let batch = [it("Debian 12", "a"), it("Debian 13", "b"), it("Fedora", "c")];
        let got = f.absorb(&batch);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].link, "b");
        // Same read again: nothing new.
        assert!(f.absorb(&[it("Debian 13", "b")]).is_empty());
    }

    #[test]
    fn torznab_swarm_numbers_freeleech_and_genre() {
        let x = r#"<rss xmlns:torznab="http://torznab.com/schemas/2015/feed"><channel><item>
            <title>Some Artist - Live Set 2024 [FLAC]</title><link>https://idx/dl/1.torrent</link>
            <category>Audio</category><category>Audio/Lossless</category>
            <torznab:attr name="seeders" value="42"/><torznab:attr name="peers" value="50"/>
            <torznab:attr name="grabs" value="310"/><torznab:attr name="infohash" value="ABCDEF"/>
            <torznab:attr name="downloadvolumefactor" value="0"/>
            <torznab:attr name="genre" value="Trance, Progressive"/>
            </item></channel></rss>"#;
        let it = &parse(x).unwrap()[0];
        assert_eq!((it.seeders, it.leechers, it.grabs), (Some(42), Some(8), Some(310)), "leechers = peers - seeders");
        assert!(it.freeleech);
        assert_eq!(it.infohash, "abcdef");
        assert_eq!(it.category, "Audio", "first category stays the main one");
        // "Audio/Lossless" splits so each part is searchable on its own; "Audio" repeats the category.
        assert_eq!(it.tags, ["Lossless", "Trance", "Progressive"].map(String::from).to_vec());
    }

    #[test]
    fn ezrss_nyaa_and_tbdev_description() {
        let ez = r#"<rss xmlns:torrent="http://xmlns.ezrss.it/0.1/"><channel><item><title>A</title>
            <link>magnet:?xt=urn:btih:aa</link><torrent:seeds>7</torrent:seeds><torrent:peers>2</torrent:peers>
            <torrent:infoHash>AA</torrent:infoHash></item></channel></rss>"#;
        let it = &parse(ez).unwrap()[0];
        assert_eq!((it.seeders, it.leechers, it.infohash.as_str()), (Some(7), Some(2), "aa"));

        let ny = r#"<rss xmlns:nyaa="https://nyaa.si/xmlns/nyaa"><channel><item><title>B</title>
            <link>https://nyaa.example/download/1.torrent</link><nyaa:seeders>12</nyaa:seeders>
            <nyaa:leechers>3</nyaa:leechers><nyaa:downloads>900</nyaa:downloads><nyaa:size>1.5 GiB</nyaa:size>
            </item></channel></rss>"#;
        let it = &parse(ny).unwrap()[0];
        assert_eq!((it.seeders, it.leechers, it.grabs), (Some(12), Some(3), Some(900)));
        assert_eq!(it.size, Some(1_610_612_736));

        let tb = r#"<rss><channel><item><title>C</title><link>https://t.example/download.php?id=3</link>
            <description>&lt;b&gt;Category:&lt;/b&gt; Music &lt;br&gt;Size: 400 MB&lt;br&gt;Seeders: 15 Leechers: 4 Snatched: 88 times &lt;br&gt;[FREELEECH]</description>
            </item></channel></rss>"#;
        let it = &parse(tb).unwrap()[0];
        assert_eq!((it.seeders, it.leechers, it.grabs), (Some(15), Some(4), Some(88)));
        assert_eq!(it.size, Some(419_430_400), "size read from the description");
        assert!(it.freeleech);
        assert!(it.description.starts_with("Category: Music"), "{}", it.description);
        assert!(!it.description.contains('<'));
    }

    #[test]
    fn human_sizes() {
        assert_eq!(parse_size("700 MB"), Some(734_003_200));
        assert_eq!(parse_size("4.5GB"), Some(4_831_838_208));
        assert_eq!(parse_size("1,5 GiB"), Some(1_610_612_736));
        assert_eq!(parse_size("12"), Some(12));
        assert_eq!(parse_size("lots"), None);
    }

    #[test]
    fn rule_is_case_insensitive_and_off_by_default() {
        let mut f = Feed { auto_regex: "debian.*netinst".into(), ..Default::default() };
        assert!(rule(&f).is_none());
        f.auto_enabled = true;
        assert!(rule(&f).unwrap().unwrap().is_match("DEBIAN 13 NETINST"));
    }
}
