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

/// 打开「蓝牙设备面板」（就是托盘快速设置里点蓝牙磁贴箭头进去的那层，
/// 标题是「蓝牙」、里面有「新设备 / 更多蓝牙设置」）。
///
/// Windows 没有和 `ms-availablenetworks:` 对称的蓝牙飞出 URI：
/// - `ms-settings:bluetooth` 是**整页设置**（不是面板）；
/// - `ms-controlcenter:` 只到快速设置的**磁贴页**，还差一层。
///
/// 所以 Win11 上只能：先打开快速设置，再用 UI Automation 点蓝牙磁贴里的
/// 「管理蓝牙设备」按钮（AutomationId 固定为 `SplitL2Button`，挂在
/// `Microsoft.QuickAction.Bluetooth` 下面 —— **这两个 ID 不随系统语言变**，
/// 所以不用去匹配「管理蓝牙设备」这种会翻译的名字）。
///
/// Win10 没有这一层：操作中心里的蓝牙磁贴只能开关，设备列表只在设置页里，
/// 所以老系统退回 `ms-settings:bluetooth`。
pub fn bluetooth_panel() {
    if windows_build() >= 22_000 {
        spawn_background(|| {
            // 整个流程共用一个 UIA 实例：轮询里反复 CoCreateInstance 既慢又多余。
            let Some(automation) = ui_automation() else {
                return;
            };
            // 快速设置已经开着就直接点，别再多弹一次。
            if click_manage_bluetooth(&automation) {
                return;
            }
            open_uri_blocking("ms-controlcenter:");
            // 面板是 XAML 岛，弹出来要一两百毫秒，轮询等它把树建好。
            for _ in 0..30 {
                std::thread::sleep(std::time::Duration::from_millis(80));
                if click_manage_bluetooth(&automation) {
                    return;
                }
            }
            // 蓝牙磁贴被用户从快速设置里编辑掉了：至少退回设置页，别点了没反应。
            open_uri_blocking("ms-settings:bluetooth");
        });
    } else {
        settings_page("bluetooth");
    }
}

/// 建一个 UI Automation 根对象。失败（COM 没起、UIA 被策略关掉）返回 `None`。
fn ui_automation() -> Option<windows::Win32::UI::Accessibility::IUIAutomation> {
    use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
    use windows::Win32::UI::Accessibility::{CUIAutomation, IUIAutomation};

    unsafe {
        CoCreateInstance::<_, IUIAutomation>(&CUIAutomation, None, CLSCTX_INPROC_SERVER).ok()
    }
}

/// 快速设置面板的 UIA 标识。全部取自系统的 XAML 自动化 Peer，
/// **都是英文常量、不随系统语言变**，所以不用去匹配「蓝牙」「管理蓝牙设备」这类会翻译的文字。
mod uia {
    /// 快速设置顶层窗口的 `ClassName`。
    pub const CONTROL_CENTER_CLASS: &str = "ControlCenterWindow";
    /// 蓝牙磁贴的开关按钮 —— 磁贴页和详情页顶部那个开关**用的是同一个 ID**。
    pub const BLUETOOTH_TILE: &str = "Microsoft.QuickAction.Bluetooth";
    /// 磁贴右下角那个「展开」小按钮（挂在磁贴的 ListItem 上，是开关的兄弟节点）。
    pub const SPLIT_BUTTON: &str = "SplitL2Button";
    /// 二级页（详情页）的容器。
    pub const PAGE_WINDOW: &str = "PageWindow";
    /// 二级页左上角的返回箭头。
    pub const BACK_BUTTON: &str = "BackButton";
}

/// 在当前已打开的快速设置里点「管理蓝牙设备」，把面板带到蓝牙设备页。
///
/// 返回 `true` 表示「现在就在蓝牙设备页」（可能是本来就开着），调用方不用再开面板；
/// 返回 `false` 表示快速设置没开 / 蓝牙磁贴被用户移掉了。
fn click_manage_bluetooth(automation: &windows::Win32::UI::Accessibility::IUIAutomation) -> bool {
    unsafe {
        let Some(control_center) = find_control_center(automation) else {
            return false;
        };

        // 已经在二级页：蓝牙页就到此为止，别的页（WLAN 等）先退回去。
        if let Some(page) = find_by_aid(automation, &control_center, uia::PAGE_WINDOW) {
            if find_by_aid(automation, &page, uia::BLUETOOTH_TILE).is_some() {
                return true;
            }
            let Some(back) = find_by_aid(automation, &page, uia::BACK_BUTTON) else {
                return false;
            };
            if !invoke(&back) {
                return false;
            }
            std::thread::sleep(std::time::Duration::from_millis(250));
            let Some(control_center) = find_control_center(automation) else {
                return false;
            };
            return click_split_button(automation, &control_center);
        }

        click_split_button(automation, &control_center)
    }
}

