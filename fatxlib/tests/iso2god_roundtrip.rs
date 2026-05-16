//! Integration smoke test for [`fatxlib::iso2god::convert_iso`] and
//! [`fatxlib::iso2god::convert_iso_to_fatx`].
//!
//! Runs end-to-end against the bundled `tiny.xiso` fixture — a synthetic
//! XISO packed via `xdvdfs pack` that contains a real `default.xex`
//! (XellLaunch2_retail, a public homebrew launcher from the Free60
//! project). The XEX has valid `XEX2` magic + execution-info fields, so
//! `TitleInfo::from_image` parses it cleanly and the full pipeline runs.
//!
//! Focuses on "the pipeline runs to completion and the output is shaped
//! correctly", not byte-equality — the GoD format is deterministic, and
//! byte-equality is best validated against an external reference
//! conversion when one is available.

mod common;

use std::fs;
use std::path::PathBuf;

use fatxlib::iso2god::{ConvertOptions, TrimMode, convert_iso, convert_iso_to_fatx};

fn fixture_path() -> Option<PathBuf> {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/tiny.xiso");
    if p.exists() { Some(p) } else { None }
}

#[test]
fn converts_fixture_into_valid_god_package() {
    let Some(iso) = fixture_path() else {
        eprintln!("skipping: fatxlib/tests/fixtures/tiny.xiso missing");
        return;
    };

    let tmp = tempfile::TempDir::new().expect("tempdir");
    let dest = tmp.path();

    let mut opts = ConvertOptions {
        trim: TrimMode::FromEnd,
        game_title: Some("XellLaunch2 fixture"),
        dry_run: false,
        progress: None,
        should_abort: None,
    };

    let report = convert_iso(&iso, dest, &mut opts).expect("convert_iso");

    assert!(report.title_id != 0, "title id should be non-zero");
    assert!(
        report.part_count >= 1,
        "fixture must produce at least one Data part; got {:?}",
        report
    );
    assert!(report.block_count >= 1);

    // CON header lives at <dest>/<title_id>/<content_type>/<media_id>
    let title_hex = format!("{:08X}", report.title_id);
    let ctype_hex = format!("{:08X}", report.content_type as u32);
    let media_hex = if matches!(
        report.content_type,
        fatxlib::iso2god::god::ContentType::XboxOriginal
    ) {
        title_hex.clone()
    } else {
        format!("{:08X}", report.media_id)
    };

    let con_header_path = dest.join(&title_hex).join(&ctype_hex).join(&media_hex);
    let data_dir = dest
        .join(&title_hex)
        .join(&ctype_hex)
        .join(format!("{}.data", media_hex));
    let first_part = data_dir.join("Data0000");

    assert!(
        con_header_path.exists(),
        "CON header missing at {}",
        con_header_path.display()
    );
    assert!(
        first_part.exists(),
        "Data0000 missing at {}",
        first_part.display()
    );

    let con_header_size = fs::metadata(&con_header_path).expect("stat header").len();
    assert_eq!(
        con_header_size, 0xB000,
        "CON header should be 45 056 bytes (empty_live template)"
    );

    let first_part_size = fs::metadata(&first_part).expect("stat data").len();
    assert!(
        first_part_size > 0,
        "Data0000 should be non-empty; got {} bytes",
        first_part_size
    );

    // CON header should start with "LIVE" (`empty_live.bin` magic).
    let head = fs::read(&con_header_path).expect("read header");
    assert_eq!(
        &head[..4],
        b"LIVE",
        "CON header missing LIVE magic; got {:?}",
        &head[..4]
    );
}

