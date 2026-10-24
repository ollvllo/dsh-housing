//! 主进程运行时：单实例锁、IPC 命令处理、实例表、托盘监督、启动 / 停止编排。
//!
//! 这里没有 Windows 消息泵：M1 的 IPC 走命名管道（`ipc.rs`），事件循环就是
//! 本模块的 supervisor 循环（250 ms 一轮）+ IPC 服务线程 + 每个停止操作一个线程。
//! 文档 §2.2.1 里「事件循环」的所有权仍然在 core，只是实现从「消息泵」换成了
//! 「命令队列 + 轮询」——这正是 §5.2 第 2 条在 M1 的裁决结果。
//!
//! 锁顺序（避免死锁，代码里统一遵守）：
//!
//! 1. `state`（实例表 / shutting_down）先，`tray` 后；
//! 2. `Instance` 自己的锁最后；
//! 3. **任何持锁路径都不等待进程**——等待都在停止线程里做。

use std::collections::BTreeMap;
use std::io;
use std::os::windows::io::{FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use dsh_housing_gui as gui;
use dsh_housing_webhook::{Trigger as HookTrigger, Webhook, WebhookFile};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{GetLastError, SetLastError, ERROR_ALREADY_EXISTS, ERROR_SUCCESS};
use windows::Win32::System::Threading::CreateMutexW;

use crate::autostart;
use crate::browser;
use crate::build;
use crate::capsule::{self, CapsulesFile};
use crate::console::{self, ConsoleService};
use crate::diag;
use crate::env;
use crate::instance::Instance;
use crate::ipc::{self, Request, Response};
use crate::job::{self, SpawnRequest, SpawnedInstance, FORCE_KILL_EXIT_CODE};
use crate::logs;
use crate::model::{InstanceSpec, InstanceView, Settings, StatusSnapshot};
use crate::net;
use crate::notify;
use crate::port::{self, PortStatus};
use crate::store;

/// supervisor 轮询间隔。
const SUPERVISE_INTERVAL: Duration = Duration::from_millis(250);
/// 采样间隔下限（防止手改 `settings.json` 把 core 拖进高频轮询）。
const MIN_SAMPLE_INTERVAL_MS: u64 = 500;
/// 优雅停等待上限（§3.7：5–10 s）。
const GRACEFUL_STOP_TIMEOUT: Duration = Duration::from_secs(5);
/// 强杀后等 job 清空的上限。
const FORCE_STOP_TIMEOUT: Duration = Duration::from_secs(5);
/// 停止后等日志线程收尾的上限（逃出 job 的孙进程可能攥着管道写端）。
const LOG_JOIN_TIMEOUT: Duration = Duration::from_secs(2);
/// 托盘存活超过这么久，就把「崩溃次数」清零（防止一整天零星崩溃累加到上限）。
const TRAY_STABLE_UPTIME: Duration = Duration::from_secs(60);
/// core 退出前等托盘自己退出的上限，超时 TerminateProcess。
const TRAY_EXIT_WAIT: Duration = Duration::from_secs(3);
/// 单实例互斥体（`Local\` = 按登录会话）。
const SINGLE_INSTANCE_MUTEX: &str = r"Local\dsh-housing-core";
/// 自启动状态缓存的有效期（控制台轮询 `/api/state` 时不能每次都打 COM，§3.12）。
const AUTOSTART_CACHE_TTL: Duration = Duration::from_secs(5);

/// `--role=core`（缺省）入口。
pub fn run() -> i32 {
    let data_dir = match store::resolve_data_dir() {
        Ok(dir) => dir,
        Err(err) => {
            diag::show_error(&format!(
                "dsh-housing 启动失败：无法定位数据目录。\n\n{err}"
            ));
            return 1;
        }
    };

    // 日志落在数据目录里，所以必须在 bootstrap 之前知道路径；写不进去时 diag 静默丢弃。
    diag::init(&data_dir);
    diag::install_panic_hook();

    let report = match store::bootstrap() {
        Ok(report) => report,
        Err(err) => {
            let text = format!(
                "dsh-housing 启动失败：数据目录不可用。\n\n{err}\n\n\
                 数据目录是 exe 同级的 .dsh-housing\\，必须可写；\
                 请把 exe 换到可写位置后重试（不静默回退到别的目录）。"
            );
            diag::error(&format!("core 启动失败：{err}"));
            diag::show_error(&text);
            return 1;
        }
    };
    diag::info(&format!(
        "core 启动（M2）：exe={}；数据目录={}；本次新建={:?}",
        report.exe_path.display(),
        report.data_dir.display(),
        report.created
    ));

    let settings = match store::load_settings(&data_dir) {
        Ok(settings) => settings,
        Err(err) => {
            // 设置文件坏掉不该让启动器起不来：告警后按默认值走。
            diag::error(&format!("{err}；按默认设置继续"));
            Settings::default()
        }
    };

    let registry = load_registry(&data_dir);
    log_registry(&registry);
    let webhooks = load_webhooks(&data_dir);

    let _instance_lock = match acquire_single_instance() {
        Ok(Some(handle)) => handle,
        Ok(None) => {
            // 第二个 core：让已有 core 把托盘点回来，然后退出。
            let pipe = ipc::pipe_name();
            match ipc::call(&pipe, &Request::Wake, Duration::from_secs(2)) {
                Ok(_) => diag::info("已有 core 在运行：已投递唤醒请求，本实例退出"),
                Err(err) => diag::warn(&format!("已有 core 在运行，但唤醒请求失败：{err}")),
            }
            return 0;
        }
        Err(err) => {
            diag::error(&format!("单实例互斥体创建失败：{err}"));
            diag::show_error(&format!(
                "dsh-housing 启动失败：无法创建单实例互斥体。\n\n{err}"
            ));
            return 1;
        }
    };

    // 控制台要在 Runtime 构造之前留一份设置（settings 随后被移动进 Runtime）。
    let console_settings = settings.clone();
    // 完整性级别是进程级常量：读一次，之后由控制台展示 / 作为自启动的启用前提（§5.4 / D17）。
    let integrity = autostart::current_integrity();
    diag::info(&format!("进程完整性级别：{}", integrity.label()));
    let runtime = Arc::new(Runtime::new(
        report.exe_path.clone(),
        data_dir.clone(),
        settings,
        registry,
        webhooks,
        integrity,
    ));
    // 启动时先清一次旧日志（§3.10）；之后由 supervisor 在跨零点时再跑。
    runtime.purge_logs_if_due();
    let pipe = runtime.pipe_name.clone();
    let handler_runtime = runtime.clone();
    match ipc::spawn_server(
        &pipe,
        Arc::new(move |request| handler_runtime.handle(request)),
    ) {
        Ok(_server) => diag::info(&format!("IPC 服务就绪：{pipe}")),
        Err(err) => {
            diag::error(&format!("{err}"));
            diag::show_error(&format!(
                "dsh-housing 启动失败：IPC 服务起不来。\n\n{err}\n\n\
                 托盘与 --stop-all 都依赖它，core 无法继续。"
            ));
            return 1;
        }
    }

    // 控制台（M3）：绑定失败只告警，实例管理不受影响（§5.8 D9）。
    match console::start(&console_settings, runtime.clone()) {
        Ok(service) => {
            diag::info(&format!(
                "控制台已就绪：http://{}:{}/（绑定 {}；token 每次启动重新生成，不落盘）",
                service.host(),
                service.port(),
                console_settings.console_host
            ));
            runtime.set_console(service);
            // v0.3 / #259：控制台就绪后先抛 Housing Start（早于实例级自启）。
            runtime.announce_housing_start();
            // v0.3 / #251：默认把控制台页面打开，避免「双击后只有托盘」的静默启动观感。
            runtime.open_console_if_configured();
        }
        Err(err) => diag::warn(&format!("控制台不可用：{err}；实例管理照常")),
    }

    // 实例级自启（M4）：IPC 与控制台都就绪之后再拉起，失败只记日志（§3.12 / D16）。
    runtime.start_autostart_instances();

    runtime.supervise_loop();
    runtime.finish_tray();
    // 退出前的 webhook：「Service Stop」事件是停止线程在收尾时抛出来的，这里等一会儿再退，
    // 免得「关掉 dsh-housing」这件事一条通知都发不出去（有界：3 s）。
    if !runtime.drain_webhooks(Duration::from_secs(3)) {
        diag::warn("webhook：退出前 3 s 内没把队列发完，剩余事件随进程丢弃");
    }
    diag::info("core 退出");
    runtime.exit_code()
}

/// `--stop-all` 兜底通道：请求已有 core「停全部实例并退出」。
///
/// 退出码固定 3（README 的退出码口径）；它只是内部兜底，不往 CLI 方向扩。
pub fn run_stop_all() -> i32 {
    if let Ok(data_dir) = store::resolve_data_dir() {
        diag::init(&data_dir);
    }
    diag::install_panic_hook();
    let pipe = ipc::pipe_name();
    match ipc::call(&pipe, &Request::Quit, Duration::from_secs(5)) {
        Ok(response) if response.ok => diag::info("--stop-all：已请求 core 停全部实例并退出"),
        Ok(response) => diag::error(&format!(
            "--stop-all：core 拒绝请求：{}",
            response.error.unwrap_or_default()
        )),
        Err(err) => diag::error(&format!(
            "--stop-all：没有运行中的 core（或 IPC 失败）：{err}"
        )),
    }
    3
}

/// 取单实例互斥体。`Ok(None)` = 已有 core 在跑。
fn acquire_single_instance() -> Result<Option<OwnedHandle>, String> {
    let name = wide(SINGLE_INSTANCE_MUTEX);
    // SAFETY: 名字是 NUL 结尾的宽字符串；CreateMutexW 失败才返回 Err。
    unsafe {
        SetLastError(ERROR_SUCCESS);
        let handle = CreateMutexW(None, false, PCWSTR(name.as_ptr()))
            .map_err(|err| format!("CreateMutexW：{err}"))?;
        if GetLastError() == ERROR_ALREADY_EXISTS {
            drop(OwnedHandle::from_raw_handle(handle.0));
            Ok(None)
        } else {
            Ok(Some(OwnedHandle::from_raw_handle(handle.0)))
        }
    }
}

struct RuntimeState {
    instances: Vec<Arc<Instance>>,
    shutting_down: bool,
    /// 下一次资源采样的时刻（§3.9）。
    next_sample: Instant,
    /// 上一次清理日志的本地日期（§3.10）。
    last_purge: (u16, u16, u16),
}

/// 登记表：落盘内容 + 展开后的规格（派生缓存）+ 加载错误。
///
/// 规格是 `file.specs()` 的缓存：`snapshot()` / `start` 都按它找实例，避免每次都重新展开。
struct RegistryState {
    file: CapsulesFile,
    specs: Vec<InstanceSpec>,
    /// 启动时 `capsules.json` 解析失败的原因；首次写盘前要备份坏文件（§2.4 硬规则 2）。
    load_error: Option<String>,
}

impl RegistryState {
    fn from_file(file: CapsulesFile) -> Self {
        let specs = file.specs();
        Self {
            file,
            specs,
            load_error: None,
        }
    }

    /// 事件里要的两个事实：**有效显示名**（§3.20，用户别名优先）与 dsh 版本（`app.version`）。
    ///
    /// 别名以登记表为真源；没登记（理论上 `start` 只按登记项走）时退回实例键。
    fn facts_for(&self, capsule_id: &str, profile: &str) -> (String, String) {
        for record in &self.file.capsules {
            if record.capsule_id != capsule_id {
                continue;
            }
            if let Some(instance) = record.instances.iter().find(|item| item.profile == profile) {
                let alias = effective_alias(record, instance);
                let version = record
                    .app
                    .as_ref()
                    .map(|app| app.version.clone())
                    .unwrap_or_else(|| record.capsule_id.clone());
                return (alias, version);
            }
        }
        (format!("{capsule_id}/{profile}"), capsule_id.to_string())
    }
}

/// 读登记表；解析失败时告警并给空表（不阻断启动，§2.4 硬规则 2）。
fn load_registry(data_dir: &Path) -> RegistryState {
    match store::load_capsules(data_dir) {
        Ok(file) => RegistryState::from_file(file),
        Err(err) => {
            diag::warn(&format!(
                "capsules.json 读不出来（{err}）；本次按空登记表继续，\
                 首次写盘前会把损坏文件改名为 capsules.json.broken-<时间戳>"
            ));
            RegistryState {
                file: CapsulesFile::default(),
                specs: Vec::new(),
                load_error: Some(err.to_string()),
            }
        }
    }
}

fn log_registry(registry: &RegistryState) {
    diag::info(&format!(
        "登记表：{} 个胶囊 / {} 个实例",
        registry.file.capsules.len(),
        registry.specs.len()
    ));
    for record in &registry.file.capsules {
        let profiles = record
            .instances
            .iter()
            .map(|instance| format!("{}({})", instance.profile, instance.port))
            .collect::<Vec<_>>()
            .join("、");
        diag::info(&format!(
            "  胶囊 {}：{}；{profiles}",
            record.capsule_id,
            record.root.display()
        ));
        for warning in &record.warnings {
            diag::warn(&format!("  登记告警（{}）：{warning}", record.capsule_id));
        }
    }
    if let Some(error) = &registry.load_error {
        diag::warn(&format!("  登记表加载错误：{error}"));
    }
}

struct TraySupervisor {
    child: Option<Child>,
    /// 已连续尝试（启动失败 + 崩溃）的次数；稳定运行 60 s 后清零。
    attempts: u32,
    next_attempt: Option<Instant>,
    started_at: Option<Instant>,
}

/// 自启动状态缓存（COM 查询有成本，`/api/state` 每 2 s 一次不能每次都打）。
struct AutostartCache {
    status: autostart::TaskStatus,
    refreshed: Option<Instant>,
}

/// webhook 配置：落盘内容 + 启动时解析失败的原因（与 `capsules.json` 同一套「坏文件先改名」）。
///
/// 运行时真正用于投递的是一份**热换的配置副本**（在 `Notifier` 里），这里只做真源与视图。
struct WebhookState {
    file: WebhookFile,
    load_error: Option<String>,
}

/// 读 `webhooks.json`；解析失败时告警并给空表（不阻断启动，§5.11 D23）。
fn load_webhooks(data_dir: &Path) -> WebhookState {
    match store::load_webhooks(data_dir) {
        Ok(file) => {
            let valid = file.valid_hooks().count();
            if valid != file.webhooks.len() {
                diag::warn(&format!(
                    "webhooks.json 里有 {} 条配置不合法，投递时跳过（控制台里改好再保存即可）",
                    file.webhooks.len() - valid
                ));
            }
            diag::info(&format!("webhook 配置：{} 条", file.webhooks.len()));
            WebhookState {
                file,
                load_error: None,
            }
        }
        Err(err) => {
            diag::warn(&format!(
                "webhooks.json 读不出来（{err}）；本次按空配置继续，\
                 首次写盘前会把损坏文件改名为 webhooks.json.broken-<时间戳>"
            ));
            WebhookState {
                file: WebhookFile::default(),
                load_error: Some(err.to_string()),
            }
        }
    }
}

/// 主进程运行时。由 `Arc` 共享给 IPC 服务线程与 supervisor 循环。
///
/// 锁顺序（避免死锁，代码里统一遵守）：`settings` → `state` → `registry` → `instance`；`tray` /
/// `autostart` 各自一把、都只做短事务。**任何持锁路径都不等待进程**，也不取别的锁。
struct Runtime {
    data_dir: PathBuf,
    /// 壳设置：M4 起可变（控制台热改），每处用到就拷一份，不做长持有。
    settings: Mutex<Settings>,
    self_exe: PathBuf,
    pipe_name: String,
    /// 本进程完整性级别（进程级常量，只读）。
    integrity: autostart::IntegrityLevel,
    state: Mutex<RuntimeState>,
    registry: Mutex<RegistryState>,
    tray: Mutex<TraySupervisor>,
    exit_code: Mutex<i32>,
    /// 控制台（M3）：`None` = 没起来 / 绑定失败（只告警，不阻断 core，§5.8 D9）。
    console: Mutex<Option<ConsoleService>>,
    /// 登录自启动任务现状（M4，§3.12）。
    autostart: Mutex<AutostartCache>,
    /// webhook 配置真源（v0.2，#244）：视图与落盘都在这里，投递副本在 `notifier` 里。
    webhooks: Mutex<WebhookState>,
    /// webhook 事件源：把实例状态机的事实交给核外的投递线程（§3.21）。
    notifier: Arc<notify::Notifier>,
    /// 构建任务宿主（v0.5 / #249，§3.24）：一次只跑一个构建，成功后由 supervisor 登记。
    builder: build::Builder,
}

impl Runtime {
    fn new(
        self_exe: PathBuf,
        data_dir: PathBuf,
        settings: Settings,
        registry: RegistryState,
        webhooks: WebhookState,
        integrity: autostart::IntegrityLevel,
    ) -> Self {
        let pipe_name = ipc::pipe_name();
        // 投递线程随 core 起来；配置改一次热换一次（`apply_webhooks`）。
        let notifier = notify::Notifier::start(webhooks.file.webhooks.clone());
        let builder = build::Builder::new(&data_dir);
        Self {
            data_dir,
            settings: Mutex::new(settings),
            self_exe,
            pipe_name,
            integrity,
            state: Mutex::new(RuntimeState {
                instances: Vec::new(),
                shutting_down: false,
                next_sample: Instant::now(),
                last_purge: (0, 0, 0),
            }),
            registry: Mutex::new(registry),
            tray: Mutex::new(TraySupervisor {
                child: None,
                attempts: 0,
                next_attempt: None,
                started_at: None,
            }),
            exit_code: Mutex::new(0),
            console: Mutex::new(None),
            autostart: Mutex::new(AutostartCache {
                status: autostart::TaskStatus {
                    task_name: autostart::TASK_NAME.to_string(),
                    state: autostart::State::Missing,
                    detail: Some("还没查询（控制台第一次轮询时刷新）".to_string()),
                    action: None,
                    legacy: None,
                },
                refreshed: None,
            }),
            webhooks: Mutex::new(webhooks),
            notifier,
            builder,
        }
    }

    fn lock_settings(&self) -> MutexGuard<'_, Settings> {
        self.settings
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 拷一份设置（调用方不需要持有锁，避免长事务）。
    fn settings_snapshot(&self) -> Settings {
        self.lock_settings().clone()
    }

    fn lock_autostart(&self) -> MutexGuard<'_, AutostartCache> {
        self.autostart
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn lock_state(&self) -> MutexGuard<'_, RuntimeState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn lock_registry(&self) -> MutexGuard<'_, RegistryState> {
        self.registry
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn lock_webhooks(&self) -> MutexGuard<'_, WebhookState> {
        self.webhooks
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn lock_tray(&self) -> MutexGuard<'_, TraySupervisor> {
        self.tray
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn exit_code(&self) -> i32 {
        *self
            .exit_code
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn is_shutting_down(&self) -> bool {
        self.lock_state().shutting_down
    }

    fn snapshot(&self) -> StatusSnapshot {
        let (live, shutting_down) = {
            let state = self.lock_state();
            (
                state
                    .instances
                    .iter()
                    .map(|item| item.snapshot())
                    .collect::<Vec<_>>(),
                state.shutting_down,
            )
        };
        // 别名以登记表为真源：控制台改完别名要立刻反映到运行中的实例行上（不用重启）。
        // 用户没设过别名的实例用派生默认名（§3.20），所以这里的 alias 一定是"有效显示名"。
        let (specs, aliases) = {
            let registry = self.lock_registry();
            let aliases = registry
                .file
                .capsules
                .iter()
                .flat_map(|record| {
                    record.instances.iter().map(move |instance| {
                        (
                            format!("{}/{}", record.capsule_id, instance.profile),
                            effective_alias(record, instance),
                        )
                    })
                })
                .collect::<Vec<_>>();
            (registry.specs.clone(), aliases)
        };
        let tray_running = self.lock_tray().child.is_some();
        let mut instances = merge_views(&specs, &live);
        for view in &mut instances {
            if let Some((_, alias)) = aliases.iter().find(|(key, _)| *key == view.key) {
                view.alias = Some(alias.clone());
            }
        }
        StatusSnapshot {
            instances,
            shutting_down,
            tray_running,
        }
    }

    /// IPC 命令入口（在 IPC 服务线程上执行，必须尽快返回）。
    fn handle(&self, request: Request) -> Response {
        match request {
            Request::Status => Response::ok(&self.snapshot()),
            Request::Register { root } => self.register(root),
            Request::Start { key } => self.start_by_key(&key),
            Request::Stop { key } => self.stop(&key),
            Request::StopAll => self.stop_all(false),
            Request::Quit => self.stop_all(true),
            Request::Wake => {
                self.wake_tray();
                Response::ok(&self.snapshot())
            }
            Request::OpenBrowser { key } => self.open_browser(&key),
            Request::OpenConsole => self.open_console(),
            Request::ConsoleInfo => match self.console_url() {
                Some(url) => Response::console(url),
                None => Response::fail_with_code(
                    503,
                    "控制台不可用（端口绑定失败）；详情见 .dsh-housing\\logs\\housing-*.log",
                ),
            },
        }
    }

    /// 登记 / 刷新一个胶囊目录（§3.13）：只读 instance.json → 校验 → 合并 → 落盘。
    fn register(&self, root: PathBuf) -> Response {
        match self.register_root(&root) {
            Ok(notice) => Response::ok_notice(&self.snapshot(), notice),
            Err(err) => Response::fail_with_code(400, err),
        }
    }

    /// 登记 / 刷新的实体（IPC `register` 与控制台 `POST /api/capsules/register` 共用）。
    fn register_root(&self, root: &Path) -> Result<String, String> {
        let record = capsule::register(root, &self.data_dir).map_err(|err| {
            diag::warn(&format!("登记失败（{}）：{err}", root.display()));
            err.to_string()
        })?;
        let capsule_id = record.capsule_id.clone();
        let warnings = record.warnings.clone();

        let outcome = {
            let mut registry = self.lock_registry();
            // 坏文件先备份再写；备份失败就放弃这次登记（绝不覆盖用户手改坏的数据）。
            self.backup_broken_registry(&mut registry)?;

            // 合并到副本，落盘成功才提交（内存与磁盘不出现分叉）。
            let mut file = registry.file.clone();
            let outcome = capsule::merge_record(&mut file, record);
            store::save_capsules(&self.data_dir, &file).map_err(|err| err.to_string())?;
            registry.file = file;
            registry.specs = registry.file.specs();
            registry.load_error = None;
            outcome
        }; // 释放 registry 锁再取快照（锁顺序 state → registry）。

        diag::info(&format!(
            "登记：{capsule_id}；{}；告警={warnings:?}",
            outcome.notice
        ));
        let mut notice = outcome.notice;
        if !warnings.is_empty() {
            notice.push_str("；提示：");
            notice.push_str(&warnings.join("；"));
        }
        Ok(notice)
    }

    /// 坏掉的 `capsules.json` 先备份再写（§2.4 硬规则 2）；备份失败就放弃这次写盘。
    fn backup_broken_registry(&self, registry: &mut RegistryState) -> Result<(), String> {
        let Some(load_error) = registry.load_error.clone() else {
            return Ok(());
        };
        match store::backup_broken_capsules(&self.data_dir) {
            Ok(Some(path)) => diag::warn(&format!(
                "capsules.json 解析失败（{load_error}）；已备份到 {}",
                path.display()
            )),
            Ok(None) => {}
            Err(err) => {
                return Err(format!("{err}；为不覆盖损坏的登记表，本次操作已取消"));
            }
        }
        registry.load_error = None;
        Ok(())
    }

    /// 按登记项启动（§5.2 #11：`start` 不再接受整份 `InstanceSpec`）。
    fn start_by_key(&self, key: &str) -> Response {
        let spec = match self.lock_registry().file.find_spec(key) {
            Some(spec) => spec,
            None => {
                return Response::fail_with_code(
                    404,
                    format!("没有登记这个实例：{key}（先在托盘菜单里「登记胶囊…」或控制台登记）"),
                )
            }
        };
        self.start(spec)
    }

    /// 用默认浏览器打开该实例的 dsh Web UI（§3.16）。
    fn open_browser(&self, key: &str) -> Response {
        let token_url = {
            let state = self.lock_state();
            state
                .instances
                .iter()
                .find(|item| item.key() == key)
                .and_then(|instance| instance.token_url())
        };
        let Some(url) = token_url else {
            // 「没就绪」与「没登记」是两件事：前者是正常状态（回 notice），后者才是错误。
            if self.lock_registry().file.find_spec(key).is_some() {
                return Response::ok_notice(
                    &self.snapshot(),
                    format!("实例 {key} 还没就绪（没有抓到 token URL）"),
                );
            }
            return Response::fail_with_code(
                404,
                format!("没有登记这个实例：{key}（先在托盘菜单里「登记胶囊…」或控制台登记）"),
            );
        };
        match browser::open_url(&url) {
            Ok(()) => {
                diag::info(&format!("已打开浏览器：key={key}"));
                Response::ok(&self.snapshot())
            }
            Err(err) => Response::fail(format!("打开浏览器失败：{err}")),
        }
    }

    /// 用默认浏览器打开控制台（M3）；控制台没起来时回 503 语义的失败。
    fn open_console(&self) -> Response {
        let Some(url) = self.console_url() else {
            return Response::fail_with_code(
                503,
                "控制台不可用（端口绑定失败）；详情见 .dsh-housing\\logs\\housing-*.log",
            );
        };
        match browser::open_url(&url) {
            Ok(()) => {
                // 不记录 URL：它带 consoleToken。
                diag::info("已打开控制台");
                Response::ok(&self.snapshot())
            }
            Err(err) => Response::fail(format!("打开控制台失败：{err}")),
        }
    }

    /// v0.3 / #259：core 启动、控制台就绪后发一次 Housing Start。
    ///
    /// 控制台没起来（绑定失败）就跳过：没有可点的 URL，发一条空通知没有意义。
    /// 事件里的 `{local_url}` / `{lan_url}` 都指控制台，不是 dsh 实例（§3.22）。
    fn announce_housing_start(&self) {
        let Some(url) = self.console_url() else {
            diag::warn("webhook：控制台不可用，跳过 Housing Start");
            return;
        };
        let port = self.console_port();
        self.notifier.housing_start(Some(url), port);
        diag::info("webhook：Housing Start 已入队（URL 指控制台；token 不落日志）");
    }

    /// v0.3 / #251：按设置打开控制台页面（只在 core 启动、控制台就绪时调用一次）。
    fn open_console_if_configured(&self) {
        if !self.settings_snapshot().auto_open_console {
            diag::info("按设置不自动打开控制台（auto_open_console=false）");
            return;
        }
        if browser::auto_open_suppressed() {
            diag::info("DSH_HOUSING_NO_BROWSER=1：跳过自动打开控制台（手动入口仍可用）");
            return;
        }
        let Some(url) = self.console_url() else {
            return;
        };
        match browser::open_url(&url) {
            Ok(()) => diag::info("已按设置打开控制台"),
            Err(err) => diag::warn(&format!("按设置打开控制台失败：{err}")),
        }
    }

    fn lock_console(&self) -> MutexGuard<'_, Option<ConsoleService>> {
        self.console
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// 控制台起来之后由 `run()` 调用（唯一写者）。
    fn set_console(&self, service: ConsoleService) {
        *self.lock_console() = Some(service);
    }

    /// 控制台 URL（含 token）：只给 IPC `console_info` / `open_console`，不落盘、不进日志。
    fn console_url(&self) -> Option<String> {
        self.lock_console().as_ref().map(ConsoleService::url)
    }

    fn console_port(&self) -> u16 {
        self.lock_console()
            .as_ref()
            .map(ConsoleService::port)
            .unwrap_or(0)
    }

    /// 控制台的一份完整状态（快照 + 登记表视图 + 控制台信息 + 壳设置 + 自启动现状 + webhook）。
    fn console_state(&self) -> gui::ConsoleState {
        let status = serde_json::to_value(self.snapshot()).unwrap_or(serde_json::Value::Null);
        let registry = self.registry_view();
        let settings = self.settings_view();
        let autostart = self.autostart_view();
        let webhooks = self.webhook_section();
        gui::ConsoleState {
            status,
            registry,
            console: gui::ConsoleInfo {
                port: self.console_port(),
            },
            settings,
            autostart,
            webhooks,
            builder: self.builder.view(),
        }
    }

    /// 构建产物登记（v0.5 / #249）：supervisor 每轮看一眼有没有待登记的产物。
    ///
    /// 构建线程不直接写 `capsules.json`——登记的唯一入口仍在这里（§2.2.1 所有权表）。
    fn supervise_build(&self) {
        if let Some(report) = self.builder.take_pending_register() {
            let outcome = self.register_root(&report.capsule_root);
            match outcome {
                Ok(notice) => {
                    diag::info(&format!(
                        "构建产物已登记：{}（{notice}）",
                        report.capsule_id
                    ));
                    self.builder.finish_registration(Ok(report.capsule_id));
                }
                Err(err) => self.builder.finish_registration(Err(err)),
            }
        }
    }

    /// 已登记实例占用的端口（构建器挑端口时不许撞）。
    fn registry_ports(&self) -> Vec<u16> {
        let registry = self.lock_registry();
        registry
            .file
            .capsules
            .iter()
            .flat_map(|capsule| capsule.instances.iter().map(|instance| instance.port))
            .filter(|port| *port != 0)
            .collect()
    }

    /// 构建产物的默认父目录 = **本 exe 所在目录**（用户 2026-10-24 拍板，§5.14 D46）。
    ///
    /// 产物落在启动器自己旁边（与 `.dsh-housing\` 同级）；
    /// 面板里仍可改成任意绝对路径（构建器只用面板提交的值）。
    fn default_build_root(&self) -> PathBuf {
        self.self_exe
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_else(|| PathBuf::from("."))
    }

    /// `POST /api/builder/start` 的实体（§3.24）：补默认值 + 挑端口 + 交给构建宿主。
    fn start_build(&self, request: gui::BuilderStartRequest) -> Result<(), (u16, String)> {
        if request.name.trim().is_empty()
            || request.dsh.trim().is_empty()
            || request.node.trim().is_empty()
        {
            return Err((400, "name / dsh / node 都是必填".to_string()));
        }
        let taken = self.registry_ports();
        let port = match request.port {
            Some(port) => {
                if port == 0 {
                    return Err((400, "端口必须在 1–65535 之间".to_string()));
                }
                if taken.contains(&port) {
                    return Err((400, format!("端口 {port} 已被登记实例占用")));
                }
                port
            }
            None => build::pick_port(&taken).ok_or_else(|| {
                (
                    409,
                    "39410–39450 里没有既能绑、又没被登记的端口".to_string(),
                )
            })?,
        };
        let root = match request.root.as_deref().map(str::trim) {
            Some(root) if !root.is_empty() => PathBuf::from(root),
            _ => self.default_build_root(),
        };
        let build_request = dsh_housing_builder::BuildRequest {
            name: request.name.trim().to_string(),
            dsh_version: request.dsh.trim().to_string(),
            node_version: request.node.trim().to_string(),
            profile: request
                .profile
                .as_deref()
                .map(str::trim)
                .filter(|profile| !profile.is_empty())
                .unwrap_or("web")
                .to_string(),
            port,
            root,
            registry: request.registry.clone(),
            overwrite: request.overwrite,
            force_app: request.force_app,
            dry_run: request.dry_run,
            built_at: diag::timestamp_text(),
        };
        // 参数与「目标目录非空」都必须**同步**拒掉（HTTP 层要回 400），不能等到构建线程里才失败
        // ——那时接口早就回了 200（§3.24 第 1 步）。
        build_request
            .validate()
            .map_err(|err| (400, err.to_string()))?;
        if dsh_housing_builder::dir_is_non_empty(&build_request.capsule_root())
            && !build_request.overwrite
        {
            return Err((
                400,
                format!(
                    "目标目录已存在且非空（{}）。继续会覆盖模板与清单、保留 home\\；确认要继续请勾选「覆盖已有目录」",
                    build_request.capsule_root().display()
                ),
            ));
        }
        self.builder.start(build_request)
    }

    /// 版本目录视图（v0.5）：缓存优先，`refresh` 只触发后台抓取。
    fn builder_versions(&self, refresh: bool) -> gui::BuilderVersionsView {
        let registry = dsh_housing_builder::BuildRequest::DEFAULT_REGISTRY.to_string();
        if refresh {
            self.builder.refresh_versions(registry.clone());
        }
        self.builder
            .versions_view(self.default_build_root().display().to_string(), registry)
    }

    /// 壳设置视图（M4，§2.4）：持久化字段 + 只读信息。
    fn settings_view(&self) -> gui::SettingsView {
        let settings = self.settings_snapshot();
        gui::SettingsView {
            version: settings.version,
            gui_port: settings.gui_port,
            console_host: settings.console_host.clone(),
            sample_interval_ms: settings.sample_interval_ms,
            auto_open_browser: settings.auto_open_browser,
            auto_open_console: settings.auto_open_console,
            tray_restart_max: settings.tray_restart_max,
            tray_restart_backoff_ms: settings.tray_restart_backoff_ms,
            log_retention_days: settings.log_retention_days,
            lan_ip: net::preferred_lan_ipv4().map(|ip| ip.to_string()),
            settings_path: self
                .data_dir
                .join(store::SETTINGS_FILE_NAME)
                .display()
                .to_string(),
            exe_path: self.self_exe.display().to_string(),
            exe_integrity: self.integrity.label().to_string(),
            console_port: self.console_port(),
            core_pid: std::process::id(),
        }
    }

    /// 自启动现状（M4，§3.12）：5 s 缓存，避免每次 `/api/state` 都打 COM。
    fn autostart_status(&self) -> autostart::TaskStatus {
        let mut cache = self.lock_autostart();
        let stale = cache
            .refreshed
            .map(|at| at.elapsed() >= AUTOSTART_CACHE_TTL)
            .unwrap_or(true);
        if stale {
            cache.status = autostart::status(&self.self_exe);
            cache.refreshed = Some(Instant::now());
        }
        cache.status.clone()
    }

    fn autostart_view(&self) -> gui::AutostartView {
        let status = self.autostart_status();
        gui::AutostartView {
            task_name: status.task_name,
            state: status.state.label().to_string(),
            detail: status.detail,
            legacy: status.legacy,
            can_enable: self.integrity.is_viable(),
        }
    }

    /// 强制刷新一次自启动缓存（启用 / 停用之后立刻反映到 UI）。
    fn refresh_autostart(&self) {
        let status = autostart::status(&self.self_exe);
        let mut cache = self.lock_autostart();
        cache.status = status;
        cache.refreshed = Some(Instant::now());
    }

    /// 登记表视图：只投影控制台要展示的字段（不暴露文件内部结构）。
    fn registry_view(&self) -> gui::RegistryView {
        let registry = self.lock_registry();
        let capsules = registry
            .file
            .capsules
            .iter()
            .map(|record| gui::CapsuleView {
                capsule_id: record.capsule_id.clone(),
                root: record.root.display().to_string(),
                alias: record.alias.clone(),
                schema: record.schema,
                registered_at: record.registered_at,
                checked_at: record.checked_at,
                runtime: record.runtime.as_ref().map(|info| gui::RuntimeView {
                    kind: info.kind.clone(),
                    version: info.version.clone(),
                }),
                app: record.app.as_ref().map(|info| gui::AppView {
                    package: info.package.clone(),
                    version: info.version.clone(),
                    bin: info.bin.clone(),
                }),
                warnings: record.warnings.clone(),
                instances: record
                    .instances
                    .iter()
                    .map(|instance| gui::InstanceConfigView {
                        profile: instance.profile.clone(),
                        port: instance.port,
                        host: instance.host.clone(),
                        workspace: instance.workspace.display().to_string(),
                        alias: Some(effective_alias(record, instance)),
                        autostart: instance.autostart,
                        env: instance.env.clone(),
                    })
                    .collect(),
            })
            .collect();
        gui::RegistryView {
            data_dir: self.data_dir.display().to_string(),
            capsules,
        }
    }

    /// 取一个实例的事件事实（别名 / 版本来自登记表，URL 来自实例本身）。
    ///
    /// 锁顺序：registry → instance（§2.2.1 的纪律）；本方法在持锁路径之外调用。
    fn instance_facts(&self, instance: &Instance) -> notify::Facts {
        let spec = instance.spec();
        let (alias, dsh_version) = {
            let registry = self.lock_registry();
            registry.facts_for(&spec.capsule_id, &spec.profile)
        };
        notify::Facts {
            alias,
            dsh_version,
            profile: spec.profile.clone(),
            port: spec.port,
            local_url: instance.token_url(),
        }
    }

    /// webhook 视图（控制台）：已配置的条目 + 新建用的默认模板 + 可用占位符。
    ///
    /// 默认模板与占位符表只在 `crates/webhook` 里有一份，前端不复制（改一处就够了）。
    fn webhook_section(&self) -> gui::WebhookSection {
        let state = self.lock_webhooks();
        let items = state.file.webhooks.iter().map(webhook_view).collect();
        gui::WebhookSection {
            file_path: self
                .data_dir
                .join(store::WEBHOOKS_FILE_NAME)
                .display()
                .to_string(),
            items,
            defaults: gui::TriggerTemplates {
                start: dsh_housing_webhook::DEFAULT_START_TEMPLATE.to_string(),
                stop: dsh_housing_webhook::DEFAULT_STOP_TEMPLATE.to_string(),
                crashed: dsh_housing_webhook::DEFAULT_CRASHED_TEMPLATE.to_string(),
                housing_start: dsh_housing_webhook::DEFAULT_HOUSING_START_TEMPLATE.to_string(),
            },
            placeholders: dsh_housing_webhook::PLACEHOLDERS
                .iter()
                .map(|name| format!("{{{name}}}"))
                .collect(),
        }
    }

    /// 保存 webhook 配置（§3.21）：校验 → 坏文件先备份 → 原子写盘 → 换内存副本 + 热换投递配置。
    fn apply_webhooks(&self, views: Vec<gui::WebhookView>) -> gui::ActionResult {
        let file = WebhookFile {
            version: dsh_housing_webhook::SCHEMA_VERSION,
            webhooks: views.iter().map(webhook_from_view).collect(),
        };
        if let Err(err) = file.validate() {
            return gui::ActionResult::fail(400, err);
        }
        {
            let mut state = self.lock_webhooks();
            if let Some(load_error) = state.load_error.clone() {
                match store::backup_broken_webhooks(&self.data_dir) {
                    Ok(Some(path)) => diag::warn(&format!(
                        "webhooks.json 解析失败（{load_error}）；已备份到 {}",
                        path.display()
                    )),
                    Ok(None) => {}
                    Err(err) => {
                        return gui::ActionResult::fail(
                            400,
                            format!("{err}；为不覆盖损坏的 webhook 配置，本次保存已取消"),
                        )
                    }
                }
                state.load_error = None;
            }
            if let Err(err) = store::save_webhooks(&self.data_dir, &file) {
                return gui::ActionResult::fail(500, format!("保存 webhooks.json 失败：{err}"));
            }
            state.file = file.clone();
        }
        self.notifier.apply(file.webhooks.clone());
        let enabled = file.webhooks.iter().filter(|hook| hook.enabled).count();
        diag::info(&format!(
            "控制台：更新 webhook 配置：{} 条（{} 条启用，即时生效）",
            file.webhooks.len(),
            enabled
        ));
        gui::ActionResult::ok(self.console_state(), "已保存 webhook（即时生效）")
    }

    /// core 退出前等投递队列排空（有界）。
    fn drain_webhooks(&self, budget: Duration) -> bool {
        let drained = self.notifier.drain(budget);
        if !drained {
            return false;
        }
        if !self.notifier.is_running() {
            diag::warn("webhook：投递线程已不在运行（本次没有事件可发）");
        }
        true
    }

    /// 把 IPC 的 `Response` 翻译成控制台的 `ActionResult`（IPC 语义不变）。
    fn action_from(&self, response: Response) -> gui::ActionResult {
        if response.ok {
            let notice = response.notice.unwrap_or_else(|| "已完成".to_string());
            gui::ActionResult::ok(self.console_state(), notice)
        } else {
            let message = response
                .error
                .unwrap_or_else(|| "请求失败（没有给出原因）".to_string());
            gui::ActionResult::fail(response.code.unwrap_or(400), message)
        }
    }

    /// 实例日志尾部：运行中优先读内存环形缓冲，否则读最新一份日志文件（§5.8 D8）。
    ///
    /// 只服务**登记过的实例**：胶囊被移除后它的日志文件还在盘上（交给保留策略老化），
    /// 但不再通过接口暴露——否则「不存在的实例」还能读出内容，与 404 的语义打架。
    fn instance_logs(&self, key: &str, tail: usize) -> Result<gui::LogPayload, gui::BackendError> {
        let instance = {
            let state = self.lock_state();
            state
                .instances
                .iter()
                .find(|instance| instance.key() == key)
                .cloned()
        };
        if instance.is_none() && self.lock_registry().file.find_spec(key).is_none() {
            return Err(gui::BackendError::not_found(format!("没有这个实例：{key}")));
        }
        if let Some(instance) = instance {
            if let Some(bytes) = instance.ring_tail(tail) {
                if !bytes.is_empty() {
                    return Ok(gui::LogPayload {
                        text: String::from_utf8_lossy(&bytes).into_owned(),
                        source: "ring".to_string(),
                    });
                }
            }
        }

        let log_dir = self.data_dir.join(store::LOGS_DIR_NAME);
        let stem = logs::instance_log_stem(key);
        match logs::tail_latest(&log_dir, &stem, tail) {
            Ok(Some(text)) => Ok(gui::LogPayload {
                text,
                source: "file".to_string(),
            }),
            Ok(None) => Ok(gui::LogPayload {
                text: String::new(),
                source: "none".to_string(),
            }),
            Err(err) => Err(gui::BackendError::internal(format!("读日志失败：{err}"))),
        }
    }

    fn is_instance_active(&self, key: &str) -> bool {
        let state = self.lock_state();
        state
            .instances
            .iter()
            .find(|instance| instance.key() == key)
            .map(|instance| instance.has_process() || instance.is_active())
            .unwrap_or(false)
    }

    /// 改登记表里某个实例的配置（env / alias）：改副本 → 落盘 → 提交内存。
    fn update_instance_config<F>(
        &self,
        capsule_id: &str,
        profile: &str,
        update: F,
    ) -> Result<(), String>
    where
        F: FnOnce(&mut capsule::RegisteredInstance),
    {
        let mut registry = self.lock_registry();
        self.backup_broken_registry(&mut registry)?;
        let mut file = registry.file.clone();
        let record = file
            .capsules
            .iter_mut()
            .find(|record| record.capsule_id == capsule_id)
            .ok_or_else(|| format!("没有登记这个胶囊：{capsule_id}"))?;
        let instance = record
            .instances
            .iter_mut()
            .find(|instance| instance.profile == profile)
            .ok_or_else(|| format!("胶囊 {capsule_id} 没有 profile：{profile}"))?;
        update(instance);
        store::save_capsules(&self.data_dir, &file).map_err(|err| err.to_string())?;
        registry.file = file;
        registry.specs = registry.file.specs();
        registry.load_error = None;
        Ok(())
    }

    /// 移除一个胶囊：有实例在跑 → 409；没登记 → 404；成功 = 删记录 + 清掉已停止的实例行。
    fn remove_capsule_record(&self, capsule_id: &str, confirm: &str) -> Result<(), (u16, String)> {
        if confirm.trim() != capsule_id {
            return Err((400, "确认文本与胶囊标识不一致".to_string()));
        }

        let running = {
            let state = self.lock_state();
            state
                .instances
                .iter()
                .filter(|instance| {
                    instance.spec().capsule_id == capsule_id
                        && (instance.has_process() || instance.is_active())
                })
                .map(|instance| instance.spec().profile.clone())
                .collect::<Vec<_>>()
        };
        if !running.is_empty() {
            return Err((
                409,
                format!(
                    "胶囊 {capsule_id} 还有实例在运行（{}）；先停止再移除",
                    running.join("、")
                ),
            ));
        }

        {
            let mut registry = self.lock_registry();
            self.backup_broken_registry(&mut registry)
                .map_err(|err| (400, err))?;
            let mut file = registry.file.clone();
            let before = file.capsules.len();
            file.capsules
                .retain(|record| record.capsule_id != capsule_id);
            if file.capsules.len() == before {
                return Err((404, format!("没有登记这个胶囊：{capsule_id}")));
            }
            store::save_capsules(&self.data_dir, &file).map_err(|err| (400, err.to_string()))?;
            registry.file = file;
            registry.specs = registry.file.specs();
            registry.load_error = None;
        }

        // 容器里只留登记项的“活记录”；这个胶囊的实例都已停止，整行清掉（§3.18）。
        {
            let mut state = self.lock_state();
            state
                .instances
                .retain(|instance| instance.spec().capsule_id != capsule_id);
        }
        diag::info(&format!(
            "控制台：已移除胶囊 {capsule_id}（不碰胶囊目录与日志文件）"
        ));
        Ok(())
    }

    /// 改壳设置（M4，§2.4）：校验 → 原子写盘 → 换内存副本。
    fn apply_settings(&self, request: gui::SettingsRequest) -> gui::ActionResult {
        let current = self.settings_snapshot();
        let next = Settings {
            version: current.version,
            gui_port: request.gui_port,
            // 旧客户端不带这个字段 = 保留当前值（不静默把默认回环写回去）。
            console_host: request
                .console_host
                .unwrap_or_else(|| current.console_host.clone()),
            sample_interval_ms: request.sample_interval_ms,
            auto_open_browser: request.auto_open_browser,
            // 旧客户端不带这个字段 = 保留当前值（不静默把默认开改回关）。
            auto_open_console: request
                .auto_open_console
                .unwrap_or(current.auto_open_console),
            tray_restart_max: request.tray_restart_max,
            tray_restart_backoff_ms: request.tray_restart_backoff_ms,
            log_retention_days: request.log_retention_days,
        };
        if let Err(err) = next.validate() {
            return gui::ActionResult::fail(400, err);
        }
        // v0.4 / #286：启动位置必须「现在绑得上」——写错 = 下次 core 起来后控制台直接消失，
        // 而控制台是唯一的设置入口（§3.23 / D33）。
        if let Ok(ip) = net::normalize_console_host(&next.console_host) {
            if let Err(err) = net::probe_console_bind(ip) {
                return gui::ActionResult::fail(400, err);
            }
        }
        if let Err(err) = store::save_settings(&self.data_dir, &next) {
            return gui::ActionResult::fail(500, format!("保存 settings.json 失败：{err}"));
        }
        {
            let mut settings = self.lock_settings();
            *settings = next.clone();
        }
        diag::info(&format!(
            "控制台：更新壳设置 gui_port={} 控制台位置={} 采样={}ms 自动开浏览器={} 自动开控制台={} 托盘重启={}次 日志保留={}天",
            next.gui_port,
            next.console_host,
            next.sample_interval_ms,
            next.auto_open_browser,
            next.auto_open_console,
            next.tray_restart_max,
            next.log_retention_days
        ));

        let mut parts: Vec<&str> = vec!["已保存设置"];
        if next.gui_port != current.gui_port {
            parts.push("控制台端口要重启 core 后生效");
        }
        if next.console_host != current.console_host {
            parts.push("控制台启动位置要重启 core 后生效");
        }
        if next.auto_open_console != current.auto_open_console {
            parts.push("启动后自动打开控制台要重启 core 后生效（只在启动时读一次）");
        }
        if next.sample_interval_ms != current.sample_interval_ms
            || next.tray_restart_max != current.tray_restart_max
            || next.tray_restart_backoff_ms != current.tray_restart_backoff_ms
            || next.log_retention_days != current.log_retention_days
            || next.auto_open_browser != current.auto_open_browser
        {
            parts.push("其余设置即时生效");
        }
        gui::ActionResult::ok(self.console_state(), parts.join("；"))
    }

    /// 改实例端口（M4，§3.19）：真源是登记表 `port`；运行中拒绝；与其它登记项撞端口拒绝。
    fn change_instance_port(&self, key: &str, port: u16) -> gui::ActionResult {
        if port == 0 {
            return gui::ActionResult::fail(400, "端口必须在 1..=65535（0 只对控制台端口有意义）");
        }
        let Some((capsule_id, profile)) = split_key(key) else {
            return gui::ActionResult::fail(
                400,
                format!("实例键格式应是 capsuleId/profile：{key}"),
            );
        };
        if self.is_instance_active(key) {
            return gui::ActionResult::fail(
                409,
                format!("实例 {key} 正在运行（或正在停止）；先停止实例再改端口"),
            );
        }
        let (old_port, conflict) = {
            let registry = self.lock_registry();
            let old_port = registry
                .specs
                .iter()
                .find(|spec| spec.key() == key)
                .map(|spec| spec.port);
            let conflict = registry
                .specs
                .iter()
                .find(|spec| spec.port == port && spec.key() != key)
                .map(|spec| spec.key());
            (old_port, conflict)
        };
        let Some(old_port) = old_port else {
            return gui::ActionResult::fail(404, format!("没有登记这个实例：{key}"));
        };
        if let Some(other) = conflict {
            return gui::ActionResult::fail(
                400,
                format!("端口 {port} 已经被登记的实例 {other} 占用（同一端口只能有一个实例）"),
            );
        }
        if old_port == port {
            return gui::ActionResult::ok(self.console_state(), format!("端口本来就是 {port}"));
        }
        match self.update_instance_config(capsule_id, profile, |instance| instance.port = port) {
            Ok(()) => {
                diag::info(&format!(
                    "控制台：改实例端口 key={key} {old_port} → {port}（只写登记表，不碰胶囊）"
                ));
                gui::ActionResult::ok(
                    self.console_state(),
                    format!("端口 {old_port} → {port} 已保存；下次启动实例时生效"),
                )
            }
            Err(err) => gui::ActionResult::fail(400, err),
        }
    }

    /// 改实例 host（v0.3 / §3.22）：空串 = 清除（回到跟随胶囊）；运行中 409。
    ///
    /// 校验复用 `net::normalize_instance_host`（与 `env.rs` 同一条规则）：只收具体 IP，
    /// wildcard `0.0.0.0` / `::` 明确拒绝并解释「局域网请填本机具体 IPv4」。
    fn change_instance_host(&self, key: &str, host: &str) -> gui::ActionResult {
        let normalized = match net::normalize_instance_host(host) {
            Ok(value) => value,
            Err(err) => return gui::ActionResult::fail(400, err),
        };
        let Some((capsule_id, profile)) = split_key(key) else {
            return gui::ActionResult::fail(
                400,
                format!("实例键格式应是 capsuleId/profile：{key}"),
            );
        };
        if self.is_instance_active(key) {
            return gui::ActionResult::fail(
                409,
                format!("实例 {key} 正在运行（或正在停止）；先停止实例再改 host"),
            );
        }
        let (old, app_version) = {
            let registry = self.lock_registry();
            let old = registry
                .specs
                .iter()
                .find(|spec| spec.key() == key)
                .map(|spec| spec.host.clone());
            let app_version = registry
                .file
                .capsules
                .iter()
                .find(|record| record.capsule_id == capsule_id)
                .and_then(|record| record.app.as_ref())
                .map(|app| app.version.clone());
            (old, app_version)
        };
        let Some(old) = old else {
            return gui::ActionResult::fail(404, format!("没有登记这个实例：{key}"));
        };
        // dsh 0.2.0 的 webserver 配置 schema 只接受 `127.0.0.1` / `0.0.0.0` 两个字面量；
        // 具体 IP 会直接 `ValidationError`（实测 0.2.0 web 用 192.168.x 启动失败）。
        // 0.2.0 的局域网档只能由胶囊 profile patch 的 `0.0.0.0` 决定（chat 现状），
        // 控制台不提供「具体 IP」这个走不通的选项。0.2.1+ 才支持具体 IP。
        if let (Some(version), Some(host)) = (app_version.as_deref(), normalized.as_deref()) {
            if version.starts_with("0.2.0") && host != "127.0.0.1" {
                return gui::ActionResult::fail(
                    400,
                    format!(
                        "dsh {version} 的 webserver 只接受 127.0.0.1 / 0.0.0.0，\
                         具体 IP（{host}）会被 dsh 拒绝（实测 ValidationError）；\
                         局域网档只能由胶囊 profile patch 的 0.0.0.0 决定。\
                         需要按实例绑具体 IP 请用 dsh 0.2.1+ 的胶囊。"
                    ),
                );
            }
        }
        if old == normalized {
            return gui::ActionResult::ok(
                self.console_state(),
                match &normalized {
                    Some(value) => format!("host 本来就是 {value}"),
                    None => "host 本来就是「跟随胶囊配置」".to_string(),
                },
            );
        }
        let next = normalized.clone();
        match self.update_instance_config(capsule_id, profile, |instance| {
            instance.host = next.clone();
        }) {
            Ok(()) => {
                diag::info(&format!(
                    "控制台：改实例 host key={key} {} → {}（只写登记表，不碰胶囊）",
                    old.as_deref().unwrap_or("(跟随胶囊)"),
                    normalized.as_deref().unwrap_or("(跟随胶囊)")
                ));
                let notice = match &normalized {
                    Some(value) => format!("host 已设为 {value}；下次启动实例时生效"),
                    None => "已清除 host：下次启动回到跟随胶囊配置".to_string(),
                };
                gui::ActionResult::ok(self.console_state(), notice)
            }
            Err(err) => gui::ActionResult::fail(400, err),
        }
    }

    /// 实例级自启开关（M4，§3.12）：只写登记表，下次 core 启动生效。
    fn set_autostart_flag(&self, key: &str, autostart: bool) -> gui::ActionResult {
        let Some((capsule_id, profile)) = split_key(key) else {
            return gui::ActionResult::fail(
                400,
                format!("实例键格式应是 capsuleId/profile：{key}"),
            );
        };
        match self.update_instance_config(capsule_id, profile, |instance| {
            instance.autostart = autostart;
        }) {
            Ok(()) => {
                diag::info(&format!("控制台：实例登录自启 {key} = {autostart}"));
                gui::ActionResult::ok(
                    self.console_state(),
                    if autostart {
                        "已标记为登录自启：下次 core 启动（含登录）时自动拉起"
                    } else {
                        "已取消登录自启"
                    },
                )
            }
            Err(err) => gui::ActionResult::fail(400, err),
        }
    }

    /// 启用登录自启动（M4，§3.12）：建 / 改任务计划。
    fn enable_login_autostart(&self) -> gui::ActionResult {
        match autostart::enable(&self.self_exe, &self.data_dir) {
            Ok(notice) => {
                diag::info(&format!("控制台：启用登录自启动：{notice}"));
                self.refresh_autostart();
                gui::ActionResult::ok(self.console_state(), notice)
            }
            Err(err) => {
                diag::warn(&format!("控制台：启用登录自启动失败：{err}"));
                gui::ActionResult::fail(400, err)
            }
        }
    }

    /// 停用登录自启动并回滚（M4，§3.12）。
    fn disable_login_autostart(&self) -> gui::ActionResult {
        match autostart::disable(&self.data_dir) {
            Ok(notice) => {
                diag::info(&format!("控制台：停用登录自启动：{notice}"));
                self.refresh_autostart();
                gui::ActionResult::ok(self.console_state(), notice)
            }
            Err(err) => {
                diag::warn(&format!("控制台：停用登录自启动失败：{err}"));
                gui::ActionResult::fail(400, err)
            }
        }
    }

    /// core 启动后按登记项拉起「登录自启」的实例（§3.12 / D16）。
    ///
    /// 一个实例失败只记日志并让它自己进 `Failed`，不影响其它实例（core 启动路径不能被拖死）。
    fn start_autostart_instances(&self) {
        let specs = {
            let registry = self.lock_registry();
            registry.file.autostart_specs()
        };
        if specs.is_empty() {
            diag::info("登录自启：没有标记自启的实例");
            return;
        }
        for spec in specs {
            let key = spec.key();
            diag::info(&format!("登录自启：拉起实例 {key}（端口 {}）", spec.port));
            let response = self.start(spec);
            if !response.ok {
                diag::warn(&format!(
                    "登录自启：实例 {key} 启动失败：{}",
                    response.error.unwrap_or_default()
                ));
            }
        }
    }

    fn start(&self, spec: InstanceSpec) -> Response {
        let key = spec.key();
        let port = spec.port;
        let instance = Instance::new(spec);
        {
            let mut state = self.lock_state();
            if state.shutting_down {
                return Response::fail_with_code(409, "core 正在退出，拒绝启动新实例");
            }
            if let Some(existing) = state.instances.iter().find(|item| item.key() == key) {
                if existing.has_process() || existing.is_active() {
                    return Response::fail_with_code(
                        409,
                        format!("实例 {key} 已在运行或正在停止，不能重复启动"),
                    );
                }
                // 上一次已经停稳（Stopped / 无进程的 Failed）：允许重开，清掉旧记录。
                state.instances.retain(|item| item.key() != key);
            }
            state.instances.push(instance.clone());
        }

        // 端口冲突：拒绝启动并把原因落到实例行上（§5.7 D3）。Unknown 只告警、照常启动。
        match port::probe(port) {
            PortStatus::InUse => {
                let reason = port::conflict_message(port);
                diag::warn(&format!("拒绝启动：key={key}；{reason}"));
                instance.mark_spawn_failed(reason.clone());
                return Response::fail_with_status_code(&self.snapshot(), 409, reason);
            }
            PortStatus::Unknown(err) => {
                diag::warn(&format!(
                    "端口探测失败（key={key} port={port}）：{err}；照常启动"
                ));
            }
            PortStatus::Free => {}
        }

        match self.spawn_instance(&instance) {
            Ok(()) => Response::ok(&self.snapshot()),
            Err(reason) => {
                diag::error(&format!("实例启动失败：key={key}；{reason}"));
                instance.mark_spawn_failed(reason.clone());
                Response::fail_with_status(&self.snapshot(), reason)
            }
        }
    }

    fn spawn_instance(&self, instance: &Arc<Instance>) -> Result<(), String> {
        let spec = instance.spec();
        let plan = env::build(spec).map_err(|err| format!("组装环境失败：{err}"))?;
        let command_line = plan.command_line.clone();

        let spawned = job::spawn(&SpawnRequest {
            program: &plan.program,
            command_line: &command_line[..],
            cwd: &plan.cwd,
            env_block: &plan.env_block,
        })
        .map_err(|err| format!("{err}"))?;

        let SpawnedInstance {
            pid,
            process,
            job: job_handle,
            stdin,
            stdout,
            stderr,
        } = spawned;

        let log_dir = self.data_dir.join(store::LOGS_DIR_NAME);
        let context = format!(
            "pid={pid} port={} cwd={} node={}",
            spec.port,
            plan.cwd.display(),
            plan.program.display()
        );
        let pump = match logs::start(&log_dir, instance.key(), &context, stdout, stderr) {
            Ok(pump) => pump,
            Err(err) => {
                // 日志线程起不来，读端没人消费会让子进程卡死；直接收掉。
                let _ = job::terminate_job(&job_handle, FORCE_KILL_EXIT_CODE);
                let _ = job::wait_process(&process, 5_000);
                return Err(format!("启动日志管线失败：{err}"));
            }
        };

        let log_path = pump.log_path();
        instance.attach(pid, process, job_handle, stdin, pump);
        diag::info(&format!(
            "实例启动：key={} pid={pid} port={} cwd={} 日志={}",
            instance.key(),
            spec.port,
            plan.cwd.display(),
            log_path.display()
        ));
        Ok(())
    }

    fn stop(&self, key: &str) -> Response {
        let instance = {
            let state = self.lock_state();
            state
                .instances
                .iter()
                .find(|item| item.key() == key)
                .cloned()
        };
        let Some(instance) = instance else {
            return Response::fail_with_code(404, format!("没有这个实例：{key}"));
        };
        self.initiate_stop(instance);
        Response::ok(&self.snapshot())
    }

    fn stop_all(&self, quitting: bool) -> Response {
        let instances = {
            let mut state = self.lock_state();
            if quitting {
                state.shutting_down = true;
            }
            state.instances.clone()
        };
        if quitting {
            diag::info("收到退出请求：停全部实例");
        }
        for instance in instances {
            self.initiate_stop(instance);
        }
        Response::ok(&self.snapshot())
    }

    /// 状态转 Stopping，把进程 / job / 日志交给一个专用线程去等它退出。
    ///
    /// 这里绝不等待进程（IPC 线程不能被一个 5 s 的等待卡住）。
    fn initiate_stop(&self, instance: Arc<Instance>) {
        let Some(resources) = instance.take_for_stop() else {
            return;
        };
        let key = instance.key().to_string();
        // 事件事实在**停止开始前**取好：token URL 会在 `finish_stop` 里被清掉（§3.21）。
        let facts = self.instance_facts(&instance);
        let notifier = self.notifier.clone();
        let stop_instance = instance.clone();
        let spawn = thread::Builder::new()
            .name(format!("stop-{}", key.replace('/', "-")))
            .spawn(move || run_stop(stop_instance, key, resources, notifier, facts));
        if let Err(err) = spawn {
            diag::error(&format!("停止线程起不来（{err}）；关 job 句柄兜底收进程"));
            // resources 在这里 drop：job 句柄关闭 → KILL_ON_JOB_CLOSE 收掉整棵树。
            instance.finish_stop(None, Some(format!("停止线程起不来：{err}")));
        }
    }

    fn wake_tray(&self) {
        let exhausted = {
            let mut tray = self.lock_tray();
            if tray.child.is_none() {
                diag::info("收到唤醒请求：立即拉托盘");
                tray.next_attempt = None;
                self.spawn_tray_locked(&mut tray)
            } else {
                None
            }
        };
        if let Some(reason) = exhausted {
            self.emergency_shutdown(&reason);
        }
    }

    /// 托盘监督：发现退出 → 退避重启；尝试次数超限 → 停全部实例并退出。
    fn supervise_tray(&self) {
        if self.is_shutting_down() {
            return;
        }
        let max_attempts = self.settings_snapshot().tray_restart_max.max(1) as u32;
        let mut exhausted: Option<String> = None;
        {
            let mut tray = self.lock_tray();

            // 1. 先看子进程是否已经退出。先把结果取出来再动其它字段，
            //    避免同时借 `child` 与 `started_at` / `child = None`。
            let mut exit: Option<(u32, Option<i32>)> = None;
            let mut wait_error: Option<String> = None;
            if let Some(child) = tray.child.as_mut() {
                match child.try_wait() {
                    Ok(None) => {}
                    Ok(Some(status)) => exit = Some((child.id(), status.code())),
                    Err(err) => wait_error = Some(err.to_string()),
                }
            }

            if let Some((pid, code)) = exit {
                let uptime = tray.started_at.map(|at| at.elapsed()).unwrap_or_default();
                tray.child = None;
                tray.started_at = None;
                let code_text = code
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "未知".to_string());
                diag::warn(&format!(
                    "托盘子进程退出：pid={pid} 退出码={code_text} 存活={:.1}s",
                    uptime.as_secs_f64()
                ));
                if uptime >= TRAY_STABLE_UPTIME {
                    tray.attempts = 0;
                }
                tray.attempts = tray.attempts.saturating_add(1);
                if tray.attempts > max_attempts {
                    exhausted = Some(format!("托盘连续失败 {max_attempts} 次（崩溃或起不来）"));
                } else {
                    let backoff = self.tray_backoff(tray.attempts);
                    diag::info(&format!(
                        "{} ms 后重启托盘（第 {}/{} 次）",
                        backoff.as_millis(),
                        tray.attempts,
                        max_attempts
                    ));
                    tray.next_attempt = Some(Instant::now() + backoff);
                }
            } else if let Some(err) = wait_error {
                tray.child = None;
                tray.started_at = None;
                diag::warn(&format!("查询托盘状态失败：{err}"));
            }

            // 2. 没有子进程且退避到期 → 拉起来。
            if exhausted.is_none() && tray.child.is_none() && self.tray_due(&tray) {
                tray.next_attempt = None;
                if let Some(reason) = self.spawn_tray_locked(&mut tray) {
                    exhausted = Some(reason);
                }
            }
        } // 锁在这里释放：emergency_shutdown 会再拿 tray 锁。

        if let Some(reason) = exhausted {
            self.emergency_shutdown(&reason);
        }
    }

    fn tray_due(&self, tray: &TraySupervisor) -> bool {
        match tray.next_attempt {
            Some(at) => Instant::now() >= at,
            None => true,
        }
    }

    fn tray_backoff(&self, attempt: u32) -> Duration {
        let list = self.settings_snapshot().tray_restart_backoff_ms;
        if list.is_empty() {
            return Duration::from_millis(1_000);
        }
        let index = (attempt.saturating_sub(1) as usize).min(list.len() - 1);
        Duration::from_millis(list[index])
    }

    /// 拉起托盘子进程。返回 `Some(原因)` 表示重试次数已耗尽。
    fn spawn_tray_locked(&self, tray: &mut TraySupervisor) -> Option<String> {
        match self.spawn_tray() {
            Ok(child) => {
                diag::info(&format!("托盘子进程已启动：pid={}", child.id()));
                tray.child = Some(child);
                tray.started_at = Some(Instant::now());
                None
            }
            Err(err) => {
                tray.attempts = tray.attempts.saturating_add(1);
                let max_attempts = self.settings_snapshot().tray_restart_max.max(1) as u32;
                diag::error(&format!("托盘子进程启动失败：{err}"));
                if tray.attempts > max_attempts {
                    Some(format!(
                        "托盘连续启动失败（{max_attempts} 次重试后仍起不来）：{err}"
                    ))
                } else {
                    let backoff = self.tray_backoff(tray.attempts);
                    tray.next_attempt = Some(Instant::now() + backoff);
                    diag::info(&format!(
                        "{} ms 后重试启动托盘（第 {}/{} 次）",
                        backoff.as_millis(),
                        tray.attempts,
                        max_attempts
                    ));
                    None
                }
            }
        }
    }

    fn spawn_tray(&self) -> io::Result<Child> {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW：托盘是 GUI 子系统，本来就没有控制台；这里只是兜底。
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let mut command = Command::new(&self.self_exe);
        command
            .arg(format!("--role={}", crate::ROLE_TRAY))
            .env(ipc::PIPE_ENV_VAR, &self.pipe_name)
            // 托盘靠这个 PID 开句柄等 core 退出（§3.15）：core 被硬杀时图标立刻消失。
            .env(ipc::CORE_PID_ENV_VAR, std::process::id().to_string())
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(CREATE_NO_WINDOW);
        command.spawn()
    }

    /// 托盘反复起不来 / 反复崩：停全部实例并退出（不留后台僵尸）。
    fn emergency_shutdown(&self, reason: &str) {
        diag::error(&format!("托盘监督放弃：{reason}；停全部实例并退出"));
        {
            let mut code = self
                .exit_code
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            *code = 1;
        }
        let instances = {
            let mut state = self.lock_state();
            state.shutting_down = true;
            state.instances.clone()
        };
        for instance in instances {
            self.initiate_stop(instance);
        }
        // 托盘已经不可信，直接收掉进程（图标由 shell 在进程消失后清理）。
        if let Some(mut child) = self.lock_tray().child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        diag::show_error(&format!(
            "dsh-housing：托盘反复启动失败，已停止全部实例并退出。\n\n{reason}\n\n\
             详情见 .dsh-housing\\logs\\housing-*.log。"
        ));
    }

    fn supervise_loop(&self) {
        loop {
            let iteration =
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.supervise_once()));
            if iteration.is_err() {
                diag::error("supervisor 单轮发生 panic（已捕获，继续运行）");
            }
            if self.is_shutting_down() && self.all_settled() {
                break;
            }
            thread::sleep(SUPERVISE_INTERVAL);
        }
    }

    fn supervise_once(&self) {
        self.supervise_tray();

        let instances = {
            let state = self.lock_state();
            state.instances.clone()
        };
        for instance in instances {
            let observation = instance.observe();
            if let Some(ready) = observation.ready {
                let pids = instance.job_pids();
                diag::info(&format!(
                    "实例就绪：key={} port={}；job 内进程 {} 个：{pids:?}",
                    instance.key(),
                    ready.port,
                    pids.len()
                ));
                if self.settings_snapshot().auto_open_browser {
                    if browser::auto_open_suppressed() {
                        diag::info(&format!(
                            "DSH_HOUSING_NO_BROWSER=1：跳过按设置打开实例页：key={}",
                            instance.key()
                        ));
                    } else if let Some(url) = instance.token_url() {
                        match browser::open_url(&url) {
                            Ok(()) => {
                                diag::info(&format!("已按设置打开浏览器：key={}", instance.key()))
                            }
                            Err(err) => diag::warn(&format!(
                                "按设置打开浏览器失败（key={}）：{err}",
                                instance.key()
                            )),
                        }
                    }
                }
                // Service Start：readiness 成功那一刻（这时才有 token URL，§3.21）。
                let facts = self.instance_facts(&instance);
                self.notifier.event(HookTrigger::Start, &facts);
            }
            if observation.readiness_timeout {
                diag::warn(&format!(
                    "实例 readiness 超时：key={}（进程保留，可手动停止）",
                    instance.key()
                ));
            }
            if let Some(exit) = observation.exited {
                diag::warn(&format!(
                    "实例自行退出：key={} 退出码={:?}",
                    instance.key(),
                    exit.exit_code
                ));
                // Service Crashed：进程**自行**退出（主动停止的退出由 `run_stop` 报 Stop）。
                let facts = self.instance_facts(&instance);
                self.notifier.event(HookTrigger::Crashed, &facts);
            }
        }

        // 资源采样（§3.9）与日志保留清理（§3.10）都按各自的节拍走，不跟 250 ms 轮询绑死。
        self.sample_if_due();
        self.purge_logs_if_due();
        // 构建产物登记（v0.5 / #249）：构建线程只留 pending，写登记表仍由这里做。
        self.supervise_build();
    }

    /// 按 `settings.sample_interval_ms` 采样（只在有进程的实例上做）。
    fn sample_if_due(&self) {
        let interval = Duration::from_millis(
            self.settings_snapshot()
                .sample_interval_ms
                .max(MIN_SAMPLE_INTERVAL_MS),
        );
        let due = {
            let mut state = self.lock_state();
            let now = Instant::now();
            if now >= state.next_sample {
                state.next_sample = now + interval;
                true
            } else {
                false
            }
        };
        if !due {
            return;
        }
        let instances = {
            let state = self.lock_state();
            state.instances.clone()
        };
        for instance in instances {
            if instance.has_process() {
                instance.sample_metrics();
            }
        }
    }

    /// 每个本地日期清一次旧日志（启动时 + 跨零点）。
    fn purge_logs_if_due(&self) {
        let today = diag::local_date();
        let due = {
            let mut state = self.lock_state();
            if state.last_purge != today {
                state.last_purge = today;
                true
            } else {
                false
            }
        };
        if !due {
            return;
        }
        let log_dir = self.data_dir.join(store::LOGS_DIR_NAME);
        let retention_days = self.settings_snapshot().log_retention_days;
        match logs::purge_old_logs(&log_dir, retention_days, today) {
            Ok(report) => {
                if report.deleted > 0 || report.failed > 0 {
                    diag::info(&format!(
                        "日志清理：删除 {} 个，失败 {} 个，保留 {} 个，跳过 {} 个（保留 {} 天）",
                        report.deleted, report.failed, report.kept, report.skipped, retention_days
                    ));
                }
            }
            Err(err) => diag::warn(&format!("日志清理失败（{}）：{err}", log_dir.display())),
        }
    }

    fn all_settled(&self) -> bool {
        let state = self.lock_state();
        state
            .instances
            .iter()
            .all(|instance| !instance.is_active() && !instance.has_process())
    }

    /// core 退出前把托盘收干净：等它自己退出（它会从状态里看到 shutdown），超时才杀。
    fn finish_tray(&self) {
        let mut tray = self.lock_tray();
        let Some(mut child) = tray.child.take() else {
            return;
        };
        let pid = child.id();
        let deadline = Instant::now() + TRAY_EXIT_WAIT;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => {
                    diag::info(&format!("托盘子进程已退出：pid={pid}"));
                    break;
                }
                Ok(None) => {
                    if Instant::now() >= deadline {
                        diag::warn(&format!("托盘 {pid} 未按时退出，TerminateProcess"));
                        let _ = child.kill();
                        let _ = child.wait();
                        break;
                    }
                    thread::sleep(Duration::from_millis(50));
                }
                Err(err) => {
                    diag::warn(&format!("等待托盘退出失败：{err}"));
                    break;
                }
            }
        }
    }
}

