//! Linux images to get from the 🐧 Linux virtualize panel.
//!
//! No link is written down here: each one is read from the distro's own download page
//! when the user clicks, newest version first. The old catalog (removed in 0.6.0) pinned
//! file names, and a point release breaks a pinned name. Torrents go to the Downloads list
//! like any other; an ISO without a torrent (Omarchy) comes over HTTPS and is kept only
//! when it matches the SHA-256 its makers publish beside it.

use std::cmp::Ordering;

pub enum Get {
    /// A .torrent link (or magnet) for the Downloads list.
    Torrent,
    /// An ISO over HTTPS, checked against `<url><sha256>` (a "hash  name" file).
    Iso { sha256: &'static str },
}

pub struct Distro {
    pub id: &'static str,
    pub name: &'static str,
    pub blurb: &'static str,
    /// About how big the download is, GB (shown before the click; the real size comes later).
    pub size_gb: f64,
    /// The page to start reading at.
    pub start: &'static str,
    /// Patterns to follow: every step but the last finds pages to read next, the last one
    /// finds the download itself (the newest version wins).
    pub steps: &'static [&'static str],
    pub get: Get,
}

pub const DISTROS: &[Distro] = &[
    Distro {
        id: "omarchy",
        name: "Omarchy",
        blurb: "DHH's Arch + Hyprland tiling desktop, keyboard first — backed by Coinbase's Brian Armstrong, who built a Coinbase plugin for it",
        size_gb: 6.4,
        start: "https://omarchy.org",
        steps: &[r"https://iso\.omarchy\.org/omarchy-[0-9.]+\.iso"],
        get: Get::Iso { sha256: ".sha256" },
    },
    Distro {
        id: "arch",
        name: "Arch Linux",
        blurb: "Rolling release, built your way (what Omarchy stands on)",
        size_gb: 1.6,
        start: "https://archlinux.org/download/",
        steps: &[r"/releng/releases/[0-9]{4}\.[0-9]{2}\.[0-9]{2}/torrent/"],
        get: Get::Torrent,
    },
    Distro {
        id: "kali",
        name: "Kali Linux",
        blurb: "Security and pen-testing toolkit, runs live",
        size_gb: 5.9,
        start: "https://cdimage.kali.org/current/",
        steps: &[r"kali-linux-[0-9.]+-live-amd64\.iso\.torrent"],
        get: Get::Torrent,
    },
    Distro {
        id: "fedora",
        name: "Fedora Workstation",
        blurb: "The newest GNOME, upstream first",
        size_gb: 2.9,
        start: "https://torrent.fedoraproject.org/torrents/",
        steps: &[r"Fedora-Workstation-Live-x86_64-[0-9]+\.torrent"],
        get: Get::Torrent,
    },
    Distro {
        id: "ubuntu",
        name: "Ubuntu 26.04 LTS",
        blurb: "The popular one, supported until 2031",
        size_gb: 6.5,
        start: "https://releases.ubuntu.com/26.04/",
        steps: &[r"ubuntu-26\.04(\.[0-9]+)?-desktop-amd64\.iso\.torrent"],
        get: Get::Torrent,
    },
    Distro {
        id: "mint",
        name: "Linux Mint Cinnamon",
        blurb: "Friendly and familiar if you come from Windows",
        size_gb: 3.1,
        start: "https://linuxmint.com/download.php",
        steps: &[r"edition\.php\?id=[0-9]+", r"https://www\.linuxmint\.com/torrents/linuxmint-[0-9.]+-cinnamon-64bit\.iso\.torrent"],
        get: Get::Torrent,
    },
    Distro {
        id: "debian",
        name: "Debian",
        blurb: "The rock everything else is built on — small network installer",
        size_gb: 0.8,
        start: "https://cdimage.debian.org/debian-cd/current/amd64/bt-cd/",
        steps: &[r"debian-[0-9.]+-amd64-netinst\.iso\.torrent"],
        get: Get::Torrent,
    },
];

