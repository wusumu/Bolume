//! 面板页脚的 1:1 可视化编辑器。
//!
//! 以前是「一行一个按钮 + ↑ / ↓ / 左 / 右 / ●」那种表格式调整：看不出按钮在面板
//! 上长什么样、摆完还得回面板确认一眼。这里直接把面板底边按**实际尺寸**摆出来 ——
//! 真按钮的画法、真 14 像素内边距、真 2 像素间距 —— 拖着换位置，点右上角的 ×
//! 收起来，收起来的落到下面「未放置」那一排，点一下又能放回去。
//!
//! 拖拽和 `ui::picker` 一个路子：按下 / 移动 / 抬起全套挂到**窗口级**
//! （`window.on_mouse_event`），按坐标判定落在哪个按钮上 —— 光标拖着按钮划出
//! 面板、甚至划出设置窗，都还跟手。gpui 的窗口级监听只留一帧，所以每帧在 canvas
//! 的 paint 里重挂一次。
//!
//! 按钮矩形不自己算：每个按钮里面垫一张透明 canvas 记自己的 bounds。带文字的
//! 按钮宽度跟着面板字号变，自己算就等于把布局抄一遍，迟早抄错。

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::Rc,
};

use gpui::prelude::*;
use gpui::{
    AnyElement, App, Bounds, DispatchPhase, MouseButton, MouseDownEvent, MouseMoveEvent,
    MouseUpEvent, Pixels, Point, Window, div, point, px, size,
};

use crate::{
    config::{Config, FooterItemSpec, FooterSide, footer_spec},
    i18n::Lang,
    ui::{icon, theme},
};

/// 页脚的三段尺寸，和 `windows::flyout::footer` 一字不差：
/// 左右内边距、按钮间距、按钮高度。改那边记得同步这里，否则「1:1」就成了大概齐。
const PAD_X: f32 = 14.0;
const GAP: f32 = 2.0;
const BTN_H: f32 = 22.0;

/// 预览里「面板其余部分」那条示意带的高度：只为让这块看着是面板的底边。
const BAND_H: f32 = 22.0;

/// 竖排面板的示意宽度：两列（一列 46）+ 列间距 2 + 两侧各 8。
///
/// 竖排的宽度由列数算出来，改列数宽度就变；但页脚是右对齐的，宽度只影响它离
/// 左边多远，示意一下够用 —— 这里较真的是**页脚内部**的摆法。
const V_PANEL_W: f32 = 46.0 * 2.0 + 2.0 + 8.0 * 2.0;

/// × 的边长（画出来的大小和 `badge_rect` 算的命中区共用）。
const BADGE: f32 = 12.0;

/// 编辑器的拖拽 / 悬停 / 量到的矩形。
///
/// 存在 `Settings` 里而不是每次重画时新建：设置页一改配置就整棵重建，状态跟着
/// 重建就等于「拖到一半回来了，东西没了」。
#[derive(Clone, Default)]
pub struct FooterPreviewState {
    /// 正在拖哪个按钮。
    drag: Rc<Cell<Option<&'static str>>>,
    /// 拖到哪儿了：`(哪一组, 该组里插在第几个)`。
    slot: Rc<Cell<Option<(FooterSide, usize)>>>,
    /// 光标压在哪个按钮上（决定要不要显形那个 ×）。
    hover: Rc<Cell<Option<&'static str>>>,
    /// 面板本身的矩形，每帧在 paint 里更新。
    panel: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// 每个按钮的矩形，每帧更新；按 id 存，隐藏的按钮留着旧值也不碍事
    /// （命中时只认当前看得见的那几个）。
    rects: Rc<RefCell<HashMap<&'static str, Bounds<Pixels>>>>,
}

/// 编辑器要改配置时抛出的一件事 —— 具体怎么写进 `FooterConfig` 交给调用方
/// （那边才有 `ConfigStore`，也知道改完要刷新哪些窗口）。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum FooterEdit {
    /// 点了 ×：收起来（配置里留着，只是不显示）。
    Hide(String),
    /// 拖到某一组的某个位置：`before` = 插在谁前面，`None` = 放到这组最后。
    Place {
        id: String,
        side: FooterSide,
        before: Option<String>,
    },
    /// 从「未放置」点回来：摆到右边那组的末尾。
    Restore(String),
}

