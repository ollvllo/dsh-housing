//! 一次构建的输入与静态校验（docs/structure.md §3.24 第 1 步）。

use std::path::{Path, PathBuf};

use crate::error::BuildError;

/// dsh 自带的 shipped profile 模板名（`dsh-app-boot` 的 `PROFILE_TEMPLATES`）。
///
/// 只有这些名字能让 dsh 在启动时自己 `initProfile()` 生成档案；未知名会直接抛
/// 「profile does not exist」。`desktop` 是 dsh 的保留档案名，构建器拒绝它。
pub const SHIPPED_PROFILES: &[&str] = &["acp", "web", "headless", "sdk", "sdk-minimal"];

/// 构建流水线的六个步骤（顺序即执行顺序）。
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
#[serde(rename_all = "snake_case")]
pub enum Step {
    /// 目录骨架。
    Skeleton,
    /// portable node。
    Runtime,
    /// app（`npm install`）。
    App,
    /// 启动器模板。
    Templates,
    /// `instance.json`。
    Manifest,
    /// 3 条版本断言。
    Verify,
}

impl Step {
    /// 全部步骤（顺序）。
    pub const ALL: [Step; 6] = [
        Step::Skeleton,
        Step::Runtime,
        Step::App,
        Step::Templates,
        Step::Manifest,
        Step::Verify,
    ];

    /// 面板上显示的标题。
    pub fn title(self) -> &'static str {
        match self {
            Step::Skeleton => "目录骨架",
            Step::Runtime => "portable node",
            Step::App => "app 安装（npm install）",
            Step::Templates => "启动器模板",
            Step::Manifest => "instance.json",
            Step::Verify => "版本断言",
        }
    }

    /// 1-based 序号（面板显示「第 n/6 步」）。
    pub fn index(self) -> u8 {
        match self {
            Step::Skeleton => 1,
            Step::Runtime => 2,
            Step::App => 3,
            Step::Templates => 4,
            Step::Manifest => 5,
            Step::Verify => 6,
        }
    }
}

/// 构建请求（`POST /api/builder/start` 的请求体 + core 补的两项）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildRequest {
    /// 目录名，也是 `instanceId`（约定 `<dsh 版本>-<name>`）。
    pub name: String,
    /// `@deepseek-ai/dsh` 的**精确**版本。
    pub dsh_version: String,
    /// portable Node 的版本（`vX.Y.Z`；不带 `v` 会补上）。
    pub node_version: String,
    /// 档案名（必须是 shipped 模板之一）。
    pub profile: String,
    /// 只绑回环的端口（由 core 算好传进来，见 §5.14 D40）。
    pub port: u16,
    /// 产物父目录（`<root>\<name>`）。
    pub root: PathBuf,
    /// npm registry（`None` = 默认 `registry.npmmirror.com`）。
    pub registry: Option<String>,
    /// 目标目录已存在且非空时是否允许继续（模板会被覆盖，`home\` 不动）。
    pub overwrite: bool,
    /// 删掉 `app\node_modules` 重装。
    pub force_app: bool,
    /// 只打印计划：不联网、不写盘、不跑 npm。
    pub dry_run: bool,
    /// 构建时间戳（人读，写进模板与清单；由 core 提供本地时间）。
    pub built_at: String,
}

impl BuildRequest {
    /// 默认 npm registry（与胶囊 `start.cmd` 的取值一致）。
    pub const DEFAULT_REGISTRY: &'static str = "https://registry.npmmirror.com/";

    /// 归一化后的 node 版本（带 `v` 前缀）。
    pub fn node_version_normalized(&self) -> String {
        normalize_node_version(&self.node_version)
    }

    /// 产物目录 `<root>\<name>`。
    pub fn capsule_root(&self) -> PathBuf {
        self.root.join(&self.name)
    }

    /// 登记用的胶囊标识（目录名即 `instanceId`）。
    pub fn capsule_id(&self) -> &str {
        &self.name
    }

    /// 生效的 npm registry（末尾保证带 `/`）。
    pub fn registry_or_default(&self) -> String {
        let raw = self
            .registry
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .unwrap_or(Self::DEFAULT_REGISTRY);
        if raw.ends_with('/') {
            raw.to_string()
        } else {
            format!("{raw}/")
        }
    }

