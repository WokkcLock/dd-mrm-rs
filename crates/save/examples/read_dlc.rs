//! 一次性诊断：对比多份 persist 的 applied/dlc 条目值。
use save::EstateFile;

fn main() {
    for path in std::env::args().skip(1) {
        match EstateFile::open(std::path::Path::new(&path)) {
            Ok(f) => {
                println!("== {}", path);
                println!("  applied: {:?}", f.applied_mods());
                println!("  dlc:     {:?}", f.dlc_entries());
            }
            Err(e) => println!("== {} ERR {:?}", path, e),
        }
    }
}
