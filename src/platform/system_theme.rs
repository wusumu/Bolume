//! 系统亮暗与系统强调色。
//!
//! 亮暗只读 `HKCU\...\Themes\Personalize\AppsUseLightTheme`，强调色读
//! `HKCU\...\DWM\AccentColor`；两个都只读不写 —— 那是系统设置页的地盘。
//! 跟随系统时每次取主题都现读一次，所以用户改完 Windows 设置立刻就是对的
//! （界面那边还有一条巡检，负责把已经开着的窗口也重画一遍）。

use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
use windows::core::PCWSTR;

const PERSONALIZE_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize";
const VALUE_NAME: &str = "AppsUseLightTheme";
/// 系统强调色（`0xAABBGGRR`，和 `COLORREF` 一样倒着存，高字节是 alpha）。
const DWM_KEY: &str = "Software\\Microsoft\\Windows\\DWM";
const ACCENT_NAME: &str = "AccentColor";

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 读一个 `REG_DWORD`；读不到返回 `None`。
fn dword(key: &str, name: &str) -> Option<u32> {
    let subkey = wide(key);
    let name = wide(name);
    let mut value: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey.as_ptr()),
            PCWSTR(name.as_ptr()),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut value as *mut u32 as *mut _),
            Some(&mut size),
        )
    };
    status.is_ok().then_some(value)
}

/// 系统当前是不是深色。
///
/// `AppsUseLightTheme` 为 1 表示亮色、0 表示深色。读不到（键缺失 / 被策略锁住）
/// 时按深色算：主题的默认那套就是深色，猜错时退回默认不刺眼。
pub fn is_dark() -> bool {
    match dword(PERSONALIZE_KEY, VALUE_NAME) {
        Some(light) => light == 0,
        None => true,
    }
}

/// 系统当前选定的强调色（`#rrggbb`，小写）。
///
/// 读不到时返回 `None`，调用方退回主题自带的那一抹强调色。注意 Windows 把
/// 这个值也用在「没挑过颜色」的情况（中性灰 `#808080`）—— 那就是用户的选择，
/// 照搬，不做「太灰了换一个」的判断。
pub fn accent_hex() -> Option<String> {
    dword(DWM_KEY, ACCENT_NAME).map(accent_from_dword)
}

/// `0xAABBGGRR` → `#rrggbb`。
fn accent_from_dword(value: u32) -> String {
    let r = (value & 0xFF) as u8;
    let g = ((value >> 8) & 0xFF) as u8;
    let b = ((value >> 16) & 0xFF) as u8;
    format!("#{r:02x}{g:02x}{b:02x}")
}

#[cfg(test)]
mod tests {
    use super::accent_from_dword;

    /// 注册表里是 ABGR：`0xff808080` 三通道相同看不出来，拿个有区分的值验。
    #[test]
    fn accent_dword_is_abgr() {
        assert_eq!(accent_from_dword(0xFF00_0000), "#000000");
        assert_eq!(accent_from_dword(0xFFFF_0000), "#0000ff");
        assert_eq!(accent_from_dword(0xFF00_FF00), "#00ff00");
        assert_eq!(accent_from_dword(0xFF00_00FF), "#ff0000");
        assert_eq!(accent_from_dword(0xFF80_8080), "#808080");
        assert_eq!(accent_from_dword(0xFF7C_9CFF), "#ff9c7c");
    }
}
