//! 显示器亮度窗口：跟在音量面板旁边一起弹出的小面板。
//!
//! 只有开启了「显示器亮度」才会出现（默认关闭）。内容按显示器一行：
//! 名称 + 亮度读数（可点开手动输入）+ 亮度条（跟音量条同一套控件）+ 预设。
//!
//! 数据都来自工作线程（`crate::display`），这里只管画和收鼠标键盘。

use std::{
    cell::RefCell,
    collections::HashMap,
    rc::Rc,
    time::{Duration, Instant},
};

use gpui::prelude::*;
use gpui::{
    Animation, AnimationExt, App, ClickEvent, Context, ElementId, FocusHandle, FontWeight,
    IntoElement, KeyDownEvent, Render, SharedString, TextRun, Window, div, px, Pixels,
};

use crate::{
    config::{BrightnessPlacement, Config, ConfigStore, DisplayNameMode},
    display::{DisplayCommand, DisplayEngine, DisplayInfo},
    i18n::Lang,
    ui::{Entrance, EntranceMotion, SliderDragState, VolumeSlider, anim, icon, theme, wheel_direction},
};

/// 步进钮宽度（横排挪到名称行里，竖排还在读数两侧）。
const STEP_WIDTH: f32 = 16.0;

const PAD: f32 = 7.0;
/// 横排的左右内边距：和音量面板里的行对齐（那边是 `mx(6)` + `px(8)`）。
const H_PAD: f32 = 14.0;
/// 亮度条左边让出的宽度、条与读数之间的缝、读数栏宽度 —— 三个数照抄音量面板的
/// 设备行（`device_row` 的 `pl(27)` / `gap(9)` / `readout_width`），
/// 这样两张条一样长、左右也对齐，上下摆在一起看不出是两套控件。
const H_BAR_INSET: f32 = 27.0;
const H_READOUT_GAP: f32 = 9.0;
const H_READOUT_WIDTH: f32 = 34.0;
const HEADER_HEIGHT: f32 = 28.0;
const NAME_ROW_HEIGHT: f32 = 20.0;
/// 亮度条那一排的高度：条本身 6px，留够圆钮（13px）上下活动的余地，
/// 比音量面板的 24 矮一档，整块跟着短一截。
const SLIDER_HEIGHT: f32 = 20.0;
/// 同一台显示器里：名称行 / 亮度条 / 预设行之间的缝。
const ROW_GAP: f32 = 1.0;
/// 预设 chip 的高度与行距。
const CHIP_HEIGHT: f32 = 20.0;
const CHIP_GAP: f32 = 3.0;
/// 每台显示器之间的间距。
const BLOCK_GAP: f32 = 8.0;
/// 拖放落点的吸附范围：落在块与块之间的缝里也算最近的那一块。
const DRAG_SNAP: f32 = 10.0;
/// 指针挪出去多远才算「拖」，以内都按点击处理。
const DRAG_THRESHOLD: f32 = 4.0;
/// 套用成功后目标块闪一下的时长（毫秒）。
const FLASH_MS: u64 = 320;
/// 闪光最亮那一下的不透明度（之后二次缓出淡掉）。
const FLASH_PEAK: f32 = 0.45;

/// 一台显示器在这一屏上的状态：能调 / 开了 HDR / DDC/CI 不通。
///
/// 三种状态长得不一样：能调的是百分比读数、条也是活的；后两种读数位置换成
/// 一句原因（`HDR` / `不支持`），条和步进钮一并变灰 —— 拖了没反应比直接说
/// 「调不了」难受得多。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Slot {
    /// 能调，值是百分比。
    Ready(u32),
    /// 开着 HDR：DDC/CI 照写不误，画面被色调映射吃掉。
    Hdr,
    /// DDC/CI 读不通（笔记本内屏、转接线、显示器里没开 DDC/CI）。
    Unsupported,
}

impl Slot {
    fn of(display: &DisplayInfo, percent: Option<u32>) -> Self {
        if display.hdr {
            Slot::Hdr
        } else {
            match percent {
                Some(value) => Slot::Ready(value),
                None => Slot::Unsupported,
            }
        }
    }

    /// 拖得动才给值，否则条摆到 0 也无所谓（反正是灰的）。
    fn value(self) -> u32 {
        match self {
            Slot::Ready(value) => value,
            Slot::Hdr | Slot::Unsupported => 0,
        }
    }

    fn editable(self) -> bool {
        matches!(self, Slot::Ready(_))
    }
}

/// 竖向排版的一列有多宽、节奏怎么走。
///
/// 一列要同时装下「− 100% +」这一排和几个预设格子，所以宽度不是随便压的：
/// 16（步进钮）+ 30（读数）+ 16（步进钮）+ 2（缝）= 64。
const V_COLUMN_WIDTH: f32 = 64.0;
const V_COLUMN_GAP: f32 = 4.0;
/// 列内左右内边距（比横排的 PAD 小一档，列本来就窄）。
const V_COLUMN_PAD: f32 = 3.0;
/// 竖向推子的最短长度：面板高度配得很矮时也别把推子压成一条线。
const V_FADER_MIN: f32 = 40.0;
/// 列里的名称行 / 读数+步进行。
const V_LABEL_ROW: f32 = 18.0;
const V_STEP_ROW: f32 = 18.0;

/// 面板整块多大（宽, 高）。
///
/// 音量面板要按这个给亮度窗让位（贴在它右 / 下侧时得挪开），所以做成自由函数，
/// 两边用同一份算法，不会各算各的。两种排版宽高的来源完全不同：
/// 横排的宽度由用户配（`brightness.width`），竖排的宽度由显示器台数决定。
pub fn panel_size(cfg: &Config, displays: usize) -> (f32, f32) {
    if cfg.brightness.panel_layout.is_vertical() {
        (vertical_width(displays), vertical_height(cfg, displays, false))
    } else {
        (cfg.brightness_width(), horizontal_height(cfg, displays, false))
    }
}

/// 亮度块每一行的高度。
///
/// 全部按音量面板的「行高」等比伸缩：面板行高（`theme::ROW_HEIGHT`，44）算 1 倍，
/// 用户把行高调高，亮度块跟着一起变高 —— 两块摆在一块儿才不会一个松一个挤。
/// 只改尺寸不改字：和面板本身「字号只改字、行高只改行」的分工一致。
#[derive(Clone, Copy, Debug)]
struct Metrics {
    name_row: f32,
    slider_row: f32,
    chip: f32,
    label_row: f32,
    step_row: f32,
}

/// 亮度块的 1 倍行高 = 默认配置下的面板行高（图标 32 + 20）。
///
/// 拿它当基准，默认配置下亮度块的行高（20）一个像素都不动；
/// 用户调「面板行高」或应用图标大小，亮度块才跟着等比伸缩。
const BASE_ROW_HEIGHT: f32 = 52.0;

fn metrics(cfg: &Config) -> Metrics {
    let scale = (crate::windows::flyout::panel_row_height(cfg) / BASE_ROW_HEIGHT).clamp(0.7, 2.5);
    Metrics {
        name_row: (NAME_ROW_HEIGHT * scale).round(),
        slider_row: (SLIDER_HEIGHT * scale).round(),
        chip: (CHIP_HEIGHT * scale).round(),
        label_row: (V_LABEL_ROW * scale).round(),
        step_row: (V_STEP_ROW * scale).round(),
    }
}

/// 横排：每台显示器一块（名称行 + 亮度条），最底下一排共用的预设。
fn horizontal_height(cfg: &Config, displays: usize, embedded: bool) -> f32 {
    let m = metrics(cfg);
    let count = displays.max(1) as f32;
    let block = m.name_row + ROW_GAP + m.slider_row;
    // 多台之间还夹一条 1px 分隔线，上下各 BLOCK_GAP。
    let separators = (count - 1.0).max(0.0);
    (chrome_height(embedded, false)
        + count * block
        + separators * (BLOCK_GAP * 2.0 + 1.0)
        + preset_bar_extra(cfg, displays))
    .max(80.0)
}

/// 竖排：一台显示器一列，宽 = 内边距 + 若干列。
fn vertical_width(displays: usize) -> f32 {
    let count = displays.max(1) as f32;
    PAD * 2.0 + count * V_COLUMN_WIDTH + (count - 1.0) * V_COLUMN_GAP
}

/// 竖排：标题 + 最高的一列 + 底部预设排。
///
/// `brightness.vertical_height` 写 0（默认）就跟着**音频面板竖向排版时的高度**走，
/// 两个面板并排时上下齐平；写了数字就自己定。
fn vertical_height(cfg: &Config, displays: usize, embedded: bool) -> f32 {
    let at_least = chrome_height(embedded, true)
        + column_fixed_height(cfg)
        + V_FADER_MIN
        + preset_bar_extra(cfg, displays);
    let target = match cfg.brightness.vertical_panel_height() {
        Some(height) => height,
        None => crate::windows::flyout::vertical_panel_height(cfg),
    };
    target.max(at_least)
}

/// 上下内边距 +（独立窗才有的）标题与分隔线。
///
/// 融合进音量面板时标题整个取消：那块地方本来就紧挨着音频，多一行「显示器亮度」
/// 只是白占高度。
fn chrome_height(embedded: bool, vertical: bool) -> f32 {
    let pad = if vertical { PAD } else { H_PAD };
    let head = if embedded { 0.0 } else { HEADER_HEIGHT + 1.0 };
    pad * 2.0 + head
}

/// 竖排时交给各列的高度：总高扣掉标题那一层、再扣掉底部那排预设。
fn columns_height(cfg: &Config, displays: usize, embedded: bool) -> f32 {
    vertical_height(cfg, displays, embedded)
        - chrome_height(embedded, true)
        - preset_bar_extra(cfg, displays)
}

/// 一列里除推子之外的高度：名称 / 读数+步进 / 保存预设，以及它们之间的缝。
fn column_fixed_height(cfg: &Config) -> f32 {
    let m = metrics(cfg);
    m.label_row + ROW_GAP * 3.0 + m.step_row + m.chip
}

/// 一个 chip 的宽度估算。
///
/// gpui 没有「排版前测文字宽度」的口子，只能按字号估：10.5px 字号下 ASCII 约
/// 6.2px/字、中文约 10.5px/字，再加左右内边距 14px。估偏几像素看不出来，
/// 但少算了会把 chip 切掉，所以宁可略大。
fn chip_width(name: &str) -> f32 {
    14.0 + name
        .chars()
        .map(|ch| if ch.is_ascii() { 6.2 } else { 10.5 })
        .sum::<f32>()
}

/// 「保存预设」这四个字：横排有大把宽度就写全，竖排一列只有 58px，英文缩成 Save。
fn save_label(lang: Lang, vertical: bool) -> &'static str {
    if vertical {
        lang.pick("保存预设", "Save")
    } else {
        lang.pick("保存预设", "Save preset")
    }
}

