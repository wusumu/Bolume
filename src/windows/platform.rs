//! Win32 窗口细节：无边框弹出窗的圆角/阴影、显示隐藏、精确定位。

use gpui::{Bounds, Pixels, Window};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::Win32::{
    Foundation::{HWND, RECT},
    Graphics::Dwm::{
        DWMWA_BORDER_COLOR, DWMWA_COLOR_NONE, DWMWA_TRANSITIONS_FORCEDISABLED,
        DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND, DWMWCP_ROUND, DwmSetWindowAttribute,
    },
    UI::WindowsAndMessaging::{
        FindWindowW, GW_HWNDNEXT, GWL_EXSTYLE, GWL_STYLE, GetClassNameW, GetClientRect,
        GetTopWindow, GetWindow,
        GetWindowLongPtrW, GetWindowRect, HWND_NOTOPMOST,
        HWND_TOP, HWND_TOPMOST, IsWindowVisible, SW_HIDE, SW_RESTORE, SW_SHOW, SW_SHOWNA,
        SWP_FRAMECHANGED, SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOOWNERZORDER, SWP_NOSIZE,
        SWP_NOZORDER, SWP_SHOWWINDOW, SetForegroundWindow, SetWindowLongPtrW, SetWindowPos,
        ShowWindow, WS_CAPTION, WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW,
        WS_EX_TOPMOST, WS_SYSMENU, WS_THICKFRAME,
    },
};

pub fn hwnd_of(window: &Window) -> Option<HWND> {
    let handle = HasWindowHandle::window_handle(window).ok()?;
    match handle.as_raw() {
        RawWindowHandle::Win32(handle) => Some(HWND(handle.hwnd.get() as *mut _)),
        _ => None,
    }
}

/// 让 DWM 负责圆角与阴影，窗口本身保持纯客户区、不进任务栏。
///
/// 注意：这里刻意不碰 GWL_STYLE 与 DWMWA_NCRENDERING_POLICY —— 一旦把非客户区渲染
/// 交回窗口样式，DWM 会把整个弹出窗当成玻璃面板处理，内容会整体变半透明。
pub fn configure_popup(window: &Window) {
    let Some(hwnd) = hwnd_of(window) else {
        return;
    };
    unsafe {
        let ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        SetWindowLongPtrW(
            hwnd,
            GWL_EXSTYLE,
            (ex_style | WS_EX_TOOLWINDOW.0 as isize) & !(WS_EX_NOACTIVATE.0 as isize),
        );

        // 去掉 WS_THICKFRAME：gpui 为了让窗口能拖边改大小会给它加这个样式，
        // 但 DWM 正是按「有可调边框」给窗口画系统阴影的 —— 面板淡入时那圈
        // 黑边就是窗口阴影，内容还没长出来它已经在合成器里了。
        // QcClip 关阴影时同样先摘掉 WS_THICKFRAME（`set_window_shadow_visible`）。
        // 面板不需要缩放，摘掉正好；顺带客户区与窗口矩形对齐，`set_frame` 不用再补 8×4。
        // 连 WS_CAPTION / WS_SYSMENU 一起去掉：只要窗口还带「有标题栏」的样式，
        // 客户区就会被系统留出 16×8 的不可见边框，面板四边会多出一圈缝，
        // 位置也得靠 set_frame 去补差值。去掉后客户区就等于窗口矩形。
        let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
        let stripped = style
            & !(WS_THICKFRAME.0 as isize)
            & !(WS_CAPTION.0 as isize)
            & !(WS_SYSMENU.0 as isize);
        if stripped != style {
            SetWindowLongPtrW(hwnd, GWL_STYLE, stripped);
            let _ = SetWindowPos(
                hwnd,
                None,
                0,
                0,
                0,
                0,
                SWP_FRAMECHANGED
                    | SWP_NOMOVE
                    | SWP_NOSIZE
                    | SWP_NOZORDER
                    | SWP_NOACTIVATE
                    | SWP_NOOWNERZORDER,
            );
        }

        let corner = DWMWCP_ROUND;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &corner as *const _ as _,
            std::mem::size_of_val(&corner) as u32,
        );
        let border = DWMWA_COLOR_NONE;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR,
            &border as *const _ as _,
            std::mem::size_of_val(&border) as u32,
        );
        // 关掉 DWM 给窗口做的出现/消失过渡：系统那套淡入会先画出窗口的
        // 边框和阴影再淡入内容，正好就是「弹出前先闪一道黑边」。
        // BOOL：非零即 true。
        let no_transition: i32 = 1;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_TRANSITIONS_FORCEDISABLED,
            &no_transition as *const _ as _,
            std::mem::size_of_val(&no_transition) as u32,
        );
    }
}

/// OSD 提示条：不抢焦点、不进 Alt+Tab、关掉 DWM 自己的圆角。
///
/// 和 `configure_popup` 的关键区别是**保留 `WS_EX_NOACTIVATE`**：提示条在游戏
/// 上面闪一下时，绝不能把游戏切到后台。圆角由卡片自己画（半径可配置），
/// 所以窗口本身的圆角要关掉，否则两层圆角叠在一起会很怪。
pub fn configure_osd(window: &Window) {
    let Some(hwnd) = hwnd_of(window) else {
        return;
    };
    unsafe {
        let ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        SetWindowLongPtrW(
            hwnd,
            GWL_EXSTYLE,
            ex_style | WS_EX_TOOLWINDOW.0 as isize | WS_EX_NOACTIVATE.0 as isize,
        );

        let style = GetWindowLongPtrW(hwnd, GWL_STYLE);
        let stripped = style
            & !(WS_THICKFRAME.0 as isize)
            & !(WS_CAPTION.0 as isize)
            & !(WS_SYSMENU.0 as isize);
        if stripped != style {
            SetWindowLongPtrW(hwnd, GWL_STYLE, stripped);
            let _ = SetWindowPos(
                hwnd,
                None,
                0,
                0,
                0,
                0,
                SWP_FRAMECHANGED
                    | SWP_NOMOVE
                    | SWP_NOSIZE
                    | SWP_NOZORDER
                    | SWP_NOACTIVATE
                    | SWP_NOOWNERZORDER,
            );
        }

        let corner = DWMWCP_DONOTROUND;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &corner as *const _ as _,
            std::mem::size_of_val(&corner) as u32,
        );
        let border = DWMWA_COLOR_NONE;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR,
            &border as *const _ as _,
            std::mem::size_of_val(&border) as u32,
        );
        let no_transition: i32 = 1;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_TRANSITIONS_FORCEDISABLED,
            &no_transition as *const _ as _,
            std::mem::size_of_val(&no_transition) as u32,
        );
    }
}

