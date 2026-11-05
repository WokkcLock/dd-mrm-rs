//! mod 目录扫描：旧 C# `ModScanner.BaseScan` 的忠实移植（无 regex 依赖）。
//!
//! 语义（与旧工具逐条对齐）：
//! - mods 根目录不存在 → 空结果（不算错误）；
//! - 每个直接子目录是一个候选；只有当目录内直接存在
//!   `project.xml` 或 `workshop.vdf`（文件名比较不区分大小写）时才算 mod：
//!   - `project.xml`：标题 = 首个 `<Title>` 开标签到**最后一个** `</Title>`
//!     之间全部内容（对应 .NET `<Title>(.*)</Title>` + Singleline 的贪婪语义）；
//!   - `workshop.vdf`：标题 = `"title"\s*"([^"]+)"` 首个完整匹配的捕获组；
//!   - 标题做 HTML 实体解码（覆盖常用命名实体 + 十进制/十六进制数字实体，
//!     未知实体原样保留——.NET `HttpUtility.HtmlDecode` 的实用子集）；
//!   - 标题文件存在但未匹配到标题：本地 → `""`，工坊 → `"[Mod id]: {目录名}"`
//!     （仍算 mod）；
//! - 目录内有 `preview_icon.png`（不区分大小写）→ `icon_path = Some(完整路径)`；
//! - 单个候选目录读取出错 → 记入 `failed_dirs`，继续扫描其余目录；
//! - 候选遍历顺序按目录名排序（旧工具依赖 NTFS 枚举序，这里固化为确定序；
//!   同一目录同时存在 `project.xml` 与 `workshop.vdf` 时取 `project.xml`）。

use std::fs;
use std::path::{Path, PathBuf};

use crate::error::{Result, SaveError};
use crate::mod_model::{ModInfo, ModSource};

/// 一次扫描的报告：成功的 mod 列表 + 扫描失败的目录。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScanReport {
    /// 扫描到的 mod（按目录名排序）。
    pub mods: Vec<ModInfo>,
    /// 读取出错、被跳过的候选目录（交由上层提示用户）。
    pub failed_dirs: Vec<PathBuf>,
}

/// 扫描单个 mods 根目录（本地目录或工坊 content 目录）。
///
/// 根目录不存在 → 空报告（旧 `BaseScan` 的 `Directory.Exists` 语义）。
pub fn scan_mod_dir(dir: &Path, source: ModSource) -> ScanReport {
    let mut report = ScanReport::default();
    let entries = match fs::read_dir(dir) {
        Ok(entries) => entries,
        Err(_) => return report, // 不存在/不可读：空结果，不算错误
    };

    let mut subdirs: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_ok_and(|t| t.is_dir()))
        .map(|e| e.path())
        .collect();
    subdirs.sort();

    for dir_path in subdirs {
        match scan_one_candidate(&dir_path, source) {
            Ok(Some(info)) => report.mods.push(info),
            Ok(None) => {}
            Err(_) => report.failed_dirs.push(dir_path),
        }
    }
    report
}

/// 一次扫完本地 + 工坊两个根（本地在前）。
pub fn scan_local_and_workshop(local_mods_dir: &Path, workshop_dir: &Path) -> ScanReport {
    let mut local = scan_mod_dir(local_mods_dir, ModSource::Local);
    let steam = scan_mod_dir(workshop_dir, ModSource::Steam);
    local.mods.extend(steam.mods);
    local.failed_dirs.extend(steam.failed_dirs);
    local
}

/// 扫描一个候选目录：返回 `Some(mod)` / `None`（无标题文件，不是 mod）/ `Err`（读失败）。
fn scan_one_candidate(dir: &Path, source: ModSource) -> Result<Option<ModInfo>> {
    let dir_name = dir
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_default();

    // 收集直接子文件，按小写文件名排序保证确定序（project.xml < workshop.vdf）。
    let mut files: Vec<(String, PathBuf)> = Vec::new();
    for entry in fs::read_dir(dir).map_err(|e| SaveError::io(dir, e))? {
        let entry = entry.map_err(|e| SaveError::io(dir, e))?;
        if entry
            .file_type()
            .map_err(|e| SaveError::io(entry.path(), e))?
            .is_file()
        {
            let name = entry.file_name().to_string_lossy().into_owned();
            files.push((name, entry.path()));
        }
    }
    files.sort_by_key(|(name, _)| name.to_lowercase());

    let mut title: Option<String> = None;
    let mut icon_path: Option<std::path::PathBuf> = None;
    for (name, path) in &files {
        let lower = name.to_lowercase();
        if title.is_none() && (lower == "project.xml" || lower == "workshop.vdf") {
            // 与 .NET File.ReadAllText 一致：UTF-8，坏字节替换（不报错）。
            let bytes = fs::read(path).map_err(|e| SaveError::io(path, e))?;
            let text = String::from_utf8_lossy(&bytes);
            title = Some(
                match_title(&lower, &text)
                    .map_or_else(|| default_title(source, &dir_name), |t| html_decode(&t)),
            );
        }
        if lower == "preview_icon.png" {
            icon_path = Some(path.clone());
        }
    }

    Ok(title.map(|title| ModInfo {
        dir_name,
        title,
        source,
        icon_path,
    }))
}

