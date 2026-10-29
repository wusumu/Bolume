//! 视觉主题：统一的色彩与尺寸令牌。
//!
//! 没有「一套一套的预设」了：打底只有深色 / 浅色两套模板（`Palette::template`），
//! 具体长什么样由用户选的背景色 + 强调色决定 —— 选了浅色底，文字和描边自动翻成
//! 深色；强调色空着就跟 Windows 的强调色走。
//!
//! 所有取色函数都读运行时的 `Palette`，所以界面代码不用改一行就能换肤。

use std::cell::{Cell, RefCell};

use gpui::{Hsla, Pixels, SharedString, px, rgb_to_hsla, rgba};

/// `#rrggbb` / `#rrggbbaa` / `rrggbb` → `Hsla`。解析不了返回 `None`。
///
/// OSD 允许用户自己指定卡片 / 文字 / 强调色，存的就是这种十六进制串。
pub fn parse_hex(text: &str) -> Option<Hsla> {
    let hex = text.trim().trim_start_matches('#');
    let value = u32::from_str_radix(hex, 16).ok()?;
    let color = match hex.len() {
        6 => (value << 8) | 0xff,
        8 => value,
        _ => return None,
    };
    Some(rgb_to_hsla(rgba(color)))
}

/// 一套配色。颜色统一存成 Win32 风格的 `0xRRGGBBAA`。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    pub surface: u32,
    pub surface_sunken: u32,
    pub surface_elevated: u32,
    pub surface_hover: u32,
    pub surface_active: u32,
    pub border: u32,
    pub border_strong: u32,
    pub accent: u32,
    pub accent_soft: u32,
    pub accent_glow: u32,
    pub muted_fill: u32,
    pub track: u32,
    /// 音量条上「正在发声」的活动层颜色：特效全靠往它上面叠透明度。
    pub activity: u32,
    pub text_primary: u32,
    pub text_secondary: u32,
    pub text_tertiary: u32,
    pub danger: u32,
}

impl Default for Palette {
    fn default() -> Self {
        Self {
            surface: 0x151519f7,
            surface_sunken: 0x10101399,
            surface_elevated: 0x24242cf7,
            surface_hover: 0xffffff0d,
            surface_active: 0xffffff14,
            border: 0xffffff14,
            border_strong: 0xffffff24,
            accent: 0x7c9cffff,
            accent_soft: 0x7c9cff2e,
            accent_glow: 0x7c9cff59,
            muted_fill: 0xffffff2e,
            track: 0xffffff1f,
            activity: 0xffffffff,
            text_primary: 0xf6f6f9ff,
            text_secondary: 0xffffff9e,
            text_tertiary: 0xffffff61,
            danger: 0xff8080ff,
        }
    }
}

impl Palette {
    /// 深色 / 浅色模板：用户自选底色时，文字、描边、悬停层的浓淡都从这两套里取，
    /// 免得选了浅色底还配上白字。
    fn template(light: bool) -> Self {
        if light {
            Self {
                surface: 0xf6f6faf8,
                surface_sunken: 0xe8e8efd9,
                surface_elevated: 0xffffffff,
                surface_hover: 0x0000000d,
                surface_active: 0x00000016,
                border: 0x00000014,
                border_strong: 0x00000026,
                accent: 0x2f6febff,
                accent_soft: 0x2f6feb2e,
                accent_glow: 0x2f6feb59,
                muted_fill: 0x00000024,
                track: 0x0000001f,
                activity: 0xffffffff,
                text_primary: 0x1b1b21ff,
                text_secondary: 0x000000a8,
                text_tertiary: 0x00000070,
                danger: 0xc62d33ff,
            }
        } else {
            Self::default()
        }
    }

    /// 换成用户自选的底色。
    ///
    /// 只改 `surface` 一个值不行：深底配白字、浅底配黑字，悬停层和描边也全是反的。
    /// 所以先按底色的明暗挑一套模板，再把三档表面按同一个色相调出来，
    /// 剩下的文字 / 描边 / 控件色交给模板保证协调。
    fn with_surface(&mut self, base: u32) {
        let light = is_light(base);
        *self = Self::template(light);
        let alpha = (base & 0xff).max(0xd8);
        let rgb = base & 0xffff_ff00;
        self.surface = rgb | alpha;
        // 下沉层比底色暗一点、再透一点；抬高层亮一点：层级靠明度差看出来，不靠描边。
        self.surface_sunken =
            tint(rgb, if light { -0.045 } else { -0.03 }) | alpha.saturating_sub(0x20);
        self.surface_elevated = tint(rgb, if light { 0.05 } else { 0.07 }) | alpha;
    }

