//! 显示器清单：友好名（`EnumDisplayDevices` 的 DeviceString）、工作区、逻辑坐标。
//!
//! gpui 只给每个屏幕的矩形和 uuid，拿不到「XG279Q」这种名字，设置页里就只能
//! 显示「1 · 1707×960」—— 两块分辨率一样的屏就完全分不出来。
//! 这里自己问系统：`EnumDisplayMonitors` 拿到显示器句柄和工作区，
//! 再用它的设备名（`\\.\DISPLAY1`）去 `EnumDisplayDevices` 查友好名。
//!
//! 坐标统一换算成**逻辑像素**（物理像素 ÷ 该屏 DPI 缩放），和 gpui 的
//! `visible_bounds()` 是一个坐标系，OSD 定位直接用这里的 `work`。

use std::mem::size_of;

use windows::{
    Win32::{
        Foundation::{LPARAM, RECT},
        Graphics::Gdi::{
            DISPLAY_DEVICEW, EnumDisplayDevicesW, EnumDisplayMonitors, GetMonitorInfoW, HDC,
            HMONITOR, MONITORINFO, MONITORINFOEXW,
        },
        UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI},
    },
    core::{BOOL, PCWSTR},
};

/// `MONITORINFOF_PRIMARY`（wingdi.h）：这台是主显示器。
const MONITORINFOF_PRIMARY: u32 = 0x0000_0001;

/// 一台显示器。
#[derive(Clone, Debug, PartialEq)]
pub struct MonitorInfo {
    /// 友好名，例如 `XG279Q`；系统没给名字时退回 `\\.\DISPLAY1`。
    pub name: String,
    /// 是不是主显示器。
    pub is_primary: bool,
    /// 工作区（逻辑像素）：x、y、宽、高。已经排除任务栏。
    pub work: (f32, f32, f32, f32),
    /// 整块屏幕（逻辑像素）：x、y、宽、高。含任务栏占的那一条。
    ///
    /// 「从任务栏后面滑出来」的动画要拿它算起点：窗口顶边贴到屏幕底，才真的是
    /// 整块卡片都在屏幕外，而不是卡在工作区底边上凭空冒出来。
    pub bounds: (f32, f32, f32, f32),
    /// 系统的 `HMONITOR`（拿来和 `MonitorFromPoint` 的结果比对）。
    pub handle: isize,
}

/// 按 OSD 的显示器序号（0 = 主显示器，n = 第 n 台）取那块屏的句柄。
///
/// 和 `osd::target_area` 一套编号：序号写大了（换了台少一台的机器）就落到主屏，
/// 别让「拖条调亮度」因为认不出屏幕就整条没反应。
pub fn handle_of(index: u32) -> Option<isize> {
    let list = monitors();
    let chosen = if index == 0 {
        list.iter()
            .find(|info| info.is_primary)
            .or_else(|| list.first())
    } else {
        list.get(index as usize - 1)
            .or_else(|| list.iter().find(|info| info.is_primary))
            .or_else(|| list.first())
    };
    chosen.map(|info| info.handle)
}

/// 鼠标指针此刻在哪台显示器上（`monitors()` 里的下标 + 1，取不到返回 `None`）。
///
/// 用 `MonitorFromPoint(MONITOR_DEFAULTTONEAREST)` 而不是自己拿光标坐标去比矩形：
/// 各屏 DPI 不同，坐标已经是逻辑像素，自己比会算错边界；系统比的是物理坐标，最准。
pub fn index_at_cursor() -> Option<u32> {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Gdi::{MONITOR_DEFAULTTONEAREST, MonitorFromPoint};

    let mut cursor = POINT::default();
    unsafe { windows::Win32::UI::WindowsAndMessaging::GetCursorPos(&mut cursor) }.ok()?;
    let monitor = unsafe { MonitorFromPoint(cursor, MONITOR_DEFAULTTONEAREST) };
    if monitor.is_invalid() {
        return None;
    }
    monitors()
        .iter()
        .position(|info| info.handle == monitor.0 as isize)
        .map(|index| index as u32 + 1)
}

