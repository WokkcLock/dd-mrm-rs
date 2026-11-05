//! 原始块类型：64B Header / 16B Meta1Entry / 12B Meta2Entry / fieldInfo 位域。
//!
//! Header 字节序按上游规格（dson-spec-robojumper.md Header 表，本仓库的
//! 权威字节序来源）；语义等式（meta1Offset==64 等）见 dson-bit31-notes §2。
//! Header 保留原始 64 字节：写出时只回填布局字段，zeroes 区域逐字节原样。

use crate::error::{DsonError, Result};

/// 文件魔数 `01 B1 00 00`（LE i32 = 0xB101）。
pub const MAGIC: [u8; 4] = [0x01, 0xB1, 0x00, 0x00];

/// Header 固定长度（headerLength 字段恒等于 64，meta1Offset 恒等于 64）。
pub const HEADER_SIZE: usize = 64;

/// meta1 条目字节数。
pub const META1_ENTRY_SIZE: usize = 16;

/// meta2 条目字节数。
pub const META2_ENTRY_SIZE: usize = 12;

fn rd_i32(b: &[u8], off: usize) -> Result<i32> {
    if off + 4 > b.len() {
        return Err(DsonError::new(format!(
            "cannot read i32 at byte {off}: file has only {} bytes",
            b.len()
        )));
    }
    let mut a = [0u8; 4];
    a.copy_from_slice(&b[off..off + 4]);
    Ok(i32::from_le_bytes(a))
}

fn rd_u32(b: &[u8], off: usize) -> Result<u32> {
    Ok(rd_i32(b, off)? as u32)
}

// ---- fieldInfo 位域（u32）-------------------------------------------------
//
// bit0      = isObject
// bits 2-10 = 字段名字节数（含结尾 NUL），9 bit，最大 511
// bits11-30 = 对象的 meta1 下标（20 bit）；非对象恒 0
// bit31     = 内容历史状态位，逐条目按树路径从源文件搬运（dson-bit31-notes §1）

/// fieldInfo 的 bit0：是否对象。
pub fn fi_is_object(fi: u32) -> bool {
    fi & 1 == 1
}

/// fieldInfo 的 bits2-10：字段名字节数（含 NUL）。
pub fn fi_name_len(fi: u32) -> u32 {
    (fi >> 2) & 0x1FF
}

/// fieldInfo 的 bits11-30：对象对应的 meta1 下标（非对象无意义，恒 0）。
pub fn fi_meta1_idx(fi: u32) -> u32 {
    (fi >> 11) & 0xFFFFF
}

/// fieldInfo 的 bit31。
pub fn fi_bit31(fi: u32) -> bool {
    fi >> 31 == 1
}

/// 组装 fieldInfo。`name_len_with_nul` 与 `meta1_idx` 超出位域宽度时返回 None。
pub fn field_info(
    is_object: bool,
    name_len_with_nul: usize,
    meta1_idx: u32,
    bit31: bool,
) -> Option<u32> {
    let nl = u32::try_from(name_len_with_nul).ok()?;
    if nl == 0 || nl > 0x1FF {
        return None;
    }
    if meta1_idx > 0xFFFFF {
        return None;
    }
    Some((u32::from(is_object)) | (nl << 2) | (meta1_idx << 11) | (u32::from(bit31) << 31))
}

/// 把 4 字节小端写进 buf[off..off+4]。
fn wr_i32(buf: &mut [u8], off: usize, v: i32) {
    buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
}

// ---- Header ---------------------------------------------------------------

/// 64 字节 Header：保留原始字节，只暴露校验过的字段。
#[derive(Debug, Clone)]
pub struct Header {
    raw: [u8; HEADER_SIZE],
    // 校验/推导后的字段（构造时已验证全部布局等式）
    num_meta1: usize,
    num_meta2: usize,
    data_length: usize,
    data_offset: usize,
    meta2_offset: usize,
}

