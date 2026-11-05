//! 编解码单元测试：手搓最小 DSON（独立编码器）→ parse → 访问器 → write
//! 逐字节回环；坏输入 → 干净 Err（验收 ⑤）；writer 拒绝路径。
//!
//! 手搓编码器故意不复用库代码：它按规格直接拼字节，是对 parse/write
//! 对偶性的独立交叉检查（金样本仍是最终真值）。

use dson::model::{DsonDoc, NewNode, NewValue};
use dson::parse;

// ---- 独立测试编码器 ---------------------------------------------------------

#[derive(Clone)]
enum TV {
    B(bool),
    I(i32),
    S(String),
    T2(bool, bool),
    O(Vec<TN>),
}

#[derive(Clone)]
struct TN {
    name: String,
    v: TV,
}

fn tn(name: &str, v: TV) -> TN {
    TN {
        name: name.into(),
        v,
    }
}

/// 按规格（§2 布局 + §2c 写出规则）独立编码一棵树。
fn encode(root: &TN) -> Vec<u8> {
    struct M1 {
        parent: i32,
        m2_idx: i32,
        direct: i32,
    }
    let mut m1: Vec<M1> = vec![];
    let mut m2: Vec<(i32, i32, u32)> = vec![]; // hash, off, fi
    let mut data: Vec<u8> = vec![];
    let mut all_stack: Vec<Vec<usize>> = vec![vec![]];

    fn hash(s: &str) -> i32 {
        let mut h: i32 = 0;
        for b in s.as_bytes() {
            h = h.wrapping_mul(53).wrapping_add(i32::from(*b));
        }
        h
    }
    fn fi(obj: bool, nl: usize, m1_idx: usize) -> u32 {
        u32::from(obj) | ((nl as u32) << 2) | ((m1_idx as u32) << 11)
    }
    // bit31 测试编码器一律 0（bit31 语义由金样本夹具覆盖）
    fn rec(
        n: &TN,
        parent_m1: i32,
        m1: &mut Vec<M1>,
        m2: &mut Vec<(i32, i32, u32)>,
        data: &mut Vec<u8>,
        all_stack: &mut Vec<Vec<usize>>,
    ) -> usize {
        let off = data.len() as i32;
        let nb = n.name.as_bytes();
        match &n.v {
            TV::O(kids) => {
                let my_m1 = m1.len();
                data.extend_from_slice(nb);
                data.push(0);
                m2.push((hash(&n.name), off, fi(true, nb.len() + 1, my_m1)));
                m1.push(M1 {
                    parent: parent_m1,
                    m2_idx: (m2.len() - 1) as i32,
                    direct: kids.len() as i32,
                });
                all_stack.push(vec![]);
                let mut total = 0usize;
                for k in kids {
                    total += rec(k, my_m1 as i32, m1, m2, data, all_stack);
                }
                let mine = all_stack.pop().unwrap();
                let _ = mine;
                total
            }
            leaf => {
                data.extend_from_slice(nb);
                data.push(0);
                m2.push((hash(&n.name), off, fi(false, nb.len() + 1, 0)));
                match leaf {
                    TV::B(b) => data.push(u8::from(*b)),
                    TV::I(i) => {
                        while !data.len().is_multiple_of(4) {
                            data.push(0);
                        }
                        data.extend_from_slice(&i.to_le_bytes());
                    }
                    TV::S(s) => {
                        while !data.len().is_multiple_of(4) {
                            data.push(0);
                        }
                        data.extend_from_slice(&((s.len() + 1) as i32).to_le_bytes());
                        data.extend_from_slice(s.as_bytes());
                        data.push(0);
                    }
                    TV::T2(a, b) => {
                        while !data.len().is_multiple_of(4) {
                            data.push(0);
                        }
                        data.extend_from_slice(&i32::from(*a).to_le_bytes());
                        data.extend_from_slice(&i32::from(*b).to_le_bytes());
                    }
                    TV::O(_) => unreachable!(),
                }
                1
            }
        }
    }
    let _ = rec(root, -1, &mut m1, &mut m2, &mut data, &mut all_stack);
    let _ = all_stack.pop();

    let mut out = vec![0x01, 0xB1, 0x00, 0x00];
    out.extend_from_slice(&[0x11, 0x22, 0x33, 0x44]); // revision（任意）
    out.extend_from_slice(&64i32.to_le_bytes());
    out.extend_from_slice(&[0; 4]); // zeroes
    out.extend_from_slice(&((m1.len() * 16) as i32).to_le_bytes());
    out.extend_from_slice(&(m1.len() as i32).to_le_bytes());
    out.extend_from_slice(&64i32.to_le_bytes()); // meta1Offset
    out.extend_from_slice(&[0; 8]); // zeroes2
    out.extend_from_slice(&[0; 8]); // zeroes3
    out.extend_from_slice(&(m2.len() as i32).to_le_bytes());
    out.extend_from_slice(&((64 + m1.len() * 16) as i32).to_le_bytes());
    out.extend_from_slice(&[0; 4]); // zeroes4
    out.extend_from_slice(&(data.len() as i32).to_le_bytes());
    out.extend_from_slice(&((64 + m1.len() * 16 + m2.len() * 12) as i32).to_le_bytes());
    for r in &m1 {
        out.extend_from_slice(&r.parent.to_le_bytes());
        out.extend_from_slice(&r.m2_idx.to_le_bytes());
        out.extend_from_slice(&r.direct.to_le_bytes());
        out.extend_from_slice(&0i32.to_le_bytes()); // numAll：占位，回填
    }
    for (h, o, f) in &m2 {
        out.extend_from_slice(&h.to_le_bytes());
        out.extend_from_slice(&o.to_le_bytes());
        out.extend_from_slice(&f.to_le_bytes());
    }
    out.extend_from_slice(&data);
    out
}