#[test]
fn fixture_dry_run_does_not_create_files() {
    let Some(iso) = fixture_path() else {
        return;
    };

    let tmp = tempfile::TempDir::new().expect("tempdir");
    let dest = tmp.path();

    let mut opts = ConvertOptions {
        trim: TrimMode::FromEnd,
        game_title: None,
        dry_run: true,
        progress: None,
        should_abort: None,
    };

    let report = convert_iso(&iso, dest, &mut opts).expect("dry-run convert");
    assert!(report.part_count >= 1);

    let entries: Vec<_> = fs::read_dir(dest)
        .expect("readdir")
        .filter_map(|e| e.ok())
        .collect();
    assert!(
        entries.is_empty(),
        "dry_run should not write anything; found {:?}",
        entries.iter().map(|e| e.path()).collect::<Vec<_>>()
    );
}

#[test]
fn fixture_extracts_expected_title_id() {
    // XellLaunch2_retail's TitleID is 0xFFFF011D (homebrew/dev range).
    // If this assertion fires, either the fixture changed or the XEX
    // parser drifted.
    let Some(iso) = fixture_path() else {
        return;
    };

    let tmp = tempfile::TempDir::new().expect("tempdir");
    let mut opts = ConvertOptions {
        dry_run: true,
        ..Default::default()
    };

    let report = convert_iso(&iso, tmp.path(), &mut opts).expect("dry-run convert");
    assert_eq!(
        report.title_id, 0xFFFF011D,
        "expected XellLaunch2_retail TitleID; fixture may have changed"
    );
}

#[test]
fn streams_fixture_into_fatx_volume() {
    let Some(iso) = fixture_path() else {
        return;
    };
    let (_tmp, mut vol) = common::create_fatx_image(8);

    let mut opts = ConvertOptions {
        trim: TrimMode::FromEnd,
        game_title: Some("XellLaunch2 fixture"),
        dry_run: false,
        progress: None,
        should_abort: None,
    };

    let report = convert_iso_to_fatx(&iso, &mut vol, "/", &mut opts).expect("convert_iso_to_fatx");

    assert!(report.title_id != 0);
    assert!(report.part_count >= 1);

    // The Title-ID tree should live at the FATX root.
    let title_dir = format!("/{:08X}", report.title_id);
    let content_dir = format!("{}/{:08X}", title_dir, report.content_type as u32);
    let media_id_hex = if matches!(
        report.content_type,
        fatxlib::iso2god::god::ContentType::XboxOriginal
    ) {
        format!("{:08X}", report.title_id)
    } else {
        format!("{:08X}", report.media_id)
    };
    let con_header_path = format!("{}/{}", content_dir, media_id_hex);
    let data_dir = format!("{}/{}.data", content_dir, media_id_hex);
    let first_part_path = format!("{}/Data0000", data_dir);

    let header_bytes = vol
        .read_file_by_path(&con_header_path)
        .expect("read CON header from FATX");
    assert_eq!(
        header_bytes.len(),
        0xB000,
        "CON header should be 45 056 bytes"
    );
    assert_eq!(
        &header_bytes[..4],
        b"LIVE",
        "CON header missing LIVE magic; got {:?}",
        &header_bytes[..4]
    );

    let first_part_bytes = vol
        .read_file_by_path(&first_part_path)
        .expect("read Data0000 from FATX");
    assert!(
        !first_part_bytes.is_empty(),
        "Data0000 should be non-empty on FATX"
    );
}

#[test]
fn streaming_dry_run_writes_nothing_to_fatx() {
    let Some(iso) = fixture_path() else {
        return;
    };
    let (_tmp, mut vol) = common::create_fatx_image(4);

    let initial_free = vol.stats().expect("stats").free_clusters;

    let mut opts = ConvertOptions {
        dry_run: true,
        ..Default::default()
    };
    let report =
        convert_iso_to_fatx(&iso, &mut vol, "/", &mut opts).expect("dry-run convert_iso_to_fatx");
    assert!(report.part_count >= 1);

    let final_free = vol.stats().expect("stats").free_clusters;
    assert_eq!(
        final_free, initial_free,
        "dry-run must not allocate any clusters"
    );
}
