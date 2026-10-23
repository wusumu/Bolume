//! 托盘悬停时用滚轮调音量。
//!
//! Windows 的托盘图标收不到滚轮消息，EarTrumpet 也是靠低级鼠标钩子实现的：
//! 这里挂一个 WH_MOUSE_LL 钩子，只在光标悬停于托盘图标上时把滚轮事件转成音量步进。

use std::sync::{
    Mutex, OnceLock,
    atomic::{AtomicBool, Ordering},
};

use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, HC_ACTION, HHOOK, MSG, MSLLHOOKSTRUCT, SetWindowsHookExW,
    UnhookWindowsHookEx, WH_MOUSE_LL, WM_MBUTTONDOWN, WM_MBUTTONUP, WM_MOUSEHWHEEL, WM_MOUSEWHEEL,
    WM_XBUTTONDOWN, WM_XBUTTONUP,
};

use crate::platform::hotkey::{
    self, VK_MOUSE_SIDE1, VK_MOUSE_SIDE2, VK_MOUSE_WHEEL_CLICK, VK_MOUSE_WHEEL_DOWN,
    VK_MOUSE_WHEEL_LEFT, VK_MOUSE_WHEEL_RIGHT, VK_MOUSE_WHEEL_UP,
};

/// 光标是否停在托盘图标上（由托盘 Enter/Leave 事件维护）。
static HOVERING: AtomicBool = AtomicBool::new(false);
/// 托盘图标的屏幕矩形（left, top, right, bottom，物理像素）。
///
/// 只靠 Enter/Leave 不可靠：托盘区自己管 hit-test，某些情况下 Leave 收不到，
/// 光标早移开了滚轮还在调音量。所以每次滚轮再拿光标位置跟这个矩形核对一遍。
static ICON_RECT: Mutex<Option<(f64, f64, f64, f64)>> = Mutex::new(None);
/// 滚轮事件出口。
static WHEEL_TX: Mutex<Option<async_channel::Sender<i32>>> = Mutex::new(None);
/// 钩子句柄，仅用于卸载。
static HOOK: OnceLock<isize> = OnceLock::new();

/// 标记光标进入/离开托盘图标。
pub fn set_hovering(hovering: bool) {
    HOVERING.store(hovering, Ordering::Relaxed);
}

/// 记下托盘图标的屏幕矩形；`None` 表示当前不知道（退回只靠 Enter/Leave）。
pub fn set_icon_rect(rect: Option<(f64, f64, f64, f64)>) {
    if let Ok(mut guard) = ICON_RECT.lock() {
        *guard = rect;
    }
}

/// 光标此刻是否真的落在托盘图标上。
///
/// 矩形拿不到时返回 true（沿用 Enter/Leave 的判断），拿到就必须在矩形里。
fn cursor_on_icon(x: f64, y: f64) -> bool {
    let Ok(guard) = ICON_RECT.lock() else {
        return true;
    };
    match *guard {
        None => true,
        // 托盘图标很小，给 2px 容差，避免边缘抖动时误判。
        Some((left, top, right, bottom)) => {
            x >= left - 2.0 && x <= right + 2.0 && y >= top - 2.0 && y <= bottom + 2.0
        }
    }
}

/// 启动钩子线程，返回滚轮步进（+1/-1）的接收端。
pub fn spawn() -> async_channel::Receiver<i32> {
    let (tx, rx) = async_channel::unbounded();
    *WHEEL_TX.lock().unwrap() = Some(tx);

    std::thread::Builder::new()
        .name("bolume-wheel-hook".into())
        .spawn(|| unsafe {
            let Ok(hook) = SetWindowsHookExW(WH_MOUSE_LL, Some(hook_proc), None, 0) else {
                return;
            };
            let _ = HOOK.set(hook.0 as isize);

            // 低级钩子依赖安装线程的消息循环，否则回调不会被调用。
            let mut message = MSG::default();
            while GetMessageW(&mut message, None, 0, 0).as_bool() {}
            let _ = UnhookWindowsHookEx(hook);
        })
        .ok();

    rx
}

