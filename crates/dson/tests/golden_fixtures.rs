//! 金样本夹具验收（Task 2 验收 ①②③ 的落地，控制器裁定 #2）。
//!
//! 结构知识（applied 条目 = 对象 "N"{name: Str, source: Str}）按裁定编码在
//! 本测试文件内；库保持泛用。三个场景全部通过 `replace_children` 变更 API
//! 构造，与旧 C# 工具（GC6 基线）的期望输出逐字节比较。

use std::path::PathBuf;

use dson::meta::fi_bit31;
use dson::model::{DsonDoc, FieldValue, NewNode, NewValue, NodeId};
use dson::parse;

fn golden_dir() -> PathBuf {
    // CARGO_MANIFEST_DIR = crates/dson
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../tests/golden")
        .canonicalize()
        .expect("tests/golden exists")
}

fn load(name: &str) -> Vec<u8> {
    std::fs::read(golden_dir().join(name)).unwrap_or_else(|e| panic!("{name}: {e}"))
}

fn parse_golden(name: &str) -> DsonDoc {
    let bytes = load(name);
    parse(&bytes).unwrap_or_else(|e| panic!("{name}: {e}"))
}

/// 递归把现有子树转成 NewNode（经类型化访问器读取，测试专用）。
fn node_to_new(doc: &DsonDoc, id: NodeId, parent_path: &[&str]) -> Option<NewNode> {
    let n = doc.node(id);
    let mut full: Vec<&str> = parent_path.to_vec();
    full.push(n.name.as_str());
    if n.is_object {
        let kids: Vec<NewNode> = n
            .children
            .iter()
            .filter_map(|&c| node_to_new(doc, c, &full))
            .collect();
        Some(NewNode::new(n.name.clone(), NewValue::Object(kids)))
    } else {
        let v = doc.field_value(&full)?;
        let nv = match v {
            FieldValue::Str(s) => NewValue::Str(s.clone()),
            FieldValue::Bool(b) => NewValue::Bool(*b),
            FieldValue::Int(i) => NewValue::Int(*i),
            FieldValue::TwoBool(a, b) => NewValue::TwoBool(*a, *b),
        };
        Some(NewNode::new(n.name.clone(), nv))
    }
}

/// applied 列表路径（含根名）。
const APPLIED: [&str; 2] = ["base_root", "applied_ugcs_1_0"];

/// 取 applied 列表的全部子节点 id。
fn applied_children(doc: &DsonDoc) -> Vec<NodeId> {
    doc.children_of(&APPLIED)
        .expect("applied list exists")
        .to_vec()
}

/// 断言一个 applied 条目的结构：对象 "N"，恰好两个 Str 子字段 name/source。
fn assert_entry_shape(doc: &DsonDoc, idx: &str, name: &str, source: &str) {
    let path = ["base_root", "applied_ugcs_1_0", idx];
    let kids = doc.children_of(&path).expect("entry is an object");
    assert_eq!(kids.len(), 2, "entry {idx} must have exactly 2 children");
    assert_eq!(doc.str_value(&[path[0], path[1], idx, "name"]), Some(name));
    assert_eq!(
        doc.str_value(&[path[0], path[1], idx, "source"]),
        Some(source)
    );
}

fn bit31_true_count(doc: &DsonDoc) -> usize {
    doc.nodes()
        .iter()
        .filter(|n| n.source_field_info().is_some_and(fi_bit31))
        .count()
}

// ---- 验收 ①/③：三个金样本同内容重写字节一致 -------------------------------

#[test]
fn same_rewrite_profile_2() {
    let bytes = load("profile_2.persist.game.json.dson");
    let doc = parse(&bytes).unwrap();
    assert_eq!(doc.write().unwrap(), bytes);
}

#[test]
fn same_rewrite_profile_7() {
    let bytes = load("profile_7.persist.game.json.dson");
    let doc = parse(&bytes).unwrap();
    let out = doc.write().unwrap();
    assert_eq!(out, bytes);
    // 与旧 C# 工具的 same-rewrite 期望文件也逐字节一致（GC6 基线）
    assert_eq!(out, load("expected/profile_7.same-rewrite.bin"));
}

#[test]
fn same_rewrite_modmgr() {
    let bytes = load("profile_7.game-modmgr-4716.dson");
    let doc = parse(&bytes).unwrap();
    assert_eq!(doc.write().unwrap(), bytes);
}

