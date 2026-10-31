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
        System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_BINARY, RegGetValueW},
        UI::HiDpi::{GetDpiForMonitor, MDT_EFFECTIVE_DPI},
    },
    core::{BOOL, PCWSTR},
};

/// `MONITORINFOF_PRIMARY`（wingdi.h）：这台是主显示器。
const MONITORINFOF_PRIMARY: u32 = 0x0000_0001;

/// 一台显示器。
#[derive(Clone, Debug, PartialEq)]
pub struct MonitorInfo {
    /// 友好名，例如 `XG279Q`；系统只报通用名时退回它，再不行才用 `\\.\DISPLAY1`。
    pub name: String,
    /// 真型号（驱动报的具体型号或 EDID 里的 `0xFC`）；只有通用名时为 `None`。
    ///
    /// 和 `name` 分开存：`name` 要保证「一定有东西显示」，而亮度面板只该拿
    /// 真正的型号去替换驱动给的「Generic Monitor」。
    pub model: Option<String>,
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
    /// GDI 设备名，形如 `\\.\DISPLAY1`（认「哪台是内置屏」要用它）。
    pub device: String,
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
    let mut list = collect_all();
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

/// 每台屏的完整信息（含 `HMONITOR` 和型号），按 `EnumDisplayMonitors` 的
/// 原始顺序（**不**排序）。
///
/// 亮度那边（`display::scan`）拿这份去要 DDC 通道：**只用句柄对号**，不靠下标。
/// 笔记本合盖、显示器走转接器、虚拟屏这些情况下，`EnumDisplayMonitors` 里的
/// 屏幕台数和 `GetPhysicalMonitorsFromHMONITOR` 给的物理显示器台数对不上，
/// 按下标取名字会把隔壁屏的型号贴过来（合盖后外接屏显示成本机的名字就是这么来的）。
pub fn screens() -> Vec<MonitorInfo> {
    collect_all()
}

/// 诊断用：把「显示器名」这一链路上的每一层原始值都摊开写出来。
///
/// 用户报「名字不对 / 认不出哪台是笔记本屏」时，我们拿不到他的机器，只能靠这份
/// 报告对号：系统枚举到几块屏、驱动报的 `DeviceString` 是什么、EDID 里的型号是
/// 什么、注册表键对不对得上。只在 `--diag-displays` 里用。
pub fn diag_report() -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    // 适配器层：`\\.\DISPLAY1` 这种设备名是这一层给的。
    let _ = writeln!(out, "── 适配器层（EnumDisplayDevices(NULL, n)）");
    for index in 0..16u32 {
        let mut display = DISPLAY_DEVICEW {
            cb: size_of::<DISPLAY_DEVICEW>() as u32,
            ..Default::default()
        };
        let ok = unsafe { EnumDisplayDevicesW(PCWSTR::null(), index, &mut display, 0) };
        if !ok.as_bool() {
            break;
        }
        let _ = writeln!(
            out,
            "  [{index}] device={:?} string={:?} id={:?} flags=0x{:X}",
            wide_to_string(&display.DeviceName),
            wide_to_string(&display.DeviceString),
            wide_to_string(&display.DeviceID),
            display.StateFlags.0
        );
    }

