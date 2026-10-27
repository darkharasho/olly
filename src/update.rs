//! Update core: signed manifest checks and tarball staging. Pure library code;
//! the daemon runs it on worker threads, never on the main loop.

use crate::engine::Level;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

pub const RELEASE_KEY: &str = include_str!("update_key.pub");

pub const MANIFEST_URL: &str =
    "https://github.com/darkharasho/olly/releases/latest/download/manifest.json";
const REFUSAL: &str = "I'll update once things are calm";

const SMALL_MAX_BYTES: u64 = 1024 * 1024;
const SMALL_TIMEOUT_SECS: u64 = 30;
const TARBALL_MAX_BYTES: u64 = 64 * 1024 * 1024;
const TARBALL_TIMEOUT_SECS: u64 = 300;

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct Manifest {
    pub version: String,
    pub tarball: String,
    pub sha256: String,
    pub notes: Vec<String>,
    pub published: String,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct UpdateInfo {
    pub version: String,
    pub notes: Vec<String>,
    pub url: String,
}

pub fn manifest_sig_url() -> String {
    format!("{MANIFEST_URL}.minisig")
}

pub fn release_page_url(version: &str) -> String {
    format!("https://github.com/darkharasho/olly/releases/tag/v{version}")
}

pub fn tarball_url(m: &Manifest) -> String {
    format!(
        "https://github.com/darkharasho/olly/releases/download/v{}/{}",
        m.version, m.tarball
    )
}

pub fn info(m: &Manifest) -> UpdateInfo {
    UpdateInfo {
        version: m.version.clone(),
        notes: m.notes.clone(),
        url: release_page_url(&m.version),
    }
}

/// Verify the minisign signature against the key file's second line, then
/// parse and sanity-check the manifest.
pub fn verify_manifest(manifest: &[u8], minisig: &str, pubkey_file: &str) -> Result<Manifest, String> {
    let key_line = pubkey_file
        .lines()
        .nth(1)
        .ok_or("public key file is malformed")?
        .trim();
    let key = minisign_verify::PublicKey::from_base64(key_line)
        .map_err(|e| format!("bad public key: {e}"))?;
    let sig = minisign_verify::Signature::decode(minisig)
        .map_err(|e| format!("bad signature: {e}"))?;
    key.verify(manifest, &sig, false)
        .map_err(|e| format!("signature check failed: {e}"))?;
    let m: Manifest =
        serde_json::from_slice(manifest).map_err(|e| format!("bad manifest: {e}"))?;
    semver::Version::parse(&m.version).map_err(|e| format!("bad version in manifest: {e}"))?;
    if m.tarball != format!("olly-{}-x86_64.tar.gz", m.version) {
        return Err("manifest names an unexpected tarball".into());
    }
    Ok(m)
}

pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (semver::Version::parse(candidate), semver::Version::parse(current)) {
        (Ok(c), Ok(cur)) => c > cur,
        _ => false,
    }
}

pub fn may_check(level: Level, incident_open: bool, paused: bool) -> bool {
    level == Level::Calm && !incident_open && !paused
}

pub fn may_update(
    level: Level,
    incident_open: bool,
    frozen: usize,
    throttled: usize,
) -> Result<(), String> {
    if level == Level::Calm && !incident_open && frozen == 0 && throttled == 0 {
        Ok(())
    } else {
        Err(REFUSAL.to_string())
    }
}

pub trait Fetch {
    fn get(&self, url: &str, max_bytes: u64, timeout_secs: u64) -> Result<Vec<u8>, String>;
    fn get_to_file(
        &self,
        url: &str,
        dest: &Path,
        max_bytes: u64,
        timeout_secs: u64,
    ) -> Result<(), String>;
}

pub struct Curl;

fn curl_cmd(url: &str, max_bytes: u64, timeout_secs: u64) -> Command {
    let mut c = Command::new("curl");
    c.args(["-fsSL", "--proto", "=https", "--proto-redir", "=https"])
        .arg("--max-time")
        .arg(timeout_secs.to_string())
        .arg("--max-filesize")
        .arg(max_bytes.to_string());
    c.arg(url);
    c
}

impl Fetch for Curl {
    fn get(&self, url: &str, max_bytes: u64, timeout_secs: u64) -> Result<Vec<u8>, String> {
        let out = curl_cmd(url, max_bytes, timeout_secs)
            .output()
            .map_err(|e| format!("couldn't run curl: {e}"))?;
        if !out.status.success() {
            return Err(format!("download failed ({})", out.status));
        }
        if out.stdout.len() as u64 > max_bytes {
            return Err("download too large".into());
        }
        Ok(out.stdout)
    }

    fn get_to_file(
        &self,
        url: &str,
        dest: &Path,
        max_bytes: u64,
        timeout_secs: u64,
    ) -> Result<(), String> {
        let status = curl_cmd(url, max_bytes, timeout_secs)
            .arg("-o")
            .arg(dest)
            .status()
            .map_err(|e| format!("couldn't run curl: {e}"))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("download failed ({status})"))
        }
    }
}

