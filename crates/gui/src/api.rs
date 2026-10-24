//! 控制台 API 的数据形状与后端 trait（docs/structure.md §3.17）。
//!
//! 为什么这里不直接用 core 的 `model::StatusSnapshot`：依赖方向是 `core → gui`，
//! gui 不能反过来引用 core。所以：
//!
//! - 实例快照用 `serde_json::Value` 原样透传（它本来就是跨进程的线上格式）；
//! - **登记表视图**（胶囊 / env / alias / warnings）在 gui 里定义类型，core 负责映射。
//!
//! `ConsoleBackend` 是 gui 与 core 之间唯一的接口：core 实现它，HTTP 层只调它。
//! 所有方法都必须是**同步且快速**的（core 侧只是读内存快照 / 写一个小 json），
//! 因为 handler 跑在 `gui` 的单个 tokio 线程上。

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// 日志尾部默认 / 上限字节数（§5.8 D8）。
pub const DEFAULT_LOG_TAIL: usize = 64 * 1024;
pub const MAX_LOG_TAIL: usize = 256 * 1024;

/// core 提供给控制台的全部能力（一个动作一个 owner：GUI 与托盘走同一套 core 方法）。
pub trait ConsoleBackend: Send + Sync {
    /// 整份状态：运行时快照 + 登记表视图 + 控制台自身信息。
    fn state(&self) -> ConsoleState;
    /// 按登记项启动实例。
    fn start(&self, key: &str) -> ActionResult;
    /// 停止实例（优雅停 → 超时强杀，由 core 的停止线程负责）。
    fn stop(&self, key: &str) -> ActionResult;
    /// 打开该实例的 dsh Web UI（没就绪时回 `ok` + notice，与托盘一致）。
    fn open(&self, key: &str) -> ActionResult;
    /// 实例日志尾部：运行中读内存环形缓冲，已停止读最新日志文件（§5.8 D8）。
    fn logs(&self, key: &str, tail: usize) -> Result<LogPayload, BackendError>;
    /// 全量替换实例级 env（§3.18；重启实例后生效）。
    fn set_env(&self, key: &str, env: BTreeMap<String, String>) -> ActionResult;
    /// 改实例显示名（空串 = 清除）。
    fn set_alias(&self, key: &str, alias: Option<String>) -> ActionResult;
    /// 登记 / 刷新一个胶囊目录（与托盘「登记胶囊…」同一条路径）。
    fn register(&self, root: &str) -> ActionResult;
    /// 移除一个胶囊（`confirm` 必须等于 `capsule_id`；有实例在跑则拒绝）。
    fn remove_capsule(&self, capsule_id: &str, confirm: &str) -> ActionResult;
    /// 改壳设置（M4，§2.4：范围校验 → 原子写盘 → 换内存副本）。
    fn set_settings(&self, settings: SettingsRequest) -> ActionResult;
    /// 改实例端口（M4，§3.19：真源是登记表；运行中拒绝）。
    fn set_instance_port(&self, key: &str, port: u16) -> ActionResult;
    /// 改实例 host（v0.3，§3.22：空串 = 清除、回到跟随胶囊；运行中拒绝）。
    fn set_instance_host(&self, key: &str, host: &str) -> ActionResult;
    /// 实例级自启开关（M4，§3.12：写 `capsules.json`，下次 core 启动生效）。
    fn set_instance_autostart(&self, key: &str, autostart: bool) -> ActionResult;
    /// 启用登录自启动（M4，§3.12：建 / 改任务计划 `dsh-housing`）。
    fn enable_autostart(&self) -> ActionResult;
    /// 停用登录自启动并回滚（M4，§3.12：删任务 + 恢复旧链路）。
    fn disable_autostart(&self) -> ActionResult;
    /// 全量替换 webhook 配置（v0.2 / #244，§3.21：校验 → 原子写盘 → 即时生效）。
    fn set_webhooks(&self, webhooks: Vec<WebhookView>) -> ActionResult;
    /// 起一次可移植版本构建（v0.5 / #249，§3.24）：已有构建在跑 → `409`；参数非法 → `400`。
    fn builder_start(&self, request: BuilderStartRequest) -> ActionResult;
    /// 取消进行中的构建（`TerminateJobObject`）；没有在跑 → `409`。
    fn builder_cancel(&self) -> ActionResult;
    /// 构建日志尾部（进行中读环形缓冲，结束后读日志文件）。
    fn builder_log(&self, tail: usize) -> Result<LogPayload, BackendError>;
    /// 版本目录（缓存优先；`refresh` 只触发**后台**重取，接口本身立刻返回）。
    fn builder_versions(&self, refresh: bool) -> BuilderVersionsView;
}

