//! 本机地址与绑定 host 的小工具（docs/structure.md §3.22 / §3.23）。
//!
//! 三件事只在这里有一份实现：
//!
//! 1. **本机局域网 IPv4 候选**：`{lan_url}` 探测（`notify.rs`）、控制台「使用局域网地址」
//!    快捷键（`runtime.rs`）与控制台 `Origin` 白名单（`console.rs`）都用它。首选默认路由那块
//!    网卡——「去公网默认走哪块网卡」才是同一局域网里的设备能访问到的地址；回环 / 未指定 /
//!    链路本地一律排除。
//! 2. **实例 host 的校验**：dsh 0.2.x 只接受具体 IPv4 / IPv6 字面量，明确拒绝 wildcard
//!    （`0.0.0.0` / `::`）。这里把规则写成一处，控制台保存与 `env.rs` 组装命令行共用。
//! 3. **控制台绑定地址的校验与探测（v0.4 / #286）**：控制台是启动器自己的 HTTP 服务，
//!    规则与实例 host 不同——它**接受** `0.0.0.0`（「局域网都能访问」的正当写法），但只收
//!    IPv4 字面量，并且写盘前要能真绑上。

use std::net::{IpAddr, Ipv4Addr, TcpListener, ToSocketAddrs, UdpSocket};
use std::sync::OnceLock;

use crate::diag;

/// 最多返回几个候选地址（多网卡 + VPN 时不至于一个个等下去）。
pub const MAX_CANDIDATES: usize = 4;

/// 「去公网默认走哪块网卡」——UDP `connect` 只做一次路由选择，不发送任何数据包。
///
/// `223.5.5.5` 只是路由选择用的目的地，不联网、不发送。
pub fn default_route_ipv4() -> Option<Ipv4Addr> {
    let socket = UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("223.5.5.5:53").ok()?;
    match socket.local_addr().ok()?.ip() {
        IpAddr::V4(v4) if is_lan_candidate(IpAddr::V4(v4)) => Some(v4),
        _ => None,
    }
}

/// 控制台 UI 用的「首选局域网地址」（进程内缓存一次，避免每次 `/api/state` 都做名字解析）。
///
/// 与 `{lan_url}` 的探测不同：这里只给「使用局域网地址」快捷键一个候选，不保证对方可达；
/// 真正投递时仍按 `lan_candidates()` 逐个探测。
pub fn preferred_lan_ipv4() -> Option<IpAddr> {
    static CACHE: OnceLock<Option<IpAddr>> = OnceLock::new();
    *CACHE.get_or_init(|| lan_candidates().first().copied())
}

/// 本机局域网候选地址：默认路由地址优先，再补本机名解析出来、且能作为 LAN URL 的地址。
///
/// 拿不到默认路由（纯离线局域网）时只退回名字解析结果；都没有就返回空表并告警。
pub fn lan_candidates() -> Vec<IpAddr> {
    let mut addresses: Vec<IpAddr> = Vec::new();
    if let Some(primary) = default_route_ipv4() {
        addresses.push(IpAddr::V4(primary));
    }
    if let Ok(host) = std::env::var("COMPUTERNAME") {
        if !host.trim().is_empty() {
            match (host.as_str(), 0u16).to_socket_addrs() {
                Ok(resolved) => {
                    for addr in resolved {
                        let ip = addr.ip();
                        if is_lan_candidate(ip) && !addresses.contains(&ip) {
                            addresses.push(ip);
                        }
                    }
                }
                Err(err) => diag::warn(&format!(
                    "解析本机名 {host} 失败（{err}），只用默认路由地址"
                )),
            }
        }
    }
    if addresses.is_empty() {
        diag::warn("找不到本机局域网 IPv4");
    }
    // 排序（不丢地址）：192.168/16 → 10/8 → 172.16/12 → 其它。
    //
    // 这是个**可翻案的启发式**：本机实测有多张网卡 + aTrust VPN，默认路由是 VPN 的
    // `10.230.x.x`，而手机在同一局域网能访问到的是 WLAN 的 `192.168.3.192`。若把
    // 默认路由排在第一位，dsh 绑 `0.0.0.0` 时 `{lan_url}` 会先探到 VPN 地址并采用它
    // （对端手机根本连不上）。家庭 / 小办公室网段绝大多数是 192.168/16，所以先试它；
    // 探测仍是「连得上才采用」，真正的连通性判定没变（§3.21 / §5.12 D30）。
    addresses.sort_by_key(|ip| lan_rank(*ip));
    addresses.truncate(MAX_CANDIDATES);
    addresses
}

