//! 全局快捷键：照搬 EarTrumpet 的 `HotkeyManager`，再补上媒体键。
//!
//! `RegisterHotKey` 注册的热键消息只会送到「注册它的那个线程」的窗口过程里，
//! 所以这里单开一条线程放一个 message-only 窗口专收 `WM_HOTKEY`：界面线程
//! （gpui）不参与消息循环，wndproc 收到按键后通过 channel 把组合送回界面。
//!
//! 和 EarTrumpet 一致的两点：
//! - 同一个组合只注册一次，触发时由调用方按配置顺序匹配「第一个相等」的动作；
//! - 注册失败（被别人占了）静默跳过，不打断其余快捷键。
//!
//! 媒体键（播放/暂停、上一首、音量键等）`RegisterHotKey` 注册不上——它们
//! 由 shell 自己吃掉。这部分改用 `WH_KEYBOARD_LL` 低级键盘钩子：命中配置里的
//! 组合时把它吞掉并派发同一个事件；录制快捷键时也靠它把这类键收进来。

use std::{
    io::Write,
    sync::{
        Mutex, OnceLock,
        atomic::{AtomicBool, AtomicIsize, AtomicU64, Ordering},
    },
};

use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, WPARAM},
        System::LibraryLoader::GetModuleHandleW,
        UI::{
            Input::KeyboardAndMouse::{
                GetAsyncKeyState, HOT_KEY_MODIFIERS, RegisterHotKey, UnregisterHotKey, VIRTUAL_KEY,
                VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
            },
            WindowsAndMessaging::{
                CallNextHookEx, CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW,
                HWND_MESSAGE, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, LLKHF_INJECTED, MSG, PostMessageW,
                RegisterClassW, SetWindowsHookExW, TranslateMessage, WH_KEYBOARD_LL,
                WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP, WM_HOTKEY, WM_KEYDOWN, WM_KEYUP,
                WM_SYSKEYDOWN, WM_SYSKEYUP, WNDCLASSW,
            },
        },
    },
    core::w,
};

use crate::config::{AppBlockConfig, Config, Hotkey, HotkeySlots};

/// 修饰符位：与 `RegisterHotKey` 的 MOD_* 数值一致，直接存进配置。
pub const MOD_ALT: u32 = 0x0001;
pub const MOD_CONTROL: u32 = 0x0002;
pub const MOD_SHIFT: u32 = 0x0004;
pub const MOD_WIN: u32 = 0x0008;
/// `RegisterHotKey` 的「按住不重复」标志：不是组合的一部分，只影响注册行为。
///
/// 带上它，按住组合键时系统只发**一发** `WM_HOTKEY`，不会按键盘的重复延迟 / 速率
/// 一直补消息 —— 「按住连续触发」要靠 `start_hold_repeat` 自己的间隔来走，
/// 系统那一路必须关掉，否则两边叠在一起，节奏既不是我们的也不是系统的。
pub const MOD_NOREPEAT: u32 = 0x4000;

/// 每个动作一个固定的注册 id；同一个动作的第 n 组用 `id + n` 区分。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum Action {
    Flyout = 1,
    Settings = 2,
    VolumeUp = 3,
    VolumeDown = 4,
    /// 当前应用（正在发声的那个）音量加减。
    AppVolumeUp = 10,
    AppVolumeDown = 11,
    /// 下面五个是媒体键动作：触发时用 `SendInput` 把对应的 VK 再打一遍。
    MediaPrev = 5,
    MediaNext = 6,
    MediaPlayPause = 7,
    MediaMute = 8,
    MediaStop = 9,
    /// 屏幕亮度：当前（鼠标所在屏）/ 全部 / 指定编号。
    BrightnessUp = 12,
    BrightnessDown = 13,
    BrightnessAllUp = 14,
    BrightnessAllDown = 15,
    BrightnessMonitorUp = 16,
    BrightnessMonitorDown = 17,
}

/// 请求重新注册（自定义消息，发给热键线程的窗口）。
const WM_APPLY: u32 = WM_APP + 1;

/// 希望的注册表：(id, mods, vk)。
static DESIRED: OnceLock<Mutex<Vec<(u32, u32, u32)>>> = OnceLock::new();
/// 实际注册成功的 id，重新注册前先按它逐个注销。
static REGISTERED: OnceLock<Mutex<Vec<i32>>> = OnceLock::new();
/// 需要走低级钩子的组合：(mods, vk)。媒体键/浏览器键都在这里。
static HOOK_BINDINGS: OnceLock<Mutex<Vec<(u32, u32)>>> = OnceLock::new();
/// 鼠标组合：(mods, 伪键码)。鼠标钩子（`mouse_wheel.rs`）按这张表拦。
static MOUSE_BINDINGS: OnceLock<Mutex<Vec<(u32, u32)>>> = OnceLock::new();
/// 小键盘组合：(mods, VK_NUMPAD*)。NumLock 关着时钩子按扫描码认回这些键再对表。
static NUMPAD_BINDINGS: OnceLock<Mutex<Vec<(u32, u32)>>> = OnceLock::new();
/// 热键线程的窗口句柄（0 = 还没建好）。
static SINK: AtomicIsize = AtomicIsize::new(0);
/// 把按下的组合送回界面线程。
static SENDER: OnceLock<async_channel::Sender<Hotkey>> = OnceLock::new();
/// 正在录制快捷键：钩子把媒体键也送回来。
static CAPTURING: AtomicBool = AtomicBool::new(false);
/// 低级键盘钩子句柄（0 = 没装上）。进程退出前一直有效。
static HOOK: AtomicIsize = AtomicIsize::new(0);
/// 「按住连续触发」开着，而且用户没勾「理会系统的长按重复」：系统补出来的重复
/// 一律不理会，重复节奏完全由我们自己掌握。
static IGNORE_SYSTEM_REPEAT: AtomicBool = AtomicBool::new(false);
/// 现在按着的键（VK 0..=255 的位图）。keyup 也走低级钩子，靠它分辨
/// 「第一次按下」和「系统自动重复补出来的 keydown」。
static DOWN_KEYS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

fn desired() -> &'static Mutex<Vec<(u32, u32, u32)>> {
    DESIRED.get_or_init(|| Mutex::new(Vec::new()))
}

fn registered() -> &'static Mutex<Vec<i32>> {
    REGISTERED.get_or_init(|| Mutex::new(Vec::new()))
}

fn hook_bindings() -> &'static Mutex<Vec<(u32, u32)>> {
    HOOK_BINDINGS.get_or_init(|| Mutex::new(Vec::new()))
}

fn mouse_bindings() -> &'static Mutex<Vec<(u32, u32)>> {
    MOUSE_BINDINGS.get_or_init(|| Mutex::new(Vec::new()))
}

fn numpad_bindings() -> &'static Mutex<Vec<(u32, u32)>> {
    NUMPAD_BINDINGS.get_or_init(|| Mutex::new(Vec::new()))
}

/// 这套 (修饰键, 鼠标键) 是不是用户配过的组合。
pub fn mouse_binding_matches(mods: u32, key: u32) -> bool {
    mouse_bindings()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .iter()
        .any(|binding| *binding == (mods, key))
}

/// 鼠标钩子录到 / 命中一发组合：送回界面线程走同一套分发。
pub fn send_mouse_hotkey(mods: u32, key: u32) {
    send_hotkey(Hotkey::new(mods, key));
}

/// 前台是不是命中黑名单（鼠标钩子也要问一句：让路时别把滚轮吞掉）。
pub fn is_blocked_now() -> bool {
    list_blocked()
}

