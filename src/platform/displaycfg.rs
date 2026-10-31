//! 当前活动显示路径：`QueryDisplayConfig(QDC_ONLY_ACTIVE_PATHS)`。
//!
//! 两件事非它不可：
//!
//! 1. **这块 GDI 源（`\\.\DISPLAY1`）此刻真正推的是哪台显示器。**
//!    `EnumDisplayDevices(device, 0)` 报的是「挂在这个源下面的显示器设备」，
//!    笔记本合盖、内置屏被关掉之后，那个源会继续挂着内置屏的设备节点、
//!    却把画面送到外接屏上 —— 于是外接屏顶着笔记本屏的名字。
//!    活动路径的目标（`GET_TARGET_NAME`）才是当下真实的落点。
//! 2. **哪条路径是内置屏。** 内置屏（eDP / LVDS）不接 MCCS，亮度得走
//!    `\\.\LCD` 的背光 IOCTL（见 `platform::backlight`）。
//!
//! 厂商驱动报的输出技术并不可靠：有的把外接 DisplayPort 一律报成
//! `DISPLAYPORT_EMBEDDED`（实测一台 RTX 5090 笔记本就这样），
//! 所以这里只提供「像内嵌接口」这个弱信号，最后是不是内置屏由
//! `display::scan` 结合「DDC/CI 读不读得出亮度」一起判。

use std::mem::size_of;

use windows::Win32::Devices::Display::{
    DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME,
    DISPLAYCONFIG_MODE_INFO, DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EMBEDDED,
    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INTERNAL, DISPLAYCONFIG_OUTPUT_TECHNOLOGY_LVDS,
    DISPLAYCONFIG_OUTPUT_TECHNOLOGY_UDI_EMBEDDED, DISPLAYCONFIG_PATH_INFO,
    DISPLAYCONFIG_SOURCE_DEVICE_NAME, DISPLAYCONFIG_TARGET_DEVICE_NAME,
    DisplayConfigGetDeviceInfo, GetDisplayConfigBufferSizes, QDC_ONLY_ACTIVE_PATHS,
    QueryDisplayConfig,
};

/// 一条活动路径：哪个源、推到哪台显示器、走的什么接口。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Path {
    /// GDI 设备名，形如 `\\.\DISPLAY1`（和 `MONITORINFOEXW.szDevice` 同一套写法）。
    pub source: Option<String>,
    /// 显示器自己报的友好名，例如 `32X1UE META`；有的驱动只给 `Generic PnP Monitor`。
    pub friendly: Option<String>,
    /// 显示器设备接口路径，`\\?\DISPLAY#SDX32AD#5&…&UID8450#{…}`，能读 EDID。
    pub device_path: Option<String>,
    /// `DISPLAYCONFIG_VIDEO_OUTPUT_TECHNOLOGY` 的取值。
    pub technology: i32,
    /// EDID 里的厂商号 / 产品号（`0x10AC` 这种，不是 ASCII 三字母码）。
    pub manufacture: u16,
    pub product: u16,
}

/// 全部活动路径。取不到就返回空表 —— 亮度那条链路上没有哪一步值得为它报错。
pub fn active_paths() -> Vec<Path> {
    let mut count = 0u32;
    let mut modes = 0u32;
    if unsafe { GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut count, &mut modes) }.0 != 0 {
        return Vec::new();
    }
    if count == 0 {
        return Vec::new();
    }
    let mut paths = vec![DISPLAYCONFIG_PATH_INFO::default(); count as usize];
    let mut mode_buffer = vec![DISPLAYCONFIG_MODE_INFO::default(); modes.max(1) as usize];
    let status = unsafe {
        QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut count,
            paths.as_mut_ptr(),
            &mut modes,
            mode_buffer.as_mut_ptr(),
            None,
        )
    };
    if status.0 != 0 {
        return Vec::new();
    }
    paths.truncate(count as usize);
    paths.iter().map(read_path).collect()
}

/// 某个 GDI 设备名对应的那条活动路径。
pub fn path_of(device: &str) -> Option<Path> {
    active_paths()
        .into_iter()
        .find(|path| {
            path.source
                .as_deref()
                .is_some_and(|source| source.eq_ignore_ascii_case(device))
        })
}

