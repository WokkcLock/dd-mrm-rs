//! Steam 三根自动探测（设置面板「自动探测」按钮的后端）。
//!
//! 探测链路（通用形态，不假设库数量/盘符/账号数）：
//!
//! ```text
//! 注册表 SteamPath（HKCU → HKLM WOW64 → HKLM）→ Steam 主目录 steam
//!   ├─ steam\userdata\<UID>\262060\remote          → 存档根（多账号按 UID 升序取第一）
//!   └─ steam\steamapps\libraryfolders.vdf            → 库列表（缺失则仅主目录）
//!        ├─ <库>\steamapps\common\DarkestDungeon    → 游戏目录 → <游戏>\mods
//!        └─ <库>\steamapps\workshop\content\262060 → 工坊根（缺失按游戏库预测）
//! ```
//!
//! 设计约束：
//! - 永不失败：探测不到的项保持 `None` 并附人读提示（`Detected::notes`），
//!   由 UI 展示；手动配置流程原样兜底（自动探测是增强，不是必经路径）。
//! - 纯文件系统核心 `detect_from` 以 Steam 主目录为参数，可用假目录树测试，
//!   不依赖本机装没装 Steam；注册表读取是唯一的平台边（`detect_roots`）。
//! - 本模块只读：不写任何文件、不写注册表。

use std::fs;
use std::path::{Path, PathBuf};

/// Darkest Dungeon 的 Steam AppId（userdata 与工坊 content 下的目录名）。
pub const DD_APP_ID: &str = "262060";
/// Steam 库内游戏目录名（`steamapps\common\` 下）。
const DD_GAME_DIR: &str = "DarkestDungeon";

/// 探测结果：三项路径 + 存档候选 + 人读提示（模块文档见首）。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Detected {
    /// 存档根（`userdata\<UID>\262060\remote`）；多账号时 = `save_root_candidates[0]`。
    pub save_root: Option<PathBuf>,
    /// 所有命中账号的存档根，UID 升序（单账号时长度 1）。
    pub save_root_candidates: Vec<PathBuf>,
    /// 本地 mods 目录（游戏目录下的 `mods`）。
    pub local_mods_dir: Option<PathBuf>,
    /// 工坊 content 目录（`<库>\steamapps\workshop\content\262060`）。
    pub workshop_dir: Option<PathBuf>,
    /// 人读提示（中文），UI 原样展示；空 = 全部一次命中、无歧义。
    pub notes: Vec<String>,
}

/// 自动探测入口：注册表定位 Steam 主目录 → 纯文件系统探测 `detect_from`。
/// 注册表读不到（未装 Steam / 权限）→ 空结果 + 提示，绝不报错。
pub fn detect_roots() -> Detected {
    match steam_install_dir() {
        Some(steam) => detect_from(&steam),
        None => Detected {
            notes: vec![
                "注册表中未找到 Steam 安装路径（Valve Steam 键），无法自动探测；请手动填写。"
                    .into(),
            ],
            ..Detected::default()
        },
    }
}

/// Windows 注册表里的 Steam 主目录（HKCU 当前用户 → HKLM 32 位视图 → HKLM 原生）。
#[cfg(windows)]
fn steam_install_dir() -> Option<PathBuf> {
    let candidates = [
        windows_registry::CURRENT_USER
            .open("Software\\Valve\\Steam")
            .and_then(|k| k.get_string("SteamPath")),
        windows_registry::LOCAL_MACHINE
            .open("SOFTWARE\\WOW6432Node\\Valve\\Steam")
            .and_then(|k| k.get_string("SteamPath")),
        windows_registry::LOCAL_MACHINE
            .open("SOFTWARE\\Valve\\Steam")
            .and_then(|k| k.get_string("SteamPath")),
    ];
    candidates
        .into_iter()
        .filter_map(|v| v.ok())
        .map(PathBuf::from)
        .find(|p| p.is_dir())
}

/// 非 Windows 编译：恒空（本工具 Windows-only；手动配置仍可用）。
#[cfg(not(windows))]
fn steam_install_dir() -> Option<PathBuf> {
    None
}

