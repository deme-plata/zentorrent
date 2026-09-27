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
    let exe = std::env::current_exe().map_err(|e| e.to_string())?;
    let staged = exe.with_file_name(format!(
        "zentorrent-{}-staged{}",
        m.version,
        if cfg!(windows) { ".exe" } else { "" }
    ));
    std::fs::write(&staged, &bytes).map_err(|e| format!("write {}: {e}", staged.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&staged, std::fs::Permissions::from_mode(0o755)).map_err(|e| e.to_string())?;
    }
    let res = self_replace::self_replace(&staged).map_err(|e| format!("could not replace the program file: {e}"));
    let _ = std::fs::remove_file(&staged);
    res
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

/// Start the freshly installed binary and leave.
pub fn relaunch() -> ! {
    if let Ok(exe) = std::env::current_exe() {
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
}
