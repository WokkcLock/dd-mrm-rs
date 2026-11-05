//! UI 无关的核心模型：条目映射（ModInfo ↔ ModEntry ↔ AppliedMod）、脏状态跟踪、
//! 热应用状态串分类、配置 dd-mrm.json 读写、显示格式化（中文文案、净化）。
//!
//! 全部为纯函数 / 纯数据结构，可脱离 slint 事件循环做单元测试（任务 5 交付项 5）。
//!
//! 关键语义（与旧 C# 工具逐字节对齐，见任务简报 ruling 4）：
//!
//! - 管道 `ModEntry { title, version }` = `(mod.Name, mod.Source)`：
//!   title = applied 名，version = DSON source 字面量（`mod_local_source` / `Steam`），
//!   对应旧 IndexViewModel.cs:523-525 的元组构造；
//! - applied 名映射（IndexViewModel.cs:505-513）：本地 mod → title，创意工坊 → dir_name。

use std::path::{Path, PathBuf};

use hotapply::ModEntry;
use save::{AppliedMod, DlcEntry, ModInfo, ModSource, SaveRoots};

/// dd-mrm.json 里预设文件的默认路径（exe 同目录 presets.json）。
pub fn default_presets_path() -> PathBuf {
    exe_dir().join("presets.json")
}

/// exe 所在目录（日志、备份、配置、预设全部落在这里，repo logs/ 同款约定）。
pub fn exe_dir() -> PathBuf {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf))
        .unwrap_or_else(|| PathBuf::from("."))
}

/// 应用配置：三根 + 预设文件路径，单一 `dd-mrm.json`，原子写。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct AppConfig {
    /// 三根（save root / 本地 mods / 工坊 content），全可空。
    #[serde(flatten)]
    pub roots: SaveRoots,
    /// 预设持久化文件路径。
    #[serde(default = "default_presets_path")]
    pub presets_path: PathBuf,
    /// 是否启用热修改（游戏运行中注入）。关闭 = 只写存档文件。默认开。
    #[serde(default = "default_hotapply")]
    pub hotapply: bool,
}

fn default_hotapply() -> bool {
    true
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            roots: SaveRoots::default(),
            presets_path: default_presets_path(),
            hotapply: true,
        }
    }
}

impl AppConfig {
    /// 读 dd-mrm.json：文件缺失 → `Ok(None)`（首跑），损坏 → `Err(中文缘由)`（绝不静默重置）。
    pub fn load(path: &Path) -> Result<Option<Self>, String> {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("读取 {} 失败：{e}", path.display())),
        };
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| format!("{} 解析失败：{e}", path.display()))
    }

    /// 原子写 dd-mrm.json（同目录 `.tmp` + rename；同 dd-mrm-save 的写盘纪律 GC4）。
    pub fn save(&self, path: &Path) -> Result<(), String> {
        let text = serde_json::to_string_pretty(self)
            .map_err(|e| format!("序列化 {} 失败：{e}", path.display()))?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, text.as_bytes())
            .map_err(|e| format!("写入 {} 失败：{e}", tmp.display()))?;
        // Windows 上 rename 到已存在目标需要先移走旧文件：rename→remove→rename。
        let old = path.with_extension("json.old");
        if path.exists() {
            let _ = std::fs::rename(path, &old);
        }
        std::fs::rename(&tmp, path).map_err(|e| {
            // 回滚：把旧文件放回去，避免半写状态。
            if old.exists() {
                let _ = std::fs::rename(&old, path);
            }
            format!("替换 {} 失败：{e}", path.display())
        })?;
        let _ = std::fs::remove_file(&old);
        Ok(())
    }
}

/// 已应用列表的一行（UI 编辑态）：name 按旧工具口径（本地=title / 工坊=dir_name），
/// `present` 标记该 mod 是否仍在本仓库扫描结果里（仅显示 ⚠ 用，不参与脏比较）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AppliedRow {
    pub name: String,
    pub source: ModSource,
    pub present: bool,
}

