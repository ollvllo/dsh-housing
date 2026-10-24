//! 登录自启动：用任务计划（`taskschd` COM）把 core 拉起来（docs/structure.md §3.12、§5.9 D13/D17）。
//!
//! # 边界
//!
//! - **只建自己的任务**：任务名固定 [`TASK_NAME`]（`dsh-housing`），不带用户名 / exe 路径。
//!   其它任务（含 `dsh-startup-notify`）一律不碰。
//! - **旧链路留回滚**：旧任务 [`LEGACY_TASK_NAME`]（`dsh-start`）若存在且启用，先把它的完整
//!   XML 备份到 `<数据目录>\backup\`，再 `SetEnabled(false)`；备份或禁用任一步失败就**中止启用**
//!   （不半途而废，避免新旧两条链路双启动抢端口）。停用（回滚）时把它重新启用。
//! - **完整性级别守卫**（§5.4）：本进程 < Medium 时拒绝启用。任务计划会把「开局即坏」固化：
//!   Low 的 core 拉起的托盘装不上图标（UIPI / `E_ACCESSDENIED`），随后重启 3 次托盘再自杀。
//!
//! # 为什么是 COM 而不是 `schtasks.exe`
//!
//! 不用写临时 XML、不用起一个控制台进程；失败拿的是 HRESULT 而不是本地化文本；读现状也走同一套
//! 接口（`schtasks /Query` 只有 `/XML` 可解析，等于自写 XML 解析器，还得处理 UTF-16 BOM）。

use std::ffi::c_void;
use std::fs;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use windows::core::{Interface, BSTR, VARIANT};
use windows::Win32::Foundation::{
    HANDLE, RPC_E_CHANGED_MODE, VARIANT_BOOL, VARIANT_FALSE, VARIANT_TRUE,
};
use windows::Win32::Security::{
    GetSidSubAuthority, GetSidSubAuthorityCount, GetTokenInformation, TokenIntegrityLevel,
    TokenUser, PSID, TOKEN_MANDATORY_LABEL, TOKEN_QUERY, TOKEN_USER,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_MULTITHREADED,
};
use windows::Win32::System::TaskScheduler::{
    IExecAction, ILogonTrigger, IRegisteredTask, ITaskDefinition, ITaskFolder, ITaskService,
    TaskScheduler, TASK_ACTION_EXEC, TASK_ACTION_TYPE, TASK_CREATE_OR_UPDATE,
    TASK_INSTANCES_IGNORE_NEW, TASK_LOGON_INTERACTIVE_TOKEN, TASK_RUNLEVEL_HIGHEST,
    TASK_TRIGGER_LOGON,
};
use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

use crate::diag;
use crate::store;

/// 本项目自己的任务名（对外可见，任务计划 UI 里就长这样）。
pub const TASK_NAME: &str = "dsh-housing";
/// 旧链路任务名（`任务计划 → cmd → start.cmd`）。只备份 / 停用 / 恢复，从不删除。
pub const LEGACY_TASK_NAME: &str = "dsh-start";
/// 任务 Action 的参数（内部角色分发，不是对外 CLI）。
pub const TASK_ARGUMENTS: &str = "--role=core";
/// 登录触发后的延迟：等 `explorer` / 任务栏就位（更早起来也有 `TaskbarCreated` 兜底，§3.15）。
pub const LOGON_DELAY: &str = "PT15S";
/// 任务描述（任务计划 UI 里显示）。
const TASK_DESCRIPTION: &str =
    "登录时启动 dsh 胶囊启动器（core）：拉起托盘与标记了自启的实例。由 dsh-housing 控制台维护。";

/// 本进程的完整性级别（决定托盘能不能装图标，§5.4）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IntegrityLevel {
    Untrusted,
    Low,
    Medium,
    High,
    System,
    /// 读不出来（`OpenProcessToken` / `GetTokenInformation` 失败）。
    Unknown,
}

impl IntegrityLevel {
    /// 给控制台显示的短标签。
    pub fn label(self) -> &'static str {
        match self {
            Self::Untrusted => "untrusted",
            Self::Low => "low",
            Self::Medium => "medium",
            Self::High => "high",
            Self::System => "system",
            Self::Unknown => "unknown",
        }
    }

    /// 能不能启用自启动：托盘要 ≥ Medium（本机任务栏是 High，UIPI 只放行同级及以上）。
    pub fn is_viable(self) -> bool {
        matches!(self, Self::Medium | Self::High | Self::System)
    }
}

