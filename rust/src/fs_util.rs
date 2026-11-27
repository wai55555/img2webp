//! Temporary files, atomic replacement, timestamps and startup cleanup.
//!
//! Design spec §3.8: temp files use umask permissions (BUG-04), the
//! `overwrite_existing = false` commit is an atomic no-replace move (BUG-05),
//! Windows creation times are preserved, and leftovers from an abnormal exit
//! are removed on startup.

use std::collections::HashSet;
use std::fs::{File, Metadata, OpenOptions};
use std::io::{self, BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::interrupt::Interrupt;
use crate::paths;

pub const TEMP_PREFIX: &str = ".img2webp-";
const TEMP_RANDOM_LEN: usize = 8;
const COPY_BUFFER: usize = 64 * 1024;

fn random_suffix() -> String {
    // 8 characters from the same alphabet Python's `tempfile` uses.
    const ALPHABET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789_";
    let mut state = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|value| value.as_nanos() as u64)
        .unwrap_or(0)
        ^ (std::process::id() as u64) << 32;
    let mut out = String::with_capacity(TEMP_RANDOM_LEN);
    for _ in 0..TEMP_RANDOM_LEN {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        out.push(ALPHABET[(state % ALPHABET.len() as u64) as usize] as char);
    }
    out
}

/// Creates an empty temporary file in `directory` next to the destination.
pub fn create_temp(directory: &Path, suffix: &str) -> io::Result<(File, PathBuf)> {
    for _ in 0..64 {
        let name = format!("{TEMP_PREFIX}{}{suffix}", random_suffix());
        let path = directory.join(name);
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            // 0o666 minus umask: the same permissions a normal file creation
            // would get (BUG-04).
            options.mode(0o666);
        }
        match options.open(&path) {
            Ok(file) => return Ok((file, path)),
            Err(err) if err.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(err) => return Err(err),
        }
    }
    Err(io::Error::new(
        io::ErrorKind::AlreadyExists,
        "一時ファイル名を決定できません",
    ))
}

/// True when the file name has the temporary-file shape.
pub fn is_temp_name(name: &str) -> bool {
    let Some(rest) = name.strip_prefix(TEMP_PREFIX) else {
        return false;
    };
    if rest.len() < TEMP_RANDOM_LEN {
        return false;
    }
    let (random, _suffix) = rest.split_at(TEMP_RANDOM_LEN);
    random
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

/// Copies file contents only (no permission bits, no alternate data streams),
/// checking for an interrupt between 64 KiB chunks.
pub fn copy_contents(
    source: &Path,
    destination: &mut File,
    interrupt: &Interrupt,
) -> io::Result<()> {
    let mut input = io::BufReader::with_capacity(COPY_BUFFER, File::open(source)?);
    let mut output = BufWriter::with_capacity(COPY_BUFFER, destination);
    let mut buffer = vec![0u8; COPY_BUFFER];
    loop {
        if interrupt.requested() {
            return Err(io::Error::new(io::ErrorKind::Interrupted, "中断しました"));
        }
        let read = input.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        output.write_all(&buffer[..read])?;
    }
    output.flush()?;
    Ok(())
}

pub fn creation_time(metadata: &Metadata) -> Option<SystemTime> {
    metadata.created().ok()
}

/// The ctime value recorded in the state database.
pub fn ctime_ns(metadata: &Metadata) -> Option<i128> {
    #[cfg(windows)]
    {
        metadata
            .created()
            .ok()
            .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
            .map(|duration| duration.as_nanos() as i128)
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        Some(metadata.ctime() as i128 * 1_000_000_000 + metadata.ctime_nsec() as i128)
    }
}

pub fn mtime_ns(metadata: &Metadata) -> Option<i128> {
    metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos() as i128)
}

pub fn atime_ns(metadata: &Metadata) -> Option<i128> {
    metadata
        .accessed()
        .ok()
        .and_then(|time| time.duration_since(SystemTime::UNIX_EPOCH).ok())
        .map(|duration| duration.as_nanos() as i128)
}

pub fn size(metadata: &Metadata) -> u64 {
    metadata.len()
}

