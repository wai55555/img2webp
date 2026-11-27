//! `config.ini` parsing and validation.
//!
//! The Python version uses `configparser`; the accepted syntax (comment lines,
//! continuation lines, `=`/`:` separators, section headers with trailing text,
//! no interpolation, no inline comments) is reproduced here together with the
//! validation order and messages (design spec §1.3, §3.2).

use std::collections::BTreeSet;
use std::fmt;
use std::path::{Path, PathBuf};

use crate::paths;

pub const DEFAULT_METHOD_STILL: i64 = 4;
pub const DEFAULT_METHOD_ANIMATION: i64 = 0;

#[derive(Debug)]
pub struct ConfigError(pub String);

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for ConfigError {}

#[derive(Debug, Clone)]
pub struct Config {
    pub input_dir: PathBuf,
    pub output_dir: PathBuf,
    pub quality: i64,
    pub missing_duration_ms: i64,
    pub copy_jpeg: bool,
    pub copy_avif: bool,
    pub copy_webp: bool,
    pub copy_other_files: bool,
    pub overwrite_existing: bool,
    pub reuse_unchanged: bool,
    pub adopt_untracked: bool,
    pub db_commit_every: i64,
    pub preserve_metadata: bool,
    pub preserve_timestamps: bool,
    // New, optional keys (design spec §7 item 1 and 2).
    pub lossless: bool,
    pub method: Option<i64>,
    pub exact: bool,
    pub parallel_jobs: Option<i64>,
}

impl Config {
    /// Effective libwebp `method` for still images.
    pub fn method_still(&self) -> i64 {
        self.method.unwrap_or(DEFAULT_METHOD_STILL)
    }

    /// Effective libwebp `method` for animations (Pillow's sequence default).
    pub fn method_animation(&self) -> i64 {
        self.method.unwrap_or(DEFAULT_METHOD_ANIMATION)
    }
}

const REQUIRED_KEYS: &[(&str, &[&str])] = &[
    ("paths", &["input_dir", "output_dir"]),
    ("webp", &["quality"]),
    ("animation", &["missing_duration_ms"]),
    ("copy", &["jpeg", "avif", "webp", "other_files"]),
    (
        "output",
        &[
            "overwrite_existing",
            "reuse_unchanged",
            "adopt_untracked",
            "db_commit_every",
        ],
    ),
    ("preserve", &["metadata", "timestamps"]),
];

const OPTIONAL_KEYS: &[(&str, &[&str])] = &[
    ("webp", &["lossless", "method", "exact"]),
    ("output", &["parallel_jobs"]),
];

#[derive(Debug, Default)]
struct Ini {
    sections: Vec<(String, Vec<(String, String)>)>,
    defaults: Vec<(String, String)>,
}

impl Ini {
    fn section(&self, name: &str) -> Option<&Vec<(String, String)>> {
        self.sections
            .iter()
            .find(|(section, _)| section == name)
            .map(|(_, entries)| entries)
    }

    fn value(&self, section: &str, key: &str) -> Option<&str> {
        self.section(section)?
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| value.as_str())
    }
}

fn parse_error(line: usize, detail: &str) -> ConfigError {
    ConfigError(format!("config.ini を読めません: {detail} (行 {line})"))
}

