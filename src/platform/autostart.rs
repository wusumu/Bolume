//! 开机自启动：`HKCU\...\CurrentVersion\Run` 里的一条启动项。
//!
//! 走用户级注册表，不需要管理员权限；便携版移动位置后重新开关一次即可更新路径。
//!
//! **两处注册表都要看**，这是老版本踩过的坑：
//!
//! * `Run` 里的 `Bolume` 是「登记」——值里写死了当时那个 exe 的完整路径；
//! * `Explorer\StartupApproved\Run` 里的 `Bolume` 是**任务管理器「启动」页那个开关**。
//!   在那里关掉时 Windows 并不删 `Run` 的值，只在这个 12 字节的二进制值里
//!   把首字节改成奇数（启用是 `0x02` / `0x06`，禁用是 `0x03` / `0x07`）。
//!
//! 所以只看 `Run` 有没有值，会把「任务管理器里明确关掉的」当成开着；反过来，
//! 光往 `Run` 里写值也**不会**让一个被关过的启动项重新跑起来。判定和写回都得成对处理。

use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_BINARY, REG_OPTION_NON_VOLATILE, REG_SAM_FLAGS,
    REG_SZ, RRF_RT_REG_BINARY, RRF_RT_REG_SZ, RegCloseKey, RegCreateKeyExW, RegDeleteValueW,
    RegGetValueW, RegSetValueExW,
};
use windows::core::PCWSTR;

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const APPROVED_KEY: &str =
    "Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\StartupApproved\\Run";
const VALUE_NAME: &str = "Bolume";

/// 任务管理器里「启用」时写的 12 个字节（首字节偶数 = 启用）。
const APPROVED_ON: [u8; 12] = [0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 打开（必要时创建）一个要写的子键。
fn open_key(path: &str, access: REG_SAM_FLAGS) -> Option<HKEY> {
    let subkey = wide(path);
    let mut handle = HKEY::default();
    let status = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey.as_ptr()),
            None,
            None,
            REG_OPTION_NON_VOLATILE,
            access,
            None,
            &mut handle,
            None,
        )
    };
    status.is_ok().then_some(handle)
}

fn write_string(path: &str, name: &str, text: &str) -> bool {
    let Some(key) = open_key(path, KEY_SET_VALUE) else {
        return false;
    };
    let name = wide(name);
    let data: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes = unsafe {
        std::slice::from_raw_parts(data.as_ptr() as *const u8, data.len() * std::mem::size_of::<u16>())
    };
    let ok = unsafe { RegSetValueExW(key, PCWSTR(name.as_ptr()), None, REG_SZ, Some(bytes)).is_ok() };
    unsafe {
        let _ = RegCloseKey(key);
    }
    ok
}

fn write_binary(path: &str, name: &str, data: &[u8]) -> bool {
    let Some(key) = open_key(path, KEY_SET_VALUE) else {
        return false;
    };
    let name = wide(name);
    let ok = unsafe { RegSetValueExW(key, PCWSTR(name.as_ptr()), None, REG_BINARY, Some(data)).is_ok() };
    unsafe {
        let _ = RegCloseKey(key);
    }
    ok
}

fn remove_value(path: &str, name: &str) -> bool {
    let Some(key) = open_key(path, KEY_SET_VALUE) else {
        return false;
    };
    let name = wide(name);
    let ok = unsafe { RegDeleteValueW(key, PCWSTR(name.as_ptr())).is_ok() };
    unsafe {
        let _ = RegCloseKey(key);
    }
    ok
}

/// `Run` 里登记的那条命令（原样，含引号）；没登记返回 `None`。
pub fn registered_command() -> Option<String> {
    let subkey = wide(RUN_KEY);
    let name = wide(VALUE_NAME);
    let mut data = [0u16; 1024];
    let mut size = (data.len() * std::mem::size_of::<u16>()) as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey.as_ptr()),
            PCWSTR(name.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(data.as_mut_ptr() as *mut _),
            Some(&mut size),
        )
    };
    if status.is_err() {
        return None;
    }
    let len = data.iter().position(|&c| c == 0).unwrap_or(data.len());
    Some(String::from_utf16_lossy(&data[..len]))
}