/// 开关窗口的系统阴影。
///
/// Windows 11 是**按「圆角窗口」给窗口画那圈阴影的**（实测：`DWMWA_WINDOW_CORNER_PREFERENCE`
/// 设成 `DONOTROUND` 阴影立刻消失，设回 `ROUND` 又回来；`DWMWA_NCRENDERING_POLICY`
/// 和 `WS_EX_LAYERED` 都动不了它）。面板的四角是**自己画的**，DWM 那个圆角设置只影响
/// 阴影，所以关掉阴影不会改变外观。
///
/// 用途：跨屏那一段「窗口不动、只挪画面」的动画里，阴影是钉在窗口矩形上的 ——
/// 内容还在窗外时它已经孤零零地出现在终位，看着就是一个空阴影框慢慢被内容填满。
/// 动画期间关掉、画面到位再打开，阴影就和面板一起出现了。
pub fn set_window_shadow(window: &Window, on: bool) {
    let Some(hwnd) = hwnd_of(window) else {
        return;
    };
    let corner = if on {
        DWMWCP_ROUND
    } else {
        DWMWCP_DONOTROUND
    };
    unsafe {
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &corner as *const _ as _,
            std::mem::size_of_val(&corner) as u32,
        );
    }
}

/// 光标是不是正压在这个窗口上（两边都用物理像素，不用换算缩放）。
///
/// gpui 的 `on_hover` 只在「鼠标移动」时更新：光标直接划出窗口时平台只发
/// `WM_MOUSELEAVE`，不会派发鼠标事件，于是 `on_hover(false)` 永远不来 ——
/// 提示条就这么一直挂着。需要「移开就收」的地方一律用这个自己轮询。
pub fn cursor_in_window(window: &Window) -> bool {
    cursor_in_window_inset(window, 0.0)
}

/// 同上，但先把窗口四边往里收 `inset`（逻辑像素，内部按 DPI 换成物理像素）。
///
/// OSD 开了投影之后窗口比卡片大一圈（留给影子），悬停判断必须只算卡片本体：
/// 否则光标飘在影子的虚边上也会把提示条钉住，松手后永远不淡出。
pub fn cursor_in_window_inset(window: &Window, inset: f32) -> bool {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::WindowsAndMessaging::{GetCursorPos, GetWindowRect};

    let Some(hwnd) = hwnd_of(window) else {
        return false;
    };
    let mut position = POINT::default();
    if unsafe { GetCursorPos(&mut position) }.is_err() {
        return false;
    }
    let mut rect = windows::Win32::Foundation::RECT::default();
    if unsafe { GetWindowRect(hwnd, &mut rect) }.is_err() {
        return false;
    }
    let pad = (inset.max(0.0) * window.scale_factor()).round() as i32;
    let left = rect.left + pad;
    let top = rect.top + pad;
    // 收得比窗口还大时至少留一条一像素的边，别让比较反过来。
    let right = (rect.right - pad).max(left + 1);
    let bottom = (rect.bottom - pad).max(top + 1);
    position.x >= left && position.x < right && position.y >= top && position.y < bottom
}

/// 光标位置（物理像素 → 逻辑像素）。
///
/// OSD 的「跟随鼠标」要拿它算落点；gpui 的窗口坐标是相对窗口的，
/// 拿不到屏幕坐标，所以直接问系统。
pub fn cursor_logical(scale: f32) -> (f32, f32) {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

    let mut position = POINT::default();
    let ok = unsafe { GetCursorPos(&mut position) }.is_ok();
    if !ok {
        return (0.0, 0.0);
    }
    let scale = scale.max(0.5);
    (position.x as f32 / scale, position.y as f32 / scale)
}

/// 窗口的原始句柄（给跨线程的取色用：那边拿不到 `Window`）。
pub fn raw_hwnd(window: &Window) -> Option<isize> {
    hwnd_of(window).map(|handle| handle.0 as isize)
}

/// 按原始句柄显示 / 隐藏窗口。
///
/// 屏幕取色要先把编辑器藏起来，否则取到的永远是自己的像素；
/// 取完再露出来并抢回焦点（`SW_SHOWNA` 不够，用户还得接着点界面）。
pub fn show_raw(raw: isize, visible: bool) {
    let hwnd = HWND(raw as *mut core::ffi::c_void);
    unsafe {
        if visible {
            let _ = ShowWindow(hwnd, SW_SHOW);
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_TOP),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
            );
            let _ = SetForegroundWindow(hwnd);
        } else {
            let _ = ShowWindow(hwnd, SW_HIDE);
        }
    }
}

/// 光标在屏幕上的物理像素坐标。
pub fn cursor_screen() -> (i32, i32) {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

    let mut position = POINT::default();
    if unsafe { GetCursorPos(&mut position) }.is_err() {
        return (0, 0);
    }
    (position.x, position.y)
}

/// 屏幕当前像素的 `0xRRGGBB`。
///
/// 取的是合成之后的桌面像素，所以取色前必须把编辑器藏起来。
pub fn screen_color(x: i32, y: i32) -> Option<u32> {
    use windows::Win32::Graphics::Gdi::{CLR_INVALID, GetDC, GetPixel, ReleaseDC};

    unsafe {
        let dc = GetDC(None);
        if dc.is_invalid() {
            return None;
        }
        let value = GetPixel(dc, x, y);
        let _ = ReleaseDC(None, dc);
        if value.0 == CLR_INVALID {
            return None;
        }
        // COLORREF 是 0x00BBGGRR，换成习惯的 RGB。
        let value = value.0;
        Some(((value & 0xFF) << 16) | (value & 0xFF00) | ((value >> 16) & 0xFF))
    }
}

/// 左键此刻是不是压着（全局，不看窗口）。
pub fn mouse_left_down() -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON};

    (unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) }) as u16 & 0x8000 != 0
}

/// Esc 此刻是不是压着。
pub fn escape_down() -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_ESCAPE};

    (unsafe { GetAsyncKeyState(VK_ESCAPE.0 as i32) }) as u16 & 0x8000 != 0
}

pub fn set_visible(window: &Window, visible: bool) {
    let Some(hwnd) = hwnd_of(window) else {
        return;
    };
    unsafe {
        let _ = ShowWindow(hwnd, if visible { SW_SHOWNA } else { SW_HIDE });
    }
}

/// 收起终点：真把窗口藏起来（`SW_HIDE`），而不是「挪到屏幕外」或「压成全透明」。
///
/// 后两种做法只是**看不见**：窗口的 `WS_VISIBLE` 一直亮着、矩形和一个固定坐标绑在一起。
/// 屏幕摆放一变（改主屏、插拔显示器、把某台屏挪到另一侧），那块「屏幕外」的坐标
/// 就可能正好落进某块真实屏幕 —— 面板原地现身，看着就像「隐藏是假的」。
/// 真隐藏和屏幕布局彻底无关。
///
/// 再 `SW_SHOWNA` 出来不会补 DWM 的出现淡入：`configure_popup` / `configure_osd`
/// 都设了 `DWMWA_TRANSITIONS_FORCEDISABLED`。隐藏期间 gpui 也不重绘
/// （vendored 里那两处 `IsWindowVisible` 补丁），重新显示后第一个 vsync 会把它置脏。
pub fn hide_window(window: &Window) {
    set_visible(window, false);
}

