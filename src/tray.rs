//! 托盘图标：喇叭用系统自带的图标资源，数字版按百分比绘制。

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;

use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use crate::config::{
    MAX_TOOLTIP_CHARS, MAX_TOOLTIP_LINE_CHARS, TooltipConfig, TrayIconStyle, TrayMotion,
};
use crate::i18n::Lang;
use crate::motion::Motion;

/// 画布尺寸的兜底值（取不到系统指标时用）。
const CANVAS_FALLBACK: f32 = 16.0;

/// 数字图标的外观：字体族 + 抗锯齿 + 按位数分档的字号 / 宽高 / 字距。
///
/// 数字是**按目标尺寸直接光栅化**的，所以这些值一改就是那个字体自己的
/// 字形，不存在「大图缩小」那种糊边和毛刺。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NumberStyle {
    pub font: String,
    pub antialias: crate::config::TrayAntialias,
    /// 一位数（0–9）/ 两位数（10–99）/ 三位数（100）各一套。
    pub metrics: [crate::config::TrayDigitMetrics; 3],
}

/// 数字的笔画粗细：托盘上 16px 用 600（半粗）比 400 清楚，旧版默认就是这个。
const DIGIT_WEIGHT: i32 = 600;

impl Default for NumberStyle {
    fn default() -> Self {
        Self {
            font: crate::config::DEFAULT_NUMBER_FONT.to_string(),
            antialias: Default::default(),
            metrics: Default::default(),
        }
    }
}

impl NumberStyle {
    /// 短哈希：托盘图标去重要用，换了字体必须重画。
    fn hash(&self) -> u64 {
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        self.font.to_lowercase().hash(&mut hasher);
        self.antialias.hash(&mut hasher);
        self.metrics.hash(&mut hasher);
        hasher.finish()
    }
}

/// 托盘画布边长 = 任务栏上小图标的实际像素尺寸。
///
/// 照 EarTrumpet 的 `IconHelper.LoadIconForTaskbar`：它取的就是
/// `GetSystemMetricsForDpi(SM_CXSMICON, taskbarDpi)`，也就是通知区域真正显示的
/// 尺寸（本机 96 DPI 下是 16px）。给的就是这个尺寸，系统不用再缩放，边缘才不糊。
fn canvas() -> f32 {
    use std::sync::OnceLock;
    use windows::Win32::UI::HiDpi::{GetDpiForSystem, GetSystemMetricsForDpi};
    use windows::Win32::UI::WindowsAndMessaging::SM_CXSMICON;

    static SIZE: OnceLock<f32> = OnceLock::new();
    *SIZE.get_or_init(|| {
        let dpi = unsafe { GetDpiForSystem() };
        let size = unsafe { GetSystemMetricsForDpi(SM_CXSMICON, dpi) };
        if size > 0 {
            size as f32
        } else {
            CANVAS_FALLBACK
        }
    })
}

/// 画布边长（供配置迁移把旧的像素值换算成百分比用）。
pub(crate) fn canvas_size() -> f32 {
    canvas()
}

/// 系统 DPI（96 / 120 / 144 …）。字号按「点」存，换算成像素要用它。
pub(crate) fn system_dpi() -> u32 {
    use std::sync::OnceLock;
    use windows::Win32::UI::HiDpi::GetDpiForSystem;

    static DPI: OnceLock<u32> = OnceLock::new();
    *DPI.get_or_init(|| {
        let dpi = unsafe { GetDpiForSystem() };
        if dpi > 0 { dpi } else { 96 }
    })
}

pub struct Tray {
    icon: TrayIcon,
    last: Cell<Option<(u32, i32, bool, u8, bool, u32, u64)>>,
    /// 上一次写进系统的提示文字：内容没变就别再调 `Shell_NotifyIcon`。
    last_tip: RefCell<String>,
    /// 动态模式的画面状态（包络 / 节拍）。
    motion: RefCell<Motion>,
}

/// 渲染提示文字要用的实时值（都已经是格式化好、本地化好的字符串）。
///
/// 提示内容是用户自己写的模板，变量由这里提供；把「取值的逻辑」放在
/// 音频泵那边（`gui`），托盘这边就只负责拼字符串，不用认识 `Snapshot`。
#[derive(Clone, Debug, Default)]
pub struct TooltipFacts {
    /// 当前读数：对数模式 `-20.0 dB`，线性模式 `62%`。
    pub reading: String,
    /// 标量百分比（始终是数字，和对数模式无关）。
    pub percent: u32,
    /// 当前响度对应的 dB（静音时 `-∞ dB`）。
    pub db: String,
    /// 当前播放设备的显示名。
    pub device: String,
    /// 设备完整描述（`扬声器 (Realtek(R) Audio)` 这种）。
    pub device_desc: String,
    pub muted: bool,
    /// 正在发声的应用（已去重、按响度排序，最多三个）。
    pub apps: Vec<String>,
    /// 正在发声的应用总数，可能大于 `apps.len()`。
    pub app_count: usize,
    /// 刻度模式（已本地化：百分比 / 对数）。
    pub mode: String,
    /// 设备音量上限（没启用限制则为 `None`）。
    pub cap: Option<u32>,
    pub logarithmic: bool,
}

/// `Tray::update` 的入参：图标外观 + 提示内容。
pub struct TrayUpdate<'a> {
    pub volume: f32,
    pub display: f32,
    pub muted: bool,
    /// 默认播放设备的峰值电平（0..1）。动态模式拿它当输入。
    pub peak: f32,
    pub style: TrayIconStyle,
    pub number: &'a NumberStyle,
    pub logarithmic: bool,
    pub min_db: f32,
    pub lang: Lang,
    /// 用户在设置里写的提示模板。
    pub tooltip: &'a TooltipConfig,
    /// 模板里那些变量此刻的值。
    pub facts: &'a TooltipFacts,
}

impl Tray {
    pub fn new(style: TrayIconStyle, number: &NumberStyle, lang: Lang) -> Result<Self, String> {
        let rgba = render_icon(0.0, 0.0, 0, false, true, style, number);
        let side = canvas() as u32;
        let icon = Icon::from_rgba(rgba, side, side)
            .map_err(|error| format!("创建托盘图标位图失败：{error}"))?;
        let tray = TrayIconBuilder::new()
            .with_tooltip(lang.pick("Bolume 音量", "Bolume Volume"))
            .with_icon(icon)
            .with_menu_on_left_click(false)
            .with_menu_on_right_click(true)
            .build()
            .map_err(|error| format!("创建托盘图标失败：{error}"))?;
        Ok(Self {
            icon: tray,
            last: Cell::new(None),
            last_tip: RefCell::new(String::new()),
            motion: RefCell::new(Motion::default()),
        })
    }

    /// 让下一帧无条件重画。
    ///
    /// 动态模式期间画面是 `animate` 一帧帧推的，`last` 记着的是「进动态之前」
    /// 那一格的音量和样式码；切回静态样式时音量和样式码往往刚好又对得上，
    /// 不清掉就会被当成「没变」跳过，托盘上停着最后一帧动画不动。
    pub fn invalidate(&self) {
        self.last.set(None);
    }

    /// `volume` 是 0..1 的原始幅度（决定喇叭亮几条弧，照 EarTrumpet 的
    /// `UpdateMasterVolumeIcon` 用幅度而不是滑条位置），`display` 是滑条位置 0..1。
    pub fn update(&self, state: &TrayUpdate<'_>) {
        // 动态模式的两条规矩：
        // - 画面由 `animate` 按 60fps 推（这里再画一遍纯属浪费，还会和它抢时间轴）；
        // - 提示文字照旧，设备名 / 应用列表变了必须跟着换。
        if matches!(state.style, TrayIconStyle::Dynamic) {
            // 动画帧不写 `last`：切回静态样式时那一格必须重画，
            // 否则只要音量和样式码跟进动态前一模一样，就会被当成「没变」跳过，
            // 托盘上一直停着最后一帧动画。
            self.last.set(None);
            let text = render_tooltip(state.tooltip, state.facts, state.lang);
            if *self.last_tip.borrow() != text {
                let _ = self.icon.set_tooltip(Some(text.as_str()));
                *self.last_tip.borrow_mut() = text;
            }
            return;
        }
        let volume = state.volume.clamp(0.0, 1.0);
        let display = state.display.clamp(0.0, 1.0);
        let percent = state.facts.percent;
        let db_tenths = if state.logarithmic {
            (crate::audio::display_to_db(display, state.min_db) * 10.0).round() as i32
        } else {
            0
        };
        let style_code = match state.style {
            TrayIconStyle::Number => 0u8,
            TrayIconStyle::Arcs => 1,
            TrayIconStyle::App => 2,
            // 上面已经提前返回了，这里只是让 match 完整。
            TrayIconStyle::Dynamic => 3,
        };
        let bar_id = bar_icon_id(volume, state.muted);
        let key = (
            percent,
            db_tenths,
            state.muted,
            style_code,
            state.logarithmic,
            bar_id,
            state.number.hash(),
        );
        if self.last.get() != Some(key) {
            self.last.set(Some(key));
            let rgba = render_icon(
                volume,
                display,
                percent,
                state.muted,
                light_taskbar(),
                state.style,
                state.number,
            );
            let side = canvas() as u32;
            if let Ok(icon) = Icon::from_rgba(rgba, side, side) {
                let _ = self.icon.set_icon(Some(icon));
            }
        }

        // 提示文字和图标分开判断：设备名、正在发声的应用这些变了，
        // 图标往往一点没动，但提示必须跟着换。
        let text = render_tooltip(state.tooltip, state.facts, state.lang);
        if *self.last_tip.borrow() != text {
            let _ = self.icon.set_tooltip(Some(text.as_str()));
            *self.last_tip.borrow_mut() = text;
        }
    }

    /// 动态模式的一帧：推进时间轴并重绘。
    ///
    /// `gain` 是配置里的灵敏度百分比；`peak` 是默认设备的峰值电平。
    pub fn animate(&self, peak: f32, gain: u32, effect: TrayMotion, muted: bool) {
        let side = canvas() as u32;
        let coverage = self.motion.borrow_mut().step(
            if muted { 0.0 } else { peak },
            gain as f32 / 100.0,
            effect,
            side,
        );
        self.push_motion(&coverage, muted);
    }

    /// 不推进时间轴，按当前状态重画一帧（切样式 / 静音状态变化时用）。
    pub fn repaint_motion(&self, effect: TrayMotion, muted: bool) {
        let side = canvas() as u32;
        let coverage = self.motion.borrow().snapshot(effect, side);
        self.push_motion(&coverage, muted);
    }

    fn push_motion(&self, coverage: &[f32], muted: bool) {
        let side = canvas() as u32;
        if let Ok(icon) =
            Icon::from_rgba(compose_motion(coverage, light_taskbar(), muted), side, side)
        {
            let _ = self.icon.set_icon(Some(icon));
        }
    }