// ── 控制台后端：gui crate 通过这个 trait 调 core（依赖方向 core → gui） ──

impl gui::ConsoleBackend for Runtime {
    fn state(&self) -> gui::ConsoleState {
        self.console_state()
    }

    fn start(&self, key: &str) -> gui::ActionResult {
        self.action_from(self.start_by_key(key))
    }

    fn stop(&self, key: &str) -> gui::ActionResult {
        self.action_from(self.stop(key))
    }

    fn open(&self, key: &str) -> gui::ActionResult {
        self.action_from(self.open_browser(key))
    }

    fn logs(&self, key: &str, tail: usize) -> Result<gui::LogPayload, gui::BackendError> {
        self.instance_logs(key, tail)
    }

    fn set_env(&self, key: &str, env: BTreeMap<String, String>) -> gui::ActionResult {
        if let Err(err) = validate_env(&env) {
            return gui::ActionResult::fail(400, err);
        }
        let Some((capsule_id, profile)) = split_key(key) else {
            return gui::ActionResult::fail(
                400,
                format!("实例键格式应是 capsuleId/profile：{key}"),
            );
        };
        let running = self.is_instance_active(key);
        match self
            .update_instance_config(capsule_id, profile, |instance| instance.env = env.clone())
        {
            Ok(()) => {
                diag::info(&format!(
                    "控制台：更新实例 env：key={key} 条数={}",
                    env.len()
                ));
                let notice = if running {
                    "已保存，重启实例后生效"
                } else {
                    "已保存，下次启动生效"
                };
                gui::ActionResult::ok(self.console_state(), notice)
            }
            Err(err) => gui::ActionResult::fail(400, err),
        }
    }

