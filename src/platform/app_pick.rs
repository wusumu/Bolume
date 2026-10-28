//! 黑名单的「拾取」入口：拖到窗口上取进程、文件 / 目录选择框。
//!
//! 三件事都只为设置页的「禁用 Bolume 的应用程序」服务：
//! - `left_button_down` / `window_under_cursor`：用户按住「拖动到要禁用的窗口上」
//!   按钮、移到目标窗口松手时，把那个窗口的 exe 取出来；
//! - `pick_path_async`：弹出的「添加…」/「添加目录」选择框。
//!
//! 拖拽不依赖 gpui 的鼠标事件：按钮按下后自己轮询左键，松手时用**屏幕坐标**
//! 找窗口 —— 设置窗本身常常压着目标窗口，所以不能直接用 `WindowFromPoint`，
//! 要沿 Z 序往下跳过自己的窗口（见 `window_under_cursor`）。

use windows::Win32::{
    Foundation::{HWND, POINT, RECT},
    Graphics::Dwm::{DWMWA_CLOAKED, DwmGetWindowAttribute},
    UI::{
        Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON},
        WindowsAndMessaging::{
            GW_HWNDNEXT, GetCursorPos, GetTopWindow, GetWindow, GetWindowRect,
            GetWindowThreadProcessId, IsIconic, IsWindowVisible,
        },
    },
};

/// 拾取到的窗口：进程的完整 exe 路径。
#[derive(Clone, Debug)]
pub struct PickedWindow {
    pub path: String,
}

/// 左键现在按着没有。
///
/// 拖拽拾取靠轮询这个而不是 `on_mouse_up`：指针移出设置窗口后就收不到 release 了。
pub fn left_button_down() -> bool {
    // 高位 = 当前按下状态（低位是「自上次查询以来有没有按过」）。
    (unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) } as u16 & 0x8000) != 0
}

/// 光标底下那个窗口的进程（跳过 Bolume 自己和桌面 / 任务栏）。
///
/// 沿顶层窗口的 Z 序从上往下找第一个「可见、没最小化、矩形包含光标、不是本进程」的窗口。
/// 用 Z 序而不是 `WindowFromPoint`，是因为设置窗往往正压在目标窗口上。
pub fn window_under_cursor() -> Option<PickedWindow> {
    let mut point = POINT::default();
    if unsafe { GetCursorPos(&mut point) }.is_err() {
        return None;
    }
    let mut hwnd = unsafe { GetTopWindow(None) }.ok()?;
    let own_pid = std::process::id();
    while !hwnd.is_invalid() {
        if considers_candidate(hwnd, point, own_pid) {
            let mut pid = 0u32;
            unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
            let path = crate::platform::fullscreen::process_path(pid);
            if !path.is_empty() {
                return Some(PickedWindow { path });
            }
        }
        match unsafe { GetWindow(hwnd, GW_HWNDNEXT) } {
            Ok(next) if !next.is_invalid() => hwnd = next,
            _ => break,
        }
    }
    None
}

/// 这个顶层窗口能不能当成「用户拖上来的目标」。
fn considers_candidate(hwnd: HWND, point: POINT, own_pid: u32) -> bool {
    if !unsafe { IsWindowVisible(hwnd) }.as_bool() || unsafe { IsIconic(hwnd) }.as_bool() {
        return false;
    }
    if crate::platform::fullscreen::is_shell_window(hwnd) || is_cloaked(hwnd) {
        return false;
    }
    let mut pid = 0u32;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if pid == 0 || pid == own_pid {
        return false;
    }
    let mut rect = RECT::default();
    if unsafe { GetWindowRect(hwnd, &mut rect) }.is_err() {
        return false;
    }
    point.x >= rect.left && point.x < rect.right && point.y >= rect.top && point.y < rect.bottom
}

/// UWP 的隐藏宿主窗口会盖满整块屏，不能算数。
fn is_cloaked(hwnd: HWND) -> bool {
    let mut cloaked = 0u32;
    let ok = unsafe {
        DwmGetWindowAttribute(
            hwnd,
            DWMWA_CLOAKED,
            &mut cloaked as *mut u32 as *mut _,
            std::mem::size_of::<u32>() as u32,
        )
    };
    ok.is_ok() && cloaked != 0
}

/// 弹一个选择框：`folder = true` 选目录，否则选 `.exe`。
///
/// 结果走 channel 回来，**不阻塞界面**：对话框在自起的 STA 线程里显示
/// （系统对话框要求单线程套间，主线程的 COM 是别处初始化的，不能保证）。
pub fn pick_path_async(folder: bool) -> async_channel::Receiver<Option<String>> {
    let (sender, receiver) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let kind = if folder { PickKind::Folder } else { PickKind::App };
        let path = unsafe { show_dialog_owned(kind, 0) };
        let _ = sender.send_blocking(path);
    });
    receiver
}

