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
            crossfeed: false,
            width: 1.0,
            speed: 1.0,
        }
    }
}

impl Sound {
    /// mpv's `af` (audio filter chain) for these settings. Flat bands are left
    /// out; a limiter follows any boost so the EQ can't clip.
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
            f.push(format!("@wide:lavfi=[extrastereo=m={:.2}]", self.width));
        }
        if self.eq.iter().any(|&g| g >= 0.1) {
            f.push("@limit:lavfi=[alimiter=limit=0.97]".into());
        }
        f.join(",")
    }

    fn path() -> Option<PathBuf> {
        dirs::config_dir().map(|d| d.join("zentorrent").join("player.json"))
    }

    pub fn load() -> Sound {
        Self::path().and_then(|p| std::fs::read(p).ok()).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default()
    }

    pub fn save(&self) {
        if let Some(p) = Self::path() {
            let _ = std::fs::create_dir_all(p.parent().unwrap());
            let _ = std::fs::write(p, serde_json::to_vec_pretty(self).unwrap_or_default());
        }
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
}

const P_TIME: u64 = 1;
const P_DURATION: u64 = 2;
const P_PAUSE: u64 = 3;
const P_TITLE: u64 = 4;
const P_POS: u64 = 5;
const P_IDLE: u64 = 6;
const P_ARTIST: u64 = 7;

struct Mpv {
    api: &'static Api,
    h: *mut Handle,
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

// The client API is thread-safe; the handle is only destroyed in Drop, after the event thread ended.
unsafe impl Send for Mpv {}

impl Mpv {
    fn new(reg: &'static Registry, live: Arc<Mutex<Live>>, repaint: Arc<dyn Fn() + Send + Sync>) -> Result<Mpv, String> {
        let api = ffi::api()?;
        let h = unsafe { (api.create)() };
        if h.is_null() {
            return Err("libmpv could not create a player".into());
        }
        let mut m = Mpv { api, h, stop: Arc::new(AtomicBool::new(false)), thread: None };
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
            ("title", "ZenTorrent player"),
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
        ] {
            let n = ffi::cstr(name);
            unsafe { (api.observe_property)(h, id, n.as_ptr(), fmt) };
        }
        let (hp, stop) = (h as usize, m.stop.clone());
        m.thread = Some(std::thread::Builder::new().name("mpv-events".into()).spawn(move || {
            let h = hp as *mut Handle;
            while !stop.load(Ordering::Acquire) {
                let ev = unsafe { &*(api.wait_event)(h, 1.0) };
                if ev.event_id == ffi::EVENT_SHUTDOWN {
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
}

impl Player {
    pub fn new(rt: tokio::runtime::Handle, repaint: Arc<dyn Fn() + Send + Sync>) -> Player {
        // Lives as long as the process: mpv's stream callbacks point at it.
        let registry: &'static Registry =
            Box::leak(Box::new(Registry { rt, torrents: Mutex::new(Default::default()), archives: Mutex::new(Default::default()) }));
        Player { mpv: None, registry, live: Default::default(), queue: Queue::default(), sound: Sound::load(), in_mpv: Vec::new(), error: None, repaint }
    }

    pub fn active(&self) -> bool {
        self.queue.pos.is_some()
    }

    fn mpv(&mut self) -> Result<&Mpv, String> {
        if self.mpv.is_none() {
            let m = Mpv::new(self.registry, self.live.clone(), self.repaint.clone())?;
            self.mpv = Some(m);
            self.apply_sound();
        }
        Ok(self.mpv.as_ref().unwrap())
    }

    /// Where mpv reads a track: the file on disk once complete, else the stream.
    /// Archive members are always read through the stream (mpv can't open archives).
    fn source(&self, track: &Track, complete: bool) -> String {
        if let Some((archive, member)) = track.archive {
            stream::member_url(&self.queue.hash, archive, member)
        } else if complete {
            self.queue.folder.join(&track.path).to_string_lossy().into_owned()
        } else {
            stream::url(&self.queue.hash, track.file)
        }
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
        let result = (|| -> Result<(), String> {
            let m = self.mpv()?;
            m.cmd(&["loadfile", &src, "replace"])?;
            m.set("pause", "no")?;
            if let Some(s) = &next_src {
                m.cmd(&["loadfile", s, "append"])?;
            }
            Ok(())
        })();
        match result {
            Ok(()) => {
                self.error = None;
                self.queue.pos = Some(pos);
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
        let pos = self.live.lock().unwrap().mpv_pos;
        let (Some(p), Some(m)) = (pos, self.mpv.as_ref()) else { return };
        let p = p as usize;
        if p == 0 || p >= self.in_mpv.len() {
            return;
        }
        // mpv moved on by itself (gapless): drop what's behind, queue the next.
        let now = self.in_mpv[p];
        let _ = m.cmd(&["playlist-clear"]); // keeps only the playing entry
        self.in_mpv = vec![now];
        self.queue.pos = Some(now);
        self.live.lock().unwrap().mpv_pos = Some(0);
        if let Some(n) = self.queue.after(now) {
            let t = self.queue.tracks[self.queue.order[n]].clone();
            let src = self.source(&t, complete(t.file));
            if self.mpv.as_ref().unwrap().cmd(&["loadfile", &src, "append"]).is_ok() {
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

    pub fn stop(&mut self) {
        if let Some(m) = &self.mpv {
            let _ = m.cmd(&["stop"]);
        }
        self.queue.pos = None;
        self.in_mpv.clear();
    }

    /// Push the sound settings to mpv and save them.
    pub fn apply_sound(&mut self) {
        self.sound.save();
        let Some(m) = &self.mpv else { return };
        let s = &self.sound;
        let r = [
            m.set("volume", &format!("{:.0}", s.volume)),
            m.set("replaygain", &s.replaygain),
            m.set("speed", &format!("{:.2}", s.speed)),
            m.set("af", &s.af()),
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
            if self.mpv.as_ref().unwrap().cmd(&["loadfile", &src, "append"]).is_ok() {
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
        assert!(af.contains("@xfeed") && af.ends_with("@limit:lavfi=[alimiter=limit=0.97]"), "a boost gets a limiter: {af}");
        s.eq = [-3.0; 10];
        assert!(!s.af().contains("@limit"), "cuts can't clip");
        let back: Sound = serde_json::from_str(r#"{"volume": 80}"#).unwrap();
        assert_eq!((back.volume, back.speed, back.replaygain.as_str()), (80.0, 1.0, "track"), "old/partial settings files load");
    }
}
