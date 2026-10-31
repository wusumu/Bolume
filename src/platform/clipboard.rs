//! 往剪贴板写纯文本。
//!
//! 目前只有诊断报告用：让用户点一下就把整份信息复制走，直接粘给支持人员，
//! 不用去 exe 旁边翻那个 txt（很多人找不到）。

use windows::Win32::{
    Foundation::HANDLE,
    System::{
        DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData},
        Ole::CF_UNICODETEXT,
        Memory::{GMEM_MOVEABLE, GlobalAlloc, GlobalLock, GlobalUnlock},
    },
};

/// 把 `text` 设成剪贴板里的文本（覆盖原来的内容）。成功返回 `true`。
pub fn set_text(text: &str) -> bool {
    let wide: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes = wide.len() * std::mem::size_of::<u16>();
    unsafe {
        if OpenClipboard(None).is_err() {
            return false;
        }
        let _ = EmptyClipboard();
        let Ok(memory) = GlobalAlloc(GMEM_MOVEABLE, bytes) else {
            let _ = CloseClipboard();
            return false;
        };
        let pointer = GlobalLock(memory);
        if pointer.is_null() {
            let _ = CloseClipboard();
            return false;
        }
        std::ptr::copy_nonoverlapping(wide.as_ptr().cast::<u8>(), pointer.cast::<u8>(), bytes);
        let _ = GlobalUnlock(memory);
        // 交出去之后这块内存归系统管，不能再 `GlobalFree`。
        let ok = SetClipboardData(CF_UNICODETEXT.0 as u32, Some(HANDLE(memory.0))).is_ok();
        let _ = CloseClipboard();
        ok
    }
}
