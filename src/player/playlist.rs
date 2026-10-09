//! A torrent's files → what to play, in which order.
//!
//! If the torrent carries `.m3u` / `.m3u8` playlists, their order and titles
//! win (several discs' playlists play one after another, in natural order).
//! Otherwise every audio/video file plays in natural path order, so
//! "2 - Intro" comes before "10 - Outro".

use std::cmp::Ordering;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Audio,
    Video,
}

const AUDIO: &[&str] = &["mp3", "flac", "ogg", "oga", "opus", "m4a", "aac", "wav", "wma", "ape", "wv", "aiff", "aif", "alac", "dsf", "dff", "mka", "mpc", "tta"];
const VIDEO: &[&str] = &["mkv", "mp4", "m4v", "avi", "webm", "mov", "ts", "m2ts", "wmv", "mpg", "mpeg", "flv", "ogv", "vob"];

pub fn kind_of(path: &str) -> Option<Kind> {
    let ext = path.rsplit('.').next()?.to_ascii_lowercase();
    if AUDIO.contains(&ext.as_str()) {
        Some(Kind::Audio)
    } else if VIDEO.contains(&ext.as_str()) {
        Some(Kind::Video)
    } else {
        None
    }
}

pub fn is_m3u(path: &str) -> bool {
    let l = path.to_ascii_lowercase();
    l.ends_with(".m3u") || l.ends_with(".m3u8")
}

#[derive(Clone, Debug, PartialEq)]
pub struct Track {
    /// Index of the file inside the torrent.
    pub file: usize,
    /// Path inside the torrent ("Album/01 - Intro.flac").
    pub path: String,
    pub title: String,
    /// Seconds, from #EXTINF (or later from the player).
    pub duration: Option<f64>,
    pub kind: Kind,
    /// Inside an archive: (first volume's file index, member number). Then
    /// `file` is the first volume and `path` is "<archive folder>/<member>".
    pub archive: Option<(usize, usize)>,
    /// The archive's volume files (for download progress); empty otherwise.
    pub parts: Vec<usize>,
}

impl Track {
    pub fn file(file: usize, path: &str, title: String, duration: Option<f64>, kind: Kind) -> Track {
        Track { file, path: path.to_string(), title, duration, kind, archive: None, parts: Vec::new() }
    }
}

/// Scene releases put a short preview in "Sample/" or "…-sample.mkv"; it plays last.
pub fn is_sample(path: &str) -> bool {
    let l = path.to_ascii_lowercase();
    let mut parts = l.split('/');
    let name = parts.next_back().unwrap_or("");
    let stem = name.rsplit_once('.').map(|(s, _)| s).unwrap_or(name);
    parts.any(|d| d == "sample" || d == "samples") || stem == "sample" || stem.ends_with("-sample") || stem.ends_with(".sample") || stem.ends_with("_sample")
}

/// Natural path order, samples last.
pub fn order(tracks: &mut [Track]) {
    tracks.sort_by(|a, b| is_sample(&a.path).cmp(&is_sample(&b.path)).then_with(|| natural_cmp(&a.path, &b.path)));
}

/// "Album/CD1/03 - Song.flac" → "03 - Song".
pub fn title_from_path(path: &str) -> String {
    let name = path.rsplit(['/', '\\']).next().unwrap_or(path);
    match name.rsplit_once('.') {
        Some((stem, _)) if !stem.is_empty() => stem.to_string(),
        _ => name.to_string(),
    }
}

/// Compare paths so embedded numbers sort by value ("2" < "10"), case-insensitively.
pub fn natural_cmp(a: &str, b: &str) -> Ordering {
    let (mut x, mut y) = (a.chars().peekable(), b.chars().peekable());
    loop {
        match (x.peek().copied(), y.peek().copied()) {
            (None, None) => return Ordering::Equal,
            (None, _) => return Ordering::Less,
            (_, None) => return Ordering::Greater,
            (Some(p), Some(q)) if p.is_ascii_digit() && q.is_ascii_digit() => {
                let take = |it: &mut std::iter::Peekable<std::str::Chars>| {
                    let mut s = String::new();
                    while let Some(c) = it.peek().copied().filter(char::is_ascii_digit) {
                        s.push(c);
                        it.next();
                    }
                    s
                };
                let (m, n) = (take(&mut x), take(&mut y));
                let (mt, nt) = (m.trim_start_matches('0'), n.trim_start_matches('0'));
                let o = mt.len().cmp(&nt.len()).then_with(|| mt.cmp(nt)).then_with(|| m.len().cmp(&n.len()));
                if o != Ordering::Equal {
                    return o;
                }
            }
            (Some(p), Some(q)) => {
                let o = p.to_lowercase().cmp(q.to_lowercase());
                if o != Ordering::Equal {
                    return o;
                }
                x.next();
                y.next();
            }
        }
    }
}

