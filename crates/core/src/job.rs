//! 进程与作业生命周期（docs/structure.md §2.2、§3.4、§3.8）。
//!
//! 用户 2026-10-19 裁决 D2：**spawn 并入本模块**——进程与它所属的 Job Object 是一个整体，
//! 分开写一定会出现「谁负责 assign」的双认领。本模块负责：
//!
//! - `CreateJobObjectW` + `KILL_ON_JOB_CLOSE`（core 退出 / 崩溃时整棵树被收掉）；
//! - 匿名管道（stdin / stdout / stderr）与继承标志；
//! - `CreateProcessW(CREATE_NO_WINDOW | CREATE_UNICODE_ENVIRONMENT | CREATE_SUSPENDED)`
//!   → `AssignProcessToJobObject` → `ResumeThread`
//!   （先挂 job 再放行：子进程连一个 fork 都跑不到 job 外面）；
//! - 优雅停（`AttachConsole` + `GenerateConsoleCtrlEvent`）与强制停（`TerminateJobObject`）；
//! - job 内 PID 枚举（资源统计用，M1 先用于「MCP 子进程是否在 job 内」的实测）。
//!
//! **不建目录、不校验文件**（用户裁决 D1/D8）：缺 node.exe / workspace 时把 Win32 错误
//! 翻译成人话返回，由调用方展示。
//!
//! # 优雅停为什么发 CTRL_C 而不是 CTRL_BREAK（2026-10-19 M1 实测修正 §3.8）
//!
//! 文档原计划是 `CTRL_BREAK_EVENT`。但 dsh 0.2.0 的启动代码只注册了
//! `process.on("SIGINT", …)` 与 `SIGTERM`（Windows 上 `SIGTERM` 收不到），**没有 `SIGBREAK`**：
//! `CTRL_BREAK` 在 Node 里映射成 `SIGBREAK`，没有监听器就是默认退出（不跑 dsh 的 dispose）。
//! `CTRL_C_EVENT` 映射成 `SIGINT`，正好落到 dsh 的优雅停机路径（退出码 130）。
//!
//! `CTRL_C_EVENT` 不能定向到进程组，但**可以定向到「共享调用方 console 的全部进程」**：
//! 我们 `AttachConsole(child_pid)` 之后发 `GenerateConsoleCtrlEvent(CTRL_C_EVENT, 0)`，
//! 命中范围就只剩这个实例自己的 console（它由 `CREATE_NO_WINDOW` 独占）。
//! 本进程也在被命中范围里，所以要在 attach **之后**装一个「忽略 console 控制事件」的
//! handler（attach 之前装的不会被这条 console 认领——实测会把自己打死）。
//! 多个实例同时停止时，`AttachConsole` 是进程级独占资源 → 用全局锁串行化。
//!
//! **不要给子进程加 `CREATE_NEW_PROCESS_GROUP`**（实测）：新进程组会禁用该组的
//! `CTRL_C`（这是为了让父进程能单独发 `CTRL_BREAK`），于是 `GenerateConsoleCtrlEvent(
//! CTRL_C_EVENT, 0)` 对组内进程无效——现象是 core 自己收到事件、node 完全没反应。
//! 我们只用 `CTRL_C`，所以不加这个 flag。

use std::ffi::c_void;
use std::fmt;
use std::mem::{size_of, zeroed};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::HANDLE_FLAGS;
use windows::Win32::Foundation::{
    GetLastError, BOOL, ERROR_ACCESS_DENIED, ERROR_DIRECTORY, ERROR_FILE_NOT_FOUND,
    ERROR_PATH_NOT_FOUND, ERROR_SHARING_VIOLATION, HANDLE, STILL_ACTIVE, TRUE, WAIT_OBJECT_0,
    WAIT_TIMEOUT,
};
use windows::Win32::Foundation::{SetHandleInformation, HANDLE_FLAG_INHERIT};
use windows::Win32::Security::SECURITY_ATTRIBUTES;
use windows::Win32::System::Console::{
    AttachConsole, FreeConsole, GenerateConsoleCtrlEvent, GetConsoleProcessList,
    SetConsoleCtrlHandler, CTRL_C_EVENT,
};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicProcessIdList,
    JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
    TerminateJobObject, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows::Win32::System::Pipes::CreatePipe;
