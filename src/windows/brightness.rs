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
    Animation, AnimationExt, ClickEvent, Context, ElementId, FocusHandle, FontWeight, IntoElement,
    KeyDownEvent, Render, SharedString, TextRun, Window, div, px,
};

use crate::{
    config::{BrightnessPlacement, BrightnessPreset, ConfigStore, DisplayNameMode},
    display::{DisplayCommand, DisplayEngine, DisplayInfo},
    i18n::Lang,
    ui::{Entrance, SliderDragState, VolumeSlider, anim, icon, theme, wheel_direction},
};

/// 面板宽度不再写死：用户可调（`config.brightness.width`），
/// 这一排横向的固定开销 = 左右内边距 + 两个步进钮 + 缝。
const STEP_WIDTH: f32 = 16.0;
const STEP_GAP: f32 = 3.0;

const PAD: f32 = 7.0;
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

/// 滑条长度：面板宽度扣掉两侧的步进钮和缝。
///
/// 默认宽度 152 时正好 100px —— 鼠标走 1 个像素就是 1 个百分点。
pub fn slider_width(panel_width: f32) -> f32 {
    (panel_width - PAD * 2.0 - STEP_WIDTH * 2.0 - STEP_GAP * 2.0).max(40.0)
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
pub fn panel_size(cfg: &crate::config::Config, displays: usize) -> (f32, f32) {
    if cfg.brightness.panel_layout.is_vertical() {
        (vertical_width(displays), vertical_height(cfg))
    } else {
        (cfg.brightness_width(), horizontal_height(cfg, displays))
    }
}

/// 横排：标题 + 每台显示器一块（名称 / 亮度条 / 预设）。
fn horizontal_height(cfg: &crate::config::Config, displays: usize) -> f32 {
    let block = NAME_ROW_HEIGHT
        + ROW_GAP
        + SLIDER_HEIGHT
        + ROW_GAP
        + preset_rows(
            &cfg.brightness.presets,
            cfg.language,
            cfg.brightness.panel_width(cfg.flyout_width()) - PAD * 2.0,
        );
    let count = displays.max(1) as f32;
    // 多台之间还夹一条 1px 分隔线，上下各 BLOCK_GAP。
    let separators = (count - 1.0).max(0.0);
    (PAD * 2.0 + HEADER_HEIGHT + 1.0 + count * block + separators * (BLOCK_GAP * 2.0 + 1.0))
        .max(80.0)
}

/// 竖排：一台显示器一列，宽 = 内边距 + 若干列。
fn vertical_width(displays: usize) -> f32 {
    let count = displays.max(1) as f32;
    PAD * 2.0 + count * V_COLUMN_WIDTH + (count - 1.0) * V_COLUMN_GAP
}

/// 竖排：标题 + 最高的一列。所有列高度一样（预设有几个是全局配置），取一列即可。
///
/// `brightness.vertical_height` 写 0（默认）就跟着**音频面板竖向排版时的高度**走，
/// 两个面板并排时上下齐平；写了数字就自己定。
fn vertical_height(cfg: &crate::config::Config) -> f32 {
    let at_least = chrome_height() + column_fixed_height(cfg) + V_FADER_MIN;
    let target = match cfg.brightness.vertical_panel_height() {
        Some(height) => height,
        None => crate::windows::flyout::vertical_panel_height(cfg),
    };
    target.max(at_least)
}

/// 面板上下那些不归列管的高度：外内边距 + 标题 + 分隔线。
fn chrome_height() -> f32 {
    PAD * 2.0 + HEADER_HEIGHT + 1.0
}

/// 一列里除推子之外的高度：名称 / 读数+步进 / 预设，以及它们之间的缝。
fn column_fixed_height(cfg: &crate::config::Config) -> f32 {
    V_LABEL_ROW
        + ROW_GAP * 3.0
        + V_STEP_ROW
        + preset_rows(
            &cfg.brightness.presets,
            cfg.language,
            V_COLUMN_WIDTH - V_COLUMN_PAD * 2.0,
        )
}

/// 一列的总高度。
fn column_height(cfg: &crate::config::Config) -> f32 {
    vertical_height(cfg) - chrome_height()
}

/// 推子那一截：列高扣掉固定的几段，剩下的都给它（最少 V_FADER_MIN）。
fn fader_height(cfg: &crate::config::Config) -> f32 {
    (column_height(cfg) - column_fixed_height(cfg)).max(V_FADER_MIN)
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

/// 预设那一排折成几行、一共多高（含最后那个「保存」格子）。
///
/// `inner_width` 是这一排能用的宽度（横排是面板内宽，竖排是一列的净宽）。
fn preset_rows(presets: &[BrightnessPreset], lang: Lang, inner_width: f32) -> f32 {
    let mut widths: Vec<f32> = presets
        .iter()
        .map(|preset| chip_width(&preset.name))
        .collect();
    // 「保存」：内边距 + 10px 图标 + 3px 间距 + 文字。
    widths.push(14.0 + 10.0 + 3.0 + chip_width(lang.pick("保存", "Save")) - 14.0);

    let inner = inner_width;
    let mut rows = 1.0;
    let mut used = 0.0;
    for width in widths {
        if used > 0.0 && used + CHIP_GAP + width > inner {
            rows += 1.0;
            used = width;
        } else {
            used += if used > 0.0 { CHIP_GAP } else { 0.0 } + width;
        }
    }
    rows * CHIP_HEIGHT + (rows - 1.0) * CHIP_GAP
}

pub struct Brightness {
    pub config: Rc<RefCell<ConfigStore>>,
    pub engine: Rc<DisplayEngine>,
    /// 工作线程报上来的显示器列表（与 `AppState` 共用同一份）。
    pub displays: Rc<RefCell<Vec<DisplayInfo>>>,
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
    /// 入场动画：和音量面板共用 `ui::anim::Entrance`，两扇窗必须同时长出来。
    anim: Entrance,
    /// 上一次处理滚轮的时刻：`on_scroll_wheel` 在同一帧里可能被派发好几遍，
    /// 不节流的话一格滚轮会连跳好几档（音量面板里踩过同一个坑）。
    wheel_last_at: Option<Instant>,
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
            focus,
            drag: SliderDragState::new(),
            drag_values: Rc::new(RefCell::new(HashMap::new())),
            editing: None,
            edit_replace: false,
            error,
            visible: false,
            last_height: 0.0,
            anim: Entrance::new(),
            wheel_last_at: None,
        }
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    fn cfg(&self) -> crate::config::Config {
        self.config.borrow().get().clone()
    }

    /// 窗口高度：横排是标题 + 每台一行；竖排是标题 + 一列。
    pub fn desired_height(&self) -> f32 {
        panel_size(&self.cfg(), self.displays.borrow().len()).1
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
            self.set_brightness(&key, value.min(100));
        }
        cx.notify();
    }

    fn cancel_edit(&mut self, cx: &mut Context<Self>) {
        self.edit_replace = false;
        if self.editing.take().is_some() {
            cx.notify();
        }
    }

    fn set_brightness(&self, key: &str, value: u32) {
        self.engine.send(DisplayCommand::SetBrightness {
            key: key.to_string(),
            value: value.min(100),
        });
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
            self.set_brightness(key, next);
            cx.notify();
        }
    }

    pub fn show(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.visible = true;
        // 上次拖动的预览值到这次就不作数了，全部以真实回读为准。
        self.drag_values.borrow_mut().clear();
        self.editing = None;
        self.edit_replace = false;
        // 每次打开都重读一遍：显示器自己的按钮、游戏全屏切换都可能改过亮度。
        self.engine.send(DisplayCommand::Refresh);
        // 动画开关与时长跟音量面板取自同一份配置，曲线也走同一个 `Entrance`，
        // 两扇窗才会一起长出来、一起收回去。
        let animate = self.cfg().popup_animation && !cx.reduce_motion();
        let config_ms = self.cfg().popup_animation_ms;
        self.anim.begin(animate, config_ms);
        // 窗口几何变更同样放到本帧绘制之后（和面板一致）。
        cx.defer_in(window, move |this, window, cx| {
            this.update_frame(window, cx);
            // 先整窗透明再显示：窗口一露头 DWM 就会画圆角框和阴影，
            // 此时内容（动画第一帧）还没提交，不透明的话会闪一下空框。
            this.anim.set_alpha(window, 0.0);
            crate::windows::platform::set_visible(window, true);
            // 只提到最上层，不抢焦点：抢了焦点音量面板会立刻因为失焦收起。
            crate::windows::platform::bring_to_front(window);
            if animate {
                this.anim.start();
                let session = this.anim.session();
                // 兜底：合成器不给帧时到点强制收尾，绝不停在半截高度。
                let deadline = this.anim.fallback_after();
                let view = cx.entity().downgrade();
                cx.spawn(async move |_, cx| {
                    cx.background_executor().timer(deadline).await;
                    let _ = view.update(cx, |this, cx| {
                        if this.anim.settle(session) {
                            cx.notify();
                        }
                    });
                })
                .detach();
            } else {
                this.anim.set_alpha(window, 1.0);
            }
            cx.notify();
        });
    }

    pub fn hide(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.visible {
            return;
        }
        self.visible = false;
        self.editing = None;
        self.edit_replace = false;
        self.drag_values.borrow_mut().clear();
        // 收起前先把画面收成 0 高度再画一帧：系统拿「上一次提交的画面」显示窗口，
        // 缓存里若是完整内容，下次弹出会先闪一整块再开始动画。
        let animate = self.cfg().popup_animation && !cx.reduce_motion();
        let config_ms = self.cfg().popup_animation_ms;
        self.anim.begin(animate, config_ms);
        cx.notify();
        cx.defer_in(window, |this, window, _| {
            crate::windows::platform::set_visible(window, false);
            // 藏好后停在全透明：下次显示不会先闪一帧不透明的空窗（和面板一致）。
            this.anim.set_alpha(window, 0.0);
        });
    }

    /// 贴着音量面板摆好：贴哪一侧由配置决定（左 / 右 / 上 / 下）。
    ///
    /// 音量面板自己贴任务栏所在的角落，所以「外侧」那两侧（右下角的面板上、
    /// 右/下）根本放不下 —— 这时候翻到对面去，绝不压到面板上。
    pub fn update_frame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let cfg = self.cfg();
        let displays = self.displays.borrow().len();
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

        crate::windows::platform::set_frame(
            window,
            gpui::Bounds::new(gpui::point(px(x), px(y)), gpui::size(px(width), px(height))),
            window.scale_factor(),
        );
        window.bounds_changed(cx);
    }
}

