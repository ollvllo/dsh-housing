//! webhook 配置：`webhooks.json` 的形状、默认模板与校验（docs/structure.md §3.21）。
//!
//! 这个模块**不知道 core、不知道实例**：它只描述「往哪个地址、在哪些事件上、发什么正文」。
//! 事件从哪来由 `crates/core/src/notify.rs` 负责（§5.11 D21 的边界）。

use serde::{Deserialize, Serialize};

/// `webhooks.json` 的格式版本（与 `capsules.json` / `settings.json` 各自独立）。
pub const SCHEMA_VERSION: u32 = 1;

/// 最多几条 webhook（每条最多三个模板；再多就不是「通知几个地方」而是日志转发了）。
pub const MAX_WEBHOOKS: usize = 16;

/// 单条 URL 的长度上限（字符）。
pub const MAX_URL_CHARS: usize = 2048;

/// 单个模板的长度上限（字符；渲染后按 UTF-8 字节数发出去）。
pub const MAX_TEMPLATE_CHARS: usize = 8192;

/// 模板里可替换的占位符——**唯一真源**。
///
/// 默认模板必须把这里每一个都写进去（用户口径：「默认模板中包含所有能够被替换的匹配符」），
/// 这条不变量由本模块的单测守着。
pub const PLACEHOLDERS: [&str; 6] = [
    "time",
    "alias",
    "dsh_version",
    "dsh_profile",
    "local_url",
    "lan_url",
];

/// Service Start 的默认模板（原文照抄任务卡 #244，包含全部占位符）。
pub const DEFAULT_START_TEMPLATE: &str = "Service Start\n\
Time : {time}\n\
Name: {alias}\n\
Version: {dsh_version}\n\
Profile: {dsh_profile}\n\
Local Url: {local_url}\n\
LAN Url: {lan_url}";

/// Service Stop 的默认模板。
pub const DEFAULT_STOP_TEMPLATE: &str = "Service Stop\n\
Time: {time}\n\
Name: {alias}\n\
Version: {dsh_version}\n\
Profile: {dsh_profile}";

/// Service Crashed 的默认模板。
pub const DEFAULT_CRASHED_TEMPLATE: &str = "Service Crashed\n\
Time: {time}\n\
Name: {alias}\n\
Version: {dsh_version}\n\
Profile: {dsh_profile}";

/// Housing Start 的默认模板（v0.3 / #259）：dsh-housing 启动时发一次，URL 指控制台。
pub const DEFAULT_HOUSING_START_TEMPLATE: &str = "Housing Start\n\
Time: {time}\n\
Local Url: {local_url}\n\
LAN Url: {lan_url}";

/// 触发条件。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Trigger {
    /// 实例就绪（readiness 抓到 token URL）那一刻。
    Start,
    /// 用户停止（含 core 退出时的停止）完成之后。
    Stop,
    /// 进程**自行**退出（不是被我们停的）。
    Crashed,
    /// dsh-housing 启动、控制台就绪之后（每个 core 生命周期一次）。
    HousingStart,
}

impl Trigger {
    /// 四个条件（固定顺序，UI 与日志都按它走）。
    pub const ALL: [Trigger; 4] = [
        Trigger::Start,
        Trigger::Stop,
        Trigger::Crashed,
        Trigger::HousingStart,
    ];

    /// 线上标签（`webhooks.json` 的字段名 / 日志）。
    pub fn label(self) -> &'static str {
        match self {
            Trigger::Start => "start",
            Trigger::Stop => "stop",
            Trigger::Crashed => "crashed",
            Trigger::HousingStart => "housing_start",
        }
    }

    /// 该条件的默认模板。
    pub fn default_template(self) -> &'static str {
        match self {
            Trigger::Start => DEFAULT_START_TEMPLATE,
            Trigger::Stop => DEFAULT_STOP_TEMPLATE,
            Trigger::Crashed => DEFAULT_CRASHED_TEMPLATE,
            Trigger::HousingStart => DEFAULT_HOUSING_START_TEMPLATE,
        }
    }

    /// 取某条「默认模板 + 启用」的配置（新建 webhook 时用）。
    pub fn default_config(self) -> TriggerConfig {
        TriggerConfig {
            enabled: true,
            template: self.default_template().to_string(),
        }
    }

    /// 从 `Webhook` 里取出本条件的配置。
    pub fn config(self, hook: &Webhook) -> &TriggerConfig {
        match self {
            Trigger::Start => &hook.start,
            Trigger::Stop => &hook.stop,
            Trigger::Crashed => &hook.crashed,
            Trigger::HousingStart => &hook.housing_start,
        }
    }
}

