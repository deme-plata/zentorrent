//! The local model: Ollama on this computer, and getting it there.
//!
//! Everything stays on the machine: ZenTorrent only ever talks to
//! `127.0.0.1:11434`. When Ollama is missing, it is installed the way
//! sigil-top does it: the installer named in a manifest signed with
//! ZenTorrent's release key (`zentorrent-ai-latest.json`), checked against
//! Ollama's own published SHA-256 and exact size while it downloads, and
//! never run on a mismatch. An Ollama that is already installed (sigil-top's,
//! or the user's own) is simply used, with the models it already has.

use std::path::PathBuf;
use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

pub const BASE: &str = "http://127.0.0.1:11434";
const MANIFEST: &str = "zentorrent-ai-latest.json";

fn client(timeout: Duration) -> reqwest::Client {
    reqwest::Client::builder()
        .no_proxy() // loopback only: never through a system or VPN proxy
        .connect_timeout(Duration::from_secs(3))
        .timeout(timeout)
        .build()
        .expect("http client")
}

/// Ollama's error text, when it sends one.
async fn fail(r: reqwest::Response) -> String {
    let code = r.status();
    let body = r.text().await.unwrap_or_default();
    match serde_json::from_str::<Value>(&body).ok().and_then(|v| v["error"].as_str().map(str::to_string)) {
        Some(e) => e,
        None => format!("Ollama answered {code}"),
    }
}

pub async fn reachable() -> bool {
    client(Duration::from_secs(2)).get(format!("{BASE}/api/version")).send().await.is_ok_and(|r| r.status().is_success())
}

#[derive(Clone, Debug, PartialEq)]
pub struct Model {
    pub name: String,
    pub size: u64,
    /// Can call tools (ZenTorrent's skills need that).
    pub tools: bool,
}

/// Families known to call tools, for an Ollama too old to list capabilities.
const TOOL_FAMILIES: &[&str] = &["qwen3", "qwen2.5", "gemma4", "llama3.1", "llama3.2", "llama3.3", "mistral", "granite3", "command-r"];

pub async fn models() -> Result<Vec<Model>, String> {
    let r = client(Duration::from_secs(5)).get(format!("{BASE}/api/tags")).send().await.map_err(|e| e.to_string())?;
    if !r.status().is_success() {
        return Err(fail(r).await);
    }
    let v: Value = r.json().await.map_err(|e| e.to_string())?;
    Ok(v["models"]
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|m| {
                    let name = m["name"].as_str().or(m["model"].as_str())?.to_string();
                    let tools = match m["capabilities"].as_array() {
                        Some(c) => c.iter().any(|c| c == "tools"),
                        None => TOOL_FAMILIES.iter().any(|f| name.starts_with(f)),
                    };
                    Some(Model { size: m["size"].as_u64().unwrap_or(0), tools, name })
                })
                .collect()
        })
        .unwrap_or_default())
}

/// The model to use: the user's pick if it can use tools, else the best one
/// already pulled (Qwen first: it follows tool instructions best), else None.
pub fn choose(models: &[Model], preferred: Option<&str>) -> Option<String> {
    let usable: Vec<&Model> = models.iter().filter(|m| m.tools && !m.name.contains("embed")).collect();
    if let Some(p) = preferred {
        if let Some(m) = usable.iter().find(|m| m.name == p) {
            return Some(m.name.clone());
        }
    }
    // An instruct (non-thinking) Qwen first: measured 2026-10-09, qwen3:4b-instruct called the
    // right tool after 29 tokens where thinking qwen3:4b wrote 340 tokens of monologue first.
    let rank = |n: &str| {
        let fam = ["qwen3", "gemma4", "qwen2.5", "llama3", "mistral"].iter().position(|f| n.starts_with(f)).unwrap_or(9);
        (fam, !n.contains("instruct"))
    };
    usable.iter().min_by_key(|m| (rank(&m.name), std::cmp::Reverse(m.size))).map(|m| m.name.clone())
}

// ---------------------------------------------------------------- chat