impl Render for Brightness {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let lang = self.cfg().language;
        let displays = self.displays.borrow().clone();
        let error = self.error.borrow().clone();

        // 竖向：一台显示器一列，横着排开；横向：一台一行，块之间夹一条分隔线。
        let vertical = self.cfg().brightness.panel_layout.is_vertical();
        let mut blocks: Vec<gpui::AnyElement> = Vec::new();
        if displays.is_empty() {
            blocks.push(self.empty_hint(lang, error));
        } else if vertical {
            for (index, display) in displays.iter().enumerate() {
                blocks.push(self.monitor_column(display, index, lang, window, cx));
            }
        } else {
            for (index, display) in displays.iter().enumerate() {
                if index > 0 {
                    blocks.push(
                        div()
                            .h(px(1.))
                            .flex_none()
                            .mx(px(2.))
                            .bg(theme::border())
                            .into_any_element(),
                    );
                }
                blocks.push(self.monitor_block(display, lang, cx));
            }
        }

        let content = div()
            .size_full()
            .flex()
            .flex_col()
            // 根底色用不透明版：主题的 `surface` 留了 3% 透光，不透明度给到 100%
            // 时还会隐隐透出桌面。要透多少由 `ui_opacity` 一个人说了算。
            .bg(theme::surface_window())
            .rounded(px(theme::RADIUS_WINDOW))
            .overflow_hidden()
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                this.handle_key(event, window, cx);
            }))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _: &gpui::MouseDownEvent, _, cx| {
                    // 点别处 = 确认这次手动输入。
                    this.commit_edit(cx);
                }),
            )
            .child(self.header(lang))
            .child(div().h(px(1.)).flex_none().bg(theme::border()))
            .child(
                div()
                    .flex()
                    .when(vertical, |style| {
                        style.flex_row().items_start().gap(px(V_COLUMN_GAP))
                    })
                    .when(!vertical, |style| style.flex_col().gap(px(BLOCK_GAP)))
                    .px(px(PAD))
                    .py(px(PAD))
                    .children(blocks),
            );

        // 入场动画：窗口按完整高度摆好，只让内容从底边往上「长」出来 ——
        // 和音量面板同一套裁切方式（改原生窗口尺寸会让 gpui 的 viewport 缓存慢一帧）。
        let progress = if self.visible {
            self.anim.tick(window)
        } else {
            self.anim.progress()
        };
        let full_height = self.last_height.max(1.0);
        let visible_height = anim::anim_height(progress, full_height);

        div().size_full().relative().child(
            div()
                .absolute()
                .left_0()
                .right_0()
                .bottom_0()
                .h(px(visible_height))
                .overflow_hidden()
                .child(
                    div()
                        .absolute()
                        .left_0()
                        .right_0()
                        .top(px(visible_height - full_height))
                        .h(px(full_height))
                        .child(content),
                ),
        )
    }
}

