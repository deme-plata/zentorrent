//! Signed self-update, the same shape as sigil-top's updater.
//!
//! The channel is ONE file, `zentorrent-latest.json`, plus a detached
//! Ed25519 signature `zentorrent-latest.json.sig` (128 hex) over its exact
//! bytes. The public key is compiled into this binary, so a manifest that
//! was not signed by the release key is rejected before any field is read.
//! The downloaded binary must then match the manifest's size and BLAKE3
//! hash before it replaces the running executable.

use serde::Deserialize;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const CHANNEL: &str = "https://quillon.xyz/downloads";
const MANIFEST: &str = "zentorrent-latest.json";

/// ZenTorrent release key (seed lives only on the release host).
const RELEASE_PUBKEY_HEX: &str = "e64325a53443b3e49292bd034e3d71e93b054a7c1ee1e222b58bbaf16c2b8e9a";

#[cfg(all(target_os = "linux", target_arch = "x86_64"))]
pub const TARGET: &str = "linux-x64";
#[cfg(all(windows, target_arch = "x86_64"))]
pub const TARGET: &str = "windows-x64";
#[cfg(not(any(all(target_os = "linux", target_arch = "x86_64"), all(windows, target_arch = "x86_64"))))]
pub const TARGET: &str = "unsupported";

#[derive(Clone, Debug, Deserialize)]
pub struct Manifest {
    pub product: String,
    pub version: String,
    #[serde(default)]
    pub notes: String,
    pub targets: std::collections::HashMap<String, Artifact>,
}

#[derive(Clone, Debug, Deserialize)]
pub struct Artifact {
    pub url: String,
    pub blake3_hex: String,
    pub size_bytes: u64,
}

fn client() -> reqwest::Client {
    reqwest::Client::builder()
        .user_agent(concat!("ZenTorrent/", env!("CARGO_PKG_VERSION"), " updater"))
        .timeout(std::time::Duration::from_secs(300))
        .build()
        .expect("http client")
}

/// Verify `sig_hex` over `body` against the pinned release key.
pub fn verify(body: &[u8], sig_hex: &str) -> Result<(), String> {
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};
    let pk: [u8; 32] = hex::decode(RELEASE_PUBKEY_HEX).unwrap().try_into().unwrap();
    let key = VerifyingKey::from_bytes(&pk).map_err(|e| e.to_string())?;
    let sig: [u8; 64] = hex::decode(sig_hex.trim())
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or("signature is not 64 bytes of hex")?;
    key.verify(body, &Signature::from_bytes(&sig))
        .map_err(|_| "MANIFEST SIGNATURE INVALID — update refused".to_string())
}

/// `a > b` for dotted numeric versions ("0.10.0" > "0.9.3").
pub fn newer(a: &str, b: &str) -> bool {
    let p = |v: &str| v.split('.').map(|x| x.parse::<u64>().unwrap_or(0)).collect::<Vec<_>>();
    p(a) > p(b)
}

/// Fetch + verify the channel. `Ok(Some)` only when a newer build for this
/// platform exists.
pub async fn check() -> Result<Option<Manifest>, String> {
    let c = client();
    let body = get(&c, MANIFEST.to_string()).await?;
    let sig = get(&c, format!("{MANIFEST}.sig")).await?;
    verify(&body, &String::from_utf8_lossy(&sig))?;
    let m: Manifest = serde_json::from_slice(&body).map_err(|e| format!("manifest: {e}"))?;
    if m.product != "zentorrent" {
        return Err(format!("manifest is for '{}', not zentorrent", m.product));
    }
    if !newer(&m.version, VERSION) || !m.targets.contains_key(TARGET) {
        return Ok(None);
    }
    Ok(Some(m))
}

async fn get(c: &reqwest::Client, name: String) -> Result<Vec<u8>, String> {
    let url = format!("{CHANNEL}/{name}");
    let r = c.get(&url).send().await.map_err(|e| format!("{name}: {e}"))?;
    if !r.status().is_success() {
        return Err(format!("{name}: HTTP {}", r.status()));
    }
    r.bytes().await.map(|b| b.to_vec()).map_err(|e| e.to_string())
}

