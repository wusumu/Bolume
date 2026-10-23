//! 松手提示音：直接播系统「默认提示音」。
//!
//! 与 EarTrumpet 完全一致：它的 `SystemSoundsHelper` 里是
//! `System.Media.SystemSounds.Beep.Play()`，.NET 内部走的就是 `MessageBeep(MB_OK)`。
//! gpui 在 Windows 上的 `Window::play_system_bell()` 也正是这一句，
//! 所以这里直接转调，让系统按当前「声音」方案播放，不合成、不自带 wav。

use gpui::Window;

/// 播放一声系统提示音。异步，不阻塞界面线程。
pub fn tick(window: &Window) {
    window.play_system_bell();
}
