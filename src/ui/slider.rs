//! 自绘音量滑条：对数刻度、发声电平特效、悬停浮出的圆钮。
//! 直接画 quad，不创建额外实体，滚动列表里也不掉帧。

use std::{cell::Cell, cell::RefCell, collections::HashMap, rc::Rc, time::Instant};

use gpui::{
    App, Background, BorderStyle, Bounds, Corners, DispatchPhase, Edges, Element, ElementId,
    GlobalElementId, Hsla, InspectorElementId, IntoElement, LayoutId, MouseButton, MouseDownEvent,
    MouseExitEvent, MouseMoveEvent, MouseUpEvent, PaintQuad, Pixels, Style, Window, point, px,
    relative, size, transparent_black,
};

use crate::config::ActivityEffect;

/// 全局拖拽令牌：同一时刻只有一个滑条处于拖拽状态。
///
/// 顺带记住「鼠标停在哪个滑条上」：圆钮只在悬停时画出来，而悬停不是布局变化，
/// 光靠 `window.mouse_position()` 在 paint 里现算的话，要等别的什么原因触发重绘
/// 圆钮才出现（表现为「不是每次都有」）。所以鼠标移进/移出时自己记一笔并请求重绘。
#[derive(Clone, Default)]
pub struct SliderDragState {
    drag: Rc<Cell<u64>>,
    hover: Rc<Cell<u64>>,
}

impl SliderDragState {
    pub fn new() -> Self {
        Self::default()
    }

    fn begin(&self, token: u64) {
        self.drag.set(token);
    }

    fn end(&self, token: u64) {
        if self.drag.get() == token {
            self.drag.set(0);
        }
    }

    fn is_active(&self, token: u64) -> bool {
        token != 0 && self.drag.get() == token
    }

    fn is_hovered(&self, token: u64) -> bool {
        token != 0 && self.hover.get() == token
    }

    fn set_hovered(&self, token: u64, hovered: bool) {
        if hovered {
            self.hover.set(token);
        } else if self.hover.get() == token {
            self.hover.set(0);
        }
    }
}

/// 电平平滑状态：按元素令牌记住上一帧的电平。
///
/// 滑条元素每帧都会重新构造，所以状态必须挂在外面共享（和拖动状态一样）。
/// 起音立刻跟上、回落按固定速度衰减，是 VU 表的标准手感；不做这一层的话
/// 30Hz 采到的原始峰值会一格一格地闪。
#[derive(Clone, Default)]
pub struct ActivityState {
    levels: Rc<RefCell<HashMap<u64, Meter>>>,
}

struct Meter {
    level: f32,
    at: Instant,
}

/// 回落速度（每秒掉多少电平）：满电平掉到 0 大约 0.45 秒。
const RELEASE_PER_SEC: f32 = 2.2;

impl ActivityState {
    pub fn new() -> Self {
        Self::default()
    }

    /// 面板收起时清掉，下次打开不会带着上次的残留电平。
    pub fn reset(&self) {
        self.levels.borrow_mut().clear();
    }

    fn level(&self, token: u64, peak: f32, now: Instant) -> f32 {
        let target = peak.clamp(0.0, 1.0);
        let mut levels = self.levels.borrow_mut();
        let meter = levels.entry(token).or_insert(Meter {
            level: target,
            at: now,
        });
        // 隔了很久才重画（面板收起来过、卡了一帧）时不要一次掉一大截外，
        // 也不要用一个巨大的 dt 把衰减算过头。
        let dt = now
            .saturating_duration_since(meter.at)
            .as_secs_f32()
            .min(0.25);
        meter.at = now;
        if target >= meter.level {
            meter.level = target;
        } else {
            meter.level = (meter.level - RELEASE_PER_SEC * dt).max(target);
        }
        if meter.level < 0.002 {
            meter.level = 0.0;
        }
        meter.level
    }
}

