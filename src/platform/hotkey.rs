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
        atomic::{AtomicBool, AtomicIsize, AtomicU32, AtomicU64, Ordering},
    },
};

use windows::{
    Win32::{
        Foundation::{HWND, LPARAM, LRESULT, WPARAM},
        System::{LibraryLoader::GetModuleHandleW, Threading::GetCurrentThreadId},
        UI::{
            Input::KeyboardAndMouse::{
                GetAsyncKeyState, HOT_KEY_MODIFIERS, RegisterHotKey, UnregisterHotKey, VIRTUAL_KEY,
                VK_CONTROL, VK_LWIN, VK_MENU, VK_RWIN, VK_SHIFT,
            },
            WindowsAndMessaging::{
                CallNextHookEx, CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW,
                HWND_MESSAGE, KBDLLHOOKSTRUCT, LLKHF_EXTENDED, LLKHF_INJECTED, MSG, PostMessageW,
                PostThreadMessageW, RegisterClassW, SetWindowsHookExW, TranslateMessage,
                UnhookWindowsHookEx, WH_KEYBOARD_LL, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP,
                WM_HOTKEY, WM_KEYDOWN, WM_KEYUP, WM_SYSKEYDOWN, WM_SYSKEYUP, WNDCLASSW,
            },
        },
    },
    core::w,
};

use crate::config::{AppBlockConfig, Config, Hotkey, HotkeySlots, OsdConfig, OsdFunction};

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
/// 需要走低级钩子的组合：(mods, vk, 动作 id)。媒体键 / 浏览器键，以及会被小键盘冒充的
/// 方向键那一组（那组要按扩展位分辨真假，`RegisterHotKey` 分不出来）。
///
/// 带上动作 id 是因为「不发送原始按键」「忽略系统重复」两项目前都是**按页签**存的：
/// 钩子里只拿得到键码，得先认出这个组合是哪个动作，才知道该问哪一份设置。
static HOOK_BINDINGS: OnceLock<Mutex<Vec<(u32, u32, u32)>>> = OnceLock::new();
/// 鼠标组合：(mods, 伪键码)。鼠标钩子（`mouse_wheel.rs`）按这张表拦。
static MOUSE_BINDINGS: OnceLock<Mutex<Vec<(u32, u32)>>> = OnceLock::new();
/// 热键线程的窗口句柄（0 = 还没建好）。
static SINK: AtomicIsize = AtomicIsize::new(0);
/// 把按下的组合送回界面线程。
static SENDER: OnceLock<async_channel::Sender<Hotkey>> = OnceLock::new();
/// 正在录制快捷键：钩子把媒体键也送回来。
static CAPTURING: AtomicBool = AtomicBool::new(false);
/// 低级键盘钩子句柄（0 = 没装上）。被系统摘掉时看门狗会重装。
static HOOK: AtomicIsize = AtomicIsize::new(0);
/// 叫醒钩子线程去重装钩子的线程消息（`WM_APP + 8`，和鼠标钩子的 `+7` 错开）。
const WAKE_HOOK: u32 = WM_APP + 8;
/// 「按住连续触发」开着，而且用户没勾「理会系统的长按重复」：系统补出来的重复
/// 一律不理会，重复节奏完全由我们自己掌握。
///
/// 两个页签各一份：音频页那套管音量 / 应用音量 / 媒体键，屏幕页那套管六个亮度动作。
static IGNORE_SYSTEM_REPEAT_AUDIO: AtomicBool = AtomicBool::new(false);
static IGNORE_SYSTEM_REPEAT_SCREEN: AtomicBool = AtomicBool::new(false);
/// 「按下快捷键时不发送原始按键」：配过的组合一律交给低级钩子吞掉，
/// 连 `RegisterHotKey` 那一路都不注册（它拦不住用 Raw Input 读键盘的程序）。
///
/// 同样是两个页签各一份：在音频页打开，屏幕页的组合照旧发给前台。
static SWALLOW_RAW_AUDIO: AtomicBool = AtomicBool::new(false);
static SWALLOW_RAW_SCREEN: AtomicBool = AtomicBool::new(false);
/// 按住会连发的动作（`Action` 的 repr 值）。`apply_config` 里按配置重算。
///
/// 钩子拿它判断「系统补出来的重复 keydown」该不该吞：没勾连发的动作按住只该动
/// 一下，重复必须当场吃掉 —— 光靠 `IGNORE_SYSTEM_REPEAT` 那个总开关不够，
/// 它一关（用户想「让系统节奏也算数」）就会波及所有键。
static REPEAT_ACTIONS: OnceLock<Mutex<Vec<u32>>> = OnceLock::new();

fn repeat_actions() -> &'static Mutex<Vec<u32>> {
    REPEAT_ACTIONS.get_or_init(|| Mutex::new(Vec::new()))
}

/// 这个动作勾了「按住连发」没有。
fn repeats_action(action: Action) -> bool {
    repeat_actions()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .contains(&(action as u32))
}

/// 按注册 id 反查动作是不是勾了连发（`id = action * 8 + slot`）。
fn repeats_action_id(id: u32) -> bool {
    repeat_actions()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .contains(&(id / 8))
}

/// 动作号反查动作（配置里存的是 `Action` 的 repr 值）。
fn action_of_id(id: u32) -> Option<Action> {
    Action::all().into_iter().find(|action| *action as u32 == id)
}

/// 这一发按下来之前就已经按着了（= 系统长按重复补的），要不要吞掉。
///
/// 两道闸：这个动作那一页的「忽略系统重复」开着就一律吞（重复由
/// `start_hold_repeat` 自己走）；关着时只看这个动作有没有勾连发 —— 没勾的按住
/// 只该动一下。认不出动作的（比如没配成快捷键的键）不额外吞，保持原来的行为。
fn swallow_system_repeat(already_down: bool, action: Option<Action>) -> bool {
    if !already_down {
        return false;
    }
    if ignore_system_repeat_for(action) {
        return true;
    }
    matches!(action, Some(action) if !repeats_action(action))
}
/// 现在按着的键（VK 0..=255 的位图）。keyup 也走低级钩子，靠它分辨
/// 「第一次按下」和「系统自动重复补出来的 keydown」。
/// 前 4 个 64 位字覆盖真实键码 0..=255，最后一个是伪键码（小键盘 `Num Left` 那一档）0x100 起。
static DOWN_KEYS: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];

fn desired() -> &'static Mutex<Vec<(u32, u32, u32)>> {
    DESIRED.get_or_init(|| Mutex::new(Vec::new()))
}

fn registered() -> &'static Mutex<Vec<i32>> {
    REGISTERED.get_or_init(|| Mutex::new(Vec::new()))
}

