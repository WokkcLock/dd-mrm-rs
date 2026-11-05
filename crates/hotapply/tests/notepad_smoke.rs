//! notepad 冒烟（集成档，默认 ignore：需要 GUI 会话 + notepad —— GC8 真机
//! 条件本机开发会话满足；CI/无 GUI 环境跳过）。
//!
//! 跑法：`cargo test -p hotapply --test notepad_smoke -- --ignored`

use hotapply::verify::run_notepad_verification;

#[test]
#[ignore = "needs notepad + interactive GUI session (real-machine condition)"]
fn notepad_inject_pattern_mismatch_clean_exit() {
    let log_dir = std::env::temp_dir().join(format!(
        "dd-mrm-ha-smoke-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
    ));
    let report = run_notepad_verification(&log_dir, None)
        .unwrap_or_else(|hard| panic!("hard failure: {hard}"));
    assert!(
        report.inject_ok,
        "inject must succeed: {:?}",
        report.engine_log
    );
    assert!(
        report.saw_init_enter,
        "no 'init enter' in log:\n{}",
        report.engine_log
    );
    assert!(
        report.saw_pattern_mismatch,
        "no 'fatal: frame pattern mismatch' in log:\n{}",
        report.engine_log
    );
    assert!(
        report.host_alive_at_check,
        "notepad must survive DLL clean-exit"
    );
    std::fs::remove_dir_all(&log_dir).ok();
}
