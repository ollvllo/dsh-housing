//! dsh-housing 控制台的 Rust 侧（crate `dsh_housing_gui`）。
//!
//! # 范围（docs/structure.md §2.5 / §3.17）
//!
//! 只做一件事：把一个**只绑回环**的 HTTP 服务跑起来，把静态资源与 JSON API 交给浏览器。
//!
//! - HTTP 服务活在 **core 进程内**，被 core 挂载，不单独开进程；
//! - 每个 `/api/*` 请求都要带 `consoleToken`；变更请求还要过 `Origin` 校验；
//! - 前端是**无构建 Vue 3**（`web/`），编译期用 `include_dir!` 编进 exe；
//! - 本 crate **不认识 core**：core 通过 [`api::ConsoleBackend`] 把能力注进来
//!   （依赖方向 `core → gui`，见 §2.1 的硬规则）。
//!
//! # 线程模型
//!
//! [`server::serve`] 起一个**专用线程**，在里面跑 current-thread tokio runtime；
//! core 仍然是 M1/M2 的同步线程模型。async 不出这个 crate。

pub mod api;
pub mod assets;
pub mod server;

pub use api::{
    ActionResult, AppView, AutostartView, BackendError, BuilderLastView, BuilderRequestView,
    BuilderResultView, BuilderStartRequest, BuilderVersionsQuery, BuilderVersionsView, BuilderView,
    CapsuleView, ConsoleBackend, ConsoleInfo, ConsoleState, InstanceConfigView, LogPayload,
    RegistryView, RuntimeView, SettingsRequest, SettingsView, TriggerTemplates, TriggerView,
    WebhookSection, WebhookView,
};
pub use server::{router, serve, ConsoleConfig, ConsoleError, ConsoleHandle};
