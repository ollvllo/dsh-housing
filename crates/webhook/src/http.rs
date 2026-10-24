//! WinHTTP 的最小 POST 客户端（docs/structure.md §5.11 D22）。

use std::ffi::c_void;

use windows::core::PCWSTR;
use windows::Win32::Networking::WinHttp::{
    WinHttpCloseHandle, WinHttpConnect, WinHttpOpen, WinHttpOpenRequest, WinHttpQueryHeaders,
    WinHttpReadData, WinHttpReceiveResponse, WinHttpSendRequest, WinHttpSetTimeouts,
    WINHTTP_ACCESS_TYPE_DEFAULT_PROXY, WINHTTP_ACCESS_TYPE_NO_PROXY, WINHTTP_FLAG_SECURE,
    WINHTTP_OPEN_REQUEST_FLAGS, WINHTTP_QUERY_FLAG_NUMBER, WINHTTP_QUERY_STATUS_CODE,
};

/// User-Agent（端点侧能看出是谁发的）。
const USER_AGENT: &str = "dsh-housing-webhook/0.1";

/// 响应体最多读多少字节（只是为了把连接干净地收掉，不解析内容）。
const MAX_RESPONSE_BYTES: usize = 64 * 1024;

/// 一次投递的超时（毫秒）：解析 / 连接 / 发送 / 接收。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Timeouts {
    pub resolve_ms: i32,
    pub connect_ms: i32,
    pub send_ms: i32,
    pub receive_ms: i32,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            resolve_ms: 2_000,
            connect_ms: 3_000,
            send_ms: 5_000,
            receive_ms: 5_000,
        }
    }
}

/// 一次 POST 的结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Outcome {
    /// HTTP 状态码。
    pub status: u32,
}

impl Outcome {
    /// 2xx 才算投递成功。
    pub fn is_success(self) -> bool {
        (200..300).contains(&self.status)
    }
}

/// 发起一次 `POST`：正文按 UTF-8 原样发（`text/plain; charset=utf-8`）。
pub fn post_text(url: &str, body: &str, timeouts: Timeouts) -> Result<Outcome, String> {
    let target = Target::parse(url)?;
    let body_bytes = body.as_bytes();
    if body_bytes.len() > u32::MAX as usize {
        return Err("正文太大".to_string());
    }

    let headers = format!(
        "Content-Type: text/plain; charset=utf-8\r\nContent-Length: {}\r\n",
        body_bytes.len()
    );
    let host = wide(&target.host);
    let path = wide(&target.path);
    let verb = wide("POST");
    let agent = wide(USER_AGENT);
    let headers: Vec<u16> = headers.encode_utf16().collect();

    // SAFETY: 所有宽字符串缓冲区都在本函数作用域内存活到对应调用结束；句柄由
    // `Handle` 的 Drop 统一关闭（包括中途 return 的错误路径）。
    unsafe {
        let access = if is_loopback_host(&target.host) {
            // 回环地址永远不该走代理。
            WINHTTP_ACCESS_TYPE_NO_PROXY
        } else {
            WINHTTP_ACCESS_TYPE_DEFAULT_PROXY
        };
        let session = Handle::new(WinHttpOpen(
            PCWSTR(agent.as_ptr()),
            access,
            PCWSTR::null(),
            PCWSTR::null(),
            0,
        ))
        .ok_or_else(|| last_error("WinHttpOpen"))?;
        WinHttpSetTimeouts(
            session.0,
            timeouts.resolve_ms,
            timeouts.connect_ms,
            timeouts.send_ms,
            timeouts.receive_ms,
        )
        .map_err(|err| format_win32("WinHttpSetTimeouts", err))?;

        let connection = Handle::new(WinHttpConnect(
            session.0,
            PCWSTR(host.as_ptr()),
            target.port,
            0,
        ))
        .ok_or_else(|| last_error("WinHttpConnect"))?;

        let flags = if target.secure {
            WINHTTP_FLAG_SECURE
        } else {
            WINHTTP_OPEN_REQUEST_FLAGS(0)
        };
        let request = Handle::new(WinHttpOpenRequest(
            connection.0,
            PCWSTR(verb.as_ptr()),
            PCWSTR(path.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            std::ptr::null(),
            flags,
        ))
        .ok_or_else(|| last_error("WinHttpOpenRequest"))?;

        WinHttpSendRequest(
            request.0,
            Some(&headers[..]),
            Some(body_bytes.as_ptr() as *const c_void),
            body_bytes.len() as u32,
            0,
            0,
        )
        .map_err(|err| format_win32("WinHttpSendRequest", err))?;

        WinHttpReceiveResponse(request.0, std::ptr::null_mut())
            .map_err(|err| format_win32("WinHttpReceiveResponse", err))?;

        let mut status: u32 = 0;
        let mut length = std::mem::size_of::<u32>() as u32;
        WinHttpQueryHeaders(
            request.0,
            WINHTTP_QUERY_STATUS_CODE | WINHTTP_QUERY_FLAG_NUMBER,
            PCWSTR::null(),
            Some(&mut status as *mut u32 as *mut c_void),
            &mut length,
            std::ptr::null_mut(),
        )
        .map_err(|err| format_win32("WinHttpQueryHeaders(状态码)", err))?;

        // 把响应体读掉：不读就关句柄会让服务端看到 RST，也拿不到「端点到底回了什么」。
        drain_body(request.0);

        Ok(Outcome { status })
    }
}

/// 有界地把响应体读干净（失败无所谓，只影响连接关得干不干净）。
unsafe fn drain_body(request: *mut c_void) {
    let mut buffer = [0u8; 4096];
    let mut total = 0usize;
    loop {
        let mut read: u32 = 0;
        if WinHttpReadData(
            request,
            buffer.as_mut_ptr() as *mut c_void,
            buffer.len() as u32,
            &mut read,
        )
        .is_err()
        {
            return;
        }
        if read == 0 {
            return;
        }
        total += read as usize;
        if total >= MAX_RESPONSE_BYTES {
            return;
        }
    }
}

/// 关闭 WinHTTP 句柄的守卫（提前 return / 错误路径都不漏）。
struct Handle(*mut c_void);

impl Handle {
    /// `h` 为空指针表示调用失败；返回 `None` 由调用方报错。
    fn new(h: *mut c_void) -> Option<Self> {
        (!h.is_null()).then_some(Self(h))
    }
}

impl Drop for Handle {
    fn drop(&mut self) {
        // SAFETY: 句柄由本结构独占，Drop 只会执行一次。
        unsafe {
            let _ = WinHttpCloseHandle(self.0);
        }
    }
}

fn last_error(stage: &str) -> String {
    // WinHTTP 的失败用 GetLastError 报告，`from_win32` 读的就是它。
    format_win32(stage, windows::core::Error::from_win32())
}

/// C 字符串口径的宽字符串（**带结尾 NUL**）。
///
/// `WinHttpOpen` / `WinHttpConnect` / `WinHttpOpenRequest` 的字符串参数都是 NUL 结尾的
/// `LPCWSTR`；少写这个 NUL 会让 WinHTTP 读到缓冲区外面（实测现象是 `WinHttpConnect`
/// 报 `ERROR_INVALID_HANDLE`，极难从错误码反推）。
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn format_win32(stage: &str, err: windows::core::Error) -> String {
    format!("{stage} 失败：{err}（0x{:08X}）", err.code().0 as u32)
}

fn is_loopback_host(host: &str) -> bool {
    let lower = host.to_ascii_lowercase();
    lower == "localhost" || lower == "127.0.0.1" || lower == "::1" || lower.ends_with(".localhost")
}

/// 解析出来的请求目标（`WinHttpConnect` 要 host + port，`WinHttpOpenRequest` 要 path）。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Target {
    secure: bool,
    host: String,
    port: u16,
    /// 以 `/` 开头的路径 + 查询串（不含 scheme / host）。
    path: String,
}