/// 纯文件系统探测：以 Steam 主目录为根，走 userdata + 各库。
pub fn detect_from(steam: &Path) -> Detected {
    let mut d = Detected::default();
    if !steam.is_dir() {
        d.notes.push(format!(
            "Steam 目录不存在（{}），无法自动探测。",
            steam.display()
        ));
        return d;
    }
    detect_save_root(steam, &mut d);
    let libs = libraries(steam, &mut d);
    detect_game_and_workshop(&libs, &mut d);
    d
}

/// 存档根：userdata 下所有「有 262060 目录」的账号，按 UID 升序取第一
/// （profile_0,1,2… 式自然序，不猜最近使用）。
/// Steam 的 userdata 只在主安装目录（不在各库），这正是手动配置最易错的一点。
fn detect_save_root(steam: &Path, d: &mut Detected) {
    let userdata = steam.join("userdata");
    let mut accounts: Vec<(u64, PathBuf)> = Vec::new();
    match fs::read_dir(&userdata) {
        Ok(entries) => {
            for entry in entries.flatten() {
                let uid = entry.file_name().to_string_lossy().into_owned();
                if uid.is_empty() || !uid.chars().all(|c| c.is_ascii_digit()) {
                    continue; // 非账号目录（杂项/遗留），跳过
                }
                let app_dir = userdata.join(&uid).join(DD_APP_ID);
                if !app_dir.is_dir() {
                    continue;
                }
                accounts.push((uid.parse().unwrap_or(u64::MAX), app_dir.join("remote")));
            }
        }
        Err(_) => {
            d.notes.push(
                "未找到/不可读 Steam userdata 目录（可能从未在此机登录过 Steam）；存档根请手动填写。"
                    .into(),
            );
            return;
        }
    }
    if accounts.is_empty() {
        d.notes.push(
            "userdata 中未见 Darkest Dungeon（appid 262060）云存档——游戏可能未在这台机器运行过；存档根请手动填写。"
                .into(),
        );
        return;
    }
    // UID 升序（确定性，多账号提示文案依赖稳定顺序）。
    accounts.sort_by_key(|(uid, _)| *uid);
    if accounts.len() > 1 {
        d.notes.push(format!(
            "检测到多个 Steam 账号，已取 UID 最小的一个（UID {}）",
            accounts[0].0
        ));
    }
    d.save_root_candidates = accounts.iter().map(|(_, p)| p.clone()).collect();
    d.save_root = d.save_root_candidates.first().cloned();
}

/// 库列表：libraryfolders.vdf 的所有 "path"；vdf 缺失/为空时回退仅主目录。
fn libraries(steam: &Path, d: &mut Detected) -> Vec<PathBuf> {
    let vdf = steam.join("steamapps").join("libraryfolders.vdf");
    let text = fs::read_to_string(&vdf).unwrap_or_default();
    let mut libs: Vec<PathBuf> = Vec::new();
    let mut seen: Vec<String> = Vec::new();
    for p in parse_vdf_library_paths(&text) {
        let key = p.to_string_lossy().to_lowercase().replace('/', "\\");
        if !seen.contains(&key) {
            seen.push(key);
            libs.push(p);
        }
    }
    if libs.is_empty() {
        d.notes.push(
            "未读取到 libraryfolders.vdf（或其中没有库），仅以 Steam 主目录作为唯一库探测。"
                .into(),
        );
        libs.push(steam.to_path_buf());
    }
    libs
}

