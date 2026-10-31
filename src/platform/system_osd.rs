//! 「禁用 Windows 音量条 / 播放信息卡片」的窗口级实现。
//!
//! 光靠键盘钩子吞键是不够的，Win10 上尤其明显：
//!
//! - 很多键盘的媒体键走 HID consumer 报告，根本不经过 `WH_KEYBOARD_LL`；
//! - 那条「音量 + 正在播放」的卡片是系统对**媒体会话**的反应，跟按键从哪来无关
//!   （播放器自己改状态、别的软件调音量，一样会弹）。
//!
//! 所以还得直接对那扇窗口下手。到 v0.88 一直是「弹出来就最小化」，问题在于它是
//! 一次性的动作：系统每次显示 OSD 都会把它重新 `ShowWindow` 出来，只能靠轮询再压
//! 一次，中间那一下已经露在屏幕上了；而这扇窗口是**懒创建**的（第一次按音量键才
//! 建），开机后头一次往往连找都找不到。
//!
//! 现在照 ReMOSD / ReVOSD 的做法：给窗口设一个**裁剪区域**（`SetWindowRgn`）。
//! 区域是挂在窗口对象上的属性，设一次就一直有效 —— 之后系统怎么显示这扇窗口，
//! 露出来的都只有区域里那一块，不存在抢时间的问题。窗口还没建就先发两条
//! `WM_APPCOMMAND`（静音再取消静音）让它建出来，音量不受影响。
//!
//! 两套类名（和 HideVolumeOSD 用的是同一组）：
//!
//! - Win10：顶层 `NativeHWNDHost`，子窗口 `DirectUIHWND`；
//! - Win11（22000 起）：顶层 `XamlExplorerHostIslandWindow`，
//!   子窗口 `Windows.UI.Composition.DesktopWindowContentBridge`。
//!
//! 两种裁法，对应设置里那两个开关：
//!
//! - **禁用音量条** → 裁成空区域，整扇窗口什么都不显示；
//! - **只藏播放信息** → 只留左边那一块（96 DPI 下 65×140），也就是音量条本体，
//!   右边那张带封面的「正在播放」卡片被裁掉。那张卡片只在 Win10 上独立于音量条，
//!   Win11 没有这块布局，那条开关在这代系统上不动窗口。
//!
//! 不改注册表、不碰音频链路，音量调节本身完全不受影响。

use std::sync::atomic::{AtomicBool, AtomicIsize, AtomicU8, Ordering};
use std::time::Duration;

use windows::core::{BOOL, PCWSTR};
use windows::Win32::Foundation::{CloseHandle, HWND, LPARAM, RECT, WPARAM};
use windows::Win32::Graphics::Gdi::{
    CreateRectRgn, DeleteObject, GetWindowRgnBox, HGDIOBJ, RGN_ERROR, SetWindowRgn,
};
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, QueryFullProcessImageNameW,
};
use windows::Win32::UI::HiDpi::GetDpiForWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    EnumWindows, FindWindowExW, GetClassNameW, GetShellWindow, GetWindowThreadProcessId, IsIconic,
    IsWindow, IsWindowVisible, SendMessageW, ShowWindow, SW_MINIMIZE, SW_RESTORE, WM_APPCOMMAND,
};

/// 不裁。
const CUT_OFF: u8 = 0;
/// 只藏「正在播放」那张卡片，音量条留着。
const CUT_MEDIA: u8 = 1;
/// 整条不显示。
const CUT_VOLUME: u8 = 2;

/// 当前的裁法（配置变更时由 `set_blocked` 写）。
static CUT: AtomicU8 = AtomicU8::new(CUT_OFF);

/// 轮询线程只起一次。
static WORKER: AtomicBool = AtomicBool::new(false);

/// 认下来的那扇 OSD 窗口（恢复、以及之后每拍都按它来）。
static TARGET: AtomicIsize = AtomicIsize::new(0);

/// 最近一次真正下过手的窗口与裁法。
static APPLIED: AtomicIsize = AtomicIsize::new(0);
static APPLIED_CUT: AtomicU8 = AtomicU8::new(CUT_OFF);

/// 轮询计数（每拍 +1），「两秒兜一次」用它算。
static TICKS: AtomicU8 = AtomicU8::new(0);