/// 面板底边的 1:1 预览。
pub fn footer_preview(
    config: &Config,
    state: &FooterPreviewState,
    lang: Lang,
    on_edit: Rc<dyn Fn(FooterEdit, &mut Window, &mut App)>,
    on_refresh: Rc<dyn Fn(&mut Window, &mut App)>,
) -> AnyElement {
    let vertical = config.panel_layout.is_vertical();
    let scale = crate::windows::flyout::panel_scale(config);

    // 当前这一帧页脚上都有谁 —— 顺序就是渲染顺序，命中判定要的也是这份。
    let mut left: Vec<&'static FooterItemSpec> = Vec::new();
    let mut right: Vec<&'static FooterItemSpec> = Vec::new();
    for item in &config.footer.items {
        if !item.visible {
            continue;
        }
        let Some(spec) = footer_spec(&item.id) else {
            continue;
        };
        // 竖排一列太窄，老规矩：只画右边那拨、且只画图标。左边那拨这时在
        // 下面的「未放置」里等着，从那儿点回来就是搬到右边。
        if vertical && item.side.is_left() {
            continue;
        }
        if item.side.is_left() {
            left.push(spec);
        } else {
            right.push(spec);
        }
    }
    let left_ids: Rc<Vec<&'static str>> = Rc::new(left.iter().map(|spec| spec.id).collect());
    let right_ids: Rc<Vec<&'static str>> = Rc::new(right.iter().map(|spec| spec.id).collect());

    let mut left_row = div().flex().items_center().gap(px(GAP));
    for spec in &left {
        left_row = left_row.child(preview_slot(spec, lang, vertical, scale, state));
    }
    let mut right_row = div().flex().items_center().gap(px(GAP));
    for spec in &right {
        right_row = right_row.child(preview_slot(spec, lang, vertical, scale, state));
    }

    // 页脚本体：和真面板一样，两拨各自保序、两头对齐（只剩一拨时贴自己那边）。
    let mut strip = div()
        .h(px(theme::FOOTER_HEIGHT))
        .px(px(PAD_X))
        .flex()
        .flex_none()
        .items_center()
        .overflow_hidden()
        .border_t_1()
        .border_color(theme::border());
    strip = if vertical {
        strip.justify_end().child(right_row)
    } else {
        match (left.is_empty(), right.is_empty()) {
            (false, false) => strip.justify_between().child(left_row).child(right_row),
            (false, true) => strip.justify_start().child(left_row),
            (true, false) => strip.justify_end().child(right_row),
            (true, true) => strip,
        }
    };

    let band = div()
        .h(px(BAND_H))
        .px(px(PAD_X))
        .flex()
        .flex_none()
        .items_center()
        .child(
            div()
                .text_size(px(10.))
                .text_color(theme::text_tertiary())
                .child(lang.pick("面板其余部分", "the rest of the panel")),
        );

    let panel_w = if vertical {
        V_PANEL_W
    } else {
        config.flyout_width()
    };

    // 落点光标：一个 2 像素的竖条，插在将要落在的那两个按钮中间。
    let caret = state.slot.get().and_then(|(side, index)| {
        let panel = state.panel.get()?;
        let rects = state.rects.borrow();
        caret_offset(side, index, &left_ids, &right_ids, &rects, &panel)
    });

    let mut panel = div()
        .id("footer-preview-panel")
        .relative()
        .w(px(panel_w))
        .flex()
        .flex_col()
        .rounded(px(theme::RADIUS_CARD))
        .bg(theme::surface_window())
        .border_1()
        .border_color(theme::border())
        .child(band)
        .child(strip);

    if let Some(x) = caret {
        panel = panel.child(
            div()
                .absolute()
                .left(px(x - 1.0))
                .bottom(px((theme::FOOTER_HEIGHT - 18.0) / 2.0))
                .w(px(2.0))
                .h(px(18.0))
                .rounded(px(1.0))
                .bg(theme::accent()),
        );
    }

    // 每帧重挂一次窗口级监听。矩形表是共享的、事件来了才读，所以这张 canvas
    // 摆在最前面也不影响——那一帧里按钮的 bounds 早就量完了。
    let listen = {
        let state = state.clone();
        let left = left_ids.clone();
        let right = right_ids.clone();
        let on_edit = on_edit.clone();
        let on_refresh = on_refresh.clone();
        gpui::canvas(
            |_, _, _| {},
            move |bounds, _, window, _| {
                state.panel.set(Some(bounds));
                register(
                    window,
                    bounds,
                    vertical,
                    &state,
                    &left,
                    &right,
                    &on_edit,
                    &on_refresh,
                );
            },
        )
        .absolute()
        .left_0()
        .top_0()
        .size_full()
    };

    // 横排的宽度就是面板宽度，能照实标出来；竖排的宽度由列数决定，这里只是
    // 拿两列示意一下（页脚右对齐，宽度只影响它离左边多远）。
    let size_label = if vertical {
        lang.pick("竖排 · 宽度示意", "Vertical · illustrative width").to_string()
    } else {
        format!(
            "{} · {}×{}",
            lang.pick("实际大小", "Actual size"),
            panel_w.round() as i32,
            theme::FOOTER_HEIGHT.round() as i32
        )
    };

    div()
        .flex()
        .flex_col()
        .gap(px(5.))
        .child(
            div()
                .flex()
                .items_center()
                .gap(px(6.))
                .px(px(2.))
                .text_size(px(10.))
                .text_color(theme::text_tertiary())
                .child(size_label)
                .child(div().flex_1().child(""))
                .child(lang.pick(
                    "拖动换位置 · 点 × 收起",
                    "drag to move · click × to remove",
                )),
        )
        .child(panel.child(listen))
        .into_any_element()
}

