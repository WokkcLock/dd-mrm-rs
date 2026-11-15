//! B 路线注入编排 —— UI 应用流程的唯一入口（替代旧管道 client）。
//!
//! 职责：游戏存活检测 → 从运行中的 Darkest.exe 现场 locate 锚点 →
//! 写 v8 配置（logDir + remoteRoot）→ 远程线程注入。管道通信已退役：
//! DLL 注入即自活（InitThread 读配置→指纹校验→装钩），二次注入天然幂等
//! （补丁字节让指纹失配 → ABORT 日志不重钩）。

use std::fmt;
use std::path::{Path, PathBuf};

use windows::core::PWSTR;
use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_FORMAT, PROCESS_QUERY_LIMITED_INFORMATION,
    QueryFullProcessImageNameW,
};

use crate::injector::{
    DEFAULT_INJECT_THREAD_WAIT_MS, InjectOptions, ProcessTarget, find_pid_by_name,
    process_running, try_inject_with,
};
use crate::locate::{self, Anchors};

/// 游戏进程名（不带 .exe，大小写不敏感）。
pub const GAME_PROCESS: &str = "Darkest";

/// 游戏是否在运行（UI 状态栏轮询用）。
pub fn game_running() -> bool {
    process_running(GAME_PROCESS)
}

/// 挂守卫失败的原因（UI 直接展示 detail 文案）。
#[derive(Debug)]
pub enum HookError {
    /// 游戏未运行（调用方应先判 game_running 再走这里）。
    GameNotRunning,
    /// 随行 DLL 找不到。
    DllNotFound(String),
    /// 进程 exe 读取 / 锚点定位失败（版本不符或权限问题）。
    Locate(String),
    /// 注入链失败（OpenProcess/写内存/CreateRemoteThread…，detail 对齐旧 C#）。
    Inject(String),
}

impl HookError {
    pub fn detail(&self) -> String {
        match self {
            HookError::GameNotRunning => "游戏未运行".into(),
            HookError::DllNotFound(s) => format!("找不到 DLL：{s}"),
            HookError::Locate(s) => format!("锚点定位失败：{s}"),
            HookError::Inject(s) => format!("注入失败：{s}"),
        }
    }
}

impl fmt::Display for HookError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.detail())
    }
}

/// 进程主模块完整路径（QueryFullProcessImageNameW）。
fn exe_path_of(pid: u32) -> Result<PathBuf, String> {
    unsafe {
        let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid)
            .map_err(|e| format!("OpenProcess({pid}) 失败：{e}"))?;
        let mut buf = [0u16; 1024];
        let mut len = buf.len() as u32;
        let r = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_FORMAT(0),
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
            .map_err(|e| format!("QueryFullProcessImageNameW({pid}) 失败：{e}"));
        CloseHandle(handle).ok();
        r?;
        let path = String::from_utf16_lossy(&buf[..len as usize]);
        Ok(PathBuf::from(path))
    }
}

/// 确保运行中的游戏挂上 size 守卫（幂等：已挂时 DLL 侧指纹失配自动跳过）。
///
/// 失败一律返回错误、不做任何兜底 —— 调用方决定是否照常写盘
/// （应用流程的约定：守卫挂不上就不写，避免裸写撞陈旧 size）。
pub fn ensure_hooked(log_dir: &Path, remote_root: Option<&Path>) -> Result<(), HookError> {
    let pid = find_pid_by_name(&ProcessTarget::Name(GAME_PROCESS.to_string()))
        .ok_or(HookError::GameNotRunning)?;
    let exe = exe_path_of(pid).map_err(HookError::Locate)?;
    let bytes = std::fs::read(&exe)
        .map_err(|e| HookError::Locate(format!("读 {} 失败：{e}", exe.display())))?;
    let anchors: Anchors = locate::locate(&bytes)
        .map_err(|e| HookError::Locate(format!("{}：{e}", exe.display())))?;
    let dll = crate::dll::locate_dll().map_err(|e| HookError::DllNotFound(e.to_string()))?;
    try_inject_with(
        ProcessTarget::Pid(pid),
        &dll,
        log_dir,
        Some(&anchors),
        remote_root,
        &InjectOptions {
            thread_wait_ms: DEFAULT_INJECT_THREAD_WAIT_MS,
        },
    )
    .map_err(|e| HookError::Inject(e.detail().to_string()))
}