/// Download, check size + BLAKE3, and replace the running executable.
pub async fn install(m: &Manifest) -> Result<(), String> {
    let a = m.targets.get(TARGET).ok_or("no build for this platform")?;
    let bytes = client()
        .get(&a.url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
        .map_err(|e| e.to_string())?
        .bytes()
        .await
        .map_err(|e| e.to_string())?;
    check_artifact(&bytes, a)?;
    let exe = program().ok_or("cannot tell where the program file is")?;
    tokio::task::spawn_blocking(move || replace_file(&exe, &bytes)).await.map_err(|e| e.to_string())?
}

/// How long to keep retrying a file operation. Antivirus scanners (Avast, Defender…)
/// open a freshly written .exe for several seconds and can refuse all other access
/// meanwhile; that is what broke 0.8.2 → 0.8.3 on Windows with Avast.
const AV_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

fn patiently<T>(what: &str, mut f: impl FnMut() -> std::io::Result<T>) -> Result<T, String> {
    let start = std::time::Instant::now();
    loop {
        match f() {
            Ok(v) => return Ok(v),
            Err(e) if e.kind() != std::io::ErrorKind::NotFound && start.elapsed() < AV_WAIT => {
                std::thread::sleep(std::time::Duration::from_millis(250));
            }
            Err(e) => return Err(format!("could not {what}: {e}")),
        }
    }
}

/// `.<program name>.<tag>`, next to the program.
fn beside(exe: &std::path::Path, tag: &str) -> std::path::PathBuf {
    let name = exe.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "zentorrent".into());
    exe.with_file_name(format!(".{name}.{tag}"))
}

