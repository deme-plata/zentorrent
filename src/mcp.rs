//! ZenTorrent as an MCP server: let an AI (Claude Code, sigil-top's Flux MoE, any
//! MCP client) look at and control the torrents.
//!
//! * Transport: MCP "Streamable HTTP" on 127.0.0.1 only (`POST /mcp`, JSON-RPC
//!   2.0, JSON replies), guarded by a random bearer token kept 0600 in the data
//!   folder (`mcp.json`) and by refusing browser origins other than localhost.
//! * `zentorrent mcp` is a stdio bridge to it, for clients that start a command.
//! * Tools: Flux MoE's skills (library, files, feed search / top picks, play,
//!   folders, playlists, moves) plus torrent control (add, pause, resume, status).
//! * Safety is the host's: the desktop app shows downloads and file moves as a
//!   card the user must Apply; headless `zentorrent serve` was started by its
//!   operator to be controlled, and says so in every answer. Feed links (with
//!   their passkeys) never leave the app — results carry ids.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

use crate::moe::skills::{self, Action, Library, Memory};

pub const DEFAULT_PORT: u16 = 47474;
const PROTOCOL: &str = "2025-06-18";

// ---------------------------------------------------------------- config

#[derive(Clone, Debug, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Config {
    pub port: u16,
    pub token: String,
}

fn config_path() -> PathBuf {
    crate::seed::data_dir().join("mcp.json")
}

/// The saved address and token, or new ones (saved 0600).
pub fn config() -> Config {
    if let Some(c) = std::fs::read(config_path()).ok().and_then(|b| serde_json::from_slice::<Config>(&b).ok()).filter(|c| c.token.len() >= 32) {
        return c;
    }
    let mut raw = [0u8; 32];
    // The token only has to be unguessable by other programs on this computer.
    let seed = format!("{:?}{:?}{}", std::time::SystemTime::now(), std::thread::current().id(), std::process::id());
    let a = blake3::hash(seed.as_bytes());
    raw.copy_from_slice(a.as_bytes());
    let c = Config { port: DEFAULT_PORT, token: hex::encode(blake3::hash(&raw).as_bytes()) };
    save(&c);
    c
}

fn save(c: &Config) {
    let p = config_path();
    let _ = std::fs::create_dir_all(crate::seed::data_dir());
    let tmp = p.with_extension("json.tmp");
    if std::fs::write(&tmp, serde_json::to_vec_pretty(c).unwrap_or_default()).is_ok() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600));
        }
        let _ = std::fs::rename(&tmp, &p);
    }
}

/// The commands that connect Claude Code to this server.
pub fn setup_text(c: &Config, exe: &str) -> String {
    format!(
        "claude mcp add --transport http zentorrent http://127.0.0.1:{}/mcp --header \"Authorization: Bearer {}\"\n\
         # or, for clients that start a command:\nclaude mcp add zentorrent -- \"{exe}\" mcp",
        c.port, c.token
    )
}

// ---------------------------------------------------------------- the host

/// Torrent control beyond the Flux MoE skills.
#[derive(Clone, Debug, PartialEq)]
pub enum Control {
    /// A magnet or .torrent URL.
    Add(String),
    /// By info-hash (the name the client gave is resolved first).
    Pause(String),
    Resume(String),
}

/// What ZenTorrent (the window, or headless serve) does for MCP.
pub trait Host: Send + Sync + 'static {
    /// A snapshot of the torrents, feeds and folder.
    fn library(&self) -> Library;
    /// Do or ask: returns what to tell the client.
    fn act(&self, action: Action) -> Result<String, String>;
    fn control(&self, c: Control) -> Result<String, String>;
    /// "desktop app" or "headless serve" — part of the server's self-description.
    fn kind(&self) -> &'static str;
}

/// What an MCP client asked the desktop app for (taken on the UI thread).
#[derive(Clone, Debug)]
pub enum Ask {
    Act(Action),
    Control(Control),
}

/// The desktop app's host: reads a library snapshot the window keeps fresh, and
/// hands every request to the window. Downloads, added torrents and file moves
/// become cards in the Flux MoE tab that the user confirms; play, pause and
/// resume happen right away (nothing is lost by them).
pub struct Relay {
    pub lib: Arc<Mutex<Library>>,
    pub asks: Arc<Mutex<Vec<Ask>>>,
    pub wake: Box<dyn Fn() + Send + Sync>,
}

