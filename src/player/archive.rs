//! Play what is packed inside archives — while it downloads.
//!
//! Films and TV episodes usually come as multi-part RARs (`name.part01.rar`…,
//! or `name.rar` + `name.r00`…) made *without compression* ("stored"), so the
//! film's bytes sit in the volumes as they are, just cut into pieces. ZenTorrent
//! reads the archive headers itself and maps the film onto byte ranges of the
//! volume files: mpv gets one ordinary seekable stream, and the torrent fetches
//! exactly the pieces being played.
//!
//! Supported: RAR 4 and RAR 5 (one file or many volumes, both naming styles),
//! ZIP (with ZIP64), TAR (ustar, GNU long names, pax) and plain splits
//! (`film.mkv.001`, `.002`…). Only stored members can be streamed; compressed
//! or password-protected ones are listed with the reason.
//!
//! Volume layouts are learned lazily. Seeking deep into a 50-part set does not
//! read 50 headers: volume 2's layout predicts where every later volume's data
//! sits (scene volumes are all the same size), and each volume's real header is
//! checked before its bytes are used. If a check fails, the volumes are read in
//! order instead — so a wrong guess costs time, never wrong bytes.

use std::future::Future;
use std::io;
use std::sync::{Arc, Mutex};

use super::playlist;

/// Reads bytes of the torrent's files. The torrent implements it with librqbit
/// streams (missing pieces are fetched first); tests use files in memory.
pub trait Source: Send + Sync {
    /// Read up to `buf.len()` bytes of `file` at `off`; 0 = end of file.
    fn read_into<'a>(&'a self, file: usize, off: u64, buf: &'a mut [u8]) -> impl Future<Output = io::Result<usize>> + Send + 'a;
}

/// Read `len` bytes (fewer only at the end of the file).
async fn read_vec<S: Source>(src: &S, file: usize, off: u64, len: usize) -> io::Result<Vec<u8>> {
    let mut buf = vec![0u8; len];
    let mut got = 0;
    while got < len {
        let n = src.read_into(file, off + got as u64, &mut buf[got..]).await?;
        if n == 0 {
            break;
        }
        got += n;
    }
    buf.truncate(got);
    Ok(buf)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Format {
    Rar,
    Zip,
    Tar,
    Split,
}

/// One volume file of an archive.
#[derive(Clone, Debug)]
pub struct Vol {
    /// Index of the file inside the torrent.
    pub file: usize,
    pub len: u64,
    pub path: String,
}

/// An archive inside the torrent: its volume files, in order.
#[derive(Clone, Debug)]
pub struct Set {
    pub format: Format,
    pub volumes: Vec<Vol>,
}

/// Where a member's bytes sit inside one volume.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Seg {
    pub off: u64,
    pub len: u64,
}

/// A file inside an archive.
#[derive(Debug)]
pub struct Member {
    /// Path inside the archive, `/`-separated.
    pub name: String,
    /// Why it cannot be streamed (None = it can).
    pub why_not: Option<String>,
    pub layout: Mutex<Layout>,
}

/// An indexed archive, as the stream reads it.
#[derive(Debug)]
pub struct Indexed {
    pub set: Set,
    pub members: Vec<Member>,
}

// ---------------------------------------------------------------- finding sets

/// Group the torrent's files into archives. `files` = (path, index, size).
pub fn find_sets(files: &[(String, usize, u64)]) -> Vec<Set> {
    use std::collections::BTreeMap;
    // (folder + base name, kind) → (order, volume)
    let mut groups: BTreeMap<(String, Format, bool), Vec<(u32, Vol)>> = BTreeMap::new();
    for (path, i, len) in files {
        let lower = path.to_ascii_lowercase();
        let vol = Vol { file: *i, len: *len, path: path.clone() };
        let mut add = |base: &str, f: Format, new_style: bool, order: u32| {
            groups.entry((base.to_string(), f, new_style)).or_default().push((order, vol.clone()))
        };
        if let Some(stem) = lower.strip_suffix(".rar") {
            match part_number(stem) {
                Some((base, n)) => add(base, Format::Rar, true, n),
                None => add(stem, Format::Rar, false, 0),
            }
        } else if let Some((base, n)) = old_rar_volume(&lower) {
            add(base, Format::Rar, false, n);
        } else if lower.ends_with(".zip") {
            add(&lower, Format::Zip, false, 0);
        } else if lower.ends_with(".tar") {
            add(&lower, Format::Tar, false, 0);
        } else if let Some((base, n)) = numbered_split(&lower) {
            if playlist::kind_of(base).is_some() {
                add(base, Format::Split, false, n);
            }
        }
    }
    let mut out = Vec::new();
    for ((_, format, new_style), mut vols) in groups {
        vols.sort_by_key(|(n, _)| *n);
        let first = vols[0].0;
        // Old-style volumes need their .rar; a split must start at .001.
        let starts = match (format, new_style) {
            (Format::Rar, false) => first == 0,
            (Format::Split, _) => first == 1,
            _ => true,
        };
        if starts {
            out.push(Set { format, volumes: vols.into_iter().map(|(_, v)| v).collect() });
        }
    }
    out.sort_by(|a, b| playlist::natural_cmp(&a.volumes[0].path, &b.volumes[0].path));
    out
}