/// One entry of an m3u: a path relative to the m3u's folder, plus #EXTINF data.
#[derive(Debug, PartialEq)]
pub struct M3uEntry {
    pub path: String,
    pub title: Option<String>,
    pub duration: Option<f64>,
}

pub fn parse_m3u(text: &str) -> Vec<M3uEntry> {
    let mut out = Vec::new();
    let (mut title, mut duration) = (None, None);
    for line in text.trim_start_matches('\u{feff}').lines() {
        let line = line.trim();
        if let Some(info) = line.strip_prefix("#EXTINF:") {
            // #EXTINF:<seconds>[ attrs],<title>
            let (head, t) = info.split_once(',').unwrap_or((info, ""));
            duration = head.split_whitespace().next().and_then(|d| d.parse::<f64>().ok()).filter(|d| *d > 0.0);
            title = Some(t.trim().to_string()).filter(|t| !t.is_empty());
        } else if line.is_empty() || line.starts_with('#') {
            continue;
        } else if line.contains("://") && !line.starts_with("file://") {
            // An internet stream is not a file in this torrent.
            title = None;
            duration = None;
        } else {
            let p = line.trim_start_matches("file://").replace('\\', "/");
            out.push(M3uEntry { path: p, title: title.take(), duration: duration.take() });
        }
    }
    out
}

/// "Album/CD1" + "../CD2/x.flac" → "Album/CD2/x.flac" (no escaping above the torrent root).
fn join(dir: &str, rel: &str) -> Option<String> {
    let mut parts: Vec<&str> = if rel.starts_with('/') { Vec::new() } else { dir.split('/').filter(|s| !s.is_empty()).collect() };
    for seg in rel.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop()?;
            }
            s => parts.push(s),
        }
    }
    Some(parts.join("/"))
}