/// "26.04.1" after "26.04", "44" after "43", "4.0.10" after "4.0.9": numbers compared as numbers.
pub fn natural(a: &str, b: &str) -> Ordering {
    let parts = |s: &str| {
        let mut v: Vec<Result<u64, char>> = Vec::new();
        let mut n: Option<u64> = None;
        for c in s.chars() {
            match c.to_digit(10) {
                Some(d) => n = Some(n.unwrap_or(0).saturating_mul(10).saturating_add(d as u64)),
                None => {
                    v.extend(n.take().map(Ok));
                    v.push(Err(c));
                }
            }
        }
        v.extend(n.map(Ok));
        v
    };
    let (pa, pb) = (parts(a), parts(b));
    for (x, y) in pa.iter().zip(&pb) {
        let o = match (x, y) {
            (Ok(x), Ok(y)) => x.cmp(y),
            (Err(x), Err(y)) => x.cmp(y),
            // A number after the same text ranks above a letter: "26.04.1-" over "26.04-".
            (Ok(_), Err(_)) => Ordering::Greater,
            (Err(_), Ok(_)) => Ordering::Less,
        };
        if o != Ordering::Equal {
            return o;
        }
    }
    pa.len().cmp(&pb.len())
}

/// The matches of `pattern` in `page`, made absolute against `base`.
pub fn links(base: &url::Url, page: &str, pattern: &str) -> Vec<String> {
    let Ok(re) = regex::Regex::new(pattern) else { return Vec::new() };
    let mut out: Vec<String> = re.find_iter(page).filter_map(|m| base.join(m.as_str()).ok()).map(|u| u.to_string()).collect();
    out.sort();
    out.dedup();
    out
}

/// The newest of the found links (by file name, numbers as numbers).
pub fn newest(found: &[String]) -> Option<String> {
    let name = |u: &str| u.trim_end_matches('/').rsplit('/').next().unwrap_or(u).to_string();
    let key = |u: &String| if u.contains("/releng/releases/") { u.clone() } else { name(u) };
    found.iter().max_by(|a, b| natural(&key(a), &key(b))).cloned()
}

/// Read the pages and return the download link.
pub async fn resolve(http: &reqwest::Client, d: &Distro) -> Result<String, String> {
    let mut pages = vec![d.start.to_string()];
    for (i, step) in d.steps.iter().enumerate() {
        let mut found = Vec::new();
        for page in pages.iter().take(8) {
            let base = url::Url::parse(page).map_err(|e| e.to_string())?;
            let r = http.get(page).header("User-Agent", crate::rss::UA).send().await.map_err(|e| format!("{}: {e}", base.host_str().unwrap_or("")))?;
            if !r.status().is_success() {
                continue;
            }
            let text = r.text().await.map_err(|e| e.to_string())?;
            found.extend(links(&base, &text, step));
        }
        if i + 1 == d.steps.len() {
            return newest(&found).ok_or_else(|| format!("no download link on {} (the page changed?)", d.start));
        }
        if found.is_empty() {
            return Err(format!("{} has no link to follow (the page changed?)", d.start));
        }
        pages = found;
    }
    Err("nothing to read".into())
}

