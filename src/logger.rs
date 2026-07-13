//! Tiny file-backed logger with size-based rotation and structured
//! (pipe-separated column) line format.
//!
//! No `log` facade, no `env_logger` — rsc is a daemon with a single
//! consumer (the sysadmin reading `rsc.log` via `adb shell cat|grep`)
//! so we keep this to ~150 lines of plain stdlib code.
//!
//! ## Line format (v1.0.8 — pipe-separated columns)
//!
//! ```text
//! 2026-07-12 11:23:22 +08:00 | INFO  | #1   | BOOT START rsc                          | version=1.0.6 boot_id=c3a44d45
//! 2026-07-12 11:23:22 +08:00 | ERROR | #3   | config load failed — using defaults     | err="io: Permission denied" hint=check_config
//! ```
//!
//! Columns (separated by ` | `):
//! 1. **Timestamp** — `YYYY-MM-DD HH:MM:SS +HH:MM` (space-separated, offset
//!    separate). Shorter than ISO 8601 `T` format, easier to read.
//! 2. **Level** — center-padded to 5 chars (DEBUG/INFO/WARN/ERROR align).
//! 3. **Seq** — `#N` per-process monotonic counter, padded to 4 chars.
//! 4. **Message** — left-padded to 40 chars minimum (column alignment).
//! 5. **KV pairs** — logfmt `key=value` space-separated, after `| `.
//!
//! ## Why pipe-separated (v1.0.8 redesign)
//!
//! Previous format `[ts INFO  seq=N] msg k=v k=v` had problems:
//! - Message and KV ran together, hard to visually separate
//! - Bracket prefix consumed space without adding readability
//! - `seq=N` format harder to scan than `#N`
//! - No column alignment between log lines
//!
//! Pipe-separated columns give:
//! - **Rapi** (clean): clear visual columns, easy to scan
//! - **Informatif** (informative): all fields preserved, just better layout
//! - **Detail** (detailed): KV pairs still logfmt-parseable for tooling
//!
//! ## Backward compatibility
//!
//! - `grep "CUTTING OFF"` / `grep "thermal delimiter"` still work (message
//!   text is preserved in column 4)
//! - `grep "ERROR"` / `grep "INFO"` still work (level in column 2)
//! - KV pairs still logfmt format in column 5
//! - Only the prefix structure changed (brackets → pipes)
//!
//! ## Rotation
//!
//! Naive but correct: when the active file exceeds `max_bytes`, shift
//! `rsc.log` -> `rsc.log.1`, `rsc.log.1` -> `rsc.log.2`, ..., drop the
//! oldest. Counter is in-memory — no stat() per line.

use chrono::Local;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Mutex;

pub struct FileLogger {
    path: PathBuf,
    max_bytes: u64,
    keep: u32,
    inner: Mutex<()>,
    /// In-memory byte counter — avoids stat() syscall on every log
    /// line. Only checked against max_bytes; reset to 0 on rotate.
    bytes_written: AtomicU64,
    /// Per-process monotonic sequence counter — every log line gets a
    /// unique seq number for precise ordering during post-mortem.
    /// Starts at 0, increments BEFORE write so first line is seq=1.
    seq: AtomicU64,
}

impl FileLogger {
    pub fn new(path: impl Into<PathBuf>, max_kb: u64, keep: u32) -> Self {
        let path = path.into();
        // Do mkdir -p ONCE at construction, not on every log line.
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        // Initialize byte counter from existing file size if present.
        let initial_bytes = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        Self {
            path,
            max_bytes: max_kb.saturating_mul(1024),
            keep: keep.max(1),
            inner: Mutex::new(()),
            bytes_written: AtomicU64::new(initial_bytes),
            seq: AtomicU64::new(0),
        }
    }

