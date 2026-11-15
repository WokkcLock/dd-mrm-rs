//! 远程线程注入器 —— 移植自旧 C# `NativeInjector.TryInject`（用户实机验证过的流程，
//! 分阶段失败 Detail 原样保留，供 UI 直说卡在哪一步）。
//!
//! 流程（顺序有讲究）：
//! 1. 解析目标 PID（按进程名 = C# `GetProcessesByName`；或直接 PID）
//! 2. **先**写共享内存配置 `Local\DdModToolHotapplyCfg`（1024B，UTF-16 logDir），
//!    映射句柄存进进程级 static 保活 —— DLL InitThread 一醒来就 LogOpen 读它
//! 3. OpenProcess → VirtualAllocEx → 写入**宽字符** DLL 路径 →
//!    CreateRemoteThread(kernel32!LoadLibraryW) → 等 5s → 取线程退出码（≠0 即成功）
//!
//! 所有 W 函数参数一律宽字符（[`crate::win::wide_null`]）—— 旧工具的 ANSI
//! marshalling bug 教训：DLL 拿到乱码路径 → 一行日志都不写。
//!
//! LoadLibraryW 地址取自**本进程** kernel32（与目标同架构时基址一致 —— 经典假设，
//! 旧工具实机验证成立；跨架构注入不在本工具范围）。

use std::fmt;
use std::path::Path;
use std::sync::OnceLock;

use windows::Win32::Foundation::{CloseHandle, GetLastError, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
use windows::Win32::System::Diagnostics::Debug::WriteProcessMemory;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_WRITE, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, MapViewOfFile,
    PAGE_READWRITE, UnmapViewOfFile, VirtualAllocEx, VirtualFreeEx,
};
use windows::Win32::System::Threading::{
    CreateRemoteThread, GetExitCodeProcess, GetExitCodeThread, LPTHREAD_START_ROUTINE, OpenProcess,
    PROCESS_ACCESS_RIGHTS, WaitForSingleObject,
};
use windows::core::{s, w};

use crate::win::{wide_null, win32_code};

/// 配置共享内存名（= DdHotapply.c `CFG_MAPNAME`）。
pub const CFG_MAP_NAME: &str = r"Local\DdModToolHotapplyCfg";
/// 配置映射大小（= DdHotapply.c / 旧 C# `WriteCfg`：1024 字节）。
pub const CFG_MAP_SIZE: usize = 1024;

// ---- 配置块 v8 布局（DdHotapply.c `ReadCfg` 按同一布局解析）----
// v2..v7：多锚点时代（frame/vpush/persist/refresh/f1/savefile/…）。
// v8：B 路线瘦身 —— 唯一锚 fs_get_file_info；新增 remoteRoot（UI 下发的
// 存档根 = 用户配置的 save_root，DLL 磁盘真值回退路径用；留空 = 禁用回退，
// 主路径不受影响：游戏进程内逻辑 CreateFileW 本就走 Steam Auto-Cloud 重定向）。
// logDir 不再带 len 字段：DLL 侧按 NUL 结尾宽串解析。
pub const CFG_MAGIC: u32 = 0x5A4D_4444; // 字节序 44 44 4D 5A
pub const CFG_VERSION: u32 = 8;
/// flags bit0：fs_rva + fs_pattern 字段有效（None 时不置位，DLL 用默认值）。
pub const CFG_FLAG_HAS_ANCHORS: u32 = 1;
const OFF_MAGIC: usize = 0;
const OFF_VERSION: usize = 4;
const OFF_FLAGS: usize = 8;
const OFF_FSVA: usize = 12;
const OFF_PATTERN: usize = 16; // 16 字节
const OFF_REMOTEROOT: usize = 32; // UTF-16LE，256 码元（512 字节）
const OFF_LOGDIR: usize = 544; // UTF-16LE，剩余 (1024-544)/2 = 240 码元
/// remoteRoot 写入上限（含 NUL，码元数）。
const CFG_REMOTE_MAX_CHARS: usize = 256;
/// logDir 写入上限（含 NUL，码元数）。
const CFG_LOGDIR_MAX_CHARS: usize = (CFG_MAP_SIZE - OFF_LOGDIR) / 2;