/// 把本进程的系统定时器精度提到 1ms，drop 时还回去。
///
/// Windows 的线程池定时器精度跟着系统定时器走，默认 **15.6ms** —— 不论请求 4ms
/// 还是 1ms，实测都是每 15～16ms 才醒一次（`pump_content_slide` 的 trace 里 dt
/// 清一色 15.x）。「整扇窗挪」那条动画走独立线程 + `DwmFlush`，帧率跟着屏幕刷新
/// 走；「窗口不动、只挪画面」这一档只能靠定时器推进，于是同一段动画在跨屏时看着
/// 就是一格一格（165Hz 屏上最多 60fps）。
///
/// `timeBeginPeriod` 在 Windows 10 2004 之后是**进程级**的（不再拖慢整机），
/// 只在动画那两三百毫秒里开着，演完立刻还回去。
///
/// 计数而不是「有开就关」：收起演到一半又被弹出时，旧循环的 guard 比新循环的晚
/// 掉一拍 —— 各自一对一地 `timeEndPeriod` 会把还没演完的那一段的精度提前收走，
/// 这里等最后一个 guard 走了才真的还原。
pub struct TimerPrecision;

static PRECISE_HOLDERS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

impl Drop for TimerPrecision {
    fn drop(&mut self) {
        use std::sync::atomic::Ordering;
        use windows::Win32::Media::timeEndPeriod;

        if PRECISE_HOLDERS.fetch_sub(1, Ordering::SeqCst) == 1 {
            unsafe {
                timeEndPeriod(1);
            }
        }
    }
}

/// 临时把定时器精度提到 1ms（返回的 guard 活着期间有效）。
pub fn precise_timers() -> TimerPrecision {
    use std::sync::atomic::Ordering;
    use windows::Win32::Media::timeBeginPeriod;

    if PRECISE_HOLDERS.fetch_add(1, Ordering::SeqCst) == 0 {
        unsafe {
            timeBeginPeriod(1);
        }
    }
    TimerPrecision
}

/// 滑动动画的方向：滑进来，还是滑出去。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlideMode {
    /// 从当前位置滑到目标位置。
    Enter,
    /// 从当前位置滑出屏幕。
    Exit,
}

/// 一段「整扇窗从当前位置滑到某处」的动画参数。
#[derive(Clone, Copy, Debug)]
pub struct SlideRun {
    /// 滑进来还是滑出去。
    pub mode: SlideMode,
    /// 目标位置（逻辑像素，窗口左上角）。
    pub to: (f32, f32),
    /// 位移方向：`offset` 为正时往哪边挪。底任务栏 = (0, 1)（从下方上来）。
    pub direction: (f32, f32),
    /// 这一段从多远的地方起步。
    ///
    /// 打断时就是上一段停下的位置 —— 于是「没出完就收起」是从半路往下滑，
    /// 而不是跳回起点再来一次。
    pub from: f32,
    /// 这一段要走到多远：`Enter` 是 0，`Exit` 是滑出屏幕的距离。
    pub end: f32,
    /// 位移的指数时间常数（毫秒）。
    pub tau_ms: f32,
    /// 淡入延迟与时长（毫秒）；`fade_ms = 0` 表示这一段不碰整窗透明度。
    pub fade_delay_ms: f32,
    pub fade_ms: f32,
    /// 整段动画多长（毫秒）。
    pub total_ms: f32,
    /// 用户设的界面不透明度（动画结束时收敛到它）。
    pub opacity: f32,
    /// 这一段开始时的整窗不透明度。
    ///
    /// 正常入场是 0（从全透明淡进来）、正常出场是 1（从实心淡出去）。
    /// 出场演到一半又被呼出时，这里就是那一刻的不透明度 —— 从半透明接着淡回实心，
    /// 不会先跳到 0 再涨（那一下看着就是「闪」）。
    pub from_alpha: f32,
}

/// 一段滑动动画的句柄：问它跑到哪了 / 跑完没有，或者让它立刻停下。
#[derive(Clone)]
pub struct SlideHandle {
    done: std::sync::Arc<std::sync::atomic::AtomicBool>,
    cancel: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// 当前停在哪（f32 的 bit 表示）：下一段动画从这里接力。
    offset: std::sync::Arc<std::sync::atomic::AtomicU32>,
    /// 当前整窗不透明度（f32 的 bit 表示，不含用户设的界面不透明度）：
    /// 出场演到一半被呼出时，新的一段就从这儿接着淡回来。
    alpha: std::sync::Arc<std::sync::atomic::AtomicU32>,
}