/// One answer from the model: its text and the tools it wants run.
#[derive(Debug, Default)]
pub struct Reply {
    pub content: String,
    pub tool_calls: Vec<Value>,
}

/// `<think>…</think>` some models still put in the text itself.
pub fn strip_thinking(s: &str) -> String {
    // Reasoning that arrives without its opening tag (qwen3 with thinking "off",
    // measured 2026-10-09 on Ollama for Windows) ends at a lone </think>.
    let s = match (s.find("<think>"), s.rfind("</think>")) {
        (None, Some(end)) => &s[end + "</think>".len()..],
        _ => s,
    };
    let mut out = String::new();
    let mut rest = s;
    while let Some(a) = rest.find("<think>") {
        out.push_str(&rest[..a]);
        match rest[a..].find("</think>") {
            Some(b) => rest = &rest[a + b + "</think>".len()..],
            None => return out,
        }
    }
    out.push_str(rest);
    out
}

/// Stream one chat turn. `on_text` gets the visible text as it arrives.
///
/// Thinking is switched off: measured 2026-10-09 on qwen3:4b (CPU, loaded box),
/// one "find trance in my feeds" turn spent 8+ minutes writing hidden reasoning
/// before its first tool call. Picking a tool needs no essay. A model that does
/// not know the switch gets the request again without it.
pub async fn chat(base: &str, model: &str, messages: &[Value], tools: &Value, cpu_only: bool, mut on_text: impl FnMut(&str)) -> Result<Reply, String> {
    let mut options = json!({ "num_ctx": 8192, "temperature": 0.3, "num_predict": 1500 });
    if cpu_only {
        options["num_gpu"] = json!(0);
    }
    let mut body = json!({ "model": model, "messages": messages, "tools": tools, "stream": true, "options": options,
                           "keep_alive": "30m", "think": false });
    let send = |body: &Value| {
        client(Duration::from_secs(600)).post(format!("{base}/api/chat")).json(body).send()
    };
    let mut r = send(&body).await.map_err(|e| format!("the local model did not answer: {e}"))?;
    if !r.status().is_success() {
        let e = fail(r).await;
        if !e.to_lowercase().contains("think") {
            return Err(e);
        }
        body.as_object_mut().unwrap().remove("think");
        r = send(&body).await.map_err(|e| format!("the local model did not answer: {e}"))?;
        if !r.status().is_success() {
            return Err(fail(r).await);
        }
    }
    let mut reply = Reply::default();
    let mut buf = Vec::new();
    while let Some(chunk) = r.chunk().await.map_err(|e| format!("the local model stopped: {e}"))? {
        buf.extend_from_slice(&chunk);
        while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=nl).collect();
            let Ok(v) = serde_json::from_slice::<Value>(&line) else { continue };
            if let Some(e) = v["error"].as_str() {
                return Err(e.to_string());
            }
            if let Some(t) = v["message"]["content"].as_str().filter(|t| !t.is_empty()) {
                reply.content.push_str(t);
                on_text(t);
            }
            if let Some(calls) = v["message"]["tool_calls"].as_array() {
                reply.tool_calls.extend(calls.iter().cloned());
            }
        }
    }
    reply.content = strip_thinking(&reply.content).trim().to_string();
    Ok(reply)
}

// ---------------------------------------------------------------- setup

/// Progress of getting the model ready, for the tab.
pub enum Setup {
    Line(String),
    Ready(String),
    Failed(String),
}

#[derive(Deserialize, Clone, Debug)]
pub struct Installer {
    pub url: String,
    pub sha256: String,
    pub size_bytes: u64,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Deserialize, Clone, Debug)]
pub struct AiManifest {
    pub product: String,
    #[serde(default)]
    pub ollama_version: String,
    /// Largest first: the first that fits this machine is pulled, smaller ones on failure.
    pub models: Vec<String>,
    #[serde(default)]
    pub installers: std::collections::BTreeMap<String, Installer>,
}

