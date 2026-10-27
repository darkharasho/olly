//! Runs scripts/get-olly.sh in dry-run mode against a fake release on disk.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

struct Fake {
    dir: PathBuf,
}

impl Fake {
    /// A fake releases tree for 1.2.3. `sha` overrides the manifest checksum.
    fn new(name: &str, sha: Option<&str>) -> Fake {
        let dir = std::env::temp_dir().join(format!("olly-get-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let pkg = dir.join("pkg");
        std::fs::create_dir_all(pkg.join("dist")).unwrap();
        std::fs::create_dir_all(pkg.join("plasmoid/package")).unwrap();
        for f in ["olly", "olly-notify", "dist/olly.service", "dist/olly-notify.service"] {
            std::fs::write(pkg.join(f), "x\n").unwrap();
        }
        std::fs::write(pkg.join("plasmoid/package/metadata.json"), "{}\n").unwrap();
        std::fs::write(pkg.join("VERSION"), "1.2.3\n").unwrap();

        let dl = dir.join("releases/download/v1.2.3");
        std::fs::create_dir_all(&dl).unwrap();
        let tarball = dl.join("olly-1.2.3-x86_64.tar.gz");
        let st = Command::new("tar")
            .arg("-C")
            .arg(&pkg)
            .arg("-czf")
            .arg(&tarball)
            .args(["olly", "olly-notify", "dist", "plasmoid", "VERSION"])
            .status()
            .unwrap();
        assert!(st.success());
        let real = String::from_utf8(Command::new("sha256sum").arg(&tarball).output().unwrap().stdout)
            .unwrap()[..64]
            .to_string();

        let latest = dir.join("releases/latest/download");
        std::fs::create_dir_all(&latest).unwrap();
        // The same layout jq writes in release-bundle.sh.
        std::fs::write(
            latest.join("manifest.json"),
            format!(
                "{{\n  \"version\": \"1.2.3\",\n  \"tarball\": \"olly-1.2.3-x86_64.tar.gz\",\n  \"sha256\": \"{}\",\n  \"notes\": [\n    \"New: a thing\"\n  ],\n  \"published\": \"2026-10-27T00:00:00Z\"\n}}\n",
                sha.unwrap_or(&real)
            ),
        )
        .unwrap();
        std::fs::create_dir_all(dir.join("raw/v1.2.3/dist")).unwrap();
        std::fs::write(dir.join("raw/v1.2.3/dist/config.toml"), "dry_run = true\n").unwrap();
        Fake { dir }
    }

    fn run(&self) -> Output {
        Command::new("bash")
            .arg(root().join("scripts/get-olly.sh"))
            .env("OLLY_RELEASES", url(&self.dir.join("releases")))
            .env("OLLY_RAW", url(&self.dir.join("raw")))
            .env("OLLY_DRY_RUN", "1")
            .output()
            .unwrap()
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn url(p: &Path) -> String {
    format!("file://{}", p.display())
}

fn text(o: &Output) -> String {
    format!("{}{}", String::from_utf8_lossy(&o.stdout), String::from_utf8_lossy(&o.stderr))
}

#[test]
fn installs_a_good_release() {
    let f = Fake::new("good", None);
    let o = f.run();
    let t = text(&o);
    assert!(o.status.success(), "{t}");
    assert!(t.contains("downloading olly 1.2.3"), "{t}");
    assert!(t.contains("/olly /usr/local/bin/olly\n"), "{t}");
    assert!(t.contains("+ sudo systemctl restart olly"), "{t}");
    assert!(t.contains("/usr/local/share/olly/plasmoid"), "{t}");
    assert!(t.contains("dry run: olly 1.2.3 checked out fine"), "{t}");
}

#[test]
fn refuses_a_checksum_mismatch() {
    let f = Fake::new("badsha", Some(&"0".repeat(64)));
    let o = f.run();
    let t = text(&o);
    assert!(!o.status.success(), "{t}");
    assert!(t.contains("didn't match its checksum"), "{t}");
    assert!(!t.contains("+ sudo"), "{t}");
}

#[test]
fn refuses_a_manifest_without_a_checksum() {
    let f = Fake::new("nosha", Some("nope"));
    let o = f.run();
    let t = text(&o);
    assert!(!o.status.success(), "{t}");
    assert!(t.contains("no checksum"), "{t}");
    assert!(!t.contains("+ sudo"), "{t}");
}

#[test]
fn says_so_when_offline() {
    let f = Fake::new("offline", None);
    std::fs::remove_dir_all(f.dir.join("releases/latest")).unwrap();
    let o = f.run();
    let t = text(&o);
    assert!(!o.status.success(), "{t}");
    assert!(t.contains("couldn't reach GitHub"), "{t}");
}
