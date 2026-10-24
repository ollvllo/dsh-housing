//! core ↔ tray / 兜底通道的本地 IPC（docs/structure.md §2.3、§3.8、§5.2 第 2 条）。
//!
//! # 选型：命名管道 + 换行分隔 JSON（不选 message-only window + `WM_COPYDATA`）
//!
//! 2026-10-19 M1 定案。理由：
//!
//! 1. **请求 / 响应都要带数据**。托盘要的是实例快照（列表、状态、PID），`WM_COPYDATA`
//!    只能单向塞一块内存，拿回执还得反向 `SendMessage`；两个进程互相同步 `SendMessage`
//!    一旦同时发生就是经典死锁（双方消息循环都卡在对方身上）。
//! 2. **命名管道天然是「客户端提问、服务端回答」**，core 用独立服务线程，不占主循环；
//!    托盘用后台轮询线程，UI 线程永远不碰 IPC。
//! 3. 协议就是 `serde_json` 一行一消息，可直接用 PowerShell 的
//!    `NamedPipeClientStream` 复现，验收脚本不需要额外工具（`test/verify-m1.ps1`）。
//!
//! # 安全口径
//!
//! - 管道名带**登录会话 ID**（`\\.\pipe\dsh-housing-core-<session>`），配合
//!   `PIPE_REJECT_REMOTE_CLIENTS`：只允许本机会话内、且被默认 DACL 放行的进程连接。
//! - 管道不是对外接口：只有 core 自己的托盘、第二个实例、`--stop-all` 会连它。

use std::fmt;
use std::io;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::PathBuf;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use windows::core::PCWSTR;
use windows::Win32::Foundation::{
    ERROR_BROKEN_PIPE, ERROR_NO_DATA, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, HANDLE,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FlushFileBuffers, ReadFile, WriteFile, FILE_FLAGS_AND_ATTRIBUTES,
    FILE_FLAG_FIRST_PIPE_INSTANCE, FILE_SHARE_NONE, OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PeekNamedPipe, WaitNamedPipeW,
    NAMED_PIPE_MODE, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_WAIT,
};
use windows::Win32::System::RemoteDesktop::ProcessIdToSessionId;
use windows::Win32::System::Threading::GetCurrentProcessId;

use crate::model::StatusSnapshot;

/// 环境变量：core 用它把管道名告诉托盘子进程（内部机制，不是对外 CLI）。
pub const PIPE_ENV_VAR: &str = "DSH_HOUSING_PIPE";
/// 环境变量：core 用它把自己的 PID 告诉托盘子进程（托盘的「core 死亡监听」，§3.15）。
pub const CORE_PID_ENV_VAR: &str = "DSH_HOUSING_CORE_PID";
/// 单条消息上限（够传一大份实例表；超出直接拒绝，避免被灌内存）。
const MAX_MESSAGE_BYTES: usize = 1024 * 1024;
/// 管道缓冲。
const PIPE_BUFFER_BYTES: u32 = 64 * 1024;
/// 客户端等待「服务端空闲」与「响应到达」的轮询间隔。
const POLL_INTERVAL: Duration = Duration::from_millis(10);

/// 请求。`cmd` 是判别字段（`{"cmd":"status"}` / `{"cmd":"start","key":"…"}`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum Request {
    /// 读整份运行时快照。
    Status,
    /// 登记 / 刷新一个胶囊目录（M2 起唯一的登记入口，§3.13）。
    Register { root: PathBuf },
    /// 按登记项启动一个实例（M2 起 `start` 只认实例键，§5.2 #11）。
    Start { key: String },
    /// 停止一个实例（按实例键）。
    Stop { key: String },
    /// 停止全部实例（不退出 core）。
    StopAll,
    /// 停全部实例并退出 core（托盘「退出」）。
    Quit,
    /// 把托盘点回来（第二个 core 实例被启动时投递）。
    Wake,
    /// 用默认浏览器打开该实例的 dsh Web UI（§3.16）。
    OpenBrowser { key: String },
    /// 用默认浏览器打开控制台（M3，§3.17）。
    OpenConsole,
    /// 取控制台 URL（含 `consoleToken`；只给本机的托盘 / 验收脚本）。
    ConsoleInfo,
}