    fn set_alias(&self, key: &str, alias: Option<String>) -> gui::ActionResult {
        let alias = alias
            .map(|text| text.trim().to_string())
            .filter(|text| !text.is_empty());
        if let Some(text) = &alias {
            if text.chars().count() > 64 {
                return gui::ActionResult::fail(400, "别名最长 64 个字符");
            }
            if text.chars().any(char::is_control) {
                return gui::ActionResult::fail(400, "别名不能含控制字符");
            }
        }
        let Some((capsule_id, profile)) = split_key(key) else {
            return gui::ActionResult::fail(
                400,
                format!("实例键格式应是 capsuleId/profile：{key}"),
            );
        };
        match self.update_instance_config(capsule_id, profile, |instance| {
            instance.alias = alias.clone();
        }) {
            Ok(()) => {
                diag::info(&format!(
                    "控制台：更新实例别名：key={key} alias={:?}",
                    alias
                ));
                gui::ActionResult::ok(self.console_state(), "已保存别名")
            }
            Err(err) => gui::ActionResult::fail(400, err),
        }
    }

    fn register(&self, root: &str) -> gui::ActionResult {
        let root = Path::new(root.trim());
        if root.as_os_str().is_empty() {
            return gui::ActionResult::fail(400, "胶囊目录不能为空");
        }
        match self.register_root(root) {
            Ok(notice) => gui::ActionResult::ok(self.console_state(), notice),
            Err(err) => gui::ActionResult::fail(400, err),
        }
    }

