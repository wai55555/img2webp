//! Input scan, classification and collision naming (`plan_jobs`).
//!
//! The naming rules must stay byte-compatible with the Python implementation
//! (design spec §1.4): copies win the plain name, converted targets fall back to
//! `<stem><original extension>.webp`, then numeric suffixes are appended to the
//! stem (`sample.png-2.webp`).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

use crate::config::Config;
use crate::paths;
use crate::state::{DEV_MARKER_NAME, DEV_STATE_DATABASE_NAME, STATE_DATABASE_NAME};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    Convert,
    Copy,
}

impl Action {
    pub fn as_str(self) -> &'static str {
        match self {
            Action::Convert => "convert",
            Action::Copy => "copy",
        }
    }
}

#[derive(Clone, Debug)]
pub struct Job {
    pub source: PathBuf,
    pub relative_source: PathBuf,
    pub action: Action,
    pub destination: PathBuf,
}

impl Job {
    pub fn relative_source_posix(&self) -> String {
        paths::to_posix(&self.relative_source)
    }

    pub fn relative_source_display(&self) -> String {
        paths::display(&self.relative_source)
    }

    pub fn destination_display(&self) -> String {
        paths::display(&self.destination)
    }
}

pub struct Plan {
    pub jobs: Vec<Job>,
    pub skipped: usize,
}

/// Names that must never be used as a destination (both builds reserve both
/// database names so a database file in the input folder is never a target).
pub fn reserved_names() -> HashSet<String> {
    let mut names = HashSet::new();
    for base in [STATE_DATABASE_NAME, DEV_STATE_DATABASE_NAME] {
        names.insert(paths::casefold(base));
        for suffix in ["-wal", "-shm", "-journal"] {
            names.insert(paths::casefold(&format!("{base}{suffix}")));
        }
    }
    names.insert(paths::casefold(DEV_MARKER_NAME));
    names
}

fn classify_path(path: &Path, config: &Config) -> Option<Action> {
    let suffix = paths::path_suffix(path).to_lowercase();
    match suffix.as_str() {
        ".jpg" | ".jpeg" => Some(if config.copy_jpeg {
            Action::Copy
        } else {
            Action::Convert
        }),
        ".png" | ".gif" => Some(Action::Convert),
        ".avif" => {
            if config.copy_avif {
                Some(Action::Copy)
            } else {
                None
            }
        }
        ".webp" => {
            if config.copy_webp {
                Some(Action::Copy)
            } else {
                None
            }
        }
        _ => {
            if config.copy_other_files {
                Some(Action::Copy)
            } else {
                None
            }
        }
    }
}

fn collision_key(relative: &Path) -> String {
    paths::casefold(&paths::to_posix(relative))
}

fn numbered(path: &Path, number: u32) -> PathBuf {
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().to_string())
        .unwrap_or_default();
    let stem = paths::stem(&name);
    let suffix = paths::suffix(&name);
    paths::with_name(path, &format!("{stem}-{number}{suffix}"))
}

/// Collects every regular file below `root` without following links.
fn collect_sources(root: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.file_type().is_symlink() {
            continue;
        }
        if metadata.is_dir() {
            collect_sources(&path, out);
        } else if metadata.is_file() {
            out.push(path);
        }
    }
}