/// 全部显示器，按左上角从先到后排序。
///
/// 设置页里「显示器 1/2/…」和 OSD 的 `monitor` 序号都用这个顺序。
pub fn monitors() -> Vec<MonitorInfo> {
    let mut list: Vec<MonitorInfo> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(collect),
            LPARAM(&mut list as *mut Vec<MonitorInfo> as isize),
        );
    }
    list.sort_by(|left, right| {
        left.work
            .0
            .partial_cmp(&right.work.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then(
                left.work
                    .1
                    .partial_cmp(&right.work.1)
                    .unwrap_or(std::cmp::Ordering::Equal),
            )
    });
    list
}

unsafe extern "system" fn collect(
    monitor: HMONITOR,
    _dc: HDC,
    _clip: *mut RECT,
    data: LPARAM,
) -> BOOL {
    let list = unsafe { &mut *(data.0 as *mut Vec<MonitorInfo>) };
    let mut info = MONITORINFOEXW {
        monitorInfo: MONITORINFO {
            cbSize: size_of::<MONITORINFOEXW>() as u32,
            ..Default::default()
        },
        ..Default::default()
    };
    let ok = unsafe {
        GetMonitorInfoW(
            monitor,
            (&mut info as *mut MONITORINFOEXW).cast::<MONITORINFO>(),
        )
    };
    if !ok.as_bool() {
        return BOOL(1);
    }

    // 每台屏的缩放可能不同（笔记本 + 外接很常见），按各自的 DPI 换算。
    let mut dpi_x = 96u32;
    let mut dpi_y = 96u32;
    let _ = unsafe { GetDpiForMonitor(monitor, MDT_EFFECTIVE_DPI, &mut dpi_x, &mut dpi_y) };
    let scale = ((dpi_x.max(48) as f32) / 96.0).min(8.0);

    let device = wide_to_string(&info.szDevice);
    let work = info.monitorInfo.rcWork;
    let screen = info.monitorInfo.rcMonitor;
    list.push(MonitorInfo {
        name: friendly_name(&device).unwrap_or(device),
        is_primary: info.monitorInfo.dwFlags & MONITORINFOF_PRIMARY != 0,
        work: (
            work.left as f32 / scale,
            work.top as f32 / scale,
            (work.right - work.left) as f32 / scale,
            (work.bottom - work.top) as f32 / scale,
        ),
        bounds: (
            screen.left as f32 / scale,
            screen.top as f32 / scale,
            (screen.right - screen.left) as f32 / scale,
            (screen.bottom - screen.top) as f32 / scale,
        ),
        handle: monitor.0 as isize,
    });
    BOOL(1)
}

/// 显示器的友好名（`EnumDisplayDevices` 的第二档：显示器本身那一层）。
fn friendly_name(device: &str) -> Option<String> {
    if device.is_empty() {
        return None;
    }
    let wide: Vec<u16> = device.encode_utf16().chain(std::iter::once(0)).collect();
    let mut display = DISPLAY_DEVICEW {
        cb: size_of::<DISPLAY_DEVICEW>() as u32,
        ..Default::default()
    };
    let ok = unsafe { EnumDisplayDevicesW(PCWSTR(wide.as_ptr()), 0, &mut display, 0) };
    if !ok.as_bool() {
        return None;
    }
    let name = wide_to_string(&display.DeviceString).trim().to_string();
    // 没装显示器驱动时会回「Generic PnP Monitor」，比设备名还难认，宁可不要。
    let generic = name.is_empty() || name.eq_ignore_ascii_case("Generic PnP Monitor");
    (!generic).then_some(name)
}

fn wide_to_string(buffer: &[u16]) -> String {
    let end = buffer
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(buffer.len());
    String::from_utf16_lossy(&buffer[..end])
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 真机探针：`cargo test monitors -- --ignored --nocapture` 能看到每台屏的名字与工作区。
    /// 默认忽略：CI / 无显示器的环境里跑不了。
    #[test]
    #[ignore]
    fn probe_monitors() {
        let list = monitors();
        for info in &list {
            println!("{info:?}");
        }
        assert!(!list.is_empty(), "至少要枚举到一台显示器");
        assert!(
            list.iter().any(|info| info.is_primary),
            "必须有一台是主显示器"
        );
    }
}
