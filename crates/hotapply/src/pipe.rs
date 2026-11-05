//! `\\.\pipe\DdModToolHotapply` 管道客户端 —— 协议权威 = native/DdHotapply.c（GC2）。
//!
//! 传输：字节模式双向命名管道；UTF-8 JSON 行（'\n' 结尾；服务端容忍 '\r\n'）。
//! 响应 state 在解码处统一净化（去控制字符、截 160 字）——设计 §5 教训：回包曾带
//! 二进制尾巴撑坏 WPF 布局。
//!
//! 服务端（DLL）硬约束（协议事实，非游戏知识）：
//! - 读缓冲 16384 字节：超长行被**静默丢弃**（客户端表现为 reply 超时）→ 编码期
//!   以 [`MAX_REQUEST_LINE`] 先行拒绝；
//! - entries 上限 [`MAX_ENTRIES`]（DdHotapply.c `MAX_ENTRIES`）；
//! - 迷你 JSON 解析器按首个 `{`..`}` 切片抽 title/version：串内裸 `{`/`}` 会破坏
//!   切片 → [`ModEntry::check_pipe_safe`] 供调用方预防性校验。

use std::fmt;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::{
    CloseHandle, ERROR_FILE_NOT_FOUND, ERROR_IO_PENDING, ERROR_PIPE_BUSY, WAIT_OBJECT_0,
    WAIT_TIMEOUT,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_NONE, FlushFileBuffers, OPEN_EXISTING, ReadFile,
    WriteFile,
};
use windows::Win32::System::IO::{CancelIo, CancelIoEx, GetOverlappedResult, OVERLAPPED};
use windows::Win32::System::Pipes::WaitNamedPipeW;
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
use windows::core::PCWSTR;

use crate::win::{wide_null, win32_code};

/// 管道名（GC7：沿用现名，= DdHotapply.c `PIPE_NAME`）。
pub const PIPE_NAME: &str = r"\\.\pipe\DdModToolHotapply";
/// 服务端读缓冲 16384 字节，行超长会被静默丢弃；编码期上限（留头部余量）。
pub const MAX_REQUEST_LINE: usize = 16_000;
/// 服务端条目数上限（DdHotapply.c `MAX_ENTRIES`）。
pub const MAX_ENTRIES: usize = 128;
/// 应用侧 title 容量（DdHotapply.c `utf8_trunc_copy(dst, 128, …)`：128 含 NUL 位）。
/// 超出的部分会被 DLL **静默截断**（mod 名悄悄变化）→ 发送前按此上限拒绝。
pub const MAX_ENTRY_TITLE_BYTES: usize = 127;
/// 应用侧 version 容量（同上，`utf8_trunc_copy(dst, 64, …)`：64 含 NUL 位）。
pub const MAX_ENTRY_VERSION_BYTES: usize = 63;
/// state 净化长度上限（设计 §5：截断 160 字）。
pub const STATE_MAX_CHARS: usize = 160;
/// 单行响应读取上限（服务端 Reply 缓冲 512 字节，此为防御值）。
const MAX_REPLY_LINE: usize = 8 * 1024;

/// mod 条目（192B 条目 {title[128], version[64]} 的两段；GC1：边界两侧各自定义）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModEntry {
    pub title: String,
    pub version: String,
}

impl ModEntry {
    /// 预防性协议校验：title/version 内的裸 `{`/`}` 会破坏 DLL 迷你解析器的
    /// 对象切片（截断/错位 → bad-entry 或错条目）。工具侧可据此在发送前拒绝。
    /// 空串是**合法**的（DLL 接受，唯一解析风险就是裸花括号）。
    pub fn check_pipe_safe(&self) -> Result<(), String> {
        for (name, s) in [("title", &self.title), ("version", &self.version)] {
            if s.contains('{') || s.contains('}') {
                return Err(format!(
                    "{name} contains '{{' or '}}' (breaks DLL line parser)"
                ));
            }
        }
        Ok(())
    }

