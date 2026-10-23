//! 可拖动的竖向滚动条。
//!
//! gpui 这一版不画滚动条（`overflow_y_scroll` 只认滚轮），所以轨道和滑块都自己画，
//! 鼠标交互也自己收。和音量滑条同一套做法：**监听必须在 paint 阶段注册**
//! （`Window::on_mouse_event` 只接受绘制期间追加的监听），否则会随 render 次数叠加。

use std::{cell::RefCell, rc::Rc};

use gpui::{
    App, Background, BorderStyle, Bounds, Corners, DispatchPhase, Edges, Element, ElementId,
    GlobalElementId, Hsla, InspectorElementId, IntoElement, LayoutId, MouseButton, MouseDownEvent,
    MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, ScrollHandle, Style, Window, point, px,
    relative, size, transparent_black,
};

/// 滑块最小高度：再短就抓不住了。
const MIN_THUMB: f32 = 28.0;

/// 点轨道空白处翻一屏的比例（Windows 滚动条就是这个手感）。
const PAGE_RATIO: f32 = 0.9;

/// 拖动状态。同一时刻只可能拖一根滚动条，所以一份就够，用 `token` 区分是哪根。
#[derive(Clone, Default)]
pub struct ScrollDragState {
    inner: Rc<RefCell<Option<DragInfo>>>,
}

struct DragInfo {
    token: u64,
    /// 按下时鼠标的窗口坐标 y。
    start_y: f32,
    /// 按下时的滚动量（正数，越小越靠上）。
    start_scrolled: f32,
    /// 滑块可走的距离（视口高 − 滑块高）。
    travel: f32,
    max: f32,
}

impl ScrollDragState {
    pub fn new() -> Self {
        Self::default()
    }

    fn begin(&self, info: DragInfo) {
        *self.inner.borrow_mut() = Some(info);
    }

    fn end(&self, token: u64) {
        let mut slot = self.inner.borrow_mut();
        if slot.as_ref().is_some_and(|info| info.token == token) {
            *slot = None;
        }
    }

    fn is_active(&self, token: u64) -> bool {
        self.inner
            .borrow()
            .as_ref()
            .is_some_and(|info| info.token == token)
    }

    /// 拖动中用到的四个数，拷出来避免和后面的 `set_offset` 抢借用。
    fn drag_snapshot(&self, token: u64) -> Option<(f32, f32, f32, f32)> {
        self.inner
            .borrow()
            .as_ref()
            .filter(|info| info.token == token)
            .map(|info| (info.start_y, info.start_scrolled, info.travel, info.max))
    }
}

/// 滚动条的几何：视口高、滑块高、滑块顶部偏移、可走距离、最大滚动量。
struct Metrics {
    viewport: f32,
    thumb_height: f32,
    thumb_top: f32,
    travel: f32,
    max: f32,
}

fn metrics(scroll: &ScrollHandle) -> Option<Metrics> {
    let viewport = scroll.bounds().size.height.as_f32();
    let max = scroll.max_offset().y.as_f32();
    // 布局还没跑过（首帧 bounds 是空的）或内容装得下：不画条。
    if viewport <= 1.0 || max <= 1.0 {
        return None;
    }

    let content = viewport + max;
    let thumb_height = (viewport / content * viewport).clamp(MIN_THUMB, viewport);
    let travel = (viewport - thumb_height).max(0.0);
    let scrolled = (-scroll.offset().y.as_f32()).clamp(0.0, max);
    let thumb_top = scrolled / max * travel;
    Some(Metrics {
        viewport,
        thumb_height,
        thumb_top,
        travel,
        max,
    })
}

/// 竖向滚动条：占满父容器（含 12px 的槽），轨道画在槽正中。
pub struct Scrollbar {
    id: ElementId,
    token: u64,
    scroll: ScrollHandle,
    drag: ScrollDragState,
    /// 轨道（滑块）宽度。
    width: f32,
    /// 能不能交互。浮层打开时，下面那根要停掉，否则点在遮罩上会穿过去。
    interactive: bool,
}

impl Scrollbar {
    pub fn new(
        id: impl Into<ElementId>,
        token: u64,
        scroll: ScrollHandle,
        drag: ScrollDragState,
        width: f32,
        interactive: bool,
    ) -> Self {
        Self {
            id: id.into(),
            token,
            scroll,
            drag,
            width,
            interactive,
        }
    }

