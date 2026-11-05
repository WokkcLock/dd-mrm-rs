//! 解析器：文件字节 → DsonDoc（arena + 原始字节区）。
//!
//! 校验（dson-bit31-notes §6 契约）：meta2 offset 单调不减且在数据区内；
//! 首条目为对象且 meta1 下标 0；对象消耗的 meta1 行数 == numMeta1；
//! 每条 fieldInfo 与实际字节严格吻合；meta1 行的 numDirectChildren /
//! numAllChildren 与重建树完全一致；类型不在支持集 → Err（§2b，验收 ⑤）。
//! 全程 checked 运算，坏输入一律 Err。

use std::collections::HashMap;

use crate::error::{DsonError, Result};
use crate::hash::dd_hash;
use crate::meta::{
    Header, META1_ENTRY_SIZE, META2_ENTRY_SIZE, Meta1Entry, Meta2Entry, fi_bit31, fi_is_object,
    fi_meta1_idx, fi_name_len,
};
use crate::model::{DsonDoc, FieldNode, FieldValue, NodeId, NodeOrigin};

/// 解析完整 DSON 字节。
pub fn parse(bytes: &[u8]) -> Result<DsonDoc> {
    let header = Header::read(bytes)?;
    let data = bytes
        .get(header.data_offset()..)
        .ok_or_else(|| DsonError::new("parser bug: data offset beyond file"))?;
    if data.len() != header.data_length() {
        return Err(DsonError::new(format!(
            "data region length {} != header dataLength {}",
            data.len(),
            header.data_length()
        )));
    }

    // meta1 / meta2 原始条目
    let mut m1 = Vec::with_capacity(header.num_meta1());
    for i in 0..header.num_meta1() {
        let off = crate::meta::HEADER_SIZE + i * META1_ENTRY_SIZE;
        let b = bytes
            .get(off..off + META1_ENTRY_SIZE)
            .ok_or_else(|| DsonError::new(format!("meta1 entry {i} at byte {off} is truncated")))?;
        m1.push(Meta1Entry::read(b)?);
    }
    let mut m2 = Vec::with_capacity(header.num_meta2());
    for i in 0..header.num_meta2() {
        let off = header.meta2_offset() + i * META2_ENTRY_SIZE;
        let b = bytes
            .get(off..off + META2_ENTRY_SIZE)
            .ok_or_else(|| DsonError::new(format!("meta2 entry {i} at byte {off} is truncated")))?;
        m2.push(Meta2Entry::read(b)?);
    }

    // §6：offset 单调不减，且全部落在数据区内。
    for w in m2.windows(2) {
        if w[0].offset > w[1].offset {
            return Err(DsonError::new(format!(
                "meta2 offsets not monotonic: {} then {}",
                w[0].offset, w[1].offset
            )));
        }
    }
    for (i, e) in m2.iter().enumerate() {
        if e.offset < 0 || e.offset as usize > data.len() {
            return Err(DsonError::new(format!(
                "meta2 entry {i}: offset {} outside data block (0..{})",
                e.offset,
                data.len()
            )));
        }
    }

    // 首条目必须是根对象（fieldInfo: obj=1, meta1 idx 0）。
    if !fi_is_object(m2[0].field_info) {
        return Err(DsonError::new(format!(
            "first meta2 entry must be an object (fieldInfo=0x{:08X})",
            m2[0].field_info
        )));
    }
    if fi_meta1_idx(m2[0].field_info) != 0 {
        return Err(DsonError::new(format!(
            "first meta2 entry must reference meta1 index 0 (got {})",
            fi_meta1_idx(m2[0].field_info)
        )));
    }

    // 显式栈驱动先序重建。meta2 先序 == 树先序 == 对象的 meta1 先序。
    struct Frame {
        m1_idx: usize,     // 该对象对应的 meta1 行号
        m2_idx: usize,     // 该对象在 meta2 中的先序下标（报错用）
        remaining: i64,    // 该对象 numDirectChildren 的剩余配额
        declared_all: i64, // meta1 行声明的 numAllChildren
        acc_all: i64,      // 实际累计的后代数（不含自身）
        node_id: NodeId,
    }

    let mut nodes: Vec<FieldNode> = Vec::with_capacity(m2.len());
    let mut bit31_by_path: HashMap<String, bool> = HashMap::with_capacity(m2.len());
    let mut stack: Vec<Frame> = Vec::new();
    let mut m1_cursor = 0usize;
    let mut seen_paths: HashMap<String, usize> = HashMap::with_capacity(m2.len());

    for idx in 0..m2.len() {
        let entry = m2[idx];
        let fi = entry.field_info;
        let region_start = entry.offset as usize;
        let region_end = if idx + 1 < m2.len() {
            m2[idx + 1].offset as usize
        } else {
            data.len()
        };
        // 单调性已验证，此处只需防御性复查。
        let region_len = region_end
            .checked_sub(region_start)
            .ok_or_else(|| DsonError::new(format!("meta2 entry {idx}: negative region")))?;
        let region = &data[region_start..region_end];

        // 名字：region 内 NUL 结尾，长度必须与 fieldInfo bits2-10 一致。
        let nl = fi_name_len(fi) as usize; // 含 NUL
        if nl == 0 || nl > region_len {
            return Err(DsonError::new(format!(
                "meta2 entry {idx}: nameLen {nl} exceeds region [{region_start},{region_end})"
            )));
        }
        if region[nl - 1] != 0 {
            return Err(DsonError::new(format!(
                "meta2 entry {idx}: name is not NUL-terminated within nameLen bytes"
            )));
        }
        let name = String::from_utf8(region[..nl - 1].to_vec()).map_err(|_| {
            DsonError::new(format!("meta2 entry {idx}: field name is not valid UTF-8"))
        })?;
        if name.len() + 1 != nl {
            return Err(DsonError::new(format!(
                "meta2 entry {idx} `{name}`: nameLen {nl} != utf8 bytes+1 ({})",
                name.len() + 1
            )));
        }

        // 哈希校验
        let expect_hash = dd_hash(&name);
        if entry.name_hash != expect_hash {
            return Err(DsonError::new(format!(
                "meta2 entry {idx} `{name}`: nameHash {} != ddHash {expect_hash}",
                entry.name_hash
            )));
        }

        // 树路径：bit31 映射键 & 重复名检测（同名兄弟 = 路径歧义，拒绝）。
        let parent_path = stack
            .last()
            .map(|f| nodes[f.node_id as usize].path_key.clone())
            .unwrap_or_default();
        let path = if parent_path.is_empty() {
            name.clone()
        } else {
            format!("{parent_path}/{name}")
        };
        if seen_paths.insert(path.clone(), idx).is_some() {
            return Err(DsonError::new(format!(
                "meta2 entry {idx}: duplicate field path `{path}`"
            )));
        }

        let node_id = nodes.len() as NodeId;
        let is_object = fi_is_object(fi);
        let (value, value_start): (Option<FieldValue>, usize);
        if is_object {
            // 对象：区间恰为 名字+NUL（无 slack）。
            if region_len != nl {
                return Err(DsonError::new(format!(
                    "meta2 entry {idx} `{name}`: object region {region_len} bytes != nameLen {nl} (slack not supported)"
                )));
            }
            if m1_cursor >= m1.len() {
                return Err(DsonError::new(format!(
                    "meta2 entry {idx} `{name}`: object count exceeds numMeta1Entries {}",
                    m1.len()
                )));
            }
            let row = m1[m1_cursor];
            let my_m1 = m1_cursor;
            m1_cursor += 1;
            if row.meta2_entry_idx != idx as i32 {
                return Err(DsonError::new(format!(
                    "meta2 entry {idx} `{name}`: meta1[{my_m1}].meta2EntryIdx={} != {idx}",
                    row.meta2_entry_idx
                )));
            }
            if fi_meta1_idx(fi) as usize != my_m1 {
                return Err(DsonError::new(format!(
                    "meta2 entry {idx} `{name}`: fieldInfo meta1 idx {} != {my_m1}",
                    fi_meta1_idx(fi)
                )));
            }
            if let Some(parent) = stack.last() {
                if row.parent_index != parent.m1_idx as i32 {
                    return Err(DsonError::new(format!(
                        "meta2 entry {idx} `{name}`: meta1[{my_m1}].parentIndex={} != parent object meta1 idx {}",
                        row.parent_index, parent.m1_idx
                    )));
                }
            } else if row.parent_index != -1 {
                return Err(DsonError::new(format!(
                    "meta2 entry {idx} `{name}`: root meta1 parentIndex={} != -1",
                    row.parent_index
                )));
            }
            value = None;
            value_start = region_len;
            nodes.push(FieldNode {
                path_key: path.clone(),
                name,
                is_object: true,
                children: Vec::new(),
                value,
                origin: NodeOrigin::Source {
                    field_info: fi,
                    region: region_start..region_end,
                    value_start,
                },
            });
            if let Some(parent) = stack.last_mut() {
                nodes[parent.node_id as usize].children.push(node_id);
                parent.remaining -= 1;
            }
            let direct = row.num_direct_children;
            if direct < 0 {
                return Err(DsonError::new(format!(
                    "meta2 entry {idx}: meta1 numDirectChildren {direct} < 0"
                )));
            }
            if row.num_all_children < 0 {
                return Err(DsonError::new(format!(
                    "meta2 entry {idx} `{}`: meta1 numAllChildren {} < 0",
                    nodes[node_id as usize].name, row.num_all_children
                )));
            }
            stack.push(Frame {
                m1_idx: my_m1,
                m2_idx: idx,
                remaining: direct as i64,
                declared_all: i64::from(row.num_all_children),
                acc_all: 0,
                node_id,
            });
        } else {
            // 叶子：bits11-30 恒 0（金样本已验证）。
            if fi_meta1_idx(fi) != 0 {
                return Err(DsonError::new(format!(
                    "meta2 entry {idx} `{name}`: leaf fieldInfo meta1 idx {} != 0",
                    fi_meta1_idx(fi)
                )));
            }
            let parent = stack.last_mut().ok_or_else(|| {
                DsonError::new(format!(
                    "meta2 entry {idx} `{name}`: leaf has no parent object"
                ))
            })?;
            if parent.remaining == 0 {
                return Err(DsonError::new(format!(
                    "meta2 entry {idx} `{name}`: parent numDirectChildren exhausted (extra field)"
                )));
            }
            let (v, vs) = infer_leaf(&name, idx, region, region_start, nl)?;
            value = Some(v);
            value_start = vs;
            nodes.push(FieldNode {
                path_key: path.clone(),
                name,
                is_object: false,
                children: Vec::new(),
                value,
                origin: NodeOrigin::Source {
                    field_info: fi,
                    region: region_start..region_end,
                    value_start,
                },
            });
            nodes[parent.node_id as usize].children.push(node_id);
            parent.remaining -= 1;
            parent.acc_all += 1;
        }
        bit31_by_path.insert(path, fi_bit31(fi));

        // 回收配额耗尽的栈帧（子树结束）：校验 numAllChildren 后并入父帧。
        while let Some(top) = stack.last() {
            if top.remaining != 0 {
                break;
            }
            let f = stack.pop().expect("just checked non-empty");
            if f.acc_all != f.declared_all {
                return Err(DsonError::new(format!(
                    "meta2 entry {} `{}`: numAllChildren {} != actual descendants {}",
                    f.m2_idx, nodes[f.node_id as usize].name, f.declared_all, f.acc_all
                )));
            }
            if let Some(parent) = stack.last_mut() {
                parent.acc_all += f.acc_all + 1;
            }
        }
    }

    if m1_cursor != m1.len() {
        return Err(DsonError::new(format!(
            "consumed {m1_cursor} of {} meta1 rows (object count mismatch)",
            m1.len()
        )));
    }
    if let Some(top) = stack.last() {
        return Err(DsonError::new(format!(
            "object `{}` declares more children than meta2 provides (missing {})",
            nodes[top.node_id as usize].name, top.remaining
        )));
    }
    debug_assert_eq!(nodes.len(), m2.len());

    let mut header_raw = [0u8; 64];
    header_raw.copy_from_slice(&bytes[..64]);

    Ok(DsonDoc {
        header_raw,
        data: data.to_vec(),
        nodes,
        bit31_by_path,
    })
}

