//! 服务级集成测试：mod 扫描、estate 发现、配置 JSON、备份与清理、原子写。

#[path = "common/mod.rs"]
mod common;

use std::fs;
use std::io::Read;
use std::path::Path;
use std::time::Duration;

use common::TempGuard;
use save::{
    AppliedMod, ModSource, Presets, SaveRoots, backup_estate_dir, clean_old_backups,
    discover_estates, scan_local_and_workshop, scan_mod_dir, validate_persist_path,
    write_persist_atomic,
};

fn write_file(path: &Path, content: &str) {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(path, content).unwrap();
}

// ---------- scan ----------

#[test]
fn scan_local_mods_full_semantics() {
    let guard = TempGuard::new("scan");

    // 正常本地 mod：标题 + 实体解码 + 图标
    write_file(
        &guard.path("mods/Alpha/project.xml"),
        r#"<Project><Title>王国 &amp; 荣耀</Title></Project>"#,
    );
    fs::write(guard.path("mods/Alpha/preview_icon.png"), b"png").unwrap();

    // 有 project.xml 但无 <Title> → 本地标题为 ""，仍是 mod
    write_file(&guard.path("mods/Beta/project.xml"), "<Project></Project>");

    // 空标题标签 → 匹配成功，标题 ""
    write_file(&guard.path("mods/Beta2/project.xml"), "<Title></Title>");

    // 贪婪语义：两个 Title 标签吃成一段
    write_file(
        &guard.path("mods/Greedy/project.xml"),
        "<Title>一</Title>尾巴<Title>二</Title>",
    );

    // 大写文件名也算 project.xml
    write_file(
        &guard.path("mods/CaseName/PROJECT.XML"),
        "<Title>大写</Title>",
    );

    // 无标题文件 → 不是 mod
    fs::create_dir_all(guard.path("mods/NoTitleFile")).unwrap();
    write_file(&guard.path("mods/NoTitleFile/readme.txt"), "x");

    // 普通文件（非目录）不算候选
    write_file(&guard.path("mods/loose_file.txt"), "x");

    let report = scan_mod_dir(&guard.path("mods"), ModSource::Local);
    assert!(report.failed_dirs.is_empty(), "{:?}", report.failed_dirs);
    let names: Vec<(&str, &str, bool)> = report
        .mods
        .iter()
        .map(|m| (m.dir_name.as_str(), m.title.as_str(), m.icon_path.is_some()))
        .collect();
    // Alpha 带 preview_icon.png：路径应指向该文件本身。
    assert_eq!(
        report.mods[0].icon_path.as_ref().map(|p| p
            .file_name()
            .unwrap()
            .to_string_lossy()
            .into_owned()),
        Some("preview_icon.png".into())
    );
    assert_eq!(
        names,
        vec![
            ("Alpha", "王国 & 荣耀", true),
            ("Beta", "", false),
            ("Beta2", "", false),
            ("CaseName", "大写", false),
            ("Greedy", "一</Title>尾巴<Title>二", false),
        ]
    );
}

#[test]
fn scan_workshop_mods_and_default_title() {
    let guard = TempGuard::new("wscan");

    write_file(
        &guard.path("workshop/1234567890/workshop.vdf"),
        "\"description\"\n{\n\t\"title\"\t\t\"我的工坊模组\"\n}",
    );
    // vdf 无 title 键 → 默认标签
    write_file(
        &guard.path("workshop/9876543210/workshop.vdf"),
        "{\"description\":\"x\"}",
    );

    let report = scan_mod_dir(&guard.path("workshop"), ModSource::Steam);
    assert!(report.failed_dirs.is_empty());
    assert_eq!(
        report
            .mods
            .iter()
            .map(|m| (m.dir_name.as_str(), m.title.as_str()))
            .collect::<Vec<_>>(),
        vec![
            ("1234567890", "我的工坊模组"),
            ("9876543210", "[Mod id]: 9876543210"),
        ]
    );
}

