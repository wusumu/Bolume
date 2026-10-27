//! 空闲时把工作集还给系统。
//!
//! # 为什么需要
//!
//! Windows 不会主动回收进程的工作集：只要整机内存不紧张，进程访问过的页就一直留在
//! 物理内存里。gpui 应用每个窗口各有一套 DirectX 图集和字形缓存，跑一天之后任务
//! 管理器上的「内存」会涨到一百多 MB —— 这些页绝大多数是**访问过、之后再也不读**的
//! （面板渲染过的字形、配色、图标位图）。
//!
//! 实测（0.83）：刚启动 21MB，跑 7 小时 126MB，两分钟采样零增长、句柄数还在下降 ——
//! 是驻留，不是泄漏。真正的修法不是找泄漏，而是**不活跃的页主动换出去**。
//!
//! # 什么时候才动
//!
//! 三条同时满足才修剪：
//!
//! * 本进程**没有可见窗口**（面板、亮度卡片、屏幕提示、设置窗全收着）—— 有窗口在
//!   眼前时修剪，下一帧就是一片缺页，掉帧；
//! * 已经这样安静了 [`IDLE_AFTER`] 以上；
//! * 距上次修剪超过 [`TRIM_EVERY`]，且工作集还在 [`TRIM_FLOOR`] 以上。
//!
//! 修剪之后重新打开面板要重新触页，几十毫秒的事，用户看不出区别。

use std::time::{Duration, Instant};

/// 没有可见窗口持续这么久才修剪。
const IDLE_AFTER: Duration = Duration::from_secs(20);
/// 两次修剪之间至少隔这么久，别让反复触页变成抖动。
const TRIM_EVERY: Duration = Duration::from_secs(180);
/// 工作集低于这个量就不值得动（空闲基座本来就在 20MB 上下）。
const TRIM_FLOOR: u64 = 20 * 1024 * 1024;

/// 空闲修剪的节拍器。放在 gui 的驱动循环里，`tick()` 自己判条件。
pub struct IdleTrim {
    last_visible: Instant,
    last_trim: Instant,
}

impl Default for IdleTrim {
    fn default() -> Self {
        let now = Instant::now();
        Self {
            last_visible: now,
            // 第一次修剪不用等两分钟：启动后安静一会儿就该归位。
            last_trim: now - TRIM_EVERY,
        }
    }
}

impl IdleTrim {
    /// 每 `interval` 调一次。返回 `true` = 这次做了修剪（只为调试日志）。
    pub fn tick(&mut self) -> bool {
        if any_window_visible() {
            self.last_visible = Instant::now();
            return false;
        }
        if self.last_visible.elapsed() < IDLE_AFTER || self.last_trim.elapsed() < TRIM_EVERY {
            return false;
        }
        let before = working_set();
        if before < TRIM_FLOOR {
            return false;
        }
        let trimmed = trim();
        self.last_trim = Instant::now();
        if trimmed {
            crate::platform::hotkey::trace(&format!(
                "memory trim working_set {:.1}MB -> {:.1}MB",
                before as f64 / 1048576.0,
                working_set() as f64 / 1048576.0
            ));
        }
        trimmed
    }
}

/// 本进程有没有**摆在屏幕里**的顶层窗口。
///
/// 只判 `IsWindowVisible` 不够：面板、亮度卡片、提示条收起时是把窗口**挪到屏幕外**
/// （`WS_VISIBLE` 一直亮着，`IsWindowVisible` 为真），靠它判会永远觉得「有窗口开着」。
/// 所以还要看矩形和虚拟屏幕有没有交集。
///
/// 托盘图标的宿主窗口是隐藏的，不会误判；被别的窗口整个盖住的情况按「有窗口」算
/// （保守：不修剪）。
pub fn any_window_visible() -> bool {
    use windows::core::BOOL;
    use windows::Win32::Foundation::{HWND, LPARAM, RECT};
    use windows::Win32::UI::WindowsAndMessaging::{
        EnumWindows, GetSystemMetrics, GetWindowRect, GetWindowThreadProcessId, IsWindowVisible,
        SM_CXVIRTUALSCREEN, SM_CYVIRTUALSCREEN, SM_XVIRTUALSCREEN, SM_YVIRTUALSCREEN,
    };

    /// 回调是裸函数指针，带不了闭包 —— 屏幕范围和结论一起塞进 `LPARAM`。
    struct Probe {
        found: bool,
        vx: i32,
        vy: i32,
        vw: i32,
        vh: i32,
    }

    unsafe extern "system" fn visit(hwnd: HWND, param: LPARAM) -> BOOL {
        let probe = &mut *(param.0 as *mut Probe);
        let mut pid = 0u32;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid != std::process::id() || !IsWindowVisible(hwnd).as_bool() {
            return BOOL(1);
        }
        let mut rect = RECT::default();
        if GetWindowRect(hwnd, &mut rect).is_err() {
            return BOOL(1);
        }
        // 和虚拟屏幕有交集才算「摆在屏幕上」。
        let overlaps = rect.right > probe.vx
            && rect.left < probe.vx + probe.vw
            && rect.bottom > probe.vy
            && rect.top < probe.vy + probe.vh;
        if overlaps {
            probe.found = true;
            return BOOL(0); // 有一个就够了，停。
        }
        BOOL(1)
    }

    let (vx, vy, vw, vh) = unsafe {
        (
            GetSystemMetrics(SM_XVIRTUALSCREEN),
            GetSystemMetrics(SM_YVIRTUALSCREEN),
            GetSystemMetrics(SM_CXVIRTUALSCREEN),
            GetSystemMetrics(SM_CYVIRTUALSCREEN),
        )
    };
    let mut probe = Probe {
        found: false,
        vx,
        vy,
        vw,
        vh,
    };
    unsafe {
        let _ = EnumWindows(
            Some(visit),
            LPARAM(&mut probe as *mut Probe as isize),
        );
    }
    probe.found
}

/// 本进程当前工作集（含共享页，和任务管理器「内存」列的算法一致）。
pub fn working_set() -> u64 {
    use windows::Win32::System::ProcessStatus::{
        GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS,
    };
    use windows::Win32::System::Threading::GetCurrentProcess;

    let mut counters = PROCESS_MEMORY_COUNTERS::default();
    counters.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
    let ok = unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut counters, counters.cb) };
    if ok.is_err() {
        return 0;
    }
    counters.WorkingSetSize as u64
}

/// 立刻修剪工作集（`(-1, -1)` = 把不活跃的页换出去）。
fn trim() -> bool {
    use windows::Win32::System::Threading::{GetCurrentProcess, SetProcessWorkingSetSize};

    unsafe {
        SetProcessWorkingSetSize(GetCurrentProcess(), usize::MAX, usize::MAX)
            .map(|_| true)
            .unwrap_or(false)
    }
}
