//! 笔记本内置屏的背光：`\\.\LCD` 的亮度 IOCTL。
//!
//! 内置屏（eDP）基本不认 DDC/CI —— MCCS 那条线是留给外接屏的，笔记本面板没有。
//! 系统那个「亮度」滑块走的是显卡驱动暴露的背光接口，就是这里的两条 IOCTL，
//! 所以从这儿改，系统滑块那边也跟着动，不会出现两边各说各的。
//!
//! 一台机器只有一块内置屏（笔记本都这样），台机连这个设备都打不开：
//! `open()` 返回 `None`，一切照旧走 DDC。

use std::mem::size_of;

use crate::platform::displaycfg;

use windows::{
    Win32::{
        Devices::Display::{
            IOCTL_VIDEO_QUERY_DISPLAY_BRIGHTNESS, IOCTL_VIDEO_QUERY_SUPPORTED_BRIGHTNESS,
            IOCTL_VIDEO_SET_DISPLAY_BRIGHTNESS,
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

/// 活动路径里「像内嵌接口」的那几条的 GDI 设备名（`\\.\DISPLAY1` 这种）。
///
/// 只给**候选**，不是结论：厂商驱动会把外接 DisplayPort 也报成
/// `DISPLAYPORT_EMBEDDED`（实测一台 RTX 5090 笔记本就这样），单看输出技术
/// 会把外接屏当成内置屏、去拧笔记本的背光。所以 `display::scan` 还要看这块屏
/// 的 DDC/CI 读不读得出亮度 —— 读得出就是普通外接屏，读不出才轮到背光。
///
/// 合盖之后内置屏不是活动路径，这里就没有它 —— 不会给一块关着的屏留滑条。
pub fn embedded_sources() -> Vec<String> {
    displaycfg::active_paths()
        .into_iter()
        .filter(|path| displaycfg::is_embedded(path.technology))
        .filter_map(|path| path.source)
        .collect()
}

/// 诊断用：把「这条屏是哪台 / 背光能不能开」的原始信息写出来。
///
/// 笔记本用户报「内置屏调不了亮度」时，这份能分辨三种情况：系统没认出内置屏、
/// 认出来了但 `\\.\LCD` 打不开（少数驱动只给 WMI）、打得开但档位不够。
pub fn diag_report() -> String {
    use std::fmt::Write as _;

    let mut out = String::new();

    let _ = writeln!(out, "── QueryDisplayConfig(QDC_ONLY_ACTIVE_PATHS)");
    let paths = displaycfg::active_paths();
    let _ = writeln!(out, "   活动路径 {} 条", paths.len());
    for (index, path) in paths.iter().enumerate() {
        let _ = writeln!(
            out,
            "   [{index}] 源={:?} 输出技术={} ({}{}) 目标友好名={:?}",
            path.source,
            path.technology,
            displaycfg::technology_name(path.technology),
            if displaycfg::is_embedded(path.technology) {
                " ← 内嵌候选"
            } else {
                ""
            },
            path.friendly
        );
        let _ = writeln!(
            out,
            "       EDID 厂商=0x{:04X} 产品=0x{:04X} 设备路径={:?}",
            path.manufacture, path.product, path.device_path
        );
    }

    let _ = writeln!(out, "\n── 内置屏候选（内嵌接口 ∩ 亮度走背光）");
    let _ = writeln!(out, "   embedded_sources() = {:?}", embedded_sources());

    let _ = writeln!(out, "\n── 背光设备 \\\\.\\LCD");
    match unsafe {
        CreateFileW(
            w!("\\\\.\\LCD"),
            GENERIC_READ.0 | GENERIC_WRITE.0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    } {
        Ok(device) => {
            let _ = writeln!(out, "   打开成功");
            let mut backlight = Backlight {
                device,
                levels: Vec::new(),
            };
            match backlight.query_levels() {
                Some(levels) => {
                    let _ = writeln!(
                        out,
                        "   档位 {} 个：{:?}",
                        levels.len(),
                        &levels[..levels.len().min(24)]
                    );
                    backlight.levels = levels;
                    match backlight.read() {
                        Some(index) => {
                            let _ = writeln!(
                                out,
                                "   当前档位下标={index}（值 {:?}），量程上限={}",
                                backlight.levels.get(index as usize),
                                backlight.levels.len().saturating_sub(1)
                            );
                        }
                        None => {
                            let _ = writeln!(out, "   读当前档位失败（驱动只给写不给读？）");
                        }
                    }
                }
                None => {
                    let _ = writeln!(out, "   打得开，但读不到档位表");
                }
            }
            // 句柄交给 `Drop` 关。
        }
        Err(error) => {
            let _ = writeln!(
                out,
                "   打开失败：{}（0x{:08X}）",
                error.message(),
                error.code().0
            );
        }
    }
    out
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
        println!("内嵌候选：{:?}", super::embedded_sources());
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
