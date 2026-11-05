//! DdHotapply.dll 定位 —— 搜索顺序（前者优先）：
//! 1. 环境变量 `DDHOTAPPLY_DLL`（显式覆盖，路径写错也会继续向下回退）
//! 2. `exe目录/DdHotapply.dll`（随 T5 程序部署的标准位置）
//! 3. `exe目录/eng/native/DdHotapply.dll`（旧 C# 工具的布局兼容）
//! 4. `crates/hotapply/bin/DdHotapply.dll`（本仓库提交的预编译副本，开发态）
//! 5. `<repo>/Gui/eng/native/DdHotapply.dll`（native/build.ps1 的产物目录，开发态）
//!
//! 全部未命中 → [`DllDiscoveryError`]，列出已搜索路径（任务要求）。

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

/// 按上述顺序定位 DLL。
pub fn locate_dll() -> Result<PathBuf, DllDiscoveryError> {
    let candidates = candidate_paths(
        std::env::var(ENV_OVERRIDE).ok().filter(|s| !s.is_empty()),
        std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(Path::to_path_buf)),
        Path::new(env!("CARGO_MANIFEST_DIR")),
    );
    for p in &candidates {
        if p.is_file() {
            return Ok(p.clone());
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
    let mut v = Vec::with_capacity(5);
    if let Some(p) = env_override.filter(|s| !s.is_empty()) {
        v.push(PathBuf::from(p));
    }
    if let Some(d) = exe_dir {
        v.push(d.join(DLL_FILE_NAME));
        v.push(d.join("eng").join("native").join(DLL_FILE_NAME));
    }
    // 开发态：仓库内提交的预编译副本 + native/build.ps1 产物目录
    v.push(manifest_dir.join("bin").join(DLL_FILE_NAME));
    v.push(
        manifest_dir
            .join("..")
            .join("..")
            .join("Gui")
            .join("eng")
            .join("native")
            .join(DLL_FILE_NAME),
    );
    v
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
        assert_eq!(paths.len(), 5);
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
        assert_eq!(paths.len(), 4);
        assert_eq!(paths[2], bin_dll);
        // build.ps1 产物路径形态正确（<repo>/Gui/eng/native），排最后
        // （含未折叠的 ..\..\ 组件，用后缀断言而非全路径相等）
        assert!(
            paths[3].ends_with(r"Gui\eng\native\DdHotapply.dll"),
            "{}",
            paths[3].display()
        );
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
        assert!(msg.matches("DdHotapply.dll").count() >= 4, "{msg}");
    }

    #[test]
    fn empty_env_override_ignored() {
        let paths = candidate_paths(
            Some(String::new()),
            Some(PathBuf::from("/x")),
            Path::new("/m"),
        );
        assert_eq!(paths.len(), 4, "空串 env 不进候选");
    }
}
