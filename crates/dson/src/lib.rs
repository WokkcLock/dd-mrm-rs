//! dson —— Darkest Dungeon 存档 DSON 二进制编解码核心（M1）。
//!
//! 保真契约（docs/dson-bit31-notes.md §6）：
//! - 单一表示：解析产出 arena 文档，未触碰字段以原始字节区引用源数据，
//!   写出时逐字节搬运（含 bit31、非规范真值字节、header zeroes）。
//! - 变更 = `DsonDoc::replace_children(path, &[NewNode])`：泛用子树替换，
//!   库内不出现任何游戏领域字段名。
//! - 未知结构（§2b 支持集之外）→ 携带上下文的 Err，绝不 panic。
//!
//! 典型用法（Task 3 消费方；IO 由调用方负责，库只吃字节）：
//!
//! ```no_run
//! use dson::{parse, NewNode, NewValue};
//! # fn main() -> dson::Result<()> {
//! let bytes = std::fs::read("profile.persist.game.json.dson").unwrap();
//! let mut doc = parse(&bytes)?;
//! let root = doc.root_name().to_string();
//! let list_path = [root.as_str(), "applied_ugcs_1_0"];
//! let kids: Vec<NewNode> = doc
//!     .children_of(&list_path)
//!     .unwrap()
//!     .iter()
//!     .filter_map(|&id| {
//!         let n = doc.node(id);
//!         let name = doc.str_value(&[list_path[0], list_path[1], &n.name])?;
//!         let src = doc.str_value(&[list_path[0], list_path[1], &n.name, "source"])?;
//!         Some(NewNode::new(
//!             n.name.clone(),
//!             NewValue::Object(vec![
//!                 NewNode::new("name", NewValue::Str(name.to_string())),
//!                 NewNode::new("source", NewValue::Str(src.to_string())),
//!             ]),
//!         ))
//!     })
//!     .collect();
//! doc.replace_children(&["base_root", "applied_ugcs_1_0"], &kids)?;
//! std::fs::write("out.dson", doc.write()?).unwrap();
//! # Ok(())
//! # }
//! ```

pub mod error;
pub mod hash;
pub mod meta;
pub mod model;
pub mod parse;
pub mod write;

pub use error::{DsonError, Result};
pub use meta::{
    Header, Meta1Entry, Meta2Entry, fi_bit31, fi_is_object, fi_meta1_idx, fi_name_len, field_info,
};
pub use model::{DsonDoc, FieldNode, FieldValue, NewNode, NewValue, NodeId};
pub use parse::parse;

/// crate 名（save crate 引用；勿删）。取自包名，改名自动跟随。
pub const NAME: &str = env!("CARGO_PKG_NAME");
