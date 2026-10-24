//! 事件与模板渲染（docs/structure.md §3.21）。
//!
//! 渲染是**单遍扫描**：替换进去的值不会再被当成模板继续展开（用户把别名写成 `{time}`
//! 时也不会出现二次替换），未知占位符原样保留（写错占位符能在收到的消息里一眼看出来）。

use crate::config::{Trigger, PLACEHOLDERS};

/// 一次要发出去的事件：core 在事件发生那一刻把事实取好，投递线程不再回读 core 状态。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    /// 触发条件。
    pub trigger: Trigger,
    /// 事件时刻（本机本地时间 `YYYY-MM-DD HH:MM:SS`，由 core 的 `diag::timestamp_text` 生成）。
    pub time: String,
    /// 实例的**有效显示名**（用户别名，没设过时是派生的 `Deepseek Harness <version> <profile>`）。
    pub alias: String,
    /// 胶囊登记里的 dsh 版本（`app.version`）；缺登记信息时退回 `capsuleId`。
    pub dsh_version: String,
    /// profile 名（`web` / `chat` …）。
    pub profile: String,
    /// dsh Web UI 的 token URL（含 `dshToken`，**是有效凭据**）。
    pub local_url: Option<String>,
    /// 局域网可达时给出的同一条 URL（host 换成探测到的局域网 IPv4）；不可达 / 不适用时为 `None`。
    pub lan_url: Option<String>,
}

impl Event {
    /// 占位符 → 值。`None` 表示「不认识这个占位符」（原样保留），空串表示「这个事件没有值」。
    pub fn value(&self, name: &str) -> Option<&str> {
        match name {
            "time" => Some(&self.time),
            "alias" => Some(&self.alias),
            "dsh_version" => Some(&self.dsh_version),
            "dsh_profile" => Some(&self.profile),
            "local_url" => Some(self.local_url.as_deref().unwrap_or("")),
            "lan_url" => Some(self.lan_url.as_deref().unwrap_or("")),
            _ => None,
        }
    }

    /// 渲染模板：`{name}` 替换成值，未知的 `{...}` 原样保留。
    pub fn render(&self, template: &str) -> String {
        let mut out = String::with_capacity(template.len() + 64);
        let mut rest = template;
        while let Some(open) = rest.find('{') {
            out.push_str(&rest[..open]);
            let after_open = &rest[open + 1..];
            match after_open.find('}') {
                Some(close) => {
                    let name = &after_open[..close];
                    match self.value(name) {
                        Some(value) => out.push_str(value),
                        None => {
                            // 不认识：连花括号一起原样吐出去。
                            out.push('{');
                            out.push_str(name);
                            out.push('}');
                        }
                    }
                    rest = &after_open[close + 1..];
                }
                None => {
                    // 没闭合：剩下全是普通文本。
                    out.push_str(&rest[open..]);
                    rest = "";
                }
            }
        }
        out.push_str(rest);
        out
    }

    /// 事件的日志标签（不打印 URL，避免把凭据写进日志）。
    pub fn label(&self) -> String {
        format!("{} · {}", self.alias, self.trigger.label())
    }
}

/// 模板里是否用了某个占位符（core 用它决定要不要做开销大的探测，例如 `{lan_url}`）。
pub fn template_uses(template: &str, name: &str) -> bool {
    debug_assert!(PLACEHOLDERS.contains(&name), "未知占位符：{name}");
    template.contains(&format!("{{{name}}}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event() -> Event {
        Event {
            trigger: Trigger::Start,
            time: "2026-10-20 16:40:00".to_string(),
            alias: "Deepseek Harness 0.2.0-rc.2 web".to_string(),
            dsh_version: "0.2.0-rc.2".to_string(),
            profile: "web".to_string(),
            local_url: Some("http://127.0.0.1:39412/?token=<token>".to_string()),
            lan_url: Some("http://192.168.3.100:39412/?token=<token>".to_string()),
        }
    }

    #[test]
    fn default_start_template_renders_every_field() {
        let text = event().render(crate::config::DEFAULT_START_TEMPLATE);
        assert_eq!(
            text,
            "Service Start\n\
             Time : 2026-10-20 16:40:00\n\
             Name: Deepseek Harness 0.2.0-rc.2 web\n\
             Version: 0.2.0-rc.2\n\
             Profile: web\n\
             Local Url: http://127.0.0.1:39412/?token=<token>\n\
             LAN Url: http://192.168.3.100:39412/?token=<token>"
        );
    }

    #[test]
    fn start_urls_always_carry_a_scheme() {
        // #270：两条 URL 占位符渲染出来必须带协议名（回归断言，防止把 authority 当 URL 发出去）。
        let text = event().render(crate::config::DEFAULT_START_TEMPLATE);
        for line in text
            .lines()
            .filter(|line| line.starts_with("Local Url:") || line.starts_with("LAN Url:"))
        {
            let url = line
                .split_once(": ")
                .expect("URL 行应形如 'Local Url: http://…'")
                .1;
            assert!(
                url.starts_with("http://") || url.starts_with("https://"),
                "{line}"
            );
        }
    }

    #[test]
    fn housing_start_template_renders_the_console_url() {
        let housing = Event {
            trigger: Trigger::HousingStart,
            time: "2026-10-22 18:30:00".to_string(),
            alias: String::new(),
            dsh_version: String::new(),
            profile: String::new(),
            local_url: Some("http://127.0.0.1:30010/?token=<console>".to_string()),
            lan_url: None,
        };
        assert_eq!(
            housing.render(crate::config::DEFAULT_HOUSING_START_TEMPLATE),
            "Housing Start\n\
             Time: 2026-10-22 18:30:00\n\
             Local Url: http://127.0.0.1:30010/?token=<console>\n\
             LAN Url: "
        );
    }

    #[test]
    fn missing_values_render_empty_and_unknown_placeholders_stay_literal() {
        let mut start = event();
        start.local_url = None;
        start.lan_url = None;
        let text = start.render("[{local_url}|{lan_url}|{nope}|{unclosed");
        assert_eq!(text, "[||{nope}|{unclosed");
    }

    #[test]
    fn rendered_values_are_not_expanded_again() {
        // 别名里带占位符字样时不能被二次替换（单遍扫描的意义）。
        let mut start = event();
        start.alias = "{time}".to_string();
        let text = start.render("{alias} / {time}");
        assert_eq!(text, "{time} / 2026-10-20 16:40:00");
    }

    #[test]
    fn placeholders_work_more_than_once_and_in_unicode_templates() {
        let text = event().render("名称：{alias}；端口行：{dsh_profile}；再次：{alias}");
        assert_eq!(
            text,
            "名称：Deepseek Harness 0.2.0-rc.2 web；端口行：web；再次：Deepseek Harness 0.2.0-rc.2 web"
        );
    }

    #[test]
    fn template_uses_detects_the_real_placeholder() {
        assert!(template_uses(
            crate::config::DEFAULT_START_TEMPLATE,
            "lan_url"
        ));
        assert!(!template_uses(
            crate::config::DEFAULT_STOP_TEMPLATE,
            "local_url"
        ));
    }
}
