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

pub async fn fetch(client: &reqwest::Client, url: &str) -> anyhow::Result<Vec<Item>> {
    let resp = client.get(url).send().await?;
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        anyhow::bail!("HTTP {status}");
    }
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
                    "attr" => match (attr("name").as_deref(), attr("value")) {
                        (Some("magneturl"), Some(v)) => {
                            c.magnet.get_or_insert(v);
                        }
                        (Some("size"), Some(v)) => {
                            if let Ok(n) = v.parse() {
                                c.size.get_or_insert(n);
                            }
                        }
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
                            "pubDate" | "published" | "updated" | "date" => {
                                c.date.get_or_insert(v);
                            }
                            "size" | "contentLength" => {
                                if let Ok(n) = v.parse() {
                                    c.size.get_or_insert(n);
                                }
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
}

impl Cand {
    fn finish(self) -> Option<Item> {
        let link = [&self.magnet, &self.enclosure, &self.link, &self.guid]
            .into_iter()
            .flatten()
            .find(|l| is_torrentish(l))
            .or(self.enclosure.as_ref())
            .or(self.link.as_ref())?
            .clone();
        Some(Item {
            title: self.title.unwrap_or_else(|| link.clone()),
            link,
            size: self.size,
            date: self.date.unwrap_or_default(),
        })
    }
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
        " — this tracker wants you logged in: open the feed and paste your login cookie"
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
        assert!(e.contains("registered users only") && e.contains("login cookie"), "{e}");
        let e = check_torrent(b"<html><head><title>x</title><style>p{}</style></head><body><p>Not found</p></body></html>")
            .unwrap_err()
            .to_string();
        assert!(e.contains("x Not found") && !e.contains("<p>"), "{e}");
    }

    #[test]
    fn passkey_is_never_displayed() {
        let u = "https://www.tracker.example/rss.php?passkey=SECRET&username=me";
        let d = display_url(u);
        assert!(!d.contains("SECRET") && !d.contains("me&"));
        assert!(d.starts_with("www.tracker.example"));
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
    fn rule_is_case_insensitive_and_off_by_default() {
        let mut f = Feed { auto_regex: "debian.*netinst".into(), ..Default::default() };
        assert!(rule(&f).is_none());
        f.auto_enabled = true;
        assert!(rule(&f).unwrap().unwrap().is_match("DEBIAN 13 NETINST"));
    }
}
