//! Claude (Anthropic's API) as Flux MoE's model, instead of the local one.
//!
//! Unlike `ollama`, this leaves the computer: the question, the skill prompt and
//! what the tools return (torrent, file and feed names) go to `api.anthropic.com`
//! over HTTPS, on the user's own API key. So it is never the default and never
//! chosen by itself: only after the user picks a Claude model and gives a key.
//!
//! It speaks Ollama's shapes at its edge (the same messages in, the same `Reply`
//! out), so `agent` stays one loop for both.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::ollama::{Reply, Stats};

pub const BASE: &str = "https://api.anthropic.com";
const VERSION: &str = "2026-10-21";

/// The Claude models the picker offers: (API id, name). Mythos 5.2 is only for
/// organisations Anthropic has verified; Fable 5.2 is the same model for everyone.
pub const MODELS: &[(&str, &str)] = &[("claude-mythos-5-2", "Claude Mythos 5.2"), ("claude-fable-5-2", "Claude Fable 5.2")];

/// Is this model one of Anthropic's (and not an Ollama tag)?
pub fn is(model: &str) -> bool {
    model.starts_with("claude-")
}

pub fn label(model: &str) -> String {
    MODELS.iter().find(|(id, _)| *id == model).map(|(_, name)| name.to_string()).unwrap_or_else(|| model.to_string())
}

// ---------------------------------------------------------------- the key

fn key_path() -> PathBuf {
    crate::seed::data_dir().join("claude-api-key")
}

/// The user's Anthropic API key: `ANTHROPIC_API_KEY`, else the one saved from the tab.
pub fn key() -> Option<String> {
    let clean = |s: String| Some(s.trim().to_string()).filter(|s| !s.is_empty());
    std::env::var("ANTHROPIC_API_KEY").ok().and_then(clean).or_else(|| std::fs::read_to_string(key_path()).ok().and_then(clean))
}

/// Keep the key for next time, readable by this user only. Never logged, never shown again.
pub fn save_key(key: &str) -> Result<(), String> {
    use std::io::Write;
    let path = key_path();
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
    }
    let tmp = path.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut open = std::fs::OpenOptions::new();
    open.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        open.mode(0o600);
    }
    let mut f = open.open(&tmp).map_err(|e| e.to_string())?;
    f.write_all(key.trim().as_bytes()).map_err(|e| e.to_string())?;
    drop(f);
    std::fs::rename(&tmp, &path).map_err(|e| e.to_string())
}

// ---------------------------------------------------------------- the request

/// Add a block to the user turn being built (Anthropic wants one user turn between
/// two assistant turns: tool results first, then any text).
fn push_user(out: &mut Vec<Value>, block: Value) {
    if let Some(last) = out.last_mut().filter(|m| m["role"] == "user") {
        if let Some(blocks) = last["content"].as_array_mut() {
            blocks.push(block);
            return;
        }
    }
    out.push(json!({"role": "user", "content": [block]}));
}

/// A tool call nothing answered (a turn that was cut short): the API refuses a
/// history with one, so it gets an answer that says so.
fn close_open_calls(out: &mut Vec<Value>, open: &mut VecDeque<String>) {
    for id in open.drain(..) {
        push_user(out, json!({"type": "tool_result", "tool_use_id": id, "content": "(no result: the call did not run)"}));
    }
}

