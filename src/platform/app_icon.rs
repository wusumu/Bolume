//! 应用图标：解析内嵌的 `assets/Bolume.ico`。
//!
//! 同一个文件三个用途：
//! - 编进 exe 当 exe 图标（`build.rs` 交给 rc.exe）；
//! - 托盘图标选「软件图标」时按托盘尺寸取对应那一档；
//! - 设置窗口 / 任务栏按钮的窗口图标（见 `windows::platform::apply_app_icon`）。
//!
//! ICO 里并存两种图：小尺寸是 32 位 BMP（含 AND 掩码），256 那一档是 PNG。
//! 这里两种都认——托盘要的就是 16/20/24 这几档 BMP，直接 1:1 贴，缩放一步都不做。

/// 图标文件本身（`assets/Bolume.ico`）。
pub const ICO_BYTES: &[u8] = include_bytes!("../../assets/Bolume.ico");

struct Entry {
    width: u32,
    height: u32,
    /// 数据在文件里的偏移与长度。
    offset: usize,
    length: usize,
}

/// 列出 ico 里的所有档位（宽高按目录里的字节读，0 表示 256）。
fn entries(bytes: &[u8]) -> Vec<Entry> {
    if bytes.len() < 6 {
        return Vec::new();
    }
    let count = u16::from_le_bytes([bytes[4], bytes[5]]) as usize;
    let mut list = Vec::with_capacity(count);
    for index in 0..count {
        let base = 6 + index * 16;
        if base + 16 > bytes.len() {
            break;
        }
        let width = if bytes[base] == 0 {
            256
        } else {
            bytes[base] as u32
        };
        let height = if bytes[base + 1] == 0 {
            256
        } else {
            bytes[base + 1] as u32
        };
        let length = u32::from_le_bytes([
            bytes[base + 8],
            bytes[base + 9],
            bytes[base + 10],
            bytes[base + 11],
        ]) as usize;
        let offset = u32::from_le_bytes([
            bytes[base + 12],
            bytes[base + 13],
            bytes[base + 14],
            bytes[base + 15],
        ]) as usize;
        if offset + length > bytes.len() || length == 0 {
            continue;
        }
        list.push(Entry {
            width,
            height,
            offset,
            length,
        });
    }
    list
}

/// 托盘画布尺寸对应的图标（RGBA，straight alpha）。
///
/// 取档顺序：正好有这一档就用它；否则用比它大的最小一档；再没有就用最大一档。
/// 只有「没有正好的档」时才会缩一次，这时候才可能出现一点点软边。
pub fn rgba_at(size: u32) -> Option<Vec<u8>> {
    rgba_from(ICO_BYTES, size)
}

/// 从任意一份 ico 字节里取托盘尺寸那一档（用户自己选的本地图标走这里）。
///
/// 解析规则和内置图标完全一致：正好有这一档就用它，没有再往大里找一档缩下来。
pub fn rgba_from(bytes: &[u8], size: u32) -> Option<Vec<u8>> {
    let list = entries(bytes);
    let exact = list
        .iter()
        .find(|entry| entry.width == size && entry.height == size);
    let chosen = exact.or_else(|| {
        list.iter()
            .filter(|entry| entry.width >= size && entry.height >= size)
            .min_by_key(|entry| entry.width)
    });
    let chosen = chosen.or_else(|| list.iter().max_by_key(|entry| entry.width))?;

    let (width, height, pixels) = decode(bytes, chosen)?;
    if width == size && height == size {
        return Some(pixels);
    }
    scale(&pixels, width, height, size)
}

/// 解一档：PNG 走 image，其余按 32 位 BMP 读。
fn decode(bytes: &[u8], entry: &Entry) -> Option<(u32, u32, Vec<u8>)> {
    let payload = bytes.get(entry.offset..entry.offset + entry.length)?;
    if payload.starts_with(b"\x89PNG\r\n\x1a\n") {
        let image = image::load_from_memory(payload).ok()?.into_rgba8();
        let (width, height) = image.dimensions();
        return Some((width, height, image.into_raw()));
    }
    decode_bmp(payload)
}

