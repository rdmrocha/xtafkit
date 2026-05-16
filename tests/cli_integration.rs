//! CLI integration tests for the `xtafkit` binary.
//!
//! Tests run the xtafkit binary against mkimage-generated test images and verify
//! stdout, stderr, and exit codes.

use std::path::PathBuf;
use std::process::Command;
use tempfile::TempDir;

/// Create xtafkit command
fn fatx_bin() -> Command {
    Command::new(env!("CARGO_BIN_EXE_xtafkit"))
}

/// Create a temp image via `fatx mkimage`. Returns (TempDir, image_path).
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

    let output = fatx_bin().args(&args).output().expect("run fatx mkimage");
    assert!(
        output.status.success(),
        "fatx mkimage failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    (tmp, img)
}

// ===========================================================================
// Basic CLI tests
// ===========================================================================

#[test]
fn test_fatx_version() {
    let output = fatx_bin()
        .arg("--version")
        .output()
        .expect("run fatx --version");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("xtafkit"),
        "version should contain 'xtafkit': {}",
        stdout
    );
}

#[test]
fn test_fatx_help() {
    let output = fatx_bin().arg("--help").output().expect("run fatx --help");
    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("FATX"), "help should mention FATX");
    assert!(stdout.contains("ls"), "help should list ls command");
    assert!(stdout.contains("info"), "help should list info command");
}

// ===========================================================================
// fatx ls
// ===========================================================================

#[test]
fn test_cli_ls_empty() {
    let (_tmp, img) = create_test_image(4, false);

    let output = fatx_bin()
        .args(["ls", img.to_str().unwrap(), "/"])
        .output()
        .expect("run fatx ls");

    assert!(
        output.status.success(),
        "fatx ls failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn test_cli_ls_populated() {
    let (_tmp, img) = create_test_image(256, true);

    let output = fatx_bin()
        .args(["ls", img.to_str().unwrap(), "/"])
        .output()
        .expect("run fatx ls");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Content"), "should list Content dir");
    assert!(stdout.contains("Cache"), "should list Cache dir");
    assert!(stdout.contains("name.txt"), "should list name.txt");
    assert!(stdout.contains("launch.ini"), "should list launch.ini");
}

#[test]
fn test_cli_ls_subdirectory() {
    let (_tmp, img) = create_test_image(256, true);

    let output = fatx_bin()
        .args(["ls", img.to_str().unwrap(), "/Apps"])
        .output()
        .expect("run fatx ls /Apps");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("Aurora"), "should list Aurora under /Apps");
}

#[test]
fn test_cli_ls_nonexistent_path() {
    let (_tmp, img) = create_test_image(4, false);

    let output = fatx_bin()
        .args(["ls", img.to_str().unwrap(), "/nonexistent"])
        .output()
        .expect("run fatx ls nonexistent");

    assert!(!output.status.success(), "ls nonexistent path should fail");
}

// ===========================================================================
// fatx info
// ===========================================================================

#[test]
fn test_cli_info() {
    let (_tmp, img) = create_test_image(4, false);

    let output = fatx_bin()
        .args(["info", img.to_str().unwrap()])
        .output()
        .expect("run fatx info");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("FATX Volume Information"),
        "should show header"
    );
    assert!(stdout.contains("FAT type:"), "should show FAT type");
    assert!(stdout.contains("Free:"), "should show free space");
}

#[test]
fn test_cli_info_json() {
    let (_tmp, img) = create_test_image(4, false);

    let output = fatx_bin()
        .args(["info", img.to_str().unwrap(), "--json"])
        .output()
        .expect("run fatx info --json");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);

    let json: serde_json::Value =
        serde_json::from_str(&stdout).expect("info --json should produce valid JSON");

    assert!(
        json["total_clusters"].is_number(),
        "should have total_clusters"
    );
    assert!(
        json["free_clusters"].is_number(),
        "should have free_clusters"
    );
    assert!(json["cluster_size"].is_number(), "should have cluster_size");
}

#[test]
fn test_cli_info_nonexistent_file() {
    let output = fatx_bin()
        .args(["info", "/tmp/does_not_exist_fatx_test.img"])
        .output()
        .expect("run fatx info nonexistent");

    assert!(
        !output.status.success(),
        "info on nonexistent file should fail"
    );
}

