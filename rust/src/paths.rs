//! Path helpers that reproduce the Python implementation's semantics.
//!
//! The Python version relies on `pathlib.Path.suffix` / `with_suffix` /
//! `with_name` and on `str.casefold()` for collision keys. Those are not the
//! same as the naive Rust equivalents, so they are reimplemented here.

use std::path::{Component, Path, PathBuf};

/// Python's `PurePath.suffix`.
pub fn suffix(name: &str) -> &str {
    match name.rfind('.') {
        Some(i) if i > 0 && i < name.len() - 1 => &name[i..],
        _ => "",
    }
}

/// Python's `PurePath.stem`.
pub fn stem(name: &str) -> &str {
    match name.rfind('.') {
        Some(i) if i > 0 && i < name.len() - 1 => &name[..i],
        _ => name,
    }
}

/// Python's `PurePath.with_suffix`.
pub fn with_suffix(name: &str, new_suffix: &str) -> String {
    let base = match name.rfind('.') {
        Some(i) if i > 0 && i < name.len() - 1 => &name[..i],
        _ => name,
    };
    format!("{base}{new_suffix}")
}

/// Replaces the final path component while keeping the parent, like
/// `PurePath.with_name`.
pub fn with_name(path: &Path, name: &str) -> PathBuf {
    let mut out = match path.parent() {
        Some(parent) => parent.to_path_buf(),
        None => PathBuf::new(),
    };
    out.push(name);
    out
}

/// Python's `PurePath.with_suffix` on a path.
pub fn path_with_suffix(path: &Path, new_suffix: &str) -> PathBuf {
    let Some(name) = path.file_name() else {
        return path.to_path_buf();
    };
    let name = name.to_string_lossy();
    with_name(path, &with_suffix(&name, new_suffix))
}

/// Python's `PurePath.stem` on a path.
pub fn path_stem(path: &Path) -> String {
    match path.file_name() {
        Some(name) => stem(&name.to_string_lossy()).to_string(),
        None => String::new(),
    }
}

/// Python's `PurePath.suffix` on a path (empty when there is no file name).
pub fn path_suffix(path: &Path) -> String {
    match path.file_name() {
        Some(name) => suffix(&name.to_string_lossy()).to_string(),
        None => String::new(),
    }
}

/// Approximates Python's `str.casefold()`.
///
/// The mapping covers the deviations from `str::to_lowercase` that can occur in
/// file names; the remaining Unicode `F` (full folding) entries are listed in
/// the Phase 0 record and are not reachable through lowercasing either, so the
/// difference is documented rather than silently ignored.
pub fn casefold(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for ch in input.chars() {
        match ch {
            '\u{00DF}' | '\u{1E9E}' => out.push_str("ss"),
            '\u{03C2}' => out.push('\u{03C3}'),
            '\u{0345}' => out.push('\u{03B9}'),
            '\u{0149}' => out.push_str("\u{02BC}n"),
            '\u{01F0}' => out.push_str("j\u{030C}"),
            '\u{0390}' => out.push_str("\u{03B9}\u{0308}\u{0301}"),
            '\u{03B0}' => out.push_str("\u{03C5}\u{0308}\u{0301}"),
            '\u{1E96}' => out.push_str("h\u{0331}"),
            '\u{1E97}' => out.push_str("t\u{0308}"),
            '\u{1E98}' => out.push_str("w\u{030A}"),
            '\u{1E99}' => out.push_str("y\u{030A}"),
            '\u{1E9A}' => out.push_str("a\u{02BE}"),
            '\u{1FBE}' => out.push('\u{03B9}'),
            '\u{0587}' => out.push_str("\u{0565}\u{0582}"),
            '\u{FB00}' => out.push_str("ff"),
            '\u{FB01}' => out.push_str("fi"),
            '\u{FB02}' => out.push_str("fl"),
            '\u{FB03}' => out.push_str("ffi"),
            '\u{FB04}' => out.push_str("ffl"),
            '\u{FB05}' => out.push_str("st"),
            '\u{FB06}' => out.push_str("st"),
            '\u{FB13}' => out.push_str("\u{0574}\u{0576}"),
            '\u{FB14}' => out.push_str("\u{0574}\u{0565}"),
            '\u{FB15}' => out.push_str("\u{0574}\u{056B}"),
            '\u{FB16}' => out.push_str("\u{057E}\u{0576}"),
            '\u{FB17}' => out.push_str("\u{0574}\u{056D}"),
            _ => {
                let mut buf = [0u8; 4];
                let lowered = ch.to_lowercase();
                if lowered.len() == 0 {
                    out.push_str(ch.encode_utf8(&mut buf));
                } else {
                    for c in lowered {
                        out.push(c);
                    }
                }
            }
        }
    }
    out
}