/// 注入线程等待上限默认值（同旧 C# 的 5000ms）。
pub const DEFAULT_INJECT_THREAD_WAIT_MS: u32 = 5_000;

/// 注入目标：按进程名（如 "Darkest"，不带 .exe，大小写不敏感）或 PID。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessTarget {
    Name(String),
    Pid(u32),
}

/// 可调参数（裁定 4：所有等待可配置）。
#[derive(Debug, Clone)]
pub struct InjectOptions {
    /// CreateRemoteThread 后的 WaitForSingleObject 预算。
    pub thread_wait_ms: u32,
}

impl Default for InjectOptions {
    fn default() -> Self {
        Self {
            thread_wait_ms: DEFAULT_INJECT_THREAD_WAIT_MS,
        }
    }
}

/// 分阶段注入失败 —— `detail()` 输出与旧 C# 完全一致的字符串
/// （UI/日志直接可用；新增 `open-process-errN` 一档，原因见变体文档）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InjectFailure {
    /// 按名找不到进程 → "no-process"
    NoProcess,
    /// 进程句柄有效但已退出 → "process-exited"
    ProcessExited,
    /// OpenProcess 失败 → "open-process-errN"（旧 C# 借 Process 类隐式打开，
    /// 无此档；Rust 侧显式打开，新增一档保持粒度）
    OpenFailed(u32),
    /// VirtualAllocEx 返回 NULL → "virtualalloc-errN"
    VirtualAllocErr(u32),
    /// WriteProcessMemory 失败 → "wpm-errN"
    WpmErr(u32),
    /// 本进程 kernel32 里找不到 LoadLibraryW → "no-loadlibraryw"
    NoLoadLibraryW,
    /// CreateRemoteThread 失败 → "thread-errN"
    ThreadErr(u32),
    /// 远程线程 5s（可配）未结束 → "inject-timeout"
    InjectTimeout,
    /// LoadLibraryW 远程返回 0 → "loadlibrary-returned-0"
    LoadLibraryReturned0,
}

impl InjectFailure {
    /// 旧 C# 的 Detail 字符串，逐字节兼容。
    pub fn detail(&self) -> String {
        match self {
            InjectFailure::NoProcess => "no-process".into(),
            InjectFailure::ProcessExited => "process-exited".into(),
            InjectFailure::OpenFailed(c) => format!("open-process-err{c}"),
            InjectFailure::VirtualAllocErr(c) => format!("virtualalloc-err{c}"),
            InjectFailure::WpmErr(c) => format!("wpm-err{c}"),
            InjectFailure::NoLoadLibraryW => "no-loadlibraryw".into(),
            InjectFailure::ThreadErr(c) => format!("thread-err{c}"),
            InjectFailure::InjectTimeout => "inject-timeout".into(),
            InjectFailure::LoadLibraryReturned0 => "loadlibrary-returned-0".into(),
        }
    }
}

impl fmt::Display for InjectFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.detail())
    }
}

impl std::error::Error for InjectFailure {}

/// 进程级保活的配置映射句柄（旧 C# `_cfgMap` 语义：写完 UnmapView 但**句柄不关**，
/// 命名映射对象因此存活到工具进程结束，DLL 随时能 OpenFileMappingW）。
///
/// HANDLE 是裸指针不自动 Send/Sync；句柄只是内核对象 ID，跨线程持有安全。
struct SyncHandle(HANDLE);
unsafe impl Send for SyncHandle {}
unsafe impl Sync for SyncHandle {}

static CFG_MAPPING: OnceLock<SyncHandle> = OnceLock::new();

/// 注入入口（默认等待参数、无锚点覆盖）。见 [`try_inject_with`]。
pub fn try_inject(
    target: ProcessTarget,
    dll_path: &Path,
    log_dir: &Path,
) -> Result<(), InjectFailure> {
    try_inject_with(
        target,
        dll_path,
        log_dir,
        None,
        None,
        &InjectOptions::default(),
    )
}