impl Relay {
    fn push(&self, a: Ask) {
        self.asks.lock().unwrap().push(a);
        (self.wake)();
    }
}

const CONFIRM: &str = "shown to the user in ZenTorrent's Flux MoE tab — it happens when they press the button. Tell them; don't ask again.";

impl Host for Relay {
    fn library(&self) -> Library {
        self.lib.lock().unwrap().clone()
    }
    fn act(&self, a: Action) -> Result<String, String> {
        let m = match &a {
            Action::Play { .. } | Action::PlayFiles { .. } => "playing in ZenTorrent".to_string(),
            _ => CONFIRM.to_string(),
        };
        self.push(Ask::Act(a));
        Ok(m)
    }
    fn control(&self, c: Control) -> Result<String, String> {
        let m = match &c {
            Control::Add(_) => CONFIRM,
            Control::Pause(_) => "paused",
            Control::Resume(_) => "resumed",
        };
        self.push(Ask::Control(c));
        Ok(m.into())
    }
    fn kind(&self) -> &'static str {
        "desktop app — downloads and file moves wait for the user's OK"
    }
}

fn tool(name: &str, desc: &str, props: Value, req: &[&str]) -> Value {
    json!({"name": name, "description": desc, "inputSchema": {"type": "object", "properties": props, "required": req}})
}

/// MCP tool list: the Flux MoE skills (same names and parameters) + control.
pub fn tools() -> Vec<Value> {
    let mut out: Vec<Value> = skills::tools()
        .as_array()
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|t| json!({"name": t["function"]["name"], "description": t["function"]["description"], "inputSchema": t["function"]["parameters"]}))
        .collect();
    out.push(tool("add_torrent", "Add a torrent by magnet link or .torrent URL (in the desktop app the user confirms).",
        json!({"link": {"type": "string", "description": "magnet:?xt=… or https://…/file.torrent"}}), &["link"]));
    out.push(tool("pause", "Pause a torrent (by name or part of it).", json!({"torrent": {"type": "string"}}), &["torrent"]));
    out.push(tool("resume", "Resume a paused torrent (by name or part of it).", json!({"torrent": {"type": "string"}}), &["torrent"]));
    out
}

/// One tool call → MCP result content.
fn call(host: &dyn Host, memory: &Mutex<Memory>, name: &str, args: &Value) -> Value {
    let text = |s: String, error: bool| json!({"content": [{"type": "text", "text": s}], "isError": error});
    let s = |k: &str| args.get(k).and_then(Value::as_str).unwrap_or("").trim().to_string();
    match name {
        "add_torrent" | "pause" | "resume" => {
            let c = match name {
                "add_torrent" => {
                    let link = s("link");
                    if !(link.starts_with("magnet:") || link.starts_with("http://") || link.starts_with("https://")) {
                        return text("link must be a magnet: link or an http(s) URL of a .torrent file".into(), true);
                    }
                    Control::Add(link)
                }
                _ => {
                    let lib = host.library();
                    let t = match skills::find(&lib, &s("torrent")) {
                        Ok(t) => t,
                        Err(e) => return text(e, true),
                    };
                    if name == "pause" { Control::Pause(t.hash.clone()) } else { Control::Resume(t.hash.clone()) }
                }
            };
            match host.control(c) {
                Ok(m) => text(m, false),
                Err(e) => text(e, true),
            }
        }
        _ if skills::tools().as_array().is_some_and(|a| a.iter().any(|t| t["function"]["name"] == name)) => {
            let lib = host.library();
            let out = skills::run(name, args, &lib, &mut memory.lock().unwrap());
            let mut body = out.for_model;
            let mut error = body.get("error").is_some();
            match out.action {
                // Result lists are data here, not a card with buttons: the client downloads by id.
                Some(Action::Picks(_)) => body["note"] = json!("to get one, call download with its id"),
                Some(a) => match host.act(a) {
                    // What ZenTorrent did replaces what the in-app chat would have said.
                    Ok(m) => body["status"] = json!(m),
                    Err(e) => {
                        body = json!({ "error": e });
                        error = true;
                    }
                },
                None => {}
            }
            text(body.to_string(), error)
        }
        other => text(format!("no tool called {other}"), true),
    }
}