/// 手工 patch 某个 meta1 行的 numAllChildren（encode 后按场景调用）。
fn patch_all(bytes: &mut [u8], m1_idx: usize, all: i32) {
    let off = 64 + m1_idx * 16 + 12;
    bytes[off..off + 4].copy_from_slice(&all.to_le_bytes());
}

fn sample_tree() -> TN {
    tn(
        "root",
        TV::O(vec![
            tn("flag", TV::B(true)),
            tn("count", TV::I(-5)),
            tn("label", TV::S("héllo".into())),
            tn("pair", TV::T2(true, false)),
            tn(
                "sub",
                TV::O(vec![tn("inner", TV::S("x".into())), tn("z", TV::B(false))]),
            ),
            tn("empty_str", TV::S(String::new())),
        ]),
    )
}

fn build_with_all(tree: &TN, alls: &[(usize, i32)]) -> Vec<u8> {
    let mut b = encode(tree);
    for (i, a) in alls {
        patch_all(&mut b, *i, *a);
    }
    b
}

// sample_tree 的 meta1 行序（先序）：0 root, 1 sub → root.all=8, sub.all=2
fn sample_bytes() -> Vec<u8> {
    build_with_all(&sample_tree(), &[(0, 8), (1, 2)])
}

// ---- 回环：手搓字节 → parse → 访问器 → write 逐字节一致 ----------------------

#[test]
fn handcrafted_roundtrip_and_accessors() {
    let bytes = sample_bytes();
    let doc = parse(&bytes).expect("handcrafted parses");
    assert_eq!(doc.root_name(), "root");
    assert_eq!(doc.bool_value(&["root", "flag"]), Some(true));
    assert_eq!(doc.int_value(&["root", "count"]), Some(-5));
    assert_eq!(doc.str_value(&["root", "label"]), Some("héllo"));
    assert_eq!(doc.twobool_value(&["root", "pair"]), Some((true, false)));
    assert_eq!(doc.str_value(&["root", "sub", "inner"]), Some("x"));
    assert_eq!(doc.bool_value(&["root", "sub", "z"]), Some(false));
    assert_eq!(doc.str_value(&["root", "empty_str"]), Some(""));
    assert_eq!(doc.write().unwrap(), bytes, "write(parse(x)) == x");
}

