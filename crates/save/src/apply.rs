//! 离线应用（旧 C# `SyncMod` + `CoverSave` 语义）与 GC4 原子写。

use std::fs;
use std::path::{Path, PathBuf};
use std::thread::sleep;
use std::time::Duration;

use dson::{DsonDoc, FieldValue, NewNode, NewValue, NodeId};

use crate::backup::backup_estate_dir;
use crate::error::{Result, SaveError};
use crate::mod_model::{AppliedMod, DlcEntry, ModSource};
use crate::paths::validate_persist_path;

/// persist 根对象里 applied mod 容器的字段名。
pub const APPLIED_FIELD_NAME: &str = "applied_ugcs_1_0";

/// persist 根对象里 DLC 权属容器的字段名（规格级容器名之一，见
/// docs/dson-bit31-notes.md §容器四名）。
pub const DLC_FIELD_NAME: &str = "dlc";

/// rename 重试参数（端口自旧 C# DumpMeta 的 10×100ms；见 `atomic_write_raw`）。
const RENAME_RETRIES: usize = 10;
const RENAME_RETRY_DELAY: Duration = Duration::from_millis(100);

/// 已解析打开的 estate 存档文件。
#[derive(Debug)]
pub struct EstateFile {
    path: PathBuf,
    doc: DsonDoc,
}

impl EstateFile {
    /// 校验白名单 → 读盘 → 解析。构造后 `path` 不再变化。
    pub fn open(path: &Path) -> Result<Self> {
        validate_persist_path(path)?;
        let bytes = fs::read(path).map_err(|e| SaveError::io(path, e))?;
        let doc = dson::parse(&bytes)?;
        Ok(EstateFile {
            path: path.to_path_buf(),
            doc,
        })
    }

    /// 源文件路径。
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// estate 显示名（`base_root.estatename`），缺失 → `None`。
    pub fn estate_name(&self) -> Option<&str> {
        self.doc.str_value(&[self.doc.root_name(), "estatename"])
    }

    /// 游戏模式（`base_root.game_mode`），缺失 → `None`。
    pub fn game_mode(&self) -> Option<&str> {
        self.doc.str_value(&[self.doc.root_name(), "game_mode"])
    }

    /// 当前 applied mod 列表（fresh Vec，无旧 C# 的活引用陷阱）。
    ///
    /// 字段缺失 → `Ok(vec![])`。字段存在但形状不符（子项不是恰好
    /// `name`+`source` 两个字符串字段的容器、来源值未知）→ `Err(Shape)`。
    pub fn applied_mods(&self) -> Result<Vec<AppliedMod>> {
        let Some(children) = self
            .doc
            .children_of(&[self.doc.root_name(), APPLIED_FIELD_NAME])
        else {
            return Ok(Vec::new());
        };
        let mut mods = Vec::with_capacity(children.len());
        for &child in children {
            mods.push(self.parse_mod_item(child)?);
        }
        Ok(mods)
    }

    /// estate 拥有的 DLC 列表（`base_root.dlc`，按文件顺序，fresh Vec）。
    ///
    /// 金样本实证：`dlc` 是权属列表（`DlcEntry` 文档），**无 enabled 布尔**——
    /// "是否启用" = 条目在不在本列表，由 UI 层推导。
    ///
    /// - 容器缺失 → `Ok(vec![])`；
    /// - 容器存在但不是对象 → `Err(Shape)`（不与"缺失"混同）；
    /// - 子项不是恰好 `name`+`source` 两个字符串字段的对象 → `Err(Shape)`
    ///   （严格口径与 [`Self::applied_mods`] 一致：读取层不静默丢数据）；
    /// - `source` 字面量不校验枚举（观测恒为 `"dlc"`，按文件原样返回）。
    pub fn dlc_entries(&self) -> Result<Vec<DlcEntry>> {
        let path = [self.doc.root_name(), DLC_FIELD_NAME];
        let Some(id) = self.doc.find(&path) else {
            return Ok(Vec::new());
        };
        if !self.doc.node(id).is_object {
            return Err(SaveError::Shape(format!(
                "{DLC_FIELD_NAME} 容器存在但不是对象"
            )));
        }
        let children = self
            .doc
            .children_of(&path)
            .expect("find 命中且为对象时 children_of 必返回 Some");
        let mut entries = Vec::with_capacity(children.len());
        for &child in children {
            entries.push(self.parse_dlc_item(child)?);
        }
        Ok(entries)
    }