use windows::Win32::System::Threading::{
    CreateProcessW, GetExitCodeProcess, ResumeThread, WaitForSingleObject, CREATE_NO_WINDOW,
    CREATE_SUSPENDED, CREATE_UNICODE_ENVIRONMENT, PROCESS_CREATION_FLAGS, PROCESS_INFORMATION,
    STARTF_USESTDHANDLES, STARTUPINFOW, STARTUPINFOW_FLAGS,
};

/// 强杀时写进 job 的退出码（任务管理器里能看出是启动器收的）。
pub const FORCE_KILL_EXIT_CODE: u32 = 0xDEAD_0001;

/// 优雅停的全局串行锁：`AttachConsole` 是「一个进程同一时刻只能挂一个 console」。
static GRACEFUL_STOP_LOCK: Mutex<()> = Mutex::new(());

/// 一次 spawn 的输入（由 `env::LaunchPlan` 直接喂进来）。
pub struct SpawnRequest<'a> {
    pub program: &'a Path,
    pub command_line: &'a [u16],
    pub cwd: &'a Path,
    pub env_block: &'a [u16],
}

/// 成功 spawn 后交给实例持有的句柄。
///
/// 父进程侧只保留 `process` / `job` / `stdin` / `stdout` / `stderr`；
/// 子进程侧的管道端在 spawn 成功后立即关闭（否则读线程永远等不到 EOF）。
pub struct SpawnedInstance {
    pub pid: u32,
    pub process: OwnedHandle,
    pub job: OwnedHandle,
    pub stdin: Option<OwnedHandle>,
    pub stdout: Option<OwnedHandle>,
    pub stderr: Option<OwnedHandle>,
}

impl fmt::Debug for SpawnedInstance {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SpawnedInstance")
            .field("pid", &self.pid)
            .finish_non_exhaustive()
    }
}

/// spawn / 作业操作失败。
#[derive(Debug)]
pub struct JobError {
    context: String,
    detail: String,
    win32: Option<u32>,
}

impl JobError {
    fn new(context: impl Into<String>, detail: impl Into<String>, win32: Option<u32>) -> Self {
        Self {
            context: context.into(),
            detail: detail.into(),
            win32,
        }
    }

    /// Win32 错误码（如果有）。
    pub fn win32_code(&self) -> Option<u32> {
        self.win32
    }
}

impl fmt::Display for JobError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}：{}", self.context, self.detail)
    }
}

impl std::error::Error for JobError {}