    pub fn set_menu(&self, menu: tray_icon::menu::Menu) {
        self.icon.set_menu(Some(Box::new(menu)));
    }

    /// 摘掉原生右键菜单：「跟随主题」模式下右键由自绘窗口接管。
    ///
    /// 必须真的清空 —— 只要还挂着菜单，右键就会先弹一个系统菜单，
    /// 紧接着自绘菜单再弹一次，两个菜单叠在一起。
    pub fn clear_menu(&self) {
        self.icon.set_menu(None);
    }

    /// 现在就把图标从通知区摘掉。
    ///
    /// 退出 / 重启走的是「摘完图标直接结束进程」这条路，不回到 gpui 的析构链
    /// （那条路在本机收尾时访问违例，见 `gui::quit_now`），所以这里必须显式摘：
    /// 漏了就会在托盘里留一个点不动的幽灵图标，直到鼠标扫过去。
    pub fn remove(&self) {
        if let Err(error) = self.icon.set_visible(false) {
            log::debug!("托盘图标摘除失败：{error}");
        }
    }
}

/// 内置内容（自定义关闭时用）：第一行读数、第二行当前播放设备。
///
/// 和「自定义」第一次打开时预填的模板是同一份 —— 一开一关不该让提示变了样。
pub fn default_template() -> [&'static str; 2] {
    ["{name} {detail}{muted}", "{device}"]
}

/// 按配置渲染悬浮提示。
///
/// 关掉自定义时走内置的两行（读数 + 设备名）；打开时逐行展开变量、
/// 丢掉空行，再用 `\r\n` 拼起来 —— Windows 的托盘提示支持多行，
/// 全部内容受 `NOTIFYICONDATAW.szTip` 的 128 个 UTF-16 单元限制。
pub fn render_tooltip(config: &TooltipConfig, facts: &TooltipFacts, lang: Lang) -> String {
    let templates: Vec<String> = if config.enabled {
        config.active_lines(facts.muted).to_vec()
    } else {
        default_template()
            .iter()
            .map(|line| line.to_string())
            .collect()
    };
    let verbatim = config.align.is_verbatim();
    let mut rows: Vec<AlignRow> = Vec::with_capacity(templates.len());
    for template in templates {
        // 行里的 `{left}` / `{center}` / `{right}` 只决定这一行怎么对齐，本身不上屏。
        let (marker, cleaned) = take_align_marker(&template);
        let expanded = expand_line(&cleaned, facts, lang);
        // 自定义模式下、这行又没写标记：一个字符都不动（首尾空格也算内容）。
        // 其余情况先裁掉模板自带的空白，免得手打的空格把补出来的对齐量算歪。
        let verbatim_row = marker.is_none() && verbatim;
        let text = if verbatim_row {
            expanded
        } else {
            expanded.trim().to_string()
        };
        rows.push(AlignRow {
            text,
            align: marker.unwrap_or(config.align),
            verbatim: verbatim_row,
        });
    }
    join_lines(align_rows(rows))
}

/// 对齐前的一行：文字 + 这一行该怎么对齐。
struct AlignRow {
    text: String,
    align: crate::config::TooltipAlign,
    /// 「原样」行：不补空格、也不会被当成空行丢掉。
    verbatim: bool,
}

/// 把 `{left}` / `{center}` / `{right}` 从一行模板里摘出来。
///
/// 只认这三个词（大小写不敏感，花括号里多几个空格也认）；摘掉之后剩下的文字
/// 照常交给 `expand_line`。`{{center}}` 是字面大括号，要原样留给它去处理。
/// 一行里写了多个时最后一个说了算 —— 用户一般是改主意时又插了一个。
fn take_align_marker(line: &str) -> (Option<crate::config::TooltipAlign>, String) {
    use crate::config::TooltipAlign;
    let mut marker = None;
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch != '{' {
            out.push(ch);
            continue;
        }
        // `{{` 是字面大括号：原样留着，交给 expand_line 收成一个 `{`。
        if chars.peek() == Some(&'{') {
            chars.next();
            out.push_str("{{");
            continue;
        }
        let mut name = String::new();
        let mut closed = false;
        for next in chars.by_ref() {
            if next == '}' {
                closed = true;
                break;
            }
            name.push(next);
        }
        if !closed {
            // 没闭合：原样留着（用户可能还在打字）。
            out.push('{');
            out.push_str(&name);
            continue;
        }
        match name.trim().to_ascii_lowercase().as_str() {
            "left" => marker = Some(TooltipAlign::Left),
            "center" => marker = Some(TooltipAlign::Center),
            "right" => marker = Some(TooltipAlign::Right),
            _ => {
                out.push('{');
                out.push_str(&name);
                out.push('}');
            }
        }
    }
    (marker, out)
}

/// 一行模板展开：`{变量}` 换成实时值，`{{` 是字面大括号。
///
/// 认不出的变量原样留着（`{volumee}`），拼错了能一眼看出来，
/// 而不是悄悄消失、只剩一条空白提示。
/// 解析本身和 OSD 的自定义文字共用一套（`textvars::expand`），两边写法一致。
fn expand_line(line: &str, facts: &TooltipFacts, lang: Lang) -> String {
    crate::textvars::expand(line, |name| tooltip_variable(name, facts, lang))
}

/// 变量表。大小写不敏感，`{Volume}` 和 `{volume}` 一样。
fn tooltip_variable(name: &str, facts: &TooltipFacts, lang: Lang) -> Option<String> {
    let key = name.trim().to_ascii_lowercase();
    Some(match key.as_str() {
        "volume" => facts.reading.clone(),
        // 完整读数：对数模式带百分比（`-20.0 dB · 10%`），线性模式就是百分比。
        "detail" => {
            if facts.logarithmic {
                format!("{} · {}%", facts.reading, facts.percent)
            } else {
                facts.reading.clone()
            }
        }
        // 静音标记：只有静音时才有内容，放在行尾正好。
        "muted" => {
            if facts.muted {
                lang.pick("（静音）", " (muted)").to_string()
            } else {
                String::new()
            }
        }
        "percent" => format!("{}%", facts.percent),
        "db" => facts.db.clone(),
        "device" => facts.device.clone(),
        "device_desc" => facts.device_desc.clone(),
        "state" => if facts.muted {
            lang.pick("静音", "Muted")
        } else {
            lang.pick("正常", "Normal")
        }
        .to_string(),
        "apps" => facts.apps.join(", "),
        "app_count" => facts.app_count.to_string(),
        "mode" => facts.mode.clone(),
        "cap" => facts
            .cap
            .map(|percent| format!("{percent}%"))
            .unwrap_or_default(),
        "time" => now_hm(),
        "name" => lang.pick("Bolume 音量", "Bolume Volume").to_string(),
        _ => return None,
    })
}

/// 当前时间 `HH:MM`（本地时区，直接问系统，不自己算时区）。
fn now_hm() -> String {
    use windows::Win32::System::SystemInformation::GetLocalTime;
    let time = unsafe { GetLocalTime() };
    format!("{:02}:{:02}", time.wHour, time.wMinute)
}

/// 按每行的对齐方式补前导空格。
///
/// 系统提示框的文字固定左对齐、也没有对齐参数，所以「居中 / 靠右」只能是算出来的：
/// 取最宽的一行，居中垫 `(最宽 - 本行) / 2` 个空格、靠右垫 `最宽 - 本行` 个。
/// 宽度按显示格数估（全角字符算 2 格、其余算 1 格）；提示框是比例字体，空格比汉字窄，
/// 所以这是近似值 —— 两三行的短提示看起来是正的，长中英混排会有点偏。
///
/// 「原样」行（自定义模式下没写对齐标记的行）一格都不垫，也不参与「最宽」的统计：
/// 不然一条很长的原样行会把别的行推得老远。
///
/// 补出来的空格同样占 `szTip` 的额度，所以要按总额度收着补：宁可少垫一格，
/// 也不能把行尾的内容挤成省略号。
fn align_rows(rows: Vec<AlignRow>) -> Vec<String> {
    use crate::config::TooltipAlign;
    let widths: Vec<usize> = rows.iter().map(|row| display_width(&row.text)).collect();
    let widest = rows
        .iter()
        .zip(&widths)
        .filter(|(row, _)| !row.verbatim)
        .map(|(_, width)| *width)
        .max()
        .unwrap_or(0);

    let mut pads: Vec<usize> = rows
        .iter()
        .zip(&widths)
        .map(|(row, width)| {
            if row.verbatim {
                return 0;
            }
            let want = match row.align {
                TooltipAlign::Center => widest.saturating_sub(*width) / 2,
                TooltipAlign::Right => widest.saturating_sub(*width),
                _ => 0,
            };
            // 单行也有 64 个字符的硬上限：垫完超了就只垫到上限为止。
            want.min(MAX_TOOLTIP_LINE_CHARS.saturating_sub(row.text.chars().count()))
        })
        .collect();

    // 整条提示（含换行）最多 127 个单元：从垫得最多的那几行开始往下扣。
    let kept = |row: &AlignRow| row.verbatim || !row.text.trim().is_empty();
    let seps = rows
        .iter()
        .filter(|row| kept(row))
        .count()
        .saturating_sub(1);
    let budget = MAX_TOOLTIP_CHARS.saturating_sub(seps);
    let mut total: usize = rows
        .iter()
        .zip(&pads)
        .filter(|(row, _)| kept(row))
        .map(|(row, pad)| row.text.chars().count() + pad)
        .sum();
    while total > budget {
        let Some(slot) = pads.iter_mut().max_by_key(|pad| **pad) else {
            break;
        };
        if *slot == 0 {
            break;
        }
        *slot -= 1;
        total -= 1;
    }

    rows.into_iter()
        .zip(pads)
        .filter(|(row, _)| kept(row))
        .map(|(row, pad)| {
            if pad == 0 {
                row.text
            } else {
                format!("{}{}", " ".repeat(pad), row.text)
            }
        })
        .collect()
}

/// 估算一段文字在提示框里占几格：全角字符 2 格，其余 1 格。
fn display_width(text: &str) -> usize {
    text.chars()
        .map(|ch| if is_wide_char(ch) { 2 } else { 1 })
        .sum()
}

/// 中日韩文字与全角标点（够用来估提示框的宽度，不需要 Unicode 全表）。
fn is_wide_char(ch: char) -> bool {
    matches!(
        ch as u32,
        0x1100..=0x115F
            | 0x2E80..=0x303E
            | 0x3041..=0x33FF
            | 0x3400..=0x4DBF
            | 0x4E00..=0x9FFF
            | 0xA000..=0xA4CF
            | 0xAC00..=0xD7A3
            | 0xF900..=0xFAFF
            | 0xFE30..=0xFE6F
            | 0xFF00..=0xFF60
            | 0xFFE0..=0xFFE6
            | 0x1F300..=0x1FAFF
    )
}