/// 响应。`status` 总是尽量带上，托盘据此刷新菜单 / tooltip。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, Default)]
pub struct Response {
    pub ok: bool,
    /// 失败原因（给人看的中文）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// 成功时的补充说明（登记刷新了哪些字段、实例还没就绪等）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notice: Option<String>,
    /// 处理完之后的快照。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<StatusSnapshot>,
    /// core 正在收尾；托盘看到后自行退出。
    #[serde(default)]
    pub shutdown: bool,
    /// 控制台 URL（只由 `console_info` 填；含 `consoleToken`，不落盘、不进日志）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub console_url: Option<String>,
    /// 失败时的 HTTP 语义状态码（M3 起 GUI 用它映射 401/404/409；IPC 客户端可以忽略）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<u16>,
}

impl Response {
    pub fn ok(status: &StatusSnapshot) -> Self {
        Self {
            ok: true,
            error: None,
            notice: None,
            status: Some(status.clone()),
            shutdown: status.shutting_down,
            console_url: None,
            code: None,
        }
    }

    /// 成功 + 一句补充说明（登记结果、无 token URL 等）。
    pub fn ok_notice(status: &StatusSnapshot, notice: impl Into<String>) -> Self {
        Self {
            ok: true,
            error: None,
            notice: Some(notice.into()),
            status: Some(status.clone()),
            shutdown: status.shutting_down,
            console_url: None,
            code: None,
        }
    }

    /// 成功但没有快照（例如只做了一次文件操作）。
    pub fn ok_message(message: impl Into<String>) -> Self {
        Self {
            ok: true,
            error: None,
            notice: Some(message.into()),
            status: None,
            shutdown: false,
            console_url: None,
            code: None,
        }
    }

    /// 控制台 URL（`console_info` 用；唯一会携带 `consoleToken` 的响应）。
    pub fn console(url: impl Into<String>) -> Self {
        Self {
            ok: true,
            error: None,
            notice: None,
            status: None,
            shutdown: false,
            console_url: Some(url.into()),
            code: None,
        }
    }

    pub fn fail(message: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(message.into()),
            notice: None,
            status: None,
            shutdown: false,
            console_url: None,
            code: None,
        }
    }

    /// 失败 + HTTP 语义状态码（GUI 映射用：404 不存在 / 409 冲突 / 400 参数）。
    pub fn fail_with_code(code: u16, message: impl Into<String>) -> Self {
        Self {
            ok: false,
            error: Some(message.into()),
            notice: None,
            status: None,
            shutdown: false,
            console_url: None,
            code: Some(code),
        }
    }

    /// 失败但**仍带上快照**（例如实例启动失败后，托盘要能看到那个 Failed 行）。
    pub fn fail_with_status(status: &StatusSnapshot, message: impl Into<String>) -> Self {
        Self::fail_with_status_code(status, 0, message)
    }

    /// 失败 + 快照 + HTTP 语义状态码（`0` = 不指定）。
    pub fn fail_with_status_code(
        status: &StatusSnapshot,
        code: u16,
        message: impl Into<String>,
    ) -> Self {
        Self {
            ok: false,
            error: Some(message.into()),
            notice: None,
            status: Some(status.clone()),
            shutdown: status.shutting_down,
            console_url: None,
            code: if code == 0 { None } else { Some(code) },
        }
    }
}

/// IPC 层的失败。
#[derive(Debug)]
pub struct IpcError {
    context: &'static str,
    detail: String,
}

impl IpcError {
    fn new(context: &'static str, detail: impl Into<String>) -> Self {
        Self {
            context,
            detail: detail.into(),
        }
    }
}

impl fmt::Display for IpcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}：{}", self.context, self.detail)
    }
}

impl std::error::Error for IpcError {}