/// 创建 job → 创建三根管道 → 挂起启动进程 → 放进 job → 放行。
pub fn spawn(request: &SpawnRequest<'_>) -> Result<SpawnedInstance, JobError> {
    let job = create_job()?;

    let inheritable = SECURITY_ATTRIBUTES {
        nLength: size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: std::ptr::null_mut(),
        bInheritHandle: TRUE,
    };

    // stdin：子进程读 read 端，父进程留 write 端。
    let (stdin_read, stdin_write) = create_pipe(&inheritable, "创建 stdin 管道")?;
    // stdout / stderr：子进程写 write 端，父进程留 read 端。
    let (stdout_read, stdout_write) = create_pipe(&inheritable, "创建 stdout 管道")?;
    let (stderr_read, stderr_write) = create_pipe(&inheritable, "创建 stderr 管道")?;

    // 父进程自己拿的端点一律不可继承，免得又被别的子进程继承走。
    for inherited in [&stdin_write, &stdout_read, &stderr_read] {
        if let Err(err) =
            unsafe { SetHandleInformation(raw(inherited), HANDLE_FLAG_INHERIT.0, HANDLE_FLAGS(0)) }
        {
            return Err(win32_error("取消父进程管道端点的继承标志", &err));
        }
    }

    let program = wide(request.program.as_os_str().to_string_lossy().as_ref());
    let cwd = wide(request.cwd.as_os_str().to_string_lossy().as_ref());
    let mut command_line = request.command_line.to_vec();

    let mut startup: STARTUPINFOW = unsafe { zeroed() };
    startup.cb = size_of::<STARTUPINFOW>() as u32;
    startup.dwFlags = STARTUPINFOW_FLAGS(STARTF_USESTDHANDLES.0);
    startup.hStdInput = raw(&stdin_read);
    startup.hStdOutput = raw(&stdout_write);
    startup.hStdError = raw(&stderr_write);

    let mut info: PROCESS_INFORMATION = unsafe { zeroed() };
    let flags = PROCESS_CREATION_FLAGS(
        CREATE_NO_WINDOW.0 | CREATE_UNICODE_ENVIRONMENT.0 | CREATE_SUSPENDED.0,
    );

    let create_result = unsafe {
        CreateProcessW(
            PCWSTR(program.as_ptr()),
            PWSTR(command_line.as_mut_ptr()),
            None,
            None,
            true,
            flags,
            Some(request.env_block.as_ptr() as *const c_void),
            PCWSTR(cwd.as_ptr()),
            &startup,
            &mut info,
        )
    };

    // 子进程侧的端点由子进程继承；父进程这一份必须马上关，否则管道永远不 EOF。
    drop(stdin_read);
    drop(stdout_write);
    drop(stderr_write);

    if let Err(err) = create_result {
        return Err(win32_error(
            format!(
                "创建实例进程（程序 {}，工作目录 {}）",
                request.program.display(),
                request.cwd.display()
            ),
            &err,
        ));
    }

    let process = unsafe { OwnedHandle::from_raw_handle(info.hProcess.0) };
    let thread = unsafe { OwnedHandle::from_raw_handle(info.hThread.0) };

    // 先挂 job 再 Resume：子进程从第一条指令起就在 job 里。
    if let Err(err) = unsafe { AssignProcessToJobObject(raw(&job), raw(&process)) } {
        unsafe {
            let _ = windows::Win32::System::Threading::TerminateProcess(raw(&process), 1);
        }
        return Err(win32_error("把实例进程加入 Job Object", &err));
    }

    let resumed = unsafe { ResumeThread(raw(&thread)) };
    if resumed == u32::MAX {
        unsafe {
            let _ = TerminateJobObject(raw(&job), FORCE_KILL_EXIT_CODE);
        }
        return Err(JobError::new(
            "恢复实例主线程",
            format!("ResumeThread 失败（Win32 {}）", last_error_code()),
            Some(last_error_code()),
        ));
    }

    Ok(SpawnedInstance {
        pid: info.dwProcessId,
        process,
        job,
        stdin: Some(stdin_write),
        stdout: Some(stdout_read),
        stderr: Some(stderr_read),
    })
}

fn create_job() -> Result<OwnedHandle, JobError> {
    let job = unsafe { CreateJobObjectW(None, PCWSTR::null()) }
        .map_err(|err| win32_error("创建 Job Object", &err))?;
    let job = unsafe { OwnedHandle::from_raw_handle(job.0) };

    let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { zeroed() };
    limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
    unsafe {
        SetInformationJobObject(
            raw(&job),
            JobObjectExtendedLimitInformation,
            &limits as *const _ as *const c_void,
            size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )
    }
    .map_err(|err| win32_error("设置 KILL_ON_JOB_CLOSE", &err))?;
    Ok(job)
}

fn create_pipe(
    attributes: &SECURITY_ATTRIBUTES,
    context: &'static str,
) -> Result<(OwnedHandle, OwnedHandle), JobError> {
    let mut read = HANDLE::default();
    let mut write = HANDLE::default();
    unsafe { CreatePipe(&mut read, &mut write, Some(attributes), 0) }
        .map_err(|err| win32_error(context, &err))?;
    Ok(unsafe {
        (
            OwnedHandle::from_raw_handle(read.0),
            OwnedHandle::from_raw_handle(write.0),
        )
    })
}

