//! 🐧 Linux virtualize — boot a Linux image ZenTorrent downloaded (.iso / .img)
//! in a virtual machine on this computer, with QEMU. A button in the Flux MoE tab.
//!
//! * the images: the torrents' files plus the Save-to folder, finished ones first
//! * QEMU: found where its installers put it; on Windows one click installs it with
//!   winget (in a console window, so Windows' own prompts show), on Linux the
//!   package command to copy
//! * speed: KVM (Linux) or the Windows Hypervisor Platform when present, else
//!   QEMU's software CPU — works everywhere, slower
//! * a downloaded image is never written to: an ISO is a read-only CD, a raw .img
//!   runs with `snapshot=on`, so seeding goes on and the torrent stays valid
//! * "Keep a disk": a growing qcow2 in ZenTorrent's data folder to install into;
//!   the VM starts from it once something is installed, else from the image

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText};

use super::skills::Library;

/// winget's id for QEMU's official Windows build (qemu.weilnetz.de).
const WINGET_ID: &str = "SoftwareFreedomConservancy.QEMU";
const DOWNLOAD_PAGE: &str = "https://www.qemu.org/download/";
/// Size limit of a kept disk; it only takes what the guest writes.
const DISK_GB: u32 = 32;
/// A VM that ends this soon with an error did not start: show why.
const QUICK_FAIL: Duration = Duration::from_secs(20);

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    /// Booted as a CD.
    Iso,
    /// A disk image (USB-stick style), booted as a disk without writing to it.
    Raw,
}

pub fn kind_of(p: &Path) -> Option<Kind> {
    match p.extension()?.to_str()?.to_ascii_lowercase().as_str() {
        "iso" => Some(Kind::Iso),
        "img" | "raw" => Some(Kind::Raw),
        _ => None,
    }
}

/// Images built for another processor than this PC's (x86-64) — named in the file name.
pub fn foreign_arch(name: &str) -> Option<&'static str> {
    let n = name.to_ascii_lowercase();
    [("aarch64", "ARM"), ("arm64", "ARM"), ("armhf", "ARM"), ("armv7", "ARM"), ("armel", "ARM"), ("riscv64", "RISC-V"), ("ppc64", "PowerPC"), ("s390x", "IBM Z")]
        .into_iter()
        .find(|(k, _)| n.contains(k))
        .map(|(_, a)| a)
}

/// Can the VM start from an image? Read from its first sectors, not guessed from the name:
/// a Downloads folder also holds game discs and phone images (2026-10-30: a voice-pack ISO
/// and an Android recovery .img were offered, and the VM said "No bootable device").
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Boot {
    /// Starts with the VM's BIOS.
    Yes,
    /// Bootable only with UEFI firmware.
    UefiOnly,
    /// An Android boot/recovery image: made for a phone.
    Android,
    /// No start-up record: a data disc, or a disk without a system.
    No,
    /// Not read (still downloading, or unreadable).
    Unknown,
}

impl Boot {
    /// Why it cannot be started, for the list.
    pub fn why_not(self) -> Option<&'static str> {
        match self {
            Boot::Yes => None,
            Boot::UefiOnly => Some("starts only with UEFI, which this does not do yet"),
            Boot::Android => Some("an Android phone image, not a system for a PC"),
            Boot::No => Some("not bootable — no start-up record (a data disc?)"),
            Boot::Unknown => Some("could not be read"),
        }
    }
}

const SECTOR: u64 = 2048;