impl SlideHandle {
    /// 跑完了吗（已经停在终点位置）。
    pub fn is_done(&self) -> bool {
        self.done.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// 是不是同一段动画（用来判断「这条完成回调是不是已经过期了」）。
    pub fn same_run(&self, other: &Self) -> bool {
        std::sync::Arc::ptr_eq(&self.done, &other.done)
    }

    /// 现在停在多远的地方（逻辑像素，正数 = 还没归位）。
    pub fn offset(&self) -> f32 {
        f32::from_bits(self.offset.load(std::sync::atomic::Ordering::SeqCst))
    }

    /// 现在的不透明度是多少（0.0 ~ 1.0，不含界面不透明度设置）。
    pub fn alpha(&self) -> f32 {
        f32::from_bits(self.alpha.load(std::sync::atomic::Ordering::SeqCst))
    }

    /// 让它别跑了：线程下一帧就退出，**不会再动窗口**。
    ///
    /// 面板收起 / 被新的一次触发顶掉时必须调用，否则旧线程会接着把
    /// 窗口往它自己的终点拽。取消后 [`Self::offset`] 仍然是它最后画的位置，
    /// 正好拿来当下一段的起点。
    pub fn cancel(&self) {
        self.cancel.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}

/// 在后台线程里把窗口从 `from` 滑到 `end`，返回一个句柄。
///
/// **为什么不走 `background_executor().timer()`**：gpui 的定时器实际按 16ms 一醒
/// （实测采样间隔稳定在 15～16ms），滑 240px 就是每帧 4～20px，看着一格一格。
/// 系统自己的面板跟的是合成帧，165Hz 屏上就是 165 帧。
///
/// 这里直接用一条独立线程 + `DwmFlush()`：它在下一帧合成完成后返回，于是每一帧
/// 都和屏幕刷新对齐。整段动画期间不动 gpui 的渲染（内容一帧都不变），
/// 只改窗口位置和分层不透明度 —— 那些都是可以跨线程调用的 Win32 API。
pub fn run_slide(window: &Window, run: SlideRun) -> SlideHandle {
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    use std::sync::Arc;
    use std::time::Instant;

    let done = Arc::new(AtomicBool::new(false));
    let cancel = Arc::new(AtomicBool::new(false));
    let offset = Arc::new(AtomicU32::new(run.from.to_bits()));
    let alpha = Arc::new(AtomicU32::new(run.from_alpha.to_bits()));
    let handle = SlideHandle {
        done: done.clone(),
        cancel: cancel.clone(),
        offset: offset.clone(),
        alpha: alpha.clone(),
    };
    let Some(hwnd) = hwnd_of(window) else {
        done.store(true, Ordering::SeqCst);
        return handle;
    };
    let scale = window.scale_factor();
    let raw = hwnd.0 as isize;
    let flag = done.clone();

    std::thread::spawn(move || {
        use windows::Win32::Graphics::Dwm::DwmFlush;

        let hwnd = HWND(raw as *mut _);
        // 纯位移：把分层属性摘掉。带着它，DWM 会把这个「刚显示出来的分层窗口」
        // 当成一次出现，补一段约 200ms 的淡入 —— 动画里卡片一直半透明，看起来
        // 就像后面垫了一层。要淡入的那一路由 `set_window_alpha` 自己挂回去。
        if run.fade_ms <= 0.0 {
            unsafe { set_window_layered(hwnd, false) };
        }

        let place = |offset: f32, alpha: f32| {
            let x = run.to.0 + run.direction.0 * offset;
            let y = run.to.1 + run.direction.1 * offset;
            unsafe {
                let _ = SetWindowPos(
                    hwnd,
                    None,
                    (x * scale).round() as i32,
                    (y * scale).round() as i32,
                    0,
                    0,
                    SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_NOZORDER,
                );
                // 只有真的要淡入淡出（`fade_ms > 0`）才碰分层 alpha。
                if run.fade_ms > 0.0 {
                    set_window_alpha(hwnd, (alpha.clamp(0.0, 1.0) * 255.0).round() as u8);
                }
            }
        };

        let start = Instant::now();
        loop {
            if cancel.load(Ordering::SeqCst) {
                // 停在这一帧的位置上，别再动窗口 —— 下一段动画从这儿接力。
                flag.store(true, Ordering::SeqCst);
                return;
            }
            let ms = start.elapsed().as_secs_f32() * 1000.0;
            let p = (ms / run.total_ms.max(1.0)).clamp(0.0, 1.0);
            let offset_now = match run.mode {
                // 指数衰减：一上来就走掉大半、尾巴拖长（系统面板实测就是这条）。
                SlideMode::Enter => run.from * (-ms / run.tau_ms.max(1.0)).exp(),
                // 出场是加速下沉（系统音量条实测约 140ms）。
                SlideMode::Exit => run.from + (run.end - run.from) * p * p,
            };
            let alpha_now = if run.fade_ms <= 0.0 {
                1.0
            } else {
                match run.mode {
                    // 淡入从「这一段开始时不透明度」涨到 1：中途接力时就是
                    // 「从半透明继续变实」，不会先掉到 0 再涨（那一下就是闪）。
                    SlideMode::Enter => {
                        let p = ((ms - run.fade_delay_ms) / run.fade_ms).clamp(0.0, 1.0);
                        run.from_alpha + (1.0 - run.from_alpha) * p
                    }
                    SlideMode::Exit => run.from_alpha * (1.0 - p),
                }
            };
            offset.store(offset_now.to_bits(), Ordering::SeqCst);
            alpha.store(alpha_now.to_bits(), Ordering::SeqCst);
            place(offset_now, alpha_now * run.opacity);
            if ms >= run.total_ms {
                break;
            }
            // 等下一帧合成完成：帧率就跟着屏幕刷新走（60 / 120 / 165Hz 都自动适配）。
            unsafe {
                let _ = DwmFlush();
            }
        }
        // 收尾：`Enter` 精确落在目标位置；`Exit` 停在滑出去的位置。
        let last = match run.mode {
            SlideMode::Enter => 0.0,
            SlideMode::Exit => run.end,
        };
        offset.store(last.to_bits(), Ordering::SeqCst);
        let last_alpha: f32 = if run.mode == SlideMode::Enter {
            1.0
        } else {
            0.0
        };
        alpha.store(last_alpha.to_bits(), Ordering::SeqCst);
        place(last, last_alpha * run.opacity);
        flag.store(true, Ordering::SeqCst);
    });

    handle
}

/// 只挪窗口位置（不改尺寸、不动 z 序），给逐帧动画用。
///
/// 和 [`set_frame`] 的区别是它不查 `GetWindowRect` / `GetClientRect` 的边框差值：
/// 面板 / 提示条都摘掉了 `WS_THICKFRAME` / `WS_CAPTION`，客户区就等于窗口矩形。
/// 动画一帧一次调用，少两次查询差别很明显（实测掉帧就是它们堆出来的）。
pub fn set_origin(window: &Window, x: f32, y: f32, scale: f32) {
    let Some(hwnd) = hwnd_of(window) else {
        return;
    };
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            None,
            (x * scale).round() as i32,
            (y * scale).round() as i32,
            0,
            0,
            SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_NOZORDER,
        );
    }
}

/// 把窗口插到**任务栏后面**（z 序低于 `Shell_TrayWnd`）。
///
/// 系统自己的音量面板就是从任务栏背后升起来的：划过任务栏的那一段被任务栏挡住，
/// 看起来才是「从任务栏后面出来」而不是「浮在任务栏上面」。两边都是 topmost，
/// 所以插到任务栏之后仍然留在 topmost 组里，只是排在任务栏下面。
pub fn behind_taskbar(window: &Window) {
    let Some(hwnd) = hwnd_of(window) else {
        return;
    };
    unsafe {
        let taskbar = match FindWindowW(windows::core::w!("Shell_TrayWnd"), None) {
            Ok(handle) if !handle.0.is_null() => handle,
            // 没有任务栏（被 explorer 重启/第三方任务栏替代）就退化成普通置前，
            // 否则面板可能被别的置顶窗口压在下面点不到。
            _ => return bring_to_front(window),
        };
        let _ = SetWindowPos(
            hwnd,
            Some(taskbar),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE | SWP_NOOWNERZORDER,
        );
    }
}

/// 把面板摆到「最前面、但排在任务栏之后」的位置。
///
/// 两步缺一不可：
///   1. `bring_to_front` 同时做两件事 —— 把窗口设成 TOPMOST（面板创建时并不带这个属性），
///      并排到 topmost 组最前；少了它，别的置顶小工具会压在面板上面。
///   2. `behind_taskbar` 再把它插到任务栏之后 —— 于是它比所有普通置顶窗口都靠前、
///      只低于任务栏，滑过任务栏的那一段和底部那圈阴影都被任务栏挡住。
pub fn present_behind_taskbar(window: &Window) {
    bring_to_front(window);
    behind_taskbar(window);
}