impl Target {
    fn parse(url: &str) -> Result<Self, String> {
        let trimmed = url.trim();
        let (scheme, rest) = trimmed
            .split_once("://")
            .ok_or_else(|| format!("URL 缺少 scheme：{trimmed}"))?;
        let secure = match scheme.to_ascii_lowercase().as_str() {
            "http" => false,
            "https" => true,
            other => return Err(format!("不支持的 scheme：{other}")),
        };

        // authority 到第一个 '/' / '?' / '#' 为止，剩下的都是 path。
        let authority_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let authority = &rest[..authority_end];
        if authority.is_empty() {
            return Err(format!("URL 缺少 host：{trimmed}"));
        }
        let mut tail = rest[authority_end..].to_string();
        if tail.is_empty() {
            tail.push('/');
        } else if tail.starts_with('?') || tail.starts_with('#') {
            tail.insert(0, '/');
        }

        // host[:port]，IPv6 必须带方括号（WinHTTP 也这么要求）。
        let (host, port_text) = if let Some(inner) = authority.strip_prefix('[') {
            let (host, after) = inner
                .split_once(']')
                .ok_or_else(|| format!("IPv6 地址少了 ']'：{authority}"))?;
            let port = after.strip_prefix(':');
            (host.to_string(), port)
        } else {
            match authority.rsplit_once(':') {
                Some((host, port)) => (host.to_string(), Some(port)),
                None => (authority.to_string(), None),
            }
        };
        if host.is_empty() {
            return Err(format!("URL 缺少 host：{trimmed}"));
        }
        let port = match port_text {
            Some(text) => text
                .parse::<u16>()
                .map_err(|_| format!("端口不合法：{text}"))?,
            None => {
                if secure {
                    443
                } else {
                    80
                }
            }
        };
        if port == 0 {
            return Err("端口不能是 0".to_string());
        }

        Ok(Self {
            secure,
            host,
            port,
            path: tail,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{Shutdown, TcpListener};
    use std::sync::{Arc, Mutex};
    use std::thread;

    #[test]
    fn target_parses_the_shapes_we_actually_get() {
        assert_eq!(
            Target::parse("https://ntfy.example.test/topic").unwrap(),
            Target {
                secure: true,
                host: "ntfy.example.test".to_string(),
                port: 443,
                path: "/topic".to_string()
            }
        );
        assert_eq!(
            Target::parse("http://127.0.0.1:8443/hook?a=1").unwrap(),
            Target {
                secure: false,
                host: "127.0.0.1".to_string(),
                port: 8443,
                path: "/hook?a=1".to_string()
            }
        );
        assert_eq!(
            Target::parse("HTTP://example.test").unwrap().path,
            "/",
            "没有路径要补一个/"
        );
        assert_eq!(
            Target::parse("http://example.test?a=1").unwrap().path,
            "/?a=1"
        );
        assert_eq!(
            Target::parse("https://[::1]:9443/x").unwrap(),
            Target {
                secure: true,
                host: "::1".to_string(),
                port: 9443,
                path: "/x".to_string()
            }
        );

        assert!(Target::parse("ftp://example.test/x").is_err());
        assert!(Target::parse("example.test/x").is_err());
        assert!(Target::parse("http://:8080/").is_err());
        assert!(Target::parse("http://example.test:0/").is_err());
    }

    #[test]
    fn loopback_hosts_skip_the_proxy() {
        assert!(is_loopback_host("127.0.0.1"));
        assert!(is_loopback_host("LOCALHOST"));
        assert!(is_loopback_host("::1"));
        assert!(!is_loopback_host("192.168.3.192"));
        assert!(!is_loopback_host("ntfy.example.test"));
    }

    /// 起一个只收一次请求的本地 HTTP 服务，返回 (端口, 收到的原始字节)。
    fn one_shot_server(
        response: &'static str,
    ) -> (u16, Arc<Mutex<Vec<u8>>>, thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("绑定本地端口");
        let port = listener.local_addr().unwrap().port();
        let seen = Arc::new(Mutex::new(Vec::new()));
        let sink = seen.clone();
        let handle = thread::spawn(move || {
            let Ok((mut stream, _)) = listener.accept() else {
                return;
            };
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .ok();
            let mut buffer = Vec::new();
            let mut chunk = [0u8; 1024];
            loop {
                match stream.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => buffer.extend_from_slice(&chunk[..n]),
                }
                if let Some(end) = find(&buffer, b"\r\n\r\n") {
                    let head = String::from_utf8_lossy(&buffer[..end]).to_ascii_lowercase();
                    let length = head
                        .lines()
                        .find_map(|line| line.strip_prefix("content-length:"))
                        .and_then(|value| value.trim().parse::<usize>().ok())
                        .unwrap_or(0);
                    if buffer.len() >= end + 4 + length {
                        break;
                    }
                }
            }
            *sink.lock().unwrap() = buffer;
            let _ = stream.write_all(response.as_bytes());
            let _ = stream.flush();
            let _ = stream.shutdown(Shutdown::Both);
        });
        (port, seen, handle)
    }

    fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
        haystack
            .windows(needle.len())
            .position(|window| window == needle)
    }

    #[test]
    fn post_reaches_a_local_server_with_the_rendered_body() {
        // 走真实 WinHTTP → 真实 TCP（只在本机回环，不出机器）。
        let (port, seen, server) = one_shot_server(
            "HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok",
        );
        let url = format!("http://127.0.0.1:{port}/hook");
        let outcome = post_text(&url, "Service Start\nName: 测试", Timeouts::default())
            .expect("本地投递应当成功");
        assert_eq!(outcome, Outcome { status: 200 });
        assert!(outcome.is_success());

        server.join().expect("服务端线程应当结束");
        let raw = seen.lock().unwrap().clone();
        let text = String::from_utf8_lossy(&raw);
        assert!(text.starts_with("POST /hook HTTP/1.1"), "{text}");
        assert!(
            text.contains("Content-Type: text/plain; charset=utf-8"),
            "{text}"
        );
        assert!(
            text.contains(&format!(
                "Content-Length: {}",
                "Service Start\nName: 测试".len()
            )),
            "{text}"
        );
        assert!(text.ends_with("Service Start\nName: 测试"), "{text}");
    }

    #[test]
    fn non_2xx_status_is_reported_but_not_an_error() {
        let (port, _seen, server) = one_shot_server(
            "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        );
        let url = format!("http://127.0.0.1:{port}/missing");
        let outcome = post_text(&url, "x", Timeouts::default()).expect("HTTP 层应当成功");
        assert_eq!(outcome.status, 404);
        assert!(!outcome.is_success());
        server.join().ok();
    }

    #[test]
    fn closed_port_is_a_transport_error() {
        // 绑一个端口再立刻关掉，拿一个几乎肯定没人听的端口。
        let port = {
            let listener = TcpListener::bind("127.0.0.1:0").unwrap();
            listener.local_addr().unwrap().port()
        };
        let url = format!("http://127.0.0.1:{port}/hook");
        let error = post_text(&url, "x", Timeouts::default()).expect_err("没有人听就必须报错");
        assert!(!error.is_empty());
    }
}