fn hook_bindings() -> &'static Mutex<Vec<(u32, u32, u32)>> {
    HOOK_BINDINGS.get_or_init(|| Mutex::new(Vec::new()))
}

fn mouse_bindings() -> &'static Mutex<Vec<(u32, u32)>> {
    MOUSE_BINDINGS.get_or_init(|| Mutex::new(Vec::new()))
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

/// 调试日志开着没有（热区滚轮那种每发都打的地方先问一句，别白拼字符串）。
pub fn trace_enabled() -> bool {
    trace_path().is_some()
}

/// 前台进程名（小写、带 `.exe`），调试日志里用来分辨「被谁的窗口挡了」。
pub fn foreground_name() -> String {
    crate::platform::fullscreen::foreground_process_name()
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

    /// 动作的稳定名字：写进配置（`hotkey_repeat_actions`），也是设置页里那个勾的键。
    ///
    /// 别跟着枚举变体名改 —— 老配置里存的就是这些字符串。
    pub fn key(self) -> &'static str {
        match self {
            Action::Flyout => "flyout",
            Action::Settings => "settings",
            Action::VolumeUp => "volume_up",
            Action::VolumeDown => "volume_down",
            Action::AppVolumeUp => "app_volume_up",
            Action::AppVolumeDown => "app_volume_down",
            Action::MediaPrev => "media_prev",
            Action::MediaNext => "media_next",
            Action::MediaPlayPause => "media_play_pause",
            Action::MediaMute => "media_mute",
            Action::MediaStop => "media_stop",
            Action::BrightnessUp => "brightness_up",
            Action::BrightnessDown => "brightness_down",
            Action::BrightnessAllUp => "brightness_all_up",
            Action::BrightnessAllDown => "brightness_all_down",
            Action::BrightnessMonitorUp => "brightness_monitor_up",
            Action::BrightnessMonitorDown => "brightness_monitor_down",
        }
    }

    /// 这个名字（[`key`](Self::key)）对应哪个动作。
    pub fn from_key(name: &str) -> Option<Action> {
        Action::all().into_iter().find(|action| action.key() == name)
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

    /// 这个动作归哪一页：`true` = 屏幕页（六个亮度动作），`false` = 音频页。
    ///
    /// 快捷键页的两个页签各有一套「按住连续触发」和「不发送原始按键」，钩子里
    /// 只拿得到键码，得靠这个把命中的组合映射回它属于哪一套 —— 在音频页把间隔
    /// 调成 60ms，不该把亮度的节奏也一起改掉。
    pub fn is_screen(self) -> bool {
        matches!(
            self,
            Action::BrightnessUp
                | Action::BrightnessDown
                | Action::BrightnessAllUp
                | Action::BrightnessAllDown
                | Action::BrightnessMonitorUp
                | Action::BrightnessMonitorDown
        )
    }

    /// 第 n 组快捷键的注册 id。
    fn id(self, slot: usize) -> u32 {
        self as u32 * 8 + slot as u32
    }
}

/// 这个动作按住不松手时要不要连续触发。
///
/// 两道闸都要过：**它那一页**的总闸开着（音频页 `hotkey_hold_repeat`、屏幕页
/// `hotkey_screen_hold_repeat`），而且这个动作在 `hotkey_repeat_actions` 里
/// （设置页里每个快捷键前面那个勾）。
pub fn repeats_on_hold(config: &Config, action: Action) -> bool {
    let gate = if action.is_screen() {
        config.hotkey_screen_hold_repeat
    } else {
        config.hotkey_hold_repeat
    };
    gate && config
        .hotkey_repeat_actions
        .iter()
        .any(|name| name == action.key())
}