/// `/api/state` 的响应：运行时快照 + 登记表视图 + 控制台信息 + 壳设置 + 自启动现状 + webhook。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ConsoleState {
    /// `model::StatusSnapshot` 的 JSON 形态（含全部实例的实时状态 / metrics）。
    pub status: serde_json::Value,
    /// 登记表视图（胶囊配置、env、alias、warnings）。
    pub registry: RegistryView,
    /// 控制台自身（目前只有端口；token 绝不出现在这里）。
    pub console: ConsoleInfo,
    /// 壳设置的当前值 + 只读信息（M4，§2.4）。
    pub settings: SettingsView,
    /// 登录自启动的任务现状（M4，§3.12）。
    pub autostart: AutostartView,
    /// webhook 配置与「新建一条时用什么默认模板」（v0.2 / #244，§3.21）。
    pub webhooks: WebhookSection,
    /// 构建任务快照（v0.5 / #249，§3.24）。
    pub builder: BuilderView,
}

/// 构建器视图（`state.builder`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuilderView {
    /// `idle` / `running` / `registering` / `done` / `failed` / `canceled`。
    pub phase: String,
    /// 当前步骤（`skeleton` / `runtime` / `app` / `templates` / `manifest` / `verify`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub step: Option<String>,
    /// 当前步骤的人话标题。
    pub step_title: String,
    /// 第几步（1-based；没有在跑时为 0）。
    pub step_index: u8,
    /// 一共几步（固定 6）。
    pub step_total: u8,
    /// 这次构建的输入（没有在跑 / 没跑过时为 `None`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<BuilderRequestView>,
    /// 开始时间（Unix 秒）。
    pub started_at: u64,
    /// 已用毫秒（结束后是总耗时）。
    pub elapsed_ms: u64,
    /// 构建子进程 PID（`npm install` 期间有用）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// 失败原因（人话）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// 成功后的产物信息。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<BuilderResultView>,
    /// 本次构建的日志文件（绝对路径，只读展示）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub log_path: Option<String>,
    /// 日志来源：`ring`（进行中）/ `file`（已结束）/ `none`。
    pub log_source: String,
    /// 上一次构建的结局（面板重开时能看到历史一句）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<BuilderLastView>,
}

/// 构建输入的可读投影。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuilderRequestView {
    pub name: String,
    pub dsh: String,
    pub node: String,
    pub profile: String,
    pub port: u16,
    pub root: String,
    pub overwrite: bool,
    pub dry_run: bool,
}

/// 构建产物的可读投影。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuilderResultView {
    pub capsule_id: String,
    /// 是否已经自动登记进「管理 dsh 版本」。
    pub registered: bool,
    pub dsh_version: String,
    pub node_version: String,
    pub capsule_root: String,
}

/// 上一次构建的结局。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuilderLastView {
    pub name: String,
    pub phase: String,
    pub elapsed_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// 版本目录视图（`GET /api/builder/versions`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BuilderVersionsView {
    /// 抓取时间（Unix 秒；0 = 从未抓到）。
    pub fetched_at: u64,
    /// 缓存是否过时（抓取失败时沿用旧缓存，这里为 `true`）。
    pub stale: bool,
    pub dsh_tags: BTreeMap<String, String>,
    pub dsh_versions: Vec<String>,
    pub node_versions: Vec<String>,
    /// 面板默认值（core 按已登记胶囊算出来的）。
    pub default_root: String,
    pub default_profile: String,
    /// 可选的 profile（dsh 的 shipped 模板）。
    pub profiles: Vec<String>,
    /// 面板默认的 npm registry。
    pub registry: String,
}

