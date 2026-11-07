//! 托盘悬停 / 屏幕热区时用滚轮调音量。
//!
//! Windows 的托盘图标收不到滚轮消息，EarTrumpet 也是靠低级鼠标钩子实现的：
//! 这里挂一个 WH_MOUSE_LL 钩子，把两种场景的滚轮事件转成音量步进：
//!
//! - 光标停在托盘图标上（`set_hovering` / `set_icon_rect`）；
//! - 光标落在「屏幕边缘 / 指定区域」里（`set_zones`，几何由主线程算好塞进来）。

use std::sync::{
    Mutex,
    atomic::{AtomicBool, AtomicIsize, Ordering},
};

use windows::Win32::Foundation::{LPARAM, LRESULT, WPARAM};
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::WindowsAndMessaging::{
    CallNextHookEx, GetMessageW, HC_ACTION, HHOOK, MSG, MSLLHOOKSTRUCT, PostThreadMessageW,
    SetWindowsHookExW, UnhookWindowsHookEx, WH_MOUSE_LL, WM_MBUTTONDOWN, WM_MBUTTONUP,
    WM_MOUSEHWHEEL, WM_MOUSEWHEEL, WM_XBUTTONDOWN, WM_XBUTTONUP,
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
/// 热区滚轮事件出口。
static ZONE_TX: Mutex<Option<async_channel::Sender<ZoneStep>>> = Mutex::new(None);
/// 钩子句柄，仅用于卸载 / 调试查询（0 = 没装上）。
static HOOK: AtomicIsize = AtomicIsize::new(0);

/// 叫醒钩子线程去重装钩子的线程消息（`WM_APP + 7`）。
const WAKE_HOOK: u32 = 0x8000 + 7;

/// 前台黑名单状态的缓存：`true` = 现在要整个让路。
///
/// **不能在钩子回调里现问 `hotkey::is_blocked_now()`**：那个函数要建一次全系统
/// 进程快照、再顺着父子链问几层 exe 路径，忙的时候（录屏编码、游戏满载）能拖到
/// 上百毫秒。低级钩子回调超过系统的 `LowLevelHooksTimeout`（默认 300ms）会被
/// Windows **直接跳过**，表现就是「平时好的，一录屏热区就突然没反应」而界面上
/// 一切正常。改成后台线程每半秒问一次，钩子里只读一个原子量。
static BLOCKED: AtomicBool = AtomicBool::new(false);

/// 钩子回调里问「现在要不要让路」（读缓存，O(1)）。
fn blocked() -> bool {
    BLOCKED.load(Ordering::Relaxed)
}

/// 一块热区的屏幕矩形（物理像素）+ 它调什么。
///
/// 目标跟几何一起塞进快照，不在回调里查配置：钩子线程跑在别的线程上，
/// 而且每块热区的目标可能不一样（贴顶调亮度、贴底调音量）。
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ZoneBox {
    pub left: f32,
    pub top: f32,
    pub right: f32,
    pub bottom: f32,
    pub target: crate::config::WheelZoneTarget,
}

impl ZoneBox {
    fn contains(&self, x: f32, y: f32) -> bool {
        x >= self.left && x <= self.right && y >= self.top && y <= self.bottom
    }
}

/// 热区滚轮（一格 = 一个步进）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ZoneStep {
    /// +1 = 往上 / 往右滚，-1 = 往下 / 往左滚。
    pub step: i32,
    /// 是不是横向滚轮（滚轮左右摆）。
    pub horizontal: bool,
    /// 命中的那块热区调什么（音量 / 亮度）。
    pub target: crate::config::WheelZoneTarget,
}

/// 热区快照：由主线程按配置 + 显示器算好，整份换进来。
#[derive(Clone, Debug, Default)]
struct ZoneSnapshot {
    /// 配置开着、也没被黑名单 / 全屏让路挡掉。
    active: bool,
    /// 横向滚轮也算。
    horizontal: bool,
    /// 命中后吞掉这一发滚动（否则底下的网页 / 笔记也跟着滚）。
    swallow: bool,
    boxes: Vec<ZoneBox>,
}

/// 当前热区快照；`None` = 这个功能没配过 / 已关。
static ZONES: Mutex<Option<ZoneSnapshot>> = Mutex::new(None);

/// 换上新的热区几何。`boxes` 为空等同关掉。
pub fn set_zones(active: bool, horizontal: bool, swallow: bool, boxes: Vec<ZoneBox>) {
    let snapshot = ZoneSnapshot {
        active: active && !boxes.is_empty(),
        horizontal,
        swallow,
        boxes,
    };
    if let Ok(mut guard) = ZONES.lock() {
        *guard = Some(snapshot);
    }
}

/// 撤掉热区（功能关掉 / 让路时用）。
pub fn clear_zones() {
    if let Ok(mut guard) = ZONES.lock() {
        *guard = None;
    }
}