/// 这个动作那个页签的连发参数：(总闸, 间隔, 延迟)。
///
/// 两个页签各存一份，按住音量键和按住亮度键可以完全不同步。
pub fn repeat_settings(config: &Config, action: Action) -> (bool, u32, u32) {
    let screen = action.is_screen();
    let on = repeats_on_hold(config, action);
    if screen {
        (
            on,
            config.hotkey_screen_repeat_interval,
            config.hotkey_screen_repeat_delay,
        )
    } else {
        (on, config.hotkey_repeat_interval, config.hotkey_repeat_delay)
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

/// 小键盘在 **NumLock 关着** 时那几颗键的伪键码。
///
/// 这会儿它们干的是方向键的活儿，但用户心里那是「小键盘上的方向键」，不是真的
/// 方向键、也不是 NumLock 开着时的数字（`Num4`）。三者得各算各的：
///
/// - `Left` 0x25          —— 方向键区那颗，带 E0 扩展位；
/// - `Num Left` 0x124     —— 小键盘 4，NumLock 关着；
/// - `Num4` 0x64          —— 小键盘 4，NumLock 开着（数字）。
///
/// 真键码只到 0xFF，所以这两档另开一段（和鼠标键 0x100 起同一套办法）：
/// 配置里存这几个数，注册时跳过，全交给键盘钩子按扫描码认。
pub const VK_NUMPAD_INS: u32 = 0x120;
pub const VK_NUMPAD_END: u32 = 0x121;
pub const VK_NUMPAD_DOWN: u32 = 0x122;
pub const VK_NUMPAD_PGDN: u32 = 0x123;
pub const VK_NUMPAD_LEFT: u32 = 0x124;
pub const VK_NUMPAD_CLEAR: u32 = 0x125;
pub const VK_NUMPAD_RIGHT: u32 = 0x126;
pub const VK_NUMPAD_HOME: u32 = 0x127;
pub const VK_NUMPAD_UP: u32 = 0x128;
pub const VK_NUMPAD_PGUP: u32 = 0x129;
pub const VK_NUMPAD_DEL: u32 = 0x12A;

/// 这一格是不是「NumLock 关着的小键盘方向键」。
pub fn is_numpad_nav(key: u32) -> bool {
    (VK_NUMPAD_INS..=VK_NUMPAD_DEL).contains(&key)
}

/// 媒体键虚拟键码（`windows` crate 里没导出这几个常量，照 winuser.h 写死）。
pub const VK_MEDIA_NEXT_TRACK: u32 = 0xB0;
pub const VK_MEDIA_PREV_TRACK: u32 = 0xB1;
pub const VK_MEDIA_STOP: u32 = 0xB2;
pub const VK_MEDIA_PLAY_PAUSE: u32 = 0xB3;
pub const VK_VOLUME_MUTE: u32 = 0xAD;
pub const VK_VOLUME_DOWN: u32 = 0xAE;
pub const VK_VOLUME_UP: u32 = 0xAF;

/// 小键盘在 NumLock 关着时会「冒充」的那一组键（就是小键盘上 0..9 和 `.` 对应的
/// Ins / End / Down / PgDn / Left / Clear / Right / Home / Up / PgUp / Del）。
///
/// 真键盘上这几颗一律是扩展键（E0 前缀），小键盘那几颗不是：`vkCode` 一模一样，
/// 只有扩展位不同。AHK 里也是同一件事 —— `Left` 与 `NumpadLeft`。
pub const VK_INSERT: u32 = 0x2D;
pub const VK_DELETE: u32 = 0x2E;
pub const VK_HOME: u32 = 0x24;
pub const VK_END: u32 = 0x23;
pub const VK_PRIOR: u32 = 0x21; // PageUp
pub const VK_NEXT: u32 = 0x22; // PageDown
pub const VK_LEFT: u32 = 0x25;
pub const VK_UP: u32 = 0x26;
pub const VK_RIGHT: u32 = 0x27;
pub const VK_DOWN: u32 = 0x28;
pub const VK_CLEAR: u32 = 0x0C; // 小键盘 5（NumLock 关着时）

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
    let mut hook = match unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), None, 0) } {
        Ok(hook) => {
            HOOK.store(hook.0 as isize, Ordering::SeqCst);
            trace(&format!("hook installed handle=0x{:X}", hook.0 as isize));
            Some(hook)
        }
        Err(error) => {
            trace(&format!("hook install FAILED {error:?}"));
            eprintln!("媒体键钩子安装失败，媒体键快捷键不可用：{error}");
            None
        }
    };

    // 看门狗：和鼠标钩子同一个理由 —— 低级钩子回调超过系统的
    // `LowLevelHooksTimeout`（默认 300ms）会被 Windows **悄悄摘掉**，而且摘了
    // 不会告诉你。键盘钩子这一摘，媒体键 / 音量键就全部「失效」：键没被吞，
    // 系统自己处理（音量条照弹），靠 `RegisterHotKey` 绑的组合却还好使 ——
    // 表现就是「有绑快捷键的正常，没绑的不灵，重启程序又好了」。
    // 每 5 秒叫醒本线程重装一次：活着等于换了张新票（顺带排到钩子链最前，
    // 别的程序装了钩子也挡不住），被摘了自动回来。
    let thread_id = unsafe { GetCurrentThreadId() };
    std::thread::Builder::new()
        .name("bolume-key-hook-watch".into())
        .spawn(move || loop {
            std::thread::sleep(std::time::Duration::from_secs(5));
            if unsafe { PostThreadMessageW(thread_id, WAKE_HOOK, WPARAM(0), LPARAM(0)) }.is_err() {
                // 队列满 / 线程没了：这一轮跳过，别把看门狗自己弄死。
                continue;
            }
        })
        .ok();

    // 窗口建好之前调用方可能已经写过期望表，先应用一次。
    unsafe { apply_desired(hwnd) };

    let mut message = MSG::default();
    while unsafe { GetMessageW(&mut message, None, 0, 0) }.0 > 0 {
        if message.message == WAKE_HOOK {
            // 先卸再装：句柄可能早就被系统摘了，Unhook 失败无所谓，重装能恢复。
            if let Some(old) = hook.take() {
                unsafe {
                    let _ = UnhookWindowsHookEx(old);
                }
                HOOK.store(0, Ordering::SeqCst);
            }
            match unsafe { SetWindowsHookExW(WH_KEYBOARD_LL, Some(keyboard_proc), None, 0) } {
                Ok(fresh) => {
                    hook = Some(fresh);
                    HOOK.store(fresh.0 as isize, Ordering::SeqCst);
                }
                Err(error) => {
                    // 装不上就空转这一轮，5 秒后再试；别把旧句柄留着当挂在链上。
                    trace(&format!("key hook rearm failed: {error:?}"));
                }
            }
            continue;
        }
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
        // 小键盘在 NumLock 关着时按下记的是 `Num Left` 那一档、松开报的是方向键，
        // 两位一起清，否则连发循环以为那颗键一直按着，手松了还在往下走。
        if let Some(identity) = key_identity(info) {
            forget_key_down(identity);
        }
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }
    // 小键盘那几颗在 NumLock 关着时报的是方向 / Ins / Del 那一组：键码和真方向键
    // 一模一样，只有扫描码（还有真方向键的扩展位）能分开。这里先换成「Num Left」
    // 那一档的身份 —— 录制、触发、连发全都按它算。
    //
    // 于是三颗键各算各的：`Left`（方向键区）、`Num Left`（NumLock 关着的小键盘 4）、
    // `Num4`（NumLock 开着的小键盘 4）。绑在 `Left` 上的动作只认真方向键，
    // 绑在 `Num Left` 上的只认 NumLock 关着时的小键盘。
    let key = key_identity(info).unwrap_or(info.vkCode);
    // 这一发按下来之前，这颗键是不是还按着 —— 是的话就是系统「长按重复」补出来的
    // keydown（键盘按住不放时，系统按自己的重复延迟 / 速率一直补）。
    //
    // 这里只信我们自己这张表，**不能**再问 `GetAsyncKeyState`：被钩子吞掉的
    // keydown 根本不会进系统的按键状态（吞掉就是没发生过），问出来的永远是「没按着」，
    // 那样判一次错一次，等于没做。表是 keyup 清的，而 keyup 也走这个钩子。
    let already_down = mark_key_down(key);

    let mods = current_modifiers();
    // 这一发命中的是哪个动作（没绑定就是空）。两项「按页签」的设置都靠它分辨：
    // 音频页和屏幕页各存一份，钩子里只拿得到键码。
    let binding = binding_action(mods, key);
    // 没绑定快捷键的裸媒体键 / 音量键，动作由这颗键本身决定（发送指令、弹卡片）。
    let action = binding
        .or_else(|| media_action(key))
        .or_else(|| volume_action(key));
    let system_repeat = already_down && ignore_system_repeat_for(action);

    // 普通键（字母 / F 键那一类）平时根本不归钩子管。只有开着「按下时不发送
    // 原始按键」、并且这一发正好是配过的组合时才接管 —— 其余一律原样放行。
    // 录制期间也放行：录制靠设置页窗口自己的 keydown，钩子插一手会把
    // Esc / Backspace 这类也吞掉，反而录不下去。
    if !is_hook_key(key) {
        if !swallow_raw_for(binding)
            || CAPTURING.load(Ordering::SeqCst)
            || list_blocked()
            || binding.is_none()
        {
            return unsafe { CallNextHookEx(None, code, wparam, lparam) };
        }
        trace(&format!("hook swallow raw vk=0x{key:02X} mods={mods}"));
        if swallow_system_repeat(already_down, action) {
            return LRESULT(1);
        }
        send_hotkey(Hotkey::new(mods, key));
        return LRESULT(1);
    }
    // 到这里都是「我们认识的键」：重复的那几发一律吞掉不派发（也不放给系统，
    // 否则系统那条音量条 / 播放器照样按自己的节奏动，等于没忽略）。
    // 重复由 `start_hold_repeat` 按设置里的间隔自己走。

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

    // 前台命中黑名单：一个键都不留，原样还给系统 / 游戏。
    // 只看名单（不查全屏）：壳层的全屏查询在低级钩子里不敢调，慢一下按键就丢了；
    // 全屏那一路由主线程兜（`forward_to_system`）。
    if list_blocked() {
        trace(&format!("hook pass-through vk=0x{key:02X} (app blocked)"));
        return unsafe { CallNextHookEx(None, code, wparam, lparam) };
    }

    let bound = binding.is_some();
    trace(&format!(
        "hook vk=0x{key:02X} mods={mods} bound={bound} injected={injected} capturing={}",
        CAPTURING.load(Ordering::SeqCst)
    ));
    if bound {
        // 「按下快捷键时不发送原始按键」关着：这一发照常还给系统（那本来就是
        // 开关的意思），系统补出来的重复也照样放行，前台该怎么重复怎么重复。
        // 例外见 `taken_over`。这一项**按页签**看：音频页开着只管音量那一套，
        // 屏幕页的组合照旧发给前台。
        //
        // 放行之后动作只认第一次按下：系统的重复 keydown 每秒几十发，每一发都
        // 派一次动作会跟 `start_hold_repeat` 叠成两套节奏，按住一颗键音量涨得飞快。
        if !swallow_raw_for(binding) && !taken_over(key) {
            if !already_down {
                send_hotkey(hotkey);
            }
            trace(&format!("hook pass-through vk=0x{key:02X} (raw allowed)"));
            return unsafe { CallNextHookEx(None, code, wparam, lparam) };
        }
        if swallow_system_repeat(already_down, action) {
            return LRESULT(1);
        }
        send_hotkey(hotkey);
        return LRESULT(1);
    }
    // 没配这个键：系统媒体键 / 音量键按「屏幕提示」里对应那一项决定拦不拦。
    // 拦下来之后一样送给主线程 —— 音量它自己动手改再弹卡片，媒体键弹完卡片
    // 把键转发给播放器。那一项关着就原样还给系统（Windows 自带那条照旧出现）。
    // 带修饰键的不算（那是用户的组合键）。
    if mods == 0 {
        if let Some(unbound) = media_action(key).or_else(|| volume_action(key)) {
            if takeover(unbound) {
                if swallow_system_repeat(already_down, Some(unbound)) {
                    return LRESULT(1);
                }
                send_hotkey(hotkey);
                return LRESULT(1);
            }
        }
    }
    unsafe { CallNextHookEx(None, code, wparam, lparam) }
}

