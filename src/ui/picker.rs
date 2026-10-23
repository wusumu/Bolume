//! 自绘取色器：饱和度 / 明度方块 + 色相条。
//!
//! 设置页的「背景色 / 强调色」和 OSD 编辑器的「元素颜色」共用这一份，别再各写
//! 一套 —— 两边原来各有一份几乎一样的实现，踩的也是同一个坑：
//!
//! 1. 拖拽挂在**根节点**的 `on_mouse_move` 上，子控件一 `stop_propagation()`
//!    就断，而且光标一移出方块就收不到事件 —— 表现就是「点完色相条再点方块
//!    没反应」「只能点一下定位，拖不动」。
//! 2. 拖拽按下的标记只在下一次无按键的移动里清，点完一块再点另一块时两块
//!    的标记同时为真，谁先谁后全看判断顺序。
//!
//! 现在按下 / 拖动 / 抬起统一注册到**窗口级**（`window.on_mouse_event`）：
//! 谁的命中区里有这个坐标就是谁，光标移出方块、甚至移出面板都照样跟手。
//! gpui 的窗口级监听只保留一帧，所以每帧都在 canvas 的 paint 里重挂一次。

use std::{cell::Cell, rc::Rc};

use gpui::prelude::*;
use gpui::{
    AnyElement, App, Bounds, DispatchPhase, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, Window, div, point, px,
};

use super::color::{hex_of, hsv_to_rgb, parse_rgb, rgb_to_hsv};
use super::theme;

/// 正在拖哪一块。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Drag {
    None,
    /// 饱和度 / 明度方块。
    Sv,
    /// 色相条。
    Hue,
}

/// 取色器状态：HSV + 色号，加上两块命中区和拖拽标记。
///
/// HSV 放在 `Rc<Cell<_>>` 里而不是普通字段：窗口级监听拿不到宿主实体，
/// 只能靠这几个格子在自己内部算完再回调宿主。
#[derive(Clone)]
pub struct Picker {
    h: Rc<Cell<f32>>,
    s: Rc<Cell<f32>>,
    v: Rc<Cell<f32>>,
    sv: Rc<Cell<Option<Bounds<Pixels>>>>,
    hue: Rc<Cell<Option<Bounds<Pixels>>>>,
    drag: Rc<Cell<Drag>>,
    /// 当前色号（`#rrggbb`）。
    pub hex: String,
    /// 正在手输 / 粘贴色号时的缓冲。
    pub edit: Option<String>,
    /// 刚点开输入框：下一次敲字 / 退格整串换掉，不用先删光七个字符。
    pub edit_fresh: bool,
}

impl Picker {
    pub fn new(h: f32, s: f32, v: f32, hex: String) -> Self {
        Self {
            h: Rc::new(Cell::new(h.rem_euclid(360.0))),
            s: Rc::new(Cell::new(s.clamp(0.0, 1.0))),
            v: Rc::new(Cell::new(v.clamp(0.0, 1.0))),
            sv: Rc::new(Cell::new(None)),
            hue: Rc::new(Cell::new(None)),
            drag: Rc::new(Cell::new(Drag::None)),
            hex,
            edit: None,
            edit_fresh: false,
        }
    }

    /// 按色号造一个；解析不了就用给定的一套 HSV。
    pub fn from_hex(hex: &str, h: f32, s: f32, v: f32) -> Self {
        match parse_rgb(hex) {
            Some(rgb) => {
                let (h, s, v) = rgb_to_hsv(rgb.0, rgb.1, rgb.2);
                Self::new(h, s, v, hex_of(rgb))
            }
            None => Self::new(h, s, v, hex.to_string()),
        }
    }

    pub fn hsv(&self) -> (f32, f32, f32) {
        (self.h.get(), self.s.get(), self.v.get())
    }

    pub fn set_hsv(&mut self, h: f32, s: f32, v: f32) {
        self.h.set(h.rem_euclid(360.0));
        self.s.set(s.clamp(0.0, 1.0));
        self.v.set(v.clamp(0.0, 1.0));
    }

    /// 色号改了：HSV 跟着换算一遍（解析不了就只记文本）。
    pub fn set_hex(&mut self, hex: String) {
        if let Some(rgb) = parse_rgb(&hex) {
            let (h, s, v) = rgb_to_hsv(rgb.0, rgb.1, rgb.2);
            self.set_hsv(h, s, v);
        }
        self.hex = hex;
    }

