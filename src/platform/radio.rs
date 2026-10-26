//! WiFi / 蓝牙无线电的开关状态与切换。
//!
//! 面板左下角那两个双按钮（左半开关、右半展开）背后接的就是这里。
//! 走 WinRT `Windows.Devices.Radios`：`Radio::GetRadiosAsync()` 列出所有无线电，
//! 按 `RadioKind`（WiFi / Bluetooth）挑出目标，`State()` 读开关、`SetStateAsync()` 切换。
//!
//! 这个 API 是**异步**的（返回 `IAsyncOperation`），这边用 `.join()`（阻塞等待完成）
//! 包成同步函数；调用方自己再放后台线程，就不会卡住 UI。读写无线电有几百毫秒到
//! 一两秒的落地延迟，读实时状态才不会让按钮的「亮 / 灭」跟系统脱节。
//!
//! ## 一个必须避开的坑
//!
//! WinRT 的 `.join()` 阻塞等待时会在**当前线程**跑 COM 消息泵。如果这个线程是
//! gpui 的 `background_executor` 线程池（它们可能没按 MTA 初始化 COM，或者
//! WinRT 把调用 marshal 回主线程），消息泵就会触发 gpui 窗口的 `window_procedure`，
//! 重入借用 App 的 RefCell —— 日志刷屏 `RefCell already borrowed`，release 下
//! `panic=abort` 直接崩。所以这里**每个调用都先在本线程初始化 MTA 套间**，
//! 并且调用方要用真正的 `std::thread::spawn`（不是 gpui 线程池）来跑。

use windows::Devices::Radios::{Radio, RadioKind, RadioState};

/// 在当前线程初始化 MTA COM 套间（没初始化才做），返回的守卫反初始化。
/// WinRT 的 Radio 调用得在明确的套间里跑，否则 `.join()` 的消息泵会乱套。
fn init_mta() -> crate::audio::com::ComApartment {
    crate::audio::com::ComApartment::init_mta()
}

/// 找到指定种类（WiFi / 蓝牙）的无线电，没有就返回 None。
fn find_radio(kind: RadioKind) -> Option<Radio> {
    let radios = Radio::GetRadiosAsync().ok()?.join().ok()?;
    (0..radios.Size().ok()?).find_map(|i| {
        let radio = radios.GetAt(i).ok()?;
        (radio.Kind().ok()? == kind).then_some(radio)
    })
}

/// WiFi 无线电当前是不是开的。没有无线网卡按「关」处理。
pub fn wifi_enabled() -> bool {
    let _apartment = init_mta();
    matches!(
        find_radio(RadioKind::WiFi).and_then(|r| r.State().ok()),
        Some(RadioState::On)
    )
}

/// 蓝牙无线电当前是不是开的。没有无线电按「关」处理。
pub fn bluetooth_enabled() -> bool {
    let _apartment = init_mta();
    matches!(
        find_radio(RadioKind::Bluetooth).and_then(|r| r.State().ok()),
        Some(RadioState::On)
    )
}

/// 一次把 WiFi 和蓝牙的状态都读出来，返回 `(wifi, bluetooth)`。
///
/// 面板开着的时候要定期对一次系统状态，两次 `GetRadiosAsync` 不如一次枚举完 ——
/// 每次枚举都要跑一遍 COM + 异步等待，能省就省。
pub fn enabled_both() -> (bool, bool) {
    let _apartment = init_mta();
    let mut wifi = false;
    let mut bluetooth = false;

    let Some(radios) = Radio::GetRadiosAsync().ok().and_then(|op| op.join().ok()) else {
        return (false, false);
    };
    let Ok(size) = radios.Size() else {
        return (false, false);
    };
    for index in 0..size {
        let Ok(radio) = radios.GetAt(index) else {
            continue;
        };
        let (Ok(kind), Ok(state)) = (radio.Kind(), radio.State()) else {
            continue;
        };
        let on = state == RadioState::On;
        match kind {
            RadioKind::WiFi => wifi = on,
            RadioKind::Bluetooth => bluetooth = on,
            _ => {}
        }
    }
    (wifi, bluetooth)
}

/// 打开 / 关闭 WiFi 无线电（同步，阻塞）。返回最终状态是否「开着」。
pub fn set_wifi_sync(enabled: bool) -> bool {
    let _apartment = init_mta();
    let Some(radio) = find_radio(RadioKind::WiFi) else {
        return false;
    };
    let target = if enabled {
        RadioState::On
    } else {
        RadioState::Off
    };
    // SetStateAsync 返回 RadioAccessStatus；无论准没准许，都以回读的实际状态为准。
    let _ = radio.SetStateAsync(target).ok().and_then(|op| op.join().ok());
    radio.State().ok() == Some(RadioState::On)
}

/// 打开 / 关闭蓝牙无线电（同步，阻塞）。返回最终状态是否「开着」。
pub fn set_bluetooth_sync(enabled: bool) -> bool {
    let _apartment = init_mta();
    let Some(radio) = find_radio(RadioKind::Bluetooth) else {
        return false;
    };
    let target = if enabled {
        RadioState::On
    } else {
        RadioState::Off
    };
    let _ = radio.SetStateAsync(target).ok().and_then(|op| op.join().ok());
    radio.State().ok() == Some(RadioState::On)
}
