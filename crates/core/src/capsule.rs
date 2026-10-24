//! 胶囊登记：读 `instance.json` → 校验 → 按 profile 展开（docs/structure.md §3.13）。
//!
//! # 边界
//!
//! - **只读 `<capsule>\instance.json` 一个文件**（硬约束 §1.3 第 2、3 条）：
//!   `capsule.lock.json` / `home\` / `scripts\` / `start*.cmd` 一律不读；
//! - **不写胶囊、不建目录、不搬运 workspace**；
//! - 校验失败就拒绝登记并给出人话原因；成功则产出 `CapsuleRecord`，由 `runtime.rs`
//!   合并进登记表、`store.rs` 落盘。
//!
//! # 为什么 `runtime{}` / `app{}` 只展示
//!
//! `instance.json` 里的 `app.bin` 如果决定「执行哪个脚本」，登记任意目录就等于给启动器
//! 加了一个「按胶囊里的路径执行 JS」的入口。本项目只适配 dsh（§1.2.1），路径布局固定在
//! `env.rs`，`runtime{}` / `app{}` 只用来展示版本（§5.7 D2）。
//!
//! 纯解析（`parse`）与 IO（`register`）分开：单测不需要真实胶囊目录。

use std::collections::BTreeMap;
use std::fmt;
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::model::{InstanceSpec, SCHEMA_VERSION};

/// 胶囊里启动器唯一会读的文件。
pub const INSTANCE_FILE_NAME: &str = "instance.json";

/// `instance.json` 的 `runtime{}`，只留展示需要的两字段。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeInfo {
    pub kind: String,
    pub version: String,
}

/// `instance.json` 的 `app{}`，只留展示需要的字段。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppInfo {
    pub package: String,
    pub version: String,
    /// `instance.json` 里写的是相对 `app\node_modules\<package>` 的路径（如 `lib/bin.js`）。
    pub bin: String,
}

/// 一个登记实例（`capsules.json` 的 `instances[]` 元素）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisteredInstance {
    /// profile 名（`web` / `chat` …）。
    pub profile: String,
    /// 启动器以登记值为准（不读 profile patch）。
    pub port: u16,
    /// 显式绑定的 host（v0.3 / §3.22）：`None` = 不传 `--host`、跟随胶囊 profile 配置。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// 子进程 cwd；也是 dsh 的「项目」根（§3.1）。
    pub workspace: PathBuf,
    /// 用户设的显示名（M2 没有编辑入口，M3 控制台补）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// 登录后是否自启（M4 用；M2 只存不用）。
    #[serde(default)]
    pub autostart: bool,
    /// 实例级环境变量覆盖，**最后**叠加到 `env.rs` 复刻值之上。
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
}

/// 一个胶囊的登记记录（`capsules.json` 的 `capsules[]` 元素）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapsuleRecord {
    /// 胶囊自己的标识（`instance.json` 的 `instanceId`），不是实例主键。
    pub capsule_id: String,
    /// canonical 路径（去掉 `\\?\` 前缀）；同一张表里不重复。
    pub root: PathBuf,
    /// 胶囊级别名（M2 无编辑入口）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    /// `instance.json` 的 `schema`（未知值只告警）。
    #[serde(default)]
    pub schema: u32,
    /// 首次登记时间（Unix 秒）。
    #[serde(default)]
    pub registered_at: u64,
    /// 最近一次登记 / 刷新时间（Unix 秒）。
    #[serde(default)]
    pub checked_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime: Option<RuntimeInfo>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub app: Option<AppInfo>,
    /// 登记时的告警（不同盘符、workspace 为相对路径等）——只提示，不阻断。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    /// 按 profile 展开的实例。
    pub instances: Vec<RegisteredInstance>,
}

/// `capsules.json` 的顶层形状。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CapsulesFile {
    #[serde(default = "default_version")]
    pub version: u32,
    #[serde(default)]
    pub capsules: Vec<CapsuleRecord>,
}