/// 调试日志写到哪：环境变量 `BOLUME_TRACE` 指的路径；没有就看程序目录里
/// 有没有 `trace.on` 这个哨兵文件，有就写同目录的 `trace.log`。
///
/// 哨兵是为「从别的程序里启动的实例」准备的（比如从 Directory Opus 里起）：
/// 那种实例继承的是**启动它的那个程序**的环境块，改用户环境变量对它没用，
/// 只有文件开关能生效。结果缓存一次 —— trace 在接管循环里是热路径。
fn trace_path() -> Option<&'static std::path::Path> {
    use std::sync::OnceLock;
    static PATH: OnceLock<Option<std::path::PathBuf>> = OnceLock::new();
    PATH.get_or_init(|| {
        if let Some(path) = std::env::var_os("BOLUME_TRACE") {
            return Some(std::path::PathBuf::from(path));
        }
        let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
        dir.join("trace.on")
            .exists()
            .then(|| dir.join("trace.log"))
    })
    .as_deref()
}

/// 调试用：把热键链路的关键节点追加写进调试日志（见 [`trace_path`]）。
///
/// 媒体键走低级键盘钩子，出问题时「钩子没装上」和「键盘发的不是这个键码」
/// 在界面上的表现完全一样（按了没反应），只能靠这条日志分开。
/// 没开调试时整个函数几乎零成本（一次查缓存）。
pub fn trace(text: &str) {
    let Some(path) = trace_path() else {
        return;
    };
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_secs_f64())
        .unwrap_or(0.0);
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
    {
        let _ = writeln!(file, "{stamp:.3} {text}");
    }
}

impl Action {
    fn all() -> [Action; 17] {
        [
            Action::Flyout,
            Action::Settings,
            Action::VolumeUp,
            Action::VolumeDown,
            Action::AppVolumeUp,
            Action::AppVolumeDown,
            Action::MediaPrev,
            Action::MediaNext,
            Action::MediaPlayPause,
            Action::MediaMute,
            Action::MediaStop,
            Action::BrightnessUp,
            Action::BrightnessDown,
            Action::BrightnessAllUp,
            Action::BrightnessAllDown,
            Action::BrightnessMonitorUp,
            Action::BrightnessMonitorDown,
        ]
    }

    /// 「按住连发」只管一档档往下走的动作：音量、亮度、切歌。
    ///
    /// 静音和播放暂停是**开关**，按住连点只会来回翻（静音图标狂闪、播放暂停
    /// 反复切），按一下是一下；面板和设置同理。
    pub fn repeats_while_held(self) -> bool {
        matches!(
            self,
            Action::VolumeUp
                | Action::VolumeDown
                | Action::BrightnessUp
                | Action::BrightnessDown
                | Action::BrightnessAllUp
                | Action::BrightnessAllDown
                | Action::BrightnessMonitorUp
                | Action::BrightnessMonitorDown
                | Action::MediaPrev
                | Action::MediaNext
                | Action::MediaStop
        )
    }

    fn slots(self, config: &Config) -> HotkeySlots {
        match self {
            Action::Flyout => config.hotkey_flyout,
            Action::Settings => config.hotkey_settings,
            Action::VolumeUp => config.hotkey_volume_up,
            Action::VolumeDown => config.hotkey_volume_down,
            Action::AppVolumeUp => config.hotkey_app_volume_up,
            Action::AppVolumeDown => config.hotkey_app_volume_down,
            Action::MediaPrev => config.hotkey_media_prev,
            Action::MediaNext => config.hotkey_media_next,
            Action::MediaPlayPause => config.hotkey_media_play_pause,
            Action::MediaMute => config.hotkey_media_mute,
            Action::MediaStop => config.hotkey_media_stop,
            Action::BrightnessUp => config.hotkey_brightness_up,
            Action::BrightnessDown => config.hotkey_brightness_down,
            Action::BrightnessAllUp => config.hotkey_brightness_all_up,
            Action::BrightnessAllDown => config.hotkey_brightness_all_down,
            Action::BrightnessMonitorUp => config.hotkey_brightness_monitor_up,
            Action::BrightnessMonitorDown => config.hotkey_brightness_monitor_down,
        }
    }

    /// 媒体键动作对应的虚拟键码。
    pub fn media_key(self) -> Option<u32> {
        match self {
            Action::MediaPrev => Some(VK_MEDIA_PREV_TRACK),
            Action::MediaNext => Some(VK_MEDIA_NEXT_TRACK),
            Action::MediaPlayPause => Some(VK_MEDIA_PLAY_PAUSE),
            Action::MediaMute => Some(VK_VOLUME_MUTE),
            Action::MediaStop => Some(VK_MEDIA_STOP),
            _ => None,
        }
    }

    /// 第 n 组快捷键的注册 id。
    fn id(self, slot: usize) -> u32 {
        self as u32 * 8 + slot as u32
    }
}

/// 鼠标键在配置里的伪键码。
///
/// 真实 VK 只到 0xFF，`RegisterHotKey` 也只认这一截，所以鼠标事件另开一段：
/// 组合里存的是这些数字，注册时统统跳过、交给鼠标钩子（`mouse_wheel.rs`）去接。
/// 老配置里不可能出现这些值，读进来不会撞车。
pub const VK_MOUSE_WHEEL_UP: u32 = 0x100;
pub const VK_MOUSE_WHEEL_DOWN: u32 = 0x101;
pub const VK_MOUSE_WHEEL_CLICK: u32 = 0x102;
pub const VK_MOUSE_WHEEL_LEFT: u32 = 0x103;
pub const VK_MOUSE_WHEEL_RIGHT: u32 = 0x104;
pub const VK_MOUSE_SIDE1: u32 = 0x105;
pub const VK_MOUSE_SIDE2: u32 = 0x106;

/// 这一格是不是「鼠标键」。
pub fn is_mouse_key(key: u32) -> bool {
    (VK_MOUSE_WHEEL_UP..=VK_MOUSE_SIDE2).contains(&key)
}

/// 媒体键虚拟键码（`windows` crate 里没导出这几个常量，照 winuser.h 写死）。
pub const VK_MEDIA_NEXT_TRACK: u32 = 0xB0;
pub const VK_MEDIA_PREV_TRACK: u32 = 0xB1;
pub const VK_MEDIA_STOP: u32 = 0xB2;
pub const VK_MEDIA_PLAY_PAUSE: u32 = 0xB3;
pub const VK_VOLUME_MUTE: u32 = 0xAD;
pub const VK_VOLUME_DOWN: u32 = 0xAE;
pub const VK_VOLUME_UP: u32 = 0xAF;

/// 我们自己注入的事件带的标记（写进 `KEYBDINPUT::dwExtraInfo`）。
///
/// 低级钩子里看不出「是谁注入的」，只能靠这个标记认出自己打出去的那一发，
/// 用来防自吞自。**其余注入事件必须当成真实按键处理**：不少键盘的音量键
/// 是厂商驱动用 `SendInput` 原样重打一遍的，一律跳过的话这些键在 Bolume
/// 里永远不生效（表现为按了只弹系统 OSD、面板和托盘都没反应）。
const SELF_INJECT_TAG: usize = 0x0042_4F4C_55; // "BOLU"

/// 把媒体键按键注入系统（钩子靠 `SELF_INJECT_TAG` 认出这一发并放行）。
pub fn send_media_key(key: u32) -> bool {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        INPUT, INPUT_0, INPUT_KEYBOARD, KEYBDINPUT, KEYEVENTF_KEYUP, SendInput, VIRTUAL_KEY,
    };

    let key = VIRTUAL_KEY(key as u16);
    let events = [
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: key,
                    dwExtraInfo: SELF_INJECT_TAG,
                    ..Default::default()
                },
            },
        },
        INPUT {
            r#type: INPUT_KEYBOARD,
            Anonymous: INPUT_0 {
                ki: KEYBDINPUT {
                    wVk: key,
                    dwFlags: KEYEVENTF_KEYUP,
                    dwExtraInfo: SELF_INJECT_TAG,
                    ..Default::default()
                },
            },
        },
    ];
    let sent = unsafe { SendInput(&events, std::mem::size_of::<INPUT>() as i32) };
    sent == events.len() as u32
}