// ===========================================================================
// fatx read
// ===========================================================================

#[test]
fn test_cli_read_file() {
    let (_tmp, img) = create_test_image(256, true);

    let output = fatx_bin()
        .args(["read", img.to_str().unwrap(), "/name.txt"])
        .output()
        .expect("run fatx read");

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains("Test Xbox 360"),
        "should read file content from populated image"
    );
}

// ===========================================================================
// fatx write
// ===========================================================================

#[test]
fn test_cli_write_and_read_roundtrip() {
    let (_tmp, img) = create_test_image(4, false);

    // Write a local file into the image
    let input_dir = TempDir::new().unwrap();
    let input_file = input_dir.path().join("hello.txt");
    std::fs::write(&input_file, b"Hello from CLI write test!").unwrap();

    let output = fatx_bin()
        .args([
            "write",
            img.to_str().unwrap(),
            "/hello.txt",
            "--input",
            input_file.to_str().unwrap(),
        ])
        .output()
        .expect("run fatx write");

    assert!(
        output.status.success(),
        "write failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Read it back
    let output = fatx_bin()
        .args(["read", img.to_str().unwrap(), "/hello.txt"])
        .output()
        .expect("run fatx read");

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "Hello from CLI write test!"
    );
}

#[test]
fn test_cli_write_existing_file_fails() {
    let (_tmp, img) = create_test_image(4, false);

    let input_dir = TempDir::new().unwrap();
    let input_file = input_dir.path().join("hello.txt");
    std::fs::write(&input_file, b"first").unwrap();

    let first = fatx_bin()
        .args([
            "write",
            img.to_str().unwrap(),
            "/hello.txt",
            "--input",
            input_file.to_str().unwrap(),
        ])
        .output()
        .expect("run first fatx write");
    assert!(first.status.success());

    std::fs::write(&input_file, b"second").unwrap();
    let second = fatx_bin()
        .args([
            "write",
            img.to_str().unwrap(),
            "/hello.txt",
            "--input",
            input_file.to_str().unwrap(),
        ])
        .output()
        .expect("run second fatx write");

    assert!(
        !second.status.success(),
        "second write should fail when the destination already exists"
    );
    let stderr = String::from_utf8_lossy(&second.stderr);
    assert!(
        stderr.contains("already exists"),
        "existing-file failure should be surfaced to the user, got: {}",
        stderr
    );
}

#[test]
fn test_cli_write_large_file() {
    let (_tmp, img) = create_test_image(16, false);

    let input_dir = TempDir::new().unwrap();
    let input_file = input_dir.path().join("large.bin");
    let data: Vec<u8> = (0..65536u32).map(|i| (i % 256) as u8).collect();
    std::fs::write(&input_file, &data).unwrap();

    let output = fatx_bin()
        .args([
            "write",
            img.to_str().unwrap(),
            "/large.bin",
            "--input",
            input_file.to_str().unwrap(),
        ])
        .output()
        .expect("run fatx write large");

    assert!(
        output.status.success(),
        "write large failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify it's listed
    let output = fatx_bin()
        .args(["ls", img.to_str().unwrap(), "/"])
        .output()
        .expect("run fatx ls");

    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("large.bin"));
}

// ===========================================================================
// fatx mkdir
// ===========================================================================

#[test]
fn test_cli_mkdir() {
    let (_tmp, img) = create_test_image(4, false);

    let output = fatx_bin()
        .args(["mkdir", img.to_str().unwrap(), "/TestDir"])
        .output()
        .expect("run fatx mkdir");

    assert!(
        output.status.success(),
        "mkdir failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify it's listed
    let output = fatx_bin()
        .args(["ls", img.to_str().unwrap(), "/"])
        .output()
        .expect("run fatx ls");

    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("TestDir"));
}

