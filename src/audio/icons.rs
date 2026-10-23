//! 应用图标提取：用 PrivateExtractIconsW 按目标尺寸直接从 exe / .ico 里取图，
//! 转成 PNG 缓存到本地目录，界面按路径加载。
//!
//! 尺寸由设置里的「应用图标」决定（16 / 32 / 64）：按那一档取、也按那一档画，
//! 1:1 才是清楚的。取 64 再缩到 20 会有一次重采样，边缘反而更糊。

use std::{
    collections::hash_map::DefaultHasher,
    ffi::c_void,
    fs,
    hash::{Hash, Hasher},
    path::{Path, PathBuf},
    time::UNIX_EPOCH,
};

use windows::Win32::{
    Graphics::Gdi::{
        BI_RGB, BITMAP, BITMAPINFO, BITMAPINFOHEADER, DIB_RGB_COLORS, DeleteObject, GetDC,
        GetDIBits, GetObjectW, HBITMAP, HGDIOBJ, ReleaseDC,
    },
    UI::WindowsAndMessaging::{DestroyIcon, GetIconInfo, HICON, ICONINFO},
};

#[link(name = "user32")]
unsafe extern "system" {
    fn PrivateExtractIconsW(
        szfilename: *const u16,
        niconindex: i32,
        cxicon: i32,
        cyicon: i32,
        phicon: *mut *mut c_void,
        piconid: *mut u32,
        nicons: u32,
        flags: u32,
    ) -> u32;
}

fn cache_key(path: &Path) -> String {
    let mut hasher = DefaultHasher::new();
    path.to_string_lossy().to_lowercase().hash(&mut hasher);
    if let Ok(metadata) = fs::metadata(path) {
        metadata.len().hash(&mut hasher);
        if let Ok(modified) = metadata.modified()
            && let Ok(since) = modified.duration_since(UNIX_EPOCH)
        {
            since.as_secs().hash(&mut hasher);
        }
    }
    format!("{:016x}", hasher.finish())
}

/// 确保图标已按 `pixels` 缓存，返回 PNG 路径。
pub fn ensure_icon(path: &Path, cache_dir: &Path, pixels: u32) -> Option<PathBuf> {
    if !path.exists() {
        return None;
    }
    let key = cache_key(path);
    let target = cache_dir.join(format!("{key}-{pixels}.png"));
    if target.exists() {
        return Some(target);
    }

    let (width, height, rgba) = extract_rgba(path, pixels)?;
    fs::create_dir_all(cache_dir).ok()?;
    let image = image::RgbaImage::from_raw(width, height, rgba)?;
    image.save(&target).ok()?;
    Some(target)
}

fn extract_rgba(path: &Path, pixels: u32) -> Option<(u32, u32, Vec<u8>)> {
    let debug = std::env::var_os("BOLUME_DEBUG_ICON").is_some();
    let wide: Vec<u16> = path
        .to_string_lossy()
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let pixels = pixels.clamp(16, 256) as i32;

    unsafe {
        let mut hicon: *mut c_void = std::ptr::null_mut();
        let mut icon_id: u32 = 0;
        let extracted = PrivateExtractIconsW(
            wide.as_ptr(),
            0,
            pixels,
            pixels,
            &mut hicon,
            &mut icon_id,
            1,
            0,
        );
        if extracted == 0 || hicon.is_null() {
            if debug {
                eprintln!("PrivateExtractIconsW 失败：{}", path.display());
            }
            return None;
        }

        let icon = HICON(hicon);
        let mut info = ICONINFO::default();
        if GetIconInfo(icon, &mut info).is_err() {
            let _ = DestroyIcon(icon);
            return None;
        }

        let result = bitmap_to_rgba(info.hbmColor, info.hbmMask);

        if !info.hbmColor.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(info.hbmColor.0));
        }
        if !info.hbmMask.is_invalid() {
            let _ = DeleteObject(HGDIOBJ(info.hbmMask.0));
        }
        let _ = DestroyIcon(icon);
        result
    }
}