    /// 正在拖某一块。
    ///
    /// OSD 编辑器靠它把「拖一下」算成一次手势（撤销栈只记一步）。
    pub fn dragging(&self) -> bool {
        self.drag.get() != Drag::None
    }

    /// 结束拖拽（面板收起 / 点别处时清一下）。
    pub fn end_drag(&mut self) {
        self.drag.set(Drag::None);
    }
}

/// 两块命中区之外，还负责一件小事：「+」再点开时从哪一色起步。
///
/// 挑预设色（或切回「跟随主题」）只是把那一格换成预设，之前调好的自选色不该跟着
/// 消失 —— 再点开色轮要能接着调，而不是从刚点的那个预设色重新开始。
///
/// `presets` 是这一格的预设表（空串 = 跟随），`current` 是这格现在的值，
/// `remembered` 是上次在这格调出来的色号。返回 `Some` = 用这个当起点。
pub fn resume_value(presets: &[&str], current: &str, remembered: Option<&str>) -> Option<String> {
    let remembered = remembered?;
    if remembered.is_empty() || remembered.eq_ignore_ascii_case(current) {
        return None;
    }
    // 只有「现在摆的不是自选色」才翻旧账：当前值本身是自选色时以它为准。
    let is_preset = current.is_empty()
        || presets
            .iter()
            .any(|preset| !preset.is_empty() && preset.eq_ignore_ascii_case(current));
    is_preset.then(|| remembered.to_string())
}

/// 这个色值该不该记进「上次调的自选色」。
///
/// 预设色只是拿来比较的：点一下预设就得把调了半天的自选色记住，
/// 而不是把记忆换成预设 —— 否则再点「+」打开的又是预设色。
pub fn remembers_value(presets: &[&str], value: &str) -> bool {
    !value.is_empty()
        && !presets
            .iter()
            .any(|preset| !preset.is_empty() && preset.eq_ignore_ascii_case(value))
}

/// 饱和度 / 明度方块：渐变、十字圆钮、按下与拖动。
///
/// `on_press` 在按下的那一瞬间回调（宿主拿它抢焦点 / 拦住事件冒泡），
/// `on_change` 在按下与每一帧拖动时回调（h, s, v）。
pub fn sv_box(
    id: &str,
    picker: &Picker,
    width: f32,
    height: f32,
    on_press: Rc<dyn Fn(&mut Window, &mut App)>,
    on_change: Rc<dyn Fn(f32, f32, f32, &mut Window, &mut App)>,
) -> AnyElement {
    let hue = picker.h.clone();
    let sat_cell = picker.s.clone();
    let val_cell = picker.v.clone();
    let store = picker.sv.clone();
    let drag = picker.drag.clone();
    let (sat, val) = (picker.s.get(), picker.v.get());

    let paint_hue = hue.clone();
    let canvas = gpui::canvas(
        move |bounds, _, _| store.set(Some(bounds)),
        move |bounds, _, window, _| {
            paint_sv(window, bounds, paint_hue.get());
            register_drag(
                bounds,
                Drag::Sv,
                hue.clone(),
                sat_cell.clone(),
                val_cell.clone(),
                drag.clone(),
                on_press.clone(),
                on_change.clone(),
                window,
            );
        },
    )
    .absolute()
    .left_0()
    .top_0()
    .size_full();

    div()
        .id(gpui::SharedString::from(format!("{id}-sv")))
        .relative()
        .w(px(width))
        .h(px(height))
        .rounded(px(4.))
        .overflow_hidden()
        .cursor_pointer()
        .child(canvas)
        .child(
            div()
                .absolute()
                .left(px((sat * width - 5.0).clamp(0.0, width - 10.0)))
                .top(px(((1.0 - val) * height - 5.0).clamp(0.0, height - 10.0)))
                .w(px(10.))
                .h(px(10.))
                .rounded(px(5.))
                .border_2()
                .border_color(theme::text_primary()),
        )
        .into_any_element()
}

