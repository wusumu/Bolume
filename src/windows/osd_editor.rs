//! OSD 布局编辑器：单独一扇可缩放的窗口。
//!
//! 左边是放大后的卡片，右边一栏是全部外观参数。卡片的元素可以拖、可以缩、
//! 可以删、可以加回来；双击标题直接改字；卡片本身的尺寸也能拖着改。

use std::{
    cell::{Cell, RefCell},
    ops::Range,
    rc::Rc,
    time::Duration,
};

use gpui::prelude::*;
use gpui::{
    AnyElement, App, Bounds, Context, FocusHandle, KeyDownEvent, MouseButton, MouseMoveEvent,
    Pixels, Point, Render, SharedString, Window, div, px,
};

use crate::{
    config::{ConfigStore, OsdAlign, OsdFunction, OsdPart, OsdTemplate},
    i18n::Lang,
    ui::{self, theme},
    windows::osd::{self, OsdHandle, OsdResize, OsdState, OsdTarget},
    windows::platform,
};

/// 右侧参数栏宽度：默认值 / 拖着左边缘能改到的范围（逻辑像素）。
///
/// 下限按「色块那一行放得下」定：标签 + 8 个预设 + 自定义入口，再窄就挤到行外了。
const PANEL_W_DEFAULT: f32 = 268.0;
const PANEL_W_MIN: f32 = 250.0;
const PANEL_W_MAX: f32 = 420.0;

/// 画布底色那三档（第四格是自由选色）：黑 / 灰 / 白。
///
/// 灰不是随手取的：卡片底板大多在中间调，摆在 18% 灰上对比最难看 —— 一看见
/// 边缘糊掉就知道该给背景色或者投影加点功夫。
const CANVAS_BGS: [&str; 3] = ["#000000", "#7a7a80", "#ffffff"];

/// 某个颜色槽的预设色（第一格常是空串 = 跟随）。
///
/// 「调过又换成预设」要能翻回旧色，判据就是「现在这格是不是预设之一」。
fn slot_presets(slot: ColorSlot) -> &'static [&'static str] {
    match slot {
        ColorSlot::Card => CARD_COLORS,
        ColorSlot::Text | ColorSlot::Part(_) => INK_COLORS,
        ColorSlot::Accent => ACCENT_COLORS,
        ColorSlot::Shadow => SHADOW_COLORS,
        ColorSlot::BarTrack => BAR_TRACK_COLORS,
        ColorSlot::BarMuted => BAR_MUTED_COLORS,
        ColorSlot::CanvasBg => &CANVAS_BGS,
    }
}

/// 画布和参数栏之间那条「拖宽度」的把手有多宽。
const GRIP_W: f32 = 6.0;

/// 自绘标题栏高度（和设置窗口一致）。
const TITLEBAR_H: f32 = 42.0;

/// 取色器轮询间隔。
const PICK_TICK: Duration = Duration::from_millis(20);

const CARD_COLORS: &[&str] = &[
    "", "#000000", "#151519", "#2b2b2f", "#1f2937", "#3b1d2b", "#0f2e2e", "#ffffff",
];
const INK_COLORS: &[&str] = &["", "#ffffff", "#f6f6f9", "#d1d5db", "#9aa3b2", "#111827"];
/// 阴影的颜色：第一个是「纯黑」，其余是同色系的深浅。空串 = 纯黑。
const SHADOW_COLORS: &[&str] = &["", "#000000", "#0b0b10", "#1c1c22", "#2f2f38"];

const ACCENT_COLORS: &[&str] = &[
    "", "#7c9cff", "#22d3ee", "#4ade80", "#facc15", "#fb7185", "#c084fc",
];
/// 音量条的轨道底色：空 = 主题那一档，其余是几种常见的「浅底色」。
const BAR_TRACK_COLORS: &[&str] = &["", "#000000", "#151519", "#2b2b2f", "#ffffff"];
/// 静音时的填充色：空 = 主题那层淡色（灰），其余给几档更醒目的提示色。
const BAR_MUTED_COLORS: &[&str] = &["", "#6b7280", "#9aa3b2", "#d1d5db", "#f87171"];

/// 按住元素边框角上的手柄转圈。
#[derive(Clone, Copy)]
struct RotateDrag {
    target: RotationTarget,
    /// 转轴（元素中心，窗口坐标）。
    center: Point<Pixels>,
    /// 按下时光标相对转轴的方向（弧度）。
    start_angle: f32,
    /// 按下那一刻的角度。
    origin: f32,
}

#[derive(Clone, Copy)]
struct MoveDrag {
    part: OsdPart,
    start: Point<Pixels>,
    origin: (f32, f32),
    size: (f32, f32),
    scale: f32,
    auto_width: bool,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum CardEdge {
    Right,
    Bottom,
    Corner,
}

#[derive(Clone, Copy)]
struct CardResize {
    start: Point<Pixels>,
    width: u32,
    height: u32,
    edge: CardEdge,
    /// 按下那一刻的显示倍数与左上角。
    ///
    /// 拖动期间**冻结**这两个值：卡片尺寸一变，自动适配的倍数就会跟着变，
    /// 手柄便会从光标底下跑掉（拖得越快差得越远），这就是「不符合直觉」的根源。
    /// 冻住之后 1 屏幕像素 = 1/scale 逻辑像素，手柄始终贴着手，松手再重新适配。
    scale: f32,
    left: f32,
    top: f32,
}

/// 取色器当前作用在哪个颜色上。
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum ColorSlot {
    Card,
    Text,
    Accent,
    /// 卡片投影的颜色。
    Shadow,
    /// 音量条的轨道底色（空 = 主题的轨道色）。
    BarTrack,
    /// 静音时音量条的填充色（空 = 主题那层淡色）。
    BarMuted,
    /// 编辑器画布的底色 —— 只影响编辑器里那块「桌面」，不影响提示条本身。
    CanvasBg,
    /// 某个元素自己的颜色（空 = 跟随「文字色」；音量条跟随「强调色」）。
    Part(OsdPart),
}

impl ColorSlot {
    fn index(self) -> usize {
        match self {
            Self::Card => 0,
            Self::Text => 1,
            Self::Accent => 2,
            Self::Shadow => 3,
            Self::BarTrack => 4,
            Self::BarMuted => 5,
            Self::CanvasBg => 6,
            // 往后顺延，保证每个元素各占一个（取色器的 id / 状态都靠它）。
            Self::Part(part) => 7 + part.index() as usize,
        }
    }

    fn get(self, config: &crate::config::Config) -> String {
        match self {
            Self::Card => config.osd.card().color,
            Self::Text => config.osd.colors.text.clone(),
            Self::Accent => config.osd.colors.accent.clone(),
            Self::Shadow => config.osd.shadow.color.clone(),
            Self::BarTrack => config.osd.colors.bar_track.clone(),
            Self::BarMuted => config.osd.colors.bar_muted.clone(),
            Self::CanvasBg => config.editor_bg.clone(),
            Self::Part(part) => config.osd.part_color(part),
        }
    }

    fn set(self, config: &mut crate::config::Config, value: String) {
        match self {
            Self::Card => config.osd.set_card(|card| card.color = value),
            // 颜色走 `edit_style`：一套模板一份，改亮度卡片的字色不会动音量那张。
            Self::Text => config.osd.edit_style(|style| style.text = value),
            Self::Accent => config.osd.edit_style(|style| style.accent = value),
            Self::Shadow => config.osd.edit_style(|style| style.shadow.color = value),
            Self::BarTrack => config.osd.edit_style(|style| style.bar_track = value),
            Self::BarMuted => config.osd.edit_style(|style| style.bar_muted = value),
            Self::CanvasBg => config.editor_bg = value,
            Self::Part(part) => config.osd.set_part_color(part, value),
        }
    }

    /// 取色器里那个「回到默认」的按钮文字：跟的是谁就写谁。
    fn follow_label(self, lang: Lang) -> &'static str {
        match self {
            Self::Card => lang.pick("跟随主题", "Theme"),
            Self::Text => lang.pick("跟随主题", "Theme"),
            Self::Accent => lang.pick("跟随主题", "Theme"),
            Self::Shadow => lang.pick("跟随主题", "Theme"),
            Self::BarTrack => lang.pick("跟随主题", "Theme"),
            Self::BarMuted => lang.pick("跟随主题", "Theme"),
            Self::CanvasBg => lang.pick("恢复默认", "Default"),
            Self::Part(OsdPart::Bar) => lang.pick("跟随强调色", "Accent"),
            Self::Part(_) => lang.pick("跟随文字色", "Text color"),
        }
    }

    /// 取色器标题上的名字（一行放得下）。
    fn label(self, lang: Lang) -> &'static str {
        match self {
            Self::Card => lang.pick("背景色", "Card color"),
            Self::Text => lang.pick("文字色", "Text color"),
            Self::Accent => lang.pick("强调色", "Accent color"),
            Self::Shadow => lang.pick("阴影色", "Shadow color"),
            Self::BarTrack => lang.pick("滑条底色", "Bar track"),
            Self::BarMuted => lang.pick("静音时的填充", "Bar when muted"),
            Self::CanvasBg => lang.pick("画布底色", "Canvas color"),
            Self::Part(_) => lang.pick("元素颜色", "Element color"),
        }
    }

    /// 面板里那一行的短名字。
    ///
    /// 参数栏窄（还能拖到更窄），「静音时音量条」这种长名字会把色块挤到行外；
    /// 这一组本来就在「音量条」标题下面，短名字不会歧义。
    fn row_label(self, lang: Lang) -> &'static str {
        match self {
            Self::BarTrack => lang.pick("底色", "Track"),
            Self::BarMuted => lang.pick("静音时", "Muted"),
            other => other.label(lang),
        }
    }
}

/// 取色器状态（一轮只开一个）。
///
/// HSV / 色号 / 方块与色相条的拖拽都在 `ui::picker` 里 —— 和设置页的
/// 「背景色 / 强调色」共用同一份，两边不用各修一遍。
struct Picker {
    slot: ColorSlot,
    /// 当前色号；跟随主题时是主题色，一经拖动就固化成具体色号。
    picker: crate::ui::Picker,
    /// 正在屏幕上取色（窗口已藏起来）。
    picking: bool,
}

// 色号 / HSV 的换算统一放在 `ui::color`：设置页的色轮用的是同一套。
use crate::ui::color::{hex_of, hsv_to_rgb, parse_rgb, push_hex, rgb_to_hsv};

struct TextEdit {
    /// 正在改哪个元素：状态文字，或某一条自定义文字。
    part: OsdPart,
    buffer: String,
    marked: Option<Range<usize>>,
    /// 光标位置（UTF-16 下标，和平台层 / 输入法一致）。
    caret: usize,
    /// 选区的另一端：和 `caret` 相等就是没有选中。
    ///
    /// 拖选 / Shift+方向键只动 `caret`，`anchor` 留在按下的那一格，
    /// 和记事本一样「回头拖会把选区缩回去」。
    anchor: usize,
    /// Ctrl+Z 的撤销栈：每次改字**之前**压一份「缓冲 + 光标 + 锚点」。
    ///
    /// 逐次记录（每次输入 / 删除 / 粘贴各一份）而不是按词合并：一个字的回退
    /// 最符合直觉，也不会出现「一按就退掉半句话」。
    undo: Vec<(String, usize, usize)>,
}

impl TextEdit {
    /// 记一步（内容没变就不重复压）。
    fn snapshot(&mut self) {
        let snap = (self.buffer.clone(), self.caret, self.anchor);
        if self.undo.last() == Some(&snap) {
            return;
        }
        if self.undo.len() >= 100 {
            self.undo.remove(0);
        }
        self.undo.push(snap);
    }

    /// 回退一步：把缓冲和光标都还原成压栈那一刻。
    fn undo(&mut self) -> bool {
        let Some((buffer, caret, anchor)) = self.undo.pop() else {
            return false;
        };
        let len = utf16_len(&buffer);
        self.buffer = buffer;
        self.marked = None;
        self.caret = caret.min(len);
        self.anchor = anchor.min(len);
        true
    }

    /// 当前选中的范围（UTF-16）；没选中就是 `caret..caret`。
    fn selection(&self) -> Range<usize> {
        if self.caret < self.anchor {
            self.caret..self.anchor
        } else {
            self.anchor..self.caret
        }
    }

    fn has_selection(&self) -> bool {
        self.caret != self.anchor
    }

    /// 把光标挪到 `at`；`keep` 假 = 顺手把选区收掉。
    fn move_caret(&mut self, at: usize, keep: bool) {
        let len = utf16_len(&self.buffer);
        self.caret = at.min(len);
        if !keep {
            self.anchor = self.caret;
        }
    }
}

/// 数值项一共几项：`index()` 的最大值 + 1，数组长度也跟着它走。
///
/// 加一项就改这里 —— 忘了改会在 `slider_bounds[index]` 上直接 panic（len 8 / index 8）。
const NUM_FIELDS: usize = 11;

/// 转角度的是谁：整张卡片（背景）还是某个元素。
///
/// 元素各自存一份角度、卡片自己一份，两边的 ±、滑条、手打都是同一套控件，
/// 所以统一成这一个「目标」再分发。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RotationTarget {
    /// 整张卡片（背景）。
    Card,
    /// 某个元素（绕自己的中心）。
    Part(OsdPart),
}

impl RotationTarget {
    /// 两条角度滑条各自的那一格矩形。
    fn slot(self) -> usize {
        match self {
            RotationTarget::Card => 0,
            RotationTarget::Part(_) => 1,
        }
    }
}

/// 右侧面板里可以手打数值的那几项。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NumField {
    Width,
    Height,
    Corner,
    /// 音量条圆角（半高的百分比，100 = 全圆头）。
    BarCorner,
    Opacity,
    ShadowX,
    ShadowY,
    ShadowBlur,
    ShadowOpacity,
    /// 角度（元素自己或背景），手打时可以是负数。
    Rotation(RotationTarget),
}

impl NumField {
    fn range(self) -> (i32, i32) {
        match self {
            NumField::Width => (120, 900),
            NumField::Height => (28, 320),
            NumField::Corner => (0, 160),
            NumField::BarCorner => (0, 100),
            NumField::Opacity => (20, 100),
            NumField::ShadowX => crate::config::OSD_SHADOW_OFFSET_RANGE,
            NumField::ShadowY => crate::config::OSD_SHADOW_OFFSET_RANGE,
            NumField::ShadowBlur => (
                crate::config::OSD_SHADOW_BLUR_RANGE.0 as i32,
                crate::config::OSD_SHADOW_BLUR_RANGE.1 as i32,
            ),
            NumField::ShadowOpacity => (
                crate::config::OSD_SHADOW_OPACITY_RANGE.0 as i32,
                crate::config::OSD_SHADOW_OPACITY_RANGE.1 as i32,
            ),
            NumField::Rotation(_) => crate::config::OSD_ROTATION_RANGE,
        }
    }

    fn unit(self) -> &'static str {
        match self {
            NumField::Opacity | NumField::ShadowOpacity | NumField::BarCorner => "%",
            NumField::Rotation(_) => "°",
            _ => "px",
        }
    }

    fn index(self) -> usize {
        match self {
            NumField::Width => 0,
            NumField::Height => 1,
            NumField::Corner => 2,
            NumField::BarCorner => 3,
            NumField::Opacity => 4,
            NumField::ShadowX => 5,
            NumField::ShadowY => 6,
            NumField::ShadowBlur => 7,
            NumField::ShadowOpacity => 8,
            // 角度两条各有自己的格：背景和元素可能同时展开，
            // 共用一个下标会让两格滑条抢同一个矩形。
            NumField::Rotation(RotationTarget::Card) => 9,
            NumField::Rotation(RotationTarget::Part(_)) => 10,
        }
    }

    fn get(self, osd: &crate::config::OsdConfig) -> i32 {
        match self {
            // 尺寸 / 圆角 / 不透明度按**当前模板**读：编辑器切到媒体键模板时，
            // 这几格显示的是媒体键那份。
            NumField::Width => osd.card().width as i32,
            NumField::Height => osd.card().height as i32,
            NumField::Corner => osd.card().corner as i32,
            NumField::BarCorner => osd.bar_round as i32,
            NumField::Opacity => osd.card().opacity as i32,
            NumField::ShadowX => osd.shadow.offset_x,
            NumField::ShadowY => osd.shadow.offset_y,
            NumField::ShadowBlur => osd.shadow.blur as i32,
            NumField::ShadowOpacity => osd.shadow.opacity as i32,
            NumField::Rotation(RotationTarget::Card) => osd.rotation,
            NumField::Rotation(RotationTarget::Part(part)) => {
                osd.rotation_of(part).round() as i32
            }
        }
    }

    fn set(self, osd: &mut crate::config::OsdConfig, value: i32) {
        let (low, high) = self.range();
        // 角度是**绕回来**的，不是夹住的：转到 195° 等于 -165°，
        // 夹成 180° 的话按十下 + 就卡死了。
        let value = match self {
            NumField::Rotation(_) => value,
            _ => value.clamp(low, high),
        };
        match self {
            NumField::Width => osd.set_card(|card| card.width = value as u32),
            NumField::Height => osd.set_card(|card| card.height = value as u32),
            NumField::Corner => osd.set_card(|card| card.corner = value as u32),
            // 条圆角 / 投影也按模板存：三套模板各调各的。
            NumField::BarCorner => {
                osd.edit_style(|style| style.bar_round = value as u32);
            }
            NumField::Opacity => osd.set_card(|card| card.opacity = value as u32),
            NumField::ShadowX => osd.edit_style(|style| style.shadow.offset_x = value),
            NumField::ShadowY => osd.edit_style(|style| style.shadow.offset_y = value),
            NumField::ShadowBlur => {
                osd.edit_style(|style| style.shadow.blur = value.max(0) as u32);
            }
            NumField::ShadowOpacity => osd.edit_style(|style| {
                style.shadow.opacity = value.clamp(0, 100) as u32;
            }),
            NumField::Rotation(RotationTarget::Card) => {
                osd.set_card(|card| card.rotation = value);
            }
            NumField::Rotation(RotationTarget::Part(part)) => {
                osd.set_part_rotation(part, value as f32);
            }
        }
    }
}

/// 正在手打的那一项数值（双击数值进入）。
struct NumEdit {
    field: NumField,
    buffer: String,
    /// 刚进入时整串是「选中」状态：第一次敲数字直接替换掉。
    fresh: bool,
}

/// 编辑器最多能退几步。
///
/// 只留最近 10 步：这个栈存的是整份 OSD 配置，留多了没意义，
/// 用户连退十下基本也已经回到想要的位置了。
const HISTORY_STEPS: usize = 10;

/// 布局编辑的撤销栈（Photoshop 那一套：Ctrl+Z 退一步，Ctrl+Shift+Z 再往前一步）。
///
/// 存的是**改之前**那一整份 `OsdConfig` —— 布局、配色、阴影、文字全在里面。
/// 比挨个字段记 diff 稳得多，也不用管这次是哪个入口改的。
struct History {
    past: Vec<crate::config::OsdConfig>,
    future: Vec<crate::config::OsdConfig>,
    /// 当前这一串连续改动（一次拖动 / 一次滑条拖拽）是不是已经记过一步了。
    ///
    /// 拖元素时每移 1px 都会写一次配置，不合并的话一次拖动就能把 10 步吃光。
    gesture: bool,
}

impl History {
    fn new() -> Self {
        Self {
            past: Vec::new(),
            future: Vec::new(),
            gesture: false,
        }
    }

    /// 记一步（`before` 是这次改动之前的样子）。
    ///
    /// `in_gesture` 为真表示「手还按着」，同一个手势里只留最开头那一步。
    fn push(&mut self, before: crate::config::OsdConfig, in_gesture: bool) {
        if in_gesture {
            if self.gesture {
                return;
            }
            self.gesture = true;
        } else {
            self.gesture = false;
        }
        // 新改动一来，原来的「重做」就作废了（和所有编辑器一致）。
        self.future.clear();
        self.past.push(before);
        if self.past.len() > HISTORY_STEPS {
            self.past.remove(0);
        }
    }

    fn undo(&mut self, current: crate::config::OsdConfig) -> Option<crate::config::OsdConfig> {
        let previous = self.past.pop()?;
        self.future.push(current);
        self.gesture = false;
        Some(previous)
    }

    fn redo(&mut self, current: crate::config::OsdConfig) -> Option<crate::config::OsdConfig> {
        let next = self.future.pop()?;
        self.past.push(current);
        if self.past.len() > HISTORY_STEPS {
            self.past.remove(0);
        }
        self.gesture = false;
        Some(next)
    }

    fn can_undo(&self) -> bool {
        !self.past.is_empty()
    }

    fn can_redo(&self) -> bool {
        !self.future.is_empty()
    }

    fn clear(&mut self) {
        self.past.clear();
        self.future.clear();
        self.gesture = false;
    }
}

/// 编辑器里哪一种下拉菜单开着。
#[derive(Clone, Copy, PartialEq, Eq)]
enum EditorMenu {
    /// 状态：音量的几个状态 + 媒体键的几个动作。
    State,
    /// 图标：媒体模板下的动作图标（播放 / 暂停 / 上一曲…），顺便带音量 / 静音。
    Icon,
    /// 音量模板下那几种喇叭样式（弧线1 / 弧线2 / 竖条 / 圆点 / 音箱）。
    IconStyle,
}

pub struct OsdEditor {
    config: Rc<RefCell<ConfigStore>>,
    focus: FocusHandle,
    selected: Option<OsdPart>,
    dragging: Option<MoveDrag>,
    resizing: Option<OsdResize>,
    card_resize: Option<CardResize>,
    guide: Option<(Option<f32>, Option<f32>)>,
    text: Option<TextEdit>,
    /// 正在按着鼠标拖选文字（松手结束）。
    text_drag: bool,
    /// 就地编辑那一行的矩形（由 canvas 每帧写回）：点击 / 拖选时把 x 换算成第几个字。
    text_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    num: Option<NumEdit>,
    picker: Option<Picker>,
    /// 每个颜色槽上一次调出来的自选色。
    ///
    /// 点预设色只是把那一格换成预设，色轮里「调好的那一色」不该跟着没了 ——
    /// 再点开「+」要能接着调。只活在这一次编辑器会话里（关掉窗口就重来）。
    picker_memory: std::collections::HashMap<ColorSlot, String>,
    /// 「恢复默认布局」的确认弹窗开着没有。
    ///
    /// 这个动作一下就抹掉整张模板的排布，而且救不回来（没有撤销），
    /// 必须问一句再动手。
    confirm_reset: bool,
    /// 饱和度方块 / 色相条在窗口里的矩形（由 canvas 的 prepaint 每帧写回）。
    ///
    /// 自绘区域拿不到「元素自己的左上角」，但拖色必须知道光标落在方块的第几格，
    /// 所以把 bounds 存下来做换算。
    /// 右侧参数栏的滚动位置（参数多了要能翻）。
    panel_scroll: gpui::ScrollHandle,
    /// 右侧参数栏宽度（逻辑像素）。拖动时只改内存里的数，松手才写盘 ——
    /// 每一帧都写一次配置文件太重。
    panel_w: f32,
    /// 正在拖参数栏的左边缘：(按下时的窗口 x, 按下时的宽度)。
    panel_drag: Option<(f32, f32)>,
    /// 标题栏按下了但还没开始拖：鼠标真动起来才交给系统拖。
    ///
    /// 按下就 `start_window_move()` 会立刻进系统的标题栏拖动模态环，
    /// 环里会把双击的第二下吞掉 —— 双击就永远收不到 `click_count == 2`。
    titlebar_armed: bool,
    /// 正在拖元素列表的哪一行（调图层顺序）。
    layer_drag: Option<OsdPart>,
    /// 元素列表里每一行报上来的矩形（元素编号 + 窗口坐标）。
    ///
    /// 以前用「列表总高 / 行数」估行高，列表里开始插展开卡之后这个估算就偏了 ——
    /// 每行各报各的，拖到哪一行一目了然。
    layer_rows: Rc<RefCell<Vec<(u32, Bounds<Pixels>)>>>,
    /// 四个尺寸 + 四个阴影参数的滑条矩形，拖动时按 x 比例换算数值。
    /// 每一项数值那条小滑条的测量范围（下标 = `NumField::index()`，数量跟着它走）。
    slider_bounds: [Rc<Cell<Option<Bounds<Pixels>>>>; NUM_FIELDS],
    /// 正在拖哪个滑条。
    slider_drag: Option<NumField>,
    /// 两条「角度」小滑条的矩形（0 = 背景，1 = 元素；只量当前展开的那两个）。
    rotation_bounds: [Rc<Cell<Option<Bounds<Pixels>>>>; 2],
    /// 正在拖谁的角度滑条。
    rotation_drag: Option<RotationTarget>,
    /// 正按着元素边框角上的手柄转圈：记下转轴和按下那一刻的角度。
    rotate_drag: Option<RotateDrag>,
    /// 画布那块区域在窗口里的矩形（由 canvas 每帧写回）。
    ///
    /// 画布里所有坐标都是「相对画布左上角」的，而鼠标事件是窗口坐标 ——
    /// 转角度要按「光标相对元素中心」的方向算，得先把两套坐标对上。
    canvas_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// 画布要不要按「已静音」画。
    ///
    /// 只有正在调「静音时音量条」那一段颜色时打开：不然那条颜色调完完全看不到效果。
    /// 点别的颜色行 / 别的色轮就自动关掉。
    preview_muted: bool,
    /// 画布上试看哪个「状态」（下标见 `PREVIEWS`）。
    ///
    /// 只影响编辑器画布，不写配置 —— 真正的文字由「这次是哪个操作」决定。
    /// 状态和图标是同一张表的两列：选了其中一边，另一边跟着变。
    preview: Option<usize>,
    /// 哪个下拉菜单开着（状态 / 图标）。
    menu: Option<EditorMenu>,
    /// 打开编辑器那一刻的 OSD 设置。
    ///
    /// 编辑器里所有改动都是**即时写进配置**的（拖动时就能在真卡片上看到），
    /// 所以「有没有改过」靠和它比较；用户点「放弃」时也靠它还原。
    baseline: crate::config::OsdConfig,
    /// 关闭窗口的确认弹窗开着没有（有未保存改动时才弹）。
    confirm_close: bool,
    /// 刚点了眼睛 / × / 预览格子：紧跟着冒上来的行点击不要再当成「选中并编辑」。
    ///
    /// 带时间戳而不是纯 bool：万一那一记 click 根本没冒上来（拖动就是这种情况），
    /// 留下一个永久的 true 会把下一次真点击吃掉。
    suppress_row_click: Option<std::time::Instant>,
    /// 撤销 / 重做栈（改之前的整份 OSD 配置）。
    history: RefCell<History>,
}

