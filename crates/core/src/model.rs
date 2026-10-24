//! 跨 crate 共享的数据类型（docs/structure.md §2.2 的 `model.rs`）。
//!
//! M1 定型三块：`settings.json`（M0 已有）、**实例规格**（启动一个实例所需的全部数据）、
//! **实例快照**（传给托盘 / 将来 GUI 的只读视图）。M2 补上 `InstanceMetrics`
//! （资源采样结果，§3.9）与「登记了但没在跑」的合成视图（§3.14）。

use std::collections::BTreeMap;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// 两份 json 共用的文件格式版本号，写在各自的 `version` 字段里。
pub const SCHEMA_VERSION: u32 = 1;

/// 启动一个实例所需的全部数据（docs/structure.md §1.5：`instance = 胶囊 × profile`）。
///
/// M1 由 IPC 的 `start` 命令直接给全（测试脚本用）；M2 由 `capsule.rs` 读
/// `<capsule>\instance.json` 后按 profile 展开得到，形状不变。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InstanceSpec {
    /// 胶囊根目录（只读；启动器不往里写任何东西）。
    pub capsule_root: PathBuf,
    /// 胶囊标识（`instance.json` 的 `instanceId`，胶囊级，不是实例主键）。
    pub capsule_id: String,
    /// profile 名（`web` / `chat` …）。
    pub profile: String,
    /// 端口（启动器以登记值为准，不读 profile patch）。
    pub port: u16,
    /// 显式绑定的 host（v0.3 / §3.22）：`None` = 不传 `--host`，跟随胶囊 profile 配置。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// 该 profile 的 workspace（子进程 cwd）。
    pub workspace: PathBuf,
    /// 实例级环境变量覆盖（`capsules.json` 的 `env{}`），**最后**叠加。
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
    /// 用户设的显示名（可选）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
}

impl InstanceSpec {
    /// 实例键 = `capsuleId/profile`（日志命名、UI 行、资源统计都用它）。
    pub fn key(&self) -> String {
        format!("{}/{}", self.capsule_id, self.profile)
    }

    /// 展示名：优先用户别名，否则用胶囊标识。
    pub fn display_name(&self) -> &str {
        self.alias.as_deref().unwrap_or(&self.capsule_id)
    }
}

/// 单个实例的只读快照。
///
/// 状态用字符串而不是枚举：它是**跨进程 IPC 的线上格式**（托盘只认字符串），
/// 枚举留在 core 的 `instance.rs` 里。取值见 `instance::InstanceState::label`。
///
/// 没有 `Eq`：`InstanceMetrics` 里有 `f32`（CPU%），`PartialEq` 足够测试与比较。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InstanceView {
    /// 实例键（`capsuleId/profile`）。
    pub key: String,
    pub capsule_id: String,
    pub profile: String,
    /// 有效显示名：用户设的别名，没设过时是 core 派生的默认名（`Deepseek Harness <version> <profile>`，§3.20）。
    /// 快照里的这个字段总是有值；`InstanceSpec.alias` 才是「用户设过的那个」。
    pub alias: Option<String>,
    pub port: u16,
    /// 显式绑定的 host（v0.3 / §3.22）；`None` = 跟随胶囊配置。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// `stopped` / `starting` / `running` / `stopping` / `failed`。
    pub state: String,
    /// 失败原因或补充说明（如 readiness 超时）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
    /// dsh 主进程 PID（无进程时为空）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// 从 stdout 抓到的 dsh Web UI URL（含 `dshToken`；本地受控数据）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token_url: Option<String>,
    /// 最近一次资源采样（只在实例有进程时有值，§3.9）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metrics: Option<InstanceMetrics>,
}

impl InstanceView {
    /// 登记了但当前没有进程的实例（快照合并用，§3.14）。
    pub fn stopped_for(spec: &InstanceSpec) -> Self {
        Self {
            key: spec.key(),
            capsule_id: spec.capsule_id.clone(),
            profile: spec.profile.clone(),
            alias: spec.alias.clone(),
            port: spec.port,
            host: spec.host.clone(),
            state: "stopped".to_string(),
            detail: None,
            pid: None,
            token_url: None,
            metrics: None,
        }
    }
}