impl Default for CapsulesFile {
    fn default() -> Self {
        Self {
            version: SCHEMA_VERSION,
            capsules: Vec::new(),
        }
    }
}

impl CapsulesFile {
    /// 全部登记实例的启动规格（按登记顺序：胶囊 → 实例）。
    pub fn specs(&self) -> Vec<InstanceSpec> {
        let mut specs = Vec::new();
        for record in &self.capsules {
            for instance in &record.instances {
                if instance.port == 0 {
                    // 手改坏了的登记项：跳过，不让它变成「端口 0 的启动请求」。
                    continue;
                }
                specs.push(InstanceSpec {
                    capsule_root: record.root.clone(),
                    capsule_id: record.capsule_id.clone(),
                    profile: instance.profile.clone(),
                    port: instance.port,
                    host: instance.host.clone(),
                    workspace: instance.workspace.clone(),
                    env: instance.env.clone(),
                    alias: instance.alias.clone(),
                });
            }
        }
        specs
    }

    /// 按实例键（`capsuleId/profile`）找规格。
    pub fn find_spec(&self, key: &str) -> Option<InstanceSpec> {
        self.specs().into_iter().find(|spec| spec.key() == key)
    }

    /// 标记了「登录自启」的实例（M4，§3.12）：core 启动后按这个清单逐个拉起。
    ///
    /// 与 [`Self::specs`] 同一套过滤（端口为 `0` 的坏记录跳过），只多一个 `autostart` 条件。
    pub fn autostart_specs(&self) -> Vec<InstanceSpec> {
        let mut specs = Vec::new();
        for record in &self.capsules {
            for instance in &record.instances {
                if !instance.autostart || instance.port == 0 {
                    continue;
                }
                specs.push(InstanceSpec {
                    capsule_root: record.root.clone(),
                    capsule_id: record.capsule_id.clone(),
                    profile: instance.profile.clone(),
                    port: instance.port,
                    host: instance.host.clone(),
                    workspace: instance.workspace.clone(),
                    env: instance.env.clone(),
                    alias: instance.alias.clone(),
                });
            }
        }
        specs
    }
}

fn default_version() -> u32 {
    SCHEMA_VERSION
}

/// 登记失败的原因（都是数据问题，不涉及 Win32）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CapsuleError {
    context: &'static str,
    detail: String,
}

impl CapsuleError {
    fn new(context: &'static str, detail: impl Into<String>) -> Self {
        Self {
            context,
            detail: detail.into(),
        }
    }

    /// 卡住的那一步（诊断日志用）。
    pub fn context(&self) -> &'static str {
        self.context
    }
}

impl fmt::Display for CapsuleError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}：{}", self.context, self.detail)
    }
}

impl std::error::Error for CapsuleError {}

/// 登记 / 刷新一个胶囊目录（生产入口：会做 IO）。
pub fn register(root: &Path, data_dir: &Path) -> Result<CapsuleRecord, CapsuleError> {
    let root = canonical_dir(root)?;
    let instance_path = root.join(INSTANCE_FILE_NAME);
    let text = fs::read_to_string(&instance_path).map_err(|err| {
        CapsuleError::new(
            "读取 instance.json",
            format!("{}：{err}", instance_path.display()),
        )
    })?;
    let mut record = parse(&root, &text, data_dir)?;
    record.checked_at = now_unix();
    Ok(record)
}

