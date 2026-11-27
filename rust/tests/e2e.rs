//! Black-box tests: the release/dev binary is copied into a throw-away project
//! directory and driven exactly like a user would.
//!
//! The expectations are written with the Phase 0 read-back rules: the
//! development build uses `outputs-dev` / `.img2webp-dev.sqlite3`, the release
//! build uses `outputs` / `.img2webp.sqlite3`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::sync::atomic::{AtomicUsize, Ordering};

use image::GenericImageView;
use img2webp::tiff::{self, Exif};
use img2webp::webp_encode;

static COUNTER: AtomicUsize = AtomicUsize::new(0);

const DEV: bool = cfg!(feature = "dev-db");

fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join(name)
}

struct Project {
    root: PathBuf,
}

struct Run {
    code: i32,
    stdout: String,
    stderr: String,
}

impl Project {
    fn new(tag: &str) -> Self {
        let index = COUNTER.fetch_add(1, Ordering::SeqCst);
        let root =
            std::env::temp_dir().join(format!("img2webp-e2e-{tag}-{}-{index}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("inputs")).unwrap();
        let exe = std::env::current_exe().unwrap();
        let _ = exe;
        fs::copy(
            Path::new(env!("CARGO_BIN_EXE_img2webp")),
            root.join("img2webp.exe"),
        )
        .unwrap();
        fs::write(root.join("config.ini"), config_text("")).unwrap();
        Project { root }
    }

    fn root(&self) -> &Path {
        &self.root
    }

    fn inputs(&self) -> PathBuf {
        self.root.join("inputs")
    }

    fn out_dir(&self) -> PathBuf {
        self.root.join(if DEV { "outputs-dev" } else { "outputs" })
    }

    fn db_path(&self) -> PathBuf {
        self.out_dir().join(if DEV {
            ".img2webp-dev.sqlite3"
        } else {
            ".img2webp.sqlite3"
        })
    }

    fn configure(&self, patches: &[(&str, &str)]) {
        fs::write(self.root.join("config.ini"), config_text_patches(patches)).unwrap();
    }

    fn copy_fixture(&self, name: &str) -> PathBuf {
        let target = self.inputs().join(name);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::copy(fixture(name), &target).unwrap();
        target
    }

    fn write_input(&self, name: &str, bytes: &[u8]) -> PathBuf {
        let target = self.inputs().join(name);
        if let Some(parent) = target.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(&target, bytes).unwrap();
        target
    }

    fn run(&self) -> Run {
        let output: Output = std::process::Command::new(self.root.join("img2webp.exe"))
            .current_dir(&self.root)
            .output()
            .unwrap();
        Run {
            code: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).to_string(),
            stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        }
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn config_text_patches(patches: &[(&str, &str)]) -> String {
    let mut lines: Vec<String> = include_str!("fixtures/config-fixture.ini")
        .lines()
        .map(|line| line.to_string())
        .collect();
    for (key, value) in patches {
        let mut replaced = false;
        for line in lines.iter_mut() {
            let trimmed = line.trim_start().to_string();
            let trimmed = trimmed.as_str();
            let spaced = format!("{key} =");
            let compact = format!("{key}=");
            if trimmed.starts_with(spaced.as_str()) || trimmed.starts_with(compact.as_str()) {
                *line = format!("{key} = {value}");
                replaced = true;
                break;
            }
        }
        if !replaced {
            if *key == "lossless" || *key == "method" || *key == "exact" {
                // New optional keys go in the [webp] section.
                let index = lines
                    .iter()
                    .position(|l| l.trim() == "[animation]")
                    .unwrap();
                lines.insert(index, format!("{key} = {value}"));
            } else {
                let index = lines.iter().position(|l| l.trim() == "[preserve]").unwrap();
                lines.insert(index, format!("{key} = {value}"));
            }
        }
    }
    let mut text = lines.join("\n");
    text.push('\n');
    text
}

fn config_text(_: &str) -> String {
    config_text_patches(&[])
}

fn webp_metadata(path: &Path) -> webp_encode::WebpMetadata {
    webp_encode::read_metadata(&fs::read(path).unwrap()).unwrap()
}

fn user_comment(path: &Path) -> Option<String> {
    let metadata = webp_metadata(path);
    let exif = Exif::parse(metadata.exif.as_deref().unwrap_or(&[])).ok()?;
    let entry = exif.exif.get(tiff::TAG_USER_COMMENT)?;
    let value = entry.value.clone();
    let data = if value.starts_with(b"UNICODE\0") {
        &value[8..]
    } else {
        &value[..]
    };
    Some(String::from_utf16_lossy(
        &data
            .chunks_exact(2)
            .map(|pair| u16::from_be_bytes([pair[0], pair[1]]))
            .collect::<Vec<u16>>(),
    ))
}

fn xp_comment(path: &Path) -> Option<String> {
    let metadata = webp_metadata(path);
    let exif = Exif::parse(metadata.exif.as_deref().unwrap_or(&[])).ok()?;
    let entry = exif.ifd0.get(tiff::TAG_XP_COMMENT)?;
    let value = entry.value.clone();
    let units: Vec<u16> = value[..value.len() / 2 * 2]
        .chunks_exact(2)
        .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
        .collect();
    let text = String::from_utf16_lossy(&units);
    Some(text.trim_end_matches('\0').to_string())
}

fn animation_frames(path: &Path) -> Vec<(u32, u32, u32, u8)> {
    let data = fs::read(path).unwrap();
    let mut frames = Vec::new();
    let mut position = 12usize;
    while position + 8 <= data.len() {
        let id = &data[position..position + 4];
        let size = u32::from_le_bytes([
            data[position + 4],
            data[position + 5],
            data[position + 6],
            data[position + 7],
        ]) as usize;
        let payload = &data[position + 8..position + 8 + size];
        if id == b"ANMF" && payload.len() >= 16 {
            let width = u32::from_le_bytes([payload[6], payload[7], payload[8], 0]) + 1;
            let height = u32::from_le_bytes([payload[9], payload[10], payload[11], 0]) + 1;
            let duration = u32::from_le_bytes([payload[12], payload[13], payload[14], 0]);
            let flags = payload[15];
            frames.push((width, height, duration, flags));
        }
        position += 8 + size + (size & 1);
    }
    frames
}

fn loop_count(path: &Path) -> Option<u16> {
    let data = fs::read(path).unwrap();
    let mut position = 12usize;
    while position + 8 <= data.len() {
        let id = &data[position..position + 4];
        let size = u32::from_le_bytes([
            data[position + 4],
            data[position + 5],
            data[position + 6],
            data[position + 7],
        ]) as usize;
        let payload = &data[position + 8..position + 8 + size];
        if id == b"ANIM" && payload.len() >= 6 {
            return Some(u16::from_le_bytes([payload[4], payload[5]]));
        }
        position += 8 + size + (size & 1);
    }
    None
}

fn png_chunk(kind: &[u8], data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut crc_input = kind.to_vec();
    crc_input.extend_from_slice(data);
    out.extend_from_slice(&crc32(&crc_input).to_be_bytes());
    out
}

fn crc32(data: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for byte in data {
        crc ^= *byte as u32;
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xedb8_8320;
            } else {
                crc >>= 1;
            }
        }
    }
    !crc
}

/// A PNG that only carries a header (used for limit checks).
fn header_only_png(width: u32, height: u32, depth: u8, color: u8) -> Vec<u8> {
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.push(depth);
    ihdr.push(color);
    ihdr.extend_from_slice(&[0, 0, 0]);
    let mut out = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a];
    out.extend_from_slice(&png_chunk(b"IHDR", &ihdr));
    out.extend_from_slice(&png_chunk(b"IEND", &[]));
    out
}

