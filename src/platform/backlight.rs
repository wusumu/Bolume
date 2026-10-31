//! 笔记本内置屏的背光：`\\.\LCD` 的亮度 IOCTL。
//!
//! 内置屏（eDP）基本不认 DDC/CI —— MCCS 那条线是留给外接屏的，笔记本面板没有。
//! 系统那个「亮度」滑块走的是显卡驱动暴露的背光接口，就是这里的两条 IOCTL，
//! 所以从这儿改，系统滑块那边也跟着动，不会出现两边各说各的。
//!
//! 一台机器只有一块内置屏（笔记本都这样），台机连这个设备都打不开：
//! `open()` 返回 `None`，一切照旧走 DDC。

use std::mem::size_of;

use windows::{
    Win32::{
        Devices::Display::{
            DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME, DISPLAYCONFIG_MODE_INFO,
            DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INTERNAL, DISPLAYCONFIG_PATH_INFO,
            DISPLAYCONFIG_SOURCE_DEVICE_NAME, DisplayConfigGetDeviceInfo,
            GetDisplayConfigBufferSizes, IOCTL_VIDEO_QUERY_DISPLAY_BRIGHTNESS,
            IOCTL_VIDEO_QUERY_SUPPORTED_BRIGHTNESS, IOCTL_VIDEO_SET_DISPLAY_BRIGHTNESS,
            QDC_ONLY_ACTIVE_PATHS, QueryDisplayConfig,
        },
        Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE},
        Storage::FileSystem::{
            CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
        },
        System::IO::DeviceIoControl,
    },
    core::w,
};

/// `IOCTL_VIDEO_*_BRIGHTNESS` 进出的那个结构：当前 AC / DC 亮度。
///
/// 字段是「档位表里的值」，不是百分比 —— 档位表由
/// `IOCTL_VIDEO_QUERY_SUPPORTED_BRIGHTNESS` 给（见 `Backlight::levels`）。
#[repr(C)]
#[derive(Clone, Copy, Default)]
struct VideoBrightness {
    ac: u16,
    dc: u16,
}

/// 内置屏的背光通道。`levels` 是这块屏支持的档位（升序）。
pub struct Backlight {
    device: HANDLE,
    levels: Vec<u16>,
}

impl Backlight {
    /// 打开内置屏；台机 / 显卡驱动不提供背光接口时返回 `None`。
    pub fn open() -> Option<Self> {
        let device = unsafe {
            CreateFileW(
                w!("\\\\.\\LCD"),
                GENERIC_READ.0 | GENERIC_WRITE.0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
        }
        .ok()?;
        let mut backlight = Backlight {
            device,
            levels: Vec::new(),
        };
        match backlight.query_levels() {
            // 只有一两档等于没有（要么全亮要么全黑），别当成能调；而且档位下标
            // 会被当成量程用，两档时量程是 1，百分比换算那边会当成 0..100 处理。
            Some(levels) if levels.len() >= 3 => {
                backlight.levels = levels;
                Some(backlight)
            }
            // 走到这儿由 `Drop` 关掉句柄。
            _ => None,
        }
    }

    /// 档位表的长度减一 —— 当量程用，百分比换算和 DDC 那一套共用。
    pub fn max_index(&self) -> u16 {
        (self.levels.len() - 1) as u16
    }

    /// 读当前档位（下标）。驱动报的是值，按最近的档位认。
    pub fn read(&self) -> Option<u16> {
        let mut value = VideoBrightness::default();
        let mut returned = 0u32;
        unsafe {
            DeviceIoControl(
                self.device,
                IOCTL_VIDEO_QUERY_DISPLAY_BRIGHTNESS,
                None,
                0,
                Some((&mut value as *mut VideoBrightness).cast()),
                size_of::<VideoBrightness>() as u32,
                Some(&mut returned),
                None,
            )
        }
        .ok()?;
        nearest_index(&self.levels, value.ac)
    }

    /// 写档位（下标）。AC / DC 一起写：不然插上电源、拔掉电源会各用各的。
    pub fn write(&self, index: u16) -> bool {
        let Some(&level) = self.levels.get(index as usize) else {
            return false;
        };
        let value = VideoBrightness { ac: level, dc: level };
        let mut returned = 0u32;
        unsafe {
            DeviceIoControl(
                self.device,
                IOCTL_VIDEO_SET_DISPLAY_BRIGHTNESS,
                Some((&value as *const VideoBrightness).cast()),
                size_of::<VideoBrightness>() as u32,
                None,
                0,
                Some(&mut returned),
                None,
            )
        }
        .is_ok()
    }

    fn query_levels(&self) -> Option<Vec<u16>> {
        // 文档给的上限就是 256 个档位；真机一般报 101 个（0..100）。
        let mut buffer = [0u16; 256];
        let mut returned = 0u32;
        unsafe {
            DeviceIoControl(
                self.device,
                IOCTL_VIDEO_QUERY_SUPPORTED_BRIGHTNESS,
                None,
                0,
                Some(buffer.as_mut_ptr().cast()),
                size_of::<u16>() as u32 * buffer.len() as u32,
                Some(&mut returned),
                None,
            )
        }
        .ok()?;
        let count = (returned as usize / size_of::<u16>()).min(buffer.len());
        (count > 0).then(|| buffer[..count].to_vec())
    }
}

/// 驱动报回来的当前值是「档位表里的值」，按最近的档位认。
///
/// 不直接当索引用：真机上报的是值（比如档位是 `0,10,…,100`、当前 50），
/// 少数驱动报的才是下标；档位均匀时两种解释算出来一样。
fn nearest_index(levels: &[u16], level: u16) -> Option<u16> {
    levels
        .iter()
        .enumerate()
        .min_by_key(|(_, known)| (**known as i32 - level as i32).abs())
        .map(|(index, _)| index as u16)
}

impl Drop for Backlight {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.device) };
    }
}