impl Brightness {
    fn header(&self, lang: Lang) -> gpui::AnyElement {
        div()
            .h(px(HEADER_HEIGHT))
            .flex_none()
            .px(px(PAD + 2.0))
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
        value: u32,
        editable: bool,
        min_width: f32,
        lang: Lang,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let editing = matches!(&self.editing, Some((edit_key, _)) if edit_key == key);
        let edit_text = match &self.editing {
            Some((edit_key, buffer)) if edit_key == key => buffer.clone(),
            _ => String::new(),
        };
        let key_for_click = key.to_string();
        div()
            .id(SharedString::from(format!("bright-value-{key}")))
            .flex_none()
            .min_w(px(min_width))
            .h(px(18.))
            .px(px(4.))
            .flex()
            .items_center()
            .justify_center()
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
                style.child(if editable {
                    format!("{value}%")
                } else {
                    lang.pick("不支持", "N/A").to_string()
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
                    this.set_brightness(&key, next);
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
        .enabled(editable)
        .vertical(vertical)
        .height(SLIDER_HEIGHT)
        .on_change(move |value, window, _| {
            drag_values
                .borrow_mut()
                .insert(key_for_slider.clone(), value);
            window.refresh();
            engine.send(DisplayCommand::SetBrightness {
                key: key_for_slider.clone(),
                value: (value * 100.0).round().clamp(0.0, 100.0) as u32,
            });
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
        let percent = self.current_percent(&key);
        let slider_width = slider_width(config.brightness_width());
        let editable = percent.is_some();
        let value = percent.unwrap_or(0);
        let slider_value = self
            .drag_values
            .borrow()
            .get(&key)
            .copied()
            .unwrap_or(value as f32 / 100.0);

        let value_slot = self.value_slot(&key, value, editable, 46.0, lang, cx);

        // 名称行：名字 + 读数。步进钮挪到下面的亮度条两侧 ——
        // 面板窄了之后，「名字 + − 100% +」挤在一行会把显示器名截得只剩几个字母。
        let label = div()
            .flex()
            .items_center()
            .gap(px(4.))
            .h(px(NAME_ROW_HEIGHT))
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
            .child(value_slot);

        let slider = self.brightness_slider(&key, slider_value, editable, false);

        // 「− 亮度条 +」：滑条长度由面板宽度换算，这一排正好铺满。
        let slider_row = div()
            .flex()
            .items_center()
            .justify_center()
            .gap(px(STEP_GAP))
            .h(px(SLIDER_HEIGHT))
            .child(self.step_button(&key, editable, "−", -1, STEP_WIDTH, cx))
            .child(div().flex_none().w(px(slider_width)).child(slider))
            .child(self.step_button(&key, editable, "+", 1, STEP_WIDTH, cx));

        let presets = self.preset_row(&key, value, editable, lang, false, cx);

        let key_for_wheel = key.clone();
        div()
            .id(SharedString::from(format!("bright-block-{key}")))
            .flex()
            .flex_col()
            .gap(px(ROW_GAP))
            // 滚轮挂在整块上，不是只挂在滑条上：光标落在名称行或预设行也算「悬停在这台显示器」，
            // 和面板里「整行都能滚」是同一套手感。
            .on_scroll_wheel(cx.listener(
                move |this, event: &gpui::ScrollWheelEvent, _window, cx| {
                    this.on_block_wheel(&key_for_wheel, event, cx);
                },
            ))
            .child(label)
            .child(slider_row)
            .child(presets)
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
        let percent = self.current_percent(&key);
        let editable = percent.is_some();
        let value = percent.unwrap_or(0);
        let slider_value = self
            .drag_values
            .borrow()
            .get(&key)
            .copied()
            .unwrap_or(value as f32 / 100.0);

        let label = self.column_label(display, index, cfg.brightness.name_mode, window);

        // 推子：整列里最高的一块，宽度撑满列。长度由面板高度算，
        // 面板配高了推子就长（调音台那种手感）。
        let slider = self.brightness_slider(&key, slider_value, editable, true);
        let fader = div()
            .w_full()
            .h(px(fader_height(&cfg)))
            .flex_none()
            .px(px(V_COLUMN_PAD))
            .child(slider);

        // 读数那排：− 80% +，正好铺满 64 的列宽。
        let step_row = div()
            .flex()
            .items_center()
            .justify_center()
            .gap(px(1.))
            .h(px(V_STEP_ROW))
            .flex_none()
            .child(self.step_button(&key, editable, "−", -1, 16.0, cx))
            .child(self.value_slot(&key, value, editable, 30.0, lang, cx))
            .child(self.step_button(&key, editable, "+", 1, 16.0, cx));

        let presets = self.preset_row(&key, value, editable, lang, true, cx);

        let key_for_wheel = key.clone();
        div()
            .id(SharedString::from(format!("bright-col-{key}")))
            .w(px(V_COLUMN_WIDTH))
            .h(px(column_height(&cfg)))
            .flex_none()
            .flex()
            .flex_col()
            .gap(px(ROW_GAP))
            .on_scroll_wheel(cx.listener(
                move |this, event: &gpui::ScrollWheelEvent, _window, cx| {
                    this.on_block_wheel(&key_for_wheel, event, cx);
                },
            ))
            // 列里每一行都先按列宽收窄再摆：名称行和预设行必须和推子共用同一条中线，
            // 不然编号和保存格子会各自贴着左边，看着和推子差几像素（列宽 64 很窄，看得出）。
            .child(div().w_full().px(px(V_COLUMN_PAD)).child(label))
            .child(fader)
            .child(step_row)
            .child(div().w_full().px(px(V_COLUMN_PAD)).child(presets))
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

    /// 预设 chips：点一下套用，右键删除；最后一个是「保存」。
    ///
    /// `center` = 竖向排版：列太窄，格子按列中线居中摆，和推子对齐。
    fn preset_row(
        &self,
        key: &str,
        value: u32,
        editable: bool,
        lang: Lang,
        center: bool,
        cx: &mut Context<Self>,
    ) -> gpui::AnyElement {
        let presets = self.cfg().brightness.presets;
        let mut chips: Vec<gpui::AnyElement> = Vec::new();

        for (index, preset) in presets.iter().enumerate() {
            let target = preset.brightness;
            let key_for_click = key.to_string();
            let active = editable && target == value;
            chips.push(
                div()
                    .id(SharedString::from(format!("bright-preset-{key}-{index}")))
                    .h(px(CHIP_HEIGHT))
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
                    .hover(|style| style.bg(theme::surface_hover()))
                    .on_click(cx.listener(move |this, _: &ClickEvent, _, cx| {
                        cx.stop_propagation();
                        // 手动输入框留着会让读数看起来没变，先收掉。
                        this.editing = None;
                        this.set_brightness(&key_for_click, target);
                        cx.notify();
                    }))
                    // 右键删掉这个预设：面板里没有「悬浮提示」，右键是这里最省事的方式。
                    .on_mouse_down(
                        gpui::MouseButton::Right,
                        cx.listener(move |this, _: &gpui::MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            let _ = this.config.borrow_mut().update(|config| {
                                if index < config.brightness.presets.len() {
                                    config.brightness.presets.remove(index);
                                }
                            });
                            // 少了一格，窗口跟着缩短。
                            this.update_frame(window, cx);
                            cx.notify();
                        }),
                    )
                    .child(preset.name.clone())
                    .into_any_element(),
            );
        }

        let key_for_save = key.to_string();
        chips.push(
            div()
                .id(SharedString::from(format!("bright-save-{key}")))
                .h(px(CHIP_HEIGHT))
                .px(px(7.))
                .flex()
                .items_center()
                .gap(px(3.))
                .rounded(px(theme::RADIUS_SMALL))
                .bg(theme::surface_sunken())
                .text_size(px(10.5))
                .text_color(theme::text_tertiary())
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
                .child(icon("check", 10.0, theme::text_tertiary()))
                .child(lang.pick("保存", "Save"))
                .into_any_element(),
        );

        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(CHIP_GAP))
            .when(center, |row| row.w_full().justify_center())
            .children(chips)
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
    use super::{panel_size, strip_model};
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
        let short = super::fader_height(&config);

        config.brightness.vertical_height = 400;
        let tall = super::fader_height(&config);
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
