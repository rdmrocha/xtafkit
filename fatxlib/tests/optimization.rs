//! Optimization regression tests for v0.2.0 features.
//!
//! Tests: prev_free allocation, free-cluster bitmap, dirty-range flush,
//! cached stats (O(1)), and configurable I/O alignment.

mod common;

use fatxlib::types::*;

// ===========================================================================
// prev_free allocation hint (next-fit)
// ===========================================================================

#[test]
fn test_prev_free_allocates_forward() {
    let (_tmp, mut vol) = common::create_fatx_image(4);

    // Allocate two clusters — second should have a higher index than first
    let c1 = vol.allocate_cluster().expect("alloc 1");
    let c2 = vol.allocate_cluster().expect("alloc 2");
    assert!(
        c2 > c1,
        "next-fit: second cluster {} should be after first {}",
        c2,
        c1
    );
}

#[test]
fn test_prev_free_after_free_still_advances() {
    let (_tmp, mut vol) = common::create_fatx_image(4);

    let c1 = vol.allocate_cluster().expect("alloc 1");
    let _c2 = vol.allocate_cluster().expect("alloc 2");
    let c3 = vol.allocate_cluster().expect("alloc 3");

    // Free c1 (early cluster)
    vol.free_chain(c1).expect("free c1");

    // Next allocation should NOT go back to c1 — it should continue forward
    let c4 = vol.allocate_cluster().expect("alloc 4");
    assert!(
        c4 > c3,
        "prev_free should advance past freed cluster: c4={} should be > c3={}",
        c4,
        c3
    );
}

#[test]
fn test_prev_free_wraps_around() {
    let (_tmp, mut vol) = common::create_fatx_image(2); // small image — fewer clusters

    let stats = vol.stats().expect("stats");
    let total_free = stats.free_clusters;

    // Allocate most clusters to push prev_free near the end
    let mut allocated = Vec::new();
    for _ in 0..(total_free - 2) {
        match vol.allocate_cluster() {
            Ok(c) => allocated.push(c),
            Err(_) => break,
        }
    }

    // Free an early cluster
    let early = allocated[0];
    vol.free_chain(early).expect("free early");

    // Allocate — should wrap around and find the freed cluster
    let c = vol.allocate_cluster().expect("alloc after wrap");
    // It should find something (we freed one)
    assert!(c > 0, "should allocate successfully after wraparound");
}

// ===========================================================================
// Free-cluster bitmap consistency
// ===========================================================================

#[test]
fn test_bitmap_matches_stats_on_open() {
    let (_tmp, vol) = common::create_fatx_image(4);

    let stats = vol.stats().expect("stats");
    // Stats uses cached counts which were computed from the same FAT scan that built the bitmap
    assert!(stats.free_clusters > 0);
    assert_eq!(stats.bad_clusters, 0);
    assert_eq!(
        stats.total_clusters,
        stats.free_clusters + stats.used_clusters + stats.bad_clusters
    );
}

#[test]
fn test_bitmap_consistent_after_allocations() {
    let (_tmp, mut vol) = common::create_fatx_image(4);

    let stats_before = vol.stats().expect("stats");

    // Allocate 10 clusters
    for _ in 0..10 {
        vol.allocate_cluster().expect("alloc");
    }

    let stats_after = vol.stats().expect("stats");
    assert_eq!(
        stats_after.free_clusters,
        stats_before.free_clusters - 10,
        "free count should decrease by exactly 10"
    );
}

#[test]
fn test_bitmap_consistent_after_free_chain() {
    let (_tmp, mut vol) = common::create_fatx_image(4);

    let stats_before = vol.stats().expect("stats");

    // Allocate a chain of 5 clusters
    let first = vol.allocate_chain(5).expect("alloc chain");
    let stats_during = vol.stats().expect("stats");
    assert_eq!(stats_during.free_clusters, stats_before.free_clusters - 5);

    // Free the chain
    vol.free_chain(first).expect("free chain");
    let stats_after = vol.stats().expect("stats");
    assert_eq!(
        stats_after.free_clusters, stats_before.free_clusters,
        "free count should return to original after free_chain"
    );
}

#[test]
fn test_bitmap_after_create_delete_cycle() {
    let (_tmp, mut vol) = common::create_fatx_image(4);

    let stats_initial = vol.stats().expect("stats");

    // Create and delete files repeatedly
    for i in 0..5 {
        let name = format!("/cycle_{}.bin", i);
        vol.create_file(&name, &vec![0xAA; 16384]).expect("create");
        vol.delete(&name).expect("delete");
    }

    let stats_final = vol.stats().expect("stats");
    assert_eq!(
        stats_final.free_clusters, stats_initial.free_clusters,
        "free count should be unchanged after create+delete cycles"
    );
}

// ===========================================================================
// Dirty-range FAT tracking
// ===========================================================================

#[test]
fn test_flush_after_no_changes_is_noop() {
    let (_tmp, mut vol) = common::create_fatx_image(4);

    // Flush without any changes — should succeed and be a no-op
    vol.flush().expect("flush no-op");
}

