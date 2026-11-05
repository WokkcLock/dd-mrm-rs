//! 内存模型：单一表示（arena `FieldNode` + 原始字节区引用）。
//!
//! 契约（dson-bit31-notes §6）：
//! - 解析产出 arena `Vec<FieldNode>`，nodes[0] 为根；节点保存
//!   `{name, is_object, field_info(原始), region(数据区内), children, value}`。
//! - 未触碰字段在写出时按原始字节区直拼（值字节不重编码；对齐垫按新
//!   绝对偏移重算，见 write.rs）。
//! - 变更 = 替换目标对象的 children（Fresh 节点或空 = 删除/置 null 语义）。
//! - bit31 之源真相：解析时构建 树路径→bit31 映射，写出时按输出树路径
//!   逐条目搬运；库内不出现游戏领域字段名。

use std::collections::HashMap;
use std::ops::Range;

use crate::error::{DsonError, Result};
use crate::write;

/// arena 节点下标。根节点恒为 0。
pub type NodeId = u32;

/// 叶子字段的类型化值（读取端按 §2b 推断；仅支持集内类型）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FieldValue {
    /// i32 长度含 NUL + UTF-8 + NUL。
    Str(String),
    /// 1 字节，不对齐。
    Bool(bool),
    /// 4 字节对齐 i32。
    Int(i32),
    /// 8 字节：两个 i32 各 0/1。
    TwoBool(bool, bool),
}

impl FieldValue {
    /// 类型名（dump/诊断用）。
    pub fn kind_name(&self) -> &'static str {
        match self {
            FieldValue::Str(_) => "Str",
            FieldValue::Bool(_) => "Bool",
            FieldValue::Int(_) => "Int",
            FieldValue::TwoBool(_, _) => "TwoBool",
        }
    }
}

/// 变更时新建节点携带的值：类型化叶子，或带子节点的对象子树。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NewValue {
    /// 字符串字段。
    Str(String),
    /// 布尔字段。
    Bool(bool),
    /// 整数字段。
    Int(i32),
    /// 双布尔字段。
    TwoBool(bool, bool),
    /// 对象：递归携带 children（空 = 空对象）。
    Object(Vec<NewNode>),
}

/// 变更时新建的节点。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewNode {
    /// 字段名。
    pub name: String,
    /// 字段值/子树。
    pub value: NewValue,
}

impl NewNode {
    /// 便捷构造：命名节点 + 值。
    pub fn new(name: impl Into<String>, value: NewValue) -> NewNode {
        NewNode {
            name: name.into(),
            value,
        }
    }
}

/// 节点来源：源文件原始区（含原始 fieldInfo），或变更时新建。
#[derive(Debug, Clone)]
pub(crate) enum NodeOrigin {
    /// 源节点：fieldInfo 原始保留；region 相对数据块；
    /// value_start 为 region 内值字节起始（Bool=name_len，对齐类型=对齐后位置）。
    Source {
        field_info: u32,
        region: Range<usize>,
        value_start: usize,
    },
    /// 新建节点：值即 FieldValue / children。
    Fresh,
}

/// arena 节点。
#[derive(Debug, Clone)]
pub struct FieldNode {
    /// 字段名（不含 NUL）。
    pub name: String,
    /// 是否对象（fieldInfo bit0）。
    pub is_object: bool,
    /// 子节点 arena 下标（先序遍历顺序）；叶子为空。
    pub children: Vec<NodeId>,
    /// 叶子的类型化值；对象为 None。
    pub value: Option<FieldValue>,
    /// 解析期的完整树路径（"a/b/c"，含根名）；Fresh 节点为空串。
    pub(crate) path_key: String,
    pub(crate) origin: NodeOrigin,
}

impl FieldNode {
    /// 原始 fieldInfo（仅源节点有意义；Fresh 为 None）。
    pub fn source_field_info(&self) -> Option<u32> {
        match &self.origin {
            NodeOrigin::Source { field_info, .. } => Some(*field_info),
            NodeOrigin::Fresh => None,
        }
    }

    /// 源区域（相对数据块；仅源节点有意义）。
    pub fn source_region(&self) -> Option<Range<usize>> {
        match &self.origin {
            NodeOrigin::Source { region, .. } => Some(region.clone()),
            NodeOrigin::Fresh => None,
        }
    }
}

