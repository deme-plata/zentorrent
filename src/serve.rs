//! `zentorrent serve`: ZenTorrent without a window — for a server, a terminal
//! session, or an AI that drives it over MCP.
//!
//! The same engine as the desktop app (torrent list, VPN kill switch, listener),
//! the RSS feeds read on their schedule with their auto-download rules, and the
//! MCP server on 127.0.0.1. There is no one to press a confirm button here: whoever
//! starts `serve` hands control to the MCP client, so its downloads and moves are
//! carried out directly (moves still only inside folders the client scanned).

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::moe::skills::{self, Action, Library};
use crate::{engine, history, mcp, moe, rss, seed, Source, Transfer};

struct Headless {
    rt: tokio::runtime::Handle,
    session: Arc<librqbit::Session>,
    http: reqwest::Client,
    tunnelled: bool,
    folder: PathBuf,
    transfers: Mutex<Vec<Transfer>>,
    ledger: Mutex<seed::Ledger>,
    history: Mutex<history::History>,
}

impl Headless {
    fn add(&self, label: String, source: Source) -> Result<String, String> {
        let t = self.rt.block_on(engine::add(self.session.clone(), self.http.clone(), self.tunnelled, self.folder.clone(), label, source))?;
        let msg = format!("added: {} → {}", t.name, t.folder.display());
        println!("{msg}");
        self.keep(t);
        Ok(msg)
    }

    fn keep(&self, t: Transfer) {
        let mut ts = self.transfers.lock().unwrap();
        if ts.iter().any(|x| x.handle.id() == t.handle.id()) {
            return;
        }
        let mut ledger = self.ledger.lock().unwrap();
        let e = ledger.entries.entry(t.handle.info_hash().as_string()).or_default();
        e.name = t.name.clone();
        e.folder = t.folder.clone();
        let _ = ledger.save();
        ts.push(t);
    }

    fn handle(&self, hash: &str) -> Option<crate::ManagedTorrentHandle> {
        self.transfers.lock().unwrap().iter().find(|t| t.handle.info_hash().as_string() == hash).map(|t| t.handle.clone())
    }
}

impl mcp::Host for Headless {
    fn library(&self) -> Library {
        let ledger = self.ledger.lock().unwrap();
        let torrents = self
            .transfers
            .lock()
            .unwrap()
            .iter()
            .map(|t| {
                let h = &t.handle;
                let s = h.stats();
                let hash = h.info_hash().as_string();
                let state = if s.error.is_some() {
                    "error"
                } else if h.is_paused() {
                    "paused"
                } else if s.finished {
                    "complete, seeding"
                } else {
                    "downloading"
                };
                skills::Torrent {
                    name: t.name.clone(),
                    state: state.into(),
                    progress: if s.total_bytes > 0 { s.progress_bytes as f64 / s.total_bytes as f64 } else { 0.0 },
                    size: s.total_bytes.max(ledger.entries.get(&hash).map(|e| e.size).unwrap_or(0)),
                    labels: ledger.entries.get(&hash).map(|e| e.labels.clone()).unwrap_or_default(),
                    sites: Vec::new(),
                    files: h
                        .with_metadata(|m| m.file_infos.iter().map(|i| (i.relative_filename.to_string_lossy().replace('\\', "/"), i.len)).collect())
                        .unwrap_or_default(),
                    root: h.output_folder().to_path_buf(),
                    hash,
                }
            })
            .collect();
        Library { folder: self.folder.clone(), torrents, feeds: self.history.lock().unwrap().entries.clone() }
    }

    fn act(&self, a: Action) -> Result<String, String> {
        match a {
            Action::Download { title, link, feed_key } => {
                let store = rss::FeedStore::load();
                let cookie = store
                    .feeds
                    .iter()
                    .find(|f| history::feed_key(&f.url) == feed_key)
                    .map(|f| rss::feed_cookie(&f.cookie, &f.url))
                    .unwrap_or_default();
                self.add(title, Source::from_feed(link, &cookie))
            }
            Action::Moves { moves, .. } => {
                let (n, problems) = moe::organize::apply(&moves);
                let msg = format!("moved {n}{}", if problems.is_empty() { String::new() } else { format!("; problems: {}", problems.join("; ")) });
                println!("{msg}");
                Ok(msg)
            }
            Action::Play { .. } | Action::PlayFiles { .. } => {
                Err("this ZenTorrent runs headless (zentorrent serve): there is no screen to play on".into())
            }
            Action::Picks(_) => Ok(String::new()),
        }
    }

    fn control(&self, c: mcp::Control) -> Result<String, String> {
        match c {
            mcp::Control::Add(link) => {
                let label = link.chars().take(60).collect::<String>();
                self.add(label, Source::from_feed(link, ""))
            }
            mcp::Control::Pause(hash) | mcp::Control::Resume(hash) if self.handle(&hash).is_none() => Err(format!("no torrent {hash}")),
            mcp::Control::Pause(hash) => {
                let h = self.handle(&hash).unwrap();
                self.rt.block_on(self.session.pause(&h)).map_err(|e| format!("{e:#}"))?;
                Ok("paused".into())
            }
            mcp::Control::Resume(hash) => {
                let h = self.handle(&hash).unwrap();
                self.rt.block_on(self.session.unpause(&h)).map_err(|e| format!("{e:#}"))?;
                Ok("resumed".into())
            }
        }
    }