    let list = screens();
    let _ = writeln!(out, "\n── EnumDisplayMonitors：{} 块屏", list.len());
    for (index, info) in list.iter().enumerate() {
        let _ = writeln!(
            out,
            "  [{index}] handle=0x{:X} device={:?} primary={}",
            info.handle, info.device, info.is_primary
        );
        let _ = writeln!(
            out,
            "       bounds={:?} work={:?}",
            info.bounds, info.work
        );
        let _ = writeln!(
            out,
            "       最终名字={:?} 型号={:?}",
            info.name, info.model
        );
        match enumerate_monitor(&info.device, 0) {
            Some(display) => {
                let _ = writeln!(
                    out,
                    "       EnumDisplayDevices(0): string={:?} id={:?}",
                    wide_to_string(&display.DeviceString),
                    wide_to_string(&display.DeviceID)
                );
            }
            None => {
                let _ = writeln!(out, "       EnumDisplayDevices(0): 失败");
            }
        }
        let Some(iface) = enumerate_monitor(&info.device, EDD_GET_DEVICE_INTERFACE_NAME) else {
            let _ = writeln!(out, "       设备接口名: 取不到");
            continue;
        };
        let iface_id = wide_to_string(&iface.DeviceID);
        let _ = writeln!(out, "       接口名: {iface_id:?}");
        let _ = writeln!(
            out,
            "       注册表键: {:?}",
            enum_key(&iface_id).unwrap_or_default()
        );
        match edid_bytes(&iface_id) {
            Some(edid) => {
                let head: Vec<String> = edid
                    .iter()
                    .take(16)
                    .map(|byte| format!("{byte:02X}"))
                    .collect();
                let _ = writeln!(
                    out,
                    "       EDID: {} 字节，头 {:?}，型号={:?}",
                    edid.len(),
                    head.join(" "),
                    monitor_name_descriptor(&edid)
                );
            }
            None => {
                let _ = writeln!(out, "       EDID: 读不到（注册表里没有）");
            }
        }
    }

    out
}

fn collect_all() -> Vec<MonitorInfo> {
    let mut list: Vec<MonitorInfo> = Vec::new();
    unsafe {
        let _ = EnumDisplayMonitors(
            None,
            None,
            Some(collect),
            LPARAM(&mut list as *mut Vec<MonitorInfo> as isize),
        );
    }
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
    let (model, reported) = device_names(&device);
    list.push(MonitorInfo {
        name: model.clone().or(reported).unwrap_or_else(|| device.clone()),
        model,
        device,
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

/// `EnumDisplayDevicesW` 的 `EDD_GET_DEVICE_INTERFACE_NAME`：把这层的 `DeviceID`
/// 换成设备接口路径（`\\?\DISPLAY#…`）。只有这种写法能对到注册表里的 EDID ——
/// 默认返回的 `MONITOR\…\{类 GUID}\0001` 在 `Enum` 底下并不存在。
const EDD_GET_DEVICE_INTERFACE_NAME: u32 = 0x0000_0001;

/// 枚举某台 GDI 设备的显示器那一层（`flags` 就是 `EnumDisplayDevicesW` 的第四个参数）。
fn enumerate_monitor(device: &str, flags: u32) -> Option<DISPLAY_DEVICEW> {
    let wide: Vec<u16> = device.encode_utf16().chain(std::iter::once(0)).collect();
    let mut display = DISPLAY_DEVICEW {
        cb: size_of::<DISPLAY_DEVICEW>() as u32,
        ..Default::default()
    };
    let ok = unsafe { EnumDisplayDevicesW(PCWSTR(wide.as_ptr()), 0, &mut display, flags) };
    ok.as_bool().then_some(display)
}

/// 一台屏的两档名字：`(真型号, 驱动报的名字)`。
///
/// 驱动给的 `DeviceString` 经常只是通用名（`Generic PnP Monitor` /
/// `Generic Monitor`），真正型号在 EDID 的 `0xFC` descriptor 里 ——
/// 设备管理器那串「Generic Monitor (型号)」的括号内容就是从这儿拿的。
/// 所以只有通用名才去读 EDID；驱动认得出型号就直接当型号用。
fn device_names(device: &str) -> (Option<String>, Option<String>) {
    if device.is_empty() {
        return (None, None);
    }
    let Some(plain) = enumerate_monitor(device, 0) else {
        return (None, None);
    };
    let reported = wide_to_string(&plain.DeviceString).trim().to_string();
    let reported = (!reported.is_empty()).then_some(reported);
    if let Some(name) = reported.as_ref().filter(|name| !is_generic_name(name)) {
        return (Some(name.clone()), reported);
    }
    let interface = enumerate_monitor(device, EDD_GET_DEVICE_INTERFACE_NAME)
        .map(|display| wide_to_string(&display.DeviceID))
        .unwrap_or_default();
    (edid_monitor_name(&interface), reported)
}

/// 驱动没认出具体型号时给的名字。`Generic Monitor (XXX)` 这种也算 ——
/// 括号里那截型号是设备管理器自己拼的，`DeviceString` 里常常只有前面一半。
pub fn is_generic_name(name: &str) -> bool {
    name.is_empty() || name.to_lowercase().starts_with("generic")
}

/// EDID 里厂商写的型号名（`0xFC` descriptor），例如 `XG279Q`。
///
/// EDID 挂在显示器设备实例的注册表下面：`Enum\<设备 ID>\Device Parameters\EDID`。
/// 设备 ID 有两种写法，`MONITOR\AUS27B1\{类 GUID}\0001` 直接就是键路径；
/// 设备接口写法 `\\?\DISPLAY#AUS27B1#5&…#{接口 GUID}` 要把 `#` 换成 `\`、
/// 去掉末尾的接口 GUID。读不到就 `None` —— 这不是关键路径，不值得为它报错。
fn edid_monitor_name(device_id: &str) -> Option<String> {
    monitor_name_descriptor(&edid_bytes(device_id)?)
}

/// 注册表里那串 EDID 原始字节（`Enum\<设备>\Device Parameters\EDID`）。
fn edid_bytes(device_id: &str) -> Option<Vec<u8>> {
    let key = format!(
        "SYSTEM\\CurrentControlSet\\Enum\\{}\\Device Parameters",
        enum_key(device_id)?
    );
    let key: Vec<u16> = key.encode_utf16().chain(std::iter::once(0)).collect();
    let value: Vec<u16> = "EDID".encode_utf16().chain(std::iter::once(0)).collect();
    let mut size = 0u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(key.as_ptr()),
            PCWSTR(value.as_ptr()),
            RRF_RT_REG_BINARY,
            None,
            None,
            Some(&mut size),
        )
    };
    if status.is_err() || size == 0 || size > 4096 {
        return None;
    }
    let mut buffer = vec![0u8; size as usize];
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            PCWSTR(key.as_ptr()),
            PCWSTR(value.as_ptr()),
            RRF_RT_REG_BINARY,
            None,
            Some(buffer.as_mut_ptr().cast()),
            Some(&mut size),
        )
    };
    if status.is_err() {
        return None;
    }
    buffer.truncate(size as usize);
    Some(buffer)
}