/// 解析产物：单一表示文档。变更后可直接写出。
#[derive(Debug, Clone)]
pub struct DsonDoc {
    pub(crate) header_raw: [u8; 64],
    /// 源数据块（region 引用其中字节）。
    pub(crate) data: Vec<u8>,
    /// arena；nodes[0] 为根。变更会追加 Fresh 节点，既有下标稳定。
    pub(crate) nodes: Vec<FieldNode>,
    /// 源文档 树路径("a/b/c") → bit31。写出的唯一 bit31 真相源。
    pub(crate) bit31_by_path: HashMap<String, bool>,
}

impl DsonDoc {
    /// 根节点 id（恒 0）。
    pub fn root(&self) -> NodeId {
        0
    }

    /// 原样 header 64 字节（诊断用；写出时仅回填布局字段）。
    pub fn header_bytes(&self) -> &[u8; 64] {
        &self.header_raw
    }

    /// 源数据块长度（诊断用）。
    pub fn data_len(&self) -> usize {
        self.data.len()
    }

    /// 根字段名。
    pub fn root_name(&self) -> &str {
        &self.nodes[0].name
    }

    /// 按 id 取节点。
    pub fn node(&self, id: NodeId) -> &FieldNode {
        &self.nodes[id as usize]
    }

    /// 整个 arena（解析后的先序；变更后含未引用的废弃节点）。
    pub fn nodes(&self) -> &[FieldNode] {
        &self.nodes
    }

    /// 按树路径找节点。路径含根名（如 `["base_root","applied_ugcs_1_0","3"]`）；
    /// 空路径返回 None。同名兄弟在解析期已被拒绝，路径唯一。
    pub fn find(&self, path: &[&str]) -> Option<NodeId> {
        self.find_inner(path).ok()
    }

    fn find_inner(&self, path: &[&str]) -> std::result::Result<NodeId, DsonError> {
        if path.is_empty() {
            return Err(DsonError::new(
                "empty path: paths start at the root field name",
            ));
        }
        let mut id = self.root();
        if self.node(id).name != path[0] {
            return Err(DsonError::new(format!(
                "path root `{}` does not match document root `{}`",
                path[0],
                self.node(id).name
            )));
        }
        for comp in &path[1..] {
            let next = self
                .node(id)
                .children
                .iter()
                .find(|&&c| self.node(c).name == *comp);
            id = *next.ok_or_else(|| {
                DsonError::new(format!(
                    "path component `{comp}` not found under `{}`",
                    self.node(id).name
                ))
            })?;
        }
        Ok(id)
    }

    /// 对象的直接子节点列表（路径错误或非对象 → None）。
    pub fn children_of(&self, path: &[&str]) -> Option<&[NodeId]> {
        let id = self.find(path)?;
        if self.node(id).is_object {
            Some(&self.node(id).children)
        } else {
            None
        }
    }

    /// 叶子字段的类型化值。
    pub fn field_value(&self, path: &[&str]) -> Option<&FieldValue> {
        let id = self.find(path)?;
        self.node(id).value.as_ref()
    }

    /// 字符串值访问器。
    pub fn str_value(&self, path: &[&str]) -> Option<&str> {
        match self.field_value(path)? {
            FieldValue::Str(s) => Some(s),
            _ => None,
        }
    }

    /// 布尔值访问器。
    pub fn bool_value(&self, path: &[&str]) -> Option<bool> {
        match self.field_value(path)? {
            FieldValue::Bool(b) => Some(*b),
            _ => None,
        }
    }

    /// 整数值访问器。
    pub fn int_value(&self, path: &[&str]) -> Option<i32> {
        match self.field_value(path)? {
            FieldValue::Int(v) => Some(*v),
            _ => None,
        }
    }

    /// 双布尔值访问器。
    pub fn twobool_value(&self, path: &[&str]) -> Option<(bool, bool)> {
        match self.field_value(path)? {
            FieldValue::TwoBool(a, b) => Some((*a, *b)),
            _ => None,
        }
    }