impl AppliedRow {
    pub fn from_applied(a: &AppliedMod) -> Self {
        Self {
            name: a.name.clone(),
            source: a.source,
            present: true,
        }
    }

    /// 与文件里读出的 applied 名单比对（present 不参与）。
    pub fn to_applied(&self) -> AppliedMod {
        AppliedMod {
            name: self.name.clone(),
            source: self.source,
        }
    }
}

/// ModInfo → AppliedMod：旧工具口径（IndexViewModel.cs:505-513）。
/// 本地 mod 用 title（可能为空串=未命名），工坊 mod 用 dir_name（数字 id）。
pub fn applied_from_mod(m: &ModInfo) -> AppliedMod {
    match m.source {
        ModSource::Local => AppliedMod::local(m.title.clone()),
        ModSource::Steam => AppliedMod::steam(m.dir_name.clone()),
    }
}

/// AppliedMod → 管道 ModEntry：旧工具逐字节口径（IndexViewModel.cs:523-525）。
/// title = mod 名，version = DSON source 字面量（`mod_local_source` / `Steam`）。
pub fn pipe_entry(a: &AppliedMod) -> ModEntry {
    ModEntry {
        title: a.name.clone(),
        version: a.source.as_dson_str().to_string(),
    }
}

/// 脏判定：当前编辑列表 vs 存档文件基线（顺序敏感 —— 顺序即应用顺序，FR3）。
pub fn is_dirty(baseline: &[AppliedMod], current: &[AppliedRow]) -> bool {
    if baseline.len() != current.len() {
        return true;
    }
    baseline
        .iter()
        .zip(current.iter())
        .any(|(a, r)| a.name != r.name || a.source != r.source)
}

/// 给已应用行标注 present：本地按 title 匹配、工坊按 dir_name 匹配（名字口径一致）。
pub fn mark_present(rows: &mut [AppliedRow], installed: &[ModInfo]) {
    for r in rows.iter_mut() {
        r.present = installed.iter().any(|m| match (r.source, m.source) {
            (ModSource::Local, ModSource::Local) => m.title == r.name,
            (ModSource::Steam, ModSource::Steam) => m.dir_name == r.name,
            _ => false,
        });
    }
}

/// 已安装列表过滤（大小写不敏感子串，命中 title 或 dir_name），返回命中下标。
/// `applied` 中的 mod 从结果排除（旧工具观感：已加入存档列表的不再出现在可用列表，
/// IndexViewModel 是 MOVE 语义 + 集合视图过滤；这里用视图过滤等价实现，目录数据不动）。
pub fn filter_indices(mods: &[ModInfo], query: &str, applied: &[AppliedRow]) -> Vec<usize> {
    let q = query.trim().to_lowercase();
    let in_applied = |m: &ModInfo| -> bool {
        let a = applied_from_mod(m);
        applied
            .iter()
            .any(|r| r.name == a.name && r.source == a.source)
    };
    mods.iter()
        .enumerate()
        .filter(|(_, m)| !in_applied(m))
        .filter(|(_, m)| {
            q.is_empty()
                || m.title.to_lowercase().contains(&q)
                || m.dir_name.to_lowercase().contains(&q)
        })
        .map(|(i, _)| i)
        .collect()
}

/// 热应用结果分类（FR9 四态 + 旧 C# 行为对齐 IndexViewModel.cs:530-560）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HotapplyVerdict {
    /// ok=true：已在游戏内存中生效。
    Ok,
    /// 引擎报告 game-not-running：自动回退离线写盘（旧工具 CoverSave 同款）。
    OfflineFallback,
    /// not-at-list：游戏正在该存档内，需要用户退回存档列表。
    NotAtList,
    /// 其余失败（含 engine-off:* / timeout / bad-* 等），state 原样展示。
    Fail,
}

/// ok 驱动图标；state 串原样保留（简报要求 verbatim 展示）。
pub fn classify_hotapply(ok: bool, state: &str) -> HotapplyVerdict {
    if ok {
        HotapplyVerdict::Ok
    } else {
        match state {
            "game-not-running" => HotapplyVerdict::OfflineFallback,
            "not-at-list" => HotapplyVerdict::NotAtList,
            _ => HotapplyVerdict::Fail,
        }
    }
}