fn fetch_manifest(fetch: &dyn Fetch, pubkey: &str) -> Result<Manifest, String> {
    let body = fetch.get(MANIFEST_URL, SMALL_MAX_BYTES, SMALL_TIMEOUT_SECS)?;
    let sig = fetch.get(&manifest_sig_url(), SMALL_MAX_BYTES, SMALL_TIMEOUT_SECS)?;
    let sig = String::from_utf8(sig).map_err(|_| "signature isn't text".to_string())?;
    verify_manifest(&body, &sig, pubkey)
}

pub fn check(fetch: &dyn Fetch, pubkey: &str, current: &str) -> Result<Option<Manifest>, String> {
    let m = fetch_manifest(fetch, pubkey)?;
    Ok(is_newer(&m.version, current).then_some(m))
}

#[derive(Debug)]
pub struct Staged {
    pub dir: PathBuf,
    pub manifest: Manifest,
}

pub fn stage(
    fetch: &dyn Fetch,
    pubkey: &str,
    current: &str,
    work: &Path,
) -> Result<Staged, String> {
    let m = fetch_manifest(fetch, pubkey)?;
    if !is_newer(&m.version, current) {
        return Err("that version isn't newer than this one".into());
    }
    let base = work.join(&m.version);
    let _ = fs::remove_dir_all(&base);
    match stage_into(fetch, &m, &base) {
        Ok(dir) => Ok(Staged { dir, manifest: m }),
        Err(e) => {
            let _ = fs::remove_dir_all(&base);
            Err(e)
        }
    }
}