#[test]
fn test_cli_mkdir_nested() {
    let (_tmp, img) = create_test_image(4, false);

    fatx_bin()
        .args(["mkdir", img.to_str().unwrap(), "/Parent"])
        .output()
        .expect("mkdir parent")
        .status
        .success()
        .then_some(())
        .expect("mkdir parent failed");

    let output = fatx_bin()
        .args(["mkdir", img.to_str().unwrap(), "/Parent/Child"])
        .output()
        .expect("run fatx mkdir nested");

    assert!(
        output.status.success(),
        "nested mkdir failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let output = fatx_bin()
        .args(["ls", img.to_str().unwrap(), "/Parent"])
        .output()
        .expect("ls parent");

    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("Child"));
}

// ===========================================================================
// Files and directories with spaces in names
// ===========================================================================

#[test]
fn test_cli_write_file_with_spaces() {
    let (_tmp, img) = create_test_image(4, false);

    let input_dir = TempDir::new().unwrap();
    let input_file = input_dir.path().join("my game save.dat");
    std::fs::write(&input_file, b"save data with spaces").unwrap();

    let output = fatx_bin()
        .args([
            "write",
            img.to_str().unwrap(),
            "/my game save.dat",
            "--input",
            input_file.to_str().unwrap(),
        ])
        .output()
        .expect("run fatx write with spaces");

    assert!(
        output.status.success(),
        "write with spaces failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let output = fatx_bin()
        .args(["read", img.to_str().unwrap(), "/my game save.dat"])
        .output()
        .expect("read file with spaces");

    assert!(output.status.success());
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        "save data with spaces"
    );
}