/// 底部那排预设有多高（放不下就折行，折出来的行数也算进去）。
///
/// 没有预设就是 0 —— 这一排整个不出现。
fn preset_bar_height(cfg: &Config, displays: usize) -> f32 {
    let presets = &cfg.brightness.presets;
    if presets.is_empty() {
        return 0.0;
    }
    let m = metrics(cfg);
    let inner = preset_bar_width(cfg, displays);
    let mut rows = 1.0;
    let mut used = 0.0;
    for preset in presets {
        let width = chip_width(&preset.name);
        if used > 0.0 && used + CHIP_GAP + width > inner {
            rows += 1.0;
            used = width;
        } else {
            used += if used > 0.0 { CHIP_GAP } else { 0.0 } + width;
        }
    }
    rows * m.chip + (rows - 1.0) * CHIP_GAP
}

/// 预设排连同上面那条缝一共占多高。
fn preset_bar_extra(cfg: &Config, displays: usize) -> f32 {
    let height = preset_bar_height(cfg, displays);
    if height > 0.0 { BLOCK_GAP + height } else { 0.0 }
}

/// 预设那一排能用的宽度：横排是面板净宽，竖排是所有列加起来的净宽。
fn preset_bar_width(cfg: &Config, displays: usize) -> f32 {
    let full = if cfg.brightness.panel_layout.is_vertical() {
        vertical_width(displays) - PAD * 2.0
    } else {
        cfg.brightness.panel_width(cfg.flyout_width()) - H_PAD * 2.0
    };
    full.max(40.0)
}

/// 融合模式下亮度块要占多大。
///
/// 尺寸由父面板决定（横排铺满宽度、竖排撑满高度），这里只记下来给度量用：
/// `cfg()` 会把这两个数写进 `brightness.width` / `brightness.vertical_height`，
/// 于是滑条长度、推子长度、预设折行全都照常按「面板多宽 / 多高」算，
/// 一行渲染代码都不用为融合另写一份。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Embed {
    /// 竖向排版（一台显示器一列），否则一行一台。
    pub vertical: bool,
    /// 横排时这一块的宽度（= 音量面板宽度）。
    pub width: f32,
    /// 竖排时这一块的高度（= 音量面板高度）。
    pub height: f32,
}

/// 融合时亮度块要占多高（横排用；宽度就是音量面板宽度）。
///
/// 与独立窗口那份算法是同一条（`horizontal_height`），只是宽度换成面板宽度 ——
/// 预设 chips 的折行也跟着换，父面板按这个数加高就行。
pub fn embed_height(cfg: &crate::config::Config, displays: usize) -> f32 {
    let mut cfg = cfg.clone();
    cfg.brightness.width = cfg.flyout_width().round().max(0.0) as u32;
    horizontal_height(&cfg, displays, true)
}

/// 融合时亮度块要占多宽（竖排用；高度由父面板撑满）。
pub fn embed_width(displays: usize) -> f32 {
    vertical_width(displays)
}

/// 竖排融合时亮度列**至少**要这么高：名称 + 读数 + 保存预设 + 底部那排预设，
/// 推子再短也有 `V_FADER_MIN`。
///
/// 父面板（音量面板）竖向排版时按自己的高度给亮度列让位，可预设攒多了这排会不够用 ——
/// 那时面板整体得抬到这一份，不然最底下的预设格子会被面板底边裁掉一半。
pub fn min_embed_height(cfg: &crate::config::Config, displays: usize) -> f32 {
    chrome_height(true, true)
        + column_fixed_height(cfg)
        + V_FADER_MIN
        + preset_bar_extra(cfg, displays)
}

pub struct Brightness {
    pub config: Rc<RefCell<ConfigStore>>,
    pub engine: Rc<DisplayEngine>,
    /// 工作线程报上来的显示器列表（与 `AppState` 共用同一份）。
    pub displays: Rc<RefCell<Vec<DisplayInfo>>>,
    /// 融合模式：`Some` 表示这块内容长在音量面板里，自己不建窗口、不跑动画。
    embed: Option<Embed>,
    focus: FocusHandle,
    drag: SliderDragState,
    /// 拖动滑条期间的本地值（key → 0..1）。DDC 写入慢，等回读再动会拖手。
    drag_values: Rc<RefCell<HashMap<String, f32>>>,
    /// 正在手动输入哪一台的数值：(显示器 key, 已输入的字符)。
    editing: Option<(String, String)>,
    /// 刚点开输入格、还没敲过数字：第一下数字要顶掉原值。
    edit_replace: bool,
    /// 读不到亮度时最后一次的错误（例如枚举失败）。
    pub error: Rc<RefCell<Option<String>>>,
    visible: bool,
    last_height: f32,
    /// 窗口该停的位置（逻辑像素，左上角 + 宽高）：`update_frame` 算好存这儿，
    /// 系统风格的滑入每帧在它上面加位移。
    base_frame: (f32, f32, f32, f32),
    /// 正在跑的那段滑动（独立线程按合成帧推进）；None = 没动画在跑。
    slide: Option<crate::windows::platform::SlideHandle>,
    /// 静止时窗口停在哪：0 = 在位，`slide_distance` = 整个在屏幕外。
    rest_offset: f32,
    /// 入场动画：和音量面板共用 `ui::anim::Entrance`，两扇窗必须同时长出来。
    anim: Entrance,
    /// 上一次处理滚轮的时刻：`on_scroll_wheel` 在同一帧里可能被派发好几遍，
    /// 不节流的话一格滚轮会连跳好几档（音量面板里踩过同一个坑）。
    wheel_last_at: Option<Instant>,
    /// 正按着某颗预设往显示器上拖（松手决定套给谁）。
    drag_preset: Option<PresetDrag>,
    /// 拖动中光标在哪（窗口坐标）：那颗跟着走的预设照它摆。
    drag_at: Option<gpui::Point<Pixels>>,
    /// 刚套用完、正在闪的那几块。
    flash: Option<Flash>,
    /// 闪光的序号：连着套用两次时，旧的那段收尾不许把新的清掉。
    flash_seq: u64,
    /// 每台显示器那一块的矩形（各块里的 canvas 每帧量一次），拖放时按坐标认落点。
    block_rects: Rc<RefCell<HashMap<String, gpui::Bounds<Pixels>>>>,
    /// 整块内容的矩形：松手时判断「还在面板里」还是拖出去了。
    drag_area: Rc<std::cell::Cell<Option<gpui::Bounds<Pixels>>>>,
    /// 右键点了哪颗预设、正等着确认删除。
    confirm_delete: Option<usize>,
}

/// 正在拖的那颗预设。
struct PresetDrag {
    /// 预设序号（配置里的下标）。
    index: usize,
    /// 要套用的亮度值 —— 按下时就记下来，拖的过程中配置要是变了也不影响。
    value: u32,
    /// 此刻悬在哪台显示器上（那一块会提亮）。
    over: Option<String>,
    /// 按下时指针在哪：区分「点了一下」和「拖出去了」。
    origin: gpui::Point<Pixels>,
    /// 指针离开过按下点：这一趟算拖放，落在空处就什么也不做。
    moved: bool,
}

/// 套用成功后的闪光：哪几块、从什么时候开始。
struct Flash {
    keys: Vec<String>,
}

impl Brightness {
    pub fn new(
        config: Rc<RefCell<ConfigStore>>,
        engine: Rc<DisplayEngine>,
        displays: Rc<RefCell<Vec<DisplayInfo>>>,
        error: Rc<RefCell<Option<String>>>,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus = cx.focus_handle();
        Self {
            config,
            engine,
            displays,
            embed: None,
            focus,
            drag: SliderDragState::new(),
            drag_values: Rc::new(RefCell::new(HashMap::new())),
            editing: None,
            edit_replace: false,
            error,
            visible: false,
            last_height: 0.0,
            base_frame: (0.0, 0.0, 0.0, 0.0),
            slide: None,
            rest_offset: 0.0,
            anim: Entrance::new(),
            wheel_last_at: None,
            drag_preset: None,
            drag_at: None,
            flash: None,
            flash_seq: 0,
            block_rects: Rc::new(RefCell::new(HashMap::new())),
            drag_area: Rc::new(std::cell::Cell::new(None)),
            confirm_delete: None,
        }
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// 建一份「长在音量面板里」的亮度块：同一套渲染与交互，只是不建窗口、
    /// 不跑入场动画，尺寸由父面板给。
    pub fn new_embedded(
        config: Rc<RefCell<ConfigStore>>,
        engine: Rc<DisplayEngine>,
        displays: Rc<RefCell<Vec<DisplayInfo>>>,
        error: Rc<RefCell<Option<String>>>,
        embed: Embed,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut view = Self::new(config, engine, displays, error, cx);
        view.embed = Some(embed);
        view
    }


    fn cfg(&self) -> crate::config::Config {
        let mut config = self.config.borrow().get().clone();
        // 融合模式：尺寸跟着父面板走，直接把数写进配置副本，后面的度量、滑条、
        // 预设折行全都按这一份算，不必为每种容器各写一套。
        if let Some(embed) = self.embed {
            config.brightness.panel_layout = if embed.vertical {
                crate::config::PanelLayout::Vertical
            } else {
                crate::config::PanelLayout::Horizontal
            };
            if embed.vertical {
                config.brightness.vertical_height = embed.height.round().max(0.0) as u32;
            } else {
                config.brightness.width = embed.width.round().max(0.0) as u32;
            }
        }
        config
    }

    /// 融合模式下的这一块；`None` = 独立窗口。
    pub fn embed(&self) -> Option<Embed> {
        self.embed
    }

    /// 换一个尺寸（父面板宽度 / 高度变了）。
    pub fn set_embed(&mut self, embed: Embed, cx: &mut Context<Self>) {
        if self.embed != Some(embed) {
            self.embed = Some(embed);
            cx.notify();
        }
    }

    /// 面板里真会画出来的那几台：DDC/CI 不通、开着 HDR 的都不进面板。
    ///
    /// 这两种是「拖了没反应」的屏，留在面板里只能让人怀疑是不是坏了 ——
    /// 显示器页的列表里会写清原因，面板只管能调的那几台。
    fn panel_displays(&self) -> Vec<DisplayInfo> {
        self.displays
            .borrow()
            .iter()
            .filter(|display| display.adjustable())
            .cloned()
            .collect()
    }

    /// 窗口高度：横排是标题 + 每台一行；竖排是标题 + 一列。
    pub fn desired_height(&self) -> f32 {
        panel_size(&self.cfg(), self.panel_displays().len()).1
    }

    /// 点数值格：进入手动输入。
    fn begin_edit(&mut self, key: &str, value: u32, window: &mut Window, cx: &mut Context<Self>) {
        self.editing = Some((key.to_string(), value.to_string()));
        self.edit_replace = true;
        self.focus.focus(window, cx);
        cx.notify();
    }

    /// 确认手动输入：空输入按「没改」处理，超过 100 按 100。
    fn commit_edit(&mut self, cx: &mut Context<Self>) {
        self.edit_replace = false;
        let Some((key, buffer)) = self.editing.take() else {
            return;
        };
        if let Ok(value) = buffer.trim().parse::<u32>() {
            self.set_brightness(&key, value.min(100), cx);
        }
        cx.notify();
    }

    fn cancel_edit(&mut self, cx: &mut Context<Self>) {
        self.edit_replace = false;
        if self.editing.take().is_some() {
            cx.notify();
        }
    }

    fn set_brightness(&self, key: &str, value: u32, cx: &mut App) {
        // 调不动的（开着 HDR、DDC/CI 不通）一律不发：工作线程那边虽然也会拦，
        // 但这里先挡住就不会记一笔假的预测值，条也不会自己跳一下。
        if !self
            .displays
            .borrow()
            .iter()
            .any(|display| display.key == key && display.adjustable())
        {
            return;
        }
        self.engine.send(DisplayCommand::SetBrightness {
            key: key.to_string(),
            value: value.min(100),
        });
        // 和快捷键记同一笔：回读要六十毫秒才回来，这期间别让它把目标盖回上一档。
        crate::gui::note_brightness(cx, key, value.min(100));
        // 立刻把本地预览摆到新位置：等 DDC 回读要几十毫秒，松手后滑条会先跳回去再回来。
        self.drag_values
            .borrow_mut()
            .insert(key.to_string(), value.min(100) as f32 / 100.0);
    }

    /// 键盘：只在手动输入时吃键。
    fn handle_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.editing.is_none() {
            return false;
        }
        let key = event.keystroke.key.as_str();
        match key {
            "enter" => {
                self.commit_edit(cx);
                return true;
            }
            "escape" => {
                self.cancel_edit(cx);
                return true;
            }
            _ => {}
        }
        let replace_first = self.edit_replace;
        let Some((_, buffer)) = self.editing.as_mut() else {
            return false;
        };
        let handled = if key == "backspace" {
            self.edit_replace = false;
            buffer.pop();
            true
        } else {
            // 数字从 `key_char` 取：主键盘上 `keystroke.key` 未必就是 "7"，
            // `key_char` 才是系统按当前布局翻出来的那个字符。
            match event.keystroke.key_char.as_deref() {
                Some(text) if text.len() == 1 && text.parse::<u8>().is_ok() => {
                    // 第一下数字直接顶掉原值 —— gpui 里没法「全选」，不这样的话
                    // 点开时格子里已经是 `100`，再敲什么都塞不进去。
                    if replace_first {
                        buffer.clear();
                        self.edit_replace = false;
                    }
                    if buffer.len() < 3 {
                        buffer.push_str(text);
                    }
                    true
                }
                _ => false,
            }
        };
        if handled {
            let _ = window;
            cx.notify();
        }
        handled
    }