/// webhook 区块：已配置条目 + 默认模板 + 可用占位符。
///
/// 默认模板与占位符表由 core 从 `crates/webhook` 带过来，**前端不复制这两份常量**。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebhookSection {
    /// `webhooks.json` 的绝对路径（只读展示）。
    pub file_path: String,
    /// 已配置的 webhook（顺序就是 UI 顺序）。
    pub items: Vec<WebhookView>,
    /// 新建一条 webhook 时三个条件各用什么模板。
    pub defaults: TriggerTemplates,
    /// 模板里可用的占位符（例如 `{local_url}`），UI 直接列出来。
    pub placeholders: Vec<String>,
}

/// 四个条件的默认模板。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TriggerTemplates {
    pub start: String,
    pub stop: String,
    pub crashed: String,
    /// v0.3 / #259：dsh-housing 启动、控制台就绪时。
    pub housing_start: String,
}

/// 一条 webhook 的配置视图（也是 `PUT /api/webhooks` 的条目形状）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebhookView {
    /// 请求地址（`http://` / `https://`）。
    pub url: String,
    /// 整条启用 / 静音。
    pub enabled: bool,
    /// 实例就绪。
    pub start: TriggerView,
    /// 实例停止。
    pub stop: TriggerView,
    /// 进程自行退出。
    pub crashed: TriggerView,
    /// dsh-housing 启动（v0.3 / #259）。旧客户端不带这个字段时，core 按「默认模板 + 启用」补齐。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub housing_start: Option<TriggerView>,
}

/// 一个触发条件的配置视图。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TriggerView {
    pub enabled: bool,
    /// 请求正文模板（`{...}` 占位符在投递时替换）。
    pub template: String,
}

/// 壳设置视图（M4）：持久化字段 + 「我在哪、我是谁」这类只读信息。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingsView {
    pub version: u32,
    pub gui_port: u16,
    /// v0.4 / #286：控制台绑定地址（`127.0.0.1` / `0.0.0.0` / 本机具体 IPv4）。
    pub console_host: String,
    pub sample_interval_ms: u64,
    pub auto_open_browser: bool,
    /// v0.3 / #251：core 启动、控制台就绪后自动打开控制台页面（改后要重启 core 生效）。
    pub auto_open_console: bool,
    pub tray_restart_max: u8,
    pub tray_restart_backoff_ms: Vec<u64>,
    pub log_retention_days: u16,
    /// 探测到的本机默认路由 IPv4（只读；控制台「使用局域网地址」快捷键用）。没有则为 `null`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lan_ip: Option<String>,
    /// `settings.json` 的绝对路径（只读展示，省得用户找文件）。
    pub settings_path: String,
    /// 当前运行的 exe 路径。
    pub exe_path: String,
    /// 本进程完整性级别：`low` / `medium` / `high` / `system` / `unknown`（§5.4）。
    pub exe_integrity: String,
    /// 当前实际控制台端口（`0` = 控制台不可用）。
    pub console_port: u16,
    /// core 自己的 PID（与实例 PID 区分开）。
    pub core_pid: u32,
}

/// 自启动视图（M4，§3.12）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AutostartView {
    /// 任务名（固定 `dsh-housing`）。
    pub task_name: String,
    /// `enabled` / `disabled` / `missing` / `foreign` / `error`。
    pub state: String,
    /// 人话补充（Foreign 时指向哪里、Error 时是什么错）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// 旧链路任务（`dsh-start`）的现状：`None` = 不存在。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub legacy: Option<String>,
    /// 是否满足启用前提（完整性级别 ≥ Medium）；不满足时由前端显示原因。
    pub can_enable: bool,
}

/// 控制台自身信息。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConsoleInfo {
    pub port: u16,
}

/// 登记表视图：`capsules.json` 的可读投影（不暴露文件内部字段以外的任何东西）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegistryView {
    /// `.dsh-housing\` 的绝对路径（只读展示，前端用来解释日志位置）。
    pub data_dir: String,
    pub capsules: Vec<CapsuleView>,
}

