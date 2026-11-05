//! 存档根发现、GC3 白名单校验、两个自有 JSON 配置（三根 + 预设）。

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::apply::EstateFile;
use crate::error::{Result, SaveError};

/// 游戏存档文件名（estate 目录内唯一的可写文件，GC3 白名单核心）。
pub const PERSIST_FILE_NAME: &str = "persist.game.json";

/// 一个存档 estate：save root 下包含 `persist.game.json` 的子目录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Estate {
    /// profile 名 = estate 目录名。
    pub profile: String,
    /// estate 目录绝对/原样路径。
    pub dir: PathBuf,
    /// `dir` 内 persist 文件路径。
    pub persist_path: PathBuf,
}

impl Estate {
    /// 打开并解析该 estate 的存档文件。
    pub fn load(&self) -> Result<EstateFile> {
        EstateFile::open(&self.persist_path)
    }
}

/// 扫描 save root：每个直接子目录内存在 `persist.game.json` 即为 estate。
///
/// save root 不存在 → `Ok(vec![])`（UI 首启/用户未配置时的正常态）。
/// 不可读 → `Err`。单个子目录枚举失败按旧 `SaveScanner` 的吞异常语义跳过
/// （旧工具无 failedDirs 报告，这里保持一致；需要诊断的调用方可自行复核）。
pub fn discover_estates(save_root: &Path) -> Result<Vec<Estate>> {
    let entries = match fs::read_dir(save_root) {
        Ok(entries) => entries,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(SaveError::io(save_root, e)),
    };

    let mut estates: Vec<Estate> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let is_dir = entry.file_type().is_ok_and(|t| t.is_dir());
        if !is_dir {
            continue;
        }
        let persist = path.join(PERSIST_FILE_NAME);
        if persist.is_file() {
            let profile = entry.file_name().to_string_lossy().into_owned();
            estates.push(Estate {
                profile,
                dir: path,
                persist_path: persist,
            });
        }
    }
    estates.sort_by(|a, b| a.profile.cmp(&b.profile));
    Ok(estates)
}

/// GC3 白名单：只允许写文件名**恰好**为 `persist.game.json` 的路径。
///
/// 拒绝：其它文件名、无文件名（目录/根）、通过 `..` 逃逸等（由文件名不匹配覆盖）。
pub fn validate_persist_path(path: &Path) -> Result<()> {
    let ok = path
        .file_name()
        .is_some_and(|n| n.eq_ignore_ascii_case(PERSIST_FILE_NAME));
    if ok {
        Ok(())
    } else {
        Err(SaveError::Whitelist {
            path: path.to_path_buf(),
            reason: format!("只有 {PERSIST_FILE_NAME} 是可写的存档文件"),
        })
    }
}

/// 三根配置（save root / 本地 mods / 工坊 content）。
///
/// JSON 格式（新定义，与旧 C# CustomData/Preset.json 不兼容——这是重写）：
///
/// ```json
/// {
///   "save_root": "C:/Saves",
///   "local_mods_dir": "C:/Mods",
///   "workshop_dir": "C:/steamapps/workshop/content/262060"
/// }
/// ```
///
/// 三个字段都是字符串路径（`null` = 未配置；反序列化容忍缺失字段）。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SaveRoots {
    /// 存档根目录。
    #[serde(default)]
    pub save_root: Option<PathBuf>,
    /// 本地 mods 根目录。
    #[serde(default)]
    pub local_mods_dir: Option<PathBuf>,
    /// 工坊 content 目录。
    #[serde(default)]
    pub workshop_dir: Option<PathBuf>,
}

impl SaveRoots {
    /// 从 JSON 文件读取；文件不存在 → `Ok(None)`，存在但损坏 → `Err`（不静默重置）。
    pub fn load(path: &Path) -> Result<Option<Self>> {
        read_json_or_none(path)
    }

    /// 原子写回（.tmp + rename，见 [`crate::apply::write_persist_atomic`] 同款实现）。
    pub fn save(&self, path: &Path) -> Result<()> {
        let text = serde_json::to_string_pretty(self)
            .map_err(|e| SaveError::config(path, format!("序列化失败: {e}")))?;
        crate::apply::atomic_write_raw(path, text.as_bytes())
    }
}

/// 预设 mod 列表集合（名字 → applied mod 列表）。
///
/// JSON 格式（新定义，与旧 C# PresetModList.json 的 `source:int` 不兼容）：
///
/// ```json
/// {
///   "presets": {
///     "我的搭配": [
///       { "name": "Mod 标题", "source": "local" },
///       { "name": "1234567890", "source": "steam" }
///     ]
///   }
/// }
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Presets {
    /// 预设名 → 列表（serde 用 BTreeMap，输出按名排序，diff 友好）。
    #[serde(default)]
    pub presets: std::collections::BTreeMap<String, Vec<crate::mod_model::AppliedMod>>,
}

impl Presets {
    /// 从 JSON 文件读取；文件不存在 → `Ok(None)`；损坏 → `Err`（不静默重置）。
    pub fn load(path: &Path) -> Result<Option<Self>> {
        read_json_or_none(path)
    }

    /// 原子写回。
    pub fn save(&self, path: &Path) -> Result<()> {
        let text = serde_json::to_string_pretty(self)
            .map_err(|e| SaveError::config(path, format!("序列化失败: {e}")))?;
        crate::apply::atomic_write_raw(path, text.as_bytes())
    }
}

/// 读取一个“可缺省”的 JSON 配置：缺失 → None，损坏 → Err。
fn read_json_or_none<T: serde::de::DeserializeOwned>(path: &Path) -> Result<Option<T>> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(SaveError::io(path, e)),
    };
    serde_json::from_slice(&bytes)
        .map(Some)
        .map_err(|e| SaveError::config(path, format!("JSON 解析失败: {e}")))
}
