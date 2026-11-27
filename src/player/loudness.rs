//! How loud a track is (EBU R128 / ITU-R BS.1770 integrated loudness), measured by
//! ZenTorrent itself for music that carries no ReplayGain tags.
//!
//! A track is decoded once, on a worker thread, and its loudness and peak are
//! remembered (by path, size and date). The player then gives mpv a gain for it,
//! so untagged tracks play at the same level as tagged ones: -18 LUFS, the
//! ReplayGain 2.0 reference. Files that have ReplayGain tags keep using those.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};

use serde::{Deserialize, Serialize};

/// The level every track is brought to (ReplayGain 2.0's reference).
pub const TARGET_LUFS: f64 = -18.0;
/// Remembered measurements kept on disk.
const KEEP: usize = 20_000;

#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Measured {
    /// Integrated loudness, LUFS.
    pub lufs: f64,
    /// Highest sample, 1.0 = full scale.
    pub peak: f64,
}

impl Measured {
    /// The gain (dB) that brings this track to the target. A quiet track is never
    /// raised so far that its highest sample would pass full scale; with `headroom`
    /// false (something after this already fills the range: an EQ boost, more
    /// volume) it is not raised at all.
    pub fn gain_db(&self, headroom: bool) -> f64 {
        let to_full_scale = -20.0 * self.peak.max(1e-6).log10();
        let most = if headroom { to_full_scale.max(0.0) } else { 0.0 };
        (TARGET_LUFS - self.lufs).min(most).clamp(-30.0, 30.0)
    }
}

// ── the meter ───────────────────────────────────────────────────────────────

#[derive(Clone, Copy, Default)]
struct Biquad {
    b: [f64; 3],
    a: [f64; 2],
    z: [f64; 2],
}

impl Biquad {
    fn run(&mut self, x: f64) -> f64 {
        let y = self.b[0] * x + self.z[0];
        self.z[0] = self.b[1] * x - self.a[0] * y + self.z[1];
        self.z[1] = self.b[2] * x - self.a[1] * y;
        y
    }
}

/// BS.1770's K-weighting for any sample rate (the standard gives 48 kHz numbers;
/// these are the same two filters re-derived, as libebur128 does): a high shelf
/// for the head's effect, then a high-pass.
fn k_weighting(rate: f64) -> [Biquad; 2] {
    let (f0, g, q) = (1681.974450955533, 3.999843853973347, 0.7071752369554196);
    let k = (std::f64::consts::PI * f0 / rate).tan();
    let vh = 10f64.powf(g / 20.0);
    let vb = vh.powf(0.4996667741545416);
    let a0 = 1.0 + k / q + k * k;
    let shelf = Biquad {
        b: [(vh + vb * k / q + k * k) / a0, 2.0 * (k * k - vh) / a0, (vh - vb * k / q + k * k) / a0],
        a: [2.0 * (k * k - 1.0) / a0, (1.0 - k / q + k * k) / a0],
        z: [0.0; 2],
    };
    let (f0, q) = (38.13547087602444, 0.5003270373238773);
    let k = (std::f64::consts::PI * f0 / rate).tan();
    let a0 = 1.0 + k / q + k * k;
    let high_pass = Biquad { b: [1.0, -2.0, 1.0], a: [2.0 * (k * k - 1.0) / a0, (1.0 - k / q + k * k) / a0], z: [0.0; 2] };
    [shelf, high_pass]
}

/// Interleaved samples in; the integrated loudness out.
pub struct Meter {
    filters: Vec<[Biquad; 2]>,
    /// How much each channel counts (surrounds 1.41, the LFE not at all).
    weight: Vec<f64>,
    /// Samples per 100 ms step, and how far into the current step we are.
    step: usize,
    filled: usize,
    energy: f64,
    /// Weighted energy of every finished 100 ms step.
    steps: Vec<f64>,
    peak: f64,
}

impl Meter {
    pub fn new(rate: u32, channels: usize) -> Meter {
        let ch = channels.max(1);
        let weight = match ch {
            6 => vec![1.0, 1.0, 1.0, 0.0, 1.41, 1.41], // L R C LFE Ls Rs
            _ => vec![1.0; ch],
        };
        Meter { filters: vec![k_weighting(rate as f64); ch], weight, step: (rate as usize / 10).max(1), filled: 0, energy: 0.0, steps: Vec::new(), peak: 0.0 }
    }

    pub fn push(&mut self, interleaved: &[f32]) {
        let ch = self.filters.len();
        for frame in interleaved.chunks_exact(ch) {
            for (c, &x) in frame.iter().enumerate() {
                let x = x as f64;
                self.peak = self.peak.max(x.abs());
                let [shelf, high_pass] = &mut self.filters[c];
                let y = high_pass.run(shelf.run(x));
                self.energy += self.weight[c] * y * y;
            }
            self.filled += 1;
            if self.filled == self.step {
                self.steps.push(self.energy);
                self.energy = 0.0;
                self.filled = 0;
            }
        }
    }