    /// 容量校验：title ≤ 127 字节 / version ≤ 63 字节（UTF-8 字节数，不含 NUL）。
    /// 超出会被 DLL **静默截断**成不同的 mod 名（utf8_trunc_copy，感知多字节边界），
    /// 与其让名字悄悄变，不如发送前拒绝。
    pub fn check_entry_capacity(&self) -> Result<(), String> {
        for (name, s, cap) in [
            ("title", &self.title, MAX_ENTRY_TITLE_BYTES),
            ("version", &self.version, MAX_ENTRY_VERSION_BYTES),
        ] {
            if s.len() > cap {
                return Err(format!(
                    "{name} {} bytes > {cap} (DLL would silently truncate)",
                    s.len()
                ));
            }
        }
        Ok(())
    }
}

/// 行协议请求（字段顺序 = 线上 JSON 字段顺序；服务端 strstr 找键，顺序必须保持）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request<'a> {
    /// `{"id":N,"cmd":"status"}` → 服务端回 `{"ok":true,"state":"ready"}`。
    Status { id: i64 },
    /// `{"id":N,"cmd":"apply","path":…,"entries":[{title,version}…]}`。
    /// path 仅用于 DLL 侧定位 profile 名（须含 `profile_<数字>`，否则 bad-args）。
    Apply {
        id: i64,
        path: &'a str,
        entries: &'a [ModEntry],
    },
}

/// 服务端响应：`{"id":N,"ok":bool,"state":"…"[,"count":N]}`。
/// `count` 仅 ok=true 时出现；未知字段（如设计文档提及的 first）忽略——向前兼容。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Reply {
    pub id: i64,
    pub ok: bool,
    pub state: String,
    pub count: Option<i64>,
}

/// 请求编码失败（协议护栏，发送前拦截）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EncodeError {
    TooManyEntries(usize),
    LineTooLarge(usize),
}

impl fmt::Display for EncodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            EncodeError::TooManyEntries(n) => write!(f, "too-many-entries: {n} > {MAX_ENTRIES}"),
            EncodeError::LineTooLarge(n) => {
                write!(f, "request-too-large: {n} > {MAX_REQUEST_LINE} bytes")
            }
        }
    }
}

#[derive(Serialize)]
struct WireStatus {
    id: i64,
    cmd: &'static str,
}

#[derive(Serialize)]
struct WireApply<'a> {
    id: i64,
    cmd: &'static str,
    path: &'a str,
    entries: &'a [ModEntry],
}

/// 请求 → 单行 JSON（不含换行；调用方在写出时补 '\n'）。
pub fn encode_request_line(req: &Request<'_>) -> Result<String, EncodeError> {
    let s = match req {
        Request::Status { id } => serde_json::to_string(&WireStatus {
            id: *id,
            cmd: "status",
        }),
        Request::Apply { id, path, entries } => {
            if entries.len() > MAX_ENTRIES {
                return Err(EncodeError::TooManyEntries(entries.len()));
            }
            serde_json::to_string(&WireApply {
                id: *id,
                cmd: "apply",
                path,
                entries,
            })
        }
    }
    .unwrap_or_default();
    if s.len() > MAX_REQUEST_LINE {
        return Err(EncodeError::LineTooLarge(s.len()));
    }
    Ok(s)
}

/// 单行 JSON（去 '\r'/'\n'）→ 响应；state 净化后返回。
pub fn parse_reply_line(line: &str) -> Result<Reply, String> {
    let line = line.trim();
    if line.is_empty() {
        return Err("empty reply line".into());
    }
    #[derive(Deserialize)]
    struct WireReply {
        id: i64,
        ok: bool,
        state: String,
        #[serde(default)]
        count: Option<i64>,
    }
    let r: WireReply =
        serde_json::from_str(line).map_err(|e| format!("bad reply json: {e}: {line:?}"))?;
    Ok(Reply {
        id: r.id,
        ok: r.ok,
        state: sanitize_state(&r.state),
        count: r.count,
    })
}

/// 状态串净化（设计 §5）：去控制字符、截断 [`STATE_MAX_CHARS`] 字。T5 可复用于任意展示串。
pub fn sanitize_state(s: &str) -> String {
    let mut out: String = s.chars().filter(|c| !c.is_control()).collect();
    if out.chars().count() > STATE_MAX_CHARS {
        out = out.chars().take(STATE_MAX_CHARS).collect();
    }
    out
}

