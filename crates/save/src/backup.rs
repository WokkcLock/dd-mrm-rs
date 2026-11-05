//! estate 目录备份（旧 C# `BackupManager` 移植）与过期备份清理。

use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use crate::error::{Result, SaveError};

/// 把整个 `estate_dir` 递归打包为 `backup_dir/{label}_{时间戳}.zip`。
///
/// - 时间戳为 **UTC** `yyyy-MM-dd-HH-mm-ss`（旧工具用本地时间；UTC 可复现、
///   不依赖机器时区——记为偏差）。同一秒内对同一 label 二次备份 → `Err`
///   （旧 `ZipFile.CreateFromDirectory` 目标存在即抛，行为一致）。
/// - 目录条目也写入 zip（以 `/` 结尾），文件路径分隔符统一 `/`。
/// - 失败时尽力删除半成品 zip 再报错。
///
/// 返回生成的 zip 路径。
pub fn backup_estate_dir(estate_dir: &Path, label: &str, backup_dir: &Path) -> Result<PathBuf> {
    fs::create_dir_all(backup_dir).map_err(|e| SaveError::io(backup_dir, e))?;

    let (year, mon, day, hour, min, sec) = civil_from_unix_secs(now_unix_secs());
    let dest = backup_dir.join(format!(
        "{label}_{year:04}-{mon:02}-{day:02}-{hour:02}-{min:02}-{sec:02}.zip"
    ));
    if dest.exists() {
        return Err(SaveError::Backup {
            path: dest.clone(),
            reason: "备份文件已存在（同一秒内重复备份？）".to_owned(),
        });
    }

    let writer = File::create(&dest).map_err(|e| SaveError::io(&dest, e))?;
    let result = write_zip_recursive(estate_dir, writer);
    if result.is_err() {
        let _ = fs::remove_file(&dest); // 不留半成品
    }
    result.map(|()| dest)
}

fn write_zip_recursive(estate_dir: &Path, out: File) -> Result<()> {
    use std::io::copy;
    use zip::ZipWriter;
    use zip::write::SimpleFileOptions;

    let mut zip = ZipWriter::new(out);
    let base_options =
        SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);

    // 收集全部相对路径（目录+文件），按路径排序保证 zip 内容可复现。
    let mut rel_paths: Vec<PathBuf> = Vec::new();
    collect_rel_paths(estate_dir, Path::new(""), &mut rel_paths)?;
    rel_paths.sort();

    for rel in &rel_paths {
        let abs = estate_dir.join(rel);
        let name = rel_to_zip_name(rel);
        let options = match file_mtime_zip(&abs) {
            Some(t) => base_options.last_modified_time(t),
            None => base_options,
        };
        if abs.is_dir() {
            zip.add_directory(format!("{name}/"), options)
                .map_err(zip_err(&abs))?;
        } else {
            zip.start_file(name, options).map_err(zip_err(&abs))?;
            let mut f = File::open(&abs).map_err(|e| SaveError::io(&abs, e))?;
            copy(&mut f, &mut zip).map_err(|e| SaveError::io(&abs, e))?;
        }
    }
    zip.finish().map_err(zip_err(estate_dir))?;
    Ok(())
}

fn zip_err(path: &Path) -> impl Fn(zip::result::ZipError) -> SaveError + '_ {
    move |e: zip::result::ZipError| SaveError::Backup {
        path: path.to_path_buf(),
        reason: e.to_string(),
    }
}

fn collect_rel_paths(base: &Path, rel: &Path, out: &mut Vec<PathBuf>) -> Result<()> {
    let abs_dir = if rel.as_os_str().is_empty() {
        base.to_path_buf()
    } else {
        base.join(rel)
    };
    let mut entries: Vec<PathBuf> = fs::read_dir(&abs_dir)
        .map_err(|e| SaveError::io(&abs_dir, e))?
        .flatten()
        .map(|e| e.path())
        .collect();
    entries.sort();
    for path in entries {
        let child_rel = rel.join(path.file_name().expect("read_dir 条目必有文件名"));
        out.push(child_rel.clone());
        if path.is_dir() {
            collect_rel_paths(base, &child_rel, out)?;
        }
    }
    Ok(())
}

