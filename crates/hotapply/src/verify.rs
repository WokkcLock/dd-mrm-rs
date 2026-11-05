//! verify-inject：无游戏的注入冒烟（= 旧 native/smoke.ps1 的 Rust 移植）。
//!
//! 流程：起 notepad → 注入真实 DLL → notepad 没有 DdHotapply 引擎的特征码
//! （FRAME_PATTERN@RVA 0x418AA0，游戏构建 1c67048e），DLL 在 init 里记
//! `fatal: frame pattern mismatch` 后**干净退出**（不崩宿主、不建管道）——
//! 这正是通过条件。断言：
//! 1. 日志出现 `init enter`（注入+日志链路通）
//! 2. 日志出现 `fatal: frame pattern mismatch`（特征码检查按预期拒绝）
//! 3. 宿主 notepad 仍存活（DLL 未拖垮进程）
//!
//! 需要 GUI 会话 + notepad（GC8：真机测试用户侧；本机开发会话可直接跑）。

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use crate::dll::locate_dll;
use crate::injector::{InjectOptions, ProcessTarget, try_inject_with};

/// 日志文件名（= DdHotapply.c LogOpen 固定 engine.log）。
pub const ENGINE_LOG_NAME: &str = "engine.log";
/// init 首行。
pub const MARK_INIT_ENTER: &str = "init enter";
/// 特征码不匹配（= 冒烟通过条件）。
pub const MARK_PATTERN_MISMATCH: &str = "fatal: frame pattern mismatch";

/// 冒烟结果（example/集成测试各自断言）。
#[derive(Debug, Clone)]
pub struct NotepadVerifyReport {
    pub dll_path: PathBuf,
    pub log_dir: PathBuf,
    pub saw_init_enter: bool,
    pub saw_pattern_mismatch: bool,
    /// 检查时宿主 notepad 是否仍存活。
    pub host_alive_at_check: bool,
    /// 注入线程是否顺利结束（TryInject Ok）。
    pub inject_ok: bool,
    /// engine.log 全文（诊断用）。
    pub engine_log: String,
}

impl NotepadVerifyReport {
    /// 三条件全绿才算 PASS。
    pub fn pass(&self) -> bool {
        self.saw_init_enter
            && self.saw_pattern_mismatch
            && self.host_alive_at_check
            && self.inject_ok
    }
}

/// 跑一次 notepad 冒烟。Err = 硬失败（DLL 找不到/notepad 起不来/注入失败）；
/// Ok 内的三条件由调用方断言。
pub fn run_notepad_verification(
    log_dir: &Path,
    dll_override: Option<&Path>,
) -> Result<NotepadVerifyReport, String> {
    let dll_path = match dll_override {
        Some(p) => p.to_path_buf(),
        None => locate_dll().map_err(|e| e.to_string())?,
    };
    std::fs::create_dir_all(log_dir).map_err(|e| format!("log dir: {e}"))?;
    // 清掉旧日志：断言必须针对本次运行
    let _ = std::fs::remove_file(log_dir.join(ENGINE_LOG_NAME));

    let mut child = Command::new("notepad.exe")
        .spawn()
        .map_err(|e| format!("spawn notepad: {e}"))?;
    let result = watch_child(&mut child, &dll_path, log_dir);
    // 无论成败都收尸：杀掉 + 等（notepad 可能已被人手关掉，kill 失败无所谓）
    let _ = child.kill();
    let _ = child.wait();
    result
}

fn watch_child(
    child: &mut Child,
    dll_path: &Path,
    log_dir: &Path,
) -> Result<NotepadVerifyReport, String> {
    // notepad 刚 spawn：确认它活着、没闪退（300ms 让窗口/模块加载稳定）
    match child.try_wait() {
        Ok(Some(_)) => return Err("notepad exited before injection".into()),
        Ok(None) => {}
        Err(e) => return Err(format!("notepad wait: {e}")),
    }
    std::thread::sleep(Duration::from_millis(300));

    // cfg（logDir）先于注入写入 —— try_inject_with 内部处理。
    // notepad 宿主无游戏锚点 → anchors=None，DLL 走编译期默认值路径。
    let inject = try_inject_with(
        ProcessTarget::Pid(child.id()),
        dll_path,
        log_dir,
        None,
        &InjectOptions::default(),
    );
    let inject_ok = inject.is_ok();
    if let Err(f) = inject {
        return Err(format!("inject failed: {}", f.detail()));
    }

    // DLL init：cfg 读取 → init enter → 特征码检查 → mismatch 干净退出
    let saw_init = wait_for_log(log_dir, MARK_INIT_ENTER, Duration::from_secs(6));
    let saw_mismatch = wait_for_log(log_dir, MARK_PATTERN_MISMATCH, Duration::from_secs(6));
    let host_alive = matches!(child.try_wait(), Ok(None));

    Ok(NotepadVerifyReport {
        dll_path: dll_path.to_path_buf(),
        log_dir: log_dir.to_path_buf(),
        saw_init_enter: saw_init,
        saw_pattern_mismatch: saw_mismatch,
        host_alive_at_check: host_alive,
        inject_ok,
        engine_log: read_engine_log(log_dir),
    })
}

/// 轮询等待 engine.log 包含 needle（50ms 间隔；超时 false）。
pub fn wait_for_log(log_dir: &Path, needle: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if read_engine_log(log_dir).contains(needle) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    false
}

/// 读 engine.log 全文（不存在 → 空串；读失败 → 空串）。
pub fn read_engine_log(log_dir: &Path) -> String {
    std::fs::read_to_string(log_dir.join(ENGINE_LOG_NAME)).unwrap_or_default()
}