#[test]
fn test_flush_after_create_preserves_data() {
    let (_tmp, mut vol) = common::create_fatx_image(4);

    vol.create_file("/dirty.txt", b"dirty range test data")
        .expect("create");
    vol.flush().expect("flush");

    // Read back to verify data survived the flush
    let data = vol.read_file_by_path("/dirty.txt").expect("read");
    assert_eq!(data, b"dirty range test data");
}

#[test]
fn test_flush_after_create_delete_no_corruption() {
    let (_tmp, mut vol) = common::create_fatx_image(4);

    // Create several files
    vol.create_file("/a.txt", b"file a").expect("create a");
    vol.create_file("/b.txt", b"file b").expect("create b");
    vol.create_file("/c.txt", &vec![0xCC; 65536])
        .expect("create c");

    // Delete one
    vol.delete("/b.txt").expect("delete b");

    // Flush
    vol.flush().expect("flush");

    // Verify remaining files are intact
    let a = vol.read_file_by_path("/a.txt").expect("read a");
    assert_eq!(a, b"file a");
    let c = vol.read_file_by_path("/c.txt").expect("read c");
    assert_eq!(c, vec![0xCC; 65536]);

    // Verify deleted file is gone
    assert!(vol.read_file_by_path("/b.txt").is_err());
}

// ===========================================================================
// Cached stats (O(1))
// ===========================================================================

#[test]
fn test_stats_decrements_on_create() {
    let (_tmp, mut vol) = common::create_fatx_image(4);

    let before = vol.stats().expect("stats").free_clusters;
    vol.create_file("/test.bin", &vec![0u8; 16384])
        .expect("create"); // 1 cluster
    let after = vol.stats().expect("stats").free_clusters;

    assert!(after < before, "free count should decrease after create");
}

#[test]
fn test_stats_increments_on_delete() {
    let (_tmp, mut vol) = common::create_fatx_image(4);

    vol.create_file("/test.bin", &vec![0u8; 16384])
        .expect("create");
    let before = vol.stats().expect("stats").free_clusters;

    vol.delete("/test.bin").expect("delete");
    let after = vol.stats().expect("stats").free_clusters;

    assert!(after > before, "free count should increase after delete");
}

#[test]
fn test_stats_matches_manual_count() {
    let (_tmp, mut vol) = common::create_fatx_image(4);

    // Create some files to make it interesting
    vol.create_file("/a.bin", &vec![0u8; 32768])
        .expect("create a");
    vol.create_file("/b.bin", &vec![0u8; 16384])
        .expect("create b");

    let stats = vol.stats().expect("stats");

    // Manual count: iterate FAT and count free entries
    let mut manual_free = 0u32;
    let mut manual_used = 0u32;
    for cluster in 1..(1 + vol.total_clusters) {
        match vol.read_fat_entry(cluster).expect("read fat") {
            FatEntry::Free => manual_free += 1,
            _ => manual_used += 1,
        }
    }

    assert_eq!(
        stats.free_clusters, manual_free,
        "cached free count should match manual scan"
    );
    assert_eq!(
        stats.used_clusters, manual_used,
        "cached used count should match manual scan"
    );
}

// ===========================================================================
// I/O alignment
// ===========================================================================

#[test]
fn test_default_alignment_works() {
    // File-backed images use default 512-byte alignment
    let (_tmp, mut vol) = common::create_fatx_image(4);

    // Basic read/write should work with default alignment
    vol.create_file("/align.txt", b"alignment test")
        .expect("create");
    let data = vol.read_file_by_path("/align.txt").expect("read");
    assert_eq!(data, b"alignment test");
}

#[test]
fn test_read_write_at_various_offsets() {
    let (_tmp, mut vol) = common::create_fatx_image(4);

    // Create files of various sizes to exercise different alignment scenarios
    vol.create_file("/tiny.txt", b"x").expect("create tiny");
    vol.create_file("/small.txt", &vec![0xBB; 511])
        .expect("create small");
    vol.create_file("/aligned.txt", &vec![0xCC; 512])
        .expect("create aligned");
    vol.create_file("/large.txt", &vec![0xDD; 4097])
        .expect("create large");

    assert_eq!(vol.read_file_by_path("/tiny.txt").expect("read"), b"x");
    assert_eq!(
        vol.read_file_by_path("/small.txt").expect("read"),
        vec![0xBB; 511]
    );
    assert_eq!(
        vol.read_file_by_path("/aligned.txt").expect("read"),
        vec![0xCC; 512]
    );
    assert_eq!(
        vol.read_file_by_path("/large.txt").expect("read"),
        vec![0xDD; 4097]
    );
}

// ===========================================================================
// XTAF (big-endian) optimization tests
// ===========================================================================

#[test]
fn test_xtaf_stats_and_allocation() {
    let (_tmp, mut vol) = common::create_xtaf_image(4);

    let stats = vol.stats().expect("stats");
    assert!(stats.free_clusters > 0);

    let before = stats.free_clusters;
    vol.create_file("/xtaf_test.bin", &vec![0u8; 32768])
        .expect("create");
    let after = vol.stats().expect("stats").free_clusters;
    assert!(
        after < before,
        "XTAF: free count should decrease after create"
    );
}