/// 纯解析 + 校验 + 展开（不碰文件系统，单测直接用字符串喂）。
///
/// `root` 应是 canonical 路径；`data_dir` 只用于「是否同一盘符」的提示。
pub fn parse(root: &Path, text: &str, data_dir: &Path) -> Result<CapsuleRecord, CapsuleError> {
    let json: InstanceJson = serde_json::from_str(text)
        .map_err(|err| CapsuleError::new("解析 instance.json", err.to_string()))?;

    let capsule_id = non_empty(json.instance_id.as_deref())
        .ok_or_else(|| CapsuleError::new("校验 instance.json", "缺 instanceId"))?;

    if let Some(declared) = json.capsule_root.as_deref() {
        if !declared.trim().is_empty() && !paths_equal(Path::new(declared.trim()), root) {
            return Err(CapsuleError::new(
                "校验 instance.json",
                format!(
                    "capsuleRoot 与所选目录不一致（选了别的胶囊？）：instance.json={}，所选={}",
                    declared.trim(),
                    root.display()
                ),
            ));
        }
    }

    let mut warnings = Vec::new();
    let mut instances = Vec::new();

    let primary_profile = non_empty(json.profile.as_deref())
        .ok_or_else(|| CapsuleError::new("校验 instance.json", "缺 profile"))?;
    push_instance(
        &mut instances,
        &mut warnings,
        root,
        &primary_profile,
        json.port,
        json.workspace.as_deref(),
    )?;

    for extra in &json.additional_profiles {
        let profile = non_empty(extra.profile.as_deref()).ok_or_else(|| {
            CapsuleError::new(
                "校验 instance.json",
                "additionalProfiles[] 里有元素缺 profile",
            )
        })?;
        push_instance(
            &mut instances,
            &mut warnings,
            root,
            &profile,
            extra.port,
            extra.workspace.as_deref(),
        )?;
    }

    // 同胶囊内端口重复：只提示（两个 profile 抢一个端口时，第二个启动会被端口探测拦下）。
    let mut seen_ports: BTreeMap<u16, &str> = BTreeMap::new();
    for instance in &instances {
        if let Some(other) = seen_ports.insert(instance.port, &instance.profile) {
            warnings.push(format!(
                "端口 {} 被 {} 与 {} 同时使用（启动第二个时会被端口探测拦下）",
                instance.port, other, instance.profile
            ));
        }
    }

    if json.schema != 0 && json.schema != SCHEMA_VERSION {
        warnings.push(format!(
            "instance.json 的 schema={} 不是已知的 {}（dsh 仍是 preview，照常登记）",
            json.schema, SCHEMA_VERSION
        ));
    }

    record_drive_warning(&mut warnings, root, data_dir);
    for instance in &instances {
        if let Some(drive) = drive_of(&instance.workspace) {
            if Some(drive) != drive_of(data_dir) {
                warnings.push(format!(
                    "{} 的 workspace 不在 .dsh-housing 所在盘符：{}（只提示，不阻断）",
                    instance.profile,
                    instance.workspace.display()
                ));
            }
        }
    }

    let runtime = match json.runtime.as_ref() {
        Some(info) => match (
            non_empty(info.kind.as_deref()),
            non_empty(info.version.as_deref()),
        ) {
            (Some(kind), Some(version)) => Some(RuntimeInfo { kind, version }),
            _ => {
                warnings.push("instance.json 的 runtime{} 不完整，登记时忽略".to_string());
                None
            }
        },
        None => None,
    };
    let app = match json.app.as_ref() {
        Some(info) => match (
            non_empty(info.package.as_deref()),
            non_empty(info.version.as_deref()),
        ) {
            (Some(package), Some(version)) => Some(AppInfo {
                package,
                version,
                bin: non_empty(info.bin.as_deref()).unwrap_or_else(|| "lib/bin.js".to_string()),
            }),
            _ => {
                warnings.push("instance.json 的 app{} 不完整，登记时忽略".to_string());
                None
            }
        },
        None => None,
    };

    let stamp = now_unix();
    Ok(CapsuleRecord {
        capsule_id,
        root: root.to_path_buf(),
        alias: None,
        schema: json.schema,
        registered_at: stamp,
        checked_at: stamp,
        runtime,
        app,
        warnings,
        instances,
    })
}