/// 色相条：色带、竖线游标、按下与拖动。
pub fn hue_box(
    id: &str,
    picker: &Picker,
    width: f32,
    height: f32,
    on_press: Rc<dyn Fn(&mut Window, &mut App)>,
    on_change: Rc<dyn Fn(f32, f32, f32, &mut Window, &mut App)>,
) -> AnyElement {
    let hue = picker.h.clone();
    let sat_cell = picker.s.clone();
    let val_cell = picker.v.clone();
    let store = picker.hue.clone();
    let drag = picker.drag.clone();
    let marker = hue.get();

    let canvas = gpui::canvas(
        move |bounds, _, _| store.set(Some(bounds)),
        move |bounds, _, window, _| {
            paint_hue(window, bounds);
            register_drag(
                bounds,
                Drag::Hue,
                hue.clone(),
                sat_cell.clone(),
                val_cell.clone(),
                drag.clone(),
                on_press.clone(),
                on_change.clone(),
                window,
            );
        },
    )
    .absolute()
    .left_0()
    .top_0()
    .size_full();

    div()
        .id(gpui::SharedString::from(format!("{id}-hue")))
        .relative()
        .w(px(width))
        .h(px(height))
        .rounded(px(3.))
        .overflow_hidden()
        .cursor_pointer()
        .child(canvas)
        .child(
            div()
                .absolute()
                .left(px((marker / 360.0 * width - 1.5).clamp(0.0, width - 3.0)))
                .top(px(-2.))
                .w(px(3.))
                .h(px(height + 4.))
                .rounded(px(1.5))
                .bg(theme::text_primary()),
        )
        .into_any_element()
}

/// 方块里按坐标算 (s, v)。
fn hit_sv(bounds: &Bounds<Pixels>, position: gpui::Point<Pixels>) -> (f32, f32) {
    let width = bounds.size.width.as_f32().max(1.0);
    let height = bounds.size.height.as_f32().max(1.0);
    let s = ((position.x.as_f32() - bounds.origin.x.as_f32()) / width).clamp(0.0, 1.0);
    let v = 1.0 - ((position.y.as_f32() - bounds.origin.y.as_f32()) / height).clamp(0.0, 1.0);
    (s, v)
}

/// 色相条里按坐标算 h（0..360）。
fn hit_hue(bounds: &Bounds<Pixels>, position: gpui::Point<Pixels>) -> f32 {
    let width = bounds.size.width.as_f32().max(1.0);
    ((position.x.as_f32() - bounds.origin.x.as_f32()) / width).clamp(0.0, 1.0) * 360.0
}

/// 把这一格的按下 / 拖动 / 抬起挂到窗口上（每帧重挂，gpui 只保留一帧）。
#[allow(clippy::too_many_arguments)]
fn register_drag(
    bounds: Bounds<Pixels>,
    kind: Drag,
    hue: Rc<Cell<f32>>,
    sat: Rc<Cell<f32>>,
    val: Rc<Cell<f32>>,
    drag: Rc<Cell<Drag>>,
    on_press: Rc<dyn Fn(&mut Window, &mut App)>,
    on_change: Rc<dyn Fn(f32, f32, f32, &mut Window, &mut App)>,
    window: &mut Window,
) {
    // 按下：落在自己身上才开始拖。两块各注册一条，谁的命中区里有坐标就是谁 ——
    // 这样「点完色相条再点方块」不会因为上一次的标记没清而失灵。
    window.on_mouse_event({
        let hue = hue.clone();
        let sat = sat.clone();
        let val = val.clone();
        let drag = drag.clone();
        let on_press = on_press.clone();
        let on_change = on_change.clone();
        move |event: &MouseDownEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble || event.button != MouseButton::Left {
                return;
            }
            if !bounds.contains(&event.position) {
                return;
            }
            drag.set(kind);
            on_press(window, cx);
            let (h, s, v) = match kind {
                Drag::Sv => {
                    let (s, v) = hit_sv(&bounds, event.position);
                    sat.set(s);
                    val.set(v);
                    (hue.get(), s, v)
                }
                Drag::Hue => {
                    let h = hit_hue(&bounds, event.position);
                    hue.set(h);
                    (h, sat.get(), val.get())
                }
                Drag::None => return,
            };
            on_change(h, s, v, window, cx);
            window.refresh();
        }
    });

    // 拖动：光标移出方块也继续跟手（窗口级监听收得到）。
    window.on_mouse_event({
        let hue = hue.clone();
        let sat = sat.clone();
        let val = val.clone();
        let drag = drag.clone();
        let on_change = on_change.clone();
        move |event: &MouseMoveEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble || drag.get() != kind {
                return;
            }
            // 松手消息丢了（在窗口外松开）时用移动事件兜底清掉。
            if event.pressed_button != Some(MouseButton::Left) {
                drag.set(Drag::None);
                return;
            }
            let (h, s, v) = match kind {
                Drag::Sv => {
                    let (s, v) = hit_sv(&bounds, event.position);
                    sat.set(s);
                    val.set(v);
                    (hue.get(), s, v)
                }
                Drag::Hue => {
                    let h = hit_hue(&bounds, event.position);
                    hue.set(h);
                    (h, sat.get(), val.get())
                }
                Drag::None => return,
            };
            on_change(h, s, v, window, cx);
        }
    });

    // 抬起：结束这一轮的拖拽。
    window.on_mouse_event({
        let drag = drag.clone();
        move |_: &MouseUpEvent, phase, _, _| {
            if phase != DispatchPhase::Bubble {
                return;
            }
            if drag.get() == kind {
                drag.set(Drag::None);
            }
        }
    });
}