pub fn boot_of(path: &Path, kind: Kind) -> Boot {
    use std::io::{Read, Seek, SeekFrom};
    let Ok(mut f) = std::fs::File::open(path) else { return Boot::Unknown };
    let mut head = [0u8; 1024];
    if f.read_exact(&mut head).is_err() {
        return Boot::No;
    }
    if head.starts_with(b"ANDROID!") || head.starts_with(b"VNDRBOOT") {
        return Boot::Android;
    }
    if kind == Kind::Raw {
        // A PC disk starts with a master boot record (also the guard record of a GPT disk).
        return if head[510..512] == [0x55, 0xAA] { Boot::Yes } else { Boot::No };
    }
    // A CD starts through El Torito: a boot record at sector 17 points to the boot catalog.
    let mut rec = [0u8; 0x4B];
    if f.seek(SeekFrom::Start(17 * SECTOR)).is_err() || f.read_exact(&mut rec).is_err() {
        return Boot::No;
    }
    if rec[0] != 0 || &rec[1..6] != b"CD001" || !rec[7..].starts_with(b"EL TORITO SPECIFICATION") {
        return Boot::No;
    }
    let lba = u32::from_le_bytes([rec[0x47], rec[0x48], rec[0x49], rec[0x4A]]) as u64;
    let mut cat = [0u8; SECTOR as usize];
    if f.seek(SeekFrom::Start(lba * SECTOR)).is_err() || f.read_exact(&mut cat).is_err() || cat[0] != 1 {
        return Boot::No;
    }
    // The validation entry names the default entry's platform; section headers (0x90, last 0x91)
    // name the others. 0 = PC BIOS, 0xEF = UEFI.
    let mut platforms = vec![cat[1]];
    platforms.extend(cat.chunks(32).skip(2).filter(|e| e[0] == 0x90 || e[0] == 0x91).map(|e| e[1]));
    if platforms.contains(&0) {
        Boot::Yes
    } else if platforms.contains(&0xEF) {
        Boot::UefiOnly
    } else {
        Boot::No
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Image {
    pub path: PathBuf,
    pub name: String,
    pub size: u64,
    pub kind: Kind,
    /// Downloaded completely.
    pub ready: bool,
    /// Which torrent it came from, or where it was found.
    pub from: String,
    pub boot: Boot,
}

impl Image {
    fn new(path: PathBuf, size: u64, kind: Kind, ready: bool, from: String) -> Image {
        let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let boot = if ready { boot_of(&path, kind) } else { Boot::Unknown };
        Image { path, name, size, kind, ready, from, boot }
    }

    /// Why the VM cannot start it, if it cannot.
    pub fn why_not(&self) -> Option<String> {
        if let Some(a) = foreign_arch(&self.name) {
            return Some(format!("built for {a}, this PC is x86-64"));
        }
        if !self.ready {
            return Some(format!("still downloading — {}", self.from));
        }
        self.boot.why_not().map(String::from)
    }
}

/// The images in the library: torrents' files, then loose ones in the Save-to folder.
pub fn images(lib: &Library) -> Vec<Image> {
    let mut out: Vec<Image> = Vec::new();
    for t in &lib.torrents {
        for (rel, size) in &t.files {
            let path = t.root.join(rel);
            let Some(kind) = kind_of(&path) else { continue };
            let ready = t.progress >= 0.9999 && std::fs::metadata(&path).map(|m| m.len() == *size).unwrap_or(false);
            out.push(Image::new(path, *size, kind, ready, format!("torrent “{}” · {:.0} %", t.name, t.progress * 100.0)));
        }
    }
    let mut loose = Vec::new();
    walk(&lib.folder, 2, &mut loose, &mut 0);
    for path in loose {
        if out.iter().any(|i| same(&i.path, &path)) {
            continue;
        }
        let (Some(kind), Ok(m)) = (kind_of(&path), std::fs::metadata(&path)) else { continue };
        out.push(Image::new(path, m.len(), kind, true, "in your download folder".into()));
    }
    // Startable first, then finished, then by name.
    out.sort_by(|a, b| {
        (b.why_not().is_none(), b.ready).cmp(&(a.why_not().is_none(), a.ready)).then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    out
}

fn same(a: &Path, b: &Path) -> bool {
    a == b || matches!((a.canonicalize(), b.canonicalize()), (Ok(x), Ok(y)) if x == y)
}

/// Image files under `dir`, `depth` folders deep, at most 5000 entries looked at.
fn walk(dir: &Path, depth: u32, out: &mut Vec<PathBuf>, seen: &mut u32) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        *seen += 1;
        if *seen > 5000 {
            return;
        }
        let p = e.path();
        let hidden = e.file_name().to_string_lossy().starts_with('.');
        // metadata() follows links: a linked image or folder counts too.
        match std::fs::metadata(&p) {
            Ok(m) if m.is_dir() && depth > 0 && !hidden => walk(&p, depth - 1, out, seen),
            Ok(m) if m.is_file() && kind_of(&p).is_some() => out.push(p),
            _ => {}
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Qemu {
    pub bin: PathBuf,
    /// `qemu-img`, for a kept disk.
    pub img: Option<PathBuf>,
    pub version: String,
}

fn quiet(cmd: &mut Command) -> &mut Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW (QEMU's own window still shows)
    }
    cmd
}

fn exe(name: &str) -> String {
    if cfg!(windows) { format!("{name}.exe") } else { name.to_string() }
}

/// Where QEMU's installers put it, then PATH.
fn qemu_dirs() -> Vec<PathBuf> {
    let mut d: Vec<PathBuf> = Vec::new();
    if cfg!(windows) {
        for var in ["ProgramFiles", "ProgramW6432", "ProgramFiles(x86)"] {
            if let Ok(p) = std::env::var(var) {
                d.push(PathBuf::from(p).join("qemu"));
            }
        }
        if let Some(l) = dirs::data_local_dir() {
            d.push(l.join("Programs").join("qemu"));
        }
        d.push(PathBuf::from(r"C:\msys64\ucrt64\bin"));
    } else {
        d.extend(["/usr/bin", "/usr/local/bin", "/opt/homebrew/bin"].map(PathBuf::from));
    }
    if let Some(path) = std::env::var_os("PATH") {
        d.extend(std::env::split_paths(&path));
    }
    d
}

/// "QEMU emulator version 8.2.2 (Debian 1:8.2.2+ds-0ubuntu1)" → "8.2.2".
pub fn parse_version(out: &str) -> Option<String> {
    let line = out.lines().find(|l| l.contains("version"))?;
    let v = line.split("version").nth(1)?.split_whitespace().next()?;
    Some(v.trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '.').to_string())
}

pub fn find_qemu() -> Option<Qemu> {
    let bin = qemu_dirs().into_iter().map(|d| d.join(exe("qemu-system-x86_64"))).find(|p| p.is_file())?;
    let img = bin.with_file_name(exe("qemu-img"));
    let out = quiet(Command::new(&bin).arg("--version").stdin(Stdio::null())).output().ok()?;
    let version = parse_version(&String::from_utf8_lossy(&out.stdout)).unwrap_or_else(|| "?".into());
    Some(Qemu { img: img.is_file().then_some(img), bin, version })
}

/// How fast the VM can run here.
#[derive(Clone, Debug, PartialEq)]
pub struct Accel {
    pub args: Vec<String>,
    pub label: String,
    /// How to make it faster, when it is the slow kind.
    pub hint: Option<String>,
}

fn owned(a: &[&str]) -> Vec<String> {
    a.iter().map(|s| s.to_string()).collect()
}

/// Is the Windows Hypervisor Platform on and running? Asks it the way QEMU does
/// (`WHvGetCapability(HypervisorPresent)`): the DLL alone is there on PCs where it is off.
#[cfg(windows)]
fn whpx_ready() -> bool {
    type GetCapability = unsafe extern "system" fn(u32, *mut u8, u32, *mut u32) -> i32;
    unsafe {
        let Ok(lib) = libloading::Library::new("WinHvPlatform.dll") else { return false };
        let Ok(get) = lib.get::<GetCapability>(b"WHvGetCapability\0") else { return false };
        let mut buf = [0u8; 64];
        let mut written = 0u32;
        get(0, buf.as_mut_ptr(), buf.len() as u32, &mut written) >= 0 && u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) != 0
    }
}

pub fn accel() -> Accel {
    #[cfg(windows)]
    {
        if whpx_ready() {
            // QEMU tries them in order: the software CPU if the hypervisor is not running.
            return Accel {
                args: owned(&["-accel", "whpx,kernel-irqchip=off", "-accel", "tcg"]),
                label: "Windows Hypervisor Platform (fast)".into(),
                hint: None,
            };
        }
        Accel {
            args: owned(&["-accel", "tcg", "-cpu", "max"]),
            label: "software CPU (works everywhere, slower)".into(),
            hint: Some(
                "Much faster: turn on “Windows Hypervisor Platform” (Start → “Turn Windows features on or off”), then restart Windows. It is free, also on Windows Home."
                    .into(),
            ),
        }
    }
    #[cfg(not(windows))]
    {
        let kvm = Path::new("/dev/kvm");
        if std::fs::OpenOptions::new().read(true).write(true).open(kvm).is_ok() {
            return Accel { args: owned(&["-accel", "kvm", "-cpu", "host"]), label: "KVM (near full speed)".into(), hint: None };
        }
        let hint = if kvm.exists() {
            "Much faster: let your user use KVM — sudo usermod -aG kvm $USER — then log out and in again."
        } else {
            "Much faster: turn on virtualization (Intel VT-x / AMD-V) in the BIOS, so /dev/kvm appears."
        };
        Accel { args: owned(&["-accel", "tcg", "-cpu", "max"]), label: "software CPU (works everywhere, slower)".into(), hint: Some(hint.into()) }
    }
}

/// Memory in this computer, MB.
pub fn total_ram_mb() -> u64 {
    #[cfg(windows)]
    {
        use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
        let mut m: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
        m.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
        if unsafe { GlobalMemoryStatusEx(&mut m) } != 0 {
            return m.ullTotalPhys >> 20;
        }
        8192
    }
    #[cfg(not(windows))]
    {
        std::fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|s| s.lines().find(|l| l.starts_with("MemTotal:"))?.split_whitespace().nth(1)?.parse::<u64>().ok())
            .map(|kb| kb >> 10)
            .unwrap_or(8192)
    }
}

/// A quarter of the memory, 1–4 GB, in 512 MB steps.
pub fn default_ram_mb(total: u64) -> u32 {
    ((total / 4).clamp(1024, 4096) / 512 * 512) as u32
}

pub fn default_cpus() -> u32 {
    let n = std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(2);
    (n / 2).clamp(1, 4)
}

pub fn vm_dir() -> PathBuf {
    crate::seed::data_dir().join("vm")
}

/// The image's name made safe for a file name.
fn stem(image: &Path) -> String {
    let s = image.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "linux".into());
    s.chars().map(|c| if c.is_ascii_alphanumeric() || "-_.".contains(c) { c } else { '_' }).collect()
}