    /// 换成用户自选的强调色。
    ///
    /// 淡底和光晕按同一色相、不同透明度派生：照抄那个不透明色的话，
    /// 选中态和滑条就没有层次了。
    fn with_accent(&mut self, accent: u32) {
        let rgb = accent & 0xffff_ff00;
        self.accent = rgb | 0xff;
        self.accent_soft = rgb | 0x2e;
        self.accent_glow = rgb | 0x59;
    }

    /// 按百分比拉开 / 收紧明暗对比（100 = 原样）。
    ///
    /// 只动「次级墨迹」：文字二三级、描边、悬停层、滑轨、控件填充和强调色的淡底。
    /// 主文字在高对比度下已经是纯色了，再推也没有空间；低对比度时把它往底色上
    /// 揉一点，否则整窗只剩标题最扎眼、读起来反而更累。
    fn with_contrast(&mut self, percent: u32) {
        let ratio = (percent.clamp(20, 250) as f32 / 100.0).clamp(0.2, 2.5);
        if (ratio - 1.0).abs() < 0.005 {
            return;
        }
        // 文字用满幅度，描边 / 填充收一点：这些面积大，按同样比例会糊成一片。
        let ink = ratio;
        let line = ratio.powf(0.7);
        self.text_secondary = scale_alpha(self.text_secondary, ink);
        self.text_tertiary = scale_alpha(self.text_tertiary, ink);
        for color in [
            &mut self.border,
            &mut self.border_strong,
            &mut self.surface_hover,
            &mut self.surface_active,
            &mut self.track,
            &mut self.muted_fill,
            &mut self.accent_soft,
            &mut self.accent_glow,
        ] {
            *color = scale_alpha(*color, line);
        }
        if ratio < 1.0 {
            // 往底色揉 25%（1.0 - ratio 归一化后）
            self.text_primary = mix_rgb(self.text_primary, self.surface, (1.0 - ratio) * 0.25);
        }
    }
}

/// 只缩放颜色的 alpha，RGB 原样保留。
fn scale_alpha(color: u32, factor: f32) -> u32 {
    let alpha = ((color & 0xff) as f32 * factor).round().clamp(0.0, 255.0) as u32;
    (color & 0xffff_ff00) | alpha
}

/// 按 `t`（0..1）把 `a` 的 RGB 往 `b` 上揉，alpha 保留 `a` 的。
fn mix_rgb(a: u32, b: u32, t: f32) -> u32 {
    let t = t.clamp(0.0, 1.0);
    let mut out = a & 0xff;
    for shift in [24, 16, 8] {
        let from = ((a >> shift) & 0xff) as f32;
        let to = ((b >> shift) & 0xff) as f32;
        let value = (from + (to - from) * t).round().clamp(0.0, 255.0) as u32;
        out |= value << shift;
    }
    out
}

/// 这一档颜色算不算「浅色底」（感知亮度超过一半）。
fn is_light(color: u32) -> bool {
    let r = ((color >> 24) & 0xff) as f32;
    let g = ((color >> 16) & 0xff) as f32;
    let b = ((color >> 8) & 0xff) as f32;
    (0.299 * r + 0.587 * g + 0.114 * b) / 255.0 > 0.5
}

/// 在 `color`（只含 RGB）上按比例提亮 / 压暗：`amount` 为正往白靠，为负往黑靠。
fn tint(color: u32, amount: f32) -> u32 {
    let mut out = 0u32;
    for shift in [24, 16, 8] {
        let channel = ((color >> shift) & 0xff) as f32;
        let value = if amount >= 0.0 {
            channel + (255.0 - channel) * amount
        } else {
            channel * (1.0 + amount)
        };
        out |= (value.round().clamp(0.0, 255.0) as u32) << shift;
    }
    out
}

