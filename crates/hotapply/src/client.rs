//! 门面 —— T5（UI）只许消费这里 + 重新导出的类型。
//!
//! 移植旧 C# `HotapplyClient.ApplyModsAsync` 的编排（frida 分支废弃，不走）：
//! 1. 先试连管道（引擎可能已在游戏里）
//! 2. 连不上 → 定位 DLL → 注入（logDir 先写共享内存）→ 重连
//! 3. 发 apply 请求 → 解析回包；各失败档映射为工具侧状态串
//!
//! 工具侧状态串（与旧 C# 对齐 + 修复轮扩充）：`engine-off:<注入Detail>` /
//! `game-not-running` / `engine-off`（注入成功但管道仍连不上）/ `timeout` /
//! `io-error: <细节>`（仍以 io-error 开头，兼容旧判断）/ `dll-not-found`
//! （完整搜索路径列表写进工具日志）/ `bad-entry`（裸花括号）/ `entry-too-long`
//! （会被 DLL 静默截断）/ `too-many-entries` / `line-too-large`；成功时透传
//! DLL 状态（`applied` / `applied-no-persist` / `no-frame` / …）。

use std::fmt::Write as _;
use std::path::PathBuf;
use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use crate::dll::{DllDiscoveryError, locate_dll};
use crate::injector::{InjectOptions, ProcessTarget, process_running, try_inject_with};
use crate::pipe::{
    EncodeError, MAX_ENTRIES, ModEntry, PipeClient, PipeError, Reply, Request, sanitize_state,
};

/// 默认管道连接超时（裁定 4：可配，默认 3s）。
pub const DEFAULT_CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// 默认 apply 往返超时（同上，10s）。
pub const DEFAULT_APPLY_TIMEOUT: Duration = Duration::from_secs(10);
/// 默认游戏进程名（旧 C# 固定 "Darkest"）。
pub const DEFAULT_GAME_PROCESS: &str = "Darkest";
/// 工具侧日志的环境变量覆盖。
pub const LOG_DIR_ENV: &str = "DDHOTAPPLY_LOG";

/// 门面配置。全字段可改，Default = 旧 C# 行为。
#[derive(Debug, Clone)]
pub struct ClientConfig {
    /// 管道连接超时（首次探测与注入后重连共用）。
    pub connect_timeout: Duration,
    /// apply 请求往返预算（写+读+解析）。
    pub apply_timeout: Duration,
    /// 注入远程线程等待（透传 [`InjectOptions::thread_wait_ms`]）。
    pub inject_thread_wait_ms: u32,
    /// 游戏进程名（按名注入与存活探测用）。
    pub game_process: String,
    /// DLL 路径覆盖（None → [`crate::dll::locate_dll`] 搜索序）。
    pub dll_override: Option<PathBuf>,
    /// 日志目录覆盖（None → `DDHOTAPPLY_LOG` → `%TEMP%\DdModToolHotapply`）。
    pub log_dir: Option<PathBuf>,
}

impl Default for ClientConfig {
    fn default() -> Self {
        Self {
            connect_timeout: DEFAULT_CONNECT_TIMEOUT,
            apply_timeout: DEFAULT_APPLY_TIMEOUT,
            inject_thread_wait_ms: InjectOptions::default().thread_wait_ms,
            game_process: DEFAULT_GAME_PROCESS.to_string(),
            dll_override: None,
            log_dir: None,
        }
    }
}

/// apply 结果。`state` 兼收并包：DLL 状态串或工具侧状态串（见模块文档），
/// 已净化（去控制字符、截 160 字）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApplyOutcome {
    pub ok: bool,
    pub state: String,
    /// DLL 回包的 count（仅 ok=true 时有；失败/工具侧错误为 None）。
    pub count: Option<i64>,
}

impl ApplyOutcome {
    fn tool_state(state: impl Into<String>) -> Self {
        Self {
            ok: false,
            state: sanitize_state(&state.into()),
            count: None,
        }
    }
}

/// 请求 id 计数器（旧 C# 固定 id:1；这里自增便于日志对账，DLL 原样回显）。
static NEXT_ID: AtomicI64 = AtomicI64::new(1);

