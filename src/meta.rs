//! Posters and ratings for feed items (IMDb, Rotten Tomatoes, Metascore).
//!
//! Source: the OMDb API (https://www.omdbapi.com), one HTTPS request per
//! title, with the user's own free key. We do NOT scrape imdb.com or
//! rottentomatoes.com pages: that breaks their terms and breaks on every
//! redesign. Care taken:
//! - HTTPS only, for the API and for posters (http poster links are
//!   upgraded; anything else is refused);
//! - every answer is cached on disk (hits 30 days, misses 3 days), so a
//!   feed refresh every 15 minutes costs nothing after the first read;
//! - one request at a time, 300 ms apart, and a daily budget below the
//!   free tier's 1,000;
//! - posters are size-capped (3 MB) and cached as files;
//! - the key is never shown or logged.

use std::{
    collections::HashMap,
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::{Deserialize, Serialize};

pub const DAILY_BUDGET: u32 = 900;
const HIT_TTL: u64 = 30 * 86400;
const MISS_TTL: u64 = 3 * 86400;
const POSTER_MAX: usize = 3 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Query {
    pub title: String,
    pub year: Option<u16>,
    pub series: bool,
}

impl Query {
    pub fn key(&self) -> String {
        format!(
            "{}|{}|{}",
            self.title.to_lowercase(),
            self.year.map(|y| y.to_string()).unwrap_or_default(),
            if self.series { "series" } else { "movie" }
        )
    }
}

#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct Info {
    pub title: String,
    pub year: String,
    pub imdb_id: String,
    pub imdb_rating: Option<f32>,
    pub imdb_votes: String,
    /// Rotten Tomatoes Tomatometer, 0–100.
    pub rotten: Option<u8>,
    pub metascore: Option<u8>,
    pub genre: String,
    pub plot: String,
    pub poster: Option<String>,
}

impl Info {
    pub fn imdb_url(&self) -> String {
        format!("https://www.imdb.com/title/{}/", self.imdb_id)
    }
}

/// Guess a film or series from a scene-style release name.
/// Returns None for things that are not films or series (music, games,
/// software, sports dates, ...), so we never waste a lookup on them.
pub fn guess(release: &str, category: &str) -> Option<Query> {
    let cat = category.to_lowercase();
    const SKIP: [&str; 12] =
        ["music", "mp3", "flac", "audio", "game", "app", "software", "ebook", "book", "xxx", "sport", "pc"];
    if SKIP.iter().any(|s| cat.contains(s)) {
        return None;
    }
    let norm: String = release
        .chars()
        .map(|c| if c == '.' || c == '_' { ' ' } else { c })
        .collect();
    // Daily shows / sports: 2026 09 26 → no single title to look up.
    if regex::Regex::new(r"\b(19|20)\d\d \d\d \d\d\b").unwrap().is_match(&norm) {
        return None;
    }
    let padded = format!(" {norm} ");
    let ep = regex::Regex::new(r"(?i) (S\d{1,2}(E\d{1,3})?|Season \d+) ").unwrap();
    let ep_at = ep.find(&padded).map(|m| m.start());
    let limit = ep_at.unwrap_or(padded.len());
    // Year candidates: 1900–2027, standing alone or in brackets. Take the LAST
    // one before the episode marker that still leaves a title in front of it
    // ("Blade Runner 2049 (2017)", "1917 2019 1080p", "2001 A Space Odyssey 1968").
    let yr = regex::Regex::new(r"[ (\[]((?:19|20)\d\d)[ )\]]").unwrap();
    let mut year_at = None;
    let mut pos = 0;
    while let Some(c) = yr.captures_at(&padded, pos) {
        let m = c.get(0).unwrap();
        pos = m.start() + 1;
        let y: u16 = c[1].parse().unwrap();
        if m.start() < limit && y <= 2027 && has_title(&padded[..m.start()]) {
            year_at = Some((m.start(), y));
        }
    }
    let cut = match (year_at, ep_at) {
        (Some((p, _)), _) => p,
        (None, Some(e)) => e,
        (None, None) => return None,
    };
    let series = ep_at.is_some() || cat.contains("tv");
    let title = regex::Regex::new(r"\s+").unwrap().replace_all(padded[..cut].trim(), " ").to_string();
    let title = title.trim_end_matches(['-', '(', '[', ' ']).to_string();
    if !has_title(&title) {
        return None;
    }
    Some(Query { title, year: year_at.map(|(_, y)| y), series })
}

