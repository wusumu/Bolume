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
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress};
use windows::Win32::UI::Shell::ShellExecuteW;
use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, SW_SHOWNORMAL};
use windows::core::{HSTRING, PCWSTR, s, w};

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

/// 打开「蓝牙设备面板」—— 快速设置里那张小面板：标题「蓝牙」，下面列已配对设备。
///
/// Win11 支持**带参数的快速设置**：`ms-controlcenter:bluetooth` 会让面板
/// **直接以蓝牙页打开**。用它的好处是两头都省：
///
/// * 用户看不到中间那张磁贴总面板。对比 `ms-controlcenter:`（不带参数）只到磁贴页，
///   还得再点一次磁贴上的展开按钮；而点那一下要等面板画出来（UIA 树几十毫秒就绪，
///   第一帧画面要一百多毫秒），中间状态总会露出来；
/// * 不用 UI Automation 去点系统 UI —— 少一层跨进程调用，也少一种会随系统更新失效的做法。
///
/// Win10 没有这一层：操作中心里的蓝牙磁贴只能开关，设备列表只在设置页里，
/// 所以退回 `ms-settings:bluetooth`。
pub fn bluetooth_panel() {
    if windows_build() >= 22_000 {
        spawn_background(|| open_control_center("bluetooth"));
    } else {
        settings_page("bluetooth");
    }
}

/// 打开快速设置，并让它直接停在某个磁贴的详情页。
///
/// `action` 用磁贴名的小写形式（`bluetooth` / `wifi` / `airplanemode` 都认，实测
/// Win11 26200）；认不出来就退回磁贴页，不会报错。
///
/// 不用 `ShellExecuteW("ms-controlcenter:…")`：那条路走完整的 shell 执行流程，
/// 光是派发出去就要 200ms 上下（实测 211ms）。协议处理器注册的命令是固定的
/// `%SystemRoot%\System32\ShellHost.exe url=%1`，直接起它只要几毫秒。
/// 起不来再退回 `ShellExecuteW`（万一系统改了这条注册）。
fn open_control_center(action: &str) {
    let root = std::env::var("SystemRoot").unwrap_or_else(|_| "C:\\Windows".to_string());
    let host = format!("{root}\\System32\\ShellHost.exe");
    let spawned = Command::new(&host)
        .arg(format!("url=ms-controlcenter:{action}"))
        .spawn()
        .is_ok();
    if !spawned {
        open_uri_blocking(&format!("ms-controlcenter:{action}"));
    }
}

/// 同步打开一个 URI（调用方已经在后台线程里，这里不再派线程）。
fn open_uri_blocking(uri: &str) {
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
}

/// 当前 Windows 的 build 号（Win10 = 1xxxx，Win11 = 22000+）。
///
/// 不用 `GetVersionExW`：它从 8.1 起就被应用兼容层骗，不声明 manifest 一律回 6.2。
/// `RtlGetVersion` 是内核里那个，不做这层伪装。
fn windows_build() -> u32 {
    #[repr(C)]
    struct OsVersionInfoW {
        size: u32,
        major: u32,
        minor: u32,
        build: u32,
        platform: u32,
        csd_version: [u16; 128],
    }

    let mut info = OsVersionInfoW {
        size: std::mem::size_of::<OsVersionInfoW>() as u32,
        major: 0,
        minor: 0,
        build: 0,
        platform: 0,
        csd_version: [0; 128],
    };

    unsafe {
        // ntdll 是每个进程都加载着的，这两个调用不会失败；真失败就当 Win10 处理。
        let Ok(module) = GetModuleHandleW(w!("ntdll.dll")) else {
            return 0;
        };
        let Some(proc) = GetProcAddress(module, s!("RtlGetVersion")) else {
            return 0;
        };
        let func: unsafe extern "system" fn(*mut OsVersionInfoW) -> i32 = std::mem::transmute(proc);
        if func(&mut info) >= 0 {
            info.build
        } else {
            0
        }
    }
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

/// 用系统默认浏览器打开一个网址（设置页「关于 → 项目主页 / 问题反馈」用）。
///
/// 和上面几个一样走后台线程：浏览器的冷启动能停一两秒，
/// 在 UI 线程里直接调会把窗口卡住（更糟的情况见 `spawn_background` 的注释）。
pub fn open_url(url: &str) {
    let url = url.to_string();
    spawn_background(move || {
        let url = HSTRING::from(url);
        let operation = HSTRING::from("open");
        unsafe {
            ShellExecuteW(
                None,
                &operation,
                &url,
                PCWSTR::null(),
                PCWSTR::null(),
                SW_SHOWNORMAL,
            );
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 版本号读得到、且落在合理区间。
    ///
    /// `RtlGetVersion` 会老实地报 10.0.19045 / 10.0.26200 这种真版本，
    /// 如果哪天被换成 `GetVersionExW`，这里会读回 6.2（build 9200）而挂掉。
    #[test]
    fn windows_build_is_real() {
        let build = windows_build();
        eprintln!("windows build = {build}");
        assert!(build >= 7600, "读到的 build 是 {build}，不像真的");

        // 蓝牙面板按 22000 分界，顺手确认当前系统落在哪边（只做记录用）。
        let uri = if build >= 22_000 {
            "ms-controlcenter:"
        } else {
            "ms-actioncenter:"
        };
        assert!(uri.starts_with("ms-"));
    }
}