    fn remove_capsule(&self, capsule_id: &str, confirm: &str) -> gui::ActionResult {
        match self.remove_capsule_record(capsule_id, confirm) {
            Ok(()) => {
                gui::ActionResult::ok(self.console_state(), format!("已移除胶囊 {capsule_id}"))
            }
            Err((status, message)) => gui::ActionResult::fail(status, message),
        }
    }

    fn set_settings(&self, settings: gui::SettingsRequest) -> gui::ActionResult {
        Runtime::apply_settings(self, settings)
    }

    fn set_webhooks(&self, webhooks: Vec<gui::WebhookView>) -> gui::ActionResult {
        Runtime::apply_webhooks(self, webhooks)
    }

    fn set_instance_port(&self, key: &str, port: u16) -> gui::ActionResult {
        Runtime::change_instance_port(self, key, port)
    }

    fn set_instance_host(&self, key: &str, host: &str) -> gui::ActionResult {
        Runtime::change_instance_host(self, key, host)
    }

    fn set_instance_autostart(&self, key: &str, autostart: bool) -> gui::ActionResult {
        Runtime::set_autostart_flag(self, key, autostart)
    }

    fn enable_autostart(&self) -> gui::ActionResult {
        Runtime::enable_login_autostart(self)
    }

    fn disable_autostart(&self) -> gui::ActionResult {
        Runtime::disable_login_autostart(self)
    }

