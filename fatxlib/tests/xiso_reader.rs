//! Integration tests for the xdvdfs-backed XISO reader.

mod common;

use std::fs::File;
use std::io::Cursor;

use fatxlib::iso::image::XisoImage;

// ---------------------------------------------------------------------------
// Negative paths — always runnable
// ---------------------------------------------------------------------------

#[test]
fn rejects_obviously_non_xiso_source() {
    let buf = vec![0u8; 4096];
    let cursor = Cursor::new(buf);
    assert!(
        XisoImage::open(cursor).is_err(),
        "all-zero buffer should not parse as XDVDFS"
    );
}

#[test]
fn rejects_too_small_source() {
    // The XDVDFS volume descriptor lives at sector 32 (offset 0x10000).
    // Anything smaller than that can't possibly be valid.
    let buf = vec![0u8; 1024];
    let cursor = Cursor::new(buf);
    assert!(
        XisoImage::open(cursor).is_err(),
        "tiny buffer should be rejected"
    );
}

// ---------------------------------------------------------------------------
// Positive paths — require a fixture XISO at tests/fixtures/tiny.xiso
// ---------------------------------------------------------------------------

const FIXTURE: &str = "tests/fixtures/tiny.xiso";

fn open_fixture() -> Option<XisoImage<File>> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(FIXTURE);
    if !path.exists() {
        eprintln!("skipping: fixture missing at {}", path.display());
        return None;
    }
    let file = File::open(&path).expect("open fixture");
    Some(XisoImage::open(file).expect("parse fixture"))
}

#[test]
fn walks_fixture_image() {
    let Some(mut img) = open_fixture() else {
        return;
    };
    let files = img.walk_files().expect("walk");
    assert!(
        !files.is_empty(),
        "fixture should contain at least one file"
    );
    let names: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
    assert!(
        names.iter().any(|n| n.ends_with("default.xex")),
        "expected default.xex (XellLaunch2_retail) in fixture; got {:?}",
        names
    );
}

#[test]
fn streams_a_file_into_buffer() {
    let Some(mut img) = open_fixture() else {
        return;
    };
    let files = img.walk_files().expect("walk");
    let first = files.first().expect("at least one file in fixture");

    let mut sink = Vec::new();
    let n = img
        .read_into(first, &mut sink, Some(64 * 1024), None)
        .expect("read_into");
    assert_eq!(n as usize, sink.len());
    assert_eq!(n, first.size);
}

#[test]
fn file_reader_matches_read_into() {
    use std::io::Read;
    let Some(mut img) = open_fixture() else {
        return;
    };
    let files = img.walk_files().expect("walk");
    let first = files.first().expect("at least one file in fixture").clone();

    let mut via_read_into = Vec::new();
    img.read_into(&first, &mut via_read_into, None, None)
        .expect("read_into");

    let mut via_reader = Vec::new();
    img.file_reader(&first)
        .read_to_end(&mut via_reader)
        .expect("read_to_end");

    assert_eq!(via_reader.len() as u64, first.size);
    assert_eq!(
        via_reader, via_read_into,
        "file_reader output must match read_into byte for byte"
    );
}

#[test]
fn extract_fixture_into_fatx_volume() {
    let Some(mut img) = open_fixture() else {
        return;
    };
    let files = img.walk_files().expect("walk");
    assert!(!files.is_empty(), "fixture must have at least one file");

    let (_tmp, mut vol) = common::create_fatx_image(4);

    for f in &files {
        // Create parent directories as needed (e.g. /Media/).
        let fatx_path = if f.path.starts_with('/') {
            f.path.clone()
        } else {
            format!("/{}", f.path)
        };
        if let Some(slash) = fatx_path.rfind('/')
            && slash > 0
        {
            let parent = &fatx_path[..slash];
            // create_directory is strict on existence; ignore "already exists"
            // because we may share parents across siblings.
            match vol.create_directory(parent) {
                Ok(()) | Err(fatxlib::error::FatxError::FileExists(_)) => {}
                Err(e) => panic!("mkdir {parent}: {e}"),
            }
        }

        let reader = img.file_reader(f);
        vol.create_file_from_reader(&fatx_path, f.size, reader, None)
            .unwrap_or_else(|e| panic!("stream {} -> {}: {}", f.path, fatx_path, e));
    }

    // Verify every extracted file matches what read_into produces.
    for f in &files {
        let fatx_path = if f.path.starts_with('/') {
            f.path.clone()
        } else {
            format!("/{}", f.path)
        };
        let mut expected = Vec::new();
        img.read_into(f, &mut expected, None, None)
            .expect("read expected");
        let got = vol.read_file_by_path(&fatx_path).expect("read fatx");
        assert_eq!(
            got, expected,
            "extracted {fatx_path} does not match XISO source"
        );
    }
}

#[test]
fn fixture_title_info_returns_xellaunch() {
    let Some(mut img) = open_fixture() else {
        return;
    };
    let info = img
        .title_info()
        .expect("title_info should succeed on fixture")
        .expect("fixture has a Default.xex, so title_info must be Some");
    assert_eq!(
        info.execution_info.title_id, 0xFFFF011D,
        "fixture default.xex is XellLaunch2_retail (TitleID 0xFFFF011D)"
    );
}

#[test]
fn streams_invokes_progress_callback() {
    let Some(mut img) = open_fixture() else {
        return;
    };
    let files = img.walk_files().expect("walk");
    let first = files.first().expect("at least one file in fixture");

    let mut progress_calls: Vec<(u64, u64)> = Vec::new();
    let mut sink = Vec::new();
    {
        let mut cb = |read: u64, total: u64| progress_calls.push((read, total));
        img.read_into(first, &mut sink, Some(64), Some(&mut cb))
            .expect("read_into with progress");
    }
    if first.size > 0 {
        assert!(
            !progress_calls.is_empty(),
            "progress should fire at least once"
        );
        let (last_read, last_total) = *progress_calls.last().unwrap();
        assert_eq!(last_read, first.size);
        assert_eq!(last_total, first.size);
    }
}
