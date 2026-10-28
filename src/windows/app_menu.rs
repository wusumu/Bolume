//! 应用右键菜单：**单独一扇窗口**。
//!
//! 以前这份菜单画在音量面板自己的浮层里，尺寸被面板窗口裁着 —— 横向排版的面板
//! 高三百多还好，竖向排版只有一百九十来像素，菜单里那串「指定此应用播放设备」
//! 根本施展不开，底下的设备看都看不到。
//!
//! 现在改成和托盘菜单同一套做法：无边框小窗，尺寸只跟菜单内容走，位置按鼠标点摆，
//! 出了屏幕边就翻到另一侧。窗口是 `WS_EX_NOACTIVATE`（`configure_osd` 那套）：
//! 点它不会把面板挤到后台，面板的失焦收起逻辑一个字都不用改。
//!
//! 点击 / 悬停自己算行号（`row_tops`）：不留边距给 gpui 的 `hover()`，
//! 因为菜单还要按行判断「点的是哪一项」，两份判断各算一遍迟早对不上。

use std::cell::RefCell;
use std::rc::Rc;
use std::time::Instant;

use gpui::prelude::*;
use gpui::{
    AnyElement, Bounds, Context, MouseButton, MouseDownEvent, MouseMoveEvent, Render, Window, div,
    point, px, size,
};

use crate::audio::{AudioEngine, Snapshot};
use crate::config::ConfigStore;
use crate::ui::{device_icon_resolved, icon, theme};
use crate::windows::platform;

/// 一行的高度（和面板里原来的右键菜单同一档观感）。
const ROW_HEIGHT: f32 = 28.0;
/// 卡片内边距。
const PAD: f32 = 4.0;
/// 分隔线占的高度（1px 线 + 上下留白）。
const SEPARATOR_HEIGHT: f32 = 9.0;
/// 「指定此应用播放设备」那行小标题的高度。
const CAPTION_HEIGHT: f32 = 20.0;
/// 卡片宽度：原来的 216 够放 `Headphones (Meizu HiFi DAC…)` 这一档设备名。
const WIDTH: f32 = 216.0;
/// 行首图标块宽度：有图标的行和没图标的行文字左沿要齐。
const ICON_SLOT: f32 = 20.0;
/// 图标与文字之间的间隙。
const ICON_GAP: f32 = 6.0;
/// 行首图标尺寸。
///
/// 13 那档在 24×24 的线条图上等于把描边压到 0.95px，抗锯齿糊成一团灰 ——
/// 音箱只剩个方框、耳机看不出是耳机。16 是这里能摆进 28 高行里的最大值。
const ICON_SIZE: f32 = 16.0;
/// 离屏幕边至少留这么多。
const EDGE_MARGIN: f32 = 4.0;

/// 打开菜单时从面板带过来的「这是哪个应用、它现在什么状态」。
#[derive(Clone, Debug)]
pub struct MenuTarget {
    pub key: String,
    /// 光标显示用的名字（改过名的用自定义名）。
    pub name: String,
    pub pid: u32,
    pub is_system: bool,
    /// 能不能改播放设备（系统声音、或者这台机器拿不到 AudioPolicyConfig 时不能）。
    pub movable: bool,
    /// 当前指定的设备；`None` = 跟随默认设备。
    pub device_id: Option<String>,
    /// 这个应用眼下是否在做响度均衡（勾选状态）。
    pub loudness: bool,
    /// 当前是否已被折叠。
    pub hidden: bool,
    /// 右键点在**面板窗口**里的坐标：改名那张卡片要摆在这儿。
    pub anchor: (f32, f32),
}

/// 菜单里点下去要干的事。
#[derive(Clone, Debug)]
pub enum Action {
    Rename,
    ToggleHidden,
    ToggleLoudness,
    /// `None` = 跟随默认设备。
    Route(Option<String>),
}

enum Row {
    Item {
        action: Action,
        label: String,
        /// 设备行的图标名（`String`：可能是配置里自定义的图标）。
        icon: Option<String>,
        checked: bool,
    },
    Separator,
    Caption(String),
}

impl Row {
    fn height(&self) -> f32 {
        match self {
            Row::Item { .. } => ROW_HEIGHT,
            Row::Separator => SEPARATOR_HEIGHT,
            Row::Caption(_) => CAPTION_HEIGHT,
        }
    }
}

pub struct AppMenu {
    config: Rc<RefCell<ConfigStore>>,
    snapshot: Rc<RefCell<Snapshot>>,
    engine: Rc<AudioEngine>,
    target: Option<MenuTarget>,
    rows: Vec<Row>,
    /// 每一行的顶端 y（鼠标落在哪一行靠它判断）。
    row_tops: Vec<f32>,
    hovered: Option<usize>,
    visible: bool,
    /// 窗口尺寸（逻辑像素）：宽固定 216，高按行数累加。
    size: (f32, f32),
    /// 弹出位置（屏幕逻辑坐标，左上角）。
    origin: (f32, f32),
    shown_at: Instant,
}