/// 一个已登记胶囊。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapsuleView {
    pub capsule_id: String,
    pub root: String,
    pub alias: Option<String>,
    pub schema: u32,
    pub registered_at: u64,
    pub checked_at: u64,
    pub runtime: Option<RuntimeView>,
    pub app: Option<AppView>,
    /// 登记时的告警（跨盘、相对 workspace、未知 schema…）——只提示，不阻断。
    pub warnings: Vec<String>,
    pub instances: Vec<InstanceConfigView>,
}

/// 胶囊运行时的展示信息（`runtime{}`，只展示、不驱动启动，§5.7 D2）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeView {
    pub kind: String,
    pub version: String,
}

/// 胶囊 app 的展示信息（`app{}`，只展示）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppView {
    pub package: String,
    pub version: String,
    pub bin: String,
}

/// 一个登记实例的配置（可按实例编辑的部分）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceConfigView {
    pub profile: String,
    pub port: u16,
    /// v0.3 / §3.22：显式绑定的 host（`None` = 跟随胶囊 profile 配置，不传 `--host`）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    pub workspace: String,
    /// 有效显示名：用户设的别名，没设过时是派生的 `Deepseek Harness <version> <profile>`
    /// （§3.20；派生在 core，接口形状不变，这个字段总有值）。
    pub alias: Option<String>,
    /// M4 用；M3 只展示（自启动改造在 M4）。
    pub autostart: bool,
    /// 实例级 env 覆盖（最后叠加到启动器复刻值之上）。
    pub env: BTreeMap<String, String>,
}

/// 一次动作的结果。`status` 只用于 HTTP 状态码，不进 JSON 响应体。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ActionResult {
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// 动作之后的完整状态（前端拿它立即刷新，不必再等下一轮轮询）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub state: Option<ConsoleState>,
    /// HTTP 状态码：200 / 400 / 404 / 409。
    #[serde(skip)]
    pub status: u16,
}

impl ActionResult {
    /// 成功：带动作后的状态与一句人话。
    pub fn ok(state: ConsoleState, notice: impl Into<String>) -> Self {
        Self {
            ok: true,
            notice: Some(notice.into()),
            error: None,
            state: Some(state),
            status: 200,
        }
    }

    /// 成功但没有状态（例如只做了一次文件操作）。
    pub fn ok_message(notice: impl Into<String>) -> Self {
        Self {
            ok: true,
            notice: Some(notice.into()),
            error: None,
            state: None,
            status: 200,
        }
    }

    /// 失败：`status` 是 HTTP 语义（400 参数 / 404 不存在 / 409 冲突 / 500 内部）。
    pub fn fail(status: u16, message: impl Into<String>) -> Self {
        Self {
            ok: false,
            notice: None,
            error: Some(message.into()),
            state: None,
            status,
        }
    }
}

/// 日志尾部的来源：运行中 = `ring`，已停止 = `file`，都没有 = `none`。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LogPayload {
    pub text: String,
    pub source: String,
}

/// 带 HTTP 语义的失败：`404` 找不到实例 / 胶囊，`500` 读盘这类内部错误（§3.17 错误码口径）。
///
/// 为什么日志接口不像别的动作那样用 [`ActionResult`]：它的成功响应是 `text/plain`，
/// 不是 JSON；所以失败只用错误码 + 人话，不需要 `ok` / `state` 那套结构。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BackendError {
    pub status: u16,
    pub message: String,
}

impl BackendError {
    pub fn not_found(message: impl Into<String>) -> Self {
        Self {
            status: 404,
            message: message.into(),
        }
    }

    pub fn internal(message: impl Into<String>) -> Self {
        Self {
            status: 500,
            message: message.into(),
        }
    }
}

// ── 请求体（HTTP 层反序列化用） ──

/// `{ "key": "capsuleId/profile" }`。
#[derive(Debug, Clone, Deserialize)]
pub struct KeyRequest {
    pub key: String,
}

