//! ✨ Flux MoE — the assistant tab, next to RSS feeds.
//!
//! A local model (Ollama, set up by itself on first use: see `ollama`) with
//! ZenTorrent's own skills (`skills`): an overview of the library, searching
//! the feeds, playing, playlists, and organizing folders (`organize`, always
//! with a preview the user applies, and an Undo).
//!
//! * `ollama`   — the local model: start/install, pick a model, streamed chat
//! * `skills`   — the system prompt, the tools, and what each tool does
//! * `organize` — folder summaries, checked moves, the undo journal
//! * `gauge`    — live GPU / VRAM / CPU load while an answer is generated

pub mod gauge;
pub mod ollama;
pub mod organize;
pub mod skills;

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use eframe::egui::{self, Color32, RichText};
use serde_json::{json, Value};

use skills::{Action, Library, Memory, Pick};

/// Model steps per message (tool call → answer → tool call …).
const MAX_STEPS: usize = 6;
/// Earlier messages kept for the model.
const KEEP: usize = 24;

enum Line {
    User(String),
    Moe(String),
    Tool(String),
    Note(String),
    Error(String),
    /// How an answer went: tokens, speed, where the model ran.
    Stats(String),
    /// A proposal waiting for the user (index into `Moe::cards`).
    Card(usize),
    /// Feed results with a Download button each (index into `Moe::picks`).
    Picks(usize),
    /// Commands to copy (how to connect an MCP client).
    Setup(String),
}

enum CardState {
    Waiting,
    Done(String),
    Cancelled,
}

struct Card {
    action: Action,
    state: CardState,
}

/// What the background tasks tell the tab.
enum Ev {
    Setup(ollama::Setup),
    /// The models that can use tools, for the picker.
    Models(Vec<String>),
    /// Bigger models from the signed list that could be fetched: (tag, GB).
    Offers(Vec<(String, f64)>),
    /// A model fetched from the picker is ready.
    Pulled(String),
    /// Ollama is not installed; wait for the user's click before downloading it.
    NeedsSetup,
    Text(String),
    /// Ollama's numbers for one model step.
    Stats(ollama::Stats),
    /// How much of the model is in graphics memory: (total, in VRAM) bytes.
    Residency(u64, u64),
    Tool(String),
    Action(Action),
    /// The turn is over: the messages to remember (assistant + tool), or an error.
    Done(Result<Vec<Value>, String>),
}

#[derive(PartialEq)]
enum Status {
    Unknown,
    /// No Ollama here yet: one click sets it up (a few GB, so not without asking).
    NeedsSetup,
    Preparing,
    Ready(String),
    Failed(String),
}

#[derive(serde::Serialize, serde::Deserialize, Default)]
struct Settings {
    #[serde(default)]
    model: Option<String>,
    #[serde(default)]
    cpu_only: bool,
    /// Serve ZenTorrent's tools over MCP (127.0.0.1, with a token).
    #[serde(default)]
    mcp: bool,
}

fn settings_path() -> std::path::PathBuf {
    crate::seed::data_dir().join("flux-moe.json")
}

/// One answer being generated: what the progress line shows. Dropping it stops
/// the load sampler and the residency poll.
struct Run {
    model: String,
    started: Instant,
    first_text: Option<Instant>,
    /// Streamed text pieces (about one token each) — live, before Ollama's own count.
    pieces: u64,
    stats: ollama::Stats,
    load: Arc<Mutex<gauge::Load>>,
    residency: Option<(u64, u64)>,
    /// Rough size of what the model reads first (≈ 4 characters a token).
    prompt_tokens: usize,
    stop: Arc<AtomicBool>,
}