/// §2b 叶子类型推断。返回 (值, region 内值字节起始)。
///
/// 对齐按数据区内绝对偏移计算：vs = align4(entry_off + nameLen)，
/// entry_off 为该字段区域起点（数据块内相对）。
fn infer_leaf(
    name: &str,
    idx: usize,
    region: &[u8],
    region_start: usize,
    name_len: usize,
) -> Result<(FieldValue, usize)> {
    let ctx = |what: &str| {
        DsonError::new(format!(
            "meta2 entry {idx} `{name}`: {what} (region {} bytes at {region_start}, nameLen {name_len}) — structure unsupported",
            region.len()
        ))
    };
    let after_name = region.len() - name_len;
    if after_name == 1 {
        // Bool：名字后紧跟 1 字节（不对齐）。
        let b = region[region.len() - 1];
        return Ok((FieldValue::Bool(b != 0), region.len() - 1));
    }
    // 对齐类型：值起始（绝对）= align4(region_start + name_len)。
    let value_abs = (region_start + name_len + 3) & !3;
    let aligned = value_abs - region_start; // region 内相对
    if region.len() < aligned.saturating_add(4) {
        return Err(ctx("region too short for an aligned i32 value"));
    }
    let v = i32::from_le_bytes(
        region[aligned..aligned + 4]
            .try_into()
            .expect("bounds checked"),
    );
    let remain = region.len() - aligned - 4;
    if remain == 0 {
        return Ok((FieldValue::Int(v), aligned));
    }
    if i64::from(v) == remain as i64 && region[region.len() - 1] == 0 {
        // String：i32 长度含 NUL + UTF-8 + NUL。
        let s = std::str::from_utf8(&region[aligned + 4..region.len() - 1])
            .map_err(|_| ctx("string content is not valid UTF-8"))?;
        return Ok((FieldValue::Str(s.to_string()), aligned));
    }
    if (v == 0 || v == 1) && remain == 4 {
        let v2 = i32::from_le_bytes(
            region[aligned + 4..aligned + 8]
                .try_into()
                .expect("remain==4"),
        );
        if v2 == 0 || v2 == 1 {
            return Ok((FieldValue::TwoBool(v != 0, v2 != 0), aligned));
        }
    }
    Err(ctx(
        "value shape matches no supported type (Int/Str/Bool/TwoBool)",
    ))
}