    fn builder_start(&self, request: gui::BuilderStartRequest) -> gui::ActionResult {
        match self.start_build(request) {
            Ok(()) => gui::ActionResult::ok(self.console_state(), "已开始构建"),
            Err((status, message)) => gui::ActionResult::fail(status, message),
        }
    }

    fn builder_cancel(&self) -> gui::ActionResult {
        match self.builder.cancel() {
            Ok(()) => gui::ActionResult::ok(self.console_state(), "已请求取消构建"),
            Err((status, message)) => gui::ActionResult::fail(status, message),
        }
    }

    fn builder_log(&self, tail: usize) -> Result<gui::LogPayload, gui::BackendError> {
        self.builder.log_tail(tail)
    }

    fn builder_versions(&self, refresh: bool) -> gui::BuilderVersionsView {
        self.builder_versions(refresh)
    }
}

/// `capsuleId/profile` → (capsuleId, profile)：键里**最后一个** `/` 是分隔符。
fn split_key(key: &str) -> Option<(&str, &str)> {
    let (capsule_id, profile) = key.rsplit_once('/')?;
    if capsule_id.is_empty() || profile.is_empty() {
        return None;
    }
    Some((capsule_id, profile))
}

/// env 校验（§3.18）：条目数、键/值长度、`=` / NUL。
fn validate_env(env: &BTreeMap<String, String>) -> Result<(), String> {
    const MAX_ENTRIES: usize = 64;
    const MAX_KEY_CHARS: usize = 128;
    const MAX_VALUE_CHARS: usize = 8192;
    if env.len() > MAX_ENTRIES {
        return Err(format!("env 变量最多 {MAX_ENTRIES} 个"));
    }
    for (key, value) in env {
        if key.trim().is_empty() {
            return Err("env 里有空变量名".to_string());
        }
        if key.starts_with('=') {
            return Err(format!("变量名不能以 '=' 开头：{key}"));
        }
        if key.contains('=') || key.contains('\0') {
            return Err(format!("变量名不能含 '=' 或 NUL：{key}"));
        }
        if key.chars().count() > MAX_KEY_CHARS {
            return Err(format!("变量名太长（>{MAX_KEY_CHARS} 字符）：{key}"));
        }
        if value.contains('\0') {
            return Err(format!("变量 {key} 的值含 NUL"));
        }
        if value.chars().count() > MAX_VALUE_CHARS {
            return Err(format!("变量 {key} 的值太长（>{MAX_VALUE_CHARS} 字符）"));
        }
    }
    Ok(())
}