impl Drop for Run {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn gb(b: u64) -> f64 {
    b as f64 / (1u64 << 30) as f64
}

/// "3.5 GB, all on the GPU" / "62 % on the GPU, the rest on the CPU" / "on the CPU".
fn placement(total: u64, vram: u64) -> String {
    let pct = if total == 0 { 0 } else { (vram * 100 / total) as u32 };
    match pct {
        99.. => format!("model {:.1} GB, all on the GPU", gb(total)),
        0 => format!("model {:.1} GB, on the CPU", gb(total)),
        p => format!("model {:.1} GB, {p} % on the GPU (the rest on the CPU — slower)", gb(total)),
    }
}

/// The line under an answer.
fn summary(model: &str, s: &ollama::Stats, wall_s: f64, residency: Option<(u64, u64)>) -> String {
    let mut out = format!("{model} · {} tokens · {:.1} tok/s", s.eval_count, s.rate());
    if s.prompt_count > 0 {
        out += &format!(" · read {} tokens in {:.1} s", s.prompt_count, s.prompt_ns as f64 / 1e9);
    }
    if s.load_ns > 500_000_000 {
        out += &format!(" · model loaded in {:.1} s", s.load_ns as f64 / 1e9);
    }
    out += &format!(" · {wall_s:.1} s in all");
    if let Some((t, v)) = residency {
        out += &format!(" · {}", placement(t, v));
    }
    out
}

pub struct Moe {
    status: Status,
    lines: Vec<Line>,
    /// The visible text of the answer being written.
    live: String,
    input: String,
    busy: bool,
    history: Vec<Value>,
    cards: Vec<Card>,
    /// Result lists shown in the chat; `bool` = that row was downloaded.
    picks: Vec<Vec<(Pick, bool)>>,
    events: Arc<Mutex<Vec<Ev>>>,
    memory: Arc<Mutex<Memory>>,
    settings: Settings,
    models: Vec<String>,
    offers: Vec<(String, f64)>,
    /// The picker's "Get …" panel: which model, and whether it fits here.
    offer: Option<(String, ollama::Fit)>,
    can_undo: bool,
    /// A message typed this frame, waiting for the app to take a library snapshot.
    pending: Option<String>,
    /// The running answer, so Stop can end it (and the generation in Ollama with it).
    task: Option<tokio::task::JoinHandle<()>>,
    run: Option<Run>,
    /// Typical answer length in tokens (for the progress bar's estimate).
    avg_tokens: f64,
}

impl Default for Moe {
    fn default() -> Self {
        let settings = std::fs::read(settings_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
        Moe {
            status: Status::Unknown,
            lines: Vec::new(),
            live: String::new(),
            input: String::new(),
            busy: false,
            history: Vec::new(),
            cards: Vec::new(),
            picks: Vec::new(),
            events: Default::default(),
            memory: Default::default(),
            settings,
            models: Vec::new(),
            offers: Vec::new(),
            offer: None,
            can_undo: organize::can_undo(),
            pending: None,
            task: None,
            run: None,
            avg_tokens: 0.0,
        }
    }
}

const STARTERS: &[(&str, &str)] = &[
    ("Overview of my library", "Give me an overview of my library: what I have, what is still downloading, and what is playable."),
    ("Top picks from my feeds", "Show me the top picks from my RSS feeds and their history: a varied mix of the best torrents, not one genre."),
    ("Find trance in my feeds", "Find trance in my RSS feeds and history, most seeders first."),
    ("Organize my music by genre", "Organize the music in my download folder into genre folders."),
    ("Play something", "Play something from my library — pick something good and tell me what it is."),
];

impl Moe {
    fn save_settings(&self) {
        let _ = std::fs::create_dir_all(crate::seed::data_dir());
        let _ = std::fs::write(settings_path(), serde_json::to_vec_pretty(&self.settings).unwrap_or_default());
    }

    fn push(events: &Arc<Mutex<Vec<Ev>>>, ctx: &egui::Context, ev: Ev) {
        events.lock().unwrap().push(ev);
        ctx.request_repaint();
    }

    /// Start (or retry) getting the model ready.
    /// `install_ok`: the user clicked Set up, so Ollama may be downloaded.
    fn prepare(&mut self, rt: &tokio::runtime::Runtime, ctx: &egui::Context, install_ok: bool) {
        self.status = Status::Preparing;
        let (events, ctx, pick) = (self.events.clone(), ctx.clone(), self.settings.model.clone());
        rt.spawn(async move {
            if !install_ok && ollama::find_binary().is_none() && !ollama::reachable().await {
                Moe::push(&events, &ctx, Ev::NeedsSetup);
                return;
            }
            ollama::ensure(pick, |s| Moe::push(&events, &ctx, Ev::Setup(s))).await;
            if let Ok(m) = ollama::models().await {
                let names = m.into_iter().filter(|m| m.tools).map(|m| m.name).collect::<Vec<_>>();
                // Models from the signed list that are not here yet: the picker offers them.
                if let Ok(man) = ollama::manifest().await {
                    let offers = man
                        .extra_models
                        .iter()
                        .chain(&man.models)
                        .filter(|t| !names.contains(t))
                        .filter_map(|t| man.sizes_gb.get(t).map(|gb| (t.clone(), *gb)))
                        .collect();
                    Moe::push(&events, &ctx, Ev::Offers(offers));
                }
                Moe::push(&events, &ctx, Ev::Models(names));
            }
        });
    }