#[test]
fn scan_missing_root_and_combined() {
    let guard = TempGuard::new("scan0");
    let report = scan_mod_dir(&guard.path("nope"), ModSource::Local);
    assert!(report.mods.is_empty() && report.failed_dirs.is_empty());

    // 组合扫描：本地 + 工坊
    write_file(&guard.path("l/M1/project.xml"), "<Title>L1</Title>");
    write_file(&guard.path("w/777/workshop.vdf"), "\"title\" \"W1\"");
    let combined = scan_local_and_workshop(&guard.path("l"), &guard.path("w"));
    assert_eq!(combined.mods.len(), 2);
    assert_eq!(combined.mods[0].source, ModSource::Local);
    assert_eq!(combined.mods[1].source, ModSource::Steam);
}

// ---------- paths ----------

#[test]
fn discover_estates_finds_only_persist_subdirs() {
    let guard = TempGuard::new("est");
    // 两个合法 estate
    write_file(&guard.path("save/estate_b/persist.game.json"), "{}");
    write_file(&guard.path("save/estate_a/persist.game.json"), "{}");
    // 无 persist 的目录不算
    fs::create_dir_all(guard.path("save/plain_dir")).unwrap();
    // 根下直接放的 persist.game.json 文件不算（只认子目录）
    write_file(&guard.path("save/persist.game.json"), "{}");

    let estates = discover_estates(&guard.path("save")).unwrap();
    let profiles: Vec<&str> = estates.iter().map(|e| e.profile.as_str()).collect();
    assert_eq!(profiles, vec!["estate_a", "estate_b"], "按 profile 排序");
    assert!(estates[0].persist_path.ends_with(save::PERSIST_FILE_NAME));

    // save root 不存在 → 空而非错误
    assert!(discover_estates(&guard.path("missing")).unwrap().is_empty());
}

#[test]
fn validate_persist_path_rules() {
    let ok = Path::new("/x/estate_7/persist.game.json");
    assert!(validate_persist_path(ok).is_ok());
    // Windows 大小写不敏感文件系统口径
    assert!(validate_persist_path(&ok.with_file_name("Persist.Game.Json")).is_ok());
    for bad in [
        "/x/other.json",
        "/x/persist.game.json.bak",
        "/x/persistgame.json",
    ] {
        assert!(validate_persist_path(Path::new(bad)).is_err(), "{bad}");
    }
    assert!(validate_persist_path(Path::new("/x/estate")).is_err());
}

