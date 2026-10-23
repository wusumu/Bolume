//! 旧版「注册表级系统 OSD 守卫」的清理。
//!
//! v0.42 之前的 `block_system_osd` 是这么实现的：往 `HKCU\...\Run` 写一条自启动，
//! 拉起 `Bolume.exe --osd-guard` 的无界面守卫，由它在 shell 之前吞掉音量键。
//! 那套东西在真机上不好使 —— 守卫和主程序抢键、主程序退出后守卫还赖着不吞不放，
//! 所以整块移除了：现在这个开关只在 **Bolume 运行时** 由主程序自己的低级钩子生效。
//!
//! 保留下来的只有这里：把旧版本留在注册表里的自启动项和可能还在跑的守卫清掉。
//! 每次启动跑一遍，幂等 —— 值不存在、事件不存在都不报错。

use windows::{
    Win32::{
        Foundation::CloseHandle,
        System::{
            Registry::{HKEY_CURRENT_USER, RegDeleteKeyValueW},
            Threading::{EVENT_MODIFY_STATE, OpenEventW, SetEvent},
        },
    },
    core::PCWSTR,
};

/// 旧版的自启动项值名（`HKCU\...\Run` 下）。
const RUN_VALUE: &str = "BolumeOsdGuard";
const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
/// 旧版守卫的退出事件名：置位一次它就自己退场。
const STOP_EVENT: &str = "Bolume.OsdGuard.Stop";

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 清掉旧版残留：先叫停守卫，再删自启动项。
///
/// 顺序不能反 —— 守卫的看门狗也在查自启动项在不在，先删值它也会退，
/// 但先发事件能保证它这一轮立刻走，不用等看门狗那 0.5 秒。
pub fn sweep() {
    let event = wide(STOP_EVENT);
    unsafe {
        if let Ok(handle) = OpenEventW(EVENT_MODIFY_STATE, false, PCWSTR(event.as_ptr())) {
            let _ = SetEvent(handle);
            let _ = CloseHandle(handle);
        }
    }

    let subkey = wide(RUN_KEY);
    let value = wide(RUN_VALUE);
    unsafe {
        // 值不存在时返回 ERROR_FILE_NOT_FOUND，正是常态，忽略即可。
        let _ = RegDeleteKeyValueW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey.as_ptr()),
            PCWSTR(value.as_ptr()),
        );
    }
}