/// `{ "key": …, "env": { "K": "V" } }`（全量替换）。
#[derive(Debug, Clone, Deserialize)]
pub struct EnvRequest {
    pub key: String,
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

/// `{ "key": …, "alias": "名字" }`（空串 = 清除）。
#[derive(Debug, Clone, Deserialize)]
pub struct AliasRequest {
    pub key: String,
    #[serde(default)]
    pub alias: String,
}

/// `{ "root": "D:\\…\\capsule" }`。
#[derive(Debug, Clone, Deserialize)]
pub struct RegisterRequest {
    pub root: String,
}

/// `{ "capsule_id": …, "confirm": … }`。
#[derive(Debug, Clone, Deserialize)]
pub struct RemoveRequest {
    pub capsule_id: String,
    #[serde(default)]
    pub confirm: String,
}

/// `GET /api/instances/logs?key=&tail=`。
#[derive(Debug, Clone, Deserialize)]
pub struct LogsQuery {
    pub key: String,
    #[serde(default)]
    pub tail: Option<usize>,
}

/// `PUT /api/settings`：整份壳设置（§2.4）。`version` 不由前端给，core 保持原值。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SettingsRequest {
    pub gui_port: u16,
    /// v0.4 / #286：缺省（旧客户端）= 保留当前值。
    #[serde(default)]
    pub console_host: Option<String>,
    pub sample_interval_ms: u64,
    pub auto_open_browser: bool,
    /// v0.3：缺省（旧客户端）= 保留当前值。
    #[serde(default)]
    pub auto_open_console: Option<bool>,
    pub tray_restart_max: u8,
    #[serde(default)]
    pub tray_restart_backoff_ms: Vec<u64>,
    pub log_retention_days: u16,
}

/// `PUT /api/instances/port`：改实例端口（§3.19）。
#[derive(Debug, Clone, Deserialize)]
pub struct PortRequest {
    pub key: String,
    pub port: u16,
}

/// `PUT /api/instances/host`：改实例 host（v0.3 / §3.22；空串 = 清除）。
#[derive(Debug, Clone, Deserialize)]
pub struct HostRequest {
    pub key: String,
    #[serde(default)]
    pub host: String,
}

/// `PUT /api/instances/autostart`：实例级自启开关（§3.12）。
#[derive(Debug, Clone, Deserialize)]
pub struct AutostartRequest {
    pub key: String,
    pub autostart: bool,
}

/// `PUT /api/webhooks`：**全量替换** webhook 列表（§3.21；与 env 编辑同一套「提交你看到的全部」）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct WebhooksRequest {
    #[serde(default)]
    pub webhooks: Vec<WebhookView>,
}

/// `POST /api/builder/start`（v0.5 / #249，§3.24）：起一次构建。
///
/// 只有 `name` / `dsh` / `node` 是必填；`profile` 缺省 `web`、`port` 缺省由 core 挑、
/// `root` 缺省取已登记胶囊的公共父目录。
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct BuilderStartRequest {
    pub name: String,
    pub dsh: String,
    pub node: String,
    #[serde(default)]
    pub profile: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub root: Option<String>,
    #[serde(default)]
    pub registry: Option<String>,
    /// 目标目录已存在且非空时必须为 `true`。
    #[serde(default)]
    pub overwrite: bool,
    #[serde(default)]
    pub force_app: bool,
    /// 只打印计划：不联网、不写盘、不跑 npm。
    #[serde(default)]
    pub dry_run: bool,
}

/// `GET /api/builder/versions?refresh=1` 的 query。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct BuilderVersionsQuery {
    #[serde(default)]
    pub refresh: Option<u8>,
}

