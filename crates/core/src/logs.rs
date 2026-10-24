//! 实例输出：管道读取线程 → 日志文件 + 内存环形缓冲（+ readiness 匹配）。
//!
//! 用户 2026-10-19 裁决：省略 `tee.mjs` 那一层 node，**管道所有权归实例侧**
//! （docs/structure.md §2.2.1）。所以这里自己做 tee 的三件事：
//!
//! 1. 按字节原样追加到 `.dsh-housing\logs\<capsuleId>-<profile>-YYYY-MM-DD.log`
//!    （不按行裁剪，行缓冲只用于 readiness 扫描）；
//! 2. 同时写入内存环形缓冲（GUI / 诊断查最近输出用，M3 才暴露）；
//! 3. stdout 额外喂给 `readiness.rs` 的滚动扫描器。
//!
//! 日志位置是被用户拍死的（§3.10）：**不写胶囊 logs\\、不写 `%APPDATA%`**。
//!
//! 读线程在管道 EOF（实例整棵树都退出）时自然结束；`LogPump::join_bounded`
//! 只等有限时间——若某个逃出 job 的孙进程还攥着管道写端，日志线程会被遗留，
//! 但不阻塞停止流程（剩余的读线程随进程退出一起消失）。

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::windows::io::{AsRawHandle, OwnedHandle};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use windows::Win32::Foundation::{ERROR_BROKEN_PIPE, ERROR_NO_DATA, HANDLE};
use windows::Win32::Storage::FileSystem::ReadFile;

use crate::readiness::{Scanner, TokenUrl};

/// 环形缓冲容量（两台实例各一份；够 GUI 展示最近几千行）。
pub const RING_CAPACITY: usize = 256 * 1024;

/// 日志格式错误（只可能是「线程起不来」，写文件失败只告警不致命）。
#[derive(Debug)]
pub struct LogError {
    detail: String,
}

impl std::fmt::Display for LogError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.detail)
    }
}

impl std::error::Error for LogError {}

/// 固定容量的滚动字节缓冲。
#[derive(Debug)]
pub struct RingBuffer {
    data: VecDeque<u8>,
    capacity: usize,
}

impl RingBuffer {
    pub fn new(capacity: usize) -> Self {
        Self {
            data: VecDeque::with_capacity(capacity.min(64 * 1024)),
            capacity,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) {
        if bytes.len() >= self.capacity {
            self.data.clear();
            self.data
                .extend(bytes[bytes.len() - self.capacity..].iter().copied());
            return;
        }
        let overflow = self.data.len() + bytes.len();
        if overflow > self.capacity {
            let drop_count = overflow - self.capacity;
            self.data.drain(..drop_count);
        }
        self.data.extend(bytes.iter().copied());
    }

    /// 最近 `max` 字节。
    pub fn tail(&self, max: usize) -> Vec<u8> {
        let take = max.min(self.data.len());
        self.data
            .iter()
            .skip(self.data.len() - take)
            .copied()
            .collect()
    }

    pub fn len(&self) -> usize {
        self.data.len()
    }

    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
}

/// 实例键 → 日志文件名前缀：`0.2.0-rc.2-default/web` → `0.2.0-rc.2-default-web`。
///
/// 只做文件名安全化（Windows 不允许 `\ / : * ? " < > |` 与控制字符）。
pub fn instance_log_stem(instance_key: &str) -> String {
    let mut stem: String = instance_key
        .chars()
        .map(|ch| match ch {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '-',
            ch if ch.is_control() => '_',
            ch => ch,
        })
        .collect();
    stem = stem.trim_matches(['.', ' ', '-']).to_string();
    if stem.is_empty() {
        "instance".to_string()
    } else {
        stem
    }
}

/// 实例日志文件：按本地日期切文件（跨零点自动换新文件）。
struct LogFile {
    dir: PathBuf,
    stem: String,
    file: Option<File>,
    date: (u16, u16, u16),
    /// 打开新文件时是否写一行分隔头（第一次打开与跨零点都写）。
    header: Option<String>,
}

impl LogFile {
    fn new(dir: &Path, stem: &str, header: Option<String>) -> Self {
        Self {
            dir: dir.to_path_buf(),
            stem: stem.to_string(),
            file: None,
            date: (0, 0, 0),
            header,
        }
    }

