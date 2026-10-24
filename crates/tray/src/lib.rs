//! dsh-housing 托盘 UI（crate `dsh_housing_tray`）。
//!
//! # 角色
//!
//! core 的**子进程**，只做四件事：装托盘图标、把 core 的实例快照画成菜单 / tooltip、
//! 把用户动作（登记胶囊 / 启动 / 停止 / 打开网页 / 停止全部 / 退出）通过 `ipc` 发给 core、
//! 以及**发现 core 已经退出时立刻自退**。它**不持有实例状态**，也不碰 Job Object
//! （docs/structure.md §2.2.1）。
//!
//! 退出路径（§3.15）：
//!
//! - 用户点「退出」→ 同步发 `quit` 给 core（core 停全部实例并退出）→ 自己销毁窗口；
//! - core 进程句柄有信号（正常退出或被硬杀）→ **立刻**销毁窗口（`WaitForSingleObject` 线程）；
//! - 拿不到 core PID 时退化为「连续 3 次状态请求失败」→ 自己退出（兜底）；
//! - 被 `taskkill` 杀掉 → **core 会按退避策略重新拉起**（M1 验收项，见 `runtime.rs`）。
//!
//! # 为什么还要自己的消息循环
//!
//! `Shell_NotifyIconW` 的回调消息要落到一个窗口上；`TrackPopupMenu` 需要一个能被
//! `SetForegroundWindow` 的前台窗口（message-only 窗口不行：菜单点了不消失）。
//! 所以是一个**隐藏的顶层窗口** + 自己的消息循环。
//!
//! # 运行环境的硬约束：完整性级别（2026-10-19 实测，别踩）
//!
//! `Shell_NotifyIconW(NIM_ADD)` 是**跨进程发给任务栏窗口**的调用，受 UIPI 约束：
//! 完整性级别更低的进程无法向更高的窗口发消息，失败时 shell32 返回
//! `E_ACCESSDENIED (0x80070005)`——**看起来像"没权限装图标"，实际是完整性级别不够**。
//! 本机 `EnableLUA=0`（UAC 关闭）→ 任务栏 `explorer.exe` 跑在 **High**，托盘进程必须
//! ≥ Medium；从带 `Low Mandatory Level` 标签的目录启动 exe 会掉到 Low，必失败。
//! 与代码无关，把 exe 放到不带该标签的目录即可（`test/*.ps1` 已内置）。

use std::ffi::c_void;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::PathBuf;
use std::ptr;
use std::sync::{Mutex, OnceLock};
use std::thread;
use std::time::Duration;

