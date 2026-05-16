//! Property-based roundtrip tests for fatxlib.
//!
//! Uses proptest to verify that write→read roundtrips preserve data
//! for arbitrary file contents, filenames, and sizes.

mod common;

use proptest::prelude::*;

// ===========================================================================
// Strategies bounded to FATX constraints
// ===========================================================================

/// FATX-valid filename: 1-42 alphanumeric chars + underscore/hyphen/dot
fn fatx_filename() -> impl Strategy<Value = String> {
    "[a-zA-Z][a-zA-Z0-9_.\\-]{0,41}".prop_filter("filename must be non-empty", |s| !s.is_empty())
}

/// File data: 0 to 256KB (bounded for test speed)
fn file_data() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(any::<u8>(), 0..262144)
}

/// Small file data: 0 to 16KB (for tests that create many files)
fn small_file_data() -> impl Strategy<Value = Vec<u8>> {
    prop::collection::vec(any::<u8>(), 0..16384)
}

// ===========================================================================
// Write → Read roundtrip
// ===========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(50))]

    #[test]
    fn test_write_read_roundtrip(data in file_data()) {
        let (_tmp, mut vol) = common::create_fatx_image(16);

        vol.create_file("/test.bin", &data).expect("create");
        let read_back = vol.read_file_by_path("/test.bin").expect("read");
        prop_assert_eq!(read_back, data);
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn test_write_read_roundtrip_with_name(
        name in fatx_filename(),
        data in small_file_data()
    ) {
        let (_tmp, mut vol) = common::create_fatx_image(4);

        let path = format!("/{}", name);
        vol.create_file(&path, &data).expect("create");
        let read_back = vol.read_file_by_path(&path).expect("read");
        prop_assert_eq!(read_back, data);
    }
}

// ===========================================================================
// Write-in-place → Read roundtrip
// ===========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(30))]

    #[test]
    fn test_write_in_place_roundtrip(
        initial in small_file_data(),
        replacement in small_file_data()
    ) {
        let (_tmp, mut vol) = common::create_fatx_image(8);

        // Create with initial data
        if initial.is_empty() {
            // write_file_in_place needs the file to exist; create_file with empty data
            // allocates 1 cluster. Skip truly empty initial data.
            return Ok(());
        }
        vol.create_file("/test.bin", &initial).expect("create");

        // Overwrite in place
        vol.write_file_in_place("/test.bin", &replacement).expect("write in place");

        let read_back = vol.read_file_by_path("/test.bin").expect("read");
        prop_assert_eq!(read_back, replacement);
    }
}

// ===========================================================================
// Create + delete preserves other files
// ===========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(20))]

    #[test]
    fn test_create_delete_preserves_other_files(
        data_a in small_file_data(),
        data_b in small_file_data(),
    ) {
        let (_tmp, mut vol) = common::create_fatx_image(8);

        vol.create_file("/a.bin", &data_a).expect("create a");
        vol.create_file("/b.bin", &data_b).expect("create b");

        // Delete a
        vol.delete("/a.bin").expect("delete a");

        // b should be unchanged
        let read_b = vol.read_file_by_path("/b.bin").expect("read b");
        prop_assert_eq!(read_b, data_b);

        // a should be gone
        prop_assert!(vol.read_file_by_path("/a.bin").is_err());
    }
}

// ===========================================================================
// Stats invariant: free + used + bad == total
// ===========================================================================

proptest! {
    #![proptest_config(ProptestConfig::with_cases(20))]

    #[test]
    fn test_stats_invariant_after_operations(
        file_count in 1usize..8,
        data in small_file_data(),
    ) {
        let (_tmp, mut vol) = common::create_fatx_image(8);

        // Create files
        for i in 0..file_count {
            let name = format!("/f{}.bin", i);
            vol.create_file(&name, &data).expect("create");
        }

        let stats = vol.stats().expect("stats");
        prop_assert_eq!(
            stats.total_clusters,
            stats.free_clusters + stats.used_clusters + stats.bad_clusters,
            "free + used + bad should equal total"
        );

        // Delete half
        for i in 0..(file_count / 2) {
            let name = format!("/f{}.bin", i);
            vol.delete(&name).expect("delete");
        }

        let stats2 = vol.stats().expect("stats after delete");
        prop_assert_eq!(
            stats2.total_clusters,
            stats2.free_clusters + stats2.used_clusters + stats2.bad_clusters,
            "invariant should hold after deletes too"
        );
    }
}