impl OsdEditor {
    pub fn new(config: Rc<RefCell<ConfigStore>>, cx: &mut Context<Self>) -> Self {
        let baseline = config.borrow().get().osd.clone();
        let panel_w = config.borrow().get().editor_panel_w;
        Self {
            config,
            baseline,
            confirm_close: false,
            focus: cx.focus_handle(),
            selected: None,
            dragging: None,
            resizing: None,
            card_resize: None,
            guide: None,
            text: None,
            text_drag: false,
            text_bounds: Rc::new(Cell::new(None)),
            num: None,
            picker: None,
            picker_memory: std::collections::HashMap::new(),
            confirm_reset: false,
            panel_scroll: gpui::ScrollHandle::new(),
            panel_w: (if panel_w > 0.0 {
                panel_w
            } else {
                PANEL_W_DEFAULT
            })
            .clamp(PANEL_W_MIN, PANEL_W_MAX),
            panel_drag: None,
            titlebar_armed: false,
            layer_drag: None,
            layer_rows: Rc::new(RefCell::new(Vec::new())),
            slider_bounds: std::array::from_fn(|_| Rc::new(Cell::new(None))),
            slider_drag: None,
            rotation_bounds: std::array::from_fn(|_| Rc::new(Cell::new(None))),
            rotation_drag: None,
            rotate_drag: None,
            canvas_bounds: Rc::new(Cell::new(None)),
            preview_muted: false,
            preview: None,
            menu: None,
            suppress_row_click: None,
            history: RefCell::new(History::new()),
        }
    }

    /// 配置里的原样，没按模板铺过 —— 撤销栈和「改没改」都拿它比。
    fn raw_cfg(&self) -> crate::config::OsdConfig {
        self.config.borrow().get().osd.clone()
    }

    /// 画布用的配置：当前是媒体键模板时，尺寸 / 圆角 / 不透明度 / 背景色
    /// 换成它自己那一套（见 `OsdConfig::view`）。
    fn cfg(&self) -> crate::config::OsdConfig {
        self.raw_cfg().view()
    }

    fn lang(&self) -> Lang {
        self.config.borrow().get().language
    }

    fn update(&self, f: impl FnOnce(&mut crate::config::OsdConfig)) {
        let before = self.raw_cfg();
        let _ = self.config.borrow_mut().update(|config| f(&mut config.osd));
        // 真的变了才记一步：滑条和拖动会连着调很多次，没变的那几次不该占位置，
        // 不然「按了 Ctrl+Z 却什么都没退」会显得莫名其妙。
        if self.raw_cfg() != before {
            let in_gesture = self.gesture_active();
            self.history.borrow_mut().push(before, in_gesture);
        }
    }

    /// 改整份配置（不只是 `osd` 那一段）。
    ///
    /// 画布底色这类「编辑器自己的外观」存在 `Config` 顶层：它们不影响提示条，
    /// 所以判脏 / 撤销栈仍然只比 `osd`（`before` 只取 `osd`，改这些不会占一步撤销）。
    fn update_config(&self, f: impl FnOnce(&mut crate::config::Config)) {
        let before = self.raw_cfg();
        let _ = self.config.borrow_mut().update(f);
        if self.raw_cfg() != before {
            let in_gesture = self.gesture_active();
            self.history.borrow_mut().push(before, in_gesture);
        }
    }

    /// 写一个颜色进去（预设色块 / 取色器 / 「跟随」都走这里）。
    fn set_color(&mut self, slot: ColorSlot, value: String) {
        // 顺手记一笔：下次在同一格点开「+」时，色轮从这一色开始，
        // 而不是从「中途点过的那个预设色」开始。所以预设色本身不记。
        if crate::ui::picker::remembers_value(slot_presets(slot), &value) {
            self.picker_memory.insert(slot, value.clone());
        }
        self.update_config(|config| slot.set(config, value));
    }

    /// 参数栏现在多宽（配置里存的值 / 拖动中的值都可能越界，统一夹一遍）。
    fn panel_w(&self) -> f32 {
        self.panel_w.clamp(PANEL_W_MIN, PANEL_W_MAX)
    }

    /// 画布底色：配置里存的是色号，读不出来就退回纯黑。
    fn canvas_bg(&self) -> gpui::Hsla {
        theme::parse_hex(&self.config.borrow().get().editor_bg)
            .unwrap_or_else(|| theme::parse_hex("#000000").unwrap_or_else(theme::surface))
    }

    /// 落在画布上、不在卡片里的那些字（「实际大小」/ 尺寸读数）用什么颜色。
    ///
    /// 画布能被换成白的，主题那层三级文字色在白底上等于消失 —— 按底色亮度翻一档。
    fn canvas_ink(&self) -> gpui::Hsla {
        let rgb = parse_rgb(&self.config.borrow().get().editor_bg).unwrap_or((0, 0, 0));
        let luma = (0.299 * rgb.0 as f32 + 0.587 * rgb.1 as f32 + 0.114 * rgb.2 as f32) / 255.0;
        if luma > 0.55 {
            theme::parse_hex("#4a4a52").unwrap_or_else(theme::text_secondary)
        } else {
            theme::text_tertiary()
        }
    }

    /// 手是不是还按着（拖动 / 缩放 / 拉滑条 / 拖色）。
    ///
    /// 一次手势只往历史里记一步，不然拖一下元素就吃掉半条撤销栈。
    fn gesture_active(&self) -> bool {
        self.dragging.is_some()
            || self.resizing.is_some()
            || self.card_resize.is_some()
            || self.slider_drag.is_some()
            || self.rotation_drag.is_some()
            || self.rotate_drag.is_some()
            || self.layer_drag.is_some()
            || self
                .picker
                .as_ref()
                .map(|picker| picker.picker.dragging())
                .unwrap_or(false)
    }

    /// 撤销上一步（Ctrl+Z / 标题栏那颗箭头）。
    fn undo(&mut self, cx: &mut Context<Self>) {
        let current = self.cfg();
        let Some(previous) = self.history.borrow_mut().undo(current) else {
            return;
        };
        self.restore(previous, cx);
    }

    /// 重做（Ctrl+Shift+Z / 标题栏那颗箭头）。
    fn redo(&mut self, cx: &mut Context<Self>) {
        let current = self.cfg();
        let Some(next) = self.history.borrow_mut().redo(current) else {
            return;
        };
        self.restore(next, cx);
    }

    /// 把整份 OSD 配置换成另一份（撤销 / 重做共用）。
    fn restore(&mut self, osd: crate::config::OsdConfig, cx: &mut Context<Self>) {
        let _ = self.config.borrow_mut().update(|config| config.osd = osd);
        // 正在编辑的临时状态全丢掉：光标 / 色号 / 手打的数值都可能指着
        // 刚被退掉的那个元素，留着下一步就会写回一个已经不存在的目标。
        self.text = None;
        self.text_drag = false;
        self.num = None;
        self.picker = None;
        self.guide = None;
        self.dragging = None;
        self.resizing = None;
        self.card_resize = None;
        self.slider_drag = None;
        self.rotation_drag = None;
        self.rotate_drag = None;
        self.layer_drag = None;
        if let Some(part) = self.selected {
            if !self.cfg().shows_part(part) {
                self.selected = None;
            }
        }
        cx.notify();
    }

    /// 这次打开编辑器之后有没有改过。
    ///
    /// 不另设「脏标记」：编辑器里所有修改都是即时落盘的，直接和打开时那份比最准，
    /// 也不会漏掉某条没标记的路径。
    fn is_dirty(&self) -> bool {
        self.cfg() != self.baseline
    }

    /// 保存：把当前样子记成新的基准（值其实早就写进配置了，这一步只是「确认」）。
    fn accept_changes(&mut self, cx: &mut Context<Self>) {
        self.baseline = self.cfg();
        cx.notify();
    }

    /// 放弃：整份还原成打开编辑器那一刻的样子，并写回配置。
    fn discard_changes(&mut self, cx: &mut Context<Self>) {
        let base = self.baseline.clone();
        let _ = self.config.borrow_mut().update(|config| config.osd = base);
        // 都「放弃」了，就别让 Ctrl+Z 再把人送回刚才丢掉的那一堆改动里。
        self.history.borrow_mut().clear();
        self.picker = None;
        self.text = None;
        self.num = None;
        self.guide = None;
        self.confirm_close = false;
        cx.notify();
    }

    /// 方向键微调：1px，按住 Shift 走 10px。
    fn nudge(&mut self, key: &str, big: bool, cx: &mut Context<Self>) -> bool {
        let Some(part) = self.selected else {
            return false;
        };
        let cfg = self.cfg();
        let (card_w, card_h) = osd::card_size(&cfg);
        let step = if big { 10.0 } else { 1.0 };
        let (dx, dy) = match key {
            "left" => (-step, 0.0),
            "right" => (step, 0.0),
            "up" => (0.0, -step),
            "down" => (0.0, step),
            _ => return false,
        };
        let position = cfg.part(part);
        let x = (position.x * card_w + dx).clamp(0.0, card_w);
        let y = (position.y * card_h + dy).clamp(0.0, card_h);
        self.update(|osd| osd.set_part(part, x / card_w, y / card_h));
        cx.notify();
        true
    }

    /// 删除选中的元素（等于把它关掉，随时能从右边列表加回来）。
    fn remove_selected(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(part) = self.selected else {
            return false;
        };
        self.update(|osd| osd.set_part_visible(part, false));
        self.selected = None;
        cx.notify();
        true
    }

    fn apply_resize(&mut self, resize: OsdResize, dx: f32, dy: f32, cx: &mut Context<Self>) {
        let cfg = self.cfg();
        let (card_w, card_h) = osd::card_size(&cfg);
        let part = resize.part;
        let (ox, oy, ow, oh) = resize.origin;
        let factor = osd::height_factor(part);

        let mut new_size = resize.origin_size as f32;
        if resize.handle.moves_y() || part == OsdPart::Icon {
            let growth = if part == OsdPart::Icon {
                if resize.handle.moves_x() && resize.handle.moves_y() {
                    let sx = if resize.handle.left() { -1.0 } else { 1.0 };
                    let sy = if resize.handle.top() { -1.0 } else { 1.0 };
                    (dx * sx + dy * sy) / 2.0
                } else if resize.handle.moves_x() {
                    if resize.handle.left() { -dx } else { dx }
                } else if resize.handle.top() {
                    -dy
                } else {
                    dy
                }
            } else if resize.handle.top() {
                -dy
            } else {
                dy
            };
            new_size += growth / factor;
        }
        let (low, high) = part.size_range();
        let new_size = (new_size.round().max(1.0) as u32).clamp(low, high);
        let new_h = osd::part_height(part, new_size as f32);

        let mut new_w = ow;
        let mut width_value = resize.origin_width;
        if part != OsdPart::Icon && resize.handle.moves_x() {
            let delta = if resize.handle.left() { -dx } else { dx };
            let ceiling = if resize.handle.left() {
                (ox + ow).max(part.min_width())
            } else {
                osd::max_part_width(&cfg, part)
            };
            new_w = (ow + delta).clamp(part.min_width(), ceiling.max(part.min_width()));
            width_value = new_w.round() as u32;
        }

        let mut new_x = ox;
        let mut new_y = oy;
        if part != OsdPart::Icon && resize.handle.left() {
            new_x = ox + ow - new_w;
        }
        if resize.handle.top() {
            new_y = oy + oh - new_h;
        }
        new_x = new_x.clamp(0.0, (card_w - new_w).max(0.0));
        new_y = new_y.clamp(0.0, (card_h - new_h).max(0.0));

        self.update(|osd| {
            if part != OsdPart::Icon {
                osd.set_part_width(part, width_value);
            }
            osd.set_part_size(part, new_size);
            osd.set_part(part, new_x / card_w, new_y / card_h);
        });
        cx.notify();
    }

    /// 双击数值进入手打模式：整串先选中，第一次敲数字直接替换。
    fn begin_num(&mut self, field: NumField, window: &mut Window, cx: &mut Context<Self>) {
        let value = field.get(&self.cfg());
        self.num = Some(NumEdit {
            field,
            buffer: value.to_string(),
            fresh: true,
        });
        self.focus.focus(window, cx);
        cx.notify();
    }

    /// 手打的那一项写回配置（回车 / 失焦时调用）。
    fn commit_num(&mut self, cx: &mut Context<Self>) {
        let Some(edit) = self.num.take() else {
            return;
        };
        if let Ok(value) = edit.buffer.trim().parse::<i32>() {
            let field = edit.field;
            self.update(|osd| field.set(osd, value));
        }
        cx.notify();
    }

    /// 手打数值的按键：只收数字，回车提交、Esc 放弃。返回 true 表示已处理。
    fn num_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        if self.num.is_none() {
            return false;
        }
        match event.keystroke.key.as_str() {
            "escape" => {
                self.num = None;
                cx.notify();
                true
            }
            "enter" => {
                self.commit_num(cx);
                true
            }
            "backspace" => {
                if let Some(edit) = self.num.as_mut() {
                    if edit.buffer.len() > 1 {
                        edit.buffer.pop();
                    } else {
                        edit.buffer.clear();
                    }
                    edit.fresh = false;
                }
                cx.notify();
                true
            }
            _ => {
                let Some(text) = event.keystroke.key_char.clone() else {
                    return false;
                };
                if text.is_empty() {
                    return false;
                }
                // 负号只认「刚进入 / 刚清空」时打的那一个：阴影偏移可以是负数。
                if text == "-" {
                    if let Some(edit) = self.num.as_mut() {
                        if edit.fresh || edit.buffer.is_empty() {
                            edit.buffer.clear();
                            edit.buffer.push('-');
                            edit.fresh = false;
                            cx.notify();
                            return true;
                        }
                    }
                    return false;
                }
                if !text.chars().all(|ch| ch.is_ascii_digit()) {
                    return false;
                }
                if let Some(edit) = self.num.as_mut() {
                    if edit.fresh {
                        edit.buffer.clear();
                        edit.fresh = false;
                    }
                    // 最多 4 位：`-180`（角度）这种就是最长的了。
                    if edit.buffer.chars().count() < 4 {
                        edit.buffer.push_str(&text);
                    }
                }
                cx.notify();
                true
            }
        }
    }

    /// 就地在元素自己的框里改字。
    ///
    /// 只有**自定义文字**能改：状态元素的内容由状态决定（音量 / 播放 / 静音…），
    /// 往里写字既没意义也会把那一格撑成一句死字，所以这里直接挡掉。
    ///
    /// 元素关着的话先把它打开 —— 关着的时候画布上什么都不画，
    /// 光有光标没有字，用户会以为输入没生效。
    fn begin_text(&mut self, part: OsdPart, window: &mut Window, cx: &mut Context<Self>) {
        if !matches!(part, OsdPart::Text(_)) {
            return;
        }
        let cfg = self.cfg();
        let buffer = cfg.text_for(part).to_string();
        if !cfg.part(part).visible {
            self.update(|osd| osd.set_part_visible(part, true));
        }
        self.selected = Some(part);
        // 光标落在末尾（和「点进输入框」的直觉一致），不预选任何字。
        let caret = utf16_len(&buffer);
        self.text = Some(TextEdit {
            part,
            buffer,
            marked: None,
            caret,
            anchor: caret,
            undo: Vec::new(),
        });
        self.text_drag = false;
        self.focus.focus(window, cx);
        cx.notify();
    }

    /// 新建一条自定义文字，并直接进入就地编辑。
    ///
    /// 新文字只有「位置」没有字，画布上还什么都看不到 —— 直接把光标给它，
    /// 用户敲第一个字就出现在卡片里，不用再双击一次。
    fn add_text_part(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let store = self.config.clone();
        let mut created = OsdPart::Text(0);
        let _ = store
            .borrow_mut()
            .update(|config| created = config.osd.add_text());
        self.begin_text(created, window, cx);
        cx.notify();
    }

    /// 文字里能用的变量按钮：点一下接在这条文字后面。
    ///
    /// 没在编辑就先给光标（不然点了没反应，像是按钮坏了）。
    fn text_chips(&self, lang: Lang, cx: &mut Context<Self>) -> AnyElement {
        // 只有自定义文字会走到这里（状态元素没有编辑入口）。
        let part = self.selected.unwrap_or(OsdPart::Text(0));
        let chips: Vec<AnyElement> = crate::textvars::TOKENS
            .iter()
            .map(|token| {
                let token = *token;
                div()
                    .id(SharedString::from(format!(
                        "ed-var-{}",
                        token.trim_matches(['{', '}'])
                    )))
                    .px(px(6.))
                    .py(px(1.5))
                    .rounded(px(3.))
                    .bg(theme::track())
                    .cursor_pointer()
                    .text_size(px(10.))
                    .text_color(theme::text_secondary())
                    .hover(|style| {
                        style
                            .bg(theme::surface_hover())
                            .text_color(theme::text_primary())
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, _, window, cx| {
                            // 别让根节点把这一下当成「点别处」把编辑提交掉。
                            cx.stop_propagation();
                            if this.text.as_ref().map(|edit| edit.part) != Some(part) {
                                this.begin_text(part, window, cx);
                            }
                            this.replace_text(None, token, false, cx);
                            cx.notify();
                        }),
                    )
                    .child(token)
                    .into_any_element()
            })
            .collect();
        div()
            .flex()
            .flex_col()
            .gap(px(3.))
            .child(div().flex().flex_wrap().gap(px(3.)).children(chips))
            .child(
                div()
                    .text_size(px(9.5))
                    .text_color(theme::text_tertiary())
                    .child(lang.pick(
                        "点一下接到文字后面；{MM} 也是分钟，月份写 {mo}，{{ 是字面大括号",
                        "Click to append; {MM} is minutes too, use {mo} for the month, {{ is a literal brace",
                    )),
            )
            .into_any_element()
    }

    fn commit_text(&mut self, cx: &mut Context<Self>) {
        self.text_drag = false;
        let Some(edit) = self.text.take() else {
            return;
        };
        let part = edit.part;
        let text = edit.buffer.trim().to_string();
        self.update(|osd| {
            // 原来空着、现在写了字：自动打开（不然写了也看不见）。
            // 其余情况一律不碰开关 —— 用户刚点眼睛关掉的那一下，不能被保存又打开。
            // 按**当前模板**判：文字内容各模板各一份。
            let was_empty = osd.style_text_for(part).trim().is_empty();
            osd.set_text_for(part, text.clone());
            if part != OsdPart::Title && was_empty && !text.is_empty() {
                osd.set_part_visible(part, true);
            }
        });
        cx.notify();
    }

    fn replace_text(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        mark: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(edit) = self.text.as_mut() else {
            return;
        };
        // 输入法给的范围 > 正在拼的那一段 > 选区 > 光标位置。
        // 有选区时整段换掉 —— 这就是「选中再打字」的常规行为；
        // 没有选区就插在光标处，不能默认「接在最后」（那样方向键 / 点击定位全白做）。
        let selection = if edit.has_selection() {
            Some(edit.selection())
        } else {
            Some(edit.caret..edit.caret)
        };
        let target = range.or(edit.marked.clone()).or(selection);
        // 改字之前记一步：Ctrl+Z 能退回到这一刻。
        edit.snapshot();
        let (next, marked, caret) = text_apply(&edit.buffer, None, target, text, mark, 40);
        edit.buffer = next;
        edit.marked = marked;
        // 拼字过程中输入法会整段替换，光标跟着到拼字末尾即可。
        edit.caret = caret;
        edit.anchor = caret;
        cx.notify();
    }

    /// 就地编辑的输入通道：顺手记下这一行的矩形（点击 / 拖选要按它换算字的位置）。
    fn input_canvas(&self, cx: &mut Context<Self>) -> AnyElement {
        let focus = self.focus.clone();
        let entity = cx.entity();
        let cell = self.text_bounds.clone();
        gpui::canvas(
            |_bounds, _window, _cx| {},
            move |bounds, _, window, cx| {
                cell.set(Some(bounds));
                window.handle_input(
                    &focus,
                    gpui::ElementInputHandler::new(bounds, entity.clone()),
                    cx,
                );
            },
        )
        .absolute()
        .left_0()
        .top_0()
        .size_full()
        .into_any_element()
    }

    /// 光标 / 选区跟着鼠标走：x 落在第几个字就挪到那一格。
    ///
    /// `extend` 真 = 拖选（锚点不动），假 = 单击定位（选区收掉）。
    /// 横坐标按**对齐方式**先减掉文字起点，不然居中 / 靠右时点哪儿都算偏左；
    /// 字号要乘 `scale`：编辑器里的字是放大画的，按原始字号量会对不上。
    fn text_click(
        &mut self,
        x: Pixels,
        scale: f32,
        extend: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(bounds) = self.text_bounds.get() else {
            return;
        };
        let Some(edit) = self.text.as_ref() else {
            return;
        };
        let cfg = self.cfg();
        let part = edit.part;
        let size = cfg.part(part).size as f32 * scale;
        let row_w = bounds.size.width.as_f32();
        let offset = x.as_f32() - bounds.origin.x.as_f32();
        let measure = osd::TextMeasure::new(window);
        // 对齐留白：这一行里文字块从哪儿开始。
        let text_w = measure.width(&edit.buffer, size);
        let start = match cfg.align_of(part) {
            OsdAlign::Right => (row_w - text_w).max(0.0),
            OsdAlign::Center => ((row_w - text_w) / 2.0).max(0.0),
            OsdAlign::Left => 0.0,
        };
        let at = index_at_x(&edit.buffer, offset - start, size, &measure);
        let anchor = if extend { edit.anchor } else { at };
        let Some(edit) = self.text.as_mut() else {
            return;
        };
        let len = utf16_len(&edit.buffer);
        edit.anchor = anchor.min(len);
        edit.caret = at.min(len);
        cx.notify();
    }

    /// 按光标删一个字（选区优先整段删掉）。
    fn delete_text(&mut self, forward: bool, cx: &mut Context<Self>) {
        let Some(edit) = self.text.as_mut() else {
            return;
        };
        if edit.has_selection() {
            let range = edit.selection();
            let next = format!(
                "{}{}",
                utf16_slice(&edit.buffer, 0..range.start),
                utf16_slice(&edit.buffer, range.end..utf16_len(&edit.buffer))
            );
            edit.snapshot();
            edit.buffer = next;
            edit.marked = None;
            edit.caret = range.start;
            edit.anchor = range.start;
            cx.notify();
            return;
        }
        let len = utf16_len(&edit.buffer);
        let at = edit.caret.min(len);
        let (from, to) = if forward {
            if at >= len {
                return;
            }
            (at, at + next_char_len(&edit.buffer, at))
        } else {
            if at == 0 {
                return;
            }
            (at - prev_char_len(&edit.buffer, at), at)
        };
        let next = format!(
            "{}{}",
            utf16_slice(&edit.buffer, 0..from),
            utf16_slice(&edit.buffer, to..len)
        );
        edit.snapshot();
        edit.buffer = next;
        edit.marked = None;
        edit.caret = from;
        edit.anchor = from;
        cx.notify();
    }

    /// 左右 / Home / End 挪光标，`extend` = 按住 Shift 拉选区。
    fn move_text_caret(&mut self, key: &str, extend: bool, cx: &mut Context<Self>) {
        let Some(edit) = self.text.as_mut() else {
            return;
        };
        let len = utf16_len(&edit.buffer);
        let at = edit.caret.min(len);
        let next = match key {
            "home" => 0,
            "end" => len,
            "left" if at > 0 => at - prev_char_len(&edit.buffer, at),
            "right" if at < len => at + next_char_len(&edit.buffer, at),
            _ => at,
        };
        if !extend && edit.has_selection() {
            // 没按 Shift：左右只把选区收掉，不额外挪一格（和记事本一致）。
            let range = edit.selection();
            edit.caret = match key {
                "left" => range.start,
                "right" => range.end,
                _ => next,
            };
            edit.anchor = edit.caret;
        } else {
            edit.caret = next;
            if !extend {
                edit.anchor = next;
            }
        }
        cx.notify();
    }
}