/// LAN 候选的优先级（数字越小越先试）。
fn lan_rank(ip: IpAddr) -> u8 {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            match (a, b) {
                (192, 168) => 0,
                (10, _) => 1,
                (172, b) if (16..=31).contains(&b) => 2,
                _ => 3,
            }
        }
        IpAddr::V6(_) => 4,
    }
}

/// 能不能出现在局域网 URL 里：只认 IPv4，排除回环 / 未指定 / 链路本地 / 广播 / 组播。
///
/// IPv6 暂不支持：拼 URL 要方括号，模板与探测都只为 IPv4 设计（与 §3.21 的口径一致）。
pub fn is_lan_candidate(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            !v4.is_loopback()
                && !v4.is_unspecified()
                && !v4.is_link_local()
                && !v4.is_broadcast()
                && !v4.is_multicast()
        }
        IpAddr::V6(_) => false,
    }
}

/// 校验并规范化实例 host；`None` / 空串 = 不显式绑定（调用方决定「跟随胶囊配置」）。
///
/// dsh 的 `webserver` 配置与 `--host` 都只接受**具体 IP 字面量**：
///
/// - 拒绝域名 / 空 host（dsh 会直接报 `not a concrete IPv4 or IPv6 address literal`）；
/// - 拒绝 wildcard `0.0.0.0` / `::`（dsh 明确拒绝：绑所有网卡 = 把 RCE 暴露到网络）；
/// - 拒绝组播 / 广播这类不可能是本机绑定地址的值。
pub fn normalize_instance_host(value: &str) -> Result<Option<String>, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Ok(None);
    }
    let ip: IpAddr = trimmed
        .parse()
        .map_err(|_| format!("host 必须是具体 IP 字面量（IPv4 / IPv6），不是 {trimmed:?}"))?;
    if ip.is_unspecified() {
        return Err(format!(
            "host 不能是通配地址 {ip}：dsh 明确拒绝绑所有网卡；\
             局域网可达请填本机具体 IPv4（控制台的「使用局域网地址」会填好）"
        ));
    }
    if ip.is_multicast() || matches!(ip, IpAddr::V4(v4) if v4.is_broadcast()) {
        return Err(format!("host {ip} 不是合法的本机绑定地址"));
    }
    Ok(Some(ip.to_string()))
}

/// 校验控制台绑定地址（v0.4 / #286）：只接受 IPv4 字面量。
///
/// 与 [`normalize_instance_host`] 的差别是有意的：实例 host 是 **dsh 的监听地址**，dsh 拒绝
/// 通配是为了防 RCE；这里是**启动器自己的控制台**，`0.0.0.0` 恰恰是「局域网都能访问」的
/// 标准写法，所以它必须合法。反过来，域名（`localhost`）/ IPv6 一律拒绝：`{lan_url}` 的探测
/// 与 `Origin` 白名单都只为 IPv4 设计（§3.21 / §3.23）。
pub fn normalize_console_host(value: &str) -> Result<Ipv4Addr, String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(
            "控制台启动位置不能为空：默认 127.0.0.1（仅本机）；要让局域网访问填 0.0.0.0"
                .to_string(),
        );
    }
    let ip: Ipv4Addr = trimmed.parse().map_err(|_| {
        format!(
            "控制台启动位置必须是 IPv4 字面量（例如 127.0.0.1 / 0.0.0.0 / 192.168.3.192），\
             不是 {trimmed:?}"
        )
    })?;
    if ip.is_multicast() || ip.is_broadcast() {
        return Err(format!("控制台启动位置 {ip} 不是合法的本机绑定地址"));
    }
    Ok(ip)
}

