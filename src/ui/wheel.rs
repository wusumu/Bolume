//! 滚轮的公共换算。
//!
//! 音量面板（设备行 / 应用行）和显示器亮度窗都用这一套：只看**方向**，
//! 一格的位移量不管是 1 行还是几十像素，都当成「一格」。
//! 万一将来要支持「转得快就多走几档」，只改这一个函数，两处一起生效。

use gpui::ScrollDelta;

/// 这次滚动相当于几格：往上/往右是正，往下/往左是负，不动是 0。
pub fn wheel_direction(delta: ScrollDelta) -> f32 {
    match delta {
        ScrollDelta::Lines(delta) => delta.y,
        ScrollDelta::Pixels(delta) => f32::from(delta.y),
    }
    .signum()
}