/// 状态文字所有可能显示的内容（下拉菜单用它列全）。
///
/// 和 `osd.rs` 里按操作生成标题的那几支一一对应：音量 / 静音 / 取消静音 / 应用音量 /
/// 上一曲 / 播放 / 暂停 / 下一曲 / 停止。
/// 画布上可以试看的状态，**图标和文字是同一行**：选其中一边，另一边跟着走。
///
/// 图标名就是 `assets/icons` 里的文件名：
/// - `volume-high` / `volume-off` 会在音量模板里再按用户选的喇叭样式换档；
/// - `apps` 是那个方块图标（「应用音量」）；
/// - `prev` / `play` / `pause` / `next` / `stop` 是媒体键那一套。
pub(crate) struct PreviewOption {
    pub zh: &'static str,
    pub en: &'static str,
    pub icon: &'static str,
    /// 这一条是不是「已静音」：画布按它决定音量条画静音时的那段颜色。
    pub muted: bool,
}

pub(crate) const PREVIEWS: &[PreviewOption] = &[
    PreviewOption {
        zh: "音量",
        en: "Volume",
        icon: "volume-high",
        muted: false,
    },
    PreviewOption {
        zh: "已静音",
        en: "Muted",
        icon: "volume-off",
        muted: true,
    },
    PreviewOption {
        zh: "取消静音",
        en: "Unmuted",
        icon: "volume-high",
        muted: false,
    },
    PreviewOption {
        zh: "应用音量",
        en: "App volume",
        icon: "apps",
        muted: false,
    },
    PreviewOption {
        zh: "上一曲",
        en: "Previous track",
        icon: "prev",
        muted: false,
    },
    PreviewOption {
        zh: "播放",
        en: "Play",
        icon: "play",
        muted: false,
    },
    PreviewOption {
        zh: "暂停",
        en: "Pause",
        icon: "pause",
        muted: false,
    },
    PreviewOption {
        zh: "下一曲",
        en: "Next track",
        icon: "next",
        muted: false,
    },
    PreviewOption {
        zh: "停止",
        en: "Stop",
        icon: "stop",
        muted: false,
    },
    PreviewOption {
        zh: "亮度",
        en: "Brightness",
        icon: "sun",
        muted: false,
    },
];

/// 图标下拉的候选：`(图标名, 对应的状态下标)`，按图标去重。
///
/// 「音量」和「取消静音」共用一张喇叭图，选它时落到第一个用它的状态上。
const ICON_CHOICES: &[(&str, usize)] = &[
    ("volume-high", 0),
    ("volume-off", 1),
    ("apps", 3),
    ("prev", 4),
    ("play", 5),
    ("pause", 6),
    ("next", 7),
    ("sun", 9),
    ("stop", 8),
];

/// 一块「只量尺寸、不画东西」的取景框：把 bounds 写回外面那个 cell。
///
/// 元素列表要按行数换算拖拽落点、滑条要按 x 换算数值，都得知道自己的矩形。
/// 自绘元素拿不到「自己的 bounds」，用 canvas 的 prepaint 每帧写一次最省事。
/// 把 `point` 绕 `center` 转 `degrees`（顺时针，和屏幕上的正角度一致）。
fn rotate_about(center: (f32, f32), point: (f32, f32), degrees: f32) -> (f32, f32) {
    if degrees == 0.0 {
        return point;
    }
    let (sin, cos) = degrees.to_radians().sin_cos();
    let dx = point.0 - center.0;
    let dy = point.1 - center.1;
    (
        center.0 + dx * cos - dy * sin,
        center.1 + dx * sin + dy * cos,
    )
}

/// 把屏幕上的位移按 `degrees` 反向转回去。
///
/// 卡片转着的时候，往右拖元素在屏幕上看起来是斜的 —— 得把位移换回卡片自己的
/// 坐标系里，元素才跟着光标走，而不是斜着跑。
fn unrotate_delta(dx: f32, dy: f32, degrees: f32) -> (f32, f32) {
    if degrees == 0.0 {
        return (dx, dy);
    }
    let (sin, cos) = (-degrees).to_radians().sin_cos();
    (dx * cos - dy * sin, dx * sin + dy * cos)
}

fn measure_canvas(cell: Rc<Cell<Option<Bounds<Pixels>>>>) -> AnyElement {
    gpui::canvas(move |bounds, _, _| cell.set(Some(bounds)), |_, _, _, _| {})
        .absolute()
        .left_0()
        .top_0()
        .size_full()
        .into_any_element()
}

/// 一块只量尺寸的取景框：把自己的矩形（带着元素编号）记进外面那张表。
///
/// 列表里会插展开卡，行高不再是统一的，「总高 / 行数」那套换算会算偏；
/// 每行各报各的矩形，拖拽落点按「光标压在哪一行上」判就准了。
fn measure_row(rows: Rc<RefCell<Vec<(u32, Bounds<Pixels>)>>>, index: u32) -> AnyElement {
    gpui::canvas(
        move |bounds, _, _| {
            let mut rows = rows.borrow_mut();
            match rows.iter_mut().find(|(part, _)| *part == index) {
                Some(slot) => slot.1 = bounds,
                None => rows.push((index, bounds)),
            }
        },
        |_, _, _, _| {},
    )
    .absolute()
    .left_0()
    .top_0()
    .size_full()
    .into_any_element()
}

/// 把一段文字塞进缓冲区：`(新串, 正在拼的那段, 新光标)`。
fn text_apply(
    current: &str,
    marked: Option<Range<usize>>,
    range: Option<Range<usize>>,
    text: &str,
    mark: bool,
    max_chars: usize,
) -> (String, Option<Range<usize>>, usize) {
    let len = utf16_len(current);
    let range = range.or(marked).unwrap_or(len..len);
    let start = range.start.min(len);
    let end = range.end.min(len).max(start);
    let prefix = utf16_slice(current, 0..start);
    let suffix = utf16_slice(current, end..len);
    let text: String = text.chars().filter(|c| !c.is_control()).collect();
    let room = max_chars.saturating_sub(prefix.chars().count() + suffix.chars().count());
    let inserted: String = text.chars().take(room).collect();
    let inserted_len = utf16_len(&inserted);
    let caret = start + inserted_len;
    let mut next = prefix;
    next.push_str(&inserted);
    next.push_str(&suffix);
    (next, mark.then_some(start..caret), caret)
}

fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// 光标左边那个字有多长（UTF-16）。
fn prev_char_len(text: &str, at: usize) -> usize {
    let mut index = 0usize;
    for ch in text.chars() {
        let len = ch.len_utf16();
        if index + len == at {
            return len;
        }
        index += len;
    }
    0
}

/// 光标右边那个字有多长（UTF-16）。
fn next_char_len(text: &str, at: usize) -> usize {
    let mut index = 0usize;
    for ch in text.chars() {
        let len = ch.len_utf16();
        if index == at {
            return len;
        }
        index += len;
    }
    0
}

/// 横坐标落在第几个字：按每个字的实际宽度累加，过半就算下一个。
fn index_at_x(text: &str, x: f32, size: f32, measure: &osd::TextMeasure) -> usize {
    if x <= 0.0 {
        return 0;
    }
    let mut width = 0.0f32;
    let mut index = 0usize;
    for ch in text.chars() {
        let mut buffer = [0u8; 4];
        let cell = measure.width(ch.encode_utf8(&mut buffer), size);
        if x < width + cell / 2.0 {
            return index;
        }
        width += cell;
        index += ch.len_utf16();
    }
    index
}

fn utf16_slice(text: &str, range: Range<usize>) -> String {
    let units: Vec<u16> = text.encode_utf16().collect();
    let start = range.start.min(units.len());
    let end = range.end.min(units.len()).max(start);
    String::from_utf16_lossy(&units[start..end])
}

impl gpui::EntityInputHandler for OsdEditor {
    fn text_for_range(
        &mut self,
        range: Range<usize>,
        adjusted_range: &mut Option<Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let edit = self.text.as_ref()?;
        let len = utf16_len(&edit.buffer);
        let start = range.start.min(len);
        let end = range.end.min(len).max(start);
        *adjusted_range = Some(start..end);
        Some(utf16_slice(&edit.buffer, start..end))
    }