/// `TerminateJobObject`：一次收掉主进程与仍在 job 内的子进程（含 MCP）。
pub fn terminate_job(job: &OwnedHandle, exit_code: u32) -> Result<(), JobError> {
    unsafe { TerminateJobObject(raw(job), exit_code) }
        .map_err(|err| win32_error("强制结束 Job Object", &err))
}

/// 等进程结束；返回是否在超时前结束。
pub fn wait_process(process: &OwnedHandle, timeout_ms: u32) -> bool {
    // SAFETY: 进程句柄有效。
    unsafe { WaitForSingleObject(raw(process), timeout_ms) == WAIT_OBJECT_0 }
}

/// 进程退出码；仍在运行返回 `None`。
pub fn process_exit_code(process: &OwnedHandle) -> Option<u32> {
    let mut code = 0u32;
    // SAFETY: 进程句柄有效；code 是栈上缓冲。
    unsafe {
        if GetExitCodeProcess(raw(process), &mut code).is_err() {
            return None;
        }
    }
    if code == STILL_ACTIVE.0 as u32 {
        None
    } else {
        Some(code)
    }
}

/// job 内当前 PID 列表（资源统计与「MCP 是否在 job 内」的实测用）。
pub fn job_process_ids(job: &OwnedHandle) -> Vec<u32> {
    // JOBOBJECT_BASIC_PROCESS_ID_LIST 是变长结构；用 u64 缓冲保证 8 字节对齐，
    // 头部 8 字节是 (NumberOfAssignedProcesses, NumberOfProcessIdsInList)。
    let mut buffer = vec![0u64; 8192]; // 64 KiB，够 8000 个 PID
    let byte_len = (buffer.len() * size_of::<u64>()) as u32;
    let mut returned = 0u32;
    // SAFETY: 缓冲区足够大且对齐；只读 job 的统计信息。
    let queried = unsafe {
        QueryInformationJobObject(
            raw(job),
            JobObjectBasicProcessIdList,
            buffer.as_mut_ptr() as *mut c_void,
            byte_len,
            Some(&mut returned),
        )
    };
    if queried.is_err() {
        return Vec::new();
    }
    let count = unsafe { *(buffer.as_ptr().cast::<u32>().add(1)) } as usize;
    let count = count.min(buffer.len() - 1);
    let pids = unsafe { std::slice::from_raw_parts(buffer.as_ptr().add(1).cast::<usize>(), count) };
    pids.iter()
        .map(|pid| *pid as u32)
        .filter(|pid| *pid != 0)
        .collect()
}

