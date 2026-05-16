//! Xbox 360 profile Account-blob decryption.
//!
//! Profile STFS packages embed an `Account` file (404 bytes) that contains
//! the gamertag, encrypted with ARC4 using an HMAC-SHA1-derived key. The
//! encryption keys are public knowledge — they appear in py360, Le Fluffie,
//! Velocity, free60 wikis, and other community tools that have shipped for
//! over a decade. They are not security-sensitive (they protect against
//! casual tampering, not against the user themselves).
//!
//! References:
//! - [py360 account.py](https://github.com/arkem/py360/blob/main/py360/account.py)
//!
//! Decrypted Account layout:
//! ```text
//!   0x00       Account flags (bit 5 = Live/Local)
//!   0x10-0x2D  Gamertag (UTF-16BE, 15 chars max, NUL-terminated)
//!   0x30-0x37  XUID
//!   0x39       Membership type
//!   0x3C-0x3F  Account type "PROD" or "PART"
//! ```

use hmac::{Hmac, Mac};
use sha1::Sha1;

/// The fixed length of an encrypted Account block.
pub const ACCOUNT_BLOCK_LEN: usize = 0x194; // 404 bytes

/// Production ("PROD") key — used by retail accounts.
const KEY_PROD: [u8; 16] = [
    0xE1, 0xBC, 0x15, 0x9C, 0x73, 0xB1, 0xEA, 0xE9, 0xAB, 0x31, 0x70, 0xF3, 0xAD, 0x47, 0xEB, 0xF3,
];

/// Partner/dev ("OTHER") key — used by some non-retail accounts.
const KEY_OTHER: [u8; 16] = [
    0xDA, 0xB6, 0x9A, 0xD9, 0x8E, 0x28, 0x76, 0x4F, 0x97, 0x7E, 0xE2, 0x48, 0x7E, 0x4F, 0x3F, 0x68,
];

const GAMERTAG_OFFSET: usize = 0x10;
const GAMERTAG_LEN: usize = 0x1E; // 30 bytes = 15 UTF-16BE chars

/// Attempt to decrypt an Account block and pull the gamertag out. Tries
/// both PROD and OTHER keys; returns the first that decrypts to a
/// plausible gamertag.
///
/// Returns `None` if `block` is the wrong length or neither key yields a
/// valid result.
pub fn extract_gamertag(block: &[u8]) -> Option<String> {
    if block.len() != ACCOUNT_BLOCK_LEN {
        return None;
    }
    for key in [&KEY_PROD, &KEY_OTHER] {
        let plaintext = decrypt(block, key);
        if let Some(name) = read_gamertag(&plaintext) {
            return Some(name);
        }
    }
    None
}

/// HMAC-SHA1(key, block[..16]) → ARC4 key → decrypt block[16..].
fn decrypt(block: &[u8], key: &[u8]) -> Vec<u8> {
    type HmacSha1 = Hmac<Sha1>;
    let mut mac = HmacSha1::new_from_slice(key).expect("HMAC accepts any key length");
    mac.update(&block[..16]);
    let hmac_out = mac.finalize().into_bytes();
    let arc4_key = &hmac_out[..16];
    let mut ciphertext = block[16..].to_vec();
    arc4(arc4_key, &mut ciphertext);
    ciphertext
}

/// In-place ARC4 (RC4) cipher. The algorithm is symmetric — same routine
/// encrypts and decrypts.
fn arc4(key: &[u8], data: &mut [u8]) {
    let mut s = [0u8; 256];
    for i in 0..256 {
        s[i] = i as u8;
    }
    let mut j: u8 = 0;
    for i in 0..256 {
        j = j.wrapping_add(s[i]).wrapping_add(key[i % key.len()]);
        s.swap(i, j as usize);
    }
    let mut i: u8 = 0;
    let mut j: u8 = 0;
    for byte in data.iter_mut() {
        i = i.wrapping_add(1);
        j = j.wrapping_add(s[i as usize]);
        s.swap(i as usize, j as usize);
        let k = s[(s[i as usize].wrapping_add(s[j as usize])) as usize];
        *byte ^= k;
    }
}