fn log_paths(project: &Project, run: &Run) -> String {
    format!(
        "stdout:\n{}\nstderr:\n{}\nroot: {}",
        run.stdout,
        run.stderr,
        project.root().display()
    )
}

#[test]
fn png_metadata_and_timestamps() {
    let project = Project::new("png-metadata");
    let source = project.copy_fixture("text.png");
    let original = fs::metadata(&source).unwrap().modified().unwrap();
    let run = project.run();
    assert_eq!(run.code, 0, "{}", log_paths(&project, &run));
    let output = project.out_dir().join("text.webp");
    assert!(output.exists(), "{}", log_paths(&project, &run));
    assert_eq!(
        user_comment(&output).as_deref(),
        Some("a cat\nSteps: 20\nDescription: NovelAI text")
    );
    let decoded = image::load_from_memory(&fs::read(&output).unwrap()).unwrap();
    let rgba = decoded.to_rgba8();
    assert_eq!(rgba.get_pixel(0, 0)[3], 0);
    let mtime = fs::metadata(&output).unwrap().modified().unwrap();
    assert_eq!(mtime, original);
}

#[test]
fn output_atime_comes_from_the_stat_taken_before_header_reading() {
    let project = Project::new("atime");
    let source = project.copy_fixture("text.png");
    let atime = filetime::FileTime::from_unix_time(1_600_000_000, 0);
    let mtime = filetime::FileTime::from_unix_time(1_600_000_100, 0);
    filetime::set_file_times(&source, atime, mtime).unwrap();
    let run = project.run();
    assert_eq!(run.code, 0, "{}", log_paths(&project, &run));
    let metadata = fs::metadata(project.out_dir().join("text.webp")).unwrap();
    assert_eq!(
        filetime::FileTime::from_last_access_time(&metadata),
        atime,
        "the output atime must be the pre-scan stat value"
    );
    assert_eq!(
        filetime::FileTime::from_last_modification_time(&metadata),
        mtime
    );
}

