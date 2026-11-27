//! SQLite state database: reuse records, migration-compatible schema and the
//! run-state meta table (design spec §1.5, §3.7, §3.8).

use std::collections::{BTreeMap, HashSet};
use std::fmt;
use std::path::Path;
use std::time::Duration;

use rusqlite::Connection;

use crate::config::Config;
use crate::paths;
use crate::plan::{Action, Job};

pub const STATE_DATABASE_NAME: &str = ".img2webp.sqlite3";
pub const DEV_STATE_DATABASE_NAME: &str = ".img2webp-dev.sqlite3";
pub const DEV_MARKER_NAME: &str = ".img2webp-dev-output";
pub const DEV_MARKER_CONTENT: &str = "img2webp development build output folder";
pub const PIPELINE_COPY: i64 = 2;
pub const PIPELINE_CONVERT: i64 = 4;

#[derive(Debug)]
pub struct DbError(pub String);

impl fmt::Display for DbError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl From<rusqlite::Error> for DbError {
    fn from(error: rusqlite::Error) -> Self {
        DbError(error.to_string())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobRecord {
    pub destination_path: String,
    pub source_size: i64,
    pub source_mtime_ns: i64,
    pub source_ctime_ns: i64,
    pub output_size: i64,
    pub output_mtime_ns: i64,
    pub settings: String,
}

/// True when SQLite failed to create the WAL shared-memory file (network
/// shares). Other I/O errors are reported normally.
fn is_shared_memory_failure(error: &rusqlite::Error) -> bool {
    const SQLITE_IOERR: i32 = 10;
    const SQLITE_IOERR_SHMSIZE: i32 = 36 << 8 | SQLITE_IOERR;
    const SQLITE_IOERR_SHMLOCK: i32 = 20 << 8 | SQLITE_IOERR;
    const SQLITE_IOERR_SHMMAP: i32 = 21 << 8 | SQLITE_IOERR;
    const SQLITE_IOERR_SHMOPEN: i32 = 34 << 8 | SQLITE_IOERR;
    match error {
        rusqlite::Error::SqliteFailure(failure, _) => matches!(
            failure.extended_code,
            SQLITE_IOERR_SHMSIZE
                | SQLITE_IOERR_SHMLOCK
                | SQLITE_IOERR_SHMMAP
                | SQLITE_IOERR_SHMOPEN
        ),
        _ => false,
    }
}

fn initialize_schema(connection: &Connection, begun: &mut bool) -> Result<(), rusqlite::Error> {
    let version: i64 = connection.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if version != 0 && version != 1 {
        return Err(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_ERROR),
            Some(format!("未対応の処理履歴DBバージョンです: {version}")),
        ));
    }
    connection.execute(
        "CREATE TABLE IF NOT EXISTS completed_jobs (
            source_path TEXT PRIMARY KEY,
            destination_path TEXT NOT NULL,
            source_size INTEGER NOT NULL,
            source_mtime_ns INTEGER NOT NULL,
            source_ctime_ns INTEGER NOT NULL,
            output_size INTEGER NOT NULL,
            output_mtime_ns INTEGER NOT NULL,
            settings TEXT NOT NULL
        )",
        [],
    )?;
    connection.execute(
        "CREATE TABLE IF NOT EXISTS img2webp_meta (
            key TEXT PRIMARY KEY,
            value TEXT NOT NULL
        )",
        [],
    )?;
    connection.execute("PRAGMA user_version = 1", [])?;
    if !*begun {
        connection.execute_batch("BEGIN")?;
        *begun = true;
    }
    Ok(())
}

#[derive(Debug, Clone, Copy)]
pub struct RecordStat {
    pub size: u64,
    pub mtime_ns: i128,
    pub ctime_ns: i128,
    pub atime_ns: i128,
}

impl RecordStat {
    pub fn from_metadata(metadata: &std::fs::Metadata) -> Self {
        RecordStat {
            size: metadata.len(),
            mtime_ns: crate::fs_util::mtime_ns(metadata).unwrap_or(0),
            ctime_ns: crate::fs_util::ctime_ns(metadata).unwrap_or(0),
            atime_ns: crate::fs_util::atime_ns(metadata).unwrap_or(0),
        }
    }
}

fn json_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out
}

fn json_value(value: &JsonValue) -> String {
    match value {
        JsonValue::Bool(value) => value.to_string(),
        JsonValue::Int(value) => value.to_string(),
        JsonValue::Text(text) => format!("\"{}\"", json_escape(text)),
    }
}

enum JsonValue {
    Bool(bool),
    Int(i64),
    Text(String),
}