// ---- 结构发现锁定：applied 条目形状 + 容器布局 ------------------------------

#[test]
fn applied_entry_shape_is_name_source_strings() {
    let doc = parse_golden("profile_7.persist.game.json.dson");
    let kids = applied_children(&doc);
    assert_eq!(kids.len(), 9, "profile_7 has 9 applied mods (\"0\"..\"8\")");
    for (i, &id) in kids.iter().enumerate() {
        let n = doc.node(id);
        assert!(n.is_object);
        assert_eq!(n.name, i.to_string(), "entries are named \"0\",\"1\",..");
    }
    // 首条与末条 spot check（值来自金样本实测）
    assert_entry_shape(&doc, "0", "2920704248", "Steam");
    assert_entry_shape(&doc, "8", "2946617425", "Steam");
    // 根的直接子字段布局（15 个，顺序敏感摘要）
    let root_kids: Vec<String> = doc
        .children_of(&["base_root"])
        .unwrap()
        .iter()
        .map(|&id| doc.node(id).name.clone())
        .collect();
    assert_eq!(
        root_kids,
        vec![
            "version",
            "totalelapsed",
            "inraid",
            "raiddungeon",
            "raid_save",
            "estatename",
            "game_mode",
            "date_time",
            "dd_options_altered",
            "profile_options",
            "applied_ugcs_1_0",
            "persistent_ugcs",
            "presented_dlc",
            "dlc_init",
            "dlc",
        ]
    );
}

// ---- 验收 ②③：remove-last-mod（9→8）与旧工具逐字节一致 ----------------------

#[test]
fn remove_last_mod_matches_old_tool() {
    let bytes = load("profile_7.persist.game.json.dson");
    let mut doc = parse(&bytes).unwrap();
    let kids = applied_children(&doc);
    assert_eq!(kids.len(), 9);

    // 保留 "0".."7"：前 8 个条目原样重建为 NewNode
    let kept: Vec<NewNode> = kids[..8]
        .iter()
        .map(|&id| node_to_new(&doc, id, &APPLIED))
        .collect::<Option<_>>()
        .expect("all kept entries convertible");
    doc.replace_children(&APPLIED, &kept).unwrap();

    let out = doc.write().unwrap();
    let expected = load("expected/profile_7.remove-last-mod.bin");
    assert_eq!(
        out.len(),
        expected.len(),
        "size: got {} want {}",
        out.len(),
        expected.len()
    );
    if out != expected {
        let pos = out
            .iter()
            .zip(&expected)
            .position(|(a, b)| a != b)
            .unwrap_or(out.len().min(expected.len()));
        panic!(
            "remove-last-mod differs from old tool at byte {pos}: got {:02x?} want {:02x?}",
            out.get(pos..pos + 8).unwrap_or(&[]),
            expected.get(pos..pos + 8).unwrap_or(&[])
        );
    }

    // 重解析：8 个条目、末条 "7"、bit31 计数 = 127 - 3（"8"+name+source 消失）
    let doc2 = parse(&out).unwrap();
    assert_eq!(applied_children(&doc2).len(), 8);
    assert_entry_shape(&doc2, "7", "2501350203", "Steam");
    assert_eq!(bit31_true_count(&doc2), 124);
    assert!(doc2.find(&["base_root", "applied_ugcs_1_0", "8"]).is_none());
}

// ---- 验收 ③：add-mod（新增 Steam id 1234567890）与旧工具逐字节一致 ----------