/// 管道客户端错误分类（工具侧状态串映射：Timeout→"timeout"，其余→"io-error"）。
#[derive(Debug)]
pub enum PipeError {
    /// 连接期内管道一直 busy / 不存在（注入后竞态重试到期）。
    ConnectTimeout,
    /// CreateFileW 失败（不可重试类错误）。
    ConnectFailed(u32),
    /// 请求编码失败（条目超限 / 行超长 —— 发送前拦截，无任何 IO 副作用）。
    Encode(EncodeError),
    WriteFailed(u32),
    ReadFailed(u32),
    /// 读写超时（也覆盖服务端静默丢行后的 reply 超时）。
    Timeout,
    /// 对端关闭（EOF）。
    Closed,
    LineTooLong,
    Parse(String),
}

impl fmt::Display for PipeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PipeError::ConnectTimeout => write!(f, "pipe connect timeout (busy or not up yet)"),
            PipeError::ConnectFailed(c) => write!(f, "pipe connect failed: err{c}"),
            PipeError::Encode(e) => write!(f, "encode: {e}"),
            PipeError::WriteFailed(c) => write!(f, "pipe write failed: err{c}"),
            PipeError::ReadFailed(c) => write!(f, "pipe read failed: err{c}"),
            PipeError::Timeout => write!(f, "pipe io timeout"),
            PipeError::Closed => write!(f, "pipe closed by peer"),
            PipeError::LineTooLong => write!(f, "reply line too long"),
            PipeError::Parse(e) => write!(f, "reply parse: {e}"),
        }
    }
}

impl std::error::Error for PipeError {}

/// 命名管道客户端连接（FILE_FLAG_OVERLAPPED + 事件等待实现严格读写超时；
/// 服务端单实例字节模式，一次连接一请求，Drop 断开）。
#[derive(Debug)]
pub struct PipeClient {
    handle: windows::Win32::Foundation::HANDLE,
}

impl Drop for PipeClient {
    fn drop(&mut self) {
        unsafe {
            let _ = CancelIo(self.handle);
            let _ = CloseHandle(self.handle);
        }
    }
}

impl PipeClient {
    /// 连接 [`PIPE_NAME`]（引擎默认管道）。
    pub fn connect(timeout: Duration) -> Result<Self, PipeError> {
        Self::connect_named(PIPE_NAME, timeout)
    }