/// Build the play order. `files` = (path inside the torrent, index); `m3us` =
/// (path of each playlist file, its text) for the playlists that could be read.
pub fn build(files: &[(String, usize)], m3us: &[(String, String)]) -> Vec<Track> {
    let norm = |p: &str| p.replace('\\', "/").to_lowercase();
    let mut out: Vec<Track> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let mut lists: Vec<&(String, String)> = m3us.iter().collect();
    lists.sort_by(|a, b| natural_cmp(&a.0, &b.0));
    for (m3u_path, text) in lists {
        let dir = m3u_path.replace('\\', "/").rsplit_once('/').map(|(d, _)| d.to_string()).unwrap_or_default();
        for e in parse_m3u(text) {
            let Some(want) = join(&dir, &e.path) else { continue };
            let want = norm(&want);
            // Exact path inside the torrent, else a unique file-name match.
            let hit = files.iter().find(|(p, _)| norm(p) == want).or_else(|| {
                let name = want.rsplit('/').next().unwrap_or(&want);
                let mut same = files.iter().filter(|(p, _)| norm(p).rsplit('/').next() == Some(name));
                match (same.next(), same.next()) {
                    (Some(one), None) => Some(one),
                    _ => None,
                }
            });
            let Some((path, idx)) = hit else { continue };
            let Some(kind) = kind_of(path) else { continue };
            if seen.insert(*idx) {
                out.push(Track::file(*idx, path, e.title.unwrap_or_else(|| title_from_path(path)), e.duration, kind));
            }
        }
    }
    if out.is_empty() {
        out = files
            .iter()
            .filter_map(|(p, i)| Some(Track::file(*i, p, title_from_path(p), None, kind_of(p)?)))
            .collect();
        order(&mut out);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn files(paths: &[&str]) -> Vec<(String, usize)> {
        paths.iter().enumerate().map(|(i, p)| (p.to_string(), i)).collect()
    }

    #[test]
    fn natural_order() {
        let mut v = vec!["Album/10 - Outro.flac", "Album/2 - Two.flac", "Album/1 - One.flac", "Album/02 - Two b.flac"];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(v, ["Album/1 - One.flac", "Album/2 - Two.flac", "Album/02 - Two b.flac", "Album/10 - Outro.flac"]);
        assert_eq!(natural_cmp("cd2/01.flac", "CD10/01.flac"), Ordering::Less);
    }

    #[test]
    fn fallback_plays_media_in_natural_order_and_skips_the_rest() {
        let f = files(&["A/10 - Ten.mp3", "A/cover.jpg", "A/1 - One.mp3", "A/info.nfo", "A/clip.mkv"]);
        let t = build(&f, &[]);
        let got: Vec<_> = t.iter().map(|t| (t.title.as_str(), t.kind)).collect();
        assert_eq!(got, [("1 - One", Kind::Audio), ("10 - Ten", Kind::Audio), ("clip", Kind::Video)]);
    }

    #[test]
    fn m3u_order_titles_and_paths_win() {
        let f = files(&["Live Set/02 - Peak.flac", "Live Set/01 - Warmup.flac", "Live Set/03 - Closing.flac", "Live Set/set.m3u8"]);
        let m3u = "\u{feff}#EXTM3U\r\n#EXTINF:412,Artist - Warm Up (Intro Mix)\r\n01 - Warmup.flac\r\n#EXTINF:600,Artist - Peak Time\r\n.\\02 - Peak.flac\r\nhttp://radio.example/stream\r\n03 - Closing.flac\r\nmissing.flac\r\n";
        let t = build(&f, &[("Live Set/set.m3u8".into(), m3u.into())]);
        assert_eq!(t.len(), 3);
        assert_eq!((t[0].title.as_str(), t[0].duration), ("Artist - Warm Up (Intro Mix)", Some(412.0)));
        assert_eq!((t[1].path.as_str(), t[1].file), ("Live Set/02 - Peak.flac", 0));
        assert_eq!(t[2].title, "03 - Closing", "no #EXTINF: the file name");
    }

    #[test]
    fn several_discs_and_relative_paths() {
        let f = files(&["Box/CD1/a.flac", "Box/CD2/b.flac", "Box/CD1/cd1.m3u", "Box/CD2/cd2.m3u"]);
        let t = build(&f, &[("Box/CD2/cd2.m3u".into(), "b.flac\n".into()), ("Box/CD1/cd1.m3u".into(), "a.flac\n../CD2/b.flac\n".into())]);
        assert_eq!(t.iter().map(|t| t.path.as_str()).collect::<Vec<_>>(), ["Box/CD1/a.flac", "Box/CD2/b.flac"], "disc order, no duplicates");
        assert_eq!(join("A", "../../x"), None, "never above the torrent root");
    }

    #[test]
    fn samples_play_last() {
        let f = files(&["Show/Sample/show-sample.mkv", "Show/show.s01e02.mkv", "Show/show.s01e01.mkv", "Film/film.sample.mkv"]);
        let t = build(&f, &[]);
        let got: Vec<_> = t.iter().map(|t| t.path.as_str()).collect();
        assert_eq!(got, ["Show/show.s01e01.mkv", "Show/show.s01e02.mkv", "Film/film.sample.mkv", "Show/Sample/show-sample.mkv"]);
        assert!(!is_sample("Samples of Life/01 - Intro.flac"), "a folder only counts when it is just 'sample(s)'");
    }

    #[test]
    fn m3u_with_only_names_matches_uniquely() {
        let f = files(&["Rip/Disc/01 Track.mp3", "Rip/list.m3u"]);
        let t = build(&f, &[("Rip/list.m3u".into(), "C:\\Users\\someone\\Music\\01 Track.mp3\n".into())]);
        assert_eq!(t.len(), 1, "a ripper's absolute Windows path still finds the file by name");
    }
}