pub struct VolumeSlider {
    id: ElementId,
    token: u64,
    /// 0..1 的显示值（已经过对数换算）。
    value: f32,
    /// 0..1 的峰值。
    peak: f32,
    muted: bool,
    enabled: bool,
    fill: Hsla,
    /// 轨道底色：`None` = 主题的轨道色（OSD 卡片可以单独指定）。
    track_color: Option<Hsla>,
    /// 静音时的填充色：`None` = 主题那层淡色。
    muted_color: Option<Hsla>,
    /// 圆角半径（逻辑像素）：`None` = 半高，也就是全圆头的胶囊形。
    corner: Option<f32>,
    effect: ActivityEffect,
    activity: ActivityState,
    /// 拖到这里就停（设备最大音量限制）；1.0 = 不限制。
    limit: f32,
    /// 轨道厚度（逻辑像素）。OSD 里音量条的粗细由用户自己调。
    thickness: f32,
    /// 触摸区高度（逻辑像素）：亮度面板比音量面板矮，单独压一档。
    touch_height: f32,
    /// 竖向推子：轨道从下往上长，拖动按 y 换算。
    vertical: bool,
    /// 整根条被画成转了多少度（度，顺时针）。拖动时要把鼠标位置转回来再换算，
    /// 不然条竖起来之后还是左右拖。
    rotation: f32,
    on_change: Option<Rc<dyn Fn(f32, &mut Window, &mut App)>>,
    on_release: Option<Rc<dyn Fn(&mut Window, &mut App)>>,
    drag: SliderDragState,
}

impl VolumeSlider {
    pub fn new(id: impl Into<ElementId>, value: f32, drag: SliderDragState) -> Self {
        let id = id.into();
        let token = hash_id(&id);
        Self {
            id,
            token,
            value,
            peak: 0.0,
            muted: false,
            enabled: true,
            fill: super::theme::accent(),
            track_color: None,
            muted_color: None,
            corner: None,
            effect: ActivityEffect::Level,
            activity: ActivityState::new(),
            limit: 1.0,
            thickness: TRACK_HEIGHT,
            touch_height: TOUCH_HEIGHT,
            vertical: false,
            rotation: 0.0,
            on_change: None,
            on_release: None,
            drag,
        }
    }

    pub fn peak(mut self, peak: f32) -> Self {
        self.peak = peak.clamp(0.0, 1.0);
        self
    }

    /// 发声特效样式（设置页里选）。
    pub fn effect(mut self, effect: ActivityEffect) -> Self {
        self.effect = effect;
        self
    }

    /// 多根滑条共用一份电平平滑状态（按元素令牌区分）。
    pub fn activity(mut self, state: ActivityState) -> Self {
        self.activity = state;
        self
    }

    /// 最大音量限制：滑条拖到头也只到 `limit`（0..1 的显示位置，1.0 = 不限制）。
    ///
    /// 钳制本身还有音频线程兜底，这里是为了手感——不然拖到 100% 会被
    /// 下一份快照瞬间拽回去，看着像滑条在弹。
    pub fn limit(mut self, limit: f32) -> Self {
        self.limit = limit.clamp(0.02, 1.0);
        self
    }

    pub fn muted(mut self, muted: bool) -> Self {
        self.muted = muted;
        self
    }

    pub fn enabled(mut self, enabled: bool) -> Self {
        self.enabled = enabled;
        self
    }

    pub fn fill(mut self, color: Hsla) -> Self {
        self.fill = color;
        self
    }

    /// 轨道底色（不传就用主题的）。
    pub fn track_color(mut self, color: Option<Hsla>) -> Self {
        self.track_color = color;
        self
    }

    /// 静音时的填充色（不传就用主题的）。
    pub fn muted_color(mut self, color: Option<Hsla>) -> Self {
        self.muted_color = color;
        self
    }

    /// 圆角半径（逻辑像素，不传就是半高 = 全圆头）。
    pub fn corner(mut self, radius: f32) -> Self {
        self.corner = Some(radius.max(0.0));
        self
    }

    /// 轨道厚度（逻辑像素）。默认 6px，和面板里一致。
    pub fn thickness(mut self, value: f32) -> Self {
        self.thickness = value.clamp(2.0, 32.0);
        self
    }