/// Could this file start an archive worth looking into? (Cheap: name only.)
pub fn may_hold_media(path: &str) -> bool {
    let l = path.to_ascii_lowercase();
    l.ends_with(".rar") || l.ends_with(".zip") || l.ends_with(".tar") || numbered_split(&l).is_some_and(|(base, n)| n == 1 && playlist::kind_of(base).is_some())
}

/// "show.part01" → ("show", 1).
fn part_number(stem: &str) -> Option<(&str, u32)> {
    let (base, n) = stem.rsplit_once(".part")?;
    (!n.is_empty() && n.len() <= 4 && n.bytes().all(|b| b.is_ascii_digit())).then(|| (base, n.parse().unwrap()))
}

/// "show.r00" → ("show", 1), "show.s00" → ("show", 101): after show.rar (0).
fn old_rar_volume(lower: &str) -> Option<(&str, u32)> {
    let (base, ext) = lower.rsplit_once('.')?;
    let b = ext.as_bytes();
    (b.len() == 3 && (b'r'..=b'z').contains(&b[0]) && b[1].is_ascii_digit() && b[2].is_ascii_digit())
        .then(|| (base, 1 + (b[0] - b'r') as u32 * 100 + ((b[1] - b'0') * 10 + (b[2] - b'0')) as u32))
}

/// "film.mkv.001" → ("film.mkv", 1).
fn numbered_split(lower: &str) -> Option<(&str, u32)> {
    let (base, ext) = lower.rsplit_once('.')?;
    (ext.len() == 3 && ext.bytes().all(|b| b.is_ascii_digit())).then(|| (base, ext.parse().unwrap()))
}

// ---------------------------------------------------------------- layout

/// What a read at some position needs.
#[derive(Debug, PartialEq, Eq)]
pub enum Spot {
    /// Read volume `vol` at `off`; `avail` bytes of the member follow there.
    At { vol: usize, off: u64, avail: u64 },
    /// Volume `k`'s header must be read first.
    Need(usize),
    End,
}

/// A member's pieces across its volumes, learned as they are needed.
#[derive(Debug)]
pub struct Layout {
    /// The member's piece in each volume it spans, once read from that volume's header.
    segs: Vec<Option<Seg>>,
    lens: Vec<u64>,
    total: u64,
    /// From the second volume: (data offset, bytes after the data) — the same in
    /// every later volume of a regular set.
    mid: Option<(u64, u64)>,
    regular: bool,
}

impl Layout {
    /// All pieces known (ZIP, TAR, splits, single-volume RAR).
    pub fn known(segs: Vec<Seg>, lens: Vec<u64>) -> Layout {
        let total = segs.iter().map(|s| s.len).sum();
        Layout { segs: segs.into_iter().map(Some).collect(), lens, total, mid: None, regular: true }
    }

    /// A member that starts with `first` in volume 1 and continues through the rest.
    pub fn spanning(first: Seg, lens: Vec<u64>, total: u64) -> Layout {
        let mut segs = vec![None; lens.len()];
        segs[0] = Some(first);
        Layout { segs, lens, total, mid: None, regular: true }
    }

    pub fn total(&self) -> u64 {
        self.total
    }

    /// Known pieces, and predicted ones where the set looks regular.
    fn plan(&self) -> Vec<Option<Seg>> {
        let n = self.segs.len();
        let mut out = Vec::with_capacity(n);
        let mut start = 0u64;
        let mut chain = true; // every earlier piece known or predicted
        for k in 0..n {
            let seg = self.segs[k].or_else(|| {
                let (off, trailer) = self.mid.filter(|_| self.regular && chain)?;
                let len = if k == n - 1 { self.total.saturating_sub(start) } else { self.lens[k].saturating_sub(off + trailer) };
                Some(Seg { off, len })
            });
            match seg {
                Some(s) => start += s.len,
                None => chain = false,
            }
            out.push(seg);
        }
        out
    }

