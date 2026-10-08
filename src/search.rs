//! Search over RSS items (live and history).
//!
//! A query is words plus optional filters:
//!
//! | you type | means |
//! |---|---|
//! | `trance live` | both words, anywhere (title counts most) |
//! | `tran` | prefix: trance, transmission… |
//! | `tarnce` | one typo is forgiven in words of 5+ letters |
//! | `"group therapy"` | exact phrase |
//! | `-remix` | leave out |
//! | `genre:trance` `cat:music` `feed:torrentleech` | field filters |
//! | `seeders>10` `size<2gb` `grabs>=100` | numbers (size in kb/mb/gb/tb) |
//! | `free` | freeleech only |
//!
//! Release names are tokenised on everything that isn't a letter or digit,
//! so `Above.and.Beyond-Group_Therapy` is four words.

use crate::history::Entry;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Sort {
    /// Best match when there are words, else newest.
    Best,
    Seeders,
    /// Grabs per hour since first seen (needs history).
    Activity,
    Newest,
    Size,
    Name,
}

impl Default for Sort {
    fn default() -> Self {
        Sort::Best
    }
}

impl Sort {
    pub const ALL: [Sort; 6] = [Sort::Best, Sort::Seeders, Sort::Activity, Sort::Newest, Sort::Size, Sort::Name];

    pub fn label(self) -> &'static str {
        match self {
            Sort::Best => "Best match",
            Sort::Seeders => "Most seeders",
            Sort::Activity => "Most active",
            Sort::Newest => "Newest",
            Sort::Size => "Largest",
            Sort::Name => "Name",
        }
    }
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Cmp {
    Gt,
    Ge,
    Lt,
    Le,
    Eq,
}

impl Cmp {
    fn ok(self, a: u64, b: u64) -> bool {
        match self {
            Cmp::Gt => a > b,
            Cmp::Ge => a >= b,
            Cmp::Lt => a < b,
            Cmp::Le => a <= b,
            Cmp::Eq => a == b,
        }
    }
}

#[derive(Clone, PartialEq, Debug)]
enum Filter {
    Genre(String),
    Cat(String),
    Feed(String),
    Seeders(Cmp, u64),
    Grabs(Cmp, u64),
    Size(Cmp, u64),
    Free,
}

#[derive(Clone, PartialEq, Debug, Default)]
pub struct Query {
    words: Vec<String>,
    phrases: Vec<String>,
    not: Vec<String>,
    filters: Vec<Filter>,
}

impl Query {
    #[cfg(test)]
    pub fn is_empty(&self) -> bool {
        *self == Query::default()
    }
}

/// Lowercase words, split on anything that isn't a letter or digit.
pub fn tokens(s: &str) -> Vec<String> {
    s.split(|c: char| !c.is_alphanumeric()).filter(|t| !t.is_empty()).map(str::to_lowercase).collect()
}

/// The same text as one normalised string, for phrase matching.
fn norm(s: &str) -> String {
    tokens(s).join(" ")
}

pub fn parse(q: &str) -> Query {
    let mut out = Query::default();
    let mut rest = q.to_string();
    // "exact phrases" first
    while let (Some(a), true) = (rest.find('"'), rest.matches('"').count() >= 2) {
        let b = a + 1 + rest[a + 1..].find('"').unwrap();
        let p = norm(&rest[a + 1..b]);
        if !p.is_empty() {
            out.phrases.push(p);
        }
        rest.replace_range(a..=b, " ");
    }
    for raw in rest.split_whitespace() {
        let lower = raw.to_lowercase();
        if lower == "free" || lower == "freeleech" {
            out.filters.push(Filter::Free);
            continue;
        }
        if let Some((k, v)) = lower.split_once(':') {
            let v = v.trim().to_string();
            let f = match k {
                "genre" | "g" | "tag" => Some(Filter::Genre(v)),
                "cat" | "category" | "c" => Some(Filter::Cat(v)),
                "feed" | "tracker" | "f" => Some(Filter::Feed(v)),
                _ => None,
            };
            if let Some(f) = f.filter(|_| !lower.ends_with(':')) {
                out.filters.push(f);
                continue;
            }
        }
        if let Some(f) = numeric(&lower) {
            out.filters.push(f);
            continue;
        }
        if let Some(w) = lower.strip_prefix('-').filter(|w| !w.is_empty()) {
            out.not.extend(tokens(w));
            continue;
        }
        out.words.extend(tokens(&lower));
    }
    out
}