/// 停止线程主体：优雅停 → 超时强杀 → 收日志 → 落 Stopped → 发 Service Stop 事件。
fn run_stop(
    instance: Arc<Instance>,
    key: String,
    resources: crate::instance::StopResources,
    notifier: Arc<notify::Notifier>,
    facts: notify::Facts,
) {
    let started = Instant::now();
    let pid = resources.pid;
    let mut graceful_sent = false;
    let mut exited_after_graceful = false;

    if let Some(process) = resources.process.as_ref() {
        if let Some(pid) = pid {
            match job::request_graceful_stop(pid) {
                Ok(()) => {
                    graceful_sent = true;
                    diag::info(&format!(
                        "停止实例：key={key} 已发送 CTRL_C（dsh 的 SIGINT 优雅停），等待退出"
                    ));
                    exited_after_graceful =
                        job::wait_process(process, GRACEFUL_STOP_TIMEOUT.as_millis() as u32);
                }
                Err(err) => {
                    diag::warn(&format!("优雅停不可用（key={key}）：{err}；直接强杀"));
                }
            }
        }

        if !exited_after_graceful {
            let pids = resources
                .job
                .as_ref()
                .map(job::job_process_ids)
                .unwrap_or_default();
            match resources.job.as_ref() {
                Some(job_handle) => {
                    diag::warn(&format!(
                        "优雅停未在 {}s 内生效（key={key}，job 内进程 {pids:?}）→ TerminateJobObject",
                        GRACEFUL_STOP_TIMEOUT.as_secs()
                    ));
                    if let Err(err) = job::terminate_job(job_handle, FORCE_KILL_EXIT_CODE) {
                        diag::error(&format!("TerminateJobObject 失败（key={key}）：{err}"));
                    }
                    let _ = job::wait_process(process, FORCE_STOP_TIMEOUT.as_millis() as u32);
                }
                None => {
                    let _ = job::wait_process(process, FORCE_STOP_TIMEOUT.as_millis() as u32);
                }
            }
        }
    }

    let exit_code = resources.process.as_ref().and_then(job::process_exit_code);

    if let Some(pump) = resources.logs {
        let log_path = pump.log_path();
        if !pump.join_bounded(LOG_JOIN_TIMEOUT) {
            diag::warn(&format!(
                "日志线程未在 {}s 内收尾（key={key}，日志 {}）；可能仍有孙进程攥着管道写端",
                LOG_JOIN_TIMEOUT.as_secs(),
                log_path.display()
            ));
        }
    }

    // Service Stop：用户请求的停止**完成后**才发（core 退出时的停止也走这条路径）。
    //
    // 顺序很重要：必须在 `finish_stop` **之前**入队。supervisor 的退出条件是
    // 「shutting_down && all_settled」，而 `all_settled` 要等状态从 `Stopping` 落成
    // `Stopped`；先入队再落状态，core 退出前的 `drain_webhooks` 才一定排在这条事件后面
    // （否则「运行中 quit」有极小概率丢最后一条 Stop）。
    notifier.event(HookTrigger::Stop, &facts);

    instance.finish_stop(exit_code, None);
    diag::info(&format!(
        "实例已停止：key={key} pid={pid:?} 用时={}ms 优雅={graceful_sent} exit={}",
        started.elapsed().as_millis(),
        exit_code
            .map(|code| format!("{code}（0x{code:08X}）"))
            .unwrap_or_else(|| "未知".to_string())
    ));
}

