//! hotapply —— B 路线（DLL v14）：注入式 size 守卫。
//!
//! 方案定案（E1c/E1d/E1e 三轮实验）：写盘交给 UI（save crate 的 DSON
//! 写入器），游戏进程内只挂一个 fs_get_file_info 钩 —— 在 size 查询
//! 返回处把陈旧的缓存 size 对齐磁盘真值，防 "Buffer isn't large enough
//! for string!" 断言。管道双向通信（v1-v13 的 named pipe 引擎）已整体
//! 退役：DLL 注入即自活，UI 与 DLL 零通信。

pub mod dll;
pub mod hook;
pub mod injector;
pub mod locate;
mod win;

pub use hook::{HookError, ensure_hooked, game_running};
pub use injector::{InjectFailure, InjectOptions, ProcessTarget};
pub use locate::{Anchors, LocateError, FS_PATTERN};