/// 一个触发条件的配置。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TriggerConfig {
    /// 是否在这个条件上发（默认开）。
    #[serde(default = "defaults::enabled")]
    pub enabled: bool,
    /// 请求正文模板；`{...}` 占位符在投递时替换（未知占位符原样保留）。
    #[serde(default)]
    pub template: String,
}

impl Default for TriggerConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            template: String::new(),
        }
    }
}

/// 一条 webhook。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Webhook {
    /// 请求地址（`http://` / `https://`；可以是 ntfy / Discord / 自建脚本…）。
    #[serde(default)]
    pub url: String,
    /// 整条是否启用（三个条件都勾了也发不出去时用它一键静音）。
    #[serde(default = "defaults::enabled")]
    pub enabled: bool,
    /// 实例就绪时。
    #[serde(default = "defaults::start")]
    pub start: TriggerConfig,
    /// 实例停止时。
    #[serde(default = "defaults::stop")]
    pub stop: TriggerConfig,
    /// 进程自行退出时。
    #[serde(default = "defaults::crashed")]
    pub crashed: TriggerConfig,
    /// dsh-housing 启动、控制台就绪时（v0.3 / #259）。
    #[serde(default = "defaults::housing_start")]
    pub housing_start: TriggerConfig,
}

impl Default for Webhook {
    fn default() -> Self {
        Self {
            url: String::new(),
            enabled: true,
            start: Trigger::Start.default_config(),
            stop: Trigger::Stop.default_config(),
            crashed: Trigger::Crashed.default_config(),
            housing_start: Trigger::HousingStart.default_config(),
        }
    }
}

impl Webhook {
    /// 校验一条 webhook（不合法 → 人话，控制台原样显示）。
    pub fn validate(&self) -> Result<(), String> {
        let url = self.url.trim();
        if url.is_empty() {
            return Err("URL 不能为空".to_string());
        }
        let lower = url.to_ascii_lowercase();
        if !(lower.starts_with("http://") || lower.starts_with("https://")) {
            return Err(format!(
                "URL 必须是 http:// 或 https:// 开头（现在是 {url}）"
            ));
        }
        if url.chars().count() > MAX_URL_CHARS {
            return Err(format!("URL 太长（>{MAX_URL_CHARS} 字符）"));
        }
        if url.chars().any(|ch| ch.is_whitespace() || ch.is_control()) {
            return Err("URL 不能含空白或控制字符".to_string());
        }
        for trigger in Trigger::ALL {
            trigger
                .config(self)
                .validate(trigger)
                .map_err(|reason| format!("{} 模板：{reason}", trigger.label()))?;
        }
        Ok(())
    }

    /// 某条件下会发出去的正文模板；条件没启用时返回 `None`。
    pub fn template_for(&self, trigger: Trigger) -> Option<&str> {
        let config = trigger.config(self);
        (self.enabled && config.enabled).then_some(config.template.as_str())
    }
}

impl TriggerConfig {
    fn validate(&self, trigger: Trigger) -> Result<(), String> {
        if self.template.chars().count() > MAX_TEMPLATE_CHARS {
            return Err(format!("长于 {MAX_TEMPLATE_CHARS} 字符"));
        }
        if self.template.contains('\0') {
            return Err("含 NUL".to_string());
        }
        let _ = trigger; // 目前校验规则三条件相同；参数留着是为了将来按条件放宽。
        Ok(())
    }
}