/// 轮询间隔。
///
/// 事件驱动做不到：这扇窗口的显示不一定伴随「新建窗口」或「前台切换」这类能挂的
/// 事件（系统只是把已经存在的窗口摆出来），而且它还会被系统重置区域。250ms 比
/// 系统的 3 秒存活期短一个数量级，一轮枚举几十微秒，开着开关时占用可以忽略。
const POLL: Duration = Duration::from_millis(250);

/// Win10 音量条本体（整张卡片左边那一块）在 96 DPI 下的尺寸。
///
/// 这是 ReMOSD「只删播放信息、保留音量条」用的裁剪范围：Win10 那张卡片左边是
/// 竖着的音量块，右边横着接一张媒体卡片，裁到左边这一列正好只留下音量条。
const VOLUME_BLOCK: (f32, f32) = (65.0, 140.0);

/// `APPCOMMAND_VOLUME_MUTE`：只是用来让系统把 OSD 建出来 / 显示出来。
const APPCOMMAND_VOLUME_MUTE: isize = 8;

/// 一代系统上的一套窗口类名。
struct Kind {
    outer: &'static str,
    inner: &'static str,
    /// Win10 那套（播放信息卡片与音量条同窗、且是独立一块）。
    win10: bool,
}

const KINDS: [Kind; 2] = [
    Kind {
        outer: "NativeHWNDHost",
        inner: "DirectUIHWND",
        win10: true,
    },
    Kind {
        outer: "XamlExplorerHostIslandWindow",
        inner: "Windows.UI.Composition.DesktopWindowContentBridge",
        win10: false,
    },
];

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

fn class_of(hwnd: HWND) -> String {
    let mut buf = [0u16; 256];
    let len = unsafe { GetClassNameW(hwnd, &mut buf) };
    if len <= 0 {
        return String::new();
    }
    String::from_utf16_lossy(&buf[..len as usize])
}

/// 这扇窗口属于系统壳吗。
///
/// `NativeHWNDHost` 不止音量条在用（输入法、通知、开始菜单的 XAML 宿主都是它），
/// 所以先按宿主进程筛一道：白名单里的优先。
/// 白名单一个都没命中时，只有**全系统唯一**一个候选才敢动 —— 宁可漏掉，
/// 也不能把别的 XAML 宿主窗口裁成 0×0。
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

/// 这扇窗口下面挂着指定类名的直接子窗口吗。
fn has_child(hwnd: HWND, inner: &[u16]) -> bool {
    let child = unsafe { FindWindowExW(Some(hwnd), None, PCWSTR(inner.as_ptr()), PCWSTR::null()) };
    child.map(|child| !child.0.is_null()).unwrap_or(false)
}

fn top_levels() -> Vec<HWND> {
    let mut raw: Vec<isize> = Vec::new();
    unsafe {
        let _ = EnumWindows(Some(collect), LPARAM(&mut raw as *mut _ as isize));
    }
    raw.into_iter().map(|raw| HWND(raw as *mut _)).collect()
}

/// 按一套类名找：顶层类名相符、**直接子窗口**里有对应内层类名。
///
/// 同一套类名往往不止一扇窗口（Win10 的 `NativeHWNDHost` 还被输入法、开始菜单
/// 这些占着），所以按「壳的进程 + 现在就显示着」排队：
///
/// 1. 壳进程的、正显示着的 —— 触发之后 OSD 就在屏幕上，这条最准；
/// 2. 壳进程的（还没显示 / 刚藏起来）；
/// 3. 只有一个候选时认它（认不出来源，但全系统只有一个，总比不动强）。
///
/// 都不成立就当作没找到 —— 宁可漏掉一次，也不能把别的 XAML 宿主裁成 0×0。
fn find_kind(kind: &Kind) -> Option<HWND> {
    let inner = wide(kind.inner);
    let outer = wide(kind.outer);
    let outer_text = String::from_utf16_lossy(&outer[..outer.len() - 1]);
    let mut owned_visible: Option<HWND> = None;
    let mut owned: Option<HWND> = None;
    let mut others: Vec<HWND> = Vec::new();
    for hwnd in top_levels() {
        if class_of(hwnd) != outer_text {
            continue;
        }
        if !has_child(hwnd, &inner) {
            continue;
        }
        let visible = unsafe { IsWindowVisible(hwnd) }.as_bool();
        if shell_owned(hwnd) == Some(true) {
            if visible && owned_visible.is_none() {
                owned_visible = Some(hwnd);
            }
            if owned.is_none() {
                owned = Some(hwnd);
            }
        } else {
            others.push(hwnd);
        }
    }
    owned_visible
        .or(owned)
        .or_else(|| if others.len() == 1 { others.pop() } else { None })
}

