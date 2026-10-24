//! dsh-housing 的唯一可执行体。
//!
//! 这里只做两件事，业务逻辑一律在 `dsh_housing_core` / `dsh_housing_tray` 里
//! （docs/structure.md §2.1）：
//!
//! 1. PE 子系统标记 —— `windows_subsystem = "windows"` 让 exe 没有控制台窗口，
//!    窗口由 PE 子系统决定，不靠运行时隐藏（§1.3 第 1 条）；
//! 2. 按 `--role` 分发 —— `core`（缺省）/ `tray` / `--stop-all`。
#![windows_subsystem = "windows"]

use std::process::ExitCode;

use dsh_housing_core::{ROLE_CORE, ROLE_TRAY};

const USAGE: &str = "用法：dsh-housing.exe [--role=core | --role=tray | --stop-all]\n\
                     \n\
                     --role=core  主进程（缺省）：数据目录、实例表、托盘监督\n\
                     --role=tray  托盘子进程\n\
                     --stop-all   停全部实例并退出（兜底通道）";

/// 角色分发结果。
enum Role {
    Core,
    Tray,
    StopAll,
}

fn main() -> ExitCode {
    match parse_role(std::env::args().skip(1)) {
        Ok(Role::Core) => ExitCode::from(exit_code(dsh_housing_core::run_core())),
        Ok(Role::Tray) => ExitCode::from(exit_code(dsh_housing_tray::run())),
        Ok(Role::StopAll) => ExitCode::from(exit_code(dsh_housing_core::run_stop_all())),
        Err(message) => {
            let text = format!("{message}\n\n{USAGE}");
            dsh_housing_core::diag::error(&format!("参数错误：{message}"));
            dsh_housing_core::diag::show_error(&text);
            ExitCode::from(2)
        }
    }
}

/// 解析命令行；`--role=core` 与 `--role core` 两种写法都收。
fn parse_role(args: impl Iterator<Item = String>) -> Result<Role, String> {
    let args: Vec<String> = args.collect();
    let mut role = Role::Core;
    let mut index = 0;
    while index < args.len() {
        let arg = args[index].as_str();
        if arg == "--stop-all" {
            return Ok(Role::StopAll);
        } else if arg == "--role" {
            index += 1;
            let value = args.get(index).ok_or("--role 后面缺少取值")?;
            role = role_from(value)?;
        } else if let Some(value) = arg.strip_prefix("--role=") {
            role = role_from(value)?;
        } else {
            return Err(format!("无法识别的参数：{arg}"));
        }
        index += 1;
    }
    Ok(role)
}

fn role_from(value: &str) -> Result<Role, String> {
    match value {
        // 角色名只在 core::ROLE_CORE / ROLE_TRAY 定义一处（M1 审计遗留项）。
        ROLE_CORE => Ok(Role::Core),
        ROLE_TRAY => Ok(Role::Tray),
        other => Err(format!(
            "未知角色：{other}（可用：{ROLE_CORE} / {ROLE_TRAY}）"
        )),
    }
}

/// 进程退出码只有低 8 位有效，这里夹到 0–255。
fn exit_code(code: i32) -> u8 {
    code.clamp(0, 255) as u8
}