pub fn disk_path(image: &Path) -> PathBuf {
    vm_dir().join(format!("{}.qcow2", stem(image)))
}

/// QEMU's option syntax: a comma inside a value is written twice.
fn esc(p: &Path) -> String {
    p.to_string_lossy().replace(',', ",,")
}

#[derive(Clone, Debug)]
pub struct Plan {
    pub image: PathBuf,
    pub kind: Kind,
    pub ram_mb: u32,
    pub cpus: u32,
    /// A disk to install into (ISO only).
    pub disk: Option<PathBuf>,
}

pub fn args(p: &Plan, accel: &Accel) -> Vec<String> {
    let name = p.image.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let mut a = owned(&["-name"]);
    a.push(format!("{name} — ZenTorrent"));
    a.extend(accel.args.iter().cloned());
    a.extend(["-m".into(), p.ram_mb.to_string(), "-smp".into(), p.cpus.to_string()]);
    // Resizable screen, a mouse that is not captured, and internet through the host.
    a.extend(owned(&["-vga", "virtio", "-usb", "-device", "usb-tablet", "-nic", "user,model=virtio-net-pci"]));
    match p.kind {
        Kind::Iso => {
            a.extend(["-drive".into(), format!("file={},media=cdrom,readonly=on", esc(&p.image))]);
            if let Some(d) = &p.disk {
                a.extend(["-drive".into(), format!("file={},format=qcow2,if=virtio", esc(d))]);
                // The disk once something is installed on it; until then the BIOS falls through to the CD.
                a.extend(owned(&["-boot", "order=cd,menu=on"]));
            } else {
                a.extend(owned(&["-boot", "order=d"]));
            }
        }
        Kind::Raw => {
            // snapshot=on: the guest's writes go to a temporary file, never into the download.
            a.extend(["-drive".into(), format!("file={},format=raw,if=virtio,snapshot=on", esc(&p.image))]);
        }
    }
    // For tests: extra QEMU options (e.g. "-display none -vnc :5").
    if let Ok(extra) = std::env::var("ZENTORRENT_VM_ARGS") {
        a.extend(extra.split_whitespace().map(String::from));
    }
    a
}