#[test]
fn save_roots_and_presets_roundtrip_and_corrupt() {
    let guard = TempGuard::new("cfg");

    // SaveRoots：缺失 → None；写读往返；损坏 → Err
    let roots_path = guard.path("roots.json");
    assert!(SaveRoots::load(&roots_path).unwrap().is_none());
    let roots = SaveRoots {
        save_root: Some(guard.path("saves")),
        local_mods_dir: Some(guard.path("mods")),
        workshop_dir: None,
    };
    roots.save(&roots_path).unwrap();
    assert_eq!(SaveRoots::load(&roots_path).unwrap().unwrap(), roots);
    let text = fs::read_to_string(&roots_path).unwrap();
    assert!(text.contains("\"save_root\"") && text.contains("\"workshop_dir\""));
    write_file(&roots_path, "{ not json");
    assert!(
        SaveRoots::load(&roots_path).is_err(),
        "损坏配置不得静默重置"
    );

    // Presets：同上
    let presets_path = guard.path("presets.json");
    assert!(Presets::load(&presets_path).unwrap().is_none());
    let presets = Presets {
        presets: [(
            "开荒".to_owned(),
            vec![
                AppliedMod::local("王国与荣耀"),
                AppliedMod::steam("1234567890"),
            ],
        )]
        .into_iter()
        .collect(),
    };
    presets.save(&presets_path).unwrap();
    assert_eq!(Presets::load(&presets_path).unwrap().unwrap(), presets);
    let text = fs::read_to_string(&presets_path).unwrap();
    assert!(text.contains("\"presets\""));
    assert!(text.contains("\"source\": \"steam\"") || text.contains("\"source\":\"steam\""));
    // 损坏（类型错）：报错而非静默重置。
    // （注：serde 对结构体的元组形式 `[]` 会因字段全 default 而接受为空预设，
    // 这是无害的长尾；语法错误与类型错误均严格报错。）
    write_file(&presets_path, r#"{"presets": "not a map"}"#);
    assert!(Presets::load(&presets_path).is_err());
    write_file(&presets_path, "{ not json");
    assert!(Presets::load(&presets_path).is_err());

    // 原子写不留 .tmp 残留
    assert!(!roots_path.with_extension("json.tmp").exists());
    assert!(!guard.path("roots.json.tmp").exists());
}

// ---------- backup ----------

#[test]
fn backup_zip_contains_tree_and_rejects_same_second() {
    let guard = TempGuard::new("bak");
    let estate = guard.path("estate_x");
    write_file(&estate.join("persist.game.json"), "SAVE-BYTES");
    write_file(&estate.join("sub/deep/other.bin"), "DEEP");
    let backup_dir = guard.path("SaveBak");

    let zip_path = backup_estate_dir(&estate, "我的庄园", &backup_dir).unwrap();
    let name = zip_path.file_name().unwrap().to_string_lossy().into_owned();
    assert!(
        name.starts_with("我的庄园_") && name.ends_with(".zip"),
        "{name}"
    );

    // zip 内容：目录 + 文件条目齐全，内容可读回
    let f = fs::File::open(&zip_path).unwrap();
    let mut archive = zip::ZipArchive::new(f).unwrap();
    let mut names: Vec<String> = (0..archive.len())
        .map(|i| archive.by_index(i).unwrap().name().to_owned())
        .collect();
    names.sort();
    assert_eq!(
        names,
        vec![
            "persist.game.json",
            "sub/",
            "sub/deep/",
            "sub/deep/other.bin",
        ]
    );
    let mut by_name = archive.by_name("sub/deep/other.bin").unwrap();
    let mut content = String::new();
    by_name.read_to_string(&mut content).unwrap();
    assert_eq!(content, "DEEP");

    // 同一 label 同一秒再备份 → 目标已存在错误
    let err = backup_estate_dir(&estate, "我的庄园", &backup_dir).unwrap_err();
    assert!(matches!(err, save::SaveError::Backup { .. }), "{err}");
    // 不同 label 不冲突
    assert!(backup_estate_dir(&estate, "另一庄园", &backup_dir).is_ok());
}

#[test]
fn clean_old_backups_respects_max_age() {
    let guard = TempGuard::new("clean");
    let backup_dir = guard.path("SaveBak");
    fs::create_dir_all(&backup_dir).unwrap();
    for i in 0..3 {
        fs::write(backup_dir.join(format!("b{i}.zip")), b"z").unwrap();
    }
    fs::write(backup_dir.join("keep.txt"), b"not a zip").unwrap();

    // 巨大 max_age：一个都不删
    assert_eq!(
        clean_old_backups(&backup_dir, Duration::from_secs(3600 * 24 * 365)).unwrap(),
        0
    );
    // max_age = 0：mtime < now 全删（创建即过去）
    assert_eq!(clean_old_backups(&backup_dir, Duration::ZERO).unwrap(), 3);
    assert!(!backup_dir.join("b0.zip").exists());
    assert!(backup_dir.join("keep.txt").exists(), "非 zip 不动");
    // 目录不存在 → 0
    assert_eq!(
        clean_old_backups(&guard.path("nope"), Duration::ZERO).unwrap(),
        0
    );
}

// ---------- atomic write ----------

#[test]
fn atomic_write_overwrites_and_leaves_no_tmp() {
    let guard = TempGuard::new("atom");
    let estate = guard.path("estate_a");
    fs::create_dir_all(&estate).unwrap();
    let persist = estate.join("persist.game.json");

    write_persist_atomic(&persist, b"first").unwrap();
    assert_eq!(fs::read(&persist).unwrap(), b"first");
    write_persist_atomic(&persist, b"second").unwrap();
    assert_eq!(fs::read(&persist).unwrap(), b"second");
    assert!(!estate.join("persist.game.json.tmp").exists(), "不留 .tmp");
}