#[test]
fn conflict_keeps_user_comment_and_writes_xp_comment() {
    let project = Project::new("conflict");
    project.copy_fixture("conflict.png");
    let run = project.run();
    assert_eq!(run.code, 0, "{}", log_paths(&project, &run));
    let output = project.out_dir().join("conflict.webp");
    assert_eq!(user_comment(&output).as_deref(), Some("original comment"));
    assert_eq!(xp_comment(&output).as_deref(), Some("new prompt"));
}

#[test]
fn metadata_can_be_disabled() {
    let project = Project::new("no-metadata");
    project.configure(&[("metadata", "false")]);
    project.copy_fixture("text.png");
    let run = project.run();
    assert_eq!(run.code, 0, "{}", log_paths(&project, &run));
    let metadata = webp_metadata(&project.out_dir().join("text.webp"));
    assert!(metadata.exif.is_none());
    assert!(metadata.icc.is_none());
    assert!(metadata.xmp.is_none());
}

#[test]
fn icc_and_xmp_are_carried_over() {
    let project = Project::new("icc-xmp");
    project.copy_fixture("text.png");
    let run = project.run();
    assert_eq!(run.code, 0, "{}", log_paths(&project, &run));
    let metadata = webp_metadata(&project.out_dir().join("text.webp"));
    let source = img2webp::probe::probe(&fixture("text.png")).unwrap();
    assert_eq!(metadata.icc.as_deref(), source.icc.as_deref());
    assert_eq!(metadata.xmp.as_deref(), source.xmp.as_deref());
}

#[test]
fn gif_animation_loop_duration_and_comment() {
    let project = Project::new("gif");
    project.copy_fixture("anim.gif");
    let run = project.run();
    assert_eq!(run.code, 0, "{}", log_paths(&project, &run));
    let output = project.out_dir().join("anim.webp");
    let frames = animation_frames(&output);
    assert_eq!(frames.len(), 2);
    assert_eq!(frames[0].2, 70);
    assert_eq!(frames[1].2, 130);
    assert_eq!(loop_count(&output), Some(2));
    assert_eq!(user_comment(&output).as_deref(), Some("gif prompt"));
}

#[test]
fn gif_without_loop_count_defaults_to_one() {
    let project = Project::new("gif-once");
    project.copy_fixture("once.gif");
    let run = project.run();
    assert_eq!(run.code, 0, "{}", log_paths(&project, &run));
    assert_eq!(loop_count(&project.out_dir().join("once.webp")), Some(1));
}