unsafe fn bitmap_to_rgba(color: HBITMAP, mask: HBITMAP) -> Option<(u32, u32, Vec<u8>)> {
    unsafe {
        if color.is_invalid() {
            return None;
        }
        let mut bitmap = BITMAP::default();
        if GetObjectW(
            HGDIOBJ(color.0),
            std::mem::size_of::<BITMAP>() as i32,
            Some(&mut bitmap as *mut _ as *mut c_void),
        ) == 0
        {
            return None;
        }
        let width = bitmap.bmWidth.max(1);
        let height = bitmap.bmHeight.max(1);

        let mut header = BITMAPINFO::default();
        header.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 32,
            biCompression: BI_RGB.0,
            ..Default::default()
        };

        let mut buffer = vec![0u8; (width * height * 4) as usize];
        let dc = GetDC(None);
        if dc.is_invalid() {
            return None;
        }
        let lines = GetDIBits(
            dc,
            color,
            0,
            height as u32,
            Some(buffer.as_mut_ptr() as *mut c_void),
            &mut header,
            DIB_RGB_COLORS,
        );
        ReleaseDC(None, dc);
        if lines == 0 {
            return None;
        }

        // BGRA → RGBA，并处理没有 alpha 通道的老式图标。
        let mut has_alpha = false;
        for pixel in buffer.chunks_exact_mut(4) {
            pixel.swap(0, 2);
            if pixel[3] != 0 {
                has_alpha = true;
            }
        }

        if !has_alpha {
            let mask_bits = mask_alpha(width, height, mask);
            for (index, pixel) in buffer.chunks_exact_mut(4).enumerate() {
                pixel[3] = match &mask_bits {
                    Some(bits) => {
                        if bits.get(index).copied().unwrap_or(1) == 0 {
                            255
                        } else {
                            0
                        }
                    }
                    None => 255,
                };
            }
        }

        Some((width as u32, height as u32, buffer))
    }
}

/// 读取 1bpp 掩码位图：1 表示透明。
unsafe fn mask_alpha(width: i32, height: i32, mask: HBITMAP) -> Option<Vec<u8>> {
    unsafe {
        if mask.is_invalid() {
            return None;
        }
        let mut header = BITMAPINFO::default();
        header.bmiHeader = BITMAPINFOHEADER {
            biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
            biWidth: width,
            biHeight: -height,
            biPlanes: 1,
            biBitCount: 1,
            biCompression: BI_RGB.0,
            ..Default::default()
        };
        let stride = (width as usize).div_ceil(32) * 4;
        let mut buffer = vec![0u8; stride * height as usize];
        let dc = GetDC(None);
        if dc.is_invalid() {
            return None;
        }
        let lines = GetDIBits(
            dc,
            mask,
            0,
            height as u32,
            Some(buffer.as_mut_ptr() as *mut c_void),
            &mut header,
            DIB_RGB_COLORS,
        );
        ReleaseDC(None, dc);
        if lines == 0 {
            return None;
        }

        let mut bits = Vec::with_capacity((width * height) as usize);
        for y in 0..height as usize {
            for x in 0..width as usize {
                let byte = buffer[y * stride + x / 8];
                let bit = (byte >> (7 - (x % 8))) & 1;
                bits.push(bit);
            }
        }
        Some(bits)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 设置里可选的三档尺寸，取出来就得是那个边长（不是取 64 再缩）。
    #[test]
    fn extracted_icon_matches_requested_size() {
        let sample = Path::new("C:\\Windows\\System32\\shell32.dll");
        if !sample.exists() {
            return;
        }
        for pixels in [16u32, 32, 64] {
            match extract_rgba(sample, pixels) {
                Some((width, height, rgba)) => {
                    assert_eq!(width, pixels, "宽度应为 {pixels}");
                    assert_eq!(height, pixels, "高度应为 {pixels}");
                    assert_eq!(rgba.len(), (pixels * pixels * 4) as usize);
                }
                // 某些精简系统里没有这个 dll 的图标资源，跳过而不是失败。
                None => eprintln!("跳过 {pixels}px：提取失败"),
            }
        }
    }
}
