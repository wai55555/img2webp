//! Running a single job: temp file, timestamps, source re-check, atomic commit.

use std::fs;
use std::path::Path;

use crate::color::Pixels;
use crate::config::Config;
use crate::convert;
use crate::fs_util;
use crate::interrupt::Interrupt;
use crate::paths;
use crate::plan::{Action, Job};
use crate::probe::Probe;
use crate::report::JobError;
use crate::state::RecordStat;

pub enum JobRun {
    Skipped,
    Done {
        warnings: Vec<String>,
        output: RecordStat,
    },
}

fn stat_file(path: &Path) -> Result<(RecordStat, fs::Metadata), JobError> {
    let metadata = fs::metadata(path).map_err(|err| JobError::from_io(&err, path))?;
    Ok((RecordStat::from_metadata(&metadata), metadata))
}

/// Runs copy or conversion for one job and commits the destination atomically.
pub fn run_job(
    job: &Job,
    config: &Config,
    probe: Option<&Probe>,
    record_stat: &RecordStat,
    interrupt: &Interrupt,
    before_commit: Option<&(dyn Fn(&Path) + Send + Sync)>,
    budget: Option<u64>,
) -> Result<JobRun, JobError> {
    let destination = &job.destination;
    if fs_util::path_exists_following_links(destination) && !config.overwrite_existing {
        return Ok(JobRun::Skipped);
    }
    let (start_stat, _) = stat_file(&job.source)?;
    if let Some(parent) = destination.parent() {
        fs::create_dir_all(parent).map_err(|err| JobError::from_io(&err, parent))?;
    }
    let suffix = paths::path_suffix(destination);
    let (file, temporary) = fs_util::create_temp(
        destination.parent().unwrap_or_else(|| Path::new(".")),
        &suffix,
    )
    .map_err(|err| JobError::from_io(&err, destination))?;
    drop(file);

    let result = (|| -> Result<Vec<String>, JobError> {
        let warnings = match job.action {
            Action::Copy => {
                let mut file = fs::OpenOptions::new()
                    .write(true)
                    .truncate(true)
                    .open(&temporary)
                    .map_err(|err| JobError::from_io(&err, &temporary))?;
                fs_util::copy_contents(&job.source, &mut file, interrupt)
                    .map_err(|err| JobError::from_io(&err, &job.source))?;
                Vec::new()
            }
            Action::Convert => {
                let probe = probe
                    .ok_or_else(|| JobError::Conversion("ヘッダ情報がありません".to_string()))?;
                convert::convert(&job.source, &temporary, config, probe, interrupt, budget)?
            }
        };
        if config.preserve_timestamps {
            fs_util::apply_record_times(&temporary, record_stat)
                .map_err(|err| JobError::from_io(&err, &temporary))?;
        }
        // Detect a source modification between the start of this stage and the
        // end of the work (design spec §3.8).
        let (end_stat, _) = stat_file(&job.source)?;
        if (start_stat.size, start_stat.mtime_ns, start_stat.ctime_ns)
            != (end_stat.size, end_stat.mtime_ns, end_stat.ctime_ns)
        {
            return Err(JobError::Conversion(
                "処理中に入力ファイルが変更されました".to_string(),
            ));
        }
        Ok(warnings)
    })();

    let warnings = match result {
        Ok(warnings) => warnings,
        Err(error) => {
            let _ = fs::remove_file(&temporary);
            return Err(error);
        }
    };

    if let Some(hook) = before_commit {
        // Test hook: allows injecting a competing writer before the commit.
        hook(destination);
    }
    let commit = if config.overwrite_existing {
        fs_util::atomic_replace(&temporary, destination)
    } else if fs_util::path_exists_following_links(destination) {
        // The destination appeared between the pre-check and the commit: do not
        // overwrite it (BUG-05, intentional change (9)).
        Err(std::io::Error::new(
            std::io::ErrorKind::AlreadyExists,
            "出力先が既に存在します",
        ))
    } else if fs_util::is_symlink(destination) {
        // Broken symbolic link: remove the link itself, then place the file.
        match fs::remove_file(destination) {
            Ok(()) => fs_util::atomic_move_no_overwrite(&temporary, destination),
            Err(err) => Err(err),
        }
    } else {
        fs_util::atomic_move_no_overwrite(&temporary, destination)
    };
    if let Err(error) = commit {
        let _ = fs::remove_file(&temporary);
        return Err(JobError::from_io(&error, destination));
    }
    let (output_stat, _) = stat_file(destination)?;
    Ok(JobRun::Done {
        warnings,
        output: output_stat,
    })
}

#[allow(dead_code)]
fn unused_pixels(_pixels: &Pixels) {}