    pub fn send(&mut self, text: String, rt: &tokio::runtime::Runtime, ctx: &egui::Context, lib: Library) {
        let Status::Ready(model) = &self.status else { return };
        let model = model.clone();
        self.lines.push(Line::User(text.clone()));
        self.busy = true;
        self.live.clear();
        let mut messages = vec![json!({"role": "system", "content": skills::system(&lib)})];
        let start = self.history.len().saturating_sub(KEEP);
        messages.extend(self.history[start..].iter().cloned());
        messages.push(json!({"role": "user", "content": text}));
        self.history.push(json!({"role": "user", "content": text}));
        // Progress: load sampled each second, and where the model sits (Ollama's /api/ps).
        let run = Run {
            model: model.clone(),
            started: Instant::now(),
            first_text: None,
            pieces: 0,
            stats: ollama::Stats::default(),
            load: Default::default(),
            residency: None,
            prompt_tokens: (messages.iter().map(|m| m.to_string().len()).sum::<usize>() + skills::tools().to_string().len()) / 4,
            stop: Arc::new(AtomicBool::new(false)),
        };
        gauge::start(run.load.clone(), run.stop.clone());
        let (events, ctx2, stop, m) = (self.events.clone(), ctx.clone(), run.stop.clone(), model.clone());
        rt.spawn(async move {
            while !stop.load(Ordering::Relaxed) {
                if let Some((t, v)) = ollama::residency(&m).await {
                    Moe::push(&events, &ctx2, Ev::Residency(t, v));
                }
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        });
        self.run = Some(run);
        let (events, ctx, memory, cpu) = (self.events.clone(), ctx.clone(), self.memory.clone(), self.settings.cpu_only);
        self.task = Some(rt.spawn(async move {
            let on = |e: Ev| Moe::push(&events, &ctx, e);
            ollama::ensure_placement(&model, cpu).await;
            let r = agent(ollama::BASE, &model, messages, cpu, &lib, &memory, &on).await;
            Moe::push(&events, &ctx, Ev::Done(r));
        }));
    }

    fn drain(&mut self, actions: &mut Vec<Action>) {
        let evs = std::mem::take(&mut *self.events.lock().unwrap());
        for ev in evs {
            match ev {
                Ev::Setup(ollama::Setup::Line(l)) => self.lines.push(Line::Note(l)),
                Ev::Models(m) => self.models = m,
                Ev::Offers(o) => self.offers = o,
                Ev::Pulled(tag) => {
                    self.lines.push(Line::Note(format!("{tag} is ready — Flux MoE uses it now.")));
                    self.offers.retain(|(t, _)| *t != tag);
                    if !self.models.contains(&tag) {
                        self.models.push(tag.clone());
                    }
                    self.settings.model = Some(tag.clone());
                    self.save_settings();
                    self.status = Status::Ready(tag);
                }
                Ev::NeedsSetup => self.status = Status::NeedsSetup,
                Ev::Setup(ollama::Setup::Ready(m)) => {
                    self.lines.push(Line::Note(format!("Flux MoE is ready — model {m}, on this computer.")));
                    self.status = Status::Ready(m);
                }
                Ev::Setup(ollama::Setup::Failed(e)) => self.status = Status::Failed(e),
                Ev::Text(t) => {
                    self.live.push_str(&t);
                    if let Some(r) = &mut self.run {
                        r.pieces += 1;
                        r.first_text.get_or_insert_with(Instant::now);
                    }
                }
                Ev::Stats(s) => {
                    if let Some(r) = &mut self.run {
                        r.stats.add(&s);
                    }
                }
                Ev::Residency(t, v) => {
                    if let Some(r) = &mut self.run {
                        r.residency = Some((t, v));
                    }
                }
                Ev::Tool(r) => {
                    self.flush_live();
                    self.lines.push(Line::Tool(r));
                }
                Ev::Action(Action::Picks(p)) => {
                    self.flush_live();
                    self.picks.push(p.into_iter().map(|p| (p, false)).collect());
                    self.lines.push(Line::Picks(self.picks.len() - 1));
                }
                Ev::Action(a) if a.needs_ok() => {
                    self.flush_live();
                    self.cards.push(Card { action: a, state: CardState::Waiting });
                    self.lines.push(Line::Card(self.cards.len() - 1));
                }
                Ev::Action(a) => actions.push(a),
                Ev::Done(r) => {
                    self.busy = false;
                    match r {
                        Ok(msgs) => {
                            // Answers before a tool call were shown when the tool ran; this is the last one.
                            self.live.clear();
                            let last = msgs.last().filter(|m| m["role"] == "assistant").and_then(|m| m["content"].as_str()).unwrap_or("").trim();
                            if !last.is_empty() {
                                self.lines.push(Line::Moe(last.to_string()));
                            }
                            self.history.extend(msgs);
                        }
                        Err(e) => {
                            self.flush_live();
                            self.lines.push(Line::Error(e));
                        }
                    }
                    if let Some(r) = self.run.take() {
                        let wall = r.started.elapsed().as_secs_f64();
                        self.lines.push(Line::Stats(summary(&r.model, &r.stats, wall, r.residency)));
                        if r.stats.eval_count > 0 {
                            // The bar's estimate: a running average of answer lengths.
                            let n = r.stats.eval_count as f64;
                            self.avg_tokens = if self.avg_tokens == 0.0 { n } else { 0.7 * self.avg_tokens + 0.3 * n };
                        }
                    }
                }
            }
        }
    }

    /// Has the user said anything yet (notes from setup don't count)?
    fn talked(&self) -> bool {
        self.lines.iter().any(|l| matches!(l, Line::User(_)))
    }

    fn flush_live(&mut self) {
        let t = ollama::strip_thinking(&std::mem::take(&mut self.live));
        if !t.trim().is_empty() {
            self.lines.push(Line::Moe(t.trim().to_string()));
        }
    }

    /// Is the MCP switch on?
    pub fn mcp_on(&self) -> bool {
        self.settings.mcp
    }

    pub fn mcp_started(&mut self, port: u16, setup: String) {
        self.lines.push(Line::Note(format!(
            "MCP is on: AI clients on this computer can use ZenTorrent's tools at 127.0.0.1:{port} (with the token below). \
             Downloads and file moves they ask for show up here for you to confirm."
        )));
        self.lines.push(Line::Setup(setup));
    }

    pub fn mcp_failed(&mut self, why: String) {
        self.settings.mcp = false;
        self.save_settings();
        self.lines.push(Line::Error(format!("MCP could not start: {why}")));
    }

    /// Something an MCP client asked for that needs the user's OK: a card.
    pub fn propose(&mut self, a: Action) {
        self.flush_live();
        self.lines.push(Line::Note("🔌 An MCP client asks:".into()));
        self.cards.push(Card { action: a, state: CardState::Waiting });
        self.lines.push(Line::Card(self.cards.len() - 1));
    }

    /// A message the user sent: the app answers with `send` and a fresh library snapshot.
    pub fn take_pending(&mut self) -> Option<String> {
        self.pending.take()
    }

    /// The tab. Returns what the app must do (play, download).
    pub fn ui(&mut self, ui: &mut egui::Ui, ctx: &egui::Context, rt: &tokio::runtime::Runtime) -> Vec<Action> {
        let mut actions = Vec::new();
        self.drain(&mut actions);
        if self.status == Status::Unknown {
            self.prepare(rt, ctx, false);
        }
        if self.busy || self.status == Status::Preparing {
            ctx.request_repaint_after(std::time::Duration::from_millis(100));
        }

        // ── header ──────────────────────────────────────────────
        let current = match &self.status {
            Status::Ready(m) => Some(m.clone()),
            _ => None,
        };
        let failed = matches!(self.status, Status::Failed(_));
        let needs_setup = self.status == Status::NeedsSetup;
        ui.horizontal(|ui| {
            ui.heading("✨ Flux MoE");
            if let Some(m) = &current {
                ui.label(RichText::new("on this computer").weak());
                let mut pick = m.clone();
                let mut get: Option<(String, f64)> = None;
                egui::ComboBox::from_id_salt("moe-model").selected_text(&pick).show_ui(ui, |ui| {
                    // Qwen only (the skills are tuned on it); the current model always shows.
                    for name in self.models.iter().filter(|n| n.starts_with("qwen") || *n == m) {
                        ui.selectable_value(&mut pick, name.clone(), name);
                    }
                    if !self.offers.is_empty() {
                        ui.separator();
                        for (tag, gb) in &self.offers {
                            if ui.selectable_label(false, format!("Get {tag} ({gb:.1} GB)…")).clicked() {
                                get = Some((tag.clone(), *gb));
                            }
                        }
                    }
                });
                if let Some((tag, gb)) = get {
                    let dir = ollama::models_dir();
                    let drive = dir.components().next().map(|c| c.as_os_str().to_string_lossy().into_owned()).unwrap_or_default();
                    let f = ollama::fit(gb, ollama::vram_gb(), ollama::free_disk_gb(&dir), &drive);
                    self.offer = Some((tag, f));
                }
                if &pick != m {
                    self.settings.model = Some(pick.clone());
                    self.save_settings();
                    self.status = Status::Ready(pick);
                }
            } else if failed {
                if ui.button("Retry").clicked() {
                    self.prepare(rt, ctx, true);
                }
            } else if !needs_setup {
                ui.spinner();
                ui.label(RichText::new("getting the local model ready…").weak());
            }
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.checkbox(&mut self.settings.cpu_only, "CPU only").on_hover_text("Keep the model off the graphics card (slower, cooler).").changed() {
                    self.save_settings();
                }
                if ui
                    .checkbox(&mut self.settings.mcp, "MCP")
                    .on_hover_text(
                        "Let AI clients on this computer (Claude Code, Flux MoE elsewhere) use ZenTorrent's tools over MCP. \
                         Only this computer, only with the token; downloads and moves wait for your OK here.",
                    )
                    .changed()
                {
                    self.save_settings();
                }
                if self.can_undo && ui.button("↶ Undo last organize").on_hover_text("Put the last applied moves back").clicked() {
                    let (n, problems) = organize::undo();
                    self.lines.push(Line::Note(format!("Undo: {n} moved back.{}", if problems.is_empty() { String::new() } else { format!(" Problems: {}", problems.join("; ")) })));
                    self.can_undo = organize::can_undo();
                }
                if self.busy && ui.button("Stop").on_hover_text("Stop this answer").clicked() {
                    if let Some(t) = self.task.take() {
                        t.abort();
                    }
                    self.busy = false;
                    self.flush_live();
                    if let Some(r) = self.run.take() {
                        let wall = r.started.elapsed().as_secs_f64();
                        self.lines.push(Line::Stats(format!("stopped after {wall:.0} s · {}", summary(&r.model, &r.stats, wall, r.residency))));
                    } else {
                        self.lines.push(Line::Note("Stopped.".into()));
                    }
                }
                if self.talked() && !self.busy && ui.button("New chat").clicked() {
                    self.lines.clear();
                    self.history.clear();
                    *self.memory.lock().unwrap() = Memory::default();
                }
            });
        });
        if let Status::Failed(e) = &self.status {
            ui.colored_label(Color32::from_rgb(230, 160, 60), e);
        }
        // The picker's "Get …": what it costs and how it would run here, before anything downloads.
        if let Some((tag, f)) = self.offer.clone() {
            egui::Frame::new().fill(Color32::from_rgb(34, 38, 48)).corner_radius(8.0).inner_margin(egui::Margin::same(10)).show(ui, |ui| {
                ui.label(RichText::new(format!("Get {tag}?")).strong());
                ui.label(&f.text);
                ui.horizontal(|ui| {
                    let go = ui.add_enabled(f.can_get, egui::Button::new(RichText::new("Download").strong()));
                    if go.clicked() {
                        let (events, ctx2, t) = (self.events.clone(), ctx.clone(), tag.clone());
                        rt.spawn(async move {
                            let say = |s| Moe::push(&events, &ctx2, Ev::Setup(s));
                            match ollama::pull(&t, &say).await {
                                Ok(()) => Moe::push(&events, &ctx2, Ev::Pulled(t)),
                                Err(e) => say(ollama::Setup::Line(format!("Could not get {t}: {e}"))),
                            }
                        });
                        self.lines.push(Line::Note(format!("Getting {tag} — progress shows here.")));
                        self.offer = None;
                    }
                    if ui.button("Cancel").clicked() {
                        self.offer = None;
                    }
                });
            });
        }
        if self.status == Status::NeedsSetup {
            ui.add_space(8.0);
            ui.label(
                "Flux MoE runs an AI model on this computer, so nothing you ask leaves it. That needs Ollama \
                 (about 1.6 GB) and a model (about 2.5 GB), downloaded once and checked against ZenTorrent's \
                 signed list before anything runs.",
            );
            if ui.button(RichText::new("Set up Flux MoE (≈ 4 GB, once)").strong()).clicked() {
                self.prepare(rt, ctx, true);
            }
        }
        ui.separator();