/// 提示条（OSD）摆 z 序：平时压在任务栏后面，全屏时提到最前。
///
/// 为什么全屏要分开处理：无边框全屏的游戏 / 播放器往往自己也挂在 TOPMOST 组里，
/// 把提示条压到任务栏后面就等于排到它下头 —— 用户看到的就是「全屏时提示条
/// 被画面盖住」（全屏禁用那个开关关着也一样，因为这里根本走不到那条判断）。
/// 全屏时任务栏本来就看不见，提到最前没有「从任务栏前面滑过去」的副作用。
///
/// 非全屏也有同样的问题：任务栏之外还有别的置顶窗口，谁抢到了组首，
/// 「任务栏之后」就落到谁下面。所以先问一句这块地方有没有人压着（
/// [`covered_by_other_topmost`]），有就直接提到最前。
///
/// 只用在**滑动阶段**（起点 / 出场前）：那几帧要的就是被任务栏挡住。
/// 停留展示时必须换 [`raise_osd`]。
pub fn place_osd(window: &Window) {
    if crate::platform::fullscreen::is_fullscreen() || covered_by_other_topmost(window) {
        bring_to_front(window);
    } else {
        present_behind_taskbar(window);
    }
}

/// 提示条要占的那块地方，现在有没有「别人的置顶窗口」压着。
///
/// `place_osd` 拿它决定这一轮还要不要往任务栏后面压：任务栏之后并不等于最前，
/// 别的置顶窗口只要抢到了组首就会把提示条整块埋掉 —— 而那时候
/// 「从任务栏背后滑出来」本来也看不见，不如直接提到最前。
///
/// **不能**用 `WindowFromPoint` 采样：提示条自己这会儿正摆在那块位置上，
/// 命中永远落在它身上，看不出底下压着谁。只能顺着 z 序从头往下走，
/// 看「和卡片矩形相交、又不是自己 / 任务栏」的置顶窗口是不是排在它前面。
///
/// 走到自己之后还要继续：z 序最前面偶尔夹着不是置顶的 UIAccess / 输入法窗口，
/// 遇到它们就停会把后面的置顶窗口全漏掉。
fn covered_by_other_topmost(window: &Window) -> bool {
    let Some(hwnd) = hwnd_of(window) else {
        return false;
    };
    let mut card = RECT::default();
    if unsafe { GetWindowRect(hwnd, &mut card) }.is_err() {
        return false;
    }
    let Ok(mut cur) = (unsafe { GetTopWindow(None) }) else {
        return false;
    };
    loop {
        // 走到提示条自己了：排在前面的窗口都检查过，没找到压着的；
        // 后面的在 z 序上都在它下面，不可能盖住它。
        if cur == hwnd {
            return false;
        }
        if unsafe { IsWindowVisible(cur) }.as_bool() {
            let ex = unsafe { GetWindowLongPtrW(cur, GWL_EXSTYLE) } as u32;
            if ex & WS_EX_TOPMOST.0 != 0 && !is_taskbar(cur) {
                let mut other = RECT::default();
                if unsafe { GetWindowRect(cur, &mut other) }.is_ok() && overlaps(&other, &card) {
                    return true;
                }
            }
        }
        cur = match unsafe { GetWindow(cur, GW_HWNDNEXT) } {
            Ok(next) if !next.is_invalid() => next,
            _ => return false,
        };
    }
}

/// 提示条现在是 z 序最前面的那个顶层窗口吗（一次 `GetTopWindow`，很便宜）。
///
/// 显示期间每帧问一遍：新冒出来的窗口会插到 z 序最前面（置顶窗口还会插到
/// TOPMOST 组首），不处理的话它就压在提示条上 —— 用户看到的是「OSD 出来以后
/// 又弹个窗口，提示条被盖住了」。
pub fn osd_is_top(window: &Window) -> bool {
    let Some(hwnd) = hwnd_of(window) else {
        return false;
    };
    matches!(unsafe { GetTopWindow(None) }, Ok(top) if top == hwnd)
}

/// 有别的置顶窗口压在提示条上吗。
///
/// 要枚举 z 序（比 [`osd_is_top`] 贵得多），只在组首已经不是自己时问。
pub fn osd_is_covered(window: &Window) -> bool {
    covered_by_other_topmost(window)
}

/// 正在「保顶」的提示条窗口（0 = 没有）。
static WATCHED_OSD: std::sync::atomic::AtomicIsize = std::sync::atomic::AtomicIsize::new(0);
/// 钩子线程是不是已经起来了（只起一次）。
static WATCHER_READY: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 提示条显示期间盯住 z 序：谁插到它前面就立刻顶回去。
///
/// `tick` 里的 4ms 轮询受系统定时器精度限制，实测被插队后要 30ms 左右才抢回来
/// （桌面右键菜单这种置顶弹窗会实打实亮一帧）。这里换成事件钩子：窗口一重排就
/// 在 1ms 量级把提示条提到 TOPMOST 组首，空闲时零开销。
pub fn watch_osd_top(window: &Window) {
    let Some(hwnd) = hwnd_of(window) else {
        return;
    };
    WATCHED_OSD.store(hwnd.0 as isize, std::sync::atomic::Ordering::SeqCst);
    if !WATCHER_READY.swap(true, std::sync::atomic::Ordering::SeqCst) {
        spawn_reorder_watcher();
    }
}

/// 提示条收起来了：不用再抢 z 序。
pub fn unwatch_osd_top() {
    WATCHED_OSD.store(0, std::sync::atomic::Ordering::SeqCst);
}

/// 钩子要一个带消息循环的线程（`WINEVENT_OUTOFCONTEXT` 的回调在里面跑）。
fn spawn_reorder_watcher() {
    use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent};
    use windows::Win32::UI::WindowsAndMessaging::{
        DispatchMessageW, GetMessageW, MSG, TranslateMessage, WINEVENT_OUTOFCONTEXT,
    };

    std::thread::spawn(move || unsafe {
        use windows::Win32::UI::WindowsAndMessaging::EVENT_OBJECT_REORDER;

        let hook = SetWinEventHook(
            EVENT_OBJECT_REORDER,
            EVENT_OBJECT_REORDER,
            None,
            Some(reorder_callback),
            0,
            0,
            WINEVENT_OUTOFCONTEXT,
        );
        let mut message = MSG::default();
        while GetMessageW(&mut message, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
        if !hook.is_invalid() {
            let _ = UnhookWinEvent(hook);
        }
    });
}

/// z 序一重排就看一眼：提示条是不是已经被挤下去了。
unsafe extern "system" fn reorder_callback(
    _hook: windows::Win32::UI::Accessibility::HWINEVENTHOOK,
    _event: u32,
    _hwnd: HWND,
    _object: i32,
    _child: i32,
    _thread: u32,
    _time: u32,
) {
    use std::sync::atomic::Ordering;

    let raw = WATCHED_OSD.load(Ordering::SeqCst);
    if raw == 0 {
        return;
    }
    let osd = HWND(raw as *mut _);
    let Ok(top) = (unsafe { GetTopWindow(None) }) else {
        return;
    };
    if top == osd {
        return;
    }
    // 组首换人了（新窗口、菜单、其它置顶软件）：立刻把提示条顶回去。
    // 只动 z 序，不带 SWP_NOZORDER 之外的活 —— 位置 / 尺寸 / 焦点都不碰。
    unsafe {
        let _ = SetWindowPos(
            osd,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOOWNERZORDER | SWP_NOACTIVATE,
        );
    }
}

