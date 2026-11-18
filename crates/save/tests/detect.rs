//! detect_from 的文件系统布局测试：假 Steam 树（TempGuard），不碰真注册表。
//! 覆盖「其他电脑」形态：多库、多账号、游戏未装、vdf 缺失、工坊未订阅。

mod common;

use std::fs;
use std::path::{Path, PathBuf};

use common::TempGuard;
use save::detect::detect_from;

fn mkdirs(p: &Path) {
    fs::create_dir_all(p).expect("mkdir");
}

/// 写 libraryfolders.vdf（路径按 vdf 惯例反斜杠转义）。
fn write_vdf(steam: &Path, libs: &[PathBuf]) {
    mkdirs(&steam.join("steamapps"));
    let entries: Vec<String> = libs
        .iter()
        .enumerate()
        .map(|(i, l)| {
            let escaped = l.to_string_lossy().replace('\\', "\\\\");
            format!("\t\"{i}\"\n\t{{\n\t\t\"path\"\t\t\"{escaped}\"\n\t}}")
        })
        .collect();
    let text = format!("\"libraryfolders\"\n{{\n{}\n}}\n", entries.join("\n"));
    fs::write(steam.join("steamapps").join("libraryfolders.vdf"), text).expect("vdf");
}

/// 游戏目录骨架：_windows\win64\Darkest.exe（身份标记）。
fn make_game(lib: &Path) -> PathBuf {
    let game = lib.join("steamapps").join("common").join("DarkestDungeon");
    mkdirs(&game.join("_windows").join("win64"));
    fs::write(game.join("_windows").join("win64").join("Darkest.exe"), b"MZ").expect("exe");
    game
}

fn account_remote(steam: &Path, uid: &str) -> PathBuf {
    let remote = steam
        .join("userdata")
        .join(uid)
        .join("262060")
        .join("remote");
    mkdirs(&remote);
    remote
}

#[test]
fn full_layout_two_accounts_two_libraries() {
    let g = TempGuard::new("detect-full");
    let steam = g.path("steam");
    let lib2 = g.path("lib2");
    mkdirs(&steam);

    let r111 = account_remote(&steam, "111");
    let r222 = account_remote(&steam, "222");

    // 游戏在主库（vdf 0 号 = 主目录），工坊在第二库。
    let game = make_game(&steam);
    mkdirs(&game.join("mods"));
    let ws2 = lib2
        .join("steamapps")
        .join("workshop")
        .join("content")
        .join("262060")
        .join("123456");
    mkdirs(&ws2);
    write_vdf(&steam, &[steam.clone(), lib2.clone()]);

    let d = detect_from(&steam);
    assert_eq!(d.save_root, Some(r111.clone()));
    assert_eq!(d.save_root_candidates, vec![r111, r222]);
    assert_eq!(d.local_mods_dir, Some(game.join("mods")));
    assert_eq!(d.workshop_dir, Some(ws2.parent().unwrap().to_path_buf()));
    assert!(d.notes.iter().any(|n| n.contains("已取 UID 最小的一个（UID 111）")), "notes: {d:?}");
}

#[test]
fn steam_dir_missing() {
    let g = TempGuard::new("detect-missing");
    let d = detect_from(&g.path("nope"));
    assert_eq!(d.save_root, None);
    assert_eq!(d.local_mods_dir, None);
    assert_eq!(d.workshop_dir, None);
    assert!(d.notes.iter().any(|n| n.contains("Steam 目录不存在")), "notes: {d:?}");
}

#[test]
fn bare_steam_no_userdata_no_vdf() {
    let g = TempGuard::new("detect-bare");
    let steam = g.path("steam");
    mkdirs(&steam);
    let d = detect_from(&steam);
    assert_eq!(d.save_root, None);
    assert_eq!(d.local_mods_dir, None);
    assert_eq!(d.workshop_dir, None);
    assert!(d.notes.iter().any(|n| n.contains("userdata")), "notes: {d:?}");
    assert!(d.notes.iter().any(|n| n.contains("libraryfolders")), "notes: {d:?}");
    assert!(d.notes.iter().any(|n| n.contains("未找到 DarkestDungeon")), "notes: {d:?}");
}

#[test]
fn workshop_predicted_from_game_library() {
    let g = TempGuard::new("detect-predict");
    let steam = g.path("steam");
    mkdirs(&steam);
    let game = make_game(&steam);
    mkdirs(&game.join("mods"));
    write_vdf(&steam, &[steam.clone()]);

    let d = detect_from(&steam);
    assert_eq!(d.local_mods_dir, Some(game.join("mods")));
    assert_eq!(
        d.workshop_dir,
        Some(steam.join("steamapps").join("workshop").join("content").join("262060"))
    );
    assert!(d.notes.iter().any(|n| n.contains("预测")), "notes: {d:?}");
}

#[test]
fn game_without_mods_dir_predicted() {
    let g = TempGuard::new("detect-nomods");
    let steam = g.path("steam");
    mkdirs(&steam);
    let game = make_game(&steam);
    write_vdf(&steam, &[steam.clone()]);

    let d = detect_from(&steam);
    assert_eq!(d.local_mods_dir, Some(game.join("mods")));
    assert!(d.notes.iter().any(|n| n.contains("mods 子目录")), "notes: {d:?}");
}

#[test]
fn game_in_second_library_found() {
    let g = TempGuard::new("detect-lib2");
    let steam = g.path("steam");
    let lib2 = g.path("lib2");
    mkdirs(&steam);
    mkdirs(&lib2.join("steamapps")); // lib2 出现在 vdf 但游戏目录由主库空缺
    let game = make_game(&lib2);
    mkdirs(&game.join("mods"));
    write_vdf(&steam, &[steam.clone(), lib2.clone()]);

    let d = detect_from(&steam);
    assert_eq!(d.local_mods_dir, Some(game.join("mods")));
}