#[test]
fn apng_default_image_is_skipped() {
    let project = Project::new("apng");
    project.copy_fixture("apng-separate.png");
    project.copy_fixture("apng-standard.png");
    let run = project.run();
    assert_eq!(run.code, 0, "{}", log_paths(&project, &run));
    let separate = animation_frames(&project.out_dir().join("apng-separate.webp"));
    assert_eq!(separate.len(), 2);
    assert_eq!(separate[0].2, 70);
    assert_eq!(separate[1].2, 130);
    assert_eq!(
        animation_frames(&project.out_dir().join("apng-standard.webp")).len(),
        3
    );
}

#[test]
fn sixteen_bit_apng_is_rejected() {
    let project = Project::new("apng16");
    project.copy_fixture("apng-16bit.png");
    let run = project.run();
    assert_eq!(run.code, 1, "{}", log_paths(&project, &run));
    assert!(run.stderr.contains("16bit"), "{}", run.stderr);
    assert!(!project.out_dir().join("apng-16bit.webp").exists());
}

#[test]
fn default_image_only_apng_fails() {
    let project = Project::new("apng-default");
    project.copy_fixture("apng-default-only.png");
    let run = project.run();
    assert_eq!(run.code, 1, "{}", log_paths(&project, &run));
    assert!(
        run.stderr.contains("保存できるフレームがありません"),
        "{}",
        run.stderr
    );
}

#[test]
fn copies_keep_bytes_and_timestamps() {
    let project = Project::new("copy");
    for name in ["plain.jpg", "sample.webp"] {
        project.copy_fixture(name);
    }
    project.configure(&[("other_files", "true")]);
    project.write_input("movie.webm", b"WebM bytes");
    let run = project.run();
    assert_eq!(run.code, 0, "{}", log_paths(&project, &run));
    for name in ["plain.jpg", "sample.webp", "movie.webm"] {
        let source = project.inputs().join(name);
        let output = project.out_dir().join(name);
        assert_eq!(fs::read(&output).unwrap(), fs::read(&source).unwrap());
        assert_eq!(
            fs::metadata(&output).unwrap().modified().unwrap(),
            fs::metadata(&source).unwrap().modified().unwrap()
        );
    }
}

#[test]
fn copies_without_timestamps_use_current_time() {
    let project = Project::new("copy-time");
    let source = project.write_input("movie.webm", b"movie");
    let old = filetime::FileTime::from_unix_time(1_500_000_000, 0);
    filetime::set_file_times(&source, old, old).unwrap();
    project.configure(&[("other_files", "true"), ("timestamps", "false")]);
    let run = project.run();
    assert_eq!(run.code, 0, "{}", log_paths(&project, &run));
    let modified = fs::metadata(project.out_dir().join("movie.webm"))
        .unwrap()
        .modified()
        .unwrap();
    assert!(modified > std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_600_000_000));
}

#[test]
fn jpeg_copy_reuses_on_quality_change_and_converts_when_disabled() {
    let project = Project::new("jpeg-quality");
    project.configure(&[("jpeg", "true")]);
    project.copy_fixture("plain.jpg");
    let run = project.run();
    assert_eq!(run.code, 0, "{}", log_paths(&project, &run));
    assert!(run.stdout.contains("コピー 1"), "{}", run.stdout);

    project.configure(&[("jpeg", "true"), ("quality", "77")]);
    let run = project.run();
    assert_eq!(run.code, 0, "{}", log_paths(&project, &run));
    assert!(run.stdout.contains("再利用 1"), "{}", run.stdout);
    assert!(!project.out_dir().join("plain.webp").exists());

    project.configure(&[("jpeg", "false")]);
    let run = project.run();
    assert_eq!(run.code, 0, "{}", log_paths(&project, &run));
    assert!(run.stdout.contains("変換 1"), "{}", run.stdout);
    assert!(project.out_dir().join("plain.webp").exists());
}