        // ── input (bottom) ──────────────────────────────────────
        let ready = matches!(self.status, Status::Ready(_)) && !self.busy;
        let mut send: Option<String> = None;
        egui::Panel::bottom("moe-input").show(ui, |ui| {
            ui.add_space(6.0);
            if !self.talked() {
                ui.horizontal_wrapped(|ui| {
                    for (label, prompt) in STARTERS {
                        if ui.add_enabled(ready, egui::Button::new(*label)).clicked() {
                            send = Some(prompt.to_string());
                        }
                    }
                });
                ui.add_space(4.0);
            }
            ui.horizontal(|ui| {
                let edit = ui.add_enabled(
                    ready,
                    egui::TextEdit::singleline(&mut self.input).hint_text("Ask Flux MoE — e.g. “make a playlist of my newest trance”").desired_width(ui.available_width() - 70.0),
                );
                let enter = edit.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
                if (ui.add_enabled(ready, egui::Button::new("Send")).clicked() || enter) && !self.input.trim().is_empty() {
                    send = Some(std::mem::take(&mut self.input));
                }
            });
            ui.add_space(4.0);
        });
        if send.is_some() {
            self.pending = send;
        }

        // ── transcript ──────────────────────────────────────────
        let mut decided: Option<(usize, bool)> = None;
        let mut grab: Option<(usize, usize)> = None;
        egui::ScrollArea::vertical().stick_to_bottom(true).auto_shrink([false, false]).show(ui, |ui| {
            if !self.talked() {
                ui.add_space(20.0);
                ui.label(
                    RichText::new(
                        "Flux MoE knows your torrents, your RSS feeds and your folders. Ask it for an overview, to find \
                         something in your feeds, to play music or video, to make a playlist, or to tidy a folder — moves \
                         are always shown first, and you press Apply.",
                    )
                    .weak(),
                );
            }
            for line in &self.lines {
                match line {
                    Line::User(t) => bubble(ui, t, Color32::from_rgb(40, 70, 110), true),
                    Line::Moe(t) => bubble(ui, t, Color32::from_rgb(38, 38, 44), false),
                    Line::Tool(t) => {
                        ui.label(RichText::new(format!("  · {t}")).small().color(Color32::from_rgb(140, 170, 200)));
                    }
                    Line::Note(t) => {
                        ui.label(RichText::new(t).small().weak());
                    }
                    Line::Setup(t) => {
                        egui::Frame::new().fill(Color32::from_rgb(22, 26, 32)).corner_radius(6.0).inner_margin(egui::Margin::same(8)).show(ui, |ui| {
                            ui.add(egui::Label::new(RichText::new(t).monospace().size(11.0)).selectable(true).wrap());
                            if ui.small_button("📋 Copy").on_hover_text("Paste it in a terminal").clicked() {
                                ui.ctx().copy_text(t.clone());
                            }
                        });
                    }
                    Line::Error(t) => {
                        ui.colored_label(Color32::from_rgb(230, 120, 100), t);
                    }
                    Line::Stats(t) => {
                        ui.label(RichText::new(t).small().color(Color32::from_rgb(120, 140, 160)));
                        ui.add_space(4.0);
                    }
                    Line::Picks(i) => {
                        if let Some(rows) = self.picks.get(*i) {
                            if let Some(r) = picks_ui(ui, rows) {
                                grab = Some((*i, r));
                            }
                        }
                    }
                    Line::Card(i) => {
                        if let Some(c) = self.cards.get(*i) {
                            if let Some(ok) = card_ui(ui, *i, c) {
                                decided = Some((*i, ok));
                            }
                        }
                    }
                }
            }
            if !self.live.is_empty() {
                bubble(ui, &ollama::strip_thinking(&self.live), Color32::from_rgb(38, 38, 44), false);
            }
            if let Some(r) = &self.run {
                progress_ui(ui, r, self.avg_tokens);
            }
        });