/// 热应用成功的状态栏文案（旧工具口径）。
pub fn hotapply_ok_text(state: &str) -> String {
    if state == "applied" {
        "✅已热应用。直接进入该存档即生效；再改请先退到存档列表。".to_string()
    } else {
        format!("✅已热应用（{state}）。")
    }
}

/// not-at-list 的警示文案（旧工具逐字口径）。
pub fn not_at_list_text() -> String {
    "⚠️游戏正在该存档内。请先退到存档列表再点应用，否则退出存档时会被覆盖。".to_string()
}

/// 其余热应用失败的文案（旧工具口径；state 已净化）。
pub fn hotapply_fail_text(state: &str) -> String {
    format!("⚠️热应用失败（{state}），未修改任何内容；可重试，或关闭游戏后应用。")
}

/// 离线回退成功的两种文案（游戏未运行 / 引擎未接入，均旧工具口径）。
pub fn offline_fallback_text(engine_state: Option<&str>) -> String {
    match engine_state {
        None => "✅已写入存档（游戏未运行）。".to_string(),
        Some(s) => format!("✅已写入存档（游戏未运行，引擎未接入：{s}）。"),
    }
}

/// 热修改关闭（设置开关）时的写盘成功文案。设计定稿 2026-11-05（比稿 2E：仅文案区分）。
pub fn file_only_ok_text() -> String {
    "✅已写入存档（未开启热修改，改动在游戏下次读档时生效）。".to_string()
}

/// 热修改关闭但游戏正在运行：写盘会在游戏退出时被覆盖，警告文案。
pub fn file_only_game_running_text() -> String {
    "⚠️已写入文件，但游戏正在运行——退出时游戏会覆盖本次修改，建议关闭游戏后再应用。".to_string()
}

/// 失败细节净化（旧 C# CleanState 口径）：控制字符 → `?`，超过 160 字符截断加 `…`。
pub fn sanitize_detail(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| {
            if (c as u32) < 0x20 || c as u32 == 0x7f {
                '?'
            } else {
                c
            }
        })
        .collect();
    if cleaned.chars().count() > 160 {
        let head: String = cleaned.chars().take(160).collect();
        format!("{head}…")
    } else {
        cleaned
    }
}

/// game_mode → 中文（旧 IndexViewModel.cs:85-91 口径；未知值原样展示）。
pub fn game_mode_cn(mode: Option<&str>) -> String {
    match mode {
        None => "—".to_string(),
        Some("bloodmoon") => "血月".to_string(),
        Some("radiant") => "明耀".to_string(),
        Some("base") => "极暗".to_string(),
        Some("new_game_plus") => "狱火".to_string(),
        Some(other) => other.to_string(),
    }
}

/// mod 来源的中文短名（列表徽标用）。
pub fn source_cn(source: ModSource) -> &'static str {
    match source {
        ModSource::Local => "本地",
        ModSource::Steam => "工坊",
    }
}

/// 已安装列表显示名：无名 title 退回 dir_name（旧工具显示空串，此处给出可用名，
/// 差异在任务 5 报告里记录）。
pub fn display_title(m: &ModInfo) -> String {
    if m.title.trim().is_empty() {
        format!("[未命名] {}", m.dir_name)
    } else {
        m.title.clone()
    }
}

/// 已知 DLC id → 中文短名（2026-11-04 用户校正：与游戏内中文本地化一致的简称）。
/// `dlc_entries` 是权属列表——条目在列表里 = 该 DLC 属于此存档（文件里没有 enabled 布尔）。
fn dlc_cn(id: &str) -> &str {
    match id {
        "shieldbreaker" => "破盾",
        "crimson_court" => "庭院",
        "color_of_madness" => "农庄",
        "musketeer" => "火枪",
        "flagellant" => "苦修",
        "districts" => "蓝图",
        "arena_mp" => "竞技场",
        other => other,
    }
}