/// 当前进程所在的登录会话 ID（命名管道五元组之外区分会话用）。
pub fn session_id() -> u32 {
    let mut session = 0u32;
    // SAFETY: 两个参数都是本进程的有效值 / 栈上缓冲。
    unsafe {
        if ProcessIdToSessionId(GetCurrentProcessId(), &mut session).is_err() {
            return 0;
        }
    }
    session
}

/// core 的管道名：`\\.\pipe\dsh-housing-core-<session>`。
pub fn pipe_name() -> String {
    format!(r"\\.\pipe\dsh-housing-core-{}", session_id())
}

/// 在后台线程里跑服务端：建管道 → 依次接受连接 → 读一行请求 → 处理 → 写一行响应。
///
/// 返回后服务端线程独立运行；**只有管道创建失败**才返回 `Err`（此时 core 没有 IPC，
/// 托盘也连不上，属于致命启动失败）。
pub fn spawn_server(
    pipe: &str,
    handler: Arc<dyn Fn(Request) -> Response + Send + Sync>,
) -> Result<thread::JoinHandle<()>, IpcError> {
    let pipe = pipe.to_string();
    let pipe_handle = create_pipe_instance(&pipe)
        .map_err(|err| IpcError::new("创建命名管道", format!("{pipe}：{err}")))?;

    thread::Builder::new()
        .name("ipc-server".to_string())
        .spawn(move || serve_loop(pipe_handle, handler))
        .map_err(|err| IpcError::new("启动 IPC 服务线程", err.to_string()))
}

fn serve_loop(pipe: OwnedHandle, handler: Arc<dyn Fn(Request) -> Response + Send + Sync>) {
    loop {
        // SAFETY: 管道句柄由本线程独占持有；连接 / 读写都在同一个线程内顺序发生。
        unsafe {
            if !wait_for_client(&pipe) {
                break;
            }
            match read_message(&pipe) {
                Ok(Some(text)) => {
                    let response = match serde_json::from_str::<Request>(&text) {
                        Ok(request) => handler(request),
                        Err(err) => Response::fail(format!("请求不是合法 JSON：{err}")),
                    };
                    let _ = write_message(&pipe, &response);
                    let _ = FlushFileBuffers(handle(&pipe));
                }
                Ok(None) => {
                    // 客户端连上就关了（例如探测）；直接断开。
                }
                Err(err) => {
                    crate::diag::warn(&format!("IPC 读请求失败：{err}"));
                }
            }
            let _ = DisconnectNamedPipe(handle(&pipe));
        }
    }
}

/// 等客户端连上；`false` 表示管道已不可用，服务端该收摊。
fn wait_for_client(pipe: &OwnedHandle) -> bool {
    // SAFETY: 阻塞式（PIPE_WAIT）连接；句柄有效。
    unsafe {
        match ConnectNamedPipe(handle(pipe), None) {
            Ok(()) => true,
            Err(err) => {
                let code = win32_code(&err);
                if code == ERROR_PIPE_CONNECTED.0 {
                    // 客户端在 ConnectNamedPipe 之前就连上了：正常情况。
                    true
                } else if code == ERROR_NO_DATA.0 {
                    // 客户端连上又立刻断开：重来。
                    true
                } else {
                    crate::diag::warn(&format!(
                        "IPC 接受连接失败（Win32 {code}）：{err}；服务线程退出"
                    ));
                    false
                }
            }
        }
    }
}

fn create_pipe_instance(pipe: &str) -> io::Result<OwnedHandle> {
    let name = to_wide(pipe);
    let open_mode =
        FILE_FLAGS_AND_ATTRIBUTES(PIPE_ACCESS_DUPLEX.0 | FILE_FLAG_FIRST_PIPE_INSTANCE.0);
    let pipe_mode = NAMED_PIPE_MODE(
        PIPE_TYPE_BYTE.0 | PIPE_READMODE_BYTE.0 | PIPE_WAIT.0 | PIPE_REJECT_REMOTE_CLIENTS.0,
    );
    // SAFETY: 名字是 NUL 结尾的宽字符串；默认安全描述符 = 创建者令牌的默认 DACL。
    let handle = unsafe {
        CreateNamedPipeW(
            PCWSTR(name.as_ptr()),
            open_mode,
            pipe_mode,
            1,
            PIPE_BUFFER_BYTES,
            PIPE_BUFFER_BYTES,
            0,
            None,
        )
    };
    if handle.is_invalid() {
        return Err(io::Error::last_os_error());
    }
    Ok(unsafe { OwnedHandle::from_raw_handle(handle.0) })
}

