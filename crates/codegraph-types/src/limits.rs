//! The one source-size limit every crate agrees on.
//!
//! The indexer, the resolver and change detection must all treat the same files as "too big",
//! or a file one of them skipped is read by another (or hashed as content while stored as a
//! stamp). They therefore ask here instead of keeping their own constant.

use std::sync::OnceLock;

/// Environment variable that sets the limit in MiB. `0` removes the limit.
pub const MAX_FILE_MB_ENV: &str = "CODEGRAPHV_MAX_FILE_MB";

/// Default limit: 10 MiB. Hand-written source files of several MiB exist (a 1.4 MB name matcher
/// is one), while the files that only waste the parser (generated `parser.c`, minified bundles,
/// dependency archives) are mostly far larger.
pub const DEFAULT_MAX_FILE_BYTES: u64 = 10 * 1024 * 1024;

/// The limit for a value of [`MAX_FILE_MB_ENV`]. `None` or an unusable value (empty, not a
/// number, negative, NaN, infinite) gives the default; `0` gives no limit.
pub fn parse_max_file_bytes(value: Option<&str>) -> u64 {
    let Some(mb) = value.and_then(|v| v.trim().parse::<f64>().ok()) else {
        return DEFAULT_MAX_FILE_BYTES;
    };
    if !mb.is_finite() || mb < 0.0 {
        DEFAULT_MAX_FILE_BYTES
    } else if mb == 0.0 {
        u64::MAX
    } else {
        // `as` saturates, so a huge value means no limit rather than wrapping.
        ((mb * 1_048_576.0) as u64).max(1)
    }
}

/// Largest source file that is parsed or read during resolution, in bytes. Read once per process.
pub fn max_source_file_bytes() -> u64 {
    static LIMIT: OnceLock<u64> = OnceLock::new();
    *LIMIT.get_or_init(|| parse_max_file_bytes(std::env::var(MAX_FILE_MB_ENV).ok().as_deref()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unset_is_ten_mib() {
        assert_eq!(parse_max_file_bytes(None), 10 * 1024 * 1024);
    }

    #[test]
    fn zero_removes_the_limit() {
        assert_eq!(parse_max_file_bytes(Some("0")), u64::MAX);
        assert_eq!(parse_max_file_bytes(Some(" 0.0 ")), u64::MAX);
    }

    #[test]
    fn a_number_is_mebibytes() {
        assert_eq!(parse_max_file_bytes(Some("2")), 2 * 1024 * 1024);
        assert_eq!(parse_max_file_bytes(Some("0.5")), 512 * 1024);
        assert_eq!(parse_max_file_bytes(Some("1")), 1024 * 1024);
    }

    #[test]
    fn unusable_values_fall_back_to_the_default() {
        for bad in ["", "abc", "-1", "NaN", "inf", "1MB"] {
            assert_eq!(parse_max_file_bytes(Some(bad)), DEFAULT_MAX_FILE_BYTES, "{bad:?}");
        }
    }

    #[test]
    fn a_tiny_positive_value_is_not_zero() {
        assert_eq!(parse_max_file_bytes(Some("0.0000001")), 1);
    }
}
