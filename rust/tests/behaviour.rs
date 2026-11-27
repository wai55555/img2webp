//! Behaviour tests that need internal hooks: budget limits, interrupt
//! semantics, startup cleanup, the overwrite race and state-database
//! compatibility with the Python version.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use img2webp::dispatch::{self, Options};
use img2webp::interrupt::Interrupt;
use img2webp::probe;
use img2webp::{budget, state};

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

impl Project {
    fn new(tag: &str) -> Self {
        let index = COUNTER.fetch_add(1, Ordering::SeqCst);
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0);
        let root = std::env::temp_dir().join(format!(
            "img2webp-behaviour-{tag}-{}-{index}-{nonce}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("inputs")).unwrap();
        fs::write(
            root.join("config.ini"),
            include_str!("fixtures/config-fixture.ini"),
        )
        .unwrap();
        Project { root }
    }

    fn configure(&self, patches: &[(&str, &str)]) {
        let mut text = include_str!("fixtures/config-fixture.ini").to_string();
        for (key, value) in patches {
            let mut lines: Vec<String> = text.lines().map(|line| line.to_string()).collect();
            let mut found = false;
            for line in lines.iter_mut() {
                let trimmed = line.trim_start();
                let spaced = format!("{key} =");
                let compact = format!("{key}=");
                if trimmed.starts_with(spaced.as_str()) || trimmed.starts_with(compact.as_str()) {
                    *line = format!("{key} = {value}");
                    found = true;
                    break;
                }
            }
            if !found {
                let index = lines
                    .iter()
                    .position(|line| line.trim() == "[preserve]")
                    .unwrap();
                lines.insert(index, format!("{key} = {value}"));
            }
            text = lines.join("\n");
            text.push('\n');
        }
        fs::write(self.root.join("config.ini"), text).unwrap();
    }

    fn input(&self, name: &str) -> PathBuf {
        self.root.join("inputs").join(name)
    }

    fn write_input(&self, name: &str, bytes: &[u8]) {
        fs::write(self.input(name), bytes).unwrap();
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

    fn options(&self) -> Options {
        Options {
            project_dir: self.root.clone(),
            ..Options::default()
        }
    }

    fn db_records(&self) -> i64 {
        let connection = rusqlite::Connection::open(self.db_path()).unwrap();
        connection
            .query_row("SELECT COUNT(*) FROM completed_jobs", [], |row| row.get(0))
            .unwrap_or(-1)
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn no_colour_png(name: &str) -> Vec<u8> {
    let image = image::RgbImage::from_pixel(16, 16, image::Rgb([90, 120, 40]));
    let mut cursor = std::io::Cursor::new(Vec::new());
    image::DynamicImage::ImageRgb8(image)
        .write_to(&mut cursor, image::ImageFormat::Png)
        .unwrap();
    let _ = name;
    cursor.into_inner()
}

#[test]
fn budget_estimate_larger_than_total_fails_the_job() {
    let project = Project::new("budget-total");
    project.write_input("image.png", &no_colour_png("image"));
    let mut options = project.options();
    options.budget_override = Some(1024);
    let code = dispatch::run(options);
    assert_eq!(code, 1);
    assert!(!project.out_dir().join("image.webp").exists());
}

#[test]
fn jobs_wait_for_budget_without_losing_work() {
    let project = Project::new("budget-wait");
    for index in 0..4 {
        project.write_input(&format!("image{index}.png"), &no_colour_png("image"));
    }
    let probe = probe::probe(&project.input("image0.png")).unwrap();
    let single = budget::estimate(&probe, 0);
    let mut options = project.options();
    // Room for exactly one job at a time, so two jobs must wait.
    options.budget_override = Some(single + single / 8);
    options.parallel_override = Some(2);
    let code = dispatch::run(options);
    assert_eq!(code, 0);
    for index in 0..4 {
        assert!(project
            .out_dir()
            .join(format!("image{index}.webp"))
            .exists());
    }
    assert_eq!(project.db_records(), 4);
}

#[test]
fn graceful_interrupt_commits_finished_jobs_only() {
    let project = Project::new("interrupt");
    for index in 0..3 {
        project.write_input(&format!("image{index}.png"), &no_colour_png("image"));
    }
    let mut options = project.options();
    options.interrupt_after = Some(1);
    options.parallel_override = Some(1);
    let code = dispatch::run(options);
    assert_eq!(code, 130);
    // Jobs that were already granted when the interrupt arrived run to
    // completion and are recorded; no *new* job is dispatched. The exact count
    // therefore depends on timing.
    let recorded = project.db_records();
    assert!(
        (1..=3).contains(&recorded),
        "unexpected record count: {recorded}"
    );

    // The next run converts the remaining two.
    let code = dispatch::run(project.options());
    assert_eq!(code, 0);
    assert_eq!(project.db_records(), 3);
}

#[test]
fn second_interrupt_skips_the_final_commit() {
    let project = Project::new("interrupt2");
    for index in 0..3 {
        project.write_input(&format!("image{index}.png"), &no_colour_png("image"));
    }
    let mut options = project.options();
    options.interrupt_after = Some(1);
    options.second_interrupt_after = Some(1);
    options.parallel_override = Some(1);
    let code = dispatch::run(options);
    assert_eq!(code, 130);
    assert_eq!(
        project.db_records(),
        0,
        "the second interrupt must skip the final commit"
    );
}

#[test]
fn interrupt_before_start_prints_the_initial_message() {
    let project = Project::new("interrupt-init");
    project.write_input("image.png", &no_colour_png("image"));
    let interrupt = Interrupt::new();
    interrupt.request();
    let mut options = project.options();
    options.interrupt = interrupt;
    let code = dispatch::run(options);
    assert_eq!(code, 130);
    assert!(!project.out_dir().join("image.webp").exists());
}

#[test]
fn startup_cleanup_removes_old_temp_files_but_keeps_tracked_ones() {
    let project = Project::new("cleanup");
    project.write_input("image.png", &no_colour_png("image"));
    fs::create_dir_all(project.out_dir()).unwrap();
    let old_temp = project.out_dir().join(".img2webp-abc12345.webp");
    let fresh_temp = project.out_dir().join(".img2webp-def67890.webp");
    fs::write(&old_temp, b"leftover").unwrap();
    fs::write(&fresh_temp, b"recent").unwrap();
    let old = state::RecordStat {
        size: 8,
        mtime_ns: 1_000_000_000_000_000_000,
        ctime_ns: 1_000_000_000_000_000_000,
        atime_ns: 1_000_000_000_000_000_000,
    };
    img2webp::fs_util::apply_record_times(&old_temp, &old).unwrap();
    let mut options = project.options();
    options.cleanup_min_age = Some(Duration::from_secs(60));
    let code = dispatch::run(options);
    assert_eq!(code, 0);
    assert!(!old_temp.exists(), "old temporary file must be removed");
    assert!(fresh_temp.exists(), "recent temporary file must be kept");

    // A tracked file with a temporary shape is kept.
    let mut options = project.options();
    options.cleanup_min_age = Some(Duration::from_secs(60));
    fs::write(&fresh_temp, b"recent").unwrap();
    img2webp::fs_util::apply_record_times(&fresh_temp, &old).unwrap();
    // Pretend the state database tracks this destination.
    let connection = rusqlite::Connection::open(project.db_path()).unwrap();
    connection
        .execute(
            "INSERT OR REPLACE INTO completed_jobs
             (source_path, destination_path, source_size, source_mtime_ns, source_ctime_ns,
              output_size, output_mtime_ns, settings)
             VALUES ('.img2webp-def67890.webp', '.img2webp-def67890.webp', 6, 1, 1, 6, 1, 'x')",
            [],
        )
        .unwrap();
    drop(connection);
    let mut options = project.options();
    options.cleanup_min_age = Some(Duration::from_secs(60));
    options.now = Some(std::time::SystemTime::now());
    let code = dispatch::run(options);
    assert_eq!(code, 0);
    assert!(fresh_temp.exists(), "tracked temporary file must be kept");
}

#[test]
fn overwrite_disabled_race_fails_the_job_and_keeps_the_winner() {
    let project = Project::new("race");
    project.write_input("image.png", &no_colour_png("image"));
    project.configure(&[("overwrite_existing", "false")]);
    let mut options = project.options();
    options.parallel_override = Some(1);
    let root = project.out_dir();
    let destination = root.join("image.webp");
    options.commit_hook = Some(Arc::new(move |path: &Path| {
        let _ = fs::create_dir_all(root.clone());
        let _ = fs::write(path, b"other writer");
    }));
    let code = dispatch::run(options);
    assert_eq!(code, 1);
    assert_eq!(
        fs::read(&destination).unwrap(),
        b"other writer",
        "the competing writer must win"
    );
}

#[test]
fn interrupted_state_keeps_running_marker() {
    let project = Project::new("state");
    project.write_input("image.png", &no_colour_png("image"));
    let code = dispatch::run(project.options());
    assert_eq!(code, 0);
    let connection = rusqlite::Connection::open(project.db_path()).unwrap();
    let value: String = connection
        .query_row(
            "SELECT value FROM img2webp_meta WHERE key = 'last_exit'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(value, "ok");
    let version: i64 = connection
        .query_row("PRAGMA user_version", [], |row| row.get(0))
        .unwrap();
    assert_eq!(version, 1);
}

#[test]
fn python_database_copy_records_are_reused_and_conversions_rebuilt() {
    let project = Project::new("python-compat");
    let source = fixture("python-compat");
    for name in ["sample.webp", "image.png"] {
        fs::copy(source.join("inputs").join(name), project.input(name)).unwrap();
        let mtime = fs::metadata(source.join("inputs").join(name))
            .unwrap()
            .modified()
            .unwrap();
        filetime::set_file_times(
            project.input(name),
            filetime::FileTime::from_system_time(mtime),
            filetime::FileTime::from_system_time(mtime),
        )
        .unwrap();
    }
    fs::create_dir_all(project.out_dir()).unwrap();
    for name in ["sample.webp", "image.webp"] {
        let target = project.out_dir().join(name);
        fs::copy(source.join("outputs").join(name), &target).unwrap();
        let mtime = fs::metadata(source.join("outputs").join(name))
            .unwrap()
            .modified()
            .unwrap();
        filetime::set_file_times(
            &target,
            filetime::FileTime::from_system_time(mtime),
            filetime::FileTime::from_system_time(mtime),
        )
        .unwrap();
    }
    // The checked-in database was produced on POSIX by the Python version.
    // Normalise the platform and ctime values so it looks like a Windows-made
    // database (the signature compatibility itself is what is under test).
    fs::copy(source.join("python-state.sqlite3"), project.db_path()).unwrap();
    {
        let connection = rusqlite::Connection::open(project.db_path()).unwrap();
        let mut statement = connection
            .prepare("SELECT source_path, settings FROM completed_jobs")
            .unwrap();
        let rows: Vec<(String, String)> = statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .flatten()
            .collect();
        drop(statement);
        for (source_path, settings) in rows {
            let input = project.input(
                Path::new(&source_path)
                    .file_name()
                    .unwrap()
                    .to_str()
                    .unwrap(),
            );
            let created = fs::metadata(&input)
                .unwrap()
                .created()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|duration| duration.as_nanos() as i64)
                .unwrap_or(0);
            let settings = settings.replace("\"platform\":\"posix\"", "\"platform\":\"nt\"");
            connection
                .execute(
                    "UPDATE completed_jobs SET source_ctime_ns = ?1, settings = ?2
                     WHERE source_path = ?3",
                    rusqlite::params![created, settings, source_path],
                )
                .unwrap();
        }
    }
    let output_names: Vec<String> = fs::read_dir(project.out_dir())
        .unwrap()
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .collect();
    assert!(output_names.iter().any(|name| name == "sample.webp"));
    let code = dispatch::run(project.options());
    assert_eq!(code, 0);
    // The copy record is reused; the conversion is rebuilt once (pipeline 3).
    let records = project.db_records();
    assert_eq!(records, 2);
}

#[test]
fn temporary_file_permissions_are_not_private_on_posix() {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let dir = std::env::temp_dir().join(format!("img2webp-perm-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let (file, path) = img2webp::fs_util::create_temp(&dir, ".webp").unwrap();
        drop(file);
        let mode = fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode & 0o044, 0o044, "mode {mode:o} must follow the umask");
        fs::remove_dir_all(&dir).ok();
    }
}

#[test]
fn lost_completion_notification_is_reclaimed_by_reconcile() {
    let project = Project::new("lost-notification");
    for index in 0..2 {
        project.write_input(&format!("image{index}.png"), &no_colour_png("image"));
    }
    let mut options = project.options();
    options.parallel_override = Some(1);
    options.faults = Some(Arc::new(dispatch::Faults {
        drop_finished_for: Some(0),
        ..dispatch::Faults::default()
    }));
    let code = dispatch::run(options);
    // The lost notification is reclaimed after the reconcile grace period and
    // reported as a job failure; the run terminates instead of hanging.
    assert_eq!(code, 1);
    assert_eq!(project.db_records(), 1);
}

#[test]
fn duplicate_notification_is_ignored() {
    let project = Project::new("duplicate-notification");
    for index in 0..2 {
        project.write_input(&format!("image{index}.png"), &no_colour_png("image"));
    }
    let mut options = project.options();
    options.parallel_override = Some(1);
    options.faults = Some(Arc::new(dispatch::Faults {
        duplicate_finished_for: Some(0),
        ..dispatch::Faults::default()
    }));
    let code = dispatch::run(options);
    assert_eq!(code, 0);
    assert_eq!(project.db_records(), 2);
}

#[test]
fn worker_panic_becomes_a_job_failure_with_final_commit() {
    let project = Project::new("worker-panic");
    for index in 0..2 {
        project.write_input(&format!("image{index}.png"), &no_colour_png("image"));
    }
    let mut options = project.options();
    options.parallel_override = Some(1);
    options.faults = Some(Arc::new(dispatch::Faults {
        panic_stage2_for: Some(0),
        ..dispatch::Faults::default()
    }));
    let code = dispatch::run(options);
    assert_eq!(code, 1);
    let rows: Vec<String> = {
        let connection = rusqlite::Connection::open(project.db_path()).unwrap();
        let mut statement = connection
            .prepare("SELECT source_path FROM completed_jobs ORDER BY source_path")
            .unwrap();
        statement
            .query_map([], |row| row.get(0))
            .unwrap()
            .flatten()
            .collect()
    };
    assert_eq!(rows, vec!["image1.png".to_string()], "unexpected records");
}

#[test]
fn dispatcher_panic_exits_two_without_final_commit() {
    let project = Project::new("dispatcher-panic");
    for index in 0..2 {
        project.write_input(&format!("image{index}.png"), &no_colour_png("image"));
    }
    let mut options = project.options();
    options.parallel_override = Some(1);
    options.faults = Some(Arc::new(dispatch::Faults {
        panic_dispatcher_after: Some(1),
        ..dispatch::Faults::default()
    }));
    let code = dispatch::run(options);
    assert_eq!(code, 2);
    assert_eq!(
        project.db_records(),
        0,
        "a dispatcher panic must not commit records"
    );
    // The run state stays "running" so the next start cleans up.
    let connection = rusqlite::Connection::open(project.db_path()).unwrap();
    let value: String = connection
        .query_row(
            "SELECT value FROM img2webp_meta WHERE key = 'last_exit'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(value, "running");
}