    /// None: too short to measure (under 0.4 s) or silent.
    pub fn finish(&self) -> Option<Measured> {
        let lufs = |power: f64| -0.691 + 10.0 * power.log10();
        // 400 ms blocks, a new one every 100 ms.
        let blocks: Vec<f64> = self.steps.windows(4).map(|w| w.iter().sum::<f64>() / (4 * self.step) as f64).collect();
        let mean = |b: &[f64]| b.iter().sum::<f64>() / b.len() as f64;
        // Gate 1: nothing quieter than -70 LUFS counts. Gate 2: nothing more than 10 LU
        // below the level of what is left (pauses, fades) counts.
        let heard: Vec<f64> = blocks.into_iter().filter(|&p| p > 0.0 && lufs(p) > -70.0).collect();
        if heard.is_empty() {
            return None;
        }
        let gate = lufs(mean(&heard)) - 10.0;
        let counted: Vec<f64> = heard.into_iter().filter(|&p| lufs(p) > gate).collect();
        if counted.is_empty() {
            return None;
        }
        Some(Measured { lufs: lufs(mean(&counted)), peak: self.peak })
    }
}

/// Decode a whole file and measure it.
pub fn measure(path: &Path) -> Result<Measured, String> {
    use symphonia::core::{
        audio::SampleBuffer, codecs::DecoderOptions, errors::Error, formats::FormatOptions, io::MediaSourceStream, meta::MetadataOptions,
        probe::Hint,
    };
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let probed = symphonia::default::get_probe().format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default()).map_err(|e| e.to_string())?;
    let mut format = probed.format;
    let track = format.default_track().ok_or("no audio track")?;
    let id = track.id;
    let mut dec = symphonia::default::get_codecs().make(&track.codec_params, &DecoderOptions::default()).map_err(|e| e.to_string())?;
    let mut meter: Option<Meter> = None;
    while let Ok(packet) = format.next_packet() {
        if packet.track_id() != id {
            continue;
        }
        let decoded = match dec.decode(&packet) {
            Ok(d) => d,
            Err(Error::DecodeError(_)) => continue,
            Err(_) => break,
        };
        let spec = *decoded.spec();
        let mut buf = SampleBuffer::<f32>::new(decoded.capacity() as u64, spec);
        buf.copy_interleaved_ref(decoded);
        meter.get_or_insert_with(|| Meter::new(spec.rate, spec.channels.count())).push(buf.samples());
    }
    meter.and_then(|m| m.finish()).ok_or_else(|| "too short or silent".to_string())
}

// ── remembered measurements ─────────────────────────────────────────────────

enum Slot {
    /// Asked for, not measured yet.
    Pending,
    /// Could not be decoded (or is silent): not asked again.
    Failed,
    Done(Measured),
}

/// The same file, unchanged: its path, size and date.
fn key(path: &Path) -> Option<String> {
    let m = std::fs::metadata(path).ok()?;
    let at = m.modified().ok()?.duration_since(std::time::UNIX_EPOCH).ok()?.as_secs();
    Some(format!("{}|{}|{at}", path.display(), m.len()))
}

pub struct Loudness {
    known: Arc<Mutex<HashMap<String, Slot>>>,
    work: mpsc::Sender<(String, PathBuf)>,
    /// Bumped when a measurement arrives, so the player can pass it on.
    version: Arc<AtomicU64>,
}

impl Loudness {
    /// `persist`: keep measurements in ZenTorrent's data between runs.
    pub fn new(persist: bool, repaint: Arc<dyn Fn() + Send + Sync>) -> Loudness {
        let saved: HashMap<String, Measured> =
            if persist { crate::db::load("loudness", None).and_then(|b| serde_json::from_slice(&b).ok()).unwrap_or_default() } else { HashMap::new() };
        let known = Arc::new(Mutex::new(saved.into_iter().map(|(k, m)| (k, Slot::Done(m))).collect::<HashMap<_, _>>()));
        let version = Arc::new(AtomicU64::new(0));
        let (work, jobs) = mpsc::channel::<(String, PathBuf)>();
        let (k, v) = (known.clone(), version.clone());
        // One track at a time: measuring is a full decode, and must not starve playback.
        let _ = std::thread::Builder::new().name("zt-loudness".into()).spawn(move || {
            for (key, path) in jobs {
                let slot = measure(&path).map(Slot::Done).unwrap_or(Slot::Failed);
                let mut all = k.lock().unwrap();
                all.insert(key, slot);
                if persist {
                    let mut done: Vec<(&String, &Measured)> = all.iter().filter_map(|(k, s)| if let Slot::Done(m) = s { Some((k, m)) } else { None }).collect();
                    done.truncate(KEEP);
                    let _ = crate::db::save("loudness", &serde_json::to_vec(&done.into_iter().collect::<HashMap<_, _>>()).unwrap_or_default());
                }
                drop(all);
                v.fetch_add(1, Ordering::Release);
                repaint();
            }
        });
        Loudness { known, work, version }
    }