/// 把设备 ID 变成 `Enum\` 底下的子键路径。
fn enum_key(device_id: &str) -> Option<String> {
    let trimmed = device_id.trim();
    let id = trimmed.strip_prefix(r"\\?\").unwrap_or(trimmed);
    if id.is_empty() {
        return None;
    }
    // 设备接口写法：`DISPLAY#AUS27B1#5&…#{接口 GUID}`。
    if let Some(cut) = id.rfind("#{") {
        let body = &id[..cut];
        if body.contains('#') {
            return Some(body.replace('#', "\\"));
        }
    }
    Some(id.to_string())
}

/// 从 EDID 主块里找型号名。
///
/// 头 8 字节是固定签名，之后第 54 字节起是四个 18 字节的 descriptor；
/// 头五个字节 `00 00 00 FC 00` 的那个，后面 13 字节是型号（`0x0A` 收尾、
/// 补空格）。CEA 扩展块里没有型号 descriptor，所以只看主块。
fn monitor_name_descriptor(edid: &[u8]) -> Option<String> {
    const HEADER: [u8; 8] = [0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00];
    if edid.len() < 128 || edid[..8] != HEADER {
        return None;
    }
    for slot in 0..4 {
        let start = 54 + slot * 18;
        let descriptor = &edid[start..start + 18];
        if descriptor[..5] != [0x00, 0x00, 0x00, 0xFC, 0x00] {
            continue;
        }
        let text: Vec<u8> = descriptor[5..]
            .iter()
            .copied()
            .take_while(|byte| *byte != 0x0A && *byte != 0x00)
            .collect();
        let name = String::from_utf8_lossy(&text).trim().to_string();
        if !name.is_empty() {
            return Some(name);
        }
    }
    None
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
        // 顺带把「驱动给的名字 / 设备 ID / EDID 型号」三样摊开，方便核对回退链路。
        let mut index = 0;
        loop {
            let mut adapter = DISPLAY_DEVICEW {
                cb: size_of::<DISPLAY_DEVICEW>() as u32,
                ..Default::default()
            };
            let ok = unsafe { EnumDisplayDevicesW(PCWSTR::null(), index, &mut adapter, 0) };
            if !ok.as_bool() {
                break;
            }
            index += 1;
            let device = wide_to_string(&adapter.DeviceName);
            let wide_device: Vec<u16> = device.encode_utf16().chain(std::iter::once(0)).collect();
            let mut monitor = DISPLAY_DEVICEW {
                cb: size_of::<DISPLAY_DEVICEW>() as u32,
                ..Default::default()
            };
            let ok = unsafe { EnumDisplayDevicesW(PCWSTR(wide_device.as_ptr()), 0, &mut monitor, 0) };
            if !ok.as_bool() {
                continue;
            }
            let reported = wide_to_string(&monitor.DeviceString);
            let id = wide_to_string(&monitor.DeviceID);
            let mut interface = DISPLAY_DEVICEW {
                cb: size_of::<DISPLAY_DEVICEW>() as u32,
                ..Default::default()
            };
            let iface_ok =
                unsafe { EnumDisplayDevicesW(PCWSTR(wide_device.as_ptr()), 0, &mut interface, 1) };
            let iface_id = if iface_ok.as_bool() {
                wide_to_string(&interface.DeviceID)
            } else {
                String::new()
            };
            println!(
                "{device}: reported={reported:?} id={id:?} iface_id={iface_id:?} enum_key={:?} edid_name={:?} names={:?}",
                enum_key(&iface_id),
                edid_monitor_name(&iface_id),
                device_names(&device)
            );
        }
    }

    #[test]
    fn edid_model_name_is_read_from_the_name_descriptor() {
        let mut edid = vec![0u8; 128];
        edid[..8].copy_from_slice(&[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]);
        // 第一个 descriptor 是时序，第二个才是型号名。
        edid[54 + 18..54 + 23].copy_from_slice(&[0x00, 0x00, 0x00, 0xFC, 0x00]);
        edid[54 + 18 + 5..54 + 18 + 5 + 8].copy_from_slice(b"XG279Q\n ");
        assert_eq!(monitor_name_descriptor(&edid).as_deref(), Some("XG279Q"));
    }

    #[test]
    fn edid_without_a_name_descriptor_gives_nothing() {
        let mut edid = vec![0u8; 128];
        edid[..8].copy_from_slice(&[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]);
        assert!(monitor_name_descriptor(&edid).is_none());
        assert!(monitor_name_descriptor(&[]).is_none());
        assert!(monitor_name_descriptor(b"not an edid").is_none());
    }

    #[test]
    fn generic_names_are_recognised() {
        for name in ["", "Generic PnP Monitor", "Generic Monitor", "GENERIC MONITOR (AOC)"] {
            assert!(is_generic_name(name), "{name:?} 应该算通用名");
        }
        for name in ["XG279Q", "ASUS ROG XG279Q(DisplayPort)", "DELL U2412M"] {
            assert!(!is_generic_name(name), "{name:?} 是型号，不该被换掉");
        }
    }

    #[test]
    fn device_ids_map_to_registry_keys() {
        assert_eq!(
            enum_key(r"MONITOR\AUS27B1\{4d36e96e-e325-11ce-bfc1-08002be10318}\0001").as_deref(),
            Some(r"MONITOR\AUS27B1\{4d36e96e-e325-11ce-bfc1-08002be10318}\0001")
        );
        assert_eq!(
            enum_key(r"\\?\DISPLAY#AUS27B1#5&2f0d1a4&0&UID4352#{e6f07b5f-ee97-4a90-b076-33f57bf4eaa7}")
                .as_deref(),
            Some(r"DISPLAY\AUS27B1\5&2f0d1a4&0&UID4352")
        );
        assert_eq!(enum_key("   "), None);
    }
}