    /// 保存当前亮度为预设（同名同值不会重复记）。
    fn save_preset(&mut self, key: &str, window: &mut Window, cx: &mut Context<Self>) {
        let Some(value) = self.current_percent(key) else {
            return;
        };
        let _ = self.config.borrow_mut().update(|config| {
            config.brightness.add_preset(value);
        });
        // 预设行多了一格，窗口要跟着长高。
        self.update_frame(window, cx);
        cx.notify();
    }

    /// 这台显示器此刻显示的值（拖动中优先用本地预览）。
    fn current_percent(&self, key: &str) -> Option<u32> {
        if let Some(value) = self.drag_values.borrow().get(key) {
            return Some((value * 100.0).round() as u32);
        }
        self.displays
            .borrow()
            .iter()
            .find(|display| display.key == key)
            .and_then(|display| display.brightness)
    }

    /// 光标所在的那台显示器 —— 预设左键点击就套给它。
    ///
    /// 口径和亮度快捷键的「鼠标所在的那块」一致（`display::monitor_index_at_cursor`），
    /// 面板摆在谁身上，左键点预设改的就是谁。
    fn cursor_display_key(&self) -> Option<String> {
        let displays = self.displays.borrow();
        if displays.is_empty() {
            return None;
        }
        let index = crate::display::monitor_index_at_cursor().min(displays.len() - 1);
        let display = displays.get(index)?;
        display.adjustable().then(|| display.key.clone())
    }

    /// 所有调得动的显示器（DDC/CI 不通、开着 HDR 的都跳过）。
    fn lit_display_keys(&self) -> Vec<String> {
        self.displays
            .borrow()
            .iter()
            .filter(|display| display.adjustable())
            .map(|display| display.key.clone())
            .collect()
    }

    /// 把一组显示器都套成同一个亮度。
    fn apply_preset(&mut self, keys: Vec<String>, value: u32, cx: &mut Context<Self>) {
        if keys.is_empty() {
            return;
        }
        // 手动输入框留着会让读数看起来没变，先收掉。
        self.editing = None;
        for key in &keys {
            self.set_brightness(key, value, cx);
        }
        // 落定那一下：被套用的块闪一下，眼睛能确认「套到的是这台」。
        self.flash_blocks(keys, cx);
        cx.notify();
    }