/// 内置屏（笔记本面板）的 GDI 设备名，形如 `\\.\DISPLAY1`；没有内置屏时 `None`。
///
/// 用 `QueryDisplayConfig(QDC_ONLY_ACTIVE_PATHS)` 看**正在用**的那几条路径：
/// 合盖之后内置屏不是活动路径，这里就返回 `None` —— 省得给一块关着的屏
/// 也放一条亮度滑条。`outputTechnology == INTERNAL` 是「这台是内置屏」的
/// 唯一可靠信号（EDID、设备名、序号都认不出来）。
pub fn internal_device() -> Option<String> {
    let mut paths = 0u32;
    let mut modes = 0u32;
    if unsafe { GetDisplayConfigBufferSizes(QDC_ONLY_ACTIVE_PATHS, &mut paths, &mut modes) }.0 != 0 {
        return None;
    }
    if paths == 0 {
        return None;
    }
    let mut path_buffer = vec![DISPLAYCONFIG_PATH_INFO::default(); paths as usize];
    let mut mode_buffer = vec![DISPLAYCONFIG_MODE_INFO::default(); modes as usize];
    let status = unsafe {
        QueryDisplayConfig(
            QDC_ONLY_ACTIVE_PATHS,
            &mut paths,
            path_buffer.as_mut_ptr(),
            &mut modes,
            mode_buffer.as_mut_ptr(),
            None,
        )
    };
    if status.0 != 0 {
        return None;
    }
    path_buffer.truncate(paths as usize);
    for path in &path_buffer {
        if path.targetInfo.outputTechnology != DISPLAYCONFIG_OUTPUT_TECHNOLOGY_INTERNAL {
            continue;
        }
        // `DISPLAYCONFIG_PATH_SOURCE_INFO` 里只有 id，GDI 设备名要再问一次。
        let mut source = DISPLAYCONFIG_SOURCE_DEVICE_NAME::default();
        source.header.r#type = DISPLAYCONFIG_DEVICE_INFO_GET_SOURCE_NAME;
        source.header.size = size_of::<DISPLAYCONFIG_SOURCE_DEVICE_NAME>() as u32;
        source.header.adapterId = path.sourceInfo.adapterId;
        source.header.id = path.sourceInfo.id;
        if unsafe { DisplayConfigGetDeviceInfo(&mut source.header) } != 0 {
            continue;
        }
        if let Some(device) = wide_text(&source.viewGdiDeviceName) {
            return Some(device);
        }
    }
    None
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
    use super::{nearest_index, wide_text};

    /// 真机最常见的两张档位表：0..100 的 101 档、和每 10 一档的 11 档。
    #[test]
    fn level_values_map_back_to_their_index() {
        let fine: Vec<u16> = (0..=100).collect();
        assert_eq!(nearest_index(&fine, 0), Some(0));
        assert_eq!(nearest_index(&fine, 37), Some(37));
        assert_eq!(nearest_index(&fine, 100), Some(100));

        let coarse: Vec<u16> = (0..=10).map(|step| step * 10).collect();
        assert_eq!(nearest_index(&coarse, 50), Some(5));
        // 驱动报了个不在表里的值：认最近的档位，别整条滑条没反应。
        assert_eq!(nearest_index(&coarse, 46), Some(5));
        assert_eq!(nearest_index(&coarse, 44), Some(4));
        assert_eq!(nearest_index(&[], 50), None);
    }

    #[test]
    fn gdi_device_names_are_trimmed_at_the_terminator() {
        let mut buffer = [0u16; 32];
        for (index, unit) in r"\\.\DISPLAY1".encode_utf16().enumerate() {
            buffer[index] = unit;
        }
        assert_eq!(wide_text(&buffer).as_deref(), Some(r"\\.\DISPLAY1"));
        assert_eq!(wide_text(&[0u16; 8]), None);
    }

    /// 真机探针：`cargo test backlight -- --ignored --nocapture`。
    #[test]
    #[ignore]
    fn probe_internal_panel() {
        println!("内置屏：{:?}", super::internal_device());
        match super::Backlight::open() {
            Some(backlight) => println!(
                "背光：档位数 {}、当前档位 {:?}",
                backlight.levels.len(),
                backlight.read()
            ),
            None => println!("背光：没有可用的内置屏背光接口"),
        }
    }
}
