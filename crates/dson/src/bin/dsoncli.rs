//! dsoncli — dson 的纯编解码检查 CLI（M1 实装）。
//!
//! 手写参数解析（GC5：std only，不用 clap）。路径一律 `OsString`：
//! 非 UTF-8 的 Windows 路径不得 panic（延迟到本任务的 T1 发现）。
//!
//! 退出码：`--help` → 0；无参数 → 2（用法到 stderr）；未知子命令 → 2；
//! 用法/IO/编解码错误 → 2；roundtrip/rewrite 字节不一致 → 1。

use std::ffi::{OsStr, OsString};
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use dson::meta::fi_bit31;
use dson::model::{DsonDoc, FieldValue};
use dson::parse;

const USAGE: &str = "\
dsoncli — DSON codec CLI (dson)

Usage:
  dsoncli --help
  dsoncli parse <file>              one-line structure summary
  dsoncli dump <file>               human-readable tree dump
  dsoncli rewrite <in> <out>        same-content rewrite (output must equal input)
  dsoncli roundtrip <file>          rewrite to a temp file, byte-compare, exit 0/1
";

fn main() -> ExitCode {
    let mut args = std::env::args_os().skip(1);
    let sub = args.next();
    let rest: Vec<OsString> = args.collect();
    match sub.as_deref().and_then(OsStr::to_str) {
        None => {
            eprint!("{USAGE}");
            ExitCode::from(2)
        }
        Some("--help") | Some("-h") | Some("help") => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        Some("parse") => cmd_parse(&rest),
        Some("dump") => cmd_dump(&rest),
        Some("rewrite") => cmd_rewrite(&rest),
        Some("roundtrip") => cmd_roundtrip(&rest),
        Some(other) => {
            eprintln!("error: unknown subcommand `{other}`");
            eprint!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

/// 读文件 + 解析；错误消息带路径上下文。
fn load(path: &OsStr) -> Result<DsonDoc, String> {
    let bytes =
        fs::read(Path::new(path)).map_err(|e| format!("{}: {e}", Path::new(path).display()))?;
    parse(&bytes).map_err(|e| format!("{}: {e}", Path::new(path).display()))
}

fn expect_args(rest: &[OsString], n: usize, usage: &str) -> Result<(), String> {
    if rest.len() != n {
        Err(format!("error: {usage}"))
    } else {
        Ok(())
    }
}

fn cmd_parse(rest: &[OsString]) -> ExitCode {
    if let Err(e) = expect_args(rest, 1, "parse takes exactly one <file>") {
        eprintln!("{e}");
        return ExitCode::from(2);
    }
    match load(&rest[0]) {
        Ok(doc) => {
            println!("{} {}", Path::new(&rest[0]).display(), one_line_stats(&doc));
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("dsoncli: {e}");
            ExitCode::from(2)
        }
    }
}

fn cmd_dump(rest: &[OsString]) -> ExitCode {
    if let Err(e) = expect_args(rest, 1, "dump takes exactly one <file>") {
        eprintln!("{e}");
        return ExitCode::from(2);
    }
    match load(&rest[0]) {
        Ok(doc) => {
            print!("{}", doc.dump_string());
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("dsoncli: {e}");
            ExitCode::from(2)
        }
    }
}

fn cmd_rewrite(rest: &[OsString]) -> ExitCode {
    if let Err(e) = expect_args(rest, 2, "rewrite takes exactly <in> <out>") {
        eprintln!("{e}");
        return ExitCode::from(2);
    }
    let (in_path, out_path) = (&rest[0], &rest[1]);
    let run = || -> Result<Vec<u8>, String> {
        let bytes = fs::read(Path::new(in_path))
            .map_err(|e| format!("{}: {e}", Path::new(in_path).display()))?;
        let doc = parse(&bytes).map_err(|e| format!("{}: {e}", Path::new(in_path).display()))?;
        doc.write().map_err(|e| format!("writer: {e}"))
    };
    match run() {
        Ok(out) => {
            if let Err(e) = fs::write(Path::new(out_path), &out) {
                eprintln!("dsoncli: {}: {e}", Path::new(out_path).display());
                return ExitCode::from(2);
            }
            // 同内容重写必须与输入逐字节一致
            match fs::read(Path::new(in_path)) {
                Ok(orig) if orig == out => {
                    println!(
                        "rewrite ok: {} == {} ({} bytes)",
                        Path::new(out_path).display(),
                        Path::new(in_path).display(),
                        out.len()
                    );
                    ExitCode::SUCCESS
                }
                Ok(_) => {
                    eprintln!("rewrite MISMATCH: output differs from input");
                    ExitCode::FAILURE
                }
                Err(e) => {
                    eprintln!("dsoncli: re-read failed: {e}");
                    ExitCode::from(2)
                }
            }
        }
        Err(e) => {
            eprintln!("dsoncli: {e}");
            ExitCode::from(2)
        }
    }
}

fn cmd_roundtrip(rest: &[OsString]) -> ExitCode {
    if let Err(e) = expect_args(rest, 1, "roundtrip takes exactly one <file>") {
        eprintln!("{e}");
        return ExitCode::from(2);
    }
    let path = PathBuf::from(&rest[0]);
    let mut tmp = OsString::from(path.as_os_str());
    tmp.push(".rt.tmp");
    let tmp = PathBuf::from(&tmp);
    let run = || -> Result<bool, String> {
        let bytes = fs::read(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        let doc = parse(&bytes).map_err(|e| format!("{}: {e}", path.display()))?;
        let out = doc.write().map_err(|e| format!("writer: {e}"))?;
        fs::write(&tmp, &out).map_err(|e| format!("{}: {e}", tmp.display()))?;
        Ok(out == bytes)
    };
    let verdict = match run() {
        Ok(v) => v,
        Err(e) => {
            eprintln!("dsoncli: {e}");
            let _ = fs::remove_file(&tmp);
            return ExitCode::from(2);
        }
    };
    if let Err(e) = fs::remove_file(&tmp) {
        eprintln!("dsoncli: cleanup of {} failed: {e}", tmp.display());
    }
    if verdict {
        println!(
            "roundtrip ok: {} ({} bytes identical)",
            path.display(),
            fs::metadata(&path).map(|m| m.len()).unwrap_or(0)
        );
        ExitCode::SUCCESS
    } else {
        eprintln!(
            "roundtrip FAILED: re-encoded bytes differ from {}",
            path.display()
        );
        ExitCode::FAILURE
    }
}

/// 一行统计。
fn one_line_stats(doc: &DsonDoc) -> String {
    let mut objects = 0usize;
    let mut str_n = 0usize;
    let mut bool_n = 0usize;
    let mut int_n = 0usize;
    let mut twobool_n = 0usize;
    let mut bit31 = 0usize;
    for n in doc.nodes() {
        if n.is_object {
            objects += 1;
        }
        match n.value.as_ref() {
            Some(FieldValue::Str(_)) => str_n += 1,
            Some(FieldValue::Bool(_)) => bool_n += 1,
            Some(FieldValue::Int(_)) => int_n += 1,
            Some(FieldValue::TwoBool(_, _)) => twobool_n += 1,
            None => {}
        }
        if n.source_field_info().is_some_and(fi_bit31) {
            bit31 += 1;
        }
    }
    let leaves = str_n + bool_n + int_n + twobool_n;
    format!(
        "root={} entries={} objects={} leaves={} (Str {} Bool {} Int {} TwoBool {}) bit31={} rev={:02x?} data={}B",
        doc.root_name(),
        doc.nodes().len(),
        objects,
        leaves,
        str_n,
        bool_n,
        int_n,
        twobool_n,
        bit31,
        &doc.header_bytes()[4..8],
        doc.data_len(),
    )
}
