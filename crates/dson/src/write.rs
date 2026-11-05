//! 写出器：DsonDoc → DSON 字节（保真重写 + 变更后重排）。
//!
//! 核心规则（dson-bit31-notes §2/§2c，C# Meta2Block.cs 171-195 的忠实移植）：
//! - 先序遍历输出树；数据块 fp 从 0 起，逐条目 emit `名字+NUL`。
//! - 叶子值：Bool 紧随名字（1 字节，不对齐）；Int/Str/TwoBool 先对齐到 4
//!   （垫零）。源节点按 §6 原始字节区搬运：名字重发 + 按新绝对偏移重算
//!   对齐垫 + 值字节从 value_start..region_end 原样复制（不重编码，保留
//!   非规范真值字节）；Fresh 节点按规范编码。
//! - fieldInfo 全部重算：对象的 bits11-30 = 输出先序中的新 meta1 行号
//!   （删改兄弟后下标会移动，不能拷贝源值）。
//! - bit31（§1/§2 writer 算法）：按输出树路径在源文档 map 中查——命中则
//!   搬运源值；未命中（新路径）则取 dynamicMode 状态；四个容器名
//!   （applied_ugcs_1_0/persistent_ugcs/dlc/presented_dlc，规格级常量）
//!   在算完自身 mark 后将 dynamicMode 置真（黏性，先序内不复位）。
//! - meta1 行：parentIndex / meta2EntryIdx(=自身 meta2 先序号) /
//!   numDirectChildren / numAllChildren(=子树大小-1，递归后回填)。
//! - Header：原样 64 字节仅回填布局字段（zeroes 逐字节保留）。

use std::collections::HashSet;

use crate::error::{DsonError, Result};
use crate::hash::dd_hash;
use crate::meta::{Meta1Entry, Meta2Entry, field_info, patch_header_raw};
use crate::model::{DsonDoc, FieldValue, NodeId, NodeOrigin};

/// §2c dynamicMode 容器名（规格级；非游戏领域常量，属于 DSON 写出算法本身）。
const DYNAMIC_CONTAINERS: [&str; 4] = [
    "applied_ugcs_1_0",
    "persistent_ugcs",
    "dlc",
    "presented_dlc",
];

struct WState {
    data: Vec<u8>,
    m1: Vec<Meta1Entry>,
    m2: Vec<Meta2Entry>,
    seen_paths: HashSet<String>,
    dynamic_mode: bool,
}

/// 写出文档字节。
pub(crate) fn write(doc: &DsonDoc) -> Result<Vec<u8>> {
    let mut st = WState {
        data: Vec::with_capacity(doc.data.len()),
        m1: Vec::new(),
        m2: Vec::new(),
        seen_paths: HashSet::with_capacity(doc.nodes.len()),
        dynamic_mode: false,
    };
    let mut path = String::new();
    walk(doc, doc.root(), -1, &mut path, &mut st)?;

    let header = patch_header_raw(&doc.header_raw, st.m1.len(), st.m2.len(), st.data.len())?;

    let mut out = Vec::with_capacity(64 + st.m1.len() * 16 + st.m2.len() * 12 + st.data.len());
    out.extend_from_slice(&header);
    for row in &st.m1 {
        out.extend_from_slice(&row.to_bytes());
    }
    for row in &st.m2 {
        out.extend_from_slice(&row.to_bytes());
    }
    out.extend_from_slice(&st.data);
    Ok(out)
}