/// Flux MoE's messages and tools (Ollama's shapes) as a Messages API request.
///
/// An assistant message Claude wrote this turn carries its own blocks (`claude`:
/// reasoning, text, tool calls with their ids) and those go back exactly as they
/// came — the API checks them. Older ones (and a local model's) have only text
/// and tool calls; their calls get ids made from their place in the list.
pub fn request(model: &str, messages: &[Value], tools: &Value) -> Value {
    let mut system = String::new();
    let mut out: Vec<Value> = Vec::new();
    let mut open: VecDeque<String> = VecDeque::new();
    for (i, m) in messages.iter().enumerate() {
        let text = m["content"].as_str().unwrap_or("");
        match m["role"].as_str().unwrap_or("") {
            "system" => {
                if !system.is_empty() {
                    system.push_str("\n\n");
                }
                system.push_str(text);
            }
            "user" => {
                close_open_calls(&mut out, &mut open);
                if !text.trim().is_empty() {
                    push_user(&mut out, json!({"type": "text", "text": text}));
                }
            }
            "assistant" => {
                close_open_calls(&mut out, &mut open);
                let blocks: Vec<Value> = match m["claude"].as_array().filter(|b| !b.is_empty()) {
                    Some(own) => own.clone(),
                    None => {
                        let mut b = Vec::new();
                        if !text.trim().is_empty() {
                            b.push(json!({"type": "text", "text": text}));
                        }
                        for (j, call) in m["tool_calls"].as_array().into_iter().flatten().enumerate() {
                            let f = &call["function"];
                            let input = match &f["arguments"] {
                                Value::String(s) => serde_json::from_str(s).unwrap_or_else(|_| json!({})),
                                Value::Object(o) => Value::Object(o.clone()),
                                _ => json!({}),
                            };
                            b.push(json!({"type": "tool_use", "id": format!("zt_{i}_{j}"), "name": f["name"].as_str().unwrap_or(""), "input": input}));
                        }
                        b
                    }
                };
                // The kept history can start mid-turn: a conversation starts with the user.
                if blocks.is_empty() || out.is_empty() {
                    continue;
                }
                open = blocks.iter().filter(|b| b["type"] == "tool_use").filter_map(|b| b["id"].as_str().map(String::from)).collect();
                out.push(json!({"role": "assistant", "content": blocks}));
            }
            "tool" => {
                // A result whose call was trimmed away with older history has nothing to answer.
                let Some(id) = open.pop_front() else { continue };
                push_user(&mut out, json!({"type": "tool_result", "tool_use_id": id, "content": if text.is_empty() { "(empty)" } else { text }}));
            }
            _ => {}
        }
    }
    let tools: Vec<Value> = tools
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| {
            let f = &t["function"];
            Some(json!({"name": f["name"].as_str()?, "description": f["description"], "input_schema": f["parameters"]}))
        })
        .collect();
    // Thinking is always on for these models (nothing to set); "medium" effort because
    // picking a tool is routine work. The stable start (tools + skill prompt) is cached
    // between the steps of a turn.
    let mut body = json!({
        "model": model, "max_tokens": 16000, "stream": true, "tools": tools, "messages": out,
        "output_config": {"effort": "medium"}, "cache_control": {"type": "ephemeral"},
    });
    if !system.is_empty() {
        body["system"] = json!(system);
    }
    body
}

// ---------------------------------------------------------------- chat

/// Why Anthropic said no, in words the tab can show.
async fn fail(model: &str, r: reqwest::Response) -> String {
    let code = r.status().as_u16();
    let body = r.text().await.unwrap_or_default();
    let said = serde_json::from_str::<Value>(&body).ok().and_then(|v| v["error"]["message"].as_str().map(str::to_string)).unwrap_or_default();
    match code {
        401 => "Anthropic did not accept the API key — enter it again under \"Anthropic API key…\" in the model picker".into(),
        403 | 404 if model.contains("mythos") => format!(
            "this API key cannot use {} ({said}). Claude Mythos is only for organisations Anthropic has verified — \
             Claude Fable 5.2 in the model picker is the same model for every account",
            label(model)
        ),
        429 => format!("Anthropic's rate limit was reached — wait a little and ask again ({said})"),
        _ if said.is_empty() => format!("Claude answered {code}"),
        _ => format!("Claude answered {code}: {said}"),
    }
}

fn append(block: &mut Value, field: &str, piece: &str) {
    match block.get_mut(field) {
        Some(Value::String(s)) => s.push_str(piece),
        _ => block[field] = json!(piece),
    }
}