    /// 连接指定名字的管道，**两类可重试错误都重试到 deadline**：
    /// - `ERROR_PIPE_BUSY`：已有实例占用 → WaitNamedPipeW 等实例；
    /// - `ERROR_FILE_NOT_FOUND`：管道还没创建 → 注入后竞态（DLL InitThread 里
    ///   日志打开/特征码检查先于 CreateNamedPipeW，客户端常赢得竞态）→ 短睡重试。
    ///
    /// 到期统一报 ConnectTimeout（= 旧 C# `Connect(ms)` 的 TimeoutException
    /// 语义）；其余错误立即失败。测试用独立管道名。
    pub fn connect_named(name: &str, timeout: Duration) -> Result<Self, PipeError> {
        let wide = wide_null(name);
        let deadline = Instant::now() + timeout;
        loop {
            let handle = unsafe {
                CreateFileW(
                    PCWSTR(wide.as_ptr()),
                    windows::Win32::Foundation::GENERIC_READ.0
                        | windows::Win32::Foundation::GENERIC_WRITE.0,
                    FILE_SHARE_NONE,
                    None,
                    OPEN_EXISTING,
                    FILE_FLAG_OVERLAPPED,
                    None,
                )
            };
            match handle {
                Ok(h) => return Ok(PipeClient { handle: h }),
                Err(e) => {
                    let code = win32_code(&e);
                    let retryable = code == ERROR_PIPE_BUSY.0 || code == ERROR_FILE_NOT_FOUND.0;
                    let now = Instant::now();
                    if !retryable {
                        return Err(PipeError::ConnectFailed(code));
                    }
                    if now >= deadline {
                        return Err(PipeError::ConnectTimeout);
                    }
                    if code == ERROR_PIPE_BUSY.0 {
                        let ms = (deadline - now).as_millis().min(5_000) as u32;
                        // false = 超时/无实例：继续循环至 deadline
                        let _ = unsafe { WaitNamedPipeW(PCWSTR(wide.as_ptr()), ms) };
                    } else {
                        // 管道未创建：短睡后重试（不忙转，CreateFileW 很便宜）
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
            }
        }
    }

    /// 一次请求-响应往返（写+读共用同一 deadline，超时语义=整轮往返预算）。
    /// 响应 `id` 必须与请求一致（错位 = 串线/坏流 → Parse 错）。
    pub fn request(&mut self, req: &Request<'_>, round_trip: Duration) -> Result<Reply, PipeError> {
        let line = encode_request_line(req).map_err(PipeError::Encode)?;
        let req_id = match req {
            Request::Status { id } | Request::Apply { id, .. } => *id,
        };
        let deadline = Instant::now() + round_trip;
        self.write_line(line.as_bytes(), deadline)?;
        let resp = self.read_line(deadline)?;
        let reply = parse_reply_line(&resp).map_err(PipeError::Parse)?;
        if reply.id != req_id {
            return Err(PipeError::Parse(format!(
                "reply id mismatch: expected {req_id}, got {}",
                reply.id
            )));
        }
        Ok(reply)
    }

    /// 写一行（补 '\n'；分块写完为止）。
    fn write_line(&mut self, line: &[u8], deadline: Instant) -> Result<(), PipeError> {
        let mut buf: Vec<u8> = Vec::with_capacity(line.len() + 1);
        buf.extend_from_slice(line);
        buf.push(b'\n');
        let mut sent = 0usize;
        while sent < buf.len() {
            let n = self.overlapped_write(&buf[sent..], deadline)?;
            if n == 0 {
                return Err(PipeError::Closed);
            }
            sent += n;
        }
        unsafe {
            let _ = FlushFileBuffers(self.handle);
        }
        Ok(())
    }

    /// 读一行（至 '\n'；截去尾部 '\r'；一行一请求，行后残余忽略——本客户端
    /// 每连接只发一个请求，服务端不会主动多发）。
    fn read_line(&mut self, deadline: Instant) -> Result<String, PipeError> {
        let mut acc: Vec<u8> = Vec::with_capacity(512);
        let mut chunk = [0u8; 512];
        loop {
            if acc.len() > MAX_REPLY_LINE {
                return Err(PipeError::LineTooLong);
            }
            let n = self.overlapped_read(&mut chunk, deadline)?;
            if n == 0 {
                return Err(PipeError::Closed);
            }
            acc.extend_from_slice(&chunk[..n]);
            if let Some(pos) = acc.iter().position(|&b| b == b'\n') {
                let mut line = &acc[..pos];
                if line.last() == Some(&b'\r') {
                    line = &line[..line.len() - 1];
                }
                return Ok(String::from_utf8_lossy(line).into_owned());
            }
        }
    }

    /// 单次 overlapped 写；超时 CancelIo 并返回 Timeout。
    fn overlapped_write(&mut self, buf: &[u8], deadline: Instant) -> Result<usize, PipeError> {
        unsafe {
            let ev = CreateEventW(None, true, false, None)
                .map_err(|e| PipeError::WriteFailed(win32_code(&e)))?;
            let mut ov: OVERLAPPED = std::mem::zeroed();
            ov.hEvent = ev;
            let pending = WriteFile(self.handle, Some(buf), None, Some(&mut ov));
            match pending {
                Ok(()) => {}
                Err(e) if win32_code(&e) == ERROR_IO_PENDING.0 => {}
                Err(e) => {
                    let _ = CloseHandle(ev);
                    return Err(PipeError::WriteFailed(win32_code(&e)));
                }
            }
            self.finish_overlapped(ev, &mut ov, PipeError::WriteFailed, deadline)
        }
    }

    /// 单次 overlapped 读；超时 CancelIo 并返回 Timeout。
    fn overlapped_read(&mut self, buf: &mut [u8], deadline: Instant) -> Result<usize, PipeError> {
        unsafe {
            let ev = CreateEventW(None, true, false, None)
                .map_err(|e| PipeError::ReadFailed(win32_code(&e)))?;
            let mut ov: OVERLAPPED = std::mem::zeroed();
            ov.hEvent = ev;
            let pending = ReadFile(self.handle, Some(buf), None, Some(&mut ov));
            match pending {
                Ok(()) => {}
                Err(e) if win32_code(&e) == ERROR_IO_PENDING.0 => {}
                Err(e) => {
                    let _ = CloseHandle(ev);
                    return Err(PipeError::ReadFailed(win32_code(&e)));
                }
            }
            self.finish_overlapped(ev, &mut ov, PipeError::ReadFailed, deadline)
        }
    }

    /// 等待 overlapped 完成（deadline 约束）并取传输字节数；事件句柄必关。
    /// 超时/失败路径必须**先排干再关事件**：CancelIoEx 只是请求取消，
    /// 内核可能仍在引用栈上的 OVERLAPPED 与事件句柄 —— GetOverlappedResult
    /// (bWait=TRUE) 在取消完成后立即返回（ERROR_OPERATION_ABORTED），
    /// 此后关闭事件才安全（MSDN：bWait=TRUE 完成前不得关事件句柄）。
    ///（edition 2024：unsafe fn 体内也要显式 unsafe 块。）
    unsafe fn finish_overlapped(
        &self,
        ev: windows::Win32::Foundation::HANDLE,
        ov: &mut OVERLAPPED,
        mk: fn(u32) -> PipeError,
        deadline: Instant,
    ) -> Result<usize, PipeError> {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let ms = remaining.as_millis().clamp(1, u32::MAX as u128) as u32;
        let wait = unsafe { WaitForSingleObject(ev, ms) };
        if wait != WAIT_OBJECT_0 {
            // 先取 last-error（后续 Cancel/GetOverlapped 会覆盖它），再取消+排干
            let wait_err = unsafe { windows::Win32::Foundation::GetLastError().0 };
            unsafe {
                let _ = CancelIoEx(self.handle, None);
                let mut n = 0u32;
                let _ = GetOverlappedResult(self.handle, ov, &mut n, true); // 排干（含已取消）
                let _ = CloseHandle(ev);
            }
            return match wait {
                WAIT_TIMEOUT => Err(PipeError::Timeout),
                _ => Err(mk(wait_err)),
            };
        }
        let mut n = 0u32;
        let r = unsafe { GetOverlappedResult(self.handle, ov, &mut n, true) };
        unsafe {
            let _ = CloseHandle(ev);
        };
        r.map_err(|e| mk(win32_code(&e)))?;
        Ok(n as usize)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::thread;
    use std::time::Duration;

    // ---------------- 纯编解码 ----------------

    #[test]
    fn encode_status_shape() {
        let s = encode_request_line(&Request::Status { id: 7 }).unwrap();
        assert_eq!(s, r#"{"id":7,"cmd":"status"}"#);
    }

    #[test]
    fn encode_apply_shape_and_field_order() {
        // 字段顺序=id,cmd,path,entries：服务端 json_get_str 用 strstr 找键，
        // 顺序即契约，锁死。
        let entries = vec![
            ModEntry {
                title: "Crusader Overhaul".into(),
                version: "1.2.3".into(),
            },
            ModEntry {
                title: "皮 Sanity 的mod".into(),
                version: String::new(),
            },
        ];
        let s = encode_request_line(&Request::Apply {
            id: 1,
            path: r"C:\saves\profile_2\persist.game.json",
            entries: &entries,
        })
        .unwrap();
        assert_eq!(
            s,
            r#"{"id":1,"cmd":"apply","path":"C:\\saves\\profile_2\\persist.game.json","entries":[{"title":"Crusader Overhaul","version":"1.2.3"},{"title":"皮 Sanity 的mod","version":""}]}"#
        );
    }

    #[test]
    fn encode_rejects_too_many_entries() {
        let entries: Vec<ModEntry> = (0..MAX_ENTRIES + 1)
            .map(|i| ModEntry {
                title: format!("m{i}"),
                version: "1".into(),
            })
            .collect();
        assert_eq!(
            encode_request_line(&Request::Apply {
                id: 1,
                path: "x/profile_1/persist.game.json",
                entries: &entries
            }),
            Err(EncodeError::TooManyEntries(MAX_ENTRIES + 1))
        );
    }

    #[test]
    fn encode_rejects_oversized_line() {
        let entries: Vec<ModEntry> = (0..100)
            .map(|_| ModEntry {
                title: "t".repeat(160),
                version: "v".repeat(70),
            })
            .collect();
        assert!(matches!(
            encode_request_line(&Request::Apply {
                id: 1,
                path: "profile_1",
                entries: &entries
            }),
            Err(EncodeError::LineTooLarge(_))
        ));
    }

    #[test]
    fn parse_reply_variants() {
        let r = parse_reply_line(r#"{"id":1,"ok":true,"state":"applied","count":3}"#).unwrap();
        assert_eq!(
            r,
            Reply {
                id: 1,
                ok: true,
                state: "applied".into(),
                count: Some(3)
            }
        );
        // 失败态无 count；含 seen=[..] 的复合 state 原样保留（净化不删合法字符）
        let r = parse_reply_line(
            "{\"id\":1,\"ok\":false,\"state\":\"envelope-not-found: profile_2 seen=[a,b,c]\"}",
        )
        .unwrap();
        assert!(!r.ok && r.count.is_none());
        assert!(r.state.contains("seen=[a,b,c]"));
        // 未知字段（first 等）忽略——向前兼容
        let r = parse_reply_line(r#"{"id":2,"ok":true,"state":"ready","first":"x","count":0}"#)
            .unwrap();
        assert_eq!(r.state, "ready");
        assert_eq!(r.count, Some(0));
        assert!(parse_reply_line("not json").is_err());
        assert!(parse_reply_line("").is_err());
    }

    #[test]
    fn sanitize_strips_controls_and_truncates() {
        assert_eq!(sanitize_state("ap\u{1}pli\u{7}ed"), "applied");
        assert_eq!(sanitize_state("no-frame\r\n"), "no-frame");
        let long = "x".repeat(300);
        assert_eq!(sanitize_state(&long).chars().count(), STATE_MAX_CHARS);
    }

    #[test]
    fn pipe_safety_check_flags_braces() {
        let e = ModEntry {
            title: "a}b".into(),
            version: "1".into(),
        };
        assert!(e.check_pipe_safe().is_err());
        // 空串合法（DLL 接受；唯一解析风险是裸花括号）
        let empty = ModEntry {
            title: String::new(),
            version: String::new(),
        };
        assert!(empty.check_pipe_safe().is_ok());
        let ok = ModEntry {
            title: "plain".into(),
            version: "1.0".into(),
        };
        assert!(ok.check_pipe_safe().is_ok());
    }

    #[test]
    fn entry_capacity_boundary() {
        // 恰好 127/63 字节：通过；128/64：拒绝（DLL utf8_trunc_copy 128/64 含 NUL）
        let exact = ModEntry {
            title: "t".repeat(MAX_ENTRY_TITLE_BYTES),
            version: "v".repeat(MAX_ENTRY_VERSION_BYTES),
        };
        assert!(exact.check_entry_capacity().is_ok());
        let t_over = ModEntry {
            title: "t".repeat(MAX_ENTRY_TITLE_BYTES + 1),
            version: "v".repeat(MAX_ENTRY_VERSION_BYTES),
        };
        assert!(t_over.check_entry_capacity().is_err());
        let v_over = ModEntry {
            title: "t".repeat(10),
            version: "v".repeat(MAX_ENTRY_VERSION_BYTES + 1),
        };
        assert!(v_over.check_entry_capacity().is_err());
        // UTF-8 按字节计：127 个三字节字符 = 381 字节 → 拒绝
        let mb = ModEntry {
            title: "中".repeat(MAX_ENTRY_TITLE_BYTES),
            version: "1".into(),
        };
        assert!(mb.check_entry_capacity().is_err());
    }

    // ---------------- 真命名管道往返（无游戏；mock 服务线程） ----------------

    /// mock 服务端：单实例字节管道，收一行回一行固定响应。返回 (就绪信号, 线程)。
    fn spawn_mock_server(
        name: &str,
        reply: &'static str,
        hang: bool,
    ) -> (mpsc::Receiver<()>, thread::JoinHandle<String>) {
        let (tx, rx) = mpsc::channel();
        let name = name.to_string();
        let handle = thread::spawn(move || {
            use windows::Win32::Foundation::ERROR_PIPE_CONNECTED;
            use windows::Win32::Storage::FileSystem::PIPE_ACCESS_DUPLEX;
            use windows::Win32::Storage::FileSystem::ReadFile as SrvRead;
            use windows::Win32::Storage::FileSystem::WriteFile as SrvWrite;
            use windows::Win32::System::Pipes::{
                ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PIPE_READMODE_BYTE,
                PIPE_TYPE_BYTE, PIPE_WAIT,
            };
            unsafe {
                let wide = crate::win::wide_null(&name);
                let pipe = CreateNamedPipeW(
                    PCWSTR(wide.as_ptr()),
                    PIPE_ACCESS_DUPLEX,
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                    1,
                    8192,
                    8192,
                    0,
                    None,
                );
                assert!(!pipe.is_invalid(), "CreateNamedPipeW failed");
                tx.send(()).unwrap();
                if let Err(e) = ConnectNamedPipe(pipe, None) {
                    assert_eq!(
                        win32_code(&e),
                        ERROR_PIPE_CONNECTED.0,
                        "unexpected connect err"
                    );
                }
                let mut acc: Vec<u8> = Vec::new();
                let mut buf = [0u8; 1024];
                loop {
                    let mut n = 0u32;
                    if SrvRead(pipe, Some(&mut buf), Some(&mut n), None).is_err() || n == 0 {
                        break;
                    }
                    acc.extend_from_slice(&buf[..n as usize]);
                    if acc.contains(&b'\n') {
                        break;
                    }
                }
                if !hang {
                    let out = format!("{reply}\n");
                    let _ = SrvWrite(pipe, Some(out.as_bytes()), None, None);
                    let _ = FlushFileBuffers(pipe);
                } else {
                    // hang：持有连接不回——驱动客户端超时路径
                    thread::sleep(Duration::from_secs(2));
                }
                let _ = DisconnectNamedPipe(pipe);
                let _ = CloseHandle(pipe);
                String::from_utf8_lossy(&acc).trim().to_string()
            }
        });
        (rx, handle)
    }

    #[test]
    fn roundtrip_apply_over_named_pipe() {
        let name = format!(
            r"\\.\pipe\dd-mrm-hotapply-test-{}-apply",
            std::process::id()
        );
        let (ready, srv) = spawn_mock_server(
            &name,
            r#"{"id":1,"ok":true,"state":"applied","count":2}"#,
            false,
        );
        ready
            .recv_timeout(Duration::from_secs(5))
            .expect("server not ready");

        let mut client = PipeClient::connect_named(&name, Duration::from_secs(3)).unwrap();
        let entries = vec![
            ModEntry {
                title: "Mod A".into(),
                version: "1.0".into(),
            },
            ModEntry {
                title: "Mod B".into(),
                version: "2.0".into(),
            },
        ];
        let reply = client
            .request(
                &Request::Apply {
                    id: 1,
                    path: r"F:\saves\profile_2\persist.game.json",
                    entries: &entries,
                },
                Duration::from_secs(3),
            )
            .unwrap();
        assert!(reply.ok);
        assert_eq!(reply.state, "applied");
        assert_eq!(reply.count, Some(2));

        let got = srv.join().unwrap();
        assert!(got.starts_with(r#"{"id":1,"cmd":"apply""#), "line: {got}");
        assert!(got.contains(r#""path":"F:\\saves\\profile_2\\persist.game.json""#));
        assert!(got.contains(r#""entries":[{"title":"Mod A","version":"1.0"}"#));
    }

    #[test]
    fn reply_timeout_when_server_hangs() {
        let name = format!(r"\\.\pipe\dd-mrm-hotapply-test-{}-hang", std::process::id());
        let (ready, srv) = spawn_mock_server(&name, "", true);
        ready
            .recv_timeout(Duration::from_secs(5))
            .expect("server not ready");

        let t0 = Instant::now();
        let mut client = PipeClient::connect_named(&name, Duration::from_secs(3)).unwrap();
        let err = client
            .request(&Request::Status { id: 5 }, Duration::from_millis(300))
            .unwrap_err();
        assert!(matches!(err, PipeError::Timeout), "err = {err:?}");
        assert!(
            t0.elapsed() < Duration::from_secs(2),
            "timeout not enforced"
        );
        let _ = srv.join();
    }

    /// 注入后竞态（review 重要项 1）：DLL 建管道有延迟（InitThread 里日志/
    /// 特征码检查在前）。客户端必须在 ERROR_FILE_NOT_FOUND 上重试到 deadline，
    /// 而不是立即失败 —— 服务端延迟 400ms 才建管道，客户端预算 3s 应当连上。
    #[test]
    fn connect_retries_until_pipe_appears() {
        let name = format!(r"\\.\pipe\dd-mrm-hotapply-test-{}-late", std::process::id());
        let name2 = name.clone();
        let srv = thread::spawn(move || {
            thread::sleep(Duration::from_millis(400)); // 先让客户端吃几次 FILE_NOT_FOUND
            let (_ready, inner) =
                spawn_mock_server(&name2, r#"{"id":1,"ok":true,"state":"ready"}"#, false);
            let _ = inner.join();
        });
        let t0 = Instant::now();
        let mut client = PipeClient::connect_named(&name, Duration::from_secs(3))
            .expect("should retry FILE_NOT_FOUND until the pipe appears");
        assert!(
            t0.elapsed() >= Duration::from_millis(350),
            "connected too early: {:?}",
            t0.elapsed()
        );
        let reply = client
            .request(&Request::Status { id: 1 }, Duration::from_secs(2))
            .unwrap();
        assert_eq!(reply.state, "ready");
        let _ = srv.join();
    }

    /// 管道始终不存在：重试到 deadline 后报 ConnectTimeout（不再是立即
    /// ConnectFailed(2) —— 语义变化即本修复点）。
    #[test]
    fn connect_absent_pipe_times_out_after_deadline() {
        let name = format!(
            r"\\.\pipe\dd-mrm-hotapply-test-{}-absent",
            std::process::id()
        );
        let t0 = Instant::now();
        match PipeClient::connect_named(&name, Duration::from_millis(300)) {
            Err(PipeError::ConnectTimeout) => {}
            other => panic!("expected ConnectTimeout, got {other:?}"),
        }
        assert!(
            t0.elapsed() >= Duration::from_millis(280),
            "gave up before deadline: {:?}",
            t0.elapsed()
        );
    }

    /// 响应 id 与请求不符 = 串线/坏流，必须报错而不是误当本请求结果。
    #[test]
    fn request_rejects_id_mismatch() {
        let name = format!(
            r"\\.\pipe\dd-mrm-hotapply-test-{}-idmismatch",
            std::process::id()
        );
        let (ready, srv) = spawn_mock_server(
            &name,
            r#"{"id":999,"ok":true,"state":"applied","count":1}"#,
            false,
        );
        ready
            .recv_timeout(Duration::from_secs(5))
            .expect("server not ready");

        let mut client = PipeClient::connect_named(&name, Duration::from_secs(3)).unwrap();
        let err = client
            .request(&Request::Status { id: 5 }, Duration::from_secs(2))
            .unwrap_err();
        match &err {
            PipeError::Parse(m) => assert!(m.contains("id mismatch"), "msg: {m}"),
            other => panic!("expected Parse(id mismatch), got {other:?}"),
        }
        let _ = srv.join();
    }

    /// 编码护栏走独立错误档（TooManyEntries/LineTooLarge），不与解析错误混流。
    #[test]
    fn request_maps_encode_errors_distinctly() {
        let name = format!(r"\\.\pipe\dd-mrm-hotapply-test-{}-enc", std::process::id());
        let (ready, srv) = spawn_mock_server(&name, "", false);
        ready
            .recv_timeout(Duration::from_secs(5))
            .expect("server not ready");

        let mut client = PipeClient::connect_named(&name, Duration::from_secs(3)).unwrap();
        let entries: Vec<ModEntry> = (0..MAX_ENTRIES + 1)
            .map(|i| ModEntry {
                title: format!("m{i}"),
                version: "1".into(),
            })
            .collect();
        // 编码在写管道前失败 —— mock 服务端不应收到任何字节
        let err = client
            .request(
                &Request::Apply {
                    id: 1,
                    path: "profile_1",
                    entries: &entries,
                },
                Duration::from_secs(2),
            )
            .unwrap_err();
        assert!(matches!(
            err,
            PipeError::Encode(EncodeError::TooManyEntries(_))
        ));
        drop(client);
        let got = srv.join().unwrap();
        assert_eq!(got, "", "server must not receive anything");
    }
}