    fn selected_text_range(
        &mut self,
        _ignore: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<gpui::UTF16Selection> {
        let edit = self.text.as_ref()?;
        Some(gpui::UTF16Selection {
            range: edit.selection(),
            reversed: edit.caret < edit.anchor,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<Range<usize>> {
        self.text.as_ref().and_then(|edit| edit.marked.clone())
    }

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some(edit) = self.text.as_mut() {
            if edit.marked.take().is_some() {
                cx.notify();
            }
        }
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.replace_text(range, text, false, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<Range<usize>>,
        new_text: &str,
        _selected: Option<Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.replace_text(range, new_text, true, cx);
    }

    fn bounds_for_range(
        &mut self,
        _range: Range<usize>,
        element_bounds: gpui::Bounds<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<gpui::Bounds<Pixels>> {
        Some(element_bounds)
    }

    fn character_index_for_point(
        &mut self,
        _point: Point<Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        None
    }

    fn text_length_utf16(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        self.text.as_ref().map(|edit| utf16_len(&edit.buffer))
    }

    fn accepts_text_input(&self, _window: &mut Window, _cx: &mut Context<Self>) -> bool {
        self.text.is_some()
    }
}

impl Render for OsdEditor {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let cfg = self.cfg();
        // 卡片转的是 90° 的奇数倍时，画面上的「横」和「竖」互换了：
        // 画在转后边上的柄、箭头方向都要跟着换（不然竖边顶着横箭头）。
        let crossed = ((cfg.rotation as f32 / 90.0).round() as i32).rem_euclid(2) == 1;
        // 确认弹窗开着时，画布先按「恢复默认之后」的样子画：用户在按下确认之前
        // 就能看见会换成什么。右侧参数栏仍显示当前值 —— 那是在改的现状，
        // 两者不一致是故意的（这里只是预览，还没写进配置）。
        let preview = self.confirm_reset.then(|| {
            let mut draft = cfg.clone();
            draft.reset_layout();
            draft
        });
        let view = preview.as_ref().unwrap_or(&cfg);
        // 预览时不着重显示选中框：看的是默认排布本身。
        let selected_part = if preview.is_some() {
            None
        } else {
            self.selected
        };
        let lang = self.lang();
        let (card_w, card_h) = osd::card_size(view);
        // 卡片转了之后占的地方是**转后的包围盒**：画布按它缩放、按它居中，
        // 45° 时才不会有一角被画布边缘切掉。
        let (box_w, box_h) = osd::rotated_bounds(view);
        let viewport = window.viewport_size();
        let vw = viewport.width.as_f32();
        let vh = viewport.height.as_f32();

        let canvas_w = (vw - self.panel_w() - GRIP_W).max(200.0);
        let canvas_h = (vh - TITLEBAR_H - 44.0).max(120.0);
        let avail_w = (canvas_w - 90.0).max(80.0);
        let avail_h = (canvas_h - 80.0).max(60.0);
        // 投影算进画面尺寸：扩散调大时影子往外长，如果只按卡片本体缩放，
        // 影子就会被画布边缘切掉一块（看起来像外面套了个框）。
        let shadow_pad = osd::shadow_margin(view);
        let mut scale = (avail_w / (box_w + shadow_pad * 2.0))
            .min(avail_h / (box_h + shadow_pad * 2.0))
            .clamp(0.2, 6.0);
        // `pixel_w/h` 是**卡片本体**的显示尺寸（元素框、尺寸手柄都按它算），
        // `box_*` 是转后包围盒 —— 外面那层盒子按包围盒摆。
        let mut pixel_w = card_w * scale;
        let mut pixel_h = card_h * scale;
        // `left` / `top` 是**卡片本体**的左上角：元素框、尺寸手柄都按它算。
        // 外面那层包围盒按包围盒居中，本体再往里缩半个差。
        let mut left = 44.0 + ((canvas_w - 88.0) - box_w * scale).max(0.0) / 2.0
            + (box_w * scale - pixel_w) * 0.5;
        let mut top = 44.0 + 40.0 + ((canvas_h - 80.0) - box_h * scale).max(0.0) / 2.0
            + (box_h * scale - pixel_h) * 0.5;

        // 拖卡片边缘改尺寸时把显示倍数和左上角冻住：否则卡一变宽、自动适配倍数就变小，
        // 手柄会从光标底下「跑掉」，越拖越偏 —— 这正是之前手感不对劲的原因。
        if let Some(resize) = self.card_resize {
            scale = resize.scale;
            pixel_w = card_w * scale;
            pixel_h = card_h * scale;
            left = resize.left;
            top = resize.top;
        }

        // 正在就地改字的那条先从卡片里抽掉：不抽的话旧的自动内容还画在底下，
        // 输入的字叠在它上面，看着像重影。下面按同一个位置、同一个字号补回来。
        let editing = self
            .text
            .as_ref()
            .filter(|_| preview.is_none())
            .map(|edit| edit.part);
        let mut draft = view.clone();
        if let Some(part) = editing {
            draft.set_part_visible(part, false);
        }
        let mut sample = sample_state(&draft, lang, self.preview);
        // 正在调「静音时音量条」的颜色：画布先切到已静音 —— 否则这条颜色
        // 调了完全看不见效果，只能猜。图标一起换成静音那个，看着才像真的。
        if self.preview_muted {
            sample.muted = true;
            sample.icon = preview_icon(&draft, "volume-off");
        }
        // 状态文字没自定义时，状态 / 图标两个下拉都能让画布试看自动内容。
        if draft.title_text.trim().is_empty() {
            if let Some(index) = self.preview {
                let option = &PREVIEWS[index.min(PREVIEWS.len() - 1)];
                sample.title = lang.pick(option.zh, option.en).to_string();
                sample.icon = preview_icon(&draft, option.icon);
            }
        }
        let measure = osd::TextMeasure::new(window);
        // 卡片本体不再自己摆绝对位置：整块（含旋转）交给后面的 `card_frame`，
        // 元素框还是按「卡片自己的坐标」摆 —— 卡片一转，它们跟着一起转。
        let mut card = osd::card_visual(&draft, &sample, scale, None, &measure, lang);
        // 元素框那套坐标是相对卡片左上角的，角手柄算转轴时要加回来。
        let card_origin = (left, top);

        // 元素框会互相重叠（标题底下往往压着若干额外文字），后画的盖住先画的。
        // 选中的那个排到最后画，它才永远在最上层：在右侧列表里点一下标题，
        // 再双击它就地改字就不会被别的框抢走。
        let mut order: Vec<OsdPart> = view.parts();
        if let Some(selected) = selected_part {
            order.retain(|part| *part != selected);
            order.push(selected);
        }
        // 预览时画布只留卡片本体 —— 编辑框和缩放手柄会挡住「恢复之后到底长什么样」。
        let order: Vec<OsdPart> = if preview.is_some() { Vec::new() } else { order };

        // 文字量尺寸时要先把 `{hh}` 这类变量展开成「这一刻」的字，
        // 跟卡片里画出来的那一串保持一致（同一帧共用一个时间）。
        let clock = crate::textvars::Clock::local();
        for part in order {
            // 正在手打的这条即使还没字（新建的空文字）也要画框：框跟着缓冲区长大，
            // 用户敲第一个字就能看见「字在框里」。
            let editing_here = self.text.as_ref().map(|edit| edit.part) == Some(part);
            if !view.shows_part(part) && !editing_here {
                continue;
            }
            let (x, y, w, h) = osd::editor_rect(view, part);
            // 文字元素的框就贴着**画出来的那句话**，几何必须和 `osd::card_visual`
            // 里那一套逐项对齐：宽度量的是展开变量之后真的显示出来的字（`{hh}` /
            // `{date}` 展开后可能比模板长也短），行高取元素自己的行高，位置按对齐
            // 方式相对**元素矩形**（`part_rect`）推 —— 不是相对这个框自己。
            //
            // 以前是拿原始模板去量、再相对「收窄后的框」对齐，于是框要么比字窄、
            // 要么比字短：字露在框外面，双击那些露出来的地方自然进不了编辑。
            let (mut box_w, mut left) = ((w * scale).max(16.0), x * scale);
            let mut box_h = (h * scale).max(16.0);
            if part.is_text() {
                let size = view.part(part).size as f32;
                let (rx, _, rw, _) = osd::part_rect(view, part);
                let frame_w = (rw * scale).max(1.0);
                // 行高和成品一样：换算到屏幕像素后就是 `anchored_text` 用的那个高。
                let line_box = osd::part_height(part, size) * scale;
                // (这段字实际多宽, 它的行高)
                let (content_w, line_h) = match part {
                    OsdPart::Title => {
                        // 状态文字里也能写变量（`{time}` 当钟用），量之前先展开。
                        let title = crate::textvars::resolve(&sample.title, &clock, lang);
                        let main = measure.width(&title, size * scale);
                        if sample.subtitle.is_empty() {
                            (main, line_box)
                        } else {
                            let sub_size = (size * 0.8).max(8.0) * scale;
                            let sub = measure.width(&sample.subtitle, sub_size);
                            (
                                main.max(sub),
                                line_box + 2.0 * scale + measure.height(&sample.subtitle, sub_size),
                            )
                        }
                    }
                    OsdPart::Percent => (measure.width(&sample.percent, size * scale), line_box),
                    OsdPart::Db => (measure.width(&sample.db, size * scale), line_box),
                    OsdPart::Text(index) => {
                        // 正在手打这条：框跟着**缓冲区**走，边打字边长大，
                        // 不然提交前框一直是旧文字的大小。
                        let raw = match self.text.as_ref().filter(|edit| edit.part == part) {
                            Some(edit) => edit.buffer.clone(),
                            None => view.text_at(index as usize).to_string(),
                        };
                        let text = crate::textvars::resolve(raw.trim(), &clock, lang);
                        if text.is_empty() && editing_here {
                            // 空文字正在编辑：框按占位提示那句话的宽度给，
                            // 不然只剩十几像素，看着像框没了。
                            let hint = lang.pick("输入文字…", "Type here…");
                            (measure.width(hint, size * scale), line_box)
                        } else {
                            (measure.width(&text, size * scale), line_box)
                        }
                    }
                    _ => (0.0, 0.0),
                };
                // 文字元素没有「容器宽」这一说：框就是这句话本身。
                // 缩放手柄改的是字号，不是这个框的宽。
                let wanted = (content_w + 2.0).max(12.0);
                left = rx * scale
                    + match view.align_of(part) {
                        OsdAlign::Right => frame_w - wanted,
                        OsdAlign::Center => (frame_w - wanted) / 2.0,
                        OsdAlign::Left => 0.0,
                    };
                box_w = wanted;
                box_h = line_h.max(12.0);
            }
            let knob = (box_h / 3.2).clamp(6.0, 12.0).min(box_w / 3.2).max(5.0);
            let selected = selected_part == Some(part);

            let mut frame = div()
                .id(SharedString::from(format!("ed-part-{}", part.index())))
                .absolute()
                .left(px(left))
                .top(px(y * scale))
                .w(px(box_w))
                .h(px(box_h))
                .rounded(px(4.))
                .border_1()
                .border_color(if selected {
                    theme::accent()
                } else {
                    theme::accent_soft()
                })
                // 选中不刷底色：框是画在内容**上面**的，半透明底会把字蒙掉一块，
                // 看着像文字被裁了。选中态交给边框颜色 + 手柄高亮。
                .cursor(gpui::CursorStyle::OpenHand)
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                        let now = this.cfg();
                        let (px_x, px_y, px_w, px_h) = osd::part_rect(&now, part);
                        this.selected = Some(part);
                        this.resizing = None;
                        this.dragging = Some(MoveDrag {
                            part,
                            start: event.position,
                            origin: (px_x, px_y),
                            size: (px_w, px_h),
                            scale,
                            auto_width: part != OsdPart::Icon && now.part(part).width == 0,
                        });
                        this.focus.focus(window, cx);
                        cx.stop_propagation();
                        cx.notify();
                    }),
                )
                .on_click(
                    cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                        // 双击自定义文字：就地改字。
                        // **状态元素（`Title`）不参与** —— 它显示的是「当前在调什么」，
                        // 内容由状态决定，改它没有意义（双击只会在框里冒出「自动」两个字）。
                        if event.click_count() >= 2 && matches!(part, OsdPart::Text(_)) {
                            this.text = None;
                            this.begin_text(part, window, cx);
                        }
                        let _ = this;
                        cx.stop_propagation();
                    }),
                );

            for handle in OsdHandle::ALL {
                // 图标只有四角（改边长）；文字也只有四角（改字号）——
                // 边上的柄是改「容器宽」的，而文字已经没有那个概念了。
                if (part == OsdPart::Icon || part.is_text()) && !handle.is_corner() {
                    continue;
                }
                let dragging = self.resizing.map(|r| (r.part, r.handle)) == Some((part, handle));
                let hx = if handle.left() {
                    0.0
                } else if handle.right() {
                    box_w - knob
                } else {
                    (box_w - knob) / 2.0
                };
                let hy = if handle.top() {
                    0.0
                } else if handle.bottom() {
                    box_h - knob
                } else {
                    (box_h - knob) / 2.0
                };
                // 元素框是跟着卡片一起转的：转 90° / 270° 之后框的「横」「竖」
                // 在屏幕上换了向，箭头必须跟着换，否则竖边顶着横箭头 —— 一拖就觉得是反的。
                let cursor = match (handle.moves_x() != crossed, handle.moves_y() != crossed) {
                    // 对角线手柄的箭头要跟拖动方向一致：左上 / 右下是 ↖↘（Windows 的
                    // IDC_SIZENWSE，gpui 叫 ResizeUpLeftDownRight），右上 / 左下才是 ↗↙。
                    (true, true) => {
                        if matches!(handle, OsdHandle::TopLeft | OsdHandle::BottomRight) {
                            gpui::CursorStyle::ResizeUpLeftDownRight
                        } else {
                            gpui::CursorStyle::ResizeUpRightDownLeft
                        }
                    }
                    (true, false) => gpui::CursorStyle::ResizeLeftRight,
                    _ => gpui::CursorStyle::ResizeUpDown,
                };
                frame = frame.child(
                    div()
                        .id(SharedString::from(format!(
                            "ed-knob-{}-{}",
                            part.index(),
                            handle.index()
                        )))
                        .absolute()
                        .left(px(hx.max(0.0)))
                        .top(px(hy.max(0.0)))
                        .w(px(knob))
                        .h(px(knob))
                        .rounded(px(2.5))
                        .bg(if dragging || selected {
                            theme::accent()
                        } else {
                            theme::accent_soft()
                        })
                        .border_1()
                        .border_color(theme::accent())
                        .cursor(cursor)
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                                let now = this.cfg();
                                let (px_x, px_y, px_w, px_h) = osd::part_rect(&now, part);
                                let position = now.part(part);
                                this.selected = Some(part);
                                this.dragging = None;
                                this.guide = None;
                                this.resizing = Some(OsdResize {
                                    part,
                                    start: event.position,
                                    origin: (px_x, px_y, px_w, px_h),
                                    origin_size: position.size,
                                    origin_width: position.width,
                                    handle,
                                    scale,
                                });
                                this.focus.focus(window, cx);
                                cx.stop_propagation();
                                cx.notify();
                            }),
                        ),
                );
            }

            // 选中的元素在四角外再挂一个「转角度」的手柄：角上的方柄是改尺寸的，
            // 转圈的放到框外面去，两个不抢位置。
            if selected {
                let rotate_cursor = gpui::CursorStyle::Crosshair;
                for (ix, iy) in [(-1.0f32, -1.0f32), (1.0, -1.0), (1.0, 1.0), (-1.0, 1.0)] {
                    let size = 11.0;
                    let gap = 6.0;
                    let kx = if ix < 0.0 {
                        -gap - size
                    } else {
                        box_w + gap
                    };
                    let ky = if iy < 0.0 {
                        -gap - size
                    } else {
                        box_h + gap
                    };
                    let pivot = (
                        card_origin.0 + left + box_w / 2.0,
                        card_origin.1 + y * scale + box_h / 2.0,
                    );
                    frame = frame.child(
                        div()
                            .id(SharedString::from(format!(
                                "ed-rot-knob-{}-{}",
                                part.index(),
                                ((ix > 0.0) as u8) * 2 + (iy > 0.0) as u8
                            )))
                            .absolute()
                            .left(px(kx))
                            .top(px(ky))
                            .w(px(size))
                            .h(px(size))
                            .rounded_full()
                            .bg(theme::accent_soft())
                            .border_1()
                            .border_color(theme::accent())
                            .cursor(rotate_cursor)
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                                    let origin = this
                                        .canvas_bounds
                                        .get()
                                        .map(|bounds| {
                                            (
                                                bounds.origin.x.as_f32(),
                                                bounds.origin.y.as_f32(),
                                            )
                                        })
                                        .unwrap_or((0.0, 0.0));
                                    let center = Point::new(
                                        px(origin.0 + pivot.0),
                                        px(origin.1 + pivot.1),
                                    );
                                    this.selected = Some(part);
                                    this.dragging = None;
                                    this.resizing = None;
                                    this.guide = None;
                                    this.rotate_drag = Some(RotateDrag {
                                        target: RotationTarget::Part(part),
                                        center,
                                        start_angle: (event.position.y.as_f32()
                                            - center.y.as_f32())
                                        .atan2(
                                            event.position.x.as_f32() - center.x.as_f32(),
                                        ),
                                        origin: this.cfg().rotation_of(part),
                                    });
                                    this.focus.focus(window, cx);
                                    cx.stop_propagation();
                                    cx.notify();
                                }),
                            ),
                    );
                }
            }

            card = card.child(frame);
        }

        // 正在改字的那一条：就画在元素自己的框里，不另加边框 / 底色 / 缩进。
        //
        // 字号、颜色、对齐都跟成品一模一样，位置也取元素自己的矩形 ——
        // 眼睛看到的就是「字直接进到框里」。光标按**光标位置**插在两段字的中间，
        // 选中的那截刷一层底色：点击定位、拖选、方向键都靠这个反馈。
        if let Some(edit) = self.text.as_ref().filter(|_| preview.is_none()) {
            let part = edit.part;
            let (tx, ty, tw, th) = osd::part_rect(&cfg, part);
            let size = (cfg.part(part).size as f32) * scale;
            let align = cfg.align_of(part);
            let buffer = edit.buffer.clone();
            let empty = buffer.trim().is_empty();
            let len = utf16_len(&buffer);
            let caret = edit.caret.min(len);
            let anchor = edit.anchor.min(len);
            let (sel_start, sel_end) = if caret < anchor {
                (caret, anchor)
            } else {
                (anchor, caret)
            };
            // 空着时给一句灰字当提示：否则一片空白，看不出「这里能打字」。
            // （能进到这个框里的只有自定义文字，状态元素没有编辑入口。）
            let canvas = self.input_canvas(cx);
            // 光标条：1.5px 竖线，高度跟字号走。
            let caret_bar = || {
                div()
                    .w(px(1.5))
                    .h(px(size.max(10.)))
                    .flex_none()
                    .bg(theme::accent())
            };
            let mut children: Vec<AnyElement> = Vec::new();
            // 选中那截刷一层半透明强调色。元素框本身（选中态）也是 `accent_soft`，
            // 同色叠同色等于看不见，所以这里要更深一点。
            let selection_bg = {
                let mut tint = theme::accent();
                tint.alpha = 0.32;
                tint
            };
            if empty {
                children.push(caret_bar().into_any_element());
                children.push(
                    div()
                        .whitespace_nowrap()
                        .text_color(theme::text_tertiary())
                        .child(lang.pick("输入文字…", "Type here…").to_string())
                        .into_any_element(),
                );
            } else {
                let head = utf16_slice(&buffer, 0..sel_start);
                let middle = utf16_slice(&buffer, sel_start..sel_end);
                let tail = utf16_slice(&buffer, sel_end..len);
                if !head.is_empty() {
                    children.push(div().whitespace_nowrap().child(head).into_any_element());
                }
                // 光标落在选区起点 → 画在选中那截前面；落在终点 → 画在后面。
                if caret == sel_start {
                    children.push(caret_bar().into_any_element());
                }
                if sel_end > sel_start {
                    children.push(
                        div()
                            .whitespace_nowrap()
                            .rounded(px(2.))
                            .bg(selection_bg)
                            .child(middle)
                            .into_any_element(),
                    );
                }
                if caret != sel_start {
                    children.push(caret_bar().into_any_element());
                }
                if !tail.is_empty() {
                    children.push(div().whitespace_nowrap().child(tail).into_any_element());
                }
            }
            card = card.child(
                div()
                    .id("ed-text-edit")
                    .relative()
                    .absolute()
                    .left(px(tx * scale))
                    .top(px(ty * scale))
                    .w(px((tw * scale).max(60.)))
                    .h(px(th * scale))
                    .flex()
                    .items_center()
                    .gap(px(1.))
                    .when(align == OsdAlign::Center, |style| style.justify_center())
                    .when(align == OsdAlign::Right, |style| style.justify_end())
                    .text_size(px(size))
                    .text_color(theme::text_primary())
                    .cursor(gpui::CursorStyle::IBeam)
                    // 点自己不算失焦：不然刚点到字上就提交、光标也没了。
                    // 顺便把光标挪到点下去的那两字之间，按住拖动就是拉选区。
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            this.text_drag = true;
                            this.text_click(
                                event.position.x,
                                scale,
                                event.modifiers.shift,
                                window,
                                cx,
                            );
                        }),
                    )
                    .children(children)
                    .child(canvas),
            );
        }

        if let Some((guide_x, guide_y)) = self.guide.filter(|_| preview.is_none()) {
            if let Some(x) = guide_x {
                card = card.child(
                    div()
                        .absolute()
                        .left(px(x * scale))
                        .top_0()
                        .w(px(1.))
                        .h_full()
                        .bg(theme::accent()),
                );
            }
            if let Some(y) = guide_y {
                card = card.child(
                    div()
                        .absolute()
                        .left_0()
                        .top(px(y * scale))
                        .w_full()
                        .h(px(1.))
                        .bg(theme::accent()),
                );
            }
        }

        // 卡片自身的尺寸手柄：右边中点、下边中点、右下角。
        // 手柄**跨在边线上**（一半在外），命中区比视觉大一圈，抓起来才跟手。
        // 卡片转了之后，手柄要跟着转到**转后的边上**去（方块本身还是正的，
        // 和 PS 里转一下物件、角上的把手仍旧横平竖直一个道理）。
        let card_rot = cfg.rotation as f32;
        let card_center = (left + pixel_w / 2.0, top + pixel_h / 2.0);
        let mut card_knobs = div()
            .absolute()
            .left(px(left))
            .top(px(top))
            .w(px(pixel_w))
            .h(px(pixel_h));
        let edges = [
            (
                CardEdge::Right,
                pixel_w - 4.0,
                pixel_h / 2.0 - 14.0,
                8.0,
                28.0,
            ),
            (
                CardEdge::Bottom,
                pixel_w / 2.0 - 14.0,
                pixel_h - 4.0,
                28.0,
                8.0,
            ),
            (CardEdge::Corner, pixel_w - 6.0, pixel_h - 6.0, 12.0, 12.0),
        ];
        for (edge, x, y, w, h) in edges {
            let corner = edge == CardEdge::Corner;
            let (kx, ky) = rotate_about(
                card_center,
                (left + x + w / 2.0, top + y + h / 2.0),
                card_rot,
            );
            card_knobs = card_knobs.child(
                div()
                    .id(SharedString::from(format!("ed-card-{}", corner as u8)))
                    .absolute()
                    .left(px((kx - w / 2.0 - left).max(0.0)))
                    .top(px((ky - h / 2.0 - top).max(0.0)))
                    .w(px(w))
                    .h(px(h))
                    .rounded(px(if corner { 3. } else { 2. }))
                    .bg(theme::accent_soft())
                    .border_1()
                    .border_color(theme::accent())
                    // 右下角是 ↖↘（IDC_SIZENWSE）；写成 ↗↙ 的话箭头跟拖动方向正好拧着。
                    // 卡片转 90° / 270° 之后，这根柄其实站到了视觉上的另一条边上，
                    // 箭头得跟着换向 —— 否则竖着的边上顶着个横箭头，一拖就觉得是反的。
                    .cursor(if corner {
                        if crossed {
                            gpui::CursorStyle::ResizeUpRightDownLeft
                        } else {
                            gpui::CursorStyle::ResizeUpLeftDownRight
                        }
                    } else if (edge == CardEdge::Right) != crossed {
                        gpui::CursorStyle::ResizeLeftRight
                    } else {
                        gpui::CursorStyle::ResizeUpDown
                    })
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                            let now = this.cfg();
                            this.card_resize = Some(CardResize {
                                start: event.position,
                                width: now.width,
                                height: now.height,
                                edge,
                                scale,
                                left,
                                top,
                            });
                            this.focus.focus(window, cx);
                            cx.stop_propagation();
                            cx.notify();
                        }),
                    ),
            );
        }

        // 背景转角度：上边中点外面挂一个圆柄，拖着整张卡片转（阴影一起转）。
        // 卡片自己转着的时候，它也转到「转后的上边」外面去。
        {
            let knob_size = 13.0;
            let (kx, ky) = rotate_about(
                card_center,
                (left + pixel_w / 2.0, top - 24.0),
                card_rot,
            );
            card_knobs = card_knobs.child(
                div()
                    .id("ed-card-rotate")
                    .absolute()
                    .left(px(kx - knob_size / 2.0 - left))
                    .top(px(ky - knob_size / 2.0 - top))
                    .w(px(knob_size))
                    .h(px(knob_size))
                    .rounded_full()
                    .bg(theme::accent_soft())
                    .border_1()
                    .border_color(theme::accent())
                    .cursor(gpui::CursorStyle::Crosshair)
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                            let origin = this
                                .canvas_bounds
                                .get()
                                .map(|bounds| (bounds.origin.x.as_f32(), bounds.origin.y.as_f32()))
                                .unwrap_or((0.0, 0.0));
                            let center =
                                Point::new(px(origin.0 + card_center.0), px(origin.1 + card_center.1));
                            this.dragging = None;
                            this.resizing = None;
                            this.guide = None;
                            this.rotate_drag = Some(RotateDrag {
                                target: RotationTarget::Card,
                                center,
                                start_angle: (event.position.y.as_f32() - center.y.as_f32())
                                    .atan2(event.position.x.as_f32() - center.x.as_f32()),
                                origin: this.cfg().rotation as f32,
                            });
                            this.focus.focus(window, cx);
                            cx.stop_propagation();
                            cx.notify();
                        }),
                    ),
            );
        }

        // 拖动时贴在手柄上的一行尺寸读数，实时看得到改到多少。
        if self.card_resize.is_some() {
            card_knobs = card_knobs.child(
                div()
                    .absolute()
                    .left(px(0.))
                    .top(px(-22.))
                    .px(px(6.))
                    .h(px(18.))
                    .flex()
                    .items_center()
                    .rounded(px(4.))
                    .bg(theme::surface_elevated())
                    .border_1()
                    .border_color(theme::accent())
                    .text_size(px(10.5))
                    .text_color(theme::text_primary())
                    .child(format!("{} × {}", cfg.width, cfg.height)),
            );
        }

        // 整块（卡片 + 阴影 + 元素框 + 手柄）绕卡片中心转一圈：元素框跟着一起转，
        // 手柄本身还是正的（和 PS 里转物件一样）。
        let box_left = left - (box_w * scale - pixel_w) * 0.5;
        let box_top = top - (box_h * scale - pixel_h) * 0.5;
        let card = osd::card_frame(&draft, scale, card)
            .absolute()
            .left(px(box_left))
            .top(px(box_top));

        let panel = self.panel(&cfg, lang, cx);
        let picker = self.picker_overlay(lang, cx);
        let confirm = self.confirm_overlay(lang, cx);
        let menu = self.menu_overlay(&cfg, lang, cx);
        let close_confirm = self.close_confirm_overlay(lang, cx);
        let titlebar = self.titlebar(lang, cx);

        // 右上角 1:1 预览：设计画布是放大过的，字号到底多大得看这个。
        // 开了投影的话框要留出影子的位置，不然会被切掉。
        //
        // 这一小块**按卡片的原宽定宽、靠右摆**：卡片一旋转，转后的包围盒就变宽
        // 变高，按它算宽度的话整块（连那行「实际大小 100%」）会跟着左右跳 ——
        // 转个角度界面自己挪一下，看着像没对齐。用原宽定宽以后，不转时那行字
        // 正好齐着卡片两边，转了也只有里面的卡片在动。
        let shadow_pad = osd::shadow_margin(view);
        let col_w = (view.width as f32 + shadow_pad * 2.0)
            .min((canvas_w - 28.0).max(60.0));
        let preview_w = (box_w + shadow_pad * 2.0).min(col_w);
        let preview_h = (box_h + shadow_pad * 2.0).min((canvas_h - 56.0).max(24.0));
        let preview_left = (canvas_w - col_w - 14.0).max(8.0);
        let preview = div()
            .absolute()
            .left(px(preview_left))
            .top(px(10.))
            .w(px(col_w))
            .flex()
            .flex_col()
            .items_center()
            .gap(px(4.))
            .child(
                div()
                    .w_full()
                    .flex()
                    .items_center()
                    .justify_between()
                    .text_size(px(10.))
                    // 这一行是直接压在画布底色上的，跟着底色明暗翻。
                    .text_color(self.canvas_ink())
                    .child(lang.pick("实际大小 100%", "Actual size 100%"))
                    .child(format!("{}×{}", view.width, view.height)),
            )
            .child(
                div()
                    .w(px(preview_w))
                    .h(px(preview_h))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(4.))
                    // 不透明度是真的会作用在整扇窗上的（文字、图标、影子一起淡），
                    // 所以这里也整块淡，看着才是「实际大小」。
                    .child(div().opacity(view.alpha()).child(osd::card_frame(
                        view,
                        1.0,
                        osd::card_visual(view, &sample, 1.0, None, &measure, lang),
                    ))),
            );

        div()
            .size_full()
            .relative()
            .flex()
            .flex_col()
            .bg(theme::surface_sunken())
            .track_focus(&self.focus)
            // 松开左键 = 拖参数栏结束，宽度落盘。
            //
            // 必须显式收尾：`on_mouse_move` 里那个「手没按着就清掉」的分支要等下一次
            // 鼠标移动才会跑到，中间这段时间 `panel_drag` 一直挂着 —— 下一次鼠标划过
            // 就会按那个已经过期的起点算宽度，右栏会莫名其妙跳到几百像素。
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _: &gpui::MouseUpEvent, _window, cx| {
                    if this.panel_drag.take().is_some() {
                        let width = this.panel_w();
                        let _ = this
                            .config
                            .borrow_mut()
                            .update(|config| config.editor_panel_w = width);
                        cx.notify();
                    }
                }),
            )
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseDownEvent, _window, cx| {
                    // 点别处 = 失焦：正在改的文字 / 手打的数值直接保存。
                    // （正在编辑的那个框和工具条上的按钮都会 stop_propagation，
                    //   所以点它们不会走到这里。）
                    if this.text.is_some() {
                        this.commit_text(cx);
                    }
                    if this.num.is_some() {
                        this.commit_num(cx);
                    }
                    // 点画布 / 参数栏的空白：收起取色器和状态下拉菜单。
                    if this.picker.take().is_some() {
                        cx.notify();
                    }
                    if this.menu.take().is_some() {
                        cx.notify();
                    }
                    let _ = event;
                }),
            )
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _window, cx| {
                let key = event.keystroke.key.as_str();
                // 确认弹窗开着时只有它说的算：回车 = 恢复，Esc = 取消，别的键全吞掉，
                // 免得下面的方向键 / Delete 在弹窗后面偷偷改布局。
                if this.confirm_reset {
                    match key {
                        "enter" => this.apply_reset(cx),
                        "escape" => {
                            this.confirm_reset = false;
                            cx.notify();
                        }
                        _ => {}
                    }
                    cx.stop_propagation();
                    return;
                }
                // 取色器正在手输色号：所有按键归它。
                if this.picker_key(event, cx) {
                    cx.stop_propagation();
                    return;
                }
                // 正在手打数值（双击宽度 / 高度 / 圆角 / 不透明度）：只认数字。
                if this.num_key(event, cx) {
                    cx.stop_propagation();
                    return;
                }
                // 正在编辑文字：回车提交、Esc 放弃，其余交给输入法 / 文本处理。
                if this.text.is_some() {
                    if key == "enter" {
                        cx.stop_propagation();
                        this.commit_text(cx);
                        return;
                    }
                    if key == "escape" {
                        cx.stop_propagation();
                        this.text = None;
                        this.text_drag = false;
                        cx.notify();
                        return;
                    }
                    // Ctrl+Z 撤销。放在输入法判断之前：这是控制键组合，
                    // 拼字过程中按它也该退掉上一步，而不是被输入法吞掉。
                    if key == "z" && event.keystroke.modifiers.control {
                        cx.stop_propagation();
                        if let Some(edit) = this.text.as_mut() {
                            if edit.undo() {
                                cx.notify();
                            }
                        }
                        return;
                    }
                    // 输入法正在拼字：方向键（选候选）和退格（删候选）都归输入法，
                    // 这里抢过去的话中州 / 微软拼音就没法选字了。
                    let composing = this
                        .text
                        .as_ref()
                        .and_then(|edit| edit.marked.clone())
                        .is_some();
                    if !composing {
                        let shift = event.keystroke.modifiers.shift;
                        let control = event.keystroke.modifiers.control;
                        match key {
                            "left" | "right" | "home" | "end" => {
                                cx.stop_propagation();
                                this.move_text_caret(key, shift, cx);
                                return;
                            }
                            "backspace" => {
                                cx.stop_propagation();
                                this.delete_text(false, cx);
                                return;
                            }
                            // 只有自定义文字会进编辑，Delete 在这里就是删后面的字，
                            // 不是「删掉这个元素」。
                            "delete" => {
                                cx.stop_propagation();
                                this.delete_text(true, cx);
                                return;
                            }
                            "a" if control => {
                                cx.stop_propagation();
                                if let Some(edit) = this.text.as_mut() {
                                    edit.anchor = 0;
                                    edit.caret = utf16_len(&edit.buffer);
                                    cx.notify();
                                }
                                return;
                            }
                            "c" | "x" if control => {
                                cx.stop_propagation();
                                let picked =
                                    this.text.as_ref().filter(|edit| edit.has_selection()).map(
                                        |edit| {
                                            let range = edit.selection();
                                            utf16_slice(&edit.buffer, range)
                                        },
                                    );
                                if let Some(picked) = picked {
                                    cx.write_to_clipboard(gpui::ClipboardItem::new_string(picked));
                                    if key == "x" {
                                        this.delete_text(false, cx);
                                        // 有选区时 delete_text 删的正是选区。
                                    }
                                }
                                return;
                            }
                            _ => {}
                        }
                    }
                    if key == "v" && event.keystroke.modifiers.control {
                        cx.stop_propagation();
                        let text = cx
                            .read_from_clipboard()
                            .and_then(|item| item.text().map(|text| text.to_string()));
                        if let Some(text) = text {
                            this.replace_text(None, &text, false, cx);
                        }
                        return;
                    }
                    if let Some(text) = event.keystroke.key_char.as_ref() {
                        if !event.keystroke.modifiers.control {
                            cx.stop_propagation();
                            this.replace_text(None, text, false, cx);
                            return;
                        }
                    }
                    return;
                }
                if key == "escape" {
                    if this.picker.take().is_some() {
                        cx.notify();
                        cx.stop_propagation();
                        return;
                    }
                    this.selected = None;
                    this.guide = None;
                    cx.notify();
                    cx.stop_propagation();
                    return;
                }
                // 布局改动：Ctrl+Z 退一步，Ctrl+Shift+Z 再往前一步（和 Photoshop 一致）。
                // 放在文字分支后面 —— 正在改字时那个分支已经先把 Ctrl+Z 用掉了，
                // 那里退的是文字本身，不会一按就把整份布局退回去。
                if key == "z" && event.keystroke.modifiers.control {
                    cx.stop_propagation();
                    if event.keystroke.modifiers.shift {
                        this.redo(cx);
                    } else {
                        this.undo(cx);
                    }
                    return;
                }
                if key == "delete" {
                    if this.remove_selected(cx) {
                        cx.stop_propagation();
                    }
                    return;
                }
                let big = event.keystroke.modifiers.shift;
                if this.nudge(key, big, cx) {
                    cx.stop_propagation();
                }
            }))
            .on_mouse_move(
                cx.listener(move |this, event: &MouseMoveEvent, window, cx| {
                    let pressed = matches!(event.pressed_button, Some(MouseButton::Left));

                    // 拖参数栏左边缘：往左拖变宽、往右拖变窄。松手才写盘。
                    if let Some((start_x, start_w)) = this.panel_drag {
                        if !pressed {
                            this.panel_drag = None;
                            let width = this.panel_w();
                            let _ = this
                                .config
                                .borrow_mut()
                                .update(|config| config.editor_panel_w = width);
                        } else {
                            let dx = event.position.x.as_f32() - start_x;
                            this.panel_w = (start_w - dx).clamp(PANEL_W_MIN, PANEL_W_MAX);
                        }
                        cx.notify();
                        return;
                    }

                    // 拖选文字：光标跟着鼠标走，锚点留在按下去的那一格。
                    // 必须排在别的拖动之前 —— 编辑框已经把按下的那一下吞掉了，
                    // 这里只负责「按住往哪儿移」。
                    if this.text_drag {
                        if pressed {
                            this.text_click(event.position.x, scale, true, window, cx);
                        } else {
                            this.text_drag = false;
                            cx.notify();
                        }
                        return;
                    }

                    // 拖尺寸滑条。
                    if this.slider_drag.is_some() {
                        if pressed {
                            this.drag_slider(event.position, cx);
                        } else {
                            this.slider_drag = None;
                            cx.notify();
                        }
                        return;
                    }

                    // 按住角手柄转圈（元素 / 背景）。
                    if this.rotate_drag.is_some() {
                        if pressed {
                            this.drag_rotate(event.position, event.modifiers.shift, cx);
                        } else {
                            this.rotate_drag = None;
                            cx.notify();
                        }
                        return;
                    }

                    // 拖元素角度滑条。
                    if this.rotation_drag.is_some() {
                        if pressed {
                            this.drag_rotation(event.position, cx);
                        } else {
                            this.rotation_drag = None;
                            cx.notify();
                        }
                        return;
                    }

                    // 拖元素列表调图层顺序。
                    if this.layer_drag.is_some() {
                        if pressed {
                            this.drag_layer(event.position, cx);
                        } else {
                            this.layer_drag = None;
                            cx.notify();
                        }
                        return;
                    }

                    // 拖卡片边缘：改卡片尺寸。
                    if let Some(resize) = this.card_resize {
                        if !pressed {
                            this.card_resize = None;
                            cx.notify();
                            return;
                        }
                        let (dx, dy) = unrotate_delta(
                            (event.position.x - resize.start.x).as_f32() / resize.scale.max(0.1),
                            (event.position.y - resize.start.y).as_f32() / resize.scale.max(0.1),
                            this.cfg().rotation as f32,
                        );
                        let mut width = resize.width;
                        let mut height = resize.height;
                        if matches!(resize.edge, CardEdge::Right | CardEdge::Corner) {
                            width = (resize.width as f32 + dx).round().clamp(120.0, 900.0) as u32;
                        }
                        if matches!(resize.edge, CardEdge::Bottom | CardEdge::Corner) {
                            height = (resize.height as f32 + dy).round().clamp(28.0, 320.0) as u32;
                        }
                        this.update(|osd| {
                            osd.set_card(|card| {
                                card.width = width;
                                card.height = height;
                            });
                        });
                        cx.notify();
                        return;
                    }

                    if let Some(resize) = this.resizing {
                        if !pressed {
                            this.resizing = None;
                            cx.notify();
                            return;
                        }
                        let (dx, dy) = unrotate_delta(
                            (event.position.x - resize.start.x).as_f32() / resize.scale.max(0.1),
                            (event.position.y - resize.start.y).as_f32() / resize.scale.max(0.1),
                            this.cfg().rotation as f32,
                        );
                        this.apply_resize(resize, dx, dy, cx);
                        return;
                    }

                    let Some(drag) = this.dragging else {
                        return;
                    };
                    if !pressed {
                        this.dragging = None;
                        this.guide = None;
                        cx.notify();
                        return;
                    }
                    let cfg = this.cfg();
                    let (card_w, card_h) = osd::card_size(&cfg);
                    let moved_x = (event.position.x - drag.start.x).as_f32();
                    let moved_y = (event.position.y - drag.start.y).as_f32();
                    // 手抖不算拖动：屏幕坐标 3px 以内一动不动，也不吸附。
                    // 双击时鼠标几乎总会挪 1~2px，一挪就吸到参考线上、框跟着跳走，
                    // 松手的位置落到框外面 → gpui 判「不是同一个元素」不派发 click，
                    // 表现就是「第一次能双击、之后再双击没反应」。
                    if moved_x.abs() < 3.0 && moved_y.abs() < 3.0 {
                        return;
                    }
                    // 卡片转着的时候，光标往右推在卡片自己的坐标系里是斜的 ——
                    // 位移先转回去，元素才跟着光标走。
                    let (moved_x, moved_y) =
                        unrotate_delta(moved_x, moved_y, this.cfg().rotation as f32);
                    let dx = moved_x / drag.scale.max(0.1);
                    let dy = moved_y / drag.scale.max(0.1);
                    // 和左边对称：最多贴到卡片右边缘（宽度已经在 part_rect 里按这个上限夹过）。
                    let x = (drag.origin.0 + dx).clamp(0.0, (card_w - drag.size.0).max(0.0));
                    let y = (drag.origin.1 + dy).clamp(0.0, (card_h - drag.size.1).max(0.0));
                    let (x, y, guide_x, guide_y) = if cfg.snap {
                        osd::snap_position(&cfg, drag.part, x, y)
                    } else {
                        (x, y, None, None)
                    };
                    this.guide = Some((guide_x, guide_y));
                    let auto = drag.auto_width;
                    let size = drag.size;
                    let part = drag.part;
                    this.update(|osd| {
                        // 文字没有容器宽，自动宽度对它是「内容多宽就多宽」，不用固化。
                        if auto && !part.is_text() {
                            osd.set_part_width(part, size.0.round() as u32);
                        }
                        osd.set_part(part, x / card_w, y / card_h);
                    });
                    cx.notify();
                }),
            )
            .child(titlebar)
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .child(
                        div()
                            .relative()
                            .flex_1()
                            .h_full()
                            .overflow_hidden()
                            // 画布底色是可选的：同一张卡片摆在深底 / 浅底上好不好看
                            // 完全是两回事，调背景色的时候得能当场换一个桌面看。
                            .bg(self.canvas_bg())
                            // 画布自己的矩形（画布坐标 → 窗口坐标要用）。
                            .child(measure_canvas(self.canvas_bounds.clone()))
                            .child(card)
                            .child(preview)
                            .child(card_knobs)
                            .child(self.snap_pill(&cfg, lang, cx))
                            .child(self.canvas_bg_bar(lang, cx)),
                    )
                    .child(self.panel_grip(cx))
                    .child(panel),
            )
            .children(picker)
            .children(confirm)
            .children(menu)
            .children(close_confirm)
    }
}