/// 从完整性级别的 RID 推级别（纯函数，单测用）。
fn integrity_from_rid(rid: u32) -> IntegrityLevel {
    match rid {
        0x0000..=0x0FFF => IntegrityLevel::Untrusted,
        0x1000..=0x1FFF => IntegrityLevel::Low,
        0x2000..=0x2FFF => IntegrityLevel::Medium,
        0x3000..=0x3FFF => IntegrityLevel::High,
        _ => IntegrityLevel::System,
    }
}

/// 任务现状。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    /// 任务存在、启用、Action 指向本 exe。
    Enabled,
    /// 任务存在、Action 指向本 exe，但被停用。
    Disabled,
    /// 没有这个任务。
    Missing,
    /// 同名任务存在，但 Action 不是本 exe（多半是旧版本 / 手工建的）。
    Foreign,
    /// 查询失败（COM 不可用等）。
    Error,
}

impl State {
    pub fn label(self) -> &'static str {
        match self {
            Self::Enabled => "enabled",
            Self::Disabled => "disabled",
            Self::Missing => "missing",
            Self::Foreign => "foreign",
            Self::Error => "error",
        }
    }
}

/// 一次状态查询的结果（不失败：错误落进 `state` + `detail`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskStatus {
    pub task_name: String,
    pub state: State,
    /// 人话补充（Foreign 时是任务实际指向的路径、Error 时是 COM 错误）。
    pub detail: Option<String>,
    /// 任务 Action 的命令部分（能读到才有）。
    pub action: Option<String>,
    /// 旧链路任务现状：`None` = 不存在。
    pub legacy: Option<String>,
}

impl TaskStatus {
    fn new(state: State) -> Self {
        Self {
            task_name: TASK_NAME.to_string(),
            state,
            detail: None,
            action: None,
            legacy: None,
        }
    }
}

/// 一次 COM 会话：服务 + 根目录（每次操作新建，不做全局缓存）。
struct Scheduler {
    service: ITaskService,
    folder: ITaskFolder,
}

/// 查询任务现状（`/api/state` 的 `autostart` 字段用它，5 s 缓存，见 `runtime.rs`）。
pub fn status(exe: &Path) -> TaskStatus {
    status_named(exe, TASK_NAME)
}

/// 按任务名查询（`TASK_NAME` 之外的名字只给单测 / 自检用，免得碰真实任务）。
pub fn status_named(exe: &Path, task_name: &str) -> TaskStatus {
    let scheduler = match connect() {
        Ok(scheduler) => scheduler,
        Err(err) => {
            let mut status = TaskStatus::new(State::Error);
            status.detail = Some(err);
            return status;
        }
    };

    let mut status = match lookup(&scheduler, task_name) {
        Ok(Some(task)) => describe(&task, exe),
        Ok(None) => TaskStatus::new(State::Missing),
        Err(err) => {
            let mut status = TaskStatus::new(State::Error);
            status.detail = Some(err);
            status
        }
    };
    status.task_name = task_name.to_string();
    status.legacy = match lookup(&scheduler, LEGACY_TASK_NAME) {
        Ok(Some(task)) => Some(if is_enabled(&task) {
            "enabled".to_string()
        } else {
            "disabled".to_string()
        }),
        Ok(None) => None,
        Err(err) => Some(format!("查询失败：{err}")),
    };
    status
}

/// 启用自启动：备份并停用旧链路（若有）→ 建 / 改自己的任务。
pub fn enable(exe: &Path, data_dir: &Path) -> Result<String, String> {
    enable_named(exe, data_dir, TASK_NAME)
}

