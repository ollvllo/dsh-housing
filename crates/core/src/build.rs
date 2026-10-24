//! 构建任务宿主（v0.5 / #249，docs/structure.md §3.24）。
//!
//! 这里只做**胶水**：起一个构建线程、把子进程放进自己的 Job Object、把输出汇进环形缓冲
//! 与日志文件、把步骤写进快照、取消时收整棵进程树、成功后把产物交给 core 去登记。
//! 「构建到底做什么」全在 `crates/builder`（§5.14 D37/D38）。

use std::ffi::OsString;
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use dsh_housing_builder::{BuildError, BuildReport, BuildRequest, Command, Host, Outcome, Step};
use dsh_housing_gui as gui;
use windows::Win32::Foundation::{ERROR_BROKEN_PIPE, ERROR_NO_DATA, HANDLE};
use windows::Win32::Storage::FileSystem::ReadFile;

use crate::{diag, env, job, logs, port};

/// 构建步骤总数（面板显示「第 n/6 步」）。
pub const STEP_TOTAL: u8 = 6;
/// 构建日志的环形缓冲容量（与实例日志同量级）。
pub const LOG_RING_CAPACITY: usize = 256 * 1024;
/// 单条命令 stdout 最多收多少字节（版本断言只需几十字节；npm 的输出已经在日志文件里）。
pub const CAPTURE_LIMIT: usize = 4 * 1024 * 1024;
/// 取消 / core 退出时写进 job 的退出码（与实例强杀区分开）。
pub const CANCEL_EXIT_CODE: u32 = 0xDEAD_0002;
/// 单条命令「等一会儿看有没有退出」的切片长度。
const WAIT_SLICE_MS: u32 = 200;
/// stderr 只留最后这么多字节（错误信息里贴尾部就够）。
const STDERR_TAIL_LIMIT: usize = 8 * 1024;

/// 构建任务的状态机（对外是字符串）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Idle,
    Running,
    /// 构建进程已成功退出，等 core 把它登记进 `capsules.json`。
    Registering,
    Done,
    Failed,
    Canceled,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Idle => "idle",
            Phase::Running => "running",
            Phase::Registering => "registering",
            Phase::Done => "done",
            Phase::Failed => "failed",
            Phase::Canceled => "canceled",
        }
    }

    fn active(self) -> bool {
        matches!(self, Phase::Running | Phase::Registering)
    }
}

struct Inner {
    phase: Phase,
    step: Option<Step>,
    request: Option<BuildRequest>,
    started_at: u64,
    started: Option<Instant>,
    elapsed_ms: u64,
    pid: Option<u32>,
    error: Option<String>,
    report: Option<BuildReport>,
    registered: bool,
    log_path: Option<PathBuf>,
    ring: logs::RingBuffer,
    file: Option<File>,
    last: Option<gui::BuilderLastView>,
    /// 构建成功但还没登记的产物（supervisor 每轮取一次）。
    pending_register: Option<BuildReport>,
    /// 版本目录缓存 + 是否正在后台抓。
    catalog: Option<dsh_housing_builder::Catalog>,
    fetching_versions: bool,
}

impl Inner {
    fn new() -> Self {
        Self {
            phase: Phase::Idle,
            step: None,
            request: None,
            started_at: 0,
            started: None,
            elapsed_ms: 0,
            pid: None,
            error: None,
            report: None,
            registered: false,
            log_path: None,
            ring: logs::RingBuffer::new(LOG_RING_CAPACITY),
            file: None,
            last: None,
            pending_register: None,
            catalog: None,
            fetching_versions: false,
        }
    }
}

