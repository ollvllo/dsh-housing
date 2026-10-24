//! 端口探测（docs/structure.md §3.11、§5.7 D3）。
//!
//! 启动前用 `TcpListener::bind("0.0.0.0:<port>")` 试绑，成功即立刻释放：
//!
//! - `Free`：可以启动；
//! - `InUse`：**拒绝启动**（不自动换端口），原因给人看；
//! - `Unknown`：探测本身失败（权限之类）→ 调用方告警后**照常启动**，
//!   不让探测变成新的失败点。
//!
//! 绑通配地址而不是 `127.0.0.1`：Windows 上通配绑定与具体地址绑定默认互斥，
//! 这样也能覆盖「别的程序只绑了 127.0.0.1」的情况。
//!
//! 承认 TOCTOU：探测与 dsh 真正 bind 之间有时间窗；真被抢了就是「启动后自行退出」
//! 这条既有路径（`Failed` + 日志），不是主路径。

use std::io;
use std::net::{Ipv4Addr, TcpListener};

/// 一次探测的结论。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortStatus {
    /// 端口空闲（探测用的监听器已经释放）。
    Free,
    /// 已被占用。
    InUse,
    /// 探测失败，原因给人看。
    Unknown(String),
}

/// 探测一个端口当前能否被监听。
pub fn probe(port: u16) -> PortStatus {
    match TcpListener::bind((Ipv4Addr::UNSPECIFIED, port)) {
        Ok(listener) => {
            drop(listener);
            PortStatus::Free
        }
        Err(err) if err.kind() == io::ErrorKind::AddrInUse => PortStatus::InUse,
        Err(err) => PortStatus::Unknown(err.to_string()),
    }
}

/// 冲突时写进实例 `Failed` 详情的一句话。
pub fn conflict_message(port: u16) -> String {
    format!(
        "端口 {port} 已被占用（可能是启动器自己管理的另一个实例，也可能是别的程序）；\
         启动器不自动换端口，请先释放它或改登记里的端口"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn busy_port_is_detected_and_freed_after_drop() {
        // 让系统分配一个空闲端口，然后自己占住它。
        let listener = TcpListener::bind((Ipv4Addr::UNSPECIFIED, 0)).expect("应能绑临时端口");
        let port = listener.local_addr().expect("应能读本地地址").port();
        assert_eq!(probe(port), PortStatus::InUse);
        drop(listener);
        assert_eq!(probe(port), PortStatus::Free);
    }

    #[test]
    fn conflict_message_names_the_port() {
        let message = conflict_message(39412);
        assert!(message.contains("39412"), "{message}");
        assert!(message.contains("不自动换端口"), "{message}");
    }
}
