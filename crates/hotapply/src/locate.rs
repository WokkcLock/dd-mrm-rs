//! 游戏版本自适应定位 —— 从 Darkest.exe **文件字节**解出热应用所需锚点。
//!
//! 背景（2026-11-05 Steam 更新事件）：DLL 的三个 RVA（帧状态机 / fPush /
//! fPersist）与入口指纹是按单个 exe 版本硬编码的，游戏一更新就全体失效，
//! 每次都要人工重新逆向。参数化 v2 把"适配"变成工具注入前的一次毫秒级
//! 计算：读游戏 exe → 用多重独立字节锚点定位三个函数 → 随配置共享内存
//! 传给 DLL（见 `injector::write_cfg`）。DLL 侧零逻辑变化，只是常量改
//! 从配置读（默认值 = 2026-11-05 的 1c67048e 版，兼容无配置注入场景）。
//!
//! ## 锚点选型（全部来自对 1c67048e 版的逆向实证，字节级断言）
//!
//! | 目标 | 锚点组合 | 设计理由 |
//! |------|---------|---------|
//! | fPush | ① `add [reg+0x210],0xC0`（向量 end 前移一条目 192B）② 同函数 `mov r,[reg+0x210]` ③ 同函数 `cmp r,[reg+0x218]`（vector begin/end/cap 三指针，容器布局多年未变）④ 同函数 ≥10 次 `0F 11`（movups 拷 192B 条目）| 0x210/0xC0/0x218 三常量同现 + 批量 movups，是 push_back 语义指纹 |
//! | fPersist | ① .rdata 引用 `"steam_profile_"` 串 ② 引用 `"profile_"` 串 ③ 同函数 `imul …,0xC10`（ENV_SIZE=3088）| 档名格式化 + 信封步长，两个域常量 |
//! | 帧状态机 | ① `mov/lea [reg+0x1768]`（信封数组在 this+0x1768）② 同函数 `[reg+0x1764]` ③ 同函数 `mov [reg+0x1f40],5`（状态转移特异字面量）| 相邻结构偏移 + 特异状态字面量 |
//!
//! 每个锚点都要求**唯一**命中函数；任何一环多候选/零命中 → [`LocateError`]
//! （带候选 RVA 诊断）→ 工具退回注入默认值路径（DLL 指纹校验照拦），
//! 失败形态是安全退化，不是错误绑定。
//!
//! 本模块纯函数、无 I/O、无 windows 依赖：输入文件字节，输出锚点。

use std::collections::BTreeSet;
use std::fmt;

/// 入口指纹长度（= DdHotapply.c `FRAME_PATTERN`）。
pub const PATTERN_LEN: usize = 16;

/// 定位结果 —— 经配置共享内存传给 DLL 的锚点集。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Anchors {
    /// 帧状态机（帧钩目标，thunk 每帧捕获 this=屏对象）。
    pub frame_rva: u32,
    /// fPush(容器, 192B 条目) —— 官方推入。
    pub vpush_rva: u32,
    /// fPersist(屏对象, 槽位) —— 官方落盘。
    pub persist_rva: u32,
    /// frame_rva 处的 16 字节入口指纹（DLL 装钩前校验，防定位错绑）。
    pub frame_pattern: [u8; PATTERN_LEN],
}

/// 定位失败 —— 每种都直说坏在哪一环（诊断串进日志/UI）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocateError {
    /// 不是可解析的 PE（缺 MZ/PE 签名/非 PE32+ x64）。
    NotPe(&'static str),
    /// 缺必备节（.text/.rdata/.pdata）。
    NoSection(&'static str),
    /// 锚点字节零命中（游戏结构大改，或锚点失效）。
    AnchorMissing(&'static str),
    /// 锚点多候选 —— 拒绝猜测（带候选函数 RVA 供诊断）。
    AnchorAmbiguous(&'static str, Vec<u32>),
}

impl fmt::Display for LocateError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LocateError::NotPe(w) => write!(f, "not-a-pe: {w}"),
            LocateError::NoSection(w) => write!(f, "missing section {w}"),
            LocateError::AnchorMissing(w) => write!(f, "anchor missing: {w}"),
            LocateError::AnchorAmbiguous(w, cands) => {
                let list = cands
                    .iter()
                    .map(|r| format!("0x{r:X}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                write!(f, "anchor ambiguous: {} ({} candidates: {list})", w, cands.len())
            }
        }
    }
}

impl std::error::Error for LocateError {}

// ---------------- 最小 PE 解析 ----------------

struct Section {
    name: String,
    rva: u32,
    vsize: u32,
    raw_ptr: u32,
    raw_size: u32,
}

struct Pe<'a> {
    buf: &'a [u8],
    sections: Vec<Section>,
}

fn u16_at(b: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes([*b.get(off)?, *b.get(off + 1)?]))
}

fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    let c = b.get(off..off + 4)?;
    Some(u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
}

impl<'a> Pe<'a> {
    fn parse(buf: &'a [u8]) -> Result<Self, LocateError> {
        if u16_at(buf, 0).ok_or(LocateError::NotPe("no MZ"))? != 0x5A4D {
            return Err(LocateError::NotPe("no MZ"));
        }
        let lfanew = u32_at(buf, 0x3C).ok_or(LocateError::NotPe("truncated"))? as usize;
        if u32_at(buf, lfanew).ok_or(LocateError::NotPe("no PE sig"))? != 0x0000_4550 {
            return Err(LocateError::NotPe("no PE sig"));
        }
        let machine = u16_at(buf, lfanew + 4).ok_or(LocateError::NotPe("truncated"))?;
        if machine != 0x8664 {
            return Err(LocateError::NotPe("not x64"));
        }
        let nsec = u16_at(buf, lfanew + 6).ok_or(LocateError::NotPe("truncated"))? as usize;
        let opt_size = u16_at(buf, lfanew + 20).ok_or(LocateError::NotPe("truncated"))? as usize;
        if nsec == 0 || nsec > 96 {
            return Err(LocateError::NotPe("bad section count"));
        }
        let opt_magic = u16_at(buf, lfanew + 24).ok_or(LocateError::NotPe("truncated"))?;
        if opt_magic != 0x20B {
            return Err(LocateError::NotPe("not PE32+"));
        }
        let table = lfanew + 24 + opt_size;
        let mut sections = Vec::with_capacity(nsec);
        for i in 0..nsec {
            let s = table + i * 40;
            let raw = buf.get(s..s + 40).ok_or(LocateError::NotPe("truncated"))?;
            let name_len = raw[..8].iter().position(|&b| b == 0).unwrap_or(8);
            let name = String::from_utf8_lossy(&raw[..name_len]).into_owned();
            sections.push(Section {
                name,
                vsize: u32::from_le_bytes([raw[8], raw[9], raw[10], raw[11]]),
                rva: u32::from_le_bytes([raw[12], raw[13], raw[14], raw[15]]),
                raw_size: u32::from_le_bytes([raw[16], raw[17], raw[18], raw[19]]),
                raw_ptr: u32::from_le_bytes([raw[20], raw[21], raw[22], raw[23]]),
            });
        }
        Ok(Self { buf, sections })
    }

    fn section(&self, name: &str) -> Option<&Section> {
        self.sections.iter().find(|s| s.name == name)
    }

    /// 节的文件字节（PointerToRawData..+SizeOfRawData，界内安全截断）。
    fn sec_bytes(&self, s: &Section) -> &'a [u8] {
        let start = s.raw_ptr as usize;
        let end = start.saturating_add(s.raw_size as usize);
        let end = end.min(self.buf.len());
        self.buf.get(start..end).unwrap_or(&[])
    }

    /// RVA → 文件偏移（仅映射到节原始数据内；虚拟填充区返回 None）。
    fn file_off(&self, rva: u32) -> Option<usize> {
        let s = self
            .sections
            .iter()
            .find(|s| rva >= s.rva && rva < s.rva + s.raw_size.max(s.vsize))?;
        let off = s.raw_ptr as u64 + (rva as u64 - s.rva as u64);
        if off >= s.raw_ptr as u64 + s.raw_size as u64 || off as usize + PATTERN_LEN > self.buf.len() {
            return None;
        }
        Some(off as usize)
    }
}

// ---------------- .pdata 函数边界 ----------------

/// RUNTIME_FUNCTION (begin, end) 列表（x64 异常表，按 begin 升序去重）。
fn parse_funcs(pdata: &[u8]) -> Vec<(u32, u32)> {
    let mut v: Vec<(u32, u32)> = pdata
        .chunks_exact(12)
        .filter_map(|c| {
            let b = u32::from_le_bytes([c[0], c[1], c[2], c[3]]);
            let e = u32::from_le_bytes([c[4], c[5], c[6], c[7]]);
            (b != 0 && e > b).then_some((b, e))
        })
        .collect();
    v.sort_unstable();
    v.dedup();
    v
}

/// RVA → 所在函数起点（二分；不在任何区间 → None）。
fn func_of(funcs: &[(u32, u32)], rva: u32) -> Option<u32> {
    let i = funcs.partition_point(|&(b, _)| b <= rva);
    if i == 0 {
        return None;
    }
    let (b, e) = funcs[i - 1];
    (rva < e).then_some(b)
}

