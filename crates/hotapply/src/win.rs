//! windows-rs 微助手（crate 内部）。

/// 字符串 → UTF-16 + NUL（所有 W 函数参数必须宽字符 —— 旧 C# 的 ANSI
/// marshalling bug 曾导致 DLL 一行日志都写不出来，绝不重蹈）。
pub(crate) fn wide_null(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// windows-rs 错误 → 数值码。
///
/// windows-result 0.4 的 `Error` 没有 `win32_error()`，这里按 HRESULT 手工解码：
/// `0x8007xxxx`（FACILITY_WIN32=7）低位 16 位即 Win32 错误码；否则原样返回
/// HRESULT 位型（跨设施错误也能显示成数字）。
pub(crate) fn win32_code(e: &windows::core::Error) -> u32 {
    let h = e.code().0 as u32;
    if (h >> 16) & 0x1FFF == 7 {
        h & 0xFFFF
    } else {
        h
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn win32_code_decodes_facility_win32() {
        // ERROR_FILE_NOT_FOUND(2) → HRESULT 0x80070002
        let e = windows::core::Error::from_hresult(windows::core::HRESULT(0x8007_0002u32 as i32));
        assert_eq!(win32_code(&e), 2);
        // 非 win32 设施：原样
        let e = windows::core::Error::from_hresult(windows::core::HRESULT(0x8013_0004u32 as i32));
        assert_eq!(win32_code(&e), 0x8013_0004);
    }
}