pub fn plan(config: &Config) -> Plan {
    let mut sources: Vec<PathBuf> = Vec::new();
    collect_sources(&config.input_dir, &mut sources);
    let mut keyed: Vec<(String, String, PathBuf, PathBuf)> = sources
        .into_iter()
        .filter_map(|path| {
            let relative = path.strip_prefix(&config.input_dir).ok()?.to_path_buf();
            Some((
                collision_key(&relative),
                path.to_string_lossy().to_string(),
                path,
                relative,
            ))
        })
        .collect();
    keyed.sort_by(|a, b| (a.0.clone(), a.1.clone()).cmp(&(b.0.clone(), b.1.clone())));

    let mut proposed: Vec<(PathBuf, PathBuf, Action, PathBuf)> = Vec::new();
    let mut skipped = 0usize;
    for (_, _, source, relative) in keyed {
        let Some(action) = classify_path(&relative, config) else {
            skipped += 1;
            continue;
        };
        let destination = if action == Action::Convert {
            paths::path_with_suffix(&relative, ".webp")
        } else {
            relative.clone()
        };
        proposed.push((source, relative, action, destination));
    }

    let mut counts: HashMap<String, usize> = HashMap::new();
    for (_, _, _, destination) in proposed.iter() {
        *counts.entry(collision_key(destination)).or_insert(0) += 1;
    }

    let mut used: HashSet<String> = reserved_names();

    let mut ordered = proposed;
    ordered.sort_by(|a, b| {
        let key = |item: &(PathBuf, PathBuf, Action, PathBuf)| {
            (
                item.2 == Action::Convert,
                collision_key(&item.3),
                collision_key(&item.1),
            )
        };
        key(a).cmp(&key(b))
    });

    let mut jobs: Vec<Job> = Vec::new();
    for (source, relative, action, mut destination) in ordered {
        if action == Action::Convert
            && counts
                .get(&collision_key(&destination))
                .copied()
                .unwrap_or(0)
                > 1
        {
            let name = relative
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
                .unwrap_or_default();
            let stem = paths::stem(&name).to_string();
            let suffix = paths::suffix(&name).to_lowercase();
            destination = paths::with_name(&destination, &format!("{stem}{suffix}.webp"));
        }
        let base = destination.clone();
        let mut number = 2u32;
        while used.contains(&collision_key(&destination)) {
            destination = numbered(&base, number);
            number += 1;
        }
        used.insert(collision_key(&destination));
        jobs.push(Job {
            source,
            relative_source: relative,
            action,
            destination: config.output_dir.join(destination),
        });
    }
    jobs.sort_by_key(|job| collision_key(&job.relative_source));
    Plan { jobs, skipped }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn setup(tag: &str) -> (Config, PathBuf) {
        let root = std::env::temp_dir().join(format!("img2webp-plan-{tag}-{}", std::process::id()));
        fs::create_dir_all(root.join("inputs")).unwrap();
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("config-fixture.ini");
        let base = fs::read_to_string(path).unwrap();
        fs::write(root.join("config.ini"), base).unwrap();
        let config = crate::config::load(&root.join("config.ini"), &root).unwrap();
        (config, root)
    }

    #[test]
    fn collision_names_include_extension_only_when_needed() {
        let (config, root) = setup("names");
        let inputs = &config.input_dir;
        fs::copy(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests")
                .join("fixtures")
                .join("sample.png"),
            inputs.join("sample.png"),
        )
        .unwrap();
        fs::copy(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests")
                .join("fixtures")
                .join("sample.jpg"),
            inputs.join("sample.jpg"),
        )
        .unwrap();
        fs::write(inputs.join("sample.webp"), b"copied webp").unwrap();
        fs::write(inputs.join("sample.png.webp"), b"copied name collision").unwrap();
        let config = Config {
            copy_jpeg: false,
            ..config
        };
        let plan = plan(&config);
        let mut names: Vec<(String, String)> = plan
            .jobs
            .iter()
            .map(|job| {
                (
                    job.relative_source
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .to_string(),
                    job.destination
                        .file_name()
                        .unwrap()
                        .to_string_lossy()
                        .to_string(),
                )
            })
            .collect();
        names.sort();
        assert_eq!(
            names,
            vec![
                ("sample.jpg".to_string(), "sample.jpg.webp".to_string()),
                ("sample.png".to_string(), "sample.png-2.webp".to_string()),
                ("sample.png.webp".to_string(), "sample.png.webp".to_string()),
                ("sample.webp".to_string(), "sample.webp".to_string()),
            ]
        );
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn database_names_are_reserved_at_the_root_only() {
        let (config, root) = setup("reserved");
        let inputs = &config.input_dir;
        fs::write(inputs.join(STATE_DATABASE_NAME), b"db").unwrap();
        fs::write(inputs.join(DEV_MARKER_NAME), b"marker").unwrap();
        fs::create_dir_all(inputs.join("sub")).unwrap();
        fs::write(inputs.join("sub").join(STATE_DATABASE_NAME), b"db").unwrap();
        fs::copy(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests")
                .join("fixtures")
                .join("sample.png"),
            inputs.join("image.png"),
        )
        .unwrap();
        let plan = plan(&config);
        let mut names: Vec<String> = plan
            .jobs
            .iter()
            .map(|job| {
                job.destination
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .to_string()
            })
            .collect();
        names.sort();
        assert!(names.contains(&"image.webp".to_string()));
        // The root-level database names get numeric suffixes, the sub-folder
        // one keeps its name.
        assert!(
            names.contains(&".img2webp-2.sqlite3".to_string()),
            "{names:?}"
        );
        assert!(
            names.contains(&".img2webp-dev-output-2".to_string()),
            "{names:?}"
        );
        fs::remove_dir_all(root).ok();
    }

    #[test]
    fn symlinks_and_extension_only_names() {
        let (config, root) = setup("symlink");
        let inputs = &config.input_dir;
        fs::copy(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("tests")
                .join("fixtures")
                .join("sample.png"),
            inputs.join(".png"),
        )
        .unwrap();
        let plan = plan(&config);
        let names: Vec<String> = plan
            .jobs
            .iter()
            .map(|job| {
                job.destination
                    .file_name()
                    .unwrap()
                    .to_string_lossy()
                    .to_string()
            })
            .collect();
        // A file called `.png` has no suffix and is therefore "other".
        assert_eq!(names, vec![".png".to_string()]);
        fs::remove_dir_all(root).ok();
    }
}