/// `seeders>10`, `size<=2gb`, `grabs=0`, `seeds>5`, `s>5`.
fn numeric(t: &str) -> Option<Filter> {
    let at = t.find(['>', '<', '='])?;
    let (k, op_v) = t.split_at(at);
    let (cmp, v) = if let Some(v) = op_v.strip_prefix(">=") {
        (Cmp::Ge, v)
    } else if let Some(v) = op_v.strip_prefix("<=") {
        (Cmp::Le, v)
    } else if let Some(v) = op_v.strip_prefix('>') {
        (Cmp::Gt, v)
    } else if let Some(v) = op_v.strip_prefix('<') {
        (Cmp::Lt, v)
    } else {
        (Cmp::Eq, op_v.trim_start_matches('='))
    };
    match k {
        "seeders" | "seeds" | "seed" | "s" => Some(Filter::Seeders(cmp, v.parse().ok()?)),
        "grabs" | "snatched" | "downloads" => Some(Filter::Grabs(cmp, v.parse().ok()?)),
        "size" => Some(Filter::Size(cmp, crate::rss::parse_size(v)?)),
        _ => None,
    }
}

/// Levenshtein distance, giving up past 1 (all a typo check needs).
fn within_one(a: &str, b: &str) -> bool {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    if a.len().abs_diff(b.len()) > 1 {
        return false;
    }
    let (mut i, mut j, mut edits) = (0, 0, 0);
    while i < a.len() && j < b.len() {
        if a[i] == b[j] {
            i += 1;
            j += 1;
            continue;
        }
        edits += 1;
        if edits > 1 {
            return false;
        }
        match a.len().cmp(&b.len()) {
            std::cmp::Ordering::Greater => i += 1,
            std::cmp::Ordering::Less => j += 1,
            std::cmp::Ordering::Equal => {
                // a swap of two neighbours ("tarnce") also counts as one typo
                if i + 1 < a.len() && j + 1 < b.len() && a[i] == b[j + 1] && a[i + 1] == b[j] {
                    i += 2;
                    j += 2;
                    continue;
                }
                i += 1;
                j += 1;
            }
        }
    }
    edits + (a.len() - i) + (b.len() - j) <= 1
}

/// How well one query word matches a list of words: exact 3, prefix 2, typo 1.
fn word_score(w: &str, toks: &[String]) -> f64 {
    let mut best: f64 = 0.0;
    for t in toks {
        if t == w {
            return 3.0;
        }
        if t.starts_with(w) {
            best = best.max(2.0);
        } else if w.chars().count() >= 5 && within_one(w, t) {
            best = best.max(1.0);
        }
    }
    best
}

/// What search looks at for one item. Built once per entry per search.
struct Hay {
    title: Vec<String>,
    /// category + tags + extra (OMDb genre)
    labels: Vec<String>,
    /// description + feed name
    rest: Vec<String>,
    phrase: String,
}

impl Hay {
    fn new(e: &Entry, extra: &str) -> Self {
        let labels_text = format!("{} {} {extra}", e.category, e.tags.join(" "));
        Hay {
            title: tokens(&e.title),
            labels: tokens(&labels_text),
            rest: tokens(&format!("{} {}", e.description, e.feed)),
            phrase: format!(" {} | {} | {} ", norm(&e.title), norm(&labels_text), norm(&e.description)),
        }
    }
}