/// 标题文件存在但未匹配到标题时的缺省（旧 BaseScan 语义）。
fn default_title(source: ModSource, dir_name: &str) -> String {
    match source {
        ModSource::Steam => format!("[Mod id]: {dir_name}"),
        ModSource::Local => String::new(),
    }
}

/// 按标题文件类型选择匹配器。
fn match_title(file_name_lower: &str, text: &str) -> Option<String> {
    if file_name_lower == "project.xml" {
        match_xml_title(text)
    } else {
        match_vdf_title(text)
    }
}

/// `<Title>(.*)</Title>`（Singleline，贪婪）的等价实现：
/// 首个 `<Title>` 开标签结束处 → 全文**最后一个** `</Title>` 起始处。
///
/// 与 .NET 引擎等价性：贪婪 `.*` 使捕获右端落在最后一个闭标签上；若最后一个
/// 闭标签先于首个开标签结束处出现，则任何开标签都无法匹配（捕获不能向左回溯），
/// 引擎同样失败。大小写敏感（旧正则如此）。
fn match_xml_title(text: &str) -> Option<String> {
    let open = text.find("<Title>")?;
    let start = open + "<Title>".len();
    let close = text.rfind("</Title>")?;
    if close < start {
        return None;
    }
    Some(text[start..close].to_string())
}

/// `"title"\s*"([^"]+)"` 的等价实现：从左到右找首个完整匹配。
fn match_vdf_title(text: &str) -> Option<String> {
    let needle = "\"title\"";
    let bytes = text.as_bytes();
    let mut from = 0;
    while let Some(rel) = text[from..].find(needle) {
        let at = from + rel;
        let mut i = at + needle.len();
        // \s* 贪婪跳过全部空白（\s 含换行；反引号不属空白，回溯无益，等价）。
        while let Some(ch) = text[i..].chars().next() {
            if ch.is_whitespace() {
                i += ch.len_utf8();
            } else {
                break;
            }
        }
        if bytes.get(i) == Some(&b'"') {
            let start = i + 1;
            let mut k = start;
            while k < bytes.len() && bytes[k] != b'"' {
                k += 1;
            }
            if k > start && k < bytes.len() {
                return Some(text[start..k].to_string());
            }
            // 捕获组为空或无闭引号：此起点失败，继续向右找（左优先语义）。
        }
        from = at + 1;
    }
    None
}

/// HTML 实体解码（`HttpUtility.HtmlDecode` 实用子集）。
///
/// 支持：`&amp; &lt; &gt; &quot; &apos; &nbsp;` 与 `&#NNN;` `&#xHHH;`
/// （十进制/十六进制数字实体）。未知实体原样保留。与 .NET 的差异：
/// 不含全量命名实体表与 0x80-0x9F 的 cp1252 映射——mod 标题实际只用到上述子集。
pub(crate) fn html_decode(s: &str) -> String {
    let Some(first_amp) = s.find('&') else {
        return s.to_owned();
    };
    let mut out = String::with_capacity(s.len());
    out.push_str(&s[..first_amp]);
    let bytes = s.as_bytes();
    let mut i = first_amp;
    while i < s.len() {
        if bytes[i] != b'&' {
            let ch = s[i..].chars().next().unwrap_or('&');
            out.push(ch);
            i += ch.len_utf8();
            continue;
        }
        match parse_entity(&s[i..]) {
            Some((ch, consumed)) => {
                out.push(ch);
                i += consumed;
            }
            None => {
                out.push('&');
                i += 1;
            }
        }
    }
    out
}