fn render_json(entries: &BTreeMap<&str, JsonValue>) -> String {
    let mut out = String::from("{");
    for (index, (key, value)) in entries.iter().enumerate() {
        if index > 0 {
            out.push(',');
        }
        out.push('"');
        out.push_str(key);
        out.push_str("\":");
        out.push_str(&json_value(value));
    }
    out.push('}');
    out
}

fn platform_name() -> &'static str {
    if cfg!(windows) {
        "nt"
    } else {
        "posix"
    }
}

/// Settings signature. Copy jobs must stay byte-identical to the Python
/// implementation so their records keep being reused after migration.
pub fn settings_signature(job: &Job, config: &Config) -> String {
    let mut entries: BTreeMap<&str, JsonValue> = BTreeMap::new();
    entries.insert("action", JsonValue::Text(job.action.as_str().to_string()));
    entries.insert("platform", JsonValue::Text(platform_name().to_string()));
    entries.insert("timestamps", JsonValue::Bool(config.preserve_timestamps));
    match job.action {
        Action::Copy => {
            entries.insert("pipeline", JsonValue::Int(PIPELINE_COPY));
        }
        Action::Convert => {
            entries.insert("pipeline", JsonValue::Int(PIPELINE_CONVERT));
            entries.insert("quality", JsonValue::Int(config.quality));
            entries.insert("metadata", JsonValue::Bool(config.preserve_metadata));
            // Stealth scanning policy changes the generated text, so it is part
            // of the identity (pipeline 4; see §4.2 `stealth`).
            entries.insert("stealth", JsonValue::Bool(config.stealth));
            entries.insert(
                "stealth_max_pixels",
                JsonValue::Int(config.stealth_max_pixels),
            );
            entries.insert("lossless", JsonValue::Bool(config.lossless));
            entries.insert("exact", JsonValue::Bool(config.exact));
            entries.insert("method_still", JsonValue::Int(config.method_still()));
            entries.insert(
                "method_animation",
                JsonValue::Int(config.method_animation()),
            );
            entries.insert(
                "dependencies",
                JsonValue::Text(env!("IMG2WEBP_DEP_VERSIONS").to_string()),
            );
            entries.insert(
                "libwebp",
                JsonValue::Text(crate::webp_encode::libwebp_version()),
            );
            let suffix = paths::path_suffix(&job.relative_source).to_lowercase();
            if suffix == ".png" || suffix == ".gif" {
                entries.insert(
                    "missing_duration_ms",
                    JsonValue::Int(config.missing_duration_ms),
                );
            }
        }
    }
    render_json(&entries)
}

pub struct StateDb {
    connection: Connection,
    pending: u32,
    commit_every: u32,
    in_transaction: bool,
    wal_enabled: bool,
}

fn tracked_destination_key(path: &str) -> String {
    paths::casefold(path)
}

