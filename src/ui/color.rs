//! 取色用的纯函数：HSV ↔ RGB、色号文本的读写。
//!
//! OSD 编辑器的取色器和设置页的色轮都要用这一套，所以单独放在 ui 里，
//! 两边都只依赖这里的一套换算（顺便保证同一串色号在两边算出来一模一样）。

/// HSV → RGB（h 0..360，s/v 0..1）。
pub fn hsv_to_rgb(h: f32, s: f32, v: f32) -> (u8, u8, u8) {
    let h = h.rem_euclid(360.0);
    let s = s.clamp(0.0, 1.0);
    let v = v.clamp(0.0, 1.0);
    let c = v * s;
    let hp = h / 60.0;
    let x = c * (1.0 - (hp.rem_euclid(2.0) - 1.0).abs());
    let (r, g, b) = match hp as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    let m = v - c;
    (
        ((r + m) * 255.0).round() as u8,
        ((g + m) * 255.0).round() as u8,
        ((b + m) * 255.0).round() as u8,
    )
}

/// RGB → HSV。
pub fn rgb_to_hsv(r: u8, g: u8, b: u8) -> (f32, f32, f32) {
    let r = r as f32 / 255.0;
    let g = g as f32 / 255.0;
    let b = b as f32 / 255.0;
    let max = r.max(g).max(b);
    let min = r.min(g).min(b);
    let delta = max - min;
    let h = if delta <= f32::EPSILON {
        0.0
    } else if max == r {
        60.0 * (((g - b) / delta) % 6.0)
    } else if max == g {
        60.0 * ((b - r) / delta + 2.0)
    } else {
        60.0 * ((r - g) / delta + 4.0)
    };
    (
        h.rem_euclid(360.0),
        if max <= 0.0 { 0.0 } else { delta / max },
        max,
    )
}

/// `#rrggbb`。
pub fn hex_of(rgb: (u8, u8, u8)) -> String {
    format!("#{:02x}{:02x}{:02x}", rgb.0, rgb.1, rgb.2)
}

/// 解析色号：允许省略 `#`，也接受 3 位简写。
pub fn parse_rgb(text: &str) -> Option<(u8, u8, u8)> {
    let hex = text.trim().trim_start_matches('#');
    let expanded = match hex.len() {
        3 => hex.chars().flat_map(|c| [c, c]).collect::<String>(),
        6 => hex.to_string(),
        _ => return None,
    };
    let value = u32::from_str_radix(&expanded, 16).ok()?;
    Some(((value >> 16) as u8, (value >> 8) as u8, value as u8))
}

/// 往色号输入缓冲里追加字符：只收十六进制与开头的 `#`，最多 `#xxxxxx`。
///
/// 粘贴一整串 `#a78bfa` 时也走这里，多余的字符直接丢掉。
pub fn push_hex(buffer: &mut String, text: &str) {
    for ch in text.chars() {
        let accept = ch.is_ascii_hexdigit() || (ch == '#' && buffer.is_empty());
        if !accept {
            continue;
        }
        if buffer.chars().count() >= 7 {
            break;
        }
        buffer.push(ch);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_round_trips_through_hsv() {
        for text in ["#7c9cff", "#151519", "#ffffff", "#000000"] {
            let rgb = parse_rgb(text).unwrap();
            let (h, s, v) = rgb_to_hsv(rgb.0, rgb.1, rgb.2);
            assert_eq!(hex_of(hsv_to_rgb(h, s, v)), text);
        }
    }

    #[test]
    fn parse_accepts_short_and_bare_forms() {
        assert_eq!(parse_rgb("f0f"), Some((0xff, 0x00, 0xff)));
        assert_eq!(parse_rgb("  #A78BFA "), Some((0xa7, 0x8b, 0xfa)));
        assert_eq!(parse_rgb("#12345"), None);
        assert_eq!(parse_rgb(""), None);
    }

    #[test]
    fn push_hex_stops_at_seven_chars_and_skips_junk() {
        let mut buffer = String::new();
        push_hex(&mut buffer, "#A78BFA extra!");
        assert_eq!(buffer, "#A78BFA");
        // 已经有内容之后再来一个 # 会被丢掉。
        push_hex(&mut buffer, "#");
        assert_eq!(buffer, "#A78BFA");
    }
}
