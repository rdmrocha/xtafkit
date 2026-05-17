//! Shared policy decisions for ISO-derived operations.

pub fn is_systemupdate_path(path: &str) -> bool {
    path.trim_start_matches('/')
        .split('/')
        .next()
        .unwrap_or("")
        .eq_ignore_ascii_case("$SystemUpdate")
}
