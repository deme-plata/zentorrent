//! Carrying the sound filters' memory across a track change.
//!
//! mpv builds its audio filters anew for every file, so an equalizer band (and
//! anything else that remembers the sound before) starts each track with an empty
//! memory. In continuous music that was a step at the track change: measured
//! 2026-11-27, 3.6 times the signal's own sample-to-sample movement with the Bass
//! preset on.
//!
//! The way around it: the last second of the track before is written to a small
//! file of its own (the lead-in), and the next track is queued as an mpv EDL of
//! "the lead-in, then the track", with mpv told to start playing where the track
//! starts but to decode from the beginning. The filters hear the previous track's
//! last second (nobody else does) and are in the state they would be in had the two
//! tracks been one file.
//!
//! What makes it exact (all measured the same day in the real player, over ten
//! sample rates from 8 to 192 kHz, 36 pairs of track lengths and five filter chains;
//! every other way tried was wrong by a sample for some of them):
//!
//! * The lead-in is a file of its own, played whole. Cutting it out of the previous
//!   track by time inside the EDL lost or repeated a sample for most lengths.
//! * The track sits a quarter of a sample after the lead-in's end, and playback
//!   starts exactly there. mpv then only drops whole blocks (the lead-in's) and
//!   keeps whole blocks (the track's); where it has to cut inside a block it rounds
//!   down, which went wrong one way or the other depending on the sample rate. A
//!   quarter sample is too little for the filters to notice: they see no gap.
//! * The track's length is given half a sample too long. Left to itself, mpv cut
//!   the track's last sample off for most lengths.
//!
//! It only works when the previous track's end is known to the sample and nothing
//! changes shape at the join, so it is only done between lossless files (FLAC,
//! WAV/AIFF) of the same sample rate, channel count and sample size, complete on
//! disk, and only while a filter is on. MP3 and AAC change tracks as before.
//!
//! All of this was measured with mpv 0.37 on Linux. On Windows ZenTorrent ships its
//! own mpv 0.39, where it has not been run: there it is off until the user turns it
//! on in the Sound window (`Sound::lead_in`). If an entry made this way fails to
//! play, the player falls back to the plain track and makes no more of them.

use std::path::Path;

/// How much of the previous track the filters hear first, in seconds. The lowest
/// equalizer band (31 Hz, an octave wide) settles in about 15 ms; a second is plenty.
pub const LEAD: f64 = 1.0;

/// A file whose samples are exactly what was recorded, and whose length is known.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Lossless {
    pub rate: u32,
    pub channels: usize,
    pub frames: u64,
    /// Bits per sample: 16, or more (24, 32) — the two sizes mpv decodes to.
    pub wide: bool,
}

impl Lossless {
    pub fn secs(&self) -> f64 {
        self.frames as f64 / self.rate as f64
    }
}

fn open(path: &Path) -> Option<Box<dyn symphonia::core::formats::FormatReader>> {
    use symphonia::core::{formats::FormatOptions, io::MediaSourceStream, meta::MetadataOptions, probe::Hint};
    let file = std::fs::File::open(path).ok()?;
    let mss = MediaSourceStream::new(Box::new(file), Default::default());
    let mut hint = Hint::new();
    if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
        hint.with_extension(ext);
    }
    Some(symphonia::default::get_probe().format(&hint, mss, &FormatOptions::default(), &MetadataOptions::default()).ok()?.format)
}

fn shape(p: &symphonia::core::codecs::CodecParameters) -> Option<Lossless> {
    use symphonia::core::codecs as c;
    let exact = [c::CODEC_TYPE_FLAC, c::CODEC_TYPE_PCM_S16LE, c::CODEC_TYPE_PCM_S24LE, c::CODEC_TYPE_PCM_S32LE, c::CODEC_TYPE_PCM_S16BE, c::CODEC_TYPE_PCM_S24BE, c::CODEC_TYPE_PCM_S32BE]
        .contains(&p.codec);
    let bits = p.bits_per_sample?;
    let channels = p.channels?.count();
    if !exact || !(16..=32).contains(&bits) || !(1..=2).contains(&channels) {
        return None;
    }
    Some(Lossless { rate: p.sample_rate?, channels, frames: p.n_frames?, wide: bits > 16 })
}