fn osd_window() -> Option<(HWND, bool)> {
    KINDS
        .iter()
        .find_map(|kind| find_kind(kind).map(|hwnd| (hwnd, kind.win10)))
}

/// 这扇窗口现在还是不是我们要裁的那扇（壳可能换了窗口）。
fn looks_like_osd(hwnd: HWND) -> bool {
    let class = class_of(hwnd);
    KINDS.iter().filter(|kind| kind.outer == class).any(|kind| {
        let inner = wide(kind.inner);
        has_child(hwnd, &inner)
    })
}

fn is_win10(hwnd: HWND) -> bool {
    let class = class_of(hwnd);
    KINDS
        .iter()
        .find(|kind| kind.outer == class)
        .map(|kind| kind.win10)
        .unwrap_or(false)
}

/// 让系统把 OSD 窗口建出来 / 显示出来。
///
/// 这扇窗口是懒创建的：开机后第一次按音量键才有，在那之前 `EnumWindows` 里根本
/// 没有它。照 ReMOSD 的办法，向 shell 窗口发两条 `APPCOMMAND_VOLUME_MUTE` ——
/// 静音再取消静音，两次之间什么都没变，但每次都让系统弹出 OSD，窗口就建出来了。
fn trigger() {
    let shell = unsafe { GetShellWindow() };
    if shell.0.is_null() {
        return;
    }
    let command = APPCOMMAND_VOLUME_MUTE << 16;
    for _ in 0..2 {
        unsafe {
            SendMessageW(shell, WM_APPCOMMAND, Some(WPARAM(0)), Some(LPARAM(command)));
        }
        std::thread::sleep(Duration::from_millis(40));
    }
}

/// 找窗口：先看上次那扇还在不在，再枚举一遍，最后才（按需）触发系统把它建出来。
fn ensure_window(allow_trigger: bool) -> Option<(HWND, bool)> {
    let saved = TARGET.load(Ordering::SeqCst);
    if saved != 0 {
        let hwnd = HWND(saved as *mut _);
        if unsafe { IsWindow(Some(hwnd)) }.as_bool() && looks_like_osd(hwnd) {
            return Some((hwnd, is_win10(hwnd)));
        }
        TARGET.store(0, Ordering::SeqCst);
    }
    if let Some(found) = osd_window() {
        return Some(found);
    }
    if !allow_trigger {
        return None;
    }
    // 最多五轮：窗口建出来要一点时间，慢机器上更久。
    for _ in 0..5 {
        trigger();
        std::thread::sleep(Duration::from_millis(250));
        if let Some(found) = osd_window() {
            return Some(found);
        }
    }
    None
}

/// 「只留音量条」那块区域按窗口 DPI 放大后的像素尺寸。
fn media_block_size(hwnd: HWND) -> (i32, i32) {
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    let scale = if dpi == 0 {
        1.0
    } else {
        dpi as f32 / 96.0
    };
    (
        (VOLUME_BLOCK.0 * scale).round() as i32,
        (VOLUME_BLOCK.1 * scale).round() as i32,
    )
}

