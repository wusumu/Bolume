//! 系统音量图标：直接从 `SndVolSSO.dll` 取 EarTrumpet 用的那批图标资源。
//!
//! 参照 EarTrumpet 的 `Interop/SndVolSSO.cs` + `IconHelper.LoadIconResource`：
//! 同一个 dll、同一组资源 id，取法也一致（先 `LookupIconIdFromDirectoryEx`
//! 在资源组里挑最接近目标尺寸的那张，再 `CreateIconFromResourceEx` 还原成 HICON）。
//! 这样画出来的喇叭和任务栏音量图标是同一份美术资源，不是自己画的。
//!
//! 静态资源，进程内缓存一份；取不到（非 Windows / dll 缺失）返回 `None`，
//! 调用方回退到数字图标。

use std::{cell::RefCell, collections::HashMap};

use windows::{
    Win32::{
        Graphics::Gdi::{
            BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, CreateCompatibleDC, DIB_RGB_COLORS,
            DeleteDC, DeleteObject, GetDIBits, GetObjectW, HBITMAP, HGDIOBJ,
        },
        System::LibraryLoader::{
            FindResourceW, LOAD_LIBRARY_AS_DATAFILE, LOAD_LIBRARY_AS_IMAGE_RESOURCE,
            LoadLibraryExW, LoadResource, LockResource, SizeofResource,
        },
        UI::WindowsAndMessaging::{
            CreateIconFromResourceEx, DestroyIcon, GetIconInfo, HICON, ICONINFO, IMAGE_FLAGS,
            LR_DEFAULTCOLOR, LookupIconIdFromDirectoryEx, RT_GROUP_ICON, RT_ICON,
        },
    },
    core::PCWSTR,
};

/// EarTrumpet `SndVolSSO.IconId` 的取值，一一对应。
pub const ID_MUTED: u32 = 120;
pub const ID_SPEAKER_ZERO_BARS: u32 = 121;
pub const ID_SPEAKER_ONE_BAR: u32 = 122;
pub const ID_SPEAKER_TWO_BARS: u32 = 123;
pub const ID_SPEAKER_THREE_BARS: u32 = 124;
pub const ID_NO_DEVICE: u32 = 125;

thread_local! {
    static CACHE: RefCell<HashMap<(u32, u32), Option<Vec<u8>>>> = RefCell::new(HashMap::new());
}

/// 取一张 `size`×`size` 的 RGBA 图标（straight alpha）。
///
/// `size` 传托盘实际显示的小图标尺寸（`SM_CXSMICON`），系统不用再缩放，边缘不糊。
pub fn icon_rgba(id: u32, size: u32) -> Option<Vec<u8>> {
    if let Some(cached) = CACHE.with(|cache| cache.borrow().get(&(id, size)).cloned()) {
        return cached;
    }
    let loaded = extract(id, size.max(8) as i32);
    CACHE.with(|cache| {
        cache.borrow_mut().insert((id, size), loaded.clone());
    });
    loaded
}

/// 按 EarTrumpet `IconHelper.ColorIcon` 的做法换色：亮色像素改成指定颜色。
///
/// 系统这批图标是白色的，浅色任务栏下 EarTrumpet 会把它们染成黑色。
pub fn tint(rgba: &mut [u8], color: [u8; 3]) {
    for pixel in rgba.chunks_exact_mut(4) {
        if pixel[0] > 220 {
            pixel[0] = color[0];
            pixel[1] = color[1];
            pixel[2] = color[2];
        }
    }
}

fn dll_path() -> Vec<u16> {
    let windir = std::env::var_os("SystemRoot")
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from("C:\\Windows"));
    windir
        .join("System32")
        .join("SndVolSSO.dll")
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect()
}

fn extract(id: u32, size: i32) -> Option<Vec<u8>> {
    unsafe {
        let path = dll_path();
        let module = LoadLibraryExW(
            PCWSTR(path.as_ptr()),
            None,
            LOAD_LIBRARY_AS_DATAFILE | LOAD_LIBRARY_AS_IMAGE_RESOURCE,
        )
        .ok()?;

        let group = FindResourceW(Some(module), make_int_resource(id), RT_GROUP_ICON);
        if group.is_invalid() {
            return None;
        }
        let group_data = LockResource(LoadResource(Some(module), group).ok()?);
        if group_data.is_null() {
            return None;
        }
        let icon_id =
            LookupIconIdFromDirectoryEx(group_data as *const u8, true, size, size, LR_DEFAULTCOLOR);
        if icon_id <= 0 {
            return None;
        }

        let resource = FindResourceW(Some(module), make_int_resource(icon_id as u32), RT_ICON);
        if resource.is_invalid() {
            return None;
        }
        let length = SizeofResource(Some(module), resource);
        if length == 0 {
            return None;
        }
        let bits = LockResource(LoadResource(Some(module), resource).ok()?);
        if bits.is_null() {
            return None;
        }
        let bytes = std::slice::from_raw_parts(bits as *const u8, length as usize);

        let hicon = CreateIconFromResourceEx(
            bytes,
            true,
            0x0003_0000,
            size,
            size,
            IMAGE_FLAGS(LR_DEFAULTCOLOR.0),
        )
        .ok()?;
        let rgba = hicon_to_rgba(hicon, size as u32);
        let _ = DestroyIcon(hicon);
        rgba
    }
}

fn make_int_resource(id: u32) -> PCWSTR {
    PCWSTR(id as usize as *const u16)
}

