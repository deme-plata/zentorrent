//! The player engine: one libmpv instance, ZenTorrent's own queue, and the
//! sound settings.
//!
//! mpv's playlist always holds the playing track plus the next one, so
//! music plays gaplessly (live sets, mixes); when mpv moves on, the track
//! after that is queued. Finished files play from disk; files still
//! downloading stream through `zt://` (see `stream.rs`).

use std::ffi::{c_char, CStr};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};

use super::ffi::{self, Api, Handle};
use super::playlist::Track;
use super::stream::{self, Registry};

// ── sound settings ──────────────────────────────────────────────────────────

/// The ten graphic-EQ bands (Hz), one octave apart.
pub const BANDS: [u32; 10] = [31, 62, 125, 250, 500, 1000, 2000, 4000, 8000, 16000];
pub const EQ_RANGE: f32 = 12.0;

/// Starting points, in dB per band.
pub const PRESETS: [(&str, [f32; 10]); 7] = [
    ("Flat", [0.0; 10]),
    ("Bass", [6.0, 5.0, 4.0, 2.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0]),
    ("Trance", [5.0, 4.0, 1.0, 0.0, -1.0, 0.0, 2.0, 3.0, 4.0, 4.0]),
    ("Vocal", [-2.0, -2.0, -1.0, 1.0, 3.0, 3.0, 2.0, 1.0, 0.0, -1.0]),
    ("Loudness", [5.0, 4.0, 2.0, 0.0, -1.0, 0.0, 0.0, 1.0, 3.0, 4.0]),
    ("Treble", [0.0, 0.0, 0.0, 0.0, 0.0, 1.0, 2.0, 4.0, 5.0, 6.0]),
    ("Classical", [3.0, 2.0, 1.0, 0.0, 0.0, 0.0, -1.0, -1.0, 0.0, 1.0]),
];

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct Sound {
    /// 0–130 %
    pub volume: f64,
    /// dB per band, ±12
    pub eq: [f32; 10],
    pub preset: String,
    /// Even out loud and quiet tracks (dynaudnorm).
    pub normalize: bool,
    /// "no", "track" or "album"
    pub replaygain: String,
    /// Keep the filters' memory across a track change (see `warm`): no step from the
    /// equalizer between lossless tracks. On where it has been measured (Linux).
    pub lead_in: bool,
    /// With "track": tracks that carry no ReplayGain tags are measured by ZenTorrent
    /// (EBU R128, see `loudness`) and played at the same level as tagged ones.
    pub measure: bool,
    /// Headphones: blend a little of each side into the other.
    pub crossfeed: bool,
    /// Stereo width: 1.0 = as recorded.
    pub width: f32,
    /// 0.5–2.0, pitch stays the same.
    pub speed: f64,
}

impl Default for Sound {
    fn default() -> Self {
        Sound {
            volume: 100.0,
            eq: [0.0; 10],
            preset: "Flat".into(),
            normalize: false,
            replaygain: "track".into(),
            lead_in: !cfg!(windows),
            measure: true,
            crossfeed: false,
            width: 1.0,
            speed: 1.0,
        }
    }
}

impl Sound {
    /// Volume above 100 % as a gain factor, on mpv's own (cubic) volume curve, so the
    /// slider feels as before. It is applied in the filter chain, before the limiter:
    /// mpv's own volume comes after every filter, where nothing can catch its clipping
    /// (measured 2026-11-27: 130 % put a full-scale track 6.7 dB over).
    pub fn boost(&self) -> Option<f64> {
        (self.volume > 100.5).then(|| (self.volume.min(130.0) / 100.0).powi(3))
    }

    /// What mpv's own volume is set to: everything up to 100 %.
    pub fn mpv_volume(&self) -> f64 {
        self.volume.clamp(0.0, 100.0)
    }

    /// Can these settings push a full-scale track over full scale? An EQ boost, a wider
    /// stereo image, or volume above 100 % can; cuts and the rest cannot.
    fn can_clip(&self) -> bool {
        self.eq.iter().any(|&g| g >= 0.1) || self.width >= 1.02 || self.boost().is_some()
    }

    /// mpv's `af` (audio filter chain) for these settings. Flat bands are left
    /// out; a limiter follows anything that can make the sound louder than full
    /// scale, so no setting clips.
    pub fn af(&self) -> String {
        let mut f: Vec<String> = Vec::new();
        for (i, (&hz, &g)) in BANDS.iter().zip(self.eq.iter()).enumerate() {
            if g.abs() >= 0.1 {
                f.push(format!("@eq{i}:lavfi=[equalizer=f={hz}:width_type=o:width=1:gain={g:.1}]"));
            }
        }
        if self.normalize {
            f.push("@norm:lavfi=[dynaudnorm=f=500:g=31]".into());
        }
        if self.crossfeed {
            f.push("@xfeed:lavfi=[crossfeed=strength=0.3]".into());
        }
        if (self.width - 1.0).abs() >= 0.02 {
            // c=0: extrastereo's own clipping is a hard cut (measured: width 2.0 flattened a
            // hard-panned track's peaks); the limiter below holds the level instead.
            f.push(format!("@wide:lavfi=[extrastereo=m={:.2}:c=0]", self.width));
        }
        if let Some(gain) = self.boost() {
            f.push(format!("@gain:lavfi=[volume={gain:.3}]"));
        }
        if self.can_clip() {
            // level=0: left on, alimiter scales its output back up to full scale, and the
            // 0.97 ceiling is gone again (measured: peak 0.00 dB with it, -0.26 dB without).
            // latency=1: the limiter looks 5 ms ahead, and without this it plays everything
            // 5 ms late — so at every track change the old track's last 5 ms were lost and
            // the new one began with 5 ms of silence (measured 2026-11-27 in the real
            // player: 219 silent frames at the change; none with it).
            f.push("@limit:lavfi=[alimiter=limit=0.97:level=0:latency=1]".into());
        }
        f.join(",")
    }

    fn path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("zentorrent").join("player.json"))
    }

    pub fn load() -> Sound {
        crate::db::load("player", Self::path().as_deref()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn save(&self) {
        let _ = crate::db::save("player", &serde_json::to_vec_pretty(self).unwrap_or_default());
    }
}

// ── the queue ───────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Repeat {
    #[default]
    Off,
    All,
    One,
}

/// What is playing: one torrent's tracks, in play order.
#[derive(Default)]
pub struct Queue {
    pub hash: String,
    pub torrent: String,
    /// The torrent's output folder (finished files are played from here).
    pub folder: PathBuf,
    pub tracks: Vec<Track>,
    /// Play order: indexes into `tracks` (shuffled or not).
    pub order: Vec<usize>,
    /// Position in `order` of the playing track.
    pub pos: Option<usize>,
    pub shuffle: bool,
    pub repeat: Repeat,
}

impl Queue {
    pub fn current(&self) -> Option<&Track> {
        self.pos.and_then(|p| self.order.get(p)).and_then(|&i| self.tracks.get(i))
    }

    /// The position after `p` in play order, honouring repeat.
    pub fn after(&self, p: usize) -> Option<usize> {
        match self.repeat {
            Repeat::One => Some(p),
            _ if p + 1 < self.order.len() => Some(p + 1),
            Repeat::All if !self.order.is_empty() => Some(0),
            _ => None,
        }
    }

    pub fn before(&self, p: usize) -> Option<usize> {
        match p.checked_sub(1) {
            Some(q) => Some(q),
            None if self.repeat == Repeat::All && !self.order.is_empty() => Some(self.order.len() - 1),
            None => None,
        }
    }

    /// Shuffle on/off, keeping the playing track where it is.
    pub fn set_shuffle(&mut self, on: bool, seed: u64) {
        let cur = self.pos.and_then(|p| self.order.get(p).copied());
        self.shuffle = on;
        self.order = (0..self.tracks.len()).collect();
        if on {
            // Fisher–Yates with a small xorshift: no rand dependency for this.
            let mut s = seed | 1;
            for i in (1..self.order.len()).rev() {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                self.order.swap(i, (s % (i as u64 + 1)) as usize);
            }
            if let Some(c) = cur {
                let at = self.order.iter().position(|&x| x == c).unwrap();
                self.order.swap(0, at);
            }
        }
        self.pos = cur.map(|c| self.order.iter().position(|&x| x == c).unwrap());
    }

    /// Take a track out of the queue (by play-order position).
    pub fn remove(&mut self, at: usize) {
        if at >= self.order.len() || Some(at) == self.pos {
            return; // the playing track stays; skip it instead
        }
        self.order.remove(at);
        if let Some(p) = self.pos.as_mut() {
            if at < *p {
                *p -= 1;
            }
        }
    }

    /// Move a track (by play-order position) to another place in the order.
    pub fn move_item(&mut self, from: usize, to: usize) {
        if from >= self.order.len() || to >= self.order.len() || from == to {
            return;
        }
        let cur = self.pos.and_then(|p| self.order.get(p).copied());
        let item = self.order.remove(from);
        self.order.insert(to, item);
        self.pos = cur.map(|c| self.order.iter().position(|&x| x == c).unwrap());
    }
}

// ── live state from mpv's events ────────────────────────────────────────────