    fn register_input(&self, bounds: Bounds<Pixels>, window: &mut Window, _cx: &mut App) {
        let Some(m) = metrics(&self.scroll) else {
            return;
        };

        let scroll = self.scroll.clone();
        let drag = self.drag.clone();
        let token = self.token;
        let viewport = m.viewport;
        let travel = m.travel;
        let max = m.max;
        // 滑块在窗口坐标里的上下边界（按下时用，只算一次就够）。
        let thumb_start = bounds.top().as_f32() + m.thumb_top;
        let thumb_end = thumb_start + m.thumb_height;

        // 按下：压在滑块上 → 开始拖；压在轨道空白处 → 往那个方向翻一屏。
        window.on_mouse_event(move |event: &MouseDownEvent, phase, _window, cx| {
            if phase != DispatchPhase::Bubble || event.button != MouseButton::Left {
                return;
            }
            if !bounds.contains(&event.position) {
                return;
            }
            let y = event.position.y.as_f32();
            if y >= thumb_start && y <= thumb_end {
                drag.begin(DragInfo {
                    token,
                    start_y: y,
                    start_scrolled: -scroll.offset().y.as_f32(),
                    travel,
                    max,
                });
                // 拖动态的滑块颜色更深，按下就得重画一次。
                cx.refresh_windows();
            } else {
                let current = -scroll.offset().y.as_f32();
                let step = viewport * PAGE_RATIO;
                let next = if y < thumb_start {
                    current - step
                } else {
                    current + step
                };
                scroll.set_offset(point(px(0.), px(-next.clamp(0.0, max))));
                cx.refresh_windows();
            }
        });

        let drag = self.drag.clone();
        let scroll_move = self.scroll.clone();
        window.on_mouse_event(move |event: &MouseMoveEvent, phase, _window, cx| {
            if phase != DispatchPhase::Bubble
                || event.pressed_button != Some(MouseButton::Left)
                || !drag.is_active(token)
            {
                return;
            }
            let Some((start_y, start_scrolled, travel, max)) = drag.drag_snapshot(token) else {
                return;
            };
            if travel <= 0.0 || max <= 0.0 {
                return;
            }
            let delta = event.position.y.as_f32() - start_y;
            let scrolled = (start_scrolled + delta * max / travel).clamp(0.0, max);
            scroll_move.set_offset(point(px(0.), px(-scrolled)));
            cx.refresh_windows();
        });

        let drag = self.drag.clone();
        window.on_mouse_event(move |_: &MouseUpEvent, phase, _window, cx| {
            if phase != DispatchPhase::Bubble || !drag.is_active(token) {
                return;
            }
            drag.end(token);
            // 松开后滑块颜色恢复常态，得重画一次。
            cx.refresh_windows();
        });
    }
}

fn quad(bounds: Bounds<Pixels>, radius: Pixels, color: Hsla) -> PaintQuad {
    PaintQuad {
        bounds,
        corner_radii: Corners::all(radius),
        background: Background::from(color),
        border_widths: Edges::all(px(0.)),
        border_color: transparent_black(),
        border_style: BorderStyle::default(),
    }
}

impl IntoElement for Scrollbar {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for Scrollbar {
    type RequestLayoutState = ();
    type PrepaintState = ();

    fn id(&self) -> Option<ElementId> {
        Some(self.id.clone())
    }

    fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        let mut style = Style::default();
        style.size.width = relative(1.0).into();
        style.size.height = relative(1.0).into();
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Self::PrepaintState {
        // 监听只能在 paint 阶段注册，这里什么都不做。
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        if self.interactive {
            self.register_input(bounds, window, cx);
        }

        let Some(m) = metrics(&self.scroll) else {
            return;
        };

        let track_x = bounds.center().x - px(self.width / 2.0);
        let radius = px(self.width / 2.0);
        let track = Bounds::new(
            point(track_x, bounds.top()),
            size(px(self.width), bounds.size.height),
        );
        window.paint_quad(quad(track, radius, super::theme::track()));

        // 拖动中滑块加深一档，给一点「抓住了」的反馈。
        let color = if self.drag.is_active(self.token) {
            super::theme::text_secondary()
        } else {
            super::theme::text_tertiary()
        };
        let thumb = Bounds::new(
            point(track_x, bounds.top() + px(m.thumb_top)),
            size(px(self.width), px(m.thumb_height)),
        );
        window.paint_quad(quad(thumb, radius, color));
    }
}