pub fn parse_manifest(body: &[u8], sig: &str) -> Result<AiManifest, String> {
    crate::update::verify(body, sig)?;
    let m: AiManifest = serde_json::from_slice(body).map_err(|e| format!("{MANIFEST}: {e}"))?;
    if m.product != "zentorrent-ai" {
        return Err(format!("{MANIFEST} is for '{}'", m.product));
    }
    for (k, i) in &m.installers {
        if i.sha256.len() != 64 || !i.sha256.bytes().all(|b| b.is_ascii_hexdigit()) || !i.url.starts_with("https://") {
            return Err(format!("{MANIFEST}: installer {k} is malformed"));
        }
    }
    Ok(m)
}

async fn manifest() -> Result<AiManifest, String> {
    let c = reqwest::Client::builder().timeout(Duration::from_secs(30)).build().map_err(|e| e.to_string())?;
    let get = |name: String| {
        let c = c.clone();
        async move {
            let r = c.get(format!("{}/{name}", crate::update::CHANNEL)).send().await.map_err(|e| e.to_string())?;
            if !r.status().is_success() {
                return Err(format!("{name}: HTTP {}", r.status()));
            }
            r.bytes().await.map(|b| b.to_vec()).map_err(|e| e.to_string())
        }
    };
    let body = get(MANIFEST.into()).await?;
    let sig = get(format!("{MANIFEST}.sig")).await?;
    parse_manifest(&body, &String::from_utf8_lossy(&sig))
}

/// Where ZenTorrent keeps an Ollama it installed itself (Linux), and its log.
fn own_dir() -> PathBuf {
    dirs::data_local_dir().unwrap_or_else(|| PathBuf::from(".")).join("zentorrent").join("ollama")
}

/// An installed Ollama: the usual places, then PATH.
pub fn find_binary() -> Option<PathBuf> {
    let mut c: Vec<PathBuf> = Vec::new();
    if cfg!(windows) {
        if let Some(d) = dirs::data_local_dir() {
            c.push(d.join("Programs").join("Ollama").join("ollama.exe"));
        }
        if let Ok(p) = std::env::var("ProgramFiles") {
            c.push(PathBuf::from(p).join("Ollama").join("ollama.exe"));
        }
    } else {
        c.push(own_dir().join("bin").join("ollama"));
        c.push("/usr/local/bin/ollama".into());
        c.push("/usr/bin/ollama".into());
    }
    let name = if cfg!(windows) { "ollama.exe" } else { "ollama" };
    if let Some(path) = std::env::var_os("PATH") {
        c.extend(std::env::split_paths(&path).map(|d| d.join(name)));
    }
    c.into_iter().find(|p| p.is_file())
}

fn start(bin: &PathBuf) -> Result<(), String> {
    std::fs::create_dir_all(own_dir()).map_err(|e| e.to_string())?;
    let log = std::fs::File::create(own_dir().join("serve.log")).map_err(|e| e.to_string())?;
    let mut cmd = std::process::Command::new(bin);
    cmd.arg("serve").stdin(std::process::Stdio::null()).stdout(log.try_clone().map_err(|e| e.to_string())?).stderr(log);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW
    }
    cmd.spawn().map(|_| ()).map_err(|e| format!("could not start Ollama ({}): {e}", bin.display()))
}