/// 选一个 `.ico`：托盘图标「自定义」用的。
///
/// 和 `pick_path_async` 是同一套（独立 STA 线程 + channel）。**不能**在 UI 线程里
/// 直接调 `GetOpenFileName` 那类模态框：它会在本线程跑消息循环，期间 Windows 把
/// 消息灌进 gpui 的 WndProc，而这时鼠标事件那一层的状态还借在手上 —— 一借就是
/// `BorrowMutError`，release 是 `panic = "abort"`，表现是「点一下程序直接没了」。
pub fn pick_icon_async(owner: isize) -> async_channel::Receiver<Option<String>> {
    pick_async(PickKind::Icon, owner)
}

/// 选一个音效文件（`.wav`）：音量音效「自定义」用的。
pub fn pick_sound_async(owner: isize) -> async_channel::Receiver<Option<String>> {
    pick_async(PickKind::Sound, owner)
}

fn pick_async(kind: PickKind, owner: isize) -> async_channel::Receiver<Option<String>> {
    let (sender, receiver) = async_channel::bounded(1);
    std::thread::spawn(move || {
        let path = unsafe { show_dialog_owned(kind, owner) };
        let _ = sender.send_blocking(path);
    });
    receiver
}

/// 选择框要挑什么（标题与过滤器都跟着它走）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum PickKind {
    /// 一个可执行文件（黑名单）。
    App,
    /// 一个目录（黑名单）。
    Folder,
    /// 一个 `.ico`（托盘图标）。
    Icon,
    /// 一个 `.wav`（音量音效）。
    Sound,
}

/// `owner` 是设置窗的 HWND（0 = 不认主）。给了主，对话框就是**模态**：
/// 一直是设置窗的前面那一层，也不会被再点一下「选择…」叠出第二个。
unsafe fn show_dialog_owned(kind: PickKind, owner: isize) -> Option<String> {
    use windows::Win32::System::Com::{
        CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
        CoTaskMemFree, CoUninitialize,
    };
    use windows::Win32::UI::Shell::{
        FOS_FILEMUSTEXIST, FOS_FORCEFILESYSTEM, FOS_PATHMUSTEXIST, FOS_PICKFOLDERS, FileOpenDialog,
        IFileOpenDialog, SIGDN_FILESYSPATH,
    };
    use windows::core::w;

    let initialized = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    let dialog: IFileOpenDialog =
        match unsafe { CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER) } {
            Ok(dialog) => dialog,
            Err(_) => {
                if initialized.is_ok() {
                    unsafe { CoUninitialize() };
                }
                return None;
            }
        };

    let mut options = match unsafe { dialog.GetOptions() } {
        Ok(options) => options,
        Err(_) => {
            if initialized.is_ok() {
                unsafe { CoUninitialize() };
            }
            return None;
        }
    };
    options |= FOS_FORCEFILESYSTEM | FOS_PATHMUSTEXIST;
    options = if kind == PickKind::Folder {
        options | FOS_PICKFOLDERS
    } else {
        options | FOS_FILEMUSTEXIST
    };
    let _ = unsafe { dialog.SetOptions(options) };
    // 手打路径或者挑错格式，最后都读不出一张图 / 一段音效，不如一开始就只让选对的。
    let filter = match kind {
        PickKind::Icon => Some(windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC {
            pszName: w!("图标文件 (*.ico)"),
            pszSpec: w!("*.ico"),
        }),
        PickKind::Sound => Some(windows::Win32::UI::Shell::Common::COMDLG_FILTERSPEC {
            pszName: w!("音效文件 (*.wav)"),
            pszSpec: w!("*.wav"),
        }),
        _ => None,
    };
    if let Some(filter) = filter {
        let spec = [filter];
        let _ = unsafe { dialog.SetFileTypes(&spec) };
    }
    let title = match kind {
        PickKind::Icon => w!("选择托盘图标"),
        PickKind::Sound => w!("选择音效文件"),
        PickKind::Folder => w!("Select the folder to disable Bolume in"),
        PickKind::App => w!("Select the app to disable Bolume in"),
    };
    let _ = unsafe { dialog.SetTitle(title) };

    // 用户取消会返回 HRESULT(0x800704C7)，这里一律当「没选」。
    let parent = (owner != 0).then(|| unsafe { HWND(owner as *mut _) });
    if unsafe { dialog.Show(parent) }.is_err() {
        if initialized.is_ok() {
            unsafe { CoUninitialize() };
        }
        return None;
    }
    let path = unsafe { dialog.GetResult() }
        .ok()
        .and_then(|item| unsafe { item.GetDisplayName(SIGDN_FILESYSPATH) }.ok())
        .and_then(|name| {
            let path = unsafe { name.to_string() }.ok();
            unsafe { CoTaskMemFree(Some(name.0 as *const _)) };
            path
        });
    if initialized.is_ok() {
        unsafe { CoUninitialize() };
    }
    path.filter(|path| !path.is_empty())
}