/// 逐行用 `\r\n` 拼成最终提示。
///
/// 这里不再 trim —— 对齐补上去的前导空格、自定义模式下的原样空格都是内容的一部分
/// （模板自带的空白在 `render_tooltip` 里已经收拾过了）。
fn join_lines(lines: Vec<String>) -> String {
    let mut out = String::new();
    for (index, line) in lines.iter().enumerate() {
        if index > 0 {
            out.push_str("\r\n");
        }
        out.push_str(&clamp_chars(line, MAX_TOOLTIP_LINE_CHARS));
    }
    clamp_utf16(&out, MAX_TOOLTIP_CHARS)
}

/// 按字符数截断，超了用省略号收尾。
fn clamp_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max.saturating_sub(1)).collect();
    out.push('…');
    out
}

/// 按 UTF-16 单元截断 —— `szTip` 的额度就是按 UTF-16 算的，
/// 中文占 1 个、emoji 占 2 个，按字节截会把字劈成两半。
fn clamp_utf16(text: &str, max: usize) -> String {
    let mut units = 0;
    let mut out = String::new();
    for ch in text.chars() {
        let width = ch.len_utf16();
        if units + width > max {
            // 被截掉了：收一个省略号，它自己也占 1 个单元。
            if out.chars().count() > 0 {
                out.pop();
            }
            out.push('…');
            return out;
        }
        units += width;
        out.push(ch);
    }
    out
}

/// 任务栏是否为浅色主题（决定图标用深色还是浅色）。
fn light_taskbar() -> bool {
    use windows::Win32::System::Registry::{HKEY_CURRENT_USER, RRF_RT_REG_DWORD, RegGetValueW};
    use windows::core::PCWSTR;

    let path: Vec<u16> = "Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let value: Vec<u16> = "SystemUsesLightTheme"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    let mut data: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(path.as_ptr()),
            PCWSTR(value.as_ptr()),
            RRF_RT_REG_DWORD,
            None,
            Some(&mut data as *mut _ as *mut _),
            Some(&mut size),
        )
    };
    status.is_ok() && data == 1
}

/// 生成托盘图标的 RGBA（straight alpha，托盘按直通 alpha 处理）。
fn render_icon(
    volume: f32,
    display: f32,
    percent: u32,
    muted: bool,
    dark_ink: bool,
    style: TrayIconStyle,
    number: &NumberStyle,
) -> Vec<u8> {
    match style {
        TrayIconStyle::Number => render_number_icon(percent, muted, dark_ink, number),
        TrayIconStyle::Arcs => {
            render_system_icon(volume, display, muted, dark_ink, percent, number)
        }
        TrayIconStyle::App => render_app_icon(percent, muted, dark_ink, number),
        // 动态模式由 `Tray::animate` 驱动；这里只画一张静止的空场，供启动时先占位。
        TrayIconStyle::Dynamic => {
            let side = canvas() as u32;
            let coverage = crate::motion::Motion::default().snapshot(TrayMotion::Bars, side);
            compose_motion(&coverage, !dark_ink, muted)
        }
    }
}

/// 把律动的覆盖率图变成托盘要的 RGBA：统一一层墨色，透明度照覆盖率。
///
/// 不做彩色：任务栏上的小图标一旦上了色就在一堆黑白图标里出头，
/// 律动的「雅」靠形状和节奏，不靠颜色。
fn compose_motion(coverage: &[f32], dark_ink: bool, muted: bool) -> Vec<u8> {
    let side = canvas() as usize;
    let mut buffer = vec![0u8; side * side * 4];
    let ink = if dark_ink { 20 } else { 248 };
    let scale = if muted { 0.45 } else { 1.0 };
    for (index, coverage) in coverage.iter().take(side * side).enumerate() {
        let alpha = (coverage * scale).clamp(0.0, 1.0);
        if alpha <= 0.004 {
            continue;
        }
        let offset = index * 4;
        buffer[offset] = ink;
        buffer[offset + 1] = ink;
        buffer[offset + 2] = ink;
        buffer[offset + 3] = (alpha * 255.0).round() as u8;
    }
    buffer
}

/// 软件图标：直接用编进 exe 的那张 `assets/Bolume.ico`（取托盘尺寸对应的那一档）。
///
/// 取不到就退回数字图标——托盘上不能出现一个空白格子。
fn render_app_icon(percent: u32, muted: bool, dark_ink: bool, number: &NumberStyle) -> Vec<u8> {
    let side = canvas() as u32;
    let Some(mut rgba) = crate::platform::app_icon::rgba_at(side) else {
        return render_number_icon(percent, muted, dark_ink, number);
    };
    if muted {
        // 静音时压一道透明度，和系统的喇叭静音图标观感一致。
        for pixel in rgba.chunks_exact_mut(4) {
            pixel[3] = (pixel[3] as f32 * 0.45).round() as u8;
        }
    }
    rgba
}

/// 喇叭图标：直接从 `SndVolSSO.dll` 取系统资源（EarTrumpet 用的同一批）。
///
/// 取不到（非 Windows / dll 被裁剪）时退回数字图标，功能不受影响。
fn render_system_icon(
    volume: f32,
    _display: f32,
    muted: bool,
    dark_ink: bool,
    percent: u32,
    number: &NumberStyle,
) -> Vec<u8> {
    let Some(mut rgba) =
        crate::platform::sound_icons::icon_rgba(bar_icon_id(volume, muted), canvas() as u32)
    else {
        return render_number_icon(percent, muted, dark_ink, number);
    };
    if dark_ink {
        // 浅色任务栏：系统图标是白色的，照 EarTrumpet 的做法染成黑色。
        crate::platform::sound_icons::tint(&mut rgba, [0, 0, 0]);
    }
    rgba
}

/// 音量对应的系统图标资源 id。
///
/// 阈值照抄 EarTrumpet `DeviceViewModel.UpdateMasterVolumeIcon` 的 Windows 11 分支：
/// `> 0.66` = 3 条弧、`> 0.33` = 2 条、`> 0` = 1 条、`0` = 0 条；静音单独一个图标。
/// 注意它用的是**幅度**（`device.Volume`），不是对数滑条的位置。
fn bar_icon_id(volume: f32, muted: bool) -> u32 {
    use crate::platform::sound_icons::*;
    if muted {
        return ID_MUTED;
    }
    if volume > 0.66 {
        ID_SPEAKER_THREE_BARS
    } else if volume > 0.33 {
        ID_SPEAKER_TWO_BARS
    } else if volume > 0.0 {
        ID_SPEAKER_ONE_BAR
    } else {
        ID_SPEAKER_ZERO_BARS
    }
}

/// 一段文字的光栅化结果：已裁到墨迹包围盒，灰度值 0..1。
struct TextMask {
    width: usize,
    height: usize,
    coverage: Vec<f32>,
}

thread_local! {
    /// 百分比只有 0..100 这 101 种／每种字号与字体各一份，缓存后拖动滑条不用反复光栅化。
    static MASK_CACHE: RefCell<HashMap<String, Rc<TextMask>>> = RefCell::new(HashMap::new());
    /// 每种字体/字重下「88 的墨迹高度 ÷ 字号」的比值，用来反推需要多大字号。
    static INK_RATIO: RefCell<HashMap<String, f32>> = RefCell::new(HashMap::new());
    /// 单字形（4 倍尺寸）的光栅化结果，同一个字永远拿同一份。
    static GLYPH_CACHE: RefCell<HashMap<String, Rc<Glyph>>> = RefCell::new(HashMap::new());
}

/// 按 (字体, 字重, 字号, 抗锯齿) 取一份已经裁到墨迹包围盒的蒙版。
///
/// 关键点：字号就是**最终显示的字号**，画出来多高就是多高，后面不再缩放。
fn sized_mask(
    family: &str,
    weight: i32,
    font_px: f32,
    text: &str,
    aa: crate::config::TrayAntialias,
) -> Rc<TextMask> {
    let key = format!(
        "{family}\u{1}{weight}\u{1}{font_px:.2}\u{1}{}\u{1}{text}",
        aa.index()
    );
    if let Some(cached) = MASK_CACHE.with(|cache| cache.borrow().get(&key).cloned()) {
        return cached;
    }
    let mask = Rc::new(rasterize_with(family, weight, text, font_px, aa));
    MASK_CACHE.with(|cache| {
        cache.borrow_mut().insert(key, mask.clone());
    });
    mask
}

/// 「88 的墨迹高度 ÷ 字号」——换字号时字高近似线性变化，量一次就能反推。
fn ink_ratio(family: &str, weight: i32, aa: crate::config::TrayAntialias) -> f32 {
    let key = format!("{family}\u{1}{weight}\u{1}{}", aa.index());
    if let Some(cached) = INK_RATIO.with(|cache| cache.borrow().get(&key).copied()) {
        return cached;
    }
    const PROBE: f32 = 96.0;
    let mask = sized_mask(family, weight, PROBE, "88", aa);
    let ratio = if mask.height == 0 {
        0.72
    } else {
        mask.height as f32 / PROBE
    };
    INK_RATIO.with(|cache| cache.borrow_mut().insert(key, ratio));
    ratio
}

