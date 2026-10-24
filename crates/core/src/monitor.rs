//! 资源采样：job 级 CPU / 峰值内存 / IO + 逐 PID 工作集（docs/structure.md §3.9）。
//!
//! 口径（与任务管理器的差异写在 §3.9）：
//!
//! - **CPU%** = 两次采样之间 job 累计 CPU（用户态 + 内核态）的差分 ÷ 墙上时间 ÷ 逻辑核数 × 100；
//! - **峰值内存** = `JOBOBJECT_EXTENDED_LIMIT_INFORMATION.PeakJobMemoryUsed`（job 生命周期内的峰值，
//!   不随采样清零）；
//! - **工作集** = job 内每个 PID 的 `WorkingSetSize` 之和（含共享页，所以通常比任务管理器的
//!   「内存」列偏大）；
//! - **IO** = job 级 `IoInfo` 的读写字节数（不逐 PID）；
//! - **进程数** = job 内 PID 数（含 MCP 子树）。
//!
//! 采样状态（上一次的 CPU 累计值与时刻）由调用方持有（`instance.rs` 的 `Sampler`），
//! 这样本模块是无状态的、可单测。

use std::ffi::c_void;
use std::mem::size_of;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use windows::Win32::Foundation::HANDLE;
use windows::Win32::System::JobObjects::{
    JobObjectBasicAndIoAccountingInformation, JobObjectExtendedLimitInformation,
    QueryInformationJobObject, JOBOBJECT_BASIC_AND_IO_ACCOUNTING_INFORMATION,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
};
use windows::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows::Win32::System::SystemInformation::{GetSystemInfo, SYSTEM_INFO};
use windows::Win32::System::Threading::{OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION};

use crate::model::InstanceMetrics;

/// 每个实例一份的采样状态（上一次的 job 累计 CPU 与时刻）。
#[derive(Debug, Clone, Copy, Default)]
pub struct Sampler {
    previous: Option<(u64, Instant)>,
}

impl Sampler {
    pub fn new() -> Self {
        Self::default()
    }
}

/// 采样一次。job 已经空掉 / 查询失败时返回 `None`（调用方保持上一次的数字）。
pub fn sample(job: &OwnedHandle, sampler: &mut Sampler) -> Option<InstanceMetrics> {
    let job_handle = HANDLE(job.as_raw_handle());
    let (cpu_100ns, io_read, io_write) = query_accounting(job_handle)?;
    let peak_memory_bytes = query_peak_memory(job_handle).unwrap_or(0);

    let pids = crate::job::job_process_ids(job);
    let mut working_set_bytes = 0u64;
    for pid in &pids {
        working_set_bytes += process_working_set(*pid).unwrap_or(0);
    }

    let now = Instant::now();
    let cpu_percent = match sampler.previous {
        Some((previous_cpu, previous_at)) => cpu_percent(
            cpu_100ns.saturating_sub(previous_cpu),
            now.saturating_duration_since(previous_at),
            logical_cores(),
        ),
        None => 0.0,
    };
    sampler.previous = Some((cpu_100ns, now));

    Some(InstanceMetrics {
        cpu_percent,
        cpu_time_ms: cpu_100ns / 10_000,
        peak_memory_bytes,
        working_set_bytes,
        process_count: u32::try_from(pids.len()).unwrap_or(u32::MAX),
        io_read_bytes: io_read,
        io_write_bytes: io_write,
    })
}

/// CPU 百分比（纯函数，单测直接喂）。
///
/// `delta_100ns` 是本次窗口内的 CPU 时间（100 ns 单位），`wall` 是窗口长度，
/// `cores` 是逻辑核数。结果夹在 `0..=100`。
pub fn cpu_percent(delta_100ns: u64, wall: Duration, cores: u32) -> f32 {
    let wall_ms = wall.as_secs_f64() * 1_000.0;
    if wall_ms <= 0.0 {
        return 0.0;
    }
    let cpu_ms = delta_100ns as f64 / 10_000.0;
    let cores = cores.max(1) as f64;
    let percent = (cpu_ms / wall_ms) / cores * 100.0;
    percent.clamp(0.0, 100.0) as f32
}

/// 逻辑核数（进程内缓存一次）。
pub fn logical_cores() -> u32 {
    static CORES: OnceLock<u32> = OnceLock::new();
    *CORES.get_or_init(|| {
        let mut info = SYSTEM_INFO::default();
        // SAFETY: 传的是本栈上的有效缓冲。
        unsafe { GetSystemInfo(&mut info) };
        info.dwNumberOfProcessors.max(1)
    })
}