/// 解析 `s`（以 `&` 开头）处的一个实体；返回（替换字符, 消费字节数）。
fn parse_entity(s: &str) -> Option<(char, usize)> {
    let b = s.as_bytes();
    if b.len() < 3 {
        return None;
    }
    if b[1] == b'#' {
        let (start, radix) = if b.len() > 2 && (b[2] == b'x' || b[2] == b'X') {
            (3, 16u32)
        } else {
            (2, 10u32)
        };
        let mut val: u32 = 0;
        let mut j = start;
        let mut digits = 0;
        while j < b.len() && digits < 8 {
            let d = match b[j] {
                b'0'..=b'9' => u32::from(b[j] - b'0'),
                b'a'..=b'f' if radix == 16 => u32::from(b[j] - b'a' + 10),
                b'A'..=b'F' if radix == 16 => u32::from(b[j] - b'A' + 10),
                _ => break,
            };
            val = val.wrapping_mul(radix).wrapping_add(d);
            j += 1;
            digits += 1;
        }
        if digits == 0 || j >= b.len() || b[j] != b';' {
            return None;
        }
        let ch = char::from_u32(val)?;
        Some((ch, j + 1))
    } else {
        let mut j = 1;
        while j < b.len() && b[j].is_ascii_alphanumeric() && j <= 32 {
            j += 1;
        }
        if j == 1 || j >= b.len() || b[j] != b';' {
            return None;
        }
        let name = &s[1..j];
        let ch = match name {
            "amp" => '&',
            "lt" => '<',
            "gt" => '>',
            "quot" => '"',
            "apos" => '\'',
            "nbsp" => '\u{a0}',
            _ => return None,
        };
        Some((ch, j + 1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn xml_title_greedy_first_open_last_close() {
        assert_eq!(match_xml_title("<Title>abc</Title>"), Some("abc".into()));
        // 贪婪：中间的闭标签被吃进捕获组
        assert_eq!(
            match_xml_title("<Title>a</Title> b <Title>c</Title>"),
            Some("a</Title> b <Title>c".into())
        );
        assert_eq!(
            match_xml_title("x <Title>多行\n标题</Title> y"),
            Some("多行\n标题".into())
        );
        // 空标题也算匹配成功（旧工具得到 ""）
        assert_eq!(match_xml_title("<Title></Title>"), Some("".into()));
        // 闭标签在开标签之前 → 不匹配
        assert_eq!(match_xml_title("</Title><Title>x"), None);
        assert_eq!(match_xml_title("<title>小写</title>"), None); // 大小写敏感
        assert_eq!(match_xml_title("no tags"), None);
    }

    #[test]
    fn vdf_title_leftmost_complete_match() {
        assert_eq!(
            match_vdf_title("\"title\"\t\"My Mod\""),
            Some("My Mod".into())
        );
        assert_eq!(
            match_vdf_title("\"title\"   \n  \"A B\""),
            Some("A B".into())
        );
        assert_eq!(match_vdf_title("\"title\"\"x\""), Some("x".into()));
        // 空捕获不算匹配，继续向右
        assert_eq!(
            match_vdf_title("\"title\"\"\" \"title\" \"y\""),
            Some("y".into())
        );
        // title 后紧跟非空白非引号 → 此处失败
        assert_eq!(match_vdf_title("\"titles\" \"x\""), None);
        assert_eq!(match_vdf_title("\"title\" x"), None);
        // 无闭引号
        assert_eq!(match_vdf_title("\"title\" \"abc"), None);
        // 大小写敏感（"Title" 不匹配）
        assert_eq!(match_vdf_title("\"Title\" \"x\""), None);
    }

    #[test]
    fn html_decode_subset() {
        assert_eq!(html_decode("A &amp; B"), "A & B");
        assert_eq!(html_decode("&lt;b&gt;"), "<b>");
        assert_eq!(html_decode("&quot;q&quot;"), "\"q\"");
        assert_eq!(html_decode("&apos;"), "'");
        // 只解码一层（与 HttpUtility.HtmlDecode 一致，不递归）
        assert_eq!(html_decode("&amp;amp;"), "&amp;");
        assert_eq!(html_decode("&#65;&#x42;"), "AB");
        assert_eq!(html_decode("&#x4E2D;&#25991;"), "中文");
        assert_eq!(html_decode("&nbsp;"), "\u{a0}");
        assert_eq!(html_decode("&unknown; &amp;"), "&unknown; &");
        assert_eq!(html_decode("no entity"), "no entity");
        assert_eq!(html_decode("&#xD800;"), "&#xD800;"); // 代理区：不解码，原样保留
    }
}