    pub fn locate(&self, v: u64) -> Spot {
        if v >= self.total {
            return Spot::End;
        }
        let mut start = 0u64;
        for (k, seg) in self.plan().into_iter().enumerate() {
            let Some(seg) = seg else { return Spot::Need(k) };
            if v < start + seg.len {
                return if self.segs[k].is_some() {
                    Spot::At { vol: k, off: seg.off + (v - start), avail: start + seg.len - v }
                } else {
                    Spot::Need(k) // predicted: check the real header first
                };
            }
            start += seg.len;
        }
        Spot::End
    }

    /// Volume `k`'s real piece. A piece that differs from the prediction turns
    /// predictions off: from then on volumes are read in order.
    pub fn learn(&mut self, k: usize, seg: Seg) {
        if k >= self.segs.len() {
            return;
        }
        let predicted = self.plan()[k];
        self.segs[k] = Some(seg);
        if k == 1 && self.mid.is_none() {
            self.mid = Some((seg.off, self.lens[1].saturating_sub(seg.off + seg.len)));
        } else if predicted.is_some_and(|p| p != seg) {
            self.regular = false;
        }
    }
}

// ---------------------------------------------------------------- reading a member

/// A seekable reader over one member, for the player's stream.
pub struct Cursor<S> {
    pub src: S,
    pub archive: Arc<Indexed>,
    pub member: usize,
    pub pos: u64,
}

impl<S: Source> Cursor<S> {
    pub fn len(&self) -> u64 {
        self.archive.members[self.member].layout.lock().unwrap().total()
    }

    pub async fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let m = &self.archive.members[self.member];
        let set = &self.archive.set;
        // Each pass either reads or learns one more volume, so this always ends.
        for _ in 0..set.volumes.len() + 2 {
            let spot = m.layout.lock().unwrap().locate(self.pos);
            match spot {
                Spot::End => return Ok(0),
                Spot::Need(k) => {
                    let seg = piece(&self.src, set, &m.name, k).await.map_err(io::Error::other)?;
                    m.layout.lock().unwrap().learn(k, seg);
                }
                Spot::At { vol, off, avail } => {
                    let want = out.len().min(avail.min(1 << 20) as usize);
                    let got = self.src.read_into(set.volumes[vol].file, off, &mut out[..want]).await?;
                    if got == 0 && want > 0 {
                        return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "archive volume ended early"));
                    }
                    self.pos += got as u64;
                    return Ok(got);
                }
            }
        }
        Err(io::Error::other("the archive's volumes don't fit together"))
    }
}

/// Where `name`'s bytes sit in volume `k` (RAR: read from that volume's headers).
async fn piece<S: Source>(src: &S, set: &Set, name: &str, k: usize) -> Result<Seg, String> {
    let vol = set.volumes.get(k).ok_or("missing archive volume")?;
    match set.format {
        Format::Rar => {
            let files = rar_walk(src, vol, Some(name)).await?;
            files
                .into_iter()
                .find(|f| f.name == name && (k == 0 || f.split_before))
                .map(|f| f.seg)
                .ok_or_else(|| format!("{} does not continue {name}", vol.path))
        }
        Format::Split => Ok(Seg { off: 0, len: vol.len }),
        Format::Zip | Format::Tar => Err("single-volume archive".into()),
    }
}

// ---------------------------------------------------------------- indexing

/// List the archive's members. Reads only headers (for a multi-volume RAR: the
/// first volume's, which hold the file list).
pub async fn index<S: Source>(src: &S, set: &Set) -> Result<Indexed, String> {
    let lens: Vec<u64> = set.volumes.iter().map(|v| v.len).collect();
    let members = match set.format {
        Format::Rar => {
            let first = &set.volumes[0];
            rar_walk(src, first, None)
                .await?
                .into_iter()
                .filter(|f| !f.dir)
                .map(|f| {
                    let why_not = if f.encrypted {
                        Some("password protected".to_string())
                    } else if !f.stored {
                        Some("packed with compression — only uncompressed (stored) archives can be played while downloading".to_string())
                    } else {
                        None
                    };
                    let layout = if f.split_after && set.volumes.len() > 1 {
                        Layout::spanning(f.seg, lens.clone(), f.size)
                    } else {
                        Layout::known(vec![f.seg], lens[..1].to_vec())
                    };
                    Member { name: f.name, why_not, layout: Mutex::new(layout) }
                })
                .collect()
        }
        Format::Zip => zip_members(src, &set.volumes[0]).await?,
        Format::Tar => tar_members(src, &set.volumes[0]).await?,
        Format::Split => {
            let first = &set.volumes[0].path;
            let name = first.rsplit('/').next().unwrap_or(first);
            let name = name.rsplit_once('.').map(|(b, _)| b).unwrap_or(name).to_string();
            let segs = set.volumes.iter().map(|v| Seg { off: 0, len: v.len }).collect();
            vec![Member { name, why_not: None, layout: Mutex::new(Layout::known(segs, lens.clone())) }]
        }
    };
    Ok(Indexed { set: set.clone(), members })
}

