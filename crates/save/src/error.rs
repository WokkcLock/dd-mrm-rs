//! 错误类型：全量检查错误，任何 malformed 输入走 `Err`，绝不 panic。

use std::fmt;
use std::io;
use std::path::PathBuf;

use dson::DsonError;

/// 本 crate 统一 Result。
pub type Result<T> = std::result::Result<T, SaveError>;

/// 领域服务错误。
#[derive(Debug)]
pub enum SaveError {
    /// 文件系统 I/O 失败（带触发路径与底层错误）。
    Io { path: PathBuf, source: io::Error },
    /// DSON 编解码失败（解析拒绝未知结构 / 写出约束）。
    Dson(DsonError),
    /// 白名单拒绝（GC3）：目标不是 `persist.game.json`，或写入口径外。
    Whitelist { path: PathBuf, reason: String },
    /// persist.game.json 结构不符合领域预期（applied_ugcs_1_0 形状等）。
    Shape(String),
    /// 配置/预设 JSON 读写或解析失败。
    Config { path: PathBuf, reason: String },
    /// 备份（zip）失败。
    Backup { path: PathBuf, reason: String },
}

impl SaveError {
    /// 构造带路径的 I/O 错误。
    pub fn io(path: impl Into<PathBuf>, source: io::Error) -> Self {
        SaveError::Io {
            path: path.into(),
            source,
        }
    }

    /// 构造配置错误。
    pub fn config(path: impl Into<PathBuf>, reason: impl Into<String>) -> Self {
        SaveError::Config {
            path: path.into(),
            reason: reason.into(),
        }
    }
}

impl fmt::Display for SaveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SaveError::Io { path, source } => {
                write!(f, "io error at `{}`: {}", path.display(), source)
            }
            SaveError::Dson(e) => write!(f, "dson: {e}"),
            SaveError::Whitelist { path, reason } => {
                write!(
                    f,
                    "whitelist violation at `{}`: {} (GC3: 只允许 persist.game.json)",
                    path.display(),
                    reason
                )
            }
            SaveError::Shape(reason) => write!(f, "unexpected persist.game.json shape: {reason}"),
            SaveError::Config { path, reason } => {
                write!(f, "config file `{}`: {}", path.display(), reason)
            }
            SaveError::Backup { path, reason } => {
                write!(f, "backup `{}`: {}", path.display(), reason)
            }
        }
    }
}

impl std::error::Error for SaveError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            SaveError::Io { source, .. } => Some(source),
            SaveError::Dson(e) => Some(e),
            _ => None,
        }
    }
}

impl From<DsonError> for SaveError {
    fn from(e: DsonError) -> Self {
        SaveError::Dson(e)
    }
}
