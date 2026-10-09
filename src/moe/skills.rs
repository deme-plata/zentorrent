//! Flux MoE's skills in ZenTorrent: what the model is told, the tools it may
//! call, and what each tool does.
//!
//! Tools run inside ZenTorrent against a snapshot of its state, so the model
//! never touches the disk or the network itself. Three kinds:
//! * READ (library, files, feed search, folder scan): the answer goes back to the model.
//! * DO (play, playlist): done at once — the user asked for it.
//! * ASK (download, moving files): shown to the user with Apply / Cancel; the
//!   model is told to wait, and nothing happens until the user presses Apply.
//!
//! The model is not trusted with paths or links: feed results are numbers it
//! hands back (a passkey link never enters the conversation), and every path
//! it writes is resolved inside a folder the user's own scan named.

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::organize;
use crate::history;
use crate::player::playlist::{self, kind_of};
use crate::search;

pub const SYSTEM: &str = "You are Flux MoE, the assistant built into ZenTorrent — a BitTorrent client with its own \
music and video player. You run on the user's own computer; nothing leaves it.

You help with the user's media: what they have, finding things in their RSS feeds, playing music and video, making \
playlists, and keeping their folders tidy.

How you work:
- Use the tools for every fact. Never invent a torrent, file, folder, feed item, size or seeder count. If a tool \
finds nothing, say so plainly.
- Reply in the user's language (Danish or English), short and warm. Lists for results; no tables wider than the screen.
- Feed results carry a number (id). To download one, call download with that id — the user confirms in the app.
- To play, call play with the torrent's name (and a track if they asked for one). It starts at once.
- Playlists: take the exact file paths from scan_folder or torrent_files — never make a path up. Then make_playlist.
- Organizing a folder:
  1. scan_folder first.
  2. Choose clear target folders. Music by genre (Trance, Progressive House, House, Techno, Drum & Bass, Dubstep, \
Hardstyle, Ambient, Hip-Hop, Pop, Rock, Metal, Jazz, Classical, Soundtracks, Other); films into Movies, series into \
TV Shows/<Show name>. Use the embedded genre tag first, then what you know about the artist, the label and the \
release name (scene names look like Artist-Title-(CAT001)-WEB-2026-GROUP).
  3. Move whole album or film folders, not single tracks, when a folder is one release. Loose tracks of the same \
album go together into one folder named 'Artist - Album'.
  4. Call propose_moves ONCE with every move, right away — do not ask first: the preview the user sees, with \
Apply and Cancel, IS the question. They press Apply. Never say files were \
moved before they apply; the tool tells you when something was refused, and why.
- Never ask for, show or repeat passwords, passkeys, cookies or links with keys in them.
- If a request is unclear, ask one short question instead of guessing.";

/// The system prompt for this library: the skill, plus where the user's files are.
pub fn system(lib: &Library) -> String {
    format!(
        "{SYSTEM}\n\nThe user's download folder (where ZenTorrent saves, and what \"this folder\" or \"my music\" \
         means unless they name another) is: {}\nscan_folder without a path scans it.",
        lib.folder.display()
    )
}

fn tool(name: &str, desc: &str, props: Value, req: &[&str]) -> Value {
    json!({"type":"function","function":{"name":name,"description":desc,"parameters":{"type":"object","properties":props,"required":req}}})
}