fn u16le(b: &[u8], at: usize) -> Option<u64> {
    Some(u16::from_le_bytes(b.get(at..at + 2)?.try_into().ok()?) as u64)
}
fn u32le(b: &[u8], at: usize) -> Option<u64> {
    Some(u32::from_le_bytes(b.get(at..at + 4)?.try_into().ok()?) as u64)
}
fn u64le(b: &[u8], at: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(at..at + 8)?.try_into().ok()?))
}

/// Archive names: UTF-8 when valid, else Latin-1; always `/`-separated.
fn name_of(b: &[u8]) -> String {
    let s = match std::str::from_utf8(b) {
        Ok(s) => s.to_string(),
        Err(_) => b.iter().map(|&c| c as char).collect(),
    };
    s.replace('\\', "/")
}

const DAMAGED: &str = "the archive's headers are damaged";

// ---------------------------------------------------------------- RAR

const RAR4_SIG: &[u8] = b"Rar!\x1a\x07\x00";
const RAR5_SIG: &[u8] = b"Rar!\x1a\x07\x01\x00";

#[derive(Debug)]
struct RarFile {
    name: String,
    size: u64,
    seg: Seg,
    split_before: bool,
    split_after: bool,
    stored: bool,
    encrypted: bool,
    dir: bool,
}

/// The file headers of one RAR volume, in order. With `until`, stops at that
/// file; without, stops at a file that continues in the next volume (the rest
/// of this volume is its data), so only headers are ever read.
async fn rar_walk<S: Source>(src: &S, vol: &Vol, until: Option<&str>) -> Result<Vec<RarFile>, String> {
    let sig = read_vec(src, vol.file, 0, 8).await.map_err(|e| e.to_string())?;
    if sig.starts_with(RAR5_SIG) {
        rar5_walk(src, vol, until).await
    } else if sig.starts_with(RAR4_SIG) {
        rar4_walk(src, vol, until).await
    } else {
        Err(format!("{} is not a RAR archive", vol.path))
    }
}

fn done(f: &RarFile, until: Option<&str>) -> bool {
    match until {
        Some(n) => f.name == n,
        None => f.split_after,
    }
}

async fn rar4_walk<S: Source>(src: &S, vol: &Vol, until: Option<&str>) -> Result<Vec<RarFile>, String> {
    let mut out = Vec::new();
    let mut pos = RAR4_SIG.len() as u64;
    let io = |e: io::Error| e.to_string();
    for _ in 0..100_000 {
        if pos + 7 > vol.len {
            break;
        }
        let h = read_vec(src, vol.file, pos, 11).await.map_err(io)?;
        let (Some(flags), Some(hs)) = (u16le(&h, 3), u16le(&h, 5)) else { break };
        if hs < 7 {
            return Err(DAMAGED.into());
        }
        let next = match h[2] {
            0x73 => {
                if flags & 0x0080 != 0 {
                    return Err("the archive's file list is password protected".into());
                }
                pos + hs
            }
            0x74 => {
                let b = read_vec(src, vol.file, pos, hs as usize).await.map_err(io)?;
                let f = rar4_file(&b, pos).ok_or(DAMAGED)?;
                let next = f.seg.off + f.seg.len;
                let stop = done(&f, until);
                out.push(f);
                if stop {
                    break;
                }
                next
            }
            0x7b => break,
            _ if flags & 0x8000 != 0 => pos + hs + u32le(&h, 7).ok_or(DAMAGED)?,
            _ => pos + hs,
        };
        pos = next;
    }
    Ok(out)
}

fn rar4_file(b: &[u8], pos: u64) -> Option<RarFile> {
    let flags = u16le(b, 3)?;
    let hs = u16le(b, 5)?;
    let mut pack = u32le(b, 7)?;
    let mut size = u32le(b, 11)?;
    let method = *b.get(25)?;
    let name_len = u16le(b, 26)? as usize;
    let mut p = 32;
    if flags & 0x100 != 0 {
        pack |= u32le(b, 32)? << 32;
        size |= u32le(b, 36)? << 32;
        p = 40;
    }
    let raw = b.get(p..p + name_len)?;
    // Unicode names are "ASCII\0packed-unicode"; the ASCII part is the name.
    let raw = if flags & 0x200 != 0 { raw.split(|&c| c == 0).next().unwrap_or(raw) } else { raw };
    Some(RarFile {
        name: name_of(raw),
        size,
        seg: Seg { off: pos + hs, len: pack },
        split_before: flags & 0x01 != 0,
        split_after: flags & 0x02 != 0,
        stored: method == 0x30,
        encrypted: flags & 0x04 != 0,
        dir: flags & 0xe0 == 0xe0,
    })
}

