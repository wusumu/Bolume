//! Win32 窗口细节：无边框弹出窗的圆角/阴影、显示隐藏、精确定位。

use gpui::{Bounds, Pixels, Window};
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use windows::Win32::{
    Foundation::HWND,
    Graphics::Dwm::{
        DWMWA_BORDER_COLOR, DWMWA_COLOR_NONE, DWMWA_TRANSITIONS_FORCEDISABLED,
        DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND, DWMWCP_ROUND, DwmSetWindowAttribute,
    },
    UI::WindowsAndMessaging::{
        GWL_EXSTYLE, GWL_STYLE, GetClientRect, GetWindowLongPtrW, GetWindowRect, HWND_NOTOPMOST,
        HWND_TOP, HWND_TOPMOST, SW_HIDE, SW_RESTORE, SW_SHOW, SW_SHOWNA, SWP_FRAMECHANGED,
        SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOOWNERZORDER, SWP_NOSIZE, SWP_NOZORDER, SWP_SHOWWINDOW,
        SetForegroundWindow, SetWindowLongPtrW, SetWindowPos, ShowWindow, WS_CAPTION,
        WS_EX_NOACTIVATE, WS_EX_TOOLWINDOW, WS_SYSMENU, WS_THICKFRAME,
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

/// 整个窗口的不透明度（0.0 ~ 1.0），照搬 QcClip 的 `set_window_opacity`。
///
/// 弹出时必须先把窗口设成全透明再 `ShowWindow`：窗口一露头，DWM 会立刻画出
/// 圆角框和一圈阴影，此时内容还没提交，看着就是「先闪一个透明的空框」。
pub fn set_opacity(window: &Window, opacity: f32) {
    use windows::Win32::{
        Foundation::COLORREF,
        UI::WindowsAndMessaging::{LWA_ALPHA, SetLayeredWindowAttributes, WS_EX_LAYERED},
    };

    let Some(hwnd) = hwnd_of(window) else {
        return;
    };
    let alpha = (opacity.clamp(0.0, 1.0) * 255.0).round() as u8;
    unsafe {
        let ex_style = GetWindowLongPtrW(hwnd, GWL_EXSTYLE);
        if ex_style & WS_EX_LAYERED.0 as isize == 0 {
            SetWindowLongPtrW(hwnd, GWL_EXSTYLE, ex_style | WS_EX_LAYERED.0 as isize);
        }
        let _ = SetLayeredWindowAttributes(hwnd, COLORREF(0), alpha, LWA_ALPHA);
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
/// 资源 id 是 1，由 `build.rs` 写进 `bolume.rc`；从这里按窗口的 DPI 取
/// 小图标和大图标两档，再通过 `WM_SETICON` 交给窗口。
/// 取不到就什么都不做——系统会退回 exe 的图标，依然是对的。
fn apply_app_icon(hwnd: HWND) {
    use windows::Win32::Foundation::HINSTANCE;
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        ICON_BIG, ICON_SMALL, IMAGE_ICON, LR_DEFAULTCOLOR, LoadImageW, SendMessageW, WM_SETICON,
    };
    use windows::core::PCWSTR;

    let module = unsafe { GetModuleHandleW(None) }.unwrap_or_default();
    for (which, size) in [(ICON_SMALL, 16i32), (ICON_BIG, 32)] {
        let Ok(icon) = (unsafe {
            LoadImageW(
                Some(HINSTANCE(module.0)),
                // MAKEINTRESOURCE(1)：整数 id 直接当指针传。
                PCWSTR(1 as *const u16),
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