use dsh_housing_core::ipc::{self, Request, Response};
use dsh_housing_core::model::{InstanceView, StatusSnapshot};
use windows::core::{w, Error, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    E_ACCESSDENIED, HANDLE, HINSTANCE, HWND, LPARAM, LRESULT, POINT, WPARAM,
};
use windows::Win32::System::Com::{CoInitializeEx, CoTaskMemFree, COINIT_APARTMENTTHREADED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{
    OpenProcess, WaitForSingleObject, INFINITE, PROCESS_SYNCHRONIZE,
};
use windows::Win32::UI::Shell::{
    SHBrowseForFolderW, SHGetPathFromIDListW, Shell_NotifyIconW, BIF_NEWDIALOGSTYLE,
    BIF_RETURNONLYFSDIRS, BROWSEINFOW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE,
    NIM_MODIFY, NOTIFYICONDATAW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DestroyWindow,
    DispatchMessageW, GetCursorPos, GetMessageW, GetSystemMetrics, LoadIconW, LoadImageW,
    MessageBoxW, PostMessageW, PostQuitMessage, RegisterClassW, RegisterWindowMessageW,
    SetForegroundWindow, TrackPopupMenu, TranslateMessage, HICON, HMENU, IDI_APPLICATION,
    IMAGE_ICON, LR_SHARED, MB_ICONERROR, MB_ICONINFORMATION, MB_OK, MESSAGEBOX_STYLE, MF_GRAYED,
    MF_POPUP, MF_SEPARATOR, MF_STRING, MSG, SM_CXSMICON, SM_CYSMICON, TPM_BOTTOMALIGN,
    TPM_LEFTALIGN, TPM_RIGHTBUTTON, WINDOW_EX_STYLE, WM_APP, WM_CLOSE, WM_COMMAND, WM_DESTROY,
    WM_LBUTTONUP, WM_NULL, WM_RBUTTONUP, WNDCLASSW, WS_POPUP,
};

/// 应用图标资源 ID：`crates/app/build.rs` 把 `assets/favicon.ico` 编进 exe 的 `1 ICON`。
const APP_ICON_RESOURCE_ID: u16 = 1;
/// 窗口类名（注册后由本进程独占）。
const WINDOW_CLASS_NAME: PCWSTR = w!("DshHousingTrayWindow");
/// 窗口标题；窗口不可见，仅供调试器 / 任务管理器辨认。
const WINDOW_TITLE: PCWSTR = w!("dsh-housing");
/// 托盘图标 ID（同一窗口内唯一）。
const TRAY_ICON_ID: u32 = 1;
/// 托盘回调用自定义消息（`WM_APP` 起是应用私有区间）。
const TRAY_CALLBACK_MESSAGE: u32 = WM_APP + 1;
/// 后台轮询线程通知 UI 线程「快照更新了」。
const SNAPSHOT_UPDATED_MESSAGE: u32 = WM_APP + 2;
/// 固定菜单项 ID（动态项从 `MENU_ID_DYNAMIC_BASE` 起，避开它们）。
const MENU_ID_EXIT: usize = 1;
const MENU_ID_STOP_ALL: usize = 2;
const MENU_ID_REGISTER: usize = 3;
const MENU_ID_OPEN_CONSOLE: usize = 4;
const MENU_ID_DYNAMIC_BASE: usize = 1000;
/// 状态轮询间隔。
const POLL_INTERVAL: Duration = Duration::from_millis(1_000);
/// 连续失败多少次认定 core 已死（IPC 单连接串行，偶发忙等不算）。
const MAX_POLL_FAILURES: u32 = 3;
/// 同步命令的超时（core 的处理都是「立即返回」）。
const COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

/// 最近一次快照（UI 线程画菜单 / tooltip 用）。
static SNAPSHOT: OnceLock<Mutex<Option<StatusSnapshot>>> = OnceLock::new();
/// 菜单项 ID → 动作（每轮弹菜单时重建；`WM_COMMAND` 里按 ID 查）。
static MENU_ACTIONS: Mutex<Vec<(usize, MenuAction)>> = Mutex::new(Vec::new());
/// `TaskbarCreated` 的消息 ID（explorer 重启后要重装图标，M1 遗留项）。
static TASKBAR_CREATED: OnceLock<u32> = OnceLock::new();

/// 一个动态菜单项要发的命令。
#[derive(Debug, Clone)]
enum MenuAction {
    Start(String),
    Stop(String),
    OpenBrowser(String),
}

/// `--role=tray` 入口：core 通过环境变量告诉它管道名与 core 自己的 PID。
///
/// 返回进程退出码：0 = 正常退出；1 = 启动失败（弹窗报错）。
pub fn run() -> i32 {
    let pipe = match std::env::var(ipc::PIPE_ENV_VAR) {
        Ok(pipe) if !pipe.is_empty() => pipe,
        _ => {
            show_error(
                "dsh-housing 托盘启动失败：没有 DSH_HOUSING_PIPE 环境变量。\n\n\
                 托盘是 core 的子进程，只能由 core 拉起（--role=core）。",
            );
            return 1;
        }
    };

    // 文件夹选择框（SHBrowseForFolderW + BIF_NEWDIALOGSTYLE）要求本线程初始化 COM；
    // 托盘 UI 线程就是消息循环线程，按 STA 初始化一次（失败只影响对话框）。
    // SAFETY: 无参数；不需要对应的 CoUninitialize —— 进程退出即回收。
    let _hr = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };

    // SAFETY: Win32 调用都要求外部同步；托盘跑在独占的专用进程里，
    // 且所有窗口与图标句柄都在本函数作用域内创建与销毁。
    match unsafe { run_tray(pipe) } {
        Ok(()) => 0,
        Err(message) => {
            show_error(&format!("dsh-housing 托盘启动失败：\n\n{message}"));
            1
        }
    }
}

fn snapshot_slot() -> &'static Mutex<Option<StatusSnapshot>> {
    SNAPSHOT.get_or_init(|| Mutex::new(None))
}

