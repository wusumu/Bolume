//! 托盘右键菜单的「跟随主题」版本。
//!
//! 原生右键菜单是 `tray-icon`（muda）弹出来的 Win32 菜单：底色跟着系统、字号固定、
//! 宽度随文字自己长、圆角高亮一概改不了。这一版自己开一扇无边框弹窗画菜单，
//! 底色 / 文字 / 强调色 / 字号全跟主题走，宽度还能按 `tray_menu_max_width` 截断
//! （超出部分用 … 收尾，鼠标悬停时工具提示里仍是全名）。
//!
//! 和原生版的两处手感差别，都是刻意的：
//! - 子菜单不弹第二扇窗，而是在同一扇窗右侧展开一列 —— 两扇窗总有一帧对不齐；
//! - 菜单窗口会被激活（`activate`），靠「失焦」收起，所以点屏幕任何别的地方都会关。

use std::cell::RefCell;
use std::rc::Rc;

use gpui::prelude::*;
use gpui::{
    AnyElement, Bounds, Context, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent, Render,
    Window, div, point, px, size,
};

use crate::audio::Snapshot;
use crate::config::ConfigStore;
use crate::ui::{icon, theme};

/// 菜单里一行的高度（和面板里那个右键菜单同一档观感）。
const ROW_HEIGHT: f32 = 24.0;
/// 卡片内边距。
const PAD: f32 = 5.0;
/// 主列与子菜单列之间的间隙 + 菜单与屏幕边的留白。
const GAP: f32 = 4.0;
const EDGE_MARGIN: f32 = 4.0;
/// 再窄就摆不下设备名了（`tray_menu_max_width` 也不会低于它）。
const MIN_WIDTH: f32 = 168.0;
/// 分隔线占的高度（1px 线 + 上下留白）。
const SEPARATOR_HEIGHT: f32 = 9.0;
/// 行首图标块宽度：有图标的行和没图标的行文字左沿要齐。
const ICON_SLOT: f32 = 20.0;
/// 图标与文字之间的间隙。
const ICON_GAP: f32 = 6.0;
/// 行首图标尺寸。
///
/// 13 那档等于把 24×24 线条图的描边压到 0.95px，抗锯齿糊成一团灰；
/// 24 高的菜单行里 16 是还能留出上下各 4px 的最大值。
const ICON_SIZE: f32 = 16.0;
/// 行尾预留（勾选圆点 / 子菜单箭头）。
const TAIL: f32 = 16.0;

/// 菜单里的一行。
#[derive(Clone, Debug)]
enum Row {
    Separator,
    /// 一句话占位（没有可用设备时）。
    Notice(String),
    Device {
        id: String,
        label: String,
        /// 设备图标名：可能是配置里的自定义图标，所以是 `String` 不是 `&'static str`。
        icon: String,
        checked: bool,
    },
    Action {
        id: String,
        label: String,
        /// 动作图标；认不出的 id 就是 `None`（只留空位，文字仍对齐）。
        icon: Option<&'static str>,
    },
    Folder {
        label: String,
        children: Vec<Row>,
    },
}

impl Row {
    fn height(&self) -> f32 {
        match self {
            Row::Separator => SEPARATOR_HEIGHT,
            _ => ROW_HEIGHT,
        }
    }

    fn is_folder(&self) -> bool {
        matches!(self, Row::Folder { .. })
    }

    /// 这一行有多宽（逻辑像素，含内边距）。用来定菜单宽度。
    fn width(&self) -> f32 {
        PAD * 2.0
            + match self {
                Row::Separator => 24.0,
                Row::Notice(text) => text_width(text, 11.5) + TAIL,
                Row::Device { label, .. } | Row::Action { label, .. } => {
                    ICON_SLOT + ICON_GAP + text_width(label, 12.0) + TAIL
                }
                Row::Folder { label, .. } => {
                    ICON_SLOT + ICON_GAP + text_width(label, 12.0) + TAIL + 8.0
                }
            }
    }
}

/// 粗估一段文字的宽度：ASCII 约 0.55 个字高，中文等全角按一个字高算。
///
/// 自绘菜单要按内容定宽，而 gpui 的排版结果得等一帧才知道 —— 与其量一遍再改窗口
/// 尺寸（会看到菜单先窄后宽地跳一下），不如先估：估偏了也只是多几像素留白。
fn text_width(text: &str, size: f32) -> f32 {
    text.chars()
        .map(|ch| if ch.is_ascii() { size * 0.55 } else { size })
        .sum()
}