/// 把窗口裁成该裁的样子。返回 false = 这扇窗口不归我们裁（或没裁成）。
fn apply_cut(hwnd: HWND, cut: u8, win10: bool) -> bool {
    let (width, height) = match cut {
        // 空区域：整扇窗口什么都不画。
        CUT_VOLUME => (0, 0),
        // Win11 的音量条里没有独立的一块播放信息，照 65×140 裁反而会把滑条切掉。
        CUT_MEDIA if win10 => media_block_size(hwnd),
        _ => return false,
    };
    let rgn = unsafe { CreateRectRgn(0, 0, width, height) };
    if rgn.0.is_null() {
        return false;
    }
    if unsafe { SetWindowRgn(hwnd, Some(rgn), true) } == 0 {
        // 失败时区域还归我们，得自己删（成功的话系统接手了）。
        unsafe {
            let _ = DeleteObject(HGDIOBJ(rgn.0));
        }
        // 兜底：至少别让它整块留在屏幕上。
        unsafe {
            let _ = ShowWindow(hwnd, SW_MINIMIZE);
        }
        return false;
    }
    APPLIED.store(hwnd.0 as isize, Ordering::SeqCst);
    APPLIED_CUT.store(cut, Ordering::SeqCst);
    true
}

/// 这一拍要不要重新下手。
///
/// 系统在某些时机会把窗口区域重置掉（重新显示、DPI 变化），所以要周期性核对；
/// 但重设会触发一次重绘 —— 「只留音量条」的时候音量条本身还在屏幕上，
/// 每拍都重设会看见闪，所以那种裁法按区域尺寸判断，只有真被改掉才补。
fn needs_apply(hwnd: HWND, cut: u8) -> bool {
    if APPLIED.load(Ordering::SeqCst) != hwnd.0 as isize
        || APPLIED_CUT.load(Ordering::SeqCst) != cut
    {
        return true;
    }
    if cut == CUT_VOLUME {
        // 空区域看不出来（也不闪），两秒兜一次就够。
        return TICKS.load(Ordering::SeqCst) % 8 == 0;
    }
    let mut rect = RECT::default();
    if unsafe { GetWindowRgnBox(hwnd, &mut rect) } == RGN_ERROR {
        return true;
    }
    let (width, height) = media_block_size(hwnd);
    (rect.right - rect.left - width).abs() > 1 || (rect.bottom - rect.top - height).abs() > 1
}

/// 跑一拍：找窗口、该补就补。返回这一拍有没有认下窗口 —— 认不下来就慢点轮询
/// （有些机器上（比如改了组策略、或声音走的是第三方通道）根本没有这扇窗口，
/// 没必要一直每 250ms 枚举一遍）。
fn refresh(allow_trigger: bool) -> bool {
    let cut = CUT.load(Ordering::SeqCst);
    if cut == CUT_OFF {
        return false;
    }
    let Some((hwnd, win10)) = ensure_window(allow_trigger) else {
        return false;
    };
    TARGET.store(hwnd.0 as isize, Ordering::SeqCst);
    if needs_apply(hwnd, cut) {
        apply_cut(hwnd, cut, win10);
    }
    true
}

/// 开关变化。
///
/// `volume` = 整条不显示；`media` = 只藏播放信息那张卡片。两个都开按音量条算
/// （整条都没了，卡片自然也看不见）。
///
/// 真正的下手在轮询线程里做：触发系统建窗口可能要等一两秒，不能堵住界面线程。
pub fn set_blocked(volume: bool, media: bool) {
    let cut = if volume {
        CUT_VOLUME
    } else if media {
        CUT_MEDIA
    } else {
        CUT_OFF
    };
    CUT.store(cut, Ordering::SeqCst);
    if cut == CUT_OFF {
        WORKER.store(false, Ordering::SeqCst);
        restore();
        return;
    }
    if !WORKER.swap(true, Ordering::SeqCst) {
        std::thread::Builder::new()
            .name("bolume-system-osd".into())
            .spawn(|| {
                // 第一拍允许触发系统把窗口建出来；之后只按已有的窗口补区域，
                // 不能每拍都发静音命令。
                let mut first = true;
                while WORKER.load(Ordering::SeqCst) {
                    let found = refresh(first);
                    first = false;
                    TICKS.fetch_add(1, Ordering::SeqCst);
                    std::thread::sleep(if found { POLL } else { POLL * 4 });
                }
            })
            .ok();
    }
}