fn latest_snapshot() -> Option<StatusSnapshot> {
    snapshot_slot().lock().ok().and_then(|slot| slot.clone())
}

fn set_snapshot(snapshot: StatusSnapshot) {
    if let Ok(mut slot) = snapshot_slot().lock() {
        *slot = Some(snapshot);
    }
}

/// 取托盘图标：优先用 exe 里嵌的 `assets/favicon.ico`，按 `SM_CXSMICON` 精确取小图标。
///
/// # Safety
///
/// 只取句柄，不碰任何跨线程状态；`LR_SHARED` 下句柄由系统缓存，不需要 DestroyIcon。
unsafe fn load_tray_icon(hinstance: HINSTANCE) -> std::result::Result<HICON, String> {
    let resource = PCWSTR(APP_ICON_RESOURCE_ID as usize as *const u16);
    let cx = GetSystemMetrics(SM_CXSMICON);
    let cy = GetSystemMetrics(SM_CYSMICON);
    if let Ok(handle) = LoadImageW(hinstance, resource, IMAGE_ICON, cx, cy, LR_SHARED) {
        return Ok(HICON(handle.0));
    }
    LoadIconW(HINSTANCE(ptr::null_mut()), IDI_APPLICATION)
        .map_err(|err| step_failed("LoadImageW/LoadIconW（加载托盘图标）", err))
}

/// 注册窗口类 → 建隐藏窗口 → 装托盘图标 → 起轮询 / core 监听线程 → 跑消息循环。
///
/// # Safety
///
/// 只能从「将要独占一个消息循环」的线程调用；函数内部不做任何线程同步。
unsafe fn run_tray(pipe: String) -> std::result::Result<(), String> {
    let hmodule = GetModuleHandleW(PCWSTR::null())
        .map_err(|err| step_failed("GetModuleHandleW（取自身模块句柄）", err))?;
    let hinstance = HINSTANCE(hmodule.0);
    let icon = load_tray_icon(hinstance)?;

    let window_class = WNDCLASSW {
        hInstance: hinstance,
        hIcon: icon,
        lpfnWndProc: Some(window_proc),
        lpszClassName: WINDOW_CLASS_NAME,
        ..Default::default()
    };
    if RegisterClassW(&window_class) == 0 {
        return Err(step_failed(
            "RegisterClassW（注册窗口类）",
            Error::from_win32(),
        ));
    }

    // 不传 WS_VISIBLE：窗口建出来就是隐藏的，永远不会出现在屏幕上。
    let hwnd = CreateWindowExW(
        WINDOW_EX_STYLE(0),
        WINDOW_CLASS_NAME,
        WINDOW_TITLE,
        WS_POPUP,
        0,
        0,
        0,
        0,
        HWND::default(),
        HMENU(ptr::null_mut()),
        hinstance,
        None,
    )
    .map_err(|err| step_failed("CreateWindowExW（建隐藏窗口）", err))?;

    add_icon(hwnd, icon)?;

    // explorer 重启（登录 / 崩溃）后任务栏会清掉所有图标：记下 TaskbarCreated 消息，
    // 在窗口过程里重装一次图标（§3.15）。
    TASKBAR_CREATED.get_or_init(|| RegisterWindowMessageW(w!("TaskbarCreated")));

    // 后台轮询 core 状态：UI 线程永远不阻塞在 IPC 上。
    let poll_hwnd = hwnd.0 as isize;
    thread::Builder::new()
        .name("tray-poller".to_string())
        .spawn(move || poll_core(poll_hwnd, pipe))
        .map_err(|err| step_failed("启动托盘状态轮询线程", Error::from(err)))?;

    // core 死亡监听：core 被硬杀时不等轮询超时，图标立刻消失（§3.15 / M1 遗留项）。
    if let Some(core_pid) = core_pid_from_env() {
        thread::Builder::new()
            .name("tray-core-watch".to_string())
            .spawn(move || watch_core(core_pid, poll_hwnd))
            .map_err(|err| step_failed("启动 core 退出监听线程", Error::from(err)))?;
    }

    let mut message = MSG::default();
    loop {
        let got = GetMessageW(&mut message, HWND::default(), 0, 0);
        if got.0 == -1 {
            let text = step_failed("GetMessageW（消息循环）", Error::from_win32());
            remove_icon(hwnd);
            let _ = DestroyWindow(hwnd);
            return Err(text);
        }
        if got.0 == 0 {
            break; // WM_QUIT
        }
        let _ = TranslateMessage(&message);
        DispatchMessageW(&message);
    }
    remove_icon(hwnd);
    Ok(())
}