/// 这一发（修饰键 + 键码）命中的是哪个动作；没配过就是 `None`。
///
/// 钩子里要用它分辨「这属于音频页还是屏幕页」—— 两页的「不发送原始按键」
/// 和「忽略系统重复」各存一份。
fn binding_action(mods: u32, key: u32) -> Option<Action> {
    hook_bindings()
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .iter()
        .find(|binding| binding.0 == mods && binding.1 == key)
        .and_then(|binding| action_of_id(binding.2))
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
    // 「按下时不发送原始按键」开着时，普通键的 keydown 也被钩子吞了：
    // 系统那边同样当它没按过，只能问钩子自己的按下表（钩子对所有键都在记）。
    if hook_covers(key) {
        return tracked_down(key);
    }
    is_key_down(key)
}

/// 小键盘的数字 / 符号键码（`VK_NUMPAD0`..`VK_DIVIDE`）。
fn is_numpad_key(key: u32) -> bool {
    (0x60..=0x6F).contains(&key)
}

/// 按下表里那一位还挂着没有。
fn tracked_down(key: u32) -> bool {
    if key > 319 {
        return false;
    }
    DOWN_KEYS[(key / 64) as usize].load(Ordering::SeqCst) & (1u64 << (key % 64)) != 0
}

/// 开 / 关「按下快捷键时不发送原始按键」（配置变更时由 `apply_config` 同步）。
///
/// 两个页签各一份：`audio` 管音量 / 应用音量 / 媒体键那一套，`screen` 管六个亮度动作。
///
/// 只置个原子量：真正「把这颗键从输入队列里拿掉」的是钩子，而哪张表进钩子
/// 是 `apply_desired` 算的 —— 那边会读这个值，所以这里之后还得走一次 `apply`。
pub fn set_swallow_raw(audio: bool, screen: bool) {
    SWALLOW_RAW_AUDIO.store(audio, Ordering::SeqCst);
    SWALLOW_RAW_SCREEN.store(screen, Ordering::SeqCst);
}

/// 这个动作那一页开没开「不发送原始按键」；认不出动作按音频页算。
fn swallow_raw_for(action: Option<Action>) -> bool {
    if action.is_some_and(|action| action.is_screen()) {
        SWALLOW_RAW_SCREEN.load(Ordering::SeqCst)
    } else {
        SWALLOW_RAW_AUDIO.load(Ordering::SeqCst)
    }
}

/// 任何一页开着都算「普通键也归钩子管」：钩子外面分不出页签，先收进来，
/// 进钩子之后再按命中的动作决定吞不吞（见 `keyboard_proc`）。
fn swallow_raw_any() -> bool {
    SWALLOW_RAW_AUDIO.load(Ordering::SeqCst) || SWALLOW_RAW_SCREEN.load(Ordering::SeqCst)
}

/// 这颗键归不归钩子管：媒体 / 小键盘 / 方向那一组本来就走钩子；
/// 打开「按下时不发送原始按键」之后，普通键也算进来。
///
/// 普通键在钩子里只做一件事 —— 命中了绑定就吞掉，没命中照常放行
/// （见 `keyboard_proc`），所以「算进来」不影响别的按键。
fn hook_covers(key: u32) -> bool {
    is_hook_key(key) || swallow_raw_any()
}

/// 开 / 关「忽略系统的长按重复」（配置变更时由 `apply_config` 同步）。
///
/// 同样按页签分两份。只影响我们**自己接管**的键：`RegisterHotKey` 那一路靠
/// `MOD_NOREPEAT`（要重新注册，所以这个函数之后还得走一次 `apply`），
/// 低级钩子那一路看的就是这个开关。
pub fn set_ignore_system_repeat(audio: bool, screen: bool) {
    IGNORE_SYSTEM_REPEAT_AUDIO.store(audio, Ordering::SeqCst);
    IGNORE_SYSTEM_REPEAT_SCREEN.store(screen, Ordering::SeqCst);
}

/// 这个动作那一页要不要无视系统自己补的长按重复；认不出动作按音频页算。
fn ignore_system_repeat_for(action: Option<Action>) -> bool {
    if action.is_some_and(|action| action.is_screen()) {
        IGNORE_SYSTEM_REPEAT_SCREEN.load(Ordering::SeqCst)
    } else {
        IGNORE_SYSTEM_REPEAT_AUDIO.load(Ordering::SeqCst)
    }
}