/// `zentorrent --distros`: resolve every distro the way the Get button does and check
/// what the link gives (a torrent must start like one; an ISO must have its SHA-256).
pub async fn check(http: &reqwest::Client) -> bool {
    let mut all = true;
    for d in DISTROS {
        let line = match resolve(http, d).await {
            Err(e) => Err(e),
            Ok(url) => {
                let probe = match d.get {
                    Get::Torrent => match http.get(&url).header("User-Agent", crate::rss::UA).send().await {
                        Ok(r) => match r.bytes().await {
                            Ok(b) if b.first() == Some(&b'd') => Ok(format!("torrent, {} KB", b.len() / 1024)),
                            Ok(b) => Err(format!("not a torrent ({} bytes)", b.len())),
                            Err(e) => Err(e.to_string()),
                        },
                        Err(e) => Err(e.to_string()),
                    },
                    Get::Iso { sha256 } => match http.get(format!("{url}{sha256}")).send().await {
                        Ok(r) => match r.text().await.ok().as_deref().and_then(parse_sha256) {
                            Some(h) => Ok(format!("ISO, SHA-256 {}…", &h[..12])),
                            None => Err("no SHA-256 beside it".into()),
                        },
                        Err(e) => Err(e.to_string()),
                    },
                };
                probe.map(|p| format!("{url}  ({p})"))
            }
        };
        match line {
            Ok(l) => println!("✓ {:<20} {l}", d.name),
            Err(e) => {
                all = false;
                println!("✗ {:<20} {e}", d.name);
            }
        }
    }
    all
}

/// The SHA-256 a "hash  name" checksum file gives.
pub fn parse_sha256(text: &str) -> Option<String> {
    let h = text.split_whitespace().next()?.to_ascii_lowercase();
    (h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit())).then_some(h)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn newest_version_wins() {
        let l = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            newest(&l(&["https://r/ubuntu-26.04-desktop-amd64.iso.torrent", "https://r/ubuntu-26.04.1-desktop-amd64.iso.torrent"])).unwrap(),
            "https://r/ubuntu-26.04.1-desktop-amd64.iso.torrent"
        );
        assert_eq!(newest(&l(&["https://t/Fedora-Workstation-Live-x86_64-9.torrent", "https://t/Fedora-Workstation-Live-x86_64-44.torrent"])).unwrap(), "https://t/Fedora-Workstation-Live-x86_64-44.torrent");
        assert_eq!(newest(&l(&["https://i/omarchy-4.0.9.iso", "https://i/omarchy-4.0.10.iso"])).unwrap(), "https://i/omarchy-4.0.10.iso");
        assert_eq!(
            newest(&l(&["https://archlinux.org/releng/releases/2026.09.01/torrent/", "https://archlinux.org/releng/releases/2026.10.01/torrent/"])).unwrap(),
            "https://archlinux.org/releng/releases/2026.10.01/torrent/"
        );
        assert_eq!(newest(&[]), None);
    }

    #[test]
    fn links_are_found_and_made_whole() {
        let base = url::Url::parse("https://torrent.fedoraproject.org/torrents/").unwrap();
        let page = r#"<a href="Fedora-Workstation-Live-x86_64-44.torrent">x</a> <a href="Fedora-Workstation-Live-x86_64-45_Beta.torrent">b</a>
                      <a href="Fedora-Workstation-Live-x86_64-44.torrent">again</a>"#;
        let f = links(&base, page, DISTROS.iter().find(|d| d.id == "fedora").unwrap().steps[0]);
        assert_eq!(f, vec!["https://torrent.fedoraproject.org/torrents/Fedora-Workstation-Live-x86_64-44.torrent"], "beta left out, once");
        let arch = url::Url::parse("https://archlinux.org/download/").unwrap();
        assert_eq!(links(&arch, r#"href="/releng/releases/2026.10.01/torrent/""#, DISTROS[1].steps[0]), vec!["https://archlinux.org/releng/releases/2026.10.01/torrent/"]);
    }

    #[test]
    fn checksums() {
        assert_eq!(parse_sha256("B542DB5FECC37395D9AF6654A8146EBED225F275AE4D5A6FCF432E40AFE7BB88  omarchy-4.0.6.iso\n").unwrap().len(), 64);
        assert_eq!(parse_sha256("not a hash"), None);
        assert_eq!(parse_sha256(""), None);
    }

    #[test]
    fn every_pattern_compiles() {
        for d in DISTROS {
            assert!(!d.steps.is_empty(), "{}", d.id);
            for s in d.steps {
                regex::Regex::new(s).unwrap_or_else(|e| panic!("{}: {e}", d.id));
            }
        }
    }
}
