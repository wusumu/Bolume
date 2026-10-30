pub mod app_icon;
pub mod app_pick;
pub mod autostart;
pub mod bootstart;
pub mod detach;
pub mod fonts;
pub mod fullscreen;
pub mod hotkey;
pub mod legacy_osd;
pub mod logfile;
pub mod media;
pub mod memory;
pub mod monitors;
pub mod mouse_wheel;
pub mod radio;
pub mod restart;
pub mod shell;
pub mod single_instance;
pub mod sound;
pub mod sound_icons;
pub mod system_osd;
pub mod system_theme;
pub mod winver;

pub use single_instance::SingleInstance;

/// 「`duration` 以前」的那个时间点 —— 给「上次扫描 / 上次弹出」这类字段铺初值。
///
/// 不要直接写 `Instant::now() - duration`。Windows 上 `Instant` 的起点是性能计数器
/// （即系统启动那一刻），开机没多久就启动本程序时这个值可能还不够减，减法会 panic，
/// release 编的是 `panic = "abort"`：进程当场消失，界面上就是「点了没反应、半天不
/// 启动」，只在 crash.log 里留一行 `overflow when subtracting duration from instant`。
///
/// 减不动时把「现在」当成那个很久以前的点：调用方顶多把首轮干活推迟一个周期。
pub fn ago(duration: std::time::Duration) -> std::time::Instant {
    let now = std::time::Instant::now();
    now.checked_sub(duration).unwrap_or(now)
}