/// 记下「这颗键现在按着」，返回它之前是不是就已经按着了。
///
/// 之前就按着 = 这一发 keydown 是系统按住重复补出来的。真实 VK 只到 0xFF，
/// 四段 u64 一位一个键，够用而且不用加锁（钩子里不能慢慢来）。
fn mark_key_down(key: u32) -> bool {
    if key > 319 {
        return false;
    }
    let word = &DOWN_KEYS[(key / 64) as usize];
    let bit = 1u64 << (key % 64);
    word.fetch_or(bit, Ordering::SeqCst) & bit != 0
}

/// 松开：把这一位清掉。
fn forget_key_down(key: u32) {
    if key > 319 {
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

/// 钩子负责的键：浏览器键 0xA6..0xAC、音量键 0xAD..0xAF、媒体键 0xB0..0xB7，
/// 小键盘（0x60..0x6F），再加上会被小键盘冒充的那一组导航键。
///
/// 后两组都必须走钩子、不能交给 `RegisterHotKey`：它只认键码，而小键盘在 NumLock
/// 关着时发的就是 `VK_LEFT` 那一组，注册了 `Left` 就会被小键盘蹭响，也没法把
/// `Num4` / `Num Left` / `Left` 三者分开（见 `key_identity`）。
pub fn is_hook_key(key: u32) -> bool {
    (0xA6..=0xB7).contains(&key)
        || is_numpad_key(key)
        || is_numpad_nav(key)
        || is_nav_key(key)
}

/// 会被小键盘在 NumLock 关着时冒充的键（见 `VK_INSERT` 那一段的说明）。
pub fn is_nav_key(key: u32) -> bool {
    matches!(
        key,
        VK_INSERT
            | VK_DELETE
            | VK_HOME
            | VK_END
            | VK_PRIOR
            | VK_NEXT
            | VK_LEFT
            | VK_UP
            | VK_RIGHT
            | VK_DOWN
            | VK_CLEAR
    )
}

/// 这一发按键带不带扩展位（E0 前缀）。
fn is_extended(info: &KBDLLHOOKSTRUCT) -> bool {
    info.flags.0 & LLKHF_EXTENDED.0 != 0
}

/// 这一发按键在 Bolume 眼里是哪颗键 —— 认不出来就用系统给的 `vkCode`。
///
/// 小键盘两档身份都在这里定：
/// - NumLock 开着：系统报的就是 `VK_NUMPAD4` 那一套，原样返回（数字）；
/// - NumLock 关着：报的是方向 / Ins / Del 那一组，键码和真方向键**完全相同**，
///   只有扫描码能认出来它其实在小键盘上，换成 `Num Left` 那几个伪键码
///   （见 `VK_NUMPAD_LEFT`）—— 于是「真方向键」「小键盘方向键」互不相干。
fn key_identity(info: &KBDLLHOOKSTRUCT) -> Option<u32> {
    if is_numpad_key(info.vkCode) {
        return Some(info.vkCode);
    }
    // 带扩展位的才是真方向键 / Ins / Del，小键盘那几颗不带。
    if !is_nav_key(info.vkCode) || is_extended(info) {
        return None;
    }
    Some(match info.scanCode {
        0x52 => VK_NUMPAD_INS,   // Ins → 小键盘 0
        0x4F => VK_NUMPAD_END,   // End → 小键盘 1
        0x50 => VK_NUMPAD_DOWN,  // Down → 小键盘 2
        0x51 => VK_NUMPAD_PGDN,  // PgDn → 小键盘 3
        0x4B => VK_NUMPAD_LEFT,  // Left → 小键盘 4
        0x4C => VK_NUMPAD_CLEAR, // Clear → 小键盘 5
        0x4D => VK_NUMPAD_RIGHT, // Right → 小键盘 6
        0x47 => VK_NUMPAD_HOME,  // Home → 小键盘 7
        0x48 => VK_NUMPAD_UP,    // Up → 小键盘 8
        0x49 => VK_NUMPAD_PGUP,  // PgUp → 小键盘 9
        0x53 => VK_NUMPAD_DEL,   // Del → 小键盘 .
        _ => return None,
    })
}

/// 没配快捷键的系统媒体键 / 音量键要不要由我们接管：**只看「屏幕提示」里对应那一项**。
///
/// 以前这里是两套闸门：「接管系统媒体键」管四个媒体键，`osd.volume` / `osd.mute`
/// 管另外两颗。结果自相矛盾 —— 接管开关关着，按音量键照样被拦下来（因为「音量」
/// 那一项开着），用户看到的就是「没开开关也给接管了」。
///
/// 其实这两件事在用户眼里是同一件事：弹不弹我们这张卡。所以并成一套 ——
/// 提示开着才拦，提示关掉就把键原样还回系统（Windows 自带那条照旧出现）。
///
/// 位下标 = [`OsdFunction::ALL`] 里的位置。
static TAKEOVER: AtomicU32 = AtomicU32::new(0);

/// 同步「哪些系统键归我们管」（`apply_config` 与设置页开关时调用）。
pub fn set_takeover(osd: &OsdConfig) {
    let mut mask = 0u32;
    for (index, function) in OsdFunction::ALL.iter().enumerate() {
        if osd.shows(*function) {
            mask |= 1 << index;
        }
    }
    TAKEOVER.store(mask, Ordering::SeqCst);
    trace(&format!(
        "takeover enabled={} mask=0x{mask:02X}",
        osd.enabled
    ));
}

/// 这个动作对应「屏幕提示」里的哪一项；不是系统媒体 / 音量键就是 `None`。
fn osd_function_of(action: Action) -> Option<OsdFunction> {
    match action {
        Action::VolumeUp | Action::VolumeDown => Some(OsdFunction::Volume),
        Action::MediaMute => Some(OsdFunction::Mute),
        Action::MediaPrev => Some(OsdFunction::PrevTrack),
        Action::MediaNext => Some(OsdFunction::NextTrack),
        Action::MediaPlayPause => Some(OsdFunction::PlayPause),
        Action::MediaStop => Some(OsdFunction::Stop),
        _ => None,
    }
}

/// 这颗键归不归我们管：只有「它的提示此刻会弹」才拦。
fn takeover(action: Action) -> bool {
    let Some(function) = osd_function_of(action) else {
        return false;
    };
    let Some(index) = OsdFunction::ALL.iter().position(|item| *item == function) else {
        return false;
    };
    TAKEOVER.load(Ordering::SeqCst) & (1 << index) != 0
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

/// 这颗键就算「按下快捷键时不发送原始按键」关着，也得从输入队列里拿掉 ——
/// 它本身就是被我们接管的系统键，放出去会再动一次：音量 / 静音键叠一档，
/// 顺便把系统那条音量条叫出来；媒体键会让播放器自己也跳一下。
///
/// 其余的键（普通键、方向 / Ins / Del 那一组、小键盘）关着时一律原样还给系统。
fn taken_over(key: u32) -> bool {
    volume_action(key).is_some() || media_action(key).is_some()
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
    let mut hooked: Vec<(u32, u32, u32)> = Vec::new();
    let mut moused: Vec<(u32, u32)> = Vec::new();
    for (id, mods, key) in bindings {
        if key == 0 || seen.contains(&(mods, key)) {
            continue;
        }
        seen.push((mods, key));
        // 鼠标键不进 RegisterHotKey（伪键码注册不上），单独交给鼠标钩子。
        if is_mouse_key(key) {
            moused.push((mods, key));
            continue;
        }
        // 两项「按页签」的设置都得先认出这个组合是哪个动作：音频页和屏幕页各存一份。
        let action = action_of_id(id / 8);
        // 钩子活着的时候，「按下时不发送原始按键」把所有配过的组合都收进钩子：
        // `RegisterHotKey` 只能让窗口收不到，拦不住用 Raw Input 读键盘的程序，
        // 而低级钩子返回 1 是真的从输入队列里抹掉。
        // 钩子没装上（HOOK = 0）时仍然走注册 —— 宁可按键漏出去，也别让快捷键整个失效。
        let hook_alive = HOOK.load(Ordering::SeqCst) != 0;
        if is_hook_key(key) || (hook_alive && swallow_raw_for(action)) {
            // 媒体键交给低级钩子，不走 RegisterHotKey（那类键注册不上，
            // 万一注册上了还会和钩子重复触发）。方向 / Ins / Del 那一组也走钩子：
            // `RegisterHotKey` 只认键码，小键盘在 NumLock 关着时冒充的就是同一个
            // 键码，注册了就会被小键盘蹭响；钩子能看扩展位，分得出真假。
            hooked.push((mods, key, id / 8));
            continue;
        }
        // 「忽略系统的长按重复」：按住组合键时只要第一发，后面的重复交给我们
        // 自己的 `start_hold_repeat`。不带这个标志时系统会按键盘的重复延迟 /
        // 速率一直补 `WM_HOTKEY`，两套节奏叠在一起。
        //
        // 没勾连发的动作**永远**带这个标志：总开关关着时也不能让系统的重复把它
        // 继续往下推 —— 那样「这个键不连发」的勾就形同虚设。
        let api_mods = if ignore_system_repeat_for(action) || !repeats_action_id(id) {
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
    // 系统媒体键 / 音量键拦不拦，跟着「屏幕提示」里每个功能自己那一项走 ——
    // 和「启用屏幕提示」是同一个开关，没有第二套闸门。
    set_takeover(&config.osd);
    // 只有「按住连续触发」开着的时候才谈得上「忽略系统的长按重复」：关掉连发时
    // 我们自己是不会补的，再把系统那一路也关掉，按住音量键就彻底不动了。
    // 两个页签各一份，各自受自己那个总闸管。
    set_ignore_system_repeat(
        config.hotkey_hold_repeat && config.hotkey_ignore_system_repeat,
        config.hotkey_screen_hold_repeat && config.hotkey_screen_ignore_system_repeat,
    );
    // 这一项决定「配过的组合交给钩子还是 RegisterHotKey」，得赶在下面的 `apply` 前面。
    set_swallow_raw(
        config.hotkey_swallow_raw,
        config.hotkey_screen_swallow_raw,
    );
    // 钩子按这张表判断「系统补出来的重复要不要吞」，所以它得先于下面的注册更新。
    *repeat_actions()
        .lock()
        .unwrap_or_else(|error| error.into_inner()) = Action::all()
        .into_iter()
        .filter(|action| repeats_on_hold(config, *action))
        .map(|action| action as u32)
        .collect();
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

/// 「NumLock 关着的小键盘」那几个键的显示名（伪键码，装不进 `u8`）。
///
/// 写全一点：`Num Left` 一眼能看出是「小键盘上那颗方向」，跟 `Left`、`Num4` 都不同。
fn numpad_nav_text(key: u32) -> Option<String> {
    Some(
        match key {
            VK_NUMPAD_INS => "Num Ins",
            VK_NUMPAD_END => "Num End",
            VK_NUMPAD_DOWN => "Num Down",
            VK_NUMPAD_PGDN => "Num PgDn",
            VK_NUMPAD_LEFT => "Num Left",
            VK_NUMPAD_CLEAR => "Num Clear",
            VK_NUMPAD_RIGHT => "Num Right",
            VK_NUMPAD_HOME => "Num Home",
            VK_NUMPAD_UP => "Num Up",
            VK_NUMPAD_PGUP => "Num PgUp",
            VK_NUMPAD_DEL => "Num Del",
            _ => return None,
        }
        .to_string(),
    )
}

fn key_text(key: u32) -> Option<String> {
    if let Some(name) = mouse_key_text(key) {
        return Some(name);
    }
    if let Some(name) = numpad_nav_text(key) {
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

    /// 动作名要能双向对上：写进配置的是它，设置页里那个勾也按它查。
    #[test]
    fn action_keys_round_trip() {
        let mut seen = std::collections::BTreeSet::new();
        for action in Action::all() {
            let key = action.key();
            assert!(seen.insert(key), "动作名不能重复：{key}");
            assert_eq!(Action::from_key(key), Some(action), "{key} 要能认回来");
        }
        assert_eq!(Action::from_key("nonesuch"), None);
        // 默认勾选的那十个都得是真实动作（写错一个就静默失效）。
        for name in crate::config::DEFAULT_REPEAT_ACTIONS {
            assert!(Action::from_key(name).is_some(), "默认动作名写错了：{name}");
        }
    }

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

    /// 拦不拦这颗系统键，只看「屏幕提示」里对应那一项 —— 没有第二套开关。
    #[test]
    fn takeover_follows_osd_switches() {
        let mut osd = OsdConfig::default();
        set_takeover(&osd);
        for action in [
            Action::VolumeUp,
            Action::VolumeDown,
            Action::MediaMute,
            Action::MediaPrev,
            Action::MediaNext,
            Action::MediaPlayPause,
            Action::MediaStop,
        ] {
            assert!(takeover(action), "默认全开时 {action:?} 该被接管");
        }
        // 不是系统媒体 / 音量键的动作永远不归这条路管。
        assert!(!takeover(Action::Flyout));
        assert!(!takeover(Action::BrightnessUp));

        // 总开关关掉：一颗都不拦，原样还给系统。
        osd.enabled = false;
        set_takeover(&osd);
        assert!(!takeover(Action::VolumeUp));
        assert!(!takeover(Action::MediaPlayPause));

        // 总开关开着、只关「静音」：只放过静音键，别的照旧。
        let mut osd = OsdConfig::default();
        osd.mute = false;
        set_takeover(&osd);
        assert!(!takeover(Action::MediaMute));
        assert!(takeover(Action::VolumeUp));
        assert!(takeover(Action::MediaNext));

        // 测完把位图恢复成「全关」，免得影响同一进程里别的用例。
        set_takeover(&OsdConfig {
            enabled: false,
            ..OsdConfig::default()
        });
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

    /// 「这个键不连发」的勾要真管住系统的长按重复：总开关关着时也不能让它往下推。
    #[test]
    fn repeat_is_swallowed_for_actions_without_the_tick() {
        let previous_ignore = (
            ignore_system_repeat_for(Some(Action::VolumeUp)),
            ignore_system_repeat_for(Some(Action::BrightnessUp)),
        );
        let previous_list = repeat_actions()
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .clone();
        *repeat_actions()
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = vec![
            Action::VolumeUp as u32,
            Action::BrightnessUp as u32,
        ];

        set_ignore_system_repeat(false, false);
        assert!(
            !swallow_system_repeat(false, Some(Action::VolumeUp)),
            "第一发按下要放行"
        );
        assert!(
            !swallow_system_repeat(true, Some(Action::VolumeUp)),
            "勾了的重复交给自己的连发循环"
        );
        assert!(
            swallow_system_repeat(true, Some(Action::MediaNext)),
            "没勾的重复要当场吞掉，按一下是一下"
        );
        assert!(!swallow_system_repeat(true, None), "认不出的不额外吞");

        set_ignore_system_repeat(true, false);
        assert!(
            swallow_system_repeat(true, Some(Action::VolumeUp)),
            "音频页那一项开着就一律吞"
        );
        assert!(
            !swallow_system_repeat(true, Some(Action::BrightnessUp)),
            "屏幕页那份还是关的 —— 两个页签互不影响"
        );

        set_ignore_system_repeat(previous_ignore.0, previous_ignore.1);
        *repeat_actions()
            .lock()
            .unwrap_or_else(|error| error.into_inner()) = previous_list;
    }

    /// 默认勾选：音量四个 + 亮度六个，切歌 / 静音 / 播放暂停 / 面板 / 设置都不连发。
    #[test]
    fn hold_repeat_defaults_to_volume_and_brightness() {
        let config = Config::default();
        for action in Action::all() {
            let expected = matches!(
                action,
                Action::VolumeUp
                    | Action::VolumeDown
                    | Action::AppVolumeUp
                    | Action::AppVolumeDown
                    | Action::BrightnessUp
                    | Action::BrightnessDown
                    | Action::BrightnessAllUp
                    | Action::BrightnessAllDown
                    | Action::BrightnessMonitorUp
                    | Action::BrightnessMonitorDown
            );
            assert_eq!(repeats_on_hold(&config, action), expected, "{action:?}");
        }
    }

    /// 设置页里那个勾：勾上就参与，去掉就退出；总闸关掉时一律不连发。
    #[test]
    fn hold_repeat_follows_the_per_key_ticks() {
        let mut config = Config::default();
        // 勾上切歌
        config.hotkey_repeat_actions.push("media_next".to_string());
        assert!(repeats_on_hold(&config, Action::MediaNext));
        // 去掉音量
        config.hotkey_repeat_actions.retain(|name| name != "volume_up");
        assert!(!repeats_on_hold(&config, Action::VolumeUp));
        // 总闸
        config.hotkey_hold_repeat = false;
        assert!(!repeats_on_hold(&config, Action::MediaNext));
    }

    /// 总闸只管「现在生效不生效」，不许动每个人挑过的键。
    ///
    /// 界面上的勾选框是照这份名单画的：关掉总闸再打开，原来勾的必须还在
    /// （用户：「开启关闭都保存勾选状态不要丢」）。勾选框显示条件与
    /// `repeats_on_hold` 是两件事 —— 后者含总闸，前者不能含。
    #[test]
    fn hold_repeat_switch_keeps_the_ticked_keys() {
        let mut config = Config::default();
        let before = config.hotkey_repeat_actions.clone();
        assert!(!before.is_empty(), "默认就该勾着音量 / 亮度");

        // 两个页签各有一个总闸：关掉音频页的，屏幕页照旧连发。
        config.hotkey_hold_repeat = false;
        assert_eq!(config.hotkey_repeat_actions, before, "关总闸抹掉了名单");
        assert!(
            !repeats_on_hold(&config, Action::VolumeUp),
            "音频页总闸关着，音量不该连发"
        );
        assert!(
            repeats_on_hold(&config, Action::BrightnessUp),
            "屏幕页那一份还开着 —— 两个页签互不影响"
        );

        config.hotkey_screen_hold_repeat = false;
        assert!(
            Action::all().into_iter().all(|a| !repeats_on_hold(&config, a)),
            "两份总闸都关掉才一个都不连发"
        );

        config.hotkey_hold_repeat = true;
        config.hotkey_screen_hold_repeat = true;
        assert_eq!(config.hotkey_repeat_actions, before, "开总闸动了名单");
        assert!(repeats_on_hold(&config, Action::VolumeUp));
        assert!(repeats_on_hold(&config, Action::BrightnessUp));
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

    /// 小键盘在 NumLock 关着时冒充的方向键，要按「小键盘那颗」记，不能记成真方向键，
    /// 也不能记成 NumLock 开着时的数字。
    ///
    /// 三颗键的键码在这一刻是这样的：
    /// - 真方向键 `VK_LEFT` + 扩展位；
    /// - 小键盘 4（NumLock 关）`VK_LEFT` + 扫描码 0x4B，没有扩展位；
    /// - 小键盘 4（NumLock 开）`VK_NUMPAD4`。
    #[test]
    fn numpad_arrows_get_their_own_identity() {
        let info = |vk: u32, scan: u32, extended: bool| {
            let mut info = KBDLLHOOKSTRUCT::default();
            info.vkCode = vk;
            info.scanCode = scan;
            if extended {
                info.flags = LLKHF_EXTENDED;
            }
            info
        };
        // NumLock 关着：11 颗全认一遍（含最容易混的 Del / Clear）。
        assert_eq!(key_identity(&info(VK_LEFT, 0x4B, false)), Some(VK_NUMPAD_LEFT));
        assert_eq!(key_identity(&info(VK_CLEAR, 0x4C, false)), Some(VK_NUMPAD_CLEAR));
        assert_eq!(key_identity(&info(VK_INSERT, 0x52, false)), Some(VK_NUMPAD_INS));
        assert_eq!(key_identity(&info(VK_DELETE, 0x53, false)), Some(VK_NUMPAD_DEL));
        assert_eq!(key_identity(&info(VK_END, 0x4F, false)), Some(VK_NUMPAD_END));
        assert_eq!(key_identity(&info(VK_PRIOR, 0x49, false)), Some(VK_NUMPAD_PGUP));
        assert_eq!(key_identity(&info(VK_HOME, 0x47, false)), Some(VK_NUMPAD_HOME));
        assert_eq!(key_identity(&info(VK_DOWN, 0x50, false)), Some(VK_NUMPAD_DOWN));
        assert_eq!(key_identity(&info(VK_NEXT, 0x51, false)), Some(VK_NUMPAD_PGDN));
        assert_eq!(key_identity(&info(VK_RIGHT, 0x4D, false)), Some(VK_NUMPAD_RIGHT));
        assert_eq!(key_identity(&info(VK_UP, 0x48, false)), Some(VK_NUMPAD_UP));
        // 真方向键 / Del 带扩展位 → 不认，照原键码走。
        assert_eq!(key_identity(&info(VK_LEFT, 0x4B, true)), None);
        assert_eq!(key_identity(&info(VK_DELETE, 0x53, true)), None);
        assert_eq!(key_identity(&info(VK_UP, 0x48, true)), None);
        // 字母、主键盘数字压根不是这一组。
        assert_eq!(key_identity(&info(0x41, 0x1E, false)), None);
        assert_eq!(key_identity(&info(0x30, 0x0B, false)), None);
        // NumLock 开着：键码本身就是小键盘那一套（数字），原样返回。
        assert_eq!(key_identity(&info(0x64, 0x4B, false)), Some(0x64));
        assert_eq!(key_identity(&info(0x6E, 0x53, false)), Some(0x6E));
    }

    /// 三种「4」互不相干，界面上也写得清楚。
    #[test]
    fn the_three_fours_stay_apart() {
        assert_ne!(VK_LEFT, VK_NUMPAD_LEFT);
        assert_ne!(VK_NUMPAD_LEFT, 0x64);
        assert_eq!(key_text(VK_LEFT).as_deref(), Some("Left"));
        assert_eq!(key_text(VK_NUMPAD_LEFT).as_deref(), Some("Num Left"));
        assert_eq!(key_text(0x64).as_deref(), Some("Num4"));
        assert_eq!(key_text(VK_NUMPAD_DEL).as_deref(), Some("Num Del"));
        assert_eq!(key_text(VK_NUMPAD_PGUP).as_deref(), Some("Num PgUp"));
        assert_eq!(key_text(VK_NUMPAD_CLEAR).as_deref(), Some("Num Clear"));
        // 伪键码也走钩子、也进按下表（连发要问得到「还按着」）。
        assert!(is_hook_key(VK_NUMPAD_LEFT));
        forget_key_down(VK_NUMPAD_LEFT);
        assert!(!is_key_held(VK_NUMPAD_LEFT));
        assert!(!mark_key_down(VK_NUMPAD_LEFT), "第一次按下");
        assert!(is_key_held(VK_NUMPAD_LEFT));
        forget_key_down(VK_NUMPAD_LEFT);
        assert!(!is_key_held(VK_NUMPAD_LEFT));
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

    /// 方向 / Ins / Del 那一组也走钩子：`RegisterHotKey` 只认键码，分不出
    /// 小键盘冒充的那一颗，注册了就会被它蹭响。
    #[test]
    fn navigation_keys_are_routed_to_the_hook() {
        assert!(is_hook_key(VK_LEFT));
        assert!(is_hook_key(VK_UP));
        assert!(is_hook_key(VK_PRIOR));
        assert!(is_hook_key(VK_DELETE));
        assert!(is_nav_key(VK_CLEAR));
        // 小键盘也一律走钩子：NumLock 关着时它冒充的就是上面那一组键码，
        // `RegisterHotKey` 分不开，注册了 `Left` 会被 `Num4` 蹭响。
        assert!(is_hook_key(0x60));
        assert!(is_hook_key(0x64));
        assert!(is_hook_key(0x6E));
        // 字母 / 主键盘数字不该被顺带收进来。
        assert!(!is_nav_key(0x41));
        assert!(!is_nav_key(0x30));
        assert!(!is_hook_key(0x41));
        assert!(!is_hook_key(0x30));
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

    /// 「按下快捷键时不发送原始按键」：打开后普通键也归钩子管。
    ///
    /// 平时字母键根本进不了钩子（`CallNextHookEx` 放行），只有这一项开着、
    /// 而且正好是配过的组合时才被吞掉 —— 范围由 `hook_covers` 决定。
    #[test]
    fn swallow_raw_extends_the_hook_to_plain_keys() {
        set_swallow_raw(false, false);
        assert!(!hook_covers(0x41), "默认不管字母键");
        assert!(hook_covers(VK_LEFT), "方向键本来就走钩子");
        assert!(hook_covers(VK_VOLUME_UP));
        set_swallow_raw(true, false);
        assert!(hook_covers(0x41), "开着时字母键也要进钩子");
        assert!(hook_covers(0x70), "F1 同理");
        assert!(
            swallow_raw_for(Some(Action::VolumeUp)),
            "音频页那份开着"
        );
        assert!(
            !swallow_raw_for(Some(Action::BrightnessUp)),
            "屏幕页那份还是关的"
        );
        set_swallow_raw(false, false);
        assert!(!hook_covers(0x41), "关掉之后恢复原状");
        // 只有屏幕页开着：普通键一样要进钩子（`hook_covers` 分不出页签），
        // 进钩子之后按命中的动作决定吞不吞。
        set_swallow_raw(false, true);
        assert!(hook_covers(0x41));
        assert!(swallow_raw_for(Some(Action::BrightnessUp)));
        assert!(!swallow_raw_for(Some(Action::VolumeUp)));
        set_swallow_raw(false, false);
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

    /// 「不发送原始按键」关着时，方向键 / 小键盘这些走钩子的键必须原样还给系统，
    /// 只有被接管的音量 / 媒体键例外（放出去会多动一次）。
    #[test]
    fn taken_over_is_only_the_media_and_volume_keys() {
        for key in [VK_LEFT, VK_UP, VK_RIGHT, VK_DOWN, VK_DELETE, VK_HOME, 0x64, 0x41] {
            assert!(!taken_over(key), "0x{key:02X} 关着开关时要放出去");
        }
        for key in [
            VK_VOLUME_UP,
            VK_VOLUME_DOWN,
            VK_VOLUME_MUTE,
            VK_MEDIA_PLAY_PAUSE,
            VK_MEDIA_NEXT_TRACK,
        ] {
            assert!(taken_over(key), "0x{key:02X} 是接管的系统键，不能放");
        }
    }
}