/// Read the file's header (nothing is decoded). None for anything but 16- to 32-bit
/// FLAC and PCM, mono or stereo.
pub fn lossless(path: &Path) -> Option<Lossless> {
    shape(&open(path)?.default_track()?.codec_params)
}

/// The file's last `LEAD` seconds, decoded: interleaved, each sample at full 32-bit scale.
/// None unless the decoding ran exactly to the file's last frame.
fn tail(path: &Path, want: &Lossless) -> Option<Vec<i32>> {
    use symphonia::core::{audio::SampleBuffer, codecs::DecoderOptions, errors::Error, formats::{SeekMode, SeekTo}, units::Time};
    let mut format = open(path)?;
    let track = format.default_track()?;
    let (id, params) = (track.id, track.codec_params.clone());
    if shape(&params)? != *want {
        return None;
    }
    let mut dec = symphonia::default::get_codecs().make(&params, &DecoderOptions::default()).ok()?;
    // Start a few seconds before the end. This runs where the window is drawn: a long
    // file that will not seek is left alone rather than decoded from its beginning.
    let from = want.secs() - LEAD - 3.0;
    if from > 0.0 && format.seek(SeekMode::Accurate, SeekTo::Time { time: Time::from(from), track_id: Some(id) }).is_err() {
        return None;
    }
    dec.reset();
    let keep = (LEAD * want.rate as f64) as usize * want.channels;
    let mut out: Vec<i32> = Vec::new();
    let mut end = 0u64;
    while let Ok(packet) = format.next_packet() {
        if packet.track_id() != id {
            continue;
        }
        let decoded = match dec.decode(&packet) {
            Ok(d) => d,
            Err(Error::DecodeError(_)) => return None, // a damaged file: its end is not known
            Err(_) => break,
        };
        let spec = *decoded.spec();
        if spec.rate != want.rate || spec.channels.count() != want.channels {
            return None;
        }
        let frames = decoded.frames() as u64;
        let mut buf = SampleBuffer::<i32>::new(decoded.capacity() as u64, spec);
        buf.copy_interleaved_ref(decoded);
        out.extend_from_slice(buf.samples());
        end = packet.ts() + frames;
        if out.len() > 4 * keep {
            out.drain(..out.len() - keep);
        }
    }
    if end != want.frames || out.len() < keep {
        return None;
    }
    Some(out.split_off(out.len() - keep))
}

/// Write the lead-in: a WAV of the same shape as the tracks (16-bit, or 32-bit for
/// anything wider — the two sizes mpv decodes FLAC and WAV to). A change of shape
/// at the join would make mpv build the filters anew, which is what this is to avoid.
fn write_lead_in(to: &Path, samples: &[i32], shape: &Lossless) -> std::io::Result<()> {
    let bytes_per = if shape.wide { 4u16 } else { 2 };
    let block = bytes_per * shape.channels as u16;
    let data = samples.len() as u32 * bytes_per as u32;
    let mut w = Vec::with_capacity(44 + data as usize);
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + data).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&1u16.to_le_bytes());
    w.extend_from_slice(&(shape.channels as u16).to_le_bytes());
    w.extend_from_slice(&shape.rate.to_le_bytes());
    w.extend_from_slice(&(shape.rate * block as u32).to_le_bytes());
    w.extend_from_slice(&block.to_le_bytes());
    w.extend_from_slice(&(bytes_per * 8).to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&data.to_le_bytes());
    for &s in samples {
        if shape.wide {
            w.extend_from_slice(&s.to_le_bytes());
        } else {
            w.extend_from_slice(&((s >> 16) as i16).to_le_bytes());
        }
    }
    std::fs::write(to, w)
}