    /// 拖到哪台显示器上了（按各块自己量出来的矩形认，不是按行号猜）。
    ///
    /// 块与块之间隔着 17px 的分隔缝，光标落在缝里也算最近的那一块 ——
    /// 落点判定和拖动时的高亮用的是同一个函数，看到亮哪块就是套给哪块。
    fn drag_over_at(&self, position: gpui::Point<Pixels>) -> Option<String> {
        let rects = self.block_rects.borrow();
        // 正中某一块的距离就是 0；落在块与块之间的缝里，按最近的吸附（半个缝以内）。
        rects
            .iter()
            .map(|(key, rect)| {
                let dx = (rect.origin.x - position.x)
                    .max(position.x - (rect.origin.x + rect.size.width))
                    .max(Pixels::ZERO);
                let dy = (rect.origin.y - position.y)
                    .max(position.y - (rect.origin.y + rect.size.height))
                    .max(Pixels::ZERO);
                (key, dx.as_f32().max(dy.as_f32()))
            })
            .filter(|(_, distance)| *distance <= DRAG_SNAP)
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(key, _)| key.clone())
    }

    /// 拖动中：记下光标在哪（画那颗跟着走的预设）、落点换了就把那一块提亮。
    fn update_drag(&mut self, position: gpui::Point<Pixels>, cx: &mut Context<Self>) {
        let Some(drag) = self.drag_preset.as_ref() else {
            return;
        };
        let origin = drag.origin;
        let over = self.drag_over_at(position);
        let moved = (position.x - origin.x).as_f32().abs() > DRAG_THRESHOLD
            || (position.y - origin.y).as_f32().abs() > DRAG_THRESHOLD;
        let position_changed = self.drag_at != Some(position);
        self.drag_at = Some(position);
        let changed = match self.drag_preset.as_mut() {
            Some(drag) => {
                let over_changed = drag.over != over;
                drag.over = over;
                let moved_now = moved && !drag.moved;
                drag.moved |= moved;
                over_changed || moved_now
            }
            None => false,
        };
        if position_changed || changed {
            cx.notify();
        }
    }

    /// 松手：落在某台显示器上就套给那台；压根没拖动（就是点了一下）才套给光标所在的那台；
    /// 拖出去了却落在空处 = 取消，什么也不做。
    fn finish_preset_drag(
        &mut self,
        position: gpui::Point<Pixels>,
        inside: bool,
        cx: &mut Context<Self>,
    ) {
        let Some(drag) = self.drag_preset.take() else {
            return;
        };
        self.drag_at = None;
        let target = match self.drag_over_at(position) {
            Some(key) => Some(key),
            None if !drag.moved && inside => self.cursor_display_key(),
            None => None,
        };
        if let Some(key) = target {
            self.apply_preset(vec![key], drag.value, cx);
        }
        cx.notify();
    }

    /// 让这几块闪一下（套用成功的动画反馈）。
    fn flash_blocks(&mut self, keys: Vec<String>, cx: &mut Context<Self>) {
        self.flash = Some(Flash { keys });
        self.flash_seq = self.flash_seq.wrapping_add(1);
        let seq = self.flash_seq;
        let weak = cx.entity().downgrade();
        cx.spawn(async move |_, cx| {
            cx.background_executor()
                .timer(Duration::from_millis(FLASH_MS + 60))
                .await;
            let _ = weak.update(cx, |this, cx| {
                // 又闪了一次就交给新的那一段收尾。
                if this.flash_seq == seq {
                    this.flash = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// 删除一颗预设（确认之后才走到这儿）。少了一格，窗口 / 父面板要跟着重量。
    fn delete_preset(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        self.confirm_delete = None;
        let _ = self.config.borrow_mut().update(|config| {
            if index < config.brightness.presets.len() {
                config.brightness.presets.remove(index);
            }
        });
        self.update_frame(window, cx);
        cx.notify();
    }

    /// 悬停在某台显示器上滚轮：按段数一格格调它的亮度。
    ///
    /// 逻辑和音量面板的「悬停滚轮」完全一致：只看方向（一格算一档）、
    /// 边界截断、同一帧重复派发只认第一遍。区别只有步进从哪来 ——
    /// 音量是配置里的百分比 / dB，亮度是「段数」换算出来的一段。
    fn on_block_wheel(
        &mut self,
        key: &str,
        event: &gpui::ScrollWheelEvent,
        cx: &mut Context<Self>,
    ) {
        let cfg = self.cfg();
        if !cfg.brightness.scroll_wheel {
            return;
        }
        let notches = wheel_direction(event.delta);
        if notches == 0.0 {
            return;
        }
        // 同一次滚轮可能连着重入好几遍，只认第一遍（和面板的 3ms 节流一致）。
        let now = Instant::now();
        if let Some(last) = self.wheel_last_at {
            if now.duration_since(last) < Duration::from_millis(3) {
                return;
            }
        }
        self.wheel_last_at = Some(now);

        let Some(current) = self.current_percent(key) else {
            return;
        };
        let next = cfg.brightness_scroll(current as f32, notches);
        if next != current {
            // 手动输入框留着会让读数看起来没变，先收掉。
            self.editing = None;
            self.set_brightness(key, next, cx);
            cx.notify();
        }
    }

    /// 这一轮走哪套动画（跟音量面板同一份配置）。
    fn entrance_motion(&self) -> EntranceMotion {
        match self.cfg().popup_anim_style {
            crate::config::PopupAnimStyle::System => EntranceMotion::System,
            crate::config::PopupAnimStyle::Fade => EntranceMotion::Fade,
        }
    }

    /// 系统风格：整扇窗从屏幕底滑到目标位置要走多远。
    ///
    /// 和音量面板取同一个起点（屏幕底），两扇窗一起从屏幕外升上来才像一套动画；
    /// 各自的终点不同，所以距离也不同。
    fn slide_distance(&self, cx: &mut Context<Self>) -> f32 {
        let (_, y, width, height) = self.base_frame;
        if width <= 0.0 || height <= 0.0 {
            return 0.0;
        }
        let area = cx
            .primary_display()
            .map(|display| display.bounds())
            .unwrap_or_else(|| {
                gpui::Bounds::new(
                    gpui::point(px(0.), px(0.)),
                    gpui::size(px(1920.), px(1080.)),
                )
            });
        let bottom = (area.origin.y + area.size.height).as_f32();
        (bottom - y).max(0.0)
    }

    /// 把「目标位置 + 当前位移」写进窗口。
    fn apply_slide(&self, window: &Window, offset: f32) {
        let (x, y, width, height) = self.base_frame;
        if width <= 0.0 || height <= 0.0 {
            return;
        }
        crate::windows::platform::set_origin(
            window,
            x,
            y + offset,
            window.scale_factor(),
        );
    }

    /// 起一段「从屏幕底滑上来」的动画（独立线程 + 合成帧节流，和音量面板同一套）。
    /// 造一段滑动：`from` → `end`，曲线和时长跟着这一档动画走
    /// （和音量面板同一份规划，方向固定「贴着屏幕底上下」）。
    fn slide_plan(
        &self,
        mode: crate::windows::platform::SlideMode,
        motion: EntranceMotion,
        config_ms: u32,
        from: f32,
        end: f32,
    ) -> crate::windows::platform::SlideRun {
        use crate::windows::platform::{SlideMode, SlideRun};
        let total = anim::entrance_duration(config_ms).as_secs_f32() * 1000.0;
        let base = SlideRun {
            mode,
            to: (self.base_frame.0, self.base_frame.1),
            direction: (0.0, 1.0),
            from,
            end,
            tau_ms: 0.0,
            fade_delay_ms: 0.0,
            fade_ms: 0.0,
            total_ms: total,
            opacity: crate::ui::theme::ui_opacity(),
            // 入场从全透明淡进来，出场从实心淡出去（中途接力由调用方改这一项）。
            from_alpha: if mode == SlideMode::Exit { 1.0 } else { 0.0 },
        };
        match motion {
            EntranceMotion::System => {
                let m = anim::SystemMotion::panel_scaled(end.max(from), total);
                let entering = mode == SlideMode::Enter;
                let slides = m.slide > 0.5;
                SlideRun {
                    from: if slides { from } else { 0.0 },
                    end: if slides { end } else { 0.0 },
                    tau_ms: m.tau_ms,
                    fade_delay_ms: m.fade_delay_ms,
                    // 出场也带上这一档的淡出：面板这一档的 `fade_ms` 是 0（纯靠滑出去），
                    // 但「纯淡出档」如果把 fade 抹成 0，整段收起就什么也不做 ——
                    // 窗口留在屏幕里、`visible` 却已是 false，之后怎么点都收不起来。
                    fade_ms: m.fade_ms,
                    total_ms: if entering {
                        m.enter_total_ms()
                    } else {
                        m.exit_ms
                    },
                    ..base
                }
            }
            EntranceMotion::Fade => SlideRun {
                from: 0.0,
                end: 0.0,
                fade_delay_ms: 0.0,
                fade_ms: total,
                total_ms: total,
                ..base
            },
        }
    }

    /// 起一段滑动，跑完自己收尾（被打断后旧回调不许动新一轮的状态）。
    fn start_slide(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
        run: crate::windows::platform::SlideRun,
    ) {
        use crate::windows::platform::SlideMode;
        let mode = run.mode;
        let end = run.end;
        let handle = crate::windows::platform::run_slide(window, run);
        self.slide = Some(handle.clone());
        let view = cx.entity().downgrade();
        cx.spawn(async move |_, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(20))
                    .await;
                if handle.is_done() {
                    break;
                }
            }
            let _ = view.update(cx, |this, cx| {
                if this.slide.as_ref().map(|now| now.same_run(&handle)) != Some(true) {
                    return;
                }
                this.slide = None;
                this.anim.finish();
                this.rest_offset = if mode == SlideMode::Exit { end } else { 0.0 };
                cx.notify();
            });
        })
        .detach();
    }

    pub fn show(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.visible = true;
        // 上次拖动的预览值到这次就不作数了，全部以真实回读为准。
        self.drag_values.borrow_mut().clear();
        self.editing = None;
        self.edit_replace = false;
        // 每次打开都重读一遍：显示器自己的按钮、游戏全屏切换都可能改过亮度。
        self.engine.send(DisplayCommand::Refresh);
        // 动画开关与时长跟音量面板取自同一份配置、同一套曲线，两扇窗一起走。
        let animate = self.cfg().popup_animation && !cx.reduce_motion();
        let config_ms = self.cfg().popup_animation_ms;
        let motion = self.entrance_motion();
        // 窗口几何变更同样放到本帧绘制之后（和面板一致）。
        cx.defer_in(window, move |this, window, cx| {
            this.enter(window, cx, animate, config_ms, motion);
            cx.notify();
        });
        // `defer_in` 要等「下一帧绘制完」才执行；空闲时 gpui 不给帧，
        // 这一段就会一直挂着 —— 实测按下到开始滑动要多等 280ms。
        // 主动敲一下重绘，defer 立刻就能跑。
        crate::windows::platform::post_repaint(window);
    }

    /// 把窗口摆出来（能从半路接力）。
    fn enter(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        animate: bool,
        config_ms: u32,
        motion: EntranceMotion,
    ) {
        use crate::windows::platform::SlideMode;
        let resume = self.slide.take().map(|handle| {
            let offset = handle.offset();
            handle.cancel();
            offset
        });
        let from = match resume {
            Some(offset) => offset,
            None => {
                let parked = animate && motion != EntranceMotion::Fade;
                self.update_frame_parked(window, cx, parked);
                if parked {
                    self.slide_distance(cx)
                } else {
                    0.0
                }
            }
        };
        self.anim.begin_with(animate, motion, from);
        // 滑入那两档不碰整窗 alpha（一碰 DWM 就补一段出现淡入，卡片会半透明）。
        if animate {
            if motion == EntranceMotion::Fade {
                crate::windows::platform::set_opacity(window, 0.0);
            }
        } else {
            crate::windows::platform::set_opacity(window, crate::ui::theme::ui_opacity());
        }
        crate::windows::platform::set_visible(window, true);
        // 只提到最上层、不抢焦点：抢了焦点音量面板会立刻因为失焦收起。
        crate::windows::platform::present_behind_taskbar(window);
        if animate {
            let run = self.slide_plan(SlideMode::Enter, motion, config_ms, from, 0.0);
            self.start_slide(window, cx, run);
        } else {
            self.rest_offset = 0.0;
        }
    }

    pub fn hide(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.visible {
            return;
        }
        self.visible = false;
        self.editing = None;
        self.edit_replace = false;
        self.drag_values.borrow_mut().clear();
        // 收起和音量面板同一套：怎么出就怎么回（滑出 / 淡出），播完停在屏幕外。
        let animate = self.cfg().popup_animation && !cx.reduce_motion();
        let config_ms = self.cfg().popup_animation_ms;
        let close_anim = self.cfg().panel_close_animation;
        let motion = self.entrance_motion();
        cx.defer_in(window, move |this, window, cx| {
            this.leave(window, cx, animate && close_anim, config_ms, motion);
            cx.notify();
        });
        // 同上：别让收起也白等一帧。
        crate::windows::platform::post_repaint(window);
    }

    /// 让窗口退场：正在滑入时接到收起，就从当前位置接着往下滑。
    fn leave(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        animate: bool,
        config_ms: u32,
        motion: EntranceMotion,
    ) {
        use crate::windows::platform::SlideMode;
        let from = match self.slide.take() {
            Some(handle) => {
                let offset = handle.offset();
                handle.cancel();
                offset
            }
            None => self.rest_offset,
        };
        if !animate {
            self.park_window(window, cx);
            self.anim.finish();
            return;
        }
        let end = if motion == EntranceMotion::Fade {
            0.0
        } else {
            self.slide_distance(cx)
        };
        let run = self.slide_plan(SlideMode::Exit, motion, config_ms, from, end);
        self.start_slide(window, cx, run);
    }

    /// 把窗口停到屏幕外并压成全透明（不隐藏）。
    ///
    /// 距离现算，不读 `anim.slide()`：关掉弹出动画时它是 0，读它等于「挪 0px」，
    /// 窗口会留在屏幕上（音量面板那边同一个坑）。
    fn park_window(&mut self, window: &Window, cx: &mut Context<Self>) {
        if self.anim.motion() == EntranceMotion::Fade {
            crate::windows::platform::set_opacity(window, 0.0);
            self.rest_offset = 0.0;
        } else {
            let slide = self.slide_distance(cx);
            self.apply_slide(window, slide);
            self.rest_offset = slide;
        }
    }

    pub fn update_frame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.update_frame_parked(window, cx, false);
    }

    /// 同上，`parked` = 直接落到屏幕外（系统风格的起点），别在目标位置停一帧。
    pub fn update_frame_parked(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
        parked: bool,
    ) {
        // 融合：窗口是父面板那一扇，位置和高度都归它管 —— 这里只让父面板重新量一遍
        // （预设多了一格、高度要跟着长）。以前没这道闸，点「保存预设」会把整扇面板
        // 挪到独立亮度窗该待的位置、还按亮度那块自己的高度缩一截。
        if self.embed.is_some() {
            cx.defer(|cx| crate::gui::refresh_flyout_size(cx));
            return;
        }
        let cfg = self.cfg();
        let displays = self.panel_displays().len();
        let (width, height) = panel_size(&cfg, displays);
        self.last_height = height;
        let area = cx
            .primary_display()
            .map(|display| display.visible_bounds())
            .unwrap_or_else(|| {
                gpui::Bounds::new(
                    gpui::point(px(0.), px(0.)),
                    gpui::size(px(1920.), px(1080.)),
                )
            });
        const MARGIN: f32 = 12.0;
        const GAP: f32 = 8.0;
        let edge = crate::windows::platform::taskbar_edge();
        let left = area.origin.x.as_f32();
        let right = (area.origin.x + area.size.width).as_f32();
        let top = area.origin.y.as_f32();
        let bottom = (area.origin.y + area.size.height).as_f32();

        // 音量面板的矩形：它自己每次摆好都会记进 AppState（见 `flyout::update_frame`）。
        // 万一还没记过（第一帧），按它的贴边规则现算一个。
        let (fx, fy, fw, fh) = crate::gui::flyout_frame(cx).unwrap_or_else(|| match edge {
            crate::windows::platform::TaskbarEdge::Left => (
                left + MARGIN,
                bottom - MARGIN - height,
                cfg.flyout_width(),
                height,
            ),
            crate::windows::platform::TaskbarEdge::Top => (
                right - MARGIN - cfg.flyout_width(),
                top + MARGIN,
                cfg.flyout_width(),
                height,
            ),
            _ => (
                right - MARGIN - cfg.flyout_width(),
                bottom - MARGIN - height,
                cfg.flyout_width(),
                height,
            ),
        });

        // 先按用户选的那一侧摆：左右跟面板底边对齐，上下跟面板右边对齐。
        let placement = cfg.brightness.placement;
        let (mut x, mut y) = match placement {
            BrightnessPlacement::Left => (fx - GAP - width, fy + fh - height),
            BrightnessPlacement::Right => (fx + fw + GAP, fy + fh - height),
            BrightnessPlacement::Top => (fx + fw - width, fy - GAP - height),
            BrightnessPlacement::Bottom => (fx + fw - width, fy + fh + GAP),
        };

        // 那一侧放不下（出去屏幕了）就翻到对面去。
        let min_x = left + MARGIN;
        let max_x = (right - MARGIN - width).max(min_x);
        let min_y = top + MARGIN;
        let max_y = (bottom - MARGIN - height).max(min_y);
        match placement {
            BrightnessPlacement::Left if x < min_x => x = fx + fw + GAP,
            BrightnessPlacement::Right if x > max_x => x = fx - GAP - width,
            BrightnessPlacement::Top if y < min_y => y = fy + fh + GAP,
            BrightnessPlacement::Bottom if y > max_y => y = fy - GAP - height,
            _ => {}
        }
        let x = x.clamp(min_x, max_x);
        let y = y.clamp(min_y, max_y);

        self.base_frame = (x, y, width, height);
        let placed_y = if parked {
            let screen = cx
                .primary_display()
                .map(|display| display.bounds())
                .unwrap_or(area);
            screen.origin.y.as_f32() + screen.size.height.as_f32()
        } else {
            y
        };
        crate::windows::platform::set_frame(
            window,
            gpui::Bounds::new(
                gpui::point(px(x), px(placed_y)),
                gpui::size(px(width), px(height)),
            ),
            window.scale_factor(),
        );
        window.bounds_changed(cx);
    }
}

impl Render for Brightness {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let cfg = self.cfg();
        let lang = cfg.language;
        // 只画能调的那几台：HDR / DDC/CI 不通的不是「灰条」，是根本不出现。
        let displays = self.panel_displays();
        let error = self.error.borrow().clone();
        let embedded = self.embed.is_some();

        // 竖向：一台显示器一列，横着排开；横向：一台一行，块之间夹一条分隔线。
        let vertical = cfg.brightness.panel_layout.is_vertical();
        let mut rows: Vec<gpui::AnyElement> = Vec::new();
        if displays.is_empty() {
            rows.push(self.empty_hint(lang, error));
        } else if vertical {
            let mut columns: Vec<gpui::AnyElement> = Vec::new();
            for (index, display) in displays.iter().enumerate() {
                columns.push(self.monitor_column(display, index, lang, window, cx));
            }
            rows.push(
                div()
                    .flex()
                    .flex_row()
                    .items_start()
                    .gap(px(V_COLUMN_GAP))
                    .children(columns)
                    .into_any_element(),
            );
        } else {
            for (index, display) in displays.iter().enumerate() {
                if index > 0 {
                    rows.push(
                        div()
                            .h(px(1.))
                            .flex_none()
                            .mx(px(2.))
                            .bg(theme::border())
                            .into_any_element(),
                    );
                }
                rows.push(self.monitor_block(display, lang, cx));
            }
        }

        // 横排左右留 14：和音量面板的行同一条边，两块并排 / 上下看着才是一条线。
        let pad = if vertical { PAD } else { H_PAD };
        let mut content = div()
            .relative()
            .flex()
            .flex_col()
            .overflow_hidden()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.handle_key(event, window, cx);
            }))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _: &gpui::MouseDownEvent, _, cx| {
                    // 点别处 = 确认这次手动输入、收起右键确认条。
                    this.commit_edit(cx);
                    if this.confirm_delete.take().is_some() {
                        cx.notify();
                    }
                }),
            );
        if embedded {
            // 长在音量面板里：不铺自己的底色、不切圆角（那是父面板的事），
            // 横排按内容撑高，竖排撑满父给的高度（推子长度就是照它算的）。
            content = content.w_full();
            if vertical {
                content = content.h_full();
            }
        } else {
            // 根底色用不透明版：主题的 `surface` 留了 3% 透光，不透明度给到 100%
            // 时还会隐隐透出桌面。要透多少由 `ui_opacity` 一个人说了算。
            content = content
                .size_full()
                .bg(theme::surface_window())
                .rounded(px(theme::RADIUS_WINDOW));
        }
        // 融合进音量面板时没有标题那一块：那块地方紧挨着音频，
        // 多一行「显示器亮度」只是白占高度。
        if !embedded {
            content = content
                .child(self.header(lang, pad))
                .child(div().h(px(1.)).flex_none().bg(theme::border()));
        }
        let preset_bar = self.preset_bar(lang, cx);
        let mut body = div()
            .flex()
            .flex_col()
            .gap(px(BLOCK_GAP))
            .px(px(pad))
            .py(px(pad))
            .children(rows);
        if let Some(bar) = preset_bar {
            body = body.child(bar);
        }
        let mut content = content.child(body).child(self.drag_sensors(cx));
        if let Some(ghost) = self.drag_ghost() {
            content = content.child(ghost);
        }

        if embedded {
            // 融合：整块由父面板布局，不自带那一层「贴底对齐」的外壳。
            return div()
                .w_full()
                .when(vertical, |style| style.h_full())
                .child(content);
        }

        // 尺寸永远是完整那一份：三档动画都是整扇窗在滑（或原地淡），内容不裁不缩。
        let full_height = self.last_height.max(1.0);

        div().size_full().relative().child(
            div()
                .absolute()
                .left_0()
                .right_0()
                .bottom_0()
                .h(px(full_height))
                .child(content),
        )
    }
}