/// `#RRGGBB` / `#RRGGBBAA`（也接受不带 # 的写法）→ `0xRRGGBBAA`。
pub fn parse_color(text: &str) -> Option<u32> {
    let text = text.trim().trim_start_matches('#');
    let value = u32::from_str_radix(text, 16).ok()?;
    match text.len() {
        6 => Some((value << 8) | 0xFF),
        8 => Some(value),
        _ => None,
    }
}

thread_local! {
    /// 当前生效的配色（只在界面线程上读写）。
    static CURRENT: RefCell<Palette> = RefCell::new(Palette::default());
    /// 界面字号缩放倍数（1.0 = 原始字号）。
    static FONT_SCALE: Cell<f32> = const { Cell::new(1.0) };
    /// 窗口整体不透明度（1.0 = 完全实心）。
    static UI_OPACITY: Cell<f32> = const { Cell::new(1.0) };
    /// 界面字体族（`None` = 跟随系统默认字体）。
    ///
    /// 只挂一次、靠 gpui 的样式继承往下传：根节点写 `.font_family(..)`，
    /// 整棵树里没显式指定字体的文字都跟着变。
    static UI_FONT: RefCell<Option<SharedString>> = const { RefCell::new(None) };
    /// 界面字重（100..900，400 = 常规）。
    static UI_FONT_WEIGHT: Cell<u32> = const { Cell::new(400) };
}

/// 界面字体族：`None` 表示跟随系统。
///
/// 传空串就是跟随系统（用户选了「默认」）。
pub fn set_ui_font(family: &str) {
    let value = family.trim();
    UI_FONT.with(|slot| {
        *slot.borrow_mut() = if value.is_empty() {
            None
        } else {
            Some(SharedString::from(value.to_string()))
        };
    });
}

pub fn ui_font() -> Option<SharedString> {
    UI_FONT.with(|slot| slot.borrow().clone())
}

/// 界面字重：根节点上挂一次，和 `ui_font()` 一样靠继承往下传。
///
/// 400（常规）时返回 `None`，调用方就不写这一条 —— 少一层对每个文本的覆盖。
pub fn ui_font_weight() -> Option<gpui::FontWeight> {
    let weight = UI_FONT_WEIGHT.with(|slot| slot.get());
    (weight != 400).then(|| gpui::FontWeight(weight as f32))
}

pub fn set_ui_font_weight(weight: u32) {
    UI_FONT_WEIGHT.with(|slot| slot.set(weight.clamp(100, 900)));
}

/// 界面字号：所有文字尺寸都走这里，改缩放时整窗跟着变。
///
/// 布局里的行高 / 内外边距不缩放（像素值），因为 gpui 的行是按内容撑开的，
/// 字变大行自然变高；反过来把边距也乘一遍，大字号下会显得空得离谱。
pub fn fs(size: f32) -> Pixels {
    px(size * FONT_SCALE.with(|slot| slot.get()))
}

pub fn set_font_scale(scale: f32) {
    FONT_SCALE.with(|slot| slot.set(scale.clamp(0.6, 2.0)));
}

pub fn font_scale() -> f32 {
    FONT_SCALE.with(|slot| slot.get())
}

pub fn set_ui_opacity(opacity: f32) {
    UI_OPACITY.with(|slot| slot.set(opacity.clamp(0.2, 1.0)));
}

pub fn ui_opacity() -> f32 {
    UI_OPACITY.with(|slot| slot.get())
}

/// 把用户自选的底色 / 强调色 / 对比度叠到当前主题上；传空串表示跟随主题，100 = 原样。
///
/// 必须排在 `set_mode()` 之后调用：它是「在模板之上再盖一层」，不是独立的一套配色。
/// 三个覆盖都是从内置模板重生算的，重复调用结果一样（幂等）。
pub fn apply_custom(surface: &str, accent: &str, contrast: u32) {
    let mut palette = CURRENT.with(|slot| *slot.borrow());
    if let Some(color) = parse_color(surface) {
        palette.with_surface(color);
    }
    if let Some(color) = parse_color(accent) {
        palette.with_accent(color);
    }
    palette.with_contrast(contrast);
    CURRENT.with(|slot| *slot.borrow_mut() = palette);
}

/// 打底：深色 / 浅色两套模板之一。
///
/// 只是把调色板重置回模板 —— 用户的背景色 / 强调色要接着用 `apply_custom` 叠上去，
/// 而且必须**先**调这个再叠：模板本身就是「没自选色时的样子」，反过来的话
/// 上一轮的自选色会留在调色板里撤不掉。
pub fn set_mode(dark: bool) {
    CURRENT.with(|slot| *slot.borrow_mut() = Palette::template(!dark));
}