    /// 通用子树变更：把 `path` 处对象的全部子字段替换为 `new_children`。
    ///
    /// - 空切片 = 清空全部子字段（配合对父对象调用即「字段整体消失」，
    ///   即 §4 的清空边界）。
    /// - 新节点为 Fresh：值按 §2b/§2c 规则编码；bit31 按输出树路径从源
    ///   文档映射搬运，路径无源对应时按 §2 启发式（dynamicMode 容器）。
    /// - 未触碰字段保持原始字节区。
    /// - 既有 NodeId 稳定；被替换子树的旧节点成为废弃节点（不再被写出）。
    pub fn replace_children(&mut self, path: &[&str], new_children: &[NewNode]) -> Result<()> {
        let id = self
            .find_inner(path)
            .map_err(|e| DsonError::new(format!("replace_children({}): {e}", path.join("/"))))?;
        if !self.node(id).is_object {
            return Err(DsonError::new(format!(
                "replace_children({}): `{}` is not an object",
                path.join("/"),
                self.node(id).name
            )));
        }
        let mut ids = Vec::with_capacity(new_children.len());
        for n in new_children {
            ids.push(self.materialize(n)?);
        }
        self.nodes[id as usize].children = ids;
        Ok(())
    }

    fn materialize(&mut self, n: &NewNode) -> Result<NodeId> {
        let id = u32::try_from(self.nodes.len())
            .map_err(|_| DsonError::new("arena overflow: too many nodes"))?;
        // 先占坑再递归：保证父节点下标先于子节点（arena 保持先序），
        // 且子节点 materialize 不会挤占本节点下标。
        self.nodes.push(FieldNode {
            name: String::new(),
            is_object: false,
            children: Vec::new(),
            value: None,
            path_key: String::new(),
            origin: NodeOrigin::Fresh,
        });
        let (is_object, value, children) = match &n.value {
            NewValue::Object(sub) => {
                let mut cids = Vec::with_capacity(sub.len());
                for sn in sub {
                    cids.push(self.materialize(sn)?);
                }
                (true, None, cids)
            }
            NewValue::Str(s) => (false, Some(FieldValue::Str(s.clone())), Vec::new()),
            NewValue::Bool(b) => (false, Some(FieldValue::Bool(*b)), Vec::new()),
            NewValue::Int(v) => (false, Some(FieldValue::Int(*v)), Vec::new()),
            NewValue::TwoBool(a, b) => (false, Some(FieldValue::TwoBool(*a, *b)), Vec::new()),
        };
        self.nodes[id as usize] = FieldNode {
            name: n.name.clone(),
            is_object,
            children,
            value,
            path_key: String::new(),
            origin: NodeOrigin::Fresh,
        };
        Ok(id)
    }

    /// 保真写出（同内容重写 = 字节一致）。见 write.rs。
    pub fn write(&self) -> Result<Vec<u8>> {
        write::write(self)
    }

    /// 人读结构转储（纯函数，无 I/O；dsoncli dump 的输出体）。
    pub fn dump_string(&self) -> String {
        fn rec(doc: &DsonDoc, id: NodeId, depth: usize, out: &mut String, m2: &mut usize) {
            let n = doc.node(id);
            let pad = "  ".repeat(depth);
            let kind = if n.is_object {
                "Object"
            } else {
                n.value.as_ref().map(|v| v.kind_name()).unwrap_or("?")
            };
            let head = format!("{pad}[{m2:4}] {kind:<7} {:<34} ", format!("{:?}", n.name));
            match (n.source_field_info(), n.source_region()) {
                (Some(fi), Some(region)) => {
                    out.push_str(&format!(
                        "{head}off={:<5} fi=0x{fi:08X} {{obj={} nameLen={} m1={} bit31={}}}",
                        region.start,
                        crate::meta::fi_is_object(fi) as u8,
                        crate::meta::fi_name_len(fi),
                        crate::meta::fi_meta1_idx(fi),
                        crate::meta::fi_bit31(fi) as u8,
                    ));
                }
                _ => out.push_str(&format!("{head}fresh")),
            }
            if let Some(v) = &n.value {
                let shown = match v {
                    FieldValue::Str(s) => format!("{s:?}"),
                    FieldValue::Bool(b) => format!("{b}"),
                    FieldValue::Int(i) => format!("{i}"),
                    FieldValue::TwoBool(a, b) => format!("({a},{b})"),
                };
                out.push_str(&format!(" val={shown}"));
            }
            out.push('\n');
            *m2 += 1;
            for &c in &n.children {
                rec(doc, c, depth + 1, out, m2);
            }
        }
        let mut out = format!(
            "root={} entries={} revision={:02x?}\n",
            self.root_name(),
            self.nodes.len(),
            &self.header_raw[4..8]
        );
        let mut m2 = 0usize;
        rec(self, self.root(), 0, &mut out, &mut m2);
        out
    }
}