// ---------------- 字节模式匹配原语 ----------------
// 布局约定（本模块只认这三种，寄存器位一律通配）：
//  A. REX.W+op+modrm+disp32（mov/lea/cmp 等内存引用）：  REX op modrm d0 d1 d2 d3
//     REX&W=1（0x48..0x4F），modrm mod=10 且 rm≠100（rm=100 是 SIB 形式，布局不同）
//  B. REX.W+op+modrm+disp32+imm32（add/mov [mem],imm）：  REX op modrm d0 d1 d2 d3 i0 i1 i2 i3
//  C. REX.W+69+modrm+imm32（imul r,r/m,imm32，寄存器形式）： REX 69 modrm(11) i0 i1 i2 i3

fn is_rex_w(b: u8) -> bool {
    b & 0xF8 == 0x48
}

/// modrm 是 mod=10（[reg+disp32]，disp32 在 modrm 后）且基址为单寄存器
/// （rm≠100 是 SIB 形式，布局不同，排除）。rm=101 此处是 rbp 基址，合法保留。
fn modrm_disp32(modrm: u8) -> bool {
    modrm >> 6 == 0b10 && (modrm & 7) != 0b100
}

/// A：`[reg+disp32]` 内存引用（op ∈ mov-load 8B / lea 8D / store 89 / cmp 3B,39）。
/// REX.W 前缀可选：编译器对 32 位访问（`8B 87 …` mov eax,[rdi+d]）省略 REX，
/// 对 64 位访问（`48 8B 87 …` mov rax,[rdi+d]）带 REX；锚点只关心 disp，两者都收
/// （实测帧函数内 0x1768 引用带 REX、0x1764 引用不带）。
fn hits_mem_disp32(text: &[u8], text_rva: u32, opcodes: &[u8], disp: u32) -> Vec<u32> {
    const N: usize = 7;
    let mut v = Vec::new();
    if text.len() < N {
        return v;
    }
    let d = disp.to_le_bytes();
    for i in 0..=text.len() - N {
        let b = &text[i..];
        let hit = if is_rex_w(b[0]) {
            opcodes.contains(&b[1]) && modrm_disp32(b[2]) && b[3..7] == d
        } else {
            opcodes.contains(&b[0]) && modrm_disp32(b[1]) && b[2..6] == d
        };
        if hit {
            v.push(text_rva + i as u32);
        }
    }
    v
}

/// B：`add/mov qword [reg+disp32], imm32`（op=81 add / C7 mov，reg 字段必为 0）。
/// REX.W 前缀可选（理由同上：dword 写 `C7 87 …` 与 qword 写 `48 C7 87 …` 都收）。
fn hits_mem_imm32(text: &[u8], text_rva: u32, opcode: u8, disp: u32, imm: u32) -> Vec<u32> {
    const N: usize = 11;
    let mut v = Vec::new();
    if text.len() < N {
        return v;
    }
    let d = disp.to_le_bytes();
    let m = imm.to_le_bytes();
    for i in 0..=text.len() - N {
        let b = &text[i..];
        let hit = if is_rex_w(b[0]) {
            b[1] == opcode
                && modrm_disp32(b[2])
                && b[2] & 0x38 == 0
                && b[3..7] == d
                && b[7..11] == m
        } else {
            b[0] == opcode
                && modrm_disp32(b[1])
                && b[1] & 0x38 == 0
                && b[2..6] == d
                && b[6..10] == m
        };
        if hit {
            v.push(text_rva + i as u32);
        }
    }
    v
}

/// C：`imul r64, r/m64, imm32`（mod=11 寄存器形式）。
fn hits_imul_imm32(text: &[u8], text_rva: u32, imm: u32) -> Vec<u32> {
    const N: usize = 7;
    let mut v = Vec::new();
    if text.len() < N {
        return v;
    }
    let m = imm.to_le_bytes();
    for i in 0..=text.len() - N {
        let b = &text[i..];
        if is_rex_w(b[0]) && b[1] == 0x69 && b[2] & 0xC0 == 0xC0 && b[3..7] == m {
            v.push(text_rva + i as u32);
        }
    }
    v
}