/// 一个实例的登记校验（主 profile 与 `additionalProfiles[]` 共用）。
fn push_instance(
    instances: &mut Vec<RegisteredInstance>,
    warnings: &mut Vec<String>,
    root: &Path,
    profile: &str,
    port: Option<i64>,
    workspace: Option<&str>,
) -> Result<(), CapsuleError> {
    if instances.iter().any(|item| item.profile == profile) {
        return Err(CapsuleError::new(
            "校验 instance.json",
            format!("profile 重复：{profile}"),
        ));
    }

    let port = match port {
        Some(value) if (1..=65535).contains(&value) => value as u16,
        Some(value) => {
            return Err(CapsuleError::new(
                "校验 instance.json",
                format!("端口非法：{value}（{profile}）"),
            ))
        }
        None => {
            return Err(CapsuleError::new(
                "校验 instance.json",
                format!("缺端口（{profile}）"),
            ))
        }
    };

    let workspace_text = non_empty(workspace).ok_or_else(|| {
        CapsuleError::new("校验 instance.json", format!("缺 workspace（{profile}）"))
    })?;
    let workspace_path = PathBuf::from(&workspace_text);
    let workspace_path = if workspace_path.is_absolute() {
        workspace_path
    } else {
        warnings.push(format!(
            "{profile} 的 workspace 是相对路径（{workspace_text}），已按胶囊目录解析"
        ));
        root.join(workspace_path)
    };

    instances.push(RegisteredInstance {
        profile: profile.to_string(),
        port,
        host: None,
        workspace: workspace_path,
        alias: None,
        autostart: false,
        env: BTreeMap::new(),
    });
    Ok(())
}

/// 刷新登记的结果（给 IPC `notice` 用）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeOutcome {
    /// `true` = 新增胶囊；`false` = 刷新已有胶囊。
    pub created: bool,
    /// 给人看的一句话（托盘 / 日志）。
    pub notice: String,
}

/// 把新读到的记录合并进登记表。
///
/// 同一 `root` 视为**刷新**：`port` / `workspace` / 版本信息取新值，**实例级用户数据
/// （`alias` / `autostart` / `env`）保留**（§3.13）。返回变化摘要供日志与提示。
pub fn merge_record(file: &mut CapsulesFile, fresh: CapsuleRecord) -> MergeOutcome {
    let existing_index = file
        .capsules
        .iter()
        .position(|record| paths_equal(&record.root, &fresh.root));

    let Some(index) = existing_index else {
        let notice = format!(
            "已登记 {}（{} 个实例：{}）",
            fresh.capsule_id,
            fresh.instances.len(),
            profile_list(&fresh)
        );
        file.capsules.push(fresh);
        return MergeOutcome {
            created: true,
            notice,
        };
    };

    let existing = &mut file.capsules[index];
    let mut changes = Vec::new();
    let mut merged = fresh.clone();
    merged.registered_at = existing.registered_at;
    merged.alias = existing.alias.clone();
    for instance in &mut merged.instances {
        let Some(old) = existing
            .instances
            .iter()
            .find(|old| old.profile == instance.profile)
        else {
            changes.push(format!("新增 profile {}", instance.profile));
            continue;
        };
        instance.alias = old.alias.clone();
        instance.autostart = old.autostart;
        instance.env = old.env.clone();
        instance.host = old.host.clone();
        if old.port != instance.port {
            changes.push(format!(
                "{} 端口 {} → {}",
                instance.profile, old.port, instance.port
            ));
        }
        if old.workspace != instance.workspace {
            changes.push(format!(
                "{} workspace {} → {}",
                instance.profile,
                old.workspace.display(),
                instance.workspace.display()
            ));
        }
    }
    for old in &existing.instances {
        if !merged
            .instances
            .iter()
            .any(|instance| instance.profile == old.profile)
        {
            changes.push(format!("移除 profile {}", old.profile));
        }
    }

    let capsule_id = merged.capsule_id.clone();
    let count = merged.instances.len();
    *existing = merged;
    let notice = if changes.is_empty() {
        format!("已刷新登记：{capsule_id}（{count} 个实例，字段无变化）")
    } else {
        format!(
            "已刷新登记：{capsule_id}（{count} 个实例；{}）",
            changes.join("，")
        )
    };
    MergeOutcome {
        created: false,
        notice,
    }
}