fn raw(owned: &OwnedHandle) -> HANDLE {
    HANDLE(owned.as_raw_handle())
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn last_error_code() -> u32 {
    unsafe { GetLastError().0 }
}

fn win32_error(context: impl Into<String>, err: &windows::core::Error) -> JobError {
    let code = (err.code().0 as u32) & 0xFFFF;
    let detail = if code == ERROR_FILE_NOT_FOUND.0 {
        "找不到可执行文件（node.exe 不存在？）".to_string()
    } else if code == ERROR_PATH_NOT_FOUND.0 {
        "路径不存在".to_string()
    } else if code == ERROR_DIRECTORY.0 {
        "工作目录不存在（胶囊的 workspace 缺失？）".to_string()
    } else if code == ERROR_ACCESS_DENIED.0 {
        "拒绝访问".to_string()
    } else if code == ERROR_SHARING_VIOLATION.0 {
        "文件被其他进程占用".to_string()
    } else {
        err.message()
    };
    JobError::new(
        context,
        format!("{detail}（Win32 错误 {code}）"),
        Some(code),
    )
}

/// 让本进程忽略 console 控制事件。
///
/// **必须在 `AttachConsole` 之后注册**（2026-10-19 M1 实测）：本进程是 GUI 子系统、
/// 平时没有 console，在 attach 之前调用 `SetConsoleCtrlHandler` 虽然返回成功，
/// 但这个 handler 不会被目标 console 认领；attach 后发 `CTRL_C_EVENT` 会把 core 一起打死
/// （实测退出码 `0xC000013A`，handler 一次都没被调用）。attach 之后注册则正常。
unsafe fn install_console_ctrl_ignore() -> bool {
    // SAFETY: handler 是 extern "system" 函数，无状态。
    SetConsoleCtrlHandler(Some(ignore_console_ctrl), true).is_ok()
}

unsafe fn remove_console_ctrl_ignore() {
    let _ = SetConsoleCtrlHandler(Some(ignore_console_ctrl), false);
}

unsafe extern "system" fn ignore_console_ctrl(_ctrl_type: u32) -> BOOL {
    HANDLER_CALLS.fetch_add(1, Ordering::SeqCst);
    TRUE
}

/// 本进程 console 控制 handler 被调用的次数（M1 诊断：确认事件真的到了我们这边）。
static HANDLER_CALLS: AtomicU32 = AtomicU32::new(0);

/// 优雅停：attach 到子进程的 console，发 `CTRL_C_EVENT`（= Node 的 `SIGINT`）。
///
/// 只负责「发信号」；等不等到退出由调用方决定（`wait_process`），超时后走
/// `terminate_job`。失败时返回原因，调用方应直接走强制停（不重试同一招）。
pub fn request_graceful_stop(pid: u32) -> Result<(), String> {
    let _guard = GRACEFUL_STOP_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());

    // SAFETY: 本进程平时没有 console（GUI 子系统）；attach 后必须 FreeConsole。
    unsafe {
        if let Err(err) = AttachConsole(pid) {
            return Err(format!(
                "AttachConsole（PID {pid}）失败（Win32 {}）：{err}",
                (err.code().0 as u32) & 0xFFFF
            ));
        }
        // 顺序不能换：attach 之后注册的 handler 才会被这条 console 认领。
        let mut console_pids = [0u32; 64];
        let console_count = GetConsoleProcessList(&mut console_pids);
        let handler_before = HANDLER_CALLS.load(Ordering::SeqCst);
        let protected = install_console_ctrl_ignore();
        let sent = GenerateConsoleCtrlEvent(CTRL_C_EVENT, 0);
        // 事件是异步派发的；给 console 一点时间把它送到各进程，再脱离 console。
        std::thread::sleep(Duration::from_millis(500));
        let handler_after = HANDLER_CALLS.load(Ordering::SeqCst);
        let _ = FreeConsole();
        remove_console_ctrl_ignore();
        let list = &console_pids[..(console_count as usize).min(console_pids.len())];
        crate::diag::info(&format!(
            "优雅停诊断：目标 PID={pid}；console 内进程={list:?}；本进程收到 console 事件 {} 次",
            handler_after.saturating_sub(handler_before)
        ));
        if !protected {
            crate::diag::warn(
                "SetConsoleCtrlHandler 安装失败：优雅停时 core 可能被自己发出的 CTRL_C 打死",
            );
        }
        match sent {
            Ok(()) => Ok(()),
            Err(err) => Err(format!(
                "GenerateConsoleCtrlEvent(CTRL_C) 失败（Win32 {}）：{err}",
                (err.code().0 as u32) & 0xFFFF
            )),
        }
    }
}