/// 「只要数字」的托盘图标。
///
/// 数字本身是**用字体直接画出来的**（没有贴图、没有裁切图标资源）：
/// 先用「88」量出这个字体的字高比例，反推出目标高度对应的字号，
/// 再按 4 倍超采样光栅化、按**整数倍**盒式降采样后 1:1 贴进画布。
///
/// 字高不再被宽度牵着走：两位数在 16px 的画布上本来放不下那么高
/// （「88」的墨迹宽高比约 1.4），以前只能靠收字号来适配宽度，于是
/// 数字高度设到 10px 以上就没效果了。现在改为**横向压窄**字形 ——
/// 高度照设，宽度压到盒子里；压得过头（会瘦成一条）才回头收字号。
fn render_number_icon(percent: u32, muted: bool, dark_ink: bool, style: &NumberStyle) -> Vec<u8> {
    let text = percent.min(100).to_string();
    let family = if style.font.trim().is_empty() {
        crate::config::DEFAULT_NUMBER_FONT
    } else {
        style.font.as_str()
    };
    let weight = DIGIT_WEIGHT;
    let aa = style.antialias;
    let ink = if dark_ink { 20.0 } else { 248.0 };
    let alpha_scale = if muted { 0.4 } else { 1.0 };

    let canvas = canvas();
    // 四周各留 1px：贴边的话任务栏会把边缘像素吃掉。
    let box_side = (canvas - 2.0).max(4.0);

    // 按位数挑这一档的参数：0–9 一位、10–99 两位、100 三位。
    let digits = text.chars().count().clamp(1, 3);
    let metrics = style.metrics[digits - 1];
    let ratio = ink_ratio(family, weight, aa).max(0.05);
    let (sx, sy) = (
        (metrics.width.clamp(10, 300) as f32) / 100.0,
        (metrics.height.clamp(10, 300) as f32) / 100.0,
    );
    // 字距：PS 的 VA 值单位是 1/1000 em，换算成像素要乘字号。
    let tracking = metrics.tracking.clamp(-500, 1000) as f32 / 1000.0;

    // 字号：0 = 自动（墨迹高度撑到画布的 `AUTO_HEIGHT_RATIO`，旧版观感）；
    // 否则照「点 × DPI / 72」换算成像素 —— 12 点在 96 DPI 下就是 16px。
    let auto = metrics.size_pt == 0;
    let font_px = if auto {
        let target = (box_side * AUTO_HEIGHT_RATIO).clamp(4.0, box_side);
        snap_font_px(family, weight, target / ratio, aa)
    } else {
        // 用户填的点数照用；只挡一下「填了个比画布大几十倍的数字」——
        // 那种字号光栅化出来几十兆，而画出来也就是整块图标全糊。
        (metrics.size_pt.clamp(1, 200) as f32 * system_dpi() as f32 / 72.0)
            .min(canvas * 4.0)
    };

    // 光栅化 → 按用户设的水平 / 垂直缩放重采样（100% 时原样返回）。
    let seed = low_res(family, weight, font_px, &text, aa, font_px * tracking);
    let mut mask = if (sx - 1.0).abs() > 0.01 || (sy - 1.0).abs() > 0.01 {
        Rc::new(resample(&seed, sx, sy))
    } else {
        seed
    };
    // 自动档的字号是按「高度 70% 画布」反推的，两位数 / 三位数注定比盒子宽 ——
    // 这里只**横向压窄**，高度一点不动。
    // 手动档一个字都不改：填多少就是多少，超出画布的部分由 `compose_native`
    // 居中裁掉。以前这里会「压不动就收字号再画」，于是同一个 15 点下
    // 三位数比两位数矮一截（用户看到的就是「两位数三位数高度不一样」）。
    if auto && mask.width > 0 && mask.width as f32 > box_side {
        mask = Rc::new(squeeze(&mask, box_side / mask.width as f32));
    }
    // 「无」这一档要**最后**再阈一次：4 倍图上已经是硬边，降采样（以及压窄时的
    // 面积平均）又会给边缘生出灰阶。二值化之后才是真的硬边。
    if aa.threshold() {
        mask = Rc::new(harden(&mask));
    }
    compose_native(&mask, ink, alpha_scale)
}

/// 覆盖率二值化（0 或 1）：「无」抗锯齿那一档用。
///
/// 阈值取 0.4 而不是 0.5：半格覆盖的边缘行本来就在墨迹包围盒里，
/// 按 0.5 砍会把边缘整行吃掉。
///
/// 还要再补一道「行保底」：`1` 的笔画末端在最后一行往往只有三成覆盖，
/// 二值化后那一行全空 —— 同一个字号下「1」就比「0」矮一像素
/// （用户看到的就是「两位数和三位数高度不一样」）。整行原本有墨却被砍光时，
/// 保留这一行最强的那一格，高度就对齐了。
fn harden(mask: &TextMask) -> TextMask {
    let (width, height) = (mask.width, mask.height);
    let mut coverage: Vec<f32> = mask
        .coverage
        .iter()
        .map(|value| if *value >= 0.4 { 1.0 } else { 0.0 })
        .collect();
    for y in 0..height {
        let row = y * width;
        let mut inked = false;
        let mut kept = false;
        let mut best = (0usize, 0.0f32);
        for x in 0..width {
            let raw = mask.coverage[row + x];
            // 阈值压得极低：只要这一行原本沾过墨，就不该被二值化整行吃掉。
            if raw > 0.004 {
                inked = true;
            }
            if coverage[row + x] > 0.5 {
                kept = true;
            }
            if raw > best.1 {
                best = (x, raw);
            }
        }
        if inked && !kept {
            coverage[row + best.0] = 1.0;
        }
    }
    TextMask {
        width,
        height,
        coverage,
    }
}

/// 宽度 / 高度缩放（PS 的水平 / 垂直缩放）。
///
/// 缩小按面积平均（网格对齐、边缘不闪），放大按双线性（比最近邻平滑）。
/// 只在比例不等于 100% 时调用；100% 原样返回，保证默认那一路和以前逐像素一致。
fn resample(mask: &TextMask, sx: f32, sy: f32) -> TextMask {
    if mask.width == 0 || mask.height == 0 {
        return TextMask {
            width: 0,
            height: 0,
            coverage: Vec::new(),
        };
    }
    let out_w = ((mask.width as f32 * sx).round() as usize).clamp(1, 1024);
    let out_h = ((mask.height as f32 * sy).round() as usize).clamp(1, 1024);
    if out_w == mask.width && out_h == mask.height {
        return TextMask {
            width: mask.width,
            height: mask.height,
            coverage: mask.coverage.clone(),
        };
    }
    let step_x = mask.width as f32 / out_w as f32;
    let step_y = mask.height as f32 / out_h as f32;
    let mut coverage = vec![0f32; out_w * out_h];
    let sample = |x: usize, y: usize| mask.coverage[y * mask.width + x];
    if step_x >= 1.0 && step_y >= 1.0 {
        // 缩小：盒式面积平均。
        for oy in 0..out_h {
            let y0 = oy as f32 * step_y;
            let y1 = y0 + step_y;
            for ox in 0..out_w {
                let x0 = ox as f32 * step_x;
                let x1 = x0 + step_x;
                let mut sum = 0f32;
                let mut area = 0f32;
                let mut y = y0.floor() as usize;
                while (y as f32) < y1 && y < mask.height {
                    let top = (y as f32).max(y0);
                    let bottom = ((y + 1) as f32).min(y1);
                    let dy = (bottom - top).max(0.0);
                    let mut x = x0.floor() as usize;
                    while (x as f32) < x1 && x < mask.width {
                        let left = (x as f32).max(x0);
                        let right = ((x + 1) as f32).min(x1);
                        let dx = (right - left).max(0.0);
                        sum += sample(x, y) * dx * dy;
                        area += dx * dy;
                        x += 1;
                    }
                    y += 1;
                }
                coverage[oy * out_w + ox] = if area > 0.0 {
                    (sum / area).clamp(0.0, 1.0)
                } else {
                    0.0
                };
            }
        }
    } else {
        // 放大：双线性（中心对中心）。
        for oy in 0..out_h {
            let v = ((oy as f32 + 0.5) * step_y - 0.5).clamp(0.0, (mask.height - 1) as f32);
            let y0 = v.floor() as usize;
            let y1 = (y0 + 1).min(mask.height - 1);
            let fy = v - y0 as f32;
            for ox in 0..out_w {
                let u = ((ox as f32 + 0.5) * step_x - 0.5).clamp(0.0, (mask.width - 1) as f32);
                let x0 = u.floor() as usize;
                let x1 = (x0 + 1).min(mask.width - 1);
                let fx = u - x0 as f32;
                let top = sample(x0, y0) * (1.0 - fx) + sample(x1, y0) * fx;
                let bottom = sample(x0, y1) * (1.0 - fx) + sample(x1, y1) * fx;
                coverage[oy * out_w + ox] = (top * (1.0 - fy) + bottom * fy).clamp(0.0, 1.0);
            }
        }
    }
    TextMask {
        width: out_w,
        height: out_h,
        coverage,
    }
}

/// 横向盒式缩放：只改宽度，高度和纵向灰度分布原样保留。
///
/// 只给「自动字号」用：自动档的字号按高度反推，两位数 / 三位数必然超宽，
/// 压窄（而不是收字号）才能让同一档里的数字高度一致。
/// 手动档不做任何压窄 —— 用户填的宽度、高度、字号就是最终结果。
fn squeeze(mask: &TextMask, scale: f32) -> TextMask {
    let src_w = mask.width;
    if src_w == 0 || mask.height == 0 || scale >= 0.999 {
        return TextMask {
            width: mask.width,
            height: mask.height,
            coverage: mask.coverage.clone(),
        };
    }
    let out_w = ((src_w as f32 * scale).round() as usize).clamp(1, src_w);
    let step = src_w as f32 / out_w as f32;
    let mut coverage = vec![0f32; out_w * mask.height];
    for y in 0..mask.height {
        let row = y * src_w;
        for ox in 0..out_w {
            let start = ox as f32 * step;
            let end = start + step;
            let mut sum = 0f32;
            let mut piece = start.floor() as usize;
            while piece < src_w {
                let piece_start = piece as f32;
                if piece_start >= end {
                    break;
                }
                let left = piece_start.max(start);
                let right = ((piece + 1) as f32).min(end);
                let weight = (right - left).max(0.0);
                sum += mask.coverage[row + piece] * weight;
                piece += 1;
            }
            coverage[y * out_w + ox] = (sum / step).clamp(0.0, 1.0);
        }
    }
    TextMask {
        width: out_w,
        height: mask.height,
        coverage,
    }
}

/// 在 `font_px` 附近找一个「数字墨迹高度是 4 的整数倍」的字号。
///
/// 4 是超采样倍数：高度对齐到 4 的倍数，降采样后每一行都是满格覆盖，
/// 上下边缘一样实。**只往小里找**——往大调会让两位数字超出盒子宽度，
/// 又触发按值缩字号，前功尽弃。
fn snap_font_px(
    family: &str,
    weight: i32,
    font_px: f32,
    aa: crate::config::TrayAntialias,
) -> f32 {
    let key = format!(
        "snap\u{1}{family}\u{1}{weight}\u{1}{}\u{1}{font_px:.2}",
        aa.index()
    );
    if let Some(cached) = INK_RATIO.with(|cache| cache.borrow().get(&key).copied()) {
        return cached;
    }
    let factor = SUPERSAMPLE as usize;
    let snapped = (0..=15)
        .map(|step| font_px - step as f32 * 0.1)
        .find(|candidate| {
            *candidate >= 4.0 && {
                let probe = glyph(family, weight, *candidate * SUPERSAMPLE as f32, '8', aa);
                probe.mask.height % factor == 0 && probe.mask.height > 0
            }
        })
        .unwrap_or(font_px);
    INK_RATIO.with(|cache| cache.borrow_mut().insert(key, snapped));
    snapped
}

/// 超采样倍数：先在 S 倍尺寸上光栅化，再按整数倍盒式降下来。
const SUPERSAMPLE: u32 = 4;

/// 自动大小时的「数字墨迹高度 / 画布」比例。
///
/// 0.70 沿用旧版观感：一位数 / 两位数高度完全一致，两位数的宽度压到
/// 画布盒子以内（见 `squeeze`），不会贴边。
const AUTO_HEIGHT_RATIO: f32 = 0.70;