fn add_icon(hwnd: HWND, icon: HICON) -> std::result::Result<(), String> {
    let data = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ICON_ID,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
        uCallbackMessage: TRAY_CALLBACK_MESSAGE,
        hIcon: icon,
        szTip: fixed_wide("dsh-housing（正在连接 core…）"),
        ..Default::default()
    };
    // SAFETY: 结构体与句柄都在本函数内有效。
    if !unsafe { Shell_NotifyIconW(NIM_ADD, &data) }.as_bool() {
        let err = Error::from_win32();
        let denied = err.code() == E_ACCESSDENIED;
        let mut message = step_failed("Shell_NotifyIconW(NIM_ADD)（装托盘图标）", err);
        if denied {
            message.push_str(
                "\n\n提示：这是 UIPI 完整性级别问题——本进程的完整性级别低于任务栏窗口。\
                 \n本机 UAC 关闭时任务栏由 High 完整性进程持有；请把 exe 放到不带\
                 \n「Mandatory Label\\Low Mandatory Level」标签的目录后再运行。",
            );
        }
        return Err(message);
    }
    Ok(())
}

fn core_pid_from_env() -> Option<u32> {
    std::env::var(ipc::CORE_PID_ENV_VAR)
        .ok()
        .and_then(|text| text.trim().parse::<u32>().ok())
        .filter(|pid| *pid != 0)
}

/// core 进程句柄一有信号（退出 / 被硬杀）就投递 `WM_CLOSE`。
///
/// 拿不到句柄（core 已经不在，或打不开）也直接退出——不留无主托盘。
fn watch_core(pid: u32, hwnd: isize) {
    // SAFETY: 只等句柄信号，不读进程内存；句柄归本线程所有。
    match unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) } {
        Ok(raw) => {
            let handle = unsafe { OwnedHandle::from_raw_handle(raw.0) };
            unsafe { WaitForSingleObject(HANDLE(handle.as_raw_handle()), INFINITE) };
        }
        Err(_) => {
            // core 已经不在：下面照常投递 WM_CLOSE。
        }
    }
    // SAFETY: 窗口句柄是主线程创建的；进程退出前一直有效。
    unsafe {
        let _ = PostMessageW(HWND(hwnd as *mut _), WM_CLOSE, WPARAM(0), LPARAM(0));
    }
}

/// 后台线程：每秒请求一次快照；连续失败 3 次认定 core 已死，投递 `WM_CLOSE`。
fn poll_core(hwnd: isize, pipe: String) {
    let mut failures = 0u32;
    loop {
        thread::sleep(POLL_INTERVAL);
        match ipc::call(&pipe, &Request::Status, Duration::from_secs(2)) {
            Ok(response) => {
                failures = 0;
                if let Some(snapshot) = response.status {
                    set_snapshot(snapshot);
                    // SAFETY: 窗口句柄是主线程创建的；进程退出前一直有效。
                    unsafe {
                        let _ = PostMessageW(
                            HWND(hwnd as *mut _),
                            SNAPSHOT_UPDATED_MESSAGE,
                            WPARAM(0),
                            LPARAM(0),
                        );
                    }
                }
                if response.shutdown {
                    // core 正在退出：托盘跟着退。
                    unsafe {
                        let _ = PostMessageW(HWND(hwnd as *mut _), WM_CLOSE, WPARAM(0), LPARAM(0));
                    }
                    return;
                }
            }
            Err(_) => {
                failures += 1;
                if failures >= MAX_POLL_FAILURES {
                    // core 不在了：托盘不能留成无主进程。
                    unsafe {
                        let _ = PostMessageW(HWND(hwnd as *mut _), WM_CLOSE, WPARAM(0), LPARAM(0));
                    }
                    return;
                }
            }
        }
    }
}

fn step_failed(step: &str, err: Error) -> String {
    format!("{step} 失败：{err}")
}