/// 画方块：32×16 格近似渐变（每格 0.6px 的余量盖住接缝）。
fn paint_sv(window: &mut Window, bounds: Bounds<Pixels>, hue: f32) {
    const NX: usize = 32;
    const NY: usize = 16;
    let cell_w = bounds.size.width.as_f32() / NX as f32;
    let cell_h = bounds.size.height.as_f32() / NY as f32;
    for i in 0..NX {
        for j in 0..NY {
            let s = (i as f32 + 0.5) / NX as f32;
            let v = 1.0 - (j as f32 + 0.5) / NY as f32;
            let (r, g, b) = hsv_to_rgb(hue, s, v);
            window.paint_quad(gpui::fill(
                Bounds::new(
                    point(
                        bounds.origin.x + px(cell_w * i as f32),
                        bounds.origin.y + px(cell_h * j as f32),
                    ),
                    gpui::size(px(cell_w + 0.6), px(cell_h + 0.6)),
                ),
                gpui::rgb_to_hsla(gpui::rgb(((r as u32) << 16) | ((g as u32) << 8) | b as u32)),
            ));
        }
    }
}

/// 画色相条：36 段饱和度满格的颜色。
fn paint_hue(window: &mut Window, bounds: Bounds<Pixels>) {
    const COUNT: usize = 36;
    let cell_w = bounds.size.width.as_f32() / COUNT as f32;
    for i in 0..COUNT {
        let (r, g, b) = hsv_to_rgb(i as f32 / COUNT as f32 * 360.0, 1.0, 1.0);
        window.paint_quad(gpui::fill(
            Bounds::new(
                point(bounds.origin.x + px(cell_w * i as f32), bounds.origin.y),
                gpui::size(px(cell_w + 0.6), bounds.size.height),
            ),
            gpui::rgb_to_hsla(gpui::rgb(((r as u32) << 16) | ((g as u32) << 8) | b as u32)),
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::{remembers_value, resume_value};

    const PRESETS: &[&str] = &["", "#151519", "#ffffff"];
    /// 「调过 → 点了预设 → 再点 +」要回到调过的那一色，而不是从预设色重来。
    #[test]
    fn resume_value_returns_to_the_last_custom_color() {
        assert_eq!(
            resume_value(PRESETS, "#151519", Some("#abcdef")).as_deref(),
            Some("#abcdef")
        );
        // 切回「跟随主题」（空串）也算离开自选色。
        assert_eq!(
            resume_value(PRESETS, "", Some("#abcdef")).as_deref(),
            Some("#abcdef")
        );
    }

    /// 当前值本来就是自己调的那一色：以它为准，别被记忆里的旧值顶掉。
    #[test]
    fn resume_value_keeps_the_current_custom_color() {
        assert_eq!(resume_value(PRESETS, "#abcdef", Some("#abcdef")), None);
        assert_eq!(resume_value(PRESETS, "#123456", Some("#abcdef")), None);
        // 没记过就没什么好翻的。
        assert_eq!(resume_value(PRESETS, "#151519", None), None);
    }

    /// 点预设色不许改写记忆：否则「调好 → 点预设 → 再点 +」翻出来的是预设色。
    #[test]
    fn remembers_value_skips_presets() {
        assert!(remembers_value(PRESETS, "#abcdef"));
        // 预设色（大小写不敏感）和空串（跟随主题）都不记。
        assert!(!remembers_value(PRESETS, "#151519"));
        assert!(!remembers_value(PRESETS, "#FFFFFF"));
        assert!(!remembers_value(PRESETS, ""));
    }
}