    fn parse_dlc_item(&self, id: NodeId) -> Result<DlcEntry> {
        let node = self.doc.node(id);
        let bad = |what: &str| SaveError::Shape(format!("{DLC_FIELD_NAME} 子项 {what}"));
        if !node.is_object {
            return Err(bad("不是对象"));
        }
        let mut name: Option<&str> = None;
        let mut source: Option<&str> = None;
        let mut extra = false;
        for &c in &node.children {
            let child = self.doc.node(c);
            match (child.name.as_str(), &child.value) {
                ("name", Some(FieldValue::Str(v))) if name.is_none() => name = Some(v),
                ("source", Some(FieldValue::Str(v))) if source.is_none() => source = Some(v),
                _ => extra = true,
            }
        }
        if extra {
            return Err(bad("含未知/多余字段"));
        }
        let name = name.ok_or_else(|| bad("缺 name"))?;
        let source = source.ok_or_else(|| bad("缺 source"))?;
        Ok(DlcEntry {
            name: name.to_owned(),
            source: source.to_owned(),
        })
    }

    fn parse_mod_item(&self, id: NodeId) -> Result<AppliedMod> {
        let node = self.doc.node(id);
        let bad = |what: &str| SaveError::Shape(format!("{APPLIED_FIELD_NAME} 子项 {what}"));
        if !node.is_object {
            return Err(bad("不是对象"));
        }
        let mut name: Option<&str> = None;
        let mut source: Option<&str> = None;
        let mut extra = false;
        for &c in &node.children {
            let child = self.doc.node(c);
            match (child.name.as_str(), &child.value) {
                ("name", Some(FieldValue::Str(v))) if name.is_none() => name = Some(v),
                ("source", Some(FieldValue::Str(v))) if source.is_none() => source = Some(v),
                _ => extra = true,
            }
        }
        if extra {
            return Err(bad("含未知/多余字段"));
        }
        let name = name.ok_or_else(|| bad("缺 name"))?;
        let source = source.ok_or_else(|| bad("缺 source"))?;
        let source = ModSource::from_dson_str(source)
            .ok_or_else(|| bad(&format!("未知 source 值 `{source}`")))?;
        Ok(AppliedMod {
            name: name.to_owned(),
            source,
        })
    }