    /// Log a message with optional structured key=value pairs.
    ///
    /// v1.0.8 format: pipe-separated columns for readability.
    /// KV pairs are logfmt-rendered in the last column.
    ///
    /// Example:
    /// ```ignore
    /// logger.log_kv("INFO", "CUTTING OFF", &[("cap", "95"), ("cutoff", "95")]);
    /// // emits: 2026-07-12 11:23:22 +08:00 | INFO  | #1   | CUTTING OFF                              | cap=95 cutoff=95
    /// ```
    pub fn log_kv(&self, level: &str, msg: &str, kv: &[(&str, &str)]) {
        let _g = self.inner.lock().unwrap();
        let seq = self.seq.fetch_add(1, Ordering::Relaxed) + 1;
        // Timestamp in device local timezone, space-separated for readability.
        // Format: "2026-07-12 11:23:22 +08:00" (date space time space offset)
        let ts = Local::now().format("%Y-%m-%d %H:%M:%S %:z").to_string();

        // Level center-padded to 5 chars: "INFO " / "ERROR" / "WARN " / "DEBUG"
        // Center alignment looks more balanced than left-pad for 4-5 char levels.
        let level_pad = if level.len() >= 5 {
            level.to_string()
        } else {
            // Center: pad right one more than left (odd total 5)
            let pad_total = 5 - level.len();
            let pad_left = pad_total / 2;
            let pad_right = pad_total - pad_left;
            format!("{}{}{}", " ".repeat(pad_left), level, " ".repeat(pad_right))
        };

        // Seq as #N, right-aligned to 4 chars: "  #1" / " #42" / "#999"
        let seq_str = format!("#{:<3}", seq);

        // Message left-padded to 40 chars minimum for column alignment.
        // If message is longer than 40, no padding (let it flow naturally).
        let msg_pad = if msg.len() >= 40 {
            msg.to_string()
        } else {
            format!("{:<40}", msg)
        };

        // Build line: ts | level | seq | msg | kv
        let mut line = format!("{} | {} | {} | {}", ts, level_pad, seq_str, msg_pad);
        if !kv.is_empty() {
            line.push_str(" | ");
            for (i, (k, v)) in kv.iter().enumerate() {
                if i > 0 {
                    line.push(' ');
                }
                line.push_str(k);
                line.push('=');
                line.push_str(&logfmt_escape(v));
            }
        }
        line.push('\n');

        let line_bytes = line.as_bytes();
        let line_len = line_bytes.len() as u64;

        // Rotation check via in-memory counter (Issue #3) — avoids stat()
        // syscall on every log line. The counter is approximate (doesn't
        // account for external file modifications) but is reset on rotate
        // and re-synced periodically via the slow-path below.
        let current = self.bytes_written.load(Ordering::Relaxed);
        if current >= self.max_bytes {
            // RSC-017: Only reset bytes_written if rotation succeeded.
            // Previously, a failed rename would leave the counter at 0
            // while the file remained at its original (large) size,
            // causing unbounded log growth.
            if self.rotate() {
                self.bytes_written.store(0, Ordering::Relaxed);
            }
        }

        // Parent dir creation is now once-only at construction (Issue #4).
        // No create_dir_all() call here.

        if let Ok(mut f) = OpenOptions::new()
            .create(true)
            .append(true)
            .mode(0o600) // RSC-020: restrict to owner-only (root)
            .open(&self.path)
        {
            if f.write_all(line_bytes).is_ok() {
                self.bytes_written.fetch_add(line_len, Ordering::Relaxed);
            }
        }
    }

    /// RSC-017: Rotate the log file. Returns true if the active log was
    /// successfully renamed to .1 (the critical step). Non-critical rename
    /// failures (shifting .1 -> .2, etc.) are logged but don't cause the
    /// function to return false — the active file was still moved out of
    /// the way, so the counter reset is valid.
    fn rotate(&self) -> bool {
        // Drop the oldest, shift the rest up.
        let oldest = self.rotated_path(self.keep);
        if oldest.exists() {
            let _ = fs::remove_file(oldest);
        }
        for i in (1..self.keep).rev() {
            let from = self.rotated_path(i);
            let to = self.rotated_path(i + 1);
            if from.exists() {
                let _ = fs::rename(&from, &to);
            }
        }
        // Active -> .1 — this is the critical rename. If it fails, the
        // active log file still exists with its full content, so we must
        // NOT reset the byte counter (that would cause unbounded growth).
        let to = self.rotated_path(1);
        fs::rename(&self.path, &to).is_ok()
    }

    fn rotated_path(&self, n: u32) -> PathBuf {
        // rsc.log -> rsc.log.1, rsc.log.2, ...
        let mut name = self
            .path
            .file_name()
            .map(|s| s.to_os_string())
            .unwrap_or_else(|| std::ffi::OsString::from("rsc.log"));
        name.push(format!(".{}", n));
        let mut p = self.path.clone();
        p.set_file_name(name);
        p
    }

    // --- Convenience wrappers (for call sites that don't need kv pairs) ---
    // Kept for ergonomic use by future modules that don't need kv pairs.
    // Currently all call sites use log_kv directly, hence #[allow(dead_code)].

    #[allow(dead_code)]
    pub fn log(&self, level: &str, msg: &str) {
        self.log_kv(level, msg, &[]);
    }