/// Handle one JSON-RPC message; None for notifications (no reply).
pub fn handle(host: &dyn Host, memory: &Mutex<Memory>, msg: &Value) -> Option<Value> {
    let id = msg.get("id").cloned()?; // notifications (initialized, cancelled…) get no reply
    let method = msg["method"].as_str().unwrap_or("");
    let ok = |result: Value| json!({"jsonrpc": "2.0", "id": id, "result": result});
    Some(match method {
        "initialize" => {
            let asked = msg["params"]["protocolVersion"].as_str().unwrap_or(PROTOCOL);
            let version = if ["2025-06-18", "2025-03-26", "2024-11-05"].contains(&asked) { asked } else { PROTOCOL };
            ok(json!({
                "protocolVersion": version,
                "capabilities": {"tools": {"listChanged": false}},
                "serverInfo": {"name": "zentorrent", "title": "ZenTorrent", "version": crate::update::VERSION},
                "instructions": format!(
                    "ZenTorrent ({}) — a BitTorrent client with RSS feeds and a media player. Use library for the torrents, \
                     search_feeds (no query + sort=top for the best overall) to find releases, then download by id. \
                     Feed links are never shown; results carry ids.", host.kind())
            }))
        }
        "ping" => ok(json!({})),
        "tools/list" => ok(json!({"tools": tools()})),
        "tools/call" => {
            let name = msg["params"]["name"].as_str().unwrap_or("");
            let args = msg["params"].get("arguments").cloned().unwrap_or(json!({}));
            ok(call(host, memory, name, &args))
        }
        _ => json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32601, "message": format!("method not found: {method}")}}),
    })
}