#[test]
fn failed_conversion_keeps_existing_output() {
    let project = Project::new("failed");
    project.write_input("broken.png", b"not a PNG");
    fs::create_dir_all(project.out_dir()).unwrap();
    fs::write(project.out_dir().join("broken.webp"), b"existing").unwrap();
    let run = project.run();
    assert_eq!(run.code, 1, "{}", log_paths(&project, &run));
    assert_eq!(
        fs::read(project.out_dir().join("broken.webp")).unwrap(),
        b"existing"
    );
    let leftovers: Vec<String> = fs::read_dir(project.out_dir())
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .filter(|name| {
            name.starts_with(".img2webp-")
                && !name.starts_with(".img2webp-dev")
                && !name.contains(".sqlite3")
        })
        .collect();
    assert!(leftovers.is_empty(), "{leftovers:?}");
}

#[test]
fn resume_skips_completed_and_rebuilds_on_changes() {
    let project = Project::new("resume");
    project.copy_fixture("sample.png");
    let run = project.run();
    assert_eq!(run.code, 0, "{}", log_paths(&project, &run));
    assert!(run.stdout.contains("変換 1"), "{}", run.stdout);

    let run = project.run();
    assert_eq!(run.code, 0, "{}", log_paths(&project, &run));
    assert!(run.stdout.contains("再利用 1"), "{}", run.stdout);

    // New source file triggers only that one conversion.
    project.copy_fixture("plain-parameters.png");
    let run = project.run();
    assert_eq!(run.code, 0, "{}", log_paths(&project, &run));
    assert!(run.stdout.contains("変換 1"), "{}", run.stdout);
    assert!(run.stdout.contains("再利用 1"), "{}", run.stdout);

    // Changing the output outside the tool triggers a rebuild.
    fs::write(project.out_dir().join("sample.webp"), b"modified outside").unwrap();
    let run = project.run();
    assert_eq!(run.code, 0, "{}", log_paths(&project, &run));
    assert!(run.stdout.contains("変換 1"), "{}", run.stdout);
    assert!(
        image::load_from_memory(&fs::read(project.out_dir().join("sample.webp")).unwrap()).is_ok()
    );
}

#[test]
fn quality_change_rebuilds_conversions_but_reuses_copies() {
    let project = Project::new("quality");
    project.copy_fixture("sample.png");
    project.copy_fixture("sample.webp");
    let run = project.run();
    assert_eq!(run.code, 0, "{}", log_paths(&project, &run));
    project.configure(&[("quality", "55")]);
    let run = project.run();
    assert_eq!(run.code, 0, "{}", log_paths(&project, &run));
    assert!(run.stdout.contains("変換 1"), "{}", run.stdout);
    assert!(run.stdout.contains("再利用 1"), "{}", run.stdout);
}

#[test]
fn untracked_output_requires_adoption() {
    let project = Project::new("adopt");
    project.copy_fixture("sample.png");
    fs::create_dir_all(project.out_dir()).unwrap();
    let existing = project.out_dir().join("sample.webp");
    fs::write(&existing, b"old output").unwrap();
    project.configure(&[("overwrite_existing", "false")]);
    let run = project.run();
    assert_eq!(run.code, 0, "{}", log_paths(&project, &run));
    assert!(run.stdout.contains("上書きは無効"), "{}", run.stdout);
    assert_eq!(fs::read(&existing).unwrap(), b"old output");

    // Without a record the next run rebuilds the output.
    project.configure(&[("overwrite_existing", "true")]);
    let run = project.run();
    assert_eq!(run.code, 0, "{}", log_paths(&project, &run));
    assert!(run.stdout.contains("変換 1"), "{}", run.stdout);
    assert_ne!(fs::read(&existing).unwrap(), b"old output");

    // After removing the database an adopt run registers the output.
    fs::remove_file(project.db_path()).unwrap();
    let _ = fs::remove_file(project.db_path().with_extension("sqlite3-wal"));
    let _ = fs::remove_file(project.db_path().with_extension("sqlite3-shm"));
    project.configure(&[("adopt_untracked", "true")]);
    fs::write(&existing, b"adopted output").unwrap();
    let run = project.run();
    assert_eq!(run.code, 0, "{}", log_paths(&project, &run));
    assert!(run.stdout.contains("登録 1"), "{}", run.stdout);
    assert_eq!(fs::read(&existing).unwrap(), b"adopted output");
    let run = project.run();
    assert!(run.stdout.contains("再利用 1"), "{}", run.stdout);
}