/// Applies atime/mtime and (on Windows) the creation time before the atomic
/// replacement, as the Python version does (BUG-06).
pub fn apply_times(path: &Path, source: &Metadata) -> io::Result<()> {
    let atime = filetime::FileTime::from_last_access_time(source);
    let mtime = filetime::FileTime::from_last_modification_time(source);
    filetime::set_file_times(path, atime, mtime)?;
    #[cfg(windows)]
    {
        if let Ok(creation) = source.created() {
            set_creation_time(path, creation)?;
        }
    }
    Ok(())
}

/// Applies the recorded atime/mtime (and the Windows creation time).
pub fn apply_record_times(path: &Path, stat: &crate::state::RecordStat) -> io::Result<()> {
    let atime = filetime::FileTime::from_unix_time(
        (stat.atime_ns / 1_000_000_000) as i64,
        (stat.atime_ns % 1_000_000_000) as u32,
    );
    let mtime = filetime::FileTime::from_unix_time(
        (stat.mtime_ns / 1_000_000_000) as i64,
        (stat.mtime_ns % 1_000_000_000) as u32,
    );
    filetime::set_file_times(path, atime, mtime)?;
    #[cfg(windows)]
    {
        let creation_ns = stat.ctime_ns;
        let seconds = (creation_ns / 1_000_000_000) as i64;
        let nanos = (creation_ns % 1_000_000_000) as u32;
        let creation = if seconds >= 0 {
            SystemTime::UNIX_EPOCH + Duration::new(seconds as u64, nanos)
        } else {
            SystemTime::UNIX_EPOCH - Duration::new((-seconds) as u64, 0)
        };
        set_creation_time(path, creation)?;
    }
    Ok(())
}

#[cfg(windows)]
fn set_creation_time(path: &Path, creation: SystemTime) -> io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, INVALID_HANDLE_VALUE};
    use windows_sys::Win32::Storage::FileSystem::{
        CreateFileW, SetFileTime, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
        FILE_WRITE_ATTRIBUTES, OPEN_EXISTING,
    };

    let mut wide: Vec<u16> = path.as_os_str().encode_wide().collect();
    wide.push(0);
    let handle = unsafe {
        CreateFileW(
            wide.as_ptr(),
            FILE_WRITE_ATTRIBUTES,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            std::ptr::null(),
            OPEN_EXISTING,
            0,
            std::ptr::null_mut(),
        )
    };
    if handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::last_os_error());
    }
    let since_epoch = creation
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|value| value.as_nanos() as u64)
        .unwrap_or(0);
    // FILETIME counts 100 ns intervals since 1601-01-01.
    let intervals = since_epoch / 100 + 116_444_736_000_000_000;
    let filetime = FILETIME {
        dwLowDateTime: (intervals & 0xffff_ffff) as u32,
        dwHighDateTime: (intervals >> 32) as u32,
    };
    let result = unsafe { SetFileTime(handle, &filetime, std::ptr::null(), std::ptr::null()) };
    unsafe { CloseHandle(handle) };
    if result == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(windows))]
#[allow(dead_code)]
fn set_creation_time(_path: &Path, _creation: SystemTime) -> io::Result<()> {
    Ok(())
}

/// `os.replace` semantics: replace the destination (including a symlink).
pub fn atomic_replace(temporary: &Path, destination: &Path) -> io::Result<()> {
    std::fs::rename(temporary, destination)
}

/// Atomic move that never replaces an existing destination.
pub fn atomic_move_no_overwrite(temporary: &Path, destination: &Path) -> io::Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        use windows_sys::Win32::Storage::FileSystem::{MoveFileExW, MOVEFILE_WRITE_THROUGH};
        let mut from: Vec<u16> = temporary.as_os_str().encode_wide().collect();
        from.push(0);
        let mut to: Vec<u16> = destination.as_os_str().encode_wide().collect();
        to.push(0);
        let result = unsafe { MoveFileExW(from.as_ptr(), to.as_ptr(), MOVEFILE_WRITE_THROUGH) };
        if result == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
    #[cfg(unix)]
    {
        std::fs::hard_link(temporary, destination)?;
        std::fs::remove_file(temporary)?;
        Ok(())
    }
}

