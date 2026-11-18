//! DdHotapply.dll 定位 —— 搜索顺序（前者优先）：
//! 1. 环境变量 `DDHOTAPPLY_DLL`（显式覆盖，路径写错也会继续向下回退）
//! 2. `exe目录/DdHotapply.dll`（随 T5 程序部署的标准位置）
//! 3. `exe目录/eng/native/DdHotapply.dll`（旧 C# 工具的布局兼容）
//!    （2、3 都是 exe 目录树内受管位：内容与嵌入副本不一致时自动覆盖为
//!    嵌入版 —— 防旧部署/旧工具布局残留漂移，见 refresh_if_stale）
//! 4. `crates/hotapply/bin/DdHotapply.dll`（native/build.ps1 的产物，开发态）
//! 5. 内嵌副本兜底：build.rs 在编译期把 DLL 嵌进 exe（见 build.rs），
//!    前 4 项全未命中（部署机场景）时解压到 exe 目录；目录只读则退
//!    `%TEMP%\DdModToolHotapply\`（与日志兜底同目录约定）。原子落位（GC4）。
//!
//! 全部未命中（且无内嵌）→ [`DllDiscoveryError`]，列出已搜索路径（任务要求）。

use std::fmt;
use std::path::{Path, PathBuf};

/// DLL 文件名。
pub const DLL_FILE_NAME: &str = "DdHotapply.dll";
/// 覆盖用环境变量名。
pub const ENV_OVERRIDE: &str = "DDHOTAPPLY_DLL";

/// 定位失败：附带按序搜索过的完整路径列表。
#[derive(Debug, Clone)]
pub struct DllDiscoveryError {
    pub searched: Vec<PathBuf>,
}

impl fmt::Display for DllDiscoveryError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let list = self
            .searched
            .iter()
            .map(|p| p.display().to_string())
            .collect::<Vec<_>>()
            .join(" | ");
        write!(f, "dll-not-found (searched: {list})")
    }
}

impl std::error::Error for DllDiscoveryError {}

/// exe 目录标准位（自家管理的解压位置）：以嵌入副本为权威，内容漂移
/// （旧部署残留、旧版自解压产物）→ 原子覆盖为新版。见 [`locate_dll`]。
pub fn locate_dll() -> Result<PathBuf, DllDiscoveryError> {
    let candidates = candidate_paths(
        std::env::var(ENV_OVERRIDE).ok().filter(|s| !s.is_empty()),
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(Path::to_path_buf)),
        Path::new(env!("CARGO_MANIFEST_DIR")),
    );
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(Path::to_path_buf));
    // exe 目录树内的两个候选位（标准位 + eng/native 旧布局兼容位）都以嵌入
    // 副本为权威：内容漂移（旧部署/旧工具布局残留）→ 覆盖为嵌入版。env 覆
    // 盖不校验（用户显式指定）；仓库内开发态候选（bin/）是
    // build.rs 权威产物，自然一致。覆盖失败（目标被映射）→ 沿用旧文件。
    let managed_positions = exe_dir.as_ref().map(|d| {
        [
            d.join(DLL_FILE_NAME),
            d.join("eng").join("native").join(DLL_FILE_NAME),
        ]
    });
    for p in &candidates {
        if p.is_file() {
            if let Some(managed) = &managed_positions {
                if managed.contains(p) {
                    refresh_if_stale(p);
                }
            }
            return Ok(p.clone());
        }
    }
    // 前 5 项全未命中：部署机场景 —— 解压编译期嵌入的副本（build.rs 准备）。
    if !EMBEDDED_DLL.is_empty() {
        if let Some(p) = extract_embedded() {
            return Ok(p);
        }
    }
    Err(DllDiscoveryError {
        searched: candidates,
    })
}