/// RAR 5 variable-length integer: 7 bits per byte, low first.
fn vint(b: &[u8], at: &mut usize) -> Option<u64> {
    let mut v = 0u64;
    for shift in (0..70).step_by(7) {
        let c = *b.get(*at)?;
        *at += 1;
        v |= ((c & 0x7f) as u64).checked_shl(shift)?;
        if c & 0x80 == 0 {
            return Some(v);
        }
    }
    None
}

async fn rar5_walk<S: Source>(src: &S, vol: &Vol, until: Option<&str>) -> Result<Vec<RarFile>, String> {
    let mut out = Vec::new();
    let mut pos = RAR5_SIG.len() as u64;
    let io = |e: io::Error| e.to_string();
    for _ in 0..100_000 {
        if pos + 5 > vol.len {
            break;
        }
        let h = read_vec(src, vol.file, pos, 14).await.map_err(io)?;
        let mut at = 4;
        let size = vint(&h, &mut at).ok_or(DAMAGED)?;
        if size == 0 || size > 2 << 20 {
            return Err(DAMAGED.into());
        }
        let start = pos + at as u64;
        let b = read_vec(src, vol.file, start, size as usize).await.map_err(io)?;
        let mut r = 0;
        let typ = vint(&b, &mut r).ok_or(DAMAGED)?;
        let flags = vint(&b, &mut r).ok_or(DAMAGED)?;
        let extra = if flags & 1 != 0 { vint(&b, &mut r).ok_or(DAMAGED)? } else { 0 };
        let data = if flags & 2 != 0 { vint(&b, &mut r).ok_or(DAMAGED)? } else { 0 };
        let data_off = start + size;
        match typ {
            4 => return Err("the archive's file list is password protected".into()),
            5 => break,
            2 => {
                let f = rar5_file(&b, r, flags, extra, Seg { off: data_off, len: data }).ok_or(DAMAGED)?;
                let stop = done(&f, until);
                out.push(f);
                if stop {
                    break;
                }
            }
            _ => {}
        }
        pos = data_off + data;
    }
    Ok(out)
}

fn rar5_file(b: &[u8], mut r: usize, flags: u64, extra: u64, seg: Seg) -> Option<RarFile> {
    let file_flags = vint(b, &mut r)?;
    let size = vint(b, &mut r)?;
    let _attributes = vint(b, &mut r)?;
    if file_flags & 0x2 != 0 {
        r += 4; // mtime
    }
    if file_flags & 0x4 != 0 {
        r += 4; // data CRC32
    }
    let comp = vint(b, &mut r)?;
    let _host = vint(b, &mut r)?;
    let name_len = vint(b, &mut r)? as usize;
    let name = name_of(b.get(r..r + name_len)?);
    // Extra area (the last `extra` bytes): record type 1 = encryption.
    let mut encrypted = false;
    let mut e = b.len().checked_sub(extra as usize)?;
    while e < b.len() {
        let rec = vint(b, &mut e)? as usize;
        let body = e;
        if vint(b, &mut e)? == 1 {
            encrypted = true;
        }
        e = body + rec;
    }
    Some(RarFile {
        name,
        size,
        seg,
        split_before: flags & 0x08 != 0,
        split_after: flags & 0x10 != 0,
        stored: (comp >> 7) & 7 == 0,
        encrypted,
        dir: file_flags & 0x1 != 0,
    })
}

// ---------------------------------------------------------------- ZIP