/// 两个矩形是否相交（只判有没有，不要交集本身）。
fn overlaps(a: &RECT, b: &RECT) -> bool {
    a.left < b.right && b.left < a.right && a.top < b.bottom && b.top < a.bottom
}

/// 任务栏（主屏 / 副屏各一扇）。
fn is_taskbar(hwnd: HWND) -> bool {
    let mut buffer = [0u16; 64];
    let length = unsafe { GetClassNameW(hwnd, &mut buffer) };
    if length <= 0 {
        return false;
    }
    let class = String::from_utf16_lossy(&buffer[..length as usize]);
    matches!(class.as_str(), "Shell_TrayWnd" | "Shell_SecondaryTrayWnd")
}

/// 提示条（OSD）进入展示阶段：提到 TOPMOST 组**最前**，压过所有置顶窗口。
///
/// 和 [`place_osd`] 的区别只在非全屏时：那条路把它插到任务栏后面，原本是为了
/// 「从任务栏背后滑出来」的观感，代价是别的置顶窗口只要排在任务栏前面
/// （很多播放器、悬浮窗都会抢组首），提示条就落到它们下面、干脆看不见。
/// 停留期间任务栏和卡片不重叠，提到最前没有任何看得出的副作用。
pub fn raise_osd(window: &Window) {
    bring_to_front(window);
}

/// 「整扇窗现在不接鼠标」的标记（按 HWND 记）。
///
/// 提示条只在卡片本体上要点击（点图标静音、滚轮调音量），影子那一圈不该吃鼠标：
/// `WS_EX_TRANSPARENT` 是整窗的开关，所以得由 OSD 的 tick 跟着光标位置来回切。
fn hollow_windows() -> &'static std::sync::Mutex<std::collections::HashMap<isize, bool>> {
    static HOLLOW: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<isize, bool>>> =
        std::sync::OnceLock::new();
    HOLLOW.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// 增删 `WS_EX_TRANSPARENT`（值没变就不碰窗口）。
fn apply_click_through(hwnd: HWND, want: bool) {
    use windows::Win32::UI::WindowsAndMessaging::WS_EX_TRANSPARENT;

    let style = unsafe { GetWindowLongPtrW(hwnd, GWL_EXSTYLE) };
    let on = style & WS_EX_TRANSPARENT.0 as isize != 0;
    if on == want {
        return;
    }
    let style = if want {
        style | WS_EX_TRANSPARENT.0 as isize
    } else {
        style & !(WS_EX_TRANSPARENT.0 as isize)
    };
    unsafe { SetWindowLongPtrW(hwnd, GWL_EXSTYLE, style) };
}

/// 提示条：现在整扇窗要不要让点击穿过去。
///
/// `WM_NCHITTEST` 回 `HTTRANSPARENT` 只在同一个线程的窗口之间转发 —— 窗口下面
/// 是别的进程时，那一下点击照样被吃掉（实测过）。按区域穿透只能整窗
/// `WS_EX_TRANSPARENT`，由 OSD 的 tick（4ms 一次）跟着光标在不在卡片上来回切。
pub fn set_click_through(window: &Window, through: bool) {
    let Some(hwnd) = hwnd_of(window) else {
        return;
    };
    {
        let mut hollow = hollow_windows()
            .lock()
            .unwrap_or_else(|poison| poison.into_inner());
        hollow.insert(hwnd.0 as isize, through);
    }
    apply_click_through(hwnd, through);
}

/// 整窗透明时把点击让给下面：`alpha = 0` 挂 `WS_EX_TRANSPARENT`，一有可见度就摘掉。
///
/// Windows 的命中测试只看矩形、不看 alpha —— 全透明的分层窗口照样把点击吃掉。
/// 面板「淡入淡出」这一档收起时**不挪窗口**，只把 alpha 压到 0，于是它就隐形地
/// 杵在屏幕角落上：任务栏在顶上时，面板贴的正是右上角，那块地方是窗口的关闭按钮，
/// 用户点不着还找不到是谁挡的（关掉动画时 `park_window` 走的是同一条路）。
///
/// 挪到屏幕外是另一条路；这里兜住所有「看不见但还在屏上」的情况，包括动画收尾那一帧。
unsafe fn sync_click_through(hwnd: HWND, alpha: u8) {
    // 提示条自己按光标位置开关的穿透也要算进来：淡出那一帧 alpha 从 255 往下掉，
    // 不能顺手把「影子那一圈穿透」重新打开成「整窗可点」。
    let hollow = hollow_windows()
        .lock()
        .ok()
        .and_then(|windows| windows.get(&(hwnd.0 as isize)).copied())
        .unwrap_or(false);
    apply_click_through(hwnd, alpha == 0 || hollow);
}

/// 挂上 / 摘掉 `WS_EX_LAYERED`（改完必须再来一次 FRAMECHANGED 才真的生效）。
///
/// 分层窗口和不分层窗口在 DWM 里走两条合成路径：**挂着的窗口每次显示都会被
/// 当成「一次出现」**，DWM 补一段约 200ms 的淡入 —— 滑入 / 滑出的全程卡片都
/// 是半透明的，看起来就像后面垫了一层。纯位移的动画必须把它摘掉。
unsafe fn set_window_layered(hwnd: HWND, layered: bool) {
    use windows::Win32::UI::WindowsAndMessaging::WS_EX_LAYERED;

    let style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
    let has = style & WS_EX_LAYERED.0 as isize != 0;
    if has == layered {
        return;
    }
    let next = if layered {
        style | WS_EX_LAYERED.0 as isize
    } else {
        style & !(WS_EX_LAYERED.0 as isize)
    };
    SetWindowLongPtrW(hwnd, GWL_EXSTYLE, next);
    let _ = SetWindowPos(
        hwnd,
        None,
        0,
        0,
        0,
        0,
        SWP_FRAMECHANGED
            | SWP_NOMOVE
            | SWP_NOSIZE
            | SWP_NOZORDER
            | SWP_NOACTIVATE
            | SWP_NOOWNERZORDER,
    );
}