    fn kind(&self) -> &'static str {
        "headless `zentorrent serve` — the operator gave this client control: downloads and moves happen directly"
    }
}

/// Run until Ctrl-C.
pub fn run(rt: tokio::runtime::Runtime, folder: PathBuf, port: Option<u16>) -> Result<(), String> {
    // Everything below is dropped inside the runtime (the engine spawns as it shuts down).
    let _inside = rt.enter();
    let opened = engine::open(&rt, &folder);
    for n in &opened.notes {
        println!("{n}");
    }
    let session = opened.session.ok_or_else(|| opened.error.unwrap_or_else(|| "the torrent engine did not start".into()))?;
    let ledger = seed::Ledger::load();
    let transfers = engine::restore(&session, &ledger, &folder);
    // Feeds and .torrent files go through the tunnel when the VPN is up.
    let http = opened.vpn.http();
    println!(
        "ZenTorrent {} headless · saving to {} · {} torrent(s){}",
        crate::update::VERSION,
        folder.display(),
        transfers.len(),
        if opened.vpn.is_up() { " · through the VPN" } else { "" }
    );
    let host = Arc::new(Headless {
        rt: rt.handle().clone(),
        session: session.clone(),
        http: http.clone(),
        tunnelled: opened.vpn.is_up(),
        folder,
        transfers: Mutex::new(transfers),
        ledger: Mutex::new(ledger),
        history: Mutex::new(history::History::load()),
    });

    let mut cfg = mcp::config();
    if let Some(p) = port {
        cfg.port = p;
    }
    let server = rt.block_on(mcp::serve(host.clone(), &cfg))?;
    let cfg = mcp::Config { port: server.port, ..cfg };
    let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_else(|_| "zentorrent".into());
    println!("MCP server: http://127.0.0.1:{}/mcp  (token in {})", server.port, crate::seed::data_dir().join("mcp.json").display());
    println!("Connect Claude Code:\n{}", mcp::setup_text(&cfg, &exe));
    println!("Ctrl-C stops ZenTorrent.");

    rt.spawn(feeds(host.clone()));
    rt.spawn(status(host.clone()));
    rt.block_on(async {
        let _ = tokio::signal::ctrl_c().await;
    });
    println!("stopping…");
    let _ = host.ledger.lock().unwrap().save();
    let _ = host.history.lock().unwrap().save();
    server.stop();
    // librqbit writes the torrent list as it changes; give the last write a moment.
    rt.block_on(tokio::time::sleep(Duration::from_millis(500)));
    Ok(())
}

/// Read every feed on its schedule: history for search, auto-download rules.
async fn feeds(host: Arc<Headless>) {
    loop {
        let mut store = rss::FeedStore::load();
        for f in store.feeds.iter_mut() {
            match rss::fetch(&host.http, &f.url).await {
                Ok(items) => {
                    let auto: Vec<rss::Item> = f.absorb(&items).into_iter().cloned().collect();
                    host.history.lock().unwrap().absorb(&f.name, &f.url, &items, history::now());
                    let cookie = rss::feed_cookie(&f.cookie, &f.url);
                    for it in auto {
                        println!("auto-download ({}): {}", f.name, it.title);
                        let (h, title, src) = (host.clone(), it.title.clone(), Source::from_feed(it.link.clone(), &cookie));
                        // add() blocks on the runtime: run it off the async workers.
                        let _ = tokio::task::spawn_blocking(move || {
                            if let Err(e) = h.add(title, src) {
                                eprintln!("auto-download failed: {e}");
                            }
                        })
                        .await;
                    }
                }
                Err(e) => eprintln!("feed {}: {e:#}", f.name),
            }
        }
        if let Err(e) = store.save() {
            eprintln!("could not save feeds: {e:#}");
        }
        if let Err(e) = host.history.lock().unwrap().save() {
            eprintln!("could not save the RSS history: {e:#}");
        }
        tokio::time::sleep(Duration::from_secs(store.refresh_minutes.max(1) * 60)).await;
    }
}

/// A line a minute, so a terminal shows it is alive and what it does.
async fn status(host: Arc<Headless>) {
    loop {
        tokio::time::sleep(Duration::from_secs(60)).await;
        let (mut down, mut up, mut active) = (0.0, 0.0, 0);
        let n = {
            let ts = host.transfers.lock().unwrap();
            for t in ts.iter() {
                let s = t.handle.stats();
                if let Some(l) = s.live.as_ref() {
                    down += l.download_speed.mbps;
                    up += l.upload_speed.mbps;
                    active += 1;
                }
            }
            ts.len()
        };
        let _ = host.ledger.lock().unwrap().save();
        println!("{n} torrent(s), {active} active · ↓ {down:.2} MiB/s ↑ {up:.2} MiB/s");
    }
}