/// 一次资源采样（docs/structure.md §3.9）。
///
/// 字段都是**绝对值或 3 s 窗口内的平均值**，不是增量；托盘直接展示，GUI（M3）再画曲线。
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct InstanceMetrics {
    /// 整机 CPU 百分比：两次采样差分 ÷ 墙上时间 ÷ 逻辑核数 × 100。
    pub cpu_percent: f32,
    /// job 累计 CPU 时间（用户态 + 内核态，毫秒）。
    pub cpu_time_ms: u64,
    /// job 生命周期内的峰值提交内存（`PeakJobMemoryUsed`，字节）。
    pub peak_memory_bytes: u64,
    /// 采样时刻 job 内各进程工作集之和（字节）。
    pub working_set_bytes: u64,
    /// job 内进程数（含 MCP 子树）。
    pub process_count: u32,
    /// job 级累计读字节。
    pub io_read_bytes: u64,
    /// job 级累计写字节。
    pub io_write_bytes: u64,
}

/// 整份运行时快照：托盘拿它画菜单与 tooltip，GUI（M3）拿它渲染实例列表。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StatusSnapshot {
    pub instances: Vec<InstanceView>,
    /// core 正在收尾（停全部实例后退出）；托盘看到即应自行退出。
    pub shutting_down: bool,
    /// 托盘子进程当前是否在跑（由 core 维护）。
    pub tray_running: bool,
}

/// `settings.json` —— **只放壳自己的设置**（用户 2026-10-19 裁决 R8）。
///
/// 实例级 `env` 不在这个文件里，它随 `capsules.json` 走。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Settings {
    /// 文件格式版本。
    #[serde(default = "defaults::version")]
    pub version: u32,
    /// 控制台（本地 HTTP）端口；`0` = 每次启动随机高位端口。
    #[serde(default = "defaults::gui_port")]
    pub gui_port: u16,
    /// v0.4 / #286：控制台绑定的 IPv4 字面量。
    ///
    /// `127.0.0.1`（默认）= 仅本机；`0.0.0.0` = 所有网卡（局域网可访问）；
    /// 其它本机具体 IPv4 = 只绑那张网卡。只收字面量（域名 / IPv6 一律拒绝，见 `net::normalize_console_host`）。
    #[serde(default = "defaults::console_host")]
    pub console_host: String,
    /// 资源采样间隔，毫秒（默认 3 s，见 §3.9 的 2–5 s 区间）。
    #[serde(default = "defaults::sample_interval_ms")]
    pub sample_interval_ms: u64,
    /// 实例就绪后是否自动开浏览器。
    #[serde(default = "defaults::auto_open_browser")]
    pub auto_open_browser: bool,
    /// v0.3 / #251：core 启动、控制台就绪后是否自动打开控制台页面（默认开）。
    #[serde(default = "defaults::auto_open_console")]
    pub auto_open_console: bool,
    /// 托盘子进程崩溃后的最大重启次数（§3.8）。
    #[serde(default = "defaults::tray_restart_max")]
    pub tray_restart_max: u8,
    /// 每次重启前的退避，毫秒；长度与 `tray_restart_max` 不必相等，
    /// 超出长度的次数沿用最后一个值。
    #[serde(default = "defaults::tray_restart_backoff_ms")]
    pub tray_restart_backoff_ms: Vec<u64>,
    /// 日志保留天数（§3.10）。
    #[serde(default = "defaults::log_retention_days")]
    pub log_retention_days: u16,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            version: defaults::version(),
            gui_port: defaults::gui_port(),
            console_host: defaults::console_host(),
            sample_interval_ms: defaults::sample_interval_ms(),
            auto_open_browser: defaults::auto_open_browser(),
            auto_open_console: defaults::auto_open_console(),
            tray_restart_max: defaults::tray_restart_max(),
            tray_restart_backoff_ms: defaults::tray_restart_backoff_ms(),
            log_retention_days: defaults::log_retention_days(),
        }
    }
}