/// One file in an EDL: its length in bytes first, so any character may be in the path.
fn part(path: &str) -> String {
    format!("%{}%{path}", path.len())
}

/// What to give mpv instead of a track so that it starts with the filters warm.
#[derive(Debug, PartialEq)]
pub struct Entry {
    /// What to load.
    pub url: String,
    /// The options that must hold for that file (without them the lead-in would be heard).
    pub opts: String,
    /// Where the track itself starts in what mpv plays, in seconds: mpv's clock for
    /// this entry runs that much ahead of the track's own.
    pub lead: f64,
}

/// Make the entry for `next`, to be played right after `prev`; the lead-in is written
/// to `lead_in`. None when it cannot be done exactly (see the top of this file).
///
/// `layout=this`: the entry is `next` for every purpose but the lead-in — its tags,
/// its title, its ReplayGain. Without it mpv takes those from the first part
/// (measured: the next track played with the previous one's ReplayGain).
pub fn entry(prev: &Path, next: &Path, lead_in: &Path) -> Option<Entry> {
    let (a, b) = (lossless(prev)?, lossless(next)?);
    if (a.rate, a.channels, a.wide) != (b.rate, b.channels, b.wide) || a.secs() < LEAD + 0.5 || b.secs() < 0.5 {
        return None;
    }
    write_lead_in(lead_in, &tail(prev, &a)?, &a).ok()?;
    let tick = 1.0 / a.rate as f64;
    // The same text goes into the EDL and into `start`, so mpv reads the same number twice.
    let at = format!("{:.9}", LEAD + 0.25 * tick);
    let url = format!("edl://{},length={at};{},length={:.9},layout=this", part(lead_in.to_str()?), part(next.to_str()?), b.secs() + 0.5 * tick);
    let opts = format!("start={at},hr-seek=yes,hr-seek-demuxer-offset={:.1}", LEAD + 0.5);
    Some(Entry { url, opts, lead: at.parse().ok()? })
}