#[test]
fn add_mod_1234567890_matches_old_tool() {
    let bytes = load("profile_7.persist.game.json.dson");
    let mut doc = parse(&bytes).unwrap();
    let kids = applied_children(&doc);

    let mut list: Vec<NewNode> = kids
        .iter()
        .map(|&id| node_to_new(&doc, id, &APPLIED))
        .collect::<Option<_>>()
        .expect("all kept entries convertible");
    list.push(NewNode::new(
        "9",
        NewValue::Object(vec![
            NewNode::new("name", NewValue::Str("1234567890".to_string())),
            NewNode::new("source", NewValue::Str("Steam".to_string())),
        ]),
    ));
    doc.replace_children(&APPLIED, &list).unwrap();

    let out = doc.write().unwrap();
    let expected = load("expected/profile_7.add-mod-1234567890.bin");
    assert_eq!(
        out.len(),
        expected.len(),
        "size: got {} want {}",
        out.len(),
        expected.len()
    );
    if out != expected {
        let pos = out
            .iter()
            .zip(&expected)
            .position(|(a, b)| a != b)
            .unwrap_or(out.len().min(expected.len()));
        panic!(
            "add-mod differs from old tool at byte {pos}: got {:02x?} want {:02x?}",
            out.get(pos..pos + 8).unwrap_or(&[]),
            expected.get(pos..pos + 8).unwrap_or(&[])
        );
    }

    // 重解析：10 个条目；新条目三条目 bit31=1（§2c dynamicMode 启发式）；
    // 总 bit31 = 127 + 3。
    let doc2 = parse(&out).unwrap();
    assert_eq!(applied_children(&doc2).len(), 10);
    assert_entry_shape(&doc2, "9", "1234567890", "Steam");
    assert_eq!(bit31_true_count(&doc2), 130);
    for p in [
        vec!["base_root", "applied_ugcs_1_0", "9"],
        vec!["base_root", "applied_ugcs_1_0", "9", "name"],
        vec!["base_root", "applied_ugcs_1_0", "9", "source"],
    ] {
        let id = doc2.find(&p).expect("fresh entry present");
        assert!(
            fi_bit31(doc2.node(id).source_field_info().unwrap()),
            "{p:?} bit31=1"
        );
    }
}

// ---- §4 清空边界：mods 清空 → applied 字段整体消失 --------------------------

#[test]
fn clear_mods_field_vanishes() {
    let bytes = load("profile_7.persist.game.json.dson");
    let mut doc = parse(&bytes).unwrap();

    // 从 base_root 的直接子字段里去掉 applied_ugcs_1_0（其余原样重建）
    let kept: Vec<NewNode> = doc
        .children_of(&["base_root"])
        .unwrap()
        .iter()
        .filter(|&&id| doc.node(id).name != "applied_ugcs_1_0")
        .map(|&id| node_to_new(&doc, id, &["base_root"]))
        .collect::<Option<_>>()
        .expect("all kept children convertible");
    doc.replace_children(&["base_root"], &kept).unwrap();

    let out = doc.write().unwrap();
    let doc2 = parse(&out).unwrap(); // 变更后的文档必须仍可解析
    assert!(doc2.find(&["base_root", "applied_ugcs_1_0"]).is_none());
    assert!(doc2.find(&["base_root", "persistent_ugcs"]).is_some());
    assert!(doc2.find(&["base_root", "dlc"]).is_some());
    // 同内容再重写：稳定
    assert_eq!(doc2.write().unwrap(), out);
    // 条目数 = 151 - (applied 1 + 9 条目 + 18 叶子)
    assert_eq!(doc2.nodes().len(), 151 - 28);
}

// ---- 空对象语义（remove all children：字段保留但为空） -----------------------

#[test]
fn replace_children_with_empty_slice_keeps_empty_object() {
    let bytes = load("profile_7.persist.game.json.dson");
    let mut doc = parse(&bytes).unwrap();
    doc.replace_children(&APPLIED, &[]).unwrap();
    let out = doc.write().unwrap();
    let doc2 = parse(&out).unwrap();
    assert_eq!(applied_children(&doc2).len(), 0);
    assert!(doc2.find(&["base_root", "applied_ugcs_1_0"]).is_some());
    assert_eq!(doc2.nodes().len(), 151 - 27); // 只去掉 9 对象 + 18 叶子
    assert_eq!(doc2.write().unwrap(), out);
}

// ---- bit31 计数锁（验收 ③ spot） ---------------------------------------------

#[test]
fn bit31_counts_match_expected() {
    for (name, want) in [
        ("profile_2.persist.game.json.dson", 41),
        ("profile_7.persist.game.json.dson", 127),
        ("profile_7.game-modmgr-4716.dson", 26),
        ("expected/profile_7.same-rewrite.bin", 127),
        ("expected/profile_7.remove-last-mod.bin", 124),
        ("expected/profile_7.add-mod-1234567890.bin", 130),
    ] {
        let doc = parse_golden(name);
        assert_eq!(bit31_true_count(&doc), want, "{name}");
    }
}