    fn path_for(&self, date: (u16, u16, u16)) -> PathBuf {
        self.dir.join(format!(
            "{}-{:04}-{:02}-{:02}.log",
            self.stem, date.0, date.1, date.2
        ))
    }

    fn current_path(&self) -> PathBuf {
        let date = crate::diag::local_date();
        self.path_for(date)
    }

    fn write(&mut self, bytes: &[u8]) {
        let date = crate::diag::local_date();
        if self.file.is_none() || date != self.date {
            if let Err(err) = self.open(date) {
                crate::diag::warn(&format!(
                    "打不开实例日志 {}：{err}；输出只进内存缓冲",
                    self.path_for(date).display()
                ));
                self.file = None;
                self.date = date;
                return;
            }
        }
        let Some(file) = self.file.as_mut() else {
            return;
        };
        if let Err(err) = file.write_all(bytes) {
            crate::diag::warn(&format!(
                "写实例日志失败（{}）：{err}",
                self.path_for(self.date).display()
            ));
            self.file = None;
            return;
        }
        let _ = file.flush();
    }

    fn open(&mut self, date: (u16, u16, u16)) -> std::io::Result<()> {
        std::fs::create_dir_all(&self.dir)?;
        let path = self.path_for(date);
        let mut file = OpenOptions::new().create(true).append(true).open(&path)?;
        if let Some(header) = self.header.take() {
            let line = format!(
                "[dsh-housing] {} {header}\r\n",
                crate::diag::timestamp_text()
            );
            file.write_all(line.as_bytes())?;
            file.flush()?;
        }
        self.file = Some(file);
        self.date = date;
        Ok(())
    }
}

struct Shared {
    log: Mutex<LogFile>,
    ring: Mutex<RingBuffer>,
    readiness: Mutex<Option<TokenUrl>>,
}

/// 一次实例运行的日志管线。
pub struct LogPump {
    log_path: PathBuf,
    shared: Arc<Shared>,
    handles: Vec<JoinHandle<()>>,
}

impl LogPump {
    /// 当前日志路径（跨零点后会变，展示用）。
    pub fn log_path(&self) -> PathBuf {
        self.shared
            .log
            .lock()
            .map(|log| log.current_path())
            .unwrap_or_else(|_| self.log_path.clone())
    }

    /// readiness 解析结果（supervisor 轮询它来把实例从 Starting 推进到 Running）。
    pub fn readiness(&self) -> Option<TokenUrl> {
        self.shared
            .readiness
            .lock()
            .map(|slot| slot.clone())
            .unwrap_or(None)
    }

    /// 最近输出（M3 的 GUI / 诊断用；M1 只在测试里读）。
    pub fn ring_tail(&self, max: usize) -> Vec<u8> {
        self.shared
            .ring
            .lock()
            .map(|ring| ring.tail(max))
            .unwrap_or_default()
    }