/// 窗口过程：托盘回调 → 弹菜单；菜单动作 → IPC 命令；拆窗口 → `WM_DESTROY` 收尾。
unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // 必须先处理：explorer 重启后要把图标重新装一遍（消息 ID 是运行时注册的，不是常量）。
    if message != 0 && Some(message) == TASKBAR_CREATED.get().copied() {
        if let Ok(hmodule) = GetModuleHandleW(PCWSTR::null()) {
            if let Ok(icon) = load_tray_icon(HINSTANCE(hmodule.0)) {
                let _ = add_icon(hwnd, icon);
                update_tooltip(hwnd);
            }
        }
        return LRESULT(0);
    }

    match message {
        TRAY_CALLBACK_MESSAGE => {
            match lparam.0 as u32 {
                WM_LBUTTONUP | WM_RBUTTONUP => show_menu(hwnd),
                _ => {}
            }
            LRESULT(0)
        }
        SNAPSHOT_UPDATED_MESSAGE => {
            update_tooltip(hwnd);
            LRESULT(0)
        }
        WM_COMMAND => {
            let id = wparam.0 & 0xffff;
            match id {
                MENU_ID_EXIT => {
                    send_command(Request::Quit);
                    let _ = DestroyWindow(hwnd);
                }
                MENU_ID_STOP_ALL => {
                    send_command(Request::StopAll);
                }
                MENU_ID_REGISTER => register_capsule(hwnd),
                MENU_ID_OPEN_CONSOLE => open_console(),
                other => dispatch_dynamic(other),
            }
            LRESULT(0)
        }
        WM_CLOSE => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }
        WM_DESTROY => {
            remove_icon(hwnd);
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, message, wparam, lparam),
    }
}

/// 找到 pipe 名字发一条命令；`show_errors = true` 时失败弹框（登记用）。
fn call_core(request: Request, show_errors: bool) -> std::result::Result<Response, String> {
    let Some(pipe) = std::env::var(ipc::PIPE_ENV_VAR)
        .ok()
        .filter(|text| !text.is_empty())
    else {
        let message = "没有 DSH_HOUSING_PIPE 环境变量（托盘不是由 core 拉起的？）".to_string();
        if show_errors {
            show_error(&message);
        }
        return Err(message);
    };
    match ipc::call(&pipe, &request, COMMAND_TIMEOUT) {
        Ok(response) if response.ok => Ok(response),
        Ok(response) => {
            let message = response
                .error
                .unwrap_or_else(|| "请求被 core 拒绝（没有给出原因）".to_string());
            if show_errors {
                show_error(&message);
            }
            Err(message)
        }
        Err(err) => {
            let message = format!("core 不可达：{err}");
            if show_errors {
                show_error(&message);
            }
            Err(message)
        }
    }
}

/// 固定菜单项的命令：失败不弹框（退出 / 停止失败最多是「没退成」，用户还能再点一次）。
fn send_command(request: Request) {
    let _ = call_core(request, false);
}

/// 动态菜单项：按 ID 找动作 → 发命令。
fn dispatch_dynamic(id: usize) {
    let action = match MENU_ACTIONS.lock() {
        Ok(actions) => actions
            .iter()
            .find(|(item_id, _)| *item_id == id)
            .map(|(_, action)| action.clone()),
        Err(_) => None,
    };
    match action {
        Some(MenuAction::Start(key)) => {
            if let Err(message) = call_core(Request::Start { key }, false) {
                show_error(&format!("启动失败：\n\n{message}"));
            }
        }
        Some(MenuAction::Stop(key)) => {
            if let Err(message) = call_core(Request::Stop { key }, false) {
                show_error(&format!("停止失败：\n\n{message}"));
            }
        }
        Some(MenuAction::OpenBrowser(key)) => {
            // 没有 token URL 时 core 返回 ok + notice，这里把它显示出来。
            match call_core(Request::OpenBrowser { key }, false) {
                Ok(response) => {
                    if let Some(notice) = response.notice {
                        show_info(&notice);
                    }
                }
                Err(message) => show_error(&format!("打开网页失败：\n\n{message}")),
            }
        }
        None => {}
    }
}