pub struct TrayMenu {
    config: Rc<RefCell<ConfigStore>>,
    snapshot: Rc<RefCell<Snapshot>>,
    rows: Vec<Row>,
    /// 当前展开的子菜单（`rows` 里的下标）。
    open: Option<usize>,
    visible: bool,
    /// 主列 / 窗口尺寸（逻辑像素）。`render` 与 `update_frame` 都用它。
    main_width: f32,
    main_height: f32,
    frame: (f32, f32),
    /// 子菜单卡片顶端（对齐到展开那一行）。
    open_top: f32,
    /// 每一行的顶端 y（鼠标落在哪一行靠它判断）。
    row_tops: Vec<f32>,
    /// 光标压在主列哪一行上（悬浮高亮；子菜单列用 `hovered_sub`）。
    ///
    /// 自己算而不是只靠 `div().hover()`：菜单里同时还要判断「这一行要不要展开子菜单」，
    /// 两套判断各算一遍迟早对不上；顺带也躲开 gpui 在指针直接划出窗口时不回调 hover 的坑。
    hovered: Option<usize>,
    hovered_sub: Option<usize>,
    /// 上次弹出的时刻。
    ///
    /// 失焦收起靠窗口激活事件（比轮询快一帧），但刚弹出时 `SetForegroundWindow`
    /// 还没落地，系统可能先报一次「未激活」—— 那几百毫秒内的失焦不算数。
    shown_at: std::time::Instant,
}

impl TrayMenu {
    pub fn new(config: Rc<RefCell<ConfigStore>>, snapshot: Rc<RefCell<Snapshot>>) -> Self {
        Self {
            config,
            snapshot,
            rows: Vec::new(),
            open: None,
            visible: false,
            main_width: MIN_WIDTH,
            main_height: ROW_HEIGHT,
            frame: (MIN_WIDTH, ROW_HEIGHT),
            open_top: 0.0,
            row_tops: Vec::new(),
            hovered: None,
            hovered_sub: None,
            shown_at: crate::platform::ago(std::time::Duration::from_secs(1)),
        }
    }

