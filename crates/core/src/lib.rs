//! dsh-housing 内核（crate `dsh_housing_core`）——主进程运行时与全部逻辑。
//!
//! # 范围
//!
//! - **M0 已完成**：数据目录（`<exe 同级>\.dsh-housing\`）+ 两份 json；
//! - **M1 已完成**：`runtime`（单实例锁 / IPC 命令循环 / 实例表 / 托盘监督）、
//!   `job`（Job Object + `CreateProcessW` + 优雅停 / 强杀）、`env`、`logs`、
//!   `readiness`、`instance`、`ipc`，以及托盘子进程与它的重启监督；
//! - **M2 已完成**：`capsule`（只读登记 + 按 profile 展开）、`store` 的 `capsules.json`
//!   持久化、`monitor`（资源采样）、`port`（端口探测）、`browser`（打开网页）、
//!   日志保留清理，以及托盘上的登记 / 启停 / 打开网页入口。
//! - **M3（进行中）**：`console`（每次启动一换的 `consoleToken` + 起 `gui` 的 HTTP 线程）、
//!   `ipc` 的 `open_console` / `console_info`、日志「读最新文件尾部」、托盘「打开控制台…」。
//!
//! 依赖方向（docs/structure.md §2.1）：`app → {core, tray}`、`core → gui`、
//! `tray -.→ core`（只取 `model` / `ipc`）。core 不认识托盘，也不认识浏览器 UI。

pub mod autostart;
pub mod browser;
pub mod build;
pub mod capsule;
pub mod console;
pub mod diag;
pub mod env;
pub mod instance;
pub mod ipc;
pub mod job;
pub mod logs;
pub mod model;
pub mod monitor;
pub mod net;
pub mod notify;
pub mod port;
pub mod readiness;
pub mod runtime;
pub mod store;

/// `--role` 的取值（与 `crates/app` 的分发、`crates/tray` 的入口保持一处口径）。
pub const ROLE_CORE: &str = "core";
pub const ROLE_TRAY: &str = "tray";

/// `--role=core`（缺省）的入口：主进程运行时。
///
/// 返回进程退出码：0 = 正常退出；1 = 启动失败（已弹窗 + 落盘日志）。
pub fn run_core() -> i32 {
    runtime::run()
}

/// `--stop-all` 兜底通道：请求已有 core「停全部实例并退出」。
///
/// 固定返回 3（README / docs/structure.md 的退出码口径）。
pub fn run_stop_all() -> i32 {
    runtime::run_stop_all()
}