/// `lea r64, [rip+disp32]`（REX 8D modrm&0xC7==0x05）→ (指令 RVA, 目标 RVA)。
fn hits_lea_rip(text: &[u8], text_rva: u32) -> Vec<(u32, u32)> {
    const N: usize = 7;
    let mut v = Vec::new();
    if text.len() < N {
        return v;
    }
    for i in 0..=text.len() - N {
        let b = &text[i..];
        if is_rex_w(b[0]) && b[1] == 0x8D && b[2] & 0xC7 == 0x05 {
            let disp = i32::from_le_bytes([b[3], b[4], b[5], b[6]]);
            let insn_rva = text_rva + i as u32;
            let target = (insn_rva as i64 + N as i64 + disp as i64) as u32;
            v.push((insn_rva, target));
        }
    }
    v
}

/// 朴素子串计数（movups 断言用）。
fn count_sub(hay: &[u8], pat: &[u8]) -> usize {
    if pat.is_empty() || hay.len() < pat.len() {
        return 0;
    }
    hay.windows(pat.len()).filter(|w| *w == pat).count()
}

/// .rdata 内 ASCII+NUL 字符串的 RVA（全部出现；"profile_" 会内嵌于
/// "steam_profile_"，由调用方按需过滤）。
fn find_str_rvas(rdata: &[u8], rdata_rva: u32, needle: &[u8]) -> Vec<u32> {
    rdata
        .windows(needle.len())
        .enumerate()
        .filter(|(_, w)| *w == needle)
        .map(|(i, _)| rdata_rva + i as u32)
        .collect()
}

/// 命中 RVA 集合 → 所在函数起点集合。
fn funcs_of_hits(funcs: &[(u32, u32)], hits: &[u32]) -> BTreeSet<u32> {
    hits.iter().filter_map(|&r| func_of(funcs, r)).collect()
}

/// 唯一性裁定：恰 1 → Ok；0 → AnchorMissing；多 → AnchorAmbiguous（带候选）。
fn exactly_one(what: &'static str, set: BTreeSet<u32>) -> Result<u32, LocateError> {
    match set.len() {
        1 => Ok(set.into_iter().next().unwrap()),
        0 => Err(LocateError::AnchorMissing(what)),
        _ => Err(LocateError::AnchorAmbiguous(what, set.into_iter().collect())),
    }
}

// ---------------- 主定位流程 ----------------