#[test]
fn mismatched_extension_is_reported() {
    let project = Project::new("mismatch");
    project.write_input("wrong.png", &fs::read(fixture("plain.jpg")).unwrap());
    let run = project.run();
    assert_eq!(run.code, 1, "{}", log_paths(&project, &run));
    assert!(run.stderr.contains("ConversionError"), "{}", run.stderr);
    assert!(!project.out_dir().join("wrong.webp").exists());
}

#[test]
fn jpeg_orientation_and_cmyk() {
    let project = Project::new("jpeg-convert");
    project.configure(&[("jpeg", "false")]);
    project.copy_fixture("rotated.jpg");
    project.copy_fixture("cmyk.jpg");
    let run = project.run();
    assert_eq!(run.code, 0, "{}", log_paths(&project, &run));
    let rotated =
        image::load_from_memory(&fs::read(project.out_dir().join("rotated.webp")).unwrap())
            .unwrap();
    assert_eq!(rotated.dimensions(), (3, 2));
    let metadata = webp_metadata(&project.out_dir().join("rotated.webp"));
    if let Some(raw) = metadata.exif.as_deref() {
        let exif = Exif::parse(raw).unwrap();
        assert_eq!(exif.orientation(), None);
    }
    let cmyk =
        image::load_from_memory(&fs::read(project.out_dir().join("cmyk.webp")).unwrap()).unwrap();
    assert_eq!(cmyk.dimensions(), (4, 4));
}

#[test]
fn mpo_and_non_eight_bit_jpeg_are_rejected_when_converted() {
    let project = Project::new("jpeg-reject");
    project.configure(&[("jpeg", "false")]);
    project.copy_fixture("mpo.jpg");
    project.copy_fixture("precision12.jpg");
    project.copy_fixture("plain.jpg");
    let run = project.run();
    assert_eq!(run.code, 1, "{}", log_paths(&project, &run));
    assert!(run.stderr.contains("MPO"), "{}", run.stderr);
    assert!(run.stderr.contains("サンプル精度"), "{}", run.stderr);
    assert!(project.out_dir().join("plain.webp").exists());
    assert!(!project.out_dir().join("mpo.webp").exists());
}

#[test]
fn orientation_is_applied_even_when_metadata_is_disabled() {
    let project = Project::new("orientation-off");
    project.configure(&[("jpeg", "false"), ("metadata", "false")]);
    project.copy_fixture("rotated.jpg");
    let run = project.run();
    assert_eq!(run.code, 0, "{}", log_paths(&project, &run));
    let output = project.out_dir().join("rotated.webp");
    let decoded = image::load_from_memory(&fs::read(&output).unwrap()).unwrap();
    assert_eq!(decoded.dimensions(), (3, 2));
    assert!(webp_metadata(&output).exif.is_none());
}

#[test]
fn config_validation_errors_exit_two() {
    for (key, value) in [
        ("quality", "101"),
        ("db_commit_every", "0"),
        ("jpeg", "maybe"),
        ("missing_duration_ms", "0"),
        ("parallel_jobs", "0"),
        ("parallel_jobs", "-3"),
    ] {
        let project = Project::new("config");
        project.configure(&[(key, value)]);
        let run = project.run();
        assert_eq!(run.code, 2, "{key}={value} {}", log_paths(&project, &run));
        assert!(run.stderr.contains("設定エラー"), "{}", run.stderr);
    }
}

#[test]
fn first_run_creates_directories() {
    let project = Project::new("dirs");
    fs::remove_dir_all(project.inputs()).unwrap();
    let run = project.run();
    assert_eq!(run.code, 0, "{}", log_paths(&project, &run));
    assert!(project.inputs().is_dir());
    assert!(project.out_dir().is_dir());
    assert!(run.stdout.contains("失敗 0"), "{}", run.stdout);
}

