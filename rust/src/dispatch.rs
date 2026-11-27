//! Dispatcher: planning, reuse/adopt decisions, two-stage execution, budget
//! grants, records and exit codes (design spec §3.4, §3.6, §3.8).

use std::cmp::Ordering as CmpOrdering;
use std::collections::{BinaryHeap, HashMap, HashSet, VecDeque};
use std::fs;
use std::panic::{catch_unwind, AssertUnwindSafe};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{channel, Sender};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant, SystemTime};

use crate::budget::{self, Budget, Grant};
use crate::config::{self, Config};
use crate::fs_util::{self, CleanupOptions};
use crate::interrupt::Interrupt;
use crate::job::{self, JobRun};
use crate::paths;
use crate::plan::{self, Action, Job, Plan};
use crate::probe;
use crate::report::{JobError, Reporter};
use crate::state::{
    JobRecord, RecordStat, StateDb, DEV_MARKER_CONTENT, DEV_MARKER_NAME, DEV_STATE_DATABASE_NAME,
    STATE_DATABASE_NAME,
};
use crate::webp_encode;

pub const DEV_BUILD: bool = cfg!(feature = "dev-db");

const DEFAULT_CLEANUP_MIN_AGE_SECS: u64 = 3600;
const CLEANUP_AGE_MULTIPLIER: u32 = 10;
const DEFAULT_STAGNATION_TIMEOUT_SECS: u64 = 600;
const RECONCILE_GRACE_SECS: u64 = 2;

/// Fault injection for the dispatcher tests (design spec §6 Phase 5). The
/// production binary never sets this.
#[derive(Clone, Default)]
pub struct Faults {
    /// Drop the completion notification of this job (simulates a lost message).
    pub drop_finished_for: Option<usize>,
    /// Send a duplicate notification with a stale generation for this job.
    pub duplicate_finished_for: Option<usize>,
    /// Panic in the stage-2 worker of this job (pure Rust panic).
    pub panic_stage2_for: Option<usize>,
    /// Panic inside the dispatcher after this many completed jobs.
    pub panic_dispatcher_after: Option<usize>,
}

#[derive(Clone)]
pub struct Options {
    pub project_dir: PathBuf,
    pub budget_override: Option<u64>,
    pub parallel_override: Option<usize>,
    pub cleanup_min_age: Option<Duration>,
    pub now: Option<SystemTime>,
    pub stagnation_timeout: Option<Duration>,
    pub interrupt: Interrupt,
    pub hook: Option<Arc<dyn Fn(&str) + Send + Sync>>,
    /// Test hook: request an interrupt after N finished jobs.
    pub interrupt_after: Option<usize>,
    /// Test hook: request a second interrupt after N finished jobs.
    pub second_interrupt_after: Option<usize>,
    /// Test hook: called right before the atomic commit of a job.
    pub commit_hook: Option<Arc<dyn Fn(&Path) + Send + Sync>>,
    /// Fault injection (tests only).
    pub faults: Option<Arc<Faults>>,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            project_dir: default_project_dir(),
            budget_override: None,
            parallel_override: None,
            cleanup_min_age: None,
            now: None,
            stagnation_timeout: None,
            interrupt: Interrupt::new(),
            hook: None,
            interrupt_after: None,
            second_interrupt_after: None,
            commit_hook: None,
            faults: None,
        }
    }
}

pub fn default_project_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(|parent| parent.to_path_buf()))
        .unwrap_or_else(|| PathBuf::from("."))
}

type Work = Box<dyn FnOnce() + Send + 'static>;

struct Task {
    priority: u8,
    seq: u64,
    work: Work,
}

impl PartialEq for Task {
    fn eq(&self, other: &Self) -> bool {
        self.priority == other.priority && self.seq == other.seq
    }
}
impl Eq for Task {}
impl PartialOrd for Task {
    fn partial_cmp(&self, other: &Self) -> Option<CmpOrdering> {
        Some(self.cmp(other))
    }
}
impl Ord for Task {
    fn cmp(&self, other: &Self) -> CmpOrdering {
        // Higher priority first, then FIFO.
        self.priority
            .cmp(&other.priority)
            .then(other.seq.cmp(&self.seq))
    }
}

struct PoolState {
    queue: BinaryHeap<Task>,
    stopping: bool,
    next_seq: u64,
}

struct Pool {
    shared: Arc<(Mutex<PoolState>, Condvar)>,
    handles: Vec<JoinHandle<()>>,
}