/// 这次按下的是不是一个「裸的」系统媒体 / 音量键（没带任何修饰）。
///
/// 让路状态下只把这类键还给系统：它们是键盘上自带的，本来就该由播放器 / 系统收。
/// 用户自己配的组合键不算 —— 那是专门配给 Bolume 的，让路时什么都不做才对。
pub fn is_raw_system_key(key: u32, mods: u32) -> bool {
    if mods != 0 {
        return false;
    }
    matches!(key, VK_VOLUME_MUTE | VK_VOLUME_DOWN | VK_VOLUME_UP)
        || (VK_MEDIA_NEXT_TRACK..=0xB7).contains(&key)
}

/// 把这一发按键原样还给系统（让路状态下用）。
///
/// 钩子先把键吞了，主线程才发现前台命中黑名单 —— 这时候不能就让它丢了：
/// 媒体键 / 音量键要重新注入一遍，等于 Bolume 从没碰过它。
/// 返回是否真的转发过（不是媒体 / 音量类的动作就不用管，那些本来就该被拦下）。
pub fn forward_to_system(action: Action) -> bool {
    let key = match action {
        Action::VolumeUp => Some(VK_VOLUME_UP),
        Action::VolumeDown => Some(VK_VOLUME_DOWN),
        other => other.media_key(),
    };
    key.map(send_media_key).unwrap_or(false)
}

/// 起热键线程，返回「按下了哪个组合」的事件流。
pub fn spawn() -> async_channel::Receiver<Hotkey> {
    let (sender, receiver) = async_channel::unbounded();
    let _ = SENDER.set(sender);
    let _ = std::thread::Builder::new()
        .name("bolume-hotkey".into())
        .spawn(|| unsafe {
            if let Err(error) = run_message_loop() {
                eprintln!("热键线程启动失败：{error}");
            }
        });
    receiver
}

unsafe fn run_message_loop() -> windows::core::Result<()> {
    let instance = unsafe { GetModuleHandleW(None) }?;
    let class_name = w!("BolumeHotkeySink");
    let class = WNDCLASSW {
        lpfnWndProc: Some(window_proc),
        hInstance: instance.into(),
        lpszClassName: class_name,
        ..Default::default()
    };
    // 类只注册一次；重复注册返回 0 不影响后续建窗。
    let _ = unsafe { RegisterClassW(&class) };

    let hwnd = unsafe {
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            class_name,
            w!("BolumeHotkeySink"),
            WINDOW_STYLE::default(),
            0,
            0,
            0,
            0,
            Some(HWND_MESSAGE),
            None,
            Some(instance.into()),
            None,
        )?
    };
    SINK.store(hwnd.0 as isize, Ordering::SeqCst);

    // 低级键盘钩子：媒体键只能这样收。装在本线程，随消息循环派发。
    match unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), None, 0) } {
        Ok(hook) => {
            HOOK.store(hook.0 as isize, Ordering::SeqCst);
            trace(&format!("hook installed handle=0x{:X}", hook.0 as isize));
        }
        Err(error) => {
            trace(&format!("hook install FAILED {error:?}"));
            eprintln!("媒体键钩子安装失败，媒体键快捷键不可用：{error}");
        }
    }

    // 窗口建好之前调用方可能已经写过期望表，先应用一次。
    unsafe { apply_desired(hwnd) };

    let mut message = MSG::default();
    while unsafe { GetMessageW(&mut message, None, 0, 0) }.0 > 0 {
        unsafe {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
    Ok(())
}

unsafe extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match message {
        WM_HOTKEY => {
            // lParam 低字是注册时的修饰符，高字是虚拟键码。
            // `MOD_NOREPEAT` 只是注册行为上的标志，不是组合的一部分；万一系统把它
            // 也塞进 lParam，不剥离的话这里拼出来的组合跟配置里那条对不上。
            let bits = lparam.0 as u32;
            let hotkey = Hotkey::new((bits & 0xFFFF) & !MOD_NOREPEAT, (bits >> 16) & 0xFFFF);
            trace(&format!(
                "wm_hotkey mods={} key={}",
                hotkey.mods, hotkey.key
            ));
            send_hotkey(hotkey);
            let _ = wparam;
            LRESULT(0)
        }
        WM_APPLY => {
            unsafe { apply_desired(hwnd) };
            LRESULT(0)
        }
        _ => unsafe { DefWindowProcW(hwnd, message, wparam, lparam) },
    }
}

fn send_hotkey(hotkey: Hotkey) {
    if let Some(sender) = SENDER.get() {
        let _ = sender.try_send(hotkey);
    }
}