#[test]
fn input_path_that_is_a_file_is_a_config_error() {
    let project = Project::new("input-file");
    fs::remove_dir_all(project.inputs()).unwrap();
    fs::write(project.inputs(), b"keep this file").unwrap();
    let run = project.run();
    assert_eq!(run.code, 2, "{}", log_paths(&project, &run));
    assert!(
        run.stderr.contains("入力フォルダにファイル"),
        "{}",
        run.stderr
    );
    assert_eq!(fs::read(project.inputs()).unwrap(), b"keep this file");
    assert!(!project.out_dir().exists());
}

#[test]
fn bad_config_encoding_reports_error_without_creating_directories() {
    let project = Project::new("config-encoding");
    fs::write(project.root().join("config.ini"), [0xffu8]).unwrap();
    fs::remove_dir_all(project.inputs()).unwrap();
    let run = project.run();
    assert_eq!(run.code, 2, "{}", log_paths(&project, &run));
    assert!(run.stderr.contains("設定エラー"), "{}", run.stderr);
    assert!(!project.inputs().exists());
    assert!(!project.out_dir().exists());
}

#[test]
fn cli_continues_after_a_bad_image() {
    let project = Project::new("continue");
    project.copy_fixture("sample.png");
    project.write_input("bad.png", b"bad image");
    let run = project.run();
    assert_eq!(run.code, 1, "{}", log_paths(&project, &run));
    assert!(run.stdout.contains("失敗 1"), "{}", run.stdout);
    assert!(run.stderr.contains("bad.png"), "{}", run.stderr);
    assert!(project.out_dir().join("sample.webp").exists());
}

#[test]
fn stealth_payloads_become_user_comments() {
    let project = Project::new("stealth");
    let names = [
        "stealth-8-6-alpha-zlib-4.png",
        "stealth-8-2-rgb-info-0.png",
        "stealth-16-6-alpha-gzip-2.png",
        "stealth-16-4-alpha-info-4.png",
        "stealth-16-2-rgb-info-0.png",
    ];
    for name in names {
        project.copy_fixture(name);
    }
    let run = project.run();
    assert_eq!(run.code, 0, "{}", log_paths(&project, &run));
    for (index, name) in names.iter().enumerate() {
        let stem = name.trim_end_matches(".png");
        let parts: Vec<&str> = stem.split('-').skip(1).collect();
        // Fixture text: "生成情報 <depth>-<color>-<mode>-<compression>-<filter>".
        let expected = format!("生成情報 {}", parts.join("-"));
        let output = project.out_dir().join(format!("{stem}.webp"));
        assert_eq!(
            user_comment(&output).as_deref(),
            Some(expected.as_str()),
            "{name}"
        );
        let _ = index;
    }
}

#[test]
fn dimension_and_pixel_limits_fail_the_job() {
    let project = Project::new("limits");
    project.write_input("wide.png", &header_only_png(16_384, 8, 8, 2));
    project.write_input("huge.png", &header_only_png(16_000, 16_000, 8, 2));
    let run = project.run();
    assert_eq!(run.code, 1, "{}", log_paths(&project, &run));
    assert!(run.stderr.contains("ConversionError"), "{}", run.stderr);
    assert!(
        run.stderr.contains("DecompressionBombError"),
        "{}",
        run.stderr
    );
    assert!(!project.out_dir().join("wide.webp").exists());
    assert!(!project.out_dir().join("huge.webp").exists());
}

#[test]
fn lossless_conversion_is_pixel_exact() {
    let project = Project::new("lossless");
    project.configure(&[("lossless", "true"), ("jpeg", "false")]);
    let source = project.copy_fixture("plain.jpg");
    let run = project.run();
    assert_eq!(run.code, 0, "{}", log_paths(&project, &run));
    let decoded =
        image::load_from_memory(&fs::read(project.out_dir().join("plain.webp")).unwrap()).unwrap();
    let input = image::open(&source).unwrap();
    assert_eq!(decoded.to_rgb8().into_raw(), input.to_rgb8().into_raw());
}

