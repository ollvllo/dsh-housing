//! 一个实例的全部运行时状态（docs/structure.md §2.2.1 所有权表）。
//!
//! 状态机：`Stopped → Starting → Running → Stopping → Stopped`，任何阶段都可能
//! 落到 `Failed(原因)`。迁移**只由 core 决定**，UI 只看快照。
//!
//! 所有权：
//!
//! - Job 句柄、子进程句柄、stdin 写端、日志管线（含两个读线程）都归本结构；
//! - `take_for_stop` 把这些资源**移交给停止线程**，停止期间 `process` 为 `None`，
//!   所以 supervisor 不会和停止线程抢着判定「进程退出了」。这是本模块唯一的并发关键点。
//!
//! 「启动成功」的口径（用户 2026-10-19 裁决）：**`CreateProcessW` 成功就算启动过**。
//! 进程起来后配置不对、dsh 自己退出 → 那是 `Failed(进程自行退出)`，不是启动前拦截。

use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

use crate::job;
use crate::logs::LogPump;
use crate::model::{InstanceMetrics, InstanceSpec, InstanceView};
use crate::monitor::Sampler;
use crate::readiness::TokenUrl;

/// readiness 超时：60 s 内没抓到 token URL 就标 Failed（进程留着，用户可停）。
pub const READINESS_TIMEOUT: Duration = Duration::from_secs(60);

/// 实例状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InstanceState {
    Stopped,
    Starting,
    Running,
    Stopping,
    /// 启动失败 / 自行退出 / readiness 超时；字符串是给人看的原因。
    Failed(String),
}

impl InstanceState {
    /// IPC / GUI 用的线上标签。
    pub fn label(&self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Stopping => "stopping",
            Self::Failed(_) => "failed",
        }
    }

    pub fn is_active(&self) -> bool {
        matches!(self, Self::Starting | Self::Running | Self::Stopping)
    }

    pub fn detail(&self) -> Option<&str> {
        match self {
            Self::Failed(reason) => Some(reason),
            _ => None,
        }
    }
}

/// 停止线程从实例手里接过去的资源。
pub struct StopResources {
    pub pid: Option<u32>,
    pub process: Option<std::os::windows::io::OwnedHandle>,
    pub job: Option<std::os::windows::io::OwnedHandle>,
    pub stdin: Option<std::os::windows::io::OwnedHandle>,
    pub logs: Option<LogPump>,
}

/// supervisor 一次轮询的观察结果（为空表示状态没变化）。
#[derive(Default)]
pub struct Observation {
    /// 刚抓到 token URL（端口）。
    pub ready: Option<TokenUrl>,
    /// 刚判定 readiness 超时。
    pub readiness_timeout: bool,
    /// 进程刚刚退出（未被主动停止）。
    pub exited: Option<ExitInfo>,
}

pub struct ExitInfo {
    pub exit_code: Option<u32>,
}

struct Inner {
    state: InstanceState,
    pid: Option<u32>,
    process: Option<std::os::windows::io::OwnedHandle>,
    job: Option<std::os::windows::io::OwnedHandle>,
    stdin: Option<std::os::windows::io::OwnedHandle>,
    logs: Option<LogPump>,
    token_url: Option<String>,
    started_at: Option<Instant>,
    stop_requested: bool,
    readiness_timeout_reported: bool,
    exit_code: Option<u32>,
    /// 最近一次资源采样（§3.9）；没有进程时为 `None`。
    metrics: Option<InstanceMetrics>,
    /// CPU% 差分的基线（上一次的 job 累计 CPU 与时刻）。
    sampler: Sampler,
}

/// 一个实例。所有可变状态在 `Mutex` 后面；快照是拷贝。
pub struct Instance {
    spec: InstanceSpec,
    key: String,
    inner: Mutex<Inner>,
}

impl Instance {
    pub fn new(spec: InstanceSpec) -> Arc<Self> {
        let key = spec.key();
        Arc::new(Self {
            spec,
            key,
            inner: Mutex::new(Inner {
                state: InstanceState::Stopped,
                pid: None,
                process: None,
                job: None,
                stdin: None,
                logs: None,
                token_url: None,
                started_at: None,
                stop_requested: false,
                readiness_timeout_reported: false,
                exit_code: None,
                metrics: None,
                sampler: Sampler::new(),
            }),
        })
    }

    pub fn spec(&self) -> &InstanceSpec {
        &self.spec
    }