fn stage_into(fetch: &dyn Fetch, m: &Manifest, base: &Path) -> Result<PathBuf, String> {
    let root = base.join("root");
    fs::create_dir_all(&root).map_err(|e| format!("couldn't make work dir: {e}"))?;
    let bundle = base.join("bundle.tar.gz");
    fetch.get_to_file(&tarball_url(m), &bundle, TARBALL_MAX_BYTES, TARBALL_TIMEOUT_SECS)?;

    let bytes = fs::read(&bundle).map_err(|e| format!("couldn't read download: {e}"))?;
    let sum: String = Sha256::digest(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    if sum != m.sha256 {
        return Err("download doesn't match its checksum".into());
    }

    let status = Command::new("tar")
        .arg("-xzf")
        .arg(&bundle)
        .arg("--no-same-owner")
        .arg("-C")
        .arg(&root)
        .status()
        .map_err(|e| format!("couldn't run tar: {e}"))?;
    if !status.success() {
        return Err("couldn't unpack the download".into());
    }

    let version = fs::read_to_string(root.join("VERSION")).map_err(|_| "bundle has no VERSION")?;
    if version.trim() != m.version {
        return Err("bundle version doesn't match the manifest".into());
    }
    for f in ["olly", "olly-notify"] {
        if !root.join(f).is_file() {
            return Err(format!("bundle is missing {f}"));
        }
    }
    Ok(root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const FIX: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/update");
    const MINISIGN: &str = "/home/linuxbrew/.linuxbrew/bin/minisign";

    fn fx(name: &str) -> Vec<u8> {
        fs::read(format!("{FIX}/{name}")).unwrap()
    }
    fn fxs(name: &str) -> String {
        String::from_utf8(fx(name)).unwrap()
    }

    #[test]
    fn verifies_good_signature() {
        let m = verify_manifest(&fx("good-manifest.json"), &fxs("good-manifest.json.minisig"), &fxs("test.pub"))
            .unwrap();
        assert_eq!(m.version, "9.9.9");
    }

    #[test]
    fn rejects_tampered_manifest() {
        let mut body = fx("good-manifest.json");
        body[12] ^= 1;
        assert!(verify_manifest(&body, &fxs("good-manifest.json.minisig"), &fxs("test.pub")).is_err());
    }

    #[test]
    fn rejects_wrong_key() {
        assert!(verify_manifest(
            &fx("good-manifest.json"),
            &fxs("good-manifest.json.other.minisig"),
            &fxs("test.pub")
        )
        .is_err());
    }

    #[test]
    fn rejects_tarball_name_with_path() {
        let e = verify_manifest(&fx("bad-name-manifest.json"), &fxs("bad-name-manifest.json.minisig"), &fxs("test.pub"))
            .unwrap_err();
        assert!(e.contains("tarball"), "{e}");
    }

    #[test]
    fn release_key_parses() {
        let key = RELEASE_KEY.lines().nth(1).unwrap().trim();
        assert!(minisign_verify::PublicKey::from_base64(key).is_ok());
    }

    #[test]
    fn newer_versions() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
        assert!(is_newer("0.10.0", "0.9.0"));
        assert!(!is_newer("junk", "0.1.0"));
    }

    #[test]
    fn check_gate() {
        assert!(may_check(Level::Calm, false, false));
        assert!(!may_check(Level::Watch, false, false));
        assert!(!may_check(Level::Calm, true, false));
        assert!(!may_check(Level::Calm, false, true));
    }

    #[test]
    fn update_gate() {
        assert_eq!(may_update(Level::Calm, false, 0, 0), Ok(()));
        let want = Err("I'll update once things are calm".to_string());
        assert_eq!(may_update(Level::Watch, false, 0, 0), want);
        assert_eq!(may_update(Level::Calm, true, 0, 0), want);
        assert_eq!(may_update(Level::Calm, false, 1, 0), want);
        assert_eq!(may_update(Level::Calm, false, 0, 1), want);
    }

    #[test]
    fn info_points_at_release_page() {
        let m = verify_manifest(&fx("good-manifest.json"), &fxs("good-manifest.json.minisig"), &fxs("test.pub"))
            .unwrap();
        let i = info(&m);
        assert_eq!(i.url, "https://github.com/darkharasho/olly/releases/tag/v9.9.9");
        assert_eq!(i.notes, m.notes);
    }

    struct FakeFetch(HashMap<String, Vec<u8>>);
    impl Fetch for FakeFetch {
        fn get(&self, url: &str, _: u64, _: u64) -> Result<Vec<u8>, String> {
            self.0.get(url).cloned().ok_or_else(|| format!("no {url}"))
        }
        fn get_to_file(&self, url: &str, dest: &Path, m: u64, t: u64) -> Result<(), String> {
            fs::write(dest, self.get(url, m, t)?).map_err(|e| e.to_string())
        }
    }

    /// Builds a signed fake release; None when minisign isn't installed.
    fn release(
        tmp: &Path,
        version: &str,
        file_version: &str,
        sha_override: Option<&str>,
    ) -> Option<(FakeFetch, String)> {
        if !Path::new(MINISIGN).exists() {
            eprintln!("skipping: no minisign at {MINISIGN}");
            return None;
        }
        let src = tmp.join("src");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("olly"), "bin").unwrap();
        fs::write(src.join("olly-notify"), "bin").unwrap();
        fs::write(src.join("VERSION"), format!("{file_version}\n")).unwrap();
        let tarball = format!("olly-{version}-x86_64.tar.gz");
        let tgz = tmp.join(&tarball);
        assert!(Command::new("tar")
            .arg("-czf").arg(&tgz).arg("-C").arg(&src)
            .args(["olly", "olly-notify", "VERSION"])
            .status().unwrap().success());
        let bytes = fs::read(&tgz).unwrap();
        let sha: String = Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect();
        let m = Manifest {
            version: version.into(),
            tarball: tarball.clone(),
            sha256: sha_override.map(String::from).unwrap_or(sha),
            notes: vec!["n".into()],
            published: "2026-01-01T00:00:00Z".into(),
        };
        let mpath = tmp.join("manifest.json");
        fs::write(&mpath, serde_json::to_vec(&m).unwrap()).unwrap();
        let spath = tmp.join("manifest.json.minisig");
        assert!(Command::new(MINISIGN)
            .args(["-S", "-s"]).arg(format!("{FIX}/test.key"))
            .arg("-m").arg(&mpath).arg("-x").arg(&spath)
            .output().unwrap().status.success());
        let mut map = HashMap::new();
        map.insert(MANIFEST_URL.to_string(), fs::read(&mpath).unwrap());
        map.insert(manifest_sig_url(), fs::read(&spath).unwrap());
        map.insert(tarball_url(&m), bytes);
        Some((FakeFetch(map), fxs("test.pub")))
    }

    #[test]
    fn stage_good_bundle() {
        let tmp = tempfile::tempdir().unwrap();
        let Some((f, key)) = release(tmp.path(), "9.9.9", "9.9.9", None) else { return };
        let work = tmp.path().join("work");
        let s = stage(&f, &key, "0.1.0", &work).unwrap();
        assert_eq!(s.manifest.version, "9.9.9");
        assert!(s.dir.join("olly").exists());
        assert_eq!(s.dir, work.join("9.9.9/root"));
    }

    #[test]
    fn stage_sha_mismatch_cleans_up() {
        let tmp = tempfile::tempdir().unwrap();
        let Some((f, key)) = release(tmp.path(), "9.9.9", "9.9.9", Some(&"0".repeat(64))) else { return };
        let work = tmp.path().join("work");
        assert!(stage(&f, &key, "0.1.0", &work).is_err());
        assert!(!work.join("9.9.9").exists());
    }

    #[test]
    fn stage_version_file_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let Some((f, key)) = release(tmp.path(), "9.9.9", "9.9.8", None) else { return };
        let work = tmp.path().join("work");
        assert!(stage(&f, &key, "0.1.0", &work).is_err());
        assert!(!work.join("9.9.9").exists());
    }

    #[test]
    fn stage_requires_newer() {
        let tmp = tempfile::tempdir().unwrap();
        let Some((f, key)) = release(tmp.path(), "9.9.9", "9.9.9", None) else { return };
        let work = tmp.path().join("work");
        assert!(stage(&f, &key, "9.9.9", &work).is_err());
        assert!(!work.join("9.9.9").exists());
    }

    #[test]
    fn check_reports_only_newer() {
        let tmp = tempfile::tempdir().unwrap();
        let Some((f, key)) = release(tmp.path(), "9.9.9", "9.9.9", None) else { return };
        assert!(check(&f, &key, "0.1.0").unwrap().is_some());
        assert!(check(&f, &key, "9.9.9").unwrap().is_none());
    }
}