    /// 挂上「失焦立刻收起」。
    ///
    /// 之前只靠 gui 那边 90ms 一轮的轮询，加上复核要再等 120ms，实测点别处以后
    /// 菜单要在屏上多停小半秒。窗口激活事件是系统推过来的，收到就收，中间没有等待。
    pub fn watch_blur(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut was_active = false;
        cx.observe_window_activation(window, move |this, window, cx| {
            let active = window.is_window_active();
            // 刚弹出来的头 120ms 不认失焦：`activate` 是异步生效的，
            // 有时候会先收到一次「未激活」，照着收就等于菜单一闪就没。
            if was_active
                && !active
                && this.visible
                && this.shown_at.elapsed() >= std::time::Duration::from_millis(120)
            {
                this.hide(window, cx);
            }
            was_active = active;
        })
        .detach();
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// 按当前配置 + 设备快照重建行列表（每次弹出都重建：设备 / 语言 / 布局都会变）。
    fn rebuild_rows(&mut self) {
        let cfg = self.config.borrow().get().clone();
        let devices = self.snapshot.borrow().devices.clone();
        self.rows = build_rows(&cfg, &devices);
        self.open = None;
        self.hovered = None;
        self.hovered_sub = None;
    }

    /// 算窗口尺寸：内容宽度夹到 `tray_menu_max_width`，高度按行数累加。
    fn measure(&mut self) {
        let cfg = self.config.borrow().get().clone();
        let max_width = (cfg.tray_menu_max_width as f32).max(MIN_WIDTH);

        let mut width = MIN_WIDTH;
        let mut y = PAD;
        self.row_tops.clear();
        for row in &self.rows {
            self.row_tops.push(y);
            y += row.height();
            width = width.max(row.width());
        }
        let main_width = width.min(max_width);
        let main_height = y + PAD;
        self.main_width = main_width;
        self.main_height = main_height;

        let mut frame = (main_width, main_height);
        self.open_top = 0.0;
        if let Some(index) = self.open {
            if let Some(Row::Folder { children, .. }) = self.rows.get(index) {
                let sub_width = children
                    .iter()
                    .map(Row::width)
                    .fold(MIN_WIDTH, f32::max)
                    .min(max_width);
                let sub_height = PAD * 2.0 + children.iter().map(Row::height).sum::<f32>();
                self.open_top = self.row_tops.get(index).copied().unwrap_or(PAD);
                frame.0 = main_width + GAP + sub_width;
                frame.1 = main_height.max(self.open_top + sub_height);
            }
        }
        self.frame = frame;
    }

    /// 鼠标此刻压在哪一行上：（主列行号, 子菜单列行号）。
    ///
    /// 主列右侧是子菜单列，两边各自算 —— 子菜单里的行不参与「要不要换展开项」的判断。
    fn hit_test(&self, x: f32, y: f32) -> (Option<usize>, Option<usize>) {
        if x > self.main_width {
            let Some(index) = self.open else {
                return (None, None);
            };
            let Some(Row::Folder { children, .. }) = self.rows.get(index) else {
                return (None, None);
            };
            let mut top = self.open_top + PAD;
            for (child_index, child) in children.iter().enumerate() {
                if y >= top && y < top + child.height() {
                    return (None, Some(child_index));
                }
                top += child.height();
            }
            return (None, None);
        }
        for (index, row) in self.rows.iter().enumerate() {
            let top = self.row_tops.get(index).copied().unwrap_or(f32::MAX);
            if y >= top && y < top + row.height() {
                return (Some(index), None);
            }
        }
        (None, None)
    }

    /// 光标离开窗口时把悬浮态清掉（窗口还挂着高亮会很怪）。
    pub fn clear_hover(&mut self, cx: &mut Context<Self>) {
        if self.hovered.is_some() || self.hovered_sub.is_some() {
            self.hovered = None;
            self.hovered_sub = None;
            cx.notify();
        }
    }

    /// 贴着托盘图标（光标）摆：任务栏在下面就往上游，在上面就往下游。
    fn update_frame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let area = cx
            .primary_display()
            .map(|display| display.visible_bounds())
            .unwrap_or_else(|| Bounds::new(point(px(0.), px(0.)), size(px(1920.), px(1080.))));
        let (cursor_x, cursor_y) = crate::windows::platform::cursor_logical(window.scale_factor());
        let (width, height) = self.frame;

        let left = area.origin.x.as_f32() + EDGE_MARGIN;
        let right = area.origin.x.as_f32() + area.size.width.as_f32() - EDGE_MARGIN;
        let top = area.origin.y.as_f32() + EDGE_MARGIN;
        let bottom = area.origin.y.as_f32() + area.size.height.as_f32() - EDGE_MARGIN;

        let (mut x, mut y) = match crate::windows::platform::taskbar_edge() {
            crate::windows::platform::TaskbarEdge::Bottom => (
                cursor_x - width / 2.0,
                cursor_y - height - EDGE_MARGIN * 1.5,
            ),
            crate::windows::platform::TaskbarEdge::Top => {
                (cursor_x - width / 2.0, cursor_y + EDGE_MARGIN * 1.5)
            }
            crate::windows::platform::TaskbarEdge::Left => {
                (cursor_x + EDGE_MARGIN * 1.5, cursor_y - height / 2.0)
            }
            crate::windows::platform::TaskbarEdge::Right => (
                cursor_x - width - EDGE_MARGIN * 1.5,
                cursor_y - height / 2.0,
            ),
        };
        // 贴边夹一下：展开子菜单后窗口会变宽，右下角别跑到屏幕外。
        x = x.clamp(left, (right - width).max(left));
        y = y.clamp(top, (bottom - height).max(top));

        crate::windows::platform::set_frame(
            window,
            Bounds::new(point(px(x), px(y)), size(px(width), px(height))),
            window.scale_factor(),
        );
        window.bounds_changed(cx);
    }