async fn zip_members<S: Source>(src: &S, vol: &Vol) -> Result<Vec<Member>, String> {
    let io = |e: io::Error| e.to_string();
    let tail_len = vol.len.min(65_557 + 22);
    let tail_at = vol.len - tail_len;
    let tail = read_vec(src, vol.file, tail_at, tail_len as usize).await.map_err(io)?;
    let e = (0..tail.len().saturating_sub(21)).rev().find(|&i| tail[i..].starts_with(b"PK\x05\x06")).ok_or("not a ZIP archive")?;
    let mut count = u16le(&tail, e + 10).ok_or(DAMAGED)?;
    let mut cd_size = u32le(&tail, e + 12).ok_or(DAMAGED)?;
    let mut cd_off = u32le(&tail, e + 16).ok_or(DAMAGED)?;
    if count == 0xffff || cd_size == 0xffff_ffff || cd_off == 0xffff_ffff {
        // ZIP64: the locator sits just before the end record.
        let l = e.checked_sub(20).filter(|&l| tail[l..].starts_with(b"PK\x06\x07")).ok_or(DAMAGED)?;
        let at = u64le(&tail, l + 8).ok_or(DAMAGED)?;
        let z = read_vec(src, vol.file, at, 56).await.map_err(io)?;
        if !z.starts_with(b"PK\x06\x06") {
            return Err(DAMAGED.into());
        }
        count = u64le(&z, 32).ok_or(DAMAGED)?;
        cd_size = u64le(&z, 40).ok_or(DAMAGED)?;
        cd_off = u64le(&z, 48).ok_or(DAMAGED)?;
    }
    if cd_size > 64 << 20 {
        return Err(DAMAGED.into());
    }
    let cd = read_vec(src, vol.file, cd_off, cd_size as usize).await.map_err(io)?;
    let mut out = Vec::new();
    let mut p = 0usize;
    for _ in 0..count {
        if !cd.get(p..).is_some_and(|c| c.starts_with(b"PK\x01\x02")) {
            return Err(DAMAGED.into());
        }
        let flags = u16le(&cd, p + 8).ok_or(DAMAGED)?;
        let method = u16le(&cd, p + 10).ok_or(DAMAGED)?;
        let mut comp = u32le(&cd, p + 20).ok_or(DAMAGED)?;
        let mut size = u32le(&cd, p + 24).ok_or(DAMAGED)?;
        let nlen = u16le(&cd, p + 28).ok_or(DAMAGED)? as usize;
        let elen = u16le(&cd, p + 30).ok_or(DAMAGED)? as usize;
        let clen = u16le(&cd, p + 32).ok_or(DAMAGED)? as usize;
        let mut local = u32le(&cd, p + 42).ok_or(DAMAGED)?;
        let name = name_of(cd.get(p + 46..p + 46 + nlen).ok_or(DAMAGED)?);
        // ZIP64 extra field: 64-bit values for the fields that read 0xffffffff, in this order.
        let extra = cd.get(p + 46 + nlen..p + 46 + nlen + elen).ok_or(DAMAGED)?;
        let mut x = 0;
        while x + 4 <= extra.len() {
            let (id, len) = (u16le(extra, x).unwrap(), u16le(extra, x + 2).unwrap() as usize);
            if id == 1 {
                let mut v = x + 4;
                for field in [&mut size, &mut comp, &mut local] {
                    if *field == 0xffff_ffff {
                        *field = u64le(extra, v).ok_or(DAMAGED)?;
                        v += 8;
                    }
                }
            }
            x += 4 + len;
        }
        p += 46 + nlen + elen + clen;
        if name.ends_with('/') {
            continue;
        }
        let why_not = if flags & 1 != 0 {
            Some("password protected".to_string())
        } else if method != 0 {
            Some("packed with compression — only uncompressed (stored) archives can be played while downloading".to_string())
        } else {
            None
        };
        // The data starts after the member's local header, whose own name/extra lengths count.
        let seg = if why_not.is_none() && playlist::kind_of(&name).is_some() {
            let h = read_vec(src, vol.file, local, 30).await.map_err(io)?;
            if !h.starts_with(b"PK\x03\x04") {
                return Err(DAMAGED.into());
            }
            Seg { off: local + 30 + u16le(&h, 26).ok_or(DAMAGED)? + u16le(&h, 28).ok_or(DAMAGED)?, len: comp }
        } else {
            Seg { off: 0, len: 0 }
        };
        out.push(Member { name, why_not, layout: Mutex::new(Layout::known(vec![seg], vec![vol.len])) });
    }
    Ok(out)
}

// ---------------------------------------------------------------- TAR