#[derive(Default, Clone)]
pub struct Live {
    pub time: f64,
    pub duration: f64,
    pub paused: bool,
    pub title: String,
    pub artist: String,
    /// mpv moved to this entry of its playlist.
    pub mpv_pos: Option<i64>,
    /// For each entry of mpv's playlist: how far into what mpv plays the track itself
    /// starts (the lead-in of `warm`; 0 for a track queued as it is).
    pub leads: Vec<f64>,
    /// mpv's own clock for the playing entry, lead-in included.
    raw_time: f64,
    raw_duration: f64,
    pub idle: bool,
    /// A track failed to play (reason text).
    pub error: Option<String>,
    /// Bumped on every change, so the UI can repaint.
    pub version: u64,
    pub fullscreen: bool,
    /// The video window's ✕, Q, or Esc outside fullscreen stopped playback (Esc in
    /// fullscreen only leaves fullscreen).
    pub close: bool,
    /// mpv shut itself down (a quit that got past the bindings): the handle must go.
    pub gone: bool,
}

impl Live {
    /// `time` and `duration` as the track's own: without the lead-in of the entry playing.
    fn settle(&mut self) {
        let lead = self.mpv_pos.and_then(|p| self.leads.get(p as usize).copied()).unwrap_or(0.0);
        self.time = (self.raw_time - lead).max(0.0);
        self.duration = if self.raw_duration > 0.0 { (self.raw_duration - lead).max(0.0) } else { 0.0 };
    }
}

/// Keys of mpv's video window that ZenTorrent handles itself. Without them the
/// window's ✕ and Q run mpv's `quit`, which ends mpv's core but leaves the window
/// standing (frozen) until ZenTorrent exits.
const BINDINGS: [(&str, &str); 4] =
    [("CLOSE_WIN", "zentorrent-close"), ("q", "zentorrent-close"), ("Q", "zentorrent-close"), ("ESC", "zentorrent-esc")];

const P_TIME: u64 = 1;
const P_DURATION: u64 = 2;
const P_PAUSE: u64 = 3;
const P_TITLE: u64 = 4;
const P_POS: u64 = 5;
const P_IDLE: u64 = 6;
const P_ARTIST: u64 = 7;
const P_FULLSCREEN: u64 = 8;

struct Mpv {
    api: &'static Api,
    h: *mut Handle,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
    /// Does this mpv's `loadfile` take an index before the per-file options (0.38 and
    /// newer)? Learned on the first load that has options.
    indexed: std::cell::Cell<Option<bool>>,
}

// The client API is thread-safe; the handle is only destroyed in Drop, after the event thread ended.
unsafe impl Send for Mpv {}

/// The video window's title (fixed: the main window finds it by this, see `tray::own_video_window`).
pub const WINDOW_TITLE: &str = "ZenTorrent player  —  Esc or Q closes";

impl Mpv {
    fn new(reg: &'static Registry, live: Arc<Mutex<Live>>, repaint: Arc<dyn Fn() + Send + Sync>) -> Result<Mpv, String> {
        let api = ffi::api()?;
        let h = unsafe { (api.create)() };
        if h.is_null() {
            return Err("libmpv could not create a player".into());
        }
        let mut m = Mpv { api, h, stop: Arc::new(AtomicBool::new(false)), thread: None, indexed: Default::default() };
        // Before initialize: options. mpv reads no config files of its own; ZenTorrent sets everything.
        let ao = std::env::var("ZENTORRENT_MPV_AO").unwrap_or_default();
        for (k, v) in [
            ("config", "no"),
            ("terminal", "no"),
            ("idle", "yes"),
            ("force-window", "no"),
            // Music: cover art does not open a video window.
            ("audio-display", "no"),
            // Video files open mpv's own window for now, with the high-quality renderer.
            // The trailing comma lets mpv fall back to its other outputs when no GPU
            // renderer starts (old or software-only drivers): mpv sets video up before
            // audio, so a failed video output would otherwise end the whole file.
            ("vo", "gpu-next,gpu,"),
            ("profile", "high-quality"),
            ("scale", "ewa_lanczossharp"),
            ("cscale", "ewa_lanczossharp"),
            ("deband", "yes"),
            ("hwdec", "auto-safe"),
            ("gapless-audio", "weak"),
            ("prefetch-playlist", "yes"),
            ("input-default-bindings", "yes"),
            ("input-vo-keyboard", "yes"),
            ("title", WINDOW_TITLE),
        ] {
            m.opt(k, v);
        }
        if !ao.is_empty() {
            m.opt("ao", &ao); // tests on a machine without sound use ao=null
        }
        // Diagnostics: mpv's own full log (what to ask for when a file won't play),
        // and the video output (a machine without a GPU: vo=null).
        for (var, opt) in [("ZENTORRENT_MPV_LOG", "log-file"), ("ZENTORRENT_MPV_VO", "vo")] {
            if let Ok(v) = std::env::var(var).map(|v| v.trim().to_string()).and_then(|v| if v.is_empty() { Err(std::env::VarError::NotPresent) } else { Ok(v) }) {
                m.opt(opt, &v);
            }
        }
        let rc = unsafe { (api.initialize)(h) };
        if rc < 0 {
            return Err(format!("libmpv: {}", ffi::err_text(api, rc)));
        }
        stream::register(api, h, reg)?;
        for (id, name, fmt) in [
            (P_TIME, "time-pos", ffi::FORMAT_DOUBLE),
            (P_DURATION, "duration", ffi::FORMAT_DOUBLE),
            (P_PAUSE, "pause", ffi::FORMAT_FLAG),
            (P_TITLE, "media-title", ffi::FORMAT_STRING),
            (P_POS, "playlist-pos", ffi::FORMAT_INT64),
            (P_IDLE, "idle-active", ffi::FORMAT_FLAG),
            (P_ARTIST, "metadata/by-key/artist", ffi::FORMAT_STRING),
            (P_FULLSCREEN, "fullscreen", ffi::FORMAT_FLAG),
        ] {
            let n = ffi::cstr(name);
            unsafe { (api.observe_property)(h, id, n.as_ptr(), fmt) };
        }
        // The window's ✕ / Q / Esc come back to ZenTorrent as script messages.
        // `keybind` is mpv 0.38+; older libmpv (Linux distributions) gets a forced section.
        let bound = BINDINGS.iter().all(|(key, msg)| m.cmd(&["keybind", key, &format!("script-message {msg}")]).is_ok());
        if !bound {
            let section: String = BINDINGS.iter().map(|(key, msg)| format!("{key} script-message {msg}\n")).collect();
            let _ = m.cmd(&["define-section", "zentorrent", &section, "force"]);
            let _ = m.cmd(&["enable-section", "zentorrent"]);
        }
        let (hp, stop) = (h as usize, m.stop.clone());
        m.thread = Some(std::thread::Builder::new().name("mpv-events".into()).spawn(move || {
            let h = hp as *mut Handle;
            while !stop.load(Ordering::Acquire) {
                let ev = unsafe { &*(api.wait_event)(h, 1.0) };
                if ev.event_id == ffi::EVENT_SHUTDOWN {
                    // Not ours (Drop sets `stop` first): mpv quit by itself. Tell the
                    // player, which destroys the handle — that is what closes the window.
                    if !stop.load(Ordering::Acquire) {
                        live.lock().unwrap().gone = true;
                        repaint();
                    }
                    break;
                }
                if ev.event_id == ffi::EVENT_NONE {
                    continue;
                }
                let mut l = live.lock().unwrap();
                match ev.event_id {
                    ffi::EVENT_PROPERTY_CHANGE if !ev.data.is_null() => {
                        let p = unsafe { &*(ev.data as *const ffi::EventProperty) };
                        let none = p.data.is_null() || p.format == 0;
                        match ev.reply_userdata {
                            P_TIME => l.raw_time = if none { 0.0 } else { unsafe { *(p.data as *const f64) } },
                            P_DURATION => l.raw_duration = if none { 0.0 } else { unsafe { *(p.data as *const f64) } },
                            P_PAUSE => l.paused = !none && unsafe { *(p.data as *const i32) } != 0,
                            P_IDLE => l.idle = !none && unsafe { *(p.data as *const i32) } != 0,
                            P_FULLSCREEN => l.fullscreen = !none && unsafe { *(p.data as *const i32) } != 0,
                            P_POS => l.mpv_pos = if none { None } else { Some(unsafe { *(p.data as *const i64) }) },
                            P_TITLE | P_ARTIST => {
                                let s = if none { String::new() } else { unsafe { CStr::from_ptr(*(p.data as *const *const c_char)) }.to_string_lossy().into_owned() };
                                if ev.reply_userdata == P_TITLE { l.title = super::warm::tidy_title(&s).to_string() } else { l.artist = s }
                            }
                            _ => {}
                        }
                    }
                    ffi::EVENT_END_FILE if !ev.data.is_null() => {
                        let e = unsafe { &*(ev.data as *const ffi::EventEndFile) };
                        if e.reason == ffi::END_ERROR {
                            l.error = Some(format!("could not play this track: {}", ffi::err_text(api, e.error)));
                        }
                    }
                    ffi::EVENT_FILE_LOADED => l.error = None,
                    ffi::EVENT_CLIENT_MESSAGE if !ev.data.is_null() => {
                        let cm = unsafe { &*(ev.data as *const ffi::EventClientMessage) };
                        if cm.num_args > 0 && !cm.args.is_null() {
                            let first = unsafe { CStr::from_ptr(*cm.args) }.to_string_lossy();
                            // Acted on right here, not in the UI: a fullscreen video hides
                            // ZenTorrent's window, and a hidden window draws no frames.
                            let leave_fullscreen = first == "zentorrent-esc" && l.fullscreen;
                            let close = first == "zentorrent-close" || (first == "zentorrent-esc" && !l.fullscreen);
                            if leave_fullscreen {
                                let (n, v) = (ffi::cstr("fullscreen"), ffi::cstr("no"));
                                unsafe { (api.set_property_string)(h, n.as_ptr(), v.as_ptr()) };
                            } else if close {
                                // Stopping closes the window (no video, and force-window is off);
                                // the player clears its queue when it next draws.
                                let s = ffi::cstr("stop");
                                let mut args = [s.as_ptr(), std::ptr::null()];
                                unsafe { (api.command)(h, args.as_mut_ptr()) };
                                l.close = true;
                            }
                        }
                    }
                    _ => {}
                }
                l.settle();
                l.version += 1;
                drop(l);
                repaint();
            }
        }).map_err(|e| e.to_string())?);
        Ok(m)
    }