    pub fn show(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.rebuild_rows();
        self.measure();
        self.shown_at = std::time::Instant::now();
        self.visible = true;
        cx.defer_in(window, |this, window, cx| {
            // 跟面板同一套顺序：先全透明再露头，否则 DWM 会先用上一帧（可能是另一套
            // 设备列表）合成一帧，看着就是「菜单先闪一下旧内容」。
            crate::windows::platform::set_opacity(window, 0.0);
            this.update_frame(window, cx);
            crate::windows::platform::set_visible(window, true);
            crate::windows::platform::bring_to_front(window);
            crate::windows::platform::activate(window);
            cx.notify();
            cx.defer_in(window, |this, window, cx| {
                crate::windows::platform::set_opacity(window, 1.0);
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
        self.open = None;
        cx.defer_in(window, |this, window, _| {
            crate::windows::platform::set_visible(window, false);
            crate::windows::platform::set_opacity(window, 0.0);
            this.visible = false;
        });
        cx.notify();
    }

    fn hide_and(&mut self, window: &mut Window, cx: &mut Context<Self>, id: &str) {
        // 先收起再干活：设置页 / 面板弹出来时菜单不该还压在上面。
        self.hide(window, cx);
        crate::gui::run_menu_action(cx, id);
    }

    /// 一行：图标 + 文字（+ 勾选点 / 子菜单箭头）。
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
            Row::Notice(text) => div()
                .h(px(ROW_HEIGHT))
                .flex_none()
                .px(px(8.))
                .flex()
                .items_center()
                .text_size(theme::fs(11.5))
                .text_color(theme::text_tertiary())
                .child(text.clone())
                .into_any_element(),
            Row::Device {
                id,
                label,
                icon: name,
                checked,
            } => {
                let id = id.clone();
                let click = cx.listener(move |this, _: &MouseUpEvent, window, cx| {
                    this.hide_and(window, cx, &id);
                });
                self.item_row(
                    Some(name.as_str()),
                    label,
                    if *checked {
                        theme::accent()
                    } else {
                        theme::text_primary()
                    },
                    Some(*checked),
                    None,
                    self.hovered == Some(index),
                    click,
                )
            }
            Row::Action {
                id,
                label,
                icon: action_icon,
            } => {
                let id = id.clone();
                let click = cx.listener(move |this, _: &MouseUpEvent, window, cx| {
                    this.hide_and(window, cx, &id);
                });
                self.item_row(
                    *action_icon,
                    label,
                    theme::text_primary(),
                    None,
                    None,
                    self.hovered == Some(index),
                    click,
                )
            }
            Row::Folder { label, .. } => {
                let open = self.open == Some(index);
                // 点在子菜单标题上就地收起，别让人以为点了没反应。
                let click = cx.listener(move |this, _: &MouseUpEvent, _, cx| {
                    this.open = if open { None } else { Some(index) };
                    cx.notify();
                });
                self.item_row(
                    Some("folder"),
                    label,
                    if open {
                        theme::accent()
                    } else {
                        theme::text_primary()
                    },
                    None,
                    Some(open),
                    // 子菜单展开着的那一行也保持高亮，否则「点了哪个子菜单」得靠颜色猜。
                    self.hovered == Some(index) || open,
                    click,
                )
            }
        }
    }

    /// 子菜单里的一行（只有动作，没有设备 / 子菜单）。
    fn sub_row_element(&self, index: usize, row: &Row, cx: &mut Context<Self>) -> AnyElement {
        match row {
            Row::Separator => div()
                .h(px(SEPARATOR_HEIGHT))
                .flex_none()
                .flex()
                .items_center()
                .px(px(6.))
                .child(div().w_full().h(px(1.)).bg(theme::border()))
                .into_any_element(),
            Row::Action {
                id,
                label,
                icon: action_icon,
            } => {
                let id = id.clone();
                let click = cx.listener(move |this, _: &MouseUpEvent, window, cx| {
                    this.hide_and(window, cx, &id);
                });
                self.item_row(
                    *action_icon,
                    label,
                    theme::text_primary(),
                    None,
                    None,
                    self.hovered_sub == Some(index),
                    click,
                )
            }
            other => div()
                .h(px(ROW_HEIGHT))
                .flex_none()
                .px(px(8.))
                .flex()
                .items_center()
                .text_size(theme::fs(12.))
                .text_color(theme::text_primary())
                .child(match other {
                    Row::Device { label, .. }
                    | Row::Action { label, .. }
                    | Row::Folder { label, .. }
                    | Row::Notice(label) => label.clone(),
                    Row::Separator => String::new(),
                })
                .into_any_element(),
        }
    }

    /// 通用的菜单行外壳：悬停高亮、按下执行、可选右侧标记。
    fn item_row(
        &self,
        icon_name: Option<&str>,
        label: &str,
        color: gpui::Hsla,
        checked: Option<bool>,
        arrow: Option<bool>,
        hovered: bool,
        onclick: impl Fn(&MouseUpEvent, &mut Window, &mut gpui::App) + 'static,
    ) -> AnyElement {
        let mut row = div()
            .h(px(ROW_HEIGHT))
            .flex_none()
            .px(px(6.))
            .flex()
            .items_center()
            .gap(px(ICON_GAP))
            .rounded(px(theme::RADIUS_SMALL))
            .cursor_pointer()
            // 悬浮高亮由根节点的 `on_mouse_move` 算好行号传进来：菜单里还要判断
            // 「这一行要不要展开子菜单」，用同一份行号判断，高亮和展开永远一致。
            .when(hovered, |style| style.bg(theme::surface_hover()))
            // 按下只高亮，**抬起**才执行 —— 跟原生菜单一致：按错了可以把光标
            // 拖出去再松手，这一项就不会被触发。
            .on_mouse_up(MouseButton::Left, onclick);
        // 认不出的动作只留同样宽的空坑，文字左沿才不会跟着跳。
        row = row.child(match icon_name {
            Some(name) => div()
                .w(px(ICON_SLOT))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .child(icon(name, ICON_SIZE, color)),
            None => div().w(px(ICON_SLOT)).flex_none(),
        });
        row = row.child(
            div()
                .flex_1()
                .min_w_0()
                .text_size(theme::fs(12.))
                .text_color(color)
                .whitespace_nowrap()
                .text_ellipsis()
                .child(label.to_string()),
        );
        if checked == Some(true) {
            row = row.child(
                div()
                    .size(px(6.))
                    .flex_none()
                    .rounded_full()
                    .bg(theme::accent()),
            );
        }
        if let Some(open) = arrow {
            row = row.child(
                div()
                    .w(px(10.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_size(theme::fs(11.))
                    .text_color(if open {
                        theme::accent()
                    } else {
                        theme::text_tertiary()
                    })
                    .child("›"),
            );
        }
        row.into_any_element()
    }
}

impl Render for TrayMenu {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let (width, height) = self.frame;

        let mut main = div()
            .absolute()
            .left(px(0.))
            .top(px(0.))
            .w(px(self.main_width))
            .h(px(self.main_height))
            .flex()
            .flex_col()
            .p(px(PAD))
            .rounded(px(theme::RADIUS_SMALL + 2.0))
            .border_1()
            .border_color(theme::border_strong())
            .bg(theme::surface_elevated())
            // 卡片吃掉按下事件：点卡片空白处不该把菜单收掉（那一层是根节点的事）。
            .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation());

        for (index, row) in self.rows.iter().enumerate() {
            main = main.child(self.row_element(index, row, cx));
        }

        let mut root = div()
            .relative()
            .w(px(width))
            .h(px(height))
            .flex_none()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _: &MouseDownEvent, window, cx| {
                    // 点到卡片外面的透明区域 = 收起。
                    this.hide(window, cx);
                }),
            )
            .on_mouse_move(cx.listener(|this, event: &MouseMoveEvent, window, cx| {
                let cursor_x = event.position.x.as_f32();
                let (hovered, hovered_sub) = this.hit_test(cursor_x, event.position.y.as_f32());
                let mut dirty = false;
                if (hovered, hovered_sub) != (this.hovered, this.hovered_sub) {
                    this.hovered = hovered;
                    this.hovered_sub = hovered_sub;
                    dirty = true;
                }
                // 只有压在「子菜单那一行」上才展开；移到主列别的行就收起来。
                // 光标已经在右侧子菜单列里时保持原样 —— 这里若跟着 hovered(空) 走，
                // 一移进子菜单就会被收掉，等于子菜单根本点不到。
                let next = if cursor_x > this.main_width {
                    this.open
                } else {
                    hovered
                        .filter(|index| this.rows.get(*index).map(Row::is_folder).unwrap_or(false))
                };
                if next != this.open {
                    this.open = next;
                    this.measure();
                    cx.defer_in(window, |this, window, cx| {
                        this.update_frame(window, cx);
                        cx.notify();
                    });
                    dirty = true;
                }
                if dirty {
                    cx.notify();
                }
            }))
            .child(main);

        if let Some(Row::Folder { children, .. }) = self.open.and_then(|index| self.rows.get(index))
        {
            let children = children.clone();
            let mut sub = div()
                .absolute()
                .left(px(self.main_width + GAP))
                .top(px(self.open_top))
                .flex()
                .flex_col()
                .p(px(PAD))
                .rounded(px(theme::RADIUS_SMALL + 2.0))
                .border_1()
                .border_color(theme::border_strong())
                .bg(theme::surface_elevated())
                .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation());
            for (index, row) in children.iter().enumerate() {
                sub = sub.child(self.sub_row_element(index, row, cx));
            }
            root = root.child(sub);
        }

