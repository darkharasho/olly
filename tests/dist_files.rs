//! Pins what the shipped unit and install scripts must contain.

use std::path::PathBuf;

fn read(rel: &str) -> String {
    std::fs::read_to_string(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(rel)).unwrap()
}

fn has_line(text: &str, line: &str) -> bool {
    text.lines().any(|l| l.trim() == line)
}

#[test]
fn unit_keeps_runtime_dir_and_protects_olly() {
    let unit = read("dist/olly.service");
    for line in [
        // A crash-restart must not wipe /run/olly/active.json, or frozen apps stay frozen.
        "RuntimeDirectoryPreserve=yes",
        // The socket lives here and is chmodded 0600 by olly; the dir stays traversable.
        "RuntimeDirectoryMode=0755",
        "UMask=0077",
        "ManagedOOMPreference=omit",
        "OOMScoreAdjust=-1000",
    ] {
        assert!(has_line(&unit, line), "missing {line}");
    }
}

#[test]
fn install_restarts_units_and_hints_only_in_dry_run() {
    let install = read("scripts/install.sh");
    for line in [
        "sudo systemctl enable olly",
        "sudo systemctl restart olly",
        "systemctl --user enable olly-notify",
        "systemctl --user restart olly-notify",
    ] {
        assert!(has_line(&install, line), "missing {line}");
    }
    assert!(!install.contains("enable --now"), "{install}");
    assert!(
        install.contains("grep -q '^dry_run = true' /etc/olly/config.toml"),
        "{install}"
    );
}

#[test]
fn uninstall_stops_before_deleting_a_running_unit() {
    let un = read("scripts/uninstall.sh");
    assert!(
        has_line(&un, "systemctl --user daemon-reload || true"),
        "{un}"
    );
    let disable = un
        .find("sudo systemctl disable --now olly")
        .expect("disables olly");
    let rm = un
        .find("sudo rm -f /etc/systemd/system/olly.service")
        .expect("removes the unit");
    assert!(disable < rm);
    assert!(
        !un.contains("sudo systemctl disable --now olly 2>/dev/null || true"),
        "a failed disable must not be ignored: {un}"
    );
}

#[test]
fn install_adds_the_tray_face_per_user_with_install_fallback() {
    let install = read("scripts/install.sh");
    let line = install
        .lines()
        .find(|l| l.contains("kpackagetool6 --type Plasma/Applet --upgrade plasmoid/package"))
        .unwrap_or_else(|| panic!("missing kpackagetool6 --upgrade: {install}"));
    assert!(
        !line.contains("sudo"),
        "tray install must be per-user: {line}"
    );
    assert!(
        line.contains("|| kpackagetool6 --type Plasma/Applet --install plasmoid/package"),
        "missing --install fallback: {line}"
    );
    assert!(
        has_line(
            &install,
            "if command -v kpackagetool6 >/dev/null 2>&1; then"
        ),
        "the tray install must be skipped without kpackagetool6: {install}"
    );
    assert!(!install.contains("plasmashell --replace"), "{install}");
    assert!(!install.contains("kquitapp"), "{install}");
}

#[test]
fn uninstall_removes_the_tray_face_without_failing() {
    let un = read("scripts/uninstall.sh");
    let line = un
        .lines()
        .find(|l| l.contains("kpackagetool6 --type Plasma/Applet --remove org.olly.tray"))
        .unwrap_or_else(|| panic!("missing kpackagetool6 --remove: {un}"));
    assert!(line.contains("2>/dev/null || true"), "{line}");
    assert!(!line.contains("sudo"), "{line}");
}