impl Pool {
    fn new(threads: usize) -> Self {
        let shared = Arc::new((
            Mutex::new(PoolState {
                queue: BinaryHeap::new(),
                stopping: false,
                next_seq: 0,
            }),
            Condvar::new(),
        ));
        let mut handles = Vec::new();
        for _ in 0..threads.max(1) {
            let shared = Arc::clone(&shared);
            handles.push(std::thread::spawn(move || loop {
                let task = {
                    let (lock, condvar) = &*shared;
                    let mut state = match lock.lock() {
                        Ok(state) => state,
                        Err(_) => return,
                    };
                    loop {
                        if let Some(task) = state.queue.pop() {
                            break Some(task);
                        }
                        if state.stopping {
                            break None;
                        }
                        state = match condvar.wait(state) {
                            Ok(state) => state,
                            Err(_) => return,
                        };
                    }
                };
                match task {
                    Some(task) => (task.work)(),
                    None => return,
                }
            }));
        }
        Pool { shared, handles }
    }

    fn submit(&self, priority: u8, work: Work) {
        let (lock, condvar) = &*self.shared;
        if let Ok(mut state) = lock.lock() {
            let seq = state.next_seq;
            state.next_seq += 1;
            state.queue.push(Task {
                priority,
                seq,
                work,
            });
            condvar.notify_one();
        }
    }

    fn shutdown(self) {
        {
            let (lock, condvar) = &*self.shared;
            if let Ok(mut state) = lock.lock() {
                state.stopping = true;
                condvar.notify_all();
            }
        }
        for handle in self.handles {
            let _ = handle.join();
        }
    }
}

enum Message {
    Probed(usize, Box<Result<probe::Probe, JobError>>),
    Finished(usize, u64, Box<Result<JobRun, JobError>>),
    Fatal(String),
}

/// How a worker panic must be handled (design spec §3.6): a panic while a
/// libwebp FFI call is in progress terminates the process, everything else is a
/// job failure.
#[derive(Debug)]
pub enum WorkerPanic {
    Fatal(String),
    Job(JobError),
}

pub fn classify_worker_panic(ffi_active: bool, message: String) -> WorkerPanic {
    if ffi_active {
        WorkerPanic::Fatal(message)
    } else {
        WorkerPanic::Job(JobError::Panic(message))
    }
}

fn panic_message(payload: Box<dyn std::any::Any + Send>) -> String {
    if let Some(text) = payload.downcast_ref::<&str>() {
        (*text).to_string()
    } else if let Some(text) = payload.downcast_ref::<String>() {
        text.clone()
    } else {
        "unknown panic".to_string()
    }
}

#[derive(Default)]
struct Counts {
    skipped: usize,
    converted: usize,
    copied: usize,
    adopted: usize,
    failed: usize,
}

struct Waiting {
    id: usize,
    estimate: u64,
    probe: probe::Probe,
}

pub fn run(options: Options) -> i32 {
    let result = catch_unwind(AssertUnwindSafe(|| run_inner(options)));
    match result {
        Ok(code) => code,
        Err(_) => {
            // A dispatcher panic loses the ledger; do not continue and do not
            // commit (design spec §3.6, intentional change 14).
            let mut reporter = Reporter::new();
            reporter.err("内部エラー: ディスパッチャが異常終了しました");
            2
        }
    }
}

fn config_error(reporter: &mut Reporter, message: &str) -> i32 {
    reporter.err(&format!("設定エラー: {message}"));
    2
}

fn ensure_dev_marker(directory: &Path, reporter: &mut Reporter) {
    let path = directory.join(DEV_MARKER_NAME);
    match fs::read_to_string(&path) {
        Ok(existing) => {
            if existing.trim_end() != DEV_MARKER_CONTENT {
                reporter.err(&format!(
                    "WARN {}: 出力先のマーカーファイルの内容が一致しません",
                    path.display()
                ));
            }
        }
        Err(_) => {
            if fs::write(&path, format!("{DEV_MARKER_CONTENT}\n")).is_err() {
                reporter.err(&format!(
                    "WARN {}: マーカーファイルを作成できません",
                    path.display()
                ));
            }
        }
    }
}

fn derived_output_dir(config: &Config) -> Result<PathBuf, String> {
    if !DEV_BUILD {
        return Ok(config.output_dir.clone());
    }
    let Some(name) = config.output_dir.file_name() else {
        return Err(format!(
            "出力フォルダの名前に -dev を付けられません: {}",
            config.output_dir.display()
        ));
    };
    let candidate = config
        .output_dir
        .with_file_name(format!("{}-dev", name.to_string_lossy()));
    if paths::is_same_or_child(&config.input_dir, &candidate)
        || paths::is_same_or_child(&candidate, &config.input_dir)
    {
        return Err("入力フォルダと出力フォルダは別々の階層にしてください".to_string());
    }
    Ok(candidate)
}