        root
    }
}

/// 把配置里的菜单布局摊平成行列表（设备块会展开成一串设备行）。
/// 菜单动作对应的图标（`assets/icons/<name>.svg`）。
///
/// 只认 `gui::tray_action_label` 里那张 id 表；配置里混进别的 id 时返回 `None`，
/// 那一行只留空位 —— 不能拿一个「看起来像图标」的占位图糊过去。
///
/// 新旧两个入口指向同一样东西，就用同一张图：`音量合成器 [旧]` 和 `音量合成器`
/// 都是推子，`声音设置 [旧]` 和 `声音设置` 都是那个对话框。同一个功能配两种图形，
/// 只会让人以为它们干的事不一样。
fn action_icon(id: &str) -> Option<&'static str> {
    Some(match id {
        // 弹面板用「窗口 + 底栏」：这块东西就是从屏幕底部弹出来的。
        "toggle" => "panel",
        "apps-volume" | "legacy-mixer" => "adjustments",
        "sound-settings" | "legacy-sounds" => "dialog",
        "legacy-playback" => "headphones",
        "legacy-recording" => "microphone",
        "restart" => "restart",
        "settings" => "settings",
        "quit" => "power",
        _ => return None,
    })
}

fn build_rows(cfg: &crate::config::Config, devices: &[crate::audio::DeviceInfo]) -> Vec<Row> {
    let lang = cfg.language;
    let mut rows: Vec<Row> = Vec::new();

    for node in &cfg.tray_menu.root {
        if !node.visible {
            continue;
        }
        if node.id == "devices" {
            let mut listed = false;
            for device in devices.iter().filter(|device| device.enabled) {
                let label = if device.name.trim().is_empty() {
                    cfg.device_label(&device.id, &device.id)
                } else {
                    cfg.device_label(&device.id, &device.name)
                };
                rows.push(Row::Device {
                    id: format!("device:{}", device.id),
                    label,
                    icon: crate::ui::device_icon_resolved(cfg, &device.id, device.kind)
                        .into_owned(),
                    checked: device.is_default,
                });
                listed = true;
            }
            if !listed {
                rows.push(Row::Notice(
                    lang.pick("没有可用的播放设备", "No playback devices")
                        .to_string(),
                ));
            }
            continue;
        }
        // 分隔线是配置里的条目：插了几条就是几条（渲染时不自动补）。
        if node.is_separator() {
            rows.push(Row::Separator);
            continue;
        }
        if node.is_folder() {
            let children: Vec<Row> = node
                .children
                .iter()
                .filter(|child| child.visible)
                .filter_map(|child| {
                    crate::gui::tray_action_label(lang, &child.id).map(|label| Row::Action {
                        id: child.id.clone(),
                        label: label.to_string(),
                        icon: action_icon(&child.id),
                    })
                })
                .collect();
            // 空子菜单不进菜单：点开啥都没有的子菜单很怪（和原生版一致）。
            if children.is_empty() {
                continue;
            }
            rows.push(Row::Folder {
                label: node.title.clone(),
                children,
            });
            continue;
        }
        if let Some(label) = crate::gui::tray_action_label(lang, &node.id) {
            rows.push(Row::Action {
                id: node.id.clone(),
                label: label.to_string(),
                icon: action_icon(&node.id),
            });
        }
    }

    // 「退出」前面那条线不再自动补：要隔开就在菜单编辑器里自己插一条。
    rows
}