impl AppMenu {
    pub fn new(
        config: Rc<RefCell<ConfigStore>>,
        snapshot: Rc<RefCell<Snapshot>>,
        engine: Rc<AudioEngine>,
    ) -> Self {
        Self {
            config,
            snapshot,
            engine,
            target: None,
            rows: Vec::new(),
            row_tops: Vec::new(),
            hovered: None,
            visible: false,
            size: (WIDTH, ROW_HEIGHT + PAD * 2.0),
            origin: (0.0, 0.0),
            shown_at: Instant::now() - std::time::Duration::from_secs(1),
        }
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// 按目标应用重建行列表（每次弹出都重建：勾选状态、设备列表都会变）。
    fn rebuild_rows(&mut self, target: &MenuTarget) {
        let cfg = self.config.borrow().get().clone();
        let lang = cfg.language;
        let mut rows = Vec::new();

        rows.push(Row::Item {
            action: Action::Rename,
            label: lang.pick("重命名", "Rename").to_string(),
            icon: Some("pencil".to_string()),
            checked: false,
        });
        rows.push(Row::Item {
            action: Action::ToggleHidden,
            label: if target.hidden {
                lang.pick("取消折叠此应用", "Uncollapse this app")
            } else {
                lang.pick("折叠此应用", "Collapse this app")
            }
            .to_string(),
            // 折叠 / 取消折叠就是箭头朝里收还是朝外放，不另配两张图形。
            icon: Some(
                if target.hidden {
                    "unfold"
                } else {
                    "fold"
                }
                .to_string(),
            ),
            checked: false,
        });
        // 系统声音没有独立会话，响度均衡对它不生效（音频线程那边也是直接跳过）。
        if !target.is_system {
            rows.push(Row::Item {
                action: Action::ToggleLoudness,
                label: lang.pick("响度均衡", "Loudness equalization").to_string(),
                icon: Some("levels".to_string()),
                checked: target.loudness,
            });
        }

        if target.movable {
            rows.push(Row::Separator);
            rows.push(Row::Caption(
                lang.pick("指定此应用播放设备", "Playback device for this app")
                    .to_string(),
            ));
            rows.push(Row::Item {
                action: Action::Route(None),
                label: lang.pick("跟随默认设备", "Default device").to_string(),
                // 不指定设备 = 用系统默认那台音箱。
                icon: Some("speaker".to_string()),
                checked: target.device_id.is_none(),
            });
            let devices = self.snapshot.borrow().devices.clone();
            for device in devices.iter().filter(|device| device.enabled) {
                rows.push(Row::Item {
                    action: Action::Route(Some(device.id.clone())),
                    label: cfg.device_label(&device.id, &device.name),
                    icon: Some(
                        device_icon_resolved(&cfg, &device.id, device.kind).to_string(),
                    ),
                    checked: target.device_id.as_deref() == Some(device.id.as_str()),
                });
            }
        }

        self.rows = rows;
    }

    /// 按行数算窗口高度（宽度固定）。
    fn measure(&mut self) {
        let mut y = PAD;
        self.row_tops.clear();
        for row in &self.rows {
            self.row_tops.push(y);
            y += row.height();
        }
        self.size = (WIDTH, y + PAD);
    }

    fn hit_test(&self, y: f32) -> Option<usize> {
        for (index, row) in self.rows.iter().enumerate() {
            let top = self.row_tops.get(index).copied().unwrap_or(f32::MAX);
            if y >= top && y < top + row.height() {
                return matches!(row, Row::Item { .. }).then_some(index);
            }
        }
        None
    }

    /// 弹出：`at` 是右键点在**屏幕**上的逻辑坐标。
    pub fn show_for(
        &mut self,
        target: MenuTarget,
        at: (f32, f32),
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.rebuild_rows(&target);
        self.measure();
        self.target = Some(target);
        self.hovered = None;
        self.origin = at;
        self.shown_at = Instant::now();
        self.visible = true;
        cx.notify();
        cx.defer_in(window, |this, window, cx| {
            // 先全透明再摆位置，否则会先按上一轮的位置闪一帧。
            platform::set_opacity(window, 0.0);
            this.update_frame(window, cx);
            platform::set_visible(window, true);
            // 只提到最前，**不激活**：菜单窗口是 WS_EX_NOACTIVATE，
            // 一激活面板就失焦，面板会当场收起来。
            platform::bring_to_front(window);
            cx.notify();
            cx.defer_in(window, |this, window, cx| {
                platform::set_opacity(window, 1.0);
                this.visible = true;
                cx.notify();
            });
        });
    }

    pub fn hide(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.visible {
            return;
        }
        self.visible = false;
        self.hovered = None;
        cx.defer_in(window, |this, window, _| {
            platform::set_visible(window, false);
            platform::set_opacity(window, 0.0);
            this.visible = false;
        });
        cx.notify();
    }

    /// 摆位置：默认在光标右下展开，右边 / 下边放不下就翻到另一侧，再夹进显示器工作区。
    fn update_frame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (width, height) = self.size;
        let (click_x, click_y) = self.origin;
        let area = display_area(cx, click_x, click_y);
        let left = area.origin.x.as_f32() + EDGE_MARGIN;
        let top = area.origin.y.as_f32() + EDGE_MARGIN;
        let right = area.origin.x.as_f32() + area.size.width.as_f32() - EDGE_MARGIN;
        let bottom = area.origin.y.as_f32() + area.size.height.as_f32() - EDGE_MARGIN;

        let mut x = click_x + 2.0;
        if x + width > right {
            x = click_x - width - 2.0;
        }
        let mut y = click_y + 2.0;
        if y + height > bottom {
            y = click_y - height - 2.0;
        }
        x = x.clamp(left, (right - width).max(left));
        y = y.clamp(top, (bottom - height).max(top));

        platform::set_frame(
            window,
            Bounds::new(point(px(x), px(y)), size(px(width), px(height))),
            window.scale_factor(),
        );
        window.bounds_changed(cx);
    }