/// 注入入口（全参数）。dll_path 必须是**绝对路径**（LoadLibraryW 在目标进程解析）；
/// log_dir 写进共享内存供 DLL 落日志；anchors = 定位结果（游戏版本参数），
/// Some 时 DLL 用它替换编译期默认 RVA/指纹，None 时 DLL 走默认值；
/// remote_root = UI 配置的存档根（DLL 磁盘真值回退路径），None 留空禁用回退。
pub fn try_inject_with(
    target: ProcessTarget,
    dll_path: &Path,
    log_dir: &Path,
    anchors: Option<&crate::locate::Anchors>,
    remote_root: Option<&Path>,
    opts: &InjectOptions,
) -> Result<(), InjectFailure> {
    let pid = resolve_pid(&target)?;
    // 先写配置再注入：DLL 的 InitThread 第一步就 ReadCfg/LogOpen 读它（顺序不可换）
    write_cfg(log_dir, anchors, remote_root);

    let access = PROCESS_ACCESS_RIGHTS(
        windows::Win32::System::Threading::PROCESS_CREATE_THREAD.0
            | windows::Win32::System::Threading::PROCESS_VM_OPERATION.0
            | windows::Win32::System::Threading::PROCESS_VM_WRITE.0
            | windows::Win32::System::Threading::PROCESS_VM_READ.0
            | windows::Win32::System::Threading::PROCESS_QUERY_LIMITED_INFORMATION.0,
    );
    let hproc = unsafe { OpenProcess(access, false, pid) }
        .map_err(|e| InjectFailure::OpenFailed(win32_code(&e)))?;
    // 打开成功 ≠ 活着：C# 在注入前查 HasExited，这里用退出码 == STILL_ACTIVE 等价检查
    let mut exit_code = 0u32;
    if unsafe { GetExitCodeProcess(hproc, &mut exit_code) }.is_ok() && exit_code != 259 {
        unsafe {
            let _ = CloseHandle(hproc);
        };
        return Err(InjectFailure::ProcessExited);
    }

    let result = inject_into(&hproc, dll_path, opts.thread_wait_ms);
    unsafe {
        let _ = CloseHandle(hproc);
    };
    result
}

/// 解析 PID：按名走 Toolhelp 快照（等价 C# GetProcessesByName：剥 .exe、
/// 大小写不敏感；多实例取第一个），直接 PID 原样返回。
fn resolve_pid(target: &ProcessTarget) -> Result<u32, InjectFailure> {
    match target {
        ProcessTarget::Pid(pid) => Ok(*pid),
        ProcessTarget::Name(_) => find_pid_by_name(target).ok_or(InjectFailure::NoProcess),
    }
}

