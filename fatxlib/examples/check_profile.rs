//! Quick diagnostic: read a raw STFS profile package from a local file,
//! walk 4KB-aligned offsets, and report the first gamertag the Account
//! decryption manages to extract.
//!
//! Usage:
//!     cargo run --example check_profile -- <path-to-profile-file>

use std::env;
use std::fs;

use fatxlib::xuids::account;

fn main() {
    let path = env::args().nth(1).expect("usage: check_profile <path>");
    let bytes = fs::read(&path).expect("read profile file");
    println!(
        "Read {} bytes from {}; scanning 0x1000-aligned offsets for Account blocks…",
        bytes.len(),
        path
    );

    let mut found_any = false;
    let mut off = 0;
    while off + account::ACCOUNT_BLOCK_LEN <= bytes.len() {
        if let Some(name) =
            account::extract_gamertag(&bytes[off..off + account::ACCOUNT_BLOCK_LEN])
        {
            println!("  hit @ 0x{off:06X}: gamertag = {name:?}");
            found_any = true;
        }
        off += 0x1000;
    }
    if !found_any {
        println!("  no Account block decrypted to a valid gamertag");
    }
}