/// 改整窗不透明度（0 ~ 255 的字节）。
///
/// **满不透明就摘掉分层属性**：`alpha = 255` 的窗口和不分层窗口画面上一样，
/// 但分层那一路会被 DWM 补出现淡入（见 [`set_window_layered`]）。
/// 所以不透明的窗口一律不留 `WS_EX_LAYERED`，只有真的半透明（用户把界面
/// 不透明度调低、或者正在淡入淡出）才挂着它。
unsafe fn set_window_alpha(hwnd: HWND, alpha: u8) {
    use windows::Win32::{
        Foundation::COLORREF,
        UI::WindowsAndMessaging::{LWA_ALPHA, SetLayeredWindowAttributes},
    };

    if alpha >= 255 {
        set_window_layered(hwnd, false);
    } else {
        set_window_layered(hwnd, true);
        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), alpha, LWA_ALPHA);
    }
    // 全透明 = 看不见，却还占着那块屏幕：不挂穿透的话点击会被它吃掉。
    sync_click_through(hwnd, alpha);
}

/// 整个窗口的不透明度（0.0 ~ 1.0），照搬 QcClip 的 `set_window_opacity`。
///
/// 弹出时必须先把窗口设成全透明再 `ShowWindow`：窗口一露头，DWM 会立刻画出
/// 圆角框和一圈阴影，此时内容还没提交，看着就是「先闪一个透明的空框」。
/// （全透明那一路挂的是分层窗口，所以这两个动作要一起看：[`set_window_alpha`]。）
pub fn set_opacity(window: &Window, opacity: f32) {
    let Some(hwnd) = hwnd_of(window) else {
        return;
    };
    let alpha = (opacity.clamp(0.0, 1.0) * 255.0).round() as u8;
    unsafe {
        set_window_alpha(hwnd, alpha);
    }
}

/// 逻辑坐标转物理像素后定位窗口（保持右下角锚点由调用方计算）。
///
/// gpui 默认会给窗口留 WS_THICKFRAME 的边框（左右各 8px、上下各 4px），
/// 客户区因此比窗口矩形小 16×8。面板窗口在 `configure_popup` 里已摘掉这个样式，
/// 但这里仍然按实测差值补偿：样式被系统改回去、或不同 DPI 下差值和预期不一致时，
/// 位置和尺寸照样是准的。
pub fn set_frame(window: &Window, bounds: Bounds<Pixels>, scale: f32) {
    let Some(hwnd) = hwnd_of(window) else {
        return;
    };
    let target_x = (bounds.origin.x.as_f32() * scale).round() as i32;
    let target_y = (bounds.origin.y.as_f32() * scale).round() as i32;
    let target_w = (bounds.size.width.as_f32() * scale).round() as i32;
    let target_h = (bounds.size.height.as_f32() * scale).round() as i32;
    let flags = SWP_NOACTIVATE | SWP_NOOWNERZORDER | SWP_NOZORDER;

    unsafe {
        let mut win = windows::Win32::Foundation::RECT::default();
        let mut client = windows::Win32::Foundation::RECT::default();
        let _ = GetWindowRect(hwnd, &mut win);
        let _ = GetClientRect(hwnd, &mut client);
        // 窗口还没真正创建完时 GetClientRect 可能返回 0，差值不能按这个算，
        // 否则会把窗口按整个宽度推出去。
        let (dw, dh) = frame_delta(&win, &client);
        let _ = SetWindowPos(
            hwnd,
            None,
            target_x - dw / 2,
            target_y - dh / 2,
            target_w + dw,
            target_h + dh,
            flags,
        );

        // 样式切换（摘 THICKFRAME / DPI 变化）后差值是新的，再量一次兜底。
        let _ = GetWindowRect(hwnd, &mut win);
        let _ = GetClientRect(hwnd, &mut client);
        let (dw2, dh2) = frame_delta(&win, &client);
        if (dw2, dh2) != (dw, dh) {
            let _ = SetWindowPos(
                hwnd,
                None,
                target_x - dw2 / 2,
                target_y - dh2 / 2,
                target_w + dw2,
                target_h + dh2,
                flags,
            );
        }
    }
}

/// 窗口矩形与客户区之差（左右/上下边框合计）。客户区为空时按 0 处理。
fn frame_delta(
    win: &windows::Win32::Foundation::RECT,
    client: &windows::Win32::Foundation::RECT,
) -> (i32, i32) {
    let client_w = client.right - client.left;
    let client_h = client.bottom - client.top;
    if client_w <= 0 || client_h <= 0 {
        return (0, 0);
    }
    (
        (win.right - win.left) - client_w,
        (win.bottom - win.top) - client_h,
    )
}

pub fn bring_to_front(window: &Window) {
    let Some(hwnd) = hwnd_of(window) else {
        return;
    };
    unsafe {
        // 必须带 SWP_NOACTIVATE：OSD 这样的提示条只该「浮上来」，不该抢前台。
        // 少了它，带 WS_EX_NOACTIVATE 的窗口照样会被 ActivateWindow，
        // 全屏游戏会被切出去（实测：提示条一弹，前台就变了）。
        let _ = ShowWindow(hwnd, SW_SHOWNA);
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOPMOST),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOOWNERZORDER | SWP_SHOWWINDOW | SWP_NOACTIVATE,
        );
    }
}

/// 面板「置顶」开关：`true` 钉在所有普通窗口上面，`false` 回到普通层级。
///
/// 只改 Z 序，不碰可见性、不抢焦点（NOACTIVATE）。
pub fn set_topmost(window: &Window, topmost: bool) {
    let Some(hwnd) = hwnd_of(window) else {
        return;
    };
    let insert_after = if topmost {
        HWND_TOPMOST
    } else {
        HWND_NOTOPMOST
    };
    unsafe {
        let _ = SetWindowPos(
            hwnd,
            Some(insert_after),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_NOOWNERZORDER | SWP_NOACTIVATE,
        );
    }
}

/// 把普通窗口提到最前，但**不**设成置顶。
///
/// 设置窗口用这个：`bring_to_front` 会走 `HWND_TOPMOST`，窗口一旦被它提过一次，
/// 之后就永远压在别的窗口上面（用户明确不要那个效果）。
pub fn raise(window: &Window) {
    let Some(hwnd) = hwnd_of(window) else {
        return;
    };
    unsafe {
        let _ = ShowWindow(hwnd, SW_RESTORE);
        let _ = SetWindowPos(
            hwnd,
            Some(HWND_TOP),
            0,
            0,
            0,
            0,
            SWP_NOMOVE | SWP_NOSIZE | SWP_SHOWWINDOW,
        );
        let _ = SetForegroundWindow(hwnd);
    }
}

/// 抢前台焦点（不用 gpui 的 activate：它会重放创建时的窗口位置，把面板弹回原点）。
pub fn activate(window: &Window) {
    let Some(hwnd) = hwnd_of(window) else {
        return;
    };
    unsafe {
        let _ = SetForegroundWindow(hwnd);
    }
}

