//! hotapply —— DdHotapply 引擎（native/DdHotapply.c，GC2 保持 C 不重写）
//! 的注入器 + 管道客户端 + 门面。协议权威 = DLL 实现，不是设计文档。
//!
//! 模块布局（全部 windows-only）：
//! - [`dll`]：DLL 定位（env → exe 目录 → eng/native → 仓库开发路径）
//! - [`injector`]：CreateRemoteThread 注入（旧 C# NativeInjector 语义）
//! - [`pipe`]：`\\.\pipe\DdModToolHotapply` 行协议客户端
//! - [`client`]：**T5 只消费这个门面**（`HotapplyClient::apply`）+ 重导出类型
//! - [`verify`]：notepad 冒烟（= 旧 smoke.ps1）
//!
//! 本 crate 不含任何游戏域知识（GC/裁定 2）：profile 名与 mod 条目列表都是数据；
//! 容器名/偏移只在 DLL 里。状态串原样透传。

/// Crate 名，供下游在运行时证明依赖边真实存在（M0 占位公共项）。取自包名，改名自动跟随。
pub const NAME: &str = env!("CARGO_PKG_NAME");

#[cfg(windows)]
mod win;

#[cfg(windows)]
pub mod client;
#[cfg(windows)]
pub mod dll;
#[cfg(windows)]
pub mod injector;
#[cfg(windows)]
pub mod pipe;
#[cfg(windows)]
pub mod verify;

// 门面重导出：T5 只 `use hotapply::{HotapplyClient, ApplyOutcome, ...}`
#[cfg(windows)]
pub use client::{ApplyOutcome, ClientConfig, DEFAULT_GAME_PROCESS, HotapplyClient};
#[cfg(windows)]
pub use injector::{InjectFailure, InjectOptions, ProcessTarget};
#[cfg(windows)]
pub use pipe::{EncodeError, ModEntry, PipeError, Reply, Request};

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crate_identity() {
        assert_eq!(NAME, env!("CARGO_PKG_NAME"));
    }
}