/// 客户端调用：连接 → 发一行请求 → 等一行响应。
///
/// `timeout` 覆盖「等管道空闲 + 等响应」的总时长；超时返回 `TimedOut` 错误。
pub fn call(pipe: &str, request: &Request, timeout: Duration) -> io::Result<Response> {
    let deadline = Instant::now() + timeout;
    let pipe_handle = connect(pipe, deadline)?;

    let mut payload = serde_json::to_vec(request)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    payload.push(b'\n');
    // SAFETY: 句柄有效；缓冲区在调用期间存活。
    unsafe {
        WriteFile(handle(&pipe_handle), Some(&payload), None, None)
            .map_err(|err| win32_io("写 IPC 请求", &err))?;
        FlushFileBuffers(handle(&pipe_handle)).map_err(|err| win32_io("刷新 IPC 请求", &err))?;
    }

    let mut received = Vec::new();
    loop {
        let available = peek_available(&pipe_handle)?;
        if available > 0 {
            let mut chunk = [0u8; 4096];
            // SAFETY: 句柄有效；chunk 是栈上缓冲。
            let mut read = 0u32;
            unsafe {
                ReadFile(
                    handle(&pipe_handle),
                    Some(&mut chunk),
                    Some(&mut read),
                    None,
                )
                .map_err(|err| win32_io("读 IPC 响应", &err))?;
            }
            received.extend_from_slice(&chunk[..read as usize]);
            if received.len() > MAX_MESSAGE_BYTES {
                return Err(io::Error::new(io::ErrorKind::InvalidData, "IPC 响应超长"));
            }
            if let Some(position) = received.iter().position(|byte| *byte == b'\n') {
                received.truncate(position);
                let text = String::from_utf8_lossy(&received);
                return serde_json::from_str(text.trim())
                    .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err));
            }
        } else if Instant::now() >= deadline {
            return Err(io::Error::new(io::ErrorKind::TimedOut, "等 IPC 响应超时"));
        } else {
            thread::sleep(POLL_INTERVAL);
        }
    }
}

fn connect(pipe: &str, deadline: Instant) -> io::Result<OwnedHandle> {
    let name = to_wide(pipe);
    loop {
        // SAFETY: 名字是 NUL 结尾的宽字符串。
        let handle = unsafe {
            CreateFileW(
                PCWSTR(name.as_ptr()),
                windows::Win32::Foundation::GENERIC_READ.0
                    | windows::Win32::Foundation::GENERIC_WRITE.0,
                FILE_SHARE_NONE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES(0),
                HANDLE::default(),
            )
        };
        match handle {
            Ok(handle) => return Ok(unsafe { OwnedHandle::from_raw_handle(handle.0) }),
            Err(err) => {
                let code = win32_code(&err);
                if code == ERROR_PIPE_BUSY.0 {
                    // 服务端正在处理上一个客户端：等它空出来。
                    // SAFETY: 名字有效；超时取剩余时间（最少 50ms）。
                    let remaining = deadline
                        .saturating_duration_since(Instant::now())
                        .as_millis()
                        .min(u32::MAX as u128) as u32;
                    unsafe {
                        let _ = WaitNamedPipeW(PCWSTR(name.as_ptr()), remaining.max(50));
                    }
                    if Instant::now() >= deadline {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "等 IPC 管道空闲超时",
                        ));
                    }
                    continue;
                }
                return Err(win32_io("连接 IPC 管道", &err));
            }
        }
    }
}

fn peek_available(pipe: &OwnedHandle) -> io::Result<u32> {
    let mut available = 0u32;
    // SAFETY: 句柄有效；只查可读字节数，不读数据。
    unsafe {
        PeekNamedPipe(handle(pipe), None, 0, None, Some(&mut available), None)
            .map_err(|err| win32_io("查询 IPC 可读字节", &err))?;
    }
    Ok(available)
}

