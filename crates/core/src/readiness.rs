//! readiness：从实例 stdout 里抓 dsh Web UI 的 token URL（docs/structure.md §3.6）。
//!
//! 【事实】实际输出行（`0.2.1-rc.2-main\logs\start-2026-10-19.log`）：
//!
//! ```text
//! dsh web: http://127.0.0.1:39413/?token=<43 字符 URL-safe token>
//! dsh web: opening the default browser; pass --no-open to disable
//! ```
//!
//! 第二行也以 `dsh web: ` 开头，但**不是** URL——所以必须按「前缀 + 合法 URL」整体匹配，
//! 不能只看前缀。
//!
//! 手写扫描而不是引 `regex`：模式是固定的，依赖越少越好（§2.3）。
//!
//! token 是会话令牌，属本机受控数据：本模块只解析，调用方负责别把它写进不该写的地方
//! （实例日志本来就会落盘它，见 §3.10）。

use serde::{Deserialize, Serialize};

/// 前缀（dsh 的 readiness 行以它开头）。
const READY_PREFIX: &str = "dsh web: ";
/// 滚动窗口上限：token URL 一行不到 128 字节，8 KiB 足够覆盖被拆成多个 chunk 的行。
const MAX_TAIL: usize = 8 * 1024;
/// token 至少这么长才算数（真实 token 是 43 字符 URL-safe）。
const MIN_TOKEN_LEN: usize = 16;

/// 解析出的 dsh Web UI 地址。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenUrl {
    /// 完整 URL（含 `?token=`）。
    pub url: String,
    /// URL 里的端口（用于与登记端口比对）。
    pub port: u16,
}

/// 有状态的滚动扫描器：喂字节块，命中就返回。
///
/// 用有状态的原因：readiness 行可能被管道拆成多个读块（§3.10 的读取线程按块读），
/// 单次 `find` 在块边界上会漏。
#[derive(Debug, Default)]
pub struct Scanner {
    tail: String,
}

impl Scanner {
    pub fn new() -> Self {
        Self::default()
    }

    /// 喂一块 stdout 字节；返回**首个**解析结果。
    ///
    /// 命中后不清空窗口（readiness 行后面还有 `opening the default browser` 等行，
    /// 清空没意义；调用方拿到 `Some` 后自己停止调用即可）。
    pub fn feed(&mut self, chunk: &[u8]) -> Option<TokenUrl> {
        let text = String::from_utf8_lossy(chunk);
        self.tail.push_str(&text);
        if self.tail.len() > MAX_TAIL {
            let mut cut = self.tail.len() - MAX_TAIL;
            while cut < self.tail.len() && !self.tail.is_char_boundary(cut) {
                cut += 1;
            }
            self.tail.drain(..cut);
        }
        find_token_url(&self.tail)
    }
}

/// 在任意文本里找 token URL。
///
/// 两个入口都接受：优先「`dsh web: ` 前缀 + URL」，找不到再退化为裸 URL 匹配
/// （兼容启动器日志被前缀装饰 / 版本输出格式微调的情况）。
pub fn find_token_url(text: &str) -> Option<TokenUrl> {
    let mut search_from = 0usize;
    while let Some(offset) = text[search_from..].find(READY_PREFIX) {
        let start = search_from + offset + READY_PREFIX.len();
        if let Some(parsed) = parse_url_at(text, start) {
            return Some(parsed);
        }
        search_from = start;
    }
    let mut search_from = 0usize;
    while let Some(offset) = text[search_from..].find("http://") {
        let start = search_from + offset;
        if let Some(parsed) = parse_url_at(text, start) {
            return Some(parsed);
        }
        search_from = start + "http://".len();
    }
    None
}

/// 从 `start` 开始按 `<prefix>http://<host>:<port>/?token=<token>` 解析。
///
/// host 接受 `localhost` 或**任意具体 IP 字面量**（含 `[::1]` / `[2001:db8::1]`）：
/// v0.3 起实例可以显式 `--host <局域网 IPv4>`，dsh 打印的就是该地址（§3.22）。
/// 域名 / 空 host 一律拒绝——不把任意 stdout 文本当 URL。
fn parse_url_at(text: &str, start: usize) -> Option<TokenUrl> {
    let rest = text.get(start..)?;
    let rest = rest.strip_prefix("http://")?;
    let (authority, path) = rest.split_once('/')?;
    let (host, port) = authority.rsplit_once(':')?;
    if !is_acceptable_host(host) {
        return None;
    }
    let port: u16 = port.parse().ok()?;
    let token = path
        .strip_prefix("?token=")?
        .split(|ch: char| !is_token_char(ch))
        .next()?;
    if token.len() < MIN_TOKEN_LEN {
        return None;
    }
    // URL 里 token 后面一般不跟别的东西；跟了也只取到空白为止（上面已切）。
    let end = start + "http://".len() + authority.len() + 1 + "?token=".len() + token.len();
    let url = text.get(start..end)?.to_string();
    Some(TokenUrl { url, port })
}

