//! fs_get_file_info 锚点定位 —— B 路线（v14）唯一锚。
//!
//! v17 及之前的多锚点定位（frame/vpush/persist/refresh/f1/savefile/…）已随
//! 重放方案一起退役：DLL 现在只钩 [`FS_PATTERN`] 命中的 fs_get_file_info，
//! 在 size 查询返回处对齐磁盘真值（E1c/E1d/E1e 实验定案的唯一守卫）。
//!
//! 定位协议（与 DdHotapply.c InitThread 的三层校验一致）：
//! 1. 16B 指纹在 .text 全节唯一（Darkest.exe 1c67048e 实测 raw=1，
//!    fileoff 0x247330 → RVA 0x247F30，.text 的 RVA-文件偏移差 0xC00）；
//! 2. 命中点后 0x300 窗口内出现双位移体校验（B8 11 00 00 / B8 15 00 00，
//!    即 cached_path 0x11B8 / cached_name 0x15B8 的 mov r32, imm32）；
//! 3. RVA 由 .text 节头换算（section.rva + 节内偏移），不经文件偏移猜测。

use std::fmt;

/// fs_get_file_info 函数头 16B 指纹（push 序 + 三个 41 5x 的 r12-r15 保存）。
pub const FS_PATTERN: [u8; 16] = [
    0x48, 0x89, 0x5C, 0x24, 0x18, 0x55, 0x56, 0x57, 0x41, 0x54, 0x41, 0x55, 0x41, 0x56, 0x41,
    0x57,
];

/// 命中点之后多大的窗口内要求出现体校验字节。
const BODY_CHECK_LEN: usize = 0x300;
/// 体校验 A：mov r32, 0x11B8 —— cached_path 位移（fs+4536）。
const BODY_A: [u8; 4] = [0xB8, 0x11, 0x00, 0x00];
/// 体校验 B：mov r32, 0x15B8 —— cached_name 位移（fs+5560）。
const BODY_B: [u8; 4] = [0xB8, 0x15, 0x00, 0x00];

/// 唯一锚：fs_get_file_info 的 RVA + 指纹（写进 v8 配置块交给 DLL）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Anchors {
    pub fs_rva: u32,
    pub fs_pattern: [u8; 16],
}

/// 定位失败原因。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocateError {
    /// 不是合法 PE64（附检查点描述）。
    NotPe(&'static str),
    /// 没有 .text 节。
    NoText,
    /// 指纹（或过体校验的指纹）零命中。
    NotFound,
    /// 多处命中 —— 版本漂移或误配，拒绝盲选。
    Ambiguous(usize),
}

impl fmt::Display for LocateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LocateError::NotPe(why) => write!(f, "不是 PE64 镜像（{why}）"),
            LocateError::NoText => write!(f, "找不到 .text 节"),
            LocateError::NotFound => write!(f, "fs_get_file_info 指纹零命中（游戏版本不符？）"),
            LocateError::Ambiguous(n) => write!(f, "指纹命中 {n} 处，拒绝盲选"),
        }
    }
}

impl std::error::Error for LocateError {}

// ---- 极简 PE 解析（只取节表，够定位用）----

struct Section {
    rva: u32,
    raw_ptr: u32,
    raw_size: u32,
}

fn u16_at(b: &[u8], off: usize) -> Option<u16> {
    let s = b.get(off..off + 2)?;
    Some(u16::from_le_bytes([s[0], s[1]]))
}

fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    let s = b.get(off..off + 4)?;
    Some(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
}

fn text_section(bytes: &[u8]) -> Result<Section, LocateError> {
    if bytes.len() < 0x40 || &bytes[..2] != b"MZ" {
        return Err(LocateError::NotPe("无 MZ 头"));
    }
    let pe_off = u32_at(bytes, 0x3c).ok_or(LocateError::NotPe("e_lfanew 越界"))? as usize;
    if bytes.get(pe_off..pe_off + 4) != Some(b"PE\0\0") {
        return Err(LocateError::NotPe("无 PE 签名"));
    }
    if u16_at(bytes, pe_off + 4) != Some(0x8664) {
        return Err(LocateError::NotPe("非 x64 镜像"));
    }
    if u16_at(bytes, pe_off + 24) != Some(0x20b) {
        return Err(LocateError::NotPe("非 PE32+ 可选头"));
    }
    let n_sections = u16_at(bytes, pe_off + 6).ok_or(LocateError::NotPe("节数越界"))? as usize;
    let opt_size = u16_at(bytes, pe_off + 20).ok_or(LocateError::NotPe("可选头大小越界"))? as usize;
    let table = pe_off + 24 + opt_size;
    for i in 0..n_sections {
        let s = table + i * 40;
        let name = bytes.get(s..s + 8).ok_or(LocateError::NotPe("节表越界"))?;
        if name.starts_with(b".text") {
            return Ok(Section {
                rva: u32_at(bytes, s + 12).ok_or(LocateError::NotPe(".text rva 越界"))?,
                raw_ptr: u32_at(bytes, s + 20).ok_or(LocateError::NotPe(".text 指针越界"))?,
                raw_size: u32_at(bytes, s + 16).ok_or(LocateError::NotPe(".text 大小越界"))?,
            });
        }
    }
    Err(LocateError::NoText)
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    if needle.is_empty() || hay.len() < needle.len() {
        return false;
    }
    hay.windows(needle.len()).any(|w| w == needle)
}

