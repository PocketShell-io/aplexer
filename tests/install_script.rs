//! The install script's contract: exactly one real binary lands in the bin
//! dir and `a` is always a symlink to it — including over a stale
//! regular-file `a` left behind by a copy-based install. That drift is not
//! hypothetical: concurrent copy-installs once materialized `a` and
//! `aplexer` as two regular files in `~/.local/bin` that then diverged.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

const SCRIPT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/scripts/install.sh");

fn run_install(bin_dir: &Path) {
    let status = Command::new("bash")
        .arg(SCRIPT)
        .arg("--bin")
        .arg(env!("CARGO_BIN_EXE_aplexer"))
        .arg(bin_dir)
        .status()
        .expect("run scripts/install.sh");
    assert!(status.success(), "install.sh failed: {status}");
}

fn assert_alias_is_symlink(bin_dir: &Path) {
    let alias = bin_dir.join("a");
    let link = std::fs::read_link(&alias).expect("`a` must be a symlink");
    assert_eq!(
        link,
        Path::new("aplexer"),
        "`a` must point at `aplexer` relatively, got {link:?}"
    );
}

#[test]
fn install_lands_one_binary_and_a_symlink_alias() {
    let dir = tempfile::tempdir().expect("bin dir tempdir");
    run_install(dir.path());

    let binary = dir.path().join("aplexer");
    let meta = std::fs::metadata(&binary).expect("installed binary");
    assert!(meta.is_file());
    assert!(meta.permissions().mode() & 0o111 != 0, "must be executable");

    assert_alias_is_symlink(dir.path());

    // The alias executes the binary, so both names behave identically.
    let through_alias = Command::new(dir.path().join("a"))
        .arg("--version")
        .output()
        .expect("run via the `a` symlink");
    assert!(
        through_alias.status.success(),
        "running through the alias failed: {}",
        String::from_utf8_lossy(&through_alias.stderr)
    );
}

#[test]
fn reinstall_replaces_a_stale_regular_file_alias_with_the_symlink() {
    let dir = tempfile::tempdir().expect("bin dir tempdir");
    let alias = dir.path().join("a");
    std::fs::write(&alias, b"stale copy-based install").expect("plant stale regular `a`");

    run_install(dir.path());

    assert_alias_is_symlink(dir.path());
}