/// 没放上页脚的按钮：配置里隐藏的，加上竖排面板下不显示的那一组。
///
/// 点一下放回右边末尾；也可以先点回来再拖。一个都不剩时整块不画。
pub fn unplaced_tray(
    config: &Config,
    lang: Lang,
    on_edit: Rc<dyn Fn(FooterEdit, &mut Window, &mut App)>,
) -> Option<AnyElement> {
    let vertical = config.panel_layout.is_vertical();
    let mut chips: Vec<AnyElement> = Vec::new();
    for item in &config.footer.items {
        let Some(spec) = footer_spec(&item.id) else {
            continue;
        };
        let shown = item.visible && !(vertical && item.side.is_left());
        if shown {
            continue;
        }
        let id = spec.id.to_string();
        let on_edit = on_edit.clone();
        chips.push(
            div()
                .id(gpui::SharedString::from(format!("footer-tray-{id}")))
                .h(px(24.))
                .px(px(8.))
                .flex()
                .flex_none()
                .items_center()
                .gap(px(5.))
                .rounded(px(theme::RADIUS_SMALL))
                .border_1()
                .border_color(theme::border())
                .bg(theme::surface_elevated())
                .cursor_pointer()
                .hover(|style| style.bg(theme::surface_hover()))
                .on_click(move |_, window, cx| {
                    on_edit(FooterEdit::Restore(id.clone()), window, cx);
                })
                .child(icon(spec.icon, 12.0, theme::text_tertiary()))
                .child(
                    div()
                        .text_size(theme::fs(11.))
                        .text_color(theme::text_secondary())
                        .child(lang.pick(spec.zh, spec.en)),
                )
                .into_any_element(),
        );
    }
    if chips.is_empty() {
        return None;
    }
    Some(
        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(6.))
            .px(px(9.))
            .py(px(4.))
            .child(
                div()
                    .text_size(theme::fs(11.))
                    .text_color(theme::text_tertiary())
                    .child(lang.pick("未放置", "Not shown")),
            )
            .children(chips)
            .into_any_element(),
    )
}

/// 页脚上的一个按钮 + 一层「量尺寸」的透明底 + 悬停时右上角那个 ×。
fn preview_slot(
    spec: &'static FooterItemSpec,
    lang: Lang,
    compact: bool,
    scale: f32,
    state: &FooterPreviewState,
) -> AnyElement {
    let id = spec.id;
    let dragging = state.drag.get() == Some(id);
    let hovered = state.hover.get() == Some(id) && !dragging;

    let rects = state.rects.clone();
    let mut slot = div()
        .id(gpui::SharedString::from(format!("footer-preview-{id}")))
        .relative()
        .flex_none()
        .h(px(BTN_H))
        .when(!dragging, |style| style.cursor_pointer())
        .child(preview_button(spec, lang, compact, scale))
        .child(
            gpui::canvas(
                move |bounds, _, _| {
                    rects.borrow_mut().insert(id, bounds);
                },
                |_, _, _, _| {},
            )
            .absolute()
            .left_0()
            .top_0()
            .size_full(),
        );

    if dragging {
        // 拖着的那个留在原位淡下去：它占的坑还在，落点光标才有参照。
        slot = slot.opacity(0.35);
    }
    if hovered {
        slot = slot.child(
            div()
                .absolute()
                .right(px(-4.0))
                .top(px(-4.0))
                .size(px(BADGE))
                .flex()
                .items_center()
                .justify_center()
                .rounded_full()
                .bg(theme::surface_elevated())
                .border_1()
                .border_color(theme::border_strong())
                .child(icon("close", 7.0, theme::text_secondary())),
        );
    }
    slot.into_any_element()
}

