//! Live machine load while Flux MoE answers: GPU and graphics memory from
//! `nvidia-smi`, CPU from the system's own counters. Sampled once a second on a
//! small thread, only while an answer is being generated. `None` = not measurable
//! here (no NVIDIA card, or a system without the counter).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Load {
    pub gpu_pct: Option<u32>,
    pub vram_used_mb: Option<u64>,
    pub vram_total_mb: Option<u64>,
    pub gpu_temp_c: Option<u32>,
    pub cpu_pct: Option<u32>,
}

/// `nvidia-smi` without a console window flashing up on Windows.
fn nvidia() -> Option<Vec<u64>> {
    let mut cmd = std::process::Command::new("nvidia-smi");
    cmd.args(["--query-gpu=utilization.gpu,memory.used,memory.total,temperature.gpu", "--format=csv,noheader,nounits"]);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000);
    }
    let out = cmd.output().ok()?;
    let p: Vec<u64> = String::from_utf8_lossy(&out.stdout).lines().next()?.split(',').filter_map(|x| x.trim().parse().ok()).collect();
    (p.len() == 4).then_some(p)
}

/// (total, idle) CPU time counters; their change over a second gives the load.
#[cfg(target_os = "linux")]
fn cpu_times() -> Option<(u64, u64)> {
    let s = std::fs::read_to_string("/proc/stat").ok()?;
    let f: Vec<u64> = s.lines().next()?.split_whitespace().skip(1).filter_map(|x| x.parse().ok()).collect();
    (f.len() >= 4).then(|| (f.iter().sum(), f[3] + f.get(4).copied().unwrap_or(0)))
}

#[cfg(windows)]
fn cpu_times() -> Option<(u64, u64)> {
    use windows_sys::Win32::Foundation::FILETIME;
    use windows_sys::Win32::System::Threading::GetSystemTimes;
    let z = || FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut idle, mut kernel, mut user) = (z(), z(), z());
    // SAFETY: three valid out-pointers to FILETIME.
    if unsafe { GetSystemTimes(&mut idle, &mut kernel, &mut user) } == 0 {
        return None;
    }
    let n = |f: FILETIME| (f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64;
    // Kernel time includes idle time.
    Some((n(kernel) + n(user), n(idle)))
}

#[cfg(not(any(target_os = "linux", windows)))]
fn cpu_times() -> Option<(u64, u64)> {
    None
}

/// CPU busy % between two counter readings.
pub fn cpu_pct(a: (u64, u64), b: (u64, u64)) -> Option<u32> {
    let total = b.0.checked_sub(a.0)?;
    let idle = b.1.checked_sub(a.1)?;
    (total > 0).then(|| (100 * total.saturating_sub(idle) / total) as u32)
}

/// Sample once a second into `slot` until `stop` is set.
pub fn start(slot: Arc<Mutex<Load>>, stop: Arc<AtomicBool>) {
    std::thread::spawn(move || {
        let mut prev = cpu_times();
        let mut has_gpu = true;
        while !stop.load(Ordering::Relaxed) {
            std::thread::sleep(Duration::from_millis(1000));
            let mut l = Load::default();
            if has_gpu {
                match nvidia() {
                    Some(p) => {
                        l.gpu_pct = Some(p[0] as u32);
                        l.vram_used_mb = Some(p[1]);
                        l.vram_total_mb = Some(p[2]);
                        l.gpu_temp_c = Some(p[3] as u32);
                    }
                    None => has_gpu = false, // no NVIDIA card: stop asking
                }
            }
            let now = cpu_times();
            if let (Some(a), Some(b)) = (prev, now) {
                l.cpu_pct = cpu_pct(a, b);
            }
            prev = now;
            if let Ok(mut g) = slot.lock() {
                *g = l;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    #[test]
    fn cpu_load_from_counter_deltas() {
        assert_eq!(super::cpu_pct((1000, 800), (2000, 1550)), Some(25));
        assert_eq!(super::cpu_pct((1000, 800), (1000, 800)), None, "no time passed");
        assert_eq!(super::cpu_pct((2000, 800), (1000, 800)), None, "counters went backwards");
    }
}