    /// 第 1 步：静态校验（不碰磁盘；目录存在性由流水线的骨架步负责）。
    pub fn validate(&self) -> Result<(), BuildError> {
        if !is_valid_name(&self.name) {
            return Err(BuildError::invalid(format!(
                "名字只允许字母数字与 . _ -，且以字母数字开头：{}",
                self.name
            )));
        }
        if !is_exact_version(&self.dsh_version) {
            return Err(BuildError::invalid(format!(
                "dsh 版本必须是精确版本（如 0.2.1-rc.3），不接受 latest / ^ / 范围：{}",
                self.dsh_version
            )));
        }
        if !is_node_version(&self.node_version) {
            return Err(BuildError::invalid(format!(
                "node 版本必须是 vX.Y.Z：{}",
                self.node_version
            )));
        }
        if self.profile.trim().is_empty() {
            return Err(BuildError::invalid("profile 不能为空"));
        }
        if !SHIPPED_PROFILES.contains(&self.profile.as_str()) {
            return Err(BuildError::invalid(format!(
                "profile 只能是 dsh 自带的模板之一（{}）；当前：{}",
                SHIPPED_PROFILES.join(" / "),
                self.profile
            )));
        }
        if self.port == 0 {
            return Err(BuildError::invalid("端口必须在 1–65535 之间"));
        }
        if !self.root.is_absolute() {
            return Err(BuildError::invalid(format!(
                "目标父目录必须是绝对路径：{}",
                self.root.display()
            )));
        }
        Ok(())
    }

    /// 「目录名应当以 dsh 版本开头」这条命名约定不成文→只提示不拦，返回提示语。
    pub fn naming_hint(&self) -> Option<String> {
        if self.name.starts_with(&self.dsh_version) {
            return None;
        }
        Some(format!(
            "目录名建议按 <dsh 版本>-<name> 命名（当前 {} 不以 {} 开头）",
            self.name, self.dsh_version
        ))
    }
}

/// `v24.22.0` / `24.22.0` 都归一化成 `v24.22.0`。
pub fn normalize_node_version(version: &str) -> String {
    let trimmed = version.trim();
    if trimmed.starts_with('v') || trimmed.starts_with('V') {
        format!("v{}", &trimmed[1..])
    } else {
        format!("v{trimmed}")
    }
}

/// 目录名白名单：字母数字开头，其余只允许字母数字与 `. _ -`。
pub fn is_valid_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(first) if first.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    name.chars()
        .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'))
}

/// 精确版本：`x.y.z` 或 `x.y.z-<prerelease>`；拒绝 `latest` / `^` / `~` / 范围 / 首尾空格。
pub fn is_exact_version(version: &str) -> bool {
    if version.is_empty() || version.trim() != version {
        return false;
    }
    if version.contains(['^', '~', '*', ' ', '>', '<', '|', '=']) {
        return false;
    }
    let (core, pre) = match version.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (version, None),
    };
    let parts: Vec<&str> = core.split('.').collect();
    if parts.len() != 3
        || parts
            .iter()
            .any(|part| !part.chars().all(|c| c.is_ascii_digit()) || part.is_empty())
    {
        return false;
    }
    match pre {
        None => true,
        Some(pre) => {
            !pre.is_empty()
                && pre
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'))
        }
    }
}

/// node 版本：`vX.Y.Z`（允许省略 `v`；不接受首尾空格）。
pub fn is_node_version(version: &str) -> bool {
    if version.trim() != version {
        return false;
    }
    let trimmed = version;
    let rest = trimmed
        .strip_prefix('v')
        .or_else(|| trimmed.strip_prefix('V'))
        .unwrap_or(trimmed);
    let parts: Vec<&str> = rest.split('.').collect();
    parts.len() == 3
        && parts
            .iter()
            .all(|part| !part.is_empty() && part.chars().all(|c| c.is_ascii_digit()))
}