    /// 触摸区高度（逻辑像素）。默认 24；亮度面板整块要短一截，可以压到 20。
    ///
    /// 只改「这一排占多高」：轨道厚度、圆钮大小都不动，画出来不变形。
    pub fn height(mut self, value: f32) -> Self {
        self.touch_height = value.clamp(12.0, 48.0);
        self
    }

    /// 立起来：轨道改成从下往上长，值 0 在底、1 在顶。
    ///
    /// 竖向时滑条自己不再要高度（`height` 那两个参数就不管了），
    /// 宽高都由外面的通道列给：列多宽、推子多长，都听调用方的。
    /// 整根条被画成转了多少度（顺时针）。只影响拖动换算，画由调用方转。
    pub fn rotation(mut self, degrees: f32) -> Self {
        self.rotation = degrees;
        self
    }

    pub fn vertical(mut self, value: bool) -> Self {
        self.vertical = value;
        self
    }

    pub fn on_change(mut self, handler: impl Fn(f32, &mut Window, &mut App) + 'static) -> Self {
        self.on_change = Some(Rc::new(handler));
        self
    }

    /// 松开鼠标时触发：调用方可以在这里清掉拖动期间的本地预览值。
    pub fn on_release(mut self, handler: impl Fn(&mut Window, &mut App) + 'static) -> Self {
        self.on_release = Some(Rc::new(handler));
        self
    }
}

fn hash_id(id: &ElementId) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    format!("{id:?}").hash(&mut hasher);
    hasher.finish()
}

fn ratio(x: Pixels, bounds: &Bounds<Pixels>) -> f32 {
    if bounds.size.width <= px(0.) {
        return 0.0;
    }
    let offset = (x - bounds.left()) / bounds.size.width;
    f32::from(offset).clamp(0.0, 1.0)
}

/// 竖向推子：底部是 0、顶部是 1。
/// 把屏幕上的鼠标位置转回元素自己的坐标系（元素整体顺时针转了 `degrees` 度）。
///
/// 转轴是元素矩形的中心，和渲染时 `Div::rotate` 的轴一致。
fn to_unrotated(position: gpui::Point<Pixels>, bounds: &Bounds<Pixels>, degrees: f32) -> gpui::Point<Pixels> {
    if degrees == 0.0 {
        return position;
    }
    let center = bounds.center();
    let dx = position.x - center.x;
    let dy = position.y - center.y;
    let (sin, cos) = degrees.to_radians().sin_cos();
    gpui::point(
        center.x + dx * cos + dy * sin,
        center.y - dx * sin + dy * cos,
    )
}

fn ratio_vertical(y: Pixels, bounds: &Bounds<Pixels>) -> f32 {
    if bounds.size.height <= px(0.) {
        return 0.0;
    }
    let offset = (bounds.bottom() - y) / bounds.size.height;
    f32::from(offset).clamp(0.0, 1.0)
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

const TRACK_HEIGHT: f32 = 6.0;
const THUMB_SIZE: f32 = 13.0;
const TOUCH_HEIGHT: f32 = 24.0;

/// 把主题色按比例压一道透明度：特效全靠同一支颜色叠不同浓度。
fn tint(color: Hsla, alpha: f32) -> Hsla {
    Hsla {
        color: color.color,
        alpha: (color.alpha * alpha.clamp(0.0, 1.0)).clamp(0.0, 1.0),
    }
}

/// 以秒为单位的周期相位（0..1）：特效的时间基准，跟重绘频率无关。
fn phase(period: f32) -> f32 {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs_f64())
        .unwrap_or(0.0);
    ((secs / period.max(0.05) as f64) % 1.0) as f32
}