    /// SyncMod 语义：用 `mods` 整体重写 `base_root.applied_ugcs_1_0`。
    ///
    /// - 空 → 字段从文档中**消失**（旧工具 WillApplyMod=null 语义）；
    ///   若字段本就缺失则为无操作。
    /// - 非空 → 容器子项重建为 `"0".."N-1"`，每项 `{name, source}`。
    ///   字段原本缺失时按旧 `JsonObject.Add` 语义追加到 base_root 末尾。
    /// - 其余子树（persistent_ugcs、estatename、game_mode、dlc…）原样保留。
    pub fn sync_mods(&mut self, mods: &[AppliedMod]) -> Result<()> {
        let root = self.doc.root_name().to_owned();
        let root_path = [root.as_str()];
        let applied_path = [root.as_str(), APPLIED_FIELD_NAME];
        let field_present = self.doc.find(&applied_path).is_some();

        if mods.is_empty() {
            if !field_present {
                return Ok(());
            }
            // 重建 base_root 全部子项、剔除 applied 字段。
            let kept: Vec<NewNode> = self
                .doc
                .children_of(&root_path)
                .map(|cs| cs.iter().copied())
                .unwrap_or_default()
                .filter(|&id| self.doc.node(id).name != APPLIED_FIELD_NAME)
                .map(|id| subtree_to_new_node(&self.doc, id))
                .collect::<Result<Vec<_>>>()?;
            self.doc.replace_children(&root_path, &kept)?;
            return Ok(());
        }

        let entries: Vec<NewNode> = mods
            .iter()
            .enumerate()
            .map(|(i, m)| {
                NewNode::new(
                    i.to_string(),
                    NewValue::Object(vec![
                        NewNode::new("name", NewValue::Str(m.name.clone())),
                        NewNode::new("source", NewValue::Str(m.source.as_dson_str().to_owned())),
                    ]),
                )
            })
            .collect();

        if field_present {
            self.doc.replace_children(&applied_path, &entries)?;
            Ok(())
        } else {
            // 旧工具：字段缺失时 Add 到 base_root 末尾 → 这里同样追加在末尾。
            let mut rebuilt: Vec<NewNode> = self
                .doc
                .children_of(&root_path)
                .map(|cs| cs.iter().copied())
                .unwrap_or_default()
                .map(|id| subtree_to_new_node(&self.doc, id))
                .collect::<Result<Vec<_>>>()?;
            rebuilt.push(NewNode::new(APPLIED_FIELD_NAME, NewValue::Object(entries)));
            self.doc.replace_children(&root_path, &rebuilt)?;
            Ok(())
        }
    }

    /// 编码为 DSON 字节（不落盘）。
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        Ok(self.doc.write()?)
    }

    /// 原子写回源路径（GC4）。
    pub fn write(&self) -> Result<()> {
        let bytes = self.to_bytes()?;
        write_persist_atomic(&self.path, &bytes)
    }
}

/// 把一棵已解析子树重建为 `NewNode`（Fresh 化；写入端按规范编码，
/// 与旧工具全量重编码字节一致——见任务 2 报告的 Source/Fresh 写出论证）。
fn subtree_to_new_node(doc: &DsonDoc, id: NodeId) -> Result<NewNode> {
    let node = doc.node(id);
    let value = if node.is_object {
        let children = node
            .children
            .iter()
            .copied()
            .map(|c| subtree_to_new_node(doc, c))
            .collect::<Result<Vec<_>>>()?;
        NewValue::Object(children)
    } else {
        match node.value.as_ref() {
            Some(FieldValue::Str(v)) => NewValue::Str(v.clone()),
            Some(FieldValue::Bool(v)) => NewValue::Bool(*v),
            Some(FieldValue::Int(v)) => NewValue::Int(*v),
            Some(FieldValue::TwoBool(a, b)) => NewValue::TwoBool(*a, *b),
            None => {
                return Err(SaveError::Shape(format!(
                    "字段 `{}` 既无值也不是对象",
                    node.name
                )));
            }
        }
    };
    Ok(NewNode::new(node.name.clone(), value))
}

/// 白名单 + 原子写：只允许写 `persist.game.json`（GC3 + GC4 的对外入口）。
pub fn write_persist_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    validate_persist_path(path)?;
    atomic_write_raw(path, bytes)
}

/// 通用原子写：`{path}.tmp` → rename（Windows 上 `fs::rename` 即
/// `MoveFileEx(..., REPLACE_EXISTING)`）。rename 失败重试 ≤10×100ms。
///
/// 旧 C# DumpMeta 的重试被其 `finally` 中的 tmp 删除破坏（每次失败都把
/// tmp 删了，重试形同虚设）；此处按其**意图**实现：重试期间保留 tmp，
/// 只有最终放弃时才清理，并报告最后一次错误。
pub(crate) fn atomic_write_raw(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut tmp: PathBuf = path.as_os_str().to_os_string().into();
    tmp.as_mut_os_string().push(".tmp");
    fs::write(&tmp, bytes).map_err(|e| SaveError::io(&tmp, e))?;

    let mut last_err: Option<std::io::Error> = None;
    for _ in 0..RENAME_RETRIES {
        match fs::rename(&tmp, path) {
            Ok(()) => return Ok(()),
            Err(e) => {
                last_err = Some(e);
                sleep(RENAME_RETRY_DELAY);
            }
        }
    }
    let err = last_err.expect("rename 重试至少一次后 last_err 必有值");
    let _ = fs::remove_file(&tmp); // 最终失败才清理（修复旧 bug）
    Err(SaveError::io(path, err))
}

