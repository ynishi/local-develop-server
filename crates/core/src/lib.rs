use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Result};

const DEFAULT_TIMEOUT_SECS: u64 = 60;
const DEFAULT_MAX_OUTPUT: usize = 102_400; // 100KB

pub struct SessionConfig {
    pub root: PathBuf,
    pub timeout_secs: Option<u64>,
    pub max_output: Option<usize>,
}

#[derive(Debug, Clone)]
pub struct Session {
    root: PathBuf,
    session_id: String,
    timeout: Duration,
    max_output: usize,
}

impl Session {
    pub fn new(config: SessionConfig) -> Result<Self> {
        let root = config.root;
        if !root.is_dir() {
            bail!("session root does not exist: {}", root.display());
        }
        let session_id = uuid_v4();
        let timeout = Duration::from_secs(config.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS));
        let max_output = config.max_output.unwrap_or(DEFAULT_MAX_OUTPUT);
        tracing::info!(
            root = %root.display(),
            session_id = %session_id,
            timeout_secs = timeout.as_secs(),
            max_output,
            "session started"
        );
        Ok(Self {
            root,
            session_id,
            timeout,
            max_output,
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn id(&self) -> &str {
        &self.session_id
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    pub fn max_output(&self) -> usize {
        self.max_output
    }
}

#[derive(Debug, Clone)]
pub struct LdsState {
    session: Option<Arc<Session>>,
}

impl LdsState {
    pub fn new() -> Self {
        Self { session: None }
    }

    pub fn start_session(&mut self, config: SessionConfig) -> Result<Arc<Session>> {
        let session = Arc::new(Session::new(config)?);
        self.session = Some(Arc::clone(&session));
        Ok(session)
    }

    pub fn session(&self) -> Result<&Arc<Session>> {
        self.session
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("no active session — call session_start first"))
    }
}

impl Default for LdsState {
    fn default() -> Self {
        Self::new()
    }
}

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

fn uuid_v4() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let pid = std::process::id();
    format!("{ts:x}-{pid:x}")
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
}
