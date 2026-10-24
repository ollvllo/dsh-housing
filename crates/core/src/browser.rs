//! 用默认浏览器打开 dsh 的 Web UI（docs/structure.md §3.16）。
//!
//! 两个触发点、一条实现：`settings.auto_open_browser`（readiness 首次成功时开一次）与
//! 托盘的「打开网页」（IPC `open_browser`）。
//!
//! **只接受 `http` / `https`**：URL 来自子进程 stdout，不能无条件交给 `ShellExecuteW`
//! （`file:` / 自定义协议都可能被别的程序接管）。

use windows::core::PCWSTR;
use windows::Win32::Foundation::HWND;
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

/// 打开失败的原因（人话）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BrowserError {
    detail: String,
}

impl BrowserError {
    fn new(detail: impl Into<String>) -> Self {
        Self {
            detail: detail.into(),
        }
    }
}

impl std::fmt::Display for BrowserError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.detail)
    }
}

impl std::error::Error for BrowserError {}

/// 自动化验收钩子：`DSH_HOUSING_NO_BROWSER=1` / `true` 时抑制**按设置自动打开**的两条路径
/// （实例就绪 / 控制台就绪）。手动「打开网页」「打开控制台」不经过这里，永远可用。
pub fn auto_open_suppressed() -> bool {
    matches!(
        std::env::var("DSH_HOUSING_NO_BROWSER")
            .unwrap_or_default()
            .trim()
            .to_ascii_lowercase()
            .as_str(),
        "1" | "true" | "yes"
    )
}

/// 用默认浏览器打开一个 http(s) URL。
pub fn open_url(url: &str) -> Result<(), BrowserError> {
    if !is_http_url(url) {
        return Err(BrowserError::new(format!("只接受 http/https URL：{url}")));
    }
    let operation = wide("open");
    let target = wide(url.trim());
    // SAFETY: 两个缓冲区都以 NUL 结尾且在调用期间存活；其余参数为空。
    let result = unsafe {
        ShellExecuteW(
            HWND::default(),
            PCWSTR(operation.as_ptr()),
            PCWSTR(target.as_ptr()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        )
    };
    // ShellExecuteW 的返回值 <= 32 表示失败（Win32 文档口径）。
    let code = result.0 as isize;
    if code <= 32 {
        return Err(BrowserError::new(format!(
            "ShellExecuteW 打不开浏览器（返回值 {code}）"
        )));
    }
    Ok(())
}

/// URL 是否是 `http` / `https`（大小写不敏感）。
pub fn is_http_url(url: &str) -> bool {
    let trimmed = url.trim();
    let lower = trimmed.to_ascii_lowercase();
    (lower.starts_with("http://") && trimmed.len() > "http://".len())
        || (lower.starts_with("https://") && trimmed.len() > "https://".len())
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_http_urls_are_accepted() {
        assert!(is_http_url("http://127.0.0.1:39412/?token=abc"));
        assert!(is_http_url("HTTPS://example.com/x"));
        assert!(!is_http_url("file:///C:/Windows/System32/calc.exe"));
        assert!(!is_http_url("ftp://example.com/x"));
        assert!(!is_http_url("http://"));
        assert!(!is_http_url("  "));
    }

    #[test]
    fn non_http_url_is_rejected_without_touching_the_shell() {
        let err = open_url("file:///C:/Windows/System32/calc.exe").expect_err("不该交给 shell");
        assert!(err.to_string().contains("http"), "{err}");
    }
}