/// 热应用门面。
///
/// ```
/// use hotapply::{ClientConfig, HotapplyClient, ModEntry, DEFAULT_GAME_PROCESS};
///
/// let client = HotapplyClient::new(); // 默认配置 = 旧 C# 行为
/// let tuned = HotapplyClient::with_config(ClientConfig {
///     connect_timeout: std::time::Duration::from_secs(4),
///     ..Default::default()
/// });
/// assert_eq!(tuned.config().game_process, DEFAULT_GAME_PROCESS);
/// // 发送前协议校验（裸花括号破坏 DLL 行解析；超容量会被 DLL 静默截断）
/// assert!(ModEntry { title: "Mod A".into(), version: "1.0".into() }.check_pipe_safe().is_ok());
/// assert!(ModEntry { title: "Mod A".into(), version: "1.0".into() }.check_entry_capacity().is_ok());
/// ```
///
/// 注：`apply()` 有真实副作用（找不到引擎时会注入 DLL）——不在 doctest 里跑，
/// 端到端验证见 `tests/notepad_smoke.rs` 与真机测试（GC8）。
#[derive(Debug, Clone, Default)]
pub struct HotapplyClient {
    config: ClientConfig,
}

impl HotapplyClient {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_config(config: ClientConfig) -> Self {
        Self { config }
    }

    pub fn config(&self) -> &ClientConfig {
        &self.config
    }

    /// 游戏进程在跑？（= 旧 C# `IsGameRunning`）
    pub fn game_running(&self) -> bool {
        process_running(&self.config.game_process)
    }

    /// 引擎就绪探测：连上管道并收到 `status → ready`。连不上/回异常即 false。
    pub fn engine_ready(&self) -> bool {
        let mut pipe = match PipeClient::connect(self.config.connect_timeout) {
            Ok(p) => p,
            Err(_) => return false,
        };
        matches!(
            pipe.request(&Request::Status { id: next_id() }, self.config.apply_timeout),
            Ok(Reply { ok: true, state, .. }) if state == "ready"
        )
    }

    /// 热应用一次：确保注入 → 连接 → apply。永不 panic、永不 Err：一切结果
    /// 都在 [`ApplyOutcome`] 里（UI 只管展示 state）。
    pub fn apply(&self, save_path: &str, entries: &[ModEntry]) -> ApplyOutcome {
        // 工具侧预防（全部在发送前，零副作用 —— 不碰管道不注入）：
        // - 裸 {}/}：唯一会破坏 DLL 迷你解析器的字符（状态串与 DLL 同名
        //   bad-entry）。空 title/version 是合法的，DLL 接受（review 修复项 2）。
        // - 超容量 title>127B / version>63B：DLL 会静默截断成别的 mod 名
        //   （entry-too-long）；
        // - 条目数 > MAX_ENTRIES：DLL 只解析前 128 条（too-many-entries）。
        for e in entries {
            if e.check_pipe_safe().is_err() {
                return self.finish(save_path, entries, ApplyOutcome::tool_state("bad-entry"));
            }
            if e.check_entry_capacity().is_err() {
                return self.finish(
                    save_path,
                    entries,
                    ApplyOutcome::tool_state("entry-too-long"),
                );
            }
        }
        if entries.len() > MAX_ENTRIES {
            return self.finish(
                save_path,
                entries,
                ApplyOutcome::tool_state("too-many-entries"),
            );
        }

        let mut pipe = match PipeClient::connect(self.config.connect_timeout) {
            Ok(p) => p,
            Err(first_err) => match self.ensure_injected() {
                // 注入后 DLL 建管道有延迟（InitThread 里日志/特征码检查在前）——
                // connect 内部对 FILE_NOT_FOUND 重试到 deadline，竞态在此吸收
                //（= 旧 C# 注入后 Connect(4000) 的语义）。
                Ok(()) => match PipeClient::connect(self.config.connect_timeout) {
                    Ok(p) => p,
                    Err(_) => {
                        return self.finish(
                            save_path,
                            entries,
                            // 注入成功但管道仍不通：DLL 可能已 idle-exit 或日志有异常
                            ApplyOutcome::tool_state(format!(
                                "engine-off (injected ok; pipe still unreachable, first connect err: {first_err})"
                            )),
                        );
                    }
                },
                Err(outcome) => {
                    return self.finish(save_path, entries, outcome);
                }
            },
        };

        let reply = pipe.request(
            &Request::Apply {
                id: next_id(),
                path: save_path,
                entries,
            },
            self.config.apply_timeout,
        );
        let outcome = match reply {
            Ok(r) => ApplyOutcome {
                ok: r.ok,
                state: r.state,
                count: r.count,
            },
            Err(e) => ApplyOutcome::tool_state(pipe_err_state(&e)),
        };
        self.finish(save_path, entries, outcome)
    }