pub struct Running {
    pub name: String,
    pub image: PathBuf,
    pub child: Child,
    pub started: Instant,
    pub log: PathBuf,
}

/// Last lines of a VM's log (what QEMU said when it failed).
pub fn tail(log: &Path, n: usize) -> String {
    let s = std::fs::read_to_string(log).unwrap_or_default();
    let lines: Vec<&str> = s.lines().filter(|l| !l.trim().is_empty()).collect();
    lines[lines.len().saturating_sub(n)..].join("\n")
}

/// Make the disk if asked for and missing, then start QEMU (its own window).
pub fn launch(q: &Qemu, plan: &Plan, accel: &Accel) -> Result<Running, String> {
    std::fs::create_dir_all(vm_dir()).map_err(|e| format!("could not make {}: {e}", vm_dir().display()))?;
    if let Some(d) = plan.disk.as_ref().filter(|d| !d.exists()) {
        let img = q.img.as_ref().ok_or("qemu-img is missing next to QEMU, so no disk can be made")?;
        let out = quiet(Command::new(img).args(["create", "-f", "qcow2"]).arg(d).arg(format!("{DISK_GB}G")).stdin(Stdio::null()))
            .output()
            .map_err(|e| format!("could not run qemu-img: {e}"))?;
        if !out.status.success() {
            return Err(format!("could not make the disk: {}", String::from_utf8_lossy(&out.stderr).trim()));
        }
    }
    let log = vm_dir().join(format!("{}.log", stem(&plan.image)));
    let f = std::fs::File::create(&log).map_err(|e| format!("could not write {}: {e}", log.display()))?;
    let child = quiet(
        Command::new(&q.bin)
            .args(args(plan, accel))
            .stdin(Stdio::null())
            .stdout(f.try_clone().map_err(|e| e.to_string())?)
            .stderr(f),
    )
    .spawn()
    .map_err(|e| format!("could not start QEMU ({}): {e}", q.bin.display()))?;
    let name = plan.image.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    Ok(Running { name, image: plan.image.clone(), child, started: Instant::now(), log })
}

/// `zentorrent --vm <image> [--disk]`: what the Start button does, from a terminal;
/// waits until the VM ends.
pub fn cli(image: &Path, disk: bool) -> Result<(), String> {
    let kind = kind_of(image).ok_or("not an .iso or .img")?;
    if !image.is_file() {
        return Err(format!("{} not found", image.display()));
    }
    if let Some(why) = boot_of(image, kind).why_not() {
        return Err(format!("{} cannot start: {why}", image.display()));
    }
    let q = find_qemu().ok_or(if cfg!(windows) {
        format!("QEMU is not installed: winget install --id {WINGET_ID}  (or {DOWNLOAD_PAGE})")
    } else {
        "QEMU is not installed: sudo apt install qemu-system-x86 qemu-utils".to_string()
    })?;
    let a = accel();
    let plan = Plan { image: image.to_path_buf(), kind, ram_mb: default_ram_mb(total_ram_mb()), cpus: default_cpus(), disk: (disk && kind == Kind::Iso).then(|| disk_path(image)) };
    println!("QEMU {} ({}) · {}", q.version, q.bin.display(), a.label);
    if let Some(h) = &a.hint {
        println!("  {h}");
    }
    println!("{} {}", q.bin.display(), args(&plan, &a).join(" "));
    let mut r = launch(&q, &plan, &a)?;
    println!("started (pid {}), log: {}", r.child.id(), r.log.display());
    let st = r.child.wait().map_err(|e| e.to_string())?;
    if st.success() || r.started.elapsed() > QUICK_FAIL {
        println!("the VM ended after {} s", r.started.elapsed().as_secs());
        Ok(())
    } else {
        Err(format!("QEMU ended at once: {}", tail(&r.log, 6)))
    }
}

/// Windows: install QEMU with winget in a console window (its agreement and
/// Windows' admin prompt show there). Elsewhere: nothing to click.
pub fn install() -> Result<(), String> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        Command::new("winget")
            .args(["install", "--id", WINGET_ID, "--exact", "--source", "winget"])
            .creation_flags(0x0000_0010) // CREATE_NEW_CONSOLE
            .spawn()
            .map(|_| ())
            .map_err(|e| format!("could not start winget: {e}"))
    }
    #[cfg(not(windows))]
    {
        Err("install QEMU with your package manager".into())
    }
}

/// What the probe thread found.
#[derive(Clone)]
struct Probe {
    qemu: Option<Qemu>,
    accel: Accel,
    ram_total_mb: u64,
}

enum Note {
    Ok(String),
    Err(String),
}

