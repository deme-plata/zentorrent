//! Auto-download rules for the RSS feeds: from now on, new feed items a rule wants are
//! downloaded by themselves.
//!
//! A rule is written like a search (see `search`), plus what release names say:
//!
//! | you write | means |
//! |---|---|
//! | `the bear` | the words in the title: prefixes, and one typo in longer words, are forgiven (`bea`, `sevrance`) |
//! | `s04` | season 4: its episodes and season packs (`S04E03`, `S04`, `4x03`, `Season 4`) |
//! | `s04e05` | that one episode |
//! | `s04e05+` / `s04+` | from that episode / season on |
//! | `1080p` `720p` `2160p` `4k` | only that quality (or pick it in the editor) |
//! | `-cam` `"exact words"` `seeders>5` `size<4gb` `feed:torrentbytes` `free` | as in search |
//!
//! "Each episode once" (on unless turned off): when a 720p and a 1080p release of an
//! episode both turn up, the first one is downloaded and the other is not.
//!
//! Old rules (one regex per feed, before 0.10.16) become rules here with `regex` set.

use std::sync::LazyLock;

use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::history::Entry;
use crate::search;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum Quality {
    #[default]
    Any,
    P480,
    P720,
    P1080,
    P2160,
}

impl Quality {
    pub const ALL: [Quality; 5] = [Quality::Any, Quality::P2160, Quality::P1080, Quality::P720, Quality::P480];

    pub fn label(self) -> &'static str {
        match self {
            Quality::Any => "Any quality",
            Quality::P480 => "480p",
            Quality::P720 => "720p",
            Quality::P1080 => "1080p",
            Quality::P2160 => "2160p / 4K",
        }
    }

    /// The word for it in a rule (`1080p`), empty for Any.
    pub fn word(self) -> &'static str {
        match self {
            Quality::Any => "",
            Quality::P480 => "480p",
            Quality::P720 => "720p",
            Quality::P1080 => "1080p",
            Quality::P2160 => "2160p",
        }
    }

    fn from_word(w: &str) -> Option<Quality> {
        match w.to_ascii_lowercase().as_str() {
            "480p" | "576p" | "sd" => Some(Quality::P480),
            "720p" => Some(Quality::P720),
            "1080p" | "1080i" | "fhd" => Some(Quality::P1080),
            "2160p" | "4k" | "uhd" => Some(Quality::P2160),
            _ => None,
        }
    }
}

/// What a release name says about itself.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Release {
    /// The words before the episode / year / quality part: the show (or film, or artist).
    pub show: String,
    pub season: Option<u32>,
    /// None with a season: a season pack.
    pub episode: Option<u32>,
    pub quality: Option<Quality>,
}