/// 这条路径的接口看起来是不是「内嵌」的（eDP / LVDS / UDI 内嵌 / INTERNAL）。
///
/// 取值按 `DISPLAYCONFIG_OUTPUT_TECHNOLOGY` 的官方编号来，**别手写数字**：
/// `DISPLAYPORT_EXTERNAL` 是 10、`DISPLAYPORT_EMBEDDED` 是 11，差一位就会把
/// 一块正常的外接屏认成笔记本面板，拖它的滑条反而去拧笔记本的背光。
///
/// **仍然只是候选**：厂商驱动的报法不统一（有的把内置屏报成 `INTERNAL`，
/// 有的报 `DISPLAYPORT_EMBEDDED`，还有的干脆两个都不报），所以接到
/// `\\.\LCD` 之前还得看 DDC/CI 那边读不读得出亮度。
pub fn is_embedded(technology: i32) -> bool {
    technology == DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INTERNAL.0
        || technology == DISPLAYCONFIG_OUTPUT_TECHNOLOGY_DISPLAYPORT_EMBEDDED.0
        || technology == DISPLAYCONFIG_OUTPUT_TECHNOLOGY_UDI_EMBEDDED.0
        || technology == DISPLAYCONFIG_OUTPUT_TECHNOLOGY_LVDS.0
}

/// 输出技术的可读名（诊断报告里用）。
///
/// 编号照 `DISPLAYCONFIG_OUTPUT_TECHNOLOGY`：`9` 起是 DP，**10 是外接、11 才是内嵌**。
pub fn technology_name(value: i32) -> &'static str {
    match value {
        -2 => "UNINITIALIZED",
        -1 => "OTHER",
        0 => "HD15",
        1 => "SVIDEO",
        2 => "COMPOSITE_VIDEO",
        3 => "COMPONENT_VIDEO",
        4 => "DVI",
        5 => "HDMI",
        6 => "LVDS",
        7 => "?",
        8 => "D_JPN",
        9 => "SDI",
        10 => "DISPLAYPORT_EXTERNAL",
        11 => "DISPLAYPORT_EMBEDDED",
        12 => "UDI_EXTERNAL",
        13 => "UDI_EMBEDDED",
        14 => "SDTVDONGLE",
        15 => "MIRACAST",
        16 => "INDIRECT_WIRED",
        17 => "INDIRECT_VIRTUAL",
        -0x8000_0000 => "INTERNAL",
        _ => "?",
    }
}

fn read_path(path: &DISPLAYCONFIG_PATH_INFO) -> Path {
    Path {
        source: source_name(path),
        ..target_name(path)
    }
}

fn target_name(path: &DISPLAYCONFIG_PATH_INFO) -> Path {
    let mut result = Path {
        technology: path.targetInfo.outputTechnology.0,
        ..Default::default()
    };
    let mut target = DISPLAYCONFIG_TARGET_DEVICE_NAME::default();
    target.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_TARGET_NAME;
    target.header.size = size_of::<DISPLAYCONFIG_TARGET_DEVICE_NAME>() as u32;
    target.header.adapterId = path.targetInfo.adapterId;
    target.header.id = path.targetInfo.id;
    if unsafe { DisplayConfigGetDeviceInfo(&mut target.header) } == 0 {
        result.friendly = wide_text(&target.monitorFriendlyDeviceName);
        result.device_path = wide_text(&target.monitorDevicePath);
        result.manufacture = target.edidManufactureId;
        result.product = target.edidProductCodeId;
    }
    result
}

/// `DISPLAYCONFIG_PATH_SOURCE_INFO` 里只有 id，GDI 设备名要再问一次。
fn source_name(path: &DISPLAYCONFIG_PATH_INFO) -> Option<String> {
    let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
    source.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
    source.header.size = size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32;
    source.header.adapterId = path.sourceInfo.adapterId;
    source.header.id = path.sourceInfo.id;
    if unsafe { DisplayConfigGetDeviceInfo(&mut source.header) } != 0 {
        return None;
    }
    wide_text(&source.viewGdiDeviceName)
}

fn wide_text(buffer: &[u16]) -> Option<String> {
    let len = buffer
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(buffer.len());
    (len > 0).then(|| String::from_utf16_lossy(&buffer[..len]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_technologies_are_recognised() {
        for technology in [
            6,            // LVDS
            11,           // DISPLAYPORT_EMBEDDED（eDP）
            13,           // UDI_EMBEDDED
            -0x8000_0000, // INTERNAL
        ] {
            assert!(is_embedded(technology), "{technology} 应该算内嵌接口");
        }
        // 10 是 DISPLAYPORT_EXTERNAL —— 差一位就会把外接屏当内置屏，
        // 拖它的滑条反而去拧笔记本的背光。
        for technology in [0, 4, 5, 10, 12, 14, 15, 16, 17] {
            assert!(!is_embedded(technology), "{technology} 不是内嵌接口");
        }
    }

    #[test]
    fn technology_names_cover_the_ones_we_print() {
        assert_eq!(technology_name(-0x8000_0000), "INTERNAL");
        assert_eq!(technology_name(10), "DISPLAYPORT_EXTERNAL");
        assert_eq!(technology_name(11), "DISPLAYPORT_EMBEDDED");
        assert_eq!(technology_name(1234), "?");
    }
}
