//! Embeds the versions of the dependencies that influence the output.
//!
//! Versions are read from `Cargo.lock` so the identity never goes stale
//! (design spec §3.7, decision D-LOCK). `cargo metadata --offline` cannot be
//! used for this because a lock file contains foreign-platform packages that
//! are not present in a local crate cache; reading the lock directly is
//! deterministic and needs no network. When the lock is missing, a
//! `cargo metadata` probe is attempted, and if that also fails an explicit
//! fallback marker is embedded so a normal build and a fallback build never
//! share a signature.

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

/// Crates whose version influences the produced bytes or metadata.
const TRACKED_CRATES: &[&str] = &[
    "image",
    "png",
    "gif",
    "zune-jpeg",
    "zune-core",
    "image-webp",
    "jpeg-decoder",
    "libwebp-sys",
    "rusqlite",
    "lcms2",
    "lcms2-sys",
    "flate2",
    "miniz_oxide",
];

fn main() {
    println!("cargo:rerun-if-changed=Cargo.lock");
    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-env-changed=CARGO_FEATURE_DEFAULT");

    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap_or_default());
    let manifest = manifest_dir.join("Cargo.toml");
    let lock = manifest_dir.join("Cargo.lock");

    let versions = match read_lock(&lock) {
        Some(map) => render(&map),
        None => match probe(&manifest) {
            Ok(map) => render(&map),
            Err(err) => {
                println!("cargo:warning=Cargo.lock/metadata probe failed: {err}");
                format!(
                    "unprobed-{}",
                    env::var("CARGO_PKG_VERSION").unwrap_or_default()
                )
            }
        },
    };
    println!("cargo:rustc-env=IMG2WEBP_DEP_VERSIONS={versions}");

    let features = active_own_features(&manifest);
    println!("cargo:rustc-env=IMG2WEBP_FEATURES={features}");

    let target_os = env::var("CARGO_CFG_TARGET_OS").unwrap_or_default();
    println!("cargo:rustc-env=IMG2WEBP_TARGET_OS={target_os}");
}

fn render(map: &[(String, String)]) -> String {
    let mut items = Vec::new();
    for name in TRACKED_CRATES {
        match map.iter().find(|(key, _)| key == name) {
            Some((key, version)) => items.push(format!("{key}={version}")),
            None => items.push(format!("{name}=absent")),
        }
    }
    items.join(";")
}

/// Minimal `[[package]] name = "..." / version = "..."` reader.
fn read_lock(path: &Path) -> Option<Vec<(String, String)>> {
    let text = fs::read_to_string(path).ok()?;
    let mut out: Vec<(String, String)> = Vec::new();
    let mut name: Option<String> = None;
    let mut version: Option<String> = None;
    let mut in_package = false;
    for line in text.lines() {
        let line = line.trim();
        if line == "[[package]]" {
            if let (Some(name), Some(version)) = (name.take(), version.take()) {
                out.push((name, version));
            }
            in_package = true;
            continue;
        }
        if line.starts_with('[') {
            if let (Some(name), Some(version)) = (name.take(), version.take()) {
                out.push((name, version));
            }
            in_package = false;
            continue;
        }
        if !in_package {
            continue;
        }
        if let Some(rest) = line.strip_prefix("name = ") {
            name = Some(trim_quotes(rest));
        } else if let Some(rest) = line.strip_prefix("version = ") {
            version = Some(trim_quotes(rest));
        }
    }
    if let (Some(name), Some(version)) = (name.take(), version.take()) {
        out.push((name, version));
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

fn trim_quotes(text: &str) -> String {
    text.trim().trim_matches('"').to_string()
}

fn active_own_features(manifest: &Path) -> String {
    let declared = declared_features(manifest);
    let mut active: Vec<String> = Vec::new();
    for (key, _) in env::vars() {
        let Some(rest) = key.strip_prefix("CARGO_FEATURE_") else {
            continue;
        };
        let lowered = rest.to_ascii_lowercase();
        let matched = declared
            .iter()
            .find(|f| f.replace('-', "_").to_ascii_uppercase() == lowered)
            .cloned()
            .unwrap_or(lowered);
        active.push(matched);
    }
    active.sort();
    active.join(",")
}

/// Reads the `[features]` table of our own manifest so that `-`/`_` differences
/// in feature names can be restored.
fn declared_features(manifest: &Path) -> Vec<String> {
    let Ok(text) = fs::read_to_string(manifest) else {
        return Vec::new();
    };
    let mut in_features = false;
    let mut names = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_features = line.replace(' ', "") == "[features]";
            continue;
        }
        if !in_features || line.is_empty() || line.starts_with('#') {
            continue;
        }
        if let Some((name, _)) = line.split_once('=') {
            names.push(name.trim().to_string());
        }
    }
    names
}

fn probe(manifest: &Path) -> Result<Vec<(String, String)>, String> {
    let cargo = env::var("CARGO").map_err(|_| "CARGO is not set".to_string())?;
    let mut cmd = Command::new(cargo);
    cmd.arg("metadata")
        .arg("--format-version")
        .arg("1")
        .arg("--locked")
        .arg("--manifest-path")
        .arg(manifest);
    if env::var_os("CARGO_FEATURE_DEFAULT").is_none() {
        cmd.arg("--no-default-features");
    }
    let output = cmd.output().map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    let json = String::from_utf8_lossy(&output.stdout);
    let mut found = Vec::new();
    for name in TRACKED_CRATES {
        let needle = format!("\"name\":\"{name}\",\"version\":\"");
        if let Some(start) = json.find(&needle) {
            let rest = &json[start + needle.len()..];
            if let Some(end) = rest.find('"') {
                found.push(((*name).to_string(), rest[..end].to_string()));
            }
        }
    }
    Ok(found)
}
