//! T3 验收矩阵（GC6）：四场景与旧 C# 工具字节对拍 + 白名单/幂等回归。

#[path = "common/mod.rs"]
mod common;

use std::fs;
use std::path::{Path, PathBuf};

use common::TempGuard;
use save::{AppliedMod, EstateFile, SaveError, apply_offline, write_persist_atomic};

/// goldens 位于**工作区根** tests/golden/（crate 目录的上两级）。
fn manifest_dir() -> PathBuf {
    let crate_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    crate_dir
        .parent()
        .and_then(|p| p.parent())
        .expect("crate 目录必有两级父目录")
        .to_path_buf()
}

fn golden(name: &str) -> Vec<u8> {
    fs::read(manifest_dir().join("tests").join("golden").join(name))
        .unwrap_or_else(|e| panic!("读 golden {name} 失败: {e}"))
}

fn expected(name: &str) -> Vec<u8> {
    fs::read(
        manifest_dir()
            .join("tests")
            .join("golden")
            .join("expected")
            .join(name),
    )
    .unwrap_or_else(|e| panic!("读 expected {name} 失败: {e}"))
}

/// 字节对拍失败时给出长度 + 首差异偏移，避免盲猜。
fn assert_bytes_equal(actual: &[u8], expected: &[u8], ctx: &str) {
    if actual == expected {
        return;
    }
    let mut msg = format!(
        "{ctx}: 字节不一致（actual {} B, expected {} B）",
        actual.len(),
        expected.len()
    );
    for (i, (a, b)) in actual.iter().zip(expected.iter()).enumerate() {
        if a != b {
            msg += &format!("；首个差异 @ {i}: actual={a:#04x} expected={b:#04x}",);
            break;
        }
    }
    panic!("{msg}");
}

/// 把 golden 复制进临时 estate 目录并返回 persist 路径。
fn stage_golden(tag: &str, name: &str) -> (TempGuard, PathBuf) {
    let guard = TempGuard::new(tag);
    let estate = guard.path("estate_7");
    fs::create_dir_all(&estate).unwrap();
    let persist = estate.join("persist.game.json");
    fs::write(&persist, golden(name)).unwrap();
    (guard, persist)
}

/// 场景骨架：打开 → sync 指定列表 → 编码字节。
fn sync_to_bytes(persist: &Path, mods: &[AppliedMod]) -> Vec<u8> {
    let mut file = EstateFile::open(persist).expect("打开 golden 失败");
    file.sync_mods(mods).expect("sync_mods 失败");
    file.to_bytes().expect("编码失败")
}

const PROFILE_7: &str = "profile_7.persist.game.json.dson";

/// 矩阵 a：原 9 个 mod 原样重写。
#[test]
fn golden_a_same_rewrite() {
    let (_guard, persist) = stage_golden("ga", PROFILE_7);
    let mods = EstateFile::open(&persist).unwrap().applied_mods().unwrap();
    assert_eq!(mods.len(), 9, "profile_7 应有 9 个 applied mod");
    let bytes = sync_to_bytes(&persist, &mods);
    assert_bytes_equal(&bytes, &expected("profile_7.same-rewrite.bin"), "场景 a");
}

/// 矩阵 b：去掉最后一个 mod。
#[test]
fn golden_b_remove_last() {
    let (_guard, persist) = stage_golden("gb", PROFILE_7);
    let mods = EstateFile::open(&persist).unwrap().applied_mods().unwrap();
    let first8: Vec<AppliedMod> = mods.into_iter().take(8).collect();
    let bytes = sync_to_bytes(&persist, &first8);
    assert_bytes_equal(&bytes, &expected("profile_7.remove-last-mod.bin"), "场景 b");
}

/// 矩阵 c：全部 9 个 + 新增工坊 mod 1234567890（共 10 项）。
///
/// ⚠ 任务文本写的是 "first 8 + 新增"，但 GC6 的 oracle 文件
/// expected/profile_7.add-mod-1234567890.bin 实际含 **10** 项
/// （9 个原 mod + 新增，键 "0".."9"）——以 golden 为准。
#[test]
fn golden_c_add_steam_mod() {
    let (_guard, persist) = stage_golden("gc", PROFILE_7);
    let mods = EstateFile::open(&persist).unwrap().applied_mods().unwrap();
    let mut list: Vec<AppliedMod> = mods.clone();
    list.push(AppliedMod::steam("1234567890"));
    let bytes = sync_to_bytes(&persist, &list);
    assert_bytes_equal(
        &bytes,
        &expected("profile_7.add-mod-1234567890.bin"),
        "场景 c",
    );
}

/// 矩阵 d：清空 → 字段消失。（清空后重开读盘的语义复核见
/// `clear_then_readd_roundtrip`——`sync_to_bytes` 不落盘。）
#[test]
fn golden_d_clear_mods() {
    let (_guard, persist) = stage_golden("gd", PROFILE_7);
    let bytes = sync_to_bytes(&persist, &[]);
    assert_bytes_equal(&bytes, &expected("profile_7.clear-mods.bin"), "场景 d");
}

