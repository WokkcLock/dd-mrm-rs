//! 编译期准备 DdHotapply.dll，供 dll.rs `include_bytes!` 嵌进 exe（自包含分发）。
//!
//! 优先级：
//! 1. `crates/hotapply/bin/DdHotapply.dll`（native/build.ps1 的产物，gitignored）
//! 2. 副本缺失、或 native/ 源比它新 → 自动调 `native/build.ps1` 重编（需 VS2022）
//! 3. 编译失败且无副本 → 写空占位 + `ddhotapply_placeholder` cfg：
//!    构建仍成功（无 VS 的贡献者可编译），运行时热应用报 dll-not-found。
//!
//! DLL 二进制不进 git（.gitignore 排除 bin/），仓库保持纯源码。

use std::env;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

fn main() {
    let manifest = PathBuf::from(env::var("CARGO_MANIFEST_DIR").unwrap());
    let repo = manifest
        .parent()
        .and_then(Path::parent)
        .unwrap()
        .to_path_buf();
    let native = repo.join("native");
    let bin_dll = manifest.join("bin").join("DdHotapply.dll");
    let out_dll = PathBuf::from(env::var("OUT_DIR").unwrap()).join("DdHotapply.dll");

    // dll.rs 的 #[cfg(ddhotapply_placeholder)] 需要 check-cfg 声明，
    // 否则 DLL 正常存在（cfg 未设置）时 unexpected_cfgs 误报。
    println!("cargo:rustc-check-cfg=cfg(ddhotapply_placeholder)");

    for p in [
        native.join("DdHotapply.c"),
        native.join("thunk64.asm"),
        native.join("build.ps1"),
        bin_dll.clone(),
    ] {
        println!("cargo:rerun-if-changed={}", p.display());
    }

    if needs_build(&native, &bin_dll) {
        let st = Command::new("powershell")
            .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
            .arg(native.join("build.ps1"))
            .current_dir(&repo)
            .output();
        if !matches!(&st, Ok(o) if o.status.success()) {
            println!("cargo:warning=native/build.ps1 失败（VS2022 缺失？）；尝试退回 bin/ 旧副本");
        }
    }

    if bin_dll.is_file() {
        fs::copy(&bin_dll, &out_dll).expect("copy DdHotapply.dll to OUT_DIR");
    } else {
        fs::write(&out_dll, b"").expect("write placeholder dll");
        println!("cargo:rustc-cfg=ddhotapply_placeholder");
        println!("cargo:warning=DdHotapply.dll 不可得：嵌入空占位，运行时热应用将报 dll-not-found（离线应用不受影响）");
    }
}

/// bin 副本缺失，或 native/ 任一源文件比它新 → 需要重编。
fn needs_build(native: &Path, bin_dll: &Path) -> bool {
    let Ok(dll_m) = fs::metadata(bin_dll).and_then(|m| m.modified()) else {
        return true;
    };
    ["DdHotapply.c", "thunk64.asm", "build.ps1"]
        .iter()
        .filter_map(|f| fs::metadata(native.join(f)).and_then(|m| m.modified()).ok())
        .any(|m| m > dll_m)
}