#[test]
fn mutate_fresh_then_roundtrip() {
    let bytes = sample_bytes();
    let mut doc = parse(&bytes).unwrap();
    doc.replace_children(
        &["root", "sub"],
        &[
            NewNode::new("inner", NewValue::Str("yy".into())),
            NewNode::new("extra", NewValue::Int(7)),
        ],
    )
    .unwrap();
    let out = doc.write().unwrap();
    let doc2 = parse(&out).unwrap();
    assert_eq!(doc2.str_value(&["root", "sub", "inner"]), Some("yy"));
    assert_eq!(doc2.int_value(&["root", "sub", "extra"]), Some(7));
    assert!(doc2.bool_value(&["root", "sub", "z"]).is_none());
    assert_eq!(doc2.write().unwrap(), out);
}

#[test]
fn non_four_aligned_start_still_aligns() {
    // 根名长度 3（非 4 对齐）迫使叶子值出现非零垫
    let tree = tn(
        "abc",
        TV::O(vec![
            tn("i", TV::I(9)),
            tn("s", TV::S("q".into())),
            tn("b", TV::B(true)),
        ]),
    );
    // meta1：0 abc → 后代 = i,s,b 三个 → all=3
    let bytes = build_with_all(&tree, &[(0, 3)]);
    let doc = parse(&bytes).unwrap();
    assert_eq!(doc.int_value(&["abc", "i"]), Some(9));
    assert_eq!(doc.str_value(&["abc", "s"]), Some("q"));
    assert_eq!(doc.bool_value(&["abc", "b"]), Some(true));
    assert_eq!(doc.write().unwrap(), bytes);
}

// ---- 验收 ⑤：坏输入 → 干净 Err（绝不 panic / 静默跳过） -----------------------

fn err_of(bytes: &[u8]) -> String {
    match parse(bytes) {
        Ok(_) => panic!("expected Err"),
        Err(e) => e.message().to_string(),
    }
}

#[test]
fn bad_magic_rejected() {
    let mut b = sample_bytes();
    b[0] = 0xFF;
    let msg = err_of(&b);
    assert!(msg.contains("magic"), "{msg}");
}

#[test]
fn truncated_rejected() {
    let b = sample_bytes();
    let msg = err_of(&b[..b.len() - 4]);
    assert!(msg.contains("truncated") || msg.contains("data"), "{msg}");
}

#[test]
fn non_monotonic_offsets_rejected() {
    let mut b = sample_bytes();
    // 把 meta2[1].offset 抬高到超过后续条目（offset 字段位于 meta2 区 + 4）
    let n1 = i32::from_le_bytes(b[20..24].try_into().unwrap()) as usize;
    let m2o = 64 + n1 * 16;
    let off = m2o + 12 + 4;
    let v = i32::from_le_bytes(b[off..off + 4].try_into().unwrap());
    b[off..off + 4].copy_from_slice(&(v + 1000).to_le_bytes());
    let msg = err_of(&b);
    assert!(
        msg.contains("monotonic") || msg.contains("outside"),
        "{msg}"
    );
}

#[test]
fn hash_mismatch_rejected() {
    let mut b = sample_bytes();
    let n1 = i32::from_le_bytes(b[20..24].try_into().unwrap()) as usize;
    let m2o = 64 + n1 * 16;
    b[m2o] ^= 0xFF; // 破坏 meta2[0].nameHash
    let msg = err_of(&b);
    assert!(msg.contains("nameHash"), "{msg}");
}

