//! Error mapping and progress reporting.
//!
//! The ERROR line uses the Python exception class names so that the message
//! format stays compatible (design spec §3.4, §5).

use std::fmt;
use std::io::Write;
use std::path::Path;

/// Upper bound for the message part of an ERROR line (decision D-LIMIT).
pub const MAX_ERROR_BYTES: usize = 4096;

#[derive(Debug, Clone)]
pub enum JobError {
    Conversion(String),
    Unidentified(String),
    Os(String),
    Permission(String),
    NotFound(String),
    DecompressionBomb(String),
    Memory(String),
    Panic(String),
}

impl JobError {
    pub fn type_name(&self) -> &'static str {
        match self {
            JobError::Conversion(_) => "ConversionError",
            JobError::Unidentified(_) => "UnidentifiedImageError",
            JobError::Os(_) => "OSError",
            JobError::Permission(_) => "PermissionError",
            JobError::NotFound(_) => "FileNotFoundError",
            JobError::DecompressionBomb(_) => "DecompressionBombError",
            JobError::Memory(_) => "MemoryError",
            JobError::Panic(_) => "PanicError",
        }
    }

    pub fn message(&self) -> &str {
        match self {
            JobError::Conversion(message)
            | JobError::Unidentified(message)
            | JobError::Os(message)
            | JobError::Permission(message)
            | JobError::NotFound(message)
            | JobError::DecompressionBomb(message)
            | JobError::Memory(message)
            | JobError::Panic(message) => message,
        }
    }

    pub fn from_io(error: &std::io::Error, path: &Path) -> Self {
        let message = format!("{error}: '{}'", path.display());
        match error.kind() {
            std::io::ErrorKind::NotFound => JobError::NotFound(message),
            std::io::ErrorKind::PermissionDenied => JobError::Permission(message),
            _ => JobError::Os(message),
        }
    }

    pub fn from_image(error: &image::ImageError, path: &Path) -> Self {
        match error {
            image::ImageError::Unsupported(inner) => JobError::Os(format!(
                "cannot identify image file '{}': {inner}",
                path.display()
            )),
            image::ImageError::Decoding(inner) => {
                JobError::Os(format!("{}: {inner}", path.display()))
            }
            other => JobError::Os(format!("{}: {other}", path.display())),
        }
    }
}

impl fmt::Display for JobError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.type_name(), self.message())
    }
}

/// Truncates a message at a character boundary and marks the cut.
pub fn truncate_message(text: &str, limit: usize) -> String {
    if text.len() <= limit {
        return text.to_string();
    }
    const MARKER: &str = "…";
    let budget = limit.saturating_sub(MARKER.len());
    let mut end = budget.min(text.len());
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}{MARKER}", &text[..end])
}

#[derive(Debug, Default)]
pub struct Reporter {
    stdout_failed: bool,
    stderr_failed: bool,
}

impl Reporter {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn out(&mut self, line: &str) {
        if self.stdout_failed {
            return;
        }
        let stdout = std::io::stdout();
        let mut handle = stdout.lock();
        if writeln!(handle, "{line}").is_err() {
            self.stdout_failed = true;
        }
    }

    pub fn err(&mut self, line: &str) {
        if self.stderr_failed {
            return;
        }
        let stderr = std::io::stderr();
        let mut handle = stderr.lock();
        if writeln!(handle, "{line}").is_err() {
            self.stderr_failed = true;
        }
    }

    pub fn warn(&mut self, relative: &str, message: &str) {
        self.err(&format!("WARN {relative}: {message}"));
    }

    pub fn error(&mut self, relative: &str, error: &JobError) {
        let message = truncate_message(error.message(), MAX_ERROR_BYTES);
        self.err(&format!(
            "ERROR {relative}: {}: {message}",
            error.type_name()
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncation_respects_char_boundaries() {
        let text = "日本語のメッセージ";
        let shortened = truncate_message(text, 10);
        assert!(shortened.len() <= 10);
        assert!(shortened.ends_with('…'));
        assert!(std::str::from_utf8(shortened.as_bytes()).is_ok());
    }
}