static SXXEYY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\bs(\d{1,2})[ ._-]?e(\d{1,3})").unwrap());
static NXNN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\b(\d{1,2})x(\d{2,3})\b").unwrap());
static SEASON: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\b(?:s(\d{1,2})|season[ ._-]?(\d{1,2}))\b").unwrap());
static QUAL: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)\b(2160p|4k|uhd|1080[pi]|720p|576p|480p)\b").unwrap());
/// Where the name part of a release ends when there is no episode: a year, a bracket.
static NAME_END: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)[\[(]|\b(19[2-9]\d|20\d\d)\b").unwrap());

pub fn release(title: &str) -> Release {
    let num = |c: &regex::Captures, i: usize| c.get(i).and_then(|m| m.as_str().parse::<u32>().ok());
    let (season, episode, at) = if let Some(c) = SXXEYY.captures(title) {
        (num(&c, 1), num(&c, 2), c.get(0).map(|m| m.start()))
    } else if let Some(c) = NXNN.captures(title) {
        (num(&c, 1), num(&c, 2), c.get(0).map(|m| m.start()))
    } else if let Some(c) = SEASON.captures(title) {
        (num(&c, 1).or(num(&c, 2)), None, c.get(0).map(|m| m.start()))
    } else {
        (None, None, None)
    };
    let quality = QUAL.captures(title).and_then(|c| Quality::from_word(&c[1]));
    // The show: before the episode; without one, before a year, bracket or the quality.
    let end = at
        .or_else(|| NAME_END.find(title).map(|m| m.start()))
        .or_else(|| QUAL.find(title).map(|m| m.start()))
        .unwrap_or(title.len());
    let mut name = &title[..end];
    // "Artist - Album": the artist.
    if season.is_none() {
        if let Some((a, _)) = name.split_once(" - ") {
            name = a;
        }
    }
    let mut words = search::tokens(name);
    // A year right before the episode ("Show.2024.S01E02") is not part of the name.
    if words.last().is_some_and(|w| w.len() == 4 && w.chars().all(|c| c.is_ascii_digit())) && words.len() > 1 {
        words.pop();
    }
    Release { show: words.join(" "), season, episode, quality }
}

/// "Each episode once": the episode's key (`s04e03`), a season pack's (`s04`).
fn episode_key(r: &Release) -> Option<String> {
    match (r.season, r.episode) {
        (Some(s), Some(e)) => Some(format!("{}|s{s:02}e{e:02}", r.show)),
        (Some(s), None) => Some(format!("{}|s{s:02}", r.show)),
        _ => None,
    }
}

fn yes() -> bool {
    true
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Rule {
    pub name: String,
    /// What to download, written like a search (see the module docs).
    pub query: String,
    #[serde(default = "yes")]
    pub enabled: bool,
    #[serde(default)]
    pub quality: Quality,
    /// The feed it watches (`history::feed_key`); empty = every feed.
    #[serde(default)]
    pub feed_key: String,
    /// Another release of an episode it already got is not downloaded.
    #[serde(default = "yes")]
    pub once_per_episode: bool,
    /// `query` is a regular expression on the title (rules from before 0.10.16).
    #[serde(default)]
    pub regex: bool,
    /// Episodes (or links) it has downloaded.
    #[serde(default)]
    pub got: Vec<String>,
    #[serde(default)]
    pub hits: u32,
    /// The newest one it downloaded, and when (unix seconds).
    #[serde(default)]
    pub last: String,
    #[serde(default)]
    pub last_at: u64,
}

/// What a rule asks for, as understood.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Plan {
    /// The rest, as a search query (words, -exclusions, field filters).
    pub search: String,
    pub season: Option<u32>,
    pub episode: Option<u32>,
    /// `+`: this season / episode and later ones.
    pub onward: bool,
    pub quality: Quality,
}

static Q_SXXEYY: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^s(\d{1,2})e(\d{1,3})(\+?)$").unwrap());
static Q_NXNN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^(\d{1,2})x(\d{2,3})(\+?)$").unwrap());
static Q_SXX: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?i)^s(\d{1,2})(\+?)$").unwrap());

pub fn plan(query: &str, quality: Quality) -> Plan {
    let mut p = Plan { quality, ..Default::default() };
    let mut rest: Vec<String> = Vec::new();
    let mut in_quote = false;
    let mut words = query.split_whitespace().peekable();
    while let Some(w) = words.next() {
        let quoted = in_quote || w.starts_with('"');
        in_quote = quoted && !(w.ends_with('"') && (w.len() > 1 || in_quote));
        if !quoted {
            let num = |c: &regex::Captures, i: usize| c.get(i).and_then(|m| m.as_str().parse::<u32>().ok());
            if let Some(c) = Q_SXXEYY.captures(w).or_else(|| Q_NXNN.captures(w)) {
                (p.season, p.episode, p.onward) = (num(&c, 1), num(&c, 2), &c[3] == "+");
                continue;
            }
            if let Some(c) = Q_SXX.captures(w) {
                (p.season, p.episode, p.onward) = (num(&c, 1), None, &c[2] == "+");
                continue;
            }
            if w.eq_ignore_ascii_case("season") {
                if let Some(n) = words.peek().and_then(|n| n.trim_end_matches('+').parse::<u32>().ok()) {
                    p.onward = words.next().is_some_and(|n| n.ends_with('+'));
                    (p.season, p.episode) = (Some(n), None);
                    continue;
                }
            }
            if let Some(q) = Quality::from_word(w) {
                if p.quality == Quality::Any {
                    p.quality = q;
                }
                continue;
            }
        }
        rest.push(w.to_string());
    }
    p.search = rest.join(" ");
    p
}

impl Plan {
    /// In words, for the editor and Flux MoE: `the bear · season 4 · 1080p`.
    pub fn describe(&self) -> String {
        let mut parts = vec![if self.search.trim().is_empty() { "(no words)".to_string() } else { self.search.trim().to_string() }];
        let on = if self.onward { " and later" } else { "" };
        match (self.season, self.episode) {
            (Some(s), Some(e)) => parts.push(format!("S{s:02}E{e:02}{on}")),
            (Some(s), None) => parts.push(format!("season {s}{on}")),
            _ => {}
        }
        if self.quality != Quality::Any {
            parts.push(self.quality.label().to_string());
        }
        parts.join(" · ")
    }
}

/// A rule ready to test items against (built once, not per item).
pub struct Matcher {
    regex: Option<Regex>,
    query: search::Query,
    plan: Plan,
    feed_key: String,
}

impl Matcher {
    pub fn new(rule: &Rule) -> Result<Matcher, String> {
        if rule.regex {
            let re = regex::RegexBuilder::new(rule.query.trim())
                .case_insensitive(true)
                .size_limit(1 << 20)
                .build()
                .map_err(|e| format!("not a valid pattern: {e}"))?;
            return Ok(Matcher { regex: Some(re), query: search::Query::default(), plan: Plan::default(), feed_key: rule.feed_key.clone() });
        }
        let plan = plan(&rule.query, rule.quality);
        let query = search::parse(&plan.search);
        if !query.has_words() {
            return Err("say what to download: a show, an artist or other words from the title".into());
        }
        Ok(Matcher { regex: None, query, plan, feed_key: rule.feed_key.clone() })
    }

    /// Whether the rule wants this item (episode bookkeeping aside).
    pub fn wants(&self, e: &Entry) -> bool {
        if !self.feed_key.is_empty() && e.feed_key != self.feed_key {
            return false;
        }
        if let Some(re) = &self.regex {
            return re.is_match(&e.title);
        }
        let r = release(&e.title);
        let p = &self.plan;
        if let Some(want) = p.season {
            let Some(s) = r.season else { return false };
            match (p.episode, r.episode) {
                (None, _) if p.onward => {
                    if s < want {
                        return false;
                    }
                }
                (None, _) => {
                    if s != want {
                        return false;
                    }
                }
                // One episode asked for: a pack of its season is not it.
                (Some(_), None) => return false,
                (Some(we), Some(e)) if p.onward => {
                    if (s, e) < (want, we) {
                        return false;
                    }
                }
                (Some(we), Some(e)) => {
                    if (s, e) != (want, we) {
                        return false;
                    }
                }
            }
        }
        if p.quality != Quality::Any && r.quality != Some(p.quality) {
            return false;
        }
        search::matches_title(e, &self.query)
    }
}

/// What a rule would have caught among `entries`, newest first.
pub fn preview(rule: &Rule, entries: &[Entry]) -> Result<Vec<usize>, String> {
    let m = Matcher::new(rule)?;
    let mut hits: Vec<usize> = entries.iter().enumerate().filter(|(_, e)| m.wants(e)).map(|(i, _)| i).collect();
    hits.sort_by_key(|&i| std::cmp::Reverse(entries[i].first_seen));
    Ok(hits)
}

/// The rule syntax, for tooltips.
pub const HELP: &str = "Write it like a search:\n\
    the bear           the words in the title (prefixes, and one typo in longer words, are forgiven)\n\
    s04                season 4: its episodes and season packs\n\
    s04e05             that one episode;  s04e05+ / s04+  from there on\n\
    1080p  720p  4k    only that quality\n\
    -cam               leave out titles with this word\n\
    \"exact words\"      a phrase\n\
    seeders>5  size<4gb  feed:torrentbytes  free\n\n\
    Only NEW items download: what a feed already listed when you made the rule does not.";

/// New feed items the rules want, each once: (rule index, item). Each rule notes what it
/// took (for "each episode once" and its counters).
pub fn pick(rules: &mut [Rule], fresh: &[Entry], now: u64) -> Vec<(usize, Entry)> {
    let mut out: Vec<(usize, Entry)> = Vec::new();
    // What any rule already took: an episode is one download, whichever rule got it.
    let mut taken: std::collections::HashSet<String> = rules.iter().flat_map(|r| r.got.iter().cloned()).collect();
    for (ri, rule) in rules.iter_mut().enumerate().filter(|(_, r)| r.enabled) {
        let Ok(m) = Matcher::new(rule) else { continue };
        for e in fresh.iter().filter(|e| m.wants(e)) {
            if out.iter().any(|(_, o)| o.link == e.link) {
                continue; // two rules, one download
            }
            let r = release(&e.title);
            let key = episode_key(&r).filter(|_| rule.once_per_episode && !rule.regex).unwrap_or_else(|| e.link.clone());
            // A season pack already taken covers that season's episodes too.
            let pack = r.season.map(|s| format!("{}|s{s:02}", r.show));
            if taken.contains(&key) || pack.is_some_and(|p| rule.once_per_episode && taken.contains(&p)) {
                continue;
            }
            taken.insert(key.clone());
            rule.got.push(key);
            if rule.got.len() > 500 {
                rule.got.remove(0);
            }
            rule.hits += 1;
            rule.last = e.title.clone();
            rule.last_at = now;
            out.push((ri, e.clone()));
        }
    }
    out
}

/// "Auto-download more like this": a rule made from one item — the show and season of an
/// episode (in its quality), else the name before the year / " - ".
pub fn like(e: &Entry) -> Rule {
    let r = release(&e.title);
    let show = r.show.split_whitespace().take(6).collect::<Vec<_>>().join(" ");
    let (query, name) = match r.season {
        Some(s) => (format!("{show} s{s:02}"), format!("{} S{s:02}", title_case(&show))),
        None => (show.clone(), title_case(&show)),
    };
    Rule { name, query, quality: if r.season.is_some() { r.quality.unwrap_or_default() } else { Quality::Any }, feed_key: e.feed_key.clone(), ..new_rule() }
}

/// A new, empty rule (on, each episode once).
pub fn new_rule() -> Rule {
    Rule { enabled: true, once_per_episode: true, ..Default::default() }
}

/// A name for a rule from what it asks for: `The Bear S04 1080p`.
pub fn name_for(query: &str, quality: Quality) -> String {
    let p = plan(query, quality);
    let mut n = title_case(&search::tokens(&p.search).join(" "));
    match (p.season, p.episode) {
        (Some(s), Some(e)) => n += &format!(" S{s:02}E{e:02}{}", if p.onward { "+" } else { "" }),
        (Some(s), None) => n += &format!(" S{s:02}{}", if p.onward { "+" } else { "" }),
        _ => {}
    }
    if p.quality != Quality::Any {
        n += &format!(" {}", p.quality.word());
    }
    n.trim().to_string()
}

fn title_case(s: &str) -> String {
    s.split_whitespace()
        .map(|w| {
            let mut c = w.chars();
            c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
        })
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(title: &str) -> Entry {
        Entry { title: title.into(), link: format!("l:{title}"), feed: "TB".into(), feed_key: "tb".into(), ..Default::default() }
    }

    #[test]
    fn release_names_are_understood() {
        let r = release("The.Bear.S04E03.1080p.WEB.h264-GRP");
        assert_eq!((r.show.as_str(), r.season, r.episode, r.quality), ("the bear", Some(4), Some(3), Some(Quality::P1080)));
        let r = release("Severance.2022.S02.2160p.ATVP.WEB-DL");
        assert_eq!((r.show.as_str(), r.season, r.episode, r.quality), ("severance", Some(2), None, Some(Quality::P2160)));
        let r = release("Taskmaster 4x07 720p HDTV");
        assert_eq!((r.season, r.episode, r.quality), (Some(4), Some(7), Some(Quality::P720)));
        let r = release("Above & Beyond - Group Therapy 600 (2026) [FLAC]");
        assert_eq!((r.show.as_str(), r.season), ("above beyond", None));
        let r = release("Dune.Part.Two.2024.1080p.BluRay.x264");
        assert_eq!(r.show, "dune part two");
    }

    #[test]
    fn a_rule_reads_season_episode_and_quality() {
        let p = plan("the bear s04 1080p -cam", Quality::Any);
        assert_eq!((p.search.as_str(), p.season, p.episode, p.quality, p.onward), ("the bear -cam", Some(4), None, Quality::P1080, false));
        let p = plan("severance s02e05+", Quality::P720);
        assert_eq!((p.season, p.episode, p.onward, p.quality), (Some(2), Some(5), true, Quality::P720));
        let p = plan("taskmaster season 4+", Quality::Any);
        assert_eq!((p.search.as_str(), p.season, p.onward), ("taskmaster", Some(4), true));
        let p = plan("\"s01 live\" foo", Quality::Any);
        assert_eq!((p.search.as_str(), p.season), ("\"s01 live\" foo", None), "quoted words stay words");
        assert_eq!(p.describe(), "\"s01 live\" foo");
        assert_eq!(plan("the bear s04 1080p", Quality::Any).describe(), "the bear · season 4 · 1080p");
    }

    #[test]
    fn matching_is_fuzzy_on_the_name_and_strict_on_the_episode() {
        let rule = Rule { query: "the bear s04 1080p -cam".into(), ..new_rule() };
        let m = Matcher::new(&rule).unwrap();
        assert!(m.wants(&e("The.Bear.S04E03.1080p.WEB.h264-GRP")));
        let typo = Matcher::new(&Rule { query: "sevrance s02".into(), ..new_rule() }).unwrap();
        assert!(typo.wants(&e("Severance.S02E01.1080p")), "one typo forgiven in a longer word");
        assert!(Matcher::new(&Rule { query: "bea s04".into(), ..new_rule() }).unwrap().wants(&e("The.Bear.S04E01")), "prefix");
        assert!(m.wants(&e("The Bear S04 1080p WEB pack")), "season pack");
        assert!(!m.wants(&e("The.Bear.S04E03.720p.WEB.h264-GRP")), "wrong quality");
        assert!(!m.wants(&e("The.Bear.S03E09.1080p.WEB.h264-GRP")), "wrong season");
        assert!(!m.wants(&e("The.Bear.S04E03.1080p.CAM")), "excluded word");
        let onward = Matcher::new(&Rule { query: "severance s02e05+".into(), ..new_rule() }).unwrap();
        assert!(!onward.wants(&e("Severance.S02E04.720p")));
        assert!(onward.wants(&e("Severance.S02E05.720p")) && onward.wants(&e("Severance.S03E01.2160p")));
        assert!(!onward.wants(&e("Severance.S02.1080p")), "a pack is not one episode");
        let other_feed = Matcher::new(&Rule { query: "bear".into(), feed_key: "other".into(), ..new_rule() }).unwrap();
        assert!(!other_feed.wants(&e("The.Bear.S04E03.1080p")));
        assert!(Matcher::new(&Rule { query: "s04 1080p".into(), ..new_rule() }).is_err(), "no words: would take everything");
        let old = Matcher::new(&Rule { query: "debian.*netinst".into(), regex: true, ..new_rule() }).unwrap();
        assert!(old.wants(&e("Debian 13 netinst amd64")) && !old.wants(&e("Fedora")));
    }

    #[test]
    fn each_episode_once_and_two_rules_one_download() {
        let mut rules = vec![Rule { query: "the bear s04".into(), ..new_rule() }, Rule { query: "bear".into(), ..new_rule() }];
        let fresh = [e("The.Bear.S04E03.720p.WEB"), e("The.Bear.S04E03.1080p.WEB"), e("The.Bear.S04E04.1080p.WEB")];
        let got = pick(&mut rules, &fresh, 100);
        let titles: Vec<&str> = got.iter().map(|(_, e)| e.title.as_str()).collect();
        assert_eq!(titles, ["The.Bear.S04E03.720p.WEB", "The.Bear.S04E04.1080p.WEB"], "S04E03 once; nothing twice");
        assert_eq!((rules[0].hits, rules[0].last.as_str(), rules[0].last_at), (2, "The.Bear.S04E04.1080p.WEB", 100));
        // The next read brings another release of E04 and the season pack: E04 is skipped.
        let more = [e("The.Bear.S04E04.2160p.WEB"), e("The.Bear.S04E05.1080p.WEB")];
        let got = pick(&mut rules, &more, 200);
        assert_eq!(got.iter().map(|(_, e)| e.title.as_str()).collect::<Vec<_>>(), ["The.Bear.S04E05.1080p.WEB"]);
        // Turned off: nothing.
        rules.iter_mut().for_each(|r| r.enabled = false);
        assert!(pick(&mut rules, &[e("The.Bear.S04E06.1080p.WEB")], 300).is_empty());
    }

    #[test]
    fn more_like_this_and_names() {
        let r = like(&e("The.Bear.S04E03.1080p.WEB.h264-GRP"));
        assert_eq!((r.query.as_str(), r.name.as_str(), r.quality, r.feed_key.as_str()), ("the bear s04", "The Bear S04", Quality::P1080, "tb"));
        let r = like(&e("Above & Beyond - Group Therapy 600 (2026) [FLAC]"));
        assert_eq!((r.query.as_str(), r.quality), ("above beyond", Quality::Any));
        assert_eq!(name_for("the bear s04e05+ 1080p", Quality::Any), "The Bear S04E05+ 1080p");
        let hits = preview(&Rule { query: "bear s04".into(), ..new_rule() }, &[e("The.Bear.S04E01"), e("Other"), e("The.Bear.S04E02")]).unwrap();
        assert_eq!(hits.len(), 2);
    }
}