/// webhook 配置 → 控制台视图（逐条）。
fn webhook_view(hook: &Webhook) -> gui::WebhookView {
    gui::WebhookView {
        url: hook.url.clone(),
        enabled: hook.enabled,
        start: trigger_view(&hook.start),
        stop: trigger_view(&hook.stop),
        crashed: trigger_view(&hook.crashed),
        housing_start: Some(trigger_view(&hook.housing_start)),
    }
}

fn trigger_view(config: &dsh_housing_webhook::TriggerConfig) -> gui::TriggerView {
    gui::TriggerView {
        enabled: config.enabled,
        template: config.template.clone(),
    }
}

/// 控制台视图 → webhook 配置（保存时用；URL 去掉首尾空白）。
fn webhook_from_view(view: &gui::WebhookView) -> Webhook {
    Webhook {
        url: view.url.trim().to_string(),
        enabled: view.enabled,
        start: trigger_config_from_view(&view.start),
        stop: trigger_config_from_view(&view.stop),
        crashed: trigger_config_from_view(&view.crashed),
        // 旧客户端不带这个字段 = 新触发条件按默认模板 + 启用补齐（与手写文件同一口径）。
        housing_start: view
            .housing_start
            .as_ref()
            .map(trigger_config_from_view)
            .unwrap_or_else(|| HookTrigger::HousingStart.default_config()),
    }
}

fn trigger_config_from_view(view: &gui::TriggerView) -> dsh_housing_webhook::TriggerConfig {
    dsh_housing_webhook::TriggerConfig {
        enabled: view.enabled,
        template: view.template.clone(),
    }
}

/// 实例的**有效显示名**（§3.20）：用户设的 `alias` 优先，否则用派生默认名。
///
/// 默认名只活在视图里、不落盘，所以刷新登记换了 `app.version` 后名字自动跟着走。
fn effective_alias(
    record: &capsule::CapsuleRecord,
    instance: &capsule::RegisteredInstance,
) -> String {
    instance.alias.clone().unwrap_or_else(|| {
        capsule::default_instance_alias(
            &record.capsule_id,
            record.app.as_ref().map(|app| app.version.as_str()),
            &instance.profile,
        )
    })
}