/// 目录是否非空（构建器只在「非空且没勾覆盖」时拒绝）。
pub fn dir_is_non_empty(path: &Path) -> bool {
    match std::fs::read_dir(path) {
        Ok(mut entries) => entries.next().is_some(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request() -> BuildRequest {
        BuildRequest {
            name: "0.2.1-rc.3-demo".to_string(),
            dsh_version: "0.2.1-rc.3".to_string(),
            node_version: "v24.22.0".to_string(),
            profile: "web".to_string(),
            port: 39414,
            root: PathBuf::from(r"D:\Images\DeepseekHarness"),
            registry: None,
            overwrite: false,
            force_app: false,
            dry_run: true,
            built_at: "2026-10-24 14:05:00 +08:00".to_string(),
        }
    }

    #[test]
    fn valid_request_passes() {
        assert_eq!(request().validate(), Ok(()));
    }

    #[test]
    fn name_must_be_safe() {
        assert!(is_valid_name("0.2.1-rc.3-demo"));
        assert!(is_valid_name("a"));
        assert!(!is_valid_name(""));
        assert!(!is_valid_name("-abc"));
        assert!(!is_valid_name("a/b"));
        assert!(!is_valid_name("a b"));
        assert!(!is_valid_name("名字"));
        let mut bad = request();
        bad.name = "x y".to_string();
        assert!(bad.validate().is_err());
    }

    #[test]
    fn dsh_version_must_be_exact() {
        assert!(is_exact_version("0.2.1-rc.3"));
        assert!(is_exact_version("0.2.0"));
        assert!(!is_exact_version("latest"));
        assert!(!is_exact_version("^0.2.1"));
        assert!(!is_exact_version("0.2"));
        assert!(!is_exact_version("0.2.1 "));
        assert!(!is_exact_version("0.2.1-"));
        let mut bad = request();
        bad.dsh_version = "latest".to_string();
        assert!(bad.validate().is_err());
    }

    #[test]
    fn node_version_normalizes() {
        assert!(is_node_version("v24.22.0"));
        assert!(is_node_version("24.22.0"));
        assert!(!is_node_version("v24"));
        assert_eq!(normalize_node_version("24.22.0"), "v24.22.0");
        assert_eq!(normalize_node_version("v24.22.0"), "v24.22.0");
        assert_eq!(normalize_node_version("V24.22.0"), "v24.22.0");
    }

    #[test]
    fn profile_must_be_shipped_template() {
        let mut bad = request();
        bad.profile = "desktop".to_string();
        assert!(bad.validate().is_err());
        bad.profile = "mine".to_string();
        assert!(bad.validate().is_err());
        for profile in SHIPPED_PROFILES {
            let mut ok = request();
            ok.profile = (*profile).to_string();
            assert_eq!(ok.validate(), Ok(()), "{profile}");
        }
    }

    #[test]
    fn port_and_root_are_checked() {
        let mut bad = request();
        bad.port = 0;
        assert!(bad.validate().is_err());
        bad.port = 39414;
        bad.root = PathBuf::from("relative\\dir");
        assert!(bad.validate().is_err());
    }

    #[test]
    fn capsule_root_joins_name() {
        assert_eq!(
            request().capsule_root(),
            PathBuf::from(r"D:\Images\DeepseekHarness\0.2.1-rc.3-demo")
        );
        assert_eq!(request().capsule_id(), "0.2.1-rc.3-demo");
    }

    #[test]
    fn registry_defaults_and_gets_trailing_slash() {
        assert_eq!(
            request().registry_or_default(),
            "https://registry.npmmirror.com/"
        );
        let mut custom = request();
        custom.registry = Some("https://registry.npmjs.org".to_string());
        assert_eq!(custom.registry_or_default(), "https://registry.npmjs.org/");
        custom.registry = Some("   ".to_string());
        assert_eq!(custom.registry_or_default(), BuildRequest::DEFAULT_REGISTRY);
    }

    #[test]
    fn naming_hint_points_at_convention() {
        assert_eq!(request().naming_hint(), None);
        let mut other = request();
        other.name = "demo".to_string();
        assert!(other.naming_hint().is_some());
    }

    #[test]
    fn step_titles_and_indexes_are_stable() {
        assert_eq!(Step::Skeleton.index(), 1);
        assert_eq!(Step::Verify.index(), 6);
        assert_eq!(Step::ALL.len(), 6);
        assert_eq!(Step::App.title(), "app 安装（npm install）");
    }
}