/// 按钮的**静态**长相：尺寸和 `flyout::footer_button` 一样，但一个 handler 都不挂 ——
/// 点、拖全交给窗口级监听按坐标判。
fn preview_button(
    spec: &'static FooterItemSpec,
    lang: Lang,
    compact: bool,
    scale: f32,
) -> AnyElement {
    let color = theme::text_tertiary();
    match spec.id {
        // WiFi / 蓝牙是「双按钮」：左半开关、中间一条竖线、右半展开箭头。
        "wifi" | "bluetooth" => div()
            .h(px(BTN_H))
            .flex()
            .flex_none()
            .items_center()
            .rounded(px(theme::RADIUS_SMALL))
            .overflow_hidden()
            .child(
                div()
                    .h_full()
                    .w(px(BTN_H))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(icon(spec.icon, 13.0, color)),
            )
            .child(
                div()
                    .w(px(1.))
                    .h(px(12.))
                    .flex_none()
                    .bg(theme::border()),
            )
            .child(
                div()
                    .h_full()
                    .w(px(16.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(icon("chevron-down", 11.0, theme::text_tertiary())),
            )
            .into_any_element(),
        _ => {
            let mut button = div()
                .h(px(BTN_H))
                .flex()
                .flex_none()
                .items_center()
                .justify_center()
                .rounded(px(theme::RADIUS_SMALL))
                .child(icon(spec.icon, 13.0, color));
            if spec.label && !compact {
                button = button.px(px(8.0)).gap(px(5.0)).child(
                    div()
                        .text_size(px(11.0 * scale))
                        .text_color(color)
                        .child(lang.pick(spec.zh, spec.en)),
                );
            } else {
                button = button.w(px(BTN_H));
            }
            button.into_any_element()
        }
    }
}

/// 把这一帧的按下 / 移动 / 抬起挂到窗口上（gpui 只留一帧，所以每帧重挂）。
#[allow(clippy::too_many_arguments)]
fn register(
    window: &mut Window,
    panel: Bounds<Pixels>,
    vertical: bool,
    state: &FooterPreviewState,
    left: &Rc<Vec<&'static str>>,
    right: &Rc<Vec<&'static str>>,
    on_edit: &Rc<dyn Fn(FooterEdit, &mut Window, &mut App)>,
    on_refresh: &Rc<dyn Fn(&mut Window, &mut App)>,
) {
    // 按下：按坐标认按钮（先认那个 ×），然后开拖。
    window.on_mouse_event({
        let state = state.clone();
        let left = left.clone();
        let right = right.clone();
        let on_edit = on_edit.clone();
        let on_refresh = on_refresh.clone();
        move |event: &MouseDownEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble || event.button != MouseButton::Left {
                return;
            }
            if !panel.contains(&event.position) {
                return;
            }
            let hit = {
                let rects = state.rects.borrow();
                hit_button(event.position, &left, &right, &rects).map(|id| {
                    let badge = rects.get(id).map(badge_rect);
                    (id, badge)
                })
            };
            let Some((id, badge)) = hit else {
                return;
            };
            // × 只在悬停时显形，命中区也只在它显形时算数：不然按钮右上角
            // 那一小块会变成「点一下就没了」的陷阱。
            if state.hover.get() == Some(id) && badge.is_some_and(|b| b.contains(&event.position)) {
                state.hover.set(None);
                state.drag.set(None);
                state.slot.set(None);
                on_edit(FooterEdit::Hide(id.to_string()), window, cx);
                return;
            }
            state.drag.set(Some(id));
            let slot = {
                let rects = state.rects.borrow();
                slot_at(
                    event.position.x.as_f32(),
                    vertical,
                    &panel,
                    &left,
                    &right,
                    &rects,
                )
            };
            state.slot.set(Some(slot));
            on_refresh(window, cx);
        }
    });

    // 移动：拖着的时候算落点；没拖的时候只看悬停在谁身上。
    window.on_mouse_event({
        let state = state.clone();
        let left = left.clone();
        let right = right.clone();
        let on_edit = on_edit.clone();
        let on_refresh = on_refresh.clone();
        move |event: &MouseMoveEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble {
                return;
            }
            if state.drag.get().is_some() {
                // 在窗口外面松手收不到抬起事件：移动里兜一下。
                if event.pressed_button != Some(MouseButton::Left) {
                    settle(window, cx, &state, &left, &right, &on_edit, &on_refresh);
                    return;
                }
                let slot = {
                    let rects = state.rects.borrow();
                    slot_at(
                        event.position.x.as_f32(),
                        vertical,
                        &panel,
                        &left,
                        &right,
                        &rects,
                    )
                };
                if state.slot.get() != Some(slot) {
                    state.slot.set(Some(slot));
                    on_refresh(window, cx);
                }
                return;
            }
            let hovered = if panel.contains(&event.position) {
                let rects = state.rects.borrow();
                hit_button(event.position, &left, &right, &rects)
            } else {
                None
            };
            if state.hover.get() != hovered {
                state.hover.set(hovered);
                on_refresh(window, cx);
            }
        }
    });

    // 抬起：落位。
    window.on_mouse_event({
        let state = state.clone();
        let left = left.clone();
        let right = right.clone();
        let on_edit = on_edit.clone();
        let on_refresh = on_refresh.clone();
        move |_: &MouseUpEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble {
                return;
            }
            settle(window, cx, &state, &left, &right, &on_edit, &on_refresh);
        }
    });
}