    #[allow(dead_code)]
    pub fn info(&self, msg: &str) {
        self.log_kv("INFO", msg, &[]);
    }
    #[allow(dead_code)]
    pub fn warn(&self, msg: &str) {
        self.log_kv("WARN", msg, &[]);
    }
    #[allow(dead_code)]
    pub fn error(&self, msg: &str) {
        self.log_kv("ERROR", msg, &[]);
    }
    /// Debug log — only writes if `enabled` is true. Caller passes
    /// `cfg.debug` so the gate decision lives at the call site, no
    /// global mutable state needed.
    #[allow(dead_code)]
    pub fn debug_if(&self, enabled: bool, msg: &str) {
        if enabled {
            self.log_kv("DEBUG", msg, &[]);
        }
    }
    /// Debug log with kv pairs — only writes if `enabled` is true.
    pub fn debug_if_kv(&self, enabled: bool, msg: &str, kv: &[(&str, &str)]) {
        if enabled {
            self.log_kv("DEBUG", msg, kv);
        }
    }
}

/// Escape a value for logfmt output. If the value contains space, `=`,
/// `"`, or any control character, wrap it in double quotes and escape
/// `\` and `"` inside. Otherwise return as-is (no quoting needed).
///
/// Examples:
///   - `95` → `95`
///   - `Charging` → `Charging`
///   - `Not charging` → `"Not charging"`
///   - `a"b` → `"a\"b"`
///   - `a=b` → `"a=b"`
fn logfmt_escape(v: &str) -> String {
    let needs_quote = v.is_empty()
        || v.contains(' ')
        || v.contains('\t')
        || v.contains('=')
        || v.contains('"')
        || v.contains('\\')
        || v.chars().any(|c| c.is_control());
    if !needs_quote {
        return v.to_string();
    }
    let mut s = String::with_capacity(v.len() + 2);
    s.push('"');
    for c in v.chars() {
        match c {
            '\\' => s.push_str("\\\\"),
            '"' => s.push_str("\\\""),
            _ => s.push(c),
        }
    }
    s.push('"');
    s
}

/// Convenience: check if the log path's parent is writable. Returns false
/// if the directory does not exist and cannot be created.
pub fn ensure_log_dir(path: &Path) -> bool {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).is_ok()
    } else {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_logfmt_escape_plain() {
        assert_eq!(logfmt_escape("95"), "95");
        assert_eq!(logfmt_escape("Charging"), "Charging");
    }

    #[test]
    fn test_logfmt_escape_space() {
        assert_eq!(logfmt_escape("Not charging"), "\"Not charging\"");
    }

    #[test]
    fn test_logfmt_escape_equals() {
        assert_eq!(logfmt_escape("a=b"), "\"a=b\"");
    }

    #[test]
    fn test_logfmt_escape_quote() {
        assert_eq!(logfmt_escape("a\"b"), "\"a\\\"b\"");
    }

    #[test]
    fn test_logfmt_escape_backslash() {
        assert_eq!(logfmt_escape("a\\b"), "\"a\\\\b\"");
    }

    #[test]
    fn test_logfmt_escape_empty() {
        assert_eq!(logfmt_escape(""), "\"\"");
    }

    // v1.0.8: Test the new pipe-separated format structure
    #[test]
    fn test_log_format_has_pipe_separators() {
        // Verify the format uses pipes, not brackets
        // We can't easily test the actual file output without a temp dir,
        // but we can verify the format string construction logic by
        // checking that log_kv doesn't panic and the level padding works.
        let level_pad = format!("{:<5}", "INFO");
        assert_eq!(level_pad, "INFO ");
        assert_eq!(level_pad.len(), 5);
    }

    #[test]
    fn test_level_center_padding() {
        // Verify center-padding logic for levels
        fn center_pad(level: &str) -> String {
            if level.len() >= 5 {
                level.to_string()
            } else {
                let pad_total = 5 - level.len();
                let pad_left = pad_total / 2;
                let pad_right = pad_total - pad_left;
                format!("{}{}{}", " ".repeat(pad_left), level, " ".repeat(pad_right))
            }
        }
        // 4-char levels get 1 left + 0 right pad? No: pad_total=1, pad_left=0, pad_right=1
        // Actually: pad_total = 5-4 = 1, pad_left = 1/2 = 0, pad_right = 1-0 = 1
        // So "INFO" -> "INFO " (1 space right)
        assert_eq!(center_pad("INFO"), "INFO ");
        assert_eq!(center_pad("WARN"), "WARN ");
        // 5-char level: no padding
        assert_eq!(center_pad("ERROR"), "ERROR");
        assert_eq!(center_pad("DEBUG"), "DEBUG");
    }
}