/// 这一发鼠标事件对应哪个「伪键码」；不是我们认的键就返回 `None`。
unsafe fn mouse_key_of(message: usize, info: &MSLLHOOKSTRUCT) -> Option<u32> {
    // mouseData 高 16 位在滚轮里是带符号的滚动量，在 XBUTTON 里是「第几个侧键」。
    let high = ((info.mouseData >> 16) & 0xffff) as u16;
    match message {
        m if m == WM_MOUSEWHEEL as usize => {
            let delta = high as i16;
            if delta > 0 {
                Some(VK_MOUSE_WHEEL_UP)
            } else if delta < 0 {
                Some(VK_MOUSE_WHEEL_DOWN)
            } else {
                None
            }
        }
        // 横向滚轮（滚轮左右摆）：正值向右。
        m if m == WM_MOUSEHWHEEL as usize => {
            let delta = high as i16;
            if delta > 0 {
                Some(VK_MOUSE_WHEEL_RIGHT)
            } else if delta < 0 {
                Some(VK_MOUSE_WHEEL_LEFT)
            } else {
                None
            }
        }
        m if m == WM_MBUTTONDOWN as usize || m == WM_MBUTTONUP as usize => {
            Some(VK_MOUSE_WHEEL_CLICK)
        }
        m if m == WM_XBUTTONDOWN as usize || m == WM_XBUTTONUP as usize => match high {
            1 => Some(VK_MOUSE_SIDE1),
            2 => Some(VK_MOUSE_SIDE2),
            _ => None,
        },
        _ => None,
    }
}

unsafe extern "system" fn hook_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code != HC_ACTION as i32 {
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }
    let message = wparam.0;
    let info = unsafe { &*(lparam.0 as *const MSLLHOOKSTRUCT) };
    let mouse_key = unsafe { mouse_key_of(message, info) };

    // 一、录制中：把鼠标组合当成快捷键送回去。
    //    必须带至少一个修饰键 —— 否则「滚轮向上」这种裸操作被拦下来，
    //    整个系统的滚动就废了。
    if let Some(key) = mouse_key {
        if hotkey::is_capturing() {
            let mods = hotkey::current_modifiers();
            if mods != 0 {
                hotkey::send_mouse_hotkey(mods, key);
                return LRESULT(1);
            }
            return unsafe { CallNextHookEx(None, code, wparam, lparam) };
        }
        // 二、配过的组合：吞掉这一发（否则滚轮还会把底下的窗口滚一遍），
        //    黑名单里的应用在前台时让路，原样还给系统。
        if !hotkey::is_blocked_now() {
            let mods = hotkey::current_modifiers();
            if hotkey::mouse_binding_matches(mods, key) {
                // 抬起只吞掉、不再触发一次（否则点一下等于按了两下）。
                let released = message == WM_MBUTTONUP as usize || message == WM_XBUTTONUP as usize;
                if !released {
                    hotkey::send_mouse_hotkey(mods, key);
                }
                return LRESULT(1);
            }
        }
    }

    // 三、托盘悬停滚轮调音量（老功能，只在没配组合时走）。
    if message == WM_MOUSEWHEEL as usize && HOVERING.load(Ordering::Relaxed) {
        // Enter/Leave 漏掉时兜底：光标不在图标矩形里就当没悬停。
        // （实测托盘图标只发 Enter、几乎不发 Leave，这条才是主力判断。）
        if !cursor_on_icon(info.pt.x as f64, info.pt.y as f64) {
            return unsafe { CallNextHookEx(None, code, wparam, lparam) };
        }
        let delta = ((info.mouseData >> 16) & 0xffff) as u16 as i16;
        let step = if delta > 0 {
            1
        } else if delta < 0 {
            -1
        } else {
            0
        };
        if step != 0 {
            if let Ok(guard) = WHEEL_TX.lock() {
                if let Some(sender) = guard.as_ref() {
                    let _ = sender.try_send(step);
                }
            }
        }
    }

    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// 供调试：当前钩子是否安装成功。
pub fn is_installed() -> bool {
    HOOK.get().is_some()
}

/// 保留句柄类型引用，避免未使用导入告警。
#[allow(dead_code)]
fn hook_handle_type(_: HHOOK) {}
