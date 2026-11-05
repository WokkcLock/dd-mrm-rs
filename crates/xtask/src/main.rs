//! cargo dist：构建 release 并组装 dist\（逻辑对齐原 scripts/dist.ps1）。
//! 入口：`.cargo/config.toml` 别名 `dist = "run -p xtask --release -- dist"`。

use std::env;
use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let root = repo_root();
    match env::args().nth(1).as_deref() {
        Some("dist") | None => dist(&root),
        Some(other) => {
            eprintln!("xtask: 未知任务 `{other}`（目前只有 `dist`）");
            std::process::exit(2);
        }
    }
}

/// 仓库根 = 本 crate 的 manifest 目录（crates/xtask）向上两级。
fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("crates/xtask 应有上级目录")
        .to_path_buf()
}

fn dist(root: &Path) {
    // 1. cargo build --release（由 cargo run 启动，CARGO 指向当前工具链）。
    let cargo = env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = Command::new(&cargo)
        .args(["build", "--release"])
        .current_dir(root)
        .status()
        .unwrap_or_else(|e| panic!("无法启动 cargo：{e}"));
    if !status.success() {
        std::process::exit(status.code().unwrap_or(1));
    }

    // 2. 组装 dist\：exe 复制进去；DLL 已嵌入 exe，清掉历史随行副本防误导。
    let dist_dir = root.join("dist");
    fs::create_dir_all(&dist_dir).expect("创建 dist\\ 失败");
    let src = root.join("target").join("release").join("dd-mrm.exe");
    let dst = dist_dir.join("dd-mrm.exe");
    if let Err(e) = fs::copy(&src, &dst) {
        panic!("复制 dd-mrm.exe 失败：{e}（若 dist\\dd-mrm.exe 正在运行请先关闭）");
    }
    match fs::remove_file(dist_dir.join("DdHotapply.dll")) {
        Ok(()) => println!("已清理历史 DLL 副本 dist\\DdHotapply.dll"),
        Err(e) if e.kind() == ErrorKind::NotFound => {}
        Err(e) => panic!("清理 DLL 失败：{e}"),
    }

    // 3. 列出产物。
    println!();
    println!("dist\\ 内容：");
    for entry in fs::read_dir(&dist_dir).expect("读 dist\\ 失败") {
        let entry = entry.expect("读 dist\\ 条目失败");
        let len = entry.metadata().expect("读元数据失败").len();
        println!(
            "  {:<24} {:>8.1} KB",
            entry.file_name().to_string_lossy(),
            len as f64 / 1024.0
        );
    }
}