#[test]
fn unsupported_leaf_shape_rejected() {
    // 构造 remain==2 且 v≠remain 的叶子区域：手工把 count 的区域拉长 2 字节
    // 手段：把后续所有 meta2 偏移 +2，并在 count 值后插 2 字节。
    let mut b = sample_bytes();
    let n1 = i32::from_le_bytes(b[20..24].try_into().unwrap()) as usize;
    let n2 = i32::from_le_bytes(b[44..48].try_into().unwrap()) as usize;
    let m2o = 64 + n1 * 16;
    let data_off = m2o + n2 * 12;
    // count 是 meta2[2]，区域从其 offset 到 meta2[3].offset
    let c_off = i32::from_le_bytes(b[m2o + 24 + 4..m2o + 24 + 8].try_into().unwrap()) as usize;
    // 在 c_off 之后找 count 值末尾（i32 边界）；直接在数据区 count 值后插 2 字节
    // count 区域: name 6B + (pad 2) + 4B 值 = 12B；值末尾 = c_off + 12
    let insert_at = data_off + c_off + 12;
    for i in 0..n2 {
        let f = m2o + i * 12 + 4;
        let o = i32::from_le_bytes(b[f..f + 4].try_into().unwrap());
        if (o as usize) > c_off {
            b[f..f + 4].copy_from_slice(&(o + 2).to_le_bytes());
        }
    }
    let dl = i32::from_le_bytes(b[56..60].try_into().unwrap());
    b[56..60].copy_from_slice(&(dl + 2).to_le_bytes());
    let mut out = b[..insert_at].to_vec();
    out.extend_from_slice(&[0xAA, 0xBB]);
    out.extend_from_slice(&b[insert_at..]);
    let msg = err_of(&out);
    assert!(msg.contains("unsupported"), "{msg}");
}

#[test]
fn duplicate_sibling_names_rejected() {
    // 手搓两个同名兄弟（测试编码器允许，规格不允许）
    let tree = tn("r", TV::O(vec![tn("a", TV::I(1)), tn("a", TV::I(2))]));
    let bytes = build_with_all(&tree, &[(0, 2)]);
    let msg = err_of(&bytes);
    assert!(msg.contains("duplicate"), "{msg}");
}

#[test]
fn trailing_bytes_rejected() {
    let mut b = sample_bytes();
    b.push(0);
    let msg = err_of(&b);
    assert!(
        msg.contains("trailing") || msg.contains("truncated"),
        "{msg}"
    );
}

#[test]
fn leaf_with_meta1_index_rejected() {
    let mut b = sample_bytes();
    let n1 = i32::from_le_bytes(b[20..24].try_into().unwrap()) as usize;
    let m2o = 64 + n1 * 16;
    // meta2[1]（flag 叶子）fieldInfo bits11-30 置 5
    let f = m2o + 12 + 8;
    let fi = u32::from_le_bytes(b[f..f + 4].try_into().unwrap());
    b[f..f + 4].copy_from_slice(&(fi | (5u32 << 11)).to_le_bytes());
    let msg = err_of(&b);
    assert!(msg.contains("meta1 idx"), "{msg}");
}

#[test]
fn wrong_num_all_children_rejected() {
    // numAllChildren 声明 9（实际 3）→ 解析期拒绝
    let tree = tn(
        "abc",
        TV::O(vec![
            tn("i", TV::I(9)),
            tn("s", TV::S("q".into())),
            tn("b", TV::B(true)),
        ]),
    );
    let bytes = build_with_all(&tree, &[(0, 9)]);
    let msg = err_of(&bytes);
    assert!(msg.contains("numAllChildren"), "{msg}");
}