/// 对 Darkest.exe 文件字节定位三个锚点。多重独立断言 + 唯一性要求；
/// 任一环失手返回错误（安全退化，绝不猜）。
pub fn locate(buf: &[u8]) -> Result<Anchors, LocateError> {
    let pe = Pe::parse(buf)?;
    let text = pe.section(".text").ok_or(LocateError::NoSection(".text"))?;
    let rdata = pe.section(".rdata").ok_or(LocateError::NoSection(".rdata"))?;
    let pdata = pe.section(".pdata").ok_or(LocateError::NoSection(".pdata"))?;
    let tb = pe.sec_bytes(text);
    let trva = text.rva;
    let rb = pe.sec_bytes(rdata);
    let rrva = rdata.rva;
    let funcs = parse_funcs(pe.sec_bytes(pdata));

    // ---- ① fPush：vector 三指针（0x210 end / 0x218 cap）+ 0xC0 步长 + movups 搬运 ----
    // 实测撞车：vector 扩容辅助函数（fPush 满容量时 call 之）同样具备
    // add[+0x210],0xC0 / mov[+0x210] / cmp[+0x218] / movups 搬运。语义区分：
    // fPush 只步进 end 绝不写回 begin（[+0x208]），扩容函数必然写回新 begin。
    let fpush_cands: BTreeSet<u32> = funcs_of_hits(
        &funcs,
        &hits_mem_imm32(tb, trva, 0x81, 0x210, 0xC0),
    )
    .intersection(&funcs_of_hits(&funcs, &hits_mem_disp32(tb, trva, &[0x8B], 0x210)))
    .cloned()
    .collect::<BTreeSet<u32>>()
    .intersection(&funcs_of_hits(&funcs, &hits_mem_disp32(tb, trva, &[0x3B, 0x39], 0x218)))
    .cloned()
    .collect::<BTreeSet<u32>>()
    .into_iter()
    .filter(|&f| {
        let (lo, hi) = match func_range(&funcs, f) {
            Some(r) => r,
            None => return false,
        };
        let s = func_slice(tb, trva, lo, hi);
        // 12×movups(192B=12×16B load+store)：store 形式 0F 11 至少 10 次
        count_sub(s, &[0x0F, 0x11]) >= 10
            // 绝不写 begin（[+0x208]）——写了就是扩容辅助，不是 fPush
            && hits_mem_disp32(s, lo, &[0x89], 0x208).is_empty()
            // 步进指令（add …,0xC0）紧邻函数尾（≤64B）：真 fPush 是薄叶子
            // （拷贝→步进→收尾）；内联了 push_back 的大函数步进后还有
            // 成片后续逻辑（实测撞车 0x38AE82 步进后还剩 204B）
            && hits_mem_imm32(s, lo, 0x81, 0x210, 0xC0)
                .into_iter()
                .any(|add_rva| hi - add_rva <= 64)
    })
    .collect();
    let vpush_rva = exactly_one("fPush", fpush_cands)?;

    // ---- ② fPersist：profile_%d / steam_profile_%d 字符串引用 + imul 0xC10（ENV_SIZE）----
    let steam_rvas = find_str_rvas(rb, rrva, b"steam_profile_%d\0");
    if steam_rvas.is_empty() {
        return Err(LocateError::AnchorMissing("steam_profile_ string"));
    }
    let steam_set: BTreeSet<u32> = steam_rvas.iter().copied().collect();
    // 独立 "profile_%d" = 排除 steam_profile_%d 内嵌项（steam_rva+6 处）
    let prof_set: BTreeSet<u32> = find_str_rvas(rb, rrva, b"profile_%d\0")
        .into_iter()
        .filter(|p| !steam_set.contains(&p.saturating_sub(6)))
        .collect();
    if prof_set.is_empty() {
        return Err(LocateError::AnchorMissing("profile_ string"));
    }
    let lea = hits_lea_rip(tb, trva);
    let refs_steam: Vec<u32> = lea
        .iter()
        .filter(|(_, t)| steam_set.contains(t))
        .map(|&(i, _)| i)
        .collect();
    let refs_prof: Vec<u32> = lea
        .iter()
        .filter(|(_, t)| prof_set.contains(t))
        .map(|&(i, _)| i)
        .collect();
    let persist_cands: BTreeSet<u32> = funcs_of_hits(&funcs, &refs_steam)
        .intersection(&funcs_of_hits(&funcs, &refs_prof))
        .cloned()
        .collect::<BTreeSet<u32>>()
        .into_iter()
        .filter(|&f| {
            // 同函数必须 imul …,0xC10（信封步长）——排除只引用字符串的无关函数
            let (lo, hi) = match func_range(&funcs, f) {
                Some(r) => r,
                None => return false,
            };
            let s = func_slice(tb, trva, lo, hi);
            !hits_imul_imm32(s, lo, 0xC10).is_empty()
        })
        .collect();
    let persist_rva = exactly_one("fPersist", persist_cands)?;

    // ---- ③ 帧状态机：this+0x1768（信封数组）+ this+0x1764 + [reg+0x1f40]=5 ----
    let frame_cands: BTreeSet<u32> = funcs_of_hits(
        &funcs,
        &hits_mem_disp32(tb, trva, &[0x8B, 0x8D, 0x89, 0x3B, 0x39], 0x1768),
    )
    .intersection(&funcs_of_hits(
        &funcs,
        &hits_mem_disp32(tb, trva, &[0x8B, 0x8D, 0x89, 0x3B, 0x39], 0x1764),
    ))
    .cloned()
    .collect::<BTreeSet<u32>>()
    .intersection(&funcs_of_hits(&funcs, &hits_mem_imm32(tb, trva, 0xC7, 0x1F40, 5)))
    .cloned()
    .collect();
    let frame_rva = exactly_one("frame", frame_cands)?;

    // ---- 交叉验证：三个函数互不相同（同 RVA 说明锚点撞车，拒绝）----
    if frame_rva == vpush_rva || frame_rva == persist_rva || vpush_rva == persist_rva {
        return Err(LocateError::AnchorAmbiguous(
            "cross-check (functions collide)",
            vec![frame_rva, vpush_rva, persist_rva],
        ));
    }

    // ---- 入口指纹 ----
    let off = pe
        .file_off(frame_rva)
        .ok_or(LocateError::AnchorMissing("frame bytes (virtual-only?)"))?;
    let mut frame_pattern = [0u8; PATTERN_LEN];
    frame_pattern.copy_from_slice(&pe.buf[off..off + PATTERN_LEN]);

    Ok(Anchors {
        frame_rva,
        vpush_rva,
        persist_rva,
        frame_pattern,
    })
}

/// 函数起点 → (begin, end) 区间（调用频率低，线性查找足够）。
fn func_range(funcs: &[(u32, u32)], begin: u32) -> Option<(u32, u32)> {
    funcs.iter().find(|&&(b, _)| b == begin).copied()
}

/// .text 内 [lo, hi) 的字节切片。
fn func_slice<'a>(tb: &'a [u8], trva: u32, lo: u32, hi: u32) -> &'a [u8] {
    let s = (lo.saturating_sub(trva)) as usize;
    let e = (hi.saturating_sub(trva)) as usize;
    tb.get(s..e.min(tb.len())).unwrap_or(&[])
}