    pub fn key(&self) -> &str {
        &self.key
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 子进程起来之后挂上资源，状态转 `Starting`。
    pub fn attach(
        &self,
        pid: u32,
        process: std::os::windows::io::OwnedHandle,
        job: std::os::windows::io::OwnedHandle,
        stdin: Option<std::os::windows::io::OwnedHandle>,
        logs: LogPump,
    ) {
        let mut inner = self.lock();
        inner.state = InstanceState::Starting;
        inner.pid = Some(pid);
        inner.process = Some(process);
        inner.job = Some(job);
        inner.stdin = stdin;
        inner.logs = Some(logs);
        inner.started_at = Some(Instant::now());
        inner.stop_requested = false;
        inner.readiness_timeout_reported = false;
        inner.exit_code = None;
        inner.metrics = None;
        inner.sampler = Sampler::new();
    }

    /// 进程没起来（`CreateProcessW` / 日志线程失败）：状态转 `Failed`。
    pub fn mark_spawn_failed(&self, reason: impl Into<String>) {
        let mut inner = self.lock();
        inner.state = InstanceState::Failed(reason.into());
        inner.pid = None;
        inner.process = None;
        inner.job = None;
        inner.stdin = None;
        inner.logs = None;
        inner.started_at = None;
        inner.metrics = None;
        inner.sampler = Sampler::new();
    }

    /// 停止路径第一步：状态转 `Stopping` 并交出全部资源。
    ///
    /// 已经停止 / 从来没有进程的实例返回 `None`（调用方视为 no-op）。
    pub fn take_for_stop(&self) -> Option<StopResources> {
        let mut inner = self.lock();
        if inner.process.is_none() {
            // 没有进程：如果还停留在 Failed，就直接落到 Stopped。
            if matches!(inner.state, InstanceState::Failed(_)) {
                inner.state = InstanceState::Stopped;
            }
            return None;
        }
        inner.state = InstanceState::Stopping;
        inner.stop_requested = true;
        Some(StopResources {
            pid: inner.pid,
            process: inner.process.take(),
            job: inner.job.take(),
            stdin: inner.stdin.take(),
            logs: inner.logs.take(),
        })
    }

    /// 停止线程收尾：状态落 `Stopped`。
    pub fn finish_stop(&self, exit_code: Option<u32>, detail: Option<String>) {
        let mut inner = self.lock();
        inner.state = match detail {
            Some(reason) => InstanceState::Failed(reason),
            None => InstanceState::Stopped,
        };
        inner.pid = None;
        inner.process = None;
        inner.job = None;
        inner.stdin = None;
        inner.logs = None;
        inner.stop_requested = false;
        inner.exit_code = exit_code;
        inner.token_url = None;
        inner.started_at = None;
        inner.metrics = None;
        inner.sampler = Sampler::new();
    }

    /// supervisor 轮询：推进 readiness、判定 readiness 超时、发现进程自行退出。
    pub fn observe(&self) -> Observation {
        let mut observation = Observation::default();
        let mut inner = self.lock();

        if inner.token_url.is_none() {
            if let Some(found) = inner.logs.as_ref().and_then(LogPump::readiness) {
                inner.token_url = Some(found.url.clone());
                if matches!(
                    inner.state,
                    InstanceState::Starting | InstanceState::Failed(_)
                ) {
                    inner.state = InstanceState::Running;
                }
                observation.ready = Some(found);
            }
        }

        if inner.token_url.is_none()
            && matches!(inner.state, InstanceState::Starting)
            && !inner.readiness_timeout_reported
        {
            if let Some(started) = inner.started_at {
                if started.elapsed() >= READINESS_TIMEOUT {
                    inner.state = InstanceState::Failed(format!(
                        "readiness 超时（{}s 内没在 stdout 看到 token URL；进程仍在运行）",
                        READINESS_TIMEOUT.as_secs()
                    ));
                    inner.readiness_timeout_reported = true;
                    observation.readiness_timeout = true;
                }
            }
        }

        if let Some(process) = inner.process.as_ref() {
            if !job::is_process_running(process) {
                let exit_code = job::process_exit_code(process);
                let was_stopping = inner.stop_requested;
                inner.process = None;
                inner.job = None;
                inner.stdin = None;
                inner.logs = None;
                inner.pid = None;
                inner.started_at = None;
                inner.state = if was_stopping {
                    InstanceState::Stopped
                } else {
                    InstanceState::Failed(format!(
                        "进程自行退出（退出码 {}，0x{:08X}）",
                        exit_code.unwrap_or(0),
                        exit_code.unwrap_or(0)
                    ))
                };
                inner.exit_code = exit_code;
                inner.metrics = None;
                inner.sampler = Sampler::new();
                observation.exited = Some(ExitInfo { exit_code });
            }
        }

        observation
    }

    /// job 内当前 PID（M1 用来实测「MCP 子进程是否留在 job 内」，M2 资源统计复用）。
    pub fn job_pids(&self) -> Vec<u32> {
        let inner = self.lock();
        inner
            .job
            .as_ref()
            .map(job::job_process_ids)
            .unwrap_or_default()
    }

    /// 采样一次资源用量并记进快照（§3.9）。没有进程 / job 查询失败时返回 `None`，
    /// 保留上一次的数字，不清空。
    pub fn sample_metrics(&self) -> Option<InstanceMetrics> {
        let mut inner = self.lock();
        // `Sampler` 是 `Copy`：先取出来再借用 job，避免同一 guard 上同时借两个字段。
        let mut sampler = inner.sampler;
        let job = inner.job.as_ref()?;
        let metrics = crate::monitor::sample(job, &mut sampler)?;
        inner.sampler = sampler;
        inner.metrics = Some(metrics);
        Some(metrics)
    }

    pub fn has_process(&self) -> bool {
        self.lock().process.is_some()
    }

    pub fn is_active(&self) -> bool {
        self.lock().state.is_active()
    }

    pub fn state_label(&self) -> &'static str {
        // label() 返回 &'static str，但要在锁外返回：先 clone 状态标签。
        // 这里直接借字符串常量，无需持有锁。
        self.lock().state.label()
    }