/// `webhooks.json` 的完整形状。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WebhookFile {
    /// 文件格式版本。
    #[serde(default = "defaults::version")]
    pub version: u32,
    /// webhook 列表（顺序就是 UI 里的顺序）。
    #[serde(default)]
    pub webhooks: Vec<Webhook>,
}

impl Default for WebhookFile {
    fn default() -> Self {
        Self {
            version: SCHEMA_VERSION,
            webhooks: Vec::new(),
        }
    }
}

impl WebhookFile {
    /// 整份校验：条数 + 逐条。
    pub fn validate(&self) -> Result<(), String> {
        if self.webhooks.len() > MAX_WEBHOOKS {
            return Err(format!(
                "最多 {} 条 webhook（现在 {} 条）",
                MAX_WEBHOOKS,
                self.webhooks.len()
            ));
        }
        for (index, hook) in self.webhooks.iter().enumerate() {
            hook.validate()
                .map_err(|reason| format!("第 {} 条 webhook：{reason}", index + 1))?;
        }
        Ok(())
    }

    /// 可以投递的那些（手改坏的条目跳过、不静默变成「全部不发」）。
    ///
    /// 注意：`enabled` 的判断在投递时做（要按事件条件分别判），这里只筛「配置本身合法」的。
    pub fn valid_hooks(&self) -> impl Iterator<Item = &Webhook> {
        self.webhooks.iter().filter(|hook| hook.validate().is_ok())
    }
}

/// 日志里用的地址标签：**只保留 scheme + host[:port]**，路径与查询串一律打码。
///
/// 理由：webhook URL 里经常直接带凭据（ntfy 的 topic + token、Discord 的 webhook token），
/// 而 `housing-*.log` 是长期保留的文件——不能把凭据抄进去（与 consoleToken 同一口径）。
pub fn endpoint_label(url: &str) -> String {
    let trimmed = url.trim();
    match trimmed.split_once("://") {
        Some((scheme, rest)) => {
            let authority = rest
                .split(['/', '?', '#'])
                .next()
                .unwrap_or_default()
                .trim();
            if authority.is_empty() {
                format!("{scheme}://…")
            } else {
                format!("{scheme}://{authority}/…")
            }
        }
        None => {
            let head: String = trimmed.chars().take(32).collect();
            format!("{head}…")
        }
    }
}

mod defaults {
    use super::{Trigger, SCHEMA_VERSION};