/// 收尾：把拖着的按钮按当前落点写进配置。
#[allow(clippy::too_many_arguments)]
fn settle(
    window: &mut Window,
    cx: &mut App,
    state: &FooterPreviewState,
    left: &[&'static str],
    right: &[&'static str],
    on_edit: &Rc<dyn Fn(FooterEdit, &mut Window, &mut App)>,
    on_refresh: &Rc<dyn Fn(&mut Window, &mut App)>,
) {
    let Some(id) = state.drag.take() else {
        return;
    };
    let slot = state.slot.take();
    // 先把淡下去的那个和落点光标收回来（落点写进配置后整棵会重建，这一下是为了
    // 「拖回原处」这种空操作也能立刻恢复原样）。
    on_refresh(window, cx);
    let Some((side, index)) = slot else {
        return;
    };
    let ids = if side.is_left() { left } else { right };
    let before = ids.get(index).map(|id| id.to_string());
    on_edit(
        FooterEdit::Place {
            id: id.to_string(),
            side,
            before,
        },
        window,
        cx,
    );
}

/// 坐标落在哪个按钮上。
fn hit_button(
    pos: Point<Pixels>,
    left: &[&'static str],
    right: &[&'static str],
    rects: &HashMap<&'static str, Bounds<Pixels>>,
) -> Option<&'static str> {
    left.iter()
        .chain(right.iter())
        .find(|id| rects.get(*id).is_some_and(|b| b.contains(&pos)))
        .copied()
}

/// × 的命中区：和画出来的位置一样（按钮右上角往外各 4 像素）。
fn badge_rect(button: &Bounds<Pixels>) -> Bounds<Pixels> {
    let right = button.origin.x.as_f32() + button.size.width.as_f32() + 4.0;
    Bounds::new(
        point(px(right - BADGE), px(button.origin.y.as_f32() - 4.0)),
        size(px(BADGE), px(BADGE)),
    )
}

/// 光标横坐标落在「哪一组、组里第几个」。
///
/// 分组按两个参照分：两拨都在时取它们中间的空档，只剩一拨时取面板中线 ——
/// 这样把最后一个按钮拖到空荡荡的另一半，就是搬到对面去。
fn slot_at(
    x: f32,
    vertical: bool,
    panel: &Bounds<Pixels>,
    left: &[&'static str],
    right: &[&'static str],
    rects: &HashMap<&'static str, Bounds<Pixels>>,
) -> (FooterSide, usize) {
    let span = |id: &'static str| {
        rects
            .get(id)
            .map(|b| (b.origin.x.as_f32(), b.origin.x.as_f32() + b.size.width.as_f32()))
    };
    let divider = match (
        left.last().and_then(|id| span(id)),
        right.first().and_then(|id| span(id)),
    ) {
        (Some((_, left_right)), Some((right_left, _))) => (left_right + right_left) / 2.0,
        _ => panel.origin.x.as_f32() + panel.size.width.as_f32() / 2.0,
    };
    // 竖排只有一组，落在哪儿都是右边那组。
    let side = if !vertical && x < divider {
        FooterSide::Left
    } else {
        FooterSide::Right
    };
    let ids = if side.is_left() { left } else { right };
    let mut index = ids.len();
    for (i, id) in ids.iter().enumerate() {
        if let Some((item_left, item_right)) = span(id) {
            if x < (item_left + item_right) / 2.0 {
                index = i;
                break;
            }
        }
    }
    (side, index)
}

/// 落点竖条在面板里的横向位置（相对面板左边）。
///
/// 插在第 0 个前面 = 第一个按钮的左边；插在最后 = 最后一个按钮的右边。
fn caret_offset(
    side: FooterSide,
    index: usize,
    left: &[&'static str],
    right: &[&'static str],
    rects: &HashMap<&'static str, Bounds<Pixels>>,
    panel: &Bounds<Pixels>,
) -> Option<f32> {
    let ids = if side.is_left() { left } else { right };
    let last = *ids.last()?;
    let edge = if index >= ids.len() {
        let b = rects.get(&last)?;
        b.origin.x.as_f32() + b.size.width.as_f32()
    } else {
        let b = rects.get(&ids[index])?;
        b.origin.x.as_f32()
    };
    Some(edge - panel.origin.x.as_f32())
}

#[cfg(test)]
mod tests {
    use super::{slot_at, FooterSide};
    use gpui::{Bounds, Pixels, point, px, size};
    use std::collections::HashMap;

    /// 三个按钮摆在面板底边：左边两个（各 22 宽，间距 2），右边一个（22 宽）。
    ///
    /// 面板 300 宽、左边距 14：左组占 14..36、38..60，右组占 264..286。
    fn layout() -> (Bounds<Pixels>, HashMap<&'static str, Bounds<Pixels>>) {
        let panel = Bounds::new(point(px(0.), px(0.)), size(px(300.), px(36.)));
        let mut rects = HashMap::new();
        rects.insert("wifi", Bounds::new(point(px(14.), px(7.)), size(px(22.), px(22.))));
        rects.insert("bluetooth", Bounds::new(point(px(38.), px(7.)), size(px(22.), px(22.))));
        rects.insert("pin", Bounds::new(point(px(264.), px(7.)), size(px(22.), px(22.))));
        (panel, rects)
    }

    /// 两拨之间那条空档的中点是分界线：落在左半边 = 左组（插在最后一个后面），
    /// 越过它 = 右组。
    #[test]
    fn slot_follows_the_gap_between_groups() {
        let (panel, rects) = layout();
        let left: Vec<&'static str> = vec!["wifi", "bluetooth"];
        let right: Vec<&'static str> = vec!["pin"];
        // 蓝牙右半边：插在蓝牙后面（左组第 2 个位置）。
        assert_eq!(
            slot_at(55.0, false, &panel, &left, &right, &rects),
            (FooterSide::Left, 2)
        );
        // 蓝牙左半边：插在它前面。
        assert_eq!(
            slot_at(40.0, false, &panel, &left, &right, &rects),
            (FooterSide::Left, 1)
        );
        // 空档（60..264）中间是分界线：靠左还是左组，过了才算右组。
        assert_eq!(
            slot_at(150.0, false, &panel, &left, &right, &rects),
            (FooterSide::Left, 2)
        );
        assert_eq!(
            slot_at(200.0, false, &panel, &left, &right, &rects),
            (FooterSide::Right, 0)
        );
        // 比右边那个还右：右组末尾。
        assert_eq!(
            slot_at(290.0, false, &panel, &left, &right, &rects),
            (FooterSide::Right, 1)
        );
    }

    /// 只剩一拨时按面板中线分：拖到空荡荡的另一半就是搬到对面去。
    #[test]
    fn slot_uses_panel_middle_when_one_group_is_empty() {
        let (panel, rects) = layout();
        let left: Vec<&'static str> = Vec::new();
        let right: Vec<&'static str> = vec!["pin"];
        assert_eq!(
            slot_at(20.0, false, &panel, &left, &right, &rects),
            (FooterSide::Left, 0)
        );
        assert_eq!(
            slot_at(280.0, false, &panel, &left, &right, &rects),
            (FooterSide::Right, 1)
        );
    }

    /// 竖排只有一个位置：落在哪儿都是右边那组。
    #[test]
    fn slot_is_always_right_when_vertical() {
        let (panel, rects) = layout();
        let left: Vec<&'static str> = vec!["wifi", "bluetooth"];
        let right: Vec<&'static str> = vec!["pin"];
        assert_eq!(
            slot_at(20.0, true, &panel, &left, &right, &rects),
            (FooterSide::Right, 0)
        );
        assert_eq!(
            slot_at(290.0, true, &panel, &left, &right, &rects),
            (FooterSide::Right, 1)
        );
    }
}
