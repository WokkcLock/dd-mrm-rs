//! E1c 实验：变尺寸写入——把 estatename 换成不同长度的新名后原子写回，
//! 验证游戏 refresh 读链在文件大小变化时拿到的是新 size 还是缓存旧 size。
//! 复刻 apply.rs sync_mods 的 Fresh 全量重建模式（与生产 apply_offline 同路径）。
//! 用法: e1c_rename <persist.game.json> <new_name>

use std::path::Path;

use dson::{DsonDoc, FieldValue, NewNode, NewValue, NodeId, parse};
use save::write_persist_atomic;

/// apply.rs 同名函数复刻：已解析子树 Fresh 化（写出端按规范编码）。
fn subtree_to_new_node(doc: &DsonDoc, id: NodeId) -> Result<NewNode, String> {
    let n = doc.node(id);
    let value = if n.is_object {
        let mut children = Vec::new();
        for &c in &n.children {
            children.push(subtree_to_new_node(doc, c)?);
        }
        NewValue::Object(children)
    } else {
        match n.value.as_ref() {
            Some(FieldValue::Str(v)) => NewValue::Str(v.clone()),
            Some(FieldValue::Bool(v)) => NewValue::Bool(*v),
            Some(FieldValue::Int(v)) => NewValue::Int(*v),
            Some(FieldValue::TwoBool(a, b)) => NewValue::TwoBool(*a, *b),
            None => return Err(format!("叶子节点 {} 无值", n.name)),
        }
    };
    Ok(NewNode::new(n.name.clone(), value))
}

fn die(msg: String) -> ! {
    eprintln!("error: {msg}");
    std::process::exit(2);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() != 3 {
        die("usage: e1c_rename <persist.game.json> <new_name>".into());
    }
    let path = args[1].clone();
    let new_name = args[2].clone();

    let bytes = std::fs::read(&path).unwrap_or_else(|e| die(format!("read {path}: {e}")));
    println!("old_size={}", bytes.len());

    let mut doc = parse(&bytes).unwrap_or_else(|e| die(format!("parse: {e:?}")));
    let root = doc.root_name().to_string();

    let root_path = [root.as_str()];
    let kids: Vec<NodeId> = doc
        .children_of(&root_path)
        .map(|cs| cs.to_vec())
        .unwrap_or_default();
    let mut old_name = String::new();
    for &id in &kids {
        let n = doc.node(id);
        if n.name == "estatename" {
            match n.value.as_ref() {
                Some(FieldValue::Str(v)) => old_name = v.clone(),
                _ => die("estatename 不是 Str 叶子".into()),
            }
        }
    }
    if old_name.is_empty() {
        die("base_root 下没找到 estatename".into());
    }
    println!("old_estatename={old_name:?} len={}", old_name.len());

    let rebuilt: Vec<NewNode> = kids
        .iter()
        .map(|&id| {
            if doc.node(id).name == "estatename" {
                NewNode::new("estatename", NewValue::Str(new_name.clone()))
            } else {
                subtree_to_new_node(&doc, id).unwrap_or_else(|e| die(e))
            }
        })
        .collect();
    doc.replace_children(&root_path, &rebuilt)
        .unwrap_or_else(|e| die(format!("replace_children: {e:?}")));

    let out = doc.write().unwrap_or_else(|e| die(format!("write: {e:?}")));
    println!("new_size={}", out.len());

    match write_persist_atomic(Path::new(&path), &out) {
        Ok(()) => println!("atomic_write=ok"),
        Err(e) => {
            eprintln!("atomic rejected ({e:?}) — fallback plain write");
            std::fs::write(&path, &out).unwrap_or_else(|e2| die(format!("fs::write: {e2}")));
            println!("plain_write=ok");
        }
    }

    let back = std::fs::read(&path).unwrap_or_else(|e| die(format!("readback: {e}")));
    let doc2 = parse(&back).unwrap_or_else(|e| die(format!("readback parse: {e:?}")));
    let kids2: Vec<NodeId> = doc2
        .children_of(&[doc2.root_name()])
        .map(|cs| cs.to_vec())
        .unwrap_or_default();
    for &id in &kids2 {
        let n = doc2.node(id);
        if n.name == "estatename" {
            if let Some(FieldValue::Str(v)) = n.value.as_ref() {
                println!("readback_estatename={v:?} readback_size={}", back.len());
            }
        }
    }
}