/// Score an entry; `None` = filtered out.
fn score(e: &Entry, q: &Query, h: &Hay) -> Option<f64> {
    for f in &q.filters {
        let ok = match f {
            Filter::Genre(g) | Filter::Cat(g) => {
                let want = tokens(g);
                want.iter().all(|w| word_score(w, &h.labels) >= 2.0)
            }
            Filter::Feed(name) => e.feed.to_lowercase().contains(name.as_str()),
            Filter::Seeders(c, n) => e.seeders.is_some_and(|s| c.ok(s as u64, *n)),
            Filter::Grabs(c, n) => e.grabs.is_some_and(|g| c.ok(g as u64, *n)),
            Filter::Size(c, n) => e.size.is_some_and(|s| c.ok(s, *n)),
            Filter::Free => e.freeleech,
        };
        if !ok {
            return None;
        }
    }
    for n in &q.not {
        if [&h.title, &h.labels, &h.rest].iter().any(|toks| toks.iter().any(|t| t.starts_with(n.as_str()))) {
            return None;
        }
    }
    for p in &q.phrases {
        if !h.phrase.contains(&format!(" {p} ")) && !h.phrase.contains(p.as_str()) {
            return None;
        }
    }
    let mut total = 0.0;
    for w in &q.words {
        // Title counts most, then category/genre/tags, then the description.
        let s = (word_score(w, &h.title) * 3.0).max(word_score(w, &h.labels) * 2.0).max(word_score(w, &h.rest));
        if s == 0.0 {
            return None;
        }
        total += s;
    }
    // Among equal matches, the healthier swarm first.
    Some(total + (e.seeders.unwrap_or(0) as f64 + 1.0).ln() * 0.1)
}