/// The panel under the Flux MoE header.
pub struct Panel {
    pub open: bool,
    /// Filled by the probe thread, taken by the next frame.
    incoming: Arc<Mutex<Option<Probe>>>,
    probe: Option<Probe>,
    probing: bool,
    ram_set: bool,
    want_scan: bool,
    images: Vec<Image>,
    chosen: Option<PathBuf>,
    ram_mb: u32,
    cpus: u32,
    keep_disk: bool,
    running: Vec<Running>,
    notes: Vec<Note>,
    /// winget was started: look for QEMU again every few seconds.
    installing: Option<Instant>,
    last_look: Instant,
}

impl Default for Panel {
    fn default() -> Self {
        Panel {
            open: false,
            incoming: Default::default(),
            probe: None,
            probing: false,
            ram_set: false,
            want_scan: false,
            images: Vec::new(),
            chosen: None,
            ram_mb: 2048,
            cpus: default_cpus(),
            keep_disk: false,
            running: Vec::new(),
            notes: Vec::new(),
            installing: None,
            last_look: Instant::now(),
        }
    }
}

impl Panel {
    pub fn toggle(&mut self) {
        self.open = !self.open;
        if self.open {
            self.want_scan = true;
            self.look();
        }
    }

    /// The app hands over a library snapshot when this is true.
    pub fn wants_scan(&self) -> bool {
        self.open && self.want_scan
    }

    pub fn scan(&mut self, lib: &Library) {
        self.want_scan = false;
        let chosen_extra: Vec<Image> = self.images.iter().filter(|i| i.from == "chosen by you").cloned().collect();
        self.images = images(lib);
        for i in chosen_extra {
            if !self.images.iter().any(|x| x.path == i.path) {
                self.images.insert(0, i);
            }
        }
        if self.chosen.as_ref().is_none_or(|c| !self.images.iter().any(|i| &i.path == c && i.why_not().is_none())) {
            self.chosen = self.images.iter().find(|i| i.why_not().is_none()).map(|i| i.path.clone());
        }
    }

    /// Find QEMU and the speed on a thread (running `qemu --version` can take a moment).
    fn look(&mut self) {
        if self.probing {
            return;
        }
        self.probing = true;
        self.last_look = Instant::now();
        let slot = self.incoming.clone();
        std::thread::spawn(move || {
            let p = Probe { qemu: find_qemu(), accel: accel(), ram_total_mb: total_ram_mb() };
            *slot.lock().unwrap() = Some(p);
        });
    }