/// 区域是不是我们设的那两种形状之一（用来收拾上次崩溃留下的残局）。
fn region_looks_ours(hwnd: HWND) -> bool {
    let mut rect = RECT::default();
    if unsafe { GetWindowRgnBox(hwnd, &mut rect) } == RGN_ERROR {
        return false;
    }
    let (width, height) = media_block_size(hwnd);
    let box_width = rect.right - rect.left;
    let box_height = rect.bottom - rect.top;
    (box_width <= 0 && box_height <= 0)
        || ((box_width - width).abs() <= 1 && (box_height - height).abs() <= 1)
}

/// 诊断用（`--probe-osd`）：把「有没有找到系统那扇 OSD 窗口、它现在长什么样」打出来。
///
/// 「开关开了但音量条照旧弹」只能靠这份判断卡在哪一步：类名不对（一个候选都没有）、
/// 找错了窗口（候选一堆）、还是找到了但区域没设上。**会触发一次静音 + 取消静音**
/// 把窗口建出来，所以跑之前先知道这一点。
pub fn diag_report() -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    let _ = writeln!(
        out,
        "── 系统 OSD 窗口（Win11 = {}）",
        crate::platform::winver::is_windows_11()
    );
    for kind in &KINDS {
        let inner = wide(kind.inner);
        let outer = wide(kind.outer);
        let outer_text = String::from_utf16_lossy(&outer[..outer.len() - 1]);
        let mut hits = 0;
        for hwnd in top_levels() {
            if class_of(hwnd) != outer_text {
                continue;
            }
            let mut rect = RECT::default();
            unsafe {
                let _ = windows::Win32::UI::WindowsAndMessaging::GetWindowRect(hwnd, &mut rect);
            }
            let mut box_rect = RECT::default();
            let region = unsafe { GetWindowRgnBox(hwnd, &mut box_rect) };
            hits += 1;
            let _ = writeln!(
                out,
                "   [{outer_text}] hwnd=0x{:X} rect=({}, {}, {}×{}) 可见={} 子窗口={} 壳进程={:?} 区域={:?}({}×{})",
                hwnd.0 as isize,
                rect.left,
                rect.top,
                rect.right - rect.left,
                rect.bottom - rect.top,
                unsafe { IsWindowVisible(hwnd) }.as_bool(),
                has_child(hwnd, &inner),
                shell_owned(hwnd),
                region.0,
                box_rect.right - box_rect.left,
                box_rect.bottom - box_rect.top,
            );
        }
        if hits == 0 {
            let _ = writeln!(out, "   [{outer_text}] 一个候选都没有");
        }
        let _ = writeln!(
            out,
            "   → 这一套类名最终选中：{:?}",
            find_kind(kind).map(|hwnd| hwnd.0 as isize)
        );
    }

    let _ = writeln!(out, "\n── 触发一次（静音 + 取消静音，音量不变）");
    let found_before = osd_window();
    let _ = writeln!(
        out,
        "   触发前：{:?}",
        found_before.map(|(hwnd, win10)| (hwnd.0 as isize, win10))
    );
    if found_before.is_none() {
        trigger();
        std::thread::sleep(Duration::from_millis(400));
    }
    match osd_window() {
        Some((hwnd, win10)) => {
            let (width, height) = media_block_size(hwnd);
            let _ = writeln!(
                out,
                "   触发后：hwnd=0x{:X} Win10 布局={win10} 只留音量条的区域={width}×{height}",
                hwnd.0 as isize
            );
        }
        None => {
            let _ = writeln!(out, "   触发后仍然没找到 —— 这台机器的 OSD 不走这两套类名");
        }
    }
    out
}

