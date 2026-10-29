//! 「禁用 Windows 音量条」的窗口级实现。
//!
//! 光靠键盘钩子吞键是不够的，Win10 上尤其明显：
//!
//! - 很多键盘的媒体键走 HID consumer 报告，根本不经过 `WH_KEYBOARD_LL`；
//! - 那条「音量 + 正在播放」的卡片是系统对**媒体会话**的反应，跟按键从哪来无关
//!   （播放器自己改状态、别的软件调音量，一样会弹）。
//!
//! 所以还得直接对那扇窗口下手：系统这个 OSD 是一扇常驻窗口，把它最小化就不再出现，
//! 关掉开关或退出程序时恢复。类名按系统分两套，和 HideVolumeOSD 用的是同一组：
//!
//! - Win10：顶层 `NativeHWNDHost`，子窗口 `DirectUIHWND`；
//! - Win11（22000 起）：顶层 `XamlExplorerHostIslandWindow`，
//!   子窗口 `Windows.UI.Composition.DesktopWindowContentBridge`。
//!
//! 不改注册表、不碰音频链路，音量调节本身完全不受影响。

use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};
use std::time::Duration;

use windows::core::{BOOL, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, FindWindowExW, GetClassNameW, GetWindowThreadProcessId, IsWindow, SW_MINIMIZE,
    SW_RESTORE, ShowWindow,
};

/// 开关（配置变更时由 `apply_config` 同步）。
static BLOCKED: AtomicBool = AtomicBool::new(false);

/// 已经最小化的那扇窗口，恢复时按它来。
static HIDDEN: AtomicIsize = AtomicIsize::new(0);

/// 轮询线程只起一次。
static WORKER: AtomicBool = AtomicBool::new(false);

/// 轮询间隔。
///
/// 事件驱动做不到：这个窗口是**常驻**的，系统每次只是把它显示出来，
/// 没有「新建窗口」或「前台切换」这类能挂的事件。轮询是唯一可靠的路子。
/// 300ms 比系统 OSD 的 3 秒存活期短一个数量级，视觉上就是「从来没出现过」；
/// 一轮枚举几十微秒，开着开关时 CPU 占用可以忽略。
const POLL: Duration = Duration::from_millis(300);

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 顶层 / 内层窗口类名。
fn classes() -> (&'static str, &'static str) {
    if crate::platform::winver::is_windows_11() {
        (
            "XamlExplorerHostIslandWindow",
            "Windows.UI.Composition.DesktopWindowContentBridge",
        )
    } else {
        ("NativeHWNDHost", "DirectUIHWND")
    }
}

/// 这扇窗口属于系统壳吗。
///
/// `NativeHWNDHost` 不止音量条在用（输入法、通知、开始菜单的 XAML 宿主都是它），
/// 所以先按宿主进程筛一道：白名单里的优先。
/// 白名单一个都没命中时，只有**全系统唯一**一个候选才敢动 —— 宁可漏掉，
/// 也不能把别的 XAML 宿主窗口最小化。
fn shell_owned(hwnd: HWND) -> Option<bool> {
    let mut pid = 0u32;
    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut pid as *mut u32));
    }
    if pid == 0 {
        return None;
    }
    let process = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    let mut buf = [0u16; 260];
    let mut len = buf.len() as u32;
    let ok = unsafe {
        QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
    }
    .is_ok();
    let _ = unsafe { CloseHandle(process) };
    if !ok {
        return None;
    }
    let path = String::from_utf16_lossy(&buf[..len as usize]);
    let name = path
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    Some(matches!(
        name.as_str(),
        "explorer.exe" | "shellexperiencehost.exe" | "systemsettings.exe" | "shellhost.exe"
    ))
}

unsafe extern "system" fn collect(hwnd: HWND, lparam: LPARAM) -> BOOL {
    let out = unsafe { &mut *(lparam.0 as *mut Vec<isize>) };
    out.push(hwnd.0 as isize);
    BOOL(1)
}

fn class_of(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    let len = unsafe { GetClassNameW(hwnd, &mut buf) };
    if len <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buf[..len as usize])
}

/// 找系统那条 OSD 的窗口。
///
/// 命中条件：顶层类名相符、**直接子窗口**里有内层类名。
/// 这样既能抓住音量条，也能抓住 Win10 那张「正在播放」的卡片（同一套宿主）。
fn osd_window() -> Option<HWND> {
    let (outer, inner) = classes();
    let outer = wide(outer);
    let outer_text = String::from_utf16_lossy(&outer[..outer.len() - 1]);
    let inner = wide(inner);
    let mut tops: Vec<isize> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(collect), LPARAM(&mut tops as *mut _ as isize));
    }
    let mut sure: Option<HWND> = None;
    let mut maybe = Vec::new();
    for raw in tops {
        let hwnd = HWND(raw as *mut _);
        if class_of(hwnd) != outer_text {
            continue;
        }
        let child = unsafe { FindWindowExW(Some(hwnd), None, PCWSTR(inner.as_ptr()), PCWSTR::null()) };
        if child.map(|child| child.0.is_null()).unwrap_or(true) {
            continue;
        }
        match shell_owned(hwnd) {
            Some(true) => {
                if sure.is_none() {
                    sure = Some(hwnd);
                }
            }
            _ => maybe.push(hwnd),
        }
    }
    sure.or_else(|| {
        if maybe.len() == 1 {
            maybe.pop()
        } else {
            None
        }
    })
}