/// `apply_offline` 的可选项。
#[derive(Debug, Clone, Default)]
pub struct ApplyOptions {
    /// 写入前对整个 estate 目录做 zip 备份的目标目录（None = 不备份）。
    pub backup_dir: Option<PathBuf>,
}

/// 一次离线应用的结果。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyOutcome {
    /// 备份与日志用的 estate 标签（estatename，缺失时用目录名）。
    pub estate_label: String,
    /// 被写入的 persist 文件路径。
    pub persist_path: PathBuf,
    /// 生成的备份 zip（未启用备份 → None）。
    pub backup_zip: Option<PathBuf>,
    /// 应用后的 mod 数。
    pub mod_count: usize,
    /// 写出字节数。
    pub bytes_written: usize,
}

/// 离线应用（旧 CoverSave = 备份 + SyncMod + 写出）：
///
/// 1. 白名单校验 + 解析存档；
/// 2. 若给了 `backup_dir`，**先**对 estate 目录整体 zip 备份；
/// 3. `sync_mods` 重写 applied 列表；
/// 4. 原子写回。
///
/// 失败于任一步则不落盘（备份已生成时不回滚——它只是多了一份备份）。
pub fn apply_offline(
    persist_path: &Path,
    mods: &[AppliedMod],
    opts: &ApplyOptions,
) -> Result<ApplyOutcome> {
    validate_persist_path(persist_path)?;
    let mut file = EstateFile::open(persist_path)?;

    let estate_dir = persist_path
        .parent()
        .filter(|p| !p.as_os_str().is_empty())
        .map(Path::to_path_buf)
        .unwrap_or_else(|| PathBuf::from("."));
    let label = backup_label(&file, &estate_dir);

    let backup_zip = match &opts.backup_dir {
        Some(dir) => Some(backup_estate_dir(&estate_dir, &label, dir)?),
        None => None,
    };

    file.sync_mods(mods)?;
    let bytes = file.to_bytes()?;
    write_persist_atomic(persist_path, &bytes)?;

    Ok(ApplyOutcome {
        estate_label: label,
        persist_path: persist_path.to_path_buf(),
        backup_zip,
        mod_count: mods.len(),
        bytes_written: bytes.len(),
    })
}

/// 备份标签：优先 estatename（消毒为文件系统安全字符），缺失/为空回退目录名。
fn backup_label(file: &EstateFile, estate_dir: &Path) -> String {
    let raw = file
        .estate_name()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(sanitize_filename)
        .unwrap_or_default();
    if raw.is_empty() {
        estate_dir
            .file_name()
            .map(|n| sanitize_filename(&n.to_string_lossy()))
            .unwrap_or_else(|| "estate".to_owned())
    } else {
        raw
    }
}