/// Decode a 30-byte UTF-16BE NUL-terminated string at the gamertag offset.
/// Returns `None` if empty or the result fails [`looks_like_gamertag`].
fn read_gamertag(plaintext: &[u8]) -> Option<String> {
    if plaintext.len() < GAMERTAG_OFFSET + GAMERTAG_LEN {
        return None;
    }
    let field = &plaintext[GAMERTAG_OFFSET..GAMERTAG_OFFSET + GAMERTAG_LEN];
    let mut code_units = Vec::with_capacity(field.len() / 2);
    for pair in field.chunks_exact(2) {
        let cu = u16::from_be_bytes([pair[0], pair[1]]);
        if cu == 0 {
            break;
        }
        code_units.push(cu);
    }
    if code_units.is_empty() {
        return None;
    }
    let s = String::from_utf16(&code_units).ok()?;
    if !looks_like_gamertag(&s) {
        return None;
    }
    Some(s)
}

/// Xbox Live gamertags: 1–15 chars, start with a letter, contain
/// letters/digits/spaces. This guard rejects junk that happens to decrypt
/// to non-empty UTF-16 (rare with the wrong key but possible).
fn looks_like_gamertag(s: &str) -> bool {
    let len = s.chars().count();
    if !(1..=15).contains(&len) {
        return false;
    }
    let mut chars = s.chars();
    let first = chars.next().unwrap();
    if !first.is_ascii_alphabetic() {
        return false;
    }
    s.chars().all(|c| c.is_ascii_alphanumeric() || c == ' ')
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Helper exposed to the parent module's tests.
    pub(crate) fn synth_block_helper(gamertag: &str) -> Vec<u8> {
        synth_block(gamertag, &KEY_PROD)
    }

    /// Build a synthetic Account block: 16-byte HMAC seed + ARC4-encrypted
    /// 388 bytes containing the gamertag at offset 0x10.
    fn synth_block(gamertag: &str, key: &[u8; 16]) -> Vec<u8> {
        let mut plaintext = vec![0u8; ACCOUNT_BLOCK_LEN - 16];
        // Write gamertag at 0x10 as UTF-16BE, NUL-padded.
        for (i, cu) in gamertag.encode_utf16().enumerate() {
            let off = GAMERTAG_OFFSET + i * 2;
            plaintext[off..off + 2].copy_from_slice(&cu.to_be_bytes());
        }
        // Random-looking seed bytes (deterministic for the test).
        let seed: Vec<u8> = (0..16u8).collect();
        // Derive ARC4 key the same way decrypt() does.
        type HmacSha1 = Hmac<Sha1>;
        let mut mac = HmacSha1::new_from_slice(key).unwrap();
        mac.update(&seed);
        let hmac_out = mac.finalize().into_bytes();
        let arc4_key = &hmac_out[..16];
        arc4(arc4_key, &mut plaintext);
        let mut out = Vec::with_capacity(ACCOUNT_BLOCK_LEN);
        out.extend_from_slice(&seed);
        out.extend_from_slice(&plaintext);
        out
    }

    #[test]
    fn extracts_gamertag_prod_key() {
        let block = synth_block("RogerR", &KEY_PROD);
        assert_eq!(extract_gamertag(&block), Some("RogerR".to_string()));
    }

    #[test]
    fn extracts_gamertag_other_key() {
        let block = synth_block("DevTester", &KEY_OTHER);
        assert_eq!(extract_gamertag(&block), Some("DevTester".to_string()));
    }

    #[test]
    fn extracts_gamertag_with_spaces() {
        let block = synth_block("MLG Pro 42", &KEY_PROD);
        assert_eq!(extract_gamertag(&block), Some("MLG Pro 42".to_string()));
    }

    #[test]
    fn rejects_wrong_length_input() {
        assert_eq!(extract_gamertag(&vec![0u8; 100]), None);
        assert_eq!(extract_gamertag(&vec![0u8; ACCOUNT_BLOCK_LEN + 1]), None);
    }

    #[test]
    fn rejects_random_bytes() {
        let mut rng_block = vec![0u8; ACCOUNT_BLOCK_LEN];
        for (i, b) in rng_block.iter_mut().enumerate() {
            *b = ((i * 31 + 17) % 256) as u8;
        }
        assert_eq!(extract_gamertag(&rng_block), None);
    }

    #[test]
    fn looks_like_gamertag_rules() {
        assert!(looks_like_gamertag("Bob"));
        assert!(looks_like_gamertag("MLG Pro 42"));
        assert!(!looks_like_gamertag(""));
        assert!(!looks_like_gamertag("1Bob")); // must start with letter
        assert!(!looks_like_gamertag("WayTooLongGamertag")); // > 15 chars
        assert!(!looks_like_gamertag("Bob!")); // special char
    }
}