impl Brightness {
    fn header(&self, lang: Lang, pad: f32) -> gpui::AnyElement {
        div()
            .h(px(HEADER_HEIGHT))
            .flex_none()
            .px(px(pad))
            .flex()
            .items_center()
            .gap(px(6.))
            .child(icon("sun", 13.0, theme::accent()))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .text_size(px(11.5))
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(theme::text_secondary())
                    .truncate()
                    .child(lang.pick("显示器亮度", "Display brightness")),
            )
            .into_any_element()
    }

    fn empty_hint(&self, lang: Lang, error: Option<String>) -> gpui::AnyElement {
        div()
            .py(px(8.))
            .px(px(4.))
            .flex()
            .flex_col()
            .gap(px(4.))
            .child(
                div()
                    .text_size(px(11.))
                    .text_color(theme::text_secondary())
                    .child(lang.pick(
                        "没有检测到支持 DDC/CI 的显示器",
                        "No DDC/CI capable display found",
                    )),
            )
            .child(
                div()
                    .text_size(px(10.))
                    .text_color(theme::text_tertiary())
                    .child(match error {
                    Some(error) => error,
                    None => lang
                        .pick(
                            "在显示器菜单里打开 DDC/CI，并使用 HDMI / DP 直连。",
                            "Enable DDC/CI on the monitor and connect it directly over HDMI / DP.",
                        )
                        .to_string(),
                }),
            )
            .into_any_element()
    }

    /// 一台显示器：名称 + 读数 + 亮度条 + 预设。
    /// 读数格子：平时显示 `80%`，点一下变成输入框。
    ///
    /// 横排给 46px（「不支持」也放得下），竖排的列窄，只给 30。
    fn value_slot(
        &self,
        key: &str,
        slot: Slot,
        width: f32,
        align_end: bool,
        lang: Lang,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let editable = slot.editable();
        let value = slot.value();
        let editing = matches!(&self.editing, Some((edit_key, _)) if edit_key == key);
        let edit_text = match &self.editing {
            Some((edit_key, buffer)) if edit_key == key => buffer.clone(),
            _ => String::new(),
        };
        let key_for_click = key.to_string();
        div()
            .id(SharedString::from(format!("bright-value-{key}")))
            .flex_none()
            .w(px(width))
            .h(px(18.))
            // 横排的读数要像音量面板那样靠右钉住（右边那一列是同一段宽度），
            // 竖排是夹在两个步进钮中间，居中。
            .px(px(if align_end { 2. } else { 4. }))
            .flex()
            .items_center()
            .when(align_end, |style| style.justify_end())
            .when(!align_end, |style| style.justify_center())
            .rounded(px(theme::RADIUS_SMALL))
            .text_size(px(11.5))
            .when(editing, |style| style.bg(theme::accent()))
            .when(!editing, |style| {
                style
                    .cursor_pointer()
                    .hover(|style| style.bg(theme::surface_hover()))
            })
            .when(!editable, |style| style.text_color(theme::text_tertiary()))
            .when(editable, |style| style.text_color(theme::text_primary()))
            .when(!editing, |style| {
                style.child(match slot {
                    Slot::Ready(value) => format!("{value}%"),
                    // HDR 单占一个词：这台的 DDC 读数是有的，但调了不生效。
                    Slot::Hdr => lang.pick("HDR", "HDR").to_string(),
                    Slot::Unsupported => lang.pick("不支持", "N/A").to_string(),
                })
            })
            .when(editing, |style| {
                style.gap(px(1.)).child(edit_text.clone()).child(
                    div()
                        .w(px(1.5))
                        .h(px(12.))
                        .flex_none()
                        .bg(theme::text_primary()),
                )
            })
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                cx.stop_propagation();
                if !editable {
                    return;
                }
                this.begin_edit(&key_for_click, value, window, cx);
            }))
            .into_any_element()
    }

    /// −/+：步进取配置里的「亮度步进」（参考实现就是一次 2 个百分点）。
    fn step_button(
        &self,
        key: &str,
        editable: bool,
        glyph: &'static str,
        direction: i32,
        width: f32,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let step = self.cfg().brightness.step.max(1);
        let key = key.to_string();
        div()
            .id(SharedString::from(format!("bright-step-{glyph}-{key}")))
            .w(px(width))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(theme::RADIUS_SMALL))
            .text_size(px(12.))
            .text_color(theme::text_secondary())
            .when(editable, |style| {
                style
                    .cursor_pointer()
                    .hover(|style| style.bg(theme::surface_hover()))
            })
            .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                cx.stop_propagation();
                if !editable {
                    return;
                }
                let Some(current) = this.current_percent(&key) else {
                    return;
                };
                let next = (current as i32 + direction * step as i32).clamp(0, 100) as u32;
                if next != current {
                    this.editing = None;
                    this.set_brightness(&key, next, cx);
                    cx.notify();
                }
            }))
            .child(glyph)
            .into_any_element()
    }

    /// 亮度条：跟音量条同一个控件，只是没有电平特效。
    fn brightness_slider(
        &self,
        key: &str,
        value: f32,
        editable: bool,
        vertical: bool,
    ) -> VolumeSlider {
        let drag_values = self.drag_values.clone();
        let engine = self.engine.clone();
        let key_for_slider = key.to_string();
        let key_for_release = key.to_string();
        let drag_values_release = self.drag_values.clone();
        VolumeSlider::new(
            SharedString::from(format!("bright-slider-{key}")),
            value,
            self.drag.clone(),
        )
        .fill(theme::accent())
        // 粗细 / 圆角跟音量面板同一套：两块面板并排放着，条不能一个粗一个细。
        .thickness(self.cfg().panel_bar_thickness())
        .corner(self.cfg().panel_bar_corner())
        .enabled(editable)
        .vertical(vertical)
        .height(SLIDER_HEIGHT)
        .on_change(move |value, window, cx| {
            drag_values
                .borrow_mut()
                .insert(key_for_slider.clone(), value);
            window.refresh();
            let percent = (value * 100.0).round().clamp(0.0, 100.0) as u32;
            engine.send(DisplayCommand::SetBrightness {
                key: key_for_slider.clone(),
                value: percent,
            });
            crate::gui::note_brightness(cx, &key_for_slider, percent);
        })
        .on_release(move |window, _| {
            // 松手后保留本地值，等下一次回读（面板重新打开时会读）再对齐。
            let _ = &drag_values_release;
            let _ = &key_for_release;
            window.refresh();
        })
    }

    fn monitor_block(
        &self,
        display: &DisplayInfo,
        lang: Lang,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let key = display.key.clone();
        let config = self.cfg();
        let name = config.display_label(&key, &display.name);
        // 开了 HDR / DDC/CI 不通都不可调：读数位置换成原因，条和步进钮一起变灰。
        let slot = Slot::of(display, self.current_percent(&key));
        let editable = slot.editable();
        let slider_value = self
            .drag_values
            .borrow()
            .get(&key)
            .copied()
            .unwrap_or(display.brightness.unwrap_or(0) as f32 / 100.0);

        // 名称行：名字 + 步进 + 保存预设。读数挪到下面跟亮度条同一排去了，
        // 这一行空出来的右端正好摆「保存预设」—— 从前它独占一行，白占 21px。
        let m = metrics(&config);
        let dragging = self.drag_preset.as_ref().and_then(|drag| drag.over.as_deref())
            == Some(key.as_str());
        let label = div()
            .flex()
            .items_center()
            .gap(px(4.))
            .h(px(m.name_row))
            .child(icon("monitor", 12.0, theme::text_tertiary()))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .truncate()
                    .text_size(px(11.))
                    .text_color(theme::text_secondary())
                    .child(name.clone()),
            )
            .child(self.step_button(&key, editable, "−", -1, STEP_WIDTH, cx))
            .child(self.step_button(&key, editable, "+", 1, STEP_WIDTH, cx))
            .child(self.save_button(&key, editable, false, lang, cx));

        let slider = self.brightness_slider(&key, slider_value, editable, false);

        // 亮度条 + 读数：左边缩进、右边读数栏、中间那条缝，全部照音量面板的设备行来，
        // 于是两张条一样长、左右也对齐（上下摆着看是一条线）。
        let slider_row = div()
            .flex()
            .items_center()
            .h(px(m.slider_row))
            .pl(px(H_BAR_INSET))
            .gap(px(H_READOUT_GAP))
            .child(div().flex_1().min_w_0().child(slider))
            .child(self.value_slot(&key, slot, H_READOUT_WIDTH, true, lang, cx));

        // 这一块的矩形：拖预设过来时按它认落点（`drag_sensors` 里读）。
        let rects = self.block_rects.clone();
        let key_for_rect = key.clone();
        let key_for_wheel = key.clone();
        div()
            .id(SharedString::from(format!("bright-block-{key}")))
            .relative()
            .flex()
            .flex_col()
            .gap(px(ROW_GAP))
            // 拖着预设悬在这台上：整块提亮一层，落点一眼看得出。
            .when(dragging, |style| {
                style
                    .rounded(px(theme::RADIUS_SMALL))
                    .bg(theme::accent_soft())
            })
            // 滚轮挂在整块上，不是只挂在滑条上：光标落在名称行也算「悬停在这台显示器」，
            // 和面板里「整行都能滚」是同一套手感。
            .on_scroll_wheel(cx.listener(
                move |this, event: &gpui::ScrollWheelEvent, _window, cx| {
                    this.on_block_wheel(&key_for_wheel, event, cx);
                },
            ))
            .child(label)
            .child(slider_row)
            .children(self.block_decor(&key, m.name_row + ROW_GAP + m.slider_row))
            .child(
                gpui::canvas(
                    move |bounds, _, _| {
                        rects.borrow_mut().insert(key_for_rect.clone(), bounds);
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .left_0()
                .top_0()
                .size_full(),
            )
            .into_any_element()
    }

    /// 竖向排版的一列：名称 / 立起来的亮度条 / 读数+步进 / 预设。
    ///
    /// 列宽只有 64，所以名称按 `name_mode` 收着写：编号就是一个数字，
    /// 全名则去掉括号里的型号、放不下就横向滚。两者都不显示括号内容。
    fn monitor_column(
        &self,
        display: &DisplayInfo,
        index: usize,
        lang: Lang,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let cfg = self.cfg();
        let key = display.key.clone();
        let slot = Slot::of(display, self.current_percent(&key));
        let editable = slot.editable();
        let slider_value = self
            .drag_values
            .borrow()
            .get(&key)
            .copied()
            .unwrap_or(display.brightness.unwrap_or(0) as f32 / 100.0);

        let label = self.column_label(display, index, cfg.brightness.name_mode, window);

        // 一列能用的高度：总高扣掉标题（独立窗才有）和底部的预设排，再扣掉固定的几段。
        let m = metrics(&cfg);
        let displays = self.panel_displays().len();
        let fader = (columns_height(&cfg, displays, self.embed.is_some())
            - column_fixed_height(&cfg))
        .max(V_FADER_MIN);

        // 推子：整列里最高的一块，宽度撑满列。长度由面板高度算，
        // 面板配高了推子就长（调音台那种手感）。
        let slider = self.brightness_slider(&key, slider_value, editable, true);
        let fader = div()
            .w_full()
            .h(px(fader))
            .flex_none()
            .px(px(V_COLUMN_PAD))
            .child(slider);

        // 读数那排：− 80% +，正好铺满 64 的列宽。
        let step_row = div()
            .flex()
            .items_center()
            .justify_center()
            .gap(px(1.))
            .h(px(m.step_row))
            .flex_none()
            .child(self.step_button(&key, editable, "−", -1, 16.0, cx))
            .child(self.value_slot(&key, slot, 30.0, false, lang, cx))
            .child(self.step_button(&key, editable, "+", 1, 16.0, cx));

        // 列底只留「保存预设」：预设本体挪到整块底部那一排共用了。
        let save = div()
            .w_full()
            .px(px(V_COLUMN_PAD))
            .flex()
            .justify_center()
            .child(self.save_button(&key, editable, true, lang, cx));

        let dragging = self.drag_preset.as_ref().and_then(|drag| drag.over.as_deref())
            == Some(key.as_str());
        let rects = self.block_rects.clone();
        let key_for_rect = key.clone();
        let key_for_wheel = key.clone();
        div()
            .id(SharedString::from(format!("bright-col-{key}")))
            .relative()
            .w(px(V_COLUMN_WIDTH))
            .h(px(columns_height(&cfg, displays, self.embed.is_some())))
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(ROW_GAP))
            // 拖着预设悬在这台上：整列提亮一层。
            .when(dragging, |style| {
                style
                    .rounded(px(theme::RADIUS_SMALL))
                    .bg(theme::accent_soft())
            })
            .on_scroll_wheel(cx.listener(
                move |this, event: &gpui::ScrollWheelEvent, _window, cx| {
                    this.on_block_wheel(&key_for_wheel, event, cx);
                },
            ))
            // 列里每一行都先按列宽收窄再摆：名称行和保存格子必须和推子共用同一条中线，
            // 不然编号和保存格子会各自贴着左边，看着和推子差几像素（列宽 64 很窄，看得出）。
            .child(div().w_full().px(px(V_COLUMN_PAD)).child(label))
            .child(fader)
            .child(step_row)
            .child(save)
            .children(
                self.block_decor(&key, columns_height(&cfg, displays, self.embed.is_some())),
            )
            .child(
                gpui::canvas(
                    move |bounds, _, _| {
                        rects.borrow_mut().insert(key_for_rect.clone(), bounds);
                    },
                    |_, _, _, _| {},
                )
                .absolute()
                .left_0()
                .top_0()
                .size_full(),
            )
            .into_any_element()
    }

    /// 竖向列头：编号 / 全名（去括号、放不下就滚）。
    fn column_label(
        &self,
        display: &DisplayInfo,
        index: usize,
        mode: DisplayNameMode,
        window: &mut Window,
    ) -> gpui::AnyElement {
        // 自定义名是自己起的，原样显示；没起才按「去掉括号里的型号」处理。
        let alias = self.cfg().display_alias(&display.key).map(str::to_string);
        let text = match mode {
            DisplayNameMode::Number => (index + 1).to_string(),
            DisplayNameMode::FullName => alias.unwrap_or_else(|| strip_model(&display.name)),
        };
        let inner = V_COLUMN_WIDTH - V_COLUMN_PAD * 2.0;
        // 列头必须自己钉死宽度再裁：不给宽度的话它按内容撑开，
        // 全名会直接压到隔壁列上（gpui 的父盒子不会自动裁子元素）。
        // 宽度用 `w_full`（外层的 px 已经把列内边距扣掉了），这样和推子同一条中线。
        let container = div()
            .w_full()
            .h(px(V_LABEL_ROW))
            .flex_none()
            .flex()
            .items_center()
            .overflow_hidden();

        match mode {
            // 编号最长两位，直接居中写。
            DisplayNameMode::Number => container.justify_center().child(
                div()
                    .w_full()
                    .text_size(px(11.))
                    .text_color(theme::text_secondary())
                    .text_center()
                    .child(text),
            ),
            DisplayNameMode::FullName => {
                let width = measure_text(&text, 11.0, window);
                let travel = (width - inner).max(0.0);
                let label = div()
                    .whitespace_nowrap()
                    .text_size(px(11.))
                    .text_color(theme::text_secondary())
                    .child(text.clone());
                let body: gpui::AnyElement = if travel > 0.5 {
                    // 一直滚：列太窄，没有「悬停才展开」的空间。
                    // 前后各留一点停顿，中间匀速滑过去。
                    label
                        .with_animation(
                            ElementId::Name(format!("bright-name-scroll-{}", display.key).into()),
                            Animation::new(Duration::from_millis(5200)).repeat(),
                            move |element, delta| {
                                let progress = ((delta - 0.15) / 0.7).clamp(0.0, 1.0);
                                element.ml(px(-(travel + 6.0) * progress))
                            },
                        )
                        .into_any_element()
                } else {
                    label.truncate().into_any_element()
                };
                container.justify_start().child(body)
            }
        }
        .into_any_element()
    }

    /// 整块底部共用的那一排预设。
    ///
    /// 从前每台显示器的拉条下面各挂一排（同一批预设重复好几遍），现在只留这一排：
    /// 左键套给光标所在的那台、中键套给所有、拖到某台显示器的块上就套给那台、
    /// 右键删（先出确认条）。
    fn preset_bar(&self, lang: Lang, cx: &mut Context<Self>) -> Option<gpui::AnyElement> {
        let cfg = self.cfg();
        let presets = cfg.brightness.presets.clone();
        if presets.is_empty() && self.confirm_delete.is_none() {
            return None;
        }
        let m = metrics(&cfg);
        if let Some(index) = self.confirm_delete {
            return Some(self.confirm_row(index, &presets, m.chip, lang, cx));
        }

        // 当前值取「光标所在那台」的读数：和左键点击的目标同一个口径，
        // 于是亮着的那一颗就是「点一下会发生什么」。
        let current = self
            .cursor_display_key()
            .and_then(|key| self.current_percent(&key));
        let dragging = self
            .drag_preset
            .as_ref()
            .filter(|drag| drag.moved)
            .map(|drag| drag.index);
        let mut chips: Vec<gpui::AnyElement> = Vec::new();
        for (index, preset) in presets.iter().enumerate() {
            chips.push(self.preset_chip(
                index,
                &preset.name,
                preset.brightness,
                current == Some(preset.brightness),
                dragging == Some(index),
                m.chip,
                cx,
            ));
        }

        Some(
            div()
                .flex()
                .flex_wrap()
                .items_center()
                .gap(px(CHIP_GAP))
                .children(chips)
                .into_any_element(),
        )
    }

    /// 一颗预设。
    fn preset_chip(
        &self,
        index: usize,
        name: &str,
        value: u32,
        active: bool,
        dragging: bool,
        chip_height: f32,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        div()
            .id(SharedString::from(format!("bright-preset-{index}")))
            .h(px(chip_height))
            .flex_none()
            .px(px(7.))
            .flex()
            .items_center()
            .rounded(px(theme::RADIUS_SMALL))
            .bg(if active {
                theme::accent()
            } else {
                theme::track()
            })
            .text_size(px(10.5))
            .text_color(if active {
                theme::text_primary()
            } else {
                theme::text_secondary()
            })
            .cursor_pointer()
            .when(dragging, |style| style.opacity(0.4))
            .hover(|style| style.bg(theme::surface_hover()))
            // 左键按下就开始「可能要拖」：拖到哪台显示器上由 `drag_sensors` 定，
            // 原地松手 = 套给光标所在那台。
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    this.confirm_delete = None;
                    this.drag_preset = Some(PresetDrag {
                        index,
                        value,
                        over: None,
                        origin: event.position,
                        moved: false,
                    });
                    this.drag_at = Some(event.position);
                    cx.notify();
                }),
            )
            // 中键：一次套给所有能调亮度的显示器。
            .on_mouse_down(
                gpui::MouseButton::Middle,
                cx.listener(move |this, _: &gpui::MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    let keys = this.lit_display_keys();
                    this.apply_preset(keys, value, cx);
                }),
            )
            // 右键：删这颗预设，先问一句（确认条见 `confirm_row`）。
            .on_mouse_down(
                gpui::MouseButton::Right,
                cx.listener(move |this, _: &gpui::MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    this.confirm_delete = Some(index);
                    cx.notify();
                }),
            )
            .child(name.to_string())
            .into_any_element()
    }

    /// 右键删除的确认条：原地顶掉整排预设，高度和原来一样，不跳。
    fn confirm_row(
        &self,
        index: usize,
        presets: &[crate::config::BrightnessPreset],
        chip_height: f32,
        lang: Lang,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let name = presets
            .get(index)
            .map(|preset| preset.name.clone())
            .unwrap_or_default();
        let question = if lang.is_en() {
            format!("Delete preset \"{name}\"?")
        } else {
            format!("删除预设「{name}」？")
        };
        div()
            .flex()
            .items_center()
            .gap(px(CHIP_GAP))
            .h(px(chip_height))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .truncate()
                    .text_size(px(10.5))
                    .text_color(theme::text_secondary())
                    .child(question),
            )
            .child(self.confirm_button(
                "bright-preset-delete",
                lang.pick("删除", "Delete"),
                true,
                chip_height,
                cx.listener(move |this, _: &ClickEvent, window, cx| {
                    cx.stop_propagation();
                    this.delete_preset(index, window, cx);
                }),
            ))
            .child(self.confirm_button(
                "bright-preset-cancel",
                lang.pick("取消", "Cancel"),
                false,
                chip_height,
                cx.listener(|this, _: &ClickEvent, _, cx| {
                    cx.stop_propagation();
                    this.confirm_delete = None;
                    cx.notify();
                }),
            ))
            .into_any_element()
    }

    /// 确认条上的那颗按钮（`danger` = 删除）。
    fn confirm_button(
        &self,
        id: &'static str,
        label: &'static str,
        danger: bool,
        height: f32,
        on_click: impl Fn(&ClickEvent, &mut Window, &mut gpui::App) + 'static,
    ) -> gpui::AnyElement {
        div()
            .id(SharedString::from(id))
            .h(px(height))
            .flex_none()
            .px(px(8.))
            .flex()
            .items_center()
            .rounded(px(theme::RADIUS_SMALL))
            .bg(theme::track())
            .text_size(px(10.5))
            .text_color(if danger {
                theme::danger()
            } else {
                theme::text_secondary()
            })
            .cursor_pointer()
            .hover(|style| style.bg(theme::surface_hover()))
            // 按下就吃掉：整块的根节点会在左键按下时收起确认条，
            // 不拦住的话按钮还没等到抬起就没了（点了等于没点）。
            .on_mouse_down(
                gpui::MouseButton::Left,
                |_: &gpui::MouseDownEvent, _: &mut Window, cx: &mut gpui::App| {
                    cx.stop_propagation();
                },
            )
            .on_click(on_click)
            .child(label)
            .into_any_element()
    }

    /// 套在显示器那一块上的两层装饰：拖放落点的高亮框、套用成功的闪光。
    ///
    /// 尺寸直接按块高写死：绝对定位的子元素在这里量不准（`size_full` / 四边贴零
    /// 都拿不到父盒子的高度，画出来是 0 高），块高本来就是算出来的，传进来最稳。
    fn block_decor(&self, key: &str, height: f32) -> Vec<gpui::AnyElement> {
        let dragging = self.drag_preset.as_ref().and_then(|drag| drag.over.as_deref()) == Some(key);
        let flashing = self
            .flash
            .as_ref()
            .is_some_and(|flash| flash.keys.iter().any(|item| item == key));
        let mut layers: Vec<gpui::AnyElement> = Vec::new();
        if !dragging && !flashing {
            return layers;
        }
        if dragging {
            // 落点这一块描一圈强调色：配合整块的淡底，一眼看出会套给谁。
            layers.push(
                div()
                    .absolute()
                    .left_0()
                    .top_0()
                    .w_full()
                    .h(px(height))
                    .rounded(px(theme::RADIUS_SMALL))
                    .border_1()
                    .border_color(theme::accent())
                    .into_any_element(),
            );
        }
        if flashing {
            // 一亮就到位，然后一路淡出（二次缓出）：闪一下比慢慢涨起来更像「做完了」。
            layers.push(
                div()
                    .absolute()
                    .left_0()
                    .top_0()
                    .w_full()
                    .h(px(height))
                    .rounded(px(theme::RADIUS_SMALL))
                    .bg(theme::accent())
                    .with_animation(
                        ElementId::Name(format!("bright-flash-{key}-{}", self.flash_seq).into()),
                        Animation::new(Duration::from_millis(FLASH_MS)),
                        |layer, delta| {
                            let left = 1.0 - delta;
                            layer.opacity(FLASH_PEAK * left * left)
                        },
                    )
                    .into_any_element(),
            );
        }
        layers
    }

    /// 拖动时跟着光标走的那颗预设：拖的是什么、拖到哪儿，手上一直看得见。
    fn drag_ghost(&self) -> Option<gpui::AnyElement> {
        let drag = self.drag_preset.as_ref().filter(|drag| drag.moved)?;
        let at = self.drag_at?;
        let area = self.drag_area.get()?;
        let cfg = self.cfg();
        let name = cfg.brightness.presets.get(drag.index)?.name.clone();
        let chip = metrics(&cfg).chip;
        Some(
            div()
                .absolute()
                .left(px(at.x.as_f32() - area.origin.x.as_f32() - 4.0))
                .top(px(at.y.as_f32() - area.origin.y.as_f32() - chip / 2.0))
                .h(px(chip))
                .px(px(7.))
                .flex()
                .items_center()
                .rounded(px(theme::RADIUS_SMALL))
                .bg(theme::accent())
                .text_size(px(10.5))
                .text_color(theme::text_primary())
                .child(name)
                .into_any_element(),
        )
    }

    /// 拖放用的窗口级监听：松手时按坐标认落点。
    ///
    /// 每帧重挂一次（和页脚预览同一套）：元素上的 `on_mouse_move` 只在悬停时触发，
    /// 判不了「拖到了哪一块」；`window.on_mouse_event` 才是全局的。
    fn drag_sensors(&self, cx: &mut Context<Self>) -> gpui::AnyElement {
        let area = self.drag_area.clone();
        let weak = cx.entity().downgrade();
        gpui::canvas(
            move |bounds, window, _cx| {
                area.set(Some(bounds));

                // 移动：拖着的时候记下光标、认落点（落点那台提亮）。
                let weak_for_move = weak.clone();
                window.on_mouse_event(move |event: &gpui::MouseMoveEvent, phase, _, cx| {
                    if phase != gpui::DispatchPhase::Bubble {
                        return;
                    }
                    // 在窗口外面松手收不到抬起事件：按钮都松了就把这一趟收掉。
                    if event.pressed_button != Some(gpui::MouseButton::Left) {
                        let position = event.position;
                        let _ = weak_for_move
                            .update(cx, |this, cx| this.finish_preset_drag(position, false, cx));
                        return;
                    }
                    let position = event.position;
                    let _ = weak_for_move.update(cx, |this, cx| this.update_drag(position, cx));
                });

                // 抬起：落点决定套给哪台；还在块里但没落到哪台就是「点了一下」。
                let area_for_up = area.clone();
                window.on_mouse_event(move |event: &gpui::MouseUpEvent, phase, _, cx| {
                    if phase != gpui::DispatchPhase::Bubble
                        || event.button != gpui::MouseButton::Left
                    {
                        return;
                    }
                    let position = event.position;
                    let inside = area_for_up
                        .get()
                        .is_some_and(|area| area.contains(&position));
                    let _ = weak.update(cx, |this, cx| {
                        this.finish_preset_drag(position, inside, cx);
                    });
                });
            },
            |_, _, _, _| {},
        )
        .absolute()
        .left_0()
        .top_0()
        .size_full()
        .into_any_element()
    }

    /// 「保存预设」：把当前亮度存成一个预设。
    ///
    /// 横排长在名称行最右端（`compact = false`，带勾图标、写全四个字）；
    /// 竖排那一列只有 58px 净宽，缩成一格不带图标的「保存预设 / Save」。
    fn save_button(
        &self,
        key: &str,
        editable: bool,
        compact: bool,
        lang: Lang,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let key_for_save = key.to_string();
        let chip_height = metrics(&self.cfg()).chip;
        div()
            .id(SharedString::from(format!("bright-save-{key}")))
            .h(px(chip_height))
            .flex_none()
            .px(px(if compact { 4. } else { 7. }))
            .flex()
            .items_center()
            .gap(px(3.))
            .rounded(px(theme::RADIUS_SMALL))
            .bg(theme::surface_sunken())
            .text_size(px(10.5))
            .text_color(theme::text_tertiary())
            .whitespace_nowrap()
            .when(editable, |style| {
                style
                    .cursor_pointer()
                    .hover(|style| style.bg(theme::surface_hover()))
            })
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                cx.stop_propagation();
                if !editable {
                    return;
                }
                this.save_preset(&key_for_save, window, cx);
            }))
            .when(!compact, |style| {
                style.child(icon("check", 10.0, theme::text_tertiary()))
            })
            .child(save_label(lang, compact))
            .into_any_element()
    }
}

