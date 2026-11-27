//! Metadata preparation and post-save verification (design spec §3.5).

use crate::config::Config;
use crate::probe::{Format, Probe};
use crate::tiff::{self, Entry, Exif};
use crate::webp_encode::WebpMetadata;

pub struct Prepared {
    pub exif: Option<Exif>,
    pub icc: Option<Vec<u8>>,
    pub xmp: Option<Vec<u8>>,
    pub orientation: Option<u16>,
}

fn text_value(value: &[u8]) -> String {
    match String::from_utf8(value.to_vec()) {
        Ok(text) => text,
        Err(_) => value.iter().map(|byte| *byte as char).collect(),
    }
}

/// The generated-text rules of `source_text()` (design spec §3.5).
pub fn source_text(probe: &Probe) -> String {
    match probe.format {
        Format::Png => {
            let mut lines: Vec<String> = Vec::new();
            if let Some((_, value)) = probe.text.iter().find(|(key, _)| key == "parameters") {
                if !value.is_empty() {
                    lines.push(value.clone());
                }
            }
            for (key, value) in probe.text.iter() {
                if key == "parameters" {
                    continue;
                }
                let lowered = key.to_lowercase();
                if lowered == "xmp" || lowered == "xml:com.adobe.xmp" {
                    continue;
                }
                if value.is_empty() {
                    continue;
                }
                lines.push(format!("{key}: {value}"));
            }
            lines.join("\n")
        }
        Format::Jpeg | Format::Gif => probe
            .comment
            .as_ref()
            .map(|comment| text_value(comment))
            .unwrap_or_default(),
        Format::Unknown => String::new(),
    }
}

const UNICODE_PREFIX: &[u8] = b"UNICODE\0";

pub fn prepare(probe: &Probe, config: &Config, stealth: &str) -> Result<Prepared, String> {
    let preserve = config.preserve_metadata;
    let mut parsed: Option<Exif> = match probe.exif.as_ref() {
        Some(raw) => match Exif::parse(raw) {
            Ok(exif) => Some(exif),
            Err(err) => {
                if preserve {
                    return Err(format!("Exif を読み取れません: {err}"));
                }
                None
            }
        },
        None => None,
    };
    let mut exif = parsed.take().unwrap_or_default();
    let mut orientation = exif.orientation();
    if orientation.is_none() {
        // Fallback: read Orientation directly from the Exif TIFF structure
        // (the equivalent of Pillow's `getexif()` fallback).
        if let Some(raw) = probe.exif.as_ref() {
            if let Ok(other) = Exif::parse(raw) {
                orientation = other.orientation();
            }
        }
    }
    if matches!(orientation, Some(2..=8)) {
        exif.ifd0.remove(tiff::TAG_ORIENTATION);
    }
    if !preserve {
        return Ok(Prepared {
            exif: None,
            icc: None,
            xmp: None,
            orientation,
        });
    }
    let mut candidate = source_text(probe);
    if !stealth.is_empty() {
        candidate = if candidate.is_empty() {
            stealth.to_string()
        } else {
            format!("{candidate}\n{stealth}")
        };
    }
    let existing_comment = exif
        .exif
        .get(tiff::TAG_USER_COMMENT)
        .map(|entry| !entry.value.is_empty())
        .unwrap_or(false);
    if !candidate.is_empty() {
        if existing_comment {
            if exif.ifd0.contains(tiff::TAG_XP_COMMENT) {
                return Err(
                    "UserComment と XPComment が使用済みで、追加テキストを保持できません"
                        .to_string(),
                );
            }
            let mut value = tiff::utf16le(&candidate);
            value.extend_from_slice(&[0, 0]);
            exif.ifd0.set(Entry::byte(tiff::TAG_XP_COMMENT, &value));
        } else {
            let mut value = UNICODE_PREFIX.to_vec();
            value.extend_from_slice(&tiff::utf16be(&candidate));
            exif.exif
                .set(Entry::undefined(tiff::TAG_USER_COMMENT, &value));
        }
    }
    Ok(Prepared {
        exif: Some(exif),
        icc: probe.icc.clone(),
        xmp: probe.xmp.clone(),
        orientation,
    })
}

fn ifd_entries_differ(expected: &tiff::Ifd, actual: &tiff::Ifd) -> bool {
    if expected.entries.len() != actual.entries.len() {
        return true;
    }
    expected
        .entries
        .iter()
        .zip(actual.entries.iter())
        .any(|(a, b)| a.tag != b.tag || a.typ != b.typ || a.count != b.count || a.value != b.value)
}

