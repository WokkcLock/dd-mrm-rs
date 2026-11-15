//! E1e 实验：真实 mod 列表变更走生产路径（EstateFile::sync_mods）。
//! 用法:
//!   e1e_mods <persist.game.json> list
//!   e1e_mods <persist.game.json> remove <index>
//!   e1e_mods <persist.game.json> add <name> <mod_local_source|Steam>

use std::path::Path;

use save::apply::EstateFile;
use save::mod_model::{AppliedMod, ModSource};

fn die(msg: String) -> ! {
    eprintln!("error: {msg}");
    std::process::exit(2);
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        die("usage: e1e_mods <persist.game.json> <list|remove|add>".into());
    }
    let path = Path::new(&args[1]);
    let mut ef = EstateFile::open(path).unwrap_or_else(|e| die(format!("open: {e:?}")));
    println!(
        "estate_name={:?} size={}",
        ef.estate_name(),
        std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
    );
    match args[2].as_str() {
        "list" => {}
        "remove" => {
            if args.len() != 4 {
                die("remove needs <index>".into());
            }
            let idx: usize = args[3].parse().unwrap_or_else(|_| die("bad index".into()));
            let mut mods = ef.applied_mods().unwrap_or_else(|e| die(format!("mods: {e:?}")));
            if idx >= mods.len() {
                die(format!("index {idx} out of range {}", mods.len()));
            }
            let removed = mods.remove(idx);
            ef.sync_mods(&mods).unwrap_or_else(|e| die(format!("sync: {e:?}")));
            ef.write().unwrap_or_else(|e| die(format!("write: {e:?}")));
            println!(
                "removed name={:?} source={}",
                removed.name,
                removed.source.as_dson_str()
            );
        }
        "add" => {
            if args.len() != 5 {
                die("add needs <name> <source>".into());
            }
            let src = ModSource::from_dson_str(&args[4]).unwrap_or_else(|| die("bad source".into()));
            let mut mods = ef.applied_mods().unwrap_or_else(|e| die(format!("mods: {e:?}")));
            mods.push(AppliedMod { name: args[3].clone(), source: src });
            ef.sync_mods(&mods).unwrap_or_else(|e| die(format!("sync: {e:?}")));
            ef.write().unwrap_or_else(|e| die(format!("write: {e:?}")));
            println!("added name={:?} source={}", args[3], args[4]);
        }
        _ => die("unknown op".into()),
    }
    let ef2 = EstateFile::open(path).unwrap_or_else(|e| die(format!("reopen: {e:?}")));
    println!("readback estate_name={:?}", ef2.estate_name());
    for (i, m) in ef2.applied_mods().unwrap_or_else(|e| die(format!("mods2: {e:?}"))).iter().enumerate() {
        println!("readback mod[{i}] name={:?} source={}", m.name, m.source.as_dson_str());
    }
    println!("final_size={}", std::fs::metadata(path).map(|m| m.len()).unwrap_or(0));
}