pub fn tools() -> Value {
    json!([
        tool("library", "Overview of the user's torrents in ZenTorrent: name, status, progress, size, labels, tracker \
              sites, and how many music/video files (or archives) each has. Also the download folder.", json!({}), &[]),
        tool("torrent_files", "The files of one torrent (playable ones marked), to play a track or build a playlist.",
            json!({"torrent": {"type":"string","description":"the torrent's name (or part of it)"}}), &["torrent"]),
        tool("search_feeds", "Search the user's RSS feeds and everything the feed history remembers. Words, \"exact phrase\", -exclude. \
              Results have an id for download.",
            json!({"query": {"type":"string","description":"e.g. trance, \"group therapy\" -radio"},
                   "sort": {"type":"string","enum":["best","seeders","activity","newest","size"],"description":"best = relevance"},
                   "limit": {"type":"integer","description":"max results, default 15"}}), &["query"]),
        tool("download", "Ask the user to confirm downloading a feed result (by its id from search_feeds).",
            json!({"id": {"type":"integer"}}), &["id"]),
        tool("play", "Play a torrent's music/video in ZenTorrent's player now (streams what isn't downloaded yet).",
            json!({"torrent": {"type":"string"}, "track": {"type":"string","description":"optional: a track name to start at"}}), &["torrent"]),
        tool("scan_folder", "Summarise a folder: each sub-folder with media (an album, film or season) with its embedded \
              tags, and loose media files. Default: the download folder.",
            json!({"path": {"type":"string","description":"folder to scan; omit for the download folder"}}), &[]),
        tool("make_playlist", "Save a playlist (.m3u8 in the download folder's Playlists folder) from file paths that \
              scan_folder or torrent_files returned, and optionally play it.",
            json!({"name": {"type":"string"},
                   "files": {"type":"array","items":{"type":"string"},"description":"paths as the tools gave them"},
                   "play": {"type":"boolean"}}), &["name", "files"]),
        tool("propose_moves", "Propose moving files/folders into other folders inside the scanned folder. The user sees \
              a preview and must press Apply. Call once with all moves.",
            json!({"moves": {"type":"array","items":{"type":"object","properties":{
                "from":{"type":"string","description":"file or folder, as scan_folder showed it"},
                "to":{"type":"string","description":"destination FOLDER, relative to the scanned folder, e.g. Music/Trance"}},
                "required":["from","to"]}}}), &["moves"]),
    ])
}

/// What the tools see of ZenTorrent, taken when the user sends a message.
#[derive(Clone, Default)]
pub struct Library {
    pub folder: PathBuf,
    pub torrents: Vec<Torrent>,
    /// The feed items (and history) search runs over.
    pub feeds: Vec<history::Entry>,
}

#[derive(Clone, Default)]
pub struct Torrent {
    pub hash: String,
    pub name: String,
    pub state: String,
    pub progress: f64,
    pub size: u64,
    pub labels: Vec<String>,
    pub sites: Vec<String>,
    /// Paths of its files, relative to `root`.
    pub files: Vec<(String, u64)>,
    /// Where its files are on disk (its own folder, or the save folder for one file).
    pub root: PathBuf,
}

/// Things the app does for the model.
#[derive(Clone, Debug, PartialEq)]
pub enum Action {
    Play { hash: String, track: Option<String> },
    PlayFiles { name: String, files: Vec<PathBuf> },
    /// Needs the user's OK.
    Download { title: String, link: String, feed_key: String },
    /// Needs the user's OK.
    Moves { root: PathBuf, moves: Vec<organize::Move> },
}

impl Action {
    pub fn needs_ok(&self) -> bool {
        matches!(self, Action::Download { .. } | Action::Moves { .. })
    }
}

/// State that lives across the turns of one conversation.
#[derive(Default)]
pub struct Memory {
    /// search_feeds ids → feed entries.
    pub results: Vec<history::Entry>,
    /// Folders the user's scans named: moves stay inside them.
    pub roots: Vec<PathBuf>,
}

/// A tool's result: JSON for the model, a short line for the chat, maybe an action.
pub struct Outcome {
    pub for_model: Value,
    pub receipt: String,
    pub action: Option<Action>,
}

fn data(for_model: Value, receipt: impl Into<String>) -> Outcome {
    Outcome { for_model, receipt: receipt.into(), action: None }
}
fn error(msg: impl Into<String>) -> Outcome {
    let m = msg.into();
    Outcome { for_model: json!({ "error": m }), receipt: format!("⚠ {m}"), action: None }
}

fn mb(b: u64) -> u64 {
    b >> 20
}

/// The torrent the model means: exact name, else a unique case-insensitive part of one.
fn find<'a>(lib: &'a Library, what: &str) -> Result<&'a Torrent, String> {
    let w = what.trim().to_lowercase();
    if let Some(t) = lib.torrents.iter().find(|t| t.name.to_lowercase() == w) {
        return Ok(t);
    }
    let hits: Vec<&Torrent> = lib.torrents.iter().filter(|t| t.name.to_lowercase().contains(&w)).collect();
    match hits.as_slice() {
        [one] => Ok(one),
        [] => Err(format!("no torrent called \"{what}\" — call library to see the names")),
        many => Err(format!("\"{what}\" matches {} torrents: {} — which one?", many.len(), many.iter().take(5).map(|t| t.name.as_str()).collect::<Vec<_>>().join("; "))),
    }
}