/// 相对路径 → zip 内部 `/` 分隔名。
fn rel_to_zip_name(rel: &Path) -> String {
    let mut name = String::new();
    for (i, comp) in rel.iter().enumerate() {
        if i > 0 {
            name.push('/');
        }
        name.push_str(&comp.to_string_lossy());
    }
    name
}

/// 文件 mtime → zip DateTime（越界裁剪到 zip 支持的 1980..=2107）。
fn file_mtime_zip(path: &Path) -> Option<zip::DateTime> {
    let meta = fs::metadata(path).ok()?;
    let modified = meta.modified().ok()?;
    let secs = modified
        .duration_since(SystemTime::UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    let (y, mon, d, h, min, s) = civil_from_unix_secs(secs);
    let (y, mon, d, h, min, s) = if y < 1980 {
        (1980, 1, 1, 0, 0, 0)
    } else if y > 2107 {
        (2107, 12, 31, 23, 59, 59)
    } else {
        (y, mon, d, h, min, s)
    };
    zip::DateTime::from_date_and_time(
        u16::try_from(y).ok()?,
        u8::try_from(mon).ok()?,
        u8::try_from(d).ok()?,
        u8::try_from(h).ok()?,
        u8::try_from(min).ok()?,
        u8::try_from(s).ok()?,
    )
    .ok()
}

/// 删除 mtime 早于 `now - max_age` 的 `*.zip`（旧 `CleanOldBackups`）。
///
/// 单个文件删除失败忽略（旧语义）；目录不存在 → `Ok(0)`。
/// 返回成功删除数。`max_age = 0` 意味着清空全部备份。
pub fn clean_old_backups(backup_dir: &Path, max_age: Duration) -> Result<usize> {
    let entries = match fs::read_dir(backup_dir) {
        Ok(entries) => entries,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(SaveError::io(backup_dir, e)),
    };

    let cutoff = SystemTime::now()
        .checked_sub(max_age)
        .unwrap_or(SystemTime::UNIX_EPOCH);
    let mut removed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        let is_zip = path
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("zip"));
        if !is_zip {
            continue;
        }
        let Ok(meta) = fs::metadata(&path) else {
            continue;
        };
        let Ok(mtime) = meta.modified() else {
            continue;
        };
        if mtime < cutoff && fs::remove_file(&path).is_ok() {
            removed += 1;
        }
    }
    Ok(removed)
}

fn now_unix_secs() -> i64 {
    SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Unix 秒 → UTC (年, 月, 日, 时, 分, 秒)。Howard Hinnant 的 civil_from_days。
fn civil_from_unix_secs(secs: i64) -> (i64, u32, u32, u32, u32, u32) {
    let days = secs.div_euclid(86_400);
    let secs_of_day = secs.rem_euclid(86_400);
    let (hour, minute, second) = (
        (secs_of_day / 3_600) as u32,
        ((secs_of_day % 3_600) / 60) as u32,
        (secs_of_day % 60) as u32,
    );

    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097); // [0, 146096]
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365; // [0, 399]
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100); // [0, 365]
    let mp = (5 * doy + 2) / 153; // [0, 11]
    let day = doy - (153 * mp + 2) / 5 + 1; // [1, 31]
    let month = if mp < 10 { mp + 3 } else { mp - 9 }; // [1, 12]
    let year = if month <= 2 { y + 1 } else { y };
    (year, month as u32, day as u32, hour, minute, second)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn civil_conversion_known_values() {
        assert_eq!(civil_from_unix_secs(0), (1970, 1, 1, 0, 0, 0));
        assert_eq!(civil_from_unix_secs(951_782_400), (2000, 2, 29, 0, 0, 0)); // 闰日
        // 2026-11-04T00:00:00Z = 20761 天 × 86400
        assert_eq!(civil_from_unix_secs(1_793_750_400), (2026, 11, 4, 0, 0, 0));
        assert_eq!(civil_from_unix_secs(-1), (1969, 12, 31, 23, 59, 59));
        // 时分秒换算
        assert_eq!(
            civil_from_unix_secs(1_793_750_400 + 13 * 3600 + 20 * 60 + 5),
            (2026, 11, 4, 13, 20, 5)
        );
    }
}