/// 「打开控制台…」：core 回控制台 URL（带 consoleToken）并用默认浏览器打开（M3）。
///
/// 控制台没起来（端口绑定失败）时 core 回失败原因，这里弹框说清楚，不静默。
fn open_console() {
    match call_core(Request::OpenConsole, false) {
        Ok(response) => {
            if let Some(notice) = response.notice {
                show_info(&notice);
            }
        }
        Err(message) => show_error(&format!("打开控制台失败：\n\n{message}")),
    }
}

/// 「登记胶囊…」：选目录 → 交给 core 校验并落盘（§3.13）。
fn register_capsule(hwnd: HWND) {
    let Some(root) = pick_capsule_root(hwnd) else {
        return; // 用户取消
    };
    // 失败时 call_core 已经把原因弹过框了；成功时把登记摘要（含告警）显示出来。
    if let Ok(response) = call_core(Request::Register { root }, true) {
        if let Some(notice) = response.notice {
            show_info(&notice);
        }
    }
}

/// 文件夹选择框（`SHBrowseForFolderW`）；取消返回 `None`。
fn pick_capsule_root(hwnd: HWND) -> Option<PathBuf> {
    let mut display = [0u16; 260];
    let title = to_wide("选择 dsh 胶囊目录（含 instance.json 的那一层）");
    let browse = BROWSEINFOW {
        hwndOwner: hwnd,
        pidlRoot: ptr::null_mut(),
        pszDisplayName: PWSTR(display.as_mut_ptr()),
        lpszTitle: PCWSTR(title.as_ptr()),
        ulFlags: BIF_RETURNONLYFSDIRS | BIF_NEWDIALOGSTYLE,
        lpfn: None,
        lParam: LPARAM(0),
        iImage: 0,
    };
    // SAFETY: 结构体与字符串缓冲区在调用期间存活；COM 已在 `run()` 里按 STA 初始化。
    let pidl = unsafe { SHBrowseForFolderW(&browse) };
    if pidl.is_null() {
        return None;
    }
    let mut path = [0u16; 260];
    let ok = unsafe { SHGetPathFromIDListW(pidl, &mut path) };
    // PIDL 由 shell 分配，必须还给 COM 分配器。
    unsafe { CoTaskMemFree(Some(pidl as *const c_void)) };
    if !ok.as_bool() {
        return None;
    }
    let length = path.iter().position(|unit| *unit == 0).unwrap_or(0);
    if length == 0 {
        return None;
    }
    Some(PathBuf::from(String::from_utf16_lossy(&path[..length])))
}