/// 高分辨率光栅化 → 整数倍降采样，得到最终分辨率的蒙版。
///
/// 按**单个字形**光栅化再拼起来，原因在 `rasterize_glyph` 的注释里：
/// 整串一起画的话，第二个字会落在非整数的笔位上，降采样后相位不同 ——
/// 「11」左边那一笔和右边那一笔会长得完全不一样。
fn low_res(
    family: &str,
    weight: i32,
    font_px: f32,
    text: &str,
    aa: crate::config::TrayAntialias,
    tracking_px: f32,
) -> Rc<TextMask> {
    let key = format!(
        "lo\u{1}{family}\u{1}{weight}\u{1}{font_px:.2}\u{1}{}\u{1}{tracking_px:.2}\u{1}{text}",
        aa.index()
    );
    if let Some(cached) = MASK_CACHE.with(|cache| cache.borrow().get(&key).cloned()) {
        return cached;
    }
    let px4 = font_px * SUPERSAMPLE as f32;
    let glyphs: Vec<Rc<Glyph>> = text
        .chars()
        .map(|ch| glyph(family, weight, px4, ch, aa))
        .collect();
    let frame = digit_frame(family, weight, px4, aa);
    let mask = Rc::new(compose_glyphs(
        &glyphs,
        tracking_px * SUPERSAMPLE as f32,
        frame,
    ));
    MASK_CACHE.with(|cache| {
        cache.borrow_mut().insert(key, mask.clone());
    });
    mask
}

/// 单个字形在 4 倍尺寸下的光栅化结果。
struct Glyph {
    /// 已裁到墨迹包围盒的覆盖率。
    mask: TextMask,
    /// 墨迹左边界相对笔位的距离（4 倍像素）。
    x0: f32,
    /// 墨迹上边界相对画布顶部的距离（4 倍像素，同一字体下所有数字一致）。
    y0: f32,
    /// 前进宽度（4 倍像素）。
    advance: f32,
}

fn glyph(
    family: &str,
    weight: i32,
    font_px4: f32,
    ch: char,
    aa: crate::config::TrayAntialias,
) -> Rc<Glyph> {
    let key = format!(
        "g\u{1}{family}\u{1}{weight}\u{1}{font_px4:.2}\u{1}{}\u{1}{ch}",
        aa.index()
    );
    if let Some(cached) = GLYPH_CACHE.with(|cache| cache.borrow().get(&key).cloned()) {
        return cached;
    }
    let glyph = Rc::new(rasterize_glyph(family, weight, ch, font_px4, aa));
    GLYPH_CACHE.with(|cache| {
        cache.borrow_mut().insert(key, glyph.clone());
    });
    glyph
}

/// 同一字体 / 字重 / 字号 / 抗锯齿下，所有阿拉伯数字共用的墨迹高度（4 倍像素）。
///
/// 圆头数字（0 6 8 9）顶部有 overshoot，比「1」多占一行；只按当前这一串的
/// 墨迹算行高的话，「11」会比「10」矮一行 —— 同一个两位数在数值变化时整串
/// 上下跳一像素，看着就是「一高一低」。取 0–9 的并集当统一行高就没有这个问题。
fn digit_frame(
    family: &str,
    weight: i32,
    font_px4: f32,
    aa: crate::config::TrayAntialias,
) -> f32 {
    let key = format!(
        "frame\u{1}{family}\u{1}{weight}\u{1}{font_px4:.2}\u{1}{}",
        aa.index()
    );
    if let Some(cached) = INK_RATIO.with(|cache| cache.borrow().get(&key).copied()) {
        return cached;
    }
    let mut top = f32::MAX;
    let mut bottom = f32::MIN;
    for ch in '0'..='9' {
        let glyph = glyph(family, weight, font_px4, ch, aa);
        if glyph.mask.height == 0 {
            continue;
        }
        top = top.min(glyph.y0);
        bottom = bottom.max(glyph.y0 + glyph.mask.height as f32);
    }
    let height = if bottom > top && top.is_finite() {
        bottom - top
    } else {
        0.0
    };
    INK_RATIO.with(|cache| cache.borrow_mut().insert(key, height));
    height
}

/// 把几个字形拼成一行：每个字形先从**自己的**包围盒降采样，再按整数像素落位。
///
/// 这样同一个字在任何数值里都是同一张位图（缓存里拿的是同一份），
/// 「11」的两个 1、「10」的 1、单独的「1」必然长得一模一样。
///
/// `tracking_px` 是字距（4 倍像素域，可为负）：累加到每个字的前进宽度上，
/// 语义照 Photoshop 的 VA（1/1000 em，由调用方换算好）。
/// `frame_h` 是这一档数字**共用的行高**（4 倍像素，见 `digit_frame`）：
/// 行高必须按它来，不能按这一串自己的墨迹算 —— 不然「11」（全是 1）比
/// 「10」（0 顶部有 overshoot）矮一行，同一个两位数在数值变化时整串会上下跳。
fn compose_glyphs(glyphs: &[Rc<Glyph>], tracking_px: f32, frame_h: f32) -> TextMask {
    let factor = SUPERSAMPLE as f32;
    if glyphs.is_empty() {
        return TextMask {
            width: 0,
            height: 0,
            coverage: Vec::new(),
        };
    }
    let lows: Vec<TextMask> = glyphs
        .iter()
        .map(|glyph| downsample(&glyph.mask, SUPERSAMPLE))
        .collect();
    let tops: Vec<i32> = glyphs
        .iter()
        .map(|glyph| (glyph.y0 / factor).round() as i32)
        .collect();
    // 按**基线**对齐（不是按顶）：GDI 在 4 倍尺寸上也会做网格拟合，
    // 个别数字的墨迹高度会差 1 像素；按顶对齐的话它就比别的字矮一截、还坐着高一点。
    let baseline = tops
        .iter()
        .zip(&lows)
        .map(|(top, low)| top + low.height as i32)
        .max()
        .unwrap_or(0);
    // 这一串自己的墨迹高度；再和「所有数字共用的行高」取大的那个。
    let own = (baseline - tops.iter().copied().min().unwrap_or(0)).max(0) as usize;
    let frame = (frame_h / factor).round().max(0.0) as usize;
    let height = own.max(frame);
    // 框底对齐：数字没有下沉部，baseline 就是框底，多出来的高度补到上面去。
    let pad_top = height as i32 - baseline;

    let mut cursor = 0f32;
    let mut placed: Vec<(usize, usize, TextMask)> = Vec::with_capacity(glyphs.len());
    let mut width = 0usize;
    for (index, (glyph, low)) in glyphs.iter().zip(lows).enumerate() {
        let x = (((cursor + glyph.x0) / factor).round().max(0.0)) as usize;
        let y = (pad_top + (baseline - (tops[index] + low.height as i32))).max(0) as usize;
        width = width.max(x + low.width);
        placed.push((x, y, low));
        cursor += glyph.advance + tracking_px;
    }

    let mut coverage = vec![0f32; width * height];
    for (x, y, low) in placed {
        for gy in 0..low.height {
            let row = (y + gy) * width + x;
            for gx in 0..low.width {
                let value = low.coverage[gy * low.width + gx];
                if value > coverage[row + gx] {
                    coverage[row + gx] = value;
                }
            }
        }
    }
    TextMask {
        width,
        height,
        coverage,
    }
}

/// 按整数倍做盒式降采样。
///
/// 边缘不满一格时**按整格面积平均**（缺少的样本按 0 算）：
/// 那半个格子本来就只盖住一部分像素，除以样本数会把它抬到满格黑，
/// 右边就多出一条本不存在的竖线 —— 9 / 0 那个「小凸起」就是这么来的。
fn downsample(mask: &TextMask, factor: u32) -> TextMask {
    if factor <= 1 || mask.width == 0 || mask.height == 0 {
        return TextMask {
            width: mask.width,
            height: mask.height,
            coverage: mask.coverage.clone(),
        };
    }
    let factor = factor as usize;
    let out_w = mask.width.div_ceil(factor);
    let out_h = mask.height.div_ceil(factor);
    let area = (factor * factor) as f32;
    let mut coverage = vec![0f32; out_w * out_h];
    for oy in 0..out_h {
        for ox in 0..out_w {
            let mut sum = 0f32;
            for sy in 0..factor {
                let y = oy * factor + sy;
                if y >= mask.height {
                    break;
                }
                for sx in 0..factor {
                    let x = ox * factor + sx;
                    if x >= mask.width {
                        break;
                    }
                    sum += mask.coverage[y * mask.width + x];
                }
            }
            coverage[oy * out_w + ox] = sum / area;
        }
    }
    TextMask {
        width: out_w,
        height: out_h,
        coverage,
    }
}

/// 把蒙版 1:1 贴进画布（居中），不缩放。
///
/// 蒙版比画布大时**居中裁**（左右 / 上下各切掉一半）：用户把字号调得比画布还大
/// 是他的自由，这里不偷偷缩小字形，只是画不下的部分自然落在图标外面。
fn compose_native(mask: &TextMask, ink: f32, alpha_scale: f32) -> Vec<u8> {
    let canvas = canvas();
    let size = canvas as usize;
    let mut buffer = vec![0u8; size * size * 4];
    if mask.width == 0 || mask.height == 0 {
        return buffer;
    }

    let ox = (size as i32 - mask.width as i32) / 2;
    let oy = (size as i32 - mask.height as i32) / 2;

    for y in 0..mask.height {
        let ty = oy + y as i32;
        if ty < 0 || ty >= size as i32 {
            continue;
        }
        for x in 0..mask.width {
            let tx = ox + x as i32;
            if tx < 0 || tx >= size as i32 {
                continue;
            }
            let alpha = mask.coverage[y * mask.width + x] * alpha_scale;
            if alpha <= 0.004 {
                continue;
            }
            let offset = (ty as usize * size + tx as usize) * 4;
            buffer[offset] = ink as u8;
            buffer[offset + 1] = ink as u8;
            buffer[offset + 2] = ink as u8;
            buffer[offset + 3] = (alpha * 255.0).round() as u8;
        }
    }
    buffer
}

/// 光栅化用的中间画布：比目标大一圈，保证 DT_CENTER 画的字形完整落在里面。
///
/// 尺寸只跟字号和字数有关，跟最终画布无关——画出来什么分辨率就是什么分辨率，
/// 后面直接 1:1 贴进托盘画布，所以不存在缩放毛刺。
fn mask_canvas(font_px: f32, text: &str) -> u32 {
    let chars = text.chars().count().max(1) as f32;
    // 每个字符按 1em 宽留位（数字实际约 0.55em），再补上下各半行余量。
    // 这个画布只是「光栅化用的草稿纸」，画小了字形会被它裁掉，所以给得宽松。
    let side = (font_px * (chars + 1.0)).ceil() + 4.0;
    side.clamp(8.0, 1024.0) as u32
}

/// 按指定字体和**最终字号**光栅化一段文字，返回裁好的墨迹包围盒。
fn rasterize_with(
    face: &str,
    weight: i32,
    text: &str,
    font_px: f32,
    aa: crate::config::TrayAntialias,
) -> TextMask {
    draw_text(face, weight, text, font_px, true, aa).0
}