async fn wait_until_up(secs: u64) -> bool {
    for _ in 0..secs * 2 {
        if reachable().await {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    false
}

/// Download `inst` to `dest`, hashing as it streams; refuse any size or SHA-256 mismatch.
async fn download_verified(inst: &Installer, dest: &PathBuf, say: &impl Fn(Setup)) -> Result<(), String> {
    let mut r = reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(15))
        .timeout(Duration::from_secs(4 * 3600))
        .build()
        .map_err(|e| e.to_string())?
        .get(&inst.url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| format!("Ollama download: {e}"))?;
    let tmp = dest.with_extension("part");
    let mut f = std::fs::File::create(&tmp).map_err(|e| format!("{}: {e}", tmp.display()))?;
    let (mut hash, mut got, mut shown) = (Sha256::new(), 0u64, 0u64);
    while let Some(c) = r.chunk().await.map_err(|e| format!("Ollama download stopped: {e}"))? {
        got += c.len() as u64;
        if got > inst.size_bytes {
            let _ = std::fs::remove_file(&tmp);
            return Err("the Ollama download is larger than the signed manifest says — refused".into());
        }
        hash.update(&c);
        std::io::Write::write_all(&mut f, &c).map_err(|e| format!("{}: {e}", tmp.display()))?;
        if got - shown >= 50 << 20 {
            shown = got;
            say(Setup::Line(format!("  downloading Ollama: {} of {} MB", got >> 20, inst.size_bytes >> 20)));
        }
    }
    drop(f);
    let hex = hex::encode(hash.finalize());
    if got != inst.size_bytes || !hex.eq_ignore_ascii_case(&inst.sha256) {
        let _ = std::fs::remove_file(&tmp);
        return Err("the Ollama download does not match the signed manifest — nothing was run".into());
    }
    std::fs::rename(&tmp, dest).map_err(|e| e.to_string())?;
    say(Setup::Line("  ✓ size and SHA-256 match the signed manifest".into()));
    Ok(())
}

async fn install(m: &AiManifest, say: &impl Fn(Setup)) -> Result<PathBuf, String> {
    let key = crate::update::TARGET;
    let inst = m.installers.get(key).ok_or_else(|| format!("no Ollama installer for {key} yet — install it from ollama.com"))?;
    say(Setup::Line(format!("Installing Ollama {} ({} MB, once)…", m.ollama_version, inst.size_bytes >> 20)));
    let dir = own_dir();
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let file = dir.join(inst.url.rsplit('/').next().unwrap_or("ollama-installer"));
    download_verified(inst, &file, say).await?;
    let (file2, args, dir2) = (file.clone(), inst.args.clone(), dir.clone());
    let run = tokio::task::spawn_blocking(move || -> Result<(), String> {
        let status = if cfg!(windows) {
            std::process::Command::new(&file2).args(&args).status()
        } else {
            // Linux: a .tar.zst / .tgz unpacked into ZenTorrent's own folder (no root needed).
            let flag = if file2.to_string_lossy().ends_with(".zst") { "--zstd" } else { "-z" };
            std::process::Command::new("tar").args(["-x", flag, "-f"]).arg(&file2).arg("-C").arg(&dir2).status()
        };
        match status {
            Ok(s) if s.success() => Ok(()),
            Ok(s) => Err(format!("the Ollama installer ended with {s}")),
            Err(e) => Err(format!("could not run the Ollama installer: {e}")),
        }
    });
    run.await.map_err(|e| e.to_string())??;
    let _ = std::fs::remove_file(&file);
    find_binary().ok_or_else(|| "Ollama was installed but its program was not found".into())
}

/// Pull `model`, reporting progress now and then.
pub async fn pull(model: &str, say: &impl Fn(Setup)) -> Result<(), String> {
    let mut r = client(Duration::from_secs(4 * 3600))
        .post(format!("{BASE}/api/pull"))
        .json(&json!({ "model": model, "stream": true }))
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !r.status().is_success() {
        return Err(fail(r).await);
    }
    let (mut buf, mut last) = (Vec::new(), String::new());
    while let Some(c) = r.chunk().await.map_err(|e| e.to_string())? {
        buf.extend_from_slice(&c);
        while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=nl).collect();
            let Ok(v) = serde_json::from_slice::<Value>(&line) else { continue };
            if let Some(e) = v["error"].as_str() {
                return Err(e.to_string());
            }
            let status = v["status"].as_str().unwrap_or("");
            let msg = match (v["completed"].as_u64(), v["total"].as_u64()) {
                (Some(c), Some(t)) if t > 0 => format!("  {model}: {status} {}%", c * 100 / t),
                _ => format!("  {model}: {status}"),
            };
            // One line per 10 % step (or new status), not one per chunk.
            let step = |s: &str| s.rsplit_once(' ').map(|(a, p)| format!("{a} {}", p.trim_end_matches('%').parse::<u64>().map(|n| n / 10).unwrap_or(0))).unwrap_or_default();
            if step(&msg) != step(&last) {
                say(Setup::Line(msg.clone()));
                last = msg;
            }
        }
    }
    Ok(())
}