/// `GET /api/builder/log?tail=`（构建日志没有 key）。
#[derive(Debug, Clone, Default, Deserialize)]
pub struct BuilderLogQuery {
    #[serde(default)]
    pub tail: Option<usize>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn state() -> ConsoleState {
        ConsoleState {
            status: serde_json::json!({"instances": [], "shutting_down": false, "tray_running": true}),
            registry: RegistryView {
                data_dir: r"D:\Tools\dsh-housing\.dsh-housing".to_string(),
                capsules: Vec::new(),
            },
            console: ConsoleInfo { port: 39420 },
            settings: SettingsView {
                version: 1,
                gui_port: 0,
                console_host: "127.0.0.1".to_string(),
                sample_interval_ms: 3_000,
                auto_open_browser: false,
                auto_open_console: true,
                tray_restart_max: 3,
                tray_restart_backoff_ms: vec![1_000, 5_000, 15_000],
                log_retention_days: 14,
                lan_ip: Some("192.168.3.100".to_string()),
                settings_path: r"D:\Tools\dsh-housing\.dsh-housing\settings.json".to_string(),
                exe_path: r"D:\Tools\dsh-housing\dsh-housing.exe".to_string(),
                exe_integrity: "high".to_string(),
                console_port: 39420,
                core_pid: 4242,
            },
            autostart: AutostartView {
                task_name: "dsh-housing".to_string(),
                state: "missing".to_string(),
                detail: None,
                legacy: None,
                can_enable: true,
            },
            webhooks: WebhookSection {
                file_path: r"D:\Tools\dsh-housing\.dsh-housing\webhooks.json".to_string(),
                items: vec![WebhookView {
                    url: "https://ntfy.example.test/_dsh".to_string(),
                    enabled: true,
                    start: TriggerView {
                        enabled: true,
                        template: "Service Start\nName: {alias}".to_string(),
                    },
                    stop: TriggerView {
                        enabled: false,
                        template: "Service Stop\nName: {alias}".to_string(),
                    },
                    crashed: TriggerView {
                        enabled: true,
                        template: "Service Crashed\nName: {alias}".to_string(),
                    },
                    housing_start: Some(TriggerView {
                        enabled: true,
                        template: "Housing Start\nLocal Url: {local_url}".to_string(),
                    }),
                }],
                defaults: TriggerTemplates {
                    start: "Service Start\nName: {alias}".to_string(),
                    stop: "Service Stop\nName: {alias}".to_string(),
                    crashed: "Service Crashed\nName: {alias}".to_string(),
                    housing_start: "Housing Start\nLocal Url: {local_url}".to_string(),
                },
                placeholders: vec!["{time}".to_string(), "{alias}".to_string()],
            },
            builder: BuilderView {
                phase: "idle".to_string(),
                step: None,
                step_title: String::new(),
                step_index: 0,
                step_total: 6,
                request: None,
                started_at: 0,
                elapsed_ms: 0,
                pid: None,
                error: None,
                result: None,
                log_path: None,
                log_source: "none".to_string(),
                last: None,
            },
        }
    }

    #[test]
    fn partial_requests_keep_backward_compatible_defaults() {
        // 旧客户端不带 `auto_open_console` → core 保留当前值（不是静默改回默认）。
        let settings: SettingsRequest = serde_json::from_str(
            r#"{"gui_port":0,"sample_interval_ms":3000,"auto_open_browser":false,
                "tray_restart_max":3,"tray_restart_backoff_ms":[1000],"log_retention_days":14}"#,
        )
        .expect("旧请求体应能解析");
        assert_eq!(settings.auto_open_console, None);

        // 旧客户端不带 `housing_start` → core 按默认模板补齐（见 runtime 的 from_view）。
        let hook: WebhookView = serde_json::from_str(
            r#"{"url":"https://example.test/x","enabled":true,
                "start":{"enabled":true,"template":"s"},
                "stop":{"enabled":false,"template":""},
                "crashed":{"enabled":true,"template":"c"}}"#,
        )
        .expect("旧 webhook 条目应能解析");
        assert_eq!(hook.housing_start, None);
    }

    #[test]
    fn action_result_keeps_status_out_of_json() {
        let result = ActionResult::ok(state(), "已保存");
        let text = serde_json::to_string(&result).unwrap();
        assert!(text.contains("\"notice\":\"已保存\""));
        // `ActionResult.status` 是 HTTP 码，不进 JSON（嵌套的 state.status 是运行时快照，允许存在）。
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert!(value.get("status").is_none(), "{text}");
        assert_eq!(result.status, 200);

        let failure = ActionResult::fail(409, "有实例在运行");
        let text = serde_json::to_string(&failure).unwrap();
        assert!(text.contains("\"ok\":false"));
        let value: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert!(value.get("status").is_none(), "{text}");
        assert_eq!(failure.status, 409);
    }

    #[test]
    fn state_round_trips_through_json() {
        let text = serde_json::to_string(&state()).unwrap();
        let parsed: ConsoleState = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed, state());
    }
}