/// 低级键盘钩子：只关心媒体键/浏览器键这类 `RegisterHotKey` 收不到的键。
unsafe extern "system" fn keyboard_proc(code: i32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if code < 0 {
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }
    let message = wparam.0 as u32;
    let is_down = message == WM_KEYDOWN as u32 || message == WM_SYSKEYDOWN as u32;
    let is_up = message == WM_KEYUP as u32 || message == WM_SYSKEYUP as u32;
    if !is_down && !is_up {
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }
    let info = unsafe { &*(lparam.0 as *const KBDLLHOOKSTRUCT) };
    let injected = info.flags.0 & LLKHF_INJECTED.0 != 0;
    // 只放行**自己**注入的那一发（`send_media_key` 打的），否则会自己吞自己、
    // 无限循环。别人的注入事件（键盘厂商驱动、按键映射工具、宏）要照常处理。
    //
    // 而且在「按下表」里也不留痕：按住媒体键时我们本来就一边转发同一个键
    // （按住「停止」→ 每档注入一次「停止」），自己那发 keyup 要是清了位，
    // 连发循环当场以为用户松手了，连发只走一档就停 —— 必须整条跳过。
    if injected && info.dwExtraInfo == SELF_INJECT_TAG {
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }
    if is_up {
        // 松开也走这个钩子：把「按下表」里那一位清掉，下一发才算新的一次按下。
        forget_key_down(info.vkCode);
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }
    // 这一发按下来之前，这颗键是不是还按着 —— 是的话就是系统「长按重复」补出来的
    // keydown（键盘按住不放时，系统按自己的重复延迟 / 速率一直补）。
    //
    // 这里只信我们自己这张表，**不能**再问 `GetAsyncKeyState`：被钩子吞掉的
    // keydown 根本不会进系统的按键状态（吞掉就是没发生过），问出来的永远是「没按着」，
    // 那样判一次错一次，等于没做。表是 keyup 清的，而 keyup 也走这个钩子。
    let already_down = mark_key_down(info.vkCode);
    let system_repeat = already_down && ignore_system_repeat();
    // 「NumLock 关着也认小键盘数字」：系统这时候发的是方向 / Ins / Del，
    // 用扫描码把它们认回 Num0..9 再对一次表。没配成快捷键就照常放过去 ——
    // 别的程序里这颗键还是方向键，我们的开关只影响自己认不认它。
    // 不看 NumLock：扫描码两种状态下一样，NumLock 开着时也会命中同一条路
    // （吞掉之后 `RegisterHotKey` 那一路自然不会再响，不会重复触发）。
    if numpad_raw() {
        if let Some(numpad_key) = numpad_vk_from_scan(info) {
            let mods = current_modifiers();
            if CAPTURING.load(Ordering::SeqCst) {
                // 录制中：按小键盘的键码记下来，NumLock 开不开录的都是同一格。
                if system_repeat {
                    return LRESULT(1);
                }
                send_hotkey(Hotkey::new(mods, numpad_key));
                return LRESULT(1);
            }
            if !list_blocked() && numpad_binding_matches(mods, numpad_key) {
                trace(&format!("hook numpad vk=0x{numpad_key:02X} mods={mods}"));
                if system_repeat {
                    return LRESULT(1);
                }
                send_hotkey(Hotkey::new(mods, numpad_key));
                return LRESULT(1);
            }
        }
    }

    let key = info.vkCode;
    if !is_hook_key(key) {
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }
    // 到这里都是「我们认识的键」：重复的那几发一律吞掉不派发（也不放给系统，
    // 否则系统那条音量条 / 播放器照样按自己的节奏动，等于没忽略）。
    // 重复由 `start_hold_repeat` 按设置里的间隔自己走。

    let mods = current_modifiers();
    let hotkey = Hotkey::new(mods, key);

    if CAPTURING.load(Ordering::SeqCst) {
        // 录制中：先送回去让设置页落子，再把这次按键吞掉。
        // 按住不放补出来的那几发不送 —— 录制只认第一次按下。
        if system_repeat {
            return LRESULT(1);
        }
        send_hotkey(hotkey);
        return LRESULT(1);
    }

    // 「禁用 Windows 音量条」：音量键不管带不带修饰都吞掉，交回主线程按普通音量键处理。
    //
    // 放在黑名单判断**前面**：黑名单是「让路给游戏」，让路就是把这个键还给系统，
    // 而系统一收到音量键就会弹它那条音量条 —— 那正是这个开关要消灭的东西。
    // 修饰键在这里也是刻意忽略的：系统那条条不认修饰键，`Ctrl+音量+` 放过去一样弹。
    // 只在主程序活着的时候有效：没有守卫进程，退出 Bolume 系统就恢复原样。
    if block_system_osd() && volume_action(key).is_some() {
        if system_repeat {
            return LRESULT(1);
        }
        send_hotkey(Hotkey::new(0, key));
        return LRESULT(1);
    }

    // 前台命中黑名单：一个键都不留，原样还给系统 / 游戏。
    // 只看名单（不查全屏）：壳层的全屏查询在低级钩子里不敢调，慢一下按键就丢了；
    // 全屏那一路由主线程兜（`forward_to_system`）。
    if list_blocked() {
        trace(&format!("hook pass-through vk=0x{key:02X} (app blocked)"));
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }

    let bound = hook_bindings()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .iter()
        .any(|binding| *binding == (mods, key));
    trace(&format!(
        "hook vk=0x{key:02X} mods={mods} bound={bound} injected={injected} capturing={}",
        CAPTURING.load(Ordering::SeqCst)
    ));
    if bound {
        if system_repeat {
            return LRESULT(1);
        }
        send_hotkey(hotkey);
        return LRESULT(1);
    }
    // 没配这个键，但开着「接管媒体键」：一样送给主线程，
    // 由它弹 OSD 再把键转发给播放器。带修饰键的不算（那是用户的组合键）。
    if mods == 0 && takeover_media() && media_action(key).is_some() {
        if system_repeat {
            return LRESULT(1);
        }
        send_hotkey(hotkey);
        return LRESULT(1);
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// 这个键现在还按着没有。
///
/// **只对没被我们吞掉的键可靠**：keydown 被钩子吞掉之后，那一发根本没进系统的
/// 输入队列，`GetAsyncKeyState` 对它永远报「没按着」。要问「按住的那颗键还按着吗」
/// 用 `is_key_held`。
pub fn is_key_down(key: u32) -> bool {
    unsafe { GetAsyncKeyState(key as i32) as u16 & 0x8000 != 0 }
}

/// 「按住连发」判断键还按着没有 —— 接管过的键和没接管的要分开问。
///
/// 媒体 / 音量键都走低级钩子，keydown 被吞掉之后系统那边当它没发生过，
/// `GetAsyncKeyState` 一律返回「没按着」，连发循环第一拍就以为松手退场了
/// （表现就是「按住音量键一点反应都没有」）。这几颗只能看钩子自己维护的按下表：
/// 表在 keydown 里置位、keyup 清位，而 keyup **永远**会送到钩子（吞掉的是 keydown）。
pub fn is_key_held(key: u32) -> bool {
    if is_hook_key(key) {
        return tracked_down(key);
    }
    is_key_down(key)
}

/// 按下表里那一位还挂着没有。
fn tracked_down(key: u32) -> bool {
    if key > 255 {
        return false;
    }
    DOWN_KEYS[(key / 64) as usize].load(Ordering::SeqCst) & (1u64 << (key % 64)) != 0
}

/// 开 / 关「忽略系统的长按重复」（配置变更时由 `apply_config` 同步）。
///
/// 只影响我们**自己接管**的键：`RegisterHotKey` 那一路靠 `MOD_NOREPEAT`（要重新注册，
/// 所以这个函数之后还得走一次 `apply`），低级钩子那一路看的就是这个开关。
pub fn set_ignore_system_repeat(on: bool) {
    IGNORE_SYSTEM_REPEAT.store(on, Ordering::SeqCst);
}

fn ignore_system_repeat() -> bool {
    IGNORE_SYSTEM_REPEAT.load(Ordering::SeqCst)
}

/// 记下「这颗键现在按着」，返回它之前是不是就已经按着了。
///
/// 之前就按着 = 这一发 keydown 是系统按住重复补出来的。真实 VK 只到 0xFF，
/// 四段 u64 一位一个键，够用而且不用加锁（钩子里不能慢慢来）。
fn mark_key_down(key: u32) -> bool {
    if key > 255 {
        return false;
    }
    let word = &DOWN_KEYS[(key / 64) as usize];
    let bit = 1u64 << (key % 64);
    word.fetch_or(bit, Ordering::SeqCst) & bit != 0
}

/// 松开：把这一位清掉。
fn forget_key_down(key: u32) {
    if key > 255 {
        return;
    }
    DOWN_KEYS[(key / 64) as usize].fetch_and(!(1u64 << (key % 64)), Ordering::SeqCst);
}

/// 当前按住的修饰键（钩子里拿不到 gpui 的修饰符状态，直接问系统）。
pub fn current_modifiers() -> u32 {
    let pressed = |key: VIRTUAL_KEY| unsafe { GetAsyncKeyState(key.0 as i32) } as u16 & 0x8000 != 0;
    let mut mods = 0;
    if pressed(VK_CONTROL) {
        mods |= MOD_CONTROL;
    }
    if pressed(VK_MENU) {
        mods |= MOD_ALT;
    }
    if pressed(VK_SHIFT) {
        mods |= MOD_SHIFT;
    }
    if pressed(VK_LWIN) || pressed(VK_RWIN) {
        mods |= MOD_WIN;
    }
    mods
}

/// 钩子负责的键：浏览器键 0xA6..0xAC、音量键 0xAD..0xAF、媒体键 0xB0..0xB7。
pub fn is_hook_key(key: u32) -> bool {
    (0xA6..=0xB7).contains(&key)
}

/// 「接管媒体键」：没配快捷键的系统媒体键也由我们处理。
///
/// 典型场景：用户没给「下一曲」配快捷键，按键盘上的媒体键时系统自己处理，
/// Bolume 根本看不到，于是不弹 OSD。开了这个开关就把这四个键也拦下来，
/// 弹一条提示后再原样转发给播放器。
static TAKEOVER_MEDIA: AtomicBool = AtomicBool::new(false);

/// 开关「接管媒体键」（配置变更时由 `apply_config` 同步）。
pub fn set_takeover_media(on: bool) {
    TAKEOVER_MEDIA.store(on, Ordering::SeqCst);
}

fn takeover_media() -> bool {
    TAKEOVER_MEDIA.load(Ordering::SeqCst)
}

/// 「禁用 Windows 音量条」：音量键连修饰键一起吞，绝不让资源管理器看到。
///
/// 系统那条音量条**不认修饰键** —— `Ctrl+音量+` 在它眼里跟裸键一模一样，
/// 所以只吞裸键是没用的（带修饰的组合键也正是这么漏过去的）。
/// 只在 Bolume 运行期间有效（钩子在主程序进程里）。
static BLOCK_SYSTEM_OSD: AtomicBool = AtomicBool::new(false);

/// 开关「禁用 Windows 音量条」（配置变更时由 `apply_config` 同步）。
pub fn set_block_system_osd(on: bool) {
    BLOCK_SYSTEM_OSD.store(on, Ordering::SeqCst);
}

fn block_system_osd() -> bool {
    BLOCK_SYSTEM_OSD.load(Ordering::SeqCst)
}

/// 「NumLock 关着也认小键盘数字」：默认关，跟 Windows 原生行为一致。
static NUMPAD_RAW: AtomicBool = AtomicBool::new(false);

/// 开关「NumLock 关着也认小键盘数字」（配置变更时由 `apply_config` 同步）。
pub fn set_numpad_raw(on: bool) {
    NUMPAD_RAW.store(on, Ordering::SeqCst);
}

fn numpad_raw() -> bool {
    NUMPAD_RAW.load(Ordering::SeqCst)
}

/// 这一发按键是不是小键盘，是的话返回它对应的数字键码。
///
/// NumLock 关着的时候系统把 `Num4` 翻译成 `VK_LEFT`，光看 `vkCode` 分不出来；
/// 但**扫描码不变** —— 真正的方向 / Ins / Del / Home 那一组都带扩展位（E0 前缀），
/// 小键盘不带。所以「扫描码在 0x47..=0x53 且没有扩展位」就是小键盘，
/// 而且跟 NumLock 开不开无关，不用再去读 NumLock 状态（那个状态在钩子线程里
/// 读到的是旧值，`GetAsyncKeyState` 的低位也不稳）。
///
/// NumLock 开着的那些键（`Num*` / `Num+` / `Num-` / `Num/`）本来就不受开关影响，
/// 照旧走 `RegisterHotKey`，这里不接。
fn numpad_vk_from_scan(info: &KBDLLHOOKSTRUCT) -> Option<u32> {
    if info.flags.0 & LLKHF_EXTENDED.0 != 0 {
        return None;
    }
    Some(match info.scanCode {
        0x47 => 0x67, // Num7
        0x48 => 0x68, // Num8
        0x49 => 0x69, // Num9
        0x4B => 0x64, // Num4
        0x4C => 0x65, // Num5
        0x4D => 0x66, // Num6
        0x4F => 0x61, // Num1
        0x50 => 0x62, // Num2
        0x51 => 0x63, // Num3
        0x52 => 0x60, // Num0
        0x53 => 0x6E, // Num.（NumLock 关着时是 Del）
        _ => return None,
    })
}

/// 这套 (修饰键, 小键盘键) 用户配过没有。
fn numpad_binding_matches(mods: u32, key: u32) -> bool {
    numpad_bindings()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .iter()
        .any(|binding| *binding == (mods, key))
}

/// 音量 / 静音键对应的动作。
pub fn volume_action(key: u32) -> Option<Action> {
    match key {
        VK_VOLUME_UP => Some(Action::VolumeUp),
        VK_VOLUME_DOWN => Some(Action::VolumeDown),
        VK_VOLUME_MUTE => Some(Action::MediaMute),
        _ => None,
    }
}

/// 黑名单快照：钩子线程里没法摸 gpui 的全局状态，配置变更时抄一份过来。
static BLOCK_LIST: Mutex<AppBlockConfig> = Mutex::new(AppBlockConfig {
    fullscreen: false,
    fullscreen_exempt: Vec::new(),
    entries: Vec::new(),
});

/// 同步黑名单（`apply_config` 里调）。
pub fn set_blocklist(block: &AppBlockConfig) {
    if let Ok(mut list) = BLOCK_LIST.lock() {
        *list = block.clone();
    }
}

/// 前台进程（含祖先）命中黑名单没有。
fn list_blocked() -> bool {
    let block = match BLOCK_LIST.lock() {
        Ok(list) => list.clone(),
        Err(error) => error.into_inner().clone(),
    };
    if block.entries.is_empty() {
        return false;
    }
    crate::platform::fullscreen::foreground_process_chain()
        .iter()
        .any(|exe| block.blocks_exe(exe))
}

/// 媒体播放键 → 对应动作。只有这四个会被「接管」；
/// 音量 / 静音键不碰（系统自己会弹它的音量条，接管了会叠两层）。
pub fn media_action(key: u32) -> Option<Action> {
    match key {
        VK_MEDIA_PREV_TRACK => Some(Action::MediaPrev),
        VK_MEDIA_NEXT_TRACK => Some(Action::MediaNext),
        VK_MEDIA_PLAY_PAUSE => Some(Action::MediaPlayPause),
        VK_MEDIA_STOP => Some(Action::MediaStop),
        _ => None,
    }
}

/// 录制快捷键期间开关钩子的接管（只影响媒体键那一小撮）。
/// 现在是不是在录快捷键（鼠标钩子用它决定拦不拦）。
pub fn is_capturing() -> bool {
    CAPTURING.load(Ordering::SeqCst)
}

pub fn set_capturing(capturing: bool) {
    CAPTURING.store(capturing, Ordering::SeqCst);
}

/// 按 DESIRED 重新注册全部热键：先全撤，再逐个注册。
unsafe fn apply_desired(hwnd: HWND) {
    let mut registered = registered()
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    for id in registered.drain(..) {
        unsafe {
            let _ = UnregisterHotKey(Some(hwnd), id);
        }
    }

    let bindings = desired()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .clone();
    let mut seen: Vec<(u32, u32)> = Vec::new();
    let mut hooked: Vec<(u32, u32)> = Vec::new();
    let mut moused: Vec<(u32, u32)> = Vec::new();
    let mut numpad: Vec<(u32, u32)> = Vec::new();
    for (id, mods, key) in bindings {
        if key == 0 || seen.contains(&(mods, key)) {
            continue;
        }
        seen.push((mods, key));
        // 小键盘的 `RegisterHotKey` 只在 NumLock 开着时响；关着那一半交给钩子，
        // 所以这张表要一直留着（开关按下去不用重新注册）。
        if (0x60..=0x6F).contains(&key) {
            numpad.push((mods, key));
        }
        // 鼠标键不进 RegisterHotKey（伪键码注册不上），单独交给鼠标钩子。
        if is_mouse_key(key) {
            moused.push((mods, key));
            continue;
        }
        if is_hook_key(key) {
            // 媒体键交给低级钩子，不走 RegisterHotKey（那类键注册不上，
            // 万一注册上了还会和钩子重复触发）。
            hooked.push((mods, key));
            continue;
        }
        // 「忽略系统的长按重复」：按住组合键时只要第一发，后面的重复交给我们
        // 自己的 `start_hold_repeat`。不带这个标志时系统会按键盘的重复延迟 /
        // 速率一直补 `WM_HOTKEY`，两套节奏叠在一起。
        let api_mods = if ignore_system_repeat() {
            mods | MOD_NOREPEAT
        } else {
            mods
        };
        if unsafe { RegisterHotKey(Some(hwnd), id as i32, HOT_KEY_MODIFIERS(api_mods), key) }.is_ok()
        {
            registered.push(id as i32);
        }
    }
    let registered_count = registered.len();
    drop(registered);

    trace(&format!(
        "apply registered={registered_count} hooked={hooked:?}"
    ));
    *hook_bindings()
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = hooked;
    *mouse_bindings()
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = moused;
    *numpad_bindings()
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = numpad;
}

/// 重新注册整张热键表。`bindings` 里空的热键会被跳过。
///
/// 注册 id 必须**每个槽位都不同**：`RegisterHotKey` 对同一个 id 只认第一次，
/// 后面换了键再注册会直接失败，那样一个动作就只能生效第一组。
pub fn apply(bindings: &[(Action, usize, Hotkey)]) {
    {
        let mut list = desired().lock().unwrap_or_else(|error| error.into_inner());
        list.clear();
        for (action, slot, hotkey) in bindings {
            if hotkey.is_empty() {
                continue;
            }
            list.push((action.id(*slot), hotkey.mods, hotkey.key));
        }
    }
    let sink = SINK.load(Ordering::SeqCst);
    if sink != 0 {
        unsafe {
            let _ = PostMessageW(Some(HWND(sink as *mut _)), WM_APPLY, WPARAM(0), LPARAM(0));
        }
    }
}

/// 按当前配置注册全部快捷键（每个动作最多三组）。
pub fn apply_config(config: &Config) {
    set_takeover_media(config.osd.takeover_media_keys);
    set_block_system_osd(config.osd.block_system_osd);
    set_numpad_raw(config.hotkey_numpad_raw);
    // 只有「按住连续触发」开着的时候才谈得上「忽略系统的长按重复」：关掉连发时
    // 我们自己是不会补的，再把系统那一路也关掉，按住音量键就彻底不动了。
    set_ignore_system_repeat(config.hotkey_hold_repeat && config.hotkey_ignore_system_repeat);
    set_blocklist(&config.app_block);
    let mut bindings: Vec<(Action, usize, Hotkey)> = Vec::new();
    for action in Action::all() {
        for (index, hotkey) in action.slots(config).all().enumerate() {
            bindings.push((action, index, hotkey));
        }
    }
    apply(&bindings);
}

/// 配置里与某个组合相等的动作（顺序与 `apply_config` 一致，第一个命中即返回）。
pub fn action_for(config: &Config, hotkey: Hotkey) -> Option<Action> {
    if hotkey.is_empty() {
        return None;
    }
    for action in Action::all() {
        if action.slots(config).all().any(|slot| slot == hotkey) {
            return Some(action);
        }
    }
    None
}

/// 界面上显示的文字（`Ctrl+Shift+A`）。未设置返回 `None`。
pub fn display_text(hotkey: Hotkey) -> Option<String> {
    if hotkey.is_empty() {
        return None;
    }
    let mut parts: Vec<String> = Vec::new();
    if hotkey.mods & MOD_CONTROL != 0 {
        parts.push("Ctrl".into());
    }
    if hotkey.mods & MOD_ALT != 0 {
        parts.push("Alt".into());
    }
    if hotkey.mods & MOD_SHIFT != 0 {
        parts.push("Shift".into());
    }
    if hotkey.mods & MOD_WIN != 0 {
        parts.push("Win".into());
    }
    parts.push(key_text(hotkey.key)?);
    Some(parts.join("+"))
}

/// 鼠标键的显示名（伪键码 0x100 起，装不进 `u8`，要在换算前拦掉）。
fn mouse_key_text(key: u32) -> Option<String> {
    Some(
        match key {
            VK_MOUSE_WHEEL_UP => "Wheel Up",
            VK_MOUSE_WHEEL_DOWN => "Wheel Down",
            VK_MOUSE_WHEEL_CLICK => "Wheel Click",
            VK_MOUSE_WHEEL_LEFT => "Wheel Left",
            VK_MOUSE_WHEEL_RIGHT => "Wheel Right",
            VK_MOUSE_SIDE1 => "Mouse 4",
            VK_MOUSE_SIDE2 => "Mouse 5",
            _ => return None,
        }
        .to_string(),
    )
}

fn key_text(key: u32) -> Option<String> {
    if let Some(name) = mouse_key_text(key) {
        return Some(name);
    }
    let key = u8::try_from(key).ok()?;
    Some(match key {
        0x08 => "Backspace".into(),
        0x09 => "Tab".into(),
        0x0D => "Enter".into(),
        0x1B => "Esc".into(),
        0x20 => "Space".into(),
        0x21 => "PageUp".into(),
        0x22 => "PageDown".into(),
        0x23 => "End".into(),
        0x24 => "Home".into(),
        0x25 => "Left".into(),
        0x26 => "Up".into(),
        0x27 => "Right".into(),
        0x28 => "Down".into(),
        0x2D => "Insert".into(),
        0x2E => "Delete".into(),
        0x5D => "Menu".into(),
        0xA6 => "Browser Back".into(),
        0xA7 => "Browser Forward".into(),
        0xA8 => "Browser Refresh".into(),
        0xA9 => "Browser Stop".into(),
        0xAA => "Browser Search".into(),
        0xAB => "Browser Favorites".into(),
        0xAC => "Browser Home".into(),
        0xAD => "Volume Mute".into(),
        0xAE => "Volume Down".into(),
        0xAF => "Volume Up".into(),
        0xB0 => "Media Next".into(),
        0xB1 => "Media Prev".into(),
        0xB2 => "Media Stop".into(),
        0xB3 => "Play/Pause".into(),
        0xB4 => "Launch Mail".into(),
        0xB5 => "Launch Media".into(),
        0xB6 => "Launch App 1".into(),
        0xB7 => "Launch App 2".into(),
        0x30..=0x39 => ((b'0' + key - 0x30) as char).to_string(),
        0x41..=0x5A => ((b'A' + key - 0x41) as char).to_string(),
        0x60..=0x69 => format!("Num{}", key - 0x60),
        0x6A => "Num*".into(),
        0x6B => "Num+".into(),
        0x6D => "Num-".into(),
        0x6E => "Num.".into(),
        0x6F => "Num/".into(),
        0x70..=0x87 => format!("F{}", key - 0x70 + 1),
        0xBA => ";".into(),
        0xBB => "=".into(),
        0xBC => ",".into(),
        0xBD => "-".into(),
        0xBE => ".".into(),
        0xBF => "/".into(),
        0xC0 => "`".into(),
        0xDB => "[".into(),
        0xDC => "\\".into(),
        0xDD => "]".into(),
        0xDE => "'".into(),
        // 鼠标键走上面的 `mouse_key_text`（伪键码装不进 u8）。
        _ => return None,
    })
}

/// 录制时把「主键盘那一排」和「小键盘」分开。
///
/// gpui 递上来的按键名是 `MapVirtualKeyW(vk, MAPVK_VK_TO_CHAR)` 出来的**字符**：
/// `Numpad0`（VK 0x60）和主键盘的 `0`（VK 0x30）拿到的都是 `"0"`，光看名字分不出来。
/// 名字里没有的信息只能回头问系统 —— 这一刻小键盘那颗键是不是按着的。
///
/// 返回 `(键码, 要不要补 Shift)`：换成小键盘键时 Shift 一律清掉
/// （`+` / `*` 在主键盘上是 Shift 层，小键盘上不是）。
pub fn resolve_numpad(name: &str, key: u32, needs_shift: bool) -> (u32, bool) {
    let numpad = match name {
        "0" => 0x60,
        "1" => 0x61,
        "2" => 0x62,
        "3" => 0x63,
        "4" => 0x64,
        "5" => 0x65,
        "6" => 0x66,
        "7" => 0x67,
        "8" => 0x68,
        "9" => 0x69,
        "." => 0x6E,
        "+" => 0x6B,
        "-" => 0x6D,
        "*" => 0x6A,
        "/" => 0x6F,
        _ => return (key, needs_shift),
    };
    // 小键盘那颗按着、主键盘那颗没按，才算小键盘。
    // 两个都按着（手搭在键盘上乱按）时按主键盘算，至少不会把普通数字键录歪。
    if is_key_down(numpad) && !is_key_down(key) {
        (numpad, false)
    } else {
        (key, needs_shift)
    }
}

/// GPUI 的 key 名（`"a"` / `"f1"` / `"escape"` / `"!"`）转虚拟键码。
///
/// 返回值第二项表示「这个字符本身就在 Shift 层」：GPUI 把 Shift+1 报成 `"!"`
/// 并把 `shift` 位摘掉，注册时要把 MOD_SHIFT 补回去。
pub fn vk_for_key_name(name: &str) -> Option<(u32, bool)> {
    let named = match name {
        "backspace" => Some(0x08),
        "tab" => Some(0x09),
        "enter" => Some(0x0D),
        "escape" => Some(0x1B),
        "space" => Some(0x20),
        "pageup" => Some(0x21),
        "pagedown" => Some(0x22),
        "end" => Some(0x23),
        "home" => Some(0x24),
        "left" => Some(0x25),
        "up" => Some(0x26),
        "right" => Some(0x27),
        "down" => Some(0x28),
        "insert" => Some(0x2D),
        "delete" => Some(0x2E),
        "menu" => Some(0x5D),
        // 万一某个平台把媒体键当成普通按键报上来，也能落到同一个虚拟键码。
        "mediaplaypause" | "play_pause" | "playpause" => Some(0xB3),
        "medianext" | "next_track" | "mediatracknext" => Some(0xB0),
        "mediaprev" | "prev_track" | "mediatrackprev" => Some(0xB1),
        "mediastop" | "stop" => Some(0xB2),
        "volumeup" => Some(0xAF),
        "volumedown" => Some(0xAE),
        "volumemute" => Some(0xAD),
        _ => None,
    };
    if let Some(vk) = named {
        return Some((vk, false));
    }

    if let Some(rest) = name.strip_prefix('f')
        && let Ok(number) = rest.parse::<u32>()
        && (1..=24).contains(&number)
    {
        return Some((0x70 + number - 1, false));
    }

    let mut chars = name.chars();
    let character = chars.next()?;
    if chars.next().is_some() {
        return None;
    }

    // Shift 层字符（数字行与 OEM 键）。
    let shifted = match character {
        '!' => 0x31,
        '@' => 0x32,
        '#' => 0x33,
        '$' => 0x34,
        '%' => 0x35,
        '^' => 0x36,
        '&' => 0x37,
        '*' => 0x38,
        '(' => 0x39,
        ')' => 0x30,
        '_' => 0xBD,
        '+' => 0xBB,
        '{' => 0xDB,
        '}' => 0xDD,
        '|' => 0xDC,
        ':' => 0xBA,
        '"' => 0xDE,
        '<' => 0xBC,
        '>' => 0xBE,
        '?' => 0xBF,
        '~' => 0xC0,
        _ => 0,
    };
    if shifted != 0 {
        return Some((shifted, true));
    }

    let plain = match character {
        'a'..='z' => 0x41 + (character as u32 - 'a' as u32),
        'A'..='Z' => 0x41 + (character as u32 - 'A' as u32),
        '0'..='9' => 0x30 + (character as u32 - '0' as u32),
        '-' => 0xBD,
        '=' => 0xBB,
        '[' => 0xDB,
        ']' => 0xDD,
        '\\' => 0xDC,
        ';' => 0xBA,
        '\'' => 0xDE,
        ',' => 0xBC,
        '.' => 0xBE,
        '/' => 0xBF,
        '`' => 0xC0,
        _ => 0,
    };
    (plain != 0).then_some((plain, false))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 音量键 → 动作的映射（「禁用 Windows 音量条」用它兜底）。
    #[test]
    fn volume_keys_map_to_actions() {
        assert_eq!(volume_action(VK_VOLUME_UP), Some(Action::VolumeUp));
        assert_eq!(volume_action(VK_VOLUME_DOWN), Some(Action::VolumeDown));
        assert_eq!(volume_action(VK_VOLUME_MUTE), Some(Action::MediaMute));
        // 别的键不归它管，免得把媒体键也当成音量。
        assert_eq!(volume_action(VK_MEDIA_NEXT_TRACK), None);
        assert_eq!(volume_action(0x41), None);
    }

    #[test]
    fn display_text_covers_modifiers_and_keys() {
        assert_eq!(display_text(Hotkey::default()), None);
        assert_eq!(
            display_text(Hotkey::new(MOD_CONTROL | MOD_SHIFT, 0x41)).as_deref(),
            Some("Ctrl+Shift+A")
        );
        assert_eq!(
            display_text(Hotkey::new(MOD_WIN | MOD_ALT, 0x70)).as_deref(),
            Some("Alt+Win+F1")
        );
        assert_eq!(
            display_text(Hotkey::new(0, 0xB3)).as_deref(),
            Some("Play/Pause")
        );
    }

    #[test]
    fn key_names_map_to_virtual_keys() {
        assert_eq!(vk_for_key_name("a"), Some((0x41, false)));
        assert_eq!(vk_for_key_name("f12"), Some((0x7B, false)));
        assert_eq!(vk_for_key_name("escape"), Some((0x1B, false)));
        // Shift+1 在 GPUI 里报成 "!"，注册时要补 MOD_SHIFT。
        assert_eq!(vk_for_key_name("!"), Some((0x31, true)));
        assert_eq!(vk_for_key_name("nonsense"), None);
    }

    #[test]
    fn action_lookup_uses_first_match() {
        let mut config = Config::default();
        config.hotkey_flyout.set(0, Hotkey::new(MOD_CONTROL, 0x31));
        config
            .hotkey_settings
            .set(1, Hotkey::new(MOD_CONTROL, 0x31));
        let action = action_for(&config, Hotkey::new(MOD_CONTROL, 0x31));
        assert!(matches!(action, Some(Action::Flyout)));
        config.hotkey_flyout.set(0, Hotkey::default());
        let action = action_for(&config, Hotkey::new(MOD_CONTROL, 0x31));
        assert!(matches!(action, Some(Action::Settings)));
        assert!(action_for(&config, Hotkey::new(MOD_CONTROL, 0x32)).is_none());
    }

    /// 亮度三组快捷键要能按配置查出来，而且注册 id 不能撞。
    #[test]
    fn brightness_actions_have_their_own_ids_and_slots() {
        let mut config = Config::default();
        config
            .hotkey_brightness_monitor_down
            .set(2, Hotkey::new(MOD_CONTROL | MOD_ALT, 0x25));
        assert!(matches!(
            action_for(&config, Hotkey::new(MOD_CONTROL | MOD_ALT, 0x25)),
            Some(Action::BrightnessMonitorDown)
        ));

        let mut ids: Vec<u32> = Vec::new();
        for action in Action::all() {
            for slot in 0..3 {
                ids.push(action.id(slot));
            }
        }
        let unique: std::collections::HashSet<u32> = ids.iter().copied().collect();
        assert_eq!(unique.len(), ids.len(), "同一个 id 注册两次会静默失败");
    }

    /// 「按住连发」覆盖音量 / 亮度的加减和切歌，开关类的动作按一下是一下。
    #[test]
    fn hold_repeat_covers_steps_but_not_toggles() {
        for action in Action::all() {
            let expected = matches!(
                action,
                Action::VolumeUp
                    | Action::VolumeDown
                    | Action::BrightnessUp
                    | Action::BrightnessDown
                    | Action::BrightnessAllUp
                    | Action::BrightnessAllDown
                    | Action::BrightnessMonitorUp
                    | Action::BrightnessMonitorDown
                    | Action::MediaPrev
                    | Action::MediaNext
                    | Action::MediaStop
            );
            assert_eq!(action.repeats_while_held(), expected, "{action:?}");
        }
        // 应用音量不在此列：它和「正在响的是谁」绑在一起，连发容易在不同应用间乱跳。
        assert!(!Action::AppVolumeUp.repeats_while_held());
        // 静音 / 播放暂停是开关：按住连点只会来回翻。
        assert!(!Action::MediaMute.repeats_while_held());
        assert!(!Action::MediaPlayPause.repeats_while_held());
    }

    /// 被钩子吞掉的键在系统那边当没按过，连发循环只能看自己的按下表。
    ///
    /// 这条错了的表现是「按住音量键就动一下，然后完全不理你」—— 循环第一拍
    /// 问系统拿到「没按着」，直接退场。
    #[test]
    fn hook_keys_read_their_state_from_the_down_table() {
        let key = VK_VOLUME_UP;
        forget_key_down(key);
        assert!(!is_key_held(key), "没按过");
        mark_key_down(key);
        assert!(is_key_held(key), "按住不放：系统那边查不到，但表里有");
        forget_key_down(key);
        assert!(!is_key_held(key), "keyup 之后表里的位要清掉");
        // 越界伪键码（鼠标键）不进表，也不该报「按着」。
        assert!(!is_key_held(VK_MOUSE_WHEEL_UP));
    }

    /// 小键盘和主键盘是两套键码：录下来的 `Num0` 不能跟主键盘的 `0` 撞。
    #[test]
    fn numpad_keys_display_apart_from_the_main_row() {
        assert_eq!(key_text(0x60).as_deref(), Some("Num0"));
        assert_eq!(key_text(0x30).as_deref(), Some("0"));
        assert_eq!(key_text(0x6E).as_deref(), Some("Num."));
        assert_eq!(key_text(0x6B).as_deref(), Some("Num+"));
        // 屏幕上写全，录的是哪一颗一眼能看出来。
        assert_eq!(
            display_text(Hotkey::new(MOD_CONTROL | MOD_ALT, 0x60)).as_deref(),
            Some("Ctrl+Alt+Num0")
        );
        // 小键盘没按着的时候，主键盘照旧。
        assert_eq!(resolve_numpad("0", 0x30, false), (0x30, false));
        assert_eq!(resolve_numpad("a", 0x41, false), (0x41, false));
    }

    /// NumLock 关着时小键盘发的是方向 / 删除：靠扫描码 + 扩展位认回数字。
    #[test]
    fn numpad_scancodes_are_read_back_as_digits() {
        let info = |scan: u32, extended: bool| {
            let mut info = KBDLLHOOKSTRUCT::default();
            info.scanCode = scan;
            if extended {
                info.flags = LLKHF_EXTENDED;
            }
            info
        };
        // NumLock 关着：4 / 5 / 0 / . 四个代表位（含最容易混的 Del）。
        assert_eq!(numpad_vk_from_scan(&info(0x4B, false)), Some(0x64));
        assert_eq!(numpad_vk_from_scan(&info(0x4C, false)), Some(0x65));
        assert_eq!(numpad_vk_from_scan(&info(0x52, false)), Some(0x60));
        assert_eq!(numpad_vk_from_scan(&info(0x53, false)), Some(0x6E));
        assert_eq!(numpad_vk_from_scan(&info(0x47, false)), Some(0x67));
        assert_eq!(numpad_vk_from_scan(&info(0x49, false)), Some(0x69));

        // 真正的方向键 / Del / Home 带 E0，不能当成小键盘。
        assert_eq!(numpad_vk_from_scan(&info(0x4B, true)), None);
        assert_eq!(numpad_vk_from_scan(&info(0x53, true)), None);
        // 别的键（字母、主键盘数字）也不是小键盘。
        assert_eq!(numpad_vk_from_scan(&info(0x1E, false)), None);
        assert_eq!(numpad_vk_from_scan(&info(0x02, false)), None);
    }

    /// 鼠标组合用伪键码 0x100 起：显示名、鼠标键判定都要对。
    #[test]
    fn mouse_keys_round_trip_through_display() {
        assert!(is_mouse_key(VK_MOUSE_WHEEL_UP));
        assert!(is_mouse_key(VK_MOUSE_SIDE2));
        assert!(!is_mouse_key(0xAF));
        assert!(!is_mouse_key(0x20));
        assert_eq!(
            display_text(Hotkey::new(MOD_CONTROL | MOD_ALT, VK_MOUSE_WHEEL_UP)).as_deref(),
            Some("Ctrl+Alt+Wheel Up")
        );
        assert_eq!(
            display_text(Hotkey::new(MOD_SHIFT, VK_MOUSE_WHEEL_CLICK)).as_deref(),
            Some("Shift+Wheel Click")
        );
        // 伪键码不能被当成真实 VK 去注册（注册必然失败，还会占一个 id）。
        assert!(!is_hook_key(VK_MOUSE_WHEEL_UP));
        assert!(!is_raw_system_key(VK_MOUSE_WHEEL_UP, MOD_CONTROL));
    }

    #[test]
    fn media_keys_are_routed_to_the_hook() {
        assert!(is_hook_key(0xB3));
        assert!(is_hook_key(0xAF));
        assert!(!is_hook_key(0x41));
    }

    /// 按下表：第一次按下返回 false，按住不放补出来的 keydown 返回 true，
    /// keyup 清掉之后又变回 false。
    ///
    /// 这张表是「忽略系统的长按重复」唯一的判断依据，错了要么重复没挡住、
    /// 要么这颗键按第二下就没反应（表里那一位一直挂着）。
    #[test]
    fn down_table_tells_first_press_from_system_repeat() {
        let key = 0xF4;
        forget_key_down(key);
        assert!(!mark_key_down(key), "第一次按下");
        assert!(mark_key_down(key), "按住不放时系统补的每一发");
        assert!(mark_key_down(key), "补多少发都还是重复");
        forget_key_down(key);
        assert!(!mark_key_down(key), "松开以后再按算新的一次");
        forget_key_down(key);

        // 两个键互不影响（位图按 VK 分位）：另一个键按住不影响这个键的第一发。
        assert!(!mark_key_down(0xF5), "另一个键第一次按下");
        assert!(mark_key_down(0xF5), "它自己的重复");
        assert!(!mark_key_down(key), "这个键松开过，仍算第一发");
        assert!(mark_key_down(key));
        forget_key_down(key);
        forget_key_down(0xF5);

        // 越界的伪键码（鼠标键）不该写进表里，也不该报「已经按着」。
        assert!(!mark_key_down(0x100));
    }

    /// 接管的只有四个播放键；音量键留给系统自己的提示条。
    #[test]
    fn media_action_covers_only_playback_keys() {
        assert_eq!(media_action(VK_MEDIA_PREV_TRACK), Some(Action::MediaPrev));
        assert_eq!(media_action(VK_MEDIA_NEXT_TRACK), Some(Action::MediaNext));
        assert_eq!(
            media_action(VK_MEDIA_PLAY_PAUSE),
            Some(Action::MediaPlayPause)
        );
        assert_eq!(media_action(VK_MEDIA_STOP), Some(Action::MediaStop));
        assert_eq!(media_action(VK_VOLUME_MUTE), None);
        assert_eq!(media_action(0xAE), None);
        assert_eq!(media_action(0xAF), None);
    }
}