#[test]
fn parallel_jobs_setting_is_applied_and_clamped() {
    let project = Project::new("parallel");
    project.configure(&[("parallel_jobs", "2")]);
    for name in ["sample.png", "plain-parameters.png", "conflict.png"] {
        project.copy_fixture(name);
    }
    let run = project.run();
    assert_eq!(run.code, 0, "{}", log_paths(&project, &run));
    assert!(run.stdout.contains("変換 3"), "{}", run.stdout);

    let project = Project::new("parallel-clamp");
    project.configure(&[("parallel_jobs", "4096")]);
    project.copy_fixture("sample.png");
    let run = project.run();
    assert_eq!(run.code, 0, "{}", log_paths(&project, &run));
    assert!(run.stderr.contains("CPU コア数"), "{}", run.stderr);
}

#[test]
fn dev_and_release_build_paths_are_separated() {
    let project = Project::new("build-paths");
    project.copy_fixture("sample.png");
    let run = project.run();
    assert_eq!(run.code, 0, "{}", log_paths(&project, &run));
    if DEV {
        assert!(run.stderr.contains("開発ビルド"), "{}", run.stderr);
        assert!(project.out_dir().join("sample.webp").exists());
        assert!(project.out_dir().join(".img2webp-dev-output").exists());
        assert!(!project.root().join("outputs").join("sample.webp").exists());
    } else {
        assert!(!run.stderr.contains("開発ビルド"), "{}", run.stderr);
        assert!(project.root().join("outputs").join("sample.webp").exists());
        assert!(project
            .root()
            .join("outputs")
            .join(".img2webp.sqlite3")
            .exists());
    }
}

#[test]
fn overwrite_disabled_reports_broken_symlink_destination() {
    // A dangling symbolic link is treated as "not existing" for the pre-check
    // and replaced at the commit (design spec §3.8).
    let project = Project::new("symlink");
    project.copy_fixture("sample.png");
    fs::create_dir_all(project.out_dir()).unwrap();
    let link = project.out_dir().join("sample.webp");
    #[cfg(windows)]
    {
        if std::os::windows::fs::symlink_file(project.out_dir().join("missing-target"), &link)
            .is_err()
        {
            // Creating symlinks needs a privilege; nothing to verify then.
            return;
        }
    }
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(project.out_dir().join("missing-target"), &link).unwrap();
    }
    project.configure(&[("overwrite_existing", "false")]);
    let run = project.run();
    assert_eq!(run.code, 0, "{}", log_paths(&project, &run));
    let metadata = fs::symlink_metadata(&link).unwrap();
    assert!(!metadata.file_type().is_symlink());
    assert!(link.exists());
}

#[cfg(windows)]
#[test]
fn unc_paths_are_supported_for_input_and_output() {
    // Uses the administrative share of the drive that holds the temporary
    // directory; the test is skipped when the share is not reachable.
    let project = Project::new("unc");
    project.copy_fixture("text.png");
    let root = project.root().to_path_buf();
    let text = root.to_string_lossy().to_string();
    let Some(rest) = text.strip_prefix("C:\\") else {
        return;
    };
    if !text.starts_with("C:\\") {
        return;
    }
    let unc_root = format!("\\\\localhost\\C$\\{}", rest);
    if fs::create_dir_all(format!("{unc_root}\\inputs")).is_err() {
        return; // admin share unavailable
    }
    let mut config = include_str!("fixtures/config-fixture.ini").to_string();
    config = config.replace(
        "input_dir = inputs",
        &format!("input_dir = {unc_root}\\inputs"),
    );
    config = config.replace(
        "output_dir = outputs",
        &format!("output_dir = {unc_root}\\outputs"),
    );
    fs::write(project.root().join("config.ini"), config).unwrap();
    let run = project.run();
    assert_eq!(run.code, 0, "{}", log_paths(&project, &run));
    assert!(project.out_dir().join("text.webp").exists());
    // The status database records POSIX-style relative paths.
    let connection = rusqlite::Connection::open(project.db_path()).unwrap();
    let rows: Vec<(String, String)> = connection
        .prepare("SELECT source_path, destination_path FROM completed_jobs")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .flatten()
        .collect();
    assert_eq!(
        rows,
        vec![("text.png".to_string(), "text.webp".to_string())]
    );
}
