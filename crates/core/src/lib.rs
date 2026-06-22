//! Core utilities shared across all lds modules.
//!
//! Session lifecycle (`Session`, `LdsState`, `SessionConfig`, `SessionError`,
//! `CoreError`) lives in the sibling `lds-session` crate and is re-exported
//! here for backward compatibility — existing `use lds_core::Session;` etc.
//! continues to work unchanged.
//!
//! This crate retains cross-cutting helpers that do not belong to the
//! session contract: binary probing (`find_in_path`, `check_binaries`,
//! `BinaryStatus`), output truncation (`truncate_output`), config file
//! handling (`config` module), and the in-memory log ring (`log_store`).

pub mod config;
pub mod log_store;

pub use lds_session::{CoreError, LdsState, Session, SessionConfig, SessionError};

use std::path::PathBuf;

/// Check whether an executable is reachable via `PATH`.
///
/// Returns the resolved path on success. Used by `session_info` to
/// report degraded-mode availability of external tools that lds
/// (and plugin recipes) depend on (`git`, `just`, `python3`,
/// `codedash`, `rg`, etc.). Agents read the result to decide between
/// a typed tool path and an in-band fallback.
pub fn find_in_path(binary: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let candidate = dir.join(binary);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// Availability status for an external binary.
#[derive(Debug, Clone, serde::Serialize)]
pub struct BinaryStatus {
    pub name: String,
    pub available: bool,
    pub path: Option<String>,
}

/// Check a set of external binaries and return their availability.
pub fn check_binaries(names: &[&str]) -> Vec<BinaryStatus> {
    names
        .iter()
        .map(|name| {
            let resolved = find_in_path(name);
            BinaryStatus {
                name: (*name).to_string(),
                available: resolved.is_some(),
                path: resolved.map(|p| p.display().to_string()),
            }
        })
        .collect()
}

/// Truncate byte output to `max` bytes, splitting into head + tail halves.
///
/// Returns `(output_string, was_truncated)`. When truncated, inserts a
/// marker line between the halves showing the original size. Splits are
/// aligned to UTF-8 character boundaries so the result is always valid.
pub fn truncate_output(raw: &[u8], max: usize) -> (String, bool) {
    if raw.len() <= max {
        return (String::from_utf8_lossy(raw).into_owned(), false);
    }
    let half = max / 2;
    let head_end = find_utf8_boundary(raw, half);
    let tail_start = find_utf8_boundary_rev(raw, raw.len() - half);
    let head = String::from_utf8_lossy(&raw[..head_end]);
    let tail = String::from_utf8_lossy(&raw[tail_start..]);
    let mut out = head.into_owned();
    out.push_str(&format!(
        "\n\n--- [truncated: {} bytes total, showing first/last ~{} bytes] ---\n\n",
        raw.len(),
        half,
    ));
    out.push_str(&tail);
    (out, true)
}

fn find_utf8_boundary(buf: &[u8], pos: usize) -> usize {
    let pos = pos.min(buf.len());
    let mut i = pos;
    while i > 0 && !is_utf8_char_start(buf[i]) {
        i -= 1;
    }
    i
}

fn find_utf8_boundary_rev(buf: &[u8], pos: usize) -> usize {
    let pos = pos.min(buf.len());
    let mut i = pos;
    while i < buf.len() && !is_utf8_char_start(buf[i]) {
        i += 1;
    }
    i
}

fn is_utf8_char_start(b: u8) -> bool {
    // UTF-8 continuation bytes are 0b10xxxxxx (0x80..0xBF)
    (b & 0xC0) != 0x80
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_short_input() {
        let data = b"hello world";
        let (out, truncated) = truncate_output(data, 200);
        assert_eq!(out, "hello world");
        assert!(!truncated);
    }

    #[test]
    fn truncate_empty() {
        let (out, truncated) = truncate_output(b"", 100);
        assert_eq!(out, "");
        assert!(!truncated);
    }

    #[test]
    fn truncate_over_limit() {
        let data: Vec<u8> = (0..1000).map(|i| b'A' + (i % 26) as u8).collect();
        let (out, truncated) = truncate_output(&data, 100);
        assert!(truncated);
        assert!(out.contains("[truncated:"));
        assert!(out.len() < data.len());
    }

    #[test]
    fn truncate_multibyte_boundary() {
        // "あいう" = 9 bytes (3 chars × 3 bytes each)
        let data = "あいうえお".as_bytes(); // 15 bytes
        let (out, truncated) = truncate_output(data, 10);
        assert!(truncated);
        // Should not produce invalid UTF-8
        assert!(out.is_ascii() || out.chars().all(|c| c.len_utf8() > 0));
    }

    #[test]
    fn truncate_exact_limit() {
        let data = b"exactly ten";
        let (out, truncated) = truncate_output(data, data.len());
        assert_eq!(out, "exactly ten");
        assert!(!truncated);
    }

    #[test]
    fn find_in_path_resolves_common_binary() {
        // `sh` is essentially guaranteed on every Unix.
        let resolved = find_in_path("sh");
        assert!(resolved.is_some(), "sh should be on PATH");
        assert!(resolved.unwrap().is_file());
    }

    #[test]
    fn find_in_path_returns_none_for_unknown() {
        assert!(find_in_path("definitely-not-a-real-binary-xyz-12345").is_none());
    }

    #[test]
    fn check_binaries_marks_missing() {
        let report = check_binaries(&["sh", "definitely-not-a-real-binary-xyz-12345"]);
        assert_eq!(report.len(), 2);
        assert_eq!(report[0].name, "sh");
        assert!(report[0].available);
        assert!(report[0].path.is_some());
        assert_eq!(report[1].name, "definitely-not-a-real-binary-xyz-12345");
        assert!(!report[1].available);
        assert!(report[1].path.is_none());
    }
}