impl Header {
    /// 从文件头 64 字节解析并校验全部布局等式（dson-bit31-notes §2）。
    pub fn read(bytes: &[u8]) -> Result<Header> {
        if bytes.len() < HEADER_SIZE {
            return Err(DsonError::new(format!(
                "not a DSON file: {} bytes is shorter than the 64-byte header",
                bytes.len()
            )));
        }
        if bytes[0..4] != MAGIC {
            return Err(DsonError::new(format!(
                "not a DSON file: bad magic {:02x?} (expected 01 b1 00 00)",
                &bytes[0..4]
            )));
        }
        let header_length = rd_i32(bytes, 8)?;
        if header_length != HEADER_SIZE as i32 {
            return Err(DsonError::new(format!(
                "unsupported header: headerLength={header_length} (expected 64)"
            )));
        }
        let meta1_offset = rd_i32(bytes, 24)?;
        if meta1_offset != HEADER_SIZE as i32 {
            return Err(DsonError::new(format!(
                "unsupported header: meta1Offset={meta1_offset} (expected 64)"
            )));
        }
        let meta1_size = rd_i32(bytes, 16)?;
        let num_meta1 = rd_i32(bytes, 20)?;
        if meta1_size < 0 || num_meta1 < 1 {
            return Err(DsonError::new(format!(
                "malformed header: meta1Size={meta1_size}, numMeta1Entries={num_meta1}"
            )));
        }
        if meta1_size
            != num_meta1
                .checked_mul(META1_ENTRY_SIZE as i32)
                .ok_or_else(|| DsonError::new("malformed header: meta1Size overflow"))?
        {
            return Err(DsonError::new(format!(
                "malformed header: meta1Size={meta1_size} != numMeta1Entries({num_meta1})*16"
            )));
        }
        let num_meta1 = num_meta1 as usize;
        let meta2_offset = 64 + meta1_size as usize;
        if rd_i32(bytes, 48)? != meta2_offset as i32 {
            return Err(DsonError::new(format!(
                "malformed header: meta2Offset={} != 64+meta1Size({meta2_offset})",
                rd_i32(bytes, 48)?
            )));
        }
        let num_meta2 = rd_i32(bytes, 44)?;
        if num_meta2 < 1 {
            return Err(DsonError::new(format!(
                "malformed header: numMeta2Entries={num_meta2}"
            )));
        }
        let num_meta2 = num_meta2 as usize;
        let meta2_end = meta2_offset
            .checked_add(
                num_meta2
                    .checked_mul(META2_ENTRY_SIZE)
                    .expect("bounded by i32"),
            )
            .ok_or_else(|| DsonError::new("malformed header: meta2 end overflow"))?;
        let data_offset = rd_i32(bytes, 60)?;
        if data_offset < 0 || data_offset as usize != meta2_end {
            return Err(DsonError::new(format!(
                "malformed header: dataOffset={data_offset} != meta2Offset+numMeta2Entries*12({meta2_end})"
            )));
        }
        let data_length = rd_i32(bytes, 56)?;
        if data_length < 1 {
            return Err(DsonError::new(format!(
                "malformed header: dataLength={data_length}"
            )));
        }
        let file_end = (data_offset as usize)
            .checked_add(data_length as usize)
            .ok_or_else(|| DsonError::new("malformed header: file end overflow"))?;
        if file_end != bytes.len() {
            return Err(DsonError::new(format!(
                "malformed header: file is {} bytes but header says data ends at {file_end} \
                 (truncated or trailing bytes)",
                bytes.len()
            )));
        }
        Ok(Header {
            raw: bytes[..HEADER_SIZE].try_into().expect("length checked"),
            num_meta1,
            num_meta2,
            data_length: data_length as usize,
            data_offset: data_offset as usize,
            meta2_offset,
        })
    }