/// 按任务名启用（`TASK_NAME` 之外的名字只给单测 / 自检用）。
pub fn enable_named(exe: &Path, data_dir: &Path, task_name: &str) -> Result<String, String> {
    let integrity = current_integrity();
    if !integrity.is_viable() {
        return Err(format!(
            "当前进程的完整性级别是 {}，低于 medium：任务计划拉起的 core 会同样低，托盘图标必被 UIPI 拒（E_ACCESSDENIED）。\
             请把 exe 放到不带 Low 完整性标签的目录（例如 C:\\Tools\\dsh-housing\\）后再启用。",
            integrity.label()
        ));
    }
    if !exe.is_absolute() {
        return Err(format!("exe 路径不是绝对路径：{}", exe.display()));
    }

    let scheduler = connect()?;
    let mut notes: Vec<String> = Vec::new();

    // 1) 旧链路：先备份 XML，再停用（失败就中止，避免两条链路双启动）。
    if let Some(legacy) = lookup(&scheduler, LEGACY_TASK_NAME)? {
        if is_enabled(&legacy) {
            let backup = backup_task_xml(&legacy, data_dir)?;
            set_enabled(&legacy, false).map_err(|err| {
                format!("备份已生成（{}），但停用旧链路任务 {LEGACY_TASK_NAME} 失败：{err}；本次启用已中止", backup.display())
            })?;
            diag::warn(&format!(
                "自启动：已停用旧链路任务 {LEGACY_TASK_NAME}（定义备份 {}）",
                backup.display()
            ));
            notes.push(format!(
                "已停用旧链路任务 {LEGACY_TASK_NAME}（定义备份 {}）",
                backup.display()
            ));
        } else {
            notes.push(format!(
                "旧链路任务 {LEGACY_TASK_NAME} 已处于停用状态，未改动"
            ));
        }
    }

    // 2) 建 / 改自己的任务。
    let definition = build_definition(&scheduler, exe)?;
    let registered = unsafe {
        scheduler.folder.RegisterTaskDefinition(
            &BSTR::from(task_name),
            &definition,
            TASK_CREATE_OR_UPDATE.0,
            &VARIANT::default(),
            &VARIANT::default(),
            TASK_LOGON_INTERACTIVE_TOKEN,
            &VARIANT::default(),
        )
    }
    .map_err(|err| format!("注册任务 {TASK_NAME} 失败：{err}"))?;
    if !is_enabled(&registered) {
        // 手工停用过同名任务时，重新注册可能继承停用状态，显式打开。
        set_enabled(&registered, true)
            .map_err(|err| format!("启用任务 {TASK_NAME} 失败：{err}"))?;
    }
    diag::info(&format!(
        "自启动：已创建 / 更新任务 {task_name}（{} {}）",
        exe.display(),
        TASK_ARGUMENTS
    ));
    notes.push(format!(
        "已创建 / 更新任务 {task_name}：{} {}（登录时运行，15 秒延迟）",
        exe.display(),
        TASK_ARGUMENTS
    ));
    Ok(notes.join("；"))
}

/// 停用自启动（回滚）：删掉自己的任务；旧链路若被我们停用过（有备份）就重新启用。
pub fn disable(data_dir: &Path) -> Result<String, String> {
    disable_named(data_dir, TASK_NAME)
}

/// 按任务名停用（`TASK_NAME` 之外的名字只给单测 / 自检用）。
pub fn disable_named(data_dir: &Path, task_name: &str) -> Result<String, String> {
    let scheduler = connect()?;
    let mut notes: Vec<String> = Vec::new();

    match lookup(&scheduler, task_name)? {
        Some(_) => {
            // 只删给定的任务名；DeleteTask 不接受通配符，不会误伤别的任务。
            unsafe { scheduler.folder.DeleteTask(&BSTR::from(task_name), 0) }
                .map_err(|err| format!("删除任务 {task_name} 失败：{err}"))?;
            diag::info(&format!("自启动：已删除任务 {task_name}"));
            notes.push(format!("已删除任务 {task_name}"));
        }
        None => notes.push(format!("任务 {task_name} 本来就不存在")),
    }

    if let Some(legacy) = lookup(&scheduler, LEGACY_TASK_NAME)? {
        if has_backup(data_dir) {
            if is_enabled(&legacy) {
                notes.push(format!("旧链路任务 {LEGACY_TASK_NAME} 已是启用状态"));
            } else {
                set_enabled(&legacy, true)
                    .map_err(|err| format!("重新启用旧链路任务 {LEGACY_TASK_NAME} 失败：{err}"))?;
                diag::info(&format!("自启动：已恢复旧链路任务 {LEGACY_TASK_NAME}"));
                notes.push(format!("已恢复旧链路任务 {LEGACY_TASK_NAME}"));
            }
        } else {
            notes.push(format!(
                "旧链路任务 {LEGACY_TASK_NAME} 存在，但不是本工具停用的，未改动"
            ));
        }
    }

    Ok(notes.join("；"))
}