fn profile_list(record: &CapsuleRecord) -> String {
    record
        .instances
        .iter()
        .map(|instance| instance.profile.as_str())
        .collect::<Vec<_>>()
        .join("、")
}

/// 实例的**默认显示名**：`Deepseek Harness <version> <profile>`（docs/structure.md §3.20）。
///
/// `version` 取登记记录 `app{}` 的 dsh 版本（如 `0.2.0-rc.2`）；`app{}` 缺失 / 不完整时
/// 退回 `capsule_id`，句子形状不变、也仍然能区分是哪个胶囊。
///
/// 这是**派生值、不落盘**：`capsules.json` 的 `alias` 只在用户显式设置过时才有值，
/// 派生点只有 `runtime::snapshot`（实例列表 / 托盘菜单）与 `runtime::registry_view`
/// （控制台登记表视图）两处；因此接口里的 `alias` 语义是「有效显示名」。
pub fn default_instance_alias(
    capsule_id: &str,
    app_version: Option<&str>,
    profile: &str,
) -> String {
    let version = app_version
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .unwrap_or(capsule_id);
    format!("Deepseek Harness {version} {profile}")
}

/// 目录必须存在；返回去掉 `\\?\` 前缀的 canonical 路径。
pub fn canonical_dir(path: &Path) -> Result<PathBuf, CapsuleError> {
    if !path.exists() {
        return Err(CapsuleError::new(
            "登记胶囊",
            format!("胶囊目录不存在：{}", path.display()),
        ));
    }
    let canonical = fs::canonicalize(path).map_err(|err| {
        CapsuleError::new(
            "登记胶囊",
            format!("解析胶囊目录失败：{}：{err}", path.display()),
        )
    })?;
    Ok(strip_verbatim(&canonical))
}

/// 去掉 Windows 的 `\\?\` 前缀（`\\?\UNC\srv\share` → `\\srv\share`）。
///
/// 存进 `capsules.json` 与展示都用不带前缀的形状：`\\?\` 只对 Win32 长路径 API 有意义，
/// 混在配置里会让人以为路径错了。
pub fn strip_verbatim(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        return PathBuf::from(rest);
    }
    path.to_path_buf()
}

/// 两个路径是否同一位置（忽略大小写与末尾分隔符）。
pub fn paths_equal(left: &Path, right: &Path) -> bool {
    let left = strip_verbatim(left);
    let right = strip_verbatim(right);
    let left = left.to_string_lossy();
    let right = right.to_string_lossy();
    trim_separators(&left).eq_ignore_ascii_case(trim_separators(&right))
}

fn trim_separators(text: &str) -> &str {
    text.trim_end_matches(['\\', '/'])
}

/// 盘符（或 UNC 前缀）；没有盘符（相对路径）返回 `None`。
fn drive_of(path: &Path) -> Option<String> {
    match path.components().next() {
        Some(Component::Prefix(prefix)) => Some(
            prefix
                .as_os_str()
                .to_string_lossy()
                .to_ascii_uppercase()
                .trim_end_matches(['\\', '/'])
                .to_string(),
        ),
        _ => None,
    }
}

/// 受控目录判据（§5.6 结论四）：只保留「是否同一盘符」这一条，只提示不阻断。
fn record_drive_warning(warnings: &mut Vec<String>, root: &Path, data_dir: &Path) {
    let root_drive = drive_of(root);
    let data_drive = drive_of(data_dir);
    let (Some(root_drive), Some(data_drive)) = (root_drive, data_drive) else {
        return;
    };
    if !root_drive.eq_ignore_ascii_case(&data_drive) {
        warnings.push(format!(
            "胶囊与 .dsh-housing 不在同一个盘符（{root_drive} ↔ {data_drive}）——只提示，不阻断"
        ));
    }
}

fn non_empty(text: Option<&str>) -> Option<String> {
    text.map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs())
        .unwrap_or(0)
}

