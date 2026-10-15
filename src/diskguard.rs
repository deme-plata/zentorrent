//! Downloads must not fill the disk. On 2026-10-10 Viktor's C: reached 0 bytes free while
//! two episodes downloaded into Downloads: Ollama quit (Flux MoE stopped answering), its
//! own update stopped at exactly 1 GiB, and the downloads could not go on either.
//!
//! So: below `LOW_GB` free on the drive ZenTorrent saves to, it pauses the downloads that
//! are running and says why; once `OK_GB` are free again it resumes exactly those (not the
//! ones the user paused). The gap between the two keeps it from flapping.

pub const LOW_GB: f64 = 2.0;
pub const OK_GB: f64 = 4.0;
/// After "Continue anyway": downloads run on until this little is left, and stop there,
/// so the drive never reaches 0 bytes (where Windows and Ollama broke on 2026-10-10).
pub const FLOOR_GB: f64 = 0.3;

#[derive(Debug, PartialEq, Eq)]
pub enum Step {
    /// Pause what is downloading.
    Pause,
    /// Resume what we paused.
    Resume,
    Nothing,
}

/// `free_gb`: free space where downloads go (None: unknown, do nothing);
/// `we_paused`: some downloads are paused by this guard right now;
/// `anyway`: the user pressed "Continue anyway" (pause only at [`FLOOR_GB`]).
pub fn step(free_gb: Option<f64>, we_paused: bool, anyway: bool) -> Step {
    let low = if anyway { FLOOR_GB } else { LOW_GB };
    match free_gb {
        Some(f) if f < low => Step::Pause,
        Some(f) if f >= OK_GB && we_paused => Step::Resume,
        _ => Step::Nothing,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pauses_when_nearly_full_and_resumes_with_room() {
        assert_eq!(step(Some(0.0), false, false), Step::Pause, "Viktor's C: on 2026-10-10");
        assert_eq!(step(Some(1.9), true, false), Step::Pause, "new downloads started meanwhile are paused too");
        assert_eq!(step(Some(3.0), true, false), Step::Nothing, "between the marks: stay paused, no flapping");
        assert_eq!(step(Some(4.0), true, false), Step::Resume);
        assert_eq!(step(Some(40.0), false, false), Step::Nothing, "plenty of room, nothing of ours paused");
        assert_eq!(step(None, true, false), Step::Nothing, "unknown: leave things as they are");
    }

    #[test]
    fn continue_anyway_runs_on_to_the_floor() {
        assert_eq!(step(Some(0.9), false, true), Step::Nothing, "Viktor 2026-10-15: 0.9 GB free, Continue anyway");
        assert_eq!(step(Some(0.29), false, true), Step::Pause, "never down to 0 bytes");
        assert_eq!(step(Some(4.0), true, true), Step::Resume);
    }
}