/// 显示器名去掉括号里的型号：`AOC 24G2 (Generic PnP Monitor)` → `AOC 24G2`。
///
/// 竖向的列只有六十来像素宽，括号里的型号一定放不下，干脆不写。
/// 括号在开头（名字本身就是型号）时原样保留，不然会剩下一个空字符串。
fn strip_model(name: &str) -> String {
    match name.find(['(', '（']) {
        Some(index) => {
            let head = name[..index].trim();
            if head.is_empty() {
                name.trim().to_string()
            } else {
                head.to_string()
            }
        }
        None => name.trim().to_string(),
    }
}

/// 量一段文字的实际宽度（字号要和要排版的那段一致）。
fn measure_text(text: &str, size: f32, window: &mut Window) -> f32 {
    let font = window.text_style().font();
    let run = TextRun {
        len: text.len(),
        font,
        color: gpui::black(),
        background_color: None,
        underline: None,
        strikethrough: None,
        letter_spacing: None,
    };
    window
        .text_system()
        .layout_line(text, px(size), &[run], None)
        .width
        .as_f32()
}

#[cfg(test)]
mod tests {
    use super::{BLOCK_GAP, CHIP_HEIGHT, HEADER_HEIGHT, panel_size, strip_model};
    use crate::config::{Config, PanelLayout};