// ── instance.json 的解析形状（只取启动器需要的字段；未知字段忽略） ──

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct InstanceJson {
    #[serde(default)]
    schema: u32,
    #[serde(default)]
    instance_id: Option<String>,
    #[serde(default)]
    capsule_root: Option<String>,
    #[serde(default)]
    profile: Option<String>,
    #[serde(default)]
    port: Option<i64>,
    #[serde(default)]
    workspace: Option<String>,
    #[serde(default)]
    additional_profiles: Vec<AdditionalProfileJson>,
    #[serde(default)]
    runtime: Option<RuntimeJson>,
    #[serde(default)]
    app: Option<AppJson>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AdditionalProfileJson {
    #[serde(default)]
    profile: Option<String>,
    #[serde(default)]
    port: Option<i64>,
    #[serde(default)]
    workspace: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeJson {
    #[serde(default)]
    kind: Option<String>,
    #[serde(default)]
    version: Option<String>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct AppJson {
    #[serde(default)]
    package: Option<String>,
    #[serde(default)]
    version: Option<String>,
    #[serde(default)]
    bin: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn data_dir() -> PathBuf {
        PathBuf::from(r"D:\Tools\dsh-housing\.dsh-housing")
    }

    fn capsule_root() -> PathBuf {
        PathBuf::from(r"D:\Images\DeepseekHarness\0.2.0-rc.2-default")
    }

    /// 与实机 `0.2.0-rc.2-default\instance.json` 同形状（去掉无关字段不影响解析）。
    fn instance_json() -> String {
        r#"{
          "schema": 1,
          "instanceId": "0.2.0-rc.2-default",
          "capsuleRoot": "D:\\Images\\DeepseekHarness\\0.2.0-rc.2-default",
          "runtime": { "kind": "node-portable-zip", "version": "24.21.0",
                       "archive": "node-v24.21.0-win-x64.zip",
                       "sha256": "158f76…", "source": "https://registry.npmmirror.com/",
                       "checksumVerified": true },
          "app": { "package": "@deepseek-ai/dsh", "version": "0.2.0-rc.2",
                   "bin": "lib/bin.js", "pnpm": "12.4.1" },
          "profile": "chat",
          "host": "0.0.0.0",
          "port": 39411,
          "workspace": "D:\\Images\\DeepseekHarness\\0.2.0-rc.2-default\\workspace",
          "patches": ["patches\\10-session-query-sqlite.yml"],
          "additionalProfiles": [
            { "profile": "web", "launcher": "start-web.cmd", "port": 39412,
              "workspace": "D:\\Images\\DeepseekHarness\\0.2.0-rc.2-default\\workspace-web",
              "patches": [], "note": "出厂默认档案" }
          ],
          "stateModel": "complete-instance-clone"
        }"#
        .to_string()
    }

    #[test]
    fn parses_real_capsule_and_expands_profiles() {
        let record = parse(&capsule_root(), &instance_json(), &data_dir()).expect("应能登记");
        assert_eq!(record.capsule_id, "0.2.0-rc.2-default");
        assert_eq!(record.root, capsule_root());
        assert_eq!(record.schema, 1);
        assert_eq!(
            record.runtime.as_ref().map(|info| info.version.as_str()),
            Some("24.21.0")
        );
        assert_eq!(
            record.app.as_ref().map(|info| info.bin.as_str()),
            Some("lib/bin.js")
        );
        assert_eq!(record.instances.len(), 2);

        let chat = &record.instances[0];
        assert_eq!(chat.profile, "chat");
        assert_eq!(chat.port, 39411);
        assert!(chat.workspace.ends_with("workspace"));
        assert!(!chat.autostart);

        let web = &record.instances[1];
        assert_eq!(web.profile, "web");
        assert_eq!(web.port, 39412);
        assert_eq!(web.workspace, capsule_root().join("workspace-web"));

        // 同一个盘符 → 没有盘符告警。
        assert!(
            record.warnings.is_empty(),
            "不该有告警：{:?}",
            record.warnings
        );

        let mut file = CapsulesFile::default();
        file.capsules.push(record);
        let specs = file.specs();
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[1].key(), "0.2.0-rc.2-default/web");
        assert!(file.find_spec("0.2.0-rc.2-default/web").is_some());
        assert!(file.find_spec("0.2.0-rc.2-default/nope").is_none());
    }

    #[test]
    fn autostart_specs_only_include_flagged_instances() {
        let mut record = parse(&capsule_root(), &instance_json(), &data_dir()).expect("应能登记");
        // 只有 web 打上自启标记。
        record.instances[1].autostart = true;

        let mut file = CapsulesFile::default();
        file.capsules.push(record);
        let specs = file.autostart_specs();
        assert_eq!(specs.len(), 1, "只该拉起打了标记的那个实例");
        assert_eq!(specs[0].key(), "0.2.0-rc.2-default/web");
        assert_eq!(specs[0].port, 39412);

        // 没有标记 = 什么都不拉起。
        let mut record = parse(&capsule_root(), &instance_json(), &data_dir()).expect("应能登记");
        record.instances[0].autostart = false;
        record.instances[1].autostart = false;
        let mut file = CapsulesFile::default();
        file.capsules.push(record);
        assert!(file.autostart_specs().is_empty());
    }

    #[test]
    fn capsule_root_mismatch_is_rejected() {
        let text = instance_json().replace(
            r#""capsuleRoot": "D:\\Images\\DeepseekHarness\\0.2.0-rc.2-default""#,
            r#""capsuleRoot": "D:\\Images\\DeepseekHarness\\some-other-capsule""#,
        );
        let err = parse(&capsule_root(), &text, &data_dir()).expect_err("选错目录必须报错");
        assert!(err.to_string().contains("capsuleRoot"), "{err}");
    }

    #[test]
    fn missing_or_duplicate_profile_is_rejected() {
        let missing = instance_json().replace(r#""profile": "chat","#, "");
        let err = parse(&capsule_root(), &missing, &data_dir()).expect_err("缺 profile 必须报错");
        assert!(err.to_string().contains("profile"), "{err}");

        // 主 profile 改成 web → 与 additionalProfiles[] 里的 web 冲突。
        let duplicate = instance_json().replace(r#""profile": "chat""#, r#""profile": "web""#);
        let err =
            parse(&capsule_root(), &duplicate, &data_dir()).expect_err("profile 重复必须报错");
        assert!(err.to_string().contains("profile 重复"), "{err}");
    }

    #[test]
    fn bad_port_is_rejected() {
        let text = instance_json().replace(r#""port": 39411"#, r#""port": 0"#);
        let err = parse(&capsule_root(), &text, &data_dir()).expect_err("端口 0 必须报错");
        assert!(err.to_string().contains("端口非法"), "{err}");
    }

    #[test]
    fn relative_workspace_is_resolved_with_warning() {
        let text = instance_json()
            .replace(
                r#""workspace": "D:\\Images\\DeepseekHarness\\0.2.0-rc.2-default\\workspace""#,
                r#""workspace": "workspace""#,
            )
            .replace(
                r#""workspace": "D:\\Images\\DeepseekHarness\\0.2.0-rc.2-default\\workspace-web""#,
                r#""workspace": "workspace-web""#,
            );
        let record = parse(&capsule_root(), &text, &data_dir()).expect("相对路径应按根解析");
        assert_eq!(
            record.instances[0].workspace,
            capsule_root().join("workspace")
        );
        assert!(
            record
                .warnings
                .iter()
                .any(|warning| warning.contains("相对路径")),
            "{:?}",
            record.warnings
        );
    }

    #[test]
    fn different_drive_and_unknown_schema_are_warnings_only() {
        let text = instance_json().replace(r#""schema": 1"#, r#""schema": 2"#);
        let record = parse(&capsule_root(), &text, &data_dir()).expect("未知 schema 照常登记");
        assert!(record
            .warnings
            .iter()
            .any(|warning| warning.contains("schema")));

        let other_drive_root = PathBuf::from(r"E:\Elsewhere");
        let moved = instance_json().replace(
            r#""capsuleRoot": "D:\\Images\\DeepseekHarness\\0.2.0-rc.2-default""#,
            r#""capsuleRoot": "E:\\Elsewhere""#,
        );
        let record = parse(&other_drive_root, &moved, &data_dir()).expect("跨盘只提示");
        assert!(
            record
                .warnings
                .iter()
                .any(|warning| warning.contains("盘符")),
            "{:?}",
            record.warnings
        );
    }

    #[test]
    fn refresh_preserves_user_fields_and_reports_changes() {
        let mut file = CapsulesFile::default();
        let first = parse(&capsule_root(), &instance_json(), &data_dir()).unwrap();
        let outcome = merge_record(&mut file, first);
        assert!(outcome.created);
        assert_eq!(file.capsules.len(), 1);

        // 用户在 M3 之前没法改，但落盘数据里可能有（手改 / 将来 GUI 写的）：刷新必须保留。
        file.capsules[0].instances[1].alias = Some("网页版".to_string());
        file.capsules[0].instances[1].autostart = true;
        file.capsules[0].instances[1]
            .env
            .insert("MY_FLAG".to_string(), "on".to_string());

        let changed = instance_json().replace(r#""port": 39412"#, r#""port": 39413"#);
        let second = parse(&capsule_root(), &changed, &data_dir()).unwrap();
        let outcome = merge_record(&mut file, second);

        assert!(!outcome.created);
        assert_eq!(file.capsules.len(), 1, "刷新不能产生重复条目");
        assert!(outcome.notice.contains("39412 → 39413"), "{outcome:?}");

        let web = &file.capsules[0].instances[1];
        assert_eq!(web.port, 39413);
        assert_eq!(web.alias.as_deref(), Some("网页版"));
        assert!(web.autostart);
        assert_eq!(web.env.get("MY_FLAG").map(String::as_str), Some("on"));
    }

    #[test]
    fn verbatim_prefix_is_stripped() {
        assert_eq!(
            strip_verbatim(Path::new(r"\\?\D:\Capsule\0.2.0")),
            PathBuf::from(r"D:\Capsule\0.2.0")
        );
        assert_eq!(
            strip_verbatim(Path::new(r"\\?\UNC\server\share\capsule")),
            PathBuf::from(r"\\server\share\capsule")
        );
        assert_eq!(
            strip_verbatim(Path::new(r"D:\Capsule")),
            PathBuf::from(r"D:\Capsule")
        );
    }

    #[test]
    fn paths_compare_case_insensitively_without_trailing_separator() {
        assert!(paths_equal(
            Path::new(r"D:\Images\Capsule\"),
            Path::new("d:\\images\\capsule")
        ));
        assert!(!paths_equal(Path::new(r"D:\a"), Path::new(r"D:\b")));
    }

    #[test]
    fn invalid_json_is_reported() {
        let err = parse(&capsule_root(), "{ not json", &data_dir()).expect_err("坏 JSON 必须报错");
        assert_eq!(err.context(), "解析 instance.json");
    }

    #[test]
    fn default_alias_uses_app_version_then_capsule_id() {
        assert_eq!(
            default_instance_alias("0.2.0-rc.2-default", Some("0.2.0-rc.2"), "chat"),
            "Deepseek Harness 0.2.0-rc.2 chat"
        );
        assert_eq!(
            default_instance_alias("0.2.1-rc.2-main", Some("0.2.1-rc.2"), "web"),
            "Deepseek Harness 0.2.1-rc.2 web"
        );
        // `app{}` 缺失 / 空白：退回 capsuleId，形状不变。
        assert_eq!(
            default_instance_alias("0.2.0-rc.2-default", None, "web"),
            "Deepseek Harness 0.2.0-rc.2-default web"
        );
        assert_eq!(
            default_instance_alias("0.2.0-rc.2-default", Some("  "), "web"),
            "Deepseek Harness 0.2.0-rc.2-default web"
        );
    }
}