/// 游戏与工坊：首个命中游戏目录的库定本地 mods；工坊优先取实际存在 content 的库，
/// 全都不存在时按游戏所在库预测（订阅工坊内容后即落在此处）。
fn detect_game_and_workshop(libs: &[PathBuf], d: &mut Detected) {
    let game_lib = libs.iter().position(|lib| {
        let g = lib.join("steamapps").join("common").join(DD_GAME_DIR);
        // 身份标记 = _windows 子目录（DD 的 exe 在 _windows\win64）；
        // mods 可能被用户删掉，不能当标记。
        g.join("_windows").is_dir() || g.join("mods").is_dir()
    });
    match game_lib {
        Some(i) => {
            let mods = libs[i]
                .join("steamapps")
                .join("common")
                .join(DD_GAME_DIR)
                .join("mods");
            if !mods.is_dir() {
                d.notes.push(
                    "已定位游戏目录，但其下尚无 mods 子目录；本地 mod 路径按预测位置填入。"
                        .into(),
                );
            }
            d.local_mods_dir = Some(mods);
        }
        None => d.notes.push(
            "各 Steam 库中未找到 DarkestDungeon 游戏目录；本地 mod 路径请手动填写。".into(),
        ),
    }

    let mut found: Option<PathBuf> = None;
    for lib in libs {
        let w = workshop_content(lib);
        if w.is_dir() {
            found = Some(w);
            break;
        }
    }
    match found {
        Some(w) => d.workshop_dir = Some(w),
        None => match game_lib {
            Some(i) => {
                d.notes.push(
                    "尚无创意工坊 content\\262060 目录（从未订阅过 DD 模组？）；已按游戏所在库填入预测路径。"
                        .into(),
                );
                d.workshop_dir = Some(workshop_content(&libs[i]));
            }
            None => d.notes.push(
                "未找到创意工坊 content\\262060 目录；Steam mod 路径请手动填写。".into(),
            ),
        },
    }
}

fn workshop_content(lib: &Path) -> PathBuf {
    lib.join("steamapps")
        .join("workshop")
        .join("content")
        .join(DD_APP_ID)
}


/// 极简 VDF 词法：抽取所有成对带引号的字符串 token；key == "path" 时
/// 下一个 token 即库路径。库 vdf 结构简单且无同名歧义，不值得引依赖。
fn parse_vdf_library_paths(text: &str) -> Vec<PathBuf> {
    let tokens = vdf_quoted_tokens(text);
    let mut out = Vec::new();
    let mut i = 0;
    while i + 1 < tokens.len() {
        if tokens[i] == "path" {
            out.push(PathBuf::from(&tokens[i + 1]));
            i += 2;
        } else {
            i += 1;
        }
    }
    out
}

/// 扫出全部 `"…"` 字符串（处理 \\ \" \n \t \r 转义；未闭合按结束处理）。
fn vdf_quoted_tokens(text: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut chars = text.chars();
    while let Some(c) = chars.next() {
        if c != '"' {
            continue;
        }
        let mut s = String::new();
        loop {
            match chars.next() {
                Some('"') => break,
                Some('\\') => match chars.next() {
                    Some('n') => s.push('\n'),
                    Some('t') => s.push('\t'),
                    Some('r') => s.push('\r'),
                    Some(e @ ('"' | '\\')) => s.push(e),
                    Some(other) => {
                        s.push('\\');
                        s.push(other);
                    }
                    None => break,
                },
                Some(other) => s.push(other),
                None => break,
            }
        }
        tokens.push(s);
    }
    tokens
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vdf_tokens_unescape() {
        // 原始串里 \\ 是字面两字符 → 解析器还原为单反斜杠。
        let toks = vdf_quoted_tokens(r#""a\\b" "c\"d" "e\nf" "#);
        assert_eq!(toks.len(), 3);
        assert_eq!(toks[0], "a\\b");
        assert_eq!(toks[1], "c\"d");
        assert_eq!(toks[2], "e\nf");
    }

    #[test]
    fn vdf_paths_only() {
        let text = r#""libraryfolders"
{
    "0"
    {
        "path"        "C:\\Program Files (x86)\\Steam"
        "label"       ""
        "contentid"   "123"
        "totalsize"   "0"
        "update_kb"   "-1"
    }
    "1"
    {
        "path"        "E:\\Steam"
    }
}"#;
        let libs = parse_vdf_library_paths(text);
        assert_eq!(
            libs,
            [
                PathBuf::from("C:\\Program Files (x86)\\Steam"),
                PathBuf::from("E:\\Steam")
            ]
        );
    }

    #[test]
    fn vdf_empty() {
        assert!(parse_vdf_library_paths("").is_empty());
        assert!(parse_vdf_library_paths(r#""foo" "bar""#).is_empty());
    }
}