    /// revision 原始 4 字节（原样保留写回）。
    pub fn revision_bytes(&self) -> [u8; 4] {
        self.raw[4..8].try_into().expect("slice length")
    }

    /// 对象条目数（meta1）。
    pub fn num_meta1(&self) -> usize {
        self.num_meta1
    }

    /// 字段条目数（meta2）。
    pub fn num_meta2(&self) -> usize {
        self.num_meta2
    }

    /// 数据区长度。
    pub fn data_length(&self) -> usize {
        self.data_length
    }

    /// 数据区绝对偏移。
    pub fn data_offset(&self) -> usize {
        self.data_offset
    }

    /// meta2 区绝对偏移。
    pub fn meta2_offset(&self) -> usize {
        self.meta2_offset
    }

    /// 写出用：在原样字节上回填布局字段（新文档的 numMeta1/numMeta2/dataLength）。
    pub fn to_bytes_patched(
        &self,
        num_meta1: usize,
        num_meta2: usize,
        data_length: usize,
    ) -> Result<[u8; HEADER_SIZE]> {
        patch_header_raw(&self.raw, num_meta1, num_meta2, data_length)
    }
}

/// 在原样 64 字节 header 上回填布局字段（writer 直接调用，不经过 Header 校验）。
///
/// 只动 16(meta1Size)/20(numMeta1)/44(numMeta2)/48(meta2Offset)/
/// 56(dataLength)/60(dataOffset) 六个 i32；magic、revision、zeroes 区
/// 逐字节保留（§2 原样保留）。
pub fn patch_header_raw(
    raw: &[u8; HEADER_SIZE],
    num_meta1: usize,
    num_meta2: usize,
    data_length: usize,
) -> Result<[u8; HEADER_SIZE]> {
    let n1 = i32::try_from(num_meta1)
        .map_err(|_| DsonError::new("writer: too many objects for i32 header field"))?;
    let n2 = i32::try_from(num_meta2)
        .map_err(|_| DsonError::new("writer: too many fields for i32 header field"))?;
    let dl = i32::try_from(data_length)
        .map_err(|_| DsonError::new("writer: data block too large for i32 header field"))?;
    let m1s = n1
        .checked_mul(META1_ENTRY_SIZE as i32)
        .ok_or_else(|| DsonError::new("writer: meta1Size overflow"))?;
    let m2o = 64i32
        .checked_add(m1s)
        .ok_or_else(|| DsonError::new("writer: meta2Offset overflow"))?;
    let m2s = n2
        .checked_mul(META2_ENTRY_SIZE as i32)
        .ok_or_else(|| DsonError::new("writer: meta2 block size overflow"))?;
    let d_o = m2o
        .checked_add(m2s)
        .ok_or_else(|| DsonError::new("writer: dataOffset overflow"))?;
    let mut out = *raw;
    wr_i32(&mut out, 16, m1s); // meta1Size
    wr_i32(&mut out, 20, n1); // numMeta1Entries
    wr_i32(&mut out, 44, n2); // numMeta2Entries
    wr_i32(&mut out, 48, m2o); // meta2Offset
    wr_i32(&mut out, 56, dl); // dataLength
    wr_i32(&mut out, 60, d_o); // dataOffset
    Ok(out)
}

// ---- Meta1 / Meta2 原始条目 ------------------------------------------------

/// meta1 条目（16B）：仅对象有条目，先序。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Meta1Entry {
    /// 父对象的 meta1 下标；根为 -1。
    pub parent_index: i32,
    /// 该对象自身的 meta2 下标。
    pub meta2_entry_idx: i32,
    /// 直接子字段数（含叶子与对象）。
    pub num_direct_children: i32,
    /// 全部后代字段数（不含自身）。
    pub num_all_children: i32,
}

