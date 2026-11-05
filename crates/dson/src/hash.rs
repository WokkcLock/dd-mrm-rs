//! Darkest Dungeon 字符串哈希（ddHash）。
//!
//! 上游规格（robojumper dson.md）：`hash = hash*53 + byte`（wrapping i32），
//! UTF-8 字节序列，**不含结尾 NUL**。金样本 meta2.nameHash 全量验证一致
//! （见 task-2 报告：bit31 笔记 §2 的「含 NUL」与数据不符，按数据为准）。

/// 计算 DD 字符串哈希（wrapping，UTF-8 字节，不含 NUL）。
pub fn dd_hash(name: &str) -> i32 {
    let mut h: i32 = 0;
    for b in name.as_bytes() {
        h = h.wrapping_mul(53).wrapping_add(i32::from(*b));
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 期望值来自金样本 meta2.nameHash 实测（profile_2/7）。
    #[test]
    fn known_hashes() {
        assert_eq!(dd_hash("base_root"), 1183861218);
        assert_eq!(dd_hash("version"), -35461582);
        assert_eq!(dd_hash("applied_ugcs_1_0"), 1748800471);
        assert_eq!(dd_hash("persistent_ugcs"), 915706954);
        assert_eq!(dd_hash("presented_dlc"), -2018488792);
        assert_eq!(dd_hash("dlc"), 286723);
        assert_eq!(dd_hash("name"), 16654821);
        assert_eq!(dd_hash("source"), 1741429013);
        assert_eq!(dd_hash("0"), 48);
        assert_eq!(dd_hash("9"), 57);
    }

    #[test]
    fn wrapping_does_not_panic() {
        // 长输入触发 wrapping，不 panic 即可
        let long = "a".repeat(10_000);
        let _ = dd_hash(&long);
    }
}