#[cfg(test)]
mod tests {
    use super::*;

    // ---------------- 合成 PE 构造器 ----------------
    // 只为锚点算法服务：不追求可加载，只求 PE 头/节表/.pdata 结构合法。

    const SEC_ALIGN: u32 = 0x1000;
    const FILE_ALIGN: u32 = 0x200;

    struct PeBuilder {
        sections: Vec<(String, Vec<u8>)>,
    }

    impl PeBuilder {
        fn new() -> Self {
            Self { sections: Vec::new() }
        }

        fn add(&mut self, name: &str, data: Vec<u8>) -> &mut Self {
            self.sections.push((name.to_string(), data));
            self
        }

        /// 生成节内布局：返回各节 (rva, raw_ptr)。
        fn layout(&self) -> Vec<(u32, u32)> {
            let hdr = 0x138 + 40 * self.sections.len() as u32;
            let mut raw = hdr.div_ceil(FILE_ALIGN) * FILE_ALIGN;
            let mut out = Vec::new();
            for (i, (_, data)) in self.sections.iter().enumerate() {
                let rva = (i as u32 + 1) * SEC_ALIGN;
                out.push((rva, raw));
                raw += data.len() as u32;
                raw = raw.div_ceil(FILE_ALIGN) * FILE_ALIGN;
            }
            out
        }

        fn build(&self) -> Vec<u8> {
            let layout = self.layout();
            let size_of_image = SEC_ALIGN * (self.sections.len() as u32 + 1);
            let mut buf = vec![0u8; 0x2000];
            // DOS 头：MZ + e_lfanew=0x40
            buf[0] = b'M';
            buf[1] = b'Z';
            buf[0x3C..0x40].copy_from_slice(&0x40u32.to_le_bytes());
            // NT 头：sig(4) + FileHeader(20) + OptionalHeader PE32+(0xF0)
            let sig = 0x40;
            buf[sig..sig + 4].copy_from_slice(&0x0000_4550u32.to_le_bytes()); // PE\0\0
            buf[sig + 4..sig + 6].copy_from_slice(&0x8664u16.to_le_bytes()); // Machine x64
            buf[sig + 6..sig + 8].copy_from_slice(&(self.sections.len() as u16).to_le_bytes());
            buf[sig + 20..sig + 22].copy_from_slice(&0xF0u16.to_le_bytes()); // SizeOfOptionalHeader
            let opt = sig + 24; // OptionalHeader 起点
            buf[opt..opt + 2].copy_from_slice(&0x20Bu16.to_le_bytes()); // PE32+ magic
            buf[opt + 56..opt + 60].copy_from_slice(&size_of_image.to_le_bytes());
            buf[opt + 60..opt + 64].copy_from_slice(&SEC_ALIGN.to_le_bytes());
            buf[opt + 64..opt + 68].copy_from_slice(&FILE_ALIGN.to_le_bytes());
            let mut p = opt + 0xF0;
            for ((name, data), (rva, raw)) in self.sections.iter().zip(&layout) {
                let mut hdr = [0u8; 40];
                let nb = name.as_bytes();
                hdr[..nb.len()].copy_from_slice(nb);
                hdr[8..12].copy_from_slice(&(data.len() as u32).to_le_bytes()); // VirtualSize
                hdr[12..16].copy_from_slice(&rva.to_le_bytes());
                hdr[16..20].copy_from_slice(&(data.len() as u32).to_le_bytes()); // SizeOfRawData
                hdr[20..24].copy_from_slice(&raw.to_le_bytes());
                buf[p..p + 40].copy_from_slice(&hdr);
                p += 40;
                let raw = *raw as usize;
                let end = raw + data.len();
                if end > buf.len() {
                    buf.resize(end, 0);
                }
                buf[raw..end].copy_from_slice(data);
            }
            buf
        }
    }