    #[test]
    fn vertical_panel_is_sized_by_the_display_count() {
        let mut config = Config::default();
        config.flyout_width = 350;
        config.brightness.width = 0;

        // 横排：宽度听配置（跟随音量面板），高度随台数长。
        let (w1, h1) = panel_size(&config, 1);
        let (_, h2) = panel_size(&config, 2);
        assert_eq!(w1, 350.0);
        assert!(h2 > h1, "多一台显示器，横排要更高");

        // 竖排：一台一列，宽度随台数长，高度不变。
        config.brightness.panel_layout = PanelLayout::Vertical;
        let (vw1, vh1) = panel_size(&config, 1);
        let (vw2, vh2) = panel_size(&config, 2);
        assert!(vw2 > vw1, "多一台显示器，竖排要更宽");
        assert_eq!(vh1, vh2, "竖排的高度和台数无关");
        assert_eq!(vw2 - vw1 > 0.0, true);
        // 宽度和列数严格成比例：多一列正好多一列宽 + 一条缝。
        let (vw3, _) = panel_size(&config, 3);
        assert!((vw3 - vw2) - (vw2 - vw1) < 0.01);
        // 竖排比横排窄得多（否则就没必要竖起来了）。
        assert!(vw1 < w1);
    }

    /// 预设只在整块底部占**一排**：加几颗预设都只多这一排，不按显示器台数重复。
    #[test]
    fn the_preset_bar_is_one_row_for_the_whole_block() {
        let mut one = Config::default();
        one.brightness.add_preset(80);
        let mut two = Config::default();
        two.brightness.add_preset(80);

        let bare = panel_size(&Config::default(), 2).1;
        let with = panel_size(&two, 2).1;
        // 少掉的只有这一排（连上面那条缝），不是每台显示器各来一遍。
        assert!(
            (with - bare - (CHIP_HEIGHT + BLOCK_GAP)).abs() < 0.01,
            "{bare} -> {with}"
        );
        // 同一颗预设，一台和两台面板的差 = 多出来那块显示器，不多一排预设。
        let one_height = panel_size(&one, 1).1;
        let two_height = panel_size(&one, 2).1;
        assert!(two_height > one_height);
    }

