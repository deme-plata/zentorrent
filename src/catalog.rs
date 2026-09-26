//! Linux catalog: resolve the CURRENT official release torrents at runtime
//! from each project's own index, so the buttons never point at an old ISO.
//! If a lookup fails (offline, page changed) we fall back to a known-good
//! link verified when this file was written (2026-09-26).

use regex::Regex;

#[derive(Clone, Debug)]
pub struct CatalogEntry {
    pub distro: String,
    pub version: String,
    /// Magnet link or http(s) URL of a .torrent file.
    pub link: String,
}

pub async fn resolve_all() -> Vec<CatalogEntry> {
    let client = reqwest::Client::builder()
        .user_agent("ZenTorrent/0.1")
        .timeout(std::time::Duration::from_secs(15))
        .build()
        .expect("http client");

    let (u, d, a) = tokio::join!(ubuntu_lts(&client), debian(&client), arch(&client));
    vec![
        u.unwrap_or_else(|| entry(
            "Ubuntu Desktop LTS",
            "26.04.1 (fallback)",
            "https://releases.ubuntu.com/26.04.1/ubuntu-26.04.1-desktop-amd64.iso.torrent",
        )),
        d.unwrap_or_else(|| entry(
            "Debian netinst",
            "13.7.0 (fallback)",
            "https://cdimage.debian.org/debian-cd/current/amd64/bt-cd/debian-13.7.0-amd64-netinst.iso.torrent",
        )),
        a.unwrap_or_else(|| entry(
            "Arch Linux",
            "2026.09.01 (fallback)",
            "magnet:?xt=urn:btih:f45add9d1a5185d8588df7dd6cd89993dd0174fa&dn=archlinux-2026.09.01-x86_64.iso",
        )),
    ]
}

fn entry(distro: &str, version: &str, link: &str) -> CatalogEntry {
    CatalogEntry { distro: distro.into(), version: version.into(), link: link.into() }
}

async fn get(client: &reqwest::Client, url: &str) -> Option<String> {
    let r = client.get(url).send().await.ok()?;
    if !r.status().is_success() {
        return None;
    }
    r.text().await.ok()
}

/// Sort key for dotted version strings: "26.04.1" -> [26, 4, 1].
fn vkey(v: &str) -> Vec<u32> {
    v.split('.').filter_map(|p| p.parse().ok()).collect()
}

/// Newest LTS = highest `YY.04[.N]` directory with an even YY.
async fn ubuntu_lts(client: &reqwest::Client) -> Option<CatalogEntry> {
    let root = get(client, "https://releases.ubuntu.com/").await?;
    let dir_re = Regex::new(r#"href="((\d\d)\.04(?:\.\d+)?)/""#).unwrap();
    let dir = dir_re
        .captures_iter(&root)
        .filter(|c| c[2].parse::<u32>().map(|y| y % 2 == 0).unwrap_or(false))
        .map(|c| c[1].to_string())
        .max_by_key(|v| vkey(v))?;

    let page = get(client, &format!("https://releases.ubuntu.com/{dir}/")).await?;
    let file_re = Regex::new(r"ubuntu-([\d.]+)-desktop-amd64\.iso\.torrent").unwrap();
    let (ver, file) = file_re
        .captures_iter(&page)
        .map(|c| (c[1].to_string(), c[0].to_string()))
        .max_by_key(|(v, _)| vkey(v))?;
    Some(entry(
        "Ubuntu Desktop LTS",
        &ver,
        &format!("https://releases.ubuntu.com/{dir}/{file}"),
    ))
}

async fn debian(client: &reqwest::Client) -> Option<CatalogEntry> {
    let base = "https://cdimage.debian.org/debian-cd/current/amd64/bt-cd/";
    let page = get(client, base).await?;
    let re = Regex::new(r"debian-([\d.]+)-amd64-netinst\.iso\.torrent").unwrap();
    let (ver, file) = re
        .captures_iter(&page)
        .map(|c| (c[1].to_string(), c[0].to_string()))
        .max_by_key(|(v, _)| vkey(v))?;
    Some(entry("Debian netinst", &ver, &format!("{base}{file}")))
}

async fn arch(client: &reqwest::Client) -> Option<CatalogEntry> {
    let body = get(client, "https://archlinux.org/releng/releases/json/").await?;
    let v: serde_json::Value = serde_json::from_str(&body).ok()?;
    let rel = v["releases"].as_array()?.iter().find(|r| r["available"].as_bool().unwrap_or(true))?;
    let magnet = rel["magnet_uri"].as_str()?;
    let ver = rel["version"].as_str().unwrap_or("latest");
    Some(entry("Arch Linux", ver, magnet))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn version_keys_order_numerically() {
        assert!(vkey("26.04.1") > vkey("26.04"));
        assert!(vkey("24.04.10") > vkey("24.04.9"));
        assert!(vkey("13.10.0") > vkey("13.7.0"));
    }

    /// Hits the network: `fluxc test -- --ignored` to run.
    #[tokio::test]
    #[ignore]
    async fn resolves_live_catalog() {
        let c = resolve_all().await;
        assert_eq!(c.len(), 3);
        for e in &c {
            assert!(!e.version.contains("fallback"), "fell back: {e:?}");
        }
    }
}
