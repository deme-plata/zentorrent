//! The music visualizer. The playing track is analysed on its own thread: decoded with
//! symphonia and cut into 32 frequency bands, 30 times a second, with an FFT (rustfft).
//! The bands drive a swarm in the style of ZenTorrent's three.js page (your client in
//! the middle, seeders and leechers circling it) and the live spectrum on the equalizer.
//! A track still streaming (its file not complete yet) has no analysis: the swarm idles.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use eframe::egui::{self, Color32, Stroke};
use rustfft::{num_complex::Complex, Fft, FftPlanner};

pub const BANDS: usize = 32;
/// Analysis frames per second of audio.
const FPS: f64 = 30.0;
/// FFT size: at 44.1 kHz, 46 ms of sound and 21.5 Hz per bin.
const WINDOW: usize = 2048;
/// The bands span 40 Hz – 16 kHz, spaced evenly on a log scale.
const LOW_HZ: f32 = 40.0;
const HIGH_HZ: f32 = 16_000.0;

/// The scene's colours (docs/index.html): seeders, leechers, private trackers, background.
const GREEN: Color32 = Color32::from_rgb(60, 170, 90);
const BLUE: Color32 = Color32::from_rgb(70, 130, 220);
const PURPLE: Color32 = Color32::from_rgb(170, 120, 230);
const NIGHT: Color32 = Color32::from_rgb(5, 7, 11);

struct Analysis {
    path: PathBuf,
    /// Frame `i` = the bands at `i / FPS` seconds, 0–255; filled while the thread decodes.
    frames: Arc<Mutex<Vec<[u8; BANDS]>>>,
}

/// What the player screen keeps between frames.
#[derive(Default)]
pub struct Vis {
    analysis: Option<Analysis>,
    /// Smoothed levels 0–1 (fast up, slow down), as drawn.
    level: [f32; BANDS],
}

impl Vis {
    /// The levels to draw at `time` seconds into the file at `path` (None: nothing to
    /// analyse). A file not seen before starts its analysis.
    pub fn bands(&mut self, path: Option<&Path>, time: f64, playing: bool) -> [f32; BANDS] {
        let target = match path {
            Some(p) => {
                if self.analysis.as_ref().is_none_or(|a| a.path != p) {
                    let frames = Arc::new(Mutex::new(Vec::new()));
                    let (out, file) = (frames.clone(), p.to_path_buf());
                    let _ = std::thread::Builder::new().name("zt-vis".into()).spawn(move || {
                        let _ = analyse(&file, &out);
                    });
                    self.analysis = Some(Analysis { path: p.to_path_buf(), frames });
                }
                let a = self.analysis.as_ref().unwrap();
                let i = (time.max(0.0) * FPS) as usize;
                a.frames.lock().unwrap().get(i).map(|f| f.map(|b| b as f32 / 255.0))
            }
            None => {
                self.analysis = None;
                None
            }
        };
        let target = target.filter(|_| playing).unwrap_or([0.0; BANDS]);
        for (l, t) in self.level.iter_mut().zip(target) {
            *l = if t > *l { *l + (t - *l) * 0.6 } else { *l * 0.86 + t * 0.14 };
        }
        self.level
    }
}

/// Decode `path` and append its frames to `out` as they are computed.
fn analyse(path: &Path, out: &Mutex<Vec<[u8; BANDS]>>) -> Result<(), String> {
    use symphonia::core::{
        audio::SampleBuffer, codecs::DecoderOptions, errors::Error, formats::FormatOptions, io::MediaSourceStream,
        meta::MetadataOptions, probe::Hint,
    };
    let file = std::fs::File::open(path).map_err(|e| e.to_string())?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    let probed = symphonia::default::get_probe()
        .format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default())
        .map_err(|e| e.to_string())?;
    let mut format = probed.format;
    let track = format.default_track().ok_or("no audio track")?;
    let id = track.id;
    let rate = track.codec_params.sample_rate.ok_or("no sample rate")? as f32;
    let mut dec = symphonia::default::get_codecs().make(&track.codec_params, &DecoderOptions::default()).map_err(|e| e.to_string())?;
    let mut an = Analyser::new(rate);
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
        let frames = an.push(buf.samples(), spec.channels.count());
        if !frames.is_empty() {
            out.lock().unwrap().extend(frames);
        }
    }
    Ok(())
}

/// Mono samples in, one frame of bands out every `rate / FPS` samples.
struct Analyser {
    hop: f64,
    /// Sample index where the next frame starts, and the index of `buf[0]`.
    next: f64,
    pos: usize,
    buf: Vec<f32>,
    fft: Arc<dyn Fft<f32>>,
    hann: Vec<f32>,
    /// FFT bins where each band starts (and the last one ends).
    edges: [usize; BANDS + 1],
    /// Loudest band level so far (dB): the top of the 60 dB range shown.
    max_db: f32,
}