fn has_title(s: &str) -> bool {
    s.chars().filter(|c| c.is_alphanumeric()).count() >= 2
}

/// Parse an OMDb JSON answer. `Ok(None)` = OMDb says "not found".
pub fn parse_omdb(json: &str) -> anyhow::Result<Option<Info>> {
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct R {
        response: String,
        #[serde(default)]
        error: String,
        #[serde(default)]
        title: String,
        #[serde(default)]
        year: String,
        #[serde(default, rename = "imdbID")]
        imdb_id: String,
        #[serde(default, rename = "imdbRating")]
        imdb_rating: String,
        #[serde(default, rename = "imdbVotes")]
        imdb_votes: String,
        #[serde(default)]
        metascore: String,
        #[serde(default)]
        genre: String,
        #[serde(default)]
        plot: String,
        #[serde(default)]
        poster: String,
        #[serde(default)]
        ratings: Vec<Rating>,
    }
    #[derive(Deserialize)]
    #[serde(rename_all = "PascalCase")]
    struct Rating {
        source: String,
        value: String,
    }
    let r: R = serde_json::from_str(json)?;
    if r.response != "True" {
        if r.error.to_lowercase().contains("not found") {
            return Ok(None);
        }
        // Bad key, limit reached, ...: an error, not a miss (do not cache).
        anyhow::bail!("OMDb: {}", r.error);
    }
    let na = |s: &str| s.is_empty() || s == "N/A";
    let rotten = r
        .ratings
        .iter()
        .find(|x| x.source == "Rotten Tomatoes")
        .and_then(|x| x.value.trim_end_matches('%').parse().ok());
    Ok(Some(Info {
        title: r.title,
        year: r.year,
        imdb_id: r.imdb_id,
        imdb_rating: (!na(&r.imdb_rating)).then(|| r.imdb_rating.parse().ok()).flatten(),
        imdb_votes: if na(&r.imdb_votes) { String::new() } else { r.imdb_votes },
        rotten,
        metascore: (!na(&r.metascore)).then(|| r.metascore.parse().ok()).flatten(),
        genre: if na(&r.genre) { String::new() } else { r.genre },
        plot: if na(&r.plot) { String::new() } else { r.plot },
        poster: https_only(&r.poster),
    }))
}