/// 32 位 BMP（ICO 里的 DIB）：行是自下而上的 BGRA，后面跟 AND 掩码。
fn decode_bmp(payload: &[u8]) -> Option<(u32, u32, Vec<u8>)> {
    if payload.len() < 40 {
        return None;
    }
    let header = u32::from_le_bytes([payload[0], payload[1], payload[2], payload[3]]) as usize;
    let width = i32::from_le_bytes([payload[4], payload[5], payload[6], payload[7]]);
    let double_height = i32::from_le_bytes([payload[8], payload[9], payload[10], payload[11]]);
    let bits = u16::from_le_bytes([payload[14], payload[15]]);
    let compression = u32::from_le_bytes([payload[16], payload[17], payload[18], payload[19]]);
    if width <= 0 || double_height == 0 || bits != 32 || compression != 0 {
        return None;
    }
    let width = width as usize;
    let height = (double_height.unsigned_abs() as usize) / 2;
    if width == 0 || height == 0 {
        return None;
    }
    let stride = width * 4;
    let start = header;
    if start + stride * height > payload.len() {
        return None;
    }
    let bottom_up = double_height > 0;

    let mut out = vec![0u8; width * height * 4];
    for y in 0..height {
        let src_y = if bottom_up { height - 1 - y } else { y };
        let row = &payload[start + src_y * stride..start + src_y * stride + stride];
        for x in 0..width {
            let offset = (y * width + x) * 4;
            out[offset] = row[x * 4 + 2];
            out[offset + 1] = row[x * 4 + 1];
            out[offset + 2] = row[x * 4];
            out[offset + 3] = row[x * 4 + 3];
        }
    }

    // 有些老图标整张 alpha 都是 0，只靠 AND 掩码描边；这种情况按掩码补不透明度，
    // 否则整张图会全透明（托盘上什么都看不到）。
    let mask_start = start + stride * height;
    let has_alpha = out.chunks_exact(4).any(|pixel| pixel[3] != 0);
    if !has_alpha && mask_start < payload.len() {
        let mask_stride = width.div_ceil(32) * 4;
        for y in 0..height {
            let src_y = if bottom_up { height - 1 - y } else { y };
            let row_start = mask_start + src_y * mask_stride;
            if row_start + mask_stride > payload.len() {
                break;
            }
            for x in 0..width {
                let byte = payload[row_start + x / 8];
                let bit = (byte >> (7 - (x % 8))) & 1;
                out[(y * width + x) * 4 + 3] = if bit == 1 { 0 } else { 255 };
            }
        }
    }
    Some((width as u32, height as u32, out))
}

/// 面积采样缩放（只在「ico 里没有正好这一档」时才走这里）。
pub fn scale(src: &[u8], width: u32, height: u32, size: u32) -> Option<Vec<u8>> {
    if size == 0 {
        return None;
    }
    let size = size as usize;
    let (sw, sh) = (width as usize, height as usize);
    let mut out = vec![0u8; size * size * 4];
    let ratio = sw as f32 / size as f32;
    for y in 0..size {
        let y0 = (y as f32 * ratio) as usize;
        let y1 = (((y + 1) as f32 * ratio).ceil() as usize)
            .min(sh)
            .max(y0 + 1);
        for x in 0..size {
            let x0 = (x as f32 * ratio) as usize;
            let x1 = (((x + 1) as f32 * ratio).ceil() as usize)
                .min(sw)
                .max(x0 + 1);
            let (mut r, mut g, mut b, mut a, mut count) = (0u32, 0u32, 0u32, 0u32, 0u32);
            for sy in y0..y1 {
                for sx in x0..x1 {
                    let offset = (sy * sw + sx) * 4;
                    let alpha = src[offset + 3] as u32;
                    // 按 alpha 加权：透明的边缘像素不该把颜色拉黑。
                    r += src[offset] as u32 * alpha;
                    g += src[offset + 1] as u32 * alpha;
                    b += src[offset + 2] as u32 * alpha;
                    a += alpha;
                    count += 1;
                }
            }
            let offset = (y * size + x) * 4;
            if a > 0 {
                out[offset] = (r / a) as u8;
                out[offset + 1] = (g / a) as u8;
                out[offset + 2] = (b / a) as u8;
            }
            out[offset + 3] = if count == 0 { 0 } else { (a / count) as u8 };
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 文件里必须带正好的 16 / 20 / 24 / 32 / 48 / 64 这几档：托盘直接用它们。
    #[test]
    fn has_common_sizes() {
        let sizes: Vec<u32> = entries(ICO_BYTES).iter().map(|entry| entry.width).collect();
        for size in [16, 20, 24, 32, 48, 64, 256] {
            assert!(sizes.contains(&size), "ico 里缺 {size} 这一档：{sizes:?}");
        }
    }

    /// 16px 那一档要能解出来，而且是真的有内容的彩色图（不是全透明）。
    #[test]
    fn decodes_small_size_with_ink() {
        let pixels = rgba_at(16).expect("16px 图标要能解出来");
        assert_eq!(pixels.len(), 16 * 16 * 4);
        let opaque = pixels.chunks_exact(4).filter(|px| px[3] > 128).count();
        assert!(opaque > 40, "16px 图标几乎没有不透明像素：{opaque}");
        // 至少存在一个彩色像素（三通道不全相等）。
        assert!(
            pixels
                .chunks_exact(4)
                .any(|px| px[3] > 128 && (px[0] != px[1] || px[1] != px[2])),
            "16px 图标是纯灰度，可能解错了通道"
        );
    }

    /// 尺寸对不上时（比如 18px 的画布）走缩放，也得是满尺寸、边缘不溢出。
    #[test]
    fn scales_to_unusual_size() {
        let pixels = rgba_at(18).expect("18px 要能缩出来");
        assert_eq!(pixels.len(), 18 * 18 * 4);
        assert!(pixels.chunks_exact(4).any(|px| px[3] > 128));
    }

    /// 临时预览：把托盘尺寸的图标原样导出来人工核对（`cargo test -- --ignored`）。
    #[test]
    #[ignore]
    fn dump_tray_icon() {
        let pixels = rgba_at(16).expect("16px 图标");
        std::fs::write("target/app-icon-16.rgba", &pixels).unwrap();
    }
}