        // A Download button in a result list: the click is the user's OK.
        if let Some((list, row)) = grab {
            if let Some((p, done)) = self.picks.get_mut(list).and_then(|l| l.get_mut(row)) {
                *done = true;
                actions.push(Action::Download { title: p.title.clone(), link: p.link.clone(), feed_key: p.feed_key.clone() });
            }
        }
        if let Some((i, ok)) = decided {
            let card = &mut self.cards[i];
            if !ok {
                card.state = CardState::Cancelled;
            } else {
                match &card.action {
                    Action::Moves { moves, .. } => {
                        let (n, problems) = organize::apply(moves);
                        card.state = CardState::Done(format!("Moved {n}.{}", if problems.is_empty() { String::new() } else { format!(" Problems: {}", problems.join("; ")) }));
                        self.can_undo = organize::can_undo();
                    }
                    other => {
                        actions.push(other.clone());
                        card.state = CardState::Done("Started — see Downloads.".into());
                    }
                }
            }
        }
        actions
    }
}

/// A result list from the feeds: each row with its numbers and a Download button.
/// Returns the row whose button was clicked.
fn picks_ui(ui: &mut egui::Ui, rows: &[(Pick, bool)]) -> Option<usize> {
    let mut clicked = None;
    egui::Frame::new()
        .fill(Color32::from_rgb(28, 34, 44))
        .stroke(egui::Stroke::new(1.0, Color32::from_rgb(70, 100, 140)))
        .corner_radius(8.0)
        .inner_margin(egui::Margin::same(8))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            for (row, (p, done)) in rows.iter().enumerate() {
                ui.horizontal(|ui| {
                    // The button is laid out first (right side), so a long title is cut, never the button.
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if *done {
                            ui.label(RichText::new("✓ started").small().color(Color32::from_rgb(120, 200, 140)));
                        } else if ui.button("⬇ Download").on_hover_text("Start this download now — see Downloads").clicked() {
                            clicked = Some(row);
                        }
                        ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                            ui.label(RichText::new(format!("{:>2}", p.id)).monospace().weak());
                            ui.vertical(|ui| {
                                ui.add(egui::Label::new(RichText::new(&p.title).strong()).truncate()).on_hover_text(&p.title);
                                let mut meta = vec![p.feed.clone()];
                                if let Some(s) = p.size {
                                    meta.push(crate::human(s));
                                }
                                if let Some(s) = p.seeders {
                                    meta.push(format!("{s} seeders"));
                                }
                                if let Some(g) = p.grabs {
                                    meta.push(format!("{g} downloads"));
                                }
                                meta.push(format!("seen {} ago", p.seen));
                                if p.freeleech {
                                    meta.push("freeleech".into());
                                }
                                ui.label(RichText::new(meta.join(" · ")).small().weak());
                            });
                        });
                    });
                });
                if row + 1 < rows.len() {
                    ui.separator();
                }
            }
        });
    ui.add_space(4.0);
    clicked
}

