//! dsh-housing 的 webhook 投递单元（crate `dsh_housing_webhook`）。
//!
//! # 为什么是单独一个 crate（docs/structure.md §5.11 D21）
//!
//! 任务卡 #244 的问题是「webhook 该放进内核编译单元还是外面」。答案是**按职责拆**，
//! 而不是整块二选一：
//!
//! - **事件源在内核**：什么时候算「起来了 / 停了 / 崩了」只有 core 的状态机知道
//!   （`instance.rs` 的 `Observation` 与停止线程），这部分搬出去只能靠复制状态机；
//! - **投递在核外**（本 crate）：配置、模板渲染、HTTP、重试、队列——用户 #244 说的
//!   「复杂的业务」，与运行时只通过一条 `Event` 数据流耦合。
//!
//! 依赖方向：`core → webhook`（与 `core → gui` 同向）；**本 crate 不认识 core、不认识实例**，
//! 只认 [`Event`] 与 [`Webhook`]。这样投递能脱离 core 单测（本地假端点就能跑通整条链路）。
//!
//! # 一份事件怎么走
//!
//! ```text
//! core/notify.rs ──Event──> Dispatcher::submit（try_send，非阻塞）
//!                             │
//!                       webhook-delivery 线程
//!                             ├─ 取配置快照（Webhook{url, enabled, 三个条件})
//!                             ├─ Event::render(template)        模板 → 正文（单遍替换）
//!                             └─ http::post_text（WinHTTP，5 s 超时，传输层失败重试 1 次）
//! ```

pub mod config;
pub mod dispatch;
pub mod http;
pub mod template;

pub use config::{
    endpoint_label, Trigger, TriggerConfig, Webhook, WebhookFile, DEFAULT_CRASHED_TEMPLATE,
    DEFAULT_HOUSING_START_TEMPLATE, DEFAULT_START_TEMPLATE, DEFAULT_STOP_TEMPLATE,
    MAX_TEMPLATE_CHARS, MAX_URL_CHARS, MAX_WEBHOOKS, PLACEHOLDERS, SCHEMA_VERSION,
};
pub use dispatch::{Dispatcher, Sink, QUEUE_CAPACITY};
pub use template::{template_uses, Event};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crate_reexports_are_wired() {
        // 只验证上级 crate 能用 `dsh_housing_webhook::...` 拿到这些东西。
        let hook = Webhook::default();
        assert!(hook.start.enabled);
        assert_eq!(Trigger::Start.label(), "start");
        assert!(PLACEHOLDERS.contains(&"lan_url"));
        assert_eq!(WebhookFile::default().version, SCHEMA_VERSION);
        assert!(template_uses(DEFAULT_START_TEMPLATE, "local_url"));
        // `QUEUE_CAPACITY` 也走 re-export（值本身是常量，不在断言里比大小）。
        let _ = QUEUE_CAPACITY;
    }
}