/// 有没有「我们停用过旧链路」的证据：`<数据目录>\backup\dsh-start-*.xml`。
fn has_backup(data_dir: &Path) -> bool {
    let dir = data_dir.join(store::BACKUP_DIR_NAME);
    fs::read_dir(dir)
        .map(|entries| {
            entries.filter_map(Result::ok).any(|entry| {
                entry
                    .file_name()
                    .to_string_lossy()
                    .starts_with(&format!("{LEGACY_TASK_NAME}-"))
            })
        })
        .unwrap_or(false)
}

// ── COM 胶水 ──

fn connect() -> Result<Scheduler, String> {
    ensure_com()?;
    // SAFETY: 参数都是 COM 约定的空 VARIANT（连接本机、用当前凭据）。
    unsafe {
        let service: ITaskService = CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER)
            .map_err(|err| format!("创建任务计划服务失败：{err}"))?;
        service
            .Connect(
                &VARIANT::default(),
                &VARIANT::default(),
                &VARIANT::default(),
                &VARIANT::default(),
            )
            .map_err(|err| format!("连接任务计划服务失败：{err}"))?;
        let folder = service
            .GetFolder(&BSTR::from("\\"))
            .map_err(|err| format!("打开任务计划根目录失败：{err}"))?;
        Ok(Scheduler { service, folder })
    }
}

/// 每个线程一次性 `CoInitializeEx`（MTA）。
///
/// 不做 `CoUninitialize`：core 的线程都是长生命周期，且 HTTP 线程上可能还有别的 COM 使用者；
/// 进程退出时系统会统一收尾。返回 `RPC_E_CHANGED_MODE` 表示该线程已用别的单元模型初始化过，
/// 继续用即可（我们只调进程内 COM 服务，不依赖单元类型）。
fn ensure_com() -> Result<(), String> {
    use std::cell::Cell;
    thread_local! {
        static INITIALIZED: Cell<bool> = const { Cell::new(false) };
    }
    INITIALIZED.with(|initialized| {
        if initialized.get() {
            return Ok(());
        }
        // SAFETY: 无参；返回 S_FALSE（已初始化）与 RPC_E_CHANGED_MODE 都算可用。
        let hr = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) };
        if hr.is_err() && hr != RPC_E_CHANGED_MODE {
            return Err(format!("初始化 COM 失败（CoInitializeEx 0x{:08X}）", hr.0));
        }
        initialized.set(true);
        Ok(())
    })
}

/// 按名字取任务；`ERROR_FILE_NOT_FOUND` 映射成 `Ok(None)`。
fn lookup(scheduler: &Scheduler, name: &str) -> Result<Option<IRegisteredTask>, String> {
    // SAFETY: 名字转 BSTR 由调用方持有；返回的接口句柄由 windows crate 管理生命周期。
    unsafe {
        match scheduler.folder.GetTask(&BSTR::from(name)) {
            Ok(task) => Ok(Some(task)),
            // 0x80070002 = HRESULT_FROM_WIN32(ERROR_FILE_NOT_FOUND)
            Err(err) if err.code().0 as u32 == 0x8007_0002 => Ok(None),
            Err(err) => Err(format!("查询任务 {name} 失败：{err}")),
        }
    }
}

fn is_enabled(task: &IRegisteredTask) -> bool {
    // SAFETY: 只读一个布尔属性。
    unsafe { task.Enabled() }
        .map(|value: VARIANT_BOOL| value.0 != 0)
        .unwrap_or(false)
}

fn set_enabled(task: &IRegisteredTask, enabled: bool) -> Result<(), String> {
    let flag = if enabled { VARIANT_TRUE } else { VARIANT_FALSE };
    // SAFETY: 布尔属性写操作；flag 在调用期间存活。
    unsafe { task.SetEnabled(flag) }.map_err(|err| err.to_string())
}

/// 任务现状 → [`TaskStatus`]（比较 Action 的 exe 路径）。
fn describe(task: &IRegisteredTask, exe: &Path) -> TaskStatus {
    let action = task_action(task);
    let matches_us = action
        .as_deref()
        .map(|path| paths_equal(path, exe))
        .unwrap_or(false);
    let enabled = is_enabled(task);
    let mut status = TaskStatus::new(match (matches_us, enabled) {
        (false, _) => State::Foreign,
        (true, true) => State::Enabled,
        (true, false) => State::Disabled,
    });
    if !matches_us {
        status.detail = Some(format!(
            "同名任务的 Action 指向别处：{}",
            action.clone().unwrap_or_else(|| "（读不到）".to_string())
        ));
    } else if !enabled {
        status.detail = Some("任务存在但已被停用".to_string());
    }
    status.action = action;
    status
}

