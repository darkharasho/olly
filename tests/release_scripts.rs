//! Exercises the release scripts against throwaway git repos.

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

fn root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

struct Repo {
    dir: PathBuf,
}

impl Repo {
    fn new(name: &str, cargo_version: &str) -> Repo {
        let dir = std::env::temp_dir().join(format!("olly-rel-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("scripts")).unwrap();
        std::fs::create_dir_all(dir.join("plasmoid/package")).unwrap();
        std::fs::create_dir_all(dir.join("dist")).unwrap();
        for s in ["release-check.sh", "release-notes.sh", "release-bundle.sh"] {
            let _ = std::fs::copy(root().join("scripts").join(s), dir.join("scripts").join(s));
        }
        let repo = Repo { dir };
        repo.git(&["init", "-q"]);
        repo.git(&["config", "user.name", "t"]);
        repo.git(&["config", "user.email", "t@example.com"]);
        repo.git(&["config", "commit.gpgsign", "false"]);
        repo.git(&["config", "tag.gpgsign", "false"]);
        repo.write(
            "Cargo.toml",
            &format!("[package]\nname = \"olly\"\nversion = \"{cargo_version}\"\n"),
        );
        repo.write("plasmoid/package/metadata.json", &meta("0.1.0"));
        repo.write("dist/olly.service", "[Service]\n");
        repo.write("dist/olly-notify.service", "[Service]\n");
        repo
    }

    fn write(&self, rel: &str, text: &str) {
        let p = self.dir.join(rel);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, text).unwrap();
    }

    fn git(&self, args: &[&str]) -> String {
        let out = Command::new("git").args(args).current_dir(&self.dir).output().unwrap();
        assert!(out.status.success(), "git {args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn commit(&self, msg: &str) {
        // Each commit needs a change; give it its own file so merges never conflict.
        let name: String = msg.chars().map(|c| if c.is_alphanumeric() { c } else { '_' }).collect();
        self.write(&format!("history/{name}.txt"), &format!("{msg}\n"));
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", msg]);
    }

    fn run(&self, script: &str, args: &[&str]) -> Output {
        Command::new("bash")
            .arg(self.dir.join("scripts").join(script))
            .args(args)
            .current_dir(&self.dir)
            .output()
            .unwrap()
    }
}

impl Drop for Repo {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

fn meta(version: &str) -> String {
    format!("{{\n    \"KPlugin\": {{\n        \"Version\": \"{version}\"\n    }}\n}}\n")
}

fn stderr(o: &Output) -> String {
    String::from_utf8_lossy(&o.stderr).into_owned()
}

fn stdout(o: &Output) -> String {
    String::from_utf8_lossy(&o.stdout).into_owned()
}

#[test]
fn check_fails_on_tag_version_mismatch() {
    let r = Repo::new("mismatch", "0.2.0");
    r.commit("feat: x");
    let o = r.run("release-check.sh", &["v0.3.0"]);
    assert!(!o.status.success());
    assert!(stderr(&o).contains("0.2.0"), "{}", stderr(&o));
}

#[test]
fn check_passes_on_match() {
    let r = Repo::new("match", "0.2.0");
    r.commit("feat: x");
    let o = r.run("release-check.sh", &["v0.2.0"]);
    assert!(o.status.success(), "{}", stderr(&o));
}

#[test]
fn check_refuses_tags_that_are_not_plain_versions() {
    let r = Repo::new("prerelease", "0.2.0-rc1");
    r.commit("feat: x");
    for tag in ["v0.2.0-rc1", "0.2.0-rc1", "v0.2", "v0.2.0.1"] {
        let o = r.run("release-check.sh", &[tag]);
        assert!(!o.status.success(), "{tag}");
        assert!(stderr(&o).contains("vX.Y.Z"), "{tag}: {}", stderr(&o));
    }
}

#[test]
fn check_fails_when_widget_changed_without_bump() {
    let r = Repo::new("widget", "0.2.0");
    r.commit("feat: first");
    r.git(&["tag", "v0.1.0"]);
    r.write("plasmoid/package/ui.qml", "Item {}\n");
    r.commit("feat(tray): new face");
    let o = r.run("release-check.sh", &["v0.2.0"]);
    assert!(!o.status.success());
    assert!(stderr(&o).contains("plasmoid"), "{}", stderr(&o));

    r.write("plasmoid/package/metadata.json", &meta("0.2.0"));
    r.commit("chore: bump widget");
    let o = r.run("release-check.sh", &["v0.2.0"]);
    assert!(o.status.success(), "{}", stderr(&o));
}

#[test]
fn notes_keep_feat_and_fix_only() {
    let r = Repo::new("notes", "0.2.0");
    r.commit("feat(tray): mood face");
    r.commit("fix: socket timeout");
    r.commit("docs: readme");
    r.commit("chore: bump");
    r.git(&["checkout", "-q", "-b", "side"]);
    r.commit("feat: on a side branch");
    r.git(&["checkout", "-q", "-"]);
    r.commit("fix: main again");
    r.git(&["merge", "--no-ff", "-q", "-m", "feat: merge side", "side"]);
    let o = r.run("release-notes.sh", &["", "v0.2.0"]);
    assert!(o.status.success(), "{}", stderr(&o));
    let out = stdout(&o);
    let lines: Vec<&str> = out.lines().collect();
    assert!(lines.contains(&"New: mood face"), "{out}");
    assert!(lines.contains(&"Fixed: socket timeout"), "{out}");
    assert!(lines.contains(&"Fixed: main again"), "{out}");
    assert!(lines.contains(&"New: on a side branch"), "{out}");
    assert!(!out.contains("readme") && !out.contains("bump") && !out.contains("merge"), "{out}");
    assert_eq!(lines.len(), 4, "{out}");
}

#[test]
fn notes_since_previous_tag_only() {
    let r = Repo::new("since", "0.2.0");
    r.commit("feat: old thing");
    r.git(&["tag", "v0.1.0"]);
    r.commit("fix: new thing");
    let o = r.run("release-notes.sh", &["v0.1.0", "v0.2.0"]);
    assert!(o.status.success(), "{}", stderr(&o));
    assert_eq!(stdout(&o), "Fixed: new thing\n");
}

#[test]
fn bundle_writes_manifest_and_tarball() {
    let r = Repo::new("bundle", "0.2.0");
    r.commit("feat: shiny");
    let bin = r.dir.join("bin");
    let out = r.dir.join("out");
    std::fs::create_dir_all(&bin).unwrap();
    std::fs::write(bin.join("olly"), "fake olly").unwrap();
    std::fs::write(bin.join("olly-notify"), "fake notify").unwrap();
    let o = r.run(
        "release-bundle.sh",
        &["0.2.0", bin.to_str().unwrap(), out.to_str().unwrap()],
    );
    assert!(o.status.success(), "{}", stderr(&o));

    let tarball = out.join("olly-0.2.0-x86_64.tar.gz");
    assert!(tarball.exists());
    let manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(out.join("manifest.json")).unwrap()).unwrap();
    assert_eq!(manifest["version"], "0.2.0");
    assert_eq!(manifest["tarball"], "olly-0.2.0-x86_64.tar.gz");
    assert!(manifest["notes"].is_array());
    assert!(manifest["published"].as_str().unwrap().ends_with('Z'));
    let sum = sha256(&tarball);
    assert_eq!(manifest["sha256"], sum.as_str());

    let list = Command::new("tar").arg("-tzf").arg(&tarball).output().unwrap();
    let list = stdout(&list);
    for want in ["VERSION", "olly", "olly-notify", "dist/olly.service", "plasmoid/package/metadata.json"] {
        assert!(list.lines().any(|l| l == want), "missing {want} in {list}");
    }
    let v = Command::new("tar").arg("-xzOf").arg(&tarball).arg("VERSION").output().unwrap();
    assert_eq!(stdout(&v), "0.2.0\n");
}

fn sha256(p: &Path) -> String {
    let o = Command::new("sha256sum").arg(p).output().unwrap();
    stdout(&o).split_whitespace().next().unwrap().to_string()
}
