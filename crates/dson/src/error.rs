//! 错误类型 —— 全库唯一错误通道（std only，手写 Display）。
//!
//! 保真优先：所有「结构不支持 / 校验失败」都以携带上下文的 Err 结束，
//! 绝不 panic、绝不静默跳过。消息包含定位信息（第几个条目、字段名、
//! 实际值），便于调用方与人工定位。

use std::fmt;

/// DSON 编解码错误。消息自带上下文（条目序号、字段名、实际值）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DsonError {
    msg: String,
}

impl DsonError {
    /// 用完整消息构造。
    pub fn new(msg: impl Into<String>) -> Self {
        DsonError { msg: msg.into() }
    }

    /// 错误消息全文。
    pub fn message(&self) -> &str {
        &self.msg
    }
}

impl fmt::Display for DsonError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.msg)
    }
}

impl std::error::Error for DsonError {}

/// 库统一 Result 别名。
pub type Result<T> = std::result::Result<T, DsonError>;