impl StateDb {
    pub fn open(path: &Path, commit_every: u32) -> Result<Self, DbError> {
        let connection = Connection::open(path)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        let mode: String =
            connection.query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))?;
        let mut wal_enabled = mode.eq_ignore_ascii_case("wal");
        connection.execute("PRAGMA synchronous = NORMAL", [])?;
        let mut begun = false;
        if let Err(error) = initialize_schema(&connection, &mut begun) {
            // A network share can refuse the WAL shared memory file; the
            // database still works with the rollback journal, so fall back
            // instead of failing (Phase 0 UNC finding).
            if wal_enabled && is_shared_memory_failure(&error) {
                let _: String =
                    connection.query_row("PRAGMA journal_mode = DELETE", [], |row| row.get(0))?;
                wal_enabled = false;
                initialize_schema(&connection, &mut begun)?;
            } else {
                return Err(error.into());
            }
        }
        Ok(StateDb {
            connection,
            pending: 0,
            commit_every: commit_every.max(1),
            in_transaction: begun,
            wal_enabled,
        })
    }

    /// False when the output folder could only be opened with the rollback
    /// journal (a network share without WAL shared memory).
    pub fn wal_enabled(&self) -> bool {
        self.wal_enabled
    }

    pub fn get(&self, job: &Job) -> Result<Option<JobRecord>, DbError> {
        let mut statement = self.connection.prepare(
            "SELECT destination_path, source_size, source_mtime_ns, source_ctime_ns,
                    output_size, output_mtime_ns, settings
             FROM completed_jobs WHERE source_path = ?1",
        )?;
        let mut rows = statement.query([job.relative_source_posix()])?;
        match rows.next()? {
            Some(row) => Ok(Some(JobRecord {
                destination_path: row.get(0)?,
                source_size: row.get(1)?,
                source_mtime_ns: row.get(2)?,
                source_ctime_ns: row.get(3)?,
                output_size: row.get(4)?,
                output_mtime_ns: row.get(5)?,
                settings: row.get(6)?,
            })),
            None => Ok(None),
        }
    }

    pub fn record(
        &mut self,
        job: &Job,
        config: &Config,
        source: &RecordStat,
        output: &RecordStat,
    ) -> Result<(), DbError> {
        let destination = job
            .destination
            .strip_prefix(&config.output_dir)
            .map(paths::to_posix)
            .unwrap_or_else(|_| paths::to_posix(&job.destination));
        self.connection.execute(
            "INSERT INTO completed_jobs
             (source_path, destination_path, source_size, source_mtime_ns,
              source_ctime_ns, output_size, output_mtime_ns, settings)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(source_path) DO UPDATE SET
              destination_path = excluded.destination_path,
              source_size = excluded.source_size,
              source_mtime_ns = excluded.source_mtime_ns,
              source_ctime_ns = excluded.source_ctime_ns,
              output_size = excluded.output_size,
              output_mtime_ns = excluded.output_mtime_ns,
              settings = excluded.settings",
            rusqlite::params![
                job.relative_source_posix(),
                destination,
                source.size as i64,
                source.mtime_ns as i64,
                source.ctime_ns as i64,
                output.size as i64,
                output.mtime_ns as i64,
                settings_signature(job, config),
            ],
        )?;
        self.pending += 1;
        Ok(())
    }

    pub fn commit(&mut self) -> Result<(), DbError> {
        if self.in_transaction {
            self.connection.execute_batch("COMMIT")?;
            self.in_transaction = false;
        }
        self.pending = 0;
        Ok(())
    }

    fn begin(&mut self) -> Result<(), DbError> {
        if !self.in_transaction {
            self.connection.execute_batch("BEGIN")?;
            self.in_transaction = true;
        }
        Ok(())
    }

    pub fn commit_if_due(&mut self) -> Result<(), DbError> {
        if self.pending >= self.commit_every {
            self.commit()?;
            self.begin()?;
        }
        Ok(())
    }

    pub fn tracked_destinations(&self) -> Result<HashSet<String>, DbError> {
        let mut statement = self
            .connection
            .prepare("SELECT destination_path FROM completed_jobs")?;
        let mut rows = statement.query([])?;
        let mut out = HashSet::new();
        while let Some(row) = rows.next()? {
            let value: String = row.get(0)?;
            out.insert(tracked_destination_key(&value));
        }
        Ok(out)
    }

    pub fn completed_count(&self) -> Result<i64, DbError> {
        let count: i64 =
            self.connection
                .query_row("SELECT COUNT(*) FROM completed_jobs", [], |row| row.get(0))?;
        Ok(count)
    }

    pub fn read_meta(&self, key: &str) -> Result<Option<String>, DbError> {
        let mut statement = self
            .connection
            .prepare("SELECT value FROM img2webp_meta WHERE key = ?1")?;
        let mut rows = statement.query([key])?;
        match rows.next()? {
            Some(row) => Ok(Some(row.get(0)?)),
            None => Ok(None),
        }
    }

    /// Writes and immediately commits a meta value (single statement).
    pub fn write_meta(&mut self, key: &str, value: &str) -> Result<(), DbError> {
        self.commit()?;
        self.connection.execute(
            "INSERT INTO img2webp_meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [key, value],
        )?;
        self.begin()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn fixture_config() -> (Config, tempdir::Dir) {
        let dir = tempdir::Dir::new("state");
        let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests")
            .join("fixtures")
            .join("config-fixture.ini");
        let text = std::fs::read_to_string(path).unwrap();
        std::fs::write(dir.path().join("config.ini"), text).unwrap();
        let config = crate::config::load(&dir.path().join("config.ini"), dir.path()).unwrap();
        (config, dir)
    }

    fn job(config: &Config, name: &str, action: Action) -> Job {
        Job {
            source: config.input_dir.join(name),
            relative_source: PathBuf::from(name),
            action,
            destination: config.output_dir.join(name),
        }
    }

    mod tempdir {
        pub struct Dir(std::path::PathBuf);
        impl Dir {
            pub fn new(tag: &str) -> Self {
                let path = std::env::temp_dir().join(format!(
                    "img2webp-{tag}-{}-{:?}",
                    std::process::id(),
                    std::thread::current().id()
                ));
                std::fs::create_dir_all(&path).unwrap();
                Dir(path)
            }
            pub fn path(&self) -> &std::path::Path {
                &self.0
            }
        }
        impl Drop for Dir {
            fn drop(&mut self) {
                std::fs::remove_dir_all(&self.0).ok();
            }
        }
    }

    #[test]
    fn copy_signature_matches_python_bytes() {
        let (config, _dir) = fixture_config();
        let job = job(&config, "photo.jpg", Action::Copy);
        assert_eq!(
            settings_signature(&job, &config),
            "{\"action\":\"copy\",\"pipeline\":2,\"platform\":\"nt\",\"timestamps\":true}"
        );
    }

    #[test]
    fn convert_signature_shape_is_pinned() {
        let (config, _dir) = fixture_config();
        let png_job = job(&config, "photo.png", Action::Convert);
        let expected = format!(
            "{{\"action\":\"convert\",\"dependencies\":\"{}\",\"exact\":false,\"libwebp\":\"{}\",\"lossless\":false,\"metadata\":true,\"method_animation\":0,\"method_still\":4,\"missing_duration_ms\":80,\"pipeline\":4,\"platform\":\"{}\",\"quality\":90,\"stealth\":true,\"stealth_max_pixels\":16777216,\"timestamps\":true}}",
            env!("IMG2WEBP_DEP_VERSIONS"),
            crate::webp_encode::libwebp_version(),
            if cfg!(windows) { "nt" } else { "posix" },
        );
        assert_eq!(settings_signature(&png_job, &config), expected);
        let jpeg_job = job(&config, "photo.jpg", Action::Convert);
        let expected_jpeg = expected.replace(",\"missing_duration_ms\":80", "");
        assert_eq!(settings_signature(&jpeg_job, &config), expected_jpeg);
    }

    #[test]
    fn convert_signature_contains_identity_and_settings() {
        let (config, _dir) = fixture_config();
        let png_job = job(&config, "photo.png", Action::Convert);
        let signature = settings_signature(&png_job, &config);
        assert!(signature.starts_with("{\"action\":\"convert\","));
        assert!(signature.contains("\"pipeline\":4"));
        assert!(signature.contains("\"quality\":90"));
        assert!(signature.contains("\"missing_duration_ms\":80"));
        assert!(signature.contains("\"method_still\":4"));
        assert!(signature.contains("\"method_animation\":0"));
        assert!(signature.contains("\"libwebp\":"));
        assert!(!signature.contains("  "));
        let jpeg = job(&config, "photo.jpeg", Action::Convert);
        assert!(!settings_signature(&jpeg, &config).contains("missing_duration_ms"));
    }

    #[test]
    fn records_round_trip_and_count() {
        let (config, dir) = fixture_config();
        std::fs::create_dir_all(&config.output_dir).unwrap();
        let mut db = StateDb::open(&dir.path().join("state.sqlite3"), 32).unwrap();
        let job = job(&config, "a.png", Action::Convert);
        let source = RecordStat {
            size: 10,
            mtime_ns: 1,
            ctime_ns: 2,
            atime_ns: 3,
        };
        let output = RecordStat {
            size: 20,
            mtime_ns: 4,
            ctime_ns: 5,
            atime_ns: 6,
        };
        db.record(&job, &config, &source, &output).unwrap();
        db.commit().unwrap();
        let record = db.get(&job).unwrap().unwrap();
        assert_eq!(record.source_size, 10);
        assert_eq!(record.output_size, 20);
        assert_eq!(record.settings, settings_signature(&job, &config));
        assert_eq!(db.completed_count().unwrap(), 1);
        db.write_meta("last_exit", "ok").unwrap();
        assert_eq!(db.read_meta("last_exit").unwrap().as_deref(), Some("ok"));
        assert!(db.tracked_destinations().unwrap().contains("a.png"));
    }

    #[test]
    fn accepts_user_version_zero_and_rejects_others() {
        let dir = tempdir::Dir::new("state-version-zero");
        let path = dir.path().join("state.sqlite3");
        StateDb::open(&path, 32).unwrap();
        assert_eq!(
            {
                let connection = Connection::open(&path).unwrap();
                connection
                    .query_row("PRAGMA user_version", [], |row| row.get::<_, i64>(0))
                    .unwrap()
            },
            1
        );
        for version in [2i64, -1] {
            let dir = tempdir::Dir::new(&format!("state-version-{version}"));
            let path = dir.path().join("state.sqlite3");
            {
                let connection = Connection::open(&path).unwrap();
                connection
                    .execute(&format!("PRAGMA user_version = {version}"), [])
                    .unwrap();
            }
            let error = StateDb::open(&path, 32).err().unwrap();
            assert!(
                error
                    .0
                    .contains(&format!("未対応の処理履歴DBバージョンです: {version}")),
                "{}",
                error.0
            );
        }
    }

    #[test]
    fn rejects_unknown_user_version() {
        let dir = tempdir::Dir::new("state-version");
        let path = dir.path().join("state.sqlite3");
        {
            let connection = Connection::open(&path).unwrap();
            connection.execute("PRAGMA user_version = 7", []).unwrap();
        }
        let error = StateDb::open(&path, 32).err().unwrap();
        assert!(error.0.contains("未対応の処理履歴DBバージョンです: 7"));
    }
}