fn record_values(
    job: &Job,
    config: &Config,
    source: &RecordStat,
    output: &RecordStat,
) -> JobRecord {
    JobRecord {
        destination_path: job
            .destination
            .strip_prefix(&config.output_dir)
            .map(paths::to_posix)
            .unwrap_or_else(|_| paths::to_posix(&job.destination)),
        source_size: source.size as i64,
        source_mtime_ns: source.mtime_ns as i64,
        source_ctime_ns: source.ctime_ns as i64,
        output_size: output.size as i64,
        output_mtime_ns: output.mtime_ns as i64,
        settings: crate::state::settings_signature(job, config),
    }
}

fn is_tracked_unchanged(db: &StateDb, job: &Job, config: &Config) -> bool {
    let Ok(Some(record)) = db.get(job) else {
        return false;
    };
    let Ok(output_metadata) = fs::metadata(&job.destination) else {
        return false;
    };
    if !output_metadata.is_file() {
        return false;
    }
    let Ok(source_metadata) = fs::metadata(&job.source) else {
        return false;
    };
    let expected = record_values(
        job,
        config,
        &RecordStat::from_metadata(&source_metadata),
        &RecordStat::from_metadata(&output_metadata),
    );
    record == expected
}

fn check_limits(probe: &probe::Probe) -> Result<(), JobError> {
    if probe.format == probe::Format::Png && probe.bit_depth == 16 && probe.is_apng {
        // 16-bit APNG is unsupported by design (intentional change 7).
        return Err(JobError::Conversion("16bit APNG は未対応です".to_string()));
    }
    if probe.width > budget::MAX_SIDE || probe.height > budget::MAX_SIDE {
        return Err(JobError::Conversion(format!(
            "画像の寸法 {}x{} は WebP の上限 ({}) を超えています",
            probe.width,
            probe.height,
            budget::MAX_SIDE
        )));
    }
    let pixels = probe.width as u64 * probe.height as u64;
    if pixels > budget::MAX_PIXELS {
        return Err(JobError::DecompressionBomb(format!(
            "Image size ({pixels} pixels) exceeds limit of {} pixels, could be decompression bomb DOS attack.",
            budget::MAX_PIXELS
        )));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn spawn_stage2(
    pool: &Pool,
    sender: &Sender<Message>,
    id: usize,
    job: &Job,
    config: &Config,
    probe: probe::Probe,
    generation: u64,
    record_stat: RecordStat,
    estimate: u64,
    interrupt: &Interrupt,
    commit_hook: Option<Arc<dyn Fn(&Path) + Send + Sync>>,
    faults: Option<Arc<Faults>>,
) {
    let job = job.clone();
    let config = config.clone();
    let interrupt = interrupt.clone();
    let sender = sender.clone();
    let expected = probe;
    pool.submit(
        2,
        Box::new(move || {
            let outcome = catch_unwind(AssertUnwindSafe(|| {
                if faults.as_ref().and_then(|faults| faults.panic_stage2_for) == Some(id) {
                    panic!("injected stage-2 panic");
                }
                let reprobe = probe::probe(&job.source)
                    .map_err(|error| JobError::from_io(&error, &job.source))?;
                if !reprobe.signature_matches(&expected) {
                    return Err(JobError::Conversion(
                        "処理中に入力ファイルが変更されました".to_string(),
                    ));
                }
                let hook = commit_hook.as_deref();
                job::run_job(
                    &job,
                    &config,
                    Some(&expected),
                    &record_stat,
                    &interrupt,
                    hook,
                    Some(estimate),
                )
            }));
            match outcome {
                Ok(result) => {
                    let _ = sender.send(Message::Finished(id, generation, Box::new(result)));
                    if faults
                        .as_ref()
                        .and_then(|faults| faults.duplicate_finished_for)
                        == Some(id)
                    {
                        let _ = sender.send(Message::Finished(
                            id,
                            generation.wrapping_add(999),
                            Box::new(Err(JobError::Panic("injected duplicate".to_string()))),
                        ));
                    }
                }
                Err(payload) => {
                    match classify_worker_panic(webp_encode::ffi_active(), panic_message(payload)) {
                        WorkerPanic::Fatal(message) => {
                            let _ = sender.send(Message::Fatal(message));
                        }
                        WorkerPanic::Job(error) => {
                            let _ = sender.send(Message::Finished(
                                id,
                                generation,
                                Box::new(Err(error)),
                            ));
                        }
                    }
                }
            }
        }),
    );
}

fn run_inner(options: Options) -> i32 {
    let mut reporter = Reporter::new();
    let project_dir = options.project_dir.clone();
    let config = match config::load(&project_dir.join("config.ini"), &project_dir) {
        Ok(config) => config,
        Err(error) => return config_error(&mut reporter, &error.0),
    };
    let db_name = if DEV_BUILD {
        DEV_STATE_DATABASE_NAME
    } else {
        STATE_DATABASE_NAME
    };
    let output_dir = match derived_output_dir(&config) {
        Ok(dir) => dir,
        Err(message) => return config_error(&mut reporter, &message),
    };
    let config = Config {
        output_dir: output_dir.clone(),
        ..config
    };
    for directory in [&config.input_dir, &output_dir] {
        if let Err(error) = fs::create_dir_all(directory) {
            return config_error(&mut reporter, &format!("{}: {error}", directory.display()));
        }
    }
    let db_path = output_dir.join(db_name);
    if DEV_BUILD {
        reporter.err(&format!(
            "img2webp (開発ビルド): 状態DB={}, 出力先={}",
            db_path.display(),
            output_dir.display()
        ));
        ensure_dev_marker(&output_dir, &mut reporter);
    }
    let mut db = match StateDb::open(&db_path, config.db_commit_every.max(1) as u32) {
        Ok(db) => db,
        Err(error) => return config_error(&mut reporter, &error.0),
    };
    if !db.wal_enabled() {
        reporter.err(
            "WARN: 出力フォルダで WAL を有効にできないため、状態 DB をロールバックジャーナルで使用します",
        );
    }
    let count = match db.completed_count() {
        Ok(count) => count,
        Err(error) => return config_error(&mut reporter, &error.0),
    };
    let last_exit = db.read_meta("last_exit").ok().flatten();
    let recorded_count = db
        .read_meta("completed_count")
        .ok()
        .flatten()
        .and_then(|value| value.parse::<i64>().ok());
    let previous_clean = last_exit.as_deref() == Some("ok") && recorded_count == Some(count);
    let tracked = match db.tracked_destinations() {
        Ok(tracked) => tracked,
        Err(error) => return config_error(&mut reporter, &error.0),
    };
    let reserved = plan::reserved_names();
    let now = options.now.unwrap_or_else(SystemTime::now);
    let min_age = options
        .cleanup_min_age
        .unwrap_or_else(|| Duration::from_secs(DEFAULT_CLEANUP_MIN_AGE_SECS));
    let cleanup_options = CleanupOptions {
        min_age,
        age_multiplier: CLEANUP_AGE_MULTIPLIER,
        previous_clean,
        now,
        tracked: &tracked,
        reserved: &reserved,
        interrupt: &options.interrupt,
    };
    let _ = fs_util::cleanup_temporary_files(&output_dir, &cleanup_options);
    if let Err(error) = db.write_meta("last_exit", "running") {
        return config_error(&mut reporter, &error.0);
    }

    let ctime_available = fs::metadata(&db_path)
        .ok()
        .and_then(|metadata| fs_util::ctime_ns(&metadata))
        .is_some();
    if !ctime_available {
        reporter
            .err("WARN: ファイルの作成日時を取得できないため、処理履歴による再利用は機能しません");
    }

    if options.interrupt.requested() {
        reporter.err("中断しました。");
        return 130;
    }
    let plan: Plan = plan::plan(&config);
    if options.interrupt.requested() {
        reporter.err("中断しました。");
        return 130;
    }

    let total_budget = options.budget_override.unwrap_or_else(budget::total_budget);
    let cores = std::thread::available_parallelism()
        .map(|value| value.get())
        .unwrap_or(1);
    let requested = options
        .parallel_override
        .or_else(|| config.parallel_jobs.map(|value| value as usize))
        .unwrap_or(cores);
    let mut threads = requested.max(1);
    if threads > cores {
        threads = cores;
        reporter.err(&format!(
            "WARN: parallel_jobs を CPU コア数 ({cores}) に合わせました"
        ));
    }
    let stagnation_timeout = options
        .stagnation_timeout
        .unwrap_or_else(|| Duration::from_secs(DEFAULT_STAGNATION_TIMEOUT_SECS));

    let mut counts = Counts {
        skipped: plan.skipped,
        ..Counts::default()
    };
    let (sender, receiver) = channel::<Message>();
    let pool = Pool::new(threads);
    let mut budget = Budget::new(total_budget);
    let mut waiting: VecDeque<Waiting> = VecDeque::new();
    let mut granted: HashMap<usize, (u64, Instant)> = HashMap::new();
    let mut spawned: HashSet<usize> = HashSet::new();
    let mut record_stats: HashMap<usize, RecordStat> = HashMap::new();
    let mut outstanding: usize = 0usize;
    let mut next_index = 0usize;
    let mut waiting_since: Option<Instant> = None;
    let mut interrupted = false;
    let mut db_failure: Option<String> = None;
    let mut fatal = false;
    let mut finished_jobs = 0usize;
    let window = (threads * 4).max(4);

    let hook = options.hook.clone();
    let notify = |hook: &Option<Arc<dyn Fn(&str) + Send + Sync>>, event: &str| {
        if let Some(hook) = hook {
            hook(event);
        }
    };

    loop {
        if !interrupted && options.interrupt.requested() {
            interrupted = true;
            waiting.clear();
            waiting_since = None;
        }
        if !interrupted && db_failure.is_none() && !fatal {
            while next_index < plan.jobs.len() && outstanding < window {
                let id = next_index;
                let job = plan.jobs[id].clone();
                next_index += 1;
                notify(&hook, "dequeue");
                if config.reuse_unchanged && is_tracked_unchanged(&db, &job, &config) {
                    counts.skipped += 1;
                    reporter.out(&format!(
                        "SKIP {}: 前回成功時の記録と一致",
                        job.relative_source_display()
                    ));
                    continue;
                }
                if config.reuse_unchanged
                    && config.adopt_untracked
                    && fs::metadata(&job.destination)
                        .map(|metadata| metadata.is_file())
                        .unwrap_or(false)
                    && matches!(db.get(&job), Ok(None))
                {
                    let source_metadata = match fs::metadata(&job.source) {
                        Ok(metadata) => metadata,
                        Err(error) => {
                            counts.failed += 1;
                            reporter.error(
                                &job.relative_source_display(),
                                &JobError::from_io(&error, &job.source),
                            );
                            continue;
                        }
                    };
                    let output_metadata = match fs::metadata(&job.destination) {
                        Ok(metadata) => metadata,
                        Err(error) => {
                            counts.failed += 1;
                            reporter.error(
                                &job.relative_source_display(),
                                &JobError::from_io(&error, &job.destination),
                            );
                            continue;
                        }
                    };
                    if let Err(error) = db.record(
                        &job,
                        &config,
                        &RecordStat::from_metadata(&source_metadata),
                        &RecordStat::from_metadata(&output_metadata),
                    ) {
                        db_failure = Some(error.0);
                        break;
                    }
                    counts.adopted += 1;
                    reporter.out(&format!(
                        "ADOPT {}: 既存の出力を現在の設定として登録",
                        job.relative_source_display()
                    ));
                    if let Err(error) = db.commit_if_due() {
                        db_failure = Some(error.0);
                        break;
                    }
                    continue;
                }
                let record_stat = match fs::metadata(&job.source) {
                    Ok(metadata) => RecordStat::from_metadata(&metadata),
                    Err(error) => {
                        counts.failed += 1;
                        reporter.error(
                            &job.relative_source_display(),
                            &JobError::from_io(&error, &job.source),
                        );
                        continue;
                    }
                };
                record_stats.insert(id, record_stat);
                outstanding += 1;
                let sender = sender.clone();
                match job.action {
                    Action::Copy => {
                        spawned.insert(id);
                        let config = config.clone();
                        let interrupt = options.interrupt.clone();
                        pool.submit(
                            2,
                            Box::new(move || {
                                let result = catch_unwind(AssertUnwindSafe(|| {
                                    job::run_job(
                                        &job,
                                        &config,
                                        None,
                                        &record_stat,
                                        &interrupt,
                                        None,
                                        None,
                                    )
                                }));
                                match result {
                                    Ok(result) => {
                                        let _ =
                                            sender.send(Message::Finished(id, 0, Box::new(result)));
                                    }
                                    Err(payload) => {
                                        if webp_encode::ffi_active() {
                                            let _ =
                                                sender.send(Message::Fatal(panic_message(payload)));
                                        } else {
                                            let _ = sender.send(Message::Finished(
                                                id,
                                                0,
                                                Box::new(Err(JobError::Panic(panic_message(
                                                    payload,
                                                )))),
                                            ));
                                        }
                                    }
                                }
                            }),
                        );
                    }
                    Action::Convert => {
                        pool.submit(
                            0,
                            Box::new(move || {
                                let result = catch_unwind(AssertUnwindSafe(|| {
                                    probe::probe(&job.source)
                                        .map_err(|error| JobError::from_io(&error, &job.source))
                                }));
                                let message = match result {
                                    Ok(result) => Message::Probed(id, Box::new(result)),
                                    Err(payload) => Message::Finished(
                                        id,
                                        0,
                                        Box::new(Err(JobError::Panic(panic_message(payload)))),
                                    ),
                                };
                                let _ = sender.send(message);
                            }),
                        );
                    }
                }
            }
        }

        if interrupted && outstanding == 0 {
            break;
        }
        if next_index >= plan.jobs.len() && outstanding == 0 {
            break;
        }
        if fatal {
            break;
        }
        if db_failure.is_some() {
            break;
        }

        // Reconcile: grants that were never turned into tasks (lost
        // notifications) are reclaimed.
        let reconcile_after = Duration::from_secs(RECONCILE_GRACE_SECS);
        let now_instant = Instant::now();
        let stale: Vec<usize> = granted
            .iter()
            .filter(|(id, (_, started))| {
                !spawned.contains(id) && now_instant.duration_since(*started) > reconcile_after
            })
            .map(|(id, _)| *id)
            .collect();
        for id in stale {
            if let Some((generation, _)) = granted.remove(&id) {
                budget.release(generation);
                if !interrupted {
                    counts.failed += 1;
                    reporter.error(
                        &plan.jobs[id].relative_source_display(),
                        &JobError::Panic("実行中でない予算割当を回収しました".to_string()),
                    );
                }
                outstanding = outstanding.saturating_sub(1);
            }
        }

        let message = match receiver.recv_timeout(Duration::from_millis(50)) {
            Ok(message) => Some(message),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => None,
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                if outstanding == 0 {
                    break;
                }
                None
            }
        };

        if let Some(message) = message {
            match message {
                Message::Probed(id, result) => match *result {
                    Err(error) => {
                        if !interrupted {
                            counts.failed += 1;
                            reporter.error(&plan.jobs[id].relative_source_display(), &error);
                        }
                        outstanding = outstanding.saturating_sub(1);
                    }
                    Ok(probe) => {
                        if interrupted {
                            outstanding = outstanding.saturating_sub(1);
                            continue;
                        }
                        if let Err(error) = check_limits(&probe) {
                            counts.failed += 1;
                            reporter.error(&plan.jobs[id].relative_source_display(), &error);
                            outstanding = outstanding.saturating_sub(1);
                            continue;
                        }
                        let estimate = budget::estimate(&probe, 0);
                        if waiting.is_empty() {
                            match budget.grant(estimate) {
                                Grant::Granted(generation) => {
                                    granted.insert(id, (generation, Instant::now()));
                                    let record_stat =
                                        record_stats.get(&id).copied().unwrap_or(RecordStat {
                                            size: 0,
                                            mtime_ns: 0,
                                            ctime_ns: 0,
                                            atime_ns: 0,
                                        });
                                    spawn_stage2(
                                        &pool,
                                        &sender,
                                        id,
                                        &plan.jobs[id],
                                        &config,
                                        probe,
                                        generation,
                                        record_stat,
                                        estimate,
                                        &options.interrupt,
                                        options.commit_hook.clone(),
                                        options.faults.clone(),
                                    );
                                    spawned.insert(id);
                                }
                                Grant::Wait => {
                                    waiting.push_back(Waiting {
                                        id,
                                        estimate,
                                        probe,
                                    });
                                    waiting_since = Some(Instant::now());
                                }
                                Grant::TooLarge => {
                                    counts.failed += 1;
                                    reporter.error(
                                        &plan.jobs[id].relative_source_display(),
                                        &JobError::Memory(format!(
                                            "必要メモリの見積もり {estimate} バイトが総量予算 {total_budget} バイトを超えています"
                                        )),
                                    );
                                    outstanding = outstanding.saturating_sub(1);
                                }
                            }
                        } else {
                            waiting.push_back(Waiting {
                                id,
                                estimate,
                                probe,
                            });
                        }
                    }
                },
                Message::Finished(id, generation, result) => {
                    if generation != 0 {
                        let matches_grant = granted
                            .get(&id)
                            .map(|(expected, _)| *expected == generation)
                            .unwrap_or(false);
                        if !matches_grant {
                            // Late or duplicated notification: ignore.
                            continue;
                        }
                    }
                    spawned.remove(&id);
                    if options
                        .faults
                        .as_ref()
                        .and_then(|faults| faults.drop_finished_for)
                        == Some(id)
                    {
                        // Simulated message loss: the ledger entry stays until
                        // the reconcile pass reclaims it.
                        continue;
                    }
                    if generation != 0 {
                        budget.release(generation);
                        granted.remove(&id);
                    }
                    outstanding = outstanding.saturating_sub(1);
                    finished_jobs += 1;
                    let job = plan.jobs[id].clone();
                    let record_source = record_stats.get(&id).copied();
                    match *result {
                        Ok(JobRun::Skipped) => {
                            counts.skipped += 1;
                            reporter.out(&format!(
                                "SKIP {}: 出力があり、上書きは無効です",
                                job.relative_source_display()
                            ));
                        }
                        Ok(JobRun::Done { warnings, output }) => {
                            let source_stat = record_source.or_else(|| {
                                fs::metadata(&job.source)
                                    .ok()
                                    .map(|metadata| RecordStat::from_metadata(&metadata))
                            });
                            match source_stat {
                                Some(source_stat) => {
                                    if let Err(error) =
                                        db.record(&job, &config, &source_stat, &output)
                                    {
                                        db_failure = Some(error.0);
                                        continue;
                                    }
                                }
                                None => {
                                    counts.failed += 1;
                                    reporter.error(
                                        &job.relative_source_display(),
                                        &JobError::NotFound(format!(
                                            "cannot stat '{}'",
                                            job.source.display()
                                        )),
                                    );
                                    continue;
                                }
                            }
                            match job.action {
                                Action::Copy => counts.copied += 1,
                                Action::Convert => counts.converted += 1,
                            }
                            let destination = job
                                .destination
                                .strip_prefix(&config.output_dir)
                                .map(paths::display)
                                .unwrap_or_else(|_| job.destination_display());
                            let verb = match job.action {
                                Action::Copy => "COPY",
                                Action::Convert => "CONVERT",
                            };
                            reporter.out(&format!(
                                "{verb} {} -> {destination}",
                                job.relative_source_display()
                            ));
                            for warning in warnings {
                                reporter.warn(&job.relative_source_display(), &warning);
                            }
                            if let Err(error) = db.commit_if_due() {
                                db_failure = Some(error.0);
                            }
                        }
                        Err(error) => {
                            counts.failed += 1;
                            reporter.error(&job.relative_source_display(), &error);
                        }
                    }
                    if let Some(limit) = options
                        .faults
                        .as_ref()
                        .and_then(|faults| faults.panic_dispatcher_after)
                    {
                        if finished_jobs >= limit {
                            panic!("injected dispatcher panic");
                        }
                    }
                    if let Some(limit) = options.interrupt_after {
                        if finished_jobs >= limit && !interrupted {
                            options.interrupt.request();
                            interrupted = true;
                            waiting.clear();
                            waiting_since = None;
                        }
                    }
                    if let Some(limit) = options.second_interrupt_after {
                        if finished_jobs >= limit {
                            options.interrupt.request();
                            interrupted = true;
                            waiting.clear();
                            waiting_since = None;
                        }
                    }
                }
                Message::Fatal(reason) => {
                    fatal = true;
                    reporter.err(&format!("内部エラー: WebP エンコーダの異常終了: {reason}"));
                    break;
                }
            }
        }

        // Grant the first waiting job as soon as budget frees up.
        loop {
            let Some(front) = waiting.front() else {
                break;
            };
            let estimate = front.estimate;
            match budget.grant(estimate) {
                Grant::Granted(generation) => {
                    let waiting_job = waiting.pop_front().unwrap();
                    granted.insert(waiting_job.id, (generation, Instant::now()));
                    let record_stat =
                        record_stats
                            .get(&waiting_job.id)
                            .copied()
                            .unwrap_or(RecordStat {
                                size: 0,
                                mtime_ns: 0,
                                ctime_ns: 0,
                                atime_ns: 0,
                            });
                    spawn_stage2(
                        &pool,
                        &sender,
                        waiting_job.id,
                        &plan.jobs[waiting_job.id],
                        &config,
                        waiting_job.probe,
                        generation,
                        record_stat,
                        waiting_job.estimate,
                        &options.interrupt,
                        options.commit_hook.clone(),
                        options.faults.clone(),
                    );
                    spawned.insert(waiting_job.id);
                    if waiting.is_empty() {
                        waiting_since = None;
                    }
                }
                Grant::Wait => break,
                Grant::TooLarge => {
                    let waiting_job = waiting.pop_front().unwrap();
                    counts.failed += 1;
                    reporter.error(
                        &plan.jobs[waiting_job.id].relative_source_display(),
                        &JobError::Memory(format!(
                            "必要メモリの見積もり {} バイトが総量予算 {total_budget} バイトを超えています",
                            waiting_job.estimate
                        )),
                    );
                    outstanding = outstanding.saturating_sub(1);
                }
            }
        }

        if let Some(since) = waiting_since {
            if since.elapsed() > stagnation_timeout {
                reporter.err(&format!(
                    "待ち行列が {:.0} 秒間停滞したため終了します",
                    stagnation_timeout.as_secs_f64()
                ));
                pool.shutdown();
                let _ = db.commit();
                return 2;
            }
        }
    }

    if fatal {
        pool.shutdown();
        return 2;
    }

    if let Some(message) = db_failure {
        pool.shutdown();
        let _ = db.commit();
        reporter.err(&format!("処理履歴DBエラー: {message}"));
        return 2;
    }

    if interrupted {
        // Record completions that already arrived; leave the rest for the next
        // run (design spec §3.4 interrupt semantics).
        while outstanding > 0 {
            if options.interrupt.is_second() {
                break;
            }
            match receiver.recv_timeout(Duration::from_millis(200)) {
                Ok(Message::Finished(id, generation, result)) => {
                    if generation != 0 {
                        budget.release(generation);
                        granted.remove(&id);
                    }
                    spawned.remove(&id);
                    outstanding = outstanding.saturating_sub(1);
                    let job = plan.jobs[id].clone();
                    if let Ok(JobRun::Done { warnings, output }) = *result {
                        let source_stat = record_stats.get(&id).copied().or_else(|| {
                            fs::metadata(&job.source)
                                .ok()
                                .map(|metadata| RecordStat::from_metadata(&metadata))
                        });
                        if let Some(source_stat) = source_stat {
                            if let Err(error) = db.record(&job, &config, &source_stat, &output) {
                                db_failure = Some(error.0);
                            }
                        }
                        for warning in warnings {
                            reporter.warn(&job.relative_source_display(), &warning);
                        }
                    }
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
        pool.shutdown();
        if options.interrupt.is_second() {
            reporter.err("中断しました。記録済みのファイルは次回再利用されます。");
            return 130;
        }
        if let Some(message) = db_failure {
            let _ = db.commit();
            reporter.err(&format!("処理履歴DBエラー: {message}"));
            return 2;
        }
        if let Err(error) = db.commit() {
            reporter.err(&format!("処理履歴DBエラー: {}", error.0));
            return 2;
        }
        if let Ok(count) = db.completed_count() {
            let _ = db.write_meta("last_exit", "ok");
            let _ = db.write_meta("completed_count", &count.to_string());
        }
        reporter.err("中断しました。記録済みのファイルは次回再利用されます。");
        return 130;
    }

    // Normal completion.
    pool.shutdown();
    if let Err(error) = db.commit() {
        reporter.err(&format!("処理履歴DBエラー: {}", error.0));
        return 2;
    }
    let count = match db.completed_count() {
        Ok(count) => count,
        Err(error) => {
            reporter.err(&format!("処理履歴DBエラー: {}", error.0));
            return 2;
        }
    };
    if let Err(error) = db.write_meta("last_exit", "ok") {
        reporter.err(&format!("処理履歴DBエラー: {}", error.0));
        return 2;
    }
    if let Err(error) = db.write_meta("completed_count", &count.to_string()) {
        reporter.err(&format!("処理履歴DBエラー: {}", error.0));
        return 2;
    }
    reporter.out(&format!(
        "完了: 変換 {} / コピー {} / 再利用 {} / 登録 {} / 失敗 {}",
        counts.converted, counts.copied, counts.skipped, counts.adopted, counts.failed
    ));
    if counts.failed > 0 {
        1
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ffi_panic_is_fatal_and_other_panics_are_job_failures() {
        match classify_worker_panic(true, "boom".to_string()) {
            WorkerPanic::Fatal(message) => assert_eq!(message, "boom"),
            other => panic!("expected a fatal panic, got {other:?}"),
        }
        match classify_worker_panic(false, "boom".to_string()) {
            WorkerPanic::Job(JobError::Panic(message)) => assert_eq!(message, "boom"),
            other => panic!("expected a job failure, got {other:?}"),
        }
    }
}