/// A file without a title tag is shown by its name, which mpv takes from the end of
/// what was loaded: here that carries the EDL's own words for the track.
pub fn tidy_title(title: &str) -> &str {
    let Some(t) = title.strip_suffix(",layout=this") else { return title };
    match t.rfind(",length=") {
        Some(i) if t[i + 8..].chars().all(|c| c.is_ascii_digit() || c == '.') => &t[..i],
        _ => t,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A WAV whose every sample says where it is: frame `i`, channel `c` holds `i * 2 + c`
    /// (16-bit: modulo 30000; 24-bit: as it is).
    fn wav(path: &Path, rate: u32, channels: u16, bits: u16, frames: u32) {
        let bytes_per = bits / 8;
        let data = frames * channels as u32 * bytes_per as u32;
        let mut w = Vec::new();
        w.extend_from_slice(b"RIFF");
        w.extend_from_slice(&(36 + data).to_le_bytes());
        w.extend_from_slice(b"WAVEfmt ");
        w.extend_from_slice(&16u32.to_le_bytes());
        w.extend_from_slice(&1u16.to_le_bytes());
        w.extend_from_slice(&channels.to_le_bytes());
        w.extend_from_slice(&rate.to_le_bytes());
        w.extend_from_slice(&(rate * channels as u32 * bytes_per as u32).to_le_bytes());
        w.extend_from_slice(&(channels * bytes_per).to_le_bytes());
        w.extend_from_slice(&bits.to_le_bytes());
        w.extend_from_slice(b"data");
        w.extend_from_slice(&data.to_le_bytes());
        for i in 0..frames {
            for c in 0..channels as u32 {
                let v = i * 2 + c;
                match bits {
                    16 => w.extend_from_slice(&((v % 30_000) as i16).to_le_bytes()),
                    _ => w.extend_from_slice(&v.to_le_bytes()[..3]),
                }
            }
        }
        std::fs::write(path, w).unwrap();
    }

    #[test]
    fn the_lead_in_is_the_previous_tracks_last_second_and_only_like_joins_like() {
        let dir = std::env::temp_dir().join(format!("zt-warm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let f = |n: &str| dir.join(n);
        let (a, b, hi, mono, short, wide, junk, lead) = (f("a, b; c.wav"), f("b.wav"), f("hi.wav"), f("mono.wav"), f("short.wav"), f("wide.wav"), f("x.mp3"), f("lead.wav"));
        wav(&a, 44_100, 2, 16, 400_011);
        wav(&b, 44_100, 2, 16, 88_200);
        wav(&hi, 96_000, 2, 16, 192_000);
        wav(&mono, 44_100, 1, 16, 88_200);
        wav(&short, 44_100, 2, 16, 44_100);
        wav(&wide, 44_100, 2, 24, 200_003);
        std::fs::write(&junk, b"not music at all, just some bytes that are no audio file").unwrap();

        assert_eq!(lossless(&a), Some(Lossless { rate: 44_100, channels: 2, frames: 400_011, wide: false }));
        assert_eq!(lossless(&wide).map(|w| (w.wide, w.frames)), Some((true, 200_003)));
        assert_eq!((lossless(&junk), lossless(&f("gone.wav"))), (None, None));

        // The last second of a 9 s file, found by seeking: exactly its last 44100 frames.
        let got = tail(&a, &lossless(&a).unwrap()).unwrap();
        assert_eq!(got.len(), 88_200);
        let first = 400_011 - 44_100u32;
        assert_eq!((got[0] >> 16, got[1] >> 16), (((first * 2) % 30_000) as i32, ((first * 2 + 1) % 30_000) as i32));
        assert_eq!(got[88_199] >> 16, ((400_010 * 2 + 1) % 30_000) as i32, "up to the very last sample");

        let e = entry(&a, &b, &lead).unwrap();
        let (pl, pb) = (lead.to_str().unwrap(), b.to_str().unwrap());
        // The lead-in, the track a quarter of a sample after its end, its length half a sample long.
        assert_eq!(e.url, format!("edl://%{}%{pl},length=1.000005669;%{}%{pb},length=2.000011338,layout=this", pl.len(), pb.len()));
        assert_eq!(e.opts, "start=1.000005669,hr-seek=yes,hr-seek-demuxer-offset=1.5");
        assert_eq!(e.lead, 1.000005669);
        // The lead-in file is a 16-bit WAV of that second, like the tracks.
        assert_eq!(lossless(&lead), Some(Lossless { rate: 44_100, channels: 2, frames: 44_100, wide: false }));
        assert_eq!(tail(&lead, &lossless(&lead).unwrap()).unwrap(), got);

        // 24-bit tracks get a 32-bit lead-in (what mpv decodes them to), sample for sample.
        let e = entry(&wide, &wide, &lead).unwrap();
        assert!(e.url.contains(",length=1.000005669;") && e.lead == 1.000005669);
        assert_eq!(lossless(&lead).map(|l| (l.wide, l.frames)), Some((true, 44_100)));
        let got = tail(&lead, &lossless(&lead).unwrap()).unwrap();
        assert_eq!((got[0] >> 8, got[88_199] >> 8), (((200_003 - 44_100) * 2) as i32, (200_002 * 2 + 1) as i32));

        assert_eq!(entry(&a, &hi, &lead), None, "another sample rate");
        assert_eq!(entry(&a, &mono, &lead), None, "another channel count");
        assert_eq!(entry(&a, &wide, &lead), None, "another sample size");
        assert_eq!(entry(&short, &b, &lead), None, "the track before is too short to lend a second");
        assert_eq!((entry(&a, &junk, &lead), entry(&junk, &b, &lead)), (None, None));
        assert_eq!((tidy_title("b.wav,length=2.000011338,layout=this"), tidy_title("b.wav,layout=this"), tidy_title("A real title")), ("b.wav", "b.wav", "A real title"));
        assert_eq!(tidy_title("Song, length=4 minutes,layout=this"), "Song, length=4 minutes", "only the EDL's own words go");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