    pub fn version() -> u32 {
        SCHEMA_VERSION
    }
    pub fn enabled() -> bool {
        true
    }
    pub fn start() -> super::TriggerConfig {
        Trigger::Start.default_config()
    }
    pub fn stop() -> super::TriggerConfig {
        Trigger::Stop.default_config()
    }
    pub fn crashed() -> super::TriggerConfig {
        Trigger::Crashed.default_config()
    }
    pub fn housing_start() -> super::TriggerConfig {
        Trigger::HousingStart.default_config()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_templates_contain_every_placeholder() {
        // 用户口径：「默认模板中包含所有能够被替换的匹配符」——这条不变量由这里守着。
        // 注意是**四个默认模板的并集**：Stop / Crashed 带四项、Start 带全部六项、
        // Housing Start 只带时间与两条 URL（其 alias / version / profile 无意义）。
        let union = Trigger::ALL
            .iter()
            .map(|trigger| trigger.default_template())
            .collect::<Vec<_>>()
            .join("\n");
        for name in PLACEHOLDERS {
            let token = format!("{{{name}}}");
            assert!(
                union.contains(&token),
                "{token} 没出现在任何默认模板里：\n{union}"
            );
        }
    }

    #[test]
    fn default_webhook_enables_all_four_triggers_with_default_templates() {
        let hook = Webhook::default();
        assert!(hook.enabled);
        assert_eq!(hook.start.template, DEFAULT_START_TEMPLATE);
        assert_eq!(hook.stop.template, DEFAULT_STOP_TEMPLATE);
        assert_eq!(hook.crashed.template, DEFAULT_CRASHED_TEMPLATE);
        assert_eq!(hook.housing_start.template, DEFAULT_HOUSING_START_TEMPLATE);
        assert_eq!(
            hook.template_for(Trigger::Start),
            Some(DEFAULT_START_TEMPLATE)
        );
        assert_eq!(
            hook.template_for(Trigger::HousingStart),
            Some(DEFAULT_HOUSING_START_TEMPLATE)
        );
        assert!(hook.validate().is_err(), "空 URL 必须被拒");
    }

    #[test]
    fn partial_json_falls_back_to_defaults() {
        // 手写 / 旧版文件只给一个 url 时，四个条件都按默认模板补上。
        let file: WebhookFile =
            serde_json::from_str(r#"{"webhooks":[{"url":"https://example.test/hook"}]}"#).unwrap();
        assert_eq!(file.version, SCHEMA_VERSION);
        let hook = &file.webhooks[0];
        assert!(hook.enabled && hook.start.enabled && hook.stop.enabled && hook.crashed.enabled);
        assert!(
            hook.housing_start.enabled,
            "旧文件缺 housing_start 时默认启用"
        );
        assert_eq!(hook.start.template, DEFAULT_START_TEMPLATE);
        assert_eq!(hook.housing_start.template, DEFAULT_HOUSING_START_TEMPLATE);
        assert_eq!(file.validate(), Ok(()));
    }

    #[test]
    fn validation_names_the_problem() {
        let bad_scheme = Webhook {
            url: "ftp://example.test".to_string(),
            ..Webhook::default()
        };
        assert!(bad_scheme
            .validate()
            .expect_err("scheme 不合法")
            .contains("http"));

        let spaced = Webhook {
            url: "https://example.test/a b".to_string(),
            ..Webhook::default()
        };
        assert!(spaced.validate().is_err());

        let too_long_template = Webhook {
            url: "https://example.test/hook".to_string(),
            start: TriggerConfig {
                enabled: true,
                template: "x".repeat(MAX_TEMPLATE_CHARS + 1),
            },
            ..Webhook::default()
        };
        assert!(too_long_template.validate().is_err());

        let too_many = WebhookFile {
            version: SCHEMA_VERSION,
            webhooks: vec![
                Webhook {
                    url: "https://example.test/hook".to_string(),
                    ..Webhook::default()
                };
                MAX_WEBHOOKS + 1
            ],
        };
        assert!(too_many
            .validate()
            .expect_err("条数有上限")
            .contains("最多"));
    }

    #[test]
    fn disabled_trigger_has_no_template_to_send() {
        let mut hook = Webhook {
            url: "https://example.test/hook".to_string(),
            ..Webhook::default()
        };
        hook.crashed.enabled = false;
        assert_eq!(hook.template_for(Trigger::Crashed), None);
        assert!(hook.template_for(Trigger::Start).is_some());

        hook.enabled = false;
        assert_eq!(hook.template_for(Trigger::Start), None);
    }

    #[test]
    fn endpoint_label_drops_path_and_query() {
        assert_eq!(
            endpoint_label("https://ntfy.example.test/secret-topic?auth=token"),
            "https://ntfy.example.test/…"
        );
        assert_eq!(
            endpoint_label("http://127.0.0.1:8443/hook"),
            "http://127.0.0.1:8443/…"
        );
        assert_eq!(
            endpoint_label("https://example.test"),
            "https://example.test/…"
        );
        assert_eq!(endpoint_label("nonsense"), "nonsense…");
    }

    #[test]
    fn invalid_hooks_are_skipped_not_fatal() {
        let file = WebhookFile {
            version: SCHEMA_VERSION,
            webhooks: vec![
                Webhook {
                    url: "https://example.test/ok".to_string(),
                    ..Webhook::default()
                },
                Webhook {
                    url: "ftp://example.test/bad".to_string(),
                    ..Webhook::default()
                },
            ],
        };
        let valid: Vec<&Webhook> = file.valid_hooks().collect();
        assert_eq!(valid.len(), 1);
        assert_eq!(valid[0].url, "https://example.test/ok");
    }
}
