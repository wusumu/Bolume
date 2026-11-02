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
    MouseUpEvent, Pixels, Window, div, point, px, relative,
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
    /// 正在手输 / 粘贴色号时的编辑状态（光标、选区、进编辑前的值都在里面）。
    pub edit: Option<HexEdit>,
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

/// 色号输入框（单行）的编辑状态。
///
/// 原来的框只能整串换：一打字就把七个字符清空重来，粘贴完还非得回车才生效，
/// 想改中间一位得删到那儿再重打。现在带光标和选区 —— 点哪站哪、按住拖一段、
/// `Ctrl+A / C / X / V`，右键还能弹菜单。
///
/// 索引全按**字符**算（不是字节）：色号只有 ASCII，但和文本编辑那边保持同一套
/// 口径，省得以后支持别的写法时又要改。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HexEdit {
    /// 框里现在的字（已按色号字符过滤过）。
    pub text: String,
    /// 光标在第几个字符之后（0..=字符数）。
    pub caret: usize,
    /// 选区另一头。和 `caret` 不等就是有选区，谁前谁后都行。
    pub anchor: usize,
    /// 这次左键是在框里按下的：只有它开始的拖动才改选区，
    /// 从别处（色块、滚动条）拖着路过时不该把选区带走。
    pub drag: bool,
    /// 刚点开：下一次打字 / 粘贴整串换掉，不用先删七个字符。
    pub fresh: bool,
}

impl HexEdit {
    pub fn new(value: &str) -> Self {
        let text = value.to_string();
        let end = text.chars().count();
        Self {
            text,
            caret: end,
            anchor: end,
            drag: false,
            fresh: true,
        }
    }

    pub fn len(&self) -> usize {
        self.text.chars().count()
    }

    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }

    /// 选区（前, 后）；没选中是 `None`。
    pub fn selection(&self) -> Option<(usize, usize)> {
        let caret = self.caret.min(self.len());
        let anchor = self.anchor.min(self.len());
        (anchor != caret).then(|| if anchor < caret {
            (anchor, caret)
        } else {
            (caret, anchor)
        })
    }

    pub fn selected_text(&self) -> Option<String> {
        let (start, end) = self.selection()?;
        Some(self.text.chars().skip(start).take(end - start).collect())
    }

    pub fn select_all(&mut self) {
        self.anchor = 0;
        self.caret = self.len();
        self.fresh = false;
    }

    /// 把光标挪到某处；`extend` = 拖着锚点走（Shift+点击 / 拖动选择）。
    pub fn move_to(&mut self, to: usize, extend: bool) {
        let to = to.min(self.len());
        self.caret = to;
        if !extend {
            self.anchor = to;
        }
        self.fresh = false;
    }

    pub fn left(&mut self, extend: bool) {
        let to = match (self.selection(), extend) {
            (Some((start, _)), false) => start,
            _ => self.caret.saturating_sub(1),
        };
        self.move_to(to, extend);
    }

    pub fn right(&mut self, extend: bool) {
        let to = match (self.selection(), extend) {
            (Some((_, end)), false) => end,
            _ => (self.caret + 1).min(self.len()),
        };
        self.move_to(to, extend);
    }

    pub fn home(&mut self, extend: bool) {
        self.move_to(0, extend);
    }

    pub fn end(&mut self, extend: bool) {
        self.move_to(self.len(), extend);
    }

    /// 插入一段文字（先删选区）。
    ///
    /// 只收 `0-9a-f`，`#` 只在最前面才有意义，整串最多七个字符 ——
    /// 和原来 `push_hex` 的规矩一样，只是现在能插在中间。
    pub fn insert(&mut self, text: &str) {
        let (start, end) = self
            .selection()
            .unwrap_or((self.caret.min(self.len()), self.caret.min(self.len())));
        let mut chars: Vec<char> = self.text.chars().collect();
        chars.drain(start..end);
        self.caret = start;
        for ch in text.chars() {
            let accept = ch.is_ascii_hexdigit() || (ch == '#' && chars.is_empty());
            if !accept {
                continue;
            }
            chars.insert(self.caret, ch);
            self.caret += 1;
        }
        // 整串选中后直接打 `ff0000`，第一个字符就把 `#` 顶掉了 —— 补回来，
        // 框里始终是看得惯的 `#rrggbb`（粘贴进来的也一视同仁）。
        if !chars.is_empty() && chars[0] != '#' {
            chars.insert(0, '#');
            self.caret += 1;
        }
        // 超长的从**末尾**截断：这样在中间插字时挤掉的是最后一位，
        // 而不是像以前那样「满了就整段不认」。
        chars.truncate(7);
        self.caret = self.caret.min(chars.len());
        self.text = chars.into_iter().collect();
        self.anchor = self.caret;
        self.fresh = false;
    }

    /// 退格：有选区就删选区，否则删光标前一个字符。
    pub fn backspace(&mut self) -> bool {
        if let Some((start, end)) = self.selection() {
            self.cut_range(start, end);
            return true;
        }
        if self.caret == 0 {
            return false;
        }
        self.cut_range(self.caret - 1, self.caret);
        true
    }

    /// Delete：有选区就删选区，否则删光标后一个字符。
    pub fn delete(&mut self) -> bool {
        if let Some((start, end)) = self.selection() {
            self.cut_range(start, end);
            return true;
        }
        if self.caret >= self.len() {
            return false;
        }
        self.cut_range(self.caret, self.caret + 1);
        true
    }

    fn cut_range(&mut self, start: usize, end: usize) {
        let mut chars: Vec<char> = self.text.chars().collect();
        let end = end.min(chars.len());
        let start = start.min(end);
        chars.drain(start..end);
        self.text = chars.into_iter().collect();
        self.caret = start;
        self.anchor = start;
        self.fresh = false;
    }
}