fn query_accounting(job: HANDLE) -> Option<(u64, u64, u64)> {
    let mut info = JOBOBJECT_BASIC_AND_IO_ACCOUNTING_INFORMATION::default();
    let length = size_of::<JOBOBJECT_BASIC_AND_IO_ACCOUNTING_INFORMATION>() as u32;
    // SAFETY: 句柄来自 OwnedHandle；缓冲大小与类型匹配；不给返回长度。
    unsafe {
        QueryInformationJobObject(
            job,
            JobObjectBasicAndIoAccountingInformation,
            &mut info as *mut _ as *mut c_void,
            length,
            None,
        )
        .ok()?;
    }
    let cpu = (info.BasicInfo.TotalUserTime.max(0) + info.BasicInfo.TotalKernelTime.max(0)) as u64;
    Some((
        cpu,
        info.IoInfo.ReadTransferCount,
        info.IoInfo.WriteTransferCount,
    ))
}

fn query_peak_memory(job: HANDLE) -> Option<u64> {
    let mut info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION::default();
    let length = size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32;
    // SAFETY: 同上。
    unsafe {
        QueryInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &mut info as *mut _ as *mut c_void,
            length,
            None,
        )
        .ok()?;
    }
    Some(info.PeakJobMemoryUsed as u64)
}

/// 单个进程的工作集。进程已经退出 / 打不开时返回 `None`（PID 复用有竞态，best-effort）。
fn process_working_set(pid: u32) -> Option<u64> {
    // SAFETY: 只查内存计数，不碰进程地址空间。
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    let handle = unsafe { OwnedHandle::from_raw_handle(handle.0) };
    let mut counters = PROCESS_MEMORY_COUNTERS::default();
    let length = size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
    // SAFETY: 句柄有效；缓冲大小与类型匹配。
    unsafe {
        GetProcessMemoryInfo(HANDLE(handle.as_raw_handle()), &mut counters, length).ok()?;
    }
    Some(counters.WorkingSetSize as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn cpu_percent_is_relative_to_all_cores() {
        // 1 秒窗口里跑满 1 个核：4 核机器 = 25%。
        assert_eq!(cpu_percent(10_000_000, Duration::from_secs(1), 4), 25.0);
        // 跑满 4 个核 = 100%。
        assert_eq!(cpu_percent(40_000_000, Duration::from_secs(1), 4), 100.0);
        // 零窗口不除零。
        assert_eq!(cpu_percent(123, Duration::ZERO, 4), 0.0);
        // 夹在 0..=100。
        assert_eq!(cpu_percent(1_000_000_000, Duration::from_secs(1), 1), 100.0);
    }

    #[test]
    fn logical_cores_is_at_least_one() {
        assert!(logical_cores() >= 1);
    }

    // ── 真实 job 上的采样（与 job.rs 的 spawn 一起验证） ──

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

    #[test]
    fn sampling_a_live_job_reports_processes_and_memory() {
        let env = test_env_block();
        let system32 = PathBuf::from(
            std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".to_string()),
        )
        .join("System32");
        let program = cmd_exe();
        let command = format!("{} /c ping -n 6 127.0.0.1 > nul", program.display())
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect::<Vec<u16>>();
        let spawned = crate::job::spawn(&crate::job::SpawnRequest {
            program: &program,
            command_line: &command,
            cwd: &system32,
            env_block: &env,
        })
        .expect("应能起 cmd.exe");

        let mut sampler = Sampler::new();
        let first = sample(&spawned.job, &mut sampler).expect("第一次采样应成功");
        assert!(first.process_count >= 1, "{first:?}");
        assert!(first.working_set_bytes > 0, "{first:?}");
        assert_eq!(first.cpu_percent, 0.0, "第一次采样没有差分基线");

        std::thread::sleep(Duration::from_millis(300));
        let second = sample(&spawned.job, &mut sampler).expect("第二次采样应成功");
        assert!(second.process_count >= 1, "{second:?}");
        assert!(second.peak_memory_bytes > 0, "{second:?}");
        assert!((0.0..=100.0).contains(&second.cpu_percent), "{second:?}");

        let _ = crate::job::terminate_job(&spawned.job, crate::job::FORCE_KILL_EXIT_CODE);
        let _ = crate::job::wait_process(&spawned.process, 5_000);
    }
}