/// `0xRRGGBBAA` → `#rrggbb`。
fn to_hex(value: u32) -> String {
    format!(
        "#{:02x}{:02x}{:02x}",
        (value >> 24) & 0xff,
        (value >> 16) & 0xff,
        (value >> 8) & 0xff
    )
}

/// 强制不透明。
///
/// 主题里的 `surface_elevated` 也留着约 3% 透光（悬浮卡片的本意），但弹出层压在
/// 页面控件上时，底下的色块会隐隐透出来，看着像重影 —— 浮层一律用它。
pub fn opaque(color: Hsla) -> Hsla {
    Hsla {
        alpha: 1.0,
        ..color
    }
}

/// 当前生效的底色（`#rrggbb`）。取色器在「跟随主题」时要回显这个值。
pub fn surface_hex() -> String {
    CURRENT.with(|slot| to_hex(slot.borrow().surface))
}

/// 当前生效的强调色（`#rrggbb`）。
pub fn accent_hex() -> String {
    CURRENT.with(|slot| to_hex(slot.borrow().accent))
}

/// 当前生效的滑条轨道底色（`#rrggbb`）。音量条「底色」留空时取色器从这里起步。
pub fn track_hex() -> String {
    CURRENT.with(|slot| to_hex(slot.borrow().track))
}

/// 当前生效的静音填充色（`#rrggbb`）。音量条「静音时」留空时取色器从这里起步。
pub fn muted_fill_hex() -> String {
    CURRENT.with(|slot| to_hex(slot.borrow().muted_fill))
}

fn color(value: u32) -> Hsla {
    rgb_to_hsla(rgba(value))
}

macro_rules! token {
    ($name:ident, $field:ident) => {
        pub fn $name() -> Hsla {
            CURRENT.with(|slot| color(slot.borrow().$field))
        }
    };
}

token!(surface, surface);

/// 窗口根底色：把主题自带的那点透明拉满。
///
/// 主题里的 `surface` 留着约 3% 的透光（悬浮面板靠它透出一点桌面），但设置窗口
/// 是一扇普通的应用窗 —— 不透明度给到 100% 时它必须真是不透明的，否则背后的一切
/// 都会隐隐透上来，看着就像「100% 了还是有点透明」。
/// 窗口级的透明由 `ui_opacity`（`set_opacity`）单独控制，所以这里直接给满 alpha。
pub fn surface_window() -> Hsla {
    CURRENT.with(|slot| color(slot.borrow().surface | 0xff))
}
token!(surface_sunken, surface_sunken);
token!(surface_elevated, surface_elevated);
token!(surface_hover, surface_hover);
token!(surface_active, surface_active);
token!(border, border);
token!(border_strong, border_strong);
token!(accent, accent);
token!(accent_soft, accent_soft);
token!(accent_glow, accent_glow);
token!(muted_fill, muted_fill);
token!(track, track);
token!(activity, activity);
token!(text_primary, text_primary);
token!(text_secondary, text_secondary);
token!(text_tertiary, text_tertiary);
token!(danger, danger);

pub const RADIUS_WINDOW: f32 = 12.0;
pub const RADIUS_CARD: f32 = 14.0;
pub const RADIUS_ROW: f32 = 9.0;
pub const RADIUS_SMALL: f32 = 6.0;