impl OsdEditor {
    /// 自绘标题栏：整条（除了关闭按钮）都能拖窗口。
    ///
    /// 窗口是 `WindowDecorations::Client`，系统标题栏被去掉之后必须自己接管拖动，
    /// 否则窗口只能靠拉边框移动 —— 那手感根本不像个正常窗口。
    fn titlebar(&self, lang: Lang, cx: &mut Context<Self>) -> AnyElement {
        let dirty = self.is_dirty();
        let mut bar = div()
            .h(px(TITLEBAR_H))
            .flex_none()
            .flex()
            .items_center()
            .gap(px(8.))
            .pl(px(14.))
            .pr(px(10.))
            .border_b_1()
            .border_color(theme::border())
            .bg(theme::surface());

        // 标题拖动区：icon + 标题 + （有改动时）「未保存」提示和两个按钮。
        // 提示就贴在标题右边（左上角），按钮自己 `stop_propagation`，不会变成拖窗口。
        let mut drag_area = div()
            .id("ed-titlebar")
            .flex_1()
            .h_full()
            .flex()
            .items_center()
            .gap(px(8.))
            // 双击 = 最大化 / 还原（跟系统标题栏一样）；拖动等鼠标动起来才交给系统。
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, event: &gpui::MouseDownEvent, window, _| {
                    if event.click_count >= 2 {
                        this.titlebar_armed = false;
                        window.zoom_window();
                    } else {
                        this.titlebar_armed = true;
                    }
                }),
            )
            .on_mouse_move(
                cx.listener(|this, event: &gpui::MouseMoveEvent, window, _| {
                    if this.titlebar_armed && event.pressed_button == Some(MouseButton::Left) {
                        this.titlebar_armed = false;
                        window.start_window_move();
                    }
                }),
            )
            .on_mouse_up(
                MouseButton::Left,
                cx.listener(|this, _, _, _| this.titlebar_armed = false),
            )
            .child(ui::icon("settings", 15.0, theme::text_secondary()))
            .child(
                div()
                    .text_size(px(13.))
                    .font_weight(gpui::FontWeight::SEMIBOLD)
                    .text_color(theme::text_primary())
                    .child(lang.pick("OSD 布局编辑器", "OSD layout editor")),
            );

        if dirty {
            drag_area = drag_area
                .child(
                    div()
                        .flex_none()
                        .flex()
                        .items_center()
                        .gap(px(5.))
                        .child(
                            div()
                                .w(px(6.))
                                .h(px(6.))
                                .flex_none()
                                .rounded(px(3.))
                                .bg(theme::accent()),
                        )
                        .child(
                            div()
                                .text_size(px(10.5))
                                .text_color(theme::text_secondary())
                                .child(lang.pick("未保存的更改", "Unsaved changes")),
                        ),
                )
                .child(
                    div()
                        .id("ed-save")
                        .flex_none()
                        .h(px(18.))
                        .px(px(8.))
                        .flex()
                        .items_center()
                        .rounded(px(4.))
                        .bg(theme::accent_soft())
                        .border_1()
                        .border_color(theme::accent())
                        .cursor_pointer()
                        .text_size(px(10.5))
                        .text_color(theme::text_primary())
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| {
                                cx.stop_propagation();
                                this.accept_changes(cx);
                            }),
                        )
                        .child(lang.pick("保存", "Save")),
                )
                .child(
                    div()
                        .id("ed-discard")
                        .flex_none()
                        .h(px(18.))
                        .px(px(8.))
                        .flex()
                        .items_center()
                        .rounded(px(4.))
                        .bg(theme::track())
                        .cursor_pointer()
                        .text_size(px(10.5))
                        .text_color(theme::text_secondary())
                        .hover(|style| style.bg(theme::surface_hover()))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|this, _, _, cx| {
                                cx.stop_propagation();
                                this.discard_changes(cx);
                            }),
                        )
                        .child(lang.pick("放弃", "Discard")),
                );
        }
        bar = bar.child(drag_area);

        // 撤销 / 重做：有历史才露面（跟「保存 / 放弃」一样，用不着的时候不占地方）。
        // 图标本身就说明一切，悬停时用系统的提示条补一句快捷键。
        let (can_undo, can_redo) = {
            let history = self.history.borrow();
            (history.can_undo(), history.can_redo())
        };
        if can_undo {
            bar = bar.child(
                div()
                    .id("ed-undo")
                    .size(px(24.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(theme::RADIUS_SMALL))
                    .cursor_pointer()
                    .hover(|style| style.bg(theme::surface_hover()))
                    // 和旁边的关闭按钮一样用 mouse_down：标题栏这一带 click 偶尔收不到。
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            cx.stop_propagation();
                            this.undo(cx);
                        }),
                    )
                    .child(ui::icon("undo", 14.0, theme::text_secondary())),
            );
        }
        if can_redo {
            bar = bar.child(
                div()
                    .id("ed-redo")
                    .size(px(24.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(theme::RADIUS_SMALL))
                    .cursor_pointer()
                    .hover(|style| style.bg(theme::surface_hover()))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(|this, _, _, cx| {
                            cx.stop_propagation();
                            this.redo(cx);
                        }),
                    )
                    .child(ui::icon("redo", 14.0, theme::text_secondary())),
            );
        }

        bar.child(
            div()
                .id("ed-close")
                .size(px(24.))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(theme::RADIUS_SMALL))
                .cursor_pointer()
                .hover(|style| style.bg(theme::surface_hover()))
                // 用 mouse_down（和保存 / 放弃一致）：标题栏这一带用 click 偶尔收不到。
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, window, cx| {
                        cx.stop_propagation();
                        // 有没保存的改动就拦一下：问清楚再关。
                        if this.is_dirty() {
                            this.confirm_close = true;
                            cx.notify();
                        } else {
                            window.remove_window();
                        }
                    }),
                )
                .child(ui::icon("close", 14.0, theme::text_secondary())),
        )
        .into_any_element()
    }

    /// 关闭窗口前的确认：保存并关闭 / 不保存 / 取消。
    fn close_confirm_overlay(&self, lang: Lang, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.confirm_close {
            return None;
        }
        let button = |id: &'static str, label: &'static str, primary: bool| {
            div()
                .id(SharedString::from(format!("ed-close-{id}")))
                .flex_1()
                .h(px(26.))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(5.))
                .cursor_pointer()
                .text_size(px(11.5))
                .when(primary, |style| {
                    style
                        .bg(theme::accent_soft())
                        .border_1()
                        .border_color(theme::accent())
                        .text_color(theme::text_primary())
                })
                .when(!primary, |style| {
                    style.bg(theme::track()).text_color(theme::text_secondary())
                })
                .hover(|style| style.bg(theme::surface_hover()))
                .child(label)
        };
        Some(
            div()
                .id("ed-close-backdrop")
                .absolute()
                .left(px(0.))
                .top(px(0.))
                .size_full()
                .occlude()
                .bg(gpui::rgb_to_hsla(gpui::rgba(0x00000073)))
                .on_mouse_down(MouseButton::Left, cx.listener(|this, _, _, cx| {
                    // 点外面 = 取消（留在编辑器里，改动还在，随时可以再保存）。
                    this.confirm_close = false;
                    cx.notify();
                }))
                .child(
                    div()
                        .absolute()
                        .left(px(0.))
                        .right(px(0.))
                        .top(px(0.))
                        .bottom(px(0.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            div()
                                .w(px(300.))
                                .flex()
                                .flex_col()
                                .gap(px(10.))
                                .p(px(14.))
                                .rounded(px(theme::RADIUS_WINDOW))
                                .bg(theme::surface_elevated())
                                .border_1()
                                .border_color(theme::border_strong())
                                .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                    cx.stop_propagation()
                                })
                                .child(
                                    div()
                                        .text_size(px(12.5))
                                        .font_weight(gpui::FontWeight::SEMIBOLD)
                                        .text_color(theme::text_primary())
                                        .child(lang.pick(
                                            "有未保存的更改",
                                            "Unsaved changes",
                                        )),
                                )
                                .child(
                                    div()
                                        .text_size(px(11.))
                                        .text_color(theme::text_secondary())
                                        .child(lang.pick(
                                            "关掉编辑器之前要不要保存这次调整？选「不保存」会恢复成打开时的样子。",
                                            "Save the changes before closing? Choosing Don't save restores what you had when the editor opened.",
                                        )),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(px(6.))
                                        .child(
                                            button("save", lang.pick("保存并关闭", "Save and close"), true)
                                                .on_mouse_down(
                                                    MouseButton::Left,
                                                    cx.listener(|this, _, window, cx| {
                                                        cx.stop_propagation();
                                                        this.accept_changes(cx);
                                                        this.confirm_close = false;
                                                        window.remove_window();
                                                    }),
                                                ),
                                        )
                                        .child(
                                            button(
                                                "drop",
                                                lang.pick("不保存", "Don't save"),
                                                false,
                                            )
                                            .on_mouse_down(
                                                MouseButton::Left,
                                                cx.listener(|this, _, window, cx| {
                                                    cx.stop_propagation();
                                                    this.discard_changes(cx);
                                                    this.confirm_close = false;
                                                    window.remove_window();
                                                }),
                                            ),
                                        )
                                        .child(
                                            button("cancel", lang.pick("取消", "Cancel"), false)
                                                .on_mouse_down(
                                                    MouseButton::Left,
                                                    cx.listener(|this, _, _, cx| {
                                                        cx.stop_propagation();
                                                        this.confirm_close = false;
                                                        cx.notify();
                                                    }),
                                                ),
                                        ),
                                ),
                        ),
                ),
        )
        .map(|element| element.into_any_element())
    }

    /// 打开 / 关闭某个颜色的取色器。
    fn open_picker(&mut self, slot: ColorSlot, cx: &mut Context<Self>) {
        if self.picker.as_ref().map(|picker| picker.slot) == Some(slot) {
            self.picker = None;
            cx.notify();
            return;
        }
        let value = slot.get(self.config.borrow().get());
        // 空串 = 跟随主题：用主题色作为起点，一动就固化成具体色号。
        let cfg = self.cfg();
        // 这一格现在摆的是预设色（或「跟随」）时，翻出上次调过的颜色：
        // 挑预设只是个比较，不该把调了半天的色号冲掉。
        let remembered = crate::ui::picker::resume_value(
            slot_presets(slot),
            &value,
            self.picker_memory.get(&slot).map(String::as_str),
        );
        let rgb = remembered
            .as_deref()
            .and_then(parse_rgb)
            .or_else(|| parse_rgb(&value))
            .unwrap_or(match slot {
                ColorSlot::Card => (0x15, 0x15, 0x19),
                ColorSlot::Text => (0xf6, 0xf6, 0xf9),
                ColorSlot::Accent => (0x7c, 0x9c, 0xff),
                ColorSlot::Shadow => (0x00, 0x00, 0x00),
                ColorSlot::CanvasBg => (0x00, 0x00, 0x00),
                ColorSlot::BarTrack => parse_rgb(&theme::track_hex()).unwrap_or((0x2b, 0x2b, 0x2f)),
                ColorSlot::BarMuted => {
                    parse_rgb(&theme::muted_fill_hex()).unwrap_or((0x9a, 0xa3, 0xb2))
                }
                // 元素没单独设色：从它自己跟随的那个颜色起步
                // （音量条跟强调色，其余跟文字色）。
                ColorSlot::Part(part) => {
                    let base = if part == OsdPart::Bar {
                        cfg.colors.accent.clone()
                    } else {
                        cfg.colors.text.clone()
                    };
                    parse_rgb(&base).unwrap_or((0xf6, 0xf6, 0xf9))
                }
            });
        // 调「静音时音量条」那段：画布先切到已静音，边拖边能看见。
        self.preview_muted = slot == ColorSlot::BarMuted;
        let (h, s, v) = rgb_to_hsv(rgb.0, rgb.1, rgb.2);
        self.picker = Some(Picker {
            slot,
            picker: crate::ui::Picker::new(h, s, v, hex_of(rgb)),
            picking: false,
        });
        cx.notify();
    }

    /// 值变了：写回配置并固化色号。
    fn apply_hsv(&mut self, h: f32, s: f32, v: f32, cx: &mut Context<Self>) {
        let Some(slot) = self.picker.as_ref().map(|picker| picker.slot) else {
            return;
        };
        let hex = hex_of(hsv_to_rgb(h, s, v));
        if let Some(picker) = self.picker.as_mut() {
            picker.picker.set_hsv(h, s, v);
            picker.picker.hex = hex.clone();
        }
        self.set_color(slot, hex);
        cx.notify();
    }

    /// 手输 / 粘贴色号时的按键处理；返回 true 表示这一键归取色器。
    fn picker_key(&mut self, event: &KeyDownEvent, cx: &mut Context<Self>) -> bool {
        let Some(mut buffer) = self
            .picker
            .as_ref()
            .and_then(|picker| picker.picker.edit.clone())
        else {
            return false;
        };
        // 刚点开输入框：第一下（打字 / 退格 / 粘贴）把整串换掉，
        // 不用先退格七次才能输入新的。回车 / Esc 不算「换掉」，照原样提交 / 放弃。
        let fresh = self
            .picker
            .as_ref()
            .map(|picker| picker.picker.edit_fresh)
            .unwrap_or(false);
        let key = event.keystroke.key.as_str();
        let typed = key.chars().count() == 1;
        let replaces = fresh && (typed || matches!(key, "backspace" | "v"));
        if replaces {
            buffer.clear();
            if let Some(picker) = self.picker.as_mut() {
                picker.picker.edit_fresh = false;
            }
        }
        match key {
            "enter" | "escape" => {
                let apply = key == "enter";
                if let Some(picker) = self.picker.as_mut() {
                    picker.picker.edit = None;
                }
                if apply {
                    if let Some(rgb) = parse_rgb(&buffer) {
                        let (h, s, v) = rgb_to_hsv(rgb.0, rgb.1, rgb.2);
                        self.apply_hsv(h, s, v, cx);
                        return true;
                    }
                }
                cx.notify();
                true
            }
            "backspace" => {
                buffer.pop();
                if let Some(picker) = self.picker.as_mut() {
                    picker.picker.edit = Some(buffer);
                }
                cx.notify();
                true
            }
            "v" if event.keystroke.modifiers.control => {
                if let Some(text) = cx
                    .read_from_clipboard()
                    .and_then(|item| item.text().map(|text| text.to_string()))
                {
                    push_hex(&mut buffer, &text);
                    if let Some(picker) = self.picker.as_mut() {
                        picker.picker.edit = Some(buffer);
                    }
                    cx.notify();
                }
                true
            }
            _ => {
                if let Some(text) = event.keystroke.key_char.as_ref() {
                    push_hex(&mut buffer, text);
                    if let Some(picker) = self.picker.as_mut() {
                        picker.picker.edit = Some(buffer);
                    }
                }
                cx.notify();
                true
            }
        }
    }

    /// 屏幕取色：把窗口藏起来 → 等一次左键 → 采样 → 露回来。
    fn start_screen_pick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(raw) = platform::raw_hwnd(window) else {
            return;
        };
        let Some(slot) = self.picker.as_ref().map(|picker| picker.slot) else {
            return;
        };
        if self
            .picker
            .as_ref()
            .map(|picker| picker.picking)
            .unwrap_or(false)
        {
            return;
        }
        if let Some(picker) = self.picker.as_mut() {
            picker.picking = true;
            picker.picker.edit = None;
        }
        platform::show_raw(raw, false);
        cx.notify();

        let store = self.config.clone();
        let (tx, rx) = std::sync::mpsc::channel::<Option<String>>();
        std::thread::spawn(move || {
            // 先等按在按钮上的那一发松开，否则会立刻当成「取色点击」。
            while platform::mouse_left_down() {
                std::thread::sleep(PICK_TICK);
            }
            let mut picked = None;
            // 最多等 20 秒：用户按了取色又改主意时，窗口必须自己回来。
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
            while std::time::Instant::now() < deadline {
                if platform::escape_down() {
                    break;
                }
                if platform::mouse_left_down() {
                    let (x, y) = platform::cursor_screen();
                    picked = platform::screen_color(x, y);
                    break;
                }
                std::thread::sleep(PICK_TICK);
            }
            // 无论如何先把窗口放回来，不能让用户面对一个消失的编辑器。
            platform::show_raw(raw, true);
            let hex =
                picked.map(|value| hex_of(((value >> 16) as u8, (value >> 8) as u8, value as u8)));
            let _ = tx.send(hex);
        });

        cx.spawn(async move |this, cx| {
            let mut result = None;
            // 最多等 30 秒：取色线程把窗口放回来之后这里一定会拿到结果。
            for _ in 0..1500 {
                match rx.try_recv() {
                    Ok(value) => {
                        result = value;
                        break;
                    }
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
                    Err(std::sync::mpsc::TryRecvError::Empty) => {
                        cx.background_executor().timer(PICK_TICK).await;
                    }
                }
            }
            if let Some(hex) = result.as_ref() {
                let hex = hex.clone();
                let _ = store.borrow_mut().update(|config| slot.set(config, hex));
            }
            let _ = this.update(cx, |this, cx| {
                if let Some(picker) = this.picker.as_mut() {
                    picker.picking = false;
                    if let Some(rgb) = result.as_ref().and_then(|text| parse_rgb(text)) {
                        picker.picker.set_hex(hex_of(rgb));
                    }
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 真的把当前模板恢复成默认排布（确认弹窗点了「恢复默认」才走这里）。
    fn apply_reset(&mut self, cx: &mut Context<Self>) {
        // 元素排布 + 卡片宽高一起回出厂：用户点这个基本就是「我把它拖乱了，
        // 回到原来那样」。配色和圆角不在此列，那是另一类改动。
        self.selected = None;
        self.guide = None;
        self.confirm_reset = false;
        // 正在改的那条文字连同它一起清掉：恢复默认会把自建文字整条删掉，
        // 留着这份编辑状态的话，下一次失焦提交又把它写回去（等于没恢复）。
        self.text = None;
        self.text_drag = false;
        self.update(|osd| osd.reset_layout());
        cx.notify();
    }

    /// 「恢复默认布局」的确认弹窗：压暗整窗 + 居中一张小卡片。
    ///
    /// 一下就抹掉整张模板的排布，所以还是要问一句 —— 虽然现在能 Ctrl+Z 退回来，
    /// 但按错时多半还没反应过来，先确认一次更省事。
    /// 点背景、按 Esc、点「取消」都只是关掉，什么都不改。
    fn confirm_overlay(&self, lang: Lang, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.confirm_reset {
            return None;
        }
        let template = match self.cfg().template() {
            crate::config::OsdTemplate::Media => lang.pick("媒体键", "Media keys"),
            crate::config::OsdTemplate::Brightness => lang.pick("亮度", "Brightness"),
            crate::config::OsdTemplate::Volume => lang.pick("音量", "Volume"),
        };
        let body = if lang.is_en() {
            format!(
                "Restore the \"{template}\" template to factory settings? Element positions, sizes, visibility and layer order, plus the card size, corners, opacity, colors and shadow all go back. Text elements are removed and the status text goes back to automatic."
            )
        } else {
            format!(
                "把「{template}」模板恢复成出厂？元素的位置、大小、显隐、图层顺序，以及卡片尺寸、圆角、不透明度、配色和阴影都会回到默认；自己加的文字会删掉，状态文字回到自动内容。"
            )
        };

        // 弹窗自己带一张缩略图。画布那半边其实已经按默认排布画了，但弹窗一挡
        // 就看不见了，所以在确认按钮上面再摆一份 —— 决定之前先看清换来的样子。
        const THUMB_W: f32 = 292.0;
        const THUMB_H: f32 = 132.0;
        let mut draft = self.cfg();
        draft.reset_layout();
        let sample = sample_state(&draft, lang, None);
        let (card_w, card_h) = osd::card_size(&draft);
        let thumb_scale = (THUMB_W / card_w).min(THUMB_H / card_h).min(3.0);
        let thumb_w = card_w * thumb_scale;
        let thumb_h = card_h * thumb_scale;

        Some(
            div()
                .id("ed-confirm-backdrop")
                .absolute()
                .left(px(0.))
                .top(px(0.))
                .size_full()
                .occlude()
                // 半透明黑压一层：只是把注意力收到弹窗上，别把编辑器整个盖死。
                .bg(gpui::rgb_to_hsla(gpui::rgba(0x00000073)))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _, _, cx| {
                        // 点外面 = 取消。
                        this.confirm_reset = false;
                        cx.notify();
                    }),
                )
                .child(
                    div()
                        .id("ed-confirm")
                        .absolute()
                        .left(px(0.))
                        .right(px(0.))
                        .top(px(0.))
                        .bottom(px(0.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .child(
                            div()
                                .w(px(320.))
                                .flex()
                                .flex_col()
                                .gap(px(8.))
                                .p(px(14.))
                                .rounded(px(theme::RADIUS_WINDOW))
                                .bg(theme::surface_elevated())
                                .border_1()
                                .border_color(theme::border_strong())
                                // 点在卡片上不算点空白处。
                                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                                .child(
                                    div()
                                        .text_size(px(12.5))
                                        .font_weight(gpui::FontWeight::SEMIBOLD)
                                        .text_color(theme::text_primary())
                                        .child(lang.pick("恢复默认布局？", "Reset layout?")),
                                )
                                .child(
                                    div()
                                        .text_size(px(11.))
                                        .text_color(theme::text_secondary())
                                        .child(body),
                                )
                                .child(
                                    // 默认排布预览：底色压深一档，卡片本身按真实比例缩放。
                                    div()
                                        .w_full()
                                        .h(px(THUMB_H + 12.0))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .rounded(px(theme::RADIUS_SMALL))
                                        .bg(theme::surface_sunken())
                                        .child(div().w(px(thumb_w)).h(px(thumb_h)).child(
                                            osd::card_visual(
                                                &draft,
                                                &sample,
                                                thumb_scale,
                                                None,
                                                &osd::TextMeasure::approx(),
                                                lang,
                                            ),
                                        )),
                                )
                                .child(
                                    // 画布那半边也已经是默认排布了，说一句免得用户以为
                                    // 「怎么什么都没变」或者「是不是已经改了」。
                                    div()
                                        .text_size(px(9.5))
                                        .text_color(theme::text_tertiary())
                                        .child(lang.pick(
                                            "上面就是恢复后的样子；点「取消」原样退回",
                                            "That is what you get; Cancel puts it back",
                                        )),
                                )
                                .child(
                                    div()
                                        .flex()
                                        .items_center()
                                        .gap(px(8.))
                                        .child(
                                            div()
                                                .id("ed-confirm-cancel")
                                                .flex_1()
                                                .h(px(26.))
                                                .flex()
                                                .items_center()
                                                .justify_center()
                                                .rounded(px(theme::RADIUS_SMALL))
                                                .border_1()
                                                .border_color(theme::border())
                                                .bg(theme::track())
                                                .cursor_pointer()
                                                .text_size(px(11.5))
                                                .text_color(theme::text_secondary())
                                                .hover(|style| style.bg(theme::surface_hover()))
                                                // 这里用 mouse_down 不用 on_click：外层卡片
                                                // 在 mouse_down 上 stop_propagation，click
                                                // 事件走不回来（点上去毫无反应）。按下的那一瞬
                                                // 就处理，和设置页托盘菜单一个套路。
                                                .on_mouse_down(
                                                    MouseButton::Left,
                                                    cx.listener(|this, _, _, cx| {
                                                        this.confirm_reset = false;
                                                        cx.notify();
                                                    }),
                                                )
                                                .child(lang.pick("取消", "Cancel")),
                                        )
                                        .child(
                                            div()
                                                .id("ed-confirm-ok")
                                                .flex_1()
                                                .h(px(26.))
                                                .flex()
                                                .items_center()
                                                .justify_center()
                                                .rounded(px(theme::RADIUS_SMALL))
                                                .border_1()
                                                .border_color(theme::accent())
                                                .bg(theme::accent_soft())
                                                .cursor_pointer()
                                                .text_size(px(11.5))
                                                .text_color(theme::text_primary())
                                                .hover(|style| style.bg(theme::surface_hover()))
                                                .on_mouse_down(
                                                    MouseButton::Left,
                                                    cx.listener(|this, _, _, cx| {
                                                        this.apply_reset(cx);
                                                    }),
                                                )
                                                .child(lang.pick("恢复默认", "Reset")),
                                        ),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }

    /// 取色器面板。
    fn picker_overlay(&self, lang: Lang, cx: &mut Context<Self>) -> Option<AnyElement> {
        const SV_W: f32 = 224.0;
        const SV_H: f32 = 104.0;
        const HUE_H: f32 = 12.0;

        let picker = self.picker.as_ref()?;
        let slot = picker.slot;
        let hex = picker.picker.hex.clone();
        let edit = picker.picker.edit.clone();
        let picking = picker.picking;

        // 方块 / 色相条走 `ui::picker`（和设置页同一份）。
        let this = cx.entity().downgrade();
        let press = {
            let this = this.clone();
            Rc::new(move |window: &mut Window, cx: &mut App| {
                let _ = this.update(cx, |this, cx| this.focus.focus(window, cx));
            }) as Rc<dyn Fn(&mut Window, &mut App)>
        };
        let change = Rc::new(
            move |h: f32, s: f32, v: f32, _window: &mut Window, cx: &mut App| {
                let _ = this.update(cx, |this, cx| this.apply_hsv(h, s, v, cx));
            },
        ) as Rc<dyn Fn(f32, f32, f32, &mut Window, &mut App)>;
        let sv_box = crate::ui::picker::sv_box(
            "ed",
            &picker.picker,
            SV_W,
            SV_H,
            press.clone(),
            change.clone(),
        );
        let hue_box = crate::ui::picker::hue_box("ed", &picker.picker, SV_W, HUE_H, press, change);

        let hex_text = match &edit {
            Some(buffer) => format!("{}|", buffer),
            None => hex.clone(),
        };
        let hex_box = div()
            .id("ed-hex")
            .flex_1()
            .h(px(22.))
            .px(px(7.))
            .flex()
            .items_center()
            .rounded(px(4.))
            .bg(theme::track())
            .border_1()
            .border_color(if edit.is_some() {
                theme::accent()
            } else {
                theme::border()
            })
            .cursor_text()
            .text_size(px(11.))
            .text_color(theme::text_primary())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &gpui::MouseDownEvent, window, cx| {
                    if let Some(picker) = this.picker.as_mut() {
                        if picker.picker.edit.is_none() {
                            picker.picker.edit = Some(picker.picker.hex.clone());
                        }
                        // 整串默认是「选中」状态：直接打字或退格就换掉。
                        picker.picker.edit_fresh = true;
                    }
                    this.focus.focus(window, cx);
                    cx.stop_propagation();
                    cx.notify();
                }),
            )
            .child(hex_text);

        let footer = div()
            .flex()
            .items_center()
            .gap(px(6.))
            .child(hex_box)
            .child(
                div()
                    .id("ed-eyedrop")
                    .h(px(22.))
                    .px(px(8.))
                    .flex()
                    .items_center()
                    .rounded(px(4.))
                    .bg(theme::track())
                    .cursor_pointer()
                    .text_size(px(11.))
                    .text_color(if picking {
                        theme::accent()
                    } else {
                        theme::text_secondary()
                    })
                    .hover(|style| style.bg(theme::surface_hover()))
                    .on_click(cx.listener(|this, _: &gpui::ClickEvent, window, cx| {
                        this.start_screen_pick(window, cx);
                    }))
                    .child(lang.pick("屏幕取色", "Pick")),
            );

        Some(
            div()
                .id("ed-picker")
                .absolute()
                .right(px(self.panel_w() + GRIP_W + 14.0))
                .bottom(px(14.0))
                .w(px(248.))
                .p(px(12.))
                .flex()
                .flex_col()
                .gap(px(8.))
                .rounded(px(8.))
                .bg(theme::surface_elevated())
                .border_1()
                .border_color(theme::border_strong())
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                .child(
                    div()
                        .flex()
                        .items_center()
                        .justify_between()
                        .child(
                            div()
                                .text_size(px(11.5))
                                .text_color(theme::text_primary())
                                .child(slot.label(lang)),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(4.))
                                // 「跟随」= 把这一项清空，回到它默认跟随的那个颜色：
                                // 音量条回到强调色、别的元素回到文字色、卡片 / 阴影回到主题。
                                // 没有这个按钮的话，给元素单独挑过色之后就再也回不去了。
                                .child(
                                    div()
                                        .id("ed-picker-follow")
                                        .h(px(16.))
                                        .px(px(4.))
                                        .flex()
                                        .items_center()
                                        .rounded(px(3.))
                                        .cursor_pointer()
                                        .text_size(px(10.))
                                        .text_color(theme::text_tertiary())
                                        .hover(|style| style.bg(theme::surface_hover()))
                                        .on_click(cx.listener(
                                            |this, _: &gpui::ClickEvent, _window, cx| {
                                                let Some(slot) =
                                                    this.picker.as_ref().map(|picker| picker.slot)
                                                else {
                                                    return;
                                                };
                                                this.set_color(slot, String::new());
                                                this.picker = None;
                                                // 关掉取色器就回到用户挑的那个预览状态，
                                                // 别一直停在「静音色试看」上。
                                                this.preview_muted = false;
                                                cx.notify();
                                            },
                                        ))
                                        .child(slot.follow_label(lang)),
                                )
                                .child(
                                    div()
                                        .id("ed-picker-close")
                                        .size(px(16.))
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .rounded(px(3.))
                                        .cursor_pointer()
                                        .text_size(px(11.))
                                        .text_color(theme::text_tertiary())
                                        .hover(|style| style.bg(theme::surface_hover()))
                                        .on_click(cx.listener(
                                            |this, _: &gpui::ClickEvent, _window, cx| {
                                                this.picker = None;
                                                this.preview_muted = false;
                                                cx.notify();
                                            },
                                        ))
                                        .child("×"),
                                ),
                        ),
                )
                .child(sv_box)
                .child(hue_box)
                .child(footer)
                .child(
                    div()
                        .text_size(px(9.5))
                        .text_color(theme::text_tertiary())
                        .child(lang.pick(
                            "拖色块选色 · 点色号可粘贴 · 取色后点屏幕任意位置",
                            "Drag to pick · click the hex to paste · then click anywhere on screen",
                        )),
                )
                .into_any_element(),
        )
    }

    /// 那三个下拉菜单（状态 / 图标 / 喇叭样式）共用一个浮层。
    ///
    /// 用绝对定位，不吃面板的布局 —— 开关它不会把下面的行推来推去。
    fn menu_overlay(
        &self,
        cfg: &crate::config::OsdConfig,
        lang: Lang,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let kind = self.menu?;
        let (top, width) = match kind {
            EditorMenu::State => (130.0, 152.0),
            EditorMenu::Icon => (162.0, 152.0),
            EditorMenu::IconStyle => (162.0, 152.0),
        };
        let mut column = div()
            .id("ed-menu")
            .absolute()
            .right(px(self.panel_w() + GRIP_W + 10.0))
            .top(px(top))
            .w(px(width))
            .p(px(4.))
            .flex()
            .flex_col()
            .gap(px(1.))
            .rounded(px(6.))
            .bg(theme::surface_elevated())
            .border_1()
            .border_color(theme::border_strong())
            // 点菜单自己不算「点别处」，否则刚点开就关。
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation());

        match kind {
            EditorMenu::State => {
                for (index, option) in PREVIEWS.iter().enumerate() {
                    let on = self.preview == Some(index);
                    column = column.child(
                        div()
                            .id(SharedString::from(format!("ed-state-opt-{index}")))
                            .h(px(21.))
                            .px(px(5.))
                            .flex()
                            .items_center()
                            .gap(px(5.))
                            .rounded(px(4.))
                            .cursor_pointer()
                            .text_size(px(11.))
                            .when(on, |style| {
                                style
                                    .bg(theme::accent_soft())
                                    .text_color(theme::text_primary())
                            })
                            .when(!on, |style| style.text_color(theme::text_secondary()))
                            .hover(|style| style.bg(theme::surface_hover()))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _, _, cx| {
                                    // 只改预览：图标跟着这一行走（同一个下标）。
                                    this.preview = Some(index);
                                    // 手动挑了状态就按用户挑的画，别停在「静音色预览」上。
                                    this.preview_muted = false;
                                    this.menu = None;
                                    cx.notify();
                                }),
                            )
                            .child(ui::icon(
                                option.icon,
                                12.0,
                                if on {
                                    theme::text_primary()
                                } else {
                                    theme::text_secondary()
                                },
                            ))
                            .child(lang.pick(option.zh, option.en)),
                    );
                }
            }
            EditorMenu::Icon => {
                let current = self
                    .preview
                    .map(|index| PREVIEWS[index.min(PREVIEWS.len() - 1)].icon)
                    .unwrap_or("play");
                for (index, (icon, state)) in ICON_CHOICES.iter().enumerate() {
                    let on = current == *icon;
                    let state = *state;
                    let option = &PREVIEWS[state];
                    column = column.child(
                        div()
                            .id(SharedString::from(format!("ed-icon-opt-{index}")))
                            .h(px(21.))
                            .px(px(5.))
                            .flex()
                            .items_center()
                            .gap(px(5.))
                            .rounded(px(4.))
                            .cursor_pointer()
                            .text_size(px(11.))
                            .when(on, |style| {
                                style
                                    .bg(theme::accent_soft())
                                    .text_color(theme::text_primary())
                            })
                            .when(!on, |style| style.text_color(theme::text_secondary()))
                            .hover(|style| style.bg(theme::surface_hover()))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _, _, cx| {
                                    // 选图标 = 把状态也切到用这张图的那一条。
                                    this.preview = Some(state);
                                    this.preview_muted = false;
                                    this.menu = None;
                                    cx.notify();
                                }),
                            )
                            .child(ui::icon(
                                icon,
                                12.0,
                                if on {
                                    theme::text_primary()
                                } else {
                                    theme::text_secondary()
                                },
                            ))
                            .child(lang.pick(option.zh, option.en)),
                    );
                }
            }
            EditorMenu::IconStyle => {
                let current = cfg.icon_style().key;
                for (index, style) in crate::config::ICON_STYLES.iter().enumerate() {
                    let on = style.key == current;
                    let key = style.key.to_string();
                    let store = self.config.clone();
                    column = column.child(
                        div()
                            .id(SharedString::from(format!("ed-style-opt-{index}")))
                            .h(px(21.))
                            .px(px(5.))
                            .flex()
                            .items_center()
                            .gap(px(5.))
                            .rounded(px(4.))
                            .cursor_pointer()
                            .text_size(px(11.))
                            .when(on, |style| {
                                style
                                    .bg(theme::accent_soft())
                                    .text_color(theme::text_primary())
                            })
                            .when(!on, |style| style.text_color(theme::text_secondary()))
                            .hover(|style| style.bg(theme::surface_hover()))
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(move |this, _, _, cx| {
                                    let key = key.clone();
                                    let _ = store
                                        .borrow_mut()
                                        .update(|config| config.osd.set_icon_style(&key));
                                    this.menu = None;
                                    cx.notify();
                                }),
                            )
                            .child(ui::icon(
                                style.icon,
                                12.0,
                                if on {
                                    theme::text_primary()
                                } else {
                                    theme::text_secondary()
                                },
                            ))
                            .child(lang.pick(style.zh, style.en)),
                    );
                }
            }
        }
        Some(column.into_any_element())
    }

    /// 画布左下角那个「拖动时自动吸附」小开关。
    ///
    /// 它管的是画布上的拖动，放在画布上比塞进右边那一长串参数里好找。
    /// 画布底色那排按钮：黑 / 灰 / 白 / 自由选色（共四个）。
    ///
    /// 摆成画布左上角的一小块浮层：它改的是「桌面」，不是提示条，放参数栏里
    /// 会让人以为是在改卡片本身。
    fn canvas_bg_bar(&self, lang: Lang, cx: &mut Context<Self>) -> AnyElement {
        // 空串（刚点过「恢复默认」）= 黑，别让第四格显示成一个「自选色」。
        let raw = self.config.borrow().get().editor_bg.to_ascii_lowercase();
        let current = if raw.is_empty() {
            CANVAS_BGS[0].to_string()
        } else {
            raw
        };
        let mut row = div()
            .id("ed-canvas-bg")
            .absolute()
            .left(px(12.))
            .top(px(12.))
            .flex()
            .items_center()
            .gap(px(5.))
            .px(px(7.))
            .py(px(4.))
            .rounded(px(theme::RADIUS_SMALL))
            .bg(theme::surface())
            .border_1()
            .border_color(theme::border())
            .child(
                div()
                    .text_size(px(9.5))
                    .text_color(theme::text_tertiary())
                    .child(lang.pick("画布", "Canvas")),
            );
        for hex in CANVAS_BGS {
            let on = current.eq_ignore_ascii_case(hex);
            let value = hex.to_string();
            row = row.child(
                div()
                    .id(SharedString::from(format!("ed-canvas-{hex}")))
                    .w(px(16.))
                    .h(px(16.))
                    .flex_none()
                    .rounded(px(4.))
                    .bg(theme::parse_hex(hex).unwrap_or_else(theme::surface))
                    .border_1()
                    .border_color(if on {
                        theme::accent()
                    } else {
                        theme::border_strong()
                    })
                    .cursor_pointer()
                    .hover(|style| style.border_color(theme::accent()))
                    .on_click(cx.listener(move |this, _: &gpui::ClickEvent, _window, cx| {
                        this.update_config(|config| config.editor_bg = value.clone());
                        cx.notify();
                    })),
            );
        }
        // 第四格：自由选色。挑过自选色就直接显示那个颜色，
        // 否则自选色在三个固定档里根本看不出来（用户会以为没生效）。
        let custom = if CANVAS_BGS
            .iter()
            .any(|hex| current.eq_ignore_ascii_case(hex))
        {
            None
        } else {
            Some(self.canvas_bg())
        };
        let open = self.picker.as_ref().map(|picker| picker.slot) == Some(ColorSlot::CanvasBg);
        let has_custom = custom.is_some();
        row = row.child(
            div()
                .id("ed-canvas-custom")
                .w(px(16.))
                .h(px(16.))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(4.))
                .bg(custom.unwrap_or_else(theme::surface_elevated))
                .border_1()
                .border_color(if open || has_custom {
                    theme::accent()
                } else {
                    theme::border_strong()
                })
                .cursor_pointer()
                .hover(|style| style.border_color(theme::accent()))
                // 用 mouse_down：根节点那条「点别处」会先把取色器关掉。
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _, _, cx| {
                        cx.stop_propagation();
                        this.open_picker(ColorSlot::CanvasBg, cx);
                    }),
                )
                .child(
                    div()
                        .text_size(px(10.))
                        .text_color(if has_custom {
                            theme::text_primary()
                        } else {
                            theme::text_secondary()
                        })
                        .child("+"),
                ),
        );
        row.into_any_element()
    }

    /// 画布与参数栏之间那条把手：按住左右拖，改参数栏宽度。
    ///
    /// 平时什么都不画 —— 一根常亮的竖线在编辑器里太抢眼，像是「这里有个分隔栏」。
    /// 光标扫过来才浮出一条 1px 的细线（颜色贴着主题的描边，不刺眼），按住时线变长、
    /// 转成强调色。命中区仍是 6px（视觉 1px，但抓得住）。
    ///
    /// 拖动只改内存里的宽度（每一帧都写一次配置文件太重），松手才落盘。
    fn panel_grip(&self, cx: &mut Context<Self>) -> AnyElement {
        let active = self.panel_drag.is_some();
        div()
            .id("ed-panel-grip")
            .group("ed-panel-grip-hover")
            .flex_none()
            .w(px(GRIP_W))
            .h_full()
            .flex()
            .items_center()
            .justify_center()
            .cursor(gpui::CursorStyle::ResizeLeftRight)
            // 这 6px 得自己刷一层画布色：不刷的话它是「没画东西」的缝，
            // 透出来的是窗口后面的桌面（浅灰一条），看着像界面缺了一块。
            .bg(self.canvas_bg())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    this.panel_drag = Some((event.position.x.as_f32(), this.panel_w()));
                    cx.notify();
                }),
            )
            .child(
                div()
                    .w(px(1.))
                    // 按住时拉长一点，给一个「抓住了」的反馈。
                    .h(px(if active { 48. } else { 24. }))
                    .rounded_full()
                    .when(active, |style| style.bg(theme::accent()))
                    .when(!active, |style| {
                        style
                            // 平时完全透明：只在光标扫到这条边界时才浮出来。
                            .bg(gpui::transparent_black())
                            .group_hover("ed-panel-grip-hover", |style| {
                                style.bg(theme::border_strong())
                            })
                    }),
            )
            .into_any_element()
    }

    fn snap_pill(
        &self,
        cfg: &crate::config::OsdConfig,
        lang: Lang,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let on = cfg.snap;
        div()
            .id("ed-snap")
            .absolute()
            .left(px(12.))
            .bottom(px(12.))
            .flex()
            .items_center()
            .gap(px(6.))
            .px(px(8.))
            .py(px(4.))
            .rounded(px(theme::RADIUS_SMALL))
            .bg(theme::surface())
            .border_1()
            .border_color(theme::border())
            .cursor_pointer()
            .hover(|style| style.bg(theme::surface_hover()))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    cx.stop_propagation();
                    let next = !this.cfg().snap;
                    this.update(|osd| osd.snap = next);
                    cx.notify();
                }),
            )
            .child(
                div()
                    .w(px(12.))
                    .h(px(12.))
                    .flex_none()
                    .rounded(px(3.))
                    .border_1()
                    .border_color(theme::border_strong())
                    .when(on, |style| style.bg(theme::accent())),
            )
            .child(
                div()
                    .text_size(px(10.5))
                    .text_color(theme::text_secondary())
                    .child(lang.pick("拖动时自动吸附", "Snap while dragging")),
            )
            .into_any_element()
    }

    /// 图标行右边那个下拉按钮：显示当前图标 + 名字，点开选。
    ///
    /// 媒体模板列的是动作图标（音量 / 静音 / 应用音量 / 播放 / 暂停 / 上一曲 / 下一曲 / 停止），
    /// 选中会顺带把「状态」也切过去；音量模板列的是那几种喇叭样式（真实配置项）。
    fn icon_pick_button(
        &self,
        cfg: &crate::config::OsdConfig,
        lang: Lang,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (icon_name, label, kind) = if cfg.template() != crate::config::OsdTemplate::Volume {
            let (icon, text) = match self.preview {
                Some(index) => {
                    let option = &PREVIEWS[index.min(PREVIEWS.len() - 1)];
                    (option.icon, lang.pick(option.zh, option.en))
                }
                // 亮度模板的真实提示条就一张太阳，默认也照它画。
                None if cfg.template() == crate::config::OsdTemplate::Brightness => {
                    ("sun", lang.pick("亮度", "Brightness"))
                }
                None => ("play", lang.pick("播放", "Play")),
            };
            (icon, text, EditorMenu::Icon)
        } else {
            let style = cfg.icon_style();
            (
                style.icon,
                lang.pick(style.zh, style.en),
                EditorMenu::IconStyle,
            )
        };
        let open = self.menu == Some(kind);
        div()
            .id("ed-icon-menu-button")
            // 不许被 flex 压缩：压扁了图标和文字都会糊。
            .flex_none()
            .h(px(18.))
            .px(px(5.))
            .flex()
            .items_center()
            .gap(px(3.))
            .rounded(px(3.))
            .cursor_pointer()
            .text_size(px(9.5))
            .text_color(theme::text_primary())
            .bg(if open {
                theme::accent_soft()
            } else {
                theme::track()
            })
            .hover(|style| style.bg(theme::surface_hover()))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _, _, cx| {
                    // 自己收掉这一下：根节点在 mouse_down 上会关菜单，不收就「刚点开就被关」。
                    cx.stop_propagation();
                    this.suppress_row_click = Some(std::time::Instant::now());
                    this.menu = if this.menu == Some(kind) {
                        None
                    } else {
                        Some(kind)
                    };
                    cx.notify();
                }),
            )
            .child(ui::icon(
                icon_name,
                12.0,
                if open {
                    theme::text_primary()
                } else {
                    theme::text_secondary()
                },
            ))
            .child(div().flex_none().whitespace_nowrap().child(label))
            .child(ui::icon("chevron-down", 9.0, theme::text_tertiary()))
            .into_any_element()
    }

    fn panel(
        &self,
        cfg: &crate::config::OsdConfig,
        lang: Lang,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut column = div()
            .id("ed-panel")
            .w(px(self.panel_w()))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            // 参数越加越多，超出高度就滚动（面板里没有别的滚轮处理，不会抢）。
            .overflow_y_scroll()
            .track_scroll(&self.panel_scroll)
            .gap(px(8.))
            .px(px(12.))
            .py(px(10.))
            .border_l_1()
            .border_color(theme::border())
            .bg(theme::surface());

        // 模板切换：音量 / 媒体键 / 亮度，各调各的。
        let mut template = div().flex().items_center().gap(px(4.));
        for (slot, label) in [
            (crate::config::OsdTemplate::Volume, lang.pick("音量", "Volume")),
            (crate::config::OsdTemplate::Media, lang.pick("媒体键", "Media keys")),
            (crate::config::OsdTemplate::Brightness, lang.pick("亮度", "Brightness")),
        ] {
            let on = cfg.template() == slot;
            let store = self.config.clone();
            template = template.child(
                div()
                    .id(SharedString::from(format!("ed-tpl-{slot:?}")))
                    .px(px(8.))
                    .py(px(2.))
                    .rounded(px(4.))
                    .text_size(px(11.))
                    .cursor_pointer()
                    .when(on, |style| {
                        style
                            .bg(theme::accent_soft())
                            .text_color(theme::text_primary())
                    })
                    .when(!on, |style| style.text_color(theme::text_secondary()))
                    .hover(|style| style.bg(theme::surface_hover()))
                    .on_click(cx.listener(move |this, _: &gpui::ClickEvent, window, cx| {
                        let _ = store.borrow_mut().update(|config| {
                            config.osd.set_template(slot);
                        });
                        this.selected = None;
                        this.focus.focus(window, cx);
                        cx.notify();
                    }))
                    .child(label),
            );
        }
        column = column.child(template);
        // 说清楚这两套是什么时候用的：模板不是「切换显示」，而是按触发的操作自动选。
        column = column.child(
            div()
                .text_size(px(10.))
                .text_color(theme::text_tertiary())
                .child(lang.pick(
                    "音量 / 静音弹音量模板，媒体键弹媒体键模板，亮度键弹亮度模板",
                    "Volume and mute use the volume template, media keys the media one, brightness its own",
                )),
        );

        // 元素区（右边带「恢复默认布局」）。
        let header_row = div()
            .flex()
            .items_center()
            .gap(px(6.))
            .child(
                div()
                    .flex_1()
                    .text_size(px(11.))
                    .text_color(theme::text_tertiary())
                    .child(lang.pick("元素", "Elements")),
            )
            // 新建一条自定义文字：点完直接给光标，敲字就有。
            .child(
                div()
                    .id("ed-add-text")
                    .px(px(6.))
                    .py(px(1.))
                    .rounded(px(4.))
                    .text_size(px(10.5))
                    .text_color(theme::text_secondary())
                    .cursor_pointer()
                    .hover(|style| {
                        style
                            .bg(theme::surface_hover())
                            .text_color(theme::text_primary())
                    })
                    .on_click(cx.listener(|this, _: &gpui::ClickEvent, window, cx| {
                        this.add_text_part(window, cx);
                    }))
                    .child(lang.pick("＋ 文字", "+ Text")),
            )
            .child(
                div()
                    .id("ed-reset")
                    .px(px(6.))
                    .py(px(1.))
                    .rounded(px(4.))
                    .text_size(px(10.5))
                    .text_color(theme::text_secondary())
                    .cursor_pointer()
                    .hover(|style| {
                        style
                            .bg(theme::surface_hover())
                            .text_color(theme::text_primary())
                    })
                    .on_click(cx.listener(|this, _: &gpui::ClickEvent, _window, cx| {
                        // 先问一句：这一下会把整张模板的元素排布抹掉，没有撤销。
                        this.confirm_reset = true;
                        cx.notify();
                    }))
                    .child(lang.pick("恢复默认布局", "Reset layout")),
            );
        // 元素列表：每一行都是「拖拽手柄 + 眼睛 + 名字 + 各自的选项」。
        // 顺序 = `cfg.parts()`（= 图层顺序，列表最后一行的元素画在最上面）。
        let mut rows: Vec<AnyElement> = Vec::new();
        // 列表倒着摆：**最上面那行画在最上层**（盖住它下面的）。
        // `parts()` 是「先画 → 后画」的顺序，所以这里翻过来。
        let template = cfg.template();
        let parts: Vec<OsdPart> = cfg
            .parts()
            .into_iter()
            // 「分贝」是音量专有的对数口径：媒体键和亮度模板里留着一行点了没读数。
            .filter(|part| !(template != OsdTemplate::Volume && *part == OsdPart::Db))
            .rev()
            .collect();
        for part in parts {
            let visible = cfg.shows_part(part);
            let selected = self.selected == Some(part);
            let editing = self.text.as_ref().map(|edit| edit.part) == Some(part);
            let store = self.config.clone();
            let name = part.label_in(lang, template);
            let text_index = part.text_index();
            // 状态文字自定义之后，自动内容的三个预览档位就没意义了。
            let title_chips = part == OsdPart::Title && cfg.title_text.trim().is_empty();
            let has_chips = part == OsdPart::Icon || part == OsdPart::Percent || title_chips;

            let mut row = div()
                .id(SharedString::from(format!("ed-row-{}", part.index())))
                .relative()
                .flex()
                .items_center()
                .gap(px(6.))
                .px(px(4.))
                .py(px(2.))
                .rounded(px(4.))
                .cursor_pointer()
                .when(selected, |style| style.bg(theme::accent_soft()))
                .hover(|style| style.bg(theme::surface_hover()))
                // 这一行自己的矩形（拖图层顺序时按它判落点）。
                .child(measure_row(self.layer_rows.clone(), part.index()))
                .on_click(
                    cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                        // 点眼睛 / × / 预览格子时，冒上来的这一下不算「选中」。
                        if let Some(at) = this.suppress_row_click.take() {
                            if at.elapsed() < std::time::Duration::from_millis(400) {
                                return;
                            }
                        }
                        this.selected = Some(part);
                        // 单击只选中：状态那一行尤其容易误触（点了想调参数，结果弹出输入框）。
                        // **双击自定义文字 = 进编辑**：字被删空之后画布上的框只剩十几像素，
                        // 鼠标根本点不中，这一行就是唯一的入口。
                        if event.click_count() >= 2 && matches!(part, OsdPart::Text(_)) {
                            this.begin_text(part, window, cx);
                            return;
                        }
                        this.focus.focus(window, cx);
                        cx.notify();
                    }),
                )
                // 拖拽手柄：按住往上/下拖，改的是图层顺序（后画的盖住先画的）。
                .child(
                    div()
                        .id(SharedString::from(format!("ed-grip-{}", part.index())))
                        .w(px(9.))
                        .h(px(14.))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(2.))
                        .cursor(gpui::CursorStyle::ResizeUpDown)
                        .text_size(px(9.))
                        .text_color(if self.layer_drag == Some(part) {
                            theme::accent()
                        } else {
                            theme::text_tertiary()
                        })
                        .hover(|style| style.bg(theme::track()))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, _, cx| {
                                this.suppress_row_click = Some(std::time::Instant::now());
                                this.layer_drag = Some(part);
                                this.selected = Some(part);
                                cx.notify();
                            }),
                        )
                        .child("≡"),
                )
                // 眼睛：开关。
                .child(
                    div()
                        .id(SharedString::from(format!("ed-eye-{}", part.index())))
                        .w(px(12.))
                        .h(px(12.))
                        .flex_none()
                        .rounded(px(3.))
                        .border_1()
                        .border_color(theme::border_strong())
                        .when(visible, |style| style.bg(theme::accent()))
                        .cursor_pointer()
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, _, cx| {
                                // 用 mouse_down + 标记：这一下别让外层行再当成点击，
                                // 否则「点一下关掉」会立刻被行的「选中并编辑」重新打开
                                // —— 状态就是这样一直关不掉的。
                                this.suppress_row_click = Some(std::time::Instant::now());
                                let next = !this.cfg().shows_part(part);
                                let _ = store.borrow_mut().update(|config| {
                                    config.osd.set_part_visible(part, next);
                                });
                                cx.notify();
                            }),
                        ),
                )
                .child(
                    div()
                        // 有右侧选项的行，名字不占剩余空间（不然选项会被挤到边外）。
                        .when(has_chips, |style| style.flex_none())
                        .when(!has_chips, |style| style.flex_1())
                        .text_size(px(11.5))
                        .text_color(if visible {
                            theme::text_primary()
                        } else {
                            theme::text_tertiary()
                        })
                        .child(name),
                );

            if has_chips {
                row = row.child(div().flex_1());
            }

            // 图标：一个下拉按钮。媒体模板里选的是动作图标（播放 / 暂停 / 音量 / 静音…），
            // 音量模板里选的是喇叭样式；两边都能和上面的「状态」互相带着走。
            if part == OsdPart::Icon {
                row = row.child(self.icon_pick_button(cfg, lang, cx));
            }

            // 状态：一个下拉按钮，菜单里列出所有会显示的内容（可手动预览）。
            if title_chips {
                // 没选过时跟画布一致：媒体模板默认「播放」，音量模板默认「音量」。
                // （两个下拉的默认值必须一样，否则一进来就显示成两个不同的状态。）
                let default = match cfg.template() {
                    crate::config::OsdTemplate::Media => 5,
                    crate::config::OsdTemplate::Brightness => 9,
                    crate::config::OsdTemplate::Volume => 0,
                };
                let index = self.preview.unwrap_or(default).min(PREVIEWS.len() - 1);
                let option = &PREVIEWS[index];
                let open = self.menu == Some(EditorMenu::State);
                row = row.child(
                    div()
                        .id("ed-title-menu-button")
                        // 不许被 flex 压缩：压扁了文字会被截成半个字。
                        .flex_none()
                        .h(px(18.))
                        .px(px(5.))
                        .flex()
                        .items_center()
                        .gap(px(3.))
                        .rounded(px(3.))
                        .cursor_pointer()
                        .text_size(px(9.5))
                        .text_color(theme::text_primary())
                        .bg(if open {
                            theme::accent_soft()
                        } else {
                            theme::track()
                        })
                        .hover(|style| style.bg(theme::surface_hover()))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, _, cx| {
                                // 这一下必须自己收掉：根节点在 mouse_down 上会把菜单关掉，
                                // 不收的话「刚点开就被它关」。
                                cx.stop_propagation();
                                this.suppress_row_click = Some(std::time::Instant::now());
                                if this.text.is_some() {
                                    this.commit_text(cx);
                                }
                                this.menu = if this.menu == Some(EditorMenu::State) {
                                    None
                                } else {
                                    Some(EditorMenu::State)
                                };
                                cx.notify();
                            }),
                        )
                        .child(
                            div()
                                .flex_none()
                                .whitespace_nowrap()
                                .child(lang.pick(option.zh, option.en)),
                        )
                        .child(ui::icon("chevron-down", 9.0, theme::text_tertiary())),
                );
            }

            // 百分比：一个小 `%` 格子，开着就在读数后面带 %（默认开）。
            if part == OsdPart::Percent {
                let on = cfg.percent_sign;
                row = row.child(
                    div()
                        .id("ed-sign")
                        .w(px(20.))
                        .h(px(18.))
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(3.))
                        .cursor_pointer()
                        .text_size(px(11.))
                        .when(on, |style| {
                            style
                                .bg(theme::accent_soft())
                                .border_1()
                                .border_color(theme::accent())
                                .text_color(theme::text_primary())
                        })
                        .when(!on, |style| {
                            style.bg(theme::track()).text_color(theme::text_tertiary())
                        })
                        .hover(|style| style.bg(theme::surface_hover()))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, _, cx| {
                                this.suppress_row_click = Some(std::time::Instant::now());
                                let next = !this.cfg().percent_sign;
                                this.update(|osd| {
                                    osd.edit_style(|style| style.percent_sign = next)
                                });
                                cx.notify();
                            }),
                        )
                        .child("%"),
                );
            }

            // 行尾那个色块撤掉了：元素自己的颜色挪到选中后展开的卡片里
            // （和「对齐」摆在一起），一行上就不会再挂着一格看不懂的小方块。

            // 正在改这一行：给一个 ✓，点它立刻保存（也可以点别处，失焦自动保存）。
            if editing {
                row = row.child(
                    div()
                        .id(SharedString::from(format!("ed-text-ok-{}", part.index())))
                        .w(px(14.))
                        .h(px(14.))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(3.))
                        .cursor_pointer()
                        .text_size(px(11.))
                        .text_color(theme::accent())
                        .hover(|style| style.bg(theme::surface_hover()))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, _, cx| {
                                this.suppress_row_click = Some(std::time::Instant::now());
                                this.commit_text(cx);
                            }),
                        )
                        .child("✓"),
                );
            }

            // 自定义文字：行尾一个小 ×，删掉这一条。
            if let Some(index) = text_index {
                let store = self.config.clone();
                row = row.child(
                    div()
                        .id(SharedString::from(format!("ed-del-{}", part.index())))
                        .w(px(12.))
                        .h(px(12.))
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(3.))
                        .cursor_pointer()
                        .text_size(px(10.))
                        .text_color(theme::text_tertiary())
                        .hover(|style| style.bg(theme::surface_hover()).text_color(theme::danger()))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |this, _, _, cx| {
                                this.suppress_row_click = Some(std::time::Instant::now());
                                let _ = store.borrow_mut().update(|config| {
                                    config.osd.remove_text(index);
                                });
                                this.text = None;
                                this.selected = None;
                                cx.notify();
                            }),
                        )
                        .child("×"),
                );
            }
            rows.push(row.into_any_element());
            // 选中的那一行就地展开一张卡：对齐、元素颜色、音量条那几项都在里面。
            // 放在这一行正下方而不是参数栏最底部 —— 「现在调的是谁」不用再回头找。
            if selected {
                rows.push(self.element_card(cfg, part, lang, cx));
            }
        }
        // 元素整块用一张卡圈起来（列表 + 展开卡都在里面）。
        let mut elements_card = div()
            .flex()
            .flex_col()
            .gap(px(2.))
            .p(px(6.))
            .rounded(px(8.))
            .bg(theme::surface_elevated())
            .border_1()
            .border_color(theme::border())
            .child(header_row);
        for row in rows {
            elements_card = elements_card.child(row);
        }
        column = column.child(elements_card);

        // 背景卡片：尺寸 / 配色 / 阴影三块，各自一张小卡。
        //
        // 以前尺寸那四行、配色那两行、阴影那一串是平铺的，中间还夹着音量条那一组，
        // 看不出「这些画的都是卡片本体」。现在归到一个标题下、各自圈起来。
        column = column.child(self.section_title(lang.pick("背景卡片", "Background card")));
        column = column.child(self.group(
            lang.pick("尺寸", "Size"),
            vec![
                self.stepper("ed-w", lang.pick("宽度", "Width"), NumField::Width, cx),
                self.stepper("ed-h", lang.pick("高度", "Height"), NumField::Height, cx),
                self.stepper("ed-r", lang.pick("圆角", "Corner"), NumField::Corner, cx),
                self.stepper(
                    "ed-o",
                    lang.pick("不透明度", "Opacity"),
                    NumField::Opacity,
                    cx,
                ),
                // 背景也能转：画布右上那个圆柄拖一下就转，这里可以手打精确值。
                self.rotation_row(RotationTarget::Card, lang, cx),
            ],
        ));
        column = column.child(self.group(
            lang.pick("配色", "Colors"),
            vec![
                self.color_row(ColorSlot::Card, CARD_COLORS, &cfg.colors.card, lang, cx),
                self.color_row(ColorSlot::Text, INK_COLORS, &cfg.colors.text, lang, cx),
            ],
        ));

        // 阴影（默认关）：开关、左右 / 上下偏移、扩散、不透明度、颜色都归这一张卡。
        let shadow = cfg.shadow.clone();
        let mut shadow_rows: Vec<AnyElement> = Vec::new();
        shadow_rows.push(
            div()
                .id("ed-shadow-toggle")
                .flex()
                .items_center()
                .gap(px(6.))
                .px(px(4.))
                .py(px(2.))
                .rounded(px(4.))
                .cursor_pointer()
                .hover(|style| style.bg(theme::surface_hover()))
                .on_click(cx.listener(|this, _: &gpui::ClickEvent, _window, cx| {
                    let next = !this.cfg().shadow.enabled;
                    this.update(|osd| osd.edit_style(|style| style.shadow.enabled = next));
                    cx.notify();
                }))
                .child(
                    div()
                        .w(px(12.))
                        .h(px(12.))
                        .flex_none()
                        .rounded(px(3.))
                        .border_1()
                        .border_color(theme::border_strong())
                        .when(shadow.enabled, |style| style.bg(theme::accent())),
                )
                .child(
                    div()
                        .text_size(px(11.))
                        .text_color(theme::text_secondary())
                        .child(lang.pick("开启阴影", "Enable shadow")),
                )
                .into_any_element(),
        );
        if shadow.enabled {
            shadow_rows.push(self.stepper(
                "ed-sh-x",
                lang.pick("左右", "Offset X"),
                NumField::ShadowX,
                cx,
            ));
            shadow_rows.push(self.stepper(
                "ed-sh-y",
                lang.pick("上下", "Offset Y"),
                NumField::ShadowY,
                cx,
            ));
            shadow_rows.push(self.stepper(
                "ed-sh-blur",
                lang.pick("扩散", "Blur"),
                NumField::ShadowBlur,
                cx,
            ));
            shadow_rows.push(self.stepper(
                "ed-sh-alpha",
                lang.pick("不透明度", "Opacity"),
                NumField::ShadowOpacity,
                cx,
            ));
            shadow_rows.push(self.color_row(
                ColorSlot::Shadow,
                SHADOW_COLORS,
                &shadow.color,
                lang,
                cx,
            ));
        }
        column = column.child(self.group(lang.pick("阴影", "Shadow"), shadow_rows));

        // 底部提示：每条一行。原来一整句横着排，参数栏没多宽，只能看见前半截。
        column = column.child(self.hint_card(lang));

        column.into_any_element()
    }

    /// 参数栏里的区块标题（在卡片外面、上面一行）。
    fn section_title(&self, title: &'static str) -> AnyElement {
        div()
            .mt(px(2.))
            .flex()
            .items_center()
            .gap(px(5.))
            .child(
                div()
                    .w(px(2.))
                    .h(px(10.))
                    .rounded(px(1.))
                    .bg(theme::accent()),
            )
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(theme::text_secondary())
                    .child(title),
            )
            .into_any_element()
    }

    /// 一张带小标题的参数卡：参数栏里每一「区域」都是它。
    fn group(&self, title: &'static str, children: Vec<AnyElement>) -> AnyElement {
        let mut card = div()
            .flex()
            .flex_col()
            .gap(px(3.))
            .p(px(6.))
            .rounded(px(6.))
            .bg(theme::surface_sunken())
            .border_1()
            .border_color(theme::border())
            .child(
                div()
                    .text_size(px(9.5))
                    .text_color(theme::text_tertiary())
                    .child(title),
            );
        for child in children {
            card = card.child(child);
        }
        card.into_any_element()
    }

    /// 选中元素后展开在它下面那张卡：对齐 / 元素颜色 / 音量条那几项。
    fn element_card(
        &self,
        cfg: &crate::config::OsdConfig,
        part: OsdPart,
        lang: Lang,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut body = div().flex().flex_col().gap(px(4.));

        // 对齐：靠左 / 居中 / 靠右。音量条不给这组 —— 它里面那根条永远是铺满整格画的，
        // 换对齐看不出任何变化，摆着只会让人以为「点了没用」。
        if part != OsdPart::Bar {
            let current = cfg.align_of(part);
            let store = self.config.clone();
            let mut align_row = div().flex().items_center().gap(px(3.)).child(
                div()
                    .w(px(56.))
                    .flex_none()
                    .text_size(px(11.))
                    .text_color(theme::text_secondary())
                    .child(lang.pick("对齐", "Align")),
            );
            for align in [OsdAlign::Left, OsdAlign::Center, OsdAlign::Right] {
                let on = current == align;
                let store = store.clone();
                align_row = align_row.child(
                    div()
                        .id(SharedString::from(format!("ed-align-{}", align.index())))
                        .px(px(7.))
                        .py(px(1.))
                        .rounded(px(4.))
                        .flex_none()
                        .text_size(px(10.5))
                        .cursor_pointer()
                        .when(on, |style| {
                            style
                                .bg(theme::accent_soft())
                                .text_color(theme::text_primary())
                        })
                        .when(!on, |style| style.text_color(theme::text_secondary()))
                        .hover(|style| style.bg(theme::surface_hover()))
                        .on_click(
                            cx.listener(move |_this, _: &gpui::ClickEvent, _window, cx| {
                                let _ = store
                                    .borrow_mut()
                                    .update(|config| config.osd.set_part_align(part, align));
                                cx.notify();
                            }),
                        )
                        .child(align.label(lang)),
                );
            }
            body = body.child(align_row);
        }

        // 旋转：绕元素自己的中心，±180°，所有元素都有。
        body = body.child(self.rotation_row(RotationTarget::Part(part), lang, cx));

        // 自定义文字：变量按钮（只有自定义文字能写内容，状态不算）。
        if matches!(part, OsdPart::Text(_)) {
            body = body.child(self.text_chips(lang, cx));
        }

        if part == OsdPart::Bar {
            // 滑条：三段颜色 + 圆角。原来散在参数栏最底下，现在贴着它自己这一行。
            // 名字跟着模板走 —— 亮度模板里这根是亮度条，别叫音量条。
            let template = cfg.template();
            body = body.child(
                div()
                    .text_size(px(9.5))
                    .text_color(theme::text_tertiary())
                    .child(part.label_in(lang, template)),
            );
            body = body.child(self.color_row(
                ColorSlot::Accent,
                ACCENT_COLORS,
                &cfg.colors.accent,
                lang,
                cx,
            ));
            body = body.child(self.color_row(
                ColorSlot::BarTrack,
                BAR_TRACK_COLORS,
                &cfg.colors.bar_track,
                lang,
                cx,
            ));
            // 「静音时的填充」只有音量用得上：媒体键和亮度都没有静音状态，
            // 摆一行点了没反应的色块只会让人以为没保存。
            if template == OsdTemplate::Volume {
                body = body.child(self.color_row(
                    ColorSlot::BarMuted,
                    BAR_MUTED_COLORS,
                    &cfg.colors.bar_muted,
                    lang,
                    cx,
                ));
            }
            // 圆角按「半高的百分比」存：条高变了圆角跟着变，永远调得出圆头 / 方角。
            body = body.child(self.stepper(
                "ed-bar-r",
                lang.pick("圆角", "Corner"),
                NumField::BarCorner,
                cx,
            ));
        } else {
            // 其它元素：自己那份颜色（空 = 跟随文字色）。
            body = body.child(self.color_row(
                ColorSlot::Part(part),
                INK_COLORS,
                &cfg.part_color(part),
                lang,
                cx,
            ));
        }

        // 往里缩一格：一眼看出这张卡是从属于上面那一行的。
        div()
            .pl(px(13.))
            .pt(px(2.))
            .pb(px(3.))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(4.))
                    .p(px(6.))
                    .rounded(px(6.))
                    .bg(theme::surface_sunken())
                    .border_1()
                    .border_color(theme::border())
                    .child(body),
            )
            .into_any_element()
    }

    /// 底部那几条操作提示：一行一条，宽度不够就换行（以前整句横排只能看见半截）。
    fn hint_card(&self, lang: Lang) -> AnyElement {
        let lines = [
            lang.pick(
                "选中后：方向键微调 1px，Shift + 方向键 10px",
                "Selected: arrow keys nudge 1px, Shift + arrows 10px",
            ),
            lang.pick("Delete 移除元素", "Delete removes the element"),
            lang.pick(
                "双击元素就地改字 / 双击数值手打 / Ctrl+Z 撤销",
                "Double-click an element to type, double-click a value to type it, Ctrl+Z undoes",
            ),
            lang.pick(
                "色块最右的 ＋ 打开色轮：自由选色 / 粘贴色号 / 屏幕取色",
                "The last swatch opens the wheel: free pick, paste a hex, or sample the screen",
            ),
        ];
        let mut card = div()
            .flex()
            .flex_col()
            .gap(px(2.))
            .p(px(6.))
            .rounded(px(6.))
            .bg(theme::surface_elevated())
            .border_1()
            .border_color(theme::border());
        for line in lines {
            card = card.child(
                div()
                    .flex()
                    .items_start()
                    .gap(px(4.))
                    .child(
                        div()
                            .flex_none()
                            .text_size(px(10.))
                            .text_color(theme::accent())
                            .child("·"),
                    )
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .text_size(px(10.))
                            .text_color(theme::text_tertiary())
                            // 长句要能折行：`whitespace_normal` 之外还得允许容器变窄。
                            .whitespace_normal()
                            .child(line),
                    ),
            );
        }
        card.into_any_element()
    }

    /// 一行「名称 − 数值 + 滑条」：一格 1px / 1%，双击数值可以手打。
    ///
    /// 滑条跟 ± 挤在同一排（原来它单独占一整行，参数一多右栏就长得离谱）。
    fn stepper(
        &self,
        id: &'static str,
        label: &'static str,
        field: NumField,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let store = self.config.clone();
        let value = field.get(&self.cfg());
        let (low, high) = field.range();
        let unit = field.unit();
        let editing = self.num.as_ref().map(|edit| edit.field) == Some(field);
        let mut row = div().flex().items_center().gap(px(4.)).child(
            div()
                .flex_1()
                .min_w(px(0.))
                .truncate()
                .text_size(px(11.5))
                .text_color(theme::text_secondary())
                .child(label),
        );
        for (suffix, delta) in [("-", -1i32), ("+", 1)] {
            let store = store.clone();
            let id_button = SharedString::from(format!("{id}-{suffix}"));
            row = row.child(
                div()
                    .id(id_button)
                    .w(px(18.))
                    .h(px(16.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(3.))
                    .bg(theme::track())
                    .cursor_pointer()
                    .text_size(px(11.))
                    .text_color(theme::text_secondary())
                    .hover(|style| style.bg(theme::surface_hover()))
                    .on_click(cx.listener(move |this, _: &gpui::ClickEvent, _window, cx| {
                        // 手打没提交就先按当前缓冲算，别让 ± 把输入吃掉。
                        if this.num.as_ref().map(|edit| edit.field) == Some(field) {
                            this.commit_num(cx);
                        }
                        let current = field.get(&this.cfg());
                        let next = if delta > 0 {
                            current.saturating_add(1).min(high)
                        } else {
                            current.saturating_sub(1).max(low)
                        };
                        let _ = store
                            .borrow_mut()
                            .update(|config| field.set(&mut config.osd, next));
                        cx.notify();
                    }))
                    .child(suffix),
            );
        }
        // 数值：双击进手打，回车提交、Esc 放弃。负号要能显示（阴影偏移）。
        let value_view: AnyElement = if editing {
            let buffer = self
                .num
                .as_ref()
                .map(|edit| edit.buffer.clone())
                .unwrap_or_default();
            div()
                .id(SharedString::from(format!("{id}-num")))
                .w(px(42.))
                .flex_none()
                .flex()
                .items_center()
                .justify_end()
                .gap(px(1.))
                .text_size(px(11.))
                .text_color(theme::text_primary())
                .child(if buffer.is_empty() {
                    "—".to_string()
                } else {
                    buffer
                })
                .child(div().w(px(1.5)).h(px(11.)).flex_none().bg(theme::accent()))
                .into_any_element()
        } else {
            div()
                .id(SharedString::from(format!("{id}-num")))
                .w(px(42.))
                .flex_none()
                .text_align(gpui::TextAlign::Right)
                .text_size(px(11.))
                .text_color(theme::text_primary())
                .cursor_pointer()
                .child(format!("{value}{unit}"))
                .on_click(
                    cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                        if event.click_count() >= 2 {
                            this.begin_num(field, window, cx);
                        }
                    }),
                )
                .into_any_element()
        };
        row = row.child(value_view);

        // 滑条：跟 ± 和数值挤在同一排，按住就能连续拖。
        // 宽 52 是「参数栏拖到最窄时名字还放得下」换来的：右栏能被拖到 250，
        // 再宽一点「不透明度」这种四字标签就只剩「不透…」。
        let ratio = (value - low) as f32 / (high - low).max(1) as f32;
        let bounds = self.slider_bounds[field.index()].clone();
        let track = div()
            .id(SharedString::from(format!("{id}-track")))
            .w(px(52.))
            .h(px(11.))
            .flex_none()
            .relative()
            .flex()
            .items_center()
            .cursor_pointer()
            .child(
                div()
                    .absolute()
                    .left_0()
                    .right_0()
                    .top(px(3.5))
                    .h(px(4.))
                    .rounded(px(2.))
                    .bg(theme::track()),
            )
            .child(
                div()
                    .absolute()
                    .left_0()
                    .top(px(3.5))
                    .w(gpui::relative(ratio.clamp(0.0, 1.0)))
                    .h(px(4.))
                    .rounded(px(2.))
                    .bg(theme::accent()),
            )
            .child(measure_canvas(bounds))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                    this.slider_drag = Some(field);
                    this.drag_slider(event.position, cx);
                }),
            );
        row = row.child(track);
        row.into_any_element()
    }

    /// 一行「角度 − 数值 + 滑条」：绕自己的中心转，±180°。
    ///
    /// 元素和背景共用这一行（`RotationTarget` 区分）。三个入口给同一件事：
    /// 边框角上的手柄拖着转、这条滑条、数值双击手打。
    fn rotation_row(
        &self,
        target: RotationTarget,
        lang: Lang,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let field = NumField::Rotation(target);
        let value = field.get(&self.cfg());
        let (low, high) = field.range();
        let editing = self.num.as_ref().map(|edit| edit.field) == Some(field);
        let id = SharedString::from(format!("ed-rot-{}", field.index()));
        let mut row = div().flex().items_center().gap(px(4.)).child(
            div()
                .flex_1()
                .min_w(px(0.))
                .truncate()
                .text_size(px(11.5))
                .text_color(theme::text_secondary())
                .child(lang.pick("角度", "Angle")),
        );
        // − / + 各走 15°：够粗也够细，真要精确还有滑条和手打。
        for (suffix, delta) in [("-", -15i32), ("+", 15)] {
            let store = self.config.clone();
            row = row.child(
                div()
                    .id(SharedString::from(format!("{id}-{suffix}")))
                    .w(px(18.))
                    .h(px(16.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(3.))
                    .bg(theme::track())
                    .cursor_pointer()
                    .text_size(px(11.))
                    .text_color(theme::text_secondary())
                    .hover(|style| style.bg(theme::surface_hover()))
                    .on_click(cx.listener(move |this, _: &gpui::ClickEvent, _window, cx| {
                        // 手打没提交就先按当前缓冲算，别让 ± 把输入吃掉。
                        if this.num.as_ref().map(|edit| edit.field) == Some(field) {
                            this.commit_num(cx);
                        }
                        let next = field.get(&this.cfg()).saturating_add(delta);
                        let _ = store
                            .borrow_mut()
                            .update(|config| field.set(&mut config.osd, next));
                        cx.notify();
                    }))
                    .child(suffix),
            );
        }
        // 数值：双击手打（可以是负数），回车提交、Esc 放弃。
        let value_view: AnyElement = if editing {
            let buffer = self
                .num
                .as_ref()
                .map(|edit| edit.buffer.clone())
                .unwrap_or_default();
            div()
                .id(SharedString::from(format!("{id}-num")))
                .w(px(38.))
                .flex_none()
                .flex()
                .items_center()
                .justify_end()
                .gap(px(1.))
                .text_size(px(11.))
                .text_color(theme::text_primary())
                .child(if buffer.is_empty() {
                    "—".to_string()
                } else {
                    format!("{buffer}°")
                })
                .child(div().w(px(1.5)).h(px(11.)).flex_none().bg(theme::accent()))
                .into_any_element()
        } else {
            div()
                .id(SharedString::from(format!("{id}-num")))
                .w(px(38.))
                .flex_none()
                .text_align(gpui::TextAlign::Right)
                .text_size(px(11.))
                .text_color(if value == 0 {
                    theme::text_secondary()
                } else {
                    theme::text_primary()
                })
                .cursor_pointer()
                .child(format!("{value}°"))
                .on_click(
                    cx.listener(move |this, event: &gpui::ClickEvent, window, cx| {
                        if event.click_count() >= 2 {
                            this.begin_num(field, window, cx);
                        }
                    }),
                )
                .into_any_element()
        };
        row = row.child(value_view);
        // 滑条：填充从中点（0°）往两边长，一眼看出是正是负。
        let ratio = ((value - low) as f32 / (high - low).max(1) as f32).clamp(0.0, 1.0);
        let bounds = self.rotation_bounds[target.slot()].clone();
        row = row.child(
            div()
                .id(SharedString::from(format!("{id}-track")))
                .w(px(52.))
                .h(px(11.))
                .flex_none()
                .relative()
                .flex()
                .items_center()
                .cursor_pointer()
                .child(
                    div()
                        .absolute()
                        .left_0()
                        .right_0()
                        .top(px(3.5))
                        .h(px(4.))
                        .rounded(px(2.))
                        .bg(theme::track()),
                )
                .child(
                    div()
                        .absolute()
                        .left(gpui::relative(ratio.min(0.5)))
                        .top(px(3.5))
                        .w(gpui::relative((ratio - 0.5).abs()))
                        .h(px(4.))
                        .rounded(px(2.))
                        .bg(theme::accent()),
                )
                .child(measure_canvas(bounds))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                        this.rotation_drag = Some(target);
                        this.drag_rotation(event.position, cx);
                    }),
                ),
        );
        row.into_any_element()
    }

    /// 拖尺寸滑条：按光标在轨道里的 x 比例换算成数值。
    fn drag_slider(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(field) = self.slider_drag else {
            return;
        };
        let Some(bounds) = self.slider_bounds[field.index()].get() else {
            return;
        };
        let width = bounds.size.width.as_f32().max(1.0);
        let ratio = ((position.x.as_f32() - bounds.origin.x.as_f32()) / width).clamp(0.0, 1.0);
        let (low, high) = field.range();
        let value = (low as f32 + ratio * (high - low) as f32).round() as i32;
        self.update(|osd| field.set(osd, value));
        cx.notify();
    }

    /// 拖角度滑条：按光标在轨道里的 x 比例换算成角度。
    fn drag_rotation(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(target) = self.rotation_drag else {
            return;
        };
        let Some(bounds) = self.rotation_bounds[target.slot()].get() else {
            return;
        };
        let width = bounds.size.width.as_f32().max(1.0);
        let ratio = ((position.x.as_f32() - bounds.origin.x.as_f32()) / width).clamp(0.0, 1.0);
        let (low, high) = crate::config::OSD_ROTATION_RANGE;
        let value = (low as f32 + ratio * (high - low) as f32).round() as i32;
        self.set_rotation(target, value);
        cx.notify();
    }

    /// 按住角手柄转圈：按「光标相对转轴的方向」转了多少，元素就跟着转多少。
    ///
    /// 按住 Shift 吸附到 90° 的整数倍 —— 摆正 / 摆竖差一点点最难对，
    /// 给一个能一下对准的档。
    fn drag_rotate(&mut self, position: Point<Pixels>, shift: bool, cx: &mut Context<Self>) {
        let Some(drag) = self.rotate_drag else {
            return;
        };
        let dx = position.x.as_f32() - drag.center.x.as_f32();
        let dy = position.y.as_f32() - drag.center.y.as_f32();
        // 光标正好压在转轴上时方向没有意义，保持原样别乱跳。
        if dx.abs() < 0.5 && dy.abs() < 0.5 {
            return;
        }
        let angle = dy.atan2(dx);
        let delta = (angle - drag.start_angle).to_degrees();
        let mut value = drag.origin + delta;
        if shift {
            value = (value / 90.0).round() * 90.0;
        }
        self.set_rotation(drag.target, value.round() as i32);
        cx.notify();
    }

    /// 把角度写进配置（元素 / 背景各一套入口，这里统一）。
    fn set_rotation(&mut self, target: RotationTarget, degrees: i32) {
        let degrees = crate::config::normalize_rotation(degrees as f32);
        self.update(|osd| match target {
            RotationTarget::Card => osd.set_card(|card| card.rotation = degrees.round() as i32),
            RotationTarget::Part(part) => osd.set_part_rotation(part, degrees),
        });
    }

    /// 拖元素列表的行：按光标压在哪一行上，决定挪到第几层。
    fn drag_layer(&mut self, position: Point<Pixels>, cx: &mut Context<Self>) {
        let Some(part) = self.layer_drag else {
            return;
        };
        let cfg = self.cfg();
        let order = cfg.parts();
        // 只认「现在还在列表里的元素」，按上、下顺序排好 —— 列表第一行是最上层。
        let mut rows: Vec<(u32, Bounds<Pixels>)> = {
            let rows = self.layer_rows.borrow();
            rows.iter()
                .filter(|(index, _)| order.iter().any(|part| part.index() == *index))
                .copied()
                .collect()
        };
        if rows.is_empty() {
            return;
        }
        rows.sort_by(|a, b| a.1.origin.y.as_f32().total_cmp(&b.1.origin.y.as_f32()));
        let count = rows.len();
        let y = position.y.as_f32();
        // 夹到两头：拖出列表上沿 = 放到最上层，拖出下沿 = 最下层。
        let row_from_top = rows
            .iter()
            .position(|(_, bounds)| {
                let top = bounds.origin.y.as_f32();
                y < top + bounds.size.height.as_f32()
            })
            .unwrap_or(count - 1);
        // 行号从上往下，`parts()` 从下往上是图层顺序 —— 要翻过来。
        let target = count - 1 - row_from_top;
        if order.iter().position(|slot| *slot == part) == Some(target) {
            return;
        }
        self.update(|osd| osd.move_part_to(part, target));
        cx.notify();
    }

    /// 一行颜色：预设色块 + 最右边一个「自定义」入口（开色轮）。
    fn color_row(
        &self,
        slot: ColorSlot,
        presets: &'static [&'static str],
        current: &str,
        lang: Lang,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let id = SharedString::from(format!("ed-color-{}", slot.index()));
        let mut row = div().flex().items_center().gap(px(2.)).child(
            div()
                // 名字都控制在三个字以内（`row_label` 负责），多留的宽度会被
                // 「静音时音量条」这种长名字吃掉，色块就挤到行外去了。
                // 44px 正好够三个 11px 的字：参数栏能拖到多窄，就卡在这点余量上。
                .w(px(44.))
                .flex_none()
                .truncate()
                .text_size(px(11.))
                .text_color(theme::text_secondary())
                .child(slot.row_label(lang)),
        );
        for (index, hex) in presets.iter().enumerate() {
            let value = hex.to_string();
            let on = current.eq_ignore_ascii_case(hex);
            // 第一个空串 = 「跟随主题」：用半深半浅那块标出来，不看文字也分得清。
            let swatch: AnyElement = if hex.is_empty() {
                ui::follow_swatch(13.0, 3.0, on).into_any_element()
            } else {
                div()
                    .flex_none()
                    .w(px(13.))
                    .h(px(13.))
                    .rounded(px(3.))
                    .bg(theme::parse_hex(hex).unwrap_or_else(theme::surface))
                    .border_1()
                    .border_color(if on {
                        theme::accent()
                    } else {
                        theme::border_strong()
                    })
                    .into_any_element()
            };
            row = row.child(
                div()
                    .id(SharedString::from(format!("{id}-{index}")))
                    .flex_none()
                    .cursor_pointer()
                    .child(swatch)
                    .on_click(cx.listener(move |this, _: &gpui::ClickEvent, _window, cx| {
                        this.set_color(slot, value.clone());
                        // 静音那段颜色要按静音态画，别的时候照旧。
                        this.preview_muted = slot == ColorSlot::BarMuted;
                        cx.notify();
                    })),
            );
        }

        // 自定义：开 / 关色轮。
        let open = self.picker.as_ref().map(|picker| picker.slot) == Some(slot);
        let swatch = theme::parse_hex(current).unwrap_or_else(theme::surface);
        row = row.child(
            div()
                .id(SharedString::from(format!("{id}-custom")))
                .w(px(16.))
                .h(px(16.))
                .flex_none()
                .rounded(px(4.))
                .bg(swatch)
                .border_1()
                .border_color(if open {
                    theme::accent()
                } else {
                    theme::border_strong()
                })
                .flex()
                .items_center()
                .justify_center()
                .cursor_pointer()
                .hover(|style| style.border_color(theme::accent()))
                .on_click(cx.listener(move |this, _: &gpui::ClickEvent, _window, cx| {
                    this.open_picker(slot, cx);
                }))
                .child(
                    div()
                        .text_size(px(10.))
                        .text_color(theme::text_primary())
                        .child("+"),
                ),
        );
        row.into_any_element()
    }
}