fn body_ok(win: &[u8]) -> bool {
    contains(win, &BODY_A) && contains(win, &BODY_B)
}

/// 在整个 PE 字节里定位 fs_get_file_info：指纹唯一 + 体校验通过 → 锚。
pub fn locate(bytes: &[u8]) -> Result<Anchors, LocateError> {
    let sec = text_section(bytes)?;
    let start = sec.raw_ptr as usize;
    let end = start.saturating_add(sec.raw_size as usize).min(bytes.len());
    let hay = bytes.get(start..end).ok_or(LocateError::NotPe(".text 数据越界"))?;
    let mut hits = Vec::new();
    if hay.len() >= FS_PATTERN.len() {
        for i in 0..=hay.len() - FS_PATTERN.len() {
            if &hay[i..i + FS_PATTERN.len()] == FS_PATTERN.as_slice() {
                let win_end = (i + BODY_CHECK_LEN).min(hay.len());
                if body_ok(&hay[i..win_end]) {
                    hits.push(i);
                }
            }
        }
    }
    match hits.as_slice() {
        [only] => Ok(Anchors {
            fs_rva: sec.rva + *only as u32,
            fs_pattern: FS_PATTERN,
        }),
        [] => Err(LocateError::NotFound),
        many => Err(LocateError::Ambiguous(many.len())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 体校验字节窗口：0x10 处 BODY_A、0x20 处 BODY_B。
    fn body_bytes() -> Vec<u8> {
        let mut v = vec![0xCC_u8; BODY_CHECK_LEN];
        v[0x10..0x14].copy_from_slice(&BODY_A);
        v[0x20..0x24].copy_from_slice(&BODY_B);
        v
    }

    /// 手搓最小 PE64：单 .text 节（rva 0x1000 / raw 0x400），内容可定制。
    fn minimal_pe(text: &[u8]) -> Vec<u8> {
        let mut b = vec![0_u8; 0x400 + text.len().max(0x400)];
        b[..2].copy_from_slice(b"MZ");
        b[0x3c..0x40].copy_from_slice(&0x40u32.to_le_bytes());
        let pe = 0x40usize;
        b[pe..pe + 4].copy_from_slice(b"PE\0\0");
        b[pe + 4..pe + 6].copy_from_slice(&0x8664u16.to_le_bytes()); // x64
        b[pe + 6..pe + 8].copy_from_slice(&1u16.to_le_bytes()); // 1 节
        b[pe + 20..pe + 22].copy_from_slice(&0xF0u16.to_le_bytes()); // 可选头
        b[pe + 24..pe + 26].copy_from_slice(&0x20Bu16.to_le_bytes()); // PE32+
        let s = pe + 24 + 0xF0;
        b[s..s + 5].copy_from_slice(b".text");
        b[s + 12..s + 16].copy_from_slice(&0x1000u32.to_le_bytes()); // rva
        b[s + 16..s + 20].copy_from_slice(&(text.len() as u32).to_le_bytes()); // raw size
        b[s + 20..s + 24].copy_from_slice(&0x400u32.to_le_bytes()); // raw ptr
        b[0x400..0x400 + text.len()].copy_from_slice(text);
        b
    }

    #[test]
    fn locate_synthetic_pe_computes_rva() {
        let mut text = FS_PATTERN.to_vec();
        text.extend(body_bytes());
        let pe = minimal_pe(&text);
        let a = locate(&pe).expect("唯一命中 + 体校验通过");
        assert_eq!(a.fs_rva, 0x1000); // rva = 节 rva + 节内偏移 0
        assert_eq!(a.fs_pattern, FS_PATTERN);
    }

    #[test]
    fn fingerprint_without_body_is_rejected() {
        let mut text = FS_PATTERN.to_vec();
        text.extend(vec![0xCC; BODY_CHECK_LEN]); // 无体校验字节
        assert_eq!(locate(&minimal_pe(&text)), Err(LocateError::NotFound));
    }

    #[test]
    fn duplicate_hits_are_ambiguous() {
        let mut text = FS_PATTERN.to_vec();
        text.extend(body_bytes());
        text.extend(FS_PATTERN.to_vec());
        text.extend(body_bytes());
        assert!(matches!(locate(&minimal_pe(&text)), Err(LocateError::Ambiguous(2))));
    }

    #[test]
    fn garbage_is_not_pe() {
        assert!(matches!(locate(&[0u8; 64]), Err(LocateError::NotPe(_))));
    }
}