/// A JSON-RPC body (one message or a batch) → the reply body, or None (202).
pub fn handle_body(host: &dyn Host, memory: &Mutex<Memory>, body: &[u8]) -> Option<Value> {
    let Ok(v) = serde_json::from_slice::<Value>(body) else {
        return Some(json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": "parse error"}}));
    };
    match v {
        Value::Array(batch) => {
            let replies: Vec<Value> = batch.iter().filter_map(|m| handle(host, memory, m)).collect();
            (!replies.is_empty()).then_some(Value::Array(replies))
        }
        one => handle(host, memory, &one),
    }
}

// ---------------------------------------------------------------- HTTP

/// The running server. Dropping it does not stop it; `stop()` does.
pub struct Server {
    pub port: u16,
    task: tokio::task::JoinHandle<()>,
}

impl Server {
    pub fn stop(self) {
        self.task.abort();
    }
}

/// Serve MCP on 127.0.0.1 (the configured port, else the next free ones).
pub async fn serve(host: Arc<dyn Host>, cfg: &Config) -> Result<Server, String> {
    let mut last = String::new();
    for port in cfg.port..cfg.port.saturating_add(10) {
        match tokio::net::TcpListener::bind(("127.0.0.1", port)).await {
            Ok(l) => {
                let token = cfg.token.clone();
                let memory = Arc::new(Mutex::new(Memory::default()));
                let task = tokio::spawn(async move {
                    loop {
                        let Ok((sock, _)) = l.accept().await else { continue };
                        let (host, token, memory) = (host.clone(), token.clone(), memory.clone());
                        tokio::spawn(async move {
                            let _ = tokio::time::timeout(Duration::from_secs(120), connection(sock, host, &token, memory)).await;
                        });
                    }
                });
                // Where it really listens (a busy port, `serve --port`): the stdio
                // bridge and `mcp-config` read it from here.
                let now = Config { port, token: cfg.token.clone() };
                let saved = std::fs::read(config_path()).ok().and_then(|b| serde_json::from_slice::<Config>(&b).ok());
                if saved.as_ref() != Some(&now) && !cfg!(test) {
                    save(&now);
                }
                return Ok(Server { port, task });
            }
            Err(e) => last = e.to_string(),
        }
    }
    Err(format!("no free port from {}: {last}", cfg.port))
}

async fn connection(sock: tokio::net::TcpStream, host: Arc<dyn Host>, token: &str, memory: Arc<Mutex<Memory>>) -> std::io::Result<()> {
    let (r, mut w) = sock.into_split();
    let mut r = BufReader::new(r);
    // One request per connection keeps this small; clients reconnect as they need.
    let mut line = String::new();
    r.read_line(&mut line).await?;
    let mut parts = line.split_whitespace();
    let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    let (mut len, mut auth, mut origin) = (0usize, String::new(), None::<String>);
    loop {
        let mut h = String::new();
        if r.read_line(&mut h).await? == 0 || h == "\r\n" || h == "\n" {
            break;
        }
        if let Some((k, v)) = h.split_once(':') {
            let v = v.trim().to_string();
            match k.trim().to_ascii_lowercase().as_str() {
                "content-length" => len = v.parse().unwrap_or(0),
                "authorization" => auth = v,
                "origin" => origin = Some(v),
                _ => {}
            }
        }
    }
    let reply = |code: &str, body: String| {
        format!("HTTP/1.1 {code}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len())
    };
    let localhost = |o: &str| ["http://127.0.0.1", "http://localhost", "https://127.0.0.1", "https://localhost"].iter().any(|p| o.starts_with(p));
    let out = if path.split('?').next() != Some("/mcp") {
        reply("404 Not Found", r#"{"error":"not found — use /mcp"}"#.into())
    } else if origin.as_deref().is_some_and(|o| !localhost(o)) {
        // A web page in a browser must not drive the torrents (DNS rebinding).
        reply("403 Forbidden", r#"{"error":"origin not allowed"}"#.into())
    } else if auth != format!("Bearer {token}") {
        reply("401 Unauthorized", r#"{"error":"missing or wrong token — see zentorrent mcp.json"}"#.into())
    } else if method != "POST" {
        // No server-to-client stream: every reply comes back on its POST.
        format!("HTTP/1.1 405 Method Not Allowed\r\nAllow: POST\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
    } else if len > 4 << 20 {
        reply("413 Payload Too Large", "{}".into())
    } else {
        let mut body = vec![0u8; len];
        r.read_exact(&mut body).await?;
        let h = host.clone();
        let m = memory.clone();
        let res = tokio::task::spawn_blocking(move || handle_body(h.as_ref(), &m, &body)).await.ok().flatten();
        match res {
            Some(v) => reply("200 OK", v.to_string()),
            None => "HTTP/1.1 202 Accepted\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".to_string(),
        }
    };
    w.write_all(out.as_bytes()).await?;
    w.shutdown().await
}

// ---------------------------------------------------------------- stdio bridge

/// `zentorrent mcp`: newline-delimited JSON-RPC on stdin/stdout, forwarded to the
/// running ZenTorrent (window or `serve`).
pub async fn stdio_bridge() -> Result<(), String> {
    let cfg = std::fs::read(config_path())
        .ok()
        .and_then(|b| serde_json::from_slice::<Config>(&b).ok())
        .ok_or("ZenTorrent's MCP server has not been set up here: start `zentorrent serve`, or switch MCP on in the Flux MoE tab")?;
    let http = reqwest::Client::builder().no_proxy().timeout(Duration::from_secs(600)).build().map_err(|e| e.to_string())?;
    let url = format!("http://127.0.0.1:{}/mcp", cfg.port);
    let mut lines = BufReader::new(tokio::io::stdin()).lines();
    let mut out = tokio::io::stdout();
    while let Some(line) = lines.next_line().await.map_err(|e| e.to_string())? {
        if line.trim().is_empty() {
            continue;
        }
        let r = http.post(&url).bearer_auth(&cfg.token).header("Content-Type", "application/json").body(line.clone()).send().await;
        let reply = match r {
            Ok(r) if r.status().as_u16() == 202 => continue,
            Ok(r) => r.text().await.unwrap_or_default(),
            Err(e) => {
                // ZenTorrent is not running: answer the request with an error instead of hanging.
                let id = serde_json::from_str::<Value>(&line).ok().and_then(|v| v.get("id").cloned());
                match id {
                    Some(id) => json!({"jsonrpc": "2.0", "id": id, "error": {"code": -32000,
                        "message": format!("ZenTorrent is not running on port {} ({e}) — start `zentorrent serve` or the app", cfg.port)}})
                    .to_string(),
                    None => continue,
                }
            }
        };
        out.write_all(reply.trim().as_bytes()).await.map_err(|e| e.to_string())?;
        out.write_all(b"\n").await.map_err(|e| e.to_string())?;
        out.flush().await.map_err(|e| e.to_string())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fake {
        acted: Mutex<Vec<String>>,
    }

    impl Host for Fake {
        fn library(&self) -> Library {
            Library {
                feeds: vec![crate::history::Entry {
                    title: "Trance Classics 2026".into(),
                    link: "https://t.example/dl?passkey=SECRET".into(),
                    in_feed: true,
                    seeders: Some(300),
                    ..Default::default()
                }],
                ..Default::default()
            }
        }
        fn act(&self, a: Action) -> Result<String, String> {
            self.acted.lock().unwrap().push(format!("{a:?}"));
            Ok("shown to the user in ZenTorrent for confirmation".into())
        }
        fn control(&self, c: Control) -> Result<String, String> {
            self.acted.lock().unwrap().push(format!("{c:?}"));
            Ok("done".into())
        }
        fn kind(&self) -> &'static str {
            "test"
        }
    }

    #[test]
    fn mcp_session_end_to_end_over_http() {
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let host = Arc::new(Fake { acted: Mutex::new(Vec::new()) });
        let cfg = Config { port: 47900 + (std::process::id() % 90) as u16, token: "t".repeat(64) };
        rt.block_on(async {
            let srv = serve(host.clone(), &cfg).await.unwrap();
            let url = format!("http://127.0.0.1:{}/mcp", srv.port);
            let c = reqwest::Client::builder().no_proxy().build().unwrap();
            let post = |body: Value, token: &str| c.post(&url).bearer_auth(token).json(&body).send();

            // No token, wrong token, browser origin: refused.
            assert_eq!(c.post(&url).json(&json!({})).send().await.unwrap().status(), 401);
            assert_eq!(post(json!({}), "nope").await.unwrap().status(), 401);
            let r = c.post(&url).bearer_auth(&cfg.token).header("Origin", "https://evil.example").json(&json!({})).send().await.unwrap();
            assert_eq!(r.status(), 403);

            let init: Value = post(json!({"jsonrpc": "2.0", "id": 1, "method": "initialize",
                "params": {"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}}), &cfg.token)
                .await.unwrap().json().await.unwrap();
            assert_eq!(init["result"]["serverInfo"]["name"], "zentorrent");
            assert_eq!(init["result"]["protocolVersion"], "2025-06-18");
            let n = post(json!({"jsonrpc": "2.0", "method": "notifications/initialized"}), &cfg.token).await.unwrap();
            assert_eq!(n.status(), 202);

            let list: Value = post(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list"}), &cfg.token).await.unwrap().json().await.unwrap();
            let names: Vec<&str> = list["result"]["tools"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap()).collect();
            for want in ["library", "search_feeds", "download", "play", "propose_moves", "add_torrent", "pause", "resume"] {
                assert!(names.contains(&want), "{want} missing from {names:?}");
            }

            let found: Value = post(json!({"jsonrpc": "2.0", "id": 3, "method": "tools/call",
                "params": {"name": "search_feeds", "arguments": {"sort": "top"}}}), &cfg.token).await.unwrap().json().await.unwrap();
            let text = found["result"]["content"][0]["text"].as_str().unwrap();
            assert!(text.contains("Trance Classics 2026") && !text.contains("SECRET"), "{text}");

            let dl: Value = post(json!({"jsonrpc": "2.0", "id": 4, "method": "tools/call",
                "params": {"name": "download", "arguments": {"id": 1}}}), &cfg.token).await.unwrap().json().await.unwrap();
            assert!(dl["result"]["content"][0]["text"].as_str().unwrap().contains("confirmation"));
            assert!(host.acted.lock().unwrap().iter().any(|a| a.contains("Download") && a.contains("Trance Classics 2026")));

            let bad: Value = post(json!({"jsonrpc": "2.0", "id": 5, "method": "nope"}), &cfg.token).await.unwrap().json().await.unwrap();
            assert_eq!(bad["error"]["code"], -32601);
            srv.stop();
        });
    }
}
