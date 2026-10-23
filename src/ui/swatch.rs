//! 「跟随」那个色块。
//!
//! 色板里第一个方块的含义是「什么都不设，跟着走」：强调色那一格跟的是
//! **Windows 的强调色**，底色那一格跟的是这套主题自己的底色。它以前画成一整块
//! 主题底色，混在一排实色方块的中间完全看不出身份 —— 用户得靠「第一个是跟随」
//! 这句话去数格子。这里把它画成**一半深、一半浅**，不用读任何说明，一眼就知道
//! 这一格不是某个具体颜色。
//!
//! 设置页和 OSD 编辑器共用这一份，两边不用各写各的。
//!
//! **两个半块是画出来的，不是「两块方 div + `overflow_hidden`」**：gpui 的
//! `overflow_hidden` 只按矩形裁，不认圆角，于是方块会从四个圆角里顶出来 ——
//! 深色那半边一顶，看上去就是「黑色部分超出去了」。这里按「左半只圆左边两个角、
//! 右半只圆右边两个角」自己铺两个四边形，描边外圈再套一层，越界在结构上就不存在。

use gpui::prelude::*;
use gpui::{BorderStyle, Bounds, Corners, Div, canvas, div, point, px, size, transparent_black};

use super::theme;

/// 深档 / 浅档两个半块的颜色：不跟主题走，永远是这个样子 ——
/// 它要表达的就是「两种主题都归它管」，自己再跟着主题变色就说不清了。
fn dark_half() -> gpui::Hsla {
    theme::parse_hex("#1b1b20").unwrap_or_else(theme::surface)
}

fn light_half() -> gpui::Hsla {
    theme::parse_hex("#f2f2f5").unwrap_or_else(theme::surface)
}

/// 一块「跟随」的小方块。
///
/// `side` 是边长，`radius` 圆角，`selected` 选中时描边用强调色（和实色方块一致）。
/// 返回的是普通 `Div`，调用方自己接 `.id()` / 点击事件。
pub fn follow_swatch(side: f32, radius: f32, selected: bool) -> Div {
    // 内圈圆角比外圈小一个描边的厚度：两个半块是从描边内侧铺出去的。
    let inner = (radius - 1.0).max(0.0);
    div()
        .relative()
        .flex_none()
        .w(px(side))
        .h(px(side))
        .rounded(px(radius))
        .border_1()
        .border_color(if selected {
            theme::accent()
        } else {
            theme::border_strong()
        })
        .child(
            canvas(
                |_, _, _| {},
                move |bounds, _, window, _| {
                    let half = bounds.size.width.as_f32() / 2.0;
                    let height = bounds.size.height.as_f32();
                    let left = Corners {
                        top_left: px(inner),
                        top_right: px(0.),
                        bottom_right: px(0.),
                        bottom_left: px(inner),
                    };
                    let right = Corners {
                        top_left: px(0.),
                        top_right: px(inner),
                        bottom_right: px(inner),
                        bottom_left: px(0.),
                    };
                    paint_half(window, bounds.origin, half, height, left, dark_half());
                    paint_half(
                        window,
                        point(bounds.origin.x + px(half), bounds.origin.y),
                        half,
                        height,
                        right,
                        light_half(),
                    );
                },
            )
            .absolute()
            .left_0()
            .top_0()
            .size_full(),
        )
}

/// 铺半块：只圆自己那一侧的两个角。
fn paint_half(
    window: &mut gpui::Window,
    origin: gpui::Point<gpui::Pixels>,
    width: f32,
    height: f32,
    corners: Corners<gpui::Pixels>,
    color: gpui::Hsla,
) {
    window.paint_quad(gpui::quad(
        Bounds::new(origin, size(px(width), px(height))),
        corners,
        color,
        px(0.),
        transparent_black(),
        BorderStyle::default(),
    ));
}