/// 把当前那扇窗口压下去。已经最小化过的不会再动。
fn press_down() -> bool {
    let Some(hwnd) = osd_window() else {
        return false;
    };
    unsafe {
        let _ = ShowWindow(hwnd, SW_MINIMIZE);
    }
    HIDDEN.store(hwnd.0 as isize, Ordering::SeqCst);
    true
}

/// 开关打开 / 关闭。
///
/// 打开时先立刻压一次（窗口已经在的话这一下就够），再挂上轮询兜住之后所有的显示；
/// 关闭时把它放回原样 —— 不恢复的话，用户关掉开关还会以为系统坏了。
pub fn set_blocked(on: bool) {
    if on {
        BLOCKED.store(true, Ordering::SeqCst);
        press_down();
        if !WORKER.swap(true, Ordering::SeqCst) {
            std::thread::Builder::new()
                .name("bolume-system-osd".into())
                .spawn(|| {
                    while WORKER.load(Ordering::SeqCst) {
                        if BLOCKED.load(Ordering::SeqCst) {
                            press_down();
                        }
                        std::thread::sleep(POLL);
                    }
                })
                .ok();
        }
    } else {
        BLOCKED.store(false, Ordering::SeqCst);
        restore();
    }
}

/// 立刻压一次（每次我们自己处理完音量 / 媒体键之后调一次）。
///
/// 系统是在按键处理**之后**才显示那条 OSD 的，所以处理完再压一次能赶在它
/// 显示之前把窗口按下去，比等下一轮轮询（最多 300ms）干净。
pub fn press_now() {
    if BLOCKED.load(Ordering::SeqCst) {
        press_down();
    }
}

/// 把窗口放回原样。退出程序、关掉开关、以及启动时清理上次的残留都调它。
pub fn restore() {
    let saved = HIDDEN.swap(0, Ordering::SeqCst);
    let mut targets = Vec::new();
    if saved != 0 {
        targets.push(HWND(saved as *mut _));
    }
    // 上次崩溃时没能恢复的：这次按窗口自己的最小化状态捞回来。
    if let Some(hwnd) = osd_window() {
        targets.push(hwnd);
    }
    for hwnd in targets {
        if unsafe { IsWindow(Some(hwnd)) }.as_bool() {
            unsafe {
                if windows::Win32::UI::WindowsAndMessaging::IsIconic(hwnd).as_bool() {
                    let _ = ShowWindow(hwnd, SW_RESTORE);
                }
            }
        }
    }
}

/// 现在是不是压着的（给设置页 / 调试用）。
pub fn is_blocked() -> bool {
    BLOCKED.load(Ordering::SeqCst)
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::core::w;
    use windows::Win32::Foundation::{LRESULT, WPARAM};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, IsIconic, RegisterClassW, WINDOW_EX_STYLE,
        WNDCLASSW, WS_CHILD, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
    };

    #[test]
    fn classes_follow_the_build() {
        let (outer, inner) = classes();
        if crate::platform::winver::is_windows_11() {
            assert_eq!(outer, "XamlExplorerHostIslandWindow");
            assert!(inner.contains("DesktopWindowContentBridge"));
        } else {
            assert_eq!(outer, "NativeHWNDHost");
            assert_eq!(inner, "DirectUIHWND");
        }
    }

    unsafe extern "system" fn stub(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
    }

    /// 注册窗口类，返回能活到窗口销毁的类名缓冲。
    fn register(class: &str) -> Vec<u16> {
        let name: Vec<u16> = class.encode_utf16().chain(std::iter::once(0)).collect();
        let module = unsafe { GetModuleHandleW(None) }.unwrap();
        let instance = windows::Win32::Foundation::HINSTANCE(module.0);
        let wc = WNDCLASSW {
            lpfnWndProc: Some(stub),
            hInstance: instance,
            lpszClassName: PCWSTR(name.as_ptr()),
            ..Default::default()
        };
        unsafe {
            RegisterClassW(&wc);
        }
        name
    }

    /// 造一对「顶层 + 内层」的窗口，类名跟系统那条 OSD 一样。
    ///
    /// 这条链路上最容易出错的是类名比对和「直接子窗口」那一层 —— 光靠真机看
    /// 是没法确认的（系统 OSD 只在按音量键那一下存在），所以这里自己造一对。
    #[test]
    fn finds_and_minimizes_a_window_that_looks_like_the_osd() {
        let (outer, inner) = classes();
        let outer_name = register(outer);
        let inner_name = register(inner);
        let module = unsafe { GetModuleHandleW(None) }.unwrap();
        let instance = windows::Win32::Foundation::HINSTANCE(module.0);
        let main = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                PCWSTR(outer_name.as_ptr()),
                w!("FakeOsd"),
                WS_OVERLAPPEDWINDOW,
                80,
                80,
                240,
                120,
                None,
                None,
                Some(instance),
                None,
            )
        }
        .expect("建顶层窗口");
        let _child = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                PCWSTR(inner_name.as_ptr()),
                w!(""),
                WS_CHILD | WS_VISIBLE,
                0,
                0,
                240,
                120,
                Some(main),
                None,
                Some(instance),
                None,
            )
        }
        .expect("建子窗口");

        assert_eq!(osd_window().map(|h| h.0), Some(main.0), "应该认出这扇窗口");
        assert!(!unsafe { IsIconic(main) }.as_bool());
        assert!(press_down());
        assert!(unsafe { IsIconic(main) }.as_bool(), "应该被压成最小化");
        restore();
        assert!(!unsafe { IsIconic(main) }.as_bool(), "关掉开关要放回来");

        unsafe {
            let _ = DestroyWindow(main);
        }
    }
}