    /// 一行。
    fn row_element(&self, index: usize, row: &Row, cx: &mut Context<Self>) -> AnyElement {
        match row {
            Row::Separator => div()
                .h(px(SEPARATOR_HEIGHT))
                .flex_none()
                .flex()
                .items_center()
                .px(px(6.))
                .child(div().w_full().h(px(1.)).bg(theme::border()))
                .into_any_element(),
            Row::Caption(text) => div()
                .h(px(CAPTION_HEIGHT))
                .flex_none()
                .px(px(8.))
                .flex()
                .items_center()
                .whitespace_nowrap()
                .text_ellipsis()
                .text_size(theme::fs(10.))
                .text_color(theme::text_tertiary())
                .child(text.clone())
                .into_any_element(),
            Row::Item {
                action,
                label,
                icon: icon_name,
                checked,
            } => {
                let action = action.clone();
                let click = cx.listener(move |this, _: &MouseDownEvent, window, cx| {
                    // 先收起再干活：动作里可能要让面板重排，菜单还挂着会很乱。
                    this.hide(window, cx);
                    if let Some(target) = this.target.clone() {
                        crate::gui::run_app_menu_action(cx, &target, action.clone());
                    }
                });
                let hovered = self.hovered == Some(index);
                let color = if *checked {
                    theme::accent()
                } else {
                    theme::text_primary()
                };
                let mut row = div()
                    .id(gpui::ElementId::Name(
                        format!("app-menu-item-{index}").into(),
                    ))
                    .h(px(ROW_HEIGHT))
                    .flex_none()
                    .px(px(6.))
                    .flex()
                    .items_center()
                    .gap(px(ICON_GAP))
                    .rounded(px(theme::RADIUS_SMALL))
                    .cursor_pointer()
                    // 悬浮高亮由根节点的 `on_mouse_move` 算好行号传进来：
                    // 用同一份行号判断，高亮和点击永远不会错位。
                    .when(hovered, |style| style.bg(theme::surface_hover()))
                    .on_mouse_down(MouseButton::Left, click);
                row = match icon_name {
                    Some(name) => row.child(
                        div()
                            .w(px(ICON_SLOT))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .child(icon(name, ICON_SIZE, if *checked {
                                theme::accent()
                            } else {
                                theme::text_tertiary()
                            })),
                    ),
                    None => row.child(div().w(px(ICON_SLOT)).flex_none()),
                };
                row = row.child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .whitespace_nowrap()
                        .text_ellipsis()
                        .text_size(theme::fs(12.))
                        .text_color(color)
                        .child(label.clone()),
                );
                if *checked {
                    row = row.child(
                        div()
                            .size(px(6.))
                            .flex_none()
                            .rounded_full()
                            .bg(theme::accent()),
                    );
                }
                row.into_any_element()
            }
        }
    }
}

/// 找出包含这个点的显示器工作区（点不在任何显示器上时退回主屏）。
fn display_area(cx: &mut gpui::App, x: f32, y: f32) -> Bounds<gpui::Pixels> {
    let at = point(px(x), px(y));
    for display in cx.displays() {
        let area = display.visible_bounds();
        if area.contains(&at) {
            return area;
        }
    }
    cx.primary_display()
        .map(|display| display.visible_bounds())
        .unwrap_or_else(|| Bounds::new(point(px(0.), px(0.)), size(px(1920.), px(1080.))))
}

impl Render for AppMenu {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (width, height) = self.size;

        let mut card = div()
            .absolute()
            .left(px(0.))
            .top(px(0.))
            .w(px(width))
            .h(px(height))
            .flex()
            .flex_col()
            .p(px(PAD))
            .rounded(px(theme::RADIUS_SMALL + 2.0))
            .border_1()
            .border_color(theme::border_strong())
            .bg(theme::surface_elevated());
        for (index, row) in self.rows.iter().enumerate() {
            card = card.child(self.row_element(index, row, cx));
        }

        div()
            .relative()
            .w(px(width))
            .h(px(height))
            .flex_none()
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, _window, cx| {
                let hit = this.hit_test(event.position.y.as_f32());
                if hit != this.hovered {
                    this.hovered = hit;
                    cx.notify();
                }
            }))
            .child(card)
    }
}
