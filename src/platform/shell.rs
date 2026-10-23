//! 系统声音相关的「外部入口」：旧版音量合成器、mmsys.cpl 各标签页、ms-settings 设置页。
//!
//! 全部照 EarTrumpet 的 `LegacyControlPanelHelper` / `SettingsPageHelper` 来：
//! - 旧版合成器：`sndvol.exe -a <MAKELPARAM(cursor.x, cursor.y)>`，弹在鼠标位置；
//! - 播放/录制/声音：`rundll32 shell32.dll,Control_RunDLL mmsys.cpl,,<tab>`；
//! - 声音设置 / 音量合成器：`ms-settings:sound` / `ms-settings:apps-volume`。
//!
//! **一律在后台线程里拉起来**，不许在 UI 线程直接调 —— 见 `spawn_background` 的注释。

use std::process::Command;

use windows::Win32::Foundation::POINT;
use windows::Win32::System::Com::{COINIT_APARTMENTTHREADED, CoInitializeEx, CoUninitialize};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, SW_SHOWNORMAL};
use windows::core::{HSTRING, PCWSTR};

/// 把「拉起另一个程序」这件事挪到后台线程。
///
/// 绝不能在当前线程（UI 线程）里直接 `ShellExecuteW`：
/// 1. 它要等目标程序接手，`ms-settings:` 冷启动能停上一两秒，这期间窗口不响应；
/// 2. 更要命的是它内部会跑消息循环（栈里是 `SHChangeNotifyRegister` → 我们窗口的
///    `window_procedure`），gpui 会顺手把排队的前台任务派发掉 —— 而我们此刻**正处在**
///    `cx.update(..)` 里面（App 已被借用），任务里再调 `AsyncApp::update` 就是
///    `RefCell already borrowed` → 直接 panic → 释放版 `panic = "abort"` 一崩了之。
///    实测点托盘「声音设置 / 音量合成器」必崩，顺带鼠标钩子也卡住。
///
/// 后台线程要自己做 COM 初始化：`ShellExecuteW` 是 COM 那套（STA）。
fn spawn_background(f: impl FnOnce() + Send + 'static) {
    std::thread::spawn(move || {
        // 已经初始化过（极少见）就别再 Uninitialize 一次，免得把别人的账销了。
        let initialized = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) }.is_ok();
        f();
        if initialized {
            unsafe { CoUninitialize() };
        }
    });
}

/// 旧版音量合成器（Win32 的 sndvol），窗口出现在鼠标位置。
pub fn legacy_mixer() {
    // 光标位置必须**现在**取：线程跑起来时鼠标早挪走了。
    let mut point = POINT::default();
    let (x, y) = unsafe {
        if GetCursorPos(&mut point).is_ok() {
            (point.x, point.y)
        } else {
            (0, 0)
        }
    };
    // MAKELPARAM：低 16 位 x、高 16 位 y。
    let position = ((y as u32) << 16) | (x as u32 & 0xFFFF);
    spawn_background(move || {
        let _ = Command::new("sndvol.exe")
            .arg("-a")
            .arg(position.to_string())
            .spawn();
    });
}

/// 旧版「声音」控制面板的某个标签页：playback / recording / sounds。
pub fn legacy_sound_panel(tab: &str) {
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string());
    let rundll = format!("{root}\\System32\\rundll32.exe");
    let tab = tab.to_string();
    spawn_background(move || {
        let _ = Command::new(rundll)
            .arg("shell32.dll,Control_RunDLL")
            .arg(format!("mmsys.cpl,,{tab}"))
            .spawn();
    });
}

/// 打开 ms-settings 里的页面（sound / apps-volume）。
pub fn settings_page(page: &str) {
    let uri = format!("ms-settings:{page}");
    spawn_background(move || {
        let uri = HSTRING::from(uri);
        let operation = HSTRING::from("open");
        unsafe {
            ShellExecuteW(
                None,
                &operation,
                &uri,
                PCWSTR::null(),
                PCWSTR::null(),
                SW_SHOWNORMAL,
            );
        }
    });
}