/// 快照合并（§3.14）：登记项为主，运行项按实例键覆盖；未登记的活实例追加在末尾。
///
/// 纯函数，便于单测；托盘只按它画菜单，不做任何状态推断。
fn merge_views(specs: &[InstanceSpec], live: &[InstanceView]) -> Vec<InstanceView> {
    let mut merged = Vec::with_capacity(specs.len() + live.len());
    let mut used = vec![false; live.len()];
    for spec in specs {
        let key = spec.key();
        match live.iter().position(|view| view.key == key) {
            Some(index) => {
                used[index] = true;
                merged.push(live[index].clone());
            }
            None => merged.push(InstanceView::stopped_for(spec)),
        }
    }
    for (index, view) in live.iter().enumerate() {
        if !used[index] {
            merged.push(view.clone());
        }
    }
    merged
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;
    use std::fs;

    fn spec(capsule: &str, profile: &str, port: u16) -> InstanceSpec {
        InstanceSpec {
            capsule_root: PathBuf::from(format!(r"D:\capsules\{capsule}")),
            capsule_id: capsule.to_string(),
            profile: profile.to_string(),
            port,
            host: None,
            workspace: PathBuf::from(format!(r"D:\capsules\{capsule}\workspace-{profile}")),
            env: BTreeMap::new(),
            alias: None,
        }
    }

    #[test]
    fn merge_keeps_registered_stopped_rows_and_live_state() {
        let specs = vec![spec("cap", "chat", 39411), spec("cap", "web", 39412)];
        let mut running = InstanceView::stopped_for(&specs[1]);
        running.state = "running".to_string();
        running.pid = Some(4242);

        let merged = merge_views(&specs, &[running]);
        assert_eq!(merged.len(), 2, "登记了两个实例就应显示两行");
        assert_eq!(merged[0].key, "cap/chat");
        assert_eq!(merged[0].state, "stopped");
        assert_eq!(merged[1].key, "cap/web");
        assert_eq!(merged[1].state, "running");
        assert_eq!(merged[1].pid, Some(4242));
    }

    #[test]
    fn merge_appends_unregistered_live_instances_instead_of_hiding_them() {
        let specs = vec![spec("cap", "web", 39412)];
        let mut live = InstanceView::stopped_for(&spec("ghost", "web", 40000));
        live.state = "running".to_string();

        let merged = merge_views(&specs, &[live]);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0].key, "cap/web");
        assert_eq!(merged[1].key, "ghost/web");
    }

    #[test]
    fn merge_without_registry_is_empty() {
        assert!(merge_views(&[], &[]).is_empty());
    }

    #[test]
    fn instance_key_splits_on_the_last_slash() {
        assert_eq!(split_key("cap/web"), Some(("cap", "web")));
        assert_eq!(
            split_key("cap.with.dots/web"),
            Some(("cap.with.dots", "web"))
        );
        assert_eq!(split_key("no-slash"), None);
        assert_eq!(split_key("/web"), None);
        assert_eq!(split_key("cap/"), None);
    }

    #[test]
    fn env_validation_rejects_bad_shapes() {
        let mut env = BTreeMap::new();
        env.insert("OK".to_string(), "1".to_string());
        assert!(validate_env(&env).is_ok());

        env.insert("BAD=KEY".to_string(), "x".to_string());
        assert!(validate_env(&env).is_err());

        let mut empty_key = BTreeMap::new();
        empty_key.insert("   ".to_string(), "x".to_string());
        assert!(validate_env(&empty_key).is_err());

        let mut nul_value = BTreeMap::new();
        nul_value.insert("K".to_string(), "a\0b".to_string());
        assert!(validate_env(&nul_value).is_err());

        let mut too_many = BTreeMap::new();
        for index in 0..65 {
            too_many.insert(format!("K{index}"), "v".to_string());
        }
        assert!(validate_env(&too_many).is_err());
    }

    fn registered(alias: Option<&str>) -> capsule::CapsuleRecord {
        capsule::CapsuleRecord {
            capsule_id: "0.2.0-rc.2-default".to_string(),
            root: PathBuf::from(r"D:\Capsules\0.2.0-rc.2-default"),
            alias: None,
            schema: 1,
            registered_at: 1,
            checked_at: 2,
            runtime: Some(capsule::RuntimeInfo {
                kind: "node-portable-zip".to_string(),
                version: "24.21.0".to_string(),
            }),
            app: Some(capsule::AppInfo {
                package: "@deepseek-ai/dsh".to_string(),
                version: "0.2.0-rc.2".to_string(),
                bin: "lib/bin.js".to_string(),
            }),
            warnings: Vec::new(),
            instances: vec![capsule::RegisteredInstance {
                profile: "web".to_string(),
                port: 39412,
                host: None,
                workspace: PathBuf::from(r"D:\Capsules\0.2.0-rc.2-default\workspace-web"),
                alias: alias.map(str::to_string),
                autostart: false,
                env: BTreeMap::new(),
            }],
        }
    }

    fn runtime_with(record: capsule::CapsuleRecord) -> Runtime {
        let mut file = CapsulesFile::default();
        file.capsules.push(record);
        Runtime::new(
            PathBuf::from(r"D:\Tools\dsh-housing.exe"),
            PathBuf::from(r"D:\Tools\.dsh-housing"),
            Settings::default(),
            RegistryState::from_file(file),
            WebhookState {
                file: WebhookFile::default(),
                load_error: None,
            },
            autostart::IntegrityLevel::High,
        )
    }

    #[test]
    fn views_expose_the_default_alias_when_the_user_has_not_set_one() {
        // §3.20：默认名是派生值（不落盘），实例列表与登记表视图都要给出它。
        let runtime = runtime_with(registered(None));

        let snapshot = runtime.snapshot();
        assert_eq!(
            snapshot.instances[0].alias.as_deref(),
            Some("Deepseek Harness 0.2.0-rc.2 web")
        );

        let view = runtime.registry_view();
        assert_eq!(
            view.capsules[0].instances[0].alias.as_deref(),
            Some("Deepseek Harness 0.2.0-rc.2 web")
        );
    }

    #[test]
    fn views_prefer_the_user_alias() {
        let runtime = runtime_with(registered(Some("网页版")));
        assert_eq!(
            runtime.snapshot().instances[0].alias.as_deref(),
            Some("网页版")
        );
        assert_eq!(
            runtime.registry_view().capsules[0].instances[0]
                .alias
                .as_deref(),
            Some("网页版")
        );
    }

    /// 测试用数据目录（与 `store` 的单测同一套约定：工作区 `target/test-tmp`）。
    fn temp_data_dir(label: &str) -> PathBuf {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("target")
            .join("test-tmp");
        fs::create_dir_all(&root).expect("创建测试根目录");
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("系统时间早于 UNIX 纪元")
            .as_nanos();
        root.join(format!("{label}-{}-{nanos}", std::process::id()))
    }

    fn webhook_view(url: &str) -> gui::WebhookView {
        gui::WebhookView {
            url: url.to_string(),
            enabled: true,
            start: gui::TriggerView {
                enabled: true,
                template: "start {alias}".to_string(),
            },
            stop: gui::TriggerView {
                enabled: false,
                template: String::new(),
            },
            crashed: gui::TriggerView {
                enabled: true,
                template: "crash {alias}".to_string(),
            },
            housing_start: Some(gui::TriggerView {
                enabled: true,
                template: "housing {local_url}".to_string(),
            }),
        }
    }

    #[test]
    fn event_facts_use_the_effective_alias_and_app_version() {
        // §3.21：通知里的 Name / Version 来自登记表（别名以登记表为真源，§3.20）。
        let runtime = runtime_with(registered(None));
        let (alias, version) = runtime
            .lock_registry()
            .facts_for("0.2.0-rc.2-default", "web");
        assert_eq!(alias, "Deepseek Harness 0.2.0-rc.2 web");
        assert_eq!(version, "0.2.0-rc.2");

        let (alias, version) = runtime.lock_registry().facts_for("ghost", "web");
        assert_eq!(alias, "ghost/web", "没登记时退回实例键，不空着");
        assert_eq!(version, "ghost");
    }

    #[test]
    fn change_instance_host_validates_and_persists() {
        let data_dir = temp_data_dir("host-change");
        fs::create_dir_all(&data_dir).expect("创建测试数据目录");
        let record = registered(None);
        let mut file = CapsulesFile::default();
        file.capsules.push(record);
        store::save_capsules(&data_dir, &file).expect("写登记表");
        let runtime = Runtime::new(
            PathBuf::from(r"D:\Tools\dsh-housing.exe"),
            data_dir.clone(),
            Settings::default(),
            RegistryState::from_file(file),
            WebhookState {
                file: WebhookFile::default(),
                load_error: None,
            },
            autostart::IntegrityLevel::High,
        );
        let key = "0.2.0-rc.2-default/web";

        let wildcard = runtime.change_instance_host(key, "0.0.0.0");
        assert_eq!(wildcard.status, 400);
        assert!(wildcard.error.unwrap().contains("通配"));

        // dsh 0.2.0 的 webserver schema 只认 127.0.0.1 / 0.0.0.0：具体 IP 必须被控制台拦下，
        // 否则实例起来后 dsh 会 ValidationError（实测见 §3.22）。
        let old_version = runtime.change_instance_host(key, "192.168.3.192");
        assert_eq!(old_version.status, 400);
        assert!(
            old_version.error.unwrap().contains("0.2.0"),
            "错误里要点名版本限制"
        );

        let ok = runtime.change_instance_host(key, "127.0.0.1");
        assert_eq!(ok.status, 200, "{:?}", ok.error);
        assert_eq!(
            store::load_capsules(&data_dir).expect("读登记表").capsules[0].instances[0]
                .host
                .as_deref(),
            Some("127.0.0.1")
        );
        assert_eq!(
            runtime.snapshot().instances[0].host.as_deref(),
            Some("127.0.0.1"),
            "快照要立刻反映（下次启动生效，但展示先更新）"
        );

        // 0.2.1+ 支持具体 IP：同一套接口换成 0.2.1 的登记记录后应当成功。
        let mut newer = registered(None);
        newer.app = Some(capsule::AppInfo {
            package: "@deepseek-ai/dsh".to_string(),
            version: "0.2.1-rc.2".to_string(),
            bin: "lib/bin.js".to_string(),
        });
        let mut newer_file = CapsulesFile::default();
        newer_file.capsules.push(newer);
        store::save_capsules(&data_dir, &newer_file).expect("写 0.2.1 登记表");
        let runtime_021 = Runtime::new(
            PathBuf::from(r"D:\Tools\dsh-housing.exe"),
            data_dir.clone(),
            Settings::default(),
            RegistryState::from_file(newer_file),
            WebhookState {
                file: WebhookFile::default(),
                load_error: None,
            },
            autostart::IntegrityLevel::High,
        );
        let lan = runtime_021.change_instance_host(key, " 192.168.3.192 ");
        assert_eq!(lan.status, 200, "{:?}", lan.error);
        assert_eq!(
            store::load_capsules(&data_dir)
                .expect("读 0.2.1 登记表")
                .capsules[0]
                .instances[0]
                .host
                .as_deref(),
            Some("192.168.3.192")
        );

        let cleared = runtime_021.change_instance_host(key, "");
        assert_eq!(cleared.status, 200);
        assert_eq!(
            store::load_capsules(&data_dir).expect("读登记表").capsules[0].instances[0].host,
            None
        );

        assert_eq!(
            runtime.change_instance_host("nope/web", "127.0.0.1").status,
            404
        );
        fs::remove_dir_all(&data_dir).ok();
    }

    #[test]
    fn webhooks_are_validated_persisted_and_exposed_to_the_console() {
        let data_dir = temp_data_dir("runtime-webhooks");
        store::prepare_data_dir(&data_dir).expect("初始化数据目录");
        let runtime = Runtime::new(
            PathBuf::from(r"D:\Tools\dsh-housing.exe"),
            data_dir.clone(),
            Settings::default(),
            RegistryState::from_file(CapsulesFile::default()),
            WebhookState {
                file: WebhookFile::default(),
                load_error: None,
            },
            autostart::IntegrityLevel::High,
        );

        // 非法 URL → 400，且不落盘。
        let bad = runtime.apply_webhooks(vec![webhook_view("ftp://example.test/x")]);
        assert_eq!(bad.status, 400);
        assert!(bad.error.unwrap().contains("http://"));
        assert_eq!(
            store::load_webhooks(&data_dir).expect("读配置"),
            WebhookFile::default(),
            "校验失败不能写盘"
        );

        // 合法 → 200 + 原子落盘 + 视图与本进程投递配置都换掉。
        let saved = runtime.apply_webhooks(vec![webhook_view("https://ntfy.example.test/_dsh")]);
        assert_eq!(saved.status, 200, "{:?}", saved.error);
        let stored = store::load_webhooks(&data_dir).expect("读配置");
        assert_eq!(stored.webhooks.len(), 1);
        assert_eq!(stored.webhooks[0].url, "https://ntfy.example.test/_dsh");
        assert!(!stored.webhooks[0].stop.enabled);

        let section = runtime.console_state().webhooks;
        assert_eq!(section.items.len(), 1);
        assert_eq!(section.items[0].crashed.template, "crash {alias}");
        assert_eq!(
            section.items[0]
                .housing_start
                .as_ref()
                .map(|trigger| trigger.template.as_str()),
            Some("housing {local_url}"),
            "housing_start 也要透传到控制台视图"
        );
        assert!(section.file_path.ends_with("webhooks.json"));
        assert!(
            section.defaults.start.contains("{local_url}"),
            "默认模板由 webhook crate 提供，UI 不复制"
        );
        assert!(
            section.defaults.housing_start.contains("Housing Start"),
            "Housing Start 默认模板由 webhook crate 提供"
        );
        assert!(section.placeholders.contains(&"{lan_url}".to_string()));

        // 全量替换：空列表 = 全部关掉（不是「没配置」）。
        let cleared = runtime.apply_webhooks(Vec::new());
        assert_eq!(cleared.status, 200);
        assert!(store::load_webhooks(&data_dir)
            .expect("读配置")
            .webhooks
            .is_empty());

        fs::remove_dir_all(&data_dir).ok();
    }

    #[test]
    fn settings_console_host_is_validated_and_persisted() {
        let data_dir = temp_data_dir("settings-console-host");
        store::prepare_data_dir(&data_dir).expect("初始化数据目录");
        let runtime = Runtime::new(
            PathBuf::from(r"D:\Tools\dsh-housing.exe"),
            data_dir.clone(),
            Settings::default(),
            RegistryState::from_file(CapsulesFile::default()),
            WebhookState {
                file: WebhookFile::default(),
                load_error: None,
            },
            autostart::IntegrityLevel::High,
        );
        let request = |console_host: Option<&str>| gui::SettingsRequest {
            gui_port: 0,
            console_host: console_host.map(str::to_string),
            sample_interval_ms: 3_000,
            auto_open_browser: false,
            auto_open_console: None,
            tray_restart_max: 3,
            tray_restart_backoff_ms: vec![1_000, 5_000, 15_000],
            log_retention_days: 14,
        };

        // 域名不是字面量 → 400；本机不存在的地址 → 400（bind 探测，§3.23 / D33）。
        let bad = runtime.apply_settings(request(Some("localhost")));
        assert_eq!(bad.status, 400);
        assert!(bad.error.unwrap().contains("控制台启动位置"));
        let unreachable = runtime.apply_settings(request(Some("203.0.113.9")));
        assert_eq!(unreachable.status, 400);
        assert!(unreachable.error.unwrap().contains("绑不上"));
        assert_eq!(
            store::load_settings(&data_dir)
                .expect("读设置")
                .console_host,
            "127.0.0.1",
            "校验失败不能写盘"
        );

        // 通配地址 → 200 + 「重启 core 后生效」+ 落盘 + 视图同步。
        let saved = runtime.apply_settings(request(Some("0.0.0.0")));
        assert_eq!(saved.status, 200, "{:?}", saved.error);
        assert!(saved.notice.unwrap().contains("控制台启动位置"));
        assert_eq!(
            store::load_settings(&data_dir)
                .expect("读设置")
                .console_host,
            "0.0.0.0"
        );
        assert_eq!(runtime.settings_view().console_host, "0.0.0.0");

        // 旧客户端不带字段 = 保留当前值（不静默回退到默认回环）。
        let kept = runtime.apply_settings(request(None));
        assert_eq!(kept.status, 200, "{:?}", kept.error);
        assert_eq!(
            store::load_settings(&data_dir)
                .expect("读设置")
                .console_host,
            "0.0.0.0"
        );

        fs::remove_dir_all(&data_dir).ok();
    }
}