#[test]
fn object_region_slack_rejected() {
    // 对象区域被拉长 1 字节（改下一个条目 offset），应报 object slack
    let mut b = sample_bytes();
    let n1 = i32::from_le_bytes(b[20..24].try_into().unwrap()) as usize;
    let n2 = i32::from_le_bytes(b[44..48].try_into().unwrap()) as usize;
    let m2o = 64 + n1 * 16;
    let data_off = m2o + n2 * 12;
    let _ = data_off;
    // sub 对象是 meta2[5]，区域 = [off, meta2[6].offset)；把 meta2[6] 抬 1 字节
    // 需要真实字节空间：在数据区 sub 名字后插入 1 字节并整体平移后续偏移
    let s_off = i32::from_le_bytes(b[m2o + 60 + 4..m2o + 60 + 8].try_into().unwrap()) as usize;
    for i in 0..n2 {
        let f = m2o + i * 12 + 4;
        let o = i32::from_le_bytes(b[f..f + 4].try_into().unwrap());
        if (o as usize) > s_off {
            b[f..f + 4].copy_from_slice(&(o + 1).to_le_bytes());
        }
    }
    let dl = i32::from_le_bytes(b[56..60].try_into().unwrap());
    b[56..60].copy_from_slice(&(dl + 1).to_le_bytes());
    let mut out = b[..data_off + s_off + 4].to_vec(); // "sub\0" 后
    out.push(0);
    out.extend_from_slice(&b[data_off + s_off + 4..]);
    let msg = err_of(&out);
    assert!(
        msg.contains("slack") || msg.contains("object region"),
        "{msg}"
    );
}

// ---- writer 拒绝 ------------------------------------------------------------

#[test]
fn writer_rejects_too_long_name() {
    let bytes = sample_bytes();
    let mut doc = parse(&bytes).unwrap();
    let long = "x".repeat(600);
    doc.replace_children(&["root"], &[NewNode::new(long, NewValue::Int(1))])
        .unwrap();
    let err = doc.write().unwrap_err();
    assert!(err.message().contains("too long"), "{}", err.message());
}

#[test]
fn writer_rejects_duplicate_fresh_paths() {
    let bytes = sample_bytes();
    let mut doc = parse(&bytes).unwrap();
    doc.replace_children(
        &["root"],
        &[
            NewNode::new("dup", NewValue::Int(1)),
            NewNode::new("dup", NewValue::Int(2)),
        ],
    )
    .unwrap();
    let err = doc.write().unwrap_err();
    assert!(err.message().contains("duplicate"), "{}", err.message());
}

// ---- 变更 API 语义 -----------------------------------------------------------

#[test]
fn replace_children_on_leaf_errors() {
    let bytes = sample_bytes();
    let mut doc = parse(&bytes).unwrap();
    let err = doc.replace_children(&["root", "flag"], &[]).unwrap_err();
    assert!(err.message().contains("not an object"), "{}", err.message());
}

#[test]
fn find_errors_report_path() {
    let bytes = sample_bytes();
    let doc = parse(&bytes).unwrap();
    assert!(doc.find(&[]).is_none());
    assert!(doc.find(&["nope"]).is_none());
    assert!(doc.find(&["root", "nope"]).is_none());
    assert!(doc.field_value(&["root", "flag", "deeper"]).is_none());
    // 叶子当对象用 → None
    assert!(doc.children_of(&["root", "flag"]).is_none());
    // 类型不匹配的访问器 → None
    assert!(doc.str_value(&["root", "flag"]).is_none());
    assert!(doc.int_value(&["root", "label"]).is_none());
}

#[test]
fn parse_reports_context_on_garbage() {
    // 纯随机短输入不 panic，给出 Err
    let msg = err_of(b"nonsense");
    assert!(!msg.is_empty());
    let msg2 = err_of(&[0u8; 64]);
    assert!(msg2.contains("magic") || msg2.contains("header"), "{msg2}");
}

#[test]
fn cloned_doc_writes_identically() {
    let bytes = sample_bytes();
    let doc: DsonDoc = parse(&bytes).unwrap();
    let doc2 = doc.clone();
    assert_eq!(doc.write().unwrap(), doc2.write().unwrap());
}