    fn opt(&mut self, k: &str, v: &str) {
        let (k, v) = (ffi::cstr(k), ffi::cstr(v));
        unsafe { (self.api.set_option_string)(self.h, k.as_ptr(), v.as_ptr()) };
    }

    fn cmd(&self, args: &[&str]) -> Result<(), String> {
        let owned: Vec<_> = args.iter().map(|a| ffi::cstr(a)).collect();
        let mut ptrs: Vec<*const c_char> = owned.iter().map(|c| c.as_ptr()).collect();
        ptrs.push(std::ptr::null());
        let rc = unsafe { (self.api.command)(self.h, ptrs.as_mut_ptr()) };
        if rc < 0 { Err(ffi::err_text(self.api, rc)) } else { Ok(()) }
    }

    /// `loadfile` with options that hold for this file only, or an error: for a load that
    /// would be wrong without them. mpv 0.38 put an index argument before the options;
    /// both forms are tried once and the one this mpv takes is remembered.
    fn load_with(&self, src: &str, mode: &str, opts: &str) -> Result<(), String> {
        let with = |indexed: bool| if indexed { self.cmd(&["loadfile", src, mode, "-1", opts]) } else { self.cmd(&["loadfile", src, mode, opts]) };
        let tries = match self.indexed.get() {
            Some(known) => vec![known],
            None => vec![true, false],
        };
        let mut last = Err("no form of loadfile was taken".to_string());
        for indexed in tries {
            last = with(indexed);
            if last.is_ok() {
                self.indexed.set(Some(indexed));
                break;
            }
        }
        last
    }

    /// `loadfile`, with options that are only an improvement (the track's measured gain):
    /// a file is never left unplayed over them. If they are not taken, it is loaded without.
    fn load(&self, src: &str, mode: &str, opts: Option<&str>) -> Result<(), String> {
        match opts {
            Some(o) if self.load_with(src, mode, o).is_ok() => Ok(()),
            _ => self.cmd(&["loadfile", src, mode]),
        }
    }

    fn set(&self, name: &str, value: &str) -> Result<(), String> {
        let (n, v) = (ffi::cstr(name), ffi::cstr(value));
        let rc = unsafe { (self.api.set_property_string)(self.h, n.as_ptr(), v.as_ptr()) };
        if rc < 0 { Err(format!("{name}: {}", ffi::err_text(self.api, rc))) } else { Ok(()) }
    }
}

impl Drop for Mpv {
    fn drop(&mut self) {
        // End the event thread first (it holds the handle), then destroy the handle.
        self.stop.store(true, Ordering::Release);
        unsafe { (self.api.wakeup)(self.h) };
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
        unsafe { (self.api.terminate_destroy)(self.h) };
    }
}

// ── the player ──────────────────────────────────────────────────────────────

impl Drop for Player {
    fn drop(&mut self) {
        // mpv first (it holds the lead-in files open), then the files.
        self.mpv = None;
        let _ = std::fs::remove_dir_all(&self.lead_dir);
    }
}

pub struct Player {
    mpv: Option<Mpv>,
    pub registry: &'static Registry,
    pub live: Arc<Mutex<Live>>,
    pub queue: Queue,
    pub sound: Sound,
    /// Order positions in mpv's playlist, in mpv's order (current, next).
    in_mpv: Vec<usize>,
    /// The lead-in each of those entries was queued with (see `warm`; 0 = none).
    leads: Vec<f64>,
    /// A lead-in entry failed to play once: none are made any more this run.
    warm_off: bool,
    /// Where this player writes its lead-in files (one second of sound each), and the
    /// number of the last one. Four names in turn: mpv may still hold the one before.
    lead_dir: PathBuf,
    lead_slot: u32,
    /// Why the player can't start (libmpv missing…), shown in the UI.
    pub error: Option<String>,
    repaint: Arc<dyn Fn() + Send + Sync>,
    /// Where you were in films and long mixes (see `bookmark`).
    pub marks: super::bookmark::Bookmarks,
    /// A bookmark to jump to once this play-order position has loaded.
    resume: Option<(usize, f64)>,
    /// Shown in the player bar for a few seconds: "resumed at …".
    pub resumed: Option<(f64, std::time::Instant)>,
    /// When the playing track last changed (the player may still report the old time).
    changed: std::time::Instant,
    /// How loud untagged tracks are (measured in the background, remembered).
    pub loud: super::loudness::Loudness,
    /// The measurements already passed on to mpv's queue (`Loudness::version`).
    loud_seen: u64,
}

