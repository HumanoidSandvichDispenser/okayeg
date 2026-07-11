//! Black-box E2E tests

use std::fs;
use std::process::Command;

fn eg() -> Command {
    Command::new(env!("CARGO_BIN_EXE_eg"))
}

#[test]
fn snapshot_then_restore_round_trips_a_directory() {
    let work = tempfile::tempdir().unwrap();
    let src = work.path().join("src");
    let out = work.path().join("doc.eg");
    let dst = work.path().join("restored");

    // A small tree with nesting.
    fs::create_dir_all(src.join("sub")).unwrap();
    fs::write(src.join("README.md"), b"hello okayeg\n").unwrap();
    fs::write(src.join("sub").join("deep.txt"), b"nested\n").unwrap();

    let status = eg().arg("snapshot").arg(&src).arg(&out).status().unwrap();
    assert!(status.success(), "snapshot failed");
    assert!(out.exists(), "snapshot file not written");

    let status = eg().arg("restore").arg(&out).arg(&dst).status().unwrap();
    assert!(status.success(), "restore failed");

    assert_eq!(fs::read(dst.join("README.md")).unwrap(), b"hello okayeg\n");
    assert_eq!(
        fs::read(dst.join("sub").join("deep.txt")).unwrap(),
        b"nested\n"
    );
}

#[test]
fn no_args_is_a_usage_error() {
    let status = eg().status().unwrap();
    assert!(!status.success(), "expected nonzero exit with no args");
}