/// 画单个字形（左上对齐），连墨迹位置和前进宽度一起返回。
///
/// 为什么按单字画：整串一起画时，第二个字的笔位是「第一个字的前进宽度」，
/// 这个宽度带小数 —— 第二个字于是落在 4 倍图上不同的子像素相位里，
/// 降采样后就变成左边 2 像素灰、右边 3 像素黑，同一个「1」两种长相。
/// 单字画 + 按整数像素落位，同一个字在任何数值里都是同一张位图。
fn rasterize_glyph(
    face: &str,
    weight: i32,
    ch: char,
    font_px: f32,
    aa: crate::config::TrayAntialias,
) -> Glyph {
    let text = ch.to_string();
    let (mask, x0, y0, advance) = draw_text(face, weight, &text, font_px, false, aa);
    Glyph {
        mask,
        x0,
        y0,
        advance: if advance > 0.0 {
            advance
        } else {
            // 量不出前进宽度就按数字的常见字宽估一个，别让字叠在一起。
            font_px * 0.55
        },
    }
}

/// 在内存位图上画一次文字。
///
/// 返回 `(墨迹, 墨迹左上角相对画布的位置, 前进宽度)`；
/// `centered` = 居中画（量字高用的探针），否则左上对齐（拼字用）。
/// `aa` 决定 GDI 的质量档，以及画完之后要不要按「锐利 / 浑厚」再拉一次灰度。
fn draw_text(
    face: &str,
    weight: i32,
    text: &str,
    font_px: f32,
    centered: bool,
    aa: crate::config::TrayAntialias,
) -> (TextMask, f32, f32, f32) {
    use std::ffi::c_void;

    use windows::Win32::Foundation::{COLORREF, RECT, SIZE};
    use windows::Win32::Graphics::Gdi::{
        BI_RGB, BITMAPINFO, BITMAPINFOHEADER, CLIP_DEFAULT_PRECIS, CreateCompatibleDC,
        CreateDIBSection, CreateFontW, DEFAULT_CHARSET, DIB_RGB_COLORS, DT_CENTER, DT_LEFT,
        DT_SINGLELINE, DT_TOP, DT_VCENTER, DeleteDC, DeleteObject, DrawTextW, FONT_QUALITY,
        GdiFlush, GetDC, GetTextExtentPoint32W, HGDIOBJ, OUT_DEFAULT_PRECIS, ReleaseDC, SelectObject, SetBkMode,
        SetTextColor, TRANSPARENT,
    };
    use windows::core::PCWSTR;

    let empty = (
        TextMask {
            width: 0,
            height: 0,
            coverage: Vec::new(),
        },
        0.0,
        0.0,
        0.0,
    );
    if font_px < 1.0 {
        return empty;
    }
    let side = mask_canvas(font_px, text);

    unsafe {
        let screen = GetDC(None);
        if screen.is_invalid() {
            return empty;
        }

        let mut info = BITMAPINFO::default();
        info.bmiHeader.biSize = std::mem::size_of::<BITMAPINFOHEADER>() as u32;
        info.bmiHeader.biWidth = side as i32;
        info.bmiHeader.biHeight = -(side as i32); // 负数 = 顶向下，省一次翻转
        info.bmiHeader.biPlanes = 1;
        info.bmiHeader.biBitCount = 32;
        info.bmiHeader.biCompression = BI_RGB.0;

        let mut bits: *mut c_void = std::ptr::null_mut();
        let Ok(bitmap) = CreateDIBSection(Some(screen), &info, DIB_RGB_COLORS, &mut bits, None, 0)
        else {
            let _ = ReleaseDC(None, screen);
            return empty;
        };

        let dc = CreateCompatibleDC(Some(screen));
        let previous = SelectObject(dc, HGDIOBJ(bitmap.0));

        SetBkMode(dc, TRANSPARENT);
        SetTextColor(dc, COLORREF(0x00FF_FFFF));

        let face: Vec<u16> = format!("{face}\0").encode_utf16().collect();
        let font = CreateFontW(
            -(font_px.round() as i32),
            0,
            0,
            0,
            weight,
            0,
            0,
            0,
            DEFAULT_CHARSET,
            OUT_DEFAULT_PRECIS,
            CLIP_DEFAULT_PRECIS,
            FONT_QUALITY(aa.quality() as u8),
            0, // DEFAULT_PITCH | FF_DONTCARE
            PCWSTR(face.as_ptr()),
        );
        let previous_font = SelectObject(dc, HGDIOBJ(font.0));

        let mut utf16: Vec<u16> = text.encode_utf16().collect();
        let mut cell = SIZE::default();
        let measured = GetTextExtentPoint32W(dc, &utf16, &mut cell).as_bool();
        let advance = if measured { cell.cx as f32 } else { 0.0 };

        let mut rect = RECT {
            left: 0,
            top: 0,
            right: side as i32,
            bottom: side as i32,
        };
        let flags = if centered {
            DT_CENTER | DT_VCENTER | DT_SINGLELINE
        } else {
            DT_LEFT | DT_TOP | DT_SINGLELINE
        };
        DrawTextW(dc, &mut utf16, &mut rect, flags);
        let _ = GdiFlush();

        let mut mask = TextMask {
            width: 0,
            height: 0,
            coverage: Vec::new(),
        };
        let (mut ink_x, mut ink_y) = (0f32, 0f32);
        if !bits.is_null() {
            let side_usize = side as usize;
            let pixels = std::slice::from_raw_parts(
                bits as *const u8,
                (side_usize * side_usize * 4) as usize,
            );
            let width = side_usize;
            // BGRA：取最大通道当覆盖率（ClearType 也不会偏色）。
            // 再按「抗锯齿」那一档改灰度：「无」压成硬边，「锐利 / 浑厚」拉一次曲线。
            let gamma = aa.gamma();
            let hard = aa.threshold();
            let shade = move |raw: f32| -> f32 {
                if hard {
                    if raw >= 0.5 { 1.0 } else { 0.0 }
                } else if (gamma - 1.0).abs() < 0.01 {
                    raw
                } else {
                    raw.powf(gamma)
                }
            };
            let coverage = |x: usize, y: usize| {
                let offset = (y * width + x) * 4;
                let raw = pixels[offset]
                    .max(pixels[offset + 1])
                    .max(pixels[offset + 2]) as f32
                    / 255.0;
                shade(raw)
            };
            // 墨迹包围盒：阈值压得很低，抗锯齿边缘也算进字形里。
            let (mut x0, mut y0, mut x1, mut y1) = (width, width, 0usize, 0usize);
            for y in 0..width {
                for x in 0..width {
                    if coverage(x, y) > 0.02 {
                        x0 = x0.min(x);
                        x1 = x1.max(x);
                        y0 = y0.min(y);
                        y1 = y1.max(y);
                    }
                }
            }
            if x1 >= x0 && y1 >= y0 {
                let w = x1 - x0 + 1;
                let h = y1 - y0 + 1;
                let mut cropped = vec![0f32; w * h];
                for y in 0..h {
                    for x in 0..w {
                        cropped[y * w + x] = coverage(x0 + x, y0 + y);
                    }
                }
                mask = TextMask {
                    width: w,
                    height: h,
                    coverage: cropped,
                };
                ink_x = x0 as f32;
                ink_y = y0 as f32;
            }
        }

        SelectObject(dc, previous_font);
        let _ = DeleteObject(HGDIOBJ(font.0));
        SelectObject(dc, previous);
        let _ = DeleteDC(dc);
        let _ = DeleteObject(HGDIOBJ(bitmap.0));
        let _ = ReleaseDC(None, screen);
        (mask, ink_x, ink_y, advance)
    }
}

#[cfg(test)]
mod tests {
    use super::{TooltipFacts, render_tooltip};
    use crate::config::TooltipConfig;
    use crate::i18n::Lang;

    fn facts(reading: &str, percent: u32, muted: bool, logarithmic: bool) -> TooltipFacts {
        TooltipFacts {
            reading: reading.to_string(),
            percent,
            db: reading.to_string(),
            device: "扬声器 (Realtek)".to_string(),
            device_desc: "Realtek Audio".to_string(),
            muted,
            apps: vec!["Chrome".to_string(), "Spotify".to_string()],
            app_count: 2,
            mode: "百分比".to_string(),
            cap: None,
            logarithmic,
        }
    }

    /// 关掉自定义时的内置内容：第一行读数、第二行设备名。
    #[test]
    fn default_tooltip_shows_reading_then_device() {
        let config = TooltipConfig::default();
        assert_eq!(
            render_tooltip(&config, &facts("-20.0 dB", 10, false, true), Lang::Zh),
            "Bolume 音量 -20.0 dB · 10%\r\n扬声器 (Realtek)"
        );
        assert_eq!(
            render_tooltip(&config, &facts("0.0 dB", 100, false, true), Lang::Zh),
            "Bolume 音量 0.0 dB · 100%\r\n扬声器 (Realtek)"
        );
        assert_eq!(
            render_tooltip(&config, &facts("62%", 62, false, false), Lang::Zh),
            "Bolume 音量 62%\r\n扬声器 (Realtek)"
        );
        assert_eq!(
            render_tooltip(&config, &facts("-20.0 dB", 10, true, true), Lang::Zh),
            "Bolume 音量 -20.0 dB · 10%（静音）\r\n扬声器 (Realtek)"
        );
        assert_eq!(
            render_tooltip(&config, &facts("62%", 62, false, false), Lang::En),
            "Bolume Volume 62%\r\n扬声器 (Realtek)"
        );
        // 没有播放设备时第二行整行消失，不会留一条空白。
        let mut no_device = facts("62%", 62, false, false);
        no_device.device.clear();
        assert_eq!(
            render_tooltip(&config, &no_device, Lang::Zh),
            "Bolume 音量 62%"
        );
    }

    /// 居中：按最宽的一行垫前导空格（全角算 2 格）。
    #[test]
    fn center_pads_shorter_lines() {
        let config = TooltipConfig {
            enabled: true,
            align: crate::config::TooltipAlign::Center,
            lines: vec!["音量 62%".to_string(), "{device}".to_string()],
            ..Default::default()
        };
        // 「音量 62%」= 2+2+1+1+1+1 = 8 格，「扬声器 (Realtek)」= 3*2+1+1+8+1 = 17 格。
        // 差 9 格，垫 (9/2)=4 个空格。
        assert_eq!(
            render_tooltip(&config, &facts("62%", 62, false, false), Lang::Zh),
            "    音量 62%\r\n扬声器 (Realtek)"
        );
        // 靠左时一格都不垫。
        let left = TooltipConfig {
            align: crate::config::TooltipAlign::Left,
            ..config.clone()
        };
        assert_eq!(
            render_tooltip(&left, &facts("62%", 62, false, false), Lang::Zh),
            "音量 62%\r\n扬声器 (Realtek)"
        );
    }

    /// 靠右：短行垫满到最宽那一行的宽度。
    #[test]
    fn right_align_pads_to_the_widest_line() {
        let config = TooltipConfig {
            enabled: true,
            align: crate::config::TooltipAlign::Right,
            lines: vec!["音量 62%".to_string(), "{device}".to_string()],
            ..Default::default()
        };
        // 「音量 62%」8 格，设备名 17 格，差 9 格全垫上。
        assert_eq!(
            render_tooltip(&config, &facts("62%", 62, false, false), Lang::Zh),
            "        音量 62%\r\n扬声器 (Realtek)"
        );
    }