/// 设置窗口：只调 DWM 的圆角与描边，**不动窗口样式**。
///
/// 这里一个字都不能改 GWL_STYLE。gpui 的 Windows 后端用
/// `WS_EX_NOREDIRECTIONBITMAP` + DirectComposition 画内容，客户区尺寸由
/// `WM_NCCALCSIZE` 决定（`hide_title_bar` 时只把 top 还原，左右 8px、底部 4px
/// 仍算边框）。把 `WS_CAPTION` / `WS_THICKFRAME` 剥掉会让系统的边框计算和 gpui
/// 记下的 `border_offset` 对不上，窗口照样显示、DWM 照样画阴影，但合成表面拿不到
/// 有效内容 —— 表现就是「整个设置窗口全透明，只剩一圈阴影」。
/// 弹窗能剥是因为它带 `WS_EX_LAYERED`，走的是另一条合成路径。
///
/// 客户区比窗口小 16×8 这件事由 gpui 自己补偿（`calculate_window_rect`），
/// 我们只要按客户区尺寸申请窗口就不会有缝。
pub fn configure_settings_window(window: &Window) {
    let Some(hwnd) = hwnd_of(window) else {
        return;
    };
    apply_app_icon(hwnd);
    unsafe {
        let corner = DWMWCP_ROUND;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_WINDOW_CORNER_PREFERENCE,
            &corner as *const _ as _,
            std::mem::size_of_val(&corner) as u32,
        );
        let border = DWMWA_COLOR_NONE;
        let _ = DwmSetWindowAttribute(
            hwnd,
            DWMWA_BORDER_COLOR,
            &border as *const _ as _,
            std::mem::size_of_val(&border) as u32,
        );
    }
}

/// 给窗口挂上编进 exe 的那张图标（任务栏按钮 / Alt+Tab / 标题栏左上角）。
///
/// 资源 id 由 `build.rs` 写进 `bolume.rc`：1 = 浅底、2 = 红底，按配置里选的配色取；
/// 从这里按窗口的 DPI 取小图标和大图标两档，再通过 `WM_SETICON` 交给窗口。
/// 取不到就什么都不做——系统会退回 exe 的图标，依然是对的。
fn apply_app_icon(hwnd: HWND) {
    use windows::Win32::Foundation::HINSTANCE;
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        ICON_BIG, ICON_SMALL, IMAGE_ICON, LR_DEFAULTCOLOR, LoadImageW, SendMessageW, WM_SETICON,
    };
    use windows::core::PCWSTR;

    let module = unsafe { GetModuleHandleW(None) }.unwrap_or_default();
    let id = crate::platform::app_icon::resource_id(crate::platform::app_icon::style());
    for (which, size) in [(ICON_SMALL, 16i32), (ICON_BIG, 32)] {
        let Ok(icon) = (unsafe {
            LoadImageW(
                Some(HINSTANCE(module.0)),
                // MAKEINTRESOURCE(id)：整数 id 直接当指针传。
                PCWSTR(id as *const u16),
                IMAGE_ICON,
                size,
                size,
                LR_DEFAULTCOLOR,
            )
        }) else {
            continue;
        };
        unsafe {
            SendMessageW(
                hwnd,
                WM_SETICON,
                Some(windows::Win32::Foundation::WPARAM(which as usize)),
                Some(windows::Win32::Foundation::LPARAM(icon.0 as isize)),
            );
        }
    }
}

/// 把新配色套到已经开着的窗口上（设置页切换「软件图标配色」之后调）。
///
/// 只认本进程的顶层窗口：别的程序不能碰，系统窗口也没有意义。
pub fn refresh_app_icons() {
    use windows::Win32::Foundation::{LPARAM, TRUE};
    use windows::core::BOOL;
    use windows::Win32::UI::WindowsAndMessaging::{EnumWindows, GetWindowThreadProcessId};

    unsafe extern "system" fn visit(hwnd: HWND, lparam: LPARAM) -> BOOL {
        let pid = lparam.0 as u32;
        let mut owner = 0u32;
        unsafe {
            GetWindowThreadProcessId(hwnd, Some(&mut owner as *mut u32));
        }
        if owner == pid {
            apply_app_icon(hwnd);
        }
        TRUE
    }

    let pid = std::process::id();
    unsafe {
        let _ = EnumWindows(Some(visit), LPARAM(pid as isize));
    }
}

/// 让设置窗口重画一次。
///
/// gpui 在 Windows 上的普通窗口带 `WS_EX_NOREDIRECTIONBITMAP`，系统不会为它
/// 维护重定向表面，`RedrawWindow(RDW_INVALIDATE)` 也就**不会**产生更新区域、
/// 更不会派发 WM_PAINT。`Window::refresh()` 只把内部标记置脏，真正把这一帧
/// 送到屏幕上还差一条 WM_PAINT（平台只在 WM_PAINT / WM_SIZE 里驱动绘制）。
/// 所以这里直接投递一条，走窗口过程里的 `draw_window`：
/// 脏了才重画，没脏就只是空过一帧。
///
/// 必须用 Post 而不是 Send：Send 会在当前调用栈里重入 gpui 的 App。
pub fn post_repaint(window: &mut Window) {
    window.refresh();
    let Some(hwnd) = hwnd_of(window) else {
        return;
    };
    unsafe {
        let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
            Some(hwnd),
            windows::Win32::UI::WindowsAndMessaging::WM_PAINT,
            windows::Win32::Foundation::WPARAM(0),
            windows::Win32::Foundation::LPARAM(0),
        );
    }
}

/// 任务栏贴在屏幕的哪条边（决定面板从哪个方向长出来）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TaskbarEdge {
    Top,
    Bottom,
    Left,
    Right,
}

/// 读任务栏位置。自动隐藏时系统仍会给出它停靠的边。
pub fn taskbar_edge() -> TaskbarEdge {
    use windows::Win32::UI::Shell::{
        ABE_LEFT, ABE_RIGHT, ABE_TOP, ABM_GETTASKBARPOS, APPBARDATA, SHAppBarMessage,
    };

    // 调试：BOLUME_TASKBAR_EDGE=top|bottom|left|right 可以模拟任务栏在四个方向，
    // 不必真的去改系统设置。
    if let Ok(forced) = std::env::var("BOLUME_TASKBAR_EDGE") {
        match forced.trim().to_ascii_lowercase().as_str() {
            "top" => return TaskbarEdge::Top,
            "left" => return TaskbarEdge::Left,
            "right" => return TaskbarEdge::Right,
            "bottom" => return TaskbarEdge::Bottom,
            _ => {}
        }
    }

    let mut data = APPBARDATA {
        cbSize: std::mem::size_of::<APPBARDATA>() as u32,
        ..Default::default()
    };
    let ok = unsafe { SHAppBarMessage(ABM_GETTASKBARPOS, &mut data) };
    if ok == 0 {
        return TaskbarEdge::Bottom;
    }
    match data.uEdge {
        edge if edge == ABE_TOP => TaskbarEdge::Top,
        edge if edge == ABE_LEFT => TaskbarEdge::Left,
        edge if edge == ABE_RIGHT => TaskbarEdge::Right,
        _ => TaskbarEdge::Bottom,
    }
}