impl Element for VolumeSlider {
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
        // 触摸高度至少 24，轨道更粗时跟着长，圆钮才不会被裁掉。
        // 竖向时高度交给外面的通道列（列给多长就多长），这里只撑满。
        style.size.height = if self.vertical {
            relative(1.0).into()
        } else {
            px(self.touch_height.max(self.thickness + 8.0)).into()
        };
        (window.request_layout(style, [], cx), ())
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _window: &mut Window,
        _cx: &mut App,
    ) -> Self::PrepaintState {
        // 注意：鼠标监听必须在 paint 阶段注册（GPUI 的 DrawPhase 硬约束），
        // 这里只做布局收尾，注册逻辑放在 paint()。
        let _ = bounds;
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        _prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        _cx: &mut App,
    ) {
        self.register_input(bounds, window);

        let vertical = self.vertical;
        let thickness = self.thickness;
        // 圆角不超过半高：再大也只是胶囊形，画出来一样。
        let radius = px(self.corner.unwrap_or(thickness / 2.0).min(thickness / 2.0));
        // 两根轴分清楚：along = 值增长的方向（水平是 x 向右，竖向是 y 向上），
        // cross = 轨道的粗细方向。下面所有几何都按这两个轴算，两套排版共用一份代码。
        let along_length = if vertical {
            bounds.size.height
        } else {
            bounds.size.width
        };
        let cross_start = if vertical {
            bounds.center().x - px(thickness / 2.0)
        } else {
            bounds.center().y - px(thickness / 2.0)
        };

        let track_bounds = if vertical {
            Bounds::new(
                point(cross_start, bounds.top()),
                size(px(thickness), along_length),
            )
        } else {
            Bounds::new(
                point(bounds.left(), cross_start),
                size(along_length, px(thickness)),
            )
        };
        let track_color = self.track_color.unwrap_or_else(super::theme::track);
        window.paint_quad(quad(track_bounds, radius, track_color));

        // 填充：先画蓝条。
        let value = self.value.clamp(0.0, self.limit);
        if value > 0.0 {
            let fill_len = (along_length * value).max(radius * 2.0).min(along_length);
            let fill_bounds = if vertical {
                Bounds::new(
                    point(cross_start, bounds.bottom() - fill_len),
                    size(px(thickness), fill_len),
                )
            } else {
                Bounds::new(
                    point(bounds.left(), cross_start),
                    size(fill_len, px(thickness)),
                )
            };
            let color = if self.muted {
                self.muted_color.unwrap_or_else(super::theme::muted_fill)
            } else {
                self.fill
            };
            window.paint_quad(quad(fill_bounds, radius, color));
        }

        // 发声电平：**必须画在填充之上**。
        //
        // 以前这一层画在填充下面，结果只有「峰值超出音量」的那一小截露在外面，
        // 音量拉满时被蓝条整个盖住 —— 于是「满音量看不出谁在响」。
        // 现在照 EarTrumpet 的算法，让电平表落在填充内部：
        // 长度 = 填充长度 × 电平，满音量时整条都是表，任何音量下都看得见。
        if self.enabled && !self.muted && self.effect.animates() && value > 0.004 {
            let level = self.activity.level(self.token, self.peak, Instant::now());
            let meter_len = (along_length * value * level).min(along_length);
            if level > 0.004 && meter_len > px(0.5) {
                self.paint_activity(window, &bounds, cross_start, meter_len);
            }
        }

        // 悬停或拖拽时显示圆钮。悬停状态由下面的鼠标监听维护，不靠每帧现算。
        let hovered = self.drag.is_hovered(self.token)
            || self.drag.is_active(self.token)
            || bounds.contains(&window.mouse_position());
        if hovered && self.enabled {
            // 轨道越粗，圆钮越大，视觉上才压得住。
            let thumb = (thickness + 7.0).max(THUMB_SIZE);
            let center = if vertical {
                point(bounds.center().x, bounds.bottom() - along_length * value)
            } else {
                point(bounds.left() + along_length * value, bounds.center().y)
            };
            let thumb_bounds = Bounds::new(
                point(center.x - px(thumb / 2.0), center.y - px(thumb / 2.0)),
                size(px(thumb), px(thumb)),
            );
            window.paint_quad(quad(
                thumb_bounds,
                px(thumb / 2.0),
                super::theme::text_primary(),
            ));
        }
    }
}