/// 读一行（服务端用）。`Ok(None)` = 对端关闭 / 没发数据。
///
/// # Safety
///
/// 调用方保证 `pipe` 是当前线程独占的、已连接的服务端管道。
unsafe fn read_message(pipe: &OwnedHandle) -> io::Result<Option<String>> {
    let mut received = Vec::new();
    loop {
        let mut chunk = [0u8; 4096];
        let mut read = 0u32;
        match ReadFile(handle(pipe), Some(&mut chunk), Some(&mut read), None) {
            Ok(()) => {
                if read == 0 {
                    return Ok(None);
                }
                received.extend_from_slice(&chunk[..read as usize]);
                if received.len() > MAX_MESSAGE_BYTES {
                    return Err(io::Error::new(io::ErrorKind::InvalidData, "IPC 请求超长"));
                }
                if let Some(position) = received.iter().position(|byte| *byte == b'\n') {
                    received.truncate(position);
                    return Ok(Some(String::from_utf8_lossy(&received).into_owned()));
                }
            }
            Err(err) => {
                let code = win32_code(&err);
                if code == ERROR_BROKEN_PIPE.0 || code == ERROR_NO_DATA.0 {
                    return Ok(None);
                }
                return Err(win32_io("读 IPC 请求", &err));
            }
        }
    }
}

/// 写一行（服务端用）。
///
/// # Safety
///
/// 调用方保证 `pipe` 是当前线程独占的、已连接的服务端管道。
unsafe fn write_message(pipe: &OwnedHandle, response: &Response) -> io::Result<()> {
    let mut payload = serde_json::to_vec(response)
        .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
    payload.push(b'\n');
    WriteFile(handle(pipe), Some(&payload), None, None)
        .map(|_| ())
        .map_err(|err| win32_io("写 IPC 响应", &err))
}

fn handle(owned: &OwnedHandle) -> HANDLE {
    HANDLE(owned.as_raw_handle())
}

fn to_wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Win32 错误码（`windows::core::Error` 里存的是 `HRESULT_FROM_WIN32`）。
fn win32_code(err: &windows::core::Error) -> u32 {
    (err.code().0 as u32) & 0xFFFF
}

