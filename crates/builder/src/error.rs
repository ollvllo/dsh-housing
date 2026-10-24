//! 构建错误：带「哪一步失败」与人话原因（docs/structure.md §3.24）。

use std::fmt;

use crate::request::Step;

/// 一次构建失败的原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuildError {
    /// 参数不合法（还没开始动磁盘）。
    Invalid(String),
    /// 某一步失败。
    Step { step: Step, message: String },
    /// 用户取消（`core` 终止了构建子进程）。
    Cancelled { step: Step },
}

impl BuildError {
    pub fn step(step: Step, message: impl Into<String>) -> Self {
        Self::Step {
            step,
            message: message.into(),
        }
    }

    pub fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid(message.into())
    }

    pub fn cancelled(step: Step) -> Self {
        Self::Cancelled { step }
    }

    /// 是否为用户取消（`core` 用它把状态落到 `canceled` 而不是 `failed`）。
    pub fn is_cancelled(&self) -> bool {
        matches!(self, Self::Cancelled { .. })
    }

    /// 出错的那一步（参数错误时是 `None`）。
    pub fn step_name(&self) -> Option<Step> {
        match self {
            Self::Invalid(_) => None,
            Self::Step { step, .. } | Self::Cancelled { step } => Some(*step),
        }
    }
}

impl fmt::Display for BuildError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(message) => write!(f, "参数不合法：{message}"),
            Self::Step { step, message } => write!(f, "{}：{message}", step.title()),
            Self::Cancelled { step } => write!(f, "{}：已取消", step.title()),
        }
    }
}

impl std::error::Error for BuildError {}
