//! 全屏检测：前台是游戏 / 全屏视频时，按配置把全局快捷键让出去。
//!
//! 两条互补的判断，任一条命中就算「全屏」：
//! - `SHQueryUserNotificationState`：系统自己的看法，正在全屏（D3D 独占、
//!   演示模式、全屏窗口）时会返回 BUSY / RUNNING_D3D_FULL_SCREEN / PRESENTATION_MODE。
//!   这是任务栏能不能冒出来的同一套判据，比自己去比矩形准。
//! - 窗口矩形比对：有些无边框全屏游戏不改变通知状态，但窗口确实盖满了整块显示器。
//!
//! 自己进程的窗口永远不算：设置页开成全屏大小也不该把自己的快捷键关掉。

use windows::Win32::{
    Foundation::{HWND, RECT},
    Graphics::Gdi::{GetMonitorInfoW, MONITOR_DEFAULTTONEAREST, MONITORINFO, MonitorFromWindow},
    UI::{
        Shell::{
            QUNS_BUSY, QUNS_PRESENTATION_MODE, QUNS_RUNNING_D3D_FULL_SCREEN,
            SHQueryUserNotificationState,
        },
        WindowsAndMessaging::{
            GetClassNameW, GetForegroundWindow, GetWindowRect, GetWindowThreadProcessId,
        },
    },
};

/// 前台窗口的进程名（小写、带 `.exe`）。取不到就是空串。
///
/// OSD 的黑名单 / 白名单按这个比：只认进程名，不认窗口标题 ——
/// 同一个播放器换首歌标题就变，按标题比会漏。
pub fn foreground_process_name() -> String {
    file_name(&foreground_exe_path())
}

/// 前台窗口进程的**完整** exe 路径（黑名单按路径比，才能分得清同名程序）。
pub fn foreground_exe_path() -> String {
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.is_invalid() {
        return String::new();
    }
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    process_path(pid)
}

/// 前台进程**和它的祖先进程**的完整 exe 路径（自己排第一）。
///
/// 为什么要往上找：有些程序把界面交给另一个进程画 —— DNF 就是这样，
/// 可见窗口 `TitleCrossRenderWindow` 属于 `CrossProxy.exe`，而 `CrossProxy.exe`
/// 是 `DNF.exe` 拉起来的子进程。只比窗口那个进程的话，「禁用 DNF.exe」永远不生效
/// （用户拖到游戏窗口上，拾到的也是 CrossProxy.exe）。
///
/// 结果按 pid 缓存 500ms：托盘滚轮调音量会连着问很多次，
/// 每次都建一次进程快照太浪费。
pub fn foreground_process_chain() -> Vec<String> {
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.is_invalid() {
        return Vec::new();
    }
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if pid == 0 {
        return Vec::new();
    }
    if let Some(chain) = chain_cache_get(pid) {
        return chain;
    }
    let chain = process_chain(pid);
    chain_cache_put(pid, &chain);
    chain
}

/// 某个进程连着它祖先的完整路径（自己排第一，最多往上 6 层）。
pub fn process_chain(pid: u32) -> Vec<String> {
    // 父子关系一次快照拿全，别每爬一层都建一次。
    let parents = process_parents();
    let mut chain = Vec::new();
    let mut current = pid;
    for _ in 0..PROCESS_CHAIN_DEPTH {
        if current == 0 {
            break;
        }
        let path = process_path(current);
        if path.is_empty() {
            break;
        }
        chain.push(path);
        match parents
            .iter()
            .find(|(child, _)| *child == current)
            .map(|(_, parent)| *parent)
        {
            Some(parent) if parent != current && parent != 0 => current = parent,
            _ => break,
        }
    }
    chain
}

/// 全系统的 `(pid, 父 pid)` 表。
fn process_parents() -> Vec<(u32, u32)> {
    use windows::Win32::{
        Foundation::CloseHandle,
        System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
            TH32CS_SNAPPROCESS,
        },
    };

    let Ok(snapshot) = (unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }) else {
        return Vec::new();
    };
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut pairs = Vec::new();
    let mut more = unsafe { Process32FirstW(snapshot, &mut entry) }.is_ok();
    while more {
        pairs.push((entry.th32ProcessID, entry.th32ParentProcessID));
        more = unsafe { Process32NextW(snapshot, &mut entry) }.is_ok();
    }
    let _ = unsafe { CloseHandle(snapshot) };
    pairs
}

/// 往上看几层祖先（够了：自己 → 宿主 → 启动器 → 壳）。
const PROCESS_CHAIN_DEPTH: usize = 6;