fn parse(text: &str) -> Result<Ini, ConfigError> {
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    let mut ini = Ini::default();
    let mut current: Option<usize> = None;
    let mut current_key: Option<String> = None;
    let mut indent_level = 0usize;

    for (index, raw_line) in normalized.split('\n').enumerate() {
        let lineno = index + 1;
        let stripped = raw_line.trim_start();
        if stripped.starts_with('#') || stripped.starts_with(';') {
            // Full-line comments never affect the continuation state.
            continue;
        }
        let value = raw_line.trim();
        if value.is_empty() {
            continue;
        }
        let first_nonspace = raw_line
            .char_indices()
            .find(|(_, c)| !c.is_whitespace())
            .map(|(i, _)| i)
            .unwrap_or(0);

        let is_continuation = current_key.is_some() && first_nonspace > indent_level;
        if is_continuation {
            let key = current_key.clone().unwrap_or_default();
            if let Some(section) = current.and_then(|i| ini.sections.get_mut(i)) {
                if let Some((_, stored)) = section.1.iter_mut().find(|(name, _)| *name == key) {
                    if !stored.is_empty() {
                        stored.push('\n');
                    }
                    stored.push_str(value);
                }
            }
            continue;
        }

        indent_level = first_nonspace;

        if value.starts_with('[') {
            let Some(close) = value.rfind(']') else {
                return Err(parse_error(lineno, "セクション見出しが閉じていません"));
            };
            if close == 0 {
                return Err(parse_error(lineno, "セクション見出しが空です"));
            }
            let name = value[1..close].to_string();
            if name == "DEFAULT" {
                // Keys that follow land in the defaults map.
                ini.sections.push((name, Vec::new()));
                current = Some(ini.sections.len() - 1);
                current_key = None;
                continue;
            }
            if ini.sections.iter().any(|(existing, _)| *existing == name) {
                return Err(parse_error(
                    lineno,
                    &format!("セクション [{name}] が重複しています"),
                ));
            }
            ini.sections.push((name, Vec::new()));
            current = Some(ini.sections.len() - 1);
            current_key = None;
            continue;
        }

        let Some(section_index) = current else {
            return Err(parse_error(lineno, "セクション見出しのない行があります"));
        };
        let Some(separator) = value.find(['=', ':']) else {
            return Err(parse_error(
                lineno,
                "キーの区切り (= または :) がありません",
            ));
        };
        let option = value[..separator].trim_end().to_lowercase();
        if option.is_empty() {
            return Err(parse_error(lineno, "キー名が空です"));
        }
        let mut option_value = value[separator + 1..].trim().to_string();
        if option_value == "\"\"" {
            option_value.clear();
        }
        let is_default = ini.sections[section_index].0 == "DEFAULT";
        let bucket = if is_default {
            &mut ini.defaults
        } else {
            &mut ini.sections[section_index].1
        };
        if bucket.iter().any(|(name, _)| *name == option) {
            return Err(parse_error(
                lineno,
                &format!("キー {option} が重複しています"),
            ));
        }
        bucket.push((option.clone(), option_value));
        current_key = Some(option);
    }
    Ok(ini)
}

/// Parses an integer the way the Rust port accepts it: optional sign, decimal
/// ASCII digits, surrounding whitespace. Underscores and non-ASCII digits are
/// rejected on purpose (design spec §5, intentional change 12).
fn parse_int(text: &str) -> Option<i128> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let (sign, digits) = match trimmed.as_bytes()[0] {
        b'+' => (1i128, &trimmed[1..]),
        b'-' => (-1i128, &trimmed[1..]),
        _ => (1i128, trimmed),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut value: i128 = 0;
    for byte in digits.bytes() {
        value = value.checked_mul(10)?.checked_add((byte - b'0') as i128)?;
    }
    Some(sign * value)
}

fn parse_bool(text: &str) -> Option<bool> {
    match text.to_lowercase().as_str() {
        "1" | "yes" | "true" | "on" => Some(true),
        "0" | "no" | "false" | "off" => Some(false),
        _ => None,
    }
}

