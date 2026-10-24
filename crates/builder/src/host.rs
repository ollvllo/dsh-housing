//! builder 与 core 之间唯一的运行时接口：**跑一条命令**。
//!
//! 为什么进程 spawn / Job Object / 日志环不在这里：那些是 Win32 与 core 的职责
//! （`crates/core/src/build.rs` 实现本 trait）。builder 只决定「跑什么、传什么环境」，
//! 这样它能脱离 Windows 单测（用假 host 记录命令、回放输出）。

use std::ffi::OsString;
use std::path::{Path, PathBuf};

use crate::error::BuildError;
use crate::request::Step;

/// 一条要跑的命令。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    /// 日志里显示这条命令在干什么（例：`下载 node-v24.22.0-win-x64.zip`）。
    pub label: String,
    /// 可执行文件（绝对路径或 `cmd.exe` 这类在 PATH 上能找到的名字）。
    pub program: PathBuf,
    pub args: Vec<OsString>,
    /// 子进程 cwd。
    pub cwd: PathBuf,
    /// **覆盖**（不是全量）继承来的环境变量；`core` 负责与父进程环境合并、排序、编码。
    pub env: Vec<(OsString, OsString)>,
    /// 前置到 `PATH` 的目录（顺序即优先级）。
    pub path_prepend: Vec<PathBuf>,
    /// 是否需要把 stdout 收进返回值（版本断言要用；npm install 不需要）。
    pub capture_stdout: bool,
}

impl Command {
    pub fn new(
        label: impl Into<String>,
        program: impl Into<PathBuf>,
        cwd: impl Into<PathBuf>,
    ) -> Self {
        Self {
            label: label.into(),
            program: program.into(),
            args: Vec::new(),
            cwd: cwd.into(),
            env: Vec::new(),
            path_prepend: Vec::new(),
            capture_stdout: false,
        }
    }

    pub fn arg(mut self, arg: impl Into<OsString>) -> Self {
        self.args.push(arg.into());
        self
    }

    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<OsString>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    pub fn env(mut self, key: impl Into<OsString>, value: impl Into<OsString>) -> Self {
        self.env.push((key.into(), value.into()));
        self
    }

    /// 批量追加环境变量覆盖。
    pub fn env_into<I>(mut self, envs: I) -> Self
    where
        I: IntoIterator<Item = (String, String)>,
    {
        self.env.extend(
            envs.into_iter()
                .map(|(key, value)| (OsString::from(key), OsString::from(value))),
        );
        self
    }

    pub fn path_prepend(mut self, path: impl Into<PathBuf>) -> Self {
        self.path_prepend.push(path.into());
        self
    }

    pub fn capture(mut self) -> Self {
        self.capture_stdout = true;
        self
    }

    /// 命令行的可读形态（写日志用；不是给 CreateProcessW 的那份 UTF-16）。
    pub fn display(&self) -> String {
        let mut text = self.program.display().to_string();
        for arg in &self.args {
            text.push(' ');
            let arg = arg.to_string_lossy();
            if arg.is_empty() || arg.contains([' ', '\t', '"']) {
                text.push('"');
                text.push_str(&arg);
                text.push('"');
            } else {
                text.push_str(&arg);
            }
        }
        text
    }
}

/// 一条命令的结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outcome {
    /// 进程退出码（Windows 口径；非 0 由调用方决定是不是失败）。
    pub code: i32,
    /// stdout（只有 `capture_stdout` 的命令才有；上限见 core 侧实现）。
    pub stdout: String,
    /// stderr 尾部（失败时拼进错误信息；成功时也会被写进日志环）。
    pub stderr_tail: String,
}

impl Outcome {
    pub fn ok(&self) -> bool {
        self.code == 0
    }
}

/// 构建期间的宿主能力（由 core 实现）。
pub trait Host {
    /// 跑一条命令并等它结束；输出边跑边进日志环。
    ///
    /// 失败（连不上进程 / 用户取消）返回 `Err`；**进程退出码非 0 不算 Err**
    /// （由 builder 按语义决定，例如 `--version` 非 0 才算错）。
    fn run(&self, command: &Command) -> Result<Outcome, BuildError>;

    /// 写一行构建日志（core 会加时间戳并进环形缓冲 + 日志文件）。
    fn log(&self, message: &str);

    /// 进入下一步（面板高亮 + 「第 n/6 步」）。
    fn step(&self, step: Step);

    /// 用户是否已请求取消（每步开始前、每条命令前后都要查）。
    fn cancelled(&self) -> bool;
}

/// 便捷：把 `Path` 转成可放进 `Command::arg` 的 `OsString`。
pub fn os(path: &Path) -> OsString {
    path.as_os_str().to_os_string()
}