impl IntoElement for VolumeSlider {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl VolumeSlider {
    /// 画发声电平层。
    ///
    /// 所有样式都只在「填充内部 0..meter_width」这段上叠 quad，颜色统一取
    /// `theme::activity`，靠透明度分级：这样深色 / 浅色主题下都不会出现
    /// 「叠在蓝条上看不见」的情况。
    fn paint_activity(
        &self,
        window: &mut Window,
        bounds: &Bounds<Pixels>,
        cross_start: Pixels,
        meter_len: Pixels,
    ) {
        let total = meter_len.as_f32();
        if total <= 0.0 {
            return;
        }
        let vertical = self.vertical;
        let base = super::theme::activity();
        let track = self.thickness;
        let along_length = if vertical {
            bounds.size.height.as_f32()
        } else {
            bounds.size.width.as_f32()
        };
        // 沿轴 `from..from+len`、横轴 `cross_from..cross_from+cross_len` 的一块。
        // 竖向时沿轴从底边往上量，横向时从左边往右量 —— 四个特效只写一份。
        let rect = |from: f32, len: f32, cross_from: Pixels, cross_len: f32| -> Bounds<Pixels> {
            if vertical {
                Bounds::new(
                    point(cross_from, bounds.bottom() - px(from + len)),
                    size(px(cross_len.max(0.0)), px(len.max(0.0))),
                )
            } else {
                Bounds::new(
                    point(bounds.left() + px(from), cross_from),
                    size(px(len.max(0.0)), px(cross_len.max(0.0))),
                )
            }
        };
        // 整条轨道厚度的色块。
        let bar = |from: f32, len: f32| rect(from, len, cross_start, track);
        // 贴轨道某一侧边缘的一道细线（横排的「上下」= 竖排的「左右」）。
        let edge = |from: f32, len: f32, inset: f32| rect(from, len, cross_start + px(inset), 2.0);
        // 圆角跟着长度收，短到只剩一个点时不至于把圆角画穿。
        let cap = px((total * 0.5).min(track * 0.5));
        // 电平头那道线：所有样式共用，当前位置一眼可辨。
        let head = 2.0f32.min(total);

        match self.effect {
            ActivityEffect::Off => {}
            // 经典电平表（EarTrumpet 的做法）：整段一层淡光，两侧各压一条亮边。
            //
            // 圆角跟轨道 / 填充用同一个 `track / 2`：早先用的是 `cap`（跟着电平段
            // 长度收），电平一小圆角就跟着变小、看着成了方块。两条亮边也往里缩
            // 半个圆角，否则会在两端从圆角里冒出一小截直角。
            ActivityEffect::Level => {
                let round = px(track * 0.5);
                window.paint_quad(quad(bar(0.0, total), round, tint(base, 0.16)));
                let line = tint(base, 0.62);
                let trim = (track * 0.5).min(total * 0.5);
                let len = (total - trim * 2.0).max(0.0);
                if len > 0.5 {
                    window.paint_quad(quad(edge(trim, len, 0.0), px(1.0), line));
                    window.paint_quad(quad(edge(trim, len, track - 2.0), px(1.0), line));
                }
            }
            // 渐变：尾巴几乎透明，越靠电平头越亮。
            ActivityEffect::Gradient => {
                let slices = ((total / 3.0).ceil() as usize).clamp(1, 96);
                for index in 0..slices {
                    let t0 = index as f32 / slices as f32;
                    let t1 = (index + 1) as f32 / slices as f32;
                    window.paint_quad(quad(
                        bar(total * t0, total * (t1 - t0)),
                        px(0.),
                        tint(base, 0.5 * t1 * t1),
                    ));
                }
                window.paint_quad(quad(bar(total - head, head), px(1.0), tint(base, 0.9)));
            }
            // 流光：一道柔光在「已填充的那段」里来回扫。
            //
            // 跑道只看**音量位置**（`value`），不看实时电平 —— 早先用的是
            // `value × level` 那段（`total`），峰值一起伏跑道就跟着伸缩，
            // 光带被扯长扯短、两头还在跳，看着就是在抖。现在只有用户自己
            // 改音量时跑道才变，动画本身是匀速的，各行之间也同步。
            ActivityEffect::Shimmer => {
                let fill = (along_length * self.value).clamp(0.0, along_length);
                if fill <= 0.5 {
                    return;
                }
                window.paint_quad(quad(bar(0.0, fill), cap, tint(base, 0.10)));
                // 光带宽度跟着跑道走，短跑道上也别糊成一团。
                let sweep = (fill * 0.45).clamp(18.0, 96.0);
                // 来回扫：0 → 1 → 0（余弦），到两头是减速停住再折返，不会突然断。
                let center = fill * (0.5 - 0.5 * (phase(2.2) * std::f32::consts::TAU).cos());
                let from = (center - sweep * 0.5).max(0.0);
                let to = (center + sweep * 0.5).min(fill);
                if to > from {
                    let slices = (((to - from) / 2.0).ceil() as usize).clamp(1, 32);
                    for index in 0..slices {
                        let t0 = from + (to - from) * (index as f32 / slices as f32);
                        let t1 = from + (to - from) * ((index + 1) as f32 / slices as f32);
                        let middle = (t0 + t1) * 0.5;
                        let distance = ((middle - center) / (sweep * 0.5)).abs().min(1.0);
                        // 钟形：光带中间最亮，两头化开，不会出现硬边。
                        let bell = (1.0 - distance * distance).powi(2);
                        window.paint_quad(quad(bar(t0, t1 - t0), px(0.), tint(base, 0.6 * bell)));
                    }
                }
            }
            // 浮光：流光 + 呼吸叠在一起，但两条动画互不干涉 ——
            // 流光只改光带的**位置**（跑道固定看音量，不看电平），
            // 呼吸只改整段的**亮度**（两条时间轴周期不同：2.2s / 1.6s）。
            // 各改各的维度，就不会出现「光带被呼吸拽着伸缩」那种抖。
            ActivityEffect::Drift => {
                let fill = (along_length * self.value).clamp(0.0, along_length);
                if fill <= 0.5 {
                    return;
                }
                let breath = 0.5 + 0.5 * (phase(1.6) * std::f32::consts::TAU).sin();
                // 底光：亮度随呼吸走，位置 / 长度一动不动。
                window.paint_quad(quad(
                    bar(0.0, fill),
                    px(track * 0.5),
                    tint(base, 0.06 + 0.10 * breath),
                ));
                // 光带：位置随流光走，亮度只乘一个很浅的呼吸系数（0.8~1.0），
                // 幅度小到看不出「在跟着呼吸变」，只让两者看起来是一体的。
                let sweep = (fill * 0.45).clamp(18.0, 96.0);
                let center = fill * (0.5 - 0.5 * (phase(2.2) * std::f32::consts::TAU).cos());
                let from = (center - sweep * 0.5).max(0.0);
                let to = (center + sweep * 0.5).min(fill);
                let gain = 0.8 + 0.2 * breath;
                if to > from {
                    let slices = (((to - from) / 2.0).ceil() as usize).clamp(1, 32);
                    for index in 0..slices {
                        let t0 = from + (to - from) * (index as f32 / slices as f32);
                        let t1 = from + (to - from) * ((index + 1) as f32 / slices as f32);
                        let middle = (t0 + t1) * 0.5;
                        let distance = ((middle - center) / (sweep * 0.5)).abs().min(1.0);
                        let bell = (1.0 - distance * distance).powi(2);
                        window.paint_quad(quad(
                            bar(t0, t1 - t0),
                            px(0.),
                            tint(base, 0.6 * bell * gain),
                        ));
                    }
                }
            }
            // 呼吸：电平头上一团会呼吸的辉光，允许往轨道两侧溢出一点，做出光晕。
            ActivityEffect::Pulse => {
                let breath = 0.5 + 0.5 * (phase(1.6) * std::f32::consts::TAU).sin();
                window.paint_quad(quad(bar(0.0, total), cap, tint(base, 0.07 + 0.09 * breath)));
                for (width, alpha, spill) in [
                    (4.0f32, 0.55f32, 0.0f32),
                    (9.0, 0.22, 2.0),
                    (16.0, 0.08, 4.0),
                ] {
                    // 光晕中心就是电平头；贴到两端时往回夹，别画到滑条外面去。
                    let from = (total - width * 0.5).clamp(0.0, (along_length - width).max(0.0));
                    let glow_thickness = track + spill * 2.0;
                    window.paint_quad(quad(
                        rect(from, width, cross_start - px(spill), glow_thickness),
                        px(glow_thickness * 0.5),
                        tint(base, alpha * (0.55 + 0.45 * breath)),
                    ));
                }
            }
        }
    }