/// The live panel while an answer is generated (as in sigil-top): what it waits
/// for or how fast it writes, an estimated bar, and the machine's load.
fn progress_ui(ui: &mut egui::Ui, r: &Run, avg_tokens: f64) {
    let elapsed = r.started.elapsed().as_secs_f64();
    egui::Frame::new().fill(Color32::from_rgb(30, 34, 40)).corner_radius(6.0).inner_margin(egui::Margin::same(8)).show(ui, |ui| {
        ui.set_width(ui.available_width());
        // Tokens so far: Ollama's own count for finished steps, else the streamed pieces.
        let tokens = r.pieces.max(r.stats.eval_count);
        match r.first_text {
            None => {
                ui.horizontal(|ui| {
                    ui.spinner();
                    let what = if r.stats.eval_count > 0 {
                        "using its tools, then reading what they found".to_string()
                    } else {
                        format!("loading the model and reading your request (~{} tokens)", r.prompt_tokens)
                    };
                    ui.label(RichText::new(format!("{what} · {elapsed:.0} s")).weak());
                });
            }
            Some(t0) => {
                let rate = r.pieces as f64 / t0.elapsed().as_secs_f64().max(0.05);
                let target = if avg_tokens > 0.0 { avg_tokens } else { 150.0 };
                // An estimate: never claims done before the model says so.
                let frac = (tokens as f64 / target).min(0.95) as f32;
                let left = match (rate > 0.5).then(|| (target - tokens as f64).max(0.0) / rate) {
                    Some(s) => format!("about {s:.0} s left"),
                    None => "…".into(),
                };
                ui.add(egui::ProgressBar::new(frac).animate(true).text(format!("{tokens} tokens · {rate:.1} tok/s · {left} · {elapsed:.0} s")));
            }
        }
        let l = r.load.lock().map(|l| *l).unwrap_or_default();
        let mut parts = Vec::new();
        if let (Some(g), Some(u), Some(t)) = (l.gpu_pct, l.vram_used_mb, l.vram_total_mb) {
            parts.push(format!("GPU {g} % · VRAM {:.1} / {:.1} GB", u as f64 / 1024.0, t as f64 / 1024.0));
        }
        if let Some(c) = l.gpu_temp_c {
            parts.push(format!("{c} °C"));
        }
        if let Some(c) = l.cpu_pct {
            parts.push(format!("CPU {c} %"));
        }
        if let Some((t, v)) = r.residency {
            parts.push(placement(t, v));
        }
        if !parts.is_empty() {
            let hot = l.gpu_temp_c.is_some_and(|c| c >= 85);
            let color = if hot {
                Color32::from_rgb(230, 120, 100)
            } else if l.gpu_pct.unwrap_or(0) >= 50 {
                Color32::from_rgb(120, 210, 140)
            } else {
                Color32::from_rgb(170, 170, 180)
            };
            let tip = if hot { "The graphics card is hot — tick CPU only to keep the model off it." } else { "Measured every second while Flux MoE answers." };
            ui.label(RichText::new(parts.join(" · ")).small().color(color)).on_hover_text(tip);
        }
    });
}

fn bubble(ui: &mut egui::Ui, text: &str, fill: Color32, right: bool) {
    let layout = if right { egui::Layout::right_to_left(egui::Align::TOP) } else { egui::Layout::left_to_right(egui::Align::TOP) };
    ui.with_layout(layout, |ui| {
        egui::Frame::new().fill(fill).corner_radius(8.0).inner_margin(egui::Margin::same(8)).show(ui, |ui| {
            ui.set_max_width(ui.available_width() * 0.8);
            ui.label(text);
        });
    });
    ui.add_space(4.0);
}