/// 先序走一个节点；返回以该节点为根的子树大小（含自身）。
fn walk(
    doc: &DsonDoc,
    id: NodeId,
    parent_m1: i32,
    path: &mut String,
    st: &mut WState,
) -> Result<usize> {
    let n = doc.node(id);

    // 树路径（与解析期同构：斜线连接、含根名）。
    let base = path.len();
    if !path.is_empty() {
        path.push('/');
    }
    path.push_str(&n.name);
    if !st.seen_paths.insert(path.clone()) {
        return Err(DsonError::new(format!(
            "writer: duplicate field path `{path}` (sibling names must be unique)"
        )));
    }

    // bit31（§2 writer 算法）：路径命中源文档 → 搬运；未命中 → dynamicMode
    // 启发式；容器名在算完自身 mark 之后置位（黏性，先序内不复位）。
    let bit31 = match doc.bit31_by_path.get(path) {
        Some(b) => *b,
        None => st.dynamic_mode,
    };
    if DYNAMIC_CONTAINERS.contains(&n.name.as_str()) {
        st.dynamic_mode = true;
    }

    // 该条目的数据区偏移（= 名字起始）；meta2 行按先序追加。
    let offset = i32::try_from(st.data.len())
        .map_err(|_| DsonError::new("writer: data block too large for i32 offsets"))?;
    let name_hash = dd_hash(&n.name);
    let name_bytes = n.name.as_bytes();
    let name_len_with_nul = name_bytes.len() + 1;

    if n.is_object {
        let m1_idx = u32::try_from(st.m1.len())
            .map_err(|_| DsonError::new("writer: too many objects for fieldInfo"))?;
        let fi = field_info(true, name_len_with_nul, m1_idx, bit31)
            .ok_or_else(|| DsonError::new(format!("writer: field name `{}` too long", n.name)))?;
        emit_name(st, name_bytes);
        st.m2.push(Meta2Entry {
            name_hash,
            offset,
            field_info: fi,
        });
        let m2_idx = st.m2.len() - 1;
        let direct = i32::try_from(n.children.len())
            .map_err(|_| DsonError::new("writer: too many direct children"))?;
        let row_idx = st.m1.len();
        st.m1.push(Meta1Entry {
            parent_index: parent_m1,
            meta2_entry_idx: i32::try_from(m2_idx)
                .map_err(|_| DsonError::new("writer: too many entries"))?,
            num_direct_children: direct,
            num_all_children: 0, // 子树走完后回填
        });
        let mut subtree = 1usize;
        for &c in &n.children {
            subtree += walk(doc, c, row_idx as i32, path, st)?;
        }
        let all = i32::try_from(subtree - 1)
            .map_err(|_| DsonError::new("writer: too many descendants"))?;
        st.m1[row_idx].num_all_children = all;
        path.truncate(base);
        return Ok(subtree);
    }

    // 叶子
    let fi = field_info(false, name_len_with_nul, 0, bit31)
        .ok_or_else(|| DsonError::new(format!("writer: field name `{}` too long", n.name)))?;
    emit_name(st, name_bytes);
    st.m2.push(Meta2Entry {
        name_hash,
        offset,
        field_info: fi,
    });
    match &n.origin {
        NodeOrigin::Source {
            region,
            value_start,
            ..
        } => {
            // §6：原始字节区搬运。Bool 不对齐；其余按新偏移重算对齐垫，
            // 值字节从 value_start..region_end 原样复制（不重编码）。
            let is_bool = matches!(n.value, Some(FieldValue::Bool(_)));
            if !is_bool {
                pad_to_align4(st);
            }
            let from = region.start + value_start;
            let bytes = doc
                .data
                .get(from..region.end)
                .ok_or_else(|| DsonError::new("writer bug: source region out of data block"))?;
            st.data.extend_from_slice(bytes);
        }
        NodeOrigin::Fresh => {
            let v = n
                .value
                .as_ref()
                .ok_or_else(|| DsonError::new(format!("writer: leaf `{}` has no value", n.name)))?;
            match v {
                FieldValue::Bool(b) => {
                    st.data.push(u8::from(*b));
                }
                FieldValue::Int(i) => {
                    pad_to_align4(st);
                    st.data.extend_from_slice(&i.to_le_bytes());
                }
                FieldValue::Str(s) => {
                    pad_to_align4(st);
                    let len = i32::try_from(s.len())
                        .ok()
                        .and_then(|l| l.checked_add(1))
                        .ok_or_else(|| DsonError::new("writer: string too long"))?;
                    st.data.extend_from_slice(&len.to_le_bytes());
                    st.data.extend_from_slice(s.as_bytes());
                    st.data.push(0);
                }
                FieldValue::TwoBool(a, b) => {
                    pad_to_align4(st);
                    st.data.extend_from_slice(&i32::from(*a).to_le_bytes());
                    st.data.extend_from_slice(&i32::from(*b).to_le_bytes());
                }
            }
        }
    }
    path.truncate(base);
    Ok(1)
}

fn emit_name(st: &mut WState, name: &[u8]) {
    st.data.extend_from_slice(name);
    st.data.push(0);
}

/// 把数据块垫到 4 字节对齐（垫零）。
fn pad_to_align4(st: &mut WState) {
    let aligned = (st.data.len() + 3) & !3;
    st.data.resize(aligned, 0);
}