    /// 有界等待两个读线程结束；返回是否在超时前全部退出。
    ///
    /// 不无限等：逃出 job 的孙进程可能仍攥着管道写端（Chromium breakaway 本期不处理，
    /// §5.1），无限 join 会让「停止」永远收不了尾。
    pub fn join_bounded(self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut all_finished = true;
        for handle in self.handles {
            loop {
                if handle.is_finished() {
                    let _ = handle.join();
                    break;
                }
                if Instant::now() >= deadline {
                    all_finished = false;
                    break;
                }
                thread::sleep(Duration::from_millis(10));
            }
        }
        all_finished
    }
}

/// 起两根读取线程（stdout / stderr 各一）。
///
/// `context` 写进日志分隔头（例：`pid=1234 port=39412`）。
pub fn start(
    log_dir: &Path,
    instance_key: &str,
    context: &str,
    stdout: Option<OwnedHandle>,
    stderr: Option<OwnedHandle>,
) -> Result<LogPump, LogError> {
    let stem = instance_log_stem(instance_key);
    let header = format!(
        "实例输出开始：key={instance_key}；{context}；以下为字节级原样记录（stdout/stderr 合流按到达顺序）"
    );
    let log = LogFile::new(log_dir, &stem, Some(header));
    let log_path = log.current_path();
    let shared = Arc::new(Shared {
        log: Mutex::new(log),
        ring: Mutex::new(RingBuffer::new(RING_CAPACITY)),
        readiness: Mutex::new(None),
    });

    let mut handles = Vec::new();
    if let Some(stdout) = stdout {
        handles.push(spawn_reader(
            shared.clone(),
            stdout,
            "stdout",
            true,
            format!("{stem}-stdout"),
        )?);
    }
    if let Some(stderr) = stderr {
        handles.push(spawn_reader(
            shared.clone(),
            stderr,
            "stderr",
            false,
            format!("{stem}-stderr"),
        )?);
    }

    Ok(LogPump {
        log_path,
        shared,
        handles,
    })
}

/// 日志保留清理的结果（写 diag 用）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PurgeReport {
    /// 删掉的文件数。
    pub deleted: usize,
    /// 删失败的文件数（只告警，不阻断）。
    pub failed: usize,
    /// 还在保留期内、没动的日志文件数。
    pub kept: usize,
    /// 不认识的文件名 / 目录 / 非日志文件数（一律不碰）。
    pub skipped: usize,
}

/// 清理超过保留天数的日志文件（docs/structure.md §3.10）。
///
/// 判据是**文件名里的日期**（`housing-YYYY-MM-DD.log` 与 `<stem>-YYYY-MM-DD.log` 两种形状），
/// 不认 mtime（拷贝 / 备份不该重置保留期）；不认识的名字一律跳过——这个函数只在
/// `.dsh-housing\logs\` 里跑，但仍然不删非本程序命名的文件。`retention_days = 0` 表示不清理。
pub fn purge_old_logs(
    log_dir: &Path,
    retention_days: u16,
    today: (u16, u16, u16),
) -> std::io::Result<PurgeReport> {
    let mut report = PurgeReport::default();
    if retention_days == 0 {
        return Ok(report);
    }

    let entries = match std::fs::read_dir(log_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(report),
        Err(err) => return Err(err),
    };

    let today_days = days_from_civil(today.0 as i32, today.1 as u32, today.2 as u32);
    for entry in entries {
        let Ok(entry) = entry else {
            report.skipped += 1;
            continue;
        };
        let path = entry.path();
        if !path.is_file() {
            report.skipped += 1;
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            report.skipped += 1;
            continue;
        };
        let Some(stem) = name.strip_suffix(".log") else {
            report.skipped += 1;
            continue;
        };
        let Some((year, month, day)) = date_suffix(stem) else {
            report.skipped += 1;
            continue;
        };
        let age = today_days - days_from_civil(year as i32, month as u32, day as u32);
        if age > retention_days as i64 {
            match std::fs::remove_file(&path) {
                Ok(()) => report.deleted += 1,
                Err(_) => report.failed += 1,
            }
        } else {
            report.kept += 1;
        }
    }
    Ok(report)
}

/// 从日志文件名的主干里取日期后缀：`cap-web-2026-10-19` → `(2026, 10, 19)`。
///
/// 要求最后 10 个字符是 `YYYY-MM-DD`、且前面还有一个 `-`（这样 `2026-10-19.log`
/// 这种「没有前缀」的文件也不算我们的）。
fn date_suffix(stem: &str) -> Option<(u16, u16, u16)> {
    if stem.len() < 11 {
        return None;
    }
    let split = stem.len() - 11;
    if stem.as_bytes().get(split) != Some(&b'-') {
        return None;
    }
    parse_date(&stem[split + 1..])
}