/// 进程是否还在（供 supervisor 判断；0 ms 超时）。
pub fn is_process_running(process: &OwnedHandle) -> bool {
    // SAFETY: 进程句柄有效。
    unsafe { WaitForSingleObject(raw(process), 0) == WAIT_TIMEOUT }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    fn cmd_exe() -> PathBuf {
        PathBuf::from(std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_string()))
            .join("System32")
            .join("cmd.exe")
    }

    fn test_env_block() -> Vec<u16> {
        let system_root = std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_string());
        let temp = std::env::var("TEMP").unwrap_or_else(|_| r"C:\Windows\Temp".to_string());
        let entries = [
            ("SystemRoot".to_string(), system_root.clone()),
            ("PATH".to_string(), format!(r"{system_root}\System32")),
            ("TEMP".to_string(), temp.clone()),
            ("TMP".to_string(), temp),
        ];
        let mut block = Vec::new();
        for (key, value) in entries {
            block.extend(key.encode_utf16());
            block.push('=' as u16);
            block.extend(value.encode_utf16());
            block.push(0);
        }
        block.push(0);
        block
    }

    fn spawn_cmd(extra: &str, cwd: &Path, env: &[u16]) -> Result<SpawnedInstance, JobError> {
        let mut command = format!("{} {extra}", cmd_exe().display())
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect::<Vec<u16>>();
        let program = cmd_exe();
        spawn(&SpawnRequest {
            program: &program,
            command_line: &mut command,
            cwd,
            env_block: env,
        })
    }

    fn wait_exit(spawned: &SpawnedInstance, timeout: Duration) -> u32 {
        let deadline = Instant::now() + timeout;
        loop {
            if let Some(code) = process_exit_code(&spawned.process) {
                return code;
            }
            if Instant::now() >= deadline {
                panic!("子进程在 {timeout:?} 内没有退出");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    #[test]
    fn child_is_assigned_to_job_and_job_termination_reaps_it() {
        // ping 会跑 29 秒；我们不等它自然结束。
        let env = test_env_block();
        let system32 = PathBuf::from(
            std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_string()),
        )
        .join("System32");
        let spawned =
            spawn_cmd("/c ping -n 30 127.0.0.1 > nul", &system32, &env).expect("应能起 cmd.exe");

        assert!(spawned.pid > 0);
        let pids = job_process_ids(&spawned.job);
        assert!(
            pids.contains(&spawned.pid),
            "子进程 {} 必须在 job 内（实际 {pids:?}）",
            spawned.pid
        );

        terminate_job(&spawned.job, FORCE_KILL_EXIT_CODE).expect("强杀 job 应成功");
        let code = wait_exit(&spawned, Duration::from_secs(10));
        assert_eq!(code, FORCE_KILL_EXIT_CODE);
        assert!(
            job_process_ids(&spawned.job).is_empty(),
            "job 清空后不应再有 PID"
        );
    }

    #[test]
    fn missing_program_is_reported_with_win32_code() {
        let env = test_env_block();
        // 父目录存在、文件不存在 → ERROR_FILE_NOT_FOUND（2）；父目录也缺失会是 3。
        let system32 = PathBuf::from(
            std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_string()),
        )
        .join("System32");
        let program = system32.join("definitely-missing-node.exe");
        let mut command = vec!['x' as u16, 0];
        let err = spawn(&SpawnRequest {
            program: &program,
            command_line: &mut command,
            cwd: system32.as_path(),
            env_block: &env,
        })
        .expect_err("缺 node.exe 必须报错");
        assert_eq!(err.win32_code(), Some(ERROR_FILE_NOT_FOUND.0));
        assert!(err.to_string().contains("找不到可执行文件"), "{err}");
    }

    #[test]
    fn missing_workspace_is_reported_as_directory_error() {
        let env = test_env_block();
        let missing = PathBuf::from(r"D:\definitely\missing\workspace");
        let err =
            spawn_cmd("/c echo hi > nul", &missing, &env).expect_err("workspace 不存在必须报错");
        // CreateProcessW 对 lpCurrentDirectory 不存在返回 267（ERROR_DIRECTORY）。
        assert_eq!(err.win32_code(), Some(ERROR_DIRECTORY.0));
        assert!(err.to_string().contains("工作目录不存在"), "{err}");
    }

    #[test]
    fn job_process_ids_starts_empty_for_unused_job() {
        let job = create_job().expect("建 job 应成功");
        assert!(job_process_ids(&job).is_empty());
    }
}