/// Make the model usable: start or install Ollama, then pick a model or pull one.
pub async fn ensure(preferred: Option<String>, say: impl Fn(Setup)) {
    match ensure_inner(preferred, &say).await {
        Ok(m) => say(Setup::Ready(m)),
        Err(e) => say(Setup::Failed(e)),
    }
}

async fn ensure_inner(preferred: Option<String>, say: &impl Fn(Setup)) -> Result<String, String> {
    let mut manifest_cache: Option<AiManifest> = None;
    if !reachable().await {
        let bin = match find_binary() {
            Some(b) => b,
            None => {
                say(Setup::Line("Ollama is not installed here — getting it (it runs the model on this computer).".into()));
                let m = manifest().await.map_err(|e| format!("could not get the signed AI manifest ({e}); install Ollama from ollama.com and press Retry"))?;
                let b = install(&m, say).await?;
                manifest_cache = Some(m);
                b
            }
        };
        say(Setup::Line("Starting Ollama…".into()));
        start(&bin)?;
        if !wait_until_up(30).await {
            return Err(format!("Ollama did not start; its log is {}", own_dir().join("serve.log").display()));
        }
    }
    let have = models().await?;
    let found = choose(&have, preferred.as_deref());
    // The user's own pick, or an instruct Qwen (the skills are tuned on it), is used as it is.
    // Anything else (a thinking model, a 9 GB gemma on a 6 GB card) is only the fallback:
    // the manifest's model is pulled first.
    if let Some(m) = &found {
        if preferred.as_deref() == Some(m.as_str()) || (m.starts_with("qwen") && m.contains("instruct")) {
            return Ok(m.clone());
        }
    }
    let m = match manifest_cache {
        Some(m) => m,
        None => match manifest().await {
            Ok(m) => m,
            Err(e) => return found.ok_or_else(|| format!("no model that can use tools is installed, and the signed AI manifest could not be read ({e})")),
        },
    };
    let mut last = String::from("no models in the manifest");
    for tag in &m.models {
        say(Setup::Line(format!("Getting the model {tag} (once)…")));
        match pull(tag, say).await {
            Ok(()) => return Ok(tag.clone()),
            Err(e) => {
                say(Setup::Line(format!("  {tag} failed: {e} — trying a smaller one")));
                last = e;
            }
        }
    }
    match found {
        Some(f) => {
            say(Setup::Line(format!("  using {f}, which is already here")));
            Ok(f)
        }
        None => Err(format!("could not get a model: {last}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn thinking_is_not_shown() {
        assert_eq!(strip_thinking("<think>hmm</think>Hello"), "Hello");
        assert_eq!(strip_thinking("A<think>x</think>B<think>y</think>C"), "ABC");
        assert_eq!(strip_thinking("A<think>unfinished"), "A");
        assert_eq!(strip_thinking("Okay, the user wants trance.\n</think>\n\nHere you go."), "\n\nHere you go.");
    }

    #[test]
    fn the_best_tool_model_is_chosen() {
        let m = |n: &str, s, t| Model { name: n.into(), size: s, tools: t };
        let have = vec![m("gemma4:latest", 9, true), m("qwen3:4b-instruct", 3, true), m("qwen3:8b", 5, true), m("llava:7b", 4, false), m("nomic-embed-text", 1, true)];
        assert_eq!(choose(&have, None).as_deref(), Some("qwen3:4b-instruct"), "instruct before a bigger thinking model");
        assert_eq!(choose(&have, Some("gemma4:latest")).as_deref(), Some("gemma4:latest"));
        assert_eq!(choose(&have, Some("llava:7b")).as_deref(), Some("qwen3:4b-instruct"), "a pick that can't use tools is not used");
        assert_eq!(choose(&[m("llava:7b", 4, false)], None), None);
    }

    #[test]
    fn an_unsigned_manifest_is_refused() {
        let body = br#"{"product":"zentorrent-ai","models":["qwen3:4b"],"installers":{}}"#;
        assert!(parse_manifest(body, &"00".repeat(64)).unwrap_err().contains("INVALID"));
    }
}