/// 把窗口放回原样。退出程序、关掉开关、以及启动时清理上次的残留都调它。
pub fn restore() {
    let mut targets = Vec::new();
    for raw in [
        APPLIED.swap(0, Ordering::SeqCst),
        TARGET.swap(0, Ordering::SeqCst),
    ] {
        if raw != 0 {
            targets.push(HWND(raw as *mut _));
        }
    }
    // 上次被强制结束（关窗口时被杀、崩溃）留下的：那颗区域还挂在这扇常驻窗口上，
    // 只认我们设过的那两种形状，别人的（哪怕同样是裁过的）不动。
    if let Some((hwnd, _)) = osd_window() {
        if region_looks_ours(hwnd) {
            targets.push(hwnd);
        }
    }
    APPLIED_CUT.store(CUT_OFF, Ordering::SeqCst);
    for hwnd in targets {
        if !unsafe { IsWindow(Some(hwnd)) }.as_bool() {
            continue;
        }
        unsafe {
            let _ = SetWindowRgn(hwnd, None, true);
            if IsIconic(hwnd).as_bool() {
                let _ = ShowWindow(hwnd, SW_RESTORE);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::core::w;
    use windows::Win32::Foundation::{HINSTANCE, LRESULT};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassW, WINDOW_EX_STYLE, WNDCLASSW,
        WS_CHILD, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
    };

    unsafe extern "system" fn stub(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
        unsafe { DefWindowProcW(hwnd, msg, wp, lp) }
    }

    /// 注册窗口类，返回能活到窗口销毁的类名缓冲。
    fn register(class: &str) -> Vec<u16> {
        let name: Vec<u16> = class.encode_utf16().chain(std::iter::once(0)).collect();
        let module = unsafe { GetModuleHandleW(None) }.unwrap();
        let instance = HINSTANCE(module.0);
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

    /// 造一对「顶层 + 内层」的窗口，类名跟 Win10 那条 OSD 一样。
    ///
    /// 这条链路上最容易出错的是类名比对、「直接子窗口」那一层、以及区域到底设上了
    /// 没有 —— 光靠真机看是没法确认的（系统 OSD 只活三秒），所以这里自己造一对。
    #[test]
    fn clips_and_restores_a_window_that_looks_like_the_osd() {
        // 只拿 Win10 那套类名造：`NativeHWNDHost` 是随便注册得起来的（XAML 那套
        // 空壳窗口测不出什么，真机也不在这条路上）。
        let kind = &KINDS[0];
        let outer_name = register(kind.outer);
        let inner_name = register(kind.inner);
        let module = unsafe { GetModuleHandleW(None) }.unwrap();
        let instance = HINSTANCE(module.0);
        let main = unsafe {
            CreateWindowExW(
                WINDOW_EX_STYLE(0),
                PCWSTR(outer_name.as_ptr()),
                w!("FakeOsd"),
                WS_OVERLAPPEDWINDOW,
                80,
                80,
                240,
                200,
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
                200,
                Some(main),
                None,
                Some(instance),
                None,
            )
        }
        .expect("建子窗口");

        assert!(looks_like_osd(main), "应该认出这扇窗口");
        assert!(!region_looks_ours(main), "一开始没有区域");

        // 整条隐藏：空区域（系统回报的方框起点是窗口原点偏移，宽高才是 0）。
        assert!(apply_cut(main, CUT_VOLUME, true));
        let mut rect = RECT::default();
        assert_ne!(unsafe { GetWindowRgnBox(main, &mut rect) }, RGN_ERROR);
        assert_eq!(
            (rect.right - rect.left, rect.bottom - rect.top),
            (0, 0),
            "空区域"
        );
        assert!(region_looks_ours(main), "空区域算我们的");

        // 只留音量条：区域正好是 96 DPI 下那 65×140。
        assert!(apply_cut(main, CUT_MEDIA, true));
        let mut rect = RECT::default();
        assert_ne!(unsafe { GetWindowRgnBox(main, &mut rect) }, RGN_ERROR);
        let (width, height) = media_block_size(main);
        assert_eq!(
            (rect.right - rect.left, rect.bottom - rect.top),
            (width, height)
        );
        assert!(region_looks_ours(main));
        assert!(!needs_apply(main, CUT_MEDIA), "刚设好就不用重设");

        restore();
        let mut rect = RECT::default();
        assert_eq!(
            unsafe { GetWindowRgnBox(main, &mut rect) },
            RGN_ERROR,
            "关掉开关要放回来"
        );

        unsafe {
            let _ = DestroyWindow(main);
        }
    }

    /// 两套类名里只有 Win10 那套谈得上「只藏播放信息」。
    #[test]
    fn media_cut_is_win10_only() {
        assert!(KINDS[0].win10);
        assert!(!KINDS[1].win10);
        // 裁法优先级：音量条整条没了，播放信息自然也看不见。
        assert!(CUT_VOLUME > CUT_MEDIA && CUT_MEDIA > CUT_OFF);
    }
}