/// 读任务第一个 Exec Action 的路径（读不到返回 `None`）。
fn task_action(task: &IRegisteredTask) -> Option<String> {
    // SAFETY: 全部是只读属性；接口句柄由 windows crate 管理。
    unsafe {
        let definition = task.Definition().ok()?;
        let actions = definition.Actions().ok()?;
        let action = actions.get_Item(1).ok()?;
        let mut kind = TASK_ACTION_TYPE(0);
        action.Type(&mut kind).ok()?;
        if kind != TASK_ACTION_EXEC {
            return None;
        }
        let exec: IExecAction = action.cast().ok()?;
        let mut path = BSTR::new();
        exec.Path(&mut path).ok()?;
        Some(path.to_string())
    }
}

/// 读任务的完整 XML（备份用）。
fn task_xml(task: &IRegisteredTask) -> Result<String, String> {
    // SAFETY: 只读属性；BSTR 由调用方持有。
    unsafe {
        let definition = task
            .Definition()
            .map_err(|err| format!("读取任务定义失败：{err}"))?;
        let mut xml = BSTR::new();
        definition
            .XmlText(&mut xml)
            .map_err(|err| format!("导出任务 XML 失败：{err}"))?;
        Ok(xml.to_string())
    }
}

/// 把旧链路任务的定义备份到 `<数据目录>\backup\<name>-<unix秒>.xml`。
fn backup_task_xml(task: &IRegisteredTask, data_dir: &Path) -> Result<PathBuf, String> {
    let xml = task_xml(task)?;
    let dir = data_dir.join(store::BACKUP_DIR_NAME);
    fs::create_dir_all(&dir)
        .map_err(|err| format!("创建备份目录失败（{}）：{err}", dir.display()))?;
    let stamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0);
    let path = dir.join(format!("{LEGACY_TASK_NAME}-{stamp}.xml"));
    fs::write(&path, xml).map_err(|err| format!("写备份文件失败（{}）：{err}", path.display()))?;
    Ok(path)
}