    /// 自定义：编辑框里打的首尾空格、空行都照原样，一格不补。
    #[test]
    fn verbatim_keeps_the_spacing_as_typed() {
        let config = TooltipConfig {
            enabled: true,
            align: crate::config::TooltipAlign::Verbatim,
            lines: vec![
                "  音量 62%".to_string(),
                String::new(),
                "  设备   ".to_string(),
            ],
            ..Default::default()
        };
        assert_eq!(
            render_tooltip(&config, &facts("62%", 62, false, false), Lang::Zh),
            "  音量 62%\r\n\r\n  设备   "
        );
    }

    /// 行里的 `{center}` / `{right}` 只改这一行的对齐，全局设置对别的行照旧。
    #[test]
    fn per_line_marker_overrides_the_global_align() {
        let config = TooltipConfig {
            enabled: true,
            align: crate::config::TooltipAlign::Left,
            lines: vec!["音量 62%".to_string(), "{right}设备".to_string()],
            ..Default::default()
        };
        // 第一行 8 格；第二行「设备」4 格、靠右 → 补 4 格。
        assert_eq!(
            render_tooltip(&config, &facts("62%", 62, false, false), Lang::Zh),
            "音量 62%\r\n    设备"
        );
        // 标记写在行中间也认，且自己不上屏。
        let middle = TooltipConfig {
            enabled: true,
            lines: vec!["音量{center} 62%".to_string()],
            ..Default::default()
        };
        assert_eq!(
            render_tooltip(&middle, &facts("62%", 62, false, false), Lang::Zh),
            "音量 62%"
        );
        // `{{center}}` 是字面大括号，不算标记。
        let escaped = TooltipConfig {
            enabled: true,
            lines: vec!["{{center}}".to_string()],
            ..Default::default()
        };
        assert_eq!(
            render_tooltip(&escaped, &facts("62%", 62, false, false), Lang::Zh),
            "{center}"
        );
    }

    /// 自定义模板：逐行展开变量，空行丢掉，换行用 `\r\n`。
    #[test]
    fn custom_lines_expand_variables() {
        let config = TooltipConfig {
            enabled: true,
            lines: vec![
                "音量 {volume}".to_string(),
                String::new(),
                "设备 {device}".to_string(),
                "{apps}（{app_count}）".to_string(),
            ],
            ..Default::default()
        };
        assert_eq!(
            render_tooltip(&config, &facts("-20.0 dB", 10, false, true), Lang::Zh),
            "音量 -20.0 dB\r\n设备 扬声器 (Realtek)\r\nChrome, Spotify（2）"
        );
    }

    /// 静音是一套单独的内容：开关关着就沿用正常那套。
    #[test]
    fn muted_lines_override_only_when_enabled() {
        let mut config = TooltipConfig {
            enabled: true,
            lines: vec!["{volume}".to_string()],
            muted_enabled: true,
            muted_lines: vec!["已静音 · {device}".to_string()],
            ..Default::default()
        };
        assert_eq!(
            render_tooltip(&config, &facts("62%", 62, true, false), Lang::Zh),
            "已静音 · 扬声器 (Realtek)"
        );
        config.muted_enabled = false;
        assert_eq!(
            render_tooltip(&config, &facts("62%", 62, true, false), Lang::Zh),
            "62%"
        );
    }

    /// 拼错的变量原样留着；`{{` 是字面大括号。
    #[test]
    fn unknown_variable_and_escape() {
        let config = TooltipConfig {
            enabled: true,
            lines: vec!["{volumee} {{ok}}".to_string()],
            ..Default::default()
        };
        assert_eq!(
            render_tooltip(&config, &facts("62%", 62, false, false), Lang::Zh),
            "{volumee} {ok}"
        );
    }

    /// `{detail}` 是对数模式下的完整读数，`{muted}` 只在静音时出现。
    #[test]
    fn detail_and_muted_variables() {
        let config = TooltipConfig {
            enabled: true,
            lines: vec!["{detail}{muted}".to_string()],
            ..Default::default()
        };
        assert_eq!(
            render_tooltip(&config, &facts("-20.0 dB", 10, false, true), Lang::Zh),
            "-20.0 dB · 10%"
        );
        assert_eq!(
            render_tooltip(&config, &facts("62%", 62, true, false), Lang::Zh),
            "62%（静音）"
        );
        assert_eq!(
            render_tooltip(&config, &facts("62%", 62, true, false), Lang::En),
            "62% (muted)"
        );
    }

    /// 总长按 UTF-16 截到 127（szTip 的硬限制），末尾给省略号。
    #[test]
    fn tooltip_is_clamped_to_sz_tip() {
        let long_line = "{device}{device}{device}{device}".to_string();
        let config = TooltipConfig {
            enabled: true,
            lines: vec![
                long_line.clone(),
                long_line.clone(),
                long_line.clone(),
                long_line,
            ],
            ..Default::default()
        };
        let text = render_tooltip(&config, &facts("62%", 62, false, false), Lang::Zh);
        assert!(text.encode_utf16().count() <= crate::config::MAX_TOOLTIP_CHARS);
        assert!(text.ends_with('…'));
    }

    /// 临时预览：把两种图标的各个档位拼成一张联系表（BMP），人工核对。
    #[test]
    #[ignore]
    fn tray_preview() {
        let number = super::NumberStyle::default();
        let icons: Vec<Vec<u8>> = {
            let mut list = Vec::new();
            for percent in [0u32, 1, 5, 9, 10, 11, 12, 25, 42, 50, 68, 75, 88, 99, 100] {
                list.push(super::render_number_icon(percent, false, false, &number));
            }
            for volume in [0.0f32, 0.15, 0.4, 0.7, 1.0] {
                list.push(super::render_icon(
                    volume,
                    volume,
                    50,
                    false,
                    false,
                    super::TrayIconStyle::Arcs,
                    &number,
                ));
            }
            list.push(super::render_icon(
                0.7,
                0.7,
                50,
                true,
                false,
                super::TrayIconStyle::Arcs,
                &number,
            ));
            list
        };
        let size = super::canvas() as usize;
        let scale = 3usize;
        let padding = 6usize;
        let icon = size * scale;
        let width = icons.len() * (icon + padding) + padding;
        let height = icon + padding * 2;
        let bg = [28u8, 28, 28];
        let mut canvas = vec![bg; width * height];
        for (index, rgba) in icons.iter().enumerate() {
            let ox = padding + index * (icon + padding);
            let oy = padding;
            for y in 0..icon {
                for x in 0..icon {
                    let sx = x / scale;
                    let sy = y / scale;
                    let offset = (sy * size + sx) * 4;
                    let alpha = rgba[offset + 3] as f32 / 255.0;
                    let pixel = &mut canvas[(oy + y) * width + ox + x];
                    for channel in 0..3 {
                        let ink = rgba[offset + channel] as f32;
                        pixel[channel] = (ink * alpha + bg[channel] as f32 * (1.0 - alpha)) as u8;
                    }
                }
            }
        }
        let stride = (width * 3 + 3) & !3;
        let mut out = Vec::with_capacity(54 + stride * height);
        out.extend_from_slice(b"BM");
        out.extend_from_slice(&((54 + stride * height) as u32).to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&54u32.to_le_bytes());
        out.extend_from_slice(&40u32.to_le_bytes());
        out.extend_from_slice(&(width as i32).to_le_bytes());
        out.extend_from_slice(&(height as i32).to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes());
        out.extend_from_slice(&24u16.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&((stride * height) as u32).to_le_bytes());
        out.extend_from_slice(&2835u32.to_le_bytes());
        out.extend_from_slice(&2835u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes());
        for y in (0..height).rev() {
            for x in 0..width {
                let pixel = canvas[y * width + x];
                out.extend_from_slice(&[pixel[2], pixel[1], pixel[0]]);
            }
            out.extend(std::iter::repeat_n(0u8, stride - width * 3));
        }
        std::fs::write("target/tray-preview.bmp", out).unwrap();
    }

    /// 数字图标：一位数 / 十几位数 / 二十几以上必须一样高（只是更居中）。
    #[test]
    fn number_icon_keeps_uniform_height() {
        let number = super::NumberStyle::default();
        let height = |percent: u32| {
            let rgba = super::render_number_icon(percent, false, false, &number);
            let size = super::canvas() as usize;
            let rows: Vec<usize> = (0..size)
                .filter(|y| (0..size).any(|x| rgba[(y * size + x) * 4 + 3] > 40))
                .collect();
            (
                rows.first().copied().unwrap_or(0),
                rows.last().copied().unwrap_or(0),
            )
        };
        let (top, bottom) = height(25);
        for percent in [0u32, 5, 9, 12, 19, 20, 55, 99] {
            let (current_top, current_bottom) = height(percent);
            assert!(
                current_top.abs_diff(top) <= 1 && current_bottom.abs_diff(bottom) <= 1,
                "percent {percent} 的字高和 25 不一致：{current_top}..{current_bottom} vs {top}..{bottom}"
            );
        }
    }

    /// 字号是按点真长个儿的：8pt 比「自动」矮，14pt 比「自动」高。
    ///
    /// 老实现里两位数被「宽度放得下」掐着，高度设到 10px 以上就原地不动了；
    /// 现在超过宽度先横向压窄，所以字号能一直长到可画区（画布减 1px 边）。
    #[test]
    fn digit_size_grows_with_points() {
        let size = super::canvas() as usize;
        let span = |size_pt: u32| {
            let mut style = super::NumberStyle::default();
            for metrics in &mut style.metrics {
                metrics.size_pt = size_pt;
            }
            let rgba = super::render_number_icon(88, false, false, &style);
            let rows: Vec<usize> = (0..size)
                .filter(|y| (0..size).any(|x| rgba[(y * size + x) * 4 + 3] > 40))
                .collect();
            (
                rows.first().copied().unwrap_or(0),
                rows.last().copied().unwrap_or(0),
            )
        };
        let (auto_top, auto_bottom) = span(0);
        let (small_top, small_bottom) = span(8);
        let (big_top, big_bottom) = span(14);
        let (auto_h, small_h, big_h) = (
            auto_bottom - auto_top,
            small_bottom - small_top,
            big_bottom - big_top,
        );
        assert!(small_h < auto_h, "8pt 没有比自动矮：{small_h} vs {auto_h}");
        assert!(big_h > auto_h, "14pt 没有比自动高：{big_h} vs {auto_h}");
        // 大字号顶到可画区，但四周仍留 1px，不会被任务栏切边。
        assert!(big_top >= 1, "14pt 贴到顶边了：{big_top}");
        assert!(big_bottom + 1 < size, "14pt 贴到底边了：{big_bottom}");
    }