impl Analyser {
    fn new(rate: f32) -> Self {
        let mut edges = [0usize; BANDS + 1];
        for (b, e) in edges.iter_mut().enumerate() {
            let hz = LOW_HZ * (HIGH_HZ / LOW_HZ).powf(b as f32 / BANDS as f32);
            *e = ((hz * WINDOW as f32 / rate).round() as usize).clamp(1, WINDOW / 2 - 1);
        }
        for b in 1..=BANDS {
            edges[b] = edges[b].max(edges[b - 1] + 1);
        }
        let hann = (0..WINDOW).map(|i| 0.5 - 0.5 * (std::f32::consts::TAU * i as f32 / WINDOW as f32).cos()).collect();
        Analyser {
            hop: rate as f64 / FPS,
            next: 0.0,
            pos: 0,
            buf: Vec::new(),
            fft: FftPlanner::new().plan_fft_forward(WINDOW),
            hann,
            edges,
            max_db: 30.0,
        }
    }

    fn push(&mut self, interleaved: &[f32], channels: usize) -> Vec<[u8; BANDS]> {
        let ch = channels.max(1);
        self.buf.extend(interleaved.chunks(ch).map(|f| f.iter().sum::<f32>() / ch as f32));
        let mut out = Vec::new();
        while self.next as usize + WINDOW <= self.pos + self.buf.len() {
            out.push(self.frame(self.next as usize - self.pos));
            self.next += self.hop;
            let drop = (self.next as usize).saturating_sub(self.pos).min(self.buf.len());
            self.buf.drain(..drop);
            self.pos += drop;
        }
        out
    }

    fn frame(&mut self, start: usize) -> [u8; BANDS] {
        let mut data: Vec<Complex<f32>> = (0..WINDOW).map(|i| Complex::new(self.buf[start + i] * self.hann[i], 0.0)).collect();
        self.fft.process(&mut data);
        let mut out = [0u8; BANDS];
        for (b, o) in out.iter_mut().enumerate() {
            let (lo, hi) = (self.edges[b], self.edges[b + 1]);
            let power = data[lo..hi].iter().map(|c| c.norm_sqr()).sum::<f32>() / (hi - lo) as f32;
            // +3 dB per octave above 1 kHz (less below): music has less energy up high.
            let hz = LOW_HZ * (HIGH_HZ / LOW_HZ).powf((b as f32 + 0.5) / BANDS as f32);
            let db = 10.0 * (power + 1e-12).log10() + 3.0 * (hz / 1000.0).log2();
            self.max_db = self.max_db.max(db);
            *o = (((db - (self.max_db - 60.0)) / 60.0).clamp(0.0, 1.0) * 255.0) as u8;
        }
        out
    }
}

fn mean(b: &[f32]) -> f32 {
    b.iter().sum::<f32>() / b.len().max(1) as f32
}

/// The swarm: your client in the middle (it swells with the bass), 48 peers circling it on
/// three tilted rings, each one a frequency band (louder: further out, bigger, brighter),
/// the mids speed the rings up and the highs make it sparkle; the spectrum along the bottom.
pub fn show(ui: &mut egui::Ui, bands: &[f32; BANDS], note: Option<&str>) {
    let size = ui.available_size();
    let (rect, _) = ui.allocate_exact_size(size, egui::Sense::hover());
    let p = ui.painter_at(rect);
    p.rect_filled(rect, 0.0, NIGHT);
    let t = ui.input(|i| i.time) as f32;
    let c = rect.center() - egui::vec2(0.0, rect.height() * 0.06);
    let scale = rect.width().min(rect.height() * 2.6) * 0.5;
    let (bass, mid, high) = (mean(&bands[..5]), mean(&bands[5..18]), mean(&bands[18..]));

    // Peers: (depth 0 back .. 1 front, position, colour, level).
    let mut peers: Vec<(f32, egui::Pos2, Color32, f32)> = (0..48)
        .map(|i| {
            let ring = i % 3;
            let lvl = bands[(i * 2 + ring) % BANDS];
            let speed = [0.20, -0.13, 0.08][ring] * (1.0 + 1.5 * mid);
            let a = i as f32 * 2.399_963 + t * speed;
            let r = scale * (0.30 + 0.22 * ring as f32) * (1.0 + 0.40 * lvl);
            let tilt = [0.40, 0.28, 0.50][ring];
            let pos = c + egui::vec2(a.cos() * r, a.sin() * r * tilt);
            let col = if i % 7 == 0 { PURPLE } else if i % 2 == 0 { GREEN } else { BLUE };
            ((a.sin() + 1.0) / 2.0, pos, col, lvl)
        })
        .collect();
    peers.sort_by(|a, b| a.0.total_cmp(&b.0));
    let peer = |(d, pos, col, lvl): (f32, egui::Pos2, Color32, f32)| {
        p.line_segment([c, pos], Stroke::new(1.0, col.gamma_multiply(0.05 + 0.45 * lvl)));
        let s = (2.0 + 6.0 * lvl) * (0.55 + 0.6 * d);
        if lvl > 0.45 {
            p.circle_filled(pos, s * 2.6, col.gamma_multiply(0.10 * lvl));
        }
        p.circle_filled(pos, s, col.gamma_multiply(0.35 + 0.65 * d));
    };
    for &x in peers.iter().filter(|x| x.0 < 0.5) {
        peer(x);
    }
    // Your client: a glow that breathes with the bass.
    let core = 12.0 + 26.0 * bass;
    for k in (1..=5).rev() {
        p.circle_filled(c, core * (1.0 + k as f32 * 0.5), Color32::from_rgba_unmultiplied(120, 170, 255, (8.0 + 22.0 * bass) as u8));
    }
    p.circle_filled(c, core, Color32::from_rgb(225, 238, 255));
    for &x in peers.iter().filter(|x| x.0 >= 0.5) {
        peer(x);
    }
    // Highs: sparkles that come and go (positions fixed per tenth of a second).
    let tick = (t * 10.0) as u32;
    for k in 0..(high * 26.0) as u32 {
        let h = (k.wrapping_mul(2_654_435_761) ^ tick.wrapping_mul(40_503)).wrapping_mul(2_246_822_519);
        let a = (h % 6283) as f32 / 1000.0;
        let r = scale * (0.25 + (h >> 16) as f32 / 65_535.0 * 0.85);
        let pos = c + egui::vec2(a.cos() * r, a.sin() * r * 0.42);
        p.circle_filled(pos, 1.2 + 1.5 * high, Color32::from_white_alpha((90.0 + 160.0 * high) as u8));
    }
    // The spectrum along the bottom.
    spectrum_in(&p, egui::Rect::from_min_max(egui::pos2(rect.left(), rect.bottom() - rect.height() * 0.16), rect.max), bands, 0.35);
    if let Some(n) = note {
        p.text(rect.left_top() + egui::vec2(10.0, 8.0), egui::Align2::LEFT_TOP, n, egui::FontId::proportional(11.5), Color32::from_gray(140));
    }
}

