//! 领域数据模型：mod 来源、扫描结果、applied mod 条目、DLC 条目。
//!
//! 共享数据边界（GC1）：本 crate 侧自持定义，不依赖 hotapply；
//! `ModEntry{title,version}`（192B 管道条目）属 hotapply 边界，与此处的
//! `AppliedMod`（DSON `applied_ugcs_1_0` 条目）是两种表示，互不引用。

use serde::{Deserialize, Serialize};

/// mod 来源。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ModSource {
    /// 本地 mods 目录。
    Local,
    /// Steam 创意工坊。
    Steam,
}

impl ModSource {
    /// DSON `applied_ugcs_1_0/*/source` 字段的本地来源字面量。
    pub const LOCAL_DSON: &'static str = "mod_local_source";
    /// DSON `applied_ugcs_1_0/*/source` 字段的工坊来源字面量。
    pub const STEAM_DSON: &'static str = "Steam";

    /// 写进 DSON 的来源字符串。
    pub fn as_dson_str(self) -> &'static str {
        match self {
            ModSource::Local => ModSource::LOCAL_DSON,
            ModSource::Steam => ModSource::STEAM_DSON,
        }
    }

    /// 从 DSON `source` 字符串还原；未知值 → `None`（调用方按错误处理，不静默猜测）。
    pub fn from_dson_str(s: &str) -> Option<Self> {
        match s {
            ModSource::LOCAL_DSON => Some(ModSource::Local),
            ModSource::STEAM_DSON => Some(ModSource::Steam),
            _ => None,
        }
    }
}

/// mod 目录扫描结果（旧 C# `ModScanner`/`BaseScanItem` 的移植）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModInfo {
    /// mods 根下的子目录名（工坊 mod 即 workshop id）。
    pub dir_name: String,
    /// 显示名：`project.xml`/`workshop.vdf` 中解析并 HTML 解码后的标题；
    /// 解析失败时本地为 `""`、工坊为 `"[Mod id]: {dir_name}"`。
    pub title: String,
    /// 来源。
    pub source: ModSource,
    /// 目录内 `preview_icon.png` 的完整路径（UI 缩略图用；无则 None）。
    pub icon_path: Option<std::path::PathBuf>,
}

/// 一条将要写入 `applied_ugcs_1_0` 的 applied mod（旧 C# `ModItem{name,source}`）。
///
/// - 本地 mod：`name` = mod 标题/目录名（调用方决定口径）；
/// - 工坊 mod：`name` = workshop id 十进制字符串。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppliedMod {
    /// mod 名（写入 DSON `name` 字段）。
    pub name: String,
    /// 来源（写入 DSON `source` 字段，序列化为 "mod_local_source"/"Steam"）。
    pub source: ModSource,
}

impl AppliedMod {
    /// 构造本地 applied mod。
    pub fn local(name: impl Into<String>) -> Self {
        AppliedMod {
            name: name.into(),
            source: ModSource::Local,
        }
    }

    /// 构造工坊 applied mod。
    pub fn steam(name: impl Into<String>) -> Self {
        AppliedMod {
            name: name.into(),
            source: ModSource::Steam,
        }
    }
}

/// 一条 `base_root.dlc` 容器条目（旧 C# `DlcItem{name,source}`）。
///
/// 金样本实证（profile_2/7 均同）：`dlc` 是按 "0".."N-1" 键排列的
/// **权属列表**——每条是恰好 `name`+`source` 两个字符串字段的对象，
/// 观测到的 `source` 恒为 `"dlc"`；文件里**没有** enabled 布尔
/// （"DLC 是否启用"在存档中表现为条目在不在列表里，由 UI 层推导）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DlcEntry {
    /// DLC 标识（如 `shieldbreaker`/`crimson_court`/`color_of_madness`…）。
    pub name: String,
    /// 来源字面量（观测恒为 `"dlc"`；按文件原样返回，不校验枚举）。
    pub source: String,
}