/// Stream one chat turn from Claude. `on_text` gets the visible text as it arrives.
pub async fn chat(base: &str, key: &str, model: &str, messages: &[Value], tools: &Value, mut on_text: impl FnMut(&str)) -> Result<Reply, String> {
    let started = Instant::now();
    let client = reqwest::Client::builder().connect_timeout(Duration::from_secs(10)).timeout(Duration::from_secs(600)).build().map_err(|e| e.to_string())?;
    let mut r = client
        .post(format!("{base}/v1/messages"))
        .header("x-api-key", key)
        .header("anthropic-version", VERSION)
        .json(&request(model, messages, tools))
        .send()
        .await
        .map_err(|e| format!("Claude did not answer: {}", e.without_url()))?;
    if !r.status().is_success() {
        return Err(fail(model, r).await);
    }
    // The answer's blocks as they fill (reasoning, text, tool calls), and each tool
    // call's arguments, which arrive as pieces of JSON text.
    let mut blocks: Vec<Value> = Vec::new();
    let mut args: Vec<String> = Vec::new();
    let mut stop = String::new();
    let mut declined: Option<String> = None;
    let mut stats = Stats::default();
    let mut first_text: Option<Instant> = None;
    let mut buf = Vec::new();
    while let Some(chunk) = r.chunk().await.map_err(|e| format!("Claude stopped: {}", e.without_url()))? {
        buf.extend_from_slice(&chunk);
        while let Some(nl) = buf.iter().position(|&b| b == b'\n') {
            let line: Vec<u8> = buf.drain(..=nl).collect();
            let Some(data) = line.strip_prefix(b"data:") else { continue };
            let Ok(v) = serde_json::from_slice::<Value>(data) else { continue };
            let i = v["index"].as_u64().unwrap_or(0) as usize;
            match v["type"].as_str().unwrap_or("") {
                "message_start" => {
                    let u = &v["message"]["usage"];
                    stats.prompt_count = ["input_tokens", "cache_read_input_tokens", "cache_creation_input_tokens"].iter().map(|k| u[*k].as_u64().unwrap_or(0)).sum();
                }
                "content_block_start" => {
                    while blocks.len() <= i {
                        blocks.push(Value::Null);
                        args.push(String::new());
                    }
                    blocks[i] = v["content_block"].clone();
                }
                "content_block_delta" => {
                    let Some(b) = blocks.get_mut(i).filter(|b| b.is_object()) else { continue };
                    let d = &v["delta"];
                    match d["type"].as_str().unwrap_or("") {
                        "text_delta" => {
                            let t = d["text"].as_str().unwrap_or("");
                            append(b, "text", t);
                            if !t.is_empty() {
                                first_text.get_or_insert_with(Instant::now);
                                on_text(t);
                            }
                        }
                        "input_json_delta" => args[i].push_str(d["partial_json"].as_str().unwrap_or("")),
                        "thinking_delta" => append(b, "thinking", d["thinking"].as_str().unwrap_or("")),
                        "signature_delta" => append(b, "signature", d["signature"].as_str().unwrap_or("")),
                        _ => {}
                    }
                }
                "content_block_stop" => {
                    if blocks.get(i).is_some_and(|b| b["type"] == "tool_use") {
                        // Cut off before its end (the token limit): the same words Ollama uses,
                        // so `agent` asks once for a shorter call.
                        blocks[i]["input"] = match args[i].trim() {
                            "" => json!({}),
                            a => serde_json::from_str(a).map_err(|_| "invalid tool call arguments: Claude's tool call was cut off".to_string())?,
                        };
                    }
                }
                "message_delta" => {
                    if let Some(s) = v["delta"]["stop_reason"].as_str() {
                        stop = s.to_string();
                    }
                    if let Some(c) = v["delta"]["stop_details"]["category"].as_str().or(v["stop_details"]["category"].as_str()) {
                        declined = Some(c.to_string());
                    }
                    if let Some(n) = v["usage"]["output_tokens"].as_u64() {
                        stats.eval_count = n;
                    }
                }
                "error" => return Err(format!("Claude: {}", v["error"]["message"].as_str().unwrap_or("the answer stopped with an error"))),
                _ => {}
            }
        }
    }
    // A decline is an ordinary 200 answer: look at why it stopped before reading it.
    if stop == "refusal" {
        return Err(match declined {
            Some(c) => format!("Claude declined to answer this (its safety check: {c}). Nothing was done."),
            None => "Claude declined to answer this. Nothing was done.".into(),
        });
    }
    blocks.retain(|b| b.is_object());
    if stop.is_empty() && blocks.is_empty() {
        return Err("Claude's answer ended before it began".into());
    }
    let content: String = blocks.iter().filter(|b| b["type"] == "text").filter_map(|b| b["text"].as_str()).collect::<Vec<_>>().join("");
    let tool_calls = blocks.iter().filter(|b| b["type"] == "tool_use").map(|b| json!({"function": {"name": b["name"], "arguments": b["input"]}})).collect();
    let end = Instant::now();
    let wrote_from = first_text.unwrap_or(started);
    stats.prompt_ns = wrote_from.duration_since(started).as_nanos() as u64;
    stats.eval_ns = end.duration_since(wrote_from).as_nanos().max(1) as u64;
    stats.total_ns = end.duration_since(started).as_nanos() as u64;
    Ok(Reply { content: content.trim().to_string(), tool_calls, stats, looped: false, raw: Some(Value::Array(blocks)) })
}