/// Put `bytes` in place of the program at `exe` without ever leaving that path empty:
/// write the new version beside it and check it reads back intact, move the running
/// one aside (Windows allows renaming a running .exe, not deleting it), move the new
/// one in, and if that last step fails, move the old one back. The set-aside file is
/// removed on the next start (`clean_up`).
pub fn replace_file(exe: &std::path::Path, bytes: &[u8]) -> Result<(), String> {
    let new = beside(exe, "new");
    let _ = std::fs::remove_file(&new);
    std::fs::write(&new, bytes).map_err(|e| format!("could not write {}: {e}", new.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&new, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
    }
    let fail = |msg: String| {
        let _ = std::fs::remove_file(&new);
        Err(msg)
    };
    match patiently("read back the new version", || std::fs::read(&new)) {
        Ok(b) if b == bytes => {}
        Ok(_) => return fail("the new version changed on disk right after it was written (antivirus?); nothing was replaced".into()),
        Err(e) => return fail(format!("{e} — an antivirus may have blocked or removed it; nothing was replaced")),
    }
    // An earlier update's set-aside file may still be running (no restart since):
    // then it cannot be removed, so take the next free name.
    let Some(old) = (0..10).map(|i| beside(exe, &format!("old{i}"))).find(|p| {
        let _ = std::fs::remove_file(p);
        !p.exists()
    }) else {
        return fail("too many earlier versions are still running; restart ZenTorrent and try again".into());
    };
    if let Err(e) = patiently("move the running version aside", || std::fs::rename(exe, &old)) {
        return fail(format!("{e}; nothing was replaced"));
    }
    if let Err(e) = patiently("move the new version in", || std::fs::rename(&new, exe)) {
        return match patiently("put the old version back", || std::fs::rename(&old, exe)) {
            Ok(()) => fail(format!("{e}; the current version was kept")),
            Err(u) => fail(format!("{e}; {u} — your ZenTorrent program is now at {}", old.display())),
        };
    }
    Ok(())
}

/// Remove what earlier updates left next to the program: set-aside versions and, from
/// before 0.8.4, `zentorrent-<version>-staged` files. Called once at start.
pub fn clean_up() {
    let Some(exe) = program() else { return };
    let (Some(dir), Some(name)) = (exe.parent(), exe.file_name()) else { return };
    let mine = format!(".{}.", name.to_string_lossy());
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    for e in entries.flatten() {
        let n = e.file_name().to_string_lossy().into_owned();
        let set_aside = n.strip_prefix(&mine).is_some_and(|t| t == "new" || t.starts_with("old"));
        let staged = n.starts_with("zentorrent-") && n.trim_end_matches(".exe").ends_with("-staged");
        if set_aside || staged {
            let _ = std::fs::remove_file(e.path());
        }
    }
}

pub fn check_artifact(bytes: &[u8], a: &Artifact) -> Result<(), String> {
    if bytes.len() as u64 != a.size_bytes {
        return Err(format!("download is {} bytes, manifest says {}", bytes.len(), a.size_bytes));
    }
    let got = blake3::hash(bytes).to_hex().to_string();
    if !got.eq_ignore_ascii_case(a.blake3_hex.trim()) {
        return Err("download hash does not match the signed manifest — refused".into());
    }
    Ok(())
}

/// The program file as it was at start. Asked once: after an update has moved the
/// running file aside, `current_exe()` names the set-aside copy (Linux follows the
/// rename), and the restart must start the file at the original path.
pub fn program() -> Option<std::path::PathBuf> {
    static EXE: std::sync::OnceLock<Option<std::path::PathBuf>> = std::sync::OnceLock::new();
    EXE.get_or_init(|| std::env::current_exe().ok()).clone()
}

/// Start the freshly installed binary and leave.
pub fn relaunch() -> ! {
    if let Some(exe) = program() {
        let _ = std::process::Command::new(exe).args(std::env::args().skip(1)).spawn();
    }
    std::process::exit(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn versions_compare_numerically() {
        assert!(newer("0.10.0", "0.9.9"));
        assert!(newer("0.3.0", "0.2.1"));
        assert!(!newer("0.2.1", "0.2.1"));
        assert!(!newer("0.2.0", "0.2.1"));
    }

    #[test]
    fn bad_signature_is_refused() {
        let e = verify(b"{}", &"00".repeat(64)).unwrap_err();
        assert!(e.contains("INVALID"));
        assert!(verify(b"{}", "nothex").is_err());
    }

    #[test]
    fn artifact_hash_and_size_are_enforced() {
        let body = b"binary";
        let a = Artifact {
            url: String::new(),
            blake3_hex: blake3::hash(body).to_hex().to_string(),
            size_bytes: body.len() as u64,
        };
        assert!(check_artifact(body, &a).is_ok());
        assert!(check_artifact(b"binarY", &a).unwrap_err().contains("hash"));
        assert!(check_artifact(b"bin", &a).unwrap_err().contains("bytes"));
    }

    #[test]
    fn replace_swaps_the_file_and_leaves_the_old_one_beside_it() {
        let dir = std::env::temp_dir().join(format!("zt-replace-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("zentorrent.exe");
        std::fs::write(&exe, b"old version").unwrap();
        replace_file(&exe, b"new version").unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"new version");
        assert_eq!(std::fs::read(dir.join(".zentorrent.exe.old0")).unwrap(), b"old version");
        assert!(!dir.join(".zentorrent.exe.new").exists());
        // A second update while the first set-aside file is still there.
        replace_file(&exe, b"newer").unwrap();
        assert_eq!(std::fs::read(&exe).unwrap(), b"newer");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_failed_replace_keeps_the_program() {
        let dir = std::env::temp_dir().join(format!("zt-replace-fail-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        // The program path is missing: nothing may be left behind, and it says so.
        let exe = dir.join("zentorrent.exe");
        let e = replace_file(&exe, b"new").unwrap_err();
        assert!(e.contains("nothing was replaced"), "{e}");
        assert!(!dir.join(".zentorrent.exe.new").exists());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