async fn tar_members<S: Source>(src: &S, vol: &Vol) -> Result<Vec<Member>, String> {
    let io = |e: io::Error| e.to_string();
    let cstr = |b: &[u8]| name_of(b.split(|&c| c == 0).next().unwrap_or(b));
    let mut out = Vec::new();
    let mut pos = 0u64;
    let (mut long_name, mut pax_name, mut pax_size) = (None, None, None);
    for _ in 0..100_000 {
        if pos + 512 > vol.len {
            break;
        }
        let h = read_vec(src, vol.file, pos, 512).await.map_err(io)?;
        if h.len() < 512 || h.iter().all(|&c| c == 0) {
            break;
        }
        let size = match pax_size.take() {
            Some(s) => s,
            None if h[124] & 0x80 != 0 => h[125..136].iter().fold(0u64, |a, &c| a << 8 | c as u64),
            None => {
                let t = cstr(&h[124..136]);
                u64::from_str_radix(t.trim(), 8).map_err(|_| DAMAGED)?
            }
        };
        let data = pos + 512;
        let typ = h[156];
        match typ {
            b'L' => long_name = Some(cstr(&read_vec(src, vol.file, data, size.min(64 << 10) as usize).await.map_err(io)?)),
            b'x' => {
                let rec = read_vec(src, vol.file, data, size.min(1 << 20) as usize).await.map_err(io)?;
                for line in String::from_utf8_lossy(&rec).lines() {
                    // "<len> key=value"
                    let Some((_, kv)) = line.split_once(' ') else { continue };
                    match kv.split_once('=') {
                        Some(("path", v)) => pax_name = Some(v.replace('\\', "/")),
                        Some(("size", v)) => pax_size = v.parse().ok(),
                        _ => {}
                    }
                }
            }
            b'0' | 0 => {
                let mut name = cstr(&h[0..100]);
                // POSIX ustar has a path prefix here; GNU ("ustar  ") keeps timestamps there.
                if &h[257..263] == b"ustar\0" && h[345] != 0 {
                    name = format!("{}/{}", cstr(&h[345..500]), name);
                }
                let name = pax_name.take().or(long_name.take()).unwrap_or(name);
                out.push(Member { name, why_not: None, layout: Mutex::new(Layout::known(vec![Seg { off: data, len: size }], vec![vol.len])) });
            }
            _ => {}
        }
        if typ != b'L' && typ != b'x' {
            (long_name, pax_name) = (None, None);
        }
        pos = data + size.div_ceil(512) * 512;
    }
    Ok(out)
}

#[cfg(test)]
pub mod tests {
    use super::*;

    /// Files in memory, as a `Source`.
    pub struct Mem(pub Vec<Vec<u8>>);

    impl Source for Mem {
        fn read_into<'a>(&'a self, file: usize, off: u64, buf: &'a mut [u8]) -> impl Future<Output = io::Result<usize>> + Send + 'a {
            async move {
                let f = &self.0[file];
                let at = (off as usize).min(f.len());
                let n = buf.len().min(f.len() - at);
                buf[..n].copy_from_slice(&f[at..at + n]);
                Ok(n)
            }
        }
    }

    #[test]
    fn sets_are_found_and_ordered() {
        let f = |p: &str, i| (p.to_string(), i, 100u64);
        let files = vec![
            f("Show.S01E01/show.r01", 0),
            f("Show.S01E01/show.rar", 1),
            f("Show.S01E01/show.r00", 2),
            f("Film/film.part2.rar", 3),
            f("Film/film.part10.rar", 4),
            f("Film/film.part1.rar", 5),
            f("Clip/clip.mkv.002", 6),
            f("Clip/clip.mkv.001", 7),
            f("Other/notes.r00", 8), // no .rar: not an archive start
            f("Sample/sample.mkv", 9),
            f("Bundle.zip", 10),
            f("Tape.tar", 11),
            f("Setup.7z.001", 12), // 7z: not supported
        ];
        let sets = find_sets(&files);
        let got: Vec<(Format, Vec<usize>)> = sets.iter().map(|s| (s.format, s.volumes.iter().map(|v| v.file).collect())).collect();
        assert_eq!(
            got,
            [
                (Format::Zip, vec![10]),
                (Format::Split, vec![7, 6]),
                (Format::Rar, vec![5, 3, 4]),
                (Format::Rar, vec![1, 2, 0]),
                (Format::Tar, vec![11]),
            ]
        );
        assert_eq!(old_rar_volume("x.s01"), Some(("x", 102)));
    }

    /// A member read through `Cursor`, in odd-sized chunks and after seeks.
    fn check(volumes: Vec<Vec<u8>>, format: Format, member: &str, want: &[u8]) {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let set = Set {
            format,
            volumes: volumes.iter().enumerate().map(|(i, v)| Vol { file: i, len: v.len() as u64, path: format!("v{i}") }).collect(),
        };
        let mem = Mem(volumes);
        let idx = Arc::new(rt.block_on(index(&mem, &set)).unwrap());
        let m = idx.members.iter().position(|m| m.name == member).unwrap_or_else(|| panic!("{member} not in {:?}", idx.members));
        assert_eq!(idx.members[m].why_not, None);
        assert_eq!(idx.members[m].layout.lock().unwrap().total(), want.len() as u64);
        // Read the end first (as a player does for an index at the end of a film), then all of it.
        for start in [want.len() as u64 - 1000, 0, 12_345] {
            let mut c = Cursor { src: Mem(mem.0.clone()), archive: idx.clone(), member: m, pos: start };
            assert_eq!(c.len(), want.len() as u64);
            let mut got = Vec::new();
            let mut buf = vec![0u8; 3001];
            loop {
                let n = rt.block_on(c.read(&mut buf)).unwrap();
                if n == 0 {
                    break;
                }
                got.extend_from_slice(&buf[..n]);
            }
            assert!(got == want[start as usize..], "{member}: bytes from {start} differ");
        }
    }