/// 组装任务定义（字段与 §3.12 的表一一对应）。
fn build_definition(scheduler: &Scheduler, exe: &Path) -> Result<ITaskDefinition, String> {
    let sid = current_user_sid()?;
    let exe_text = exe.to_string_lossy().to_string();
    let work_dir = exe
        .parent()
        .map(|dir| dir.to_string_lossy().to_string())
        .unwrap_or_default();

    // SAFETY: 全部是 COM 属性写操作；所有 BSTR / VARIANT 实参在调用期间存活。
    unsafe {
        let definition = scheduler
            .service
            .NewTask(0)
            .map_err(|err| format!("创建任务定义失败：{err}"))?;

        let info = definition
            .RegistrationInfo()
            .map_err(|err| format!("写任务描述失败：{err}"))?;
        info.SetAuthor(&BSTR::from("dsh-housing"))
            .map_err(|err| format!("写任务作者失败：{err}"))?;
        info.SetDescription(&BSTR::from(TASK_DESCRIPTION))
            .map_err(|err| format!("写任务描述失败：{err}"))?;

        let principal = definition
            .Principal()
            .map_err(|err| format!("写任务主体失败：{err}"))?;
        principal
            .SetUserId(&BSTR::from(sid.as_str()))
            .map_err(|err| format!("写任务主体用户失败：{err}"))?;
        // InteractiveToken：保住 logon SID（用户 2026-10-19 口径；否则 workspace-write 沙箱失效）。
        principal
            .SetLogonType(TASK_LOGON_INTERACTIVE_TOKEN)
            .map_err(|err| format!("设置 logon type 失败：{err}"))?;
        principal
            .SetRunLevel(TASK_RUNLEVEL_HIGHEST)
            .map_err(|err| format!("设置运行级别失败：{err}"))?;

        let triggers = definition
            .Triggers()
            .map_err(|err| format!("写触发器失败：{err}"))?;
        let trigger = triggers
            .Create(TASK_TRIGGER_LOGON)
            .map_err(|err| format!("创建登录触发器失败：{err}"))?;
        trigger
            .SetEnabled(VARIANT_TRUE)
            .map_err(|err| format!("启用登录触发器失败：{err}"))?;
        let logon: ILogonTrigger = trigger
            .cast()
            .map_err(|err| format!("登录触发器类型不对：{err}"))?;
        logon
            .SetUserId(&BSTR::from(sid.as_str()))
            .map_err(|err| format!("设置触发用户失败：{err}"))?;
        logon
            .SetDelay(&BSTR::from(LOGON_DELAY))
            .map_err(|err| format!("设置登录延迟失败：{err}"))?;

        let settings = definition
            .Settings()
            .map_err(|err| format!("写任务设置失败：{err}"))?;
        settings
            .SetEnabled(VARIANT_TRUE)
            .map_err(|err| format!("启用任务失败：{err}"))?;
        settings
            .SetStartWhenAvailable(VARIANT_TRUE)
            .map_err(|err| format!("设置「错过触发后补跑」失败：{err}"))?;
        settings
            .SetDisallowStartIfOnBatteries(VARIANT_FALSE)
            .map_err(|err| format!("设置电池策略失败：{err}"))?;
        settings
            .SetStopIfGoingOnBatteries(VARIANT_FALSE)
            .map_err(|err| format!("设置电池策略失败：{err}"))?;
        // PT0S = 不设运行时长上限：它是常驻进程，不是批处理。
        settings
            .SetExecutionTimeLimit(&BSTR::from("PT0S"))
            .map_err(|err| format!("设置运行时长上限失败：{err}"))?;
        settings
            .SetMultipleInstances(TASK_INSTANCES_IGNORE_NEW)
            .map_err(|err| format!("设置多实例策略失败：{err}"))?;
        settings
            .SetAllowDemandStart(VARIANT_TRUE)
            .map_err(|err| format!("设置「允许手动运行」失败：{err}"))?;
        let idle = settings
            .IdleSettings()
            .map_err(|err| format!("写空闲设置失败：{err}"))?;
        idle.SetStopOnIdleEnd(VARIANT_FALSE)
            .map_err(|err| format!("设置空闲结束行为失败：{err}"))?;
        idle.SetRestartOnIdle(VARIANT_FALSE)
            .map_err(|err| format!("设置空闲重启行为失败：{err}"))?;

        let actions = definition
            .Actions()
            .map_err(|err| format!("写任务动作失败：{err}"))?;
        let action = actions
            .Create(TASK_ACTION_EXEC)
            .map_err(|err| format!("创建 Exec 动作失败：{err}"))?;
        let exec: IExecAction = action
            .cast()
            .map_err(|err| format!("Exec 动作类型不对：{err}"))?;
        exec.SetPath(&BSTR::from(exe_text.as_str()))
            .map_err(|err| format!("设置动作程序失败：{err}"))?;
        exec.SetArguments(&BSTR::from(TASK_ARGUMENTS))
            .map_err(|err| format!("设置动作参数失败：{err}"))?;
        if !work_dir.is_empty() {
            exec.SetWorkingDirectory(&BSTR::from(work_dir.as_str()))
                .map_err(|err| format!("设置工作目录失败：{err}"))?;
        }

        Ok(definition)
    }
}

// ── 令牌 / SID ──

/// 当前进程的完整性级别（读不出来返回 `Unknown`，不 panic）。
pub fn current_integrity() -> IntegrityLevel {
    match token_buffer(TokenIntegrityLevel) {
        Some(buffer) => {
            // SAFETY: GetTokenInformation(TokenIntegrityLevel) 成功时缓冲区就是一个 TOKEN_MANDATORY_LABEL。
            let rid = unsafe {
                let label = &*(buffer.as_ptr() as *const TOKEN_MANDATORY_LABEL);
                last_sub_authority(label.Label.Sid)
            };
            match rid {
                Some(rid) => integrity_from_rid(rid),
                None => IntegrityLevel::Unknown,
            }
        }
        None => IntegrityLevel::Unknown,
    }
}

