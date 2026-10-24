//! 控制台的服务端胶水（docs/structure.md §2.5 / §3.17 / §3.23）。
//!
//! 这里只做四件事：
//!
//! 1. 每次启动生成一个新的 `consoleToken`（`BCryptGenRandom(32 字节)` → 64 个 hex 字符）；
//! 2. 按 `settings.console_host` 决定绑哪张网卡（v0.4 / #286；非法值回退回环并告警）；
//! 3. 绑非回环时把本机局域网地址交给 `gui` 当 `Origin` 白名单（页面是从那些地址打开的）；
//! 4. 把 [`ConsoleConfig`] 与 core 的 `ConsoleBackend` 实现交给 `gui` crate 起 HTTP 线程，
//!    对外提供「控制台 URL」与端口。
//!
//! **控制台起不来不阻断 core**（§5.8 D9）：绑定失败只返回 `Err`，由 `runtime.rs` 记一行
//! 告警，实例管理照常。

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;

use dsh_housing_gui::{ConsoleBackend, ConsoleConfig, ConsoleHandle};
use windows::Win32::Security::Cryptography::{
    BCryptGenRandom, BCRYPT_ALG_HANDLE, BCRYPT_USE_SYSTEM_PREFERRED_RNG,
};

use crate::model::Settings;
use crate::{diag, net};

/// token 字节数（hex 之后 64 个字符）。
const TOKEN_BYTES: usize = 32;

/// 已启动的控制台。
pub struct ConsoleService {
    bind: SocketAddr,
    token: String,
    handle: ConsoleHandle,
}

impl ConsoleService {
    /// 实际监听端口（`settings.gui_port = 0` 时由系统分配）。
    pub fn port(&self) -> u16 {
        self.bind.port()
    }

    /// 实际绑定的地址（诊断日志用；不拼 token）。
    pub fn host(&self) -> IpAddr {
        self.bind.ip()
    }

    /// 带 token 的控制台 URL（只交给「打开控制台」与 IPC `console_info`，不落盘）。
    ///
    /// 绑 `0.0.0.0` 时给 `127.0.0.1`：本机浏览器直接能开，没必要让用户点开 `0.0.0.0`；
    /// 绑具体 IPv4 时只能用那个地址（回环在这个档下不可达）。
    pub fn url(&self) -> String {
        console_url(self.bind, &self.token)
    }

    /// 主动停掉 HTTP 线程（目前只有单测与将来的「换端口重启」会用）。
    pub fn stop(self) {
        self.handle.stop();
    }
}

/// 启动控制台：解析绑定地址 → 生成 token → 绑定端口 → 起 HTTP 线程。
pub fn start(
    settings: &Settings,
    backend: Arc<dyn ConsoleBackend>,
) -> Result<ConsoleService, String> {
    let host = match net::normalize_console_host(&settings.console_host) {
        Ok(ip) => ip,
        Err(err) => {
            // 手改坏了的 settings.json 不该让控制台彻底消失（§3.23 / D35）。
            diag::warn(&format!("{err}；控制台回退到 127.0.0.1"));
            Ipv4Addr::LOCALHOST
        }
    };
    let token = random_token()?;
    let mut config = ConsoleConfig::on(SocketAddr::from((host, settings.gui_port)), token.clone());
    if !host.is_loopback() {
        // 局域网档：页面的 `Origin` 是 `http://<本机某张网卡的地址>:<port>`。
        config.origin_hosts = net::lan_candidates();
    }
    let handle = dsh_housing_gui::serve(config, backend).map_err(|err| err.to_string())?;
    let bind = handle.addr();
    Ok(ConsoleService {
        bind,
        token,
        handle,
    })
}

/// 拼控制台 URL：绑定通配地址时用回环，其余用实际绑定地址（v0.4 / §3.23）。
fn console_url(bind: SocketAddr, token: &str) -> String {
    let host = if bind.ip().is_unspecified() {
        IpAddr::V4(Ipv4Addr::LOCALHOST)
    } else {
        bind.ip()
    };
    format!("http://{host}:{}/?token={token}", bind.port())
}

/// 32 字节密码学随机数 → 64 个 hex 字符。
fn random_token() -> Result<String, String> {
    let mut bytes = [0u8; TOKEN_BYTES];
    // SAFETY: 缓冲区由本函数持有；BCRYPT_USE_SYSTEM_PREFERRED_RNG 表示用系统首选 RNG，
    // 此时算法句柄必须为空（`None::<&BCRYPT_ALG_HANDLE>`）。
    let status = unsafe {
        BCryptGenRandom(
            None::<&BCRYPT_ALG_HANDLE>,
            &mut bytes,
            BCRYPT_USE_SYSTEM_PREFERRED_RNG,
        )
    };
    status
        .ok()
        .map_err(|err| format!("生成 consoleToken 失败（BCryptGenRandom）：{err}"))?;
    let mut token = String::with_capacity(TOKEN_BYTES * 2);
    for byte in bytes {
        use std::fmt::Write as _;
        let _ = write!(token, "{byte:02x}");
    }
    Ok(token)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tokens_are_64_hex_chars_and_unique() {
        let first = random_token().expect("系统 RNG 应可用");
        let second = random_token().expect("系统 RNG 应可用");
        assert_eq!(first.len(), 64);
        assert!(first.chars().all(|ch| ch.is_ascii_hexdigit()));
        assert_ne!(first, second, "每次启动都应换一个新 token");
    }

    #[test]
    fn console_url_follows_the_bind_address() {
        // v0.4 / #286：通配地址给回环（本机浏览器能直接开），具体地址照原样用。
        assert_eq!(
            console_url("0.0.0.0:30010".parse().unwrap(), "abc"),
            "http://127.0.0.1:30010/?token=abc"
        );
        assert_eq!(
            console_url("127.0.0.1:30010".parse().unwrap(), "abc"),
            "http://127.0.0.1:30010/?token=abc"
        );
        assert_eq!(
            console_url("192.168.3.192:30010".parse().unwrap(), "abc"),
            "http://192.168.3.192:30010/?token=abc"
        );
    }
}