impl Player {
    pub fn new(rt: tokio::runtime::Handle, repaint: Arc<dyn Fn() + Send + Sync>) -> Player {
        // Lives as long as the process: mpv's stream callbacks point at it.
        let registry: &'static Registry =
            Box::leak(Box::new(Registry { rt, torrents: Mutex::new(Default::default()), archives: Mutex::new(Default::default()) }));
        Player {
            mpv: None,
            registry,
            live: Default::default(),
            queue: Queue::default(),
            sound: Sound::load(),
            loud: super::loudness::Loudness::new(!cfg!(test), repaint.clone()),
            loud_seen: 0,
            in_mpv: Vec::new(),
            leads: Vec::new(),
            warm_off: false,
            lead_dir: {
                static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
                std::env::temp_dir().join(format!("zentorrent-lead-in-{}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)))
            },
            lead_slot: 0,
            error: None,
            repaint,
            marks: super::bookmark::Bookmarks::load(),
            resume: None,
            resumed: None,
            changed: std::time::Instant::now(),
        }
    }

    pub fn active(&self) -> bool {
        self.queue.pos.is_some()
    }

    fn mpv(&mut self) -> Result<&Mpv, String> {
        // mpv quit by itself since: start a fresh one.
        if std::mem::take(&mut self.live.lock().unwrap().gone) {
            self.mpv = None;
        }
        if self.mpv.is_none() {
            let m = Mpv::new(self.registry, self.live.clone(), self.repaint.clone())?;
            self.mpv = Some(m);
            self.push_sound();
        }
        Ok(self.mpv.as_ref().unwrap())
    }

    /// Where mpv reads a track: the file on disk once complete, else the stream.
    /// Archive members are always read through the stream (mpv can't open archives).
    fn source(&self, track: &Track, complete: bool) -> String {
        if self.queue.hash.is_empty() {
            // A playlist of files on disk (Flux MoE): the path is the whole story.
            track.path.clone()
        } else if let Some((archive, member)) = track.archive {
            stream::member_url(&self.queue.hash, archive, member)
        } else if complete {
            self.queue.folder.join(&track.path).to_string_lossy().into_owned()
        } else {
            stream::url(&self.queue.hash, track.file)
        }
    }

    /// The file on disk behind a music track, when all of it is there: what can be measured.
    fn on_disk(&self, track: &Track, complete: bool) -> Option<PathBuf> {
        if track.archive.is_some() || !matches!(track.kind, super::playlist::Kind::Audio) {
            return None;
        }
        if self.queue.hash.is_empty() {
            Some(PathBuf::from(&track.path))
        } else if complete {
            Some(self.queue.folder.join(&track.path))
        } else {
            None
        }
    }

    /// The per-file option that plays this track at the common level: its measured gain
    /// as mpv's ReplayGain fallback, which mpv uses only for a file without ReplayGain
    /// tags. None: not wanted, or not measured yet (asking starts the measuring).
    fn gain(&self, track: &Track, complete: bool) -> Option<String> {
        if !self.sound.measure || self.sound.replaygain != "track" {
            return None;
        }
        let m = self.loud.lookup(&self.on_disk(track, complete)?)?;
        // This gain comes after the filters: where they already fill the range (an EQ
        // boost, more volume) a quiet track is not raised on top of that.
        Some(format!("replaygain-fallback={:.2}", m.gain_db(!self.sound.can_clip())))
    }

    /// Start a new queue at play-order position `pos`. `complete(file)` says
    /// whether a file is fully downloaded.
    pub fn start(&mut self, queue: Queue, pos: usize, complete: &dyn Fn(usize) -> bool) {
        self.queue = queue;
        if self.queue.shuffle {
            let seed = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos() as u64).unwrap_or(7);
            self.queue.set_shuffle(true, seed);
        }
        self.jump(pos, complete);
    }

    /// What to queue instead of `next` so that the filters keep their memory across the
    /// change from the track at play-order position `cur` (see `warm`). None: nothing has
    /// a memory to keep (no filter is on), or it cannot be done exactly for these two.
    fn warm(&mut self, cur: usize, next: &Track, complete: &dyn Fn(usize) -> bool) -> Option<super::warm::Entry> {
        if self.warm_off || !self.sound.lead_in || self.sound.af().is_empty() {
            return None;
        }
        let prev = self.queue.tracks.get(*self.queue.order.get(cur)?)?;
        let a = self.on_disk(prev, complete(prev.file))?;
        let b = self.on_disk(next, complete(next.file))?;
        std::fs::create_dir_all(&self.lead_dir).ok()?;
        self.lead_slot = self.lead_slot.wrapping_add(1);
        super::warm::entry(&a, &b, &self.lead_dir.join(format!("{}.wav", self.lead_slot % 4)))
    }

    /// Tell the event thread which lead-in each of mpv's entries has.
    fn share_leads(&self) {
        let mut l = self.live.lock().unwrap();
        l.leads = self.leads.clone();
        l.settle();
    }

    /// Put the track after play-order position `cur` behind the playing one in mpv's
    /// queue: with a lead-in when that keeps the filters' memory, else as it is.
    fn append_next(&mut self, cur: usize, complete: &dyn Fn(usize) -> bool) {
        let Some(n) = self.queue.after(cur) else { return };
        let t = self.queue.tracks[self.queue.order[n]].clone();
        let src = self.source(&t, complete(t.file));
        let gain = self.gain(&t, complete(t.file));
        let warm = self.warm(cur, &t, complete);
        let Some(m) = self.mpv.as_ref() else { return };
        let mut lead = None;
        if let Some(e) = warm {
            let opts = match &gain {
                Some(g) => format!("{},{g}", e.opts),
                None => e.opts.clone(),
            };
            // Without its options this entry would play the lead-in out loud: all or nothing.
            if m.load_with(&e.url, "append", &opts).is_ok() {
                lead = Some(e.lead);
            }
        }
        if lead.is_none() && m.load(&src, "append", gain.as_deref()).is_ok() {
            lead = Some(0.0);
        }
        if let Some(lead) = lead {
            self.in_mpv.push(n);
            self.leads.push(lead);
            self.share_leads();
        }
    }

    /// Play the track at play-order position `pos`, and queue the one after it.
    pub fn jump(&mut self, pos: usize, complete: &dyn Fn(usize) -> bool) {
        let Some(&ti) = self.queue.order.get(pos) else { return };
        let track = self.queue.tracks[ti].clone();
        let src = self.source(&track, complete(track.file));
        let gain = self.gain(&track, complete(track.file));
        let result = (|| -> Result<(), String> {
            let m = self.mpv()?;
            m.load(&src, "replace", gain.as_deref())
        })();
        match result {
            Ok(()) => {
                self.error = None;
                self.queue.pos = Some(pos);
                self.changed = std::time::Instant::now();
                self.resumed = None;
                self.resume = self.marks.get(&super::bookmark::key(&self.queue.hash, &track.path)).map(|at| (pos, at));
                self.in_mpv = vec![pos];
                self.leads = vec![0.0];
                if let Ok(mut l) = self.live.lock() {
                    l.mpv_pos = Some(0);
                    l.leads = vec![0.0];
                    l.raw_time = 0.0;
                    l.raw_duration = track.duration.unwrap_or(0.0);
                    l.settle();
                }
                // The next track is queued before playing goes on: from a pause, nothing
                // is heard until mpv knows what follows.
                self.append_next(pos, complete);
                if let Some(Err(e)) = self.mpv.as_ref().map(|m| m.set("pause", "no")) {
                    self.error = Some(e);
                }
            }
            Err(e) => self.error = Some(e),
        }
    }

    /// Called every frame: follow mpv to the next track, queue the one after.
    pub fn tick(&mut self, complete: &dyn Fn(usize) -> bool) {
        // The video window was closed (✕, Q, Esc — already stopped by the event
        // thread), or mpv quit by itself: the queue ends here too.
        let (close, gone) = {
            let mut l = self.live.lock().unwrap();
            (std::mem::take(&mut l.close), std::mem::take(&mut l.gone))
        };
        if gone {
            // Destroying the handle is what takes mpv's window down.
            self.mpv = None;
        }
        if close || gone {
            self.marks.save();
            self.queue.pos = None;
            self.in_mpv.clear();
        self.leads.clear();
            return;
        }
        // A track queued with a lead-in (see `warm`) could not be played: play it the plain
        // way, and make no more lead-ins this run. A step at a track change is better than
        // music that stops.
        let failed = {
            let mut l = self.live.lock().unwrap();
            if l.error.is_some() && l.leads.iter().any(|&lead| lead > 0.0) { l.error.take() } else { None }
        };
        if failed.is_some() {
            self.warm_off = true;
            let at = self.in_mpv.iter().zip(&self.leads).find(|(_, &lead)| lead > 0.0).map(|(&p, _)| p);
            if let Some(p) = at {
                self.jump(p, complete);
            }
            return;
        }
        self.bookmark();
        let pos = self.live.lock().unwrap().mpv_pos;
        let (Some(p), Some(m)) = (pos, self.mpv.as_ref()) else { return };
        let p = p as usize;
        if p == 0 || p >= self.in_mpv.len() {
            // A track was measured meanwhile (or the sound settings changed): queue the next
            // track again, with its level. Not in a track's last seconds, where mpv is
            // already reading the next one for the gapless change.
            let v = self.loud.version();
            let (time, dur) = {
                let l = self.live.lock().unwrap();
                (l.time, l.duration)
            };
            if v != self.loud_seen && (dur <= 0.0 || dur - time > 4.0) {
                self.loud_seen = v;
                self.requeue(complete);
            }
            return;
        }
        // mpv moved on by itself (gapless): drop what's behind, queue the next.
        let now = self.in_mpv[p];
        let lead = self.leads.get(p).copied().unwrap_or(0.0);
        // First what the event thread reads (which entry plays, and its lead-in), then
        // mpv's own list: there is no moment where the two disagree.
        self.in_mpv = vec![now];
        self.leads = vec![lead];
        {
            let mut l = self.live.lock().unwrap();
            l.mpv_pos = Some(0);
            l.leads = vec![lead];
            l.settle();
        }
        let _ = m.cmd(&["playlist-clear"]); // keeps only the playing entry
        self.queue.pos = Some(now);
        self.changed = std::time::Instant::now();
        let path = self.queue.tracks[self.queue.order[now]].path.clone();
        self.resume = self.marks.get(&super::bookmark::key(&self.queue.hash, &path)).map(|at| (now, at));
        self.append_next(now, complete);
    }

    pub fn toggle_pause(&mut self) {
        if let Some(m) = &self.mpv {
            let _ = m.cmd(&["cycle", "pause"]);
        }
    }

    pub fn next(&mut self, complete: &dyn Fn(usize) -> bool) {
        if let Some(n) = self.queue.pos.and_then(|p| self.queue.after(p)) {
            self.jump(n, complete);
        }
    }

    pub fn prev(&mut self, complete: &dyn Fn(usize) -> bool) {
        // More than 3 s in: back to the start of this track, like every player.
        if self.live.lock().unwrap().time > 3.0 {
            self.seek(0.0);
        } else if let Some(p) = self.queue.pos.and_then(|p| self.queue.before(p)) {
            self.jump(p, complete);
        }
    }

    pub fn seek(&mut self, secs: f64) {
        if let Some(m) = &self.mpv {
            // mpv's clock for an entry with a lead-in starts that much earlier.
            let lead = {
                let l = self.live.lock().unwrap();
                l.mpv_pos.and_then(|p| l.leads.get(p as usize).copied()).unwrap_or(0.0)
            };
            let _ = m.cmd(&["seek", &format!("{:.2}", secs + lead), "absolute"]);
        }
    }

    /// Note where the playing file is, and jump to its bookmark once it has loaded.
    fn bookmark(&mut self) {
        let Some(pos) = self.queue.pos else { return };
        let Some(t) = self.queue.current() else { return };
        let key = super::bookmark::key(&self.queue.hash, &t.path);
        let video = matches!(t.kind, super::playlist::Kind::Video);
        let (time, dur, paused) = {
            let l = self.live.lock().unwrap();
            (l.time, l.duration, l.paused)
        };
        let since = self.changed.elapsed();
        if let Some((p, at)) = self.resume {
            // This file has loaded (its length is known, it is at its start): carry on there.
            if p == pos && since > std::time::Duration::from_millis(300) && dur > 0.0 && time < 5.0 {
                self.resume = None;
                if at < dur - 1.0 {
                    self.seek(at);
                    self.resumed = Some((at, std::time::Instant::now()));
                }
            }
            return; // nothing is noted before the jump
        }
        // Right after a change the old file's time may still be reported.
        if since > std::time::Duration::from_secs(2) && !paused && dur > 0.0 && time <= dur {
            self.marks.note(&key, time, dur, video);
        }
        self.marks.maybe_save();
    }

    /// "Start over": back to the beginning, and the bookmark goes.
    pub fn start_over(&mut self) {
        if let Some(t) = self.queue.current() {
            let key = super::bookmark::key(&self.queue.hash, &t.path);
            self.marks.forget(&key);
        }
        self.resumed = None;
        self.seek(0.0);
        self.marks.save();
    }

    pub fn stop(&mut self) {
        self.marks.save();
        if let Some(m) = &self.mpv {
            let _ = m.cmd(&["stop"]);
        }
        self.queue.pos = None;
        self.in_mpv.clear();
        self.leads.clear();
    }

    /// Push the sound settings to mpv and save them.
    pub fn apply_sound(&mut self) {
        self.sound.save();
        self.push_sound();
        // The next track's level depends on these settings: `tick` queues it again.
        self.loud_seen = u64::MAX;
    }

    fn push_sound(&mut self) {
        let Some(m) = &self.mpv else { return };
        let s = &self.sound;
        let r = [
            m.set("volume", &format!("{:.0}", s.mpv_volume())),
            m.set("replaygain", &s.replaygain),
            m.set("speed", &format!("{:.2}", s.speed)),
            // An FFmpeg from before 5.1 does not know the limiter's `latency`: then rather
            // the limiter with its 5 ms hole at a track change than no limiter at all.
            m.set("af", &s.af()).or_else(|_| m.set("af", &s.af().replace(":latency=1", ""))),
        ];
        self.error = r.into_iter().find_map(Result::err).map(|e| format!("sound settings: {e}"));
    }

    /// Re-queue the track after the playing one (after shuffle/repeat/reorder changes).
    pub fn requeue(&mut self, complete: &dyn Fn(usize) -> bool) {
        let (Some(cur), Some(m)) = (self.queue.pos, self.mpv.as_ref()) else { return };
        let at = self.live.lock().unwrap().mpv_pos.unwrap_or(0) as usize;
        let lead = self.leads.get(at).copied().unwrap_or(0.0);
        self.in_mpv = vec![cur];
        self.leads = vec![lead];
        {
            let mut l = self.live.lock().unwrap();
            l.mpv_pos = Some(0);
            l.leads = vec![lead];
            l.settle();
        }
        let _ = m.cmd(&["playlist-clear"]);
        self.append_next(cur, complete);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::player::playlist::Kind;

    fn queue(n: usize) -> Queue {
        let tracks = (0..n)
            .map(|i| Track::file(i, &format!("A/{i}.flac"), format!("T{i}"), None, Kind::Audio))
            .collect();
        Queue { tracks, order: (0..n).collect(), pos: Some(0), ..Default::default() }
    }

    #[test]
    fn next_and_previous_with_repeat() {
        let mut q = queue(3);
        assert_eq!((q.after(0), q.after(2), q.before(0)), (Some(1), None, None));
        q.repeat = Repeat::All;
        assert_eq!((q.after(2), q.before(0)), (Some(0), Some(2)));
        q.repeat = Repeat::One;
        assert_eq!(q.after(1), Some(1));
    }

    #[test]
    fn shuffle_keeps_the_playing_track_and_every_track() {
        let mut q = queue(20);
        q.pos = Some(7);
        q.set_shuffle(true, 12345);
        assert_eq!(q.current().unwrap().file, 7, "the playing track keeps playing");
        let mut sorted = q.order.clone();
        sorted.sort();
        assert_eq!(sorted, (0..20).collect::<Vec<_>>(), "nothing lost or doubled");
        assert_ne!(q.order, (0..20).collect::<Vec<_>>(), "actually shuffled");
        q.set_shuffle(false, 0);
        assert_eq!(q.order, (0..20).collect::<Vec<_>>());
        assert_eq!(q.current().unwrap().file, 7);
    }

    #[test]
    fn reorder_keeps_the_playing_track() {
        let mut q = queue(5);
        q.pos = Some(2);
        q.move_item(4, 0);
        assert_eq!(q.order, [4, 0, 1, 2, 3]);
        assert_eq!(q.current().unwrap().file, 2);
    }

    #[test]
    fn sound_filter_chain() {
        let mut s = Sound::default();
        assert_eq!(s.af(), "", "flat = no filters at all");
        s.eq[0] = 5.0;
        s.eq[9] = -2.0;
        s.crossfeed = true;
        let af = s.af();
        assert!(af.starts_with("@eq0:lavfi=[equalizer=f=31:width_type=o:width=1:gain=5.0]"), "{af}");
        assert!(af.contains("@eq9:lavfi=[equalizer=f=16000") && af.contains("gain=-2.0]"));
        assert!(af.contains("@xfeed") && af.ends_with("@limit:lavfi=[alimiter=limit=0.97:level=0:latency=1]"), "a boost gets a limiter that keeps its ceiling: {af}");
        s.eq = [-3.0; 10];
        assert!(!s.af().contains("@limit"), "cuts can't clip");
        // Everything else that can go over full scale gets the limiter too, as the last filter.
        let wide = Sound { width: 2.0, ..Default::default() };
        assert_eq!(wide.af(), "@wide:lavfi=[extrastereo=m=2.00:c=0],@limit:lavfi=[alimiter=limit=0.97:level=0:latency=1]");
        let narrow = Sound { width: 0.5, ..Default::default() };
        assert_eq!(narrow.af(), "@wide:lavfi=[extrastereo=m=0.50:c=0]", "narrower can't clip");
        let loud = Sound { volume: 130.0, ..Default::default() };
        assert_eq!(loud.af(), "@gain:lavfi=[volume=2.197],@limit:lavfi=[alimiter=limit=0.97:level=0:latency=1]");
        assert_eq!((loud.mpv_volume(), Sound { volume: 80.0, ..Default::default() }.mpv_volume()), (100.0, 80.0), "mpv's own volume stops at 100 %");
        assert_eq!((Sound { volume: 100.0, ..Default::default() }.af(), Sound { volume: 60.0, ..Default::default() }.af()), (String::new(), String::new()));
        let back: Sound = serde_json::from_str(r#"{"volume": 80}"#).unwrap();
        assert_eq!((back.volume, back.speed, back.replaygain.as_str()), (80.0, 1.0, "track"), "old/partial settings files load");
    }

    /// 3 s of a near-full-scale stereo track (16-bit WAV): bass both sides, and a 440 Hz
    /// tone that differs between the sides, so a bass boost and a wider image both add to it.
    fn loud_wav(path: &std::path::Path) {
        let (rate, secs) = (44_100u32, 3u32);
        let mut data = Vec::new();
        for n in 0..rate * secs {
            let t = n as f64 / rate as f64;
            let bass = 0.6 * (std::f64::consts::TAU * 62.0 * t).sin();
            let l = bass + 0.37 * (std::f64::consts::TAU * 440.0 * t).sin();
            let r = bass - 0.37 * (std::f64::consts::TAU * 440.0 * t + 1.0).sin();
            for v in [l, r] {
                data.extend_from_slice(&((v * 32767.0) as i16).to_le_bytes());
            }
        }
        let mut w = Vec::new();
        w.extend_from_slice(b"RIFF");
        w.extend_from_slice(&(36 + data.len() as u32).to_le_bytes());
        w.extend_from_slice(b"WAVEfmt ");
        for v in [16u32.to_le_bytes().to_vec(), 1u16.to_le_bytes().to_vec(), 2u16.to_le_bytes().to_vec(), rate.to_le_bytes().to_vec(),
                  (rate * 4).to_le_bytes().to_vec(), 4u16.to_le_bytes().to_vec(), 16u16.to_le_bytes().to_vec()] {
            w.extend_from_slice(&v);
        }
        w.extend_from_slice(b"data");
        w.extend_from_slice(&(data.len() as u32).to_le_bytes());
        w.extend_from_slice(&data);
        std::fs::write(path, w).unwrap();
    }

    /// The highest sample (1.0 = full scale) in a WAV mpv wrote, and how many samples it has.
    fn peak_of(path: &std::path::Path) -> (f32, usize) {
        let b = std::fs::read(path).unwrap();
        let u16at = |i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
        let u32at = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        let (mut at, mut tag, mut bits) = (12, 1u16, 16u16);
        while at + 8 <= b.len() {
            let (id, len, body) = (&b[at..at + 4], u32at(at + 4) as usize, at + 8);
            if id == b"fmt " {
                tag = u16at(body);
                bits = u16at(body + 14);
                if tag == 0xFFFE {
                    tag = u16at(body + 24); // WAVE_FORMAT_EXTENSIBLE: the real format is in the sub-format
                }
            }
            if id == b"data" {
                let end = if len == 0 { b.len() } else { body.saturating_add(len).min(b.len()) };
                let samples: Vec<f32> = match (tag, bits) {
                    (3, 32) => b[body..end].chunks_exact(4).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect(),
                    (1, 16) => b[body..end].chunks_exact(2).map(|c| i16::from_le_bytes([c[0], c[1]]) as f32 / 32768.0).collect(),
                    (1, 32) => b[body..end].chunks_exact(4).map(|c| i32::from_le_bytes([c[0], c[1], c[2], c[3]]) as f32 / 2147483648.0).collect(),
                    other => panic!("a WAV format this test does not read: {other:?}"),
                };
                return (samples.iter().fold(0.0f32, |m, v| m.max(v.abs())), samples.len());
            }
            at = body + len + (len & 1);
        }
        panic!("no audio in {}", path.display());
    }

    /// A stereo 16-bit WAV: `n` frames of a sine, as if cut out of one long tone at frame
    /// `from` — so two of these in a row continue each other exactly.
    fn tone_wav(path: &std::path::Path, rate: u32, from: u32, n: u32, hz: f64, amp: f64) {
        let mut w = Vec::new();
        let bytes = n * 4;
        w.extend_from_slice(b"RIFF");
        w.extend_from_slice(&(36 + bytes).to_le_bytes());
        w.extend_from_slice(b"WAVEfmt ");
        for v in [16u32.to_le_bytes().to_vec(), 1u16.to_le_bytes().to_vec(), 2u16.to_le_bytes().to_vec(), rate.to_le_bytes().to_vec(),
                  (rate * 4).to_le_bytes().to_vec(), 4u16.to_le_bytes().to_vec(), 16u16.to_le_bytes().to_vec()] {
            w.extend_from_slice(&v);
        }
        w.extend_from_slice(b"data");
        w.extend_from_slice(&bytes.to_le_bytes());
        for i in from..from + n {
            let v = ((amp * (std::f64::consts::TAU * hz * i as f64 / rate as f64).sin()) * 32767.0).round() as i16;
            w.extend_from_slice(&v.to_le_bytes());
            w.extend_from_slice(&v.to_le_bytes());
        }
        std::fs::write(path, w).unwrap();
    }

    /// The left channel of a float WAV mpv wrote, and its sample rate.
    fn left_of(path: &std::path::Path) -> (Vec<f32>, u32) {
        let b = std::fs::read(path).unwrap();
        let u16at = |i: usize| u16::from_le_bytes([b[i], b[i + 1]]);
        let u32at = |i: usize| u32::from_le_bytes([b[i], b[i + 1], b[i + 2], b[i + 3]]);
        let (mut at, mut ch, mut rate, mut bits) = (12, 2usize, 0u32, 0u16);
        while at + 8 <= b.len() {
            let (id, len, body) = (&b[at..at + 4], u32at(at + 4) as usize, at + 8);
            if id == b"fmt " {
                ch = u16at(body + 2) as usize;
                rate = u32at(body + 4);
                bits = u16at(body + 14);
            }
            if id == b"data" {
                assert_eq!(bits, 32, "float samples were asked for");
                let end = if len == 0 { b.len() } else { body.saturating_add(len).min(b.len()) };
                let left = b[body..end].chunks_exact(4 * ch).map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]])).collect();
                return (left, rate);
            }
            at = body + len + (len & 1);
        }
        panic!("no audio in {}", path.display());
    }

    /// A lead-in of a second (and a quarter of a sample).
    fn led(lead: f64) -> bool {
        lead > 1.0 && lead < 1.001
    }

    fn render(out: &std::path::Path, sound: Sound, files: &[PathBuf], frames: u64, measured: bool) -> (Vec<f32>, u32) {
        let (left, rate, _) = render_led(out, sound, files, frames, measured);
        (left, rate)
    }

    /// Play `files` as a queue through the real player (libmpv, writing `frames` frames
    /// to `out` as fast as it decodes) and return what came out: left channel, sample
    /// rate, and whether the second track was queued with a lead-in.
    fn render_led(out: &std::path::Path, sound: Sound, files: &[PathBuf], frames: u64, measured: bool) -> (Vec<f32>, u32, bool) {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let mut p = Player::new(rt.handle().clone(), Arc::new(|| {}));
        p.sound = sound;
        if measured {
            for f in files {
                p.loud.measure_now(f).unwrap();
            }
        }
        {
            let m = p.mpv().expect("the player starts");
            m.set("ao", "pcm").unwrap();
            m.set("ao-pcm-file", &out.to_string_lossy()).unwrap();
            m.set("audio-format", "float").unwrap();
            // This writes as fast as it decodes: a short first track could be over before
            // the second is queued. From a pause, the queue is complete before it starts.
            m.set("pause", "yes").unwrap();
        }
        let tracks: Vec<Track> = files.iter().enumerate().map(|(i, f)| Track::file(i, &f.to_string_lossy(), format!("T{i}"), None, Kind::Audio)).collect();
        let n = tracks.len();
        p.start(Queue { tracks, order: (0..n).collect(), ..Default::default() }, 0, &|_| true);
        assert_eq!(p.error, None);
        let with_lead_in = p.leads.get(1).is_some_and(|&l| led(l));
        let t0 = std::time::Instant::now();
        loop {
            p.tick(&|_| true);
            let (idle, err) = {
                let l = p.live.lock().unwrap();
                (l.idle, l.error.clone())
            };
            assert_eq!(err, None);
            let size = std::fs::metadata(out).map(|m| m.len()).unwrap_or(0);
            if idle && size >= frames * 8 * 9 / 10 {
                break;
            }
            assert!(t0.elapsed() < std::time::Duration::from_secs(60), "the player did not finish ({size} bytes written)");
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        drop(p); // closes the file
        let (left, rate) = left_of(out);
        (left, rate, with_lead_in)
    }

    fn scratch(name: &str) -> Option<PathBuf> {
        if ffi::api().is_err() {
            eprintln!("libmpv is not on this machine: not tested here");
            return None;
        }
        let dir = std::env::temp_dir().join(format!("zt-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        Some(dir)
    }

    /// Gapless, measured in the real player: one tone cut into two files at an odd place
    /// comes out as the one tone again — every sample where it belongs, nothing added,
    /// nothing lost — plain, and with the EQ, widening and limiter in the chain.
    #[test]
    fn two_tracks_in_a_row_play_without_a_gap_in_the_real_player() {
        let Some(dir) = scratch("gapless") else { return };
        let (rate, cut, total, hz, amp) = (44_100u32, 88_211u32, 176_400u32, 440.0, 0.5);
        let (a, b) = (dir.join("a.wav"), dir.join("b.wav"));
        tone_wav(&a, rate, 0, cut, hz, amp);
        tone_wav(&b, rate, cut, total - cut, hz, amp);
        let quiet = Sound { replaygain: "no".into(), measure: false, ..Default::default() };

        let (left, got_rate) = render(&dir.join("plain.wav"), quiet.clone(), &[a.clone(), b.clone()], total as u64, false);
        assert_eq!(got_rate, rate);
        assert_eq!(left.len(), total as usize, "not one frame more or less than the two files hold");
        let worst = left.iter().enumerate().map(|(n, &x)| (x as f64 - amp * (std::f64::consts::TAU * hz * n as f64 / rate as f64).sin()).abs()).fold(0.0, f64::max);
        assert!(worst < 1e-3, "the tone carries on across the files exactly: worst sample is {worst} off");

        // With filters the level and phase move, so look for the gap itself: silence at
        // the change, or a jump between two neighbouring samples bigger than this tone makes.
        // None is left by anything: the limiter used to leave 5 ms of silence, and the
        // equalizer a step 3.6 times the tone's own (it began every file with an empty
        // memory; now it hears the previous track's last second first, see `warm`).
        let mut eq = [0.0; 10];
        eq[..4].copy_from_slice(&[6.0, 5.0, 4.0, 2.0]);
        for (name, sound, most) in [
            ("limiter", Sound { volume: 130.0, ..quiet.clone() }, 1.05),
            ("wide", Sound { width: 1.5, ..quiet.clone() }, 1.05),
            ("bass", Sound { eq, ..quiet.clone() }, 1.05),
            ("everything", Sound { eq, width: 1.5, crossfeed: true, volume: 110.0, normalize: true, ..quiet.clone() }, 1.05),
        ] {
            let (left, _) = render(&dir.join(format!("{name}.wav")), sound, &[a.clone(), b.clone()], total as u64, false);
            assert_eq!(left.len(), total as usize, "{name}: not one frame more or less");
            let around = &left[cut as usize - 2000..cut as usize + 2000];
            let (mut run, mut silent) = (0usize, 0usize);
            for v in around {
                run = if v.abs() < 1e-4 { run + 1 } else { 0 };
                silent = silent.max(run);
            }
            assert!(silent <= 2, "{name}: {silent} frames of silence at the track change");
            let body = &left[rate as usize / 5..left.len() - rate as usize / 5];
            let peak = body.iter().fold(0.0f32, |m, v| m.max(v.abs()));
            let step = body.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0f32, f32::max);
            let own = peak * (std::f32::consts::TAU * hz as f32 / rate as f32);
            assert!(step <= own * most, "{name}: a jump of {step} between two samples (the tone's own is {own})");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The lead-in at the sample rates and track lengths where every other way of doing it
    /// was a sample off (2026-11-27: lengths that are a round time, like exactly 2 s, and
    /// every rate outside the 44.1 kHz family). Two checks in the real player: a filter that
    /// leaves this signal as it is (narrowing, on a tone that is the same on both sides), so
    /// every sample can be compared; and the Bass preset, where there must be no step.
    #[test]
    fn a_lead_in_is_exact_at_any_sample_rate_and_track_length() {
        let Some(dir) = scratch("grid") else { return };
        let (hz, amp) = (440.0, 0.25);
        let mut eq = [0.0; 10];
        eq[..4].copy_from_slice(&[6.0, 5.0, 4.0, 2.0]);
        let quiet = Sound { replaygain: "no".into(), measure: false, ..Default::default() };
        for rate in [22_050u32, 44_100, 48_000, 96_000] {
            for (cut, len) in [(2 * rate, rate), (2 * rate, 2 * rate), (2 * rate - 11, 2 * rate), (2 * rate, 2 * rate + 11), (3 * rate + 1, (1.37 * rate as f64) as u32 + 1)] {
                let (a, b, total) = (dir.join("a.wav"), dir.join("b.wav"), cut + len);
                tone_wav(&a, rate, 0, cut, hz, amp);
                tone_wav(&b, rate, cut, len, hz, amp);
                let what = format!("{rate} Hz, {cut} + {len} frames");

                let (left, got, with) = render_led(&dir.join("same.wav"), Sound { width: 0.5, ..quiet.clone() }, &[a.clone(), b.clone()], total as u64, false);
                assert!(with && got == rate, "{what}: a lead-in is used");
                assert_eq!(left.len(), total as usize, "{what}: not one frame more or less");
                let worst = left.iter().enumerate().map(|(n, &x)| (x as f64 - amp * (std::f64::consts::TAU * hz * n as f64 / rate as f64).sin()).abs()).fold(0.0, f64::max);
                assert!(worst < 1e-3, "{what}: every sample where it belongs (worst is {worst} off)");

                let (left, _, with) = render_led(&dir.join("bass.wav"), Sound { eq, ..quiet.clone() }, &[a, b], total as u64, false);
                assert!(with, "{what}: a lead-in is used");
                assert_eq!(left.len(), total as usize, "{what}, Bass preset: not one frame more or less");
                let body = &left[rate as usize / 5..left.len() - rate as usize / 5];
                let peak = body.iter().fold(0.0f32, |m, v| m.max(v.abs()));
                let step = body.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0f32, f32::max);
                let own = peak * (std::f32::consts::TAU * hz as f32 / rate as f32);
                assert!(step <= own * 1.05, "{what}, Bass preset: a jump of {step} between two samples (the tone's own is {own})");
            }
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// FLAC (made with ffmpeg, when it is here), 16 and 24 bit: with a lead-in the next
    /// track is still itself — its own ReplayGain and its own title, not those of the
    /// second of the track before that mpv is given first — and there is no step.
    #[test]
    fn flac_tracks_with_a_lead_in_keep_their_own_tags() {
        let Some(dir) = scratch("flac") else { return };
        let ffmpeg = |args: &[&str]| std::process::Command::new("ffmpeg").args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y"]).args(args).status().map(|s| s.success()).unwrap_or(false);
        if !ffmpeg(&["-version"]) {
            eprintln!("ffmpeg is not on this machine: FLAC not tested here");
            return;
        }
        let (rate, cut, total, hz, amp) = (44_100u32, 88_211u32, 176_400u32, 440.0, 0.25);
        let (wa, wb) = (dir.join("a.wav"), dir.join("b.wav"));
        tone_wav(&wa, rate, 0, cut, hz, amp);
        tone_wav(&wb, rate, cut, total - cut, hz, amp);
        let mut eq = [0.0; 10];
        eq[..4].copy_from_slice(&[6.0, 5.0, 4.0, 2.0]);
        for bits in ["16", "24"] {
            let (a, b) = (dir.join(format!("a{bits}.flac")), dir.join(format!("b{bits}.flac")));
            for (src, dst, gain, title) in [(&wa, &a, "-6.02 dB", "First"), (&wb, &b, "+6.02 dB", "Second")] {
                let fmt = if bits == "24" { "s32" } else { "s16" };
                assert!(ffmpeg(&["-i", &src.to_string_lossy(), "-sample_fmt", fmt, "-bits_per_raw_sample", bits, "-metadata", &format!("REPLAYGAIN_TRACK_GAIN={gain}"),
                    "-metadata", "REPLAYGAIN_TRACK_PEAK=0.250000", "-metadata", &format!("TITLE={title}"), &dst.to_string_lossy()]));
            }
            assert_eq!(crate::player::warm::lossless(&a).map(|l| l.wide), Some(bits == "24"), "{bits}-bit FLAC");

            // No step, not a frame off, with the Bass preset (ReplayGain off, so the level stays).
            let sound = Sound { eq, replaygain: "no".into(), measure: false, ..Default::default() };
            let (left, _, with) = render_led(&dir.join("eq.wav"), sound, &[a.clone(), b.clone()], total as u64, false);
            assert!(with, "{bits}-bit FLAC: a lead-in is used");
            assert_eq!(left.len(), total as usize, "{bits}-bit FLAC: not one frame more or less");
            let body = &left[rate as usize / 5..left.len() - rate as usize / 5];
            let peak = body.iter().fold(0.0f32, |m, v| m.max(v.abs()));
            let step = body.windows(2).map(|w| (w[1] - w[0]).abs()).fold(0.0f32, f32::max);
            let own = peak * (std::f32::consts::TAU * hz as f32 / rate as f32);
            assert!(step <= own * 1.05, "{bits}-bit FLAC: a jump of {step} between two samples (the tone's own is {own})");

            // Each track at its own ReplayGain: the first 6 dB down, the second 6 dB up. A cut
            // at 16 kHz is the filter here: it leaves a 440 Hz tone alone.
            let mut cut_high = [0.0; 10];
            cut_high[9] = -3.0;
            let (left, _, with) = render_led(&dir.join("rg.wav"), Sound { eq: cut_high, measure: false, ..Default::default() }, &[a, b], total as u64, false);
            assert!(with);
            let level = |t: f64| left[(t * rate as f64) as usize..][..441].iter().fold(0.0f32, |m, v| m.max(v.abs()));
            assert!((level(1.0) / 0.125 - 1.0).abs() < 0.05 && (level(3.0) / 0.5 - 1.0).abs() < 0.05, "{bits}-bit FLAC: levels {} then {} (wanted 0.125 then 0.5)", level(1.0), level(3.0));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A track queued with a lead-in is still that track to everything else: its clock
    /// starts at 0 and runs its own length, a seek lands where it was asked to, its name
    /// is its own — and the queue goes on after it, with a lead-in for the next again.
    #[test]
    fn a_track_with_a_lead_in_keeps_its_own_clock_and_name() {
        let Some(dir) = scratch("clock") else { return };
        let files: Vec<PathBuf> = (0..3u32).map(|i| dir.join(format!("{i}.wav"))).collect();
        for (i, f) in files.iter().enumerate() {
            tone_wav(f, 44_100, 0, 2 * 44_100, 300.0 + 100.0 * i as f64, 0.3);
        }
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let mut p = Player::new(rt.handle().clone(), Arc::new(|| {}));
        let mut eq = [0.0; 10];
        eq[1] = 5.0;
        p.sound = Sound { eq, measure: false, ..Default::default() };
        p.mpv().expect("the player starts").set("ao", "null").unwrap();
        let tracks: Vec<Track> = files.iter().enumerate().map(|(i, f)| Track::file(i, &f.to_string_lossy(), format!("T{i}"), None, Kind::Audio)).collect();
        p.start(Queue { tracks, order: vec![0, 1, 2], ..Default::default() }, 0, &|_| true);
        assert!(p.leads.len() == 2 && p.leads[0] == 0.0 && led(p.leads[1]), "the second track is queued with a lead-in: {:?}", p.leads);
        let (t0, mut seen, mut playing, mut sought) = (std::time::Instant::now(), vec![0usize], false, None);
        // (time, length, name, was this 0.25 s or more after the seek?)
        let mut second: Vec<(f64, f64, String, bool)> = Vec::new();
        loop {
            p.tick(&|_| true);
            let (idle, time, dur, title) = {
                let l = p.live.lock().unwrap();
                (l.idle, l.time, l.duration, l.title.clone())
            };
            playing |= dur > 0.0 && !idle;
            if let Some(pos) = p.queue.pos.filter(|pos| seen.last() != Some(pos)) {
                seen.push(pos);
                if pos == 1 {
                    assert!(p.leads.len() == 2 && p.leads.iter().all(|&l| led(l)), "and the third one too: {:?}", p.leads);
                }
            }
            // The second track, once it has settled in (0.3 s after the change).
            if p.queue.pos == Some(1) && p.changed.elapsed() > std::time::Duration::from_millis(300) && dur > 0.0 {
                let settled = sought.is_some_and(|s: std::time::Instant| s.elapsed() > std::time::Duration::from_millis(250));
                second.push((time, dur, title, settled));
                if sought.is_none() {
                    p.seek(1.2);
                    sought = Some(std::time::Instant::now());
                }
            }
            if playing && idle {
                break;
            }
            assert!(t0.elapsed() < std::time::Duration::from_secs(30), "stuck after {seen:?}");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(seen, [0, 1, 2]);
        let (first_time, dur, title, _) = second.first().cloned().expect("the second track was seen playing");
        assert!((dur - 2.0).abs() < 0.01, "its own length, not the lead-in's on top: {dur}");
        assert!((0.2..0.9).contains(&first_time), "its own time: {first_time} s, 0.3 s in");
        assert_eq!(title, "1.wav");
        // After the seek to 1.2 s: it carries on from there (never before it, never past its end).
        let after: Vec<f64> = second.iter().filter(|s| s.3).map(|s| s.0).collect();
        assert!(!after.is_empty() && after.iter().all(|t| (1.15..=2.0).contains(t)), "after seeking to 1.2 s: {:?}", &after[..after.len().min(8)]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A lead-in entry that mpv cannot play must not stop the music: the track is played
    /// the plain way, and lead-ins are left alone for the rest of the run.
    #[test]
    fn a_lead_in_that_fails_falls_back_to_the_plain_track() {
        let Some(dir) = scratch("fallback") else { return };
        let files: Vec<PathBuf> = (0..3u32).map(|i| dir.join(format!("{i}.wav"))).collect();
        for (i, f) in files.iter().enumerate() {
            tone_wav(f, 44_100, 0, 2 * 44_100, 300.0 + 100.0 * i as f64, 0.3);
        }
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let mut p = Player::new(rt.handle().clone(), Arc::new(|| {}));
        let mut eq = [0.0; 10];
        eq[1] = 5.0;
        p.sound = Sound { eq, measure: false, ..Default::default() };
        p.mpv().expect("the player starts").set("ao", "null").unwrap();
        let tracks: Vec<Track> = files.iter().enumerate().map(|(i, f)| Track::file(i, &f.to_string_lossy(), format!("T{i}"), None, Kind::Audio)).collect();
        p.start(Queue { tracks, order: vec![0, 1, 2], ..Default::default() }, 0, &|_| true);
        assert!(p.leads.len() == 2 && led(p.leads[1]), "{:?}", p.leads);
        // What mpv reports when an entry will not play.
        p.live.lock().unwrap().error = Some("could not play this track: no audio or video data played".into());
        p.tick(&|_| true);
        assert!(p.warm_off);
        assert_eq!((p.queue.pos, p.error.clone()), (Some(1), None), "the track that had the lead-in plays, as it is");
        assert_eq!(p.leads, [0.0, 0.0], "and the one after it is queued as it is too");
        let (t0, mut seen, mut playing) = (std::time::Instant::now(), vec![1usize], false);
        loop {
            p.tick(&|_| true);
            let (idle, dur) = {
                let l = p.live.lock().unwrap();
                (l.idle, l.duration)
            };
            playing |= dur > 0.0 && !idle;
            if let Some(pos) = p.queue.pos.filter(|pos| seen.last() != Some(pos)) {
                seen.push(pos);
            }
            if playing && idle {
                break;
            }
            assert!(t0.elapsed() < std::time::Duration::from_secs(30), "stuck after {seen:?}");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(seen, [1, 2], "and the queue goes on");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The queue keeps going past the two tracks mpv holds: with real timing (no sound
    /// card, but the clock of one), three tracks play one after the other, in order.
    #[test]
    fn the_queue_carries_on_track_after_track() {
        let Some(dir) = scratch("queue") else { return };
        let files: Vec<PathBuf> = (0..3u32).map(|i| dir.join(format!("{i}.wav"))).collect();
        for (i, f) in files.iter().enumerate() {
            tone_wav(f, 44_100, 0, 44_100, 300.0 + 100.0 * i as f64, 0.3);
        }
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let mut p = Player::new(rt.handle().clone(), Arc::new(|| {}));
        p.sound = Sound { measure: false, ..Default::default() };
        p.mpv().expect("the player starts").set("ao", "null").unwrap();
        let tracks: Vec<Track> = files.iter().enumerate().map(|(i, f)| Track::file(i, &f.to_string_lossy(), format!("T{i}"), None, Kind::Audio)).collect();
        p.start(Queue { tracks, order: vec![0, 1, 2], ..Default::default() }, 0, &|_| true);
        let (t0, mut seen, mut playing) = (std::time::Instant::now(), vec![0usize], false);
        loop {
            p.tick(&|_| true);
            let (idle, dur) = {
                let l = p.live.lock().unwrap();
                (l.idle, l.duration)
            };
            playing |= dur > 0.0 && !idle;
            if let Some(pos) = p.queue.pos.filter(|pos| seen.last() != Some(pos)) {
                seen.push(pos);
            }
            if playing && idle {
                break;
            }
            assert!(t0.elapsed() < std::time::Duration::from_secs(30), "stuck after {seen:?}");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(seen, [0, 1, 2]);
        let took = t0.elapsed().as_secs_f64();
        assert!((2.6..6.0).contains(&took), "three 1 s tracks took {took:.1} s");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Measured loudness in the real player: a loud and a quiet track without ReplayGain
    /// tags come out at the same level (-18 LUFS; for a 1 kHz tone that is a peak of
    /// 0.126), and the change of gain between them does not land on the wrong track.
    #[test]
    fn untagged_tracks_come_out_at_the_same_level_in_the_real_player() {
        let Some(dir) = scratch("level") else { return };
        let (rate, each) = (44_100u32, 88_200u32);
        let (a, b) = (dir.join("loud.wav"), dir.join("quiet.wav"));
        tone_wav(&a, rate, 0, each, 1000.0, 0.5);
        tone_wav(&b, rate, each, each, 1000.0, 0.05);
        let peaks = |left: &[f32]| -> Vec<f32> { left.chunks(rate as usize / 100).map(|c| c.iter().fold(0.0f32, |m, v| m.max(v.abs()))).collect() };

        let off = Sound { measure: false, ..Default::default() };
        let (left, _) = render(&dir.join("off.wav"), off, &[a.clone(), b.clone()], 2 * each as u64, true);
        let p = peaks(&left);
        assert!((p[50] - 0.5).abs() < 0.01 && (p[350] - 0.05).abs() < 0.002, "as recorded when it is off: {} then {}", p[50], p[350]);

        let (left, _) = render(&dir.join("on.wav"), Sound::default(), &[a, b], 2 * each as u64, true);
        let p = peaks(&left);
        let want = 10f32.powf(-18.0 / 20.0);
        assert!((p[50] / want - 1.0).abs() < 0.04, "the loud track came down to {} (wanted {want})", p[50]);
        assert!((p[350] / want - 1.0).abs() < 0.04, "the quiet track came up to {} (wanted {want})", p[350]);
        // Around the change (2.0 s): the loud track's end must not get the quiet track's
        // +8 dB, nor the quiet track's start the loud one's -12 dB.
        let wrong: Vec<(usize, f32)> = p.iter().copied().enumerate().skip(5).take(390).filter(|(_, v)| (v / want - 1.0).abs() > 0.25).collect();
        assert!(wrong.len() <= 3, "{} hundredths of a second at the wrong level around the change: {:?}", wrong.len(), &wrong[..wrong.len().min(12)]);

        // With a filter on, the second track is queued with a lead-in (see `warm`): its
        // gain must still be its own, and none of the loud lead-in may be heard.
        let mut eq = [0.0; 10];
        eq[5] = -3.0;
        let (left, _) = render(&dir.join("eq.wav"), Sound { eq, ..Default::default() }, &[dir.join("loud.wav"), dir.join("quiet.wav")], 2 * each as u64, true);
        assert_eq!(left.len(), 2 * each as usize);
        let p = peaks(&left);
        let level = p[50];
        assert!(level > want * 0.5 && level < want, "1 kHz cut by 3 dB from {want}: {level}");
        let wrong: Vec<(usize, f32)> = p.iter().copied().enumerate().skip(5).take(390).filter(|(_, v)| (v / level - 1.0).abs() > 0.25).collect();
        assert!(wrong.len() <= 3, "with a lead-in: {} hundredths of a second at the wrong level: {:?}", wrong.len(), &wrong[..wrong.len().min(12)]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The real player (libmpv, writing to a file instead of the sound card): with the
    /// loudest settings the Sound window offers, a full-scale track stays under full scale.
    /// 2026-11-27, measured with the same filters in ffmpeg before this fix: volume 130 %
    /// was 6.7 dB over, width 2.0 hard-clipped, and the EQ's limiter let peaks back up to 0 dB.
    #[test]
    fn the_loudest_settings_do_not_clip_in_the_real_player() {
        if ffi::api().is_err() {
            eprintln!("libmpv is not on this machine: not tested here");
            return;
        }
        std::env::set_var("ZENTORRENT_MPV_AO", "pcm");
        let dir = std::env::temp_dir().join(format!("zt-loud-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("loud.wav");
        loud_wav(&src);
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let play = |sound: Sound, name: &str| -> f32 {
            let out = dir.join(name);
            let mut p = Player::new(rt.handle().clone(), Arc::new(|| {}));
            p.sound = sound;
            {
                let m = p.mpv().expect("the player starts");
                m.set("ao-pcm-file", &out.to_string_lossy()).unwrap();
                m.set("audio-format", "float").unwrap();
                m.cmd(&["loadfile", &src.to_string_lossy(), "replace"]).unwrap();
            }
            assert_eq!(p.error, None, "the sound settings were taken");
            // Written as fast as it decodes; done when all of it is there and the player is idle.
            let (t0, want) = (std::time::Instant::now(), 3 * 44_100 * 2 * 2 * 9 / 10);
            loop {
                let (idle, err) = {
                    let l = p.live.lock().unwrap();
                    (l.idle, l.error.clone())
                };
                assert_eq!(err, None);
                let size = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
                if idle && size >= want {
                    break;
                }
                assert!(t0.elapsed() < std::time::Duration::from_secs(60), "the player did not finish ({size} bytes written)");
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            drop(p); // closes the file
            let (peak, n) = peak_of(&out);
            assert!(n >= 3 * 44_100 * 2 * 9 / 10, "{name}: only {n} samples");
            peak
        };
        let plain = play(Sound { replaygain: "no".into(), measure: false, ..Default::default() }, "plain.wav");
        assert!((0.90..=1.0).contains(&plain), "untouched settings play the track as it is: peak {plain}");
        let mut eq = [0.0; 10];
        eq[..4].copy_from_slice(&[6.0, 5.0, 4.0, 2.0]);
        for (name, sound) in [
            ("volume.wav", Sound { volume: 130.0, ..Default::default() }),
            ("width.wav", Sound { width: 2.5, ..Default::default() }),
            ("bass.wav", Sound { eq, ..Default::default() }),
            ("all.wav", Sound { volume: 130.0, width: 2.5, eq, crossfeed: true, normalize: true, ..Default::default() }),
        ] {
            let peak = play(Sound { replaygain: "no".into(), ..sound }, name);
            assert!(peak <= 0.975, "{name}: peak {peak} is over the limiter's ceiling");
            assert!(peak >= 0.5, "{name}: peak {peak} — the sound went missing");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }
}