    /// 数字是整数倍降采样出来的，不该留下孤立的毛刺像素。
    /// 判定：每个墨迹像素的 8 邻域里至少还有一个墨迹像素，且每个连通块
    /// 至少 4 个像素。旧实现按非整数比例缩放，9 / 10 右边那一小块凸起
    /// 就是这种 1~2 像素的孤立碎块。
    #[test]
    fn number_icon_has_no_speckles() {
        // 自动字号是整数倍降采样；14pt 还会再横向压一遍，两条路径都要干净。
        let styles: Vec<super::NumberStyle> = [0u32, 14]
            .into_iter()
            .map(|size_pt| {
                let mut style = super::NumberStyle::default();
                for metrics in &mut style.metrics {
                    metrics.size_pt = size_pt;
                }
                style
            })
            .collect();
        let size = super::canvas() as usize;
        for number in &styles {
            for percent in [
                0u32, 3, 7, 9, 10, 11, 17, 21, 33, 47, 58, 66, 74, 88, 99, 100,
            ] {
                let rgba = super::render_number_icon(percent, false, false, number);
                let inked = |x: usize, y: usize| rgba[(y * size + x) * 4 + 3] > 40;

                // 1) 不许有孤立像素。
                for y in 0..size {
                    for x in 0..size {
                        if !inked(x, y) {
                            continue;
                        }
                        let mut neighbours = 0;
                        for dy in -1i32..=1 {
                            for dx in -1i32..=1 {
                                if dx == 0 && dy == 0 {
                                    continue;
                                }
                                let nx = x as i32 + dx;
                                let ny = y as i32 + dy;
                                if nx >= 0
                                    && ny >= 0
                                    && (nx as usize) < size
                                    && (ny as usize) < size
                                    && inked(nx as usize, ny as usize)
                                {
                                    neighbours += 1;
                                }
                            }
                        }
                        assert!(neighbours > 0, "percent {percent} 在 ({x},{y}) 有孤立像素");
                    }
                }

                // 2) 每个连通块至少 4 个像素（一个字的笔画不可能更小）。
                let mut seen = vec![false; size * size];
                for y in 0..size {
                    for x in 0..size {
                        if !inked(x, y) || seen[y * size + x] {
                            continue;
                        }
                        let mut stack = vec![(x, y)];
                        seen[y * size + x] = true;
                        let mut count = 0usize;
                        while let Some((cx, cy)) = stack.pop() {
                            count += 1;
                            for dy in -1i32..=1 {
                                for dx in -1i32..=1 {
                                    let nx = cx as i32 + dx;
                                    let ny = cy as i32 + dy;
                                    if nx < 0
                                        || ny < 0
                                        || nx as usize >= size
                                        || ny as usize >= size
                                    {
                                        continue;
                                    }
                                    let (nx, ny) = (nx as usize, ny as usize);
                                    if seen[ny * size + nx] || !inked(nx, ny) {
                                        continue;
                                    }
                                    seen[ny * size + nx] = true;
                                    stack.push((nx, ny));
                                }
                            }
                        }
                        assert!(
                            count >= 4,
                            "字号 {}pt / percent {percent} 在 ({x},{y}) 有一块只有 {count} 个像素的碎点",
                            number.metrics[1].size_pt
                        );
                    }
                }
            }
        }
    }

    /// 数字图标：每个数值都要画出对应字形（100% 也不会被裁掉）。
    #[test]
    fn number_icon_covers_digits() {
        let number = super::NumberStyle::default();
        for percent in [0u32, 1, 9, 10, 42, 99, 100] {
            let rgba = super::render_number_icon(percent, false, false, &number);
            let side = super::canvas() as usize;
            assert_eq!(rgba.len(), side * side * 4);
            let inked = rgba.chunks_exact(4).filter(|px| px[3] > 40).count();
            assert!(inked > 20, "percent {percent} 画不出字形");
            // 数字必须全部落在画布内：贴边像素不能有墨。
            let size = super::canvas() as usize;
            for i in 0..size {
                assert_eq!(rgba[i * 4 + 3], 0, "percent {percent} 顶边越界");
                assert_eq!(
                    rgba[((size - 1) * size + i) * 4 + 3],
                    0,
                    "percent {percent} 底边越界"
                );
                assert_eq!(rgba[(i * size) * 4 + 3], 0, "percent {percent} 左边越界");
                assert_eq!(
                    rgba[(i * size + size - 1) * 4 + 3],
                    0,
                    "percent {percent} 右边越界"
                );
            }
        }
    }

    /// 把数字图标打成字符画（`cargo test --bin Bolume dump_digits -- --ignored --nocapture`）。
    ///
    /// 字形的问题（某个数字变粗、边上有凸起、同一个字两种长相）肉眼在 16px 上看不真切，
    /// 打成字符画一列一列对着看最直接。
    #[test]
    #[ignore]
    fn dump_digits() {
        use crate::config::TrayDigitMetrics;
        let size = super::canvas() as usize;
        // 几组对照，看字号 / 宽 / 高 / 字距 是不是真的各自生效。
        let cases: [(&str, TrayDigitMetrics, crate::config::TrayAntialias); 6] = [
            (
                "自动",
                TrayDigitMetrics::default(),
                crate::config::TrayAntialias::Smooth,
            ),
            (
                "12pt",
                TrayDigitMetrics {
                    size_pt: 12,
                    ..TrayDigitMetrics::default()
                },
                crate::config::TrayAntialias::Smooth,
            ),
            (
                "12pt 宽60%",
                TrayDigitMetrics {
                    size_pt: 12,
                    width: 60,
                    ..TrayDigitMetrics::default()
                },
                crate::config::TrayAntialias::Smooth,
            ),
            (
                "12pt 高140%",
                TrayDigitMetrics {
                    size_pt: 12,
                    height: 140,
                    ..TrayDigitMetrics::default()
                },
                crate::config::TrayAntialias::Smooth,
            ),
            (
                "12pt 字距200",
                TrayDigitMetrics {
                    size_pt: 12,
                    tracking: 200,
                    ..TrayDigitMetrics::default()
                },
                crate::config::TrayAntialias::Smooth,
            ),
            (
                "12pt 抗锯齿无",
                TrayDigitMetrics {
                    size_pt: 12,
                    ..TrayDigitMetrics::default()
                },
                crate::config::TrayAntialias::None,
            ),
        ];
        for (label, metrics, aa) in cases {
            let mut number = super::NumberStyle::default();
            number.antialias = aa;
            for slot in &mut number.metrics {
                *slot = metrics;
            }
            for percent in [1u32, 10, 11, 20, 44, 88, 91, 99, 100] {
                let rgba = super::render_number_icon(percent, false, false, &number);
                println!("=== {label} / {percent} ===");
                for y in 0..size {
                    let mut line = String::new();
                    for x in 0..size {
                        let a = rgba[(y * size + x) * 4 + 3] as f32 / 255.0;
                        line.push(if a < 0.02 {
                            ' '
                        } else if a < 0.2 {
                            '.'
                        } else if a < 0.4 {
                            ':'
                        } else if a < 0.6 {
                            '+'
                        } else if a < 0.8 {
                            '*'
                        } else {
                            '#'
                        });
                    }
                    println!("|{line}|");
                }
            }
        }
    }

    /// 「11」里两个 1 必须逐像素一样。
    ///
    /// 这条曾经是坏的：整串一起光栅化时第二个字的笔位带小数，降采样后
    /// 落在不同的子像素相位上，左边 2 像素中灰、右边 3 像素实心。
    #[test]
    fn repeated_digit_is_pixel_identical() {
        let family = crate::config::DEFAULT_NUMBER_FONT;
        let mask = super::low_res(family, 600, 12.7, "11", Default::default(), 0.0);
        let inked = |x: usize| (0..mask.height).any(|y| mask.coverage[y * mask.width + x] > 0.02);
        // 按「连续有墨的列」切段：两个字各一段，中间的空白不算。
        let mut runs: Vec<(usize, usize)> = Vec::new();
        for x in 0..mask.width {
            if !inked(x) {
                continue;
            }
            match runs.last_mut() {
                Some((_, end)) if *end + 1 == x => *end = x,
                _ => runs.push((x, x)),
            }
        }
        assert_eq!(runs.len(), 2, "「11」应该正好两段墨");
        let (left_start, left_end) = runs[0];
        let (right_start, right_end) = runs[1];
        assert_eq!(
            left_end - left_start,
            right_end - right_start,
            "两个 1 的宽度不一样"
        );
        for y in 0..mask.height {
            for x in 0..=(left_end - left_start) {
                let left = mask.coverage[y * mask.width + left_start + x];
                let right = mask.coverage[y * mask.width + right_start + x];
                assert!(
                    (left - right).abs() < 1e-6,
                    "({x},{y}) 处两个 1 的覆盖率不同：{left} vs {right}"
                );
            }
        }
    }

    /// 同一个字号下，不同数值的整串必须占同一个行高。
    ///
    /// 圆头数字（0 6 8 9）顶部有 overshoot，比「1」高一行；只按当前这一串算行高的话，
    /// 「11」会比「10」矮一行，托盘上数字就会随着音量上下跳一像素（用户看到的是
    /// 「一高一低」）。`digit_frame` 取 0–9 的并集当统一行高。
    #[test]
    fn digits_share_one_line_height() {
        let family = crate::config::DEFAULT_NUMBER_FONT;
        let one = super::low_res(family, 600, 16.0, "11", Default::default(), 0.0);
        for text in ["1", "10", "20", "44", "88", "91", "99", "100"] {
            let mask = super::low_res(family, 600, 16.0, text, Default::default(), 0.0);
            assert_eq!(
                mask.height, one.height,
                "「{text}」的行高 {} 和「11」的 {} 不一样",
                mask.height, one.height
            );
        }
    }

    /// 同一个数字在不同数值里必须是同一张位图。
    ///
    /// 坑在「按宽度收字号」：两位数的宽度不一样，收完字体后「10」的 1 会比
    /// 「11」的 1 小一圈。现在 0..99 共用一个字号，这条能盯住别退回去。
    #[test]
    fn same_digit_is_stable_across_values() {
        let family = crate::config::DEFAULT_NUMBER_FONT;
        let one = super::low_res(family, 600, 12.7, "1", Default::default(), 0.0);
        for text in ["10", "12", "21"] {
            let mask = super::low_res(family, 600, 12.7, text, Default::default(), 0.0);
            // 落位是「整数像素 + 相对基线」，所以总能找到一处整数偏移对上。
            let matched = (0..mask.width).any(|dx| {
                (0..mask.height).any(|dy| {
                    (0..one.height).all(|y| {
                        (0..one.width).all(|x| {
                            let (tx, ty) = (x + dx, y + dy);
                            if tx >= mask.width || ty >= mask.height {
                                return one.coverage[y * one.width + x] < 0.02;
                            }
                            (one.coverage[y * one.width + x] - mask.coverage[ty * mask.width + tx])
                                .abs()
                                < 1e-6
                        })
                    })
                })
            });
            assert!(matched, "「{text}」里的 1 和单独的 1 长得不一样");
        }
    }
}