/// 按进程名查 PID；顺带作为 `game_running` / verify 的进程探测原语。
pub fn find_pid_by_name(target: &ProcessTarget) -> Option<u32> {
    let want = match target {
        ProcessTarget::Name(name) => {
            let lower = name.to_ascii_lowercase();
            let lower = lower.strip_suffix(".exe").unwrap_or(&lower);
            lower.to_string()
        }
        ProcessTarget::Pid(_) => return None,
    };
    unsafe {
        let snap = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?;
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut found = None;
        if Process32FirstW(snap, &mut entry).is_ok() {
            loop {
                let exe_len = entry
                    .szExeFile
                    .iter()
                    .position(|&c| c == 0)
                    .unwrap_or(entry.szExeFile.len());
                let exe: String = String::from_utf16_lossy(&entry.szExeFile[..exe_len]);
                let exe = exe.to_ascii_lowercase();
                let exe = exe.strip_suffix(".exe").unwrap_or(&exe);
                if exe == want {
                    found = Some(entry.th32ProcessID);
                    break;
                }
                if Process32NextW(snap, &mut entry).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
        found
    }
}

/// 进程是否在跑（按名；verify/门面用）。
pub fn process_running(name: &str) -> bool {
    find_pid_by_name(&ProcessTarget::Name(name.to_string())).is_some()
}

/// 核心注入序列（hproc 已打开）。
///
/// 句柄与内存纪律（review 修复轮）：
/// - 远程**线程**句柄：所有路径（含每一条错误路径）都 CloseHandle；
/// - 远程**路径缓冲**：仅当远程线程确定已结束（等到 WAIT_OBJECT_0）或从未
///   启动（CreateRemoteThread 之前/本身失败）时才 VirtualFreeEx。
///   inject-timeout / WaitForSingleObject 失败时线程可能**仍在读路径串**，
///   此刻释放会让远端 LoadLibraryW 读到已释放内存 —— 宁可泄漏这一小块
///   （宽字符路径 <1KB），由目标进程生命周期兜底。
fn inject_into(hproc: &HANDLE, dll_path: &Path, wait_ms: u32) -> Result<(), InjectFailure> {
    // LoadLibraryW 参数必须是宽字符路径 + NUL
    let wide: Vec<u16> = wide_null(&dll_path.to_string_lossy());
    let bytes = wide.len() * 2;

    unsafe {
        let mem = VirtualAllocEx(
            *hproc,
            None,
            bytes,
            MEM_COMMIT | MEM_RESERVE,
            PAGE_READWRITE,
        );
        if mem.is_null() {
            return Err(InjectFailure::VirtualAllocErr(GetLastError().0));
        }

        // 返回 (结果, 远程缓冲可否释放)
        let run = || -> (Result<(), InjectFailure>, bool) {
            if let Err(e) = WriteProcessMemory(*hproc, mem, wide.as_ptr().cast(), bytes, None) {
                return (Err(InjectFailure::WpmErr(win32_code(&e))), true);
            }

            let k32 = match GetModuleHandleW(w!("kernel32.dll")) {
                Ok(h) => h,
                Err(_) => return (Err(InjectFailure::NoLoadLibraryW), true),
            };
            let load_library: LPTHREAD_START_ROUTINE = match GetProcAddress(k32, s!("LoadLibraryW"))
            {
                Some(f) => Some(std::mem::transmute::<
                    unsafe extern "system" fn() -> isize,
                    unsafe extern "system" fn(*mut core::ffi::c_void) -> u32,
                >(f)),
                None => return (Err(InjectFailure::NoLoadLibraryW), true),
            };

            let thread = match CreateRemoteThread(*hproc, None, 0, load_library, Some(mem), 0, None)
            {
                Ok(t) => t,
                Err(e) => return (Err(InjectFailure::ThreadErr(win32_code(&e))), true),
            };

            // 线程已启动：以下任何出口都必须关线程句柄
            let wait_result = WaitForSingleObject(thread, wait_ms);
            let (result, free_remote) = match wait_result {
                WAIT_OBJECT_0 => {
                    // 线程确定结束：取退出码，缓冲可释放
                    let mut code = 0u32;
                    let r = GetExitCodeThread(thread, &mut code)
                        .map_err(|e| InjectFailure::ThreadErr(win32_code(&e)));
                    let result = match r {
                        Err(e) => Err(e),
                        Ok(()) if code == 0 => Err(InjectFailure::LoadLibraryReturned0),
                        Ok(()) => Ok(()),
                    };
                    (result, true)
                }
                // 线程可能仍在跑 LoadLibraryW 并读路径串 → 不释放缓冲
                WAIT_TIMEOUT => (Err(InjectFailure::InjectTimeout), false),
                // 等待失败，线程状态未知 → 按仍在跑处理
                _ => (Err(InjectFailure::ThreadErr(GetLastError().0)), false),
            };
            let _ = CloseHandle(thread);
            (result, free_remote)
        };

        let (outcome, free_remote) = run();
        if free_remote {
            let _ = VirtualFreeEx(*hproc, mem, 0, MEM_RELEASE);
        }
        outcome
    }
}

/// 写共享内存配置（v8 布局）。失败静默 —— 与旧 C# 一致：
/// 写不进去最坏是 DLL 走编译期默认值、日志落回自己目录，不该阻断注入。
/// 幂等：首次创建后复用同一映射，重写内容（多次注入换参数的场景）。
pub fn write_cfg(
    log_dir: &Path,
    anchors: Option<&crate::locate::Anchors>,
    remote_root: Option<&Path>,
) {
    let handle = CFG_MAPPING
        .get_or_init(|| {
            let created = unsafe {
                let name = wide_null(CFG_MAP_NAME);
                CreateFileMappingW(
                    windows::Win32::Foundation::INVALID_HANDLE_VALUE,
                    None,
                    PAGE_READWRITE,
                    0,
                    CFG_MAP_SIZE as u32,
                    windows::core::PCWSTR(name.as_ptr()),
                )
            };
            SyncHandle(created.unwrap_or(windows::Win32::Foundation::HANDLE::default()))
        })
        .0;
    if handle.is_invalid() {
        return;
    }
    // 宽串字段各自截到容量内（含 NUL，不劈开宽字符）
    let trunc_wide = |p: &Path, max: usize| {
        let mut wide: Vec<u16> = wide_null(&p.to_string_lossy());
        if wide.len() > max {
            wide.truncate(max);
            *wide.last_mut().unwrap() = 0;
        }
        wide
    };
    let remote = remote_root.map(|p| trunc_wide(p, CFG_REMOTE_MAX_CHARS));
    let log_wide = trunc_wide(log_dir, CFG_LOGDIR_MAX_CHARS);
    unsafe {
        let view = MapViewOfFile(handle, FILE_MAP_WRITE, 0, 0, CFG_MAP_SIZE);
        if !view.Value.is_null() {
            let buf = std::slice::from_raw_parts_mut(view.Value.cast::<u8>(), CFG_MAP_SIZE);
            buf.fill(0);
            buf[OFF_MAGIC..OFF_MAGIC + 4].copy_from_slice(&CFG_MAGIC.to_le_bytes());
            buf[OFF_VERSION..OFF_VERSION + 4].copy_from_slice(&CFG_VERSION.to_le_bytes());
            let flags = if anchors.is_some() { CFG_FLAG_HAS_ANCHORS } else { 0 };
            buf[OFF_FLAGS..OFF_FLAGS + 4].copy_from_slice(&flags.to_le_bytes());
            if let Some(a) = anchors {
                buf[OFF_FSVA..OFF_FSVA + 4].copy_from_slice(&a.fs_rva.to_le_bytes());
                buf[OFF_PATTERN..OFF_PATTERN + 16].copy_from_slice(&a.fs_pattern);
            }
            // remoteRoot / logDir：NUL 结尾 UTF-16，DLL 侧按宽串解析
            let mut put = |off: usize, wide: &[u16]| {
                for (i, &ch) in wide.iter().enumerate() {
                    let o = off + i * 2;
                    buf[o..o + 2].copy_from_slice(&ch.to_le_bytes());
                }
            };
            if let Some(w) = &remote {
                put(OFF_REMOTEROOT, w);
            }
            put(OFF_LOGDIR, &log_wide);
            let _ = UnmapViewOfFile(view);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detail_strings_match_legacy_cs() {
        // 逐字节对齐旧 NativeInjector.cs 的 Detail 串
        assert_eq!(InjectFailure::NoProcess.detail(), "no-process");
        assert_eq!(InjectFailure::ProcessExited.detail(), "process-exited");
        assert_eq!(InjectFailure::OpenFailed(5).detail(), "open-process-err5");
        assert_eq!(
            InjectFailure::VirtualAllocErr(87).detail(),
            "virtualalloc-err87"
        );
        assert_eq!(InjectFailure::WpmErr(299).detail(), "wpm-err299");
        assert_eq!(InjectFailure::NoLoadLibraryW.detail(), "no-loadlibraryw");
        assert_eq!(InjectFailure::ThreadErr(8).detail(), "thread-err8");
        assert_eq!(InjectFailure::InjectTimeout.detail(), "inject-timeout");
        assert_eq!(
            InjectFailure::LoadLibraryReturned0.detail(),
            "loadlibrary-returned-0"
        );
    }

    #[test]
    fn find_pid_by_name_matches_own_process() {
        // 自身进程必然存在：exe 名（剥 .exe、大小写不敏感）应能找到某个 PID
        let my = std::env::current_exe().unwrap();
        let name = my.file_stem().unwrap().to_string_lossy().into_owned();
        let pid = find_pid_by_name(&ProcessTarget::Name(name.to_ascii_uppercase()));
        assert!(pid.is_some(), "own process {name} not found");
        // Pid 分支不做名字解析
        assert_eq!(find_pid_by_name(&ProcessTarget::Pid(0)), None);
    }

    #[test]
    fn no_process_detail_for_missing_name() {
        let r = try_inject(
            ProcessTarget::Name("dd-mrm-definitely-not-running-xyz".into()),
            Path::new(r"C:\nonexistent\DdHotapply.dll"),
            Path::new("."),
        );
        assert_eq!(r, Err(InjectFailure::NoProcess));
    }

    #[test]
    fn inject_into_dead_pid_gives_open_or_exited() {
        // PID 0 是空位（System Idle 在 0 上不可打开）—— 覆盖 Open/NoProcess 路径之一
        let r = try_inject(ProcessTarget::Pid(0), Path::new("x"), Path::new("."));
        assert!(matches!(
            r,
            Err(InjectFailure::OpenFailed(_)) | Err(InjectFailure::ProcessExited)
        ));
    }

    #[test]
    fn write_cfg_creates_mapping_and_is_idempotent() {
        let tmp = std::env::temp_dir().join(format!("dd-mrm-ha-cfg-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        write_cfg(&tmp, None, None);
        write_cfg(&tmp, None, None); // 二次写不 panic、不换映射
        read_cfg_and_assert(&tmp, None, None);
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn write_cfg_with_anchors_writes_layout() {
        let tmp = std::env::temp_dir().join(format!("dd-mrm-ha-cfg2-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let anchors = crate::locate::Anchors {
            fs_rva: 0x247F30,
            fs_pattern: crate::locate::FS_PATTERN,
        };
        let remote = std::path::Path::new("E:/steam/userdata/442034120/262060/remote");
        write_cfg(&tmp, Some(&anchors), Some(remote));
        read_cfg_and_assert(&tmp, Some(&anchors), Some(remote));
        std::fs::remove_dir_all(&tmp).ok();
    }

    /// DLL 视角验证：OpenFileMappingW(FILE_MAP_READ) 读回，逐字段比对。
    fn read_cfg_and_assert(
        log_dir: &std::path::Path,
        anchors: Option<&crate::locate::Anchors>,
        remote_root: Option<&std::path::Path>,
    ) {
        unsafe {
            use windows::Win32::System::Memory::{FILE_MAP_READ, OpenFileMappingW};
            let name = wide_null(CFG_MAP_NAME);
            let handle =
                OpenFileMappingW(FILE_MAP_READ.0, false, windows::core::PCWSTR(name.as_ptr()))
                    .expect("mapping should be openable like the DLL does");
            let view = MapViewOfFile(handle, FILE_MAP_READ, 0, 0, CFG_MAP_SIZE);
            assert!(
                !view.Value.is_null(),
                "map view should be readable like the DLL does"
            );
            let buf = std::slice::from_raw_parts(view.Value.cast::<u8>(), CFG_MAP_SIZE);
            let u32_at = |off: usize| u32::from_le_bytes(buf[off..off + 4].try_into().unwrap());
            let wide_at = |off: usize, max: usize| -> String {
                let mut units = Vec::new();
                for i in 0..max {
                    let o = off + i * 2;
                    let ch = u16::from_le_bytes([buf[o], buf[o + 1]]);
                    if ch == 0 {
                        break;
                    }
                    units.push(ch);
                }
                String::from_utf16_lossy(&units)
            };
            assert_eq!(u32_at(OFF_MAGIC), CFG_MAGIC);
            assert_eq!(u32_at(OFF_VERSION), CFG_VERSION);
            match anchors {
                Some(a) => {
                    assert_eq!(u32_at(OFF_FLAGS), CFG_FLAG_HAS_ANCHORS);
                    assert_eq!(u32_at(OFF_FSVA), a.fs_rva);
                    assert_eq!(&buf[OFF_PATTERN..OFF_PATTERN + 16], &a.fs_pattern);
                }
                None => assert_eq!(u32_at(OFF_FLAGS), 0),
            }
            match remote_root {
                Some(p) => assert_eq!(
                    wide_at(OFF_REMOTEROOT, CFG_REMOTE_MAX_CHARS),
                    p.to_string_lossy()
                ),
                None => assert_eq!(u32_at(OFF_REMOTEROOT), 0),
            }
            assert_eq!(
                wide_at(OFF_LOGDIR, CFG_LOGDIR_MAX_CHARS),
                log_dir.to_string_lossy()
            );
            let _ = UnmapViewOfFile(view);
            let _ = CloseHandle(handle);
        }
    }
}
