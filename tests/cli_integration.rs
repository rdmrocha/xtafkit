//! CLI integration tests for the `xtafkit` binary.
//!
//! Tests run the xtafkit binary against mkimage-generated test images and
//! verify stdout, stderr, and exit codes. After the post-rename simplification
//! the CLI surface is small: `ls`, `scan`, `mkimage`, plus `--version`/`--help`.
//! Everything else (read/write/mkdir/rm/rename/copy/info/hexdump/cleanup) is
//! tested through fatxlib unit/integration tests and the TUI.

use std::path::PathBuf;
use std::process::Command;
use tempfile::TempDir;

/// Create xtafkit command
fn xtafkit_bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_xtafkit"))
}

/// Create a temp image via `xtafkit mkimage`. Returns (TempDir, image_path).
fn create_test_image(size_mb: u32, populate: bool) -> (TempDir, PathBuf) {
    let tmp = TempDir::new().expect("create temp dir");
    let img = tmp.path().join("test.img");

    let mut args = vec![
        "mkimage".to_string(),
        img.to_str().unwrap().to_string(),
        "--size".to_string(),
        format!("{}M", size_mb),
        "--force".to_string(),
    ];
    if populate {
        args.push("--populate".to_string());
    }

    let output = xtafkit_bin()
        .args(&args)
        .output()
        .expect("run xtafkit mkimage");
    assert!(
        output.status.success(),
        "xtafkit mkimage failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    (tmp, img)
}

// ===========================================================================
// Basic CLI tests
// ===========================================================================

#[test]
fn test_version() {
    let output = xtafkit_bin()
        .arg("--version")
        .output()
        .expect("run --version");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("xtafkit"),
        "version should contain 'xtafkit': {}",
        stdout
    );
}

#[test]
fn test_help() {
    let output = xtafkit_bin()
        .arg("--help")
        .output()
        .expect("run --help");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("ls"), "help should list ls command");
    assert!(stdout.contains("scan"), "help should list scan command");
    assert!(stdout.contains("mkimage"), "help should list mkimage command");
    assert!(stdout.contains("browse"), "help should list browse command");
}

// ===========================================================================
// xtafkit ls
// ===========================================================================

#[test]
fn test_ls_empty() {
    let (_tmp, img) = create_test_image(4, false);

    let output = xtafkit_bin()
        .args(["ls", img.to_str().unwrap(), "/"])
        .output()
        .expect("run ls");

    assert!(
        output.status.success(),
        "ls failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn test_ls_populated() {
    let (_tmp, img) = create_test_image(256, true);

    let output = xtafkit_bin()
        .args(["ls", img.to_str().unwrap(), "/"])
        .output()
        .expect("run ls");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Content"), "should list Content dir");
    assert!(stdout.contains("Cache"), "should list Cache dir");
    assert!(stdout.contains("name.txt"), "should list name.txt");
    assert!(stdout.contains("launch.ini"), "should list launch.ini");
}

#[test]
fn test_ls_subdirectory() {
    let (_tmp, img) = create_test_image(256, true);

    let output = xtafkit_bin()
        .args(["ls", img.to_str().unwrap(), "/Apps"])
        .output()
        .expect("run ls /Apps");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Aurora"), "should list Aurora under /Apps");
}

#[test]
fn test_ls_nonexistent_path() {
    let (_tmp, img) = create_test_image(4, false);

    let output = xtafkit_bin()
        .args(["ls", img.to_str().unwrap(), "/nonexistent"])
        .output()
        .expect("run ls nonexistent");

    assert!(!output.status.success(), "ls nonexistent path should fail");
}

#[test]
fn test_ls_json() {
    let (_tmp, img) = create_test_image(256, true);

    let output = xtafkit_bin()
        .args(["--json", "ls", img.to_str().unwrap(), "/"])
        .output()
        .expect("run ls --json");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.starts_with('[') || stdout.starts_with('{'),
        "JSON output should start with a bracket; got: {}",
        &stdout[..stdout.len().min(80)]
    );
    assert!(
        stdout.contains("\"name\""),
        "JSON should have a name field"
    );
}

// ===========================================================================
// xtafkit mkimage
// ===========================================================================

#[test]
fn test_mkimage_creates_valid_fatx() {
    let tmp = TempDir::new().unwrap();
    let img = tmp.path().join("new.img");

    let output = xtafkit_bin()
        .args(["mkimage", img.to_str().unwrap(), "--size", "4M"])
        .output()
        .expect("run mkimage");

    assert!(
        output.status.success(),
        "mkimage failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(img.exists(), "image file should be created");

    // Listing the empty image should succeed.
    let output = xtafkit_bin()
        .args(["ls", img.to_str().unwrap(), "/"])
        .output()
        .expect("run ls on new image");
    assert!(output.status.success(), "ls on fresh image should succeed");
}

#[test]
fn test_mkimage_xtaf_format() {
    let tmp = TempDir::new().unwrap();
    let img = tmp.path().join("xbox360.img");

    let output = xtafkit_bin()
        .args([
            "mkimage",
            img.to_str().unwrap(),
            "--size",
            "4M",
            "--format",
            "xtaf",
        ])
        .output()
        .expect("run mkimage xtaf");
    assert!(
        output.status.success(),
        "mkimage xtaf failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let output = xtafkit_bin()
        .args(["ls", img.to_str().unwrap(), "/"])
        .output()
        .expect("run ls on xtaf image");
    assert!(output.status.success(), "ls on xtaf image should succeed");
}

// ===========================================================================
// xtafkit scan
// ===========================================================================

#[test]
fn test_scan_nonexistent_device() {
    let output = xtafkit_bin()
        .args(["scan", "/nonexistent/device"])
        .output()
        .expect("run scan");

    assert!(!output.status.success());
}

#[test]
fn test_scan_image() {
    let (_tmp, img) = create_test_image(4, false);

    let output = xtafkit_bin()
        .args(["scan", img.to_str().unwrap()])
        .output()
        .expect("run scan on image");

    // Scan on a small image may or may not find partitions, but should not crash.
    let _ = output.status;
}