/// 组装候选路径（纯函数，便于测试）。
///
/// * `env_override` - `DDHOTAPPLY_DLL` 的值（空串视为未设置）
/// * `exe_dir` - 当前可执行文件目录（单测传 tempdir）
/// * `manifest_dir` - 本 crate 目录（编译期常量；单测传 fake）
pub fn candidate_paths(
    env_override: Option<String>,
    exe_dir: Option<PathBuf>,
    manifest_dir: &Path,
) -> Vec<PathBuf> {
    let mut v = Vec::with_capacity(4);
    if let Some(p) = env_override.filter(|s| !s.is_empty()) {
        v.push(PathBuf::from(p));
    }
    if let Some(d) = exe_dir {
        v.push(d.join(DLL_FILE_NAME));
        v.push(d.join("eng").join("native").join(DLL_FILE_NAME));
    }
    // 开发态：仓库内提交的预编译副本
    v.push(manifest_dir.join("bin").join(DLL_FILE_NAME));
    v
}

/// 编译期嵌入的 DLL 字节（build.rs 从 native/ 构建链准备到 OUT_DIR）。
/// 无 VS 环境下 build.rs 写空占位文件 → 空切片，运行时跳过兜底。
static EMBEDDED_DLL: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/DdHotapply.dll"));

/// 把嵌入副本解压到 exe 目录；目录只读时退到 `%TEMP%\DdModToolHotapply\`。
/// 已解压过（目标存在）则直接复用，不重复写盘。
fn extract_embedded() -> Option<PathBuf> {
    if EMBEDDED_DLL.is_empty() {
        return None;
    }
    let exe_dir = std::env::current_exe()
        .ok()?
        .parent()?
        .to_path_buf();
    if let Some(p) = write_dll_atomically(&exe_dir) {
        return Some(p);
    }
    let tmp = std::env::temp_dir().join("DdModToolHotapply");
    write_dll_atomically(&tmp)
}

/// `dir/DdHotapply.dll` 不存在则原子写入（.tmp + rename，GC4），存在则复用。
fn write_dll_atomically(dir: &Path) -> Option<PathBuf> {
    let target = dir.join(DLL_FILE_NAME);
    if target.is_file() {
        return Some(target);
    }
    std::fs::create_dir_all(dir).ok()?;
    let tmp = dir.join(format!("{DLL_FILE_NAME}.tmp"));
    std::fs::write(&tmp, EMBEDDED_DLL).ok()?;
    match std::fs::rename(&tmp, &target) {
        Ok(()) => Some(target),
        // 并发解压竞态：另一实例已落位 → 复用它的
        Err(_) => {
            let _ = std::fs::remove_file(&tmp);
            target.is_file().then_some(target)
        }
    }
}