/// A proposal card. Returns Some(true) on Apply, Some(false) on Cancel.
fn card_ui(ui: &mut egui::Ui, i: usize, c: &Card) -> Option<bool> {
    let mut out = None;
    egui::Frame::new().fill(Color32::from_rgb(34, 44, 38)).stroke(egui::Stroke::new(1.0, Color32::from_rgb(90, 160, 110))).corner_radius(8.0).inner_margin(egui::Margin::same(10)).show(ui, |ui| {
        match &c.action {
            Action::Download { title, link, feed_key } => {
                ui.label(RichText::new("Download?").strong());
                ui.label(title);
                // Not from one of your feeds (an MCP client's link): say where it comes from.
                if feed_key.is_empty() {
                    let from = if link.starts_with("magnet:") { "a magnet link".to_string() } else { crate::rss::display_url(link) };
                    ui.label(RichText::new(format!("from {from}")).small().weak());
                }
            }
            Action::Moves { root, moves } => {
                ui.label(RichText::new(format!("Move {} item{} in {}", moves.len(), if moves.len() == 1 { "" } else { "s" }, root.display())).strong());
                egui::ScrollArea::vertical().id_salt(("moves", i)).max_height(220.0).show(ui, |ui| {
                    for m in moves {
                        let from = m.from.strip_prefix(root).unwrap_or(&m.from).to_string_lossy().into_owned();
                        let to = m.to.strip_prefix(root).unwrap_or(&m.to).to_string_lossy().into_owned();
                        ui.label(RichText::new(format!("{from}  →  {to}/")).monospace().small());
                    }
                });
            }
            _ => {}
        }
        match &c.state {
            CardState::Waiting => {
                ui.horizontal(|ui| {
                    let go = if matches!(c.action, Action::Download { .. }) { "Download" } else { "Apply" };
                    if ui.button(RichText::new(go).strong()).clicked() {
                        out = Some(true);
                    }
                    if ui.button("Cancel").clicked() {
                        out = Some(false);
                    }
                });
            }
            CardState::Done(t) => {
                ui.label(RichText::new(format!("✓ {t}")).color(Color32::from_rgb(120, 200, 140)));
            }
            CardState::Cancelled => {
                ui.label(RichText::new("Cancelled — nothing changed.").weak());
            }
        }
    });
    ui.add_space(4.0);
    out
}

/// One user message: model ↔ tools until the model answers in words. Everything
/// it does is reported through `on` (the tab's event queue, or `--ask`'s printer).
async fn agent(
    base: &str,
    model: &str,
    mut messages: Vec<Value>,
    cpu_only: bool,
    lib: &Library,
    memory: &Mutex<Memory>,
    on: &(impl Fn(Ev) + Sync),
) -> Result<Vec<Value>, String> {
    let tools = skills::tools();
    let first_new = messages.len();
    for _ in 0..MAX_STEPS {
        let reply = ollama::chat(base, model, &messages, &tools, cpu_only, |t| on(Ev::Text(t.to_string()))).await?;
        on(Ev::Stats(reply.stats));
        messages.push(json!({"role": "assistant", "content": reply.content, "tool_calls": reply.tool_calls}));
        if reply.tool_calls.is_empty() {
            return Ok(messages.split_off(first_new));
        }
        for call in &reply.tool_calls {
            let name = call["function"]["name"].as_str().unwrap_or("");
            // Some models send the arguments as a JSON string.
            let args = match &call["function"]["arguments"] {
                Value::String(s) => serde_json::from_str(s).unwrap_or(Value::Null),
                v => v.clone(),
            };
            let out = {
                let mut mem = memory.lock().unwrap();
                skills::run(name, &args, lib, &mut mem)
            };
            on(Ev::Tool(out.receipt));
            if let Some(a) = out.action {
                on(Ev::Action(a));
            }
            let mut content = out.for_model.to_string();
            if content.len() > 12_000 {
                let mut cut = 12_000;
                while !content.is_char_boundary(cut) {
                    cut -= 1;
                }
                content.truncate(cut);
                content.push_str("…(cut)");
            }
            messages.push(json!({"role": "tool", "tool_name": name, "content": content}));
        }
    }
    Ok(messages.split_off(first_new))
}