    /// 融合进音量面板时不再有「显示器亮度」那一行标题。
    #[test]
    fn a_merged_block_drops_the_title() {
        let mut config = Config::default();
        config.flyout_width = 350;
        config.brightness.width = 0;
        let standalone = panel_size(&config, 2).1;
        let merged = super::embed_height(&config, 2);
        assert!(
            (standalone - merged - (HEADER_HEIGHT + 1.0)).abs() < 0.01,
            "{standalone} vs {merged}"
        );
    }

    /// 亮度块每一行都跟着面板「行高」等比伸缩：行高调高，整块跟着长高。
    #[test]
    fn the_block_follows_the_panel_row_height() {
        let mut config = Config::default();
        let base = super::embed_height(&config, 2);
        // 默认行高 52（图标 32 + 20）；写成 88 就是 1.7 倍。
        config.panel_line_height = 88.0;
        let tall = super::embed_height(&config, 2);
        // 两台显示器各有名称行 + 亮度条，都得跟着变高。
        assert!(tall - base > 40.0, "{base} -> {tall}");
    }

    /// 竖排融合时亮度列的下限随预设长 —— 父面板要按它抬高度，否则最底那排被裁。
    #[test]
    fn a_merged_vertical_column_grows_with_its_presets() {
        let mut config = Config::default();
        config.panel_layout = PanelLayout::Vertical;
        config.brightness.panel_layout = PanelLayout::Vertical;
        config.brightness.enabled = true;
        config.brightness.merged = true;
        let bare = super::min_embed_height(&config, 2);
        config.brightness.add_preset(30);
        let one = super::min_embed_height(&config, 2);
        assert!(one > bare, "加了一个预设反而没变高：{bare} -> {one}");
        // 一列装不下就折行，折出来的那一行同样要占高度。
        for value in [40, 50, 60, 70, 80, 90] {
            config.brightness.add_preset(value);
        }
        let many = super::min_embed_height(&config, 2);
        assert!(many > one, "预设多到折行就该再高一行：{one} -> {many}");
    }

    #[test]
    fn vertical_height_follows_the_audio_panel_until_it_is_set() {
        let mut config = Config::default();
        config.panel_layout = PanelLayout::Vertical;
        config.brightness.panel_layout = PanelLayout::Vertical;

        // 默认（0）：跟音频面板竖向排版时一样高，两个面板并排齐平。
        let audio = crate::windows::flyout::vertical_panel_height(&config);
        assert_eq!(panel_size(&config, 2).1, audio);

        // 音频面板配高了，亮度面板跟着一起长（它自己还是 0）。
        config.vertical_height = 260;
        assert_eq!(panel_size(&config, 2).1, 260.0);

        // 亮度面板写了数字就自己说了算。
        config.brightness.vertical_height = 320;
        assert_eq!(panel_size(&config, 2).1, 320.0);

        // 写得太小按「至少放得下标题 + 名称 + 推子 + 读数 + 预设」算，不会把推子压没。
        config.brightness.vertical_height = 10;
        let height = panel_size(&config, 2).1;
        assert!(
            height >= 140.0 && height < 200.0,
            "高度 {height} 应该被抬到下限附近"
        );
    }

    #[test]
    fn a_taller_panel_gives_the_extra_room_to_the_faders() {
        let mut config = Config::default();
        config.brightness.panel_layout = PanelLayout::Vertical;
        config.brightness.vertical_height = 200;
        let short = super::columns_height(&config, 2, false) - super::column_fixed_height(&config);

        config.brightness.vertical_height = 400;
        let tall = super::columns_height(&config, 2, false) - super::column_fixed_height(&config);
        // 多出来的高度全部进推子，正好差 200。
        assert!((tall - short - 200.0).abs() < 0.01, "{short} -> {tall}");
    }

    #[test]
    fn display_name_drops_the_model_in_parentheses() {
        assert_eq!(strip_model("AOC 24G2 (Generic PnP Monitor)"), "AOC 24G2");
        // 全角括号也认。
        assert_eq!(strip_model("戴尔 U2720Q（DP）"), "戴尔 U2720Q");
        // 括号里还套括号时只切第一个。
        assert_eq!(strip_model("DELL (Realtek(R) Audio)"), "DELL");
        // 没有括号：原样。
        assert_eq!(strip_model("内置显示器"), "内置显示器");
        // 括号在开头：原样保留，不能变成空字符串。
        assert_eq!(strip_model("(USB Audio)"), "(USB Audio)");
    }
}