fn media_counts(files: &[(String, u64)]) -> Value {
    let (mut audio, mut video, mut archives) = (0, 0, 0);
    for (p, _) in files {
        match kind_of(p) {
            Some(playlist::Kind::Audio) => audio += 1,
            Some(playlist::Kind::Video) => video += 1,
            None if crate::player::archive::may_hold_media(p) => archives += 1,
            None => {}
        }
    }
    json!({ "audio": audio, "video": video, "archives": archives })
}

pub fn run(name: &str, args: &Value, lib: &Library, mem: &mut Memory) -> Outcome {
    let s = |k: &str| args.get(k).and_then(Value::as_str).unwrap_or("").trim().to_string();
    match name {
        "library" => {
            let list: Vec<Value> = lib
                .torrents
                .iter()
                .take(200)
                .map(|t| {
                    json!({ "name": t.name, "status": t.state, "progress": format!("{:.0}%", t.progress * 100.0), "mb": mb(t.size),
                            "labels": t.labels, "sites": t.sites, "media": media_counts(&t.files) })
                })
                .collect();
            data(
                json!({ "download_folder": lib.folder.to_string_lossy(), "torrents": list, "count": lib.torrents.len(),
                        "total_mb": mb(lib.torrents.iter().map(|t| t.size).sum()) }),
                format!("looked at your library ({} torrents)", lib.torrents.len()),
            )
        }
        "torrent_files" => match find(lib, &s("torrent")) {
            Err(e) => error(e),
            Ok(t) => {
                let files: Vec<Value> = t
                    .files
                    .iter()
                    .take(400)
                    .map(|(p, len)| {
                        let kind = match kind_of(p) {
                            Some(playlist::Kind::Audio) => "audio",
                            Some(playlist::Kind::Video) => "video",
                            None if crate::player::archive::may_hold_media(p) => "archive",
                            None => "other",
                        };
                        json!({ "path": t.root.join(p).to_string_lossy(), "kind": kind, "mb": mb(*len) })
                    })
                    .collect();
                data(json!({ "torrent": t.name, "files": files }), format!("listed the files of {}", t.name))
            }
        },
        "search_feeds" => {
            let q = s("query");
            let sort = match s("sort").as_str() {
                "seeders" => search::Sort::Seeders,
                "activity" => search::Sort::Activity,
                "newest" => search::Sort::Newest,
                "size" => search::Sort::Size,
                _ => search::Sort::Best,
            };
            let limit = args.get("limit").and_then(Value::as_u64).unwrap_or(15).clamp(1, 40) as usize;
            let now = history::now();
            let hits = search::run(&lib.feeds, |_| true, &search::parse(&q), sort, |_| String::new(), now);
            mem.results.clear();
            let rows: Vec<Value> = hits
                .iter()
                .take(limit)
                .map(|&i| {
                    let e = &lib.feeds[i];
                    mem.results.push(e.clone());
                    json!({ "id": mem.results.len(), "title": e.title, "feed": e.feed, "mb": e.size.map(mb),
                            "seeders": e.seeders, "grabs": e.grabs, "category": e.category, "tags": e.tags,
                            "freeleech": e.freeleech, "still_in_feed": e.in_feed, "seen": history::ago(now.saturating_sub(e.first_seen)) })
                })
                .collect();
            if lib.feeds.is_empty() {
                return error("there are no RSS feeds yet — add one in the RSS feeds tab");
            }
            data(json!({ "query": q, "found": hits.len(), "results": rows }), format!("searched your feeds for “{q}” ({} found)", hits.len()))
        }
        "download" => {
            let id = args.get("id").and_then(Value::as_u64).unwrap_or(0) as usize;
            match id.checked_sub(1).and_then(|i| mem.results.get(i)) {
                None => error(format!("no result {id} — search first")),
                Some(e) => Outcome {
                    for_model: json!({ "status": "shown to the user — they confirm with Download; don't call it again" }),
                    receipt: format!("asks to download {}", e.title),
                    action: Some(Action::Download { title: e.title.clone(), link: e.link.clone(), feed_key: e.feed_key.clone() }),
                },
            }
        }
        "play" => match find(lib, &s("torrent")) {
            Err(e) => error(e),
            Ok(t) => {
                let track = Some(s("track")).filter(|t| !t.is_empty());
                Outcome {
                    for_model: json!({ "status": "playing", "torrent": t.name }),
                    receipt: format!("▶ playing {}", t.name),
                    action: Some(Action::Play { hash: t.hash.clone(), track }),
                }
            }
        },
        "scan_folder" => {
            // Models write "." or "Music" for "this folder" / "a folder in it": those are
            // relative to the download folder, never to wherever ZenTorrent was started.
            let p = s("path").replace('\\', "/");
            let root = match p.trim_end_matches('/') {
                "" | "." => lib.folder.clone(),
                rel if !Path::new(rel).is_absolute() => lib.folder.join(rel.trim_start_matches("./")),
                abs => PathBuf::from(abs),
            };
            if !root.is_dir() {
                return error(format!("{} is not a folder — leave the path out to scan the download folder ({})", root.display(), lib.folder.display()));
            }
            if !mem.roots.contains(&root) {
                mem.roots.push(root.clone());
            }
            let summary = organize::scan(&root);
            let n = summary["media_files"].as_u64().unwrap_or(0);
            data(summary, format!("scanned {} ({n} media files)", root.display()))
        }
        "make_playlist" => {
            let name = s("name").replace(['/', '\\', ':', '*', '?', '"', '<', '>', '|'], " ");
            let name = if name.trim().is_empty() { "Flux MoE playlist".to_string() } else { name.trim().to_string() };
            let mut files = Vec::new();
            let mut skipped = Vec::new();
            for f in args.get("files").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str) {
                let p = resolve(f, mem, lib);
                if p.as_ref().is_some_and(|p| p.is_file() && kind_of(&p.to_string_lossy()).is_some()) {
                    files.push(p.unwrap());
                } else {
                    skipped.push(f.to_string());
                }
            }
            if files.is_empty() {
                return error("none of those files exist — take the paths from scan_folder or torrent_files");
            }
            let dir = lib.folder.join("Playlists");
            let path = dir.join(format!("{name}.m3u8"));
            let mut text = String::from("#EXTM3U\n");
            for f in &files {
                text += &format!("#EXTINF:-1,{}\n{}\n", playlist::title_from_path(&f.to_string_lossy()), f.to_string_lossy());
            }
            if let Err(e) = std::fs::create_dir_all(&dir).and_then(|_| std::fs::write(&path, text)) {
                return error(format!("could not save the playlist: {e}"));
            }
            let play = args.get("play").and_then(Value::as_bool).unwrap_or(false);
            Outcome {
                for_model: json!({ "saved": path.to_string_lossy(), "tracks": files.len(), "skipped": skipped, "playing": play }),
                receipt: format!("saved playlist “{name}” ({} tracks){}", files.len(), if play { " ▶" } else { "" }),
                action: play.then(|| Action::PlayFiles { name, files }),
            }
        }
        "propose_moves" => {
            let Some(root) = mem.roots.last().cloned() else {
                return error("call scan_folder first: moves happen inside the folder you scanned");
            };
            let raw: Vec<(String, String)> = args
                .get("moves")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|m| Some((m.get("from")?.as_str()?.to_string(), m.get("to")?.as_str()?.to_string())))
                .collect();
            let busy: Vec<PathBuf> = lib.torrents.iter().filter(|t| t.state != "removed").flat_map(|t| torrent_paths(t)).collect();
            let (ok, refused) = organize::check(&root, &raw, &busy);
            if ok.is_empty() {
                return Outcome { for_model: json!({ "refused": refused }), receipt: format!("⚠ no moves possible ({} refused)", refused.len()), action: None };
            }
            Outcome {
                for_model: json!({ "status": "shown to the user as a preview — they press Apply. Tell them; don't call again.",
                                   "moves": ok.len(), "refused": refused }),
                receipt: format!("proposes {} moves{}", ok.len(), if refused.is_empty() { String::new() } else { format!(" ({} refused)", refused.len()) }),
                action: Some(Action::Moves { root, moves: ok }),
            }
        }
        other => error(format!("there is no tool called {other}")),
    }
}