/// 预览图标：音量类还要按用户选的喇叭样式换档（弧线1 / 弧线2 / 竖条 / 圆点 / 音箱）。
fn preview_icon(cfg: &crate::config::OsdConfig, raw: &'static str) -> &'static str {
    if !raw.starts_with("volume-") {
        return raw;
    }
    let style = cfg.icon_style();
    match style.states {
        Some(states) => states[crate::windows::osd::volume_slot(raw)],
        None => style.icon,
    }
}

/// 编辑器里的示例内容。
fn sample_state(cfg: &crate::config::OsdConfig, lang: Lang, preview: Option<usize>) -> OsdState {
    // 状态和图标是同一张表（`PREVIEWS`）的两列：画布按选中的那一条同时拿图标和文字。
    // 没选（自动）时，媒体模板照真实提示条画「播放」，音量模板画用户选的喇叭。
    let (icon, auto_title, muted) = match preview {
        Some(index) => {
            let option = &PREVIEWS[index.min(PREVIEWS.len() - 1)];
            (
                preview_icon(cfg, option.icon),
                lang.pick(option.zh, option.en),
                option.muted,
            )
        }
        None if cfg.template() == crate::config::OsdTemplate::Media => ("play", lang.pick("播放", "Play"), false),
        None if cfg.template() == crate::config::OsdTemplate::Brightness => {
            ("sun", lang.pick("亮度", "Brightness"), false)
        }
        None => (cfg.icon_style().icon, lang.pick("音量", "Volume"), false),
    };
    OsdState {
        function: if cfg.template() == crate::config::OsdTemplate::Brightness {
            OsdFunction::Brightness
        } else {
            OsdFunction::Volume
        },
        target: OsdTarget::Device,
        icon,
        title: if !cfg.title_text.trim().is_empty() {
            cfg.title_text.clone()
        } else {
            auto_title.to_string()
        },
        subtitle: String::new(),
        // 三样读数在**两套模板里都画**：媒体键模板现在也能自己打开它们，
        // 打开就得看得见（否则参数调了却毫无反应）。宽度不够时把元素拉宽即可。
        // 条体固定画 72%：留 28% 才看得见「底色」那一段。
        // 读数必须跟条体一致，否则调底色 / 静音色时读数写 100%、条却是七成，对不上。
        percent: cfg.percent_display("72%"),
        db: "-0.0 dB".to_string(),
        fraction: 0.72,
        show_bar: true,
        muted,
        playing: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 加了数值项忘记把 `NUM_FIELDS` 加上去，会在 `slider_bounds[index]` 上 panic。
    #[test]
    fn num_fields_cover_every_index() {
        let max = [
            NumField::Width,
            NumField::Height,
            NumField::Corner,
            NumField::BarCorner,
            NumField::Opacity,
            NumField::ShadowX,
            NumField::ShadowY,
            NumField::ShadowBlur,
            NumField::ShadowOpacity,
            NumField::Rotation(RotationTarget::Card),
            NumField::Rotation(RotationTarget::Part(OsdPart::Bar)),
        ]
        .iter()
        .map(|field| field.index())
        .max()
        .unwrap();
        assert_eq!(max, NUM_FIELDS - 1, "NUM_FIELDS 要等于最大的下标 + 1");
    }

    /// 背景和元素的角度都走 `NumField`：读回来要跟写进去的一样（含正负）。
    #[test]
    fn rotation_field_round_trips() {
        let mut osd = crate::config::OsdConfig::default();

        let card = NumField::Rotation(RotationTarget::Card);
        card.set(&mut osd, 45);
        assert_eq!(card.get(&osd), 45);
        card.set(&mut osd, -90);
        assert_eq!(card.get(&osd), -90);
        // 越界的绕回来：195° 就是 -165°。
        card.set(&mut osd, 195);
        assert_eq!(card.get(&osd), -165);

        let part = NumField::Rotation(RotationTarget::Part(OsdPart::Icon));
        part.set(&mut osd, 30);
        assert_eq!(part.get(&osd), 30);
        part.set(&mut osd, -30);
        assert_eq!(part.get(&osd), -30);
        // 两个目标互不干扰。
        assert_eq!(card.get(&osd), -165);
    }

    #[test]
    fn hsv_round_trips_primary_colors() {
        for (rgb, hue) in [
            ((255u8, 0u8, 0u8), 0.0),
            ((0, 255, 0), 120.0),
            ((0, 0, 255), 240.0),
        ] {
            let (h, s, v) = rgb_to_hsv(rgb.0, rgb.1, rgb.2);
            assert!((h - hue).abs() < 0.01, "{rgb:?} -> {h}");
            assert!((s - 1.0).abs() < 0.001);
            assert!((v - 1.0).abs() < 0.001);
            assert_eq!(hsv_to_rgb(h, s, v), rgb);
        }
    }

    #[test]
    fn hsv_matches_hex_for_known_color() {
        // h=210 (sector 3), s=0.8 => p=51, q=153, t=153 -> #3399ff
        assert_eq!(hex_of(hsv_to_rgb(210.0, 0.8, 1.0)), "#3399ff");
        assert_eq!(hex_of(hsv_to_rgb(0.0, 0.0, 1.0)), "#ffffff");
        assert_eq!(hex_of(hsv_to_rgb(0.0, 1.0, 1.0)), "#ff0000");
        assert_eq!(hex_of(hsv_to_rgb(120.0, 1.0, 1.0)), "#00ff00");
    }

    #[test]
    fn hex_parses_with_and_without_hash() {
        assert_eq!(parse_rgb("#7c9cff"), Some((0x7c, 0x9c, 0xff)));
        assert_eq!(parse_rgb("7c9cff"), Some((0x7c, 0x9c, 0xff)));
        assert_eq!(parse_rgb("#fff"), Some((0xff, 0xff, 0xff)));
        assert_eq!(parse_rgb("  #FFF  "), Some((0xff, 0xff, 0xff)));
        assert_eq!(parse_rgb("#12345"), None);
        assert_eq!(parse_rgb("zzzzzz"), None);
    }

    #[test]
    fn push_hex_keeps_one_hash_and_caps_length() {
        let mut buffer = String::new();
        push_hex(&mut buffer, "#7c9cff");
        assert_eq!(buffer, "#7c9cff");

        // 已有的 # 不会被顶掉，超出长度的字符直接丢。
        push_hex(&mut buffer, "#ff");
        assert_eq!(buffer, "#7c9cff");

        let mut short = String::from("#12");
        push_hex(&mut short, "3456789");
        assert_eq!(short, "#123456");
    }

    #[test]
    fn insert_lands_where_the_caret_was() {
        // 光标在中间：字插进去，光标跟着往后挪一格。
        let (next, marked, caret) = text_apply("hello", None, Some(2..2), "X", false, 40);
        assert_eq!(next, "heXllo");
        assert_eq!(caret, 3);
        assert!(marked.is_none());
    }

    #[test]
    fn replacing_a_selection_drops_the_caret_at_its_end() {
        // 选中 1..4 再打字：整段被换掉，光标落在插入的文字后面。
        let (next, _, caret) = text_apply("hello", None, Some(1..4), "i", false, 40);
        assert_eq!(next, "hio");
        assert_eq!(caret, 2);
    }

    #[test]
    fn composing_marks_the_inserted_range() {
        let (next, marked, caret) = text_apply("ab", None, Some(1..1), "拼", true, 40);
        assert_eq!(next, "a拼b");
        assert_eq!(marked, Some(1..2));
        assert_eq!(caret, 2);
    }

    #[test]
    fn char_steps_respect_utf16_width() {
        // 中文 / emoji 在 UTF-16 里占两格，左右挪一格要整块跳过去。
        assert_eq!(next_char_len("中文", 0), 1);
        assert_eq!(prev_char_len("中文", 2), 1);
        assert_eq!(next_char_len("😀a", 0), 2);
        assert_eq!(prev_char_len("😀a", 2), 2);
        assert_eq!(prev_char_len("abc", 0), 0);
        assert_eq!(next_char_len("abc", 3), 0);
    }

    #[test]
    fn selection_normalizes_both_directions() {
        let mut edit = TextEdit {
            part: OsdPart::Text(0),
            buffer: String::from("hello"),
            marked: None,
            caret: 4,
            anchor: 1,
            undo: Vec::new(),
        };
        assert_eq!(edit.selection(), 1..4);
        // 从右往左拖：范围同样是从左到右。
        edit.caret = 1;
        edit.anchor = 4;
        assert_eq!(edit.selection(), 1..4);
        // 光标挪走但要求保留锚点 = 拖选；不保留 = 收掉选区。
        edit.move_caret(3, true);
        assert_eq!(edit.selection(), 3..4);
        edit.move_caret(0, false);
        assert!(!edit.has_selection());
        // 越界自动夹到末尾。
        edit.move_caret(99, false);
        assert_eq!(edit.caret, 5);
    }

    #[test]
    fn undo_steps_back_one_edit_at_a_time() {
        let mut edit = TextEdit {
            part: OsdPart::Text(0),
            buffer: String::from("ab"),
            marked: None,
            caret: 2,
            anchor: 2,
            undo: Vec::new(),
        };
        // 敲一个字：压栈的是「敲之前」的样子。
        edit.snapshot();
        edit.buffer.push('c');
        edit.caret = 3;
        edit.anchor = 3;
        edit.snapshot();
        edit.buffer.push('d');
        edit.caret = 4;
        edit.anchor = 4;
        assert_eq!(edit.buffer, "abcd");

        assert!(edit.undo());
        assert_eq!(edit.buffer, "abc");
        assert_eq!(edit.caret, 3);
        assert!(edit.undo());
        assert_eq!(edit.buffer, "ab");
        assert_eq!(edit.caret, 2);
        // 压栈都退完了：再按没反应，也不能把内容弄丢。
        assert!(!edit.undo());
        assert_eq!(edit.buffer, "ab");
    }

    #[test]
    fn undo_restores_the_selection_too() {
        let mut edit = TextEdit {
            part: OsdPart::Text(0),
            buffer: String::from("hello"),
            marked: None,
            caret: 0,
            anchor: 0,
            undo: Vec::new(),
        };
        // 选中 1..4 再删掉：撤销要把选区一起还回来。
        edit.caret = 4;
        edit.anchor = 1;
        edit.snapshot();
        edit.buffer = String::from("ho");
        edit.caret = 1;
        edit.anchor = 1;
        assert!(edit.undo());
        assert_eq!(edit.buffer, "hello");
        assert_eq!(edit.selection(), 1..4);
    }

    fn osd_with_width(width: u32) -> crate::config::OsdConfig {
        let mut osd = crate::config::OsdConfig::default();
        osd.width = width;
        osd
    }

    /// 撤销按「后进先出」，重做按原顺序回来；新改动会清掉重做栈。
    #[test]
    fn history_undo_redo_walks_back_and_forth() {
        let mut history = History::new();
        assert!(!history.can_undo() && !history.can_redo());
        history.push(osd_with_width(180), false);
        history.push(osd_with_width(190), false);
        // 现在值 200。
        let previous = history.undo(osd_with_width(200)).expect("能退");
        assert_eq!(previous.width, 190);
        assert!(history.can_redo());
        let previous = history.undo(osd_with_width(190)).expect("能退");
        assert_eq!(previous.width, 180);
        assert!(!history.can_undo(), "两步就退到底了");
        let next = history.redo(osd_with_width(180)).expect("能重做");
        assert_eq!(next.width, 190);
        // 退回去以后又改了：重做栈清空。
        history.push(osd_with_width(190), false);
        assert!(!history.can_redo());
    }

    /// 一次拖动只算一步：手势里连着 push 只留最开头那份。
    #[test]
    fn history_merges_one_gesture_into_one_step() {
        let mut history = History::new();
        for width in [181u32, 182, 183, 184] {
            history.push(osd_with_width(width), true);
        }
        assert_eq!(history.past.len(), 1);
        // 松手之后的下一次改动重新算一步。
        history.push(osd_with_width(185), false);
        assert_eq!(history.past.len(), 2);
    }

    /// 最多留 10 步：再多就从最老的那头丢。
    #[test]
    fn history_keeps_only_ten_steps() {
        let mut history = History::new();
        for width in 0..20u32 {
            history.push(osd_with_width(180 + width), false);
        }
        assert_eq!(history.past.len(), HISTORY_STEPS);
        // 最老的是第 10 次记的那份（w = 190）。
        assert_eq!(history.past.first().map(|osd| osd.width), Some(190));
        assert_eq!(history.past.last().map(|osd| osd.width), Some(199));
    }
}
