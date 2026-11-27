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
                            P_TIME => l.time = if none { 0.0 } else { unsafe { *(p.data as *const f64) } },
                            P_DURATION => l.duration = if none { 0.0 } else { unsafe { *(p.data as *const f64) } },
                            P_PAUSE => l.paused = !none && unsafe { *(p.data as *const i32) } != 0,
                            P_IDLE => l.idle = !none && unsafe { *(p.data as *const i32) } != 0,
                            P_FULLSCREEN => l.fullscreen = !none && unsafe { *(p.data as *const i32) } != 0,
                            P_POS => l.mpv_pos = if none { None } else { Some(unsafe { *(p.data as *const i64) }) },
                            P_TITLE | P_ARTIST => {
                                let s = if none { String::new() } else { unsafe { CStr::from_ptr(*(p.data as *const *const c_char)) }.to_string_lossy().into_owned() };
                                if ev.reply_userdata == P_TITLE { l.title = s } else { l.artist = s }
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

    /// `loadfile`, with options that hold for this file only (its measured gain).
    /// mpv 0.38 put an index argument before the options; both forms are tried once and
    /// the one this mpv takes is remembered. A file is never left unplayed over its
    /// options: if neither form is taken, it is loaded without them.
    fn load(&self, src: &str, mode: &str, opts: Option<&str>) -> Result<(), String> {
        let Some(o) = opts else { return self.cmd(&["loadfile", src, mode]) };
        let with = |indexed: bool| if indexed { self.cmd(&["loadfile", src, mode, "-1", o]) } else { self.cmd(&["loadfile", src, mode, o]) };
        let tries = match self.indexed.get() {
            Some(known) => vec![known],
            None => vec![true, false],
        };
        for indexed in tries {
            if with(indexed).is_ok() {
                self.indexed.set(Some(indexed));
                return Ok(());
            }
        }
        self.cmd(&["loadfile", src, mode])
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

pub struct Player {
    mpv: Option<Mpv>,
    pub registry: &'static Registry,
    pub live: Arc<Mutex<Live>>,
    pub queue: Queue,
    pub sound: Sound,
    /// Order positions in mpv's playlist, in mpv's order (current, next).
    in_mpv: Vec<usize>,
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

    /// Play the track at play-order position `pos`, and queue the one after it.
    pub fn jump(&mut self, pos: usize, complete: &dyn Fn(usize) -> bool) {
        let Some(&ti) = self.queue.order.get(pos) else { return };
        let track = self.queue.tracks[ti].clone();
        let src = self.source(&track, complete(track.file));
        let next = self.queue.after(pos).map(|p| (p, self.queue.tracks[self.queue.order[p]].clone()));
        let next_src = next.as_ref().map(|(_, t)| self.source(t, complete(t.file)));
        let gain = self.gain(&track, complete(track.file));
        let next_gain = next.as_ref().and_then(|(_, t)| self.gain(t, complete(t.file)));
        let result = (|| -> Result<(), String> {
            let m = self.mpv()?;
            m.load(&src, "replace", gain.as_deref())?;
            m.set("pause", "no")?;
            if let Some(s) = &next_src {
                m.load(s, "append", next_gain.as_deref())?;
            }
            Ok(())
        })();
        match result {
            Ok(()) => {
                self.error = None;
                self.queue.pos = Some(pos);
                self.changed = std::time::Instant::now();
                self.resumed = None;
                self.resume = self.marks.get(&super::bookmark::key(&self.queue.hash, &track.path)).map(|at| (pos, at));
                self.in_mpv = std::iter::once(pos).chain(next.map(|(p, _)| p)).collect();
                if let Ok(mut l) = self.live.lock() {
                    l.mpv_pos = Some(0);
                    l.time = 0.0;
                    l.duration = track.duration.unwrap_or(0.0);
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
        let _ = m.cmd(&["playlist-clear"]); // keeps only the playing entry
        self.in_mpv = vec![now];
        self.queue.pos = Some(now);
        self.changed = std::time::Instant::now();
        let path = self.queue.tracks[self.queue.order[now]].path.clone();
        self.resume = self.marks.get(&super::bookmark::key(&self.queue.hash, &path)).map(|at| (now, at));
        self.live.lock().unwrap().mpv_pos = Some(0);
        if let Some(n) = self.queue.after(now) {
            let t = self.queue.tracks[self.queue.order[n]].clone();
            let src = self.source(&t, complete(t.file));
            let gain = self.gain(&t, complete(t.file));
            if self.mpv.as_ref().unwrap().load(&src, "append", gain.as_deref()).is_ok() {
                self.in_mpv.push(n);
            }
        }
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
            let _ = m.cmd(&["seek", &format!("{secs:.2}"), "absolute"]);
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
        let _ = m.cmd(&["playlist-clear"]);
        self.in_mpv = vec![cur];
        self.live.lock().unwrap().mpv_pos = Some(0);
        if let Some(n) = self.queue.after(cur) {
            let t = self.queue.tracks[self.queue.order[n]].clone();
            let src = self.source(&t, complete(t.file));
            let gain = self.gain(&t, complete(t.file));
            if self.mpv.as_ref().unwrap().load(&src, "append", gain.as_deref()).is_ok() {
                self.in_mpv.push(n);
            }
        }
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

    /// Play `files` as a queue through the real player (libmpv, writing `frames` frames
    /// to `out` as fast as it decodes) and return what came out: left channel, sample rate.
    fn render(out: &std::path::Path, sound: Sound, files: &[PathBuf], frames: u64, measured: bool) -> (Vec<f32>, u32) {
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
        }
        let tracks: Vec<Track> = files.iter().enumerate().map(|(i, f)| Track::file(i, &f.to_string_lossy(), format!("T{i}"), None, Kind::Audio)).collect();
        let n = tracks.len();
        p.start(Queue { tracks, order: (0..n).collect(), ..Default::default() }, 0, &|_| true);
        assert_eq!(p.error, None);
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
        left_of(out)
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
        // The limiter and the widening must leave none at all (the limiter used to leave
        // 5 ms of silence). The equalizer is different: mpv builds the filters anew for
        // each file, so its bands start the new track with an empty memory. That is no
        // silence and no lost sample, but it is a step at the change — measured 2026-11-27
        // with the Bass preset, crossfeed, widening and 110 % at once: 3.6 times the tone's
        // own. Known, not fixed here; the bound only keeps it from growing unnoticed.
        let mut eq = [0.0; 10];
        eq[..4].copy_from_slice(&[6.0, 5.0, 4.0, 2.0]);
        for (name, sound, most) in [
            ("limiter", Sound { volume: 130.0, ..quiet.clone() }, 1.05),
            ("wide", Sound { width: 1.5, ..quiet.clone() }, 1.05),
            ("everything", Sound { eq, width: 1.5, crossfeed: true, volume: 110.0, ..quiet.clone() }, 6.0),
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
