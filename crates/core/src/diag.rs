//! 启动器自身诊断日志 + 出错弹窗 + panic 落盘。
//!
//! 日志只写 `<数据目录>\logs\housing-YYYY-MM-DD.log`，不写胶囊、不写 `%APPDATA%`
//! （docs/structure.md §3.10、用户 2026-10-19 裁决 D5）。
//!
//! 不用 `log` / `tracing`：这里只需要「追加一行本地时间戳」，依赖越少越好（§2.3）。
//! M1 起多个线程会同时写（supervisor / IPC / 实例读线程），所以文件句柄放在
//! `Mutex` 后面；按本地日期切文件（跨零点自动换）。

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, Once, OnceLock};

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HWND, SYSTEMTIME};
use windows::Win32::System::SystemInformation::GetLocalTime;
use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};

use crate::store;

/// 日志目录；未 `init` 前所有写日志调用静默丢弃。
static LOG_DIR: OnceLock<PathBuf> = OnceLock::new();

/// 当前日志文件句柄与它对应的本地日期。
static LOG_STATE: Mutex<Option<LogState>> = Mutex::new(None);

struct LogState {
    date: (u16, u16, u16),
    file: File,
}

/// 指定数据目录，日志文件为 `<data_dir>\logs\housing-<本地日期>.log`。
///
/// 幂等：重复调用只保留第一次的路径。
pub fn init(data_dir: &Path) {
    let _ = LOG_DIR.set(data_dir.join(store::LOGS_DIR_NAME));
}

pub fn info(message: &str) {
    write_line("INFO ", message);
}

pub fn warn(message: &str) {
    write_line("WARN ", message);
}

pub fn error(message: &str) {
    write_line("ERROR", message);
}

/// 安装 panic hook：panic 先落盘（带 backtrace），再交给默认 hook。
///
/// 用户裁决 R7：`panic = "unwind"` + 关键边界 `catch_unwind` + 落盘——不用 `abort`，
/// 否则连捕获与记录的机会都没有。
pub fn install_panic_hook() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        let default_hook = std::panic::take_hook();
        std::panic::set_hook(Box::new(move |info| {
            let backtrace = std::backtrace::Backtrace::force_capture();
            error(&format!("panic：{info}\nbacktrace:\n{backtrace}"));
            default_hook(info);
        }));
    });
}

/// 弹一个错误框。exe 是 GUI 子系统、没有控制台，启动失败只能这样让人看见。
pub fn show_error(message: &str) {
    let text = wide(message);
    let caption = wide("dsh-housing");
    // SAFETY: 两个缓冲区都以 NUL 结尾（wide 保证），并在调用期间存活。
    unsafe {
        MessageBoxW(
            HWND::default(),
            PCWSTR(text.as_ptr()),
            PCWSTR(caption.as_ptr()),
            MB_OK | MB_ICONERROR,
        );
    }
}

/// 本地时间戳 `YYYY-MM-DD HH:MM:SS`（日志行与实例日志分隔头共用）。
pub fn timestamp_text() -> String {
    let stamp = local_now();
    format!(
        "{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        stamp.wYear, stamp.wMonth, stamp.wDay, stamp.wHour, stamp.wMinute, stamp.wSecond
    )
}

/// 本地日期 `(年, 月, 日)`；日志按它切文件。
pub fn local_date() -> (u16, u16, u16) {
    let stamp = local_now();
    (stamp.wYear, stamp.wMonth, stamp.wDay)
}

fn write_line(level: &str, message: &str) {
    let Some(dir) = LOG_DIR.get() else {
        return;
    };
    let date = local_date();
    let line = format!("[{}] [{}] {}\r\n", timestamp_text(), level, message);

    let mut state = match LOG_STATE.lock() {
        Ok(state) => state,
        Err(poisoned) => poisoned.into_inner(),
    };
    let needs_open = match state.as_ref() {
        Some(current) => current.date != date,
        None => true,
    };
    if needs_open {
        if std::fs::create_dir_all(dir).is_err() {
            return;
        }
        let path = dir.join(format!(
            "housing-{:04}-{:02}-{:02}.log",
            date.0, date.1, date.2
        ));
        match OpenOptions::new().create(true).append(true).open(path) {
            Ok(file) => *state = Some(LogState { date, file }),
            Err(_) => {
                *state = None;
                return;
            }
        }
    }
    if let Some(current) = state.as_mut() {
        let _ = current.file.write_all(line.as_bytes());
        let _ = current.file.flush();
    }
}

/// 本地时间（日志文件名与时间戳都用本地时钟，跨零点要靠它切文件）。
fn local_now() -> SYSTEMTIME {
    // SAFETY: 无参数、无指针，直接返回 SYSTEMTIME（windows 0.58 的签名）。
    unsafe { GetLocalTime() }
}

/// UTF-16 + NUL 结尾，供 Win32 宽字符 API 使用。
fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}