fn is_acceptable_host(host: &str) -> bool {
    if host == "localhost" {
        return true;
    }
    let bare = host
        .strip_prefix('[')
        .and_then(|inner| inner.strip_suffix(']'))
        .unwrap_or(host);
    bare.parse::<std::net::IpAddr>().is_ok()
}

fn is_token_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || ch == '-' || ch == '_'
}

#[cfg(test)]
mod tests {
    use super::*;

    fn token() -> String {
        "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopq" // 42
            .to_string()
    }

    #[test]
    fn parses_the_real_readiness_line() {
        let line = format!("dsh web: http://127.0.0.1:39412/?token={}\r\n", token());
        let found = find_token_url(&line).expect("应命中 readiness 行");
        assert_eq!(found.port, 39412);
        assert_eq!(
            found.url,
            format!("http://127.0.0.1:39412/?token={}", token())
        );

        // 后一行不是 URL，不能被误判成新鲜 URL。
        let full =
            format!("{line}dsh web: opening the default browser; pass --no-open to disable\r\n");
        assert_eq!(find_token_url(&full), Some(found));
    }

    #[test]
    fn ignores_non_url_dsh_lines() {
        assert_eq!(
            find_token_url("dsh web: opening the default browser; pass --no-open to disable"),
            None
        );
        assert_eq!(find_token_url("dsh web: http://127.0.0.1:39412/"), None);
        assert_eq!(
            find_token_url("dsh web: http://example.com:80/?token=abcdefghijklmnopqrst"),
            None
        );
    }

    #[test]
    fn accepts_localhost_and_token_punctuation() {
        let found = find_token_url("dsh web: http://localhost:39412/?token=ab-cd_ef-gh_ij-kl_mn")
            .expect("localhost 也是本机地址");
        assert_eq!(found.port, 39412);
        assert_eq!(
            found.url,
            "http://localhost:39412/?token=ab-cd_ef-gh_ij-kl_mn"
        );
    }

    #[test]
    fn accepts_a_lan_ip_host_printed_after_explicit_bind() {
        // v0.3：`--host 192.168.3.192` 时 dsh 打印的就是这个 host（§3.22）。
        let line = format!("dsh web: http://192.168.3.192:39412/?token={}\r\n", token());
        let found = find_token_url(&line).expect("局域网 IP 也要认");
        assert_eq!(found.port, 39412);
        assert_eq!(
            found.url,
            format!("http://192.168.3.192:39412/?token={}", token())
        );
    }

    #[test]
    fn accepts_bracketed_ipv6_but_rejects_domains() {
        let found = find_token_url("dsh web: http://[::1]:39412/?token=abcdefghijklmnopqrst")
            .expect("[::1] 是合法的本机地址");
        assert_eq!(found.port, 39412);

        assert_eq!(
            find_token_url("dsh web: http://capsule.local:39412/?token=abcdefghijklmnopqrst"),
            None,
            "域名不是 dsh 的绑定形态，别把任意文本当 URL"
        );
    }

    #[test]
    fn rejects_short_tokens() {
        assert_eq!(
            find_token_url("dsh web: http://127.0.0.1:39412/?token=short"),
            None
        );
    }

    #[test]
    fn scanner_handles_split_chunks() {
        let line = format!("dsh web: http://127.0.0.1:39412/?token={}\r\n", token());
        let bytes = line.as_bytes();
        let split = 20; // 切在 URL 中间
        let mut scanner = Scanner::new();
        assert_eq!(scanner.feed(&bytes[..split]), None);
        let found = scanner.feed(&bytes[split..]).expect("跨块也要命中");
        assert_eq!(found.port, 39412);
    }

    #[test]
    fn scanner_window_is_bounded() {
        let mut scanner = Scanner::new();
        for _ in 0..1000 {
            assert_eq!(scanner.feed(b"xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx\r\n"), None);
        }
        assert!(scanner.tail.len() <= MAX_TAIL);
        let line = format!("dsh web: http://127.0.0.1:39412/?token={}\r\n", token());
        assert!(scanner.feed(line.as_bytes()).is_some());
    }

    #[test]
    fn bare_url_fallback_works() {
        let found = find_token_url(&format!(
            "[dsh-housing] 已就绪 http://127.0.0.1:39412/?token={}",
            token()
        ))
        .expect("退化的裸 URL 也要能认");
        assert_eq!(found.port, 39412);
    }
}