/// 色号输入框的共用回调。
///
/// 两个宿主（设置页的色轮弹层、OSD 编辑器的颜色行）都只是「拿到事件改自己的状态」，
/// 所以这里给一组闭包，别把整套文本编辑各写一遍 —— 上一次两处各写一份，
/// 结果就是「这边能粘贴、那边只能打字」。
pub struct HexHandlers {
    /// 鼠标点到第几个字符（`extend` = 按住 Shift）。
    pub press: Rc<dyn Fn(usize, bool, &mut Window, &mut App)>,
    /// 按住拖动：光标跟到第几个字符。
    pub drag: Rc<dyn Fn(usize, &mut Window, &mut App)>,
    /// 松手：结束这次拖动。
    pub release: Rc<dyn Fn(&mut App)>,
    /// 右键：在（窗口坐标）处弹菜单。
    pub context: Rc<dyn Fn(f32, f32, &mut Window, &mut App)>,
}

/// 单行色号输入框：逐字铺块 + 左右半区命中 + 选区底色 + 光标。
///
/// 沿用设置页多行模板编辑器那一套（`tooltip_editor`）：每个字自己带两个透明命中区，
/// 左半 = 光标在这个字前面、右半 = 后面，所以「点哪站哪」不需要字体度量，
/// 拖动选择也不会因为字宽对不齐而漂。
#[allow(clippy::too_many_arguments)]
pub fn hex_field(
    prefix: &'static str,
    edit: &HexEdit,
    font_size: f32,
    height: f32,
    handlers: HexHandlers,
) -> AnyElement {
    let chars: Vec<char> = edit.text.chars().collect();
    let len = chars.len();
    let caret = edit.caret.min(len);
    let selection = edit.selection();
    let HexHandlers {
        press,
        drag,
        release,
        context,
    } = handlers;

    let caret_bar = || {
        div()
            .w(px(1.5))
            .h(px(font_size + 2.0))
            .flex_none()
            .bg(theme::accent())
            .into_any_element()
    };

    let mut row = div()
        .id(gpui::SharedString::from(format!("{prefix}-hex-row")))
        .relative()
        .flex()
        .items_center()
        .h(px(height))
        .pl(px(6.))
        .text_size(px(font_size))
        .text_color(theme::text_primary())
        .cursor_text();

    // 这一行不再垫「整行兜底命中区」。gpui 的鼠标事件不是最上面独占：按绘制
    // 倒序过所有 listener，垫在字底下的那块会**后到**，每次都把光标打回 0 ——
    // 点哪儿都变成「光标落到最前面」，拖拽选区也会被它一路抹掉。
    // 空白与行尾由下面的 `hex-tail` 负责（`flex_1` 把剩下的宽度全接下来），
    // 空框时它自然铺满整行。

    for (index, ch) in chars.iter().enumerate() {
        if caret == index {
            row = row.child(caret_bar());
        }
        let selected = selection.is_some_and(|(start, end)| start <= index && index < end);
        let mut glyph = div()
            .id(gpui::SharedString::from(format!("{prefix}-hex-{index}")))
            .relative()
            .flex_none()
            .flex()
            .items_center()
            .h(px(height))
            .when(selected, |style| style.bg(theme::accent_glow()))
            .child(ch.to_string());

        for right in [false, true] {
            let to_col = index + usize::from(right);
            let press = press.clone();
            let drag = drag.clone();
            let context = context.clone();
            glyph = glyph.child(
                div()
                    .id(gpui::SharedString::from(format!(
                        "{prefix}-hex-hit-{index}-{}",
                        u8::from(right)
                    )))
                    .absolute()
                    .top_0()
                    .bottom_0()
                    .left(relative(if right { 0.5 } else { 0.0 }))
                    .w(relative(0.5))
                    .cursor_text()
                    .on_mouse_down(MouseButton::Left, move |event, window, cx| {
                        press(to_col, event.modifiers.shift, window, cx)
                    })
                    .on_mouse_move(move |event, window, cx| {
                        if event.pressed_button == Some(MouseButton::Left) {
                            drag(to_col, window, cx);
                        }
                    })
                    .on_mouse_down(MouseButton::Right, {
                        let ctx = context.clone();
                        move |event, window, cx| {
                            ctx(event.position.x.as_f32(), event.position.y.as_f32(), window, cx)
                        }
                    })
                    .on_aux_click({
                        let ctx = context.clone();
                        move |event, window, cx| {
                            let pos = event.position();
                            ctx(pos.x.as_f32(), pos.y.as_f32(), window, cx)
                        }
                    }),
            );
        }
        row = row.child(glyph);
    }
    if caret == len {
        row = row.child(caret_bar());
    }

    // 行尾留白：点这里等于按 End，也负责「框比字宽」时那一段的点击。
    let tail_press = press.clone();
    let tail_drag = drag.clone();
    let tail_ctx = context.clone();
    row = row.child(
        div()
            .id(gpui::SharedString::from(format!("{prefix}-hex-tail")))
            .flex_1()
            .min_w(px(0.))
            .h(px(height))
            .when(
                selection.is_some_and(|(start, end)| start <= len && len < end),
                |style| style.bg(theme::accent_glow()),
            )
            .on_mouse_down(MouseButton::Left, move |event, window, cx| {
                tail_press(len, event.modifiers.shift, window, cx)
            })
            .on_mouse_move(move |event, window, cx| {
                if event.pressed_button == Some(MouseButton::Left) {
                    tail_drag(len, window, cx);
                }
            })
            .on_mouse_down(MouseButton::Right, {
                let ctx = tail_ctx.clone();
                move |event, window, cx| {
                    ctx(event.position.x.as_f32(), event.position.y.as_f32(), window, cx)
                }
            })
            .on_aux_click({
                let ctx = tail_ctx.clone();
                move |event, window, cx| {
                    let pos = event.position();
                    ctx(pos.x.as_f32(), pos.y.as_f32(), window, cx)
                }
            }),
    );

    let release = release.clone();
    row.on_mouse_up(MouseButton::Left, move |_event, _window, cx| {
        release(cx);
    })
    .into_any_element()
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
    use super::{HexEdit, remembers_value, resume_value};

    /// 光标 / 选区：点哪站哪、拖到哪选到哪、退格删的是选区。
    #[test]
    fn hex_edit_selects_and_deletes_like_a_text_box() {
        let mut edit = HexEdit::new("#7c9cff");
        assert_eq!(edit.caret, 7, "进编辑时光标停在末尾");
        assert_eq!(edit.selection(), None);

        // 拖选：锚点留在 1，光标拖到 3 —— 选中中间两位。
        edit.move_to(1, false);
        edit.move_to(3, true);
        assert_eq!(edit.selection(), Some((1, 3)));
        assert_eq!(edit.selected_text().as_deref(), Some("7c"));

        // 退格删掉选区，不是删一个字符。
        assert!(edit.backspace());
        assert_eq!(edit.text, "#9cff");
        assert_eq!(edit.caret, 1);
        assert_eq!(edit.selection(), None);

        // 全选之后打字整串换掉。
        edit.select_all();
        edit.insert("#FF8800");
        // 敲进去什么就存什么（大小写不替他改），解析色号时本来就不分大小写。
        assert_eq!(edit.text, "#FF8800");
    }

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