    /// 手工 .text：五个函数（fPush / fPersist / 帧 / 两个干扰）。
    /// 返回 (text 字节, 各函数 .text 内偏移)。
    fn build_text() -> (Vec<u8>, [usize; 5]) {
        let mut t: Vec<u8> = Vec::new();
        let offs = [0x0000usize, 0x0100, 0x0200, 0x0300, 0x0400];
        // funcA @0x0000 —— 真 fPush
        let a = offs[0];
        t.extend(vec![0xCC; a - t.len()]);
        t.extend([0x48, 0x8B, 0x87, 0x10, 0x02, 0x00, 0x00]); // mov rdx,[rdi+0x210]
        t.extend([0x48, 0x3B, 0x97, 0x18, 0x02, 0x00, 0x00]); // cmp rdx,[rdi+0x218]
        for _ in 0..12 {
            t.extend([0x0F, 0x11, 0x07]); // movups [rdi], xmm0（占位搬运）
        }
        t.extend([0x48, 0x81, 0x87, 0x10, 0x02, 0x00, 0x00, 0xC0, 0x00, 0x00, 0x00]); // add [rdi+0x210],0xC0
        // funcB @0x0100 —— 真 fPersist（lea 引用两字符串 + imul 0xC10 + mov edx,0x20）
        // 字符串目标在 .rdata（第 2 节 RVA=0x2000：profile_@+0，steam_profile_@+16）
        let b = offs[1];
        t.extend(vec![0xCC; b - t.len()]);
        let b_rva = 0x1000 + b as u32;
        // lea rcx,[rip+X]：disp = target - (insn_rva+7)
        let prof_target = 0x2000;
        let steam_target = 0x2000 + 16;
        let d1 = (prof_target as i64 - (b_rva as i64 + 7)) as i32;
        let d2 = (steam_target as i64 - (b_rva as i64 + 7 + 7)) as i32;
        t.extend([0x48, 0x8D, 0x0D]);
        t.extend(d1.to_le_bytes());
        t.extend([0x48, 0x8D, 0x15]);
        t.extend(d2.to_le_bytes());
        t.extend([0x48, 0x69, 0xF7, 0x10, 0x0C, 0x00, 0x00]); // imul rdi,rsi,0xC10
        t.extend([0xBA, 0x20, 0x00, 0x00, 0x00]); // mov edx,0x20
        // funcC @0x0200 —— 真帧函数
        let c = offs[2];
        t.extend(vec![0xCC; c - t.len()]);
        t.extend([0x48, 0x8B, 0x87, 0x68, 0x17, 0x00, 0x00]); // mov rax,[rdi+0x1768]
        t.extend([0x4C, 0x8D, 0x8F, 0x68, 0x17, 0x00, 0x00]); // lea r9,[rdi+0x1768]
        t.extend([0x48, 0x8B, 0x97, 0x64, 0x17, 0x00, 0x00]); // mov rdx,[rdi+0x1764]
        t.extend([0x48, 0xC7, 0x87, 0x40, 0x1F, 0x00, 0x00, 0x05, 0x00, 0x00, 0x00]); // mov [rdi+0x1f40],5
        // funcD @0x0300 —— 干扰：只有 add [rsi+0x210],0xC0（无 mov/cmp/movups）
        let d = offs[3];
        t.extend(vec![0xCC; d - t.len()]);
        t.extend([0x48, 0x81, 0x86, 0x10, 0x02, 0x00, 0x00, 0xC0, 0x00, 0x00, 0x00]);
        // funcE @0x0400 —— 干扰：lea 引用 profile_ + imul 0xC10，但无 steam 引用
        let e = offs[4];
        t.extend(vec![0xCC; e - t.len()]);
        let e_rva = 0x1000 + e as u32;
        let d3 = (prof_target as i64 - (e_rva as i64 + 7)) as i32;
        t.extend([0x48, 0x8D, 0x0D]);
        t.extend(d3.to_le_bytes());
        t.extend([0x48, 0x69, 0xF7, 0x10, 0x0C, 0x00, 0x00]);
        // 统一补齐到 0x480：保证所有函数区间（含末尾函数）都在节 raw 数据内
        t.extend(vec![0xCC; 0x480 - t.len()]);
        (t, offs)
    }

    fn build_synthetic() -> (Vec<u8>, Anchors) {
        let (text, offs) = build_text();
        let mut rdata = Vec::new();
        rdata.extend_from_slice(b"profile_%d\0");
        rdata.extend(vec![0u8; 16 - rdata.len()]);
        rdata.extend_from_slice(b"steam_profile_%d\0");
        rdata.extend_from_slice(b"persist.game.json\0");
        // .pdata：五函数边界（区间收紧到 0x40：内容 ≤61B，避免 padding
        // 拉大"步进指令到函数尾"的距离，贴合真 fPush 的薄叶子形态）
        let mut pdata = Vec::new();
        for &o in &offs {
            let begin = 0x1000 + o as u32;
            let end = begin + 0x40;
            pdata.extend_from_slice(&begin.to_le_bytes());
            pdata.extend_from_slice(&end.to_le_bytes());
            pdata.extend_from_slice(&0u32.to_le_bytes()); // UnwindInfo（不用）
        }
        let mut pb = PeBuilder::new();
        pb.add(".text", text);
        pb.add(".rdata", rdata);
        pb.add(".pdata", pdata);
        pb.add(".reloc", vec![0u8; 16]);
        let buf = pb.build();
        // 期望值：fPush=第一个函数，fPersist=第二个，帧=第三个；
        // pattern = 帧函数前 16B（两条 7B 指令 + 第三条前 2B）
        let expect = [
            0x48u8, 0x8B, 0x87, 0x68, 0x17, 0x00, 0x00, 0x4C, 0x8D, 0x8F, 0x68, 0x17, 0x00, 0x00,
            0x48, 0x8B,
        ];
        (
            buf,
            Anchors {
                frame_rva: 0x1000 + offs[2] as u32,
                vpush_rva: 0x1000 + offs[0] as u32,
                persist_rva: 0x1000 + offs[1] as u32,
                frame_pattern: expect,
            },
        )
    }