/// `zentorrent --ask "<question>"`: one Flux MoE turn in the terminal, against the
/// real local model and the real folder — but actions are only printed, never done.
pub async fn ask(question: &str, lib: Library, cpu_only: bool) -> Result<(), String> {
    use std::io::Write;
    let settings: Settings = std::fs::read(settings_path()).ok().and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default();
    let cpu_only = cpu_only || settings.cpu_only;
    if !ollama::reachable().await {
        return Err("Ollama is not running on this computer (open the Flux MoE tab once to set it up)".into());
    }
    let model = ollama::choose(&ollama::models().await?, settings.model.as_deref()).ok_or("no model that can use tools is installed")?;
    println!("Flux MoE · {model}{} · {}", if cpu_only { " (CPU only)" } else { "" }, lib.folder.display());
    let started = std::time::Instant::now();
    let messages = vec![json!({"role": "system", "content": skills::system(&lib)}), json!({"role": "user", "content": question})];
    let memory = Mutex::new(Memory::default());
    let totals = Mutex::new(ollama::Stats::default());
    let print = |ev: Ev| {
        match ev {
            Ev::Text(t) => print!("{t}"),
            Ev::Stats(s) => totals.lock().unwrap().add(&s),
            Ev::Tool(r) => println!("\n  · {r}"),
            Ev::Action(Action::Moves { root, moves }) => {
                println!("\n  [would ask the user to apply {} moves in {}]", moves.len(), root.display());
                for m in &moves {
                    println!("     {}  →  {}/", m.from.strip_prefix(&root).unwrap_or(&m.from).display(), m.to.strip_prefix(&root).unwrap_or(&m.to).display());
                }
            }
            Ev::Action(a) => println!("\n  [would {a:?}]"),
            _ => {}
        }
        let _ = std::io::stdout().flush();
    };
    if ollama::ensure_placement(&model, cpu_only).await {
        println!("  (the model was on the CPU though a GPU is here — reloading it onto the GPU)");
    }
    agent(ollama::BASE, &model, messages, cpu_only, &lib, &memory, &print).await?;
    let s = *totals.lock().unwrap();
    println!("\n({})", summary(&model, &s, started.elapsed().as_secs_f64(), ollama::residency(&model).await));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};

    /// A one-connection-per-reply stand-in for Ollama's /api/chat: each request
    /// gets the next canned NDJSON stream; the request bodies are returned.
    fn fake_ollama(replies: Vec<Vec<Value>>) -> (String, std::thread::JoinHandle<Vec<Value>>) {
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", l.local_addr().unwrap());
        let h = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for lines in replies {
                let (mut s, _) = l.accept().unwrap();
                let mut r = BufReader::new(s.try_clone().unwrap());
                let mut len = 0;
                loop {
                    let mut h = String::new();
                    r.read_line(&mut h).unwrap();
                    if let Some(v) = h.to_ascii_lowercase().strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap();
                    }
                    if h == "\r\n" {
                        break;
                    }
                }
                let mut body = vec![0; len];
                r.read_exact(&mut body).unwrap();
                seen.push(serde_json::from_slice(&body).unwrap());
                let out: String = lines.iter().map(|l| l.to_string() + "\n").collect();
                write!(s, "HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nConnection: close\r\n\r\n{out}").unwrap();
            }
            seen
        });
        (base, h)
    }

    fn library() -> Library {
        let entry = |title: &str, seeders| crate::history::Entry {
            title: title.into(),
            link: format!("https://tracker.example/dl?id=1&passkey=SECRET-{title}"),
            feed: "Music".into(),
            in_feed: true,
            seeders: Some(seeders),
            ..Default::default()
        };
        Library { feeds: vec![entry("Trance Classics 2026", 300), entry("Uplifting Trance 2025", 77), entry("Jazz Night", 9)], ..Default::default() }
    }

    #[test]
    fn a_tool_call_runs_the_skill_and_the_answer_comes_back() {
        let (base, server) = fake_ollama(vec![
            vec![
                json!({"message": {"role": "assistant", "content": "", "tool_calls": [
                    {"function": {"name": "search_feeds", "arguments": {"query": "trance", "sort": "seeders"}}}]}, "done": false}),
                json!({"done": true, "eval_count": 29, "eval_duration": 1_000_000_000u64, "prompt_eval_count": 2114, "prompt_eval_duration": 2_000_000_000u64}),
            ],
            vec![
                json!({"message": {"content": "Top pick: Trance Classics 2026 (300 seeders)."}, "done": false}),
                json!({"message": {"content": " Want it? ", "tool_calls": [
                    {"function": {"name": "download", "arguments": "{\"id\": 1}"}}]}, "done": false}),
                json!({"done": true}),
            ],
            vec![json!({"message": {"content": "Waiting for you to press Download."}, "done": true})],
        ]);
        let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
        let events = Mutex::new(Vec::new());
        let on = |e: Ev| events.lock().unwrap().push(e);
        let memory = Mutex::new(Memory::default());
        let msgs = vec![json!({"role": "system", "content": skills::SYSTEM}), json!({"role": "user", "content": "find trance"})];
        let out = rt.block_on(agent(&base, "qwen3:4b", msgs, true, &library(), &memory, &on)).unwrap();
        let requests = server.join().unwrap();

        // The model got the tools, no thinking, CPU only; then the tool's answer.
        assert_eq!(requests[0]["think"], false);
        assert_eq!(requests[0]["options"]["num_gpu"], 0);
        assert!(requests[0]["tools"].as_array().unwrap().len() >= 8);
        let tool_msg = requests[1]["messages"].as_array().unwrap().iter().find(|m| m["role"] == "tool").unwrap();
        let text = tool_msg["content"].as_str().unwrap();
        assert!(text.contains("Trance Classics 2026") && !text.contains("SECRET") && !text.contains("https://"), "{text}");

        // The tab saw the receipts, the streamed text and a download that waits for the user.
        let evs = events.into_inner().unwrap();
        assert!(evs.iter().any(|e| matches!(e, Ev::Tool(r) if r.contains("searched your feeds"))));
        assert!(evs.iter().any(|e| matches!(e, Ev::Stats(s) if s.eval_count == 29 && s.prompt_count == 2114)), "Ollama's numbers reach the tab");
        assert!(evs.iter().any(|e| matches!(e, Ev::Action(Action::Download { title, .. }) if title == "Trance Classics 2026")));
        assert_eq!(out.last().unwrap()["content"], "Waiting for you to press Download.");
        assert_eq!(out.iter().filter(|m| m["role"] == "tool").count(), 2);
    }

    #[test]
    fn the_summary_says_how_it_went() {
        let s = ollama::Stats { eval_count: 29, eval_ns: 1_000_000_000, prompt_count: 2114, prompt_ns: 2_000_000_000, load_ns: 1_500_000_000, total_ns: 0 };
        let t = summary("qwen3:4b-instruct", &s, 4.3, Some((3_760_000_000, 3_760_000_000)));
        assert_eq!(t, "qwen3:4b-instruct · 29 tokens · 29.0 tok/s · read 2114 tokens in 2.0 s · model loaded in 1.5 s · 4.3 s in all · model 3.5 GB, all on the GPU");
        assert!(placement(5_030_000_000, 3_880_000_000).contains("77 % on the GPU"));
        assert!(placement(3_000_000_000, 0).ends_with("on the CPU"));
    }
}