/// A stand-in for api.anthropic.com in tests: each request gets the next canned
/// event stream; the requests' headers (lower case) and bodies are returned.
#[cfg(test)]
pub mod testing {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};

    pub fn sse(events: &[Value]) -> String {
        events.iter().map(|e| format!("event: {}\ndata: {e}\n\n", e["type"].as_str().unwrap_or(""))).collect()
    }

    pub fn fake(replies: Vec<(u16, String)>) -> (String, std::thread::JoinHandle<Vec<(String, Value)>>) {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        let h = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for (code, out) in replies {
                let (mut s, _) = l.accept().unwrap();
                let mut r = BufReader::new(s.try_clone().unwrap());
                let (mut len, mut head) = (0, String::new());
                loop {
                    let mut h = String::new();
                    r.read_line(&mut h).unwrap();
                    let low = h.to_ascii_lowercase();
                    if let Some(v) = low.strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap();
                    }
                    if h == "\r\n" {
                        break;
                    }
                    head.push_str(&low);
                }
                let mut body = vec![0; len];
                r.read_exact(&mut body).unwrap();
                seen.push((head, serde_json::from_slice(&body).unwrap()));
                let kind = if code == 200 { "text/event-stream" } else { "application/json" };
                write!(s, "HTTP/1.1 {code} X\r\nContent-Type: {kind}\r\nConnection: close\r\n\r\n{out}").unwrap();
            }
            seen
        });
        (base, h)
    }

    /// One answer: optional reasoning, text, then tool calls (name, arguments).
    pub fn answer(thinking: Option<&str>, text: &str, calls: &[(&str, Value)], stop: &str) -> String {
        let mut ev = vec![json!({"type": "message_start", "message": {"usage": {"input_tokens": 12, "cache_read_input_tokens": 2100}}})];
        let mut i = 0;
        if let Some(t) = thinking {
            ev.push(json!({"type": "content_block_start", "index": i, "content_block": {"type": "thinking", "thinking": "", "signature": ""}}));
            ev.push(json!({"type": "content_block_delta", "index": i, "delta": {"type": "thinking_delta", "thinking": t}}));
            ev.push(json!({"type": "content_block_delta", "index": i, "delta": {"type": "signature_delta", "signature": "sig-abc"}}));
            ev.push(json!({"type": "content_block_stop", "index": i}));
            i += 1;
        }
        if !text.is_empty() {
            let (a, b) = text.split_at(text.len() / 2);
            ev.push(json!({"type": "content_block_start", "index": i, "content_block": {"type": "text", "text": ""}}));
            ev.push(json!({"type": "content_block_delta", "index": i, "delta": {"type": "text_delta", "text": a}}));
            ev.push(json!({"type": "ping"}));
            ev.push(json!({"type": "content_block_delta", "index": i, "delta": {"type": "text_delta", "text": b}}));
            ev.push(json!({"type": "content_block_stop", "index": i}));
            i += 1;
        }
        for (n, (name, input)) in calls.iter().enumerate() {
            let s = input.to_string();
            let (a, b) = s.split_at(s.len() / 2);
            ev.push(json!({"type": "content_block_start", "index": i, "content_block": {"type": "tool_use", "id": format!("toolu_{n}"), "name": name, "input": {}}}));
            ev.push(json!({"type": "content_block_delta", "index": i, "delta": {"type": "input_json_delta", "partial_json": a}}));
            ev.push(json!({"type": "content_block_delta", "index": i, "delta": {"type": "input_json_delta", "partial_json": b}}));
            ev.push(json!({"type": "content_block_stop", "index": i}));
            i += 1;
        }
        ev.push(json!({"type": "message_delta", "delta": {"stop_reason": stop}, "usage": {"output_tokens": 41}}));
        ev.push(json!({"type": "message_stop"}));
        sse(&ev)
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    fn rt() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
    }

    fn tools() -> Value {
        json!([{"type": "function", "function": {"name": "search_feeds", "description": "Search the feeds.",
                "parameters": {"type": "object", "properties": {"query": {"type": "string"}}, "required": []}}}])
    }

    #[test]
    fn ollama_shaped_history_becomes_a_messages_request() {
        let own = json!([{"type": "thinking", "thinking": "", "signature": "sig-1"}, {"type": "tool_use", "id": "toolu_9", "name": "library", "input": {}}]);
        let msgs = vec![
            json!({"role": "system", "content": "You are Flux MoE."}),
            // Left over from trimmed history: a result and an answer with nothing before them.
            json!({"role": "tool", "tool_name": "library", "content": "orphan"}),
            json!({"role": "assistant", "content": "an answer to a question that was trimmed away"}),
            json!({"role": "user", "content": "find trance"}),
            json!({"role": "assistant", "content": "", "tool_calls": [
                {"function": {"name": "search_feeds", "arguments": {"query": "trance"}}},
                {"function": {"name": "search_feeds", "arguments": "{\"query\": \"goa\"}"}}]}),
            json!({"role": "tool", "tool_name": "search_feeds", "content": "3 results"}),
            json!({"role": "tool", "tool_name": "search_feeds", "content": ""}),
            json!({"role": "user", "content": "[ZenTorrent] really do it"}),
            json!({"role": "assistant", "content": "", "tool_calls": [{"function": {"name": "library", "arguments": {}}}], "claude": own}),
            json!({"role": "tool", "tool_name": "library", "content": "12 torrents"}),
        ];
        let r = request("claude-mythos-5-2", &msgs, &tools());
        assert_eq!(r["model"], "claude-mythos-5-2");
        assert_eq!(r["system"], "You are Flux MoE.");
        assert_eq!(r["stream"], true);
        assert!(r.get("thinking").is_none() && r.get("temperature").is_none() && r.get("think").is_none(), "nothing these models refuse");
        assert_eq!(r["tools"][0], json!({"name": "search_feeds", "description": "Search the feeds.",
            "input_schema": {"type": "object", "properties": {"query": {"type": "string"}}, "required": []}}));
        let m = r["messages"].as_array().unwrap();
        assert_eq!(m.len(), 5, "{m:#?}");
        assert_eq!(m[4]["content"], json!([{"type": "tool_result", "tool_use_id": "toolu_9", "content": "12 torrents"}]));
        assert_eq!(m[0], json!({"role": "user", "content": [{"type": "text", "text": "find trance"}]}), "starts with the user; orphans are gone");
        assert_eq!(m[1]["content"], json!([
            {"type": "tool_use", "id": "zt_4_0", "name": "search_feeds", "input": {"query": "trance"}},
            {"type": "tool_use", "id": "zt_4_1", "name": "search_feeds", "input": {"query": "goa"}}]));
        // Both results and the note after them are one user turn, results first, in the calls' order.
        assert_eq!(m[2], json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "zt_4_0", "content": "3 results"},
            {"type": "tool_result", "tool_use_id": "zt_4_1", "content": "(empty)"},
            {"type": "text", "text": "[ZenTorrent] really do it"}]}));
        // Claude's own blocks go back untouched, and its call id is the one answered.
        assert_eq!(m[3]["content"], own);
        let again = request("claude-mythos-5-2", &msgs[..msgs.len() - 1], &tools());
        assert_eq!(again["messages"].as_array().unwrap()[..3], m[..3], "the same history gives the same start (ids too)");
        let with_result = request("claude-mythos-5-2", &[msgs.clone(), vec![json!({"role": "user", "content": "thanks"})]].concat(), &tools());
        assert_eq!(with_result["messages"][4], json!({"role": "user", "content": [
            {"type": "tool_result", "tool_use_id": "toolu_9", "content": "12 torrents"}, {"type": "text", "text": "thanks"}]}));
    }

    #[test]
    fn a_call_nobody_answered_is_closed() {
        let msgs = vec![
            json!({"role": "user", "content": "play something"}),
            json!({"role": "assistant", "content": "On it.", "tool_calls": [{"function": {"name": "library", "arguments": {}}}]}),
            json!({"role": "user", "content": "never mind"}),
        ];
        let r = request("claude-fable-5-2", &msgs, &json!([]));
        assert_eq!(r["messages"][2]["content"][0]["type"], "tool_result");
        assert_eq!(r["messages"][2]["content"][0]["tool_use_id"], "zt_1_0");
        assert_eq!(r["messages"][2]["content"][1]["text"], "never mind");
        assert!(r.get("system").is_none());
    }

    #[test]
    fn a_streamed_answer_becomes_a_reply() {
        let (base, server) = fake(vec![(200, answer(Some("They want trance."), "Looking in your feeds.", &[("search_feeds", json!({"query": "trance", "sort": "seeders"}))], "tool_use"))]);
        let mut seen = String::new();
        let msgs = vec![json!({"role": "system", "content": "skill"}), json!({"role": "user", "content": "find trance"})];
        let reply = rt().block_on(chat(&base, "sk-test-1", "claude-mythos-5-2", &msgs, &tools(), |t| seen.push_str(t))).unwrap();
        let (head, body) = server.join().unwrap().remove(0);
        assert!(head.contains("x-api-key: sk-test-1") && head.contains("anthropic-version: 2026-10-21"), "{head}");
        assert_eq!(body["model"], "claude-mythos-5-2");
        assert_eq!(seen, "Looking in your feeds.", "the text streams; the reasoning does not");
        assert_eq!(reply.content, "Looking in your feeds.");
        assert_eq!(reply.tool_calls, vec![json!({"function": {"name": "search_feeds", "arguments": {"query": "trance", "sort": "seeders"}}})]);
        assert_eq!((reply.stats.prompt_count, reply.stats.eval_count), (2112, 41));
        assert_eq!(reply.raw.unwrap(), json!([
            {"type": "thinking", "thinking": "They want trance.", "signature": "sig-abc"},
            {"type": "text", "text": "Looking in your feeds."},
            {"type": "tool_use", "id": "toolu_0", "name": "search_feeds", "input": {"query": "trance", "sort": "seeders"}}]));
    }

    #[test]
    fn a_decline_a_bad_key_and_no_mythos_access_are_said_plainly() {
        let declined = sse(&[
            json!({"type": "message_start", "message": {"usage": {"input_tokens": 5}}}),
            json!({"type": "message_delta", "delta": {"stop_reason": "refusal", "stop_details": {"type": "refusal", "category": "cyber"}}, "usage": {"output_tokens": 0}}),
            json!({"type": "message_stop"}),
        ]);
        let cut = sse(&[
            json!({"type": "content_block_start", "index": 0, "content_block": {"type": "tool_use", "id": "toolu_0", "name": "make_playlist", "input": {}}}),
            json!({"type": "content_block_delta", "index": 0, "delta": {"type": "input_json_delta", "partial_json": "{\"files\": [1, 2,"}}),
            json!({"type": "content_block_stop", "index": 0}),
            json!({"type": "message_delta", "delta": {"stop_reason": "max_tokens"}, "usage": {"output_tokens": 16000}}),
        ]);
        let (base, server) = fake(vec![
            (200, declined),
            (401, json!({"type": "error", "error": {"type": "authentication_error", "message": "invalid x-api-key"}}).to_string()),
            (404, json!({"type": "error", "error": {"type": "not_found_error", "message": "model: claude-mythos-5-2"}}).to_string()),
            (200, cut),
            (200, sse(&[json!({"type": "error", "error": {"type": "overloaded_error", "message": "Overloaded"}})])),
        ]);
        let msgs = vec![json!({"role": "user", "content": "hi"})];
        let rt = rt();
        let ask = || rt.block_on(chat(&base, "k", "claude-mythos-5-2", &msgs, &json!([]), |_| {})).unwrap_err();
        let e = ask();
        assert!(e.contains("declined") && e.contains("cyber"), "{e}");
        let e = ask();
        assert!(e.contains("did not accept the API key") && !e.contains("x-api-key"), "{e}");
        let e = ask();
        assert!(e.contains("verified") && e.contains("Claude Fable 5.2"), "{e}");
        let e = ask();
        assert!(e.contains("invalid tool call arguments"), "{e}");
        assert_eq!(ask(), "Claude: Overloaded");
        server.join().unwrap();
    }
}