impl Meta1Entry {
    /// 从 16 字节解析。
    pub fn read(b: &[u8]) -> Result<Meta1Entry> {
        Ok(Meta1Entry {
            parent_index: rd_i32(b, 0)?,
            meta2_entry_idx: rd_i32(b, 4)?,
            num_direct_children: rd_i32(b, 8)?,
            num_all_children: rd_i32(b, 12)?,
        })
    }

    /// 编码为 16 字节。
    pub fn to_bytes(&self) -> [u8; META1_ENTRY_SIZE] {
        let mut b = [0u8; META1_ENTRY_SIZE];
        b[0..4].copy_from_slice(&self.parent_index.to_le_bytes());
        b[4..8].copy_from_slice(&self.meta2_entry_idx.to_le_bytes());
        b[8..12].copy_from_slice(&self.num_direct_children.to_le_bytes());
        b[12..16].copy_from_slice(&self.num_all_children.to_le_bytes());
        b
    }
}

/// meta2 条目（12B）：nameHash / offset（数据区内相对）/ fieldInfo。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Meta2Entry {
    /// 字段名哈希（ddHash，不含 NUL）。
    pub name_hash: i32,
    /// 字段原始区域在数据块内的起始偏移。
    pub offset: i32,
    /// 原始 fieldInfo（bit31 逐位保留的来源）。
    pub field_info: u32,
}

impl Meta2Entry {
    /// 从 12 字节解析。
    pub fn read(b: &[u8]) -> Result<Meta2Entry> {
        Ok(Meta2Entry {
            name_hash: rd_i32(b, 0)?,
            offset: rd_i32(b, 4)?,
            field_info: rd_u32(b, 8)?,
        })
    }

    /// 编码为 12 字节。
    pub fn to_bytes(&self) -> [u8; META2_ENTRY_SIZE] {
        let mut b = [0u8; META2_ENTRY_SIZE];
        b[0..4].copy_from_slice(&self.name_hash.to_le_bytes());
        b[4..8].copy_from_slice(&self.offset.to_le_bytes());
        b[8..12].copy_from_slice(&self.field_info.to_le_bytes());
        b
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_info_roundtrip() {
        let fi = field_info(true, 10, 3, true).unwrap();
        assert!(fi_is_object(fi));
        assert_eq!(fi_name_len(fi), 10);
        assert_eq!(fi_meta1_idx(fi), 3);
        assert!(fi_bit31(fi));
        // 位域精确值：1 | 10<<2 | 0<<11 | 1<<31
        assert_eq!(field_info(true, 10, 0, true).unwrap(), 0x8000_0029);
        assert_eq!(field_info(false, 8, 0, false).unwrap(), 0x0000_0020);
        // 金样本 spot：profile_2 'version'（nameLen 8，叶子，b31=1）= bit31|8<<2
        assert_eq!(field_info(false, 8, 0, true).unwrap(), 0x8000_0020);
    }

    #[test]
    fn field_info_width_limits() {
        assert!(field_info(false, 0, 0, false).is_none()); // 至少 1（NUL）
        assert!(field_info(false, 511, 0, false).is_some());
        assert!(field_info(false, 512, 0, false).is_none()); // 超 9 bit
        assert!(field_info(true, 8, 0x10_0000, false).is_none()); // 超 20 bit
        assert!(field_info(true, 8, 0xFFFFF, false).is_some());
    }

    #[test]
    fn meta_entry_roundtrip() {
        let m1 = Meta1Entry {
            parent_index: -1,
            meta2_entry_idx: 0,
            num_direct_children: 15,
            num_all_children: 72,
        };
        assert_eq!(Meta1Entry::read(&m1.to_bytes()).unwrap(), m1);
        let m2 = Meta2Entry {
            name_hash: dd_hash_for_test(),
            offset: 942,
            field_info: 0x8000_0195,
        };
        assert_eq!(Meta2Entry::read(&m2.to_bytes()).unwrap(), m2);
    }

    fn dd_hash_for_test() -> i32 {
        crate::hash::dd_hash("9")
    }
}