/// 设置的合法区间（等价于 `docs/structure.md` §2.4 的校验表；控制台 `PUT /api/settings`
/// 与「手改坏了的 settings.json 启动时只告警」共用这一套判定）。
pub mod limits {
    /// 采样间隔下限：再小就是把 core 拖进高频轮询。
    pub const MIN_SAMPLE_INTERVAL_MS: u64 = 500;
    /// 采样间隔上限（1 分钟）。
    pub const MAX_SAMPLE_INTERVAL_MS: u64 = 60_000;
    /// 托盘重启次数上限。
    pub const MAX_TRAY_RESTART: u8 = 10;
    /// 退避列表最多几项（超出长度的次数沿用最后一项，见 `Runtime::tray_backoff`）。
    pub const MAX_BACKOFF_ENTRIES: usize = 8;
    /// 单次退避上限（10 分钟）。
    pub const MAX_BACKOFF_MS: u64 = 600_000;
    /// 日志保留天数上限（1 年）。
    pub const MAX_LOG_RETENTION_DAYS: u16 = 365;
}

impl Settings {
    /// 范围校验：不合法返回「哪一项 + 区间」的人话（控制台直接显示）。
    ///
    /// 只校验范围，不管「要不要重启 core」——那是 §2.4 的热改边界，由 `runtime.rs` 决定。
    pub fn validate(&self) -> Result<(), String> {
        crate::net::normalize_console_host(&self.console_host)?;
        if !(limits::MIN_SAMPLE_INTERVAL_MS..=limits::MAX_SAMPLE_INTERVAL_MS)
            .contains(&self.sample_interval_ms)
        {
            return Err(format!(
                "采样间隔必须在 {}–{} 毫秒之间（现在是 {}）",
                limits::MIN_SAMPLE_INTERVAL_MS,
                limits::MAX_SAMPLE_INTERVAL_MS,
                self.sample_interval_ms
            ));
        }
        if self.tray_restart_max == 0 || self.tray_restart_max > limits::MAX_TRAY_RESTART {
            return Err(format!(
                "托盘重启次数必须在 1–{} 之间（现在是 {}）",
                limits::MAX_TRAY_RESTART,
                self.tray_restart_max
            ));
        }
        if self.tray_restart_backoff_ms.is_empty() {
            return Err("托盘重启退避至少要有一项（毫秒）".to_string());
        }
        if self.tray_restart_backoff_ms.len() > limits::MAX_BACKOFF_ENTRIES {
            return Err(format!(
                "托盘重启退避最多 {} 项（现在是 {}）",
                limits::MAX_BACKOFF_ENTRIES,
                self.tray_restart_backoff_ms.len()
            ));
        }
        if let Some(value) = self
            .tray_restart_backoff_ms
            .iter()
            .find(|value| **value > limits::MAX_BACKOFF_MS)
        {
            return Err(format!(
                "托盘重启退避的每一项都不能超过 {} 毫秒（现在是 {}）",
                limits::MAX_BACKOFF_MS,
                value
            ));
        }
        if self.log_retention_days > limits::MAX_LOG_RETENTION_DAYS {
            return Err(format!(
                "日志保留天数不能超过 {} 天（现在是 {}）",
                limits::MAX_LOG_RETENTION_DAYS,
                self.log_retention_days
            ));
        }
        Ok(())
    }
}

/// 逐字段默认值：老文件缺字段时按这里的值补齐，而不是按 `Default::default()` 的类型零值
/// （否则 `tray_restart_backoff_ms` 会静默变成空表）。
mod defaults {
    use super::SCHEMA_VERSION;

