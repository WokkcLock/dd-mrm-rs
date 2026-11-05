//! verify-inject —— 无游戏的注入冒烟（旧 native/smoke.ps1 的 Rust 版）。
//!
//! 用法：
//! ```text
//! cargo run -p hotapply --example verify-inject [log_dir]
//! ```
//! 不带参数时自动建 `%TEMP%\dd-mrm-verify-inject-<pid>`。
//!
//! 通过条件（与 smoke.ps1 相同）：注入 notepad 后，DLL 日志出现
//! `init enter` + `fatal: frame pattern mismatch`（notepad 没有游戏特征码，
//! DLL 干净退出），且 notepad 存活。退出码 0 = PASS。

use std::path::PathBuf;

use hotapply::verify::{NotepadVerifyReport, run_notepad_verification};

fn main() {
    let log_dir = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            std::env::temp_dir().join(format!("dd-mrm-verify-inject-{}", std::process::id()))
        });
    println!("log dir : {}", log_dir.display());
    match run_notepad_verification(&log_dir, None) {
        Ok(report) if report.pass() => {
            print_report(&report);
            println!("SMOKE PASS");
        }
        Ok(report) => {
            print_report(&report);
            eprintln!("SMOKE FAIL (see conditions above)");
            std::process::exit(1);
        }
        Err(hard) => {
            eprintln!("SMOKE FAIL (hard): {hard}");
            std::process::exit(1);
        }
    }
}

fn print_report(r: &NotepadVerifyReport) {
    println!("dll     : {}", r.dll_path.display());
    println!("inject  : {}", ok(r.inject_ok));
    println!("init-enter log   : {}", ok(r.saw_init_enter));
    println!("pattern-mismatch : {}", ok(r.saw_pattern_mismatch));
    println!("notepad alive    : {}", ok(r.host_alive_at_check));
    if !r.engine_log.trim().is_empty() {
        println!("--- engine.log ---");
        println!("{}", r.engine_log.trim_end());
        println!("------------------");
    }
}

fn ok(v: bool) -> &'static str {
    if v { "OK" } else { "MISSING" }
}