/// 应用会话行高。
pub const ROW_HEIGHT: f32 = 44.0;
/// 设备卡片高度（两行：名称 + 音量条）。
pub const DEVICE_ROW_HEIGHT: f32 = 92.0;
pub const APPS_HEADER_HEIGHT: f32 = 28.0;
pub const FOOTER_HEIGHT: f32 = 36.0;
/// 应用列表最多显示的行数，超出后滚动。
pub const MAX_VISIBLE_APPS: usize = 6;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hex_colors_parse_with_and_without_alpha() {
        assert_eq!(parse_color("#7c9cff"), Some(0x7c9cffff));
        assert_eq!(parse_color("7c9cff2e"), Some(0x7c9cff2e));
        assert_eq!(parse_color("#fff"), None);
        assert_eq!(parse_color("nope"), None);
    }

    /// 打底只有深 / 浅两套模板，切回来必须是「干净的样子」——
    /// 叠过自选色之后还能靠 `set_mode` 整个撤掉，不然浅色档会一直停在上一轮的颜色上。
    #[test]
    fn set_mode_resets_the_palette() {
        set_mode(true);
        assert!(is_light(Palette::default().text_primary), "深色模板配浅色字");
        assert_eq!(surface_hex(), to_hex(Palette::default().surface));

        set_mode(false);
        assert!(
            !is_light(Palette::template(true).text_primary),
            "浅色模板配深色字"
        );
        assert_eq!(surface_hex(), to_hex(Palette::template(true).surface));

        // 叠一层自选底色，再切回深色：必须回到模板，而不是留着刚才那层。
        apply_custom("#f4efe6", "", 100);
        assert_eq!(surface_hex(), to_hex(parse_color("#f4efe6").unwrap()));
        set_mode(true);
        assert_eq!(surface_hex(), to_hex(Palette::default().surface));
    }

    /// 强调色传空串 = 跟随系统（调用方负责把系统强调色填进来），
    /// 这里只验「不传就不动」：模板自带的强调色要原样留着。
    #[test]
    fn empty_accent_keeps_the_template_accent() {
        set_mode(true);
        let before = accent_hex();
        apply_custom("", "", 100);
        assert_eq!(accent_hex(), before);
    }

    /// 换了底色，文字必须反过来：浅底配深字、深底配浅字，否则读不出来。
    #[test]
    fn custom_surface_flips_the_text_contrast() {
        let mut palette = Palette::default();
        palette.with_surface(parse_color("#f0f0f0").unwrap());
        assert!(is_light(palette.surface), "选的是浅色底");
        assert!(!is_light(palette.text_primary), "浅色底要配深色字");
        // 三档表面要能拉开层次，不然卡片和背景糊成一片。
        assert_ne!(palette.surface, palette.surface_elevated);
        assert_ne!(palette.surface, palette.surface_sunken);

        palette.with_surface(parse_color("#101014").unwrap());
        assert!(!is_light(palette.surface), "选的是深色底");
        assert!(is_light(palette.text_primary), "深色底要配浅色字");
    }

    /// 自选强调色只换色相，三档透明度保持原样（否则选中态就没层次了）。
    #[test]
    fn custom_accent_keeps_its_alpha_levels() {
        let mut palette = Palette::default();
        palette.with_accent(parse_color("#34d399").unwrap());
        assert_eq!(palette.accent, 0x34d399ff);
        assert_eq!(palette.accent_soft, 0x34d3992e);
        assert_eq!(palette.accent_glow, 0x34d39959);
    }

    #[test]
    fn tint_moves_towards_white_or_black() {
        assert!(tint(0x80808000, 0.2) > 0x80808000);
        assert!(tint(0x80808000, -0.2) < 0x80808000);
        // 已经到头就夹住，不能回绕。
        assert_eq!(tint(0xffffff00, 0.5), 0xffffff00);
        assert_eq!(tint(0x00000000, -0.5), 0x00000000);
    }

    /// 字号缩放要夹在能看的范围内：用户手滑输入 500% 会把窗口撑爆。
    #[test]
    fn font_scale_is_clamped_and_restored() {
        set_font_scale(9.0);
        assert_eq!(font_scale(), 2.0);
        set_font_scale(0.1);
        assert_eq!(font_scale(), 0.6);
        set_font_scale(1.0);
        assert_eq!(font_scale(), 1.0);
    }

}

#[cfg(test)]
mod hex_tests {
    use super::parse_hex;

    /// 色相按 -180..180 存，比较前要绕回 0..360。
    fn hue_of(text: &str) -> f32 {
        let degrees = parse_hex(text).unwrap().color.hue.into_degrees();
        (degrees + 360.0) % 360.0
    }

    /// `#cc3333` 是红：H≈0。R/B 弄反的话会落在 240°（蓝）。
    #[test]
    fn parse_hex_keeps_channel_order() {
        let red = hue_of("#cc3333");
        assert!(red < 20.0 || red > 340.0, "hue={red}");
        let blue = hue_of("#3333cc");
        assert!((blue - 240.0).abs() < 20.0, "hue={blue}");
    }
}