/// 防漂移：`path`（exe 目录树内受管位）内容与嵌入副本不一致 → 原子覆盖
/// （.tmp + rename，GC4）。一致或嵌入为空（占位构建）→ 不动。
/// 覆盖失败（目标被映射/占用）→ 静默退回旧文件，下次再试。
fn refresh_if_stale(path: &Path) {
    if EMBEDDED_DLL.is_empty() {
        return;
    }
    if let Ok(bytes) = std::fs::read(path) {
        if bytes == EMBEDDED_DLL {
            return;
        }
    }
    let Some(dir) = path.parent() else { return };
    let tmp = dir.join(format!("{DLL_FILE_NAME}.tmp"));
    if std::fs::write(&tmp, EMBEDDED_DLL).is_ok() {
        if std::fs::rename(&tmp, path).is_ok() {
            return;
        }
        let _ = std::fs::remove_file(&tmp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn touch(dir: &Path, rel: &[&str]) -> PathBuf {
        let p = dir.join(rel.iter().collect::<PathBuf>());
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(&p, b"MZ dummy").unwrap();
        p
    }

    #[test]
    fn order_env_first_then_exe_then_eng_native() {
        let tmp = std::env::temp_dir().join(format!("dd-mrm-ha-dll-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        // 三处都有 → 环境变量赢
        let env_dll = touch(&tmp, &["env", "DdHotapply.dll"]);
        touch(&tmp, &["exe", "DdHotapply.dll"]);
        touch(&tmp, &["exe", "eng", "native", "DdHotapply.dll"]);
        let paths = candidate_paths(
            Some(env_dll.to_string_lossy().into_owned()),
            Some(tmp.join("exe")),
            Path::new("/nonexistent-manifest"),
        );
        assert_eq!(paths.len(), 4);
        assert_eq!(paths[0], env_dll, "env override must be first");
        assert!(
            paths[0].is_file(),
            "env override dll exists → must be picked"
        );
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn order_exe_dir_beats_eng_native() {
        let tmp = std::env::temp_dir().join(format!("dd-mrm-ha-dll2-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let expected = touch(&tmp, &["exe", "DdHotapply.dll"]);
        touch(&tmp, &["exe", "eng", "native", "DdHotapply.dll"]);
        let paths = candidate_paths(
            None,
            Some(tmp.join("exe")),
            Path::new("/nonexistent-manifest"),
        );
        // 候选 2 命中（无 env 覆盖）
        assert_eq!(paths[0], expected);
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn manifest_bin_and_build_ps1_outfit_are_last_resort() {
        let tmp = std::env::temp_dir().join(format!("dd-mrm-ha-dll3-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        let manifest = tmp.join("crates").join("hotapply");
        let bin_dll = touch(&manifest, &["bin", "DdHotapply.dll"]);
        // exe 目录无 DLL、无 env → manifest/bin 命中（无 env 时它是第 3 候选）
        let paths = candidate_paths(None, Some(tmp.join("exe")), &manifest);
        assert_eq!(paths.len(), 3);
        assert_eq!(paths[2], bin_dll);
        std::fs::remove_dir_all(&tmp).ok();
    }

    #[test]
    fn missing_everywhere_lists_all_searched() {
        let paths = candidate_paths(
            None,
            Some(PathBuf::from("/definitely/not/here")),
            Path::new("/also/not/here"),
        );
        let err = DllDiscoveryError { searched: paths };
        let msg = err.to_string();
        assert!(msg.starts_with("dll-not-found (searched:"), "{msg}");
        assert!(msg.contains("DdHotapply.dll"), "{msg}");
        assert!(msg.matches("DdHotapply.dll").count() >= 3, "{msg}");
    }

    #[test]
    fn empty_env_override_ignored() {
        let paths = candidate_paths(
            Some(String::new()),
            Some(PathBuf::from("/x")),
            Path::new("/m"),
        );
        assert_eq!(paths.len(), 3, "空串 env 不进候选");
    }

    #[cfg(not(ddhotapply_placeholder))]
    #[test]
    fn embedded_dll_is_real_pe_image() {
        assert!(EMBEDDED_DLL.starts_with(b"MZ"), "嵌入字节应为 PE 镜像");
        assert!(EMBEDDED_DLL.len() > 1024, "占位才 <1KB，真 DLL 应远大于此");
    }

    #[cfg(not(ddhotapply_placeholder))]
    #[test]
    fn refresh_overwrites_stale_and_idempotent() {
        let dir = std::env::temp_dir().join(format!("dd-mrm-ha-refresh-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        let p = dir.join(DLL_FILE_NAME);
        // 旧部署残留（字节≠嵌入）→ 覆盖
        std::fs::write(&p, b"MZ stale-bytes").unwrap();
        refresh_if_stale(&p);
        assert_eq!(std::fs::read(&p).unwrap(), EMBEDDED_DLL, "残留旧 DLL 必须被嵌入版覆盖");
        // 已是嵌入版 → 不动（幂等，且不写 .tmp）
        refresh_if_stale(&p);
        assert!(!dir.join(format!("{DLL_FILE_NAME}.tmp")).exists(), "一致时不得留 .tmp");
        assert_eq!(std::fs::read(&p).unwrap(), EMBEDDED_DLL);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn write_dll_atomically_extracts_then_reuses() {
        let dir = std::env::temp_dir().join(format!("dd-mrm-ha-emb-{}", std::process::id()));
        std::fs::remove_dir_all(&dir).ok();
        std::fs::create_dir_all(&dir).unwrap();
        // 空目录 → 首次解压，字节与嵌入一致
        let p = write_dll_atomically(&dir).expect("空目录应成功解压");
        assert!(p.is_file());
        assert_eq!(std::fs::read(&p).unwrap(), EMBEDDED_DLL, "落盘字节 = 嵌入字节");
        // 目标已存在 → 复用，不覆盖
        std::fs::write(&p, b"stale-bytes").unwrap();
        let p2 = write_dll_atomically(&dir).unwrap();
        assert_eq!(p2, p);
        assert_eq!(std::fs::read(&p).unwrap(), b"stale-bytes", "已存在时不得重写");
        std::fs::remove_dir_all(&dir).ok();
    }
}