/// 当前用户的 SID 字符串（`S-1-5-21-...`），任务主体用它而不是 `机器名\用户`。
pub fn current_user_sid() -> Result<String, String> {
    let buffer = token_buffer(TokenUser)
        .ok_or_else(|| "读取当前用户令牌失败（GetTokenInformation TokenUser）".to_string())?;
    // SAFETY: 成功时缓冲区就是一个 TOKEN_USER，Sid 指向缓冲区内部的 SID。
    let sid = unsafe {
        let user = &*(buffer.as_ptr() as *const TOKEN_USER);
        user.User.Sid
    };
    let text = sid_text(sid);
    if text.is_empty() {
        return Err("当前用户的 SID 读不出来".to_string());
    }
    Ok(text)
}

/// 取当前进程令牌的某类信息（`GetTokenInformation` 两次调用：先问长度再取内容）。
fn token_buffer(class: windows::Win32::Security::TOKEN_INFORMATION_CLASS) -> Option<Vec<u8>> {
    // SAFETY: 令牌句柄由 OwnedHandle 持有到函数结束；缓冲区大小就是 API 自己给的长度。
    unsafe {
        let mut raw = HANDLE::default();
        OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut raw).ok()?;
        let token = OwnedHandle::from_raw_handle(raw.0);

        let mut length = 0u32;
        let _ = GetTokenInformation(
            HANDLE(token.as_raw_handle()),
            class,
            None,
            0,
            &mut length as *mut u32,
        );
        if length == 0 {
            return None;
        }
        let mut buffer = vec![0u8; length as usize];
        GetTokenInformation(
            HANDLE(token.as_raw_handle()),
            class,
            Some(buffer.as_mut_ptr() as *mut c_void),
            length,
            &mut length as *mut u32,
        )
        .ok()?;
        Some(buffer)
    }
}

/// SID 的最后一个子权威 = 完整性级别 RID。
fn last_sub_authority(sid: PSID) -> Option<u32> {
    if sid.0.is_null() {
        return None;
    }
    // SAFETY: 传入的是 API 返回的合法 SID；子权威个数由 SID 自己给。
    unsafe {
        let count = *GetSidSubAuthorityCount(sid);
        if count == 0 {
            return None;
        }
        Some(*GetSidSubAuthority(sid, u32::from(count) - 1))
    }
}

/// 把 SID 结构拼成 `S-1-5-21-…`（不用 `ConvertSidToStringSidW`，省一个 feature 与一次 LocalFree）。
fn sid_text(sid: PSID) -> String {
    if sid.0.is_null() {
        return String::new();
    }
    // SAFETY: 传入的是 API 返回的合法 SID；子权威逐个用 GetSidSubAuthority 读。
    unsafe {
        let raw = sid.0 as *const windows::Win32::Security::SID;
        if raw.is_null() {
            return String::new();
        }
        let revision = (*raw).Revision;
        let authority = (*raw).IdentifierAuthority.Value;
        let mut text = format!("S-{revision}-{}", authority_value(authority));
        let count = *GetSidSubAuthorityCount(sid);
        for index in 0..u32::from(count) {
            text.push('-');
            text.push_str(&(*GetSidSubAuthority(sid, index)).to_string());
        }
        text
    }
}

/// 6 字节大端标识权威 → 十进制（`NT AUTHORITY` = 5）。
fn authority_value(bytes: [u8; 6]) -> u64 {
    bytes
        .iter()
        .fold(0u64, |acc, byte| (acc << 8) | u64::from(*byte))
}