/// 构建任务宿主（core 持有唯一一份）。
pub struct Builder {
    data_dir: PathBuf,
    inner: Arc<Mutex<Inner>>,
    cancel: Arc<AtomicBool>,
    /// 构建子进程的 Job（取消 / core 退出时整棵树收掉）。
    job: Arc<Mutex<Option<OwnedHandle>>>,
    /// 版本目录抓取的 Job——**必须与构建分开**：清空一个 Job 槽会 drop 句柄，
    /// 而 `KILL_ON_JOB_CLOSE` 会把还在跑的 curl 一起杀掉（实测踩过：抓取返回空 stdout）。
    versions_job: Arc<Mutex<Option<OwnedHandle>>>,
    thread: Mutex<Option<JoinHandle<()>>>,
}

impl Builder {
    pub fn new(data_dir: &Path) -> Self {
        let mut inner = Inner::new();
        // 缓存落盘，重启后下拉仍可用（§5.14 D43）。
        let cache = dsh_housing_builder::versions::cache_path(data_dir);
        inner.catalog = dsh_housing_builder::versions::load(&cache);
        Self {
            data_dir: data_dir.to_path_buf(),
            inner: Arc::new(Mutex::new(inner)),
            cancel: Arc::new(AtomicBool::new(false)),
            job: Arc::new(Mutex::new(None)),
            versions_job: Arc::new(Mutex::new(None)),
            thread: Mutex::new(None),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 构建快照（`/api/state` 的 `builder` 区块）。
    pub fn view(&self) -> gui::BuilderView {
        let inner = self.lock();
        let step = inner.step;
        let elapsed_ms = match inner.started {
            Some(started) if inner.phase == Phase::Running => started.elapsed().as_millis() as u64,
            _ => inner.elapsed_ms,
        };
        gui::BuilderView {
            phase: inner.phase.as_str().to_string(),
            step: step.map(|step| format!("{step:?}").to_lowercase()),
            step_title: step
                .map(|step| step.title().to_string())
                .unwrap_or_default(),
            step_index: step.map(|step| step.index()).unwrap_or(0),
            step_total: STEP_TOTAL,
            request: inner
                .request
                .as_ref()
                .map(|request| gui::BuilderRequestView {
                    name: request.name.clone(),
                    dsh: request.dsh_version.clone(),
                    node: request.node_version.clone(),
                    profile: request.profile.clone(),
                    port: request.port,
                    root: request.root.display().to_string(),
                    overwrite: request.overwrite,
                    dry_run: request.dry_run,
                }),
            started_at: inner.started_at,
            elapsed_ms,
            pid: inner.pid,
            error: inner.error.clone(),
            result: inner.report.as_ref().map(|report| gui::BuilderResultView {
                capsule_id: report.capsule_id.clone(),
                registered: inner.registered,
                dsh_version: report.dsh_version.clone(),
                node_version: report.node_version.clone(),
                capsule_root: report.capsule_root.display().to_string(),
            }),
            log_path: inner
                .log_path
                .as_ref()
                .map(|path| path.display().to_string()),
            log_source: if inner.phase == Phase::Running {
                "ring".to_string()
            } else if inner.log_path.as_ref().is_some_and(|path| path.is_file()) {
                "file".to_string()
            } else {
                "none".to_string()
            },
            last: inner.last.clone(),
        }
    }

    /// 起一次构建（已有构建在跑 → `409`）。
    pub fn start(&self, request: BuildRequest) -> Result<(), (u16, String)> {
        let path = {
            let mut inner = self.lock();
            if inner.phase.active() {
                return Err((409, "已有构建在进行，等它结束或先取消".to_string()));
            }
            let log_path = self.build_log_path(&request.name);
            if let Some(parent) = log_path.parent() {
                if let Err(err) = std::fs::create_dir_all(parent) {
                    return Err((
                        500,
                        format!("建日志目录失败（{}）：{err}", parent.display()),
                    ));
                }
            }
            let file = match OpenOptions::new().create(true).append(true).open(&log_path) {
                Ok(file) => file,
                Err(err) => {
                    return Err((
                        500,
                        format!("打开构建日志失败（{}）：{err}", log_path.display()),
                    ))
                }
            };
            inner.phase = Phase::Running;
            inner.step = None;
            inner.request = Some(request.clone());
            inner.started_at = now_unix();
            inner.started = Some(Instant::now());
            inner.elapsed_ms = 0;
            inner.pid = None;
            inner.error = None;
            inner.report = None;
            inner.registered = false;
            inner.log_path = Some(log_path.clone());
            inner.ring = logs::RingBuffer::new(LOG_RING_CAPACITY);
            inner.file = Some(file);
            inner.pending_register = None;
            log_path
        };
        self.cancel.store(false, Ordering::SeqCst);
        *self
            .job
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;

        diag::info(&format!(
            "构建开始：{}（dsh {} / node {}，日志 {}）",
            request.name,
            request.dsh_version,
            request.node_version,
            path.display()
        ));

        let inner = self.inner.clone();
        let cancel = self.cancel.clone();
        let job_slot = self.job.clone();
        let handle = thread::Builder::new()
            .name("builder".to_string())
            .spawn(move || {
                let runner = Runner {
                    inner: inner.clone(),
                    cancel: cancel.clone(),
                    job: job_slot.clone(),
                    track: true,
                };
                let result = dsh_housing_builder::run(&request, &runner);
                let mut guard = inner
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                guard.file = None;
                if let Some(started) = guard.started.take() {
                    guard.elapsed_ms = started.elapsed().as_millis() as u64;
                }
                match result {
                    Ok(report) if report.dry_run => {
                        guard.phase = Phase::Done;
                        guard.registered = false;
                        guard.last = Some(last_view(&report, Phase::Done, guard.elapsed_ms, None));
                        guard.report = Some(report);
                        diag::info("构建结束：干跑（未登记）");
                    }
                    Ok(report) => {
                        guard.phase = Phase::Registering;
                        guard.pending_register = Some(report.clone());
                        guard.report = Some(report);
                        diag::info("构建结束：产物就绪，等待登记");
                    }
                    Err(err) if err.is_cancelled() => {
                        guard.phase = Phase::Canceled;
                        guard.error = Some(err.to_string());
                        guard.last = Some(gui::BuilderLastView {
                            name: request.name.clone(),
                            phase: Phase::Canceled.as_str().to_string(),
                            elapsed_ms: guard.elapsed_ms,
                            error: None,
                        });
                        diag::warn(&format!("构建已取消：{}", request.name));
                    }
                    Err(err) => {
                        guard.phase = Phase::Failed;
                        guard.error = Some(err.to_string());
                        guard.last = Some(gui::BuilderLastView {
                            name: request.name.clone(),
                            phase: Phase::Failed.as_str().to_string(),
                            elapsed_ms: guard.elapsed_ms,
                            error: Some(err.to_string()),
                        });
                        diag::warn(&format!("构建失败：{} —— {err}", request.name));
                    }
                }
            })
            .map_err(|err| (500, format!("起构建线程失败：{err}")))?;
        *self
            .thread
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(handle);
        Ok(())
    }

    /// 取消进行中的构建（没有在跑 → `409`）。
    pub fn cancel(&self) -> Result<(), (u16, String)> {
        {
            let inner = self.lock();
            if !inner.phase.active() {
                return Err((409, "没有进行中的构建".to_string()));
            }
        }
        self.cancel.store(true, Ordering::SeqCst);
        let job = self
            .job
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(job) = job.as_ref() {
            if let Err(err) = job::terminate_job(job, CANCEL_EXIT_CODE) {
                diag::warn(&format!("取消构建时终止 Job 失败：{err}"));
            }
        }
        diag::info("构建取消请求已发出");
        Ok(())
    }

    /// supervisor 每轮取一次：有产物就交给 core 登记。
    pub fn take_pending_register(&self) -> Option<BuildReport> {
        let mut inner = self.lock();
        inner.pending_register.take()
    }

    /// 登记结果回填（`Ok(capsule_id)` / `Err(原因)`）。
    pub fn finish_registration(&self, result: Result<String, String>) {
        let mut inner = self.lock();
        match result {
            Ok(capsule_id) => {
                inner.phase = Phase::Done;
                inner.registered = true;
                if let Some(report) = inner.report.as_mut() {
                    report.capsule_id = capsule_id.clone();
                }
                let elapsed = inner.elapsed_ms;
                if let Some(report) = inner.report.clone() {
                    inner.last = Some(last_view(&report, Phase::Done, elapsed, None));
                }
                diag::info("构建产物已登记");
            }
            Err(err) => {
                inner.phase = Phase::Failed;
                inner.registered = false;
                inner.error = Some(format!("构建成功但登记失败：{err}"));
                let elapsed = inner.elapsed_ms;
                if let Some(report) = inner.report.clone() {
                    inner.last = Some(last_view(
                        &report,
                        Phase::Failed,
                        elapsed,
                        inner.error.clone(),
                    ));
                }
                diag::warn(&format!("构建产物登记失败：{err}"));
            }
        }
    }

    /// 构建日志尾部：进行中读环形缓冲，结束后读日志文件。
    pub fn log_tail(&self, max: usize) -> Result<gui::LogPayload, gui::BackendError> {
        let (text, source) = {
            let inner = self.lock();
            if inner.phase == Phase::Running {
                (
                    String::from_utf8_lossy(&inner.ring.tail(max)).to_string(),
                    "ring",
                )
            } else if let Some(path) = inner.log_path.clone() {
                if path.is_file() {
                    // 已结束：读文件尾部（与实例日志同一套语义）。
                    let text = read_tail(&path, max).map_err(|err| {
                        gui::BackendError::internal(format!("读构建日志失败：{err}"))
                    })?;
                    (text, "file")
                } else {
                    (String::new(), "none")
                }
            } else {
                return Err(gui::BackendError::not_found("还没有构建记录"));
            }
        };
        Ok(gui::LogPayload {
            text,
            source: source.to_string(),
        })
    }

    /// 版本目录视图（缓存优先；`refresh` 只触发后台抓取）。
    pub fn versions_view(
        &self,
        default_root: String,
        registry: String,
    ) -> gui::BuilderVersionsView {
        let mut inner = self.lock();
        if inner.catalog.is_none() && !inner.fetching_versions {
            inner.fetching_versions = true;
            drop(inner);
            self.spawn_versions_fetch(registry.clone());
            inner = self.lock();
        }
        let catalog = inner.catalog.clone().unwrap_or_default();
        let stale = !dsh_housing_builder::versions::is_fresh(
            &catalog,
            dsh_housing_builder::versions::now_unix(),
        );
        gui::BuilderVersionsView {
            fetched_at: catalog.fetched_at,
            stale,
            dsh_tags: catalog.dsh_tags,
            dsh_versions: catalog.dsh_versions,
            node_versions: catalog.node_versions,
            default_root,
            default_profile: "web".to_string(),
            profiles: dsh_housing_builder::SHIPPED_PROFILES
                .iter()
                .map(|profile| (*profile).to_string())
                .collect(),
            registry,
        }
    }

    /// 后台抓一次版本目录（抓不到就保留旧缓存）。
    pub fn refresh_versions(&self, registry: String) {
        {
            let mut inner = self.lock();
            if inner.fetching_versions {
                return;
            }
            inner.fetching_versions = true;
        }
        self.spawn_versions_fetch(registry);
    }

    fn spawn_versions_fetch(&self, registry: String) {
        let inner = self.inner.clone();
        let cancel = self.cancel.clone();
        // 独立的 Job 槽：构建开始 / 结束清的是构建那个槽，不能连带杀掉抓取的 curl。
        let job_slot = self.versions_job.clone();
        let cache = dsh_housing_builder::versions::cache_path(&self.data_dir);
        let cwd = self.data_dir.clone();
        thread::Builder::new()
            .name("builder-versions".to_string())
            .spawn(move || {
                let runner = Runner {
                    inner: inner.clone(),
                    cancel,
                    job: job_slot,
                    track: false,
                };
                let result = dsh_housing_builder::versions::fetch(&runner, &registry, &cwd);
                let mut guard = inner
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                guard.fetching_versions = false;
                match result {
                    Ok(catalog) => {
                        if let Err(err) = dsh_housing_builder::versions::save(&cache, &catalog) {
                            diag::warn(&format!("写版本目录缓存失败：{err}"));
                        }
                        guard.catalog = Some(catalog);
                    }
                    Err(err) => diag::warn(&format!("抓版本目录失败：{err}")),
                }
            })
            .ok();
    }

    fn build_log_path(&self, name: &str) -> PathBuf {
        let (year, month, day) = diag::local_date();
        self.data_dir
            .join(crate::store::LOGS_DIR_NAME)
            .join(format!("build-{name}-{year:04}-{month:02}-{day:02}.log"))
    }
}

fn last_view(
    report: &BuildReport,
    phase: Phase,
    elapsed_ms: u64,
    error: Option<String>,
) -> gui::BuilderLastView {
    gui::BuilderLastView {
        name: report.capsule_id.clone(),
        phase: phase.as_str().to_string(),
        elapsed_ms,
        error,
    }
}

pub fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

/// 构建器要用的端口：从 `39410..=39450` 里挑一个**既没登记、也真能绑**的。
///
/// 「空闲」不等于「未登记」（§5.14 D40）：39412 空闲但已登记给 `0.2.0-rc.2-default/web`，
/// 挑它会在启动时撞登记表。
pub fn pick_port(taken: &[u16]) -> Option<u16> {
    (39410..=39450)
        .find(|port| !taken.contains(port) && matches!(port::probe(*port), port::PortStatus::Free))
}

// ── 命令执行（Job Object + 管道泵） ──────────────────────────────────────────

/// `builder::Host` 的实现：跑命令、写日志、响应取消。
struct Runner {
    inner: Arc<Mutex<Inner>>,
    cancel: Arc<AtomicBool>,
    job: Arc<Mutex<Option<OwnedHandle>>>,
    /// 是否把 pid / 日志写进构建快照（版本目录抓取时为 false）。
    track: bool,
}

impl Host for Runner {
    fn run(&self, command: &Command) -> Result<Outcome, BuildError> {
        if self.cancel.load(Ordering::SeqCst) {
            return Err(self.cancelled_error());
        }
        let command_line = command_line(command);
        let env_block = env_block(command);
        let request = job::SpawnRequest {
            program: &command.program,
            command_line: &command_line,
            cwd: &command.cwd,
            env_block: &env_block,
        };
        let spawned = job::spawn(&request).map_err(|err| {
            BuildError::invalid(format!("起进程失败（{}）：{err}", command.display()))
        })?;

        if self.track {
            self.log_line(&format!(
                "$ {}（pid {}，cwd {}）",
                command.display(),
                spawned.pid,
                command.cwd.display()
            ));
            let mut inner = self.lock();
            inner.pid = Some(spawned.pid);
        }

        // 把 job 句柄挂到共享槽：取消时由 `Builder::cancel` 终止整棵树。
        *self
            .job
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(spawned.job);

        let capture: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let err_tail: Arc<Mutex<Vec<u8>>> = Arc::new(Mutex::new(Vec::new()));
        let mut pumps: Vec<JoinHandle<()>> = Vec::new();
        if let Some(stdout) = spawned.stdout {
            pumps.push(self.spawn_pump(stdout, PumpSink::Capture(capture.clone())));
        }
        if let Some(stderr) = spawned.stderr {
            pumps
                .push(self.spawn_pump(stderr, PumpSink::Tail(err_tail.clone(), STDERR_TAIL_LIMIT)));
        }
        drop(spawned.stdin);

        let mut cancelled = false;
        loop {
            if job::wait_process(&spawned.process, WAIT_SLICE_MS) {
                break;
            }
            if self.cancel.load(Ordering::SeqCst) {
                cancelled = true;
                if let Some(job) = self
                    .job
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .as_ref()
                {
                    let _ = job::terminate_job(job, CANCEL_EXIT_CODE);
                }
                let _ = job::wait_process(&spawned.process, 5_000);
                break;
            }
        }
        join_bounded(pumps, Duration::from_secs(2));
        let code = job::process_exit_code(&spawned.process)
            .map(|code| code as i32)
            .unwrap_or(-1);
        *self
            .job
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
        if self.track {
            self.lock().pid = None;
        }
        if cancelled {
            return Err(self.cancelled_error());
        }
        let stdout = String::from_utf8_lossy(
            &capture
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
        .to_string();
        let stderr_tail = String::from_utf8_lossy(
            &err_tail
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()),
        )
        .to_string();
        Ok(Outcome {
            code,
            stdout,
            stderr_tail,
        })
    }

    fn log(&self, message: &str) {
        if self.track {
            self.log_line(message);
        }
    }

    fn step(&self, step: Step) {
        if self.track {
            self.lock().step = Some(step);
            self.log_line(&format!(
                "=== {}/{} {} ===",
                step.index(),
                STEP_TOTAL,
                step.title()
            ));
        }
    }

    fn cancelled(&self) -> bool {
        self.cancel.load(Ordering::SeqCst)
    }
}

impl Runner {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn cancelled_error(&self) -> BuildError {
        let step = self.lock().step.unwrap_or(Step::Skeleton);
        BuildError::cancelled(step)
    }

    fn log_line(&self, message: &str) {
        let line = format!("[builder] {message}\n");
        let mut inner = self.lock();
        inner.ring.push(line.as_bytes());
        if let Some(file) = inner.file.as_mut() {
            let _ = file.write_all(line.as_bytes());
        }
    }

    fn spawn_pump(&self, pipe: OwnedHandle, sink: PumpSink) -> JoinHandle<()> {
        let inner = self.inner.clone();
        let track = self.track;
        thread::Builder::new()
            .name("builder-pump".to_string())
            .spawn(move || {
                let mut buffer = [0u8; 8192];
                loop {
                    let mut read = 0u32;
                    // SAFETY: 句柄由本线程独占持有。
                    let result = unsafe {
                        ReadFile(
                            HANDLE(pipe.as_raw_handle()),
                            Some(&mut buffer),
                            Some(&mut read),
                            None,
                        )
                    };
                    match result {
                        Ok(()) => {
                            if read == 0 {
                                break;
                            }
                            let bytes = &buffer[..read as usize];
                            sink.feed(bytes);
                            if track {
                                if let Ok(mut inner) = inner.lock() {
                                    inner.ring.push(bytes);
                                    if let Some(file) = inner.file.as_mut() {
                                        let _ = file.write_all(bytes);
                                    }
                                }
                            }
                        }
                        Err(err) => {
                            let code = (err.code().0 as u32) & 0xFFFF;
                            if code != ERROR_BROKEN_PIPE.0 && code != ERROR_NO_DATA.0 {
                                diag::warn(&format!("读构建输出失败（Win32 {code}）：{err}"));
                            }
                            break;
                        }
                    }
                }
            })
            .ok()
            .unwrap_or_else(|| {
                thread::Builder::new()
                    .name("builder-pump-noop".to_string())
                    .spawn(|| {})
                    .expect("起泵线程")
            })
    }
}

/// 泵把字节喂到哪里。
enum PumpSink {
    /// 全收（stdout；版本断言要看）。
    Capture(Arc<Mutex<Vec<u8>>>),
    /// 只留最后 N 字节（stderr；错误信息要看尾部）。
    Tail(Arc<Mutex<Vec<u8>>>, usize),
}

impl PumpSink {
    fn feed(&self, bytes: &[u8]) {
        match self {
            PumpSink::Capture(sink) => {
                if let Ok(mut sink) = sink.lock() {
                    if sink.len() < CAPTURE_LIMIT {
                        let room = CAPTURE_LIMIT - sink.len();
                        let take = room.min(bytes.len());
                        sink.extend_from_slice(&bytes[..take]);
                    }
                }
            }
            PumpSink::Tail(sink, limit) => {
                if let Ok(mut sink) = sink.lock() {
                    sink.extend_from_slice(bytes);
                    let len = sink.len();
                    if len > *limit {
                        sink.drain(..len - *limit);
                    }
                }
            }
        }
    }
}

fn join_bounded(handles: Vec<JoinHandle<()>>, timeout: Duration) {
    let deadline = Instant::now() + timeout;
    for handle in handles {
        while !handle.is_finished() {
            if Instant::now() >= deadline {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
        let _ = handle.join();
    }
}

/// 读一个文件的尾部（最多 `max` 字节；非 UTF-8 用替换字符兜住）。
fn read_tail(path: &Path, max: usize) -> std::io::Result<String> {
    use std::io::{Read, Seek, SeekFrom};

    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    let take = (max as u64).min(len);
    file.seek(SeekFrom::Start(len - take))?;
    let mut buffer = Vec::with_capacity(take as usize);
    file.read_to_end(&mut buffer)?;
    Ok(String::from_utf8_lossy(&buffer).to_string())
}

/// `program arg1 arg2 …` → UTF-16（CreateProcessW 需要 lpCommandLine 含 argv[0]）。
fn command_line(command: &Command) -> Vec<u16> {
    let mut text = env::quote_arg(&command.program.to_string_lossy());
    for arg in &command.args {
        text.push(' ');
        text.push_str(&env::quote_arg(&arg.to_string_lossy()));
    }
    env::wide(&text)
}

/// 继承环境 + 覆盖，排序后编码成 environment block。
fn env_block(command: &Command) -> Vec<u16> {
    let mut entries: Vec<(String, String)> = Vec::new();
    for (key, value) in std::env::vars_os() {
        let key = key.to_string_lossy().to_string();
        entries.push((key, value.to_string_lossy().to_string()));
    }
    entries.retain(|(key, _)| !is_stripped(key));
    if let Some(path) = wide_path(command) {
        entries.push(("PATH".to_string(), path));
    }
    for (key, value) in &command.env {
        entries.push((
            key.to_string_lossy().to_string(),
            value.to_string_lossy().to_string(),
        ));
    }
    entries.sort_by_key(|entry| entry.0.to_ascii_lowercase());
    entries.dedup_by(|left, right| left.0.eq_ignore_ascii_case(&right.0));

    let mut text = String::new();
    for (key, value) in entries {
        text.push_str(&key);
        text.push('=');
        text.push_str(&value);
        text.push('\0');
    }
    text.push('\0');
    text.encode_utf16().collect()
}

/// 这些变量不继承：壳自己的（`DSH_*`）、npm 的全局配置（会盖掉我们想固定的 registry / cache）、
/// 以及我们要重设的 `PATH` / `TEMP` / `TMP`。
fn is_stripped(key: &str) -> bool {
    let lower = key.to_ascii_lowercase();
    lower.starts_with("dsh_")
        || lower.starts_with("npm_")
        || lower == "node_options"
        || matches!(lower.as_str(), "path" | "temp" | "tmp" | "node_path")
}

fn wide_path(command: &Command) -> Option<String> {
    let inherited = std::env::var_os("PATH").unwrap_or_default();
    let mut parts: Vec<String> = command
        .path_prepend
        .iter()
        .map(|path| path.display().to_string())
        .collect();
    let base = inherited.to_string_lossy().to_string();
    if !base.is_empty() {
        parts.push(base);
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts.join(";"))
    }
}

/// `OsStr` 便于调用方拼参数。
pub fn os(value: &str) -> OsString {
    OsString::from(value)
}

/// 让 `OsStr` 也能参与 `Command`（避免调用方到处 `to_string_lossy`）。
pub fn os_path(path: &Path) -> OsString {
    path.as_os_str().to_os_string()
}