/// Windows compares paths case-insensitively using a simple lowercase mapping
/// (the equivalent of `os.path.normcase`), not Unicode case folding.
pub fn normalize_for_compare(path: &Path) -> String {
    let text = path.to_string_lossy().replace('/', "\\");
    if cfg!(windows) {
        text.to_lowercase()
    } else {
        text
    }
}

/// POSIX-style relative path, as stored in the state database.
pub fn to_posix(path: &Path) -> String {
    let mut out = String::new();
    for (index, component) in path.components().enumerate() {
        if index > 0 {
            out.push('/');
        }
        out.push_str(&component.as_os_str().to_string_lossy());
    }
    out
}

/// `Path.parents`-style check on whole path elements (never string prefixes).
pub fn is_same_or_child(parent: &Path, child: &Path) -> bool {
    let parent = normalize_for_compare(parent);
    let child = normalize_for_compare(child);
    if parent == child {
        return true;
    }
    let mut child_components: Vec<String> = child
        .split('\\')
        .filter(|part| !part.is_empty())
        .map(|part| part.to_string())
        .collect();
    let parent_components: Vec<String> = parent
        .split('\\')
        .filter(|part| !part.is_empty())
        .map(|part| part.to_string())
        .collect();
    if parent_components.is_empty() || parent_components.len() > child_components.len() {
        return false;
    }
    // Preserve the leading empty component that a UNC path starts with.
    if child.starts_with('\\') {
        child_components.insert(0, String::new());
    }
    let mut parent_components = parent_components;
    if parent.starts_with('\\') && !parent_components.is_empty() {
        parent_components.insert(0, String::new());
    }
    if child_components.len() < parent_components.len() {
        return false;
    }
    parent_components
        .iter()
        .zip(child_components.iter())
        .all(|(a, b)| a == b)
}

/// Windows verbatim paths (`\\?\C:\...`) are rewritten to their normal form.
pub fn strip_verbatim(path: &Path) -> PathBuf {
    let text = path.to_string_lossy().to_string();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        // `\\?\C:\...` -> `C:\...`
        return PathBuf::from(rest);
    }
    path.to_path_buf()
}

/// Lexically normalizes `.` and `..` without touching the file system.
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push("..");
                }
            }
            other => out.push(other),
        }
    }
    out
}

/// Equivalent of Python's `Path.resolve()` (non-strict): resolve the deepest
/// existing ancestor, then attach the remaining components lexically.
pub fn resolve_non_strict(path: &Path) -> PathBuf {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        std::env::current_dir()
            .unwrap_or_else(|_| PathBuf::from("."))
            .join(path)
    };
    let mut existing = absolute.clone();
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    loop {
        if let Ok(canonical) = std::fs::canonicalize(&existing) {
            let mut out = strip_verbatim(&canonical);
            for part in tail.iter().rev() {
                out.push(part);
            }
            return lexical_normalize(&strip_verbatim(&out));
        }
        match (
            existing.file_name().map(|n| n.to_os_string()),
            existing.parent(),
        ) {
            (Some(name), Some(parent)) => {
                tail.push(name);
                existing = parent.to_path_buf();
            }
            _ => break,
        }
    }
    lexical_normalize(&absolute)
}

/// Joins a relative config path against the project directory and resolves it.
pub fn resolve_against(base: &Path, raw: &str) -> PathBuf {
    let candidate = Path::new(raw);
    let joined = if candidate.is_absolute() {
        candidate.to_path_buf()
    } else {
        base.join(candidate)
    };
    resolve_non_strict(&joined)
}

/// Renders a path with the platform separator, as the progress output does.
pub fn display(path: &Path) -> String {
    path.to_string_lossy().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suffix_matches_python() {
        assert_eq!(suffix(".png"), "");
        assert_eq!(suffix("file."), "");
        assert_eq!(suffix("a.b.png"), ".png");
        assert_eq!(suffix("file"), "");
        assert_eq!(stem(".png"), ".png");
        assert_eq!(stem("file."), "file.");
        assert_eq!(stem("a.b.png"), "a.b");
        assert_eq!(with_suffix(".png", ".webp"), ".png.webp");
        assert_eq!(with_suffix("file.", ".webp"), "file..webp");
        assert_eq!(with_suffix("a.PNG", ".webp"), "a.webp");
    }

    #[test]
    fn casefold_covers_known_deviations() {
        assert_eq!(casefold("STRASSE"), "strasse");
        assert_eq!(casefold("Straße"), "strasse");
        assert_eq!(casefold("ΣΊΣΥΦΟΣ"), "σίσυφοσ");
        assert_eq!(casefold("ﬁle"), "file");
    }

    #[test]
    fn sibling_folder_is_not_a_child() {
        assert!(is_same_or_child(Path::new("/a/b"), Path::new("/a/b/c")));
        assert!(!is_same_or_child(Path::new("/a/b"), Path::new("/a/bc")));
        assert!(is_same_or_child(Path::new("/a/b"), Path::new("/a/b")));
    }
}