#[test]
fn test_cli_mkdir_with_spaces() {
    let (_tmp, img) = create_test_image(4, false);

    let output = fatx_bin()
        .args(["mkdir", img.to_str().unwrap(), "/My Game Folder"])
        .output()
        .expect("run fatx mkdir with spaces");

    assert!(
        output.status.success(),
        "mkdir with spaces failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let output = fatx_bin()
        .args(["ls", img.to_str().unwrap(), "/"])
        .output()
        .unwrap();

    assert!(String::from_utf8_lossy(&output.stdout).contains("My Game Folder"));
}

#[test]
fn test_cli_copy_dir_with_spaces() {
    let (_tmp, img) = create_test_image(16, false);

    // Create a local directory with spaces in its name
    let input_dir = TempDir::new().unwrap();
    let src = input_dir.path().join("Call of Duty");
    std::fs::create_dir_all(src.join("sub folder")).unwrap();
    std::fs::write(src.join("readme.txt"), b"game data").unwrap();
    std::fs::write(src.join("sub folder").join("level 1.dat"), b"level data").unwrap();

    let output = fatx_bin()
        .args([
            "copy",
            img.to_str().unwrap(),
            "--from",
            src.to_str().unwrap(),
            "--to",
            "/Call of Duty",
        ])
        .output()
        .expect("run fatx copy with spaces");

    assert!(
        output.status.success(),
        "copy with spaces failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify the directory structure
    let output = fatx_bin()
        .args(["ls", img.to_str().unwrap(), "/Call of Duty"])
        .output()
        .unwrap();

    assert!(output.status.success());
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("readme.txt"), "should contain readme.txt");
    assert!(stdout.contains("sub folder"), "should contain sub folder");
}

#[test]
fn test_cli_rm_file_with_spaces() {
    let (_tmp, img) = create_test_image(4, false);

    // Create file with spaces
    let input_dir = TempDir::new().unwrap();
    let input_file = input_dir.path().join("my file.txt");
    std::fs::write(&input_file, b"data").unwrap();

    fatx_bin()
        .args([
            "write",
            img.to_str().unwrap(),
            "/my file.txt",
            "--input",
            input_file.to_str().unwrap(),
        ])
        .output()
        .unwrap();

    // Delete it
    let output = fatx_bin()
        .args(["rm", img.to_str().unwrap(), "/my file.txt"])
        .output()
        .expect("rm file with spaces");

    assert!(
        output.status.success(),
        "rm with spaces failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

// ===========================================================================
// fatx rm
// ===========================================================================

#[test]
fn test_cli_rm_file() {
    let (_tmp, img) = create_test_image(256, true);

    // Verify file exists
    let output = fatx_bin()
        .args(["ls", img.to_str().unwrap(), "/"])
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&output.stdout).contains("name.txt"));

    // Delete it
    let output = fatx_bin()
        .args(["rm", img.to_str().unwrap(), "/name.txt"])
        .output()
        .expect("run fatx rm");

    assert!(
        output.status.success(),
        "rm failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    // Verify it's gone
    let output = fatx_bin()
        .args(["ls", img.to_str().unwrap(), "/"])
        .output()
        .unwrap();
    assert!(!String::from_utf8_lossy(&output.stdout).contains("name.txt"));
}

#[test]
fn test_cli_rm_nonexistent_fails() {
    let (_tmp, img) = create_test_image(4, false);

    let output = fatx_bin()
        .args(["rm", img.to_str().unwrap(), "/nonexistent.txt"])
        .output()
        .expect("run fatx rm nonexistent");

    assert!(!output.status.success());
}

// ===========================================================================
// fatx rename
// ===========================================================================

#[test]
fn test_cli_rename() {
    let (_tmp, img) = create_test_image(256, true);

    let output = fatx_bin()
        .args(["rename", img.to_str().unwrap(), "/name.txt", "renamed.txt"])
        .output()
        .expect("run fatx rename");

    assert!(
        output.status.success(),
        "rename failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let output = fatx_bin()
        .args(["ls", img.to_str().unwrap(), "/"])
        .output()
        .unwrap();

    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(!stdout.contains("name.txt"), "old name should be gone");
    assert!(stdout.contains("renamed.txt"), "new name should be present");
}

// ===========================================================================
// fatx rmr (recursive delete)
// ===========================================================================

#[test]
fn test_cli_rmr() {
    let (_tmp, img) = create_test_image(256, true);

    // Content/ has subdirectories and files
    let output = fatx_bin()
        .args(["rmr", img.to_str().unwrap(), "/Content"])
        .output()
        .expect("run fatx rmr");

    assert!(
        output.status.success(),
        "rmr failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let output = fatx_bin()
        .args(["ls", img.to_str().unwrap(), "/"])
        .output()
        .unwrap();

    assert!(
        !String::from_utf8_lossy(&output.stdout).contains("Content"),
        "Content should be deleted"
    );
}

// ===========================================================================
// fatx hexdump
// ===========================================================================

#[test]
fn test_cli_hexdump() {
    let (_tmp, img) = create_test_image(4, false);

    let output = fatx_bin()
        .args(["hexdump", img.to_str().unwrap(), "--count", "64"])
        .output()
        .expect("run fatx hexdump");

    assert!(
        output.status.success(),
        "hexdump failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    // Should contain the FATX magic bytes in hex
    assert!(
        stdout.contains("46 41 54 58") || stdout.contains("46414458") || stdout.contains("FATX"),
        "hexdump should show FATX magic: {}",
        stdout
    );
}

// ===========================================================================
// fatx mkimage (via the fatx dispatcher)
// ===========================================================================

#[test]
fn test_cli_mkimage() {
    let tmp = TempDir::new().unwrap();
    let img = tmp.path().join("new.img");

    let output = fatx_bin()
        .args(["mkimage", img.to_str().unwrap(), "--size", "4M"])
        .output()
        .expect("run fatx mkimage");

    assert!(
        output.status.success(),
        "mkimage failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(img.exists(), "image file should be created");

    // Verify the image is valid by running info on it
    let output = fatx_bin()
        .args(["info", img.to_str().unwrap()])
        .output()
        .expect("run fatx info on new image");

    assert!(output.status.success());
    assert!(String::from_utf8_lossy(&output.stdout).contains("FATX Volume Information"));
}

#[test]
fn test_cli_mkimage_xtaf() {
    let tmp = TempDir::new().unwrap();
    let img = tmp.path().join("xbox360.img");

    let output = fatx_bin()
        .args([
            "mkimage",
            img.to_str().unwrap(),
            "--size",
            "4M",
            "--format",
            "xtaf",
        ])
        .output()
        .expect("run fatx mkimage xtaf");

    assert!(
        output.status.success(),
        "mkimage xtaf failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let output = fatx_bin()
        .args(["info", img.to_str().unwrap()])
        .output()
        .unwrap();

    assert!(output.status.success());
}

// ===========================================================================
// fatx scan
// ===========================================================================

#[test]
fn test_cli_scan_nonexistent_device() {
    let output = fatx_bin()
        .args(["scan", "/nonexistent/device"])
        .output()
        .expect("run fatx scan");

    assert!(!output.status.success());
}

#[test]
fn test_cli_scan_image() {
    let (_tmp, img) = create_test_image(4, false);

    let output = fatx_bin()
        .args(["scan", img.to_str().unwrap()])
        .output()
        .expect("run fatx scan on image");

    // Scan on a small image may or may not find partitions
    // but should not crash
    let _ = output.status;
}

// ===========================================================================
// fatx cleanup
// ===========================================================================

#[test]
fn test_cli_cleanup_dry_run_finds_ds_store() {
    let (_tmp, img) = create_test_image(4, false);

    let input_dir = TempDir::new().unwrap();
    let ds_store = input_dir.path().join(".DS_Store");
    std::fs::write(&ds_store, b"macos junk").unwrap();
    let real_file = input_dir.path().join("game.bin");
    std::fs::write(&real_file, b"real data").unwrap();

    fatx_bin()
        .args([
            "write",
            img.to_str().unwrap(),
            "/.DS_Store",
            "--input",
            ds_store.to_str().unwrap(),
        ])
        .output()
        .expect("write .DS_Store");

    fatx_bin()
        .args([
            "write",
            img.to_str().unwrap(),
            "/game.bin",
            "--input",
            real_file.to_str().unwrap(),
        ])
        .output()
        .expect("write game.bin");

    let output = fatx_bin()
        .args(["cleanup", img.to_str().unwrap(), "--dry-run"])
        .output()
        .expect("run fatx cleanup --dry-run");

    assert!(
        output.status.success(),
        "cleanup --dry-run failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout.contains(".DS_Store"),
        "dry-run should list .DS_Store, got: {}",
        stdout
    );

    // Verify .DS_Store still exists after dry-run
    let ls_output = fatx_bin()
        .args(["ls", img.to_str().unwrap(), "/"])
        .output()
        .expect("ls");
    let ls_stdout = String::from_utf8_lossy(&ls_output.stdout);
    assert!(
        ls_stdout.contains(".DS_Store"),
        ".DS_Store should still exist after dry-run"
    );
    assert!(
        ls_stdout.contains("game.bin"),
        "game.bin should still exist"
    );
}

#[test]
fn test_cli_copy_skips_macos_metadata() {
    let (_tmp, img) = create_test_image(16, false);

    let input_dir = TempDir::new().unwrap();
    let src = input_dir.path().join("GameDir");
    std::fs::create_dir(&src).unwrap();
    std::fs::write(src.join("game.bin"), b"real game data").unwrap();
    std::fs::write(src.join("save.dat"), b"save file").unwrap();
    std::fs::write(src.join(".DS_Store"), b"macos junk").unwrap();
    std::fs::write(src.join("._game.bin"), b"resource fork").unwrap();
    std::fs::create_dir(src.join(".Spotlight-V100")).unwrap();
    std::fs::write(src.join(".Spotlight-V100").join("store.db"), b"spotlight").unwrap();

    let output = fatx_bin()
        .args([
            "copy",
            img.to_str().unwrap(),
            "--from",
            src.to_str().unwrap(),
            "--to",
            "/GameDir",
        ])
        .output()
        .expect("run fatx copy");

    assert!(
        output.status.success(),
        "copy failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let ls_output = fatx_bin()
        .args(["ls", img.to_str().unwrap(), "/GameDir"])
        .output()
        .expect("ls GameDir");
    assert!(ls_output.status.success());
    let stdout = String::from_utf8_lossy(&ls_output.stdout);
    assert!(stdout.contains("game.bin"), "should have game.bin");
    assert!(stdout.contains("save.dat"), "should have save.dat");
    assert!(!stdout.contains(".DS_Store"), ".DS_Store should be skipped");
    assert!(
        !stdout.contains("._game.bin"),
        "._game.bin should be skipped"
    );
    assert!(
        !stdout.contains(".Spotlight"),
        ".Spotlight-V100 should be skipped"
    );
}