/// profile_2（另一份工具写出的档）：no-change sync 应字节稳定。
#[test]
fn profile_2_no_change_is_stable() {
    let (_guard, persist) = stage_golden("p2", "profile_2.persist.game.json.dson");
    let source = golden("profile_2.persist.game.json.dson");
    let mods = EstateFile::open(&persist).unwrap().applied_mods().unwrap();
    let bytes = sync_to_bytes(&persist, &mods);
    assert_bytes_equal(&bytes, &source, "profile_2 no-change");
}

/// 游戏写出的档（game-modmgr-4716）：纯解析/编码往返字节不变；
/// sync 相同列表后语义不变（列表一致）。
#[test]
fn game_written_roundtrip_and_sync_same() {
    let guard = TempGuard::new("gmg");
    let estate = guard.path("estate_g");
    fs::create_dir_all(&estate).unwrap();
    let persist = estate.join("persist.game.json");
    let source = golden("profile_7.game-modmgr-4716.dson");
    fs::write(&persist, &source).unwrap();

    // 1) 未修改直接编码 == 源字节
    let file = EstateFile::open(&persist).unwrap();
    assert_bytes_equal(&file.to_bytes().unwrap(), &source, "game 档纯往返");

    // 2) sync 相同列表：语义幂等（重新解析后列表一致）
    let mods = file.applied_mods().unwrap();
    let mut file = EstateFile::open(&persist).unwrap();
    file.sync_mods(&mods).unwrap();
    file.write().unwrap();
    assert_eq!(
        EstateFile::open(&persist).unwrap().applied_mods().unwrap(),
        mods,
        "game 档 sync 相同列表后语义不变"
    );
}

/// 清空 → 落盘 → 重开加回一个 mod → 再落盘 → 重读（端到端往返）。
#[test]
fn clear_then_readd_roundtrip() {
    let (_guard, persist) = stage_golden("rt", PROFILE_7);
    let m = AppliedMod::local("重生之我是测试");
    apply_offline(&persist, &[]).unwrap();
    assert!(
        EstateFile::open(&persist)
            .unwrap()
            .applied_mods()
            .unwrap()
            .is_empty()
    );

    apply_offline(&persist, std::slice::from_ref(&m)).unwrap();
    assert_eq!(
        EstateFile::open(&persist).unwrap().applied_mods().unwrap(),
        vec![m.clone()],
        "清空后重新应用一个 mod 应可读回"
    );
}

/// GC3 白名单：非 persist.game.json 的路径一律拒绝（读与写入口）。
#[test]
fn whitelist_rejects_non_persist_targets() {
    let guard = TempGuard::new("wl");
    let dir = guard.path("estate");
    fs::create_dir_all(&dir).unwrap();

    let bad = dir.join("other.json");
    fs::write(&bad, b"{}").unwrap();
    assert!(matches!(
        EstateFile::open(&bad),
        Err(SaveError::Whitelist { .. })
    ));
    assert!(matches!(
        write_persist_atomic(&bad, b"x"),
        Err(SaveError::Whitelist { .. })
    ));
    assert!(matches!(
        apply_offline(&bad, &[]),
        Err(SaveError::Whitelist { .. })
    ));

    // 恶意变体：正确基名 + 后缀 / 前缀 / 无文件名
    for name in [
        "persist.game.json.bak",
        "notpersist.game.json",
        "persist.game.jsonx",
    ] {
        let p = dir.join(name);
        assert!(
            matches!(
                write_persist_atomic(&p, b"x"),
                Err(SaveError::Whitelist { .. })
            ),
            "{name} 应被白名单拒绝"
        );
    }
    assert!(matches!(
        write_persist_atomic(&guard.dir.join("plain_dir"), b"x"),
        Err(SaveError::Whitelist { .. })
    ));

    // 白名单通过的合法名（不存在的文件 → Io 而非 Whitelist）
    assert!(matches!(
        EstateFile::open(&dir.join("persist.game.json")),
        Err(SaveError::Io { .. })
    ));
}

/// DLC 权属列表（FR1 读侧）：三份金样本的 `base_root.dlc` 实测内容。
///
/// 实证：7 条 `{name, source:"dlc"}`，按文件顺序，**无 enabled 布尔**
/// （与旧 C# `DlcItem{name,source}` 一致；"是否启用"= 条目在不在列表）。
#[test]
fn dlc_entries_match_goldens() {
    let expected_names = [
        "arena_mp",
        "musketeer",
        "crimson_court",
        "districts",
        "flagellant",
        "shieldbreaker",
        "color_of_madness",
    ];
    for name in [
        "profile_7.persist.game.json.dson",
        "profile_7.game-modmgr-4716.dson",
        "profile_2.persist.game.json.dson",
    ] {
        let (_guard, persist) = stage_golden("dlc_read", name);
        let file = EstateFile::open(&persist).unwrap();
        let entries = file.dlc_entries().unwrap();
        let got: Vec<(&str, &str)> = entries
            .iter()
            .map(|e| (e.name.as_str(), e.source.as_str()))
            .collect();
        let want: Vec<(&str, &str)> = expected_names.iter().map(|&n| (n, "dlc")).collect();
        assert_eq!(got, want, "golden {name} 的 dlc 列表应逐项一致");
    }
}