    /// Notice VMs that ended. Runs every frame the tab is shown.
    fn tick(&mut self) {
        let mut ended = Vec::new();
        for (i, r) in self.running.iter_mut().enumerate() {
            if let Ok(Some(st)) = r.child.try_wait() {
                ended.push((i, st.success(), r.started.elapsed()));
            }
        }
        for (i, ok, ran) in ended.into_iter().rev() {
            let r = self.running.remove(i);
            if ok || ran > QUICK_FAIL {
                self.notes.push(Note::Ok(format!("{} closed after {} min.", r.name, ran.as_secs() / 60)));
            } else {
                let why = tail(&r.log, 4);
                self.notes.push(Note::Err(format!("{} did not start: {}", r.name, if why.is_empty() { "QEMU ended without a message".into() } else { why })));
            }
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        self.tick();
        let fresh = self.incoming.lock().unwrap().take();
        if let Some(p) = fresh {
            self.probing = false;
            if !self.ram_set {
                self.ram_mb = default_ram_mb(p.ram_total_mb);
                self.ram_set = true;
            }
            if p.qemu.is_some() && self.installing.take().is_some() {
                self.notes.push(Note::Ok("QEMU is installed.".into()));
            }
            self.probe = Some(p);
        }
        let probe = self.probe.clone();
        // While winget runs: look again every 5 s (for up to 30 min).
        if let Some(t) = self.installing {
            if t.elapsed() > Duration::from_secs(1800) {
                self.installing = None;
            } else if self.last_look.elapsed() > Duration::from_secs(5) && probe.as_ref().is_none_or(|p| p.qemu.is_none()) {
                self.look();
            }
        }
        if self.probing || self.installing.is_some() || !self.running.is_empty() {
            ui.ctx().request_repaint_after(Duration::from_secs(1));
        }

        egui::Frame::new().fill(Color32::from_rgb(30, 36, 46)).corner_radius(8.0).inner_margin(egui::Margin::same(10)).show(ui, |ui| {
            ui.horizontal(|ui| {
                ui.label(RichText::new("🐧 Linux virtualize").strong());
                ui.label(RichText::new("— try a Linux image you downloaded, in a window, without installing it on this PC").weak());
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui.small_button("Close").clicked() {
                        self.open = false;
                    }
                });
            });
            ui.add_space(4.0);

            // ── QEMU ──
            let qemu = match &probe {
                None => {
                    ui.horizontal(|ui| {
                        ui.spinner();
                        ui.label(RichText::new("looking for QEMU…").weak());
                    });
                    None
                }
                Some(Probe { qemu: Some(q), accel, .. }) => {
                    ui.label(RichText::new(format!("QEMU {} · {}", q.version, accel.label)).small().color(Color32::from_rgb(120, 200, 140)));
                    if let Some(h) = &accel.hint {
                        ui.label(RichText::new(h).small().weak());
                    }
                    Some((q.clone(), accel.clone()))
                }
                Some(Probe { qemu: None, .. }) => {
                    self.missing_ui(ui);
                    None
                }
            };
            ui.add_space(6.0);

            // ── images ──
            ui.horizontal(|ui| {
                ui.label(RichText::new("Image").strong());
                if ui.small_button("Choose file…").on_hover_text("An .iso or .img from anywhere on this computer").clicked() {
                    if let Some(p) = rfd::FileDialog::new().add_filter("Linux image", &["iso", "img", "raw"]).pick_file() {
                        if let Some(kind) = kind_of(&p) {
                            let size = std::fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
                            let img = Image::new(p.clone(), size, kind, true, "chosen by you".into());
                            match img.why_not() {
                                None => self.chosen = Some(p.clone()),
                                Some(why) => self.notes.push(Note::Err(format!("{} cannot start: {why}.", img.name))),
                            }
                            self.images.retain(|i| i.path != p);
                            self.images.insert(0, img);
                        }
                    }
                }
                if ui.small_button("Look again").on_hover_text("Find images in your torrents and download folder again").clicked() {
                    self.want_scan = true;
                    self.look();
                }
            });
            if self.images.is_empty() {
                ui.label(
                    RichText::new("No .iso or .img in your torrents or download folder yet. Download a Linux image (e.g. Debian, Ubuntu, Fedora — they all offer torrents), or choose a file.")
                        .small()
                        .weak(),
                );
            }
            egui::ScrollArea::vertical().id_salt("vm-images").max_height(150.0).show(ui, |ui| {
                for img in &self.images {
                    let why_not = img.why_not();
                    let mut text = format!("{}  ·  {:.2} GB", img.name, img.size as f64 / 1e9);
                    if img.kind == Kind::Raw {
                        text += "  ·  disk image";
                    }
                    ui.horizontal(|ui| {
                        let sel = self.chosen.as_ref() == Some(&img.path);
                        if ui.add_enabled(why_not.is_none(), egui::RadioButton::new(sel, text)).clicked() {
                            self.chosen = Some(img.path.clone());
                        }
                        match &why_not {
                            Some(w) => ui.label(RichText::new(w).small().color(Color32::from_rgb(200, 150, 90))),
                            None => ui.label(RichText::new(&img.from).small().weak()),
                        };
                    });
                }
            });

            // ── options ──
            let chosen = self.chosen.clone().and_then(|c| self.images.iter().find(|i| i.path == c).cloned());
            let total = probe.as_ref().map(|p| p.ram_total_mb).unwrap_or(8192);
            ui.add_space(4.0);
            ui.horizontal_wrapped(|ui| {
                let max_ram = ((total / 2).max(1024) / 512 * 512) as u32;
                ui.label("Memory");
                ui.add(egui::Slider::new(&mut self.ram_mb, 512..=max_ram).step_by(512.0).suffix(" MB"));
                ui.label("CPUs");
                let n = std::thread::available_parallelism().map(|n| n.get() as u32).unwrap_or(2);
                ui.add(egui::Slider::new(&mut self.cpus, 1..=n.max(1)));
                let iso = chosen.as_ref().is_some_and(|c| c.kind == Kind::Iso);
                ui.add_enabled(iso, egui::Checkbox::new(&mut self.keep_disk, format!("Keep a disk (install into it, up to {DISK_GB} GB)")))
                    .on_hover_text("A virtual disk in ZenTorrent's data folder that only takes the space the VM writes. Next time it starts from what you installed.");
            });
            if let Some(c) = chosen.as_ref().filter(|c| c.kind == Kind::Iso && disk_path(&c.path).exists()) {
                ui.label(RichText::new(format!("Its disk: {}", disk_path(&c.path).display())).small().weak());
            }

            ui.add_space(4.0);
            ui.horizontal(|ui| {
                // One VM per image: a second one could not open the same disk.
                let already = chosen.as_ref().is_some_and(|c| self.running.iter().any(|r| r.image == c.path));
                let can = qemu.is_some() && chosen.as_ref().is_some_and(|c| c.why_not().is_none()) && !already;
                let start = ui
                    .add_enabled(can, egui::Button::new(RichText::new("▶ Start the virtual machine").strong()))
                    .on_disabled_hover_text(if already { "It is running already — see its window" } else { "Choose an image, and QEMU is needed" });
                if start.clicked() {
                    if let (Some((q, a)), Some(img)) = (&qemu, &chosen) {
                        let disk = (img.kind == Kind::Iso && self.keep_disk).then(|| disk_path(&img.path));
                        let plan = Plan { image: img.path.clone(), kind: img.kind, ram_mb: self.ram_mb, cpus: self.cpus, disk };
                        match launch(q, &plan, a) {
                            Ok(r) => {
                                self.notes.push(Note::Ok(format!("{} is starting in its own window. Closing that window turns the VM off.", r.name)));
                                self.running.push(r);
                            }
                            Err(e) => self.notes.push(Note::Err(e)),
                        }
                    }
                }
                if ui.small_button("Open VM folder").on_hover_text("Kept disks and QEMU's logs").clicked() {
                    let _ = std::fs::create_dir_all(vm_dir());
                    let _ = crate::open_folder(&vm_dir());
                }
            });

            // ── running / what happened ──
            let mut stop = None;
            for (i, r) in self.running.iter().enumerate() {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(format!("Running: {} · {} min", r.name, r.started.elapsed().as_secs() / 60)).color(Color32::from_rgb(120, 200, 140)));
                    if ui.small_button("Turn off").on_hover_text("Ends the VM at once (like pulling the plug)").clicked() {
                        stop = Some(i);
                    }
                });
            }
            if let Some(i) = stop {
                let mut r = self.running.remove(i);
                let _ = r.child.kill();
                let _ = r.child.wait();
                self.notes.push(Note::Ok(format!("{} turned off.", r.name)));
            }
            let skip = self.notes.len().saturating_sub(4);
            for n in &self.notes[skip..] {
                match n {
                    Note::Ok(t) => ui.label(RichText::new(t).small().weak()),
                    Note::Err(t) => ui.colored_label(Color32::from_rgb(230, 120, 100), t),
                };
            }
        });
    }

    fn missing_ui(&mut self, ui: &mut egui::Ui) {
        ui.label(RichText::new("The virtual machine runs in QEMU (free, open source), which is not on this computer yet.").color(Color32::from_rgb(240, 170, 60)));
        if cfg!(windows) {
            ui.horizontal_wrapped(|ui| {
                if self.installing.is_some() {
                    ui.spinner();
                    ui.label(RichText::new("Installing with winget in the console window — answer its questions there. This panel notices when it is done.").small());
                } else if ui
                    .button(RichText::new("Install QEMU (≈ 200 MB download)").strong())
                    .on_hover_text(format!("Runs: winget install --id {WINGET_ID} — the official QEMU build. Windows asks for permission."))
                    .clicked()
                {
                    match install() {
                        Ok(()) => self.installing = Some(Instant::now()),
                        Err(e) => self.notes.push(Note::Err(format!("{e}. Get QEMU from {DOWNLOAD_PAGE} instead."))),
                    }
                }
                if ui.small_button("Download page").on_hover_text(DOWNLOAD_PAGE).clicked() {
                    let _ = crate::open_folder(Path::new(DOWNLOAD_PAGE));
                }
            });
        } else {
            let cmds = "Debian / Ubuntu:  sudo apt install qemu-system-x86 qemu-utils\nFedora:           sudo dnf install qemu-system-x86 qemu-img\nArch:             sudo pacman -S qemu-desktop";
            ui.add(egui::Label::new(RichText::new(cmds).monospace().size(11.0)).selectable(true));
            ui.horizontal(|ui| {
                if ui.small_button("📋 Copy").clicked() {
                    ui.ctx().copy_text(cmds.into());
                }
                if ui.small_button("Look again").clicked() {
                    self.look();
                }
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::moe::skills::Torrent;

    fn plan(kind: Kind, image: &str, disk: Option<&str>) -> Plan {
        Plan { image: image.into(), kind, ram_mb: 2048, cpus: 2, disk: disk.map(PathBuf::from) }
    }

    fn tcg() -> Accel {
        Accel { args: owned(&["-accel", "tcg"]), label: String::new(), hint: None }
    }

    #[test]
    fn kinds_and_arches() {
        assert_eq!(kind_of(Path::new("debian-13.7.0-amd64-netinst.ISO")), Some(Kind::Iso));
        assert_eq!(kind_of(Path::new("a/b/raspios.img")), Some(Kind::Raw));
        assert_eq!(kind_of(Path::new("x.iso.torrent")), None);
        assert_eq!(kind_of(Path::new("x.img.xz")), None);
        assert_eq!(foreign_arch("ubuntu-24.04-live-server-arm64.iso"), Some("ARM"));
        assert_eq!(foreign_arch("Fedora-Server-aarch64-41.iso"), Some("ARM"));
        assert_eq!(foreign_arch("debian-13.7.0-amd64-netinst.iso"), None);
        assert_eq!(foreign_arch("debian-12-i386-netinst.iso"), None);
    }

    /// A CD image with an El Torito boot catalog whose entries are for `platforms`.
    fn cd(platforms: &[u8]) -> Vec<u8> {
        let mut b = vec![0u8; 21 * SECTOR as usize];
        let rec = 17 * SECTOR as usize;
        b[rec + 1..rec + 6].copy_from_slice(b"CD001");
        b[rec + 6] = 1;
        b[rec + 7..rec + 30].copy_from_slice(b"EL TORITO SPECIFICATION");
        b[rec + 0x47..rec + 0x4B].copy_from_slice(&20u32.to_le_bytes());
        let cat = 20 * SECTOR as usize;
        b[cat] = 1;
        b[cat + 1] = platforms[0];
        b[cat + 32] = 0x88;
        for (i, p) in platforms[1..].iter().enumerate() {
            let e = cat + 64 + i * 64;
            b[e] = if i + 2 == platforms.len() { 0x91 } else { 0x90 };
            b[e + 1] = *p;
            b[e + 32] = 0x88;
        }
        b
    }

    #[test]
    fn only_bootable_images_can_start() {
        let dir = std::env::temp_dir().join(format!("zt-vm-boot-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = |name: &str, bytes: Vec<u8>| {
            let p = dir.join(name);
            std::fs::write(&p, bytes).unwrap();
            p
        };
        // A hybrid distro ISO (BIOS default + UEFI section), a UEFI-only one, a data disc.
        assert_eq!(boot_of(&file("distro.iso", cd(&[0, 0xEF])), Kind::Iso), Boot::Yes);
        assert_eq!(boot_of(&file("uefi.iso", cd(&[0xEF])), Kind::Iso), Boot::UefiOnly);
        let mut data = cd(&[0]);
        data[17 * SECTOR as usize + 1] = b'X';
        assert_eq!(boot_of(&file("voices.iso", data), Kind::Iso), Boot::No);
        // Disk images: an MBR disk, an Android recovery, blank.
        let mut mbr = vec![0u8; 4096];
        mbr[510] = 0x55;
        mbr[511] = 0xAA;
        assert_eq!(boot_of(&file("stick.img", mbr), Kind::Raw), Boot::Yes);
        let mut android = b"ANDROID!".to_vec();
        android.resize(4096, 0);
        assert_eq!(boot_of(&file("twrp-3.4.0-0-guacamole.img", android), Kind::Raw), Boot::Android);
        assert_eq!(boot_of(&file("blank.img", vec![0u8; 4096]), Kind::Raw), Boot::No);
        assert_eq!(boot_of(&file("tiny.iso", vec![1, 2, 3]), Kind::Iso), Boot::No);
        assert_eq!(boot_of(&dir.join("gone.iso"), Kind::Iso), Boot::Unknown);
        let img = Image::new(dir.join("twrp-3.4.0-0-guacamole.img"), 4096, Kind::Raw, true, String::new());
        assert!(img.why_not().unwrap().contains("Android"));
        assert_eq!(Image::new(dir.join("distro.iso"), 1, Kind::Iso, true, String::new()).why_not(), None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn versions() {
        assert_eq!(parse_version("QEMU emulator version 8.2.2 (Debian 1:8.2.2+ds-0ubuntu1.18)\nCopyright"), Some("8.2.2".into()));
        assert_eq!(parse_version("QEMU emulator version 11.1.0 (v11.1.0-12046-g1234)\n"), Some("11.1.0".into()));
        assert_eq!(parse_version("nothing"), None);
    }

    #[test]
    fn iso_boots_as_a_read_only_cd() {
        let a = args(&plan(Kind::Iso, "/d/debian,13.iso", None), &tcg());
        let s = a.join(" ");
        assert!(s.contains("-drive file=/d/debian,,13.iso,media=cdrom,readonly=on"), "{s}");
        assert!(s.contains("-boot order=d"));
        assert!(s.contains("-accel tcg") && s.contains("-m 2048") && s.contains("-smp 2"));
        assert!(!s.contains("qcow2"));
        assert!(a.contains(&"debian,13.iso — ZenTorrent".to_string()));
    }

    #[test]
    fn disk_comes_first_once_installed() {
        let s = args(&plan(Kind::Iso, "/d/x.iso", Some("/vm/x.qcow2")), &tcg()).join(" ");
        assert!(s.contains("file=/vm/x.qcow2,format=qcow2,if=virtio"));
        assert!(s.contains("-boot order=cd,menu=on"));
    }

    #[test]
    fn raw_image_is_never_written() {
        let s = args(&plan(Kind::Raw, "/d/x.img", None), &tcg()).join(" ");
        assert!(s.contains("file=/d/x.img,format=raw,if=virtio,snapshot=on"), "{s}");
        assert!(!s.contains("-boot"));
    }

    #[test]
    fn ram_defaults() {
        assert_eq!(default_ram_mb(16 * 1024), 4096);
        assert_eq!(default_ram_mb(8 * 1024), 2048);
        assert_eq!(default_ram_mb(2 * 1024), 1024);
        assert_eq!(default_ram_mb(64 * 1024), 4096);
        assert_eq!(default_ram_mb(6000), 1024);
    }

    #[test]
    fn disk_names_are_safe() {
        assert!(disk_path(Path::new("/d/My Distro: 1.0.iso")).ends_with("My_Distro__1.0.qcow2"));
    }

    #[test]
    fn finds_images_in_torrents_and_folder() {
        let root = std::env::temp_dir().join(format!("zt-vm-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let t1 = root.join("Debian");
        std::fs::create_dir_all(&t1).unwrap();
        std::fs::write(t1.join("debian-13-amd64-netinst.iso"), vec![0u8; 100]).unwrap();
        std::fs::write(t1.join("SHA256SUMS"), b"x").unwrap();
        let t2 = root.join("Fedora");
        std::fs::create_dir_all(&t2).unwrap();
        std::fs::write(t2.join("Fedora-41-x86_64.iso"), vec![0u8; 10]).unwrap();
        std::fs::create_dir_all(root.join("misc/deeper")).unwrap();
        std::fs::write(root.join("misc/alpine-virt.iso"), b"iso").unwrap();
        std::fs::write(root.join("misc/notes.txt"), b"x").unwrap();
        let tor = |name: &str, rootp: &Path, file: &str, size: u64, progress: f64| Torrent {
            name: name.into(),
            files: vec![(file.into(), size), ("SHA256SUMS".into(), 1)],
            root: rootp.to_path_buf(),
            progress,
            ..Default::default()
        };
        let lib = Library {
            folder: root.clone(),
            torrents: vec![tor("Debian", &t1, "debian-13-amd64-netinst.iso", 100, 1.0), tor("Fedora", &t2, "Fedora-41-x86_64.iso", 2000, 0.4)],
            feeds: Vec::new(),
        };
        let found = images(&lib);
        let names: Vec<(&str, bool)> = found.iter().map(|i| (i.name.as_str(), i.ready)).collect();
        // Ready first (by name), the unfinished Fedora last, each file once.
        assert_eq!(names, vec![("alpine-virt.iso", true), ("debian-13-amd64-netinst.iso", true), ("Fedora-41-x86_64.iso", false)]);
        assert!(found[1].from.contains("Debian"));
        let _ = std::fs::remove_dir_all(&root);
    }
}