    /// 注册按下 / 拖动 / 抬起监听。只能在 paint 阶段调用。
    fn register_input(&self, bounds: Bounds<Pixels>, window: &mut Window) {
        if !self.enabled {
            return;
        }
        let token = self.token;
        // 上限在拖拽时就要生效：拖过线也只发上限值。
        let limit = self.limit;
        // 竖向时按 y 换算（底 0 顶 1），横向按 x。
        let vertical = self.vertical;
        let rotation = self.rotation;
        let ratio_at = move |position: gpui::Point<Pixels>| -> f32 {
            // 条是转着画的：先把鼠标位置转回条自己的坐标系，否则竖起来之后
            // 还是按左右拖（拖起来跟视觉方向对不上）。
            let position = to_unrotated(position, &bounds, rotation);
            if vertical {
                ratio_vertical(position.y, &bounds)
            } else {
                ratio(position.x, &bounds)
            }
        };

        // 鼠标进出滑条：只在悬停状态真的变了时请求重绘。
        // 圆钮不是布局的一部分，没有这一笔就得等别的重绘顺带把它带出来。
        window.on_mouse_event({
            let hover = self.drag.clone();
            move |event: &MouseMoveEvent, phase, window, _cx| {
                if phase != DispatchPhase::Bubble {
                    return;
                }
                let inside = bounds.contains(&event.position);
                if hover.is_hovered(token) == inside {
                    return;
                }
                hover.set_hovered(token, inside);
                window.refresh();
            }
        });
        // 鼠标直接移出窗口时收不到 MouseMove，补一条清掉，免得圆钮留在原地。
        window.on_mouse_event({
            let hover = self.drag.clone();
            move |_: &MouseExitEvent, phase, window, _cx| {
                if phase != DispatchPhase::Bubble || !hover.is_hovered(token) {
                    return;
                }
                hover.set_hovered(token, false);
                window.refresh();
            }
        });

        let Some(on_change) = self.on_change.clone() else {
            return;
        };
        let drag = self.drag.clone();
        let drag_move = self.drag.clone();
        let drag_release = self.drag.clone();
        let release_handler = self.on_release.clone();

        window.on_mouse_event({
            let drag = drag;
            let press_handler = on_change.clone();
            move |event: &MouseDownEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble || event.button != MouseButton::Left {
                    return;
                }
                if !bounds.contains(&event.position) {
                    return;
                }
                drag.begin(token);
                press_handler(ratio_at(event.position).min(limit), window, cx);
            }
        });

        window.on_mouse_event({
            let drag = drag_move;
            let move_handler = on_change.clone();
            move |event: &MouseMoveEvent, phase, window, cx| {
                if phase != DispatchPhase::Bubble
                    || event.pressed_button != Some(MouseButton::Left)
                    || !drag.is_active(token)
                {
                    return;
                }
                move_handler(ratio_at(event.position).min(limit), window, cx);
            }
        });

        window.on_mouse_event(move |_: &MouseUpEvent, phase, window, cx| {
            if phase != DispatchPhase::Bubble {
                return;
            }
            if !drag_release.is_active(token) {
                return;
            }
            drag_release.end(token);
            if let Some(release) = release_handler.as_ref() {
                release(window, cx);
            }
        });
    }
}