/// Where a torrent's files are: its own folder, or its single file.
fn torrent_paths(t: &Torrent) -> Vec<PathBuf> {
    let top: std::collections::BTreeSet<String> = t.files.iter().filter_map(|(p, _)| p.split('/').next().map(str::to_string)).collect();
    top.into_iter().map(|p| t.root.join(p)).collect()
}

/// A path the model wrote: absolute, or relative to the last scanned folder.
fn resolve(p: &str, mem: &Memory, lib: &Library) -> Option<PathBuf> {
    let p = Path::new(p.trim());
    if p.is_absolute() {
        return Some(p.to_path_buf());
    }
    let base = mem.roots.last().unwrap_or(&lib.folder);
    Some(base.join(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lib() -> Library {
        let t = |name: &str, files: &[&str]| Torrent {
            hash: format!("h-{name}"),
            name: name.into(),
            state: "seeding".into(),
            progress: 1.0,
            files: files.iter().map(|f| (f.to_string(), 1 << 20)).collect(),
            root: PathBuf::from("/dl"),
            ..Default::default()
        };
        let entry = |title: &str, seeders| history::Entry {
            title: title.into(),
            link: format!("https://tracker.example/dl/{title}?passkey=SECRET"),
            feed: "Music".into(),
            in_feed: true,
            seeders: Some(seeders),
            ..Default::default()
        };
        Library {
            folder: PathBuf::from("/dl"),
            torrents: vec![t("Armin van Buuren - A State of Trance 1298", &["ASOT/01.mp3"]), t("Some.Show.S01E01", &["Show/show.part1.rar"])],
            feeds: vec![entry("Above and Beyond - Group Therapy 600 (Trance)", 50), entry("Trance Classics 2026", 300), entry("Jazz Night", 9)],
        }
    }

    #[test]
    fn every_tool_definition_is_well_formed() {
        for t in tools().as_array().unwrap() {
            let f = &t["function"];
            assert!(f["name"].as_str().is_some_and(|n| !n.is_empty()));
            assert_eq!(f["parameters"]["type"], "object");
        }
    }

    #[test]
    fn feed_search_hands_out_ids_never_links() {
        let (l, mut mem) = (lib(), Memory::default());
        let o = run("search_feeds", &json!({"query": "trance", "sort": "seeders"}), &l, &mut mem);
        let text = o.for_model.to_string();
        assert!(!text.contains("passkey") && !text.contains("https://"), "{text}");
        assert_eq!(o.for_model["results"][0]["title"], "Trance Classics 2026", "most seeders first");
        let d = run("download", &json!({"id": 1}), &l, &mut mem);
        assert!(matches!(d.action, Some(Action::Download { ref title, .. }) if title == "Trance Classics 2026"));
        assert!(d.action.unwrap().needs_ok());
        assert!(run("download", &json!({"id": 9}), &l, &mut mem).for_model["error"].is_string());
    }

    #[test]
    fn play_finds_torrents_by_part_of_the_name() {
        let (l, mut mem) = (lib(), Memory::default());
        let o = run("play", &json!({"torrent": "state of trance"}), &l, &mut mem);
        assert_eq!(o.action, Some(Action::Play { hash: "h-Armin van Buuren - A State of Trance 1298".into(), track: None }));
        assert!(run("play", &json!({"torrent": "nothing like this"}), &l, &mut mem).for_model["error"].is_string());
        let o = run("library", &json!({}), &l, &mut mem);
        assert_eq!(o.for_model["torrents"][1]["media"]["archives"], 1);
    }

    #[test]
    fn moves_need_a_scan_first() {
        let (l, mut mem) = (lib(), Memory::default());
        let o = run("propose_moves", &json!({"moves": [{"from": "a", "to": "b"}]}), &l, &mut mem);
        assert!(o.for_model["error"].as_str().unwrap().contains("scan_folder"));
    }
}