/// 光标这一发滚轮落在哪块热区里；命中就给回那块热区（含它调什么、要不要吞）。
///
/// 边角是两条带的交集，这里**先横后竖**：上/下边排在左/右边前面，所以
/// 贴上边再往左滑到角落仍然是「上边」那个目标，不会走到一半目标就变了。
fn zone_hit(x: f32, y: f32, horizontal: bool) -> Option<ZoneHit> {
    let guard = ZONES.lock().ok()?;
    let zone = guard.as_ref()?;
    if !zone.active || (horizontal && !zone.horizontal) {
        return None;
    }
    zone.boxes
        .iter()
        .find(|box_| box_.contains(x, y))
        .map(|box_| ZoneHit {
            target: box_.target,
            swallow: zone.swallow,
        })
}

/// `zone_hit` 的结果：命中哪块、要不要吞掉这一发。
struct ZoneHit {
    target: crate::config::WheelZoneTarget,
    swallow: bool,
}

/// 标记光标进入/离开托盘图标。
///
/// 顺带记一笔 trace：这对事件**不成对**是「边缘热区时灵时不灵」的老根因
/// （`HOVERING` 卡在 true，托盘那条分支把滚轮吃掉）。用户报问题时开 trace
/// 扫一遍托盘，一眼就能看出 Leave 到底来没来。
pub fn set_hovering(hovering: bool) {
    HOVERING.store(hovering, Ordering::Relaxed);
    hotkey::trace(if hovering {
        "tray enter"
    } else {
        "tray leave"
    });
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

/// 启动钩子线程，返回（托盘滚轮步进，热区滚轮）两个接收端。
pub fn spawn() -> (
    async_channel::Receiver<i32>,
    async_channel::Receiver<ZoneStep>,
) {
    let (tx, rx) = async_channel::unbounded();
    *WHEEL_TX.lock().unwrap() = Some(tx);
    let (zone_tx, zone_rx) = async_channel::unbounded();
    *ZONE_TX.lock().unwrap() = Some(zone_tx);

    // 黑名单状态的后台轮询：钩子里只读结果（见 `BLOCKED` 的注释）。
    std::thread::Builder::new()
        .name("bolume-block-poll".into())
        .spawn(|| loop {
            BLOCKED.store(hotkey::is_blocked_now(), Ordering::Relaxed);
            std::thread::sleep(std::time::Duration::from_millis(500));
        })
        .ok();

    std::thread::Builder::new()
        .name("bolume-wheel-hook".into())
        .spawn(|| unsafe {
            use windows::Win32::System::Threading::{
                GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
            };
            // 回调就跑在这个线程上。录屏编码、游戏满载的时候 CPU 一紧张，普通优先级
            // 的线程会被排到后面 —— 回调一旦超过系统的 `LowLevelHooksTimeout`
            // （默认 300ms），这一发输入就被跳过，Windows 7 之后还会**直接把钩子卸掉**。
            // 「平时都好，一开始录屏热区就没了」多半就是这个：相机一样的负载下
            // 这个线程要能抢到时间片。回调本身只做几次加锁 + 一次向量扫描，占用极短。
            let _ = SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL);
            let Ok(mut hook) = SetWindowsHookExW(WH_MOUSE_LL, Some(hook_proc), None, 0) else {
                return;
            };
            HOOK.store(hook.0 as isize, Ordering::Relaxed);

            // 看门狗：低级钩子回调超时会被系统**悄悄卸掉**（MSDN 明说卸了也没法知道），
            // 而超时正是满载时才会发生。每隔几秒叫醒这个线程，把钩子重装一遍：
            // 还活着等价于换了张新票（顺带排到链最前面，别的程序装了钩子也挡不住我们），
            // 被卸了也自动回来，不用等用户重启程序。
            //
            // 周期取 5 秒而不是 20 秒：真被卸掉时，用户感知到的「失灵」时长就是它 ——
            // 20 秒会被说成「坏了」，5 秒多半只让人觉得「刚卡了一下」。重装本身只是
            // 一次 Unhook + Set，开销可以忽略。
            let thread_id = GetCurrentThreadId();
            std::thread::Builder::new()
                .name("bolume-hook-watch".into())
                .spawn(move || loop {
                    std::thread::sleep(std::time::Duration::from_secs(5));
                    if PostThreadMessageW(thread_id, WAKE_HOOK, WPARAM(0), LPARAM(0)).is_err() {
                        // 队列满 / 线程没了：这一轮跳过，别把看门狗自己弄死。
                        continue;
                    }
                })
                .ok();

            // 低级钩子依赖安装线程的消息循环，否则回调不会被调用。
            let mut message = MSG::default();
            loop {
                if !GetMessageW(&mut message, None, 0, 0).as_bool() {
                    break;
                }
                if message.message == WAKE_HOOK {
                    let _ = UnhookWindowsHookEx(hook);
                    HOOK.store(0, Ordering::Relaxed);
                    match SetWindowsHookExW(WH_MOUSE_LL, Some(hook_proc), None, 0) {
                        Ok(fresh) => {
                            hook = fresh;
                            HOOK.store(fresh.0 as isize, Ordering::Relaxed);
                            // 成功不记：这是每 5 秒一次的常态，记了只会把日志刷满。
                        }
                        Err(error) => {
                            // 装不上就空转这一轮，5 秒后再试。别把旧句柄留着当挂在链上 ——
                            // 它刚被 Unhook 过，`is_installed()` 会说谎。
                            hotkey::trace(&format!("wheel hook rearm failed: {error:?}"));
                        }
                    }
                }
            }
            let _ = UnhookWindowsHookEx(hook);
            HOOK.store(0, Ordering::Relaxed);
        })
        .ok();

    (rx, zone_rx)
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
        if !blocked() {
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
    //
    // 「光标在不在图标上」必须进条件、并且在不在时**往下走**，不能像以前那样
    // 先 `if HOVERING` 再核对位置、对不上就直接 return：
    // `HOVERING` 靠托盘图标的 Enter / Leave 维护，而 Leave 经常不来（explorer
    // 重启、任务栏换边、DPI 一变都会漏）。一旦它卡在 true，那条 return 会把
    // 下面整段热区判断一起吃掉 —— 用户看到的就是「边缘触发时灵时不灵，重启就好」
    // （重启时 HOVERING 归 false，所以「好了」）。位置对不上时顺手清零，自愈。
    if message == WM_MOUSEWHEEL as usize && HOVERING.load(Ordering::Relaxed) {
        if cursor_on_icon(info.pt.x as f64, info.pt.y as f64) {
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
                // 托盘已经认下这一发：别再让热区来一遍。托盘图标就长在任务栏上，
                // 贴着底边 / 右下角的热区跟它重叠，否则一次滚动调两档。
                return unsafe { CallNextHookEx(None, code, wparam, lparam) };
            }
        } else if HOVERING.swap(false, Ordering::Relaxed) {
            // Leave 漏了：这一发就是证据。清零后热区照常往下判断。
            debug_wheel(info.pt.x, info.pt.y, false, None, false);
        }
    }

    // 四、屏幕边缘 / 指定区域的热区：滚轮直接调音量。
    //     排在托盘之后：托盘图标本身就压在屏幕边上，两者重叠时让托盘先认，
    //     免得「想调托盘那个」却走了热区那套步进。
    let horizontal = message == WM_MOUSEHWHEEL as usize;
    let wheel = horizontal || message == WM_MOUSEWHEEL as usize;
    // 黑名单那个应用在前台时整个让路：不吞滚轮，也不调音量。
    if wheel && !blocked() {
        if let Some(hit) = zone_hit(info.pt.x as f32, info.pt.y as f32, horizontal) {
            debug_wheel(info.pt.x, info.pt.y, horizontal, Some(hit.target.index()), false);
            let delta = ((info.mouseData >> 16) & 0xffff) as u16 as i16;
            if delta != 0 {
                let step = if delta > 0 { 1 } else { -1 };
                if let Ok(guard) = ZONE_TX.lock() {
                    if let Some(sender) = guard.as_ref() {
                        let _ = sender.try_send(ZoneStep {
                            step,
                            horizontal,
                            target: hit.target,
                        });
                    }
                }
                if hit.swallow {
                    return LRESULT(1);
                }
            }
        } else {
            debug_wheel(info.pt.x, info.pt.y, horizontal, None, false);
        }
    } else if wheel {
        debug_wheel(info.pt.x, info.pt.y, horizontal, None, true);
    }

    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// 供调试：当前钩子是否安装成功。
pub fn is_installed() -> bool {
    HOOK.load(Ordering::Relaxed) != 0
}

/// 调试用：把每一发滚轮记进 trace 日志。
///
/// 「录屏 / 截图工具在前台时热区不触发」这种问题，光看界面分不清是**钩子没收到**
/// （一行都不会有）、**命中盒算错**（记 miss）、还是**被黑名单挡了**（记 blocked）——
/// 这一行能一次分开。只在 `trace.on` 存在时才写，平时零开销。
fn debug_wheel(x: i32, y: i32, horizontal: bool, target: Option<u32>, blocked: bool) {
    if !hotkey::trace_enabled() {
        return;
    }
    let status = if blocked {
        format!("blocked fg={}", hotkey::foreground_name())
    } else {
        match target {
            Some(index) => format!("hit target={index}"),
            None => "miss".to_string(),
        }
    };
    hotkey::trace(&format!(
        "zone wheel ({x},{y}) horizontal={horizontal} {status}"
    ));
}

/// 保留句柄类型引用，避免未使用导入告警。
#[allow(dead_code)]
fn hook_handle_type(_: HHOOK) {}