    /// 连不上时注入引擎：定位 DLL（覆盖或搜索序）→ 写 cfg → TryInject。
    /// 失败映射为旧 C# 的 engine-off / game-not-running / dll-not-found 档。
    fn ensure_injected(&self) -> Result<(), ApplyOutcome> {
        let dll = match &self.config.dll_override {
            Some(p) => p.clone(),
            None => locate_dll().map_err(|e: DllDiscoveryError| {
                // 状态串 = 旧 C# 原词；完整搜索列表进工具日志（160 字净化会截长列表）
                log_tool_line(&self.resolve_log_dir(), &e.to_string());
                ApplyOutcome::tool_state("dll-not-found")
            })?,
        };
        let log_dir = self.resolve_log_dir();
        let inject = try_inject_with(
            ProcessTarget::Name(self.config.game_process.clone()),
            &dll,
            &log_dir,
            &InjectOptions {
                thread_wait_ms: self.config.inject_thread_wait_ms,
            },
        );
        match inject {
            Ok(()) => Ok(()),
            Err(f) => {
                if self.game_running() {
                    Err(ApplyOutcome::tool_state(format!(
                        "engine-off:{}",
                        f.detail()
                    )))
                } else {
                    Err(ApplyOutcome::tool_state("game-not-running"))
                }
            }
        }
    }

    /// 日志目录解析：配置 → `DDHOTAPPLY_LOG` → `%TEMP%\DdModToolHotapply`（建目录）。
    pub fn resolve_log_dir(&self) -> PathBuf {
        if let Some(d) = &self.config.log_dir {
            return d.clone();
        }
        if let Some(env_dir) = std::env::var(LOG_DIR_ENV).ok().filter(|s| !s.is_empty()) {
            return PathBuf::from(env_dir);
        }
        std::env::temp_dir().join("DdModToolHotapply")
    }

    /// 收尾：写工具侧行到 engine.log（静默失败，同旧 C#），透传结果。
    fn finish(&self, save_path: &str, entries: &[ModEntry], outcome: ApplyOutcome) -> ApplyOutcome {
        let line = format!(
            "apply path={save_path} entries={} -> ok={} state={} count={}",
            entries.len(),
            outcome.ok,
            outcome.state,
            outcome
                .count
                .map(|c| c.to_string())
                .unwrap_or_else(|| "-".into())
        );
        log_tool_line(&self.resolve_log_dir(), &line);
        outcome
    }
}

fn next_id() -> i64 {
    NEXT_ID.fetch_add(1, Ordering::Relaxed)
}

/// 管道层错误 → 工具侧状态串（review 修复项 d/f）：
/// - Timeout → `timeout`（旧 C# 原词）；
/// - 编码护栏各有专名（too-many-entries / line-too-large），不混入 io-error；
/// - 其余 → `io-error: <细节>`（仍以 io-error 开头，旧判断兼容；细节含
///   id 错配/读写失败码等，直说坏在哪）。
fn pipe_err_state(e: &PipeError) -> String {
    match e {
        PipeError::Timeout => "timeout".into(),
        PipeError::Encode(EncodeError::TooManyEntries(_)) => "too-many-entries".into(),
        PipeError::Encode(EncodeError::LineTooLarge(_)) => "line-too-large".into(),
        other => format!("io-error: {other}"),
    }
}

/// 工具侧行：`<UTC ISO8601>Z tool: <line>` 追加到 `<log_dir>/engine.log`
/// （与 DLL 引擎同文件；任何失败静默 —— 旧 C# 同义）。
pub fn log_tool_line(log_dir: &std::path::Path, line: &str) {
    use std::io::Write;
    let _ = std::fs::create_dir_all(log_dir);
    let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_dir.join("engine.log"))
    else {
        return;
    };
    let _ = writeln!(f, "{}Z tool: {line}", format_utc_now());
}

/// 当前时刻 → `YYYY-MM-DDTHH:MM:SS.mmmZ`（无 chrono 依赖的手写 UTC 格式化）。
pub fn format_utc_now() -> String {
    format_unix_ms(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
    )
}