/// Verification after saving: UserComment/XPComment are mandatory, the other
/// IFDs, thumbnail, ICC and XMP only warn (design spec §3.5, BUG-13).
pub fn verify(
    metadata: &WebpMetadata,
    expected_exif: Option<&Exif>,
    expected_icc: Option<&[u8]>,
    expected_xmp: Option<&[u8]>,
) -> Result<Vec<String>, String> {
    let mut warnings = Vec::new();
    if let Some(exif) = expected_exif {
        if exif.has_data() {
            let expected = Exif::parse(&exif.serialize())
                .map_err(|err| format!("出力の Exif を読み戻せません: {err}"))?;
            let actual_raw = metadata.exif.as_deref().unwrap_or(&[]);
            let actual = Exif::parse(actual_raw)
                .map_err(|err| format!("出力の Exif を読み戻せません: {err}"))?;
            let expected_comment = expected.exif.get(tiff::TAG_USER_COMMENT);
            let actual_comment = actual.exif.get(tiff::TAG_USER_COMMENT);
            if expected_comment.map(|entry| &entry.value)
                != actual_comment.map(|entry| &entry.value)
            {
                return Err("出力の UserComment が元データと一致しません".to_string());
            }
            let expected_xp = expected.ifd0.get(tiff::TAG_XP_COMMENT);
            let actual_xp = actual.ifd0.get(tiff::TAG_XP_COMMENT);
            if expected_xp.map(|entry| &entry.value) != actual_xp.map(|entry| &entry.value) {
                return Err("出力の XPComment が元データと一致しません".to_string());
            }
            for (name, expected_ifd, actual_ifd) in [
                ("0th", &expected.ifd0, &actual.ifd0),
                ("Exif", &expected.exif, &actual.exif),
                ("GPS", &expected.gps, &actual.gps),
                ("Interop", &expected.interop, &actual.interop),
                ("1st", &expected.ifd1, &actual.ifd1),
            ] {
                if ifd_entries_differ(expected_ifd, actual_ifd) {
                    warnings.push(format!("Exif {name} の一部が変わりました"));
                }
            }
            if expected.thumbnail != actual.thumbnail {
                warnings.push("Exif サムネイルが変わりました".to_string());
            }
        }
    }
    if let Some(icc) = expected_icc {
        if metadata.icc.as_deref() != Some(icc) {
            warnings.push("ICC プロファイルを完全には保持できませんでした".to_string());
        }
    }
    if let Some(xmp) = expected_xmp {
        if metadata.xmp.as_deref() != Some(xmp) {
            warnings.push("XMP を完全には保持できませんでした".to_string());
        }
    }
    Ok(warnings)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn probe(name: &str) -> Probe {
        crate::probe::probe(
            &std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests")
                .join("fixtures")
                .join(name),
        )
        .unwrap()
    }

    #[test]
    fn parameters_come_first_and_xmp_is_excluded() {
        let probe = probe("text.png");
        assert_eq!(
            source_text(&probe),
            "a cat\nSteps: 20\nDescription: NovelAI text"
        );
    }

    #[test]
    fn gif_comment_is_the_generated_text() {
        let probe = probe("anim.gif");
        assert_eq!(source_text(&probe), "gif prompt");
    }

    #[test]
    fn user_comment_becomes_xp_comment_when_present() {
        let mut probe = probe("text.png");
        let mut exif = Exif::default();
        exif.exif.set(Entry::undefined(
            tiff::TAG_USER_COMMENT,
            b"UNICODE\0e\0x\0i\0s\0t\0",
        ));
        probe.exif = Some(exif.serialize());
        let config_text = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests")
                .join("fixtures")
                .join("config-fixture.ini"),
        );
        let dir = std::env::temp_dir().join(format!("img2webp-meta-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.ini"), config_text.unwrap()).unwrap();
        let config = crate::config::load(&dir.join("config.ini"), &dir).unwrap();
        let prepared = prepare(&probe, &config, "").unwrap();
        let exif = prepared.exif.unwrap();
        assert!(exif.ifd0.get(tiff::TAG_XP_COMMENT).is_some());
        let xp = exif.ifd0.get(tiff::TAG_XP_COMMENT).unwrap();
        let expected = "a cat\nSteps: 20\nDescription: NovelAI text";
        let mut value = tiff::utf16le(expected);
        value.extend_from_slice(&[0, 0]);
        assert_eq!(xp.value, value);
        std::fs::remove_dir_all(&dir).ok();
    }
}