    /// The measurement for this file, if there is one. A file not seen before is
    /// queued for measuring (and answers None until that is done).
    pub fn lookup(&self, path: &Path) -> Option<Measured> {
        let key = key(path)?;
        let mut all = self.known.lock().unwrap();
        match all.get(&key) {
            Some(Slot::Done(m)) => Some(*m),
            Some(_) => None,
            None => {
                all.insert(key.clone(), Slot::Pending);
                let _ = self.work.send((key, path.to_path_buf()));
                None
            }
        }
    }

    /// Measure right now, on this thread.
    #[cfg(test)]
    pub fn measure_now(&self, path: &Path) -> Result<Measured, String> {
        let m = measure(path)?;
        if let Some(key) = key(path) {
            self.known.lock().unwrap().insert(key, Slot::Done(m));
        }
        Ok(m)
    }

    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stereo sine: `secs` at `dbfs` (peak), appended to `out`.
    fn sine(out: &mut Vec<f32>, rate: u32, hz: f64, dbfs: f64, secs: f64) {
        let amp = 10f64.powf(dbfs / 20.0);
        let start = out.len() / 2;
        for n in 0..(rate as f64 * secs) as usize {
            let v = (amp * (std::f64::consts::TAU * hz * (start + n) as f64 / rate as f64).sin()) as f32;
            out.extend_from_slice(&[v, v]);
        }
    }

    fn lufs(rate: u32, samples: &[f32]) -> f64 {
        let mut m = Meter::new(rate, 2);
        // In uneven pieces, as a decoder hands them over.
        for piece in samples.chunks(2 * 1151) {
            m.push(piece);
        }
        m.finish().unwrap().lufs
    }

    /// EBU Tech 3341's own test signals: a 1 kHz stereo sine at -23 dBFS reads -23.0 LUFS
    /// (case 1), at -33 dBFS -33.0 (case 2), and quieter passages around a -23 one are
    /// gated out (case 3). All within the standard's ±0.1 LU, at 48 and 44.1 kHz.
    #[test]
    fn it_reads_the_ebu_test_signals() {
        for rate in [48_000, 44_100, 96_000] {
            let mut a = Vec::new();
            sine(&mut a, rate, 1000.0, -23.0, 20.0);
            let got = lufs(rate, &a);
            assert!((got + 23.0).abs() <= 0.1, "case 1 at {rate} Hz: {got}");
            let mut b = Vec::new();
            sine(&mut b, rate, 1000.0, -33.0, 20.0);
            let got = lufs(rate, &b);
            assert!((got + 33.0).abs() <= 0.1, "case 2 at {rate} Hz: {got}");
            let mut c = Vec::new();
            sine(&mut c, rate, 1000.0, -36.0, 10.0);
            sine(&mut c, rate, 1000.0, -23.0, 60.0);
            sine(&mut c, rate, 1000.0, -36.0, 10.0);
            let got = lufs(rate, &c);
            assert!((got + 23.0).abs() <= 0.1, "case 3 (gating) at {rate} Hz: {got}");
        }
    }

    #[test]
    fn silence_and_scraps_have_no_loudness() {
        let mut m = Meter::new(44_100, 2);
        m.push(&vec![0.0; 2 * 44_100 * 3]);
        assert_eq!(m.finish(), None, "silence");
        let mut short = Vec::new();
        sine(&mut short, 44_100, 1000.0, -10.0, 0.3);
        let mut m = Meter::new(44_100, 2);
        m.push(&short);
        assert_eq!(m.finish(), None, "under 0.4 s");
    }

    #[test]
    fn the_gain_brings_a_track_to_the_target_without_clipping() {
        let loud = Measured { lufs: -8.0, peak: 1.0 };
        assert_eq!(loud.gain_db(true), -10.0, "a modern master comes down 10 dB");
        assert_eq!(loud.gain_db(false), -10.0);
        let quiet = Measured { lufs: -30.0, peak: 0.5 };
        assert!((quiet.gain_db(true) - 6.02).abs() < 0.01, "raised only until its peak is at full scale: {}", quiet.gain_db(true));
        assert_eq!(quiet.gain_db(false), 0.0, "and not at all when an EQ boost or extra volume already fills the range");
        assert_eq!(Measured { lufs: -18.0, peak: 0.9 }.gain_db(true), 0.0);
    }
}