/// 任务管理器那个「已禁用」标记的判定：首字节奇数 = 被关掉了。
///
/// 值不存在（`StartupApproved` 没有这一条）按**启用**算 —— 那是 Windows 的默认，
/// 用户没动过任务管理器时就是这种情况。
fn approved_from_bytes(bytes: &[u8]) -> bool {
    match bytes.first() {
        Some(first) => first & 1 == 0,
        None => true,
    }
}

/// 任务管理器「启动」页里 Bolume 是不是启用状态（没被关过）。
pub fn is_approved() -> bool {
    let subkey = wide(APPROVED_KEY);
    let name = wide(VALUE_NAME);
    let mut data = [0u8; 12];
    let mut size = data.len() as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey.as_ptr()),
            PCWSTR(name.as_ptr()),
            RRF_RT_REG_BINARY,
            None,
            Some(data.as_mut_ptr() as *mut _),
            Some(&mut size),
        )
    };
    if status.is_err() {
        return true;
    }
    approved_from_bytes(&data[..(size as usize).min(data.len())])
}

/// 当前是否**真的**会随登录启动：登记了、并且没在任务管理器里被关掉。
pub fn is_enabled() -> bool {
    registered_command().is_some() && is_approved()
}

/// 把 `Run` 里那条的路径改成本进程的路径（便携版挪了目录之后用）。
///
/// 只动路径，不碰任务管理器那个开关 —— 用户关掉的状态要留着。
pub fn ensure_path() -> bool {
    let Some(command) = registered_command() else {
        return false;
    };
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let wanted = format!("\"{}\"", exe.display());
    if command == wanted {
        return true;
    }
    write_string(RUN_KEY, VALUE_NAME, &wanted)
}

/// 开启/关闭开机自启动，返回是否成功。
///
/// 打开时**两个值一起写**：`Run` 里的路径 + 任务管理器那个「启用」标记。
/// 只写前者的话，之前被任务管理器关过的启动项依旧不会跑（这就是「开了没反应」）。
pub fn set_enabled(enabled: bool) -> bool {
    if enabled {
        let Ok(exe) = std::env::current_exe() else {
            return false;
        };
        if !write_string(RUN_KEY, VALUE_NAME, &format!("\"{}\"", exe.display())) {
            return false;
        }
        // 这一步失败也不回滚：只是「任务管理器里关过」的那种情况会继续不启动，
        // 下次打开设置页看到开关是关的，再点一次就好过整条设置被清掉。
        let _ = write_binary(APPROVED_KEY, VALUE_NAME, &APPROVED_ON);
        true
    } else {
        let _ = remove_value(RUN_KEY, VALUE_NAME);
        // 顺手清掉标记：`Run` 那条都没了，留着它只是一个孤儿项
        // （任务管理器列表里点不回来，老版本遗留过一条 `BolumeOsdGuard`）。
        let _ = remove_value(APPROVED_KEY, VALUE_NAME);
        registered_command().is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::approved_from_bytes;

    /// 任务管理器写的第一字节：偶数启用（02 / 06）、奇数禁用（03 / 07）。
    ///
    /// 只看偶奇、不看具体值 —— 用户重开过一次之后 Windows 会把 02 写成 06，
    /// 按等值判会漏掉这种「重新启用」。
    #[test]
    fn approved_byte_decides_run_at_login() {
        assert!(approved_from_bytes(&[0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]));
        assert!(approved_from_bytes(&[0x06, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]));
        assert!(!approved_from_bytes(&[0x03, 0, 0, 0, 0x85, 0x89, 0x60, 0x1B, 0xB0, 0x45, 0xDD, 0x01]));
        assert!(!approved_from_bytes(&[0x07, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]));
    }

    /// 没有这一条（用户没动过任务管理器）= 启用，不是禁用。
    #[test]
    fn missing_marker_means_enabled() {
        assert!(approved_from_bytes(&[]));
    }
}