/// 严格解析 `YYYY-MM-DD`（10 个字符、范围合法）。
fn parse_date(text: &str) -> Option<(u16, u16, u16)> {
    let bytes = text.as_bytes();
    if bytes.len() != 10 || bytes[4] != b'-' || bytes[7] != b'-' {
        return None;
    }
    let year: u16 = text.get(0..4)?.parse().ok()?;
    let month: u16 = text.get(5..7)?.parse().ok()?;
    let day: u16 = text.get(8..10)?.parse().ok()?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    Some((year, month, day))
}

/// 读 `<log_dir>` 里该实例**最新一天**日志文件的尾部（M3 控制台用，§5.8 D8）。
///
/// 实例停止后 `LogPump`（内存环形缓冲）已随实例释放，这时只能回读文件；文件名形状
/// 与 [`purge_old_logs`] 同一套判据（`<stem>-YYYY-MM-DD.log`），认不出的文件一律跳过。
/// 返回 `Ok(None)` = 没有这个实例的日志文件（不等于出错）。
pub fn tail_latest(
    log_dir: &Path,
    stem: &str,
    max_bytes: usize,
) -> std::io::Result<Option<String>> {
    let entries = match std::fs::read_dir(log_dir) {
        Ok(entries) => entries,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(err),
    };

    let mut newest: Option<((u16, u16, u16), PathBuf)> = None;
    for entry in entries {
        let Ok(entry) = entry else {
            continue;
        };
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let Some(name) = path.file_name().and_then(|name| name.to_str()) else {
            continue;
        };
        let Some(rest) = name
            .strip_suffix(".log")
            .and_then(|rest| rest.strip_prefix(stem))
            .and_then(|rest| rest.strip_prefix('-'))
        else {
            continue;
        };
        let Some(date) = parse_date(rest) else {
            continue;
        };
        if newest
            .as_ref()
            .map(|(current, _)| date > *current)
            .unwrap_or(true)
        {
            newest = Some((date, path));
        }
    }

    let Some((_, path)) = newest else {
        return Ok(None);
    };
    let mut file = File::open(&path)?;
    let length = file.metadata()?.len();
    let offset = length.saturating_sub(max_bytes as u64);
    if offset > 0 {
        file.seek(SeekFrom::Start(offset))?;
    }
    let mut buffer = Vec::new();
    file.read_to_end(&mut buffer)?;
    Ok(Some(String::from_utf8_lossy(&buffer).into_owned()))
}