/// The live spectrum as a strip of bars `height` high (the equalizer window).
pub fn spectrum(ui: &mut egui::Ui, bands: &[f32; BANDS], height: f32) {
    let (rect, _) = ui.allocate_exact_size(egui::vec2(ui.available_width(), height), egui::Sense::hover());
    let p = ui.painter_at(rect);
    p.rect_filled(rect, 6.0, NIGHT);
    spectrum_in(&p, rect.shrink(4.0), bands, 0.9);
}

fn spectrum_in(p: &egui::Painter, rect: egui::Rect, bands: &[f32; BANDS], alpha: f32) {
    let w = rect.width() / BANDS as f32;
    for (i, v) in bands.iter().enumerate() {
        let h = v * rect.height();
        let col = lerp(GREEN, BLUE, i as f32 / (BANDS - 1) as f32).gamma_multiply(alpha);
        let x = rect.left() + i as f32 * w;
        p.rect_filled(egui::Rect::from_min_max(egui::pos2(x + 1.0, rect.bottom() - h), egui::pos2(x + w - 1.0, rect.bottom())), 2.0, col);
    }
}

fn lerp(a: Color32, b: Color32, t: f32) -> Color32 {
    let m = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * t) as u8;
    Color32::from_rgb(m(a.r(), b.r()), m(a.g(), b.g()), m(a.b(), b.b()))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// One second of a 1 kHz tone at 44.1 kHz, stereo.
    fn tone(hz: f32) -> Vec<f32> {
        (0..44_100).flat_map(|i| {
            let s = (std::f32::consts::TAU * hz * i as f32 / 44_100.0).sin() * 0.5;
            [s, s]
        })
        .collect()
    }

    #[test]
    fn a_tone_lights_its_own_band_30_times_a_second() {
        let mut an = Analyser::new(44_100.0);
        let mut frames = Vec::new();
        for chunk in tone(1000.0).chunks(4096) {
            frames.extend(an.push(chunk, 2));
        }
        // (44100 - 2048) / 1470 hops + 1 = 29 frames from one second.
        assert_eq!(frames.len(), 29);
        let last = frames.last().unwrap();
        let loudest = (0..BANDS).max_by_key(|&b| last[b]).unwrap();
        let hz = |b: usize| LOW_HZ * (HIGH_HZ / LOW_HZ).powf(b as f32 / BANDS as f32);
        assert!(hz(loudest) <= 1000.0 && 1000.0 < hz(loudest + 1), "band {loudest} ({:.0}–{:.0} Hz)", hz(loudest), hz(loudest + 1));
        assert!(last[loudest] > 200 && last[0] < 60 && last[BANDS - 1] < 60, "{last:?}");
    }

    #[test]
    fn bands_cover_the_range_without_gaps() {
        let an = Analyser::new(48_000.0);
        assert!(an.edges.windows(2).all(|w| w[1] > w[0]));
        assert!(an.edges[BANDS] < WINDOW / 2);
    }
}