pub fn load(path: &Path, project_dir: &Path) -> Result<Config, ConfigError> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(_) => {
            return Err(ConfigError(format!(
                "設定ファイルがありません: {}",
                path.display()
            )))
        }
    };
    let text = String::from_utf8(bytes)
        .map_err(|err| ConfigError(format!("config.ini を読めません: {err}")))?;
    let ini = parse(&text)?;

    let allowed_keys = |name: &str| -> Vec<&str> {
        let mut keys: Vec<&str> = Vec::new();
        for (section, list) in REQUIRED_KEYS.iter().chain(OPTIONAL_KEYS.iter()) {
            if *section == name {
                keys.extend(list.iter().copied());
            }
        }
        keys
    };

    let mut unknown_sections: BTreeSet<&str> = BTreeSet::new();
    for (name, _) in ini.sections.iter() {
        if name == "DEFAULT" {
            continue;
        }
        if !REQUIRED_KEYS.iter().any(|(section, _)| section == name) {
            unknown_sections.insert(name.as_str());
        }
    }
    if !unknown_sections.is_empty() || !ini.defaults.is_empty() {
        let detail = if unknown_sections.is_empty() {
            "DEFAULT".to_string()
        } else {
            unknown_sections
                .iter()
                .map(|s| s.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        };
        return Err(ConfigError(format!("不明な設定セクション: {detail}")));
    }

    for (section, required) in REQUIRED_KEYS {
        let Some(entries) = ini.section(section) else {
            return Err(ConfigError(format!(
                "設定セクションがありません: [{section}]"
            )));
        };
        let found: BTreeSet<&str> = entries.iter().map(|(name, _)| name.as_str()).collect();
        let allowed = allowed_keys(section);
        let missing: Vec<&str> = required
            .iter()
            .copied()
            .filter(|key| !found.contains(key))
            .collect();
        let unknown: Vec<&str> = found
            .iter()
            .copied()
            .filter(|key| !allowed.contains(key))
            .collect();
        if !missing.is_empty() || !unknown.is_empty() {
            // BUG-09: report the missing and the unknown keys together.
            let mut parts = Vec::new();
            if !missing.is_empty() {
                parts.push(format!("不足: {}", missing.join(", ")));
            }
            if !unknown.is_empty() {
                parts.push(format!("不明: {}", unknown.join(", ")));
            }
            return Err(ConfigError(format!("[{section}] {}", parts.join(" / "))));
        }
    }

    // Optional keys never make a section required, but unknown keys in a
    // section are still rejected (the loop above already covers the required
    // sections, this covers a section that only contains optional keys).
    for (section, _) in OPTIONAL_KEYS {
        if let Some(entries) = ini.section(section) {
            let allowed = allowed_keys(section);
            let unknown: Vec<&str> = entries
                .iter()
                .map(|(name, _)| name.as_str())
                .filter(|key| !allowed.contains(key))
                .collect();
            if !unknown.is_empty() {
                return Err(ConfigError(format!(
                    "[{section}] 不明: {}",
                    unknown.join(", ")
                )));
            }
        }
    }

    let get_bool = |section: &str, key: &str| -> Result<bool, ConfigError> {
        let raw = ini.value(section, key).unwrap_or_default();
        parse_bool(raw).ok_or_else(|| {
            ConfigError(format!(
                "[{section}] {key} は true または false にしてください"
            ))
        })
    };
    let get_int = |section: &str, key: &str| -> Result<i128, ConfigError> {
        let raw = ini.value(section, key).unwrap_or_default();
        parse_int(raw).ok_or_else(|| ConfigError(format!("[{section}] {key} は整数にしてください")))
    };

    let get_path = |key: &str| -> Result<PathBuf, ConfigError> {
        let raw = ini.value("paths", key).unwrap_or_default().trim();
        if raw.is_empty() {
            return Err(ConfigError(format!("[paths] {key} が空です")));
        }
        Ok(paths::resolve_against(project_dir, raw))
    };

    let input_dir = get_path("input_dir")?;
    let output_dir = get_path("output_dir")?;
    if paths::is_same_or_child(&input_dir, &output_dir)
        || paths::is_same_or_child(&output_dir, &input_dir)
    {
        return Err(ConfigError(
            "入力フォルダと出力フォルダは別々の階層にしてください".to_string(),
        ));
    }
    if input_dir.exists() && !input_dir.is_dir() {
        return Err(ConfigError(format!(
            "入力フォルダにファイルが指定されています: {}",
            input_dir.display()
        )));
    }

    let quality = get_int("webp", "quality")?;
    if !(0..=100).contains(&quality) {
        return Err(ConfigError(
            "[webp] quality は 0～100 にしてください".to_string(),
        ));
    }

    let method = match ini.value("webp", "method") {
        Some(raw) => {
            let value = parse_int(raw)
                .ok_or_else(|| ConfigError("[webp] method は整数にしてください".to_string()))?;
            if !(0..=6).contains(&value) {
                return Err(ConfigError(
                    "[webp] method は 0～6 にしてください".to_string(),
                ));
            }
            Some(value as i64)
        }
        None => None,
    };

    let duration = get_int("animation", "missing_duration_ms")?;
    if duration <= 0 {
        return Err(ConfigError(
            "[animation] missing_duration_ms は正の整数にしてください".to_string(),
        ));
    }

    let commit_every = get_int("output", "db_commit_every")?;
    if commit_every <= 0 {
        return Err(ConfigError(
            "[output] db_commit_every は正の整数にしてください".to_string(),
        ));
    }

    let parallel_jobs = match ini.value("output", "parallel_jobs") {
        Some(raw) => {
            let value = parse_int(raw).ok_or_else(|| {
                ConfigError("[output] parallel_jobs は整数にしてください".to_string())
            })?;
            if value <= 0 {
                return Err(ConfigError(
                    "[output] parallel_jobs は正の整数にしてください".to_string(),
                ));
            }
            Some(value as i64)
        }
        None => None,
    };

    // Optional boolean keys default to `false` when the key is absent.
    let get_bool_optional = |section: &str, key: &str| -> Result<bool, ConfigError> {
        match ini.value(section, key) {
            None => Ok(false),
            Some(raw) => parse_bool(raw).ok_or_else(|| {
                ConfigError(format!(
                    "[{section}] {key} は true または false にしてください"
                ))
            }),
        }
    };
    let lossless = get_bool_optional("webp", "lossless")?;
    let exact = get_bool_optional("webp", "exact")?;

    Ok(Config {
        input_dir,
        output_dir,
        quality: quality as i64,
        missing_duration_ms: duration as i64,
        copy_jpeg: get_bool("copy", "jpeg")?,
        copy_avif: get_bool("copy", "avif")?,
        copy_webp: get_bool("copy", "webp")?,
        copy_other_files: get_bool("copy", "other_files")?,
        overwrite_existing: get_bool("output", "overwrite_existing")?,
        reuse_unchanged: get_bool("output", "reuse_unchanged")?,
        adopt_untracked: get_bool("output", "adopt_untracked")?,
        db_commit_every: commit_every as i64,
        preserve_metadata: get_bool("preserve", "metadata")?,
        preserve_timestamps: get_bool("preserve", "timestamps")?,
        lossless,
        method,
        exact,
        parallel_jobs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn write_config(dir: &Path, body: &str) -> PathBuf {
        let path = dir.join("config.ini");
        let mut file = std::fs::File::create(&path).unwrap();
        file.write_all(body.as_bytes()).unwrap();
        path
    }

    const BASE: &str = "[paths]\ninput_dir = inputs\noutput_dir = outputs\n\n[webp]\nquality = 90\n\n[animation]\nmissing_duration_ms = 80\n\n[copy]\njpeg = true\navif = true\nwebp = true\nother_files = true\n\n[output]\noverwrite_existing = true\nreuse_unchanged = true\nadopt_untracked = false\ndb_commit_every = 32\n\n[preserve]\nmetadata = true\ntimestamps = true\n";

    #[test]
    fn parses_default_configuration() {
        let dir = std::env::temp_dir().join(format!("img2webp-config-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = write_config(&dir, BASE);
        let config = load(&path, &dir).unwrap();
        assert_eq!(config.quality, 90);
        assert_eq!(config.missing_duration_ms, 80);
        assert!(config.copy_jpeg);
        assert!(!config.adopt_untracked);
        assert_eq!(config.db_commit_every, 32);
        assert_eq!(config.method_still(), 4);
        assert_eq!(config.method_animation(), 0);
        assert!(!config.lossless);
        assert!(!config.exact);
        assert_eq!(config.parallel_jobs, None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn accepts_comments_continuations_and_section_tail() {
        let dir = std::env::temp_dir().join(format!("img2webp-config2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let body = BASE
            .replace("[webp]", "; a comment\n[webp] trailing text")
            .replace("quality = 90", "quality : 90\n  # indented comment")
            .replace("metadata = true", "metadata = true\n  extra continuation");
        let path = write_config(&dir, &body);
        // The continuation corrupts `metadata`, so this must be a boolean error.
        let error = load(&path, &dir).unwrap_err();
        assert!(error.0.contains("[preserve] metadata"), "{}", error.0);

        let body = BASE
            .replace("[webp]", "; a comment\n[webp] trailing text")
            .replace("quality = 90", "quality : 90\n  # indented comment");
        let path = write_config(&dir, &body);
        let config = load(&path, &dir).unwrap();
        assert_eq!(config.quality, 90);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rejects_unknown_keys_and_sections() {
        let dir = std::env::temp_dir().join(format!("img2webp-config3-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let body = BASE.replace("[webp]", "[webp]\nextra = 1");
        let path = write_config(&dir, &body);
        let error = load(&path, &dir).unwrap_err();
        assert!(error.0.contains("不明: extra"), "{}", error.0);

        let body = BASE.replace("[copy]", "[copy]\njpeg = true\n[extra]");
        let path = write_config(&dir, &body);
        let error = load(&path, &dir).unwrap_err();
        assert_eq!(error.0, "不明な設定セクション: extra");

        let body = BASE.replace("[webp]", "[DEFAULT]\nfoo = bar\n[webp]");
        let path = write_config(&dir, &body);
        let error = load(&path, &dir).unwrap_err();
        assert_eq!(error.0, "不明な設定セクション: DEFAULT");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rejects_missing_and_unknown_keys_together() {
        let dir = std::env::temp_dir().join(format!("img2webp-config4-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let body = BASE.replace("jpeg = true", "extra = true");
        let path = write_config(&dir, &body);
        let error = load(&path, &dir).unwrap_err();
        assert!(error.0.contains("不足: jpeg"), "{}", error.0);
        assert!(error.0.contains("不明: extra"), "{}", error.0);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn validates_ranges_and_types() {
        let dir = std::env::temp_dir().join(format!("img2webp-config5-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        for (from, to, expected) in [
            ("quality = 90", "quality = 101", "[webp] quality は 0～100"),
            ("quality = 90", "quality = 1_0", "[webp] quality は整数"),
            ("quality = 90", "quality = ９", "[webp] quality は整数"),
            (
                "missing_duration_ms = 80",
                "missing_duration_ms = 0",
                "正の整数",
            ),
            ("db_commit_every = 32", "db_commit_every = 0", "正の整数"),
            ("jpeg = true", "jpeg = maybe", "true または false"),
            (
                "quality = 90",
                "quality = 90\nmethod = 7",
                "[webp] method は 0～6",
            ),
            (
                "quality = 90",
                "quality = 90\nlossless = perhaps",
                "true または false",
            ),
            (
                "db_commit_every = 32",
                "db_commit_every = 32\nparallel_jobs = -1",
                "正の整数",
            ),
        ] {
            let body = BASE.replace(from, to);
            let path = write_config(&dir, &body);
            let error = load(&path, &dir).unwrap_err();
            assert!(error.0.contains(expected), "{} => {}", to, error.0);
        }
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rejects_same_or_nested_paths() {
        let dir = std::env::temp_dir().join(format!("img2webp-config6-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let body = BASE.replace("output_dir = outputs", "output_dir = inputs");
        let path = write_config(&dir, &body);
        let error = load(&path, &dir).unwrap_err();
        assert_eq!(
            error.0,
            "入力フォルダと出力フォルダは別々の階層にしてください"
        );

        let body = BASE.replace("output_dir = outputs", "output_dir = inputs/outputs");
        let path = write_config(&dir, &body);
        let error = load(&path, &dir).unwrap_err();
        assert_eq!(
            error.0,
            "入力フォルダと出力フォルダは別々の階層にしてください"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn bundled_template_matches_code_defaults() {
        let text = include_str!("../../config.ini");
        let dir = std::env::temp_dir().join(format!("img2webp-config8-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = write_config(&dir, text);
        let config = load(&path, &dir).unwrap();
        assert_eq!(config.quality, 90);
        assert_eq!(config.missing_duration_ms, 80);
        assert_eq!(config.db_commit_every, 32);
        assert!(
            config.copy_jpeg && config.copy_avif && config.copy_webp && config.copy_other_files
        );
        assert!(config.overwrite_existing && config.reuse_unchanged && !config.adopt_untracked);
        assert!(config.preserve_metadata && config.preserve_timestamps);
        // New optional keys: the template keeps the documented defaults.
        assert!(!config.lossless);
        assert!(!config.exact);
        assert_eq!(config.method, None);
        assert_eq!(config.method_still(), DEFAULT_METHOD_STILL);
        assert_eq!(config.method_animation(), DEFAULT_METHOD_ANIMATION);
        assert_eq!(config.parallel_jobs, None);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn rejects_bad_encoding() {
        let dir = std::env::temp_dir().join(format!("img2webp-config7-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.ini");
        std::fs::write(&path, [0xffu8]).unwrap();
        let error = load(&path, &dir).unwrap_err();
        assert!(
            error.0.starts_with("config.ini を読めません"),
            "{}",
            error.0
        );
        std::fs::remove_dir_all(&dir).ok();
    }
}