/// 公历日期 → 距 1970-01-01 的天数（Howard Hinnant 的 `days_from_civil`）。
fn days_from_civil(year: i32, month: u32, day: u32) -> i64 {
    let year = if month <= 2 { year - 1 } else { year } as i64;
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month = month as i64;
    let day_of_year =
        (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day as i64 - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn spawn_reader(
    shared: Arc<Shared>,
    pipe: OwnedHandle,
    stream: &'static str,
    scan_readiness: bool,
    thread_name: String,
) -> Result<JoinHandle<()>, LogError> {
    thread::Builder::new()
        .name(thread_name)
        .spawn(move || read_loop(shared, pipe, stream, scan_readiness))
        .map_err(|err| LogError {
            detail: format!("启动 {stream} 读取线程失败：{err}"),
        })
}

fn read_loop(shared: Arc<Shared>, pipe: OwnedHandle, stream: &'static str, scan_readiness: bool) {
    let mut scanner = Scanner::new();
    let mut buffer = [0u8; 8192];
    loop {
        let mut read = 0u32;
        // SAFETY: 句柄由本线程独占持有；buffer 是栈上缓冲。
        let result = unsafe {
            ReadFile(
                HANDLE(pipe.as_raw_handle()),
                Some(&mut buffer),
                Some(&mut read),
                None,
            )
        };
        match result {
            Ok(()) => {
                if read == 0 {
                    break;
                }
                let bytes = &buffer[..read as usize];
                if scan_readiness {
                    if let Some(found) = scanner.feed(bytes) {
                        if let Ok(mut slot) = shared.readiness.lock() {
                            if slot.is_none() {
                                crate::diag::info(&format!(
                                    "readiness：已抓到 token URL（端口 {}）",
                                    found.port
                                ));
                                *slot = Some(found);
                            }
                        }
                    }
                }
                if let Ok(mut log) = shared.log.lock() {
                    log.write(bytes);
                }
                if let Ok(mut ring) = shared.ring.lock() {
                    ring.push(bytes);
                }
            }
            Err(err) => {
                let code = (err.code().0 as u32) & 0xFFFF;
                if code != ERROR_BROKEN_PIPE.0 && code != ERROR_NO_DATA.0 {
                    crate::diag::warn(&format!("读实例 {stream} 失败（Win32 {code}）：{err}"));
                }
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_root(label: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("..")
            .join("..")
            .join("target")
            .join("test-tmp")
            .join(format!("logs-{label}-{}-{nanos}", std::process::id()))
    }

    #[test]
    fn ring_keeps_only_the_tail() {
        let mut ring = RingBuffer::new(8);
        ring.push(b"abc");
        ring.push(b"def");
        assert_eq!(ring.len(), 6);
        ring.push(b"ghi");
        assert_eq!(ring.len(), 8);
        assert_eq!(ring.tail(100), b"bcdefghi".to_vec());
        assert_eq!(ring.tail(3), b"ghi".to_vec());

        // 一次塞入超过容量的块：只留最后 capacity 字节。
        ring.push(b"0123456789AB");
        assert_eq!(ring.len(), 8);
        assert_eq!(ring.tail(100), b"456789AB".to_vec());
    }

    #[test]
    fn log_stem_is_file_name_safe() {
        assert_eq!(
            instance_log_stem("0.2.0-rc.2-default/web"),
            "0.2.0-rc.2-default-web"
        );
        assert_eq!(instance_log_stem(r"weird/na:me*?"), "weird-na-me");
        assert_eq!(instance_log_stem("..."), "instance");
        assert_eq!(instance_log_stem(""), "instance");
    }

    #[test]
    fn log_file_rolls_over_on_date_change() {
        let dir = test_root("rollover");
        let mut log = LogFile::new(&dir, "cap-web", None);
        let first_day = (2026, 10, 19);
        log.write_on(first_day, b"day one\n").unwrap();
        log.write_on(first_day, b"still day one\n").unwrap();
        log.write_on((2026, 10, 20), b"day two\n").unwrap();

        let day_one = std::fs::read(dir.join("cap-web-2026-10-19.log")).unwrap();
        assert_eq!(day_one, b"day one\nstill day one\n".to_vec());
        let day_two = std::fs::read(dir.join("cap-web-2026-10-20.log")).unwrap();
        assert_eq!(day_two, b"day two\n".to_vec());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn log_file_writes_header_once() {
        let dir = test_root("header");
        let mut log = LogFile::new(&dir, "cap-web", Some("实例输出开始".to_string()));
        log.write_on((2026, 10, 19), b"hello\n").unwrap();
        log.write_on((2026, 10, 19), b"bye\n").unwrap();
        let text = std::fs::read_to_string(dir.join("cap-web-2026-10-19.log")).unwrap();
        assert!(text.starts_with("[dsh-housing] "));
        assert!(text.contains("实例输出开始"));
        assert_eq!(text.matches("实例输出开始").count(), 1);
        assert!(text.ends_with("hello\nbye\n"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn purge_deletes_only_dated_logs_older_than_retention() {
        let dir = test_root("purge");
        std::fs::create_dir_all(&dir).unwrap();
        for name in [
            "housing-2026-10-01.log",           // 19 天前 → 删
            "0.2.0-default-web-2026-09-30.log", // 20 天前 → 删
            "housing-2026-10-19.log",           // 1 天前 → 留
            "0.2.0-default-web-2026-10-20.log", // 今天 → 留
            "notes.txt",                        // 不是 .log → 跳过
            "cap-web.log",                      // 没有日期 → 跳过
            "2026-10-01.log",                   // 没有前缀 → 跳过（不是我们的命名）
            "housing-2026-13-40.log",           // 日期非法 → 跳过
        ] {
            std::fs::write(dir.join(name), b"x").unwrap();
        }

        let report = purge_old_logs(&dir, 14, (2026, 10, 20)).expect("清理应成功");
        assert_eq!(report.deleted, 2, "{report:?}");
        assert_eq!(report.kept, 2, "{report:?}");
        assert_eq!(report.failed, 0, "{report:?}");
        assert_eq!(report.skipped, 4, "{report:?}");

        assert!(!dir.join("housing-2026-10-01.log").exists());
        assert!(dir.join("housing-2026-10-19.log").exists());
        assert!(dir.join("notes.txt").exists());
        assert!(dir.join("cap-web.log").exists());
        assert!(dir.join("2026-10-01.log").exists());
        assert!(dir.join("housing-2026-13-40.log").exists());

        // retention = 0 → 一个都不删。
        let report = purge_old_logs(&dir, 0, (2026, 10, 20)).expect("清理应成功");
        assert_eq!(report, PurgeReport::default());

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn purge_is_quiet_when_log_dir_is_missing() {
        let missing = test_root("purge-missing");
        let report = purge_old_logs(&missing, 14, (2026, 10, 20)).expect("目录不存在不算错误");
        assert_eq!(report, PurgeReport::default());
    }

    #[test]
    fn date_suffix_parses_only_our_shape() {
        assert_eq!(date_suffix("housing-2026-10-19"), Some((2026, 10, 19)));
        assert_eq!(date_suffix("cap-web-2026-10-19"), Some((2026, 10, 19)));
        assert_eq!(date_suffix("2026-10-19"), None);
        assert_eq!(date_suffix("cap-web"), None);
        assert_eq!(date_suffix("cap-web-2026-10-99"), None);
    }

    #[test]
    fn tail_latest_reads_the_newest_instance_file() {
        let dir = test_root("tail");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("cap-web-2026-10-19.log"), b"old\n").unwrap();
        std::fs::write(dir.join("cap-web-2026-10-20.log"), b"newer\n").unwrap();
        std::fs::write(dir.join("other-web-2026-10-21.log"), b"other\n").unwrap();
        std::fs::write(dir.join("cap-web.log"), b"no date\n").unwrap();

        let text = tail_latest(&dir, "cap-web", 64).unwrap().unwrap();
        assert_eq!(text, "newer\n");

        // 只取最后 4 个字节（跨越 UTF-8 边界时用 lossy 解码，不 panic）。
        let text = tail_latest(&dir, "cap-web", 4).unwrap().unwrap();
        assert_eq!(text, "wer\n");

        assert!(tail_latest(&dir, "missing", 64).unwrap().is_none());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn tail_latest_is_quiet_when_log_dir_is_missing() {
        let missing = test_root("tail-missing");
        assert!(tail_latest(&missing, "cap-web", 64).unwrap().is_none());
    }

    /// 测试专用：绕过本地时间，指定日期写一段（模拟跨零点）。
    impl LogFile {
        fn write_on(&mut self, date: (u16, u16, u16), bytes: &[u8]) -> std::io::Result<()> {
            if self.file.is_none() || date != self.date {
                self.open(date)?;
            }
            let file = self.file.as_mut().unwrap();
            file.write_all(bytes)?;
            file.flush()
        }
    }
}