/// 把 Windows 文件名非法字符与控制字符替换为 `_`。
fn sanitize_filename(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_replaces_invalid_chars() {
        assert_eq!(
            sanitize_filename(r#"a<b>c:"d/e\f|g?h*i"#),
            "a_b_c__d_e_f_g_h_i"
        );
        assert_eq!(sanitize_filename("干净名字"), "干净名字");
        assert_eq!(sanitize_filename("ctrl\u{7}x"), "ctrl_x");
    }

    #[test]
    fn tmp_path_is_append_not_replace_extension() {
        let p = Path::new(r"C:\saves\e1\persist.game.json");
        let mut tmp = p.as_os_str().to_os_string();
        tmp.push(".tmp");
        assert_eq!(
            PathBuf::from(tmp),
            PathBuf::from(r"C:\saves\e1\persist.game.json.tmp")
        );
    }

    // —— dlc_entries：合成形状用例（金样本读取走集成测试）——

    fn golden_bytes(name: &str) -> Vec<u8> {
        let p = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../tests/golden")
            .join(name);
        fs::read(&p).unwrap_or_else(|e| panic!("读 golden {name} 失败: {e}"))
    }

    fn estate_from_bytes(bytes: &[u8]) -> EstateFile {
        EstateFile {
            path: PathBuf::from("persist.game.json"),
            doc: dson::parse(bytes).expect("解析失败"),
        }
    }

    /// 剔除 base_root 指定子项后重建（crate 内工具，与 sync_mods 同手法）。
    fn drop_root_field(file: &mut EstateFile, field: &str) {
        let root = file.doc.root_name().to_owned();
        let kept: Vec<NewNode> = file
            .doc
            .children_of(&[root.as_str()])
            .map(|cs| cs.iter().copied())
            .unwrap_or_default()
            .filter(|&id| file.doc.node(id).name != field)
            .map(|id| subtree_to_new_node(&file.doc, id))
            .collect::<Result<Vec<_>>>()
            .unwrap();
        file.doc.replace_children(&[root.as_str()], &kept).unwrap();
    }

    #[test]
    fn dlc_entries_missing_container_is_empty() {
        let bytes = golden_bytes("profile_7.persist.game.json.dson");
        let mut file = estate_from_bytes(&bytes);
        drop_root_field(&mut file, DLC_FIELD_NAME);
        assert!(file.dlc_entries().unwrap().is_empty());
    }

    #[test]
    fn dlc_entries_non_object_container_is_shape_err() {
        let bytes = golden_bytes("profile_7.persist.game.json.dson");
        let mut file = estate_from_bytes(&bytes);
        let root = file.doc.root_name().to_owned();
        let rebuilt: Vec<NewNode> = file
            .doc
            .children_of(&[root.as_str()])
            .map(|cs| cs.iter().copied())
            .unwrap_or_default()
            .map(|id| {
                if file.doc.node(id).name == DLC_FIELD_NAME {
                    NewNode::new(DLC_FIELD_NAME, NewValue::Str("oops".to_owned()))
                } else {
                    subtree_to_new_node(&file.doc, id).unwrap()
                }
            })
            .collect();
        file.doc
            .replace_children(&[root.as_str()], &rebuilt)
            .unwrap();
        assert!(matches!(file.dlc_entries(), Err(SaveError::Shape(_))));
    }

    #[test]
    fn dlc_entries_malformed_children_are_shape_err() {
        let bytes = golden_bytes("profile_7.persist.game.json.dson");
        let mut file = estate_from_bytes(&bytes);
        let root = file.doc.root_name().to_owned();
        let path = [root.as_str(), DLC_FIELD_NAME];
        // 子项不是对象。
        file.doc
            .replace_children(&path, &[NewNode::new("0", NewValue::Str("x".to_owned()))])
            .unwrap();
        assert!(matches!(file.dlc_entries(), Err(SaveError::Shape(_))));
        // 子项缺 name。
        file.doc
            .replace_children(
                &path,
                &[NewNode::new(
                    "0",
                    NewValue::Object(vec![NewNode::new(
                        "source",
                        NewValue::Str("dlc".to_owned()),
                    )]),
                )],
            )
            .unwrap();
        assert!(matches!(file.dlc_entries(), Err(SaveError::Shape(_))));
        // 子项含多余字段。
        file.doc
            .replace_children(
                &path,
                &[NewNode::new(
                    "0",
                    NewValue::Object(vec![
                        NewNode::new("name", NewValue::Str("shieldbreaker".to_owned())),
                        NewNode::new("source", NewValue::Str("dlc".to_owned())),
                        NewNode::new("extra", NewValue::Str("?".to_owned())),
                    ]),
                )],
            )
            .unwrap();
        assert!(matches!(file.dlc_entries(), Err(SaveError::Shape(_))));
    }
}