/// 在磁贴页上点蓝牙磁贴的展开按钮。
fn click_split_button(
    automation: &windows::Win32::UI::Accessibility::IUIAutomation,
    control_center: &windows::Win32::UI::Accessibility::IUIAutomationElement,
) -> bool {
    unsafe {
        let Some(tile) = find_by_aid(automation, control_center, uia::BLUETOOTH_TILE) else {
            return false;
        };
        // 展开按钮是磁贴（ListItem）的兄弟节点，不在磁贴自己的子树里，
        // 得先沿 raw view 往上走一层拿 ListItem。
        let Ok(walker) = automation.RawViewWalker() else {
            return false;
        };
        let Ok(item) = walker.GetParentElement(&tile) else {
            return false;
        };
        let Some(split) = find_by_aid(automation, &item, uia::SPLIT_BUTTON) else {
            return false;
        };
        invoke(&split)
    }
}

/// 找快速设置顶层窗口。没开时返回 `None`。
///
/// 不按名字（`"快速设置"` / `"Quick settings"`）找，按 `ClassName` 找 —— 那个是常量。
/// 从桌面根节点直接 Descendants 搜会把整个桌面遍历一遍（几百毫秒），
/// 所以只在顶层窗口里挨个比类名。
fn find_control_center(
    automation: &windows::Win32::UI::Accessibility::IUIAutomation,
) -> Option<windows::Win32::UI::Accessibility::IUIAutomationElement> {
    use windows::Win32::UI::Accessibility::{TreeScope_Children, UIA_ClassNamePropertyId};

    unsafe {
        let root = automation.GetRootElement().ok()?;
        let all = automation.CreateTrueCondition().ok()?;
        let windows = root.FindAll(TreeScope_Children, &all).ok()?;
        let count = windows.Length().ok()?;

        for index in 0..count {
            let Ok(window) = windows.GetElement(index) else {
                continue;
            };
            let Ok(class) = window.GetCurrentPropertyValue(UIA_ClassNamePropertyId) else {
                continue;
            };
            // VARIANT 转字符串：BSTR 的情况才可能是类名。
            let Ok(name) = windows::core::BSTR::try_from(&class) else {
                continue;
            };
            if name.to_string() == uia::CONTROL_CENTER_CLASS {
                return Some(window);
            }
        }
        None
    }
}

/// 在 `parent` 的子树里按 `AutomationId` 找第一个元素。
fn find_by_aid(
    automation: &windows::Win32::UI::Accessibility::IUIAutomation,
    parent: &windows::Win32::UI::Accessibility::IUIAutomationElement,
    automation_id: &str,
) -> Option<windows::Win32::UI::Accessibility::IUIAutomationElement> {
    use windows::core::Interface;
    use windows::Win32::System::Variant::VARIANT;
    use windows::Win32::UI::Accessibility::{TreeScope_Descendants, UIA_AutomationIdPropertyId};

    unsafe {
        let condition = automation
            .CreatePropertyCondition(UIA_AutomationIdPropertyId, &VARIANT::from(automation_id))
            .ok()?;
        let element = parent.FindFirst(TreeScope_Descendants, &condition).ok()?;
        (!element.as_raw().is_null()).then_some(element)
    }
}

/// 对元素调 `Invoke`。没有该模式（不是按钮）返回 `false`。
fn invoke(element: &windows::Win32::UI::Accessibility::IUIAutomationElement) -> bool {
    use windows::Win32::UI::Accessibility::{IUIAutomationInvokePattern, UIA_InvokePatternId};

    unsafe {
        let Ok(pattern) = element.GetCurrentPatternAs::<IUIAutomationInvokePattern>(UIA_InvokePatternId)
        else {
            return false;
        };
        pattern.Invoke().is_ok()
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