/// `(pid, 取的时间, 链)`。
static CHAIN_CACHE: std::sync::Mutex<Option<(u32, std::time::Instant, Vec<String>)>> =
    std::sync::Mutex::new(None);

/// 缓存有效时间。
const CHAIN_TTL: std::time::Duration = std::time::Duration::from_millis(500);

fn chain_cache_get(pid: u32) -> Option<Vec<String>> {
    let cache = CHAIN_CACHE.lock().ok()?;
    let (cached_pid, at, chain) = cache.as_ref()?;
    (*cached_pid == pid && at.elapsed() < CHAIN_TTL).then(|| chain.clone())
}

fn chain_cache_put(pid: u32, chain: &[String]) {
    if let Ok(mut cache) = CHAIN_CACHE.lock() {
        *cache = Some((pid, std::time::Instant::now(), chain.to_vec()));
    }
}

/// 按 pid 取可执行文件的完整路径。取不到就是空串。
pub(crate) fn process_path(pid: u32) -> String {
    use windows::{
        Win32::{
            Foundation::CloseHandle,
            System::Threading::{
                OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
                QueryFullProcessImageNameW,
            },
        },
        core::PWSTR,
    };

    if pid == 0 {
        return String::new();
    }
    let Ok(handle) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }) else {
        return String::new();
    };
    // 长路径（\\?\ 前缀）比 512 长，缓冲区留够，不够再按返回的长度补一次。
    let mut buffer = vec![0u16; 1024];
    let mut length = buffer.len() as u32;
    let ok = unsafe {
        QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            PWSTR(buffer.as_mut_ptr()),
            &mut length,
        )
    };
    let _ = unsafe { CloseHandle(handle) };
    if ok.is_err() {
        return String::new();
    }
    String::from_utf16_lossy(&buffer[..length as usize])
}

/// 路径里的文件名（小写）。
pub(crate) fn file_name(path: &str) -> String {
    path.rsplit(['\\', '/'])
        .next()
        .unwrap_or("")
        .to_ascii_lowercase()
}

/// 前台是不是一个全屏应用（排除 Bolume 自己）。
pub fn foreground_fullscreen() -> bool {
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.is_invalid() {
        return false;
    }
    // 自己的窗口（设置页 / 面板）不算全屏。
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if pid == std::process::id() {
        return false;
    }
    if !covers_monitor(hwnd) {
        return false;
    }
    // 桌面 / 任务栏本身也是「盖满屏幕」的，但它们不是全屏应用。
    if is_shell_window(hwnd) {
        return false;
    }
    true
}

/// 系统认为此刻处于全屏 / 演示状态。
pub fn notification_state_fullscreen() -> bool {
    let Ok(state) = (unsafe { SHQueryUserNotificationState() }) else {
        return false;
    };
    state == QUNS_BUSY || state == QUNS_RUNNING_D3D_FULL_SCREEN || state == QUNS_PRESENTATION_MODE
}

/// 前台全屏（两种判据合并）。
pub fn is_fullscreen() -> bool {
    foreground_fullscreen() || notification_state_fullscreen()
}

/// 窗口矩形是否盖住了它所在显示器的整块区域（允许 1px 的边框误差）。
fn covers_monitor(hwnd: HWND) -> bool {
    let mut window = RECT::default();
    if unsafe { GetWindowRect(hwnd, &mut window) }.is_err() {
        return false;
    }
    let monitor = unsafe { MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST) };
    if monitor.is_invalid() {
        return false;
    }
    let mut info = MONITORINFO {
        cbSize: std::mem::size_of::<MONITORINFO>() as u32,
        ..Default::default()
    };
    if !unsafe { GetMonitorInfoW(monitor, &mut info) }.as_bool() {
        return false;
    }
    let screen = info.rcMonitor;
    // 用「小于」而不是「等于」：无边框全屏的窗口往往比显示器大几个像素。
    window.left <= screen.left + 1
        && window.top <= screen.top + 1
        && window.right >= screen.right - 1
        && window.bottom >= screen.bottom - 1
}

/// 桌面、任务栏、开始菜单这类外壳窗口。
pub(crate) fn is_shell_window(hwnd: HWND) -> bool {
    let mut buffer = [0u16; 64];
    let length = unsafe { GetClassNameW(hwnd, &mut buffer) };
    if length <= 0 {
        return false;
    }
    let class = String::from_utf16_lossy(&buffer[..length as usize]);
    matches!(
        class.as_str(),
        "Progman"
            | "WorkerW"
            | "Shell_TrayWnd"
            | "Shell_SecondaryTrayWnd"
            | "NotifyIconOverflowWindow"
            | "MultitaskingViewFrame"
            | "XamlExplorerHostIslandWindow"
    )
}