fn hicon_to_rgba(hicon: HICON, size: u32) -> Option<Vec<u8>> {
    unsafe {
        let mut info = ICONINFO::default();
        GetIconInfo(hicon, &mut info).ok()?;

        let rgba = if !info.hbmColor.is_invalid() {
            color_bitmap_to_rgba(info.hbmColor, size)
        } else {
            None
        };

        if !info.hbmColor.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(info.hbmColor.0));
        }
        if !info.hbmMask.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(info.hbmMask.0));
        }
        rgba
    }
}

fn color_bitmap_to_rgba(bitmap: HBITMAP, size: u32) -> Option<Vec<u8>> {
    unsafe {
        let mut info = BITMAP::default();
        if GetObjectW(
            HGDIOBJ(bitmap.0),
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut info as *mut _ as *mut _),
        ) == 0
        {
            return None;
        }
        let (width, height) = (info.bmWidth, info.bmHeight);
        if width <= 0 || height <= 0 {
            return None;
        }

        let mut header = BITMAPINFO::default();
        header.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        header.bmiHeader.biWidth = width;
        // 负数高度 = 顶向下，省一次翻转。
        header.bmiHeader.biHeight = -height;
        header.bmiHeader.biPlanes = 1;
        header.bmiHeader.biBitCount = 32;
        header.bmiHeader.biCompression = BI_RGB.0;

        let mut buffer = vec![0u8; (width * height * 4) as usize];
        let dc = CreateCompatibleDC(None);
        let copied = GetDIBits(
            dc,
            bitmap,
            0,
            height as u32,
            Some(buffer.as_mut_ptr() as *mut _),
            &mut header,
            DIB_RGB_COLORS,
        );
        let _ = DeleteDC(dc);
        if copied == 0 {
            return None;
        }

        // BGRA → RGBA
        for pixel in buffer.chunks_exact_mut(4) {
            pixel.swap(0, 2);
        }

        // 32 位图标都带 alpha；万一整张透明（老式 24 位 + 掩码图标），
        // 用「有没有颜色」当覆盖率，至少不会画成一片空白。
        if !buffer.chunks_exact(4).any(|pixel| pixel[3] != 0) {
            for pixel in buffer.chunks_exact_mut(4) {
                let ink = pixel[0].max(pixel[1]).max(pixel[2]);
                pixel[3] = ink;
            }
        }

        // 系统给的位图尺寸和要的不一致时按最近邻缩放（同一档位不会走这里）。
        Some(resize_to(buffer, width as u32, height as u32, size))
    }
}

/// 最近邻缩放（透明度也一起放），只在资源尺寸与目标尺寸不一致时用到。
fn resize_to(source: Vec<u8>, width: u32, height: u32, target: u32) -> Vec<u8> {
    if width == target && height == target {
        return source;
    }
    let target = target.max(1);
    let mut out = vec![0u8; (target * target * 4) as usize];
    for y in 0..target {
        let sy = (y as f32 + 0.5) / target as f32 * height as f32;
        let sy = (sy as u32).min(height - 1);
        for x in 0..target {
            let sx = (x as f32 + 0.5) / target as f32 * width as f32;
            let sx = (sx as u32).min(width - 1);
            let from = ((sy * width + sx) * 4) as usize;
            let to = ((y * target + x) * 4) as usize;
            out[to..to + 4].copy_from_slice(&source[from..from + 4]);
        }
    }
    out
}

/// 仅供调试：把图标写成灰度字符画，确认取到的是「喇叭 + N 条弧」。
#[cfg(test)]
pub fn ascii_preview(id: u32, side: u32) -> String {
    let Some(rgba) = icon_rgba(id, side) else {
        return format!("图标 {id} 取不到\n");
    };
    let size = side as usize;
    let mut out = String::new();
    for y in 0..size {
        for x in 0..size {
            let alpha = rgba[(y * size + x) * 4 + 3];
            out.push(match alpha {
                0..=29 => ' ',
                30..=109 => '.',
                110..=199 => '*',
                _ => '#',
            });
        }
        out.push('\n');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 系统图标必须取得到、尺寸对得上，而且五个档位互不相同。
    #[test]
    fn system_icons_are_available_and_distinct() {
        let ids = [
            ID_MUTED,
            ID_SPEAKER_ZERO_BARS,
            ID_SPEAKER_ONE_BAR,
            ID_SPEAKER_TWO_BARS,
            ID_SPEAKER_THREE_BARS,
        ];
        let mut seen: Vec<Vec<u8>> = Vec::new();
        for id in ids {
            let rgba = icon_rgba(id, 16).unwrap_or_else(|| panic!("图标 {id} 取不到"));
            assert_eq!(rgba.len(), (16 * 16 * 4) as usize);
            let inked = rgba.chunks_exact(4).filter(|pixel| pixel[3] > 40).count();
            assert!(inked > 60, "图标 {id} 几乎是空的（{inked} 个不透明像素）");
            assert!(!seen.contains(&rgba), "图标 {id} 和前面的完全一样");
            seen.push(rgba);
        }
    }

    #[test]
    fn tint_replaces_bright_ink() {
        let mut rgba = vec![255, 255, 255, 255, 10, 10, 10, 255];
        tint(&mut rgba, [0, 0, 0]);
        assert_eq!(&rgba[0..4], &[0, 0, 0, 255]);
        assert_eq!(&rgba[4..8], &[10, 10, 10, 255]);
    }

    #[test]
    #[ignore]
    fn print_icons() {
        for id in [
            ID_SPEAKER_ZERO_BARS,
            ID_SPEAKER_ONE_BAR,
            ID_SPEAKER_TWO_BARS,
            ID_SPEAKER_THREE_BARS,
            ID_MUTED,
        ] {
            println!("--- {id} ---\n{}", ascii_preview(id, 16));
        }
    }
}