    pub fn version() -> u32 {
        SCHEMA_VERSION
    }
    pub fn gui_port() -> u16 {
        0
    }
    /// v0.4 / #286：默认只绑回环（M3 起的行为不变）。
    pub fn console_host() -> String {
        "127.0.0.1".to_string()
    }
    pub fn sample_interval_ms() -> u64 {
        3_000
    }
    pub fn auto_open_browser() -> bool {
        false
    }
    /// v0.3 / #251：默认**开**（用户口径「默认情况下打开程序会自动打开浏览器网页」）。
    pub fn auto_open_console() -> bool {
        true
    }
    pub fn tray_restart_max() -> u8 {
        3
    }
    pub fn tray_restart_backoff_ms() -> Vec<u64> {
        vec![1_000, 5_000, 15_000]
    }
    pub fn log_retention_days() -> u16 {
        14
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_settings_are_stable() {
        let settings = Settings::default();
        assert_eq!(settings.version, SCHEMA_VERSION);
        assert_eq!(settings.gui_port, 0);
        assert_eq!(settings.console_host, "127.0.0.1", "v0.4 起默认只绑回环");
        assert_eq!(settings.sample_interval_ms, 3_000);
        assert!(!settings.auto_open_browser);
        assert!(settings.auto_open_console, "v0.3 起默认启动后打开控制台");
        assert_eq!(settings.tray_restart_max, 3);
        assert_eq!(settings.tray_restart_backoff_ms, vec![1_000, 5_000, 15_000]);
        assert_eq!(settings.log_retention_days, 14);
    }

    #[test]
    fn missing_fields_fall_back_to_field_defaults() {
        let settings: Settings = serde_json::from_str("{}").expect("空对象应能补齐默认值");
        assert_eq!(settings, Settings::default());
    }

    #[test]
    fn default_settings_pass_validation() {
        assert_eq!(Settings::default().validate(), Ok(()));
    }

    #[test]
    fn validation_names_the_offending_field() {
        let bad_interval = Settings {
            sample_interval_ms: 10,
            ..Settings::default()
        };
        let error = bad_interval.validate().expect_err("采样间隔太小必须被拒");
        assert!(error.contains("采样间隔"), "{error}");

        assert!(
            Settings {
                tray_restart_max: 0,
                ..Settings::default()
            }
            .validate()
            .is_err(),
            "0 次重启等于不监督托盘"
        );

        assert!(
            Settings {
                tray_restart_backoff_ms: Vec::new(),
                ..Settings::default()
            }
            .validate()
            .is_err(),
            "空退避表会让托盘重启没有节拍"
        );

        assert!(
            Settings {
                tray_restart_backoff_ms: vec![1_000; 9],
                ..Settings::default()
            }
            .validate()
            .is_err(),
            "退避项数要有上限"
        );

        assert!(
            Settings {
                tray_restart_backoff_ms: vec![700_000],
                ..Settings::default()
            }
            .validate()
            .is_err(),
            "单次退避有上限"
        );

        assert!(
            Settings {
                log_retention_days: 400,
                ..Settings::default()
            }
            .validate()
            .is_err(),
            "日志保留天数有上限"
        );

        // 边界值要能过：500 ms / 365 天 / 上限退避 / 60000 ms。
        assert_eq!(
            Settings {
                sample_interval_ms: limits::MIN_SAMPLE_INTERVAL_MS,
                log_retention_days: limits::MAX_LOG_RETENTION_DAYS,
                tray_restart_backoff_ms: vec![limits::MAX_BACKOFF_MS],
                ..Settings::default()
            }
            .validate(),
            Ok(())
        );
        assert_eq!(
            Settings {
                sample_interval_ms: limits::MAX_SAMPLE_INTERVAL_MS,
                ..Settings::default()
            }
            .validate(),
            Ok(())
        );
    }

    #[test]
    fn console_host_accepts_ipv4_literals_only() {
        // v0.4 / #286：回环 / 通配 / 本机具体 IPv4 都合法（「本机」由运行时的 bind 探测把关）。
        for host in ["127.0.0.1", "0.0.0.0", "192.168.3.192", " 10.0.0.5 "] {
            assert_eq!(
                Settings {
                    console_host: host.to_string(),
                    ..Settings::default()
                }
                .validate(),
                Ok(()),
                "{host}"
            );
        }

        for host in [
            "",
            "localhost",
            "console.local",
            "::1",
            "224.0.0.1",
            "255.255.255.255",
        ] {
            let error = Settings {
                console_host: host.to_string(),
                ..Settings::default()
            }
            .validate()
            .expect_err("非法 console_host 必须被拒");
            assert!(error.contains("控制台启动位置"), "{host} → {error}");
        }
    }
}
