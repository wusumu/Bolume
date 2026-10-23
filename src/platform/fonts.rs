//! 枚举系统字体族：托盘数字图标让用户自己选字体用。
//!
//! 走 `EnumFontFamiliesExW`，和系统「字体设置」看到的是同一份列表。
//! 结果进程内缓存一次——枚举一遍几百个字体要几十毫秒，没必要每帧重来。

use std::cell::RefCell;

use windows::Win32::{
    Foundation::LPARAM,
    Graphics::Gdi::{
        DEFAULT_CHARSET, EnumFontFamiliesExW, GetDC, LOGFONTW, ReleaseDC, TEXTMETRICW,
    },
};

thread_local! {
    static CACHE: RefCell<Option<Vec<String>>> = const { RefCell::new(None) };
}

/// 系统里能用的字体族名（已去重、已排序、去掉 `@` 开头的竖排变体）。
pub fn families() -> Vec<String> {
    if let Some(cached) = CACHE.with(|cache| cache.borrow().clone()) {
        return cached;
    }

    let mut names: Vec<String> = Vec::new();
    unsafe {
        let dc = GetDC(None);
        if !dc.is_invalid() {
            let mut logfont = LOGFONTW {
                lfCharSet: DEFAULT_CHARSET,
                ..Default::default()
            };
            // 空字体名 = 枚举全部族；DEFAULT_CHARSET 会把每个字符集都报一遍，
            // 所以回调里统一按名字去重。
            logfont.lfFaceName[0] = 0;
            let _ = EnumFontFamiliesExW(
                dc,
                &logfont,
                Some(collect),
                LPARAM(&mut names as *mut _ as isize),
                0,
            );
            let _ = ReleaseDC(None, dc);
        }
    }

    names.sort_by_key(|name| name.to_lowercase());
    names.dedup_by(|a, b| a.eq_ignore_ascii_case(b));
    CACHE.with(|cache| *cache.borrow_mut() = Some(names.clone()));
    names
}

unsafe extern "system" fn collect(
    logfont: *const LOGFONTW,
    _metric: *const TEXTMETRICW,
    _font_type: u32,
    lparam: LPARAM,
) -> i32 {
    let list = unsafe { &mut *(lparam.0 as *mut Vec<String>) };
    let font = unsafe { &*logfont };
    let name = face_name(&font.lfFaceName);
    // '@' 开头的是同一族的中文竖排版本，选来当水平文字毫无意义。
    if !name.is_empty() && !name.starts_with('@') {
        list.push(name);
    }
    // 非 0 = 继续枚举。
    1
}

fn face_name(raw: &[u16; 32]) -> String {
    let len = raw.iter().position(|unit| *unit == 0).unwrap_or(raw.len());
    String::from_utf16_lossy(&raw[..len])
}

#[cfg(test)]
mod tests {
    /// 系统里至少得有字体；列表要排好序且不含竖排变体。
    #[test]
    fn families_are_sorted_and_horizontal() {
        let names = super::families();
        assert!(!names.is_empty(), "枚举不到任何字体");
        assert!(
            names
                .iter()
                .any(|name| name.eq_ignore_ascii_case("Segoe UI")),
            "列表里没有 Segoe UI"
        );
        assert!(!names.iter().any(|name| name.starts_with('@')));
        let mut sorted = names.clone();
        sorted.sort_by_key(|name| name.to_lowercase());
        assert_eq!(names, sorted, "字体列表没有按名字排序");
    }
}