fn win32_io(context: &'static str, err: &windows::core::Error) -> io::Error {
    io::Error::other(format!("{context}（Win32 {}）：{err}", win32_code(err)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::InstanceView;

    fn snapshot() -> StatusSnapshot {
        StatusSnapshot {
            instances: vec![InstanceView {
                key: "0.2.0-rc.2-default/web".to_string(),
                capsule_id: "0.2.0-rc.2-default".to_string(),
                profile: "web".to_string(),
                alias: None,
                port: 39412,
                host: None,
                state: "running".to_string(),
                detail: None,
                pid: Some(4242),
                token_url: None,
                metrics: None,
            }],
            shutting_down: false,
            tray_running: true,
        }
    }

    fn unique_pipe(label: &str) -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        format!(
            r"\\.\pipe\dsh-housing-test-{label}-{}-{nanos}",
            std::process::id()
        )
    }

    #[test]
    fn request_round_trips_over_json() {
        let text = r#"{"cmd":"stop","key":"0.2.0-rc.2-default/web"}"#;
        let request: Request = serde_json::from_str(text).unwrap();
        assert_eq!(
            request,
            Request::Stop {
                key: "0.2.0-rc.2-default/web".to_string()
            }
        );
        assert_eq!(
            serde_json::to_string(&request).unwrap(),
            r#"{"cmd":"stop","key":"0.2.0-rc.2-default/web"}"#
        );

        let status = Request::Status;
        assert_eq!(
            serde_json::to_string(&status).unwrap(),
            r#"{"cmd":"status"}"#
        );

        // M2 的两个新命令：登记（路径）与按登记项启动。
        let register: Request =
            serde_json::from_str(r#"{"cmd":"register","root":"D:\\capsules\\cap"}"#).unwrap();
        assert_eq!(
            register,
            Request::Register {
                root: std::path::PathBuf::from(r"D:\capsules\cap")
            }
        );
        let start: Request = serde_json::from_str(r#"{"cmd":"start","key":"cap/web"}"#).unwrap();
        assert_eq!(
            start,
            Request::Start {
                key: "cap/web".to_string()
            }
        );
        let open: Request =
            serde_json::from_str(r#"{"cmd":"open_browser","key":"cap/web"}"#).unwrap();
        assert_eq!(
            open,
            Request::OpenBrowser {
                key: "cap/web".to_string()
            }
        );

        // M3：控制台的两条命令（`console_info` 是唯一会回 token URL 的响应）。
        let open_console: Request = serde_json::from_str(r#"{"cmd":"open_console"}"#).unwrap();
        assert_eq!(open_console, Request::OpenConsole);
        let console_info: Request = serde_json::from_str(r#"{"cmd":"console_info"}"#).unwrap();
        assert_eq!(console_info, Request::ConsoleInfo);

        // M1 的「整份 InstanceSpec 走 IPC」通道已删除（§5.2 #11）。
        assert!(
            serde_json::from_str::<Request>(r#"{"cmd":"start","spec":{}}"#).is_err(),
            "spec 形式的 start 不该再被接受"
        );
    }

    #[test]
    fn response_carries_snapshot_and_shutdown_flag() {
        let mut snap = snapshot();
        snap.shutting_down = true;
        let response = Response::ok(&snap);
        assert!(response.ok);
        assert!(response.shutdown);
        let text = serde_json::to_string(&response).unwrap();
        let parsed: Response = serde_json::from_str(&text).unwrap();
        assert_eq!(parsed, response);
        assert_eq!(parsed.status.unwrap().instances[0].pid, Some(4242));
    }

    #[test]
    fn console_info_carries_url_but_status_does_not() {
        let url = "http://127.0.0.1:39420/?token=0123456789abcdef";
        let response = Response::console(url);
        assert!(response.ok);
        assert_eq!(response.console_url.as_deref(), Some(url));
        let parsed: Response =
            serde_json::from_str(&serde_json::to_string(&response).unwrap()).unwrap();
        assert_eq!(parsed.console_url.as_deref(), Some(url));

        // 普通状态快照不该带 token URL。
        let status = snapshot();
        let text = serde_json::to_string(&Response::ok(&status)).unwrap();
        assert!(!text.contains("console_url"), "{text}");
        assert!(!text.contains("code"), "{text}");
    }

    #[test]
    fn pipe_name_is_session_scoped() {
        let name = pipe_name();
        assert!(name.starts_with(r"\\.\pipe\dsh-housing-core-"));
        assert!(name.ends_with(&session_id().to_string()));
    }

    #[test]
    fn server_and_client_talk() {
        let pipe = unique_pipe("echo");
        let status = snapshot();
        let handler_status = status.clone();
        let server = spawn_server(
            &pipe,
            Arc::new(move |request| match request {
                Request::Status => Response::ok(&handler_status),
                Request::Start { key } => Response::fail(format!("不启动 {key}")),
                _ => Response::fail("unknown"),
            }),
        )
        .expect("建管道应成功");

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut last_error;
        loop {
            match call(&pipe, &Request::Status, Duration::from_millis(500)) {
                Ok(response) => {
                    assert!(response.ok);
                    assert_eq!(response.status.as_ref().unwrap(), &status);
                    break;
                }
                Err(err) => {
                    last_error = err;
                    if Instant::now() >= deadline {
                        panic!("5s 内连不上测试服务端：{last_error:?}");
                    }
                    thread::sleep(POLL_INTERVAL);
                }
            }
        }

        let response = call(
            &pipe,
            &Request::Start {
                key: "cap/web".to_string(),
            },
            Duration::from_secs(2),
        )
        .expect("第二次调用应成功");
        assert!(!response.ok);
        assert!(response.error.unwrap().contains("cap/web"));

        // 服务线程没有 stop 通道（进程退出即回收），这里只验证它还能继续服务。
        drop(server);
    }
}