    fn fixture(rel: &str) -> Vec<u8> {
        std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/archive/").to_string() + rel).unwrap()
    }
    fn volumes(names: &[&str]) -> Vec<Vec<u8>> {
        names.iter().map(|n| fixture(n)).collect()
    }

    #[test]
    fn rar5_multivolume_stored() {
        let want = fixture("Show.S01E01.mkv");
        check(volumes(&["r5split/show.part1.rar", "r5split/show.part2.rar", "r5split/show.part3.rar", "r5split/show.part4.rar"]), Format::Rar, "Show.S01E01.mkv", &want);
    }

    #[test]
    fn rar4_old_style_volumes_stored() {
        let want = fixture("Show.S01E01.mkv");
        check(volumes(&["r4old/show.rar", "r4old/show.r00", "r4old/show.r01", "r4old/show.r02"]), Format::Rar, "Show.S01E01.mkv", &want);
    }

    #[test]
    fn rar4_new_style_volumes_stored() {
        let want = fixture("Show.S01E01.mkv");
        check(volumes(&["r4new/show.part1.rar", "r4new/show.part2.rar", "r4new/show.part3.rar", "r4new/show.part4.rar"]), Format::Rar, "Show.S01E01.mkv", &want);
    }

    #[test]
    fn single_volume_rar_zip_zip64_tar() {
        let want = fixture("Show.S01E01.mkv");
        check(volumes(&["r5single/show.rar"]), Format::Rar, "Show.S01E01.mkv", &want);
        check(volumes(&["r4single/show.rar"]), Format::Rar, "Show.S01E01.mkv", &want);
        check(volumes(&["zip/show.zip"]), Format::Zip, "Show.S01E01.mkv", &want);
        check(volumes(&["zip/show64.zip"]), Format::Zip, "Show.S01E01.mkv", &want);
        check(volumes(&["tar/show.tar"]), Format::Tar, "Show.S01E01.mkv", &want);
        let long = format!("{}.mkv", "A".repeat(120));
        check(volumes(&["tar/long.tar"]), Format::Tar, &long, &want);
        check(volumes(&["tar/pax.tar"]), Format::Tar, &long, &want);
    }

    #[test]
    fn plain_split() {
        let want = fixture("Show.S01E01.mkv");
        let parts = want.chunks(15_000).map(|c| c.to_vec()).collect();
        check(parts, Format::Split, "v0", &want);
    }

    #[test]
    fn compressed_and_password_protected_are_explained() {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        let one = |rel: &str| {
            let v = fixture(rel);
            let set = Set { format: Format::Rar, volumes: vec![Vol { file: 0, len: v.len() as u64, path: rel.into() }] };
            rt.block_on(index(&Mem(vec![v]), &set))
        };
        // (Made from zeros: RAR stores incompressible data as is, even with -m3.)
        assert!(one("r5packed/show.rar").unwrap().members[0].why_not.as_deref().unwrap().contains("compression"));
        assert_eq!(one("r5enc/show.rar").unwrap().members[0].why_not.as_deref(), Some("password protected"));
        assert!(one("r5hdrenc/show.rar").unwrap_err().contains("password"));
    }

    #[test]
    fn a_wrong_prediction_falls_back_to_reading_in_order() {
        // Volume 3's piece is not where volume 2 predicts: the layout must notice.
        let mut l = Layout::spanning(Seg { off: 50, len: 950 }, vec![1000, 1000, 1000, 600], 3300);
        assert_eq!(l.locate(2000), Spot::Need(1));
        l.learn(1, Seg { off: 40, len: 940 }); // mid = (40, 20)
        assert_eq!(l.locate(2000), Spot::Need(2), "predicted, so checked first");
        l.learn(2, Seg { off: 41, len: 939 });
        assert_eq!(l.locate(10), Spot::At { vol: 0, off: 60, avail: 940 });
        assert_eq!(l.locate(950 + 940 + 5), Spot::At { vol: 2, off: 46, avail: 934 });
        assert_eq!(l.locate(3299), Spot::Need(3), "no more guessing after a miss");
    }
}