/// 这个地址**现在**能不能在本机绑定（v0.4 / #286：写盘前的第二道门）。
///
/// 只做 `bind((ip, 0))` 再立刻释放，什么也不监听：它证明地址属于本机、网卡在线。
/// 为什么必须在写盘前做——启动位置写错 = 下次 core 起来后控制台直接消失，而控制台是
/// 唯一的设置入口（手改 `settings.json` 不算可用路径，见 §3.23 / D33）。
pub fn probe_console_bind(ip: Ipv4Addr) -> Result<(), String> {
    TcpListener::bind((ip, 0)).map(drop).map_err(|err| {
        format!(
            "控制台启动位置 {ip} 现在绑不上（{err}）：填本机确实拥有的 IPv4，\
             或用 0.0.0.0 表示所有网卡"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn loopback_and_lan_are_valid_but_wildcard_is_not() {
        assert_eq!(
            normalize_instance_host("127.0.0.1").unwrap(),
            Some("127.0.0.1".to_string())
        );
        assert_eq!(
            normalize_instance_host(" 192.168.3.192 ").unwrap(),
            Some("192.168.3.192".to_string())
        );
        assert_eq!(
            normalize_instance_host("::1").unwrap(),
            Some("::1".to_string())
        );
        assert_eq!(normalize_instance_host("").unwrap(), None);
        assert_eq!(normalize_instance_host("   ").unwrap(), None);

        for wildcard in ["0.0.0.0", "::"] {
            let error = normalize_instance_host(wildcard).expect_err("wildcard 必须被拒");
            assert!(error.contains("通配"), "{error}");
        }
        assert!(normalize_instance_host("localhost").is_err());
        assert!(normalize_instance_host("example.test").is_err());
        assert!(normalize_instance_host("224.0.0.1").is_err());
    }

    #[test]
    fn lan_rank_prefers_home_lan_then_private_then_vpn() {
        let rank = |ip: &str| lan_rank(ip.parse().unwrap());
        assert!(
            rank("192.168.3.192") < rank("10.230.133.47"),
            "VPN 排在家用网段之后"
        );
        assert!(rank("10.0.0.5") < rank("172.17.0.1"));
        assert!(rank("172.17.0.1") < rank("8.8.8.8"));
    }

    #[test]
    fn lan_candidates_never_include_loopback_or_link_local() {
        assert!(!is_lan_candidate("127.0.0.1".parse().unwrap()));
        assert!(!is_lan_candidate("169.254.10.10".parse().unwrap()));
        assert!(!is_lan_candidate("0.0.0.0".parse().unwrap()));
        assert!(!is_lan_candidate("::1".parse().unwrap()));
        assert!(is_lan_candidate("192.168.3.100".parse().unwrap()));
        assert!(is_lan_candidate("10.0.0.5".parse().unwrap()));
    }

    #[test]
    fn default_route_address_is_a_real_lan_candidate() {
        // 有默认路由时它必须是「能出现在 URL 里」的地址；离线机器返回 None 也算通过。
        if let Some(ip) = default_route_ipv4() {
            assert!(
                is_lan_candidate(IpAddr::V4(ip)),
                "{ip} 不该被当成局域网地址"
            );
            assert!(
                !ip.to_string().starts_with("169.254."),
                "{ip} 是链路本地地址"
            );
        }
    }

    #[test]
    fn console_host_accepts_wildcard_but_not_domains_or_ipv6() {
        // 与实例 host 的差别是刻意的：控制台的「局域网可访问」就是 0.0.0.0。
        assert_eq!(
            normalize_console_host("127.0.0.1").unwrap(),
            Ipv4Addr::LOCALHOST
        );
        assert!(normalize_console_host("0.0.0.0").unwrap().is_unspecified());
        assert_eq!(
            normalize_console_host(" 192.168.3.192 ").unwrap(),
            Ipv4Addr::new(192, 168, 3, 192)
        );

        for bad in [
            "",
            "   ",
            "localhost",
            "console.local",
            "::1",
            "224.0.0.1",
            "255.255.255.255",
        ] {
            assert!(normalize_console_host(bad).is_err(), "{bad} 应被拒");
        }
        let error = normalize_console_host("localhost").expect_err("域名不是字面量");
        assert!(error.contains("控制台启动位置"), "{error}");
    }

    #[test]
    fn console_bind_probe_rejects_addresses_this_machine_does_not_own() {
        // 回环与通配永远可绑；TEST-NET-3（203.0.113.0/24，RFC 5737 文档专用）不该属于任何网卡。
        assert!(probe_console_bind(Ipv4Addr::LOCALHOST).is_ok());
        assert!(probe_console_bind(Ipv4Addr::UNSPECIFIED).is_ok());
        let error = probe_console_bind(Ipv4Addr::new(203, 0, 113, 9))
            .expect_err("本机不拥有的地址必须绑不上");
        assert!(error.contains("203.0.113.9"), "{error}");
    }
}