/// Unix 毫秒 → UTC ISO8601（civil_from_days 算法，单位测试锁定已知日期）。
pub fn format_unix_ms(unix_ms: u64) -> String {
    let secs = unix_ms / 1000;
    let millis = unix_ms % 1000;
    let days = (secs / 86400) as i64;
    let sod = secs % 86400;
    let (y, m, d) = civil_from_days(days);
    let (hh, mm, ss) = (sod / 3600, (sod % 3600) / 60, sod % 60);
    let mut s = String::new();
    let _ = write!(
        s,
        "{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}.{millis:03}"
    );
    s
}

/// Howard Hinnant civil_from_days：自 1970-01-01 的天数 → (年, 月, 日)。
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m as u32, d as u32)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipe::parse_reply_line;
    use crate::win::wide_null;

    #[test]
    fn utc_formatter_known_dates() {
        assert_eq!(format_unix_ms(0), "1970-01-01T00:00:00.000");
        // 2026-11-04T00:00:00Z = 1793750400（date -u 锁定）
        assert_eq!(format_unix_ms(1_793_750_400_000), "2026-11-04T00:00:00.000");
        // 同日 13:05:59.123
        assert_eq!(
            format_unix_ms(1_793_750_400_000 + 13 * 3600 * 1000 + 5 * 60 * 1000 + 59_123),
            "2026-11-04T13:05:59.123"
        );
        // 闰日：2024-02-29T12:00:00Z = 1709208000
        assert_eq!(format_unix_ms(1_709_208_000_000), "2024-02-29T12:00:00.000");
        // 年末：1999-12-31T23:59:59.999Z = 946684799.999
        assert_eq!(format_unix_ms(946_684_799_999), "1999-12-31T23:59:59.999");
    }

    /// 快速失败配置：连不上管道（150ms）、注入必然 no-process、无游戏进程。
    fn fast_client() -> HotapplyClient {
        HotapplyClient::with_config(ClientConfig {
            connect_timeout: Duration::from_millis(150),
            game_process: "dd-mrm-no-such-game-xyz".into(),
            dll_override: Some(PathBuf::from(r"C:\dd-mrm-nonexistent\DdHotapply.dll")),
            log_dir: Some(
                std::env::temp_dir().join(format!("dd-mrm-ha-log-{}", std::process::id())),
            ),
            ..Default::default()
        })
    }

    #[test]
    fn apply_prechecks_braces_only() {
        let client = fast_client();
        // 裸 { / }：破坏 DLL 切片 → bad-entry
        let o = client.apply(
            "x/profile_1/persist.game.json",
            &[ModEntry {
                title: "a}b".into(),
                version: "1".into(),
            }],
        );
        assert_eq!((o.ok, o.state.as_str()), (false, "bad-entry"));
        let o = client.apply(
            "x/profile_1/persist.game.json",
            &[ModEntry {
                title: "A".into(),
                version: "1{0".into(),
            }],
        );
        assert_eq!(o.state, "bad-entry");
    }

    #[test]
    fn apply_accepts_empty_strings_dll_semantics() {
        // review 修复项 2：空 title/version 是 DLL 合法输入（唯一解析风险是
        // 裸花括号）——预检不得拦截。此处证明没被 bad-entry 挡下：
        // 走完连接/注入分支 → 无游戏 → game-not-running。
        let client = fast_client();
        let o = client.apply(
            "x/profile_1/persist.game.json",
            &[ModEntry {
                title: String::new(),
                version: String::new(),
            }],
        );
        assert_eq!(o.state, "game-not-running", "state={}", o.state);
    }

    #[test]
    fn apply_prechecks_entry_capacity() {
        let client = fast_client();
        // 恰 127/63 字节：不拦（走到 game-not-running）
        let o = client.apply(
            "x/profile_1/persist.game.json",
            &[ModEntry {
                title: "t".repeat(127),
                version: "v".repeat(63),
            }],
        );
        assert_eq!(o.state, "game-not-running", "state={}", o.state);
        // title 128 → entry-too-long
        let o = client.apply(
            "x/profile_1/persist.game.json",
            &[ModEntry {
                title: "t".repeat(128),
                version: "v".repeat(63),
            }],
        );
        assert_eq!(o.state, "entry-too-long");
        // version 64 → entry-too-long
        let o = client.apply(
            "x/profile_1/persist.game.json",
            &[ModEntry {
                title: "t".repeat(10),
                version: "v".repeat(64),
            }],
        );
        assert_eq!(o.state, "entry-too-long");
    }

    #[test]
    fn apply_prechecks_entry_count() {
        let client = fast_client();
        let entries: Vec<ModEntry> = (0..MAX_ENTRIES + 1)
            .map(|i| ModEntry {
                title: format!("m{i}"),
                version: "1".into(),
            })
            .collect();
        let o = client.apply("x/profile_1/persist.game.json", &entries);
        assert_eq!(o.state, "too-many-entries");
    }

    #[test]
    fn pipe_err_state_mapping() {
        assert_eq!(pipe_err_state(&PipeError::Timeout), "timeout");
        assert_eq!(
            pipe_err_state(&PipeError::Encode(EncodeError::TooManyEntries(129))),
            "too-many-entries"
        );
        assert_eq!(
            pipe_err_state(&PipeError::Encode(EncodeError::LineTooLarge(99))),
            "line-too-large"
        );
        // id 错配 → io-error 带细节（仍以 io-error 开头）
        let s = pipe_err_state(&PipeError::Parse(
            "reply id mismatch: expected 5, got 999".into(),
        ));
        assert!(s.starts_with("io-error: "), "{s}");
        assert!(s.contains("id mismatch"), "{s}");
        // 裸 Win32 失败同款
        let s = pipe_err_state(&PipeError::WriteFailed(232));
        assert!(s.starts_with("io-error: "), "{s}");
    }

    #[test]
    fn apply_no_game_maps_to_game_not_running() {
        // dll 覆盖指向不存在的路径（跳过搜索序），游戏进程名保证不存在：
        // 注入必然 no-process，且 game_running=false → 旧 C# 的优先级映射
        // game-not-running（若进程存在则映射 engine-off:no-process）。
        let client = HotapplyClient::with_config(ClientConfig {
            connect_timeout: Duration::from_millis(200),
            game_process: "dd-mrm-no-such-game-xyz".into(),
            dll_override: Some(PathBuf::from(r"C:\dd-mrm-nonexistent\DdHotapply.dll")),
            log_dir: Some(
                std::env::temp_dir().join(format!("dd-mrm-ha-log-{}", std::process::id())),
            ),
            ..Default::default()
        });
        let o = client.apply("x/profile_1/persist.game.json", &[]);
        assert!(!o.ok);
        assert_eq!(o.state, "game-not-running", "state={}", o.state);
    }

    #[test]
    fn tool_log_line_written() {
        let dir = std::env::temp_dir().join(format!("dd-mrm-ha-toollog-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        log_tool_line(&dir, "unit-test-line");
        let content = std::fs::read_to_string(dir.join("engine.log")).unwrap();
        assert!(content.contains("Z tool: unit-test-line"), "{content}");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn resolve_log_dir_priority() {
        let base = HotapplyClient::with_config(ClientConfig::default());
        assert_eq!(
            base.resolve_log_dir(),
            std::env::temp_dir().join("DdModToolHotapply")
        );
        let with_dir = HotapplyClient::with_config(ClientConfig {
            log_dir: Some(PathBuf::from("F:/logs")),
            ..Default::default()
        });
        assert_eq!(with_dir.resolve_log_dir(), PathBuf::from("F:/logs"));
    }

    // —— 端到端（无游戏）：mock 管道服务器充当引擎 ——
    // 门面 apply() 默认先连真管道 DdModToolHotapply——测试机上不存在，
    // 会走 ensure_injected → dll 定位失败/注入 no-process 档。因此这里
    // 只测请求可复用部分（编码/解析在 pipe.rs 已锁），门面编排逻辑由
    // notepad 冒烟（ignored）与真机测试（GC8）覆盖。

    #[test]
    fn wide_null_terminates() {
        let v = wide_null("ab");
        assert_eq!(v, vec![97, 98, 0]);
    }

    #[test]
    fn parse_reply_via_facade_types() {
        let r = parse_reply_line(r#"{"id":1,"ok":true,"state":"applied","count":9}"#).unwrap();
        assert_eq!(r.count, Some(9));
    }
}
