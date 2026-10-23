//! 开机自启动：写入 HKCU\Software\Microsoft\Windows\CurrentVersion\Run。
//!
//! 走用户级注册表，不需要管理员权限；便携版移动位置后重新开关一次即可更新路径。

use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_OPTION_NON_VOLATILE, REG_SZ, RegCloseKey,
    RegCreateKeyExW, RegDeleteValueW, RegSetValueExW,
};
use windows::core::PCWSTR;

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const VALUE_NAME: &str = "Bolume";

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn open_run_key() -> Option<HKEY> {
    let subkey = wide(RUN_KEY);
    let mut handle = HKEY::default();
    let status = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey.as_ptr()),
            None,
            None,
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE,
            None,
            &mut handle,
            None,
        )
    };
    status.is_ok().then_some(handle)
}

/// 开启/关闭开机自启动，返回是否成功。
pub fn set_enabled(enabled: bool) -> bool {
    let Some(key) = open_run_key() else {
        return false;
    };
    let name = wide(VALUE_NAME);

    let result = if enabled {
        let Ok(exe) = std::env::current_exe() else {
            unsafe {
                let _ = RegCloseKey(key);
            }
            return false;
        };
        let command = wide(&format!("\"{}\"", exe.display()));
        let bytes = unsafe {
            std::slice::from_raw_parts(
                command.as_ptr() as *const u8,
                command.len() * std::mem::size_of::<u16>(),
            )
        };
        unsafe { RegSetValueExW(key, PCWSTR(name.as_ptr()), None, REG_SZ, Some(bytes)).is_ok() }
    } else {
        unsafe { RegDeleteValueW(key, PCWSTR(name.as_ptr())).is_ok() }
    };

    unsafe {
        let _ = RegCloseKey(key);
    }
    result
}

/// 当前是否已登记自启动。
pub fn is_enabled() -> bool {
    let subkey = wide(RUN_KEY);
    let name = wide(VALUE_NAME);
    let mut data = [0u8; 1024];
    let mut size = data.len() as u32;
    let status = unsafe {
        windows::Win32::System::Registry::RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey.as_ptr()),
            PCWSTR(name.as_ptr()),
            windows::Win32::System::Registry::RRF_RT_REG_SZ,
            None,
            Some(data.as_mut_ptr() as *mut _),
            Some(&mut size),
        )
    };
    status.is_ok()
}