/// 路径比较：忽略大小写、`/` 与 `\`、首尾引号（任务计划里存的是原始字符串）。
fn paths_equal(left: &str, right: &Path) -> bool {
    let normalize = |text: &str| {
        text.trim()
            .trim_matches('"')
            .replace('/', "\\")
            .to_ascii_lowercase()
    };
    normalize(left) == normalize(&right.to_string_lossy())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn integrity_levels_map_by_rid() {
        assert_eq!(integrity_from_rid(0x0000), IntegrityLevel::Untrusted);
        assert_eq!(integrity_from_rid(0x1000), IntegrityLevel::Low);
        assert_eq!(integrity_from_rid(0x1100), IntegrityLevel::Low);
        assert_eq!(integrity_from_rid(0x2000), IntegrityLevel::Medium);
        assert_eq!(integrity_from_rid(0x2100), IntegrityLevel::Medium);
        assert_eq!(integrity_from_rid(0x3000), IntegrityLevel::High);
        assert_eq!(integrity_from_rid(0x4000), IntegrityLevel::System);
        assert!(IntegrityLevel::Medium.is_viable());
        assert!(IntegrityLevel::High.is_viable());
        assert!(!IntegrityLevel::Low.is_viable(), "Low 起不了托盘");
    }

    #[test]
    fn paths_compare_case_and_separator_insensitively() {
        let exe = Path::new(r"D:\Tools\dsh-housing\dsh-housing.exe");
        assert!(paths_equal(r"D:\Tools\dsh-housing\dsh-housing.exe", exe));
        assert!(paths_equal(r"d:/tools/DSH-HOUSING/dsh-housing.EXE", exe));
        assert!(paths_equal(
            r#""D:\Tools\dsh-housing\dsh-housing.exe""#,
            exe
        ));
        assert!(!paths_equal(r"D:\Tools\other\dsh-housing.exe", exe));
        assert!(!paths_equal("", exe));
    }

    #[test]
    fn authority_value_is_big_endian() {
        assert_eq!(authority_value([0, 0, 0, 0, 0, 5]), 5);
        assert_eq!(authority_value([0, 0, 0, 0, 0, 0]), 0);
    }

    #[test]
    fn sid_text_formats_a_local_sid() {
        // 手工拼一个 SID：revision 1 / authority 5 / sub 21, 1000 → S-1-5-21-1000
        let subs: [u32; 2] = [21, 1000];
        let mut buffer = vec![0u8; 8 + subs.len() * 4];
        buffer[0] = 1; // Revision
        buffer[1] = subs.len() as u8; // SubAuthorityCount
        buffer[7] = 5; // IdentifierAuthority（大端 6 字节）
        for (index, value) in subs.iter().enumerate() {
            let offset = 8 + index * 4;
            buffer[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
        }
        let sid = PSID(buffer.as_mut_ptr() as *mut c_void);
        assert_eq!(sid_text(sid), "S-1-5-21-1000");
    }

    #[test]
    fn current_integrity_is_readable_and_current_sid_looks_like_a_sid() {
        // 本机实测环境一定读得出来；读不出来说明令牌 API 用法坏了，应当立刻发现。
        let level = current_integrity();
        assert_ne!(level, IntegrityLevel::Unknown, "完整性级别读不出来");
        let sid = current_user_sid().expect("当前用户 SID 应可读");
        assert!(sid.starts_with("S-1-5-"), "SID 形状不对：{sid}");
    }
}

/// 自检（默认不跑）：真实建一个**临时任务名**、读到它、再删掉。
///
/// 任务名带 PID，所以即使有人在开着真实自启动的机器上跑 `cargo test -- --ignored`，
/// 也只会碰这条一次性任务。跑法：
/// `cargo test -p dsh-housing-core -- --ignored autostart_roundtrip_smoke --nocapture`
#[test]
#[ignore = "会真的创建 / 删除一个任务计划（临时任务名），手动跑"]
fn autostart_roundtrip_smoke() {
    let name = format!("{TASK_NAME}-selftest-{}", std::process::id());
    let exe = std::env::current_exe().expect("当前测试 exe 路径");
    let data_dir = std::env::temp_dir().join("dsh-housing-autostart-selftest");

    let before = status_named(&exe, &name);
    assert_eq!(before.state, State::Missing, "自检任务不该预先存在");

    let notice = enable_named(&exe, &data_dir, &name).expect("启用自检任务");
    println!("enable → {notice}");

    let after = status_named(&exe, &name);
    assert_eq!(
        after.state,
        State::Enabled,
        "启用后应读到 enabled：{after:?}"
    );
    assert!(
        after
            .action
            .as_deref()
            .map(|path| paths_equal(path, &exe))
            .unwrap_or(false),
        "任务 Action 应指向测试 exe：{:?}",
        after.action
    );

    // 把定义打出来，方便人工核对 Principal / Trigger / Settings（verify-m4 里还有逐字段断言）。
    {
        let scheduler = connect().expect("连接任务计划");
        let task = lookup(&scheduler, &name)
            .expect("查询自检任务")
            .expect("自检任务应存在");
        println!("task xml:\n{}", task_xml(&task).expect("导出任务 XML"));
    }

    let notice = disable_named(&data_dir, &name).expect("删除自检任务");
    println!("disable → {notice}");
    assert_eq!(
        status_named(&exe, &name).state,
        State::Missing,
        "停用后任务应消失"
    );
}