/// 在光标处弹出托盘菜单（§3.15）。
unsafe fn show_menu(hwnd: HWND) {
    let Ok(menu) = CreatePopupMenu() else {
        return;
    };
    let snapshot = latest_snapshot();
    let mut actions: Vec<(usize, MenuAction)> = Vec::new();
    let mut next_id = MENU_ID_DYNAMIC_BASE;
    let mut any_active = false;

    match &snapshot {
        None => {
            let _ = AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, w!("（正在连接 core…）"));
        }
        Some(snapshot) if snapshot.instances.is_empty() => {
            let _ = AppendMenuW(
                menu,
                MF_STRING | MF_GRAYED,
                0,
                w!("没有实例（先「登记胶囊…」）"),
            );
        }
        Some(snapshot) => {
            for instance in &snapshot.instances {
                any_active |= is_active(&instance.state);
                let Ok(sub) = CreatePopupMenu() else {
                    continue;
                };
                if instance.token_url.is_some() {
                    append_action(
                        sub,
                        next_id,
                        "打开网页",
                        MenuAction::OpenBrowser(instance.key.clone()),
                        &mut actions,
                    );
                    next_id += 1;
                }
                match instance.state.as_str() {
                    "stopped" | "failed" => {
                        append_action(
                            sub,
                            next_id,
                            "启动",
                            MenuAction::Start(instance.key.clone()),
                            &mut actions,
                        );
                        next_id += 1;
                    }
                    "starting" | "running" => {
                        append_action(
                            sub,
                            next_id,
                            "停止",
                            MenuAction::Stop(instance.key.clone()),
                            &mut actions,
                        );
                        next_id += 1;
                    }
                    _ => {
                        let _ = AppendMenuW(sub, MF_STRING | MF_GRAYED, 0, w!("（正在停止…）"));
                    }
                }
                let line = menu_line(instance);
                let text = to_wide(&line);
                let _ = AppendMenuW(menu, MF_POPUP, sub.0 as usize, PCWSTR(text.as_ptr()));
            }
        }
    }

    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, w!(""));
    let _ = AppendMenuW(menu, MF_STRING, MENU_ID_OPEN_CONSOLE, w!("打开控制台…"));
    let _ = AppendMenuW(menu, MF_STRING, MENU_ID_REGISTER, w!("登记胶囊…"));
    if any_active {
        let _ = AppendMenuW(menu, MF_STRING, MENU_ID_STOP_ALL, w!("停止全部实例"));
    }
    let _ = AppendMenuW(menu, MF_STRING, MENU_ID_EXIT, w!("退出 dsh-housing"));

    // 换动作表要在弹菜单之前：`WM_COMMAND` 是弹菜单期间同步派发的。
    if let Ok(mut slot) = MENU_ACTIONS.lock() {
        *slot = actions;
    }

    let mut point = POINT::default();
    if GetCursorPos(&mut point).is_ok() {
        // 弹菜单前把窗口提到前台，否则点到别处菜单不会消失。
        let _ = SetForegroundWindow(hwnd);
        let _ = TrackPopupMenu(
            menu,
            TPM_RIGHTBUTTON | TPM_BOTTOMALIGN | TPM_LEFTALIGN,
            point.x,
            point.y,
            0,
            hwnd,
            None,
        );
        // 菜单收尾后补一条空消息：Win32 托盘菜单的经典要求，避免菜单残留。
        let _ = PostMessageW(hwnd, WM_NULL, WPARAM(0), LPARAM(0));
    }

    // 旧 ID 不能留：下一轮同一个 ID 可能指向别的实例。
    if let Ok(mut slot) = MENU_ACTIONS.lock() {
        slot.clear();
    }
    let _ = DestroyMenu(menu);
}

unsafe fn append_action(
    sub: HMENU,
    id: usize,
    label: &str,
    action: MenuAction,
    actions: &mut Vec<(usize, MenuAction)>,
) {
    let text = to_wide(label);
    if AppendMenuW(sub, MF_STRING, id, PCWSTR(text.as_ptr())).is_ok() {
        actions.push((id, action));
    }
}

fn is_active(state: &str) -> bool {
    matches!(state, "starting" | "running")
}

/// 菜单里的一行：`0.2.0-rc.2-default · web  39412  运行中  pid 1234  412 MB · CPU 12%`。
fn menu_line(instance: &InstanceView) -> String {
    let state = match instance.state.as_str() {
        "starting" => "启动中",
        "running" => "运行中",
        "stopping" => "停止中",
        "failed" => "失败",
        _ => "已停止",
    };
    let name = instance.alias.as_deref().unwrap_or(&instance.capsule_id);
    // v0.2（§3.20）：默认别名已经以 profile 结尾（`Deepseek Harness 0.2.0-rc.2 web`），
    // 再拼一次就成了「… web · web」；只有名字里没带 profile 时才补。
    let mut line = if name.ends_with(&format!(" {}", instance.profile)) {
        format!("{name}  {}  {}", instance.port, state)
    } else {
        format!(
            "{name} · {}  {}  {}",
            instance.profile, instance.port, state
        )
    };
    if let Some(pid) = instance.pid {
        line.push_str(&format!("  pid {pid}"));
    }
    if let Some(metrics) = &instance.metrics {
        line.push_str(&format!(
            "  {} · CPU {:.0}%",
            format_bytes(metrics.working_set_bytes),
            metrics.cpu_percent
        ));
    }
    if let Some(detail) = &instance.detail {
        line.push_str(&format!("  （{}）", truncate_chars(detail, 24)));
    }
    line
}

fn format_bytes(bytes: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    const GB: f64 = MB * 1024.0;
    let value = bytes as f64;
    if value >= GB {
        format!("{:.1} GB", value / GB)
    } else {
        format!("{:.0} MB", value / MB)
    }
}

fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut output: String = text.chars().take(max).collect();
    output.push('…');
    output
}