    #[test]
    fn synthetic_pe_full_pipeline() {
        let (buf, expect) = build_synthetic();
        let got = locate(&buf).expect("synthetic PE should locate");
        assert_eq!(got, expect, "\ngot {got:?}\nexpect {expect:?}");
    }

    #[test]
    fn non_pe_rejected() {
        assert!(matches!(locate(b"not a pe"), Err(LocateError::NotPe(_))));
        // MZ 但无 PE 头
        let mut b = vec![0u8; 0x200];
        b[0] = b'M';
        b[1] = b'Z';
        assert!(matches!(locate(&b), Err(LocateError::NotPe(_))));
    }

    #[test]
    fn missing_anchor_detected() {
        // 抹掉 steam_profile_%d 字符串 → fPersist 锚 Missing
        let (mut buf, _) = build_synthetic();
        let needle = b"steam_profile_%d\0";
        let pos = buf
            .windows(needle.len())
            .position(|w| w == needle)
            .expect("needle present");
        for i in 0..needle.len() {
            buf[pos + i] = b'X';
        }
        assert!(matches!(
            locate(&buf),
            Err(LocateError::AnchorMissing(w)) if w.contains("steam")
        ));
    }

    #[test]
    fn ambiguous_anchor_detected() {
        // 复制帧函数（含全部三锚）到 funcE 区域 → frame 锚 Ambiguous
        let (mut buf, _) = build_synthetic();
        // .text 是第 1 节：rva 0x1000。函数 C @0x0200，E @0x0400。
        // 找 raw：直接在 buf 里搜帧函数特征序列再整体复制 32B 不行（三锚分散）——
        // 简化：把 funcC 的 32B 序列（两个 0x1768 + 0x1764 + 0x1f40=5 恰在前 32B）拷到 E。
        let text_raw = buf
            .windows(4)
            .position(|w| w == [0x68, 0x17, 0x00, 0x00])
            .expect("0x1768 disp present");
        let c_start = text_raw - 3; // REX 前缀
        let e_start = c_start + 0x200; // funcE = funcC + 0x200
        let chunk: [u8; 32] = buf[c_start..c_start + 32].try_into().unwrap();
        buf[e_start..e_start + 32].copy_from_slice(&chunk);
        assert!(matches!(
            locate(&buf),
            Err(LocateError::AnchorAmbiguous(w, _)) if w.contains("frame")
        ));
    }

    /// 真机验证（本地 --ignored 跑；默认找本机 Steam 安装，DD_TEST_EXE 可覆盖）：
    /// `cargo test -p hotapply --lib locate:: -- --ignored --nocapture`
    /// 预期值 = 2026-11-05 逆向定位的 1c67048e 版三锚点。
    #[test]
    #[ignore = "needs local Darkest.exe (set DD_TEST_EXE to override)"]
    fn real_darkest_exe_1c67048e() {
        let path = std::env::var("DD_TEST_EXE").unwrap_or_else(|_| {
            r"F:\SteamLibrary\steamapps\common\DarkestDungeon\_windows\win64\Darkest.exe".into()
        });
        let buf = std::fs::read(&path).unwrap_or_else(|e| panic!("read {path}: {e}"));
        let a = locate(&buf).expect("locate should succeed on real exe");
        assert_eq!(a.frame_rva, 0x418AA0, "frame {a:?}");
        assert_eq!(a.vpush_rva, 0x3F3810, "vpush {a:?}");
        assert_eq!(a.persist_rva, 0x41A430, "persist {a:?}");
        assert_eq!(
            a.frame_pattern,
            [
                0x48, 0x8b, 0xc4, 0x48, 0x89, 0x58, 0x10, 0x48, 0x89, 0x70, 0x18, 0x55, 0x57, 0x41,
                0x56, 0x48
            ]
        );
    }
}