    pub fn token_url(&self) -> Option<String> {
        self.lock().token_url.clone()
    }

    /// 内存环形缓冲的尾部（M3 控制台用）。没有进程 / 还没起日志管线时返回 `None`。
    pub fn ring_tail(&self, max: usize) -> Option<Vec<u8>> {
        self.lock().logs.as_ref().map(|pump| pump.ring_tail(max))
    }

    pub fn exit_code(&self) -> Option<u32> {
        self.lock().exit_code
    }

    /// 只读快照（托盘 / GUI 拿到的唯一形式）。
    pub fn snapshot(&self) -> InstanceView {
        let inner = self.lock();
        InstanceView {
            key: self.key.clone(),
            capsule_id: self.spec.capsule_id.clone(),
            profile: self.spec.profile.clone(),
            alias: self.spec.alias.clone(),
            port: self.spec.port,
            host: self.spec.host.clone(),
            state: inner.state.label().to_string(),
            detail: inner.state.detail().map(str::to_string),
            pid: inner.pid,
            token_url: inner.token_url.clone(),
            metrics: inner.metrics,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn spec() -> InstanceSpec {
        InstanceSpec {
            capsule_root: PathBuf::from(r"D:\capsules\cap"),
            capsule_id: "cap".to_string(),
            profile: "web".to_string(),
            port: 39412,
            host: None,
            workspace: PathBuf::from(r"D:\capsules\cap\workspace-web"),
            env: BTreeMap::new(),
            alias: Some("测试实例".to_string()),
        }
    }

    #[test]
    fn new_instance_is_stopped_and_has_a_stable_key() {
        let instance = Instance::new(spec());
        assert_eq!(instance.key(), "cap/web");
        assert_eq!(instance.spec().port, 39412);
        let snapshot = instance.snapshot();
        assert_eq!(snapshot.state, "stopped");
        assert_eq!(snapshot.alias.as_deref(), Some("测试实例"));
        assert_eq!(snapshot.pid, None);
        assert!(!instance.is_active());
        assert!(!instance.has_process());
    }

    #[test]
    fn spawn_failure_is_reported_as_failed_without_process() {
        let instance = Instance::new(spec());
        instance.mark_spawn_failed("找不到 node.exe");
        let snapshot = instance.snapshot();
        assert_eq!(snapshot.state, "failed");
        assert_eq!(snapshot.detail.as_deref(), Some("找不到 node.exe"));
        assert!(instance.take_for_stop().is_none(), "没有进程无需停止");
        assert_eq!(instance.state_label(), "stopped");
    }

    #[test]
    fn observation_is_empty_without_process() {
        let instance = Instance::new(spec());
        let observation = instance.observe();
        assert!(observation.ready.is_none());
        assert!(!observation.readiness_timeout);
        assert!(observation.exited.is_none());
    }
}