pub fn path_exists_following_links(path: &Path) -> bool {
    std::fs::metadata(path).is_ok()
}

pub fn is_symlink(path: &Path) -> bool {
    std::fs::symlink_metadata(path)
        .map(|metadata| metadata.file_type().is_symlink())
        .unwrap_or(false)
}

#[cfg(windows)]
fn is_reparse_directory(path: &Path) -> bool {
    use std::os::windows::fs::MetadataExt;
    std::fs::symlink_metadata(path)
        .map(|metadata| {
            metadata.is_dir() && metadata.file_attributes() & 0x400 != 0 // REPARSE_POINT
        })
        .unwrap_or(false)
}

#[cfg(not(windows))]
fn is_reparse_directory(_path: &Path) -> bool {
    false
}

pub struct CleanupOptions<'a> {
    /// Temporary files younger than this are kept on a clean shutdown.
    pub min_age: Duration,
    /// Multiple of `min_age` after which a leftover is removed even when the
    /// previous run finished cleanly.
    pub age_multiplier: u32,
    /// True when the previous run finished cleanly.
    pub previous_clean: bool,
    pub now: SystemTime,
    /// Relative POSIX paths (case folded) that have a completion record.
    pub tracked: &'a HashSet<String>,
    pub reserved: &'a HashSet<String>,
    pub interrupt: &'a Interrupt,
}

/// Removes leftover temporary files (design spec §3.8).
pub fn cleanup_temporary_files(root: &Path, options: &CleanupOptions) -> Vec<PathBuf> {
    let mut removed = Vec::new();
    if !root.exists() {
        return removed;
    }
    let threshold = if options.previous_clean {
        options.min_age * options.age_multiplier.max(1)
    } else {
        options.min_age
    };
    walk_cleanup(root, root, options, threshold, &mut removed);
    removed
}

fn walk_cleanup(
    root: &Path,
    directory: &Path,
    options: &CleanupOptions,
    threshold: Duration,
    removed: &mut Vec<PathBuf>,
) {
    if options.interrupt.requested() {
        return;
    }
    let Ok(entries) = std::fs::read_dir(directory) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(metadata) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            if is_reparse_directory(&path) {
                continue;
            }
            walk_cleanup(root, &path, options, threshold, removed);
            continue;
        }
        let Some(name) = path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
        else {
            continue;
        };
        if !is_temp_name(&name) {
            continue;
        }
        if options.reserved.contains(&paths::casefold(&name)) {
            continue;
        }
        let Ok(relative) = path.strip_prefix(root) else {
            continue;
        };
        let key = paths::casefold(&paths::to_posix(relative));
        if options.tracked.contains(&key) {
            continue;
        }
        let Some(created) = creation_time(&metadata) else {
            continue;
        };
        let Ok(age) = options.now.duration_since(created) else {
            continue;
        };
        if age < threshold {
            continue;
        }
        if std::fs::remove_file(&path).is_ok() {
            removed.push(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn temp_names_are_recognised() {
        assert!(is_temp_name(".img2webp-abcdefgh.webp"));
        assert!(is_temp_name(".img2webp-abc_def1"));
        assert!(!is_temp_name(".img2webp-dev.sqlite3"));
        assert!(!is_temp_name(".img2webp-dev-output"));
        assert!(!is_temp_name("other.webp"));
    }

    #[test]
    fn temp_file_permissions_follow_umask() {
        let dir = std::env::temp_dir().join(format!("img2webp-temp-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let (file, path) = create_temp(&dir, ".webp").unwrap();
        drop(file);
        assert!(is_temp_name(&path.file_name().unwrap().to_string_lossy()));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode & 0o600, 0o600, "mode {mode:o}");
            assert_eq!(
                mode & 0o044,
                0o044,
                "umask should not be bypassed: {mode:o}"
            );
        }
        std::fs::remove_dir_all(&dir).ok();
    }
}