/// 存档 DLC 名称列表（详情区 chips 用）：文件顺序、已知 id 译中文、未知 id 原样。
pub fn dlc_names(entries: &[DlcEntry]) -> Vec<String> {
    entries
        .iter()
        .map(|e| dlc_cn(&e.name).to_string())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn local(title: &str) -> ModInfo {
        ModInfo {
            dir_name: format!("d{title}"),
            title: title.to_string(),
            source: ModSource::Local,
            icon_path: None,
        }
    }

    fn steam(id: &str) -> ModInfo {
        ModInfo {
            dir_name: id.to_string(),
            title: format!("W:{id}"),
            source: ModSource::Steam,
            icon_path: None,
        }
    }

    // ---- ModInfo ↔ AppliedMod ↔ ModEntry 映射（ruling 4 的回归锚） ----

    #[test]
    fn applied_name_local_uses_title_steam_uses_dir_name() {
        assert_eq!(
            applied_from_mod(&local("皮鞭修正")),
            AppliedMod::local("皮鞭修正")
        );
        // 本地未命名：title 空串原样进入（旧工具口径）。
        assert_eq!(applied_from_mod(&local("")).name, "");
        assert_eq!(
            applied_from_mod(&steam("1234567890")),
            AppliedMod::steam("1234567890")
        );
    }

    #[test]
    fn pipe_entry_uses_dson_source_literal_as_version() {
        let e = pipe_entry(&AppliedMod::local("X"));
        assert_eq!(e.title, "X");
        assert_eq!(e.version, "mod_local_source");
        let e = pipe_entry(&AppliedMod::steam("42"));
        assert_eq!(e.title, "42");
        assert_eq!(e.version, "Steam");
    }

    // ---- 脏状态跟踪 ----

    fn rows(list: &[(&str, ModSource)]) -> Vec<AppliedRow> {
        list.iter()
            .map(|(n, s)| AppliedRow {
                name: n.to_string(),
                source: *s,
                present: true,
            })
            .collect()
    }

    #[test]
    fn dirty_detects_order_add_remove_and_clean() {
        let baseline = vec![AppliedMod::local("a"), AppliedMod::steam("7")];
        assert!(!is_dirty(
            &baseline,
            &rows(&[("a", ModSource::Local), ("7", ModSource::Steam)])
        ));
        // 顺序变 → 脏（顺序即应用顺序）。
        assert!(is_dirty(
            &baseline,
            &rows(&[("7", ModSource::Steam), ("a", ModSource::Local)])
        ));
        // 增 / 删 → 脏。
        assert!(is_dirty(&baseline, &rows(&[("a", ModSource::Local)])));
        assert!(is_dirty(
            &baseline,
            &rows(&[
                ("a", ModSource::Local),
                ("7", ModSource::Steam),
                ("b", ModSource::Local)
            ])
        ));
        // 同名不同源 → 脏。
        assert!(is_dirty(
            &baseline,
            &rows(&[("a", ModSource::Local), ("7", ModSource::Local)])
        ));
        // present 标记不影响脏判定（仅显示）。
        let mut r = rows(&[("a", ModSource::Local), ("7", ModSource::Steam)]);
        r[0].present = false;
        assert!(!is_dirty(&baseline, &r));
    }

    // ---- present 标注 ----

    #[test]
    fn mark_present_matches_by_name_convention() {
        let installed = vec![local("A"), steam("9")];
        let mut r = rows(&[
            ("A", ModSource::Local),
            ("8", ModSource::Steam),
            ("A", ModSource::Steam),
        ]);
        mark_present(&mut r, &installed);
        assert_eq!(
            [r[0].present, r[1].present, r[2].present],
            [true, false, false]
        );
    }

    // ---- 已安装过滤 ----

    #[test]
    fn filter_is_substring_case_insensitive_on_title_or_dir() {
        let mods = vec![local("Piper Remaster"), steam("112233")];
        assert_eq!(filter_indices(&mods, "", &[]), vec![0, 1]);
        assert_eq!(filter_indices(&mods, "  ", &[]), vec![0, 1]);
        assert_eq!(filter_indices(&mods, "piper", &[]), vec![0]);
        assert_eq!(filter_indices(&mods, "112", &[]), vec![1]);
        assert!(filter_indices(&mods, "zzz", &[]).is_empty());
    }

    #[test]
    fn filter_excludes_applied_mods_like_old_move_semantics() {
        let mods = vec![local("A"), local("B"), steam("9")];
        let applied = rows(&[("A", ModSource::Local), ("9", ModSource::Steam)]);
        // 已加入的 A / 9 隐藏；本地 A 不影响工坊 9 的隐藏判断（同源同名才命中）
        assert_eq!(filter_indices(&mods, "", &applied), vec![1]);
        assert_eq!(filter_indices(&mods, "9", &applied), Vec::<usize>::new());
    }

    // ---- 热应用状态分类（ok 驱动，state verbatim） ----

    #[test]
    fn classify_all_documented_states() {
        // ok=true 一律成功（含 applied / applied-no-persist）。
        assert_eq!(classify_hotapply(true, "applied"), HotapplyVerdict::Ok);
        assert_eq!(
            classify_hotapply(true, "applied-no-persist"),
            HotapplyVerdict::Ok
        );
        // ok=false 的分支。
        assert_eq!(
            classify_hotapply(false, "game-not-running"),
            HotapplyVerdict::OfflineFallback
        );
        assert_eq!(
            classify_hotapply(false, "not-at-list"),
            HotapplyVerdict::NotAtList
        );
        for s in [
            "no-frame",
            "apply-timeout",
            "apply-exception",
            "no-envelope-array",
            "envelope-not-found:profile_x",
            "bad-vector:0..7",
            "bad-args",
            "bad-cmd",
            "engine-off:CreateRemoteThread failed",
            "timeout",
            "io-error",
            "dll-not-found",
            "bad-entry",
        ] {
            assert_eq!(
                classify_hotapply(false, s),
                HotapplyVerdict::Fail,
                "state={s}"
            );
        }
    }

    #[test]
    fn ok_and_fail_texts_match_old_tool_wording() {
        assert_eq!(
            hotapply_ok_text("applied"),
            "✅已热应用。直接进入该存档即生效；再改请先退到存档列表。"
        );
        assert_eq!(
            hotapply_ok_text("applied-no-persist"),
            "✅已热应用（applied-no-persist）。"
        );
        assert_eq!(
            hotapply_fail_text("timeout"),
            "⚠️热应用失败（timeout），未修改任何内容；可重试，或关闭游戏后应用。"
        );
        assert_eq!(offline_fallback_text(None), "✅已写入存档（游戏未运行）。");
        assert_eq!(
            offline_fallback_text(Some("engine-off:no pipe")),
            "✅已写入存档（游戏未运行，引擎未接入：engine-off:no pipe）。"
        );
        assert_eq!(
            file_only_ok_text(),
            "✅已写入存档（未开启热修改，改动在游戏下次读档时生效）。"
        );
        assert_eq!(
            file_only_game_running_text(),
            "⚠️已写入文件，但游戏正在运行——退出时游戏会覆盖本次修改，建议关闭游戏后再应用。"
        );
    }

    // ---- 净化 ----

    #[test]
    fn sanitize_replaces_control_chars_and_truncates_at_160() {
        assert_eq!(sanitize_detail("ab\u{1}c"), "ab?c");
        assert_eq!(sanitize_detail("del\u{7f}ete"), "del?ete");
        let long = "字".repeat(200);
        let out = sanitize_detail(&long);
        assert_eq!(out.chars().count(), 161);
        assert!(out.ends_with('…'));
        let exact = "x".repeat(160);
        assert_eq!(sanitize_detail(&exact).chars().count(), 160);
        // 中文按字符截断，不切 UTF-8 字节。
        assert!(sanitize_detail(&long).starts_with("字"));
    }

    // ---- game_mode 中文 ----

    #[test]
    fn game_mode_mapping() {
        assert_eq!(game_mode_cn(Some("bloodmoon")), "血月");
        assert_eq!(game_mode_cn(Some("radiant")), "明耀");
        assert_eq!(game_mode_cn(Some("base")), "极暗");
        assert_eq!(game_mode_cn(Some("new_game_plus")), "狱火");
        assert_eq!(game_mode_cn(None), "—");
        assert_eq!(game_mode_cn(Some("future_mode")), "future_mode");
    }

    // ---- DLC 摘要（FR1 DLC 态；权属列表 = 条目在即启用） ----

    fn dlc(name: &str) -> DlcEntry {
        DlcEntry {
            name: name.to_string(),
            source: "dlc".to_string(),
        }
    }

    #[test]
    fn dlc_names_empty_is_empty() {
        assert!(dlc_names(&[]).is_empty());
    }

    #[test]
    fn dlc_names_known_ids_in_file_order() {
        // 金样本 profile_7 的文件顺序（T3 报告）：全部 7 条。
        let entries: Vec<DlcEntry> = [
            "arena_mp",
            "musketeer",
            "crimson_court",
            "districts",
            "flagellant",
            "shieldbreaker",
            "color_of_madness",
        ]
        .iter()
        .map(|&n| dlc(n))
        .collect();
        assert_eq!(
            dlc_names(&entries),
            vec!["竞技场", "火枪", "庭院", "蓝图", "苦修", "破盾", "农庄"]
        );
    }

    #[test]
    fn dlc_names_unknown_id_shown_raw() {
        let entries = vec![dlc("shieldbreaker"), dlc("future_dlc")];
        assert_eq!(dlc_names(&entries), vec!["破盾", "future_dlc"]);
    }

    // ---- 配置读写 round-trip ----

    #[test]
    fn config_roundtrip_and_missing_file() {
        let dir = std::env::temp_dir().join(format!("dd-mrm-test-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("dd-mrm.json");

        // 首跑：文件不存在 → None。
        let _ = std::fs::remove_file(&path);
        assert!(AppConfig::load(&path).unwrap().is_none());

        let cfg = AppConfig {
            roots: SaveRoots {
                save_root: Some(dir.join("saves")),
                local_mods_dir: Some(dir.join("mods")),
                workshop_dir: None,
            },
            presets_path: dir.join("p.json"),
            hotapply: false,
        };
        cfg.save(&path).unwrap();
        assert!(path.exists());
        assert!(!dir.join("dd-mrm.json.tmp").exists());
        let back = AppConfig::load(&path).unwrap().unwrap();
        assert_eq!(back, cfg);
        // 覆盖写（旧文件 rename 路径）也走一遍。
        cfg.save(&path).unwrap();
        assert_eq!(AppConfig::load(&path).unwrap().unwrap(), cfg);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_corrupt_is_err_never_silent_reset() {
        let dir = std::env::temp_dir().join(format!("dd-mrm-test-bad-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("dd-mrm.json");
        std::fs::write(&path, b"{ not json").unwrap();
        let err = AppConfig::load(&path).unwrap_err();
        assert!(err.contains("解析失败"), "err={err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn config_defaults_presets_path_to_exe_dir_and_omitted_fields_ok() {
        // presets_path 缺省 → exe 同目录 presets.json；roots 字段缺失 → None；hotapply 缺省 → true。
        let dir = std::env::temp_dir().join(format!("dd-mrm-test-dflt-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("dd-mrm.json");
        std::fs::write(&path, br#"{"save_root":"C:/S"}"#).unwrap();
        let cfg = AppConfig::load(&path).unwrap().unwrap();
        assert_eq!(cfg.roots.save_root, Some(PathBuf::from("C:/S")));
        assert_eq!(cfg.roots.local_mods_dir, None);
        assert_eq!(cfg.presets_path, default_presets_path());
        assert!(cfg.hotapply, "缺省字段应回退为开启热修改");
        let _ = std::fs::remove_dir_all(&dir);
    }

    // ---- 显示辅助 ----

    #[test]
    fn display_title_falls_back_to_dir_name() {
        let mut m = local("Named");
        assert_eq!(display_title(&m), "Named");
        m = local("  ");
        assert_eq!(display_title(&m), format!("[未命名] {}", m.dir_name));
    }
}