fn tooltip_text() -> String {
    let Some(snapshot) = latest_snapshot() else {
        return "dsh-housing（正在连接 core…）".to_string();
    };
    let total = snapshot.instances.len();
    if total == 0 {
        return "dsh-housing：就绪（无胶囊）".to_string();
    }
    let active = snapshot
        .instances
        .iter()
        .filter(|instance| is_active(&instance.state))
        .count();
    let failed = snapshot
        .instances
        .iter()
        .filter(|instance| instance.state == "failed")
        .count();
    let mut working_set = 0u64;
    let mut cpu = 0.0f32;
    for instance in &snapshot.instances {
        if let Some(metrics) = &instance.metrics {
            working_set += metrics.working_set_bytes;
            cpu += metrics.cpu_percent;
        }
    }

    let mut text = format!("dsh-housing：{active}/{total} 个实例运行中");
    if working_set > 0 {
        text.push_str(&format!(" · 内存 {}", format_bytes(working_set)));
    }
    if cpu > 0.0 {
        text.push_str(&format!(" · CPU {cpu:.0}%"));
    }
    if failed > 0 {
        text.push_str(&format!(" · {failed} 个失败"));
    }
    text
}

/// 刷新 tooltip（快照更新后调用）。
unsafe fn update_tooltip(hwnd: HWND) {
    let data = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ICON_ID,
        uFlags: NIF_TIP,
        szTip: fixed_wide(&tooltip_text()),
        ..Default::default()
    };
    let _ = Shell_NotifyIconW(NIM_MODIFY, &data);
}

/// 摘掉托盘图标（`NIM_DELETE` 只认 `hWnd` + `uID`）。
unsafe fn remove_icon(hwnd: HWND) {
    let data = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ICON_ID,
        ..Default::default()
    };
    let _ = Shell_NotifyIconW(NIM_DELETE, &data);
}

/// 出错弹窗：托盘没有控制台，失败只能这样让人看见。
fn show_error(message: &str) {
    show_box(message, MB_ICONERROR);
}

/// 提示弹窗（登记结果 / 没有 token URL 之类）。
fn show_info(message: &str) {
    show_box(message, MB_ICONINFORMATION);
}

fn show_box(message: &str, icon: MESSAGEBOX_STYLE) {
    let text = to_wide(message);
    let caption = to_wide("dsh-housing");
    // SAFETY: 两个缓冲区都以 NUL 结尾，且在调用期间存活。
    unsafe {
        MessageBoxW(
            HWND::default(),
            PCWSTR(text.as_ptr()),
            PCWSTR(caption.as_ptr()),
            MB_OK | icon,
        );
    }
}

/// 定长宽字符缓冲区（`szTip` 这类字段），自动 NUL 结尾、超长截断。
fn fixed_wide<const N: usize>(text: &str) -> [u16; N] {
    let mut buffer = [0u16; N];
    for (slot, unit) in buffer.iter_mut().zip(text.encode_utf16().take(N - 1)) {
        *slot = unit;
    }
    buffer
}

fn to_wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(alias: Option<&str>, profile: &str) -> InstanceView {
        InstanceView {
            key: format!("cap/{profile}"),
            capsule_id: "cap".to_string(),
            profile: profile.to_string(),
            alias: alias.map(str::to_string),
            port: 39412,
            host: None,
            state: "stopped".to_string(),
            detail: None,
            pid: None,
            token_url: None,
            metrics: None,
        }
    }

    #[test]
    fn menu_line_does_not_repeat_a_profile_suffix() {
        // §3.20 的默认别名已以 profile 结尾，不能再拼一次「· web」。
        let line = menu_line(&view(Some("Deepseek Harness 0.2.0-rc.2 web"), "web"));
        assert_eq!(line, "Deepseek Harness 0.2.0-rc.2 web  39412  已停止");
    }

    #[test]
    fn menu_line_keeps_profile_for_capsule_ids_and_plain_aliases() {
        assert_eq!(menu_line(&view(None, "web")), "cap · web  39412  已停止");
        assert_eq!(
            menu_line(&view(Some("日常"), "web")),
            "日常 · web  39412  已停止"
        );
        // 只是「以 web 结尾」但不是一个独立词：仍然补 profile，避免误判。
        assert_eq!(
            menu_line(&view(Some("myweb"), "web")),
            "myweb · web  39412  已停止"
        );
    }
}
