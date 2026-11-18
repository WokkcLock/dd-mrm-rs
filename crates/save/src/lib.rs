//! 领域服务：找档（estate 发现）、mod 目录扫描、离线应用（SyncMod）、
//! 原子写、备份与清理、三根/预设持久化 —— 依赖 dd-mrm-dson。
//!
//! # 安全边界（GC3/GC4）
//!
//! - 本 crate 唯一可写的存档目标是 estate 目录内的 `persist.game.json`：
//!   一切写入口径都过 [`validate_persist_path`] 白名单；
//! - 写入一律走 `.tmp` + rename 原子替换（[`write_persist_atomic`]）；
//! - 三根目录（save root / 本地 mods / 工坊）常规来源是调用方传入；
//!   另提供只读的自动探测 [`detect_roots`]（注册表 + 文件系统，探测失败
//!   只返回提示、绝不落盘），手动配置始终兜底。也从不写调用方目录之外
//!   的地方（备份目录、配置文件路径同样由调用方指定）。
//!
//! # 自有 JSON 文件格式（重写后的新格式，与旧 C# 不兼容）
//!
//! 两个小 JSON，定义见 [`paths`]（`SaveRoots` / `Presets`）：
//!
//! **三根配置**（路径由调用方决定，如 `<配置目录>/roots.json`）：
//!
//! ```json
//! {
//!   "save_root": "C:/Saves",
//!   "local_mods_dir": "C:/Mods",
//!   "workshop_dir": "C:/steamapps/workshop/content/262060"
//! }
//! ```
//!
//! **预设列表**（如 `<配置目录>/presets.json`）：
//!
//! ```json
//! {
//!   "presets": {
//!     "我的搭配": [
//!       { "name": "Mod 标题", "source": "local" },
//!       { "name": "1234567890", "source": "steam" }
//!     ]
//!   }
//! }
//! ```
//!
//! 均为“文件缺失 = 未配置（None），文件损坏 = 报错”语义，绝不静默重置。
//!
//! # 读侧 API（只读，不触发任何写盘）
//!
//! - [`EstateFile::open`] 后：`estate_name()` / `game_mode()` / `applied_mods()`
//!   / `dlc_entries()`（DLC 权属列表，见 [`DlcEntry`]——文件里没有 enabled
//!   布尔，“是否启用”= 条目在不在列表，由 UI 推导）。
//!
//! # 典型调用流（T4/T5 UI 视角）
//!
//! ```no_run
//! use std::path::Path;
//! use save::{AppliedMod, ApplyOptions, apply_offline, discover_estates, scan_local_and_workshop};
//!
//! # fn main() -> save::Result<()> {
//! // 1) 找档：save root 下含 persist.game.json 的子目录
//! let estates = discover_estates(Path::new("C:/Saves"))?;
//! // 2) 扫 mod：本地 + 工坊
//! let report = scan_local_and_workshop(Path::new("C:/Mods"), Path::new("C:/workshop/content/262060"));
//! // 3) 离线应用（可选先备份）
//! let outcome = apply_offline(
//!     &estates[0].persist_path,
//!     &[AppliedMod::steam("1234567890")],
//!     &ApplyOptions { backup_dir: Some("C:/Saves/SaveBak".into()) },
//! )?;
//! # let _ = (report, outcome);
//! # Ok(())
//! # }
//! ```

pub mod apply;
pub mod backup;
pub mod detect;
pub mod error;
pub mod mod_model;
pub mod paths;
pub mod scan;

pub use apply::{
    APPLIED_FIELD_NAME, ApplyOptions, ApplyOutcome, DLC_FIELD_NAME, EstateFile, apply_offline,
    write_persist_atomic,
};
pub use backup::{backup_estate_dir, clean_old_backups};
pub use detect::{Detected, detect_roots};
pub use error::{Result, SaveError};
pub use mod_model::{AppliedMod, DlcEntry, ModInfo, ModSource};
pub use paths::{
    Estate, PERSIST_FILE_NAME, Presets, SaveRoots, discover_estates, validate_persist_path,
};
pub use scan::{ScanReport, scan_local_and_workshop, scan_mod_dir};

/// Crate 名，供下游在运行时证明依赖边真实存在（M0 占位公共项）。取自包名，改名自动跟随。
pub const NAME: &str = env!("CARGO_PKG_NAME");

/// 底层编解码 crate 名——引用 dson 常量，使 save→dson 依赖边真实生效
/// （M0 占位公共项；与 ui-main 引用 save/hotapply 常量同一手法）。
pub const CODEC: &str = dson::NAME;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crate_identity() {
        assert_eq!(NAME, env!("CARGO_PKG_NAME"));
        assert_eq!(CODEC, dson::NAME);
    }
}