/// Posters must be fetched over HTTPS. http:// is upgraded; anything else refused.
pub fn https_only(url: &str) -> Option<String> {
    let u = url.trim();
    if let Some(rest) = u.strip_prefix("http://") {
        Some(format!("https://{rest}"))
    } else if u.starts_with("https://") {
        Some(u.to_string())
    } else {
        None
    }
}

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Cached {
    info: Option<Info>,
    at: u64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
pub struct Cache {
    entries: HashMap<String, Cached>,
    /// (UTC day number, lookups made that day)
    #[serde(default)]
    budget: (u64, u32),
}

impl Cache {
    fn path() -> PathBuf {
        crate::seed::data_dir().join("omdb-cache.json")
    }

    pub fn load() -> Self {
        std::fs::read(Self::path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn save(&self) {
        let p = Self::path();
        let _ = std::fs::create_dir_all(p.parent().unwrap());
        let tmp = p.with_extension("json.tmp");
        if std::fs::write(&tmp, serde_json::to_vec(self).unwrap_or_default()).is_ok() {
            let _ = std::fs::rename(tmp, p);
        }
    }

    /// Some(answer) if we have a fresh cached answer (answer may be "not found").
    pub fn get(&self, key: &str) -> Option<Option<Info>> {
        let c = self.entries.get(key)?;
        let ttl = if c.info.is_some() { HIT_TTL } else { MISS_TTL };
        (now().saturating_sub(c.at) < ttl).then(|| c.info.clone())
    }

    pub fn put(&mut self, key: String, info: Option<Info>) {
        self.entries.insert(key, Cached { info, at: now() });
    }

    /// Take one lookup from today's budget; false when it is spent.
    pub fn spend(&mut self) -> bool {
        let day = now() / 86400;
        if self.budget.0 != day {
            self.budget = (day, 0);
        }
        if self.budget.1 >= DAILY_BUDGET {
            return false;
        }
        self.budget.1 += 1;
        true
    }

    pub fn used_today(&self) -> u32 {
        if self.budget.0 == now() / 86400 { self.budget.1 } else { 0 }
    }
}

pub async fn lookup(http: &reqwest::Client, key: &str, q: &Query) -> anyhow::Result<Option<Info>> {
    let mut url = reqwest::Url::parse("https://www.omdbapi.com/")?;
    {
        let mut qp = url.query_pairs_mut();
        qp.append_pair("apikey", key.trim());
        qp.append_pair("t", &q.title);
        qp.append_pair("type", if q.series { "series" } else { "movie" });
        if let Some(y) = q.year.filter(|_| !q.series) {
            qp.append_pair("y", &y.to_string());
        }
    }
    let r = http.get(url).send().await.map_err(|e| anyhow::anyhow!("OMDb unreachable: {}", e.without_url()))?;
    let status = r.status();
    // without_url(): the request URL carries the API key; never put it in an error.
    let body = r.text().await.map_err(|e| anyhow::anyhow!("OMDb: {}", e.without_url()))?;
    if status == reqwest::StatusCode::UNAUTHORIZED {
        // OMDb answers 401 with a JSON error for a bad key or a spent limit.
        return parse_omdb(&body).and_then(|_| anyhow::bail!("OMDb: key refused"));
    }
    if !status.is_success() {
        anyhow::bail!("OMDb: HTTP {status}");
    }
    let mut hit = parse_omdb(&body)?;
    // A year from a release name is often the release year of an episode
    // pack; for films a miss with a year gets one retry without it.
    if hit.is_none() && q.year.is_some() && !q.series {
        let q2 = Query { year: None, ..q.clone() };
        return Box::pin(lookup(http, key, &q2)).await;
    }
    if let Some(i) = hit.as_mut() {
        i.poster = i.poster.as_deref().and_then(https_only);
    }
    Ok(hit)
}

/// Poster bytes, from the disk cache or HTTPS. Capped at 3 MB.
pub async fn poster(http: &reqwest::Client, imdb_id: &str, url: &str) -> anyhow::Result<Vec<u8>> {
    let safe_id: String = imdb_id.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    let file = crate::seed::data_dir().join("posters").join(format!("{safe_id}.img"));
    if let Ok(b) = std::fs::read(&file) {
        return Ok(b);
    }
    let url = https_only(url).ok_or_else(|| anyhow::anyhow!("poster link is not https"))?;
    let r = http.get(&url).send().await?.error_for_status()?;
    if r.content_length().is_some_and(|n| n as usize > POSTER_MAX) {
        anyhow::bail!("poster too large");
    }
    let b = r.bytes().await?;
    if b.len() > POSTER_MAX {
        anyhow::bail!("poster too large");
    }
    let _ = std::fs::create_dir_all(file.parent().unwrap());
    let _ = std::fs::write(&file, &b);
    Ok(b.to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(t: &str, y: Option<u16>, s: bool) -> Option<Query> {
        Some(Query { title: t.into(), year: y, series: s })
    }

    #[test]
    fn guesses_films_and_series_from_release_names() {
        assert_eq!(guess("The.Matrix.1999.1080p.BluRay.x264-GRP", "Movies/HD"), q("The Matrix", Some(1999), false));
        assert_eq!(guess("Blade Runner 2049 (2017) 2160p WEB-DL", "Movies"), q("Blade Runner 2049", Some(2017), false));
        assert_eq!(guess("Severance.S02E05.1080p.WEB.h264-ETHEL", "TV/HD"), q("Severance", None, true));
        assert_eq!(guess("Doctor.Who.2023.S01E03.720p", "TV"), q("Doctor Who", Some(2023), true));
        assert_eq!(guess("Shogun.Season.1.Complete.1080p", "TV"), q("Shogun", None, true));
        assert_eq!(guess("1917.2019.1080p.BluRay", "Movies"), q("1917", Some(2019), false));
        assert_eq!(guess("2001.A.Space.Odyssey.1968.REMASTERED", "Movies"), q("2001 A Space Odyssey", Some(1968), false));
        assert_eq!(guess("Blade.Runner.2049.2017.2160p", "Movies"), q("Blade Runner 2049", Some(2017), false));
    }

    #[test]
    fn skips_what_has_no_imdb_page() {
        assert_eq!(guess("MLB.2026.09.26.New.York.Mets.vs.Washington.Nationals.1080p", "TV/HD"), None);
        assert_eq!(guess("Liquefaction-Delay-(LQBLQF001)-WEB-2026-PTC", "Music/MP3"), None);
        assert_eq!(guess("Some.Game.2024-RUNE", "Games/PC"), None);
        assert_eq!(guess("debian-13.7.0-amd64-netinst.iso", ""), None);
    }

    #[test]
    fn parses_omdb_answers() {
        let hit = r#"{"Title":"The Matrix","Year":"1999","Genre":"Action, Sci-Fi","Plot":"A hacker learns.",
          "Poster":"http://m.media-amazon.com/images/M/x.jpg","Ratings":[{"Source":"Internet Movie Database","Value":"8.7/10"},
          {"Source":"Rotten Tomatoes","Value":"83%"},{"Source":"Metacritic","Value":"73/100"}],
          "Metascore":"73","imdbRating":"8.7","imdbVotes":"2,100,000","imdbID":"tt0133093","Type":"movie","Response":"True"}"#;
        let i = parse_omdb(hit).unwrap().unwrap();
        assert_eq!(i.imdb_rating, Some(8.7));
        assert_eq!(i.rotten, Some(83));
        assert_eq!(i.metascore, Some(73));
        assert_eq!(i.poster.as_deref(), Some("https://m.media-amazon.com/images/M/x.jpg"));
        assert_eq!(i.imdb_url(), "https://www.imdb.com/title/tt0133093/");
        assert_eq!(parse_omdb(r#"{"Response":"False","Error":"Movie not found!"}"#).unwrap(), None);
        assert!(parse_omdb(r#"{"Response":"False","Error":"Invalid API key!"}"#).is_err());
        let na = r#"{"Title":"X","Year":"2020","imdbRating":"N/A","Metascore":"N/A","Poster":"N/A","imdbID":"tt1","Response":"True"}"#;
        let i = parse_omdb(na).unwrap().unwrap();
        assert_eq!((i.imdb_rating, i.rotten, i.metascore, i.poster), (None, None, None, None));
    }

    #[test]
    fn https_is_enforced() {
        assert_eq!(https_only("http://a/b.jpg").as_deref(), Some("https://a/b.jpg"));
        assert_eq!(https_only("https://a/b.jpg").as_deref(), Some("https://a/b.jpg"));
        assert_eq!(https_only("N/A"), None);
        assert_eq!(https_only("ftp://a"), None);
    }

    #[test]
    fn cache_and_budget() {
        let mut c = Cache::default();
        let k = Query { title: "A".into(), year: None, series: false }.key();
        assert_eq!(c.get(&k), None);
        c.put(k.clone(), None);
        assert_eq!(c.get(&k), Some(None));
        for _ in 0..DAILY_BUDGET {
            assert!(c.spend());
        }
        assert!(!c.spend());
        assert_eq!(c.used_today(), DAILY_BUDGET);
    }
}