/// Indexes into `entries` that match, best first.
/// `extra(e)` adds text search should see (e.g. the OMDb genre).
pub fn run(entries: &[Entry], keep: impl Fn(&Entry) -> bool, q: &Query, sort: Sort, extra: impl Fn(&Entry) -> String, now: u64) -> Vec<usize> {
    let mut hits: Vec<(usize, f64)> = entries
        .iter()
        .enumerate()
        .filter(|(_, e)| keep(e))
        .filter_map(|(i, e)| score(e, q, &Hay::new(e, &extra(e))).map(|s| (i, s)))
        .collect();
    let sort = if sort == Sort::Best && q.words.is_empty() && q.phrases.is_empty() { Sort::Newest } else { sort };
    let key = |i: usize| &entries[i];
    match sort {
        Sort::Best => hits.sort_by(|a, b| b.1.total_cmp(&a.1)),
        Sort::Seeders => hits.sort_by_key(|&(i, _)| std::cmp::Reverse(key(i).seeders.unwrap_or(0))),
        Sort::Activity => hits.sort_by(|a, b| {
            let act = |i: usize| key(i).activity(now).unwrap_or(-1.0);
            act(b.0).total_cmp(&act(a.0)).then_with(|| key(b.0).seeders.cmp(&key(a.0).seeders))
        }),
        Sort::Newest => hits.sort_by_key(|&(i, _)| std::cmp::Reverse(key(i).first_seen)),
        Sort::Size => hits.sort_by_key(|&(i, _)| std::cmp::Reverse(key(i).size.unwrap_or(0))),
        Sort::Name => hits.sort_by(|a, b| key(a.0).title.to_lowercase().cmp(&key(b.0).title.to_lowercase())),
    }
    hits.into_iter().map(|(i, _)| i).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn e(title: &str, cat: &str, tags: &[&str], seeders: u32, size_mb: u64) -> Entry {
        Entry {
            title: title.into(),
            category: cat.into(),
            tags: tags.iter().map(|s| s.to_string()).collect(),
            seeders: Some(seeders),
            size: Some(size_mb * 1024 * 1024),
            feed: "TorrentLeech".into(),
            ..Default::default()
        }
    }

    fn titles(v: &[Entry], q: &str, sort: Sort) -> Vec<String> {
        run(v, |_| true, &parse(q), sort, |_| String::new(), 0).into_iter().map(|i| v[i].title.clone()).collect()
    }

    fn sample() -> Vec<Entry> {
        vec![
            e("Above.and.Beyond-Group_Therapy_600-WEB-2024", "Music", &["Trance"], 40, 300),
            e("Armin.van.Buuren-A.State.of.Trance.1200", "Music/MP3", &[], 90, 200),
            e("Some.Techno.Mix.2024", "Music", &["Techno"], 5, 150),
            e("Debian 13 netinst", "Apps/Linux", &[], 300, 700),
            e("Trance.Classics.Remix.Pack", "Music", &["Trance"], 2, 2500),
        ]
    }

    #[test]
    fn words_prefix_typo_and_tokenising() {
        let v = sample();
        assert_eq!(titles(&v, "group therapy", Sort::Best), ["Above.and.Beyond-Group_Therapy_600-WEB-2024"]);
        assert_eq!(titles(&v, "deb", Sort::Best), ["Debian 13 netinst"], "prefix");
        assert_eq!(titles(&v, "debain", Sort::Best), ["Debian 13 netinst"], "one typo");
        assert!(titles(&v, "deb13", Sort::Best).is_empty(), "no match is empty, not everything");
    }

    #[test]
    fn genre_search_finds_tags_and_titles_title_ranks_first() {
        let v = sample();
        let got = titles(&v, "trance", Sort::Best);
        assert_eq!(got.len(), 3, "{got:?}");
        assert!(!got.contains(&"Some.Techno.Mix.2024".to_string()));
        // genre: only looks at category/tags (+ OMDb genre), not titles
        let g = titles(&v, "genre:trance", Sort::Seeders);
        assert_eq!(g, ["Above.and.Beyond-Group_Therapy_600-WEB-2024", "Trance.Classics.Remix.Pack"]);
    }

    #[test]
    fn filters_exclusion_phrase_and_sorts() {
        let v = sample();
        assert_eq!(titles(&v, "trance -remix", Sort::Seeders), [
            "Armin.van.Buuren-A.State.of.Trance.1200",
            "Above.and.Beyond-Group_Therapy_600-WEB-2024"
        ]);
        assert_eq!(titles(&v, "cat:music seeders>10", Sort::Seeders), [
            "Armin.van.Buuren-A.State.of.Trance.1200",
            "Above.and.Beyond-Group_Therapy_600-WEB-2024"
        ]);
        assert_eq!(titles(&v, "size>1gb", Sort::Best), ["Trance.Classics.Remix.Pack"]);
        assert_eq!(titles(&v, "\"state of trance\"", Sort::Best), ["Armin.van.Buuren-A.State.of.Trance.1200"]);
        assert_eq!(titles(&v, "feed:torrentleech size<=150mb", Sort::Best), ["Some.Techno.Mix.2024"]);
        assert_eq!(titles(&v, "music", Sort::Size)[0], "Trance.Classics.Remix.Pack");
        assert_eq!(titles(&v, "", Sort::Name)[0], "Above.and.Beyond-Group_Therapy_600-WEB-2024");
    }

    #[test]
    fn free_and_activity() {
        let mut v = sample();
        v[2].freeleech = true;
        assert_eq!(titles(&v, "free", Sort::Best), ["Some.Techno.Mix.2024"]);
        v[0].first_grabs = Some(0);
        v[0].grabs = Some(10);
        v[4].first_grabs = Some(0);
        v[4].grabs = Some(500);
        let got = run(&v, |_| true, &parse("music"), Sort::Activity, |_| String::new(), 3600);
        assert_eq!(v[got[0]].title, "Trance.Classics.Remix.Pack", "500 grabs/h beats 10/h");
    }

    #[test]
    fn extra_text_lets_omdb_genre_match() {
        let v = vec![e("Blade.Runner.2049.2017.1080p", "Movies/HD", &[], 10, 8000)];
        let got = run(&v, |_| true, &parse("genre:sci"), Sort::Best, |_| "Action, Drama, Sci-Fi".into(), 0);
        assert_eq!(got, [0]);
    }

    #[test]
    fn typo_check() {
        assert!(within_one("trance", "trance"));
        assert!(within_one("tarnce", "trance"), "neighbour swap");
        assert!(within_one("trnce", "trance"), "missing letter");
        assert!(within_one("trancee", "trance"));
        assert!(!within_one("techno", "trance"));
    }

    #[test]
    fn query_parsing() {
        let q = parse("Trance \"Group Therapy\" -remix genre:trance seeders>=5 size<2GB free");
        assert_eq!(q.words, ["trance"]);
        assert_eq!(q.phrases, ["group therapy"]);
        assert_eq!(q.not, ["remix"]);
        assert_eq!(q.filters.len(), 4);
        assert!(parse("").is_empty());
        // A half-typed filter is just a word, not an error.
        assert_eq!(parse("genre:").words, ["genre"]);
    }
}
