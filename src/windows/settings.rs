//! 设置窗口：设备选择 + 对齐 EarTrumpet 的设置项
//! （常规 / 应用列表 / 音量 / 鼠标 / 外观 / 关于）。

use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::Arc,
};

use gpui::prelude::*;
use gpui::{
    Animation, AnimationExt, AnyElement, App, Bounds, ClickEvent, Context, ElementId, FocusHandle,
    IntoElement, KeyDownEvent, Modifiers, MouseButton, Pixels, Render, RenderImage, SharedString,
    Window, div, img, px, relative, rgba,
};

use crate::{
    audio::{AudioCommand, AudioEngine, DeviceInfo, Snapshot},
    config::{
        ActivityEffect, AppBlockEntry, AppBlockKind, BrightnessMergeSide, BrightnessPlacement,
        Config, ConfigStore,
        DisplayNameMode, FooterSide, Hotkey, HotkeySlots, MAX_TOOLTIP_LINE_CHARS, MAX_TOOLTIP_LINES,
        OsdAnchor, OsdAnimStyle, OsdConfig, OsdFilterMode, OsdFunction, OsdPart, PanelLayout,
        PopupAnimStyle, TrayIconStyle, TrayMotion,
    },
    display::DisplayInfo,
    i18n::Lang,
    platform::hotkey::{MOD_ALT, MOD_CONTROL, MOD_SHIFT, MOD_WIN},
    tray::render_tooltip,
    ui::{
        DEVICE_ICON_CHOICES, Picker, ScrollDragState, Scrollbar,
        color::{hex_of, hsv_to_rgb, parse_rgb, push_hex, rgb_to_hsv},
        device_icon_resolved,
        footer_preview::FooterEdit,
        icon, theme,
    },
    windows::osd::{OsdState, OsdTarget},
};

/// 每个动作可以录几组快捷键（配置与界面都按这个数来）。
const HOTKEY_SLOTS: usize = 3;

/// 项目地址：写在设置页「关于」里，点一下用默认浏览器打开。
///
/// 只认这一处，改名换地址只改这里（README 里也是同一个）。
const PROJECT_URL: &str = "https://github.com/wusumu/Bolume";
/// 问题反馈（GitHub Issues）。
const ISSUES_URL: &str = "https://github.com/wusumu/Bolume/issues";

/// 屏幕取色的轮询间隔（和 OSD 编辑器保持一致）。
const COLOR_PICK_TICK: std::time::Duration = std::time::Duration::from_millis(20);

/// 「点别处」把改名落盘之后，这一下点击的**后续动作**还要不要算数。
///
/// 设备行是「点一下切成默认设备」，而落盘发生在按下阶段、`click` 在松开之后，
/// 中间隔着几十毫秒。这段时间内的点击视作「只是来收尾改名的」。
const RENAME_CLICK_GRACE: std::time::Duration = std::time::Duration::from_millis(400);

/// 左侧导航的宽度与按钮尺寸（照 QcClip 的设置页侧栏）。
const SIDEBAR_WIDTH: f32 = 168.0;
const NAV_HEIGHT: f32 = 32.0;
const NAV_GAP: f32 = 3.0;

/// 滚动条：滑块 6px 宽，外面留 12px 的槽（不压内容）。
const SCROLLBAR_WIDTH: f32 = 6.0;
const SCROLLBAR_GUTTER: f32 = 12.0;

/// 数字字体弹出层的尺寸（固定高度：字体族多到几百个，不固定会顶出屏幕）。
const FONT_MENU_WIDTH: f32 = 260.0;
const FONT_MENU_HEIGHT: f32 = 320.0;

/// 设备下拉的尺寸。设备名字长，宽度给足；台数不多，高度只是封顶。
const DEVICE_MENU_WIDTH: f32 = 300.0;
const DEVICE_MENU_HEIGHT: f32 = 280.0;

/// 设置页：每个大功能单独一页，左栏切换。
///
/// 顺序就是左栏从上到下的顺序，也是「用得多的在上面」这个顺序：
/// 常规（含开机自启与启动时套用的状态）→ 设备（页签里还有显示器）
/// → 外观 / 音量 / 面板 / 托盘 → 快捷键 / 屏幕提示 / 黑名单 / 关于。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
enum Page {
    #[default]
    General,
    /// 设备：播放 / 录制设备列表；「显示器」（亮度）放在这一页的第二个页签里。
    Devices,
    Appearance,
    /// 音量：页签里还有「响度均衡」。
    Volume,
    /// 面板：音量面板的布局 / 尺寸 / 动画 / 页脚，以及自动隐藏和列表行为。
    Panel,
    /// 托盘：图标样式、右键菜单、悬浮提示。
    Tray,
    Shortcuts,
    Osd,
    /// 黑名单：哪些应用在前台时整个 Bolume 让路。
    Blocklist,
    About,
}

const PAGE_COUNT: usize = 10;

/// 解除静音矩阵里的一格：开关 id、当前值、点一下怎么改。
type UnmuteCell = (&'static str, bool, fn(&mut Config));

impl Page {
    const ALL: [Page; PAGE_COUNT] = [
        Page::General,
        Page::Devices,
        Page::Appearance,
        Page::Volume,
        Page::Panel,
        Page::Tray,
        Page::Shortcuts,
        Page::Osd,
        Page::Blocklist,
        Page::About,
    ];

    fn index(self) -> usize {
        Self::ALL.iter().position(|page| *page == self).unwrap_or(0)
    }

    fn id(self) -> &'static str {
        match self {
            Self::General => "general",
            Self::Devices => "devices",
            Self::Appearance => "appearance",
            Self::Volume => "volume",
            Self::Panel => "panel",
            Self::Tray => "tray",
            Self::Shortcuts => "shortcuts",
            Self::Osd => "osd",
            Self::Blocklist => "blocklist",
            Self::About => "about",
        }
    }

    fn icon(self) -> &'static str {
        match self {
            Self::General => "settings",
            Self::Devices => "headphones",
            Self::Appearance => "palette",
            Self::Volume => "volume-high",
            // 面板 = 从屏幕底边弹出来的那块卡片；托盘 = 任务栏上那个小方图标。
            // 别再拿音量条 / 喇叭顶替：左侧导航里「音量」「面板」「托盘」三页
            // 各是各的事，图标重复了分不清点的是哪一页。
            Self::Panel => "panel",
            Self::Tray => "tray",
            Self::Shortcuts => "keyboard",
            Self::Osd => "monitor",
            Self::Blocklist => "ban",
            Self::About => "info",
        }
    }

    fn title(self, lang: Lang) -> &'static str {
        match self {
            Self::General => lang.pick("常规", "General"),
            Self::Devices => lang.pick("设备", "Devices"),
            Self::Appearance => lang.pick("外观", "Appearance"),
            Self::Volume => lang.pick("音量", "Volume"),
            Self::Panel => lang.pick("面板", "Panel"),
            Self::Tray => lang.pick("托盘", "Tray"),
            Self::Shortcuts => lang.pick("快捷键", "Shortcuts"),
            Self::Osd => lang.pick("屏幕提示", "On-screen"),
            Self::Blocklist => lang.pick("黑名单", "Blocklist"),
            Self::About => lang.pick("关于", "About"),
        }
    }
}

/// 每页管哪些顶层配置字段（`Config` 的 serde 字段名）。
///
/// 「恢复本页更改 / 本页恢复出厂设置」按这张表动配置：只碰本页能改到的字段，
/// 别的页一个字都不动。切分照的是设置页的实际归属 ——
///
/// - 「常规」页除了自启 / 管理员 / 语言，还带着「启动时套用」那两张卡（`startup`）；
/// - 「设备」页的两个页签是设备名单与显示器亮度（`brightness`）；
/// - 外观 / 面板 / 托盘三组本来是同一批卡片拆出来的，这儿的归属照那三组划分；
/// - 「关于」页没有自己的设置项，清单是空的。
///
/// 这里故意不用 `#[serde(flatten)]` 之类的结构化写法：一张平铺的表最容易看懂，
/// 加字段时也只要在对应那一行补一个名字（测试会盯着「有没有名字没被任何一页收」）。
fn page_fields(page: Page) -> &'static [&'static str] {
    match page {
        Page::General => &[
            "language",
            "launch_at_login",
            "launch_method",
            "run_as_admin",
            "startup",
        ],
        Page::Devices => &["device_names", "display_names", "device_icons", "brightness"],
        Page::Appearance => &[
            "theme_follow_system",
            "theme_light_surface",
            "theme_light_accent",
            "theme_dark_surface",
            "theme_dark_accent",
            "theme_surface",
            "theme_accent",
            "ui_font_scale",
            "ui_opacity",
            "ui_font",
            "ui_font_weight",
            "ui_contrast",
            "ui_motion",
        ],
        Page::Volume => &[
            "use_logarithmic_volume",
            "logarithmic_min_db",
            "use_scroll_wheel_in_tray",
            "use_global_mouse_wheel_hook",
            "use_scroll_wheel_in_apps",
            "scroll_wheel_step",
            "scroll_wheel_in_db",
            "log_scroll_wheel_step",
            "release_sound",
            "release_sound_osd",
            "volume_cap",
            "unmute",
            "volume_sound",
            "loudness",
            "app_loudness",
        ],
        Page::Panel => &[
            "hide_on_focus_lost",
            "show_system_sounds",
            "sounding_apps_first",
            "app_sort",
            "app_names",
            "hidden_apps",
            "flyout_width",
            "panel_bar_thickness",
            "panel_bar_round",
            "panel_layout",
            "vertical_app_names",
            "vertical_height",
            "panel_font",
            "panel_font_size",
            "panel_letter_spacing",
            "panel_line_height",
            "panel_font_weight",
            "device_name_size",
            "app_icon_size",
            "app_name_capitalize",
            "activity_effect",
            "popup_animation",
            "popup_animation_ms",
            "popup_anim_style",
            "panel_close_animation",
            "footer",
        ],
        Page::Tray => &[
            "tray_icon_style",
            "tray_icon_color",
            "tray_icon_custom",
            "tray_color_by_level",
            "tray_motion",
            "tray_motion_gain",
            "tray_number_font",
            "tray_number_antialias",
            // `tray_number_size` 不在清单里：它是 rev 67 之前的旧字段，只剩迁移读它，
            // 而且 `skip_serializing` 让它在 JSON 里根本不存在（搬不过去）。
            "tray_digit_one",
            "tray_digit_two",
            "tray_digit_three",
            "tray_menu",
            "tray_menu_style",
            "tray_menu_max_width",
            "tooltip",
        ],
        Page::Shortcuts => &[
            "hotkey_flyout",
            "hotkey_settings",
            "hotkey_volume_up",
            "hotkey_volume_down",
            "hotkey_app_volume_up",
            "hotkey_app_volume_down",
            "hotkey_volume_step",
            "hotkey_volume_follow_log",
            "hotkey_volume_in_db",
            "hotkey_app_volume_step",
            "hotkey_app_volume_follow_log",
            "hotkey_app_volume_in_db",
            "hotkey_media_prev",
            "hotkey_media_next",
            "hotkey_media_play_pause",
            "hotkey_media_mute",
            "hotkey_media_stop",
            "hotkey_brightness_up",
            "hotkey_brightness_down",
            "hotkey_brightness_all_up",
            "hotkey_brightness_all_down",
            "hotkey_brightness_monitor_up",
            "hotkey_brightness_monitor_down",
            "hotkey_hold_repeat",
            "hotkey_repeat_interval",
            "hotkey_repeat_delay",
            "hotkey_repeat_actions",
            "hotkey_ignore_system_repeat",
            "hotkey_numpad_raw",
        ],
        Page::Osd => &["osd", "editor_bg", "editor_panel_w"],
        Page::Blocklist => &["app_block"],
        Page::About => &[],
    }
}

/// 任何一页都不该碰的字段：窗口尺寸是「上次开多大」，跟着重置会把窗口缩掉；
/// `config_rev` 是配置结构自己的版本，重置后还得是当前这一版。
const UNTOUCHED_FIELDS: [&str; 3] = ["settings_width", "settings_height", "config_rev"];

/// 关窗确认框里按了哪一颗。
#[derive(Clone, Copy, PartialEq, Eq)]
enum CloseAnswer {
    Save,
    Discard,
    Cancel,
}

/// 菜单里的「恢复」那一条：恢复出厂还是恢复更改、只动本页还是整份。
#[derive(Clone, Copy, PartialEq, Eq)]
struct Restore {
    /// `true` = 恢复出厂默认，`false` = 退回到打开设置窗时的样子。
    factory: bool,
    /// `true` = 整份配置，`false` = 只动当前这一页。
    whole: bool,
}

pub struct Settings {
    config: Rc<RefCell<ConfigStore>>,
    config_path: SharedString,
    snapshot: Rc<RefCell<Snapshot>>,
    engine: Rc<AudioEngine>,
    /// 当前在哪一页（左侧导航）。
    page: Page,
    /// 每页一个滚动句柄：切回来时还停在原来的位置。
    scrolls: [gpui::ScrollHandle; PAGE_COUNT],
    /// 上一次记下的窗口尺寸（用来判断要不要落盘）。
    last_size: std::cell::Cell<(f32, f32)>,
    /// 尺寸落盘的世代号：拖动过程中只写最后一次。
    size_generation: Rc<std::cell::Cell<u64>>,
    /// 录快捷键用的焦点：键盘事件只在焦点元素上派发。
    focus: FocusHandle,
    /// 正在录哪个快捷键（`None` = 没在录）：动作 + 第几组。
    capturing: Option<(HotkeySlot, usize)>,
    /// 刚用 × 清空过哪一格、什么时候。
    ///
    /// 格子自己挂着「点一下开始录」，× 是叠在它上面的子元素：按下时已经
    /// `stop_propagation` 了，但 gpui 的 click 是另一条链路，万一还是冒上来，
    /// 清完立刻又被打开录制，看着就像「没清掉」。用 400ms 的时间窗兜一下。
    just_cleared: Option<(HotkeySlot, usize, std::time::Instant)>,
    /// 光标正停在哪一行的「连续触发」勾选框上（行 id）。
    ///
    /// 方框只有 14px、旁边又没字，得靠悬浮气泡说清它管什么。气泡是行里
    /// **最后**一个子元素 —— 放前面会被后面的标题文字盖住（gpui 没有 z-index，
    /// 后画的在上），越出行边界的又会被下一行或卡片的 `overflow_hidden` 吃掉。
    /// 所以只能靠这个状态决定「这一行要不要多画一个气泡」。
    repeat_tip: Option<&'static str>,
    /// 快捷键页当前在哪一页签：0 = 音频设备，1 = 屏幕设备。
    ///
    /// 只是界面状态，不落盘：下次打开回到第一页，免得一进来就是空的第二页。
    shortcut_tab: usize,
    /// 设备页当前在哪一页签：0 = 设备，1 = 显示器（亮度）。
    ///
    /// 和快捷键页同一套：只是界面状态，不落盘。
    device_tab: usize,
    /// 音量页当前在哪一页签：0 = 音量，1 = 响度均衡。
    volume_tab: usize,
    /// 页面切换的序号：**每次换页 / 换页签都 +1**。
    ///
    /// 它被拼进内容的动画 id 里，于是每次切换都是一个「全新的元素」，
    /// 淡入从 0 重新开始。不这么写的话，gpui 按 id 记动画状态，
    /// 切回上一页会命中已经播完的那份状态，动画就一次都不会再放。
    page_anim: u64,
    /// 正在手动输入哪个数值（字段 + 已输入的字符）。
    editing: Option<(NumberField, String)>,
    /// 字体弹出层：`Some((改哪个字体, x, y))` = 打开着，坐标是点击处的窗口坐标。
    /// 用绝对定位浮在根节点上，才不会被滚动区剪掉。
    font_menu: Option<(FontTarget, f32, f32)>,
    /// 字体列表自己的滚动位置（弹出层每次打开都重置）。
    font_scroll: gpui::ScrollHandle,
    /// 字体弹层里的搜索词（空 = 不过滤；打开时清空）。
    font_filter: String,
    /// 弹层刚打开时要滚到第几行（当前选中的那个字体），滚过一次就清掉。
    font_pending_scroll: Option<usize>,
    /// 设备下拉：`Some((哪一行, x, y))`，坐标同样是点击处的窗口坐标。
    picker: Option<(DevicePicker, f32, f32)>,
    /// 设备图标选择器：`Some((设备 id, x, y))` = 打开着，坐标是点击处的窗口坐标。
    ///
    /// 单独一个浮层而不是塞进 `picker`：那边列的是设备，这边列的是图标，
    /// 布局和行高都不一样。
    icon_picker: Option<(String, f32, f32)>,
    /// 设备列表自己的滚动位置。
    picker_scroll: gpui::ScrollHandle,
    /// 滚动条的拖动状态：页面一根、字体列表一根，共用一个状态（同一时刻只拖一根）。
    scroll_drag: ScrollDragState,
    /// 正在编辑托盘提示的哪一份内容（正常 / 静音），以及文本和光标。
    tooltip_edit: Option<TooltipEditor>,
    /// 页脚可视化编辑器的拖拽 / 悬停 / 量到的矩形。
    ///
    /// 放在实体上而不是每次重画时新建：改一次配置整棵就重建，状态跟着重建
    /// 就等于「拖到一半东西没了」。
    footer_preview: crate::ui::footer_preview::FooterPreviewState,
    /// 「启动时设置应用音量」名单里某一行的就地编辑：(第几行, 已输入的字符)。
    app_edit: Option<(usize, String)>,
    /// 托盘菜单编辑器弹层是否打开。
    tray_menu_open: bool,
    /// 弹层自己的滚动位置。
    tray_menu_scroll: gpui::ScrollHandle,
    /// 正在拖的条目：`(所在子菜单, 下标)`，`None` 的那一半表示在右键根菜单里。
    tray_drag: Option<(Option<usize>, usize)>,
    /// 拖的是不是子菜单本身（子菜单只有一层，不能再塞进别的子菜单）。
    tray_drag_is_folder: bool,
    /// 拖动时当前悬停的落点（松手就搬到这里）。
    tray_drop: Option<TrayDrop>,
    /// 正在重命名的子菜单：(子菜单 id, 已输入的文字)。
    tray_rename: Option<(String, String)>,
    /// 正在改名的设备：(设备 id, 已输入的文字)。
    device_rename: Option<(String, String)>,
    /// 正在改名的显示器：(显示器 key, 已输入的文字)。
    ///
    /// 和音频设备同一套就地编辑（光标、点哪改哪、中文走输入法），只是存到
    /// `display_names` 而不是 `device_names`。
    display_rename: Option<(String, String)>,
    /// 设备改名时的光标（UTF-16 偏移，0..=长度）。
    ///
    /// 只有设备改名用它：点文字能把光标点到那一格，退格/Delete 也按它删，
    /// 其余几个就地输入框仍按「追加到末尾」处理。
    rename_cursor: usize,
    /// 设备改名输入区的矩形（由 canvas 每帧写回）：点击时按 x 反推光标落在第几个字。
    rename_bounds: Rc<Cell<Option<Bounds<Pixels>>>>,
    /// 上一次因「点别处」而落盘的时刻。
    ///
    /// 设备行本身是「点一下切成默认设备」，而点别处提交就发生在那一下的按下阶段。
    /// 刚提交完的这一下不该顺带把设备切走 —— 见 `device_row` 里的判断。
    rename_commit_at: Option<std::time::Instant>,
    /// 上一次点别处落盘的是哪一格（设备 id / 显示器 key）。
    ///
    /// 「点同一行的『改名』按钮」和「点别处」是同一个按下事件：捕获相位先把名字
    /// 存下去，按钮再看到一个干净的状态就会开一格空白的 —— 接着敲的字会把刚存
    /// 的名字整个替换掉。记着是谁刚存过，按钮这一下就别重开。
    rename_commit_key: Option<String>,
    /// Ctrl+Z 的撤销栈，所有就地输入框共用一份：(改之前的文字, 改之前的光标)。
    ///
    /// 同一时刻只会有一个输入框在编辑，所以一份就够；换了格子（见
    /// `text_undo_owner`）整栈清空 —— 否则会把上一格的内容退到这一格里。
    text_undo: Vec<(String, usize)>,
    /// 上面那份撤销栈属于哪一格（见 `undo_slot`）。
    text_undo_owner: Option<u8>,
    /// 就地编辑时输入法正在拼的那一段（UTF-16 下标，`None` = 没有在拼）。
    /// 设备改名和子菜单改名共用一份 —— 同一时刻只会有一个在编辑。
    text_marked: Option<std::ops::Range<usize>>,
    /// 正在就地编辑 OSD 文字：`(槽位, 缓冲)`，槽位 0 = 标题，1..=3 = 额外文字。
    osd_text_edit: Option<(u8, String)>,
    /// 「保持显示」是不是开着（设置页自己的状态，和按钮文案一致）。
    osd_pinned: bool,
    /// 标题栏按下了但还没开始拖：真正的拖动等鼠标动起来再交给系统。
    ///
    /// **不能在按下的瞬间就 `start_window_move()`** —— 那会立刻进系统的标题栏拖动
    /// 模态环，环里会把双击的第二次按下吞掉，双击就永远收不到 `click_count == 2`。
    titlebar_armed: bool,
    /// 关窗通知的订阅：设置窗关掉时把「保持显示」钉住的提示条一起收走。
    window_closed: Option<gpui::Subscription>,
    /// 正在「拖到目标窗口上」拾取应用：记着往哪块名单里放，松手后落名单。
    block_pick: Option<BlockList>,
    /// 黑名单页「手动输入 exe 名」的草稿（`Some` = 框里有内容）。
    ///
    /// 点别处只是**收起键盘**，草稿留在这儿 —— 只有回车或「添加」才真往名单里写。
    block_name_edit: Option<String>,
    /// 输入框是不是拿着键盘（点进框里才吃键；失焦后草稿还在，续敲要先点回来）。
    block_name_focus: bool,
    /// 「全屏例外」那份名单的手输草稿 / 焦点（和上面那对完全平行）。
    block_exempt_edit: Option<String>,
    block_exempt_focus: bool,
    /// 黑名单页的一条临时提示：(文字, 序号)，序号用来让旧定时器失效。
    block_notice: Option<(String, u64)>,
    /// 自选底色的取色器（`None` = 没打开）。
    color_wheel: Option<ColorWheel>,
    /// 每项颜色上一次调出来的自选色（点过预设色还能翻回来）。
    ///
    /// 只活在这一次设置窗里：它记的是「刚才在色轮里调成什么」，不是持久偏好，
    /// 没必要写进配置文件。
    color_memory: std::collections::HashMap<ColorTarget, String>,
    /// 捐赠浮层是不是开着（「关于」页那行按钮点开的收款码）。
    ///
    /// 只是界面状态，不落盘：关掉窗口再打开就该是收着的。
    donate_open: bool,
    /// 收款码解码后的图：解一次就留着，浮层重画不用反复解 PNG。
    donate_image: RefCell<Option<Arc<RenderImage>>>,
    /// 标题栏那个三横杠菜单是不是开着（恢复更改 / 恢复出厂设置都在里面）。
    restore_menu_open: bool,
    /// 「有未保存的更改，关掉就没了」那个确认框是不是开着。
    ///
    /// 设置页是草稿模式：改了只在内存里，按「应用 / 保存」才落盘。
    /// 所以点 × 时得先问一句，不然用户以为改好了、关掉才发现全丢了。
    confirm_close: bool,
}

/// 往黑名单里加一条的结果。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum BlockAdd {
    /// 加进去了。
    Added,
    /// 名单里已经有这一条（同类型 + 同路径）。
    Duplicate,
    /// 看不出来是什么程序（空串之类）。
    Invalid,
}

/// 黑名单页里两块名单：让路名单 / 全屏例外名单。
///
/// 两块的行、输入框、文件选择、移除按钮长得一模一样，只差往哪个 `Vec` 里写，
/// 所以都走同一套函数，靠这个枚举分派 —— 免得两份实现各自长歪。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum BlockList {
    /// 前台是它们就让路。
    Block,
    /// 全屏时也不让路（「全屏状态禁用」的例外）。
    Exempt,
}

/// 拖动条目的落点：插到某个容器（根菜单 / 第 i 个子菜单）的第 `index` 个位置。
///
/// 只区分「插在哪一格之间」，不放回哪个容器 —— 拖到根菜单某一行上就是回根菜单，
/// 拖进子菜单卡片里就是进子菜单，同一个模型就够用。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TrayDrop {
    Insert {
        /// `None` = 右键根菜单，`Some(i)` = 第 i 个子菜单。
        folder: Option<usize>,
        /// 插入位置（`len` 表示末尾）。
        index: usize,
    },
}

/// 设备下拉：选项来自运行时设备列表，铺成一排标签会把整页撑得很长。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum DevicePicker {
    /// 启动时切到的那台默认设备。
    StartupDefault,
    /// 最大音量限制绑定的设备。
    BoundDevice,
    /// 手动往「启动时套用设备开关名单」里加一台设备。
    AddDeviceState,
    /// 手动往「启动时设置应用音量」的名单里加一个应用。
    AddAppVolume,
    /// 屏幕提示「手动选择显示器」那行的下拉（列所有显示器）。
    OsdMonitor,
}

/// 托盘提示的编辑目标：正常状态 / 静音状态，各有一份模板。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum TooltipTarget {
    Normal,
    Muted,
}

/// 多行模板编辑器的状态。
///
/// 这里没有用系统输入框：本项目整套界面是自绘的，文本框也照同一套做法自己维护
/// 「行 + 光标」。行列都按**字符**算（不是字节），中文才不会切到半个字。
struct TooltipEditor {
    target: TooltipTarget,
    lines: Vec<String>,
    row: usize,
    /// 光标在这一行的第几个字符之后（0..=该行字符数）。
    col: usize,
    /// 选区的另一头（按下 / Shift+方向键定下的锚点）。和 `(row, col)` 不同时
    /// 就是有选区；松手后也不清 —— 用户还要接着删 / 输入替换。
    anchor: Option<(usize, usize)>,
    /// 这次左键是不是在编辑区里按下的。
    ///
    /// 光看「按键压着」不够：从别处开始拖（拖滚动条、拖窗口）路过编辑区时，
    /// 左键同样压着，选区会被一路带跑 —— 那正是「有时不太正常」的来源。
    drag: bool,
    /// Ctrl+Z 的撤销栈：每次改字前压一份「全部行 + 光标」。
    history: Vec<(Vec<String>, usize, usize)>,
}

impl TooltipEditor {
    fn new(target: TooltipTarget, lines: Vec<String>) -> Self {
        let mut lines = lines;
        if lines.is_empty() {
            lines.push(String::new());
        }
        let row = lines.len() - 1;
        let col = lines[row].chars().count();
        Self {
            target,
            lines,
            row,
            col,
            anchor: None,
            drag: false,
            history: Vec::new(),
        }
    }

    /// 改字之前记一步（内容没变就不重复压）。
    fn snapshot(&mut self) {
        let snap = (self.lines.clone(), self.row, self.col);
        if self.history.last() == Some(&snap) {
            return;
        }
        if self.history.len() >= 100 {
            self.history.remove(0);
        }
        self.history.push(snap);
    }

    /// Ctrl+Z：退回上一步（行、光标、选区一起还原）。
    fn undo(&mut self) -> bool {
        let Some((lines, row, col)) = self.history.pop() else {
            return false;
        };
        self.lines = lines;
        self.anchor = None;
        self.drag = false;
        let last = self.lines.len().saturating_sub(1);
        self.row = row.min(last);
        self.col = col.min(self.lines[self.row].chars().count());
        true
    }

    fn current(&self) -> &str {
        self.lines
            .get(self.row)
            .map(|line| line.as_str())
            .unwrap_or("")
    }

    /// 把光标挪到某个位置（越界就夹到最近的有效位置）。
    fn set_caret(&mut self, row: usize, col: usize) {
        self.row = row.min(self.lines.len().saturating_sub(1));
        self.col = col.min(self.current().chars().count());
    }

    /// 按下鼠标：光标落到这里，并把它当成选区锚点。
    ///
    /// `extend` 是 Shift+点击：锚点不动，只把另一头拖过去，接着扩选区。
    fn begin_selection(&mut self, row: usize, col: usize, extend: bool) {
        let anchor = if extend {
            self.anchor.or(Some((self.row, self.col)))
        } else {
            None
        };
        self.set_caret(row, col);
        self.anchor = anchor.or(Some((self.row, self.col)));
        // 从这一刻起，鼠标移动就是在拖选区。
        self.drag = true;
    }

    /// 按住左键拖过某个位置：只挪光标那一头。
    fn drag_to(&mut self, row: usize, col: usize) {
        self.set_caret(row, col);
    }

    /// 规范化的选区（前 → 后）；没有选中东西时返回 `None`。
    fn selection(&self) -> Option<((usize, usize), (usize, usize))> {
        let anchor = self.anchor?;
        let caret = (self.row, self.col);
        let (start, end) = if anchor <= caret {
            (anchor, caret)
        } else {
            (caret, anchor)
        };
        if start == end {
            None
        } else {
            Some((start, end))
        }
    }

    /// 有选区时按方向键：先收成一个点（`to_end` 决定收到哪一头）。
    /// 返回 true 表示这次按键只是把选区收掉了，光标别再动。
    fn collapse_selection(&mut self, to_end: bool) -> bool {
        let Some((start, end)) = self.selection() else {
            self.anchor = None;
            return false;
        };
        let (row, col) = if to_end { end } else { start };
        self.set_caret(row, col);
        self.anchor = None;
        true
    }

    /// Shift+方向键：没锚点就以当前光标为锚点，然后照常挪光标。
    fn extend_selection(&mut self) {
        if self.anchor.is_none() {
            self.anchor = Some((self.row, self.col));
        }
    }

    /// 删掉选区并把光标落到选区起点；返回删没删东西。
    fn delete_selection(&mut self) -> bool {
        let Some(((start_row, start_col), (end_row, end_col))) = self.selection() else {
            self.anchor = None;
            return false;
        };
        let head: String = self.lines[start_row].chars().take(start_col).collect();
        let tail: String = self.lines[end_row].chars().skip(end_col).collect();
        let merged = format!("{head}{tail}");
        self.lines
            .splice(start_row..=end_row, std::iter::once(merged));
        self.set_caret(start_row, start_col);
        self.anchor = None;
        true
    }

    /// 选区里的文字（Ctrl+C 用）。
    fn selected_text(&self) -> String {
        let Some(((start_row, start_col), (end_row, end_col))) = self.selection() else {
            return String::new();
        };
        if start_row == end_row {
            return self.lines[start_row]
                .chars()
                .skip(start_col)
                .take(end_col - start_col)
                .collect();
        }
        let mut parts: Vec<String> = Vec::new();
        parts.push(self.lines[start_row].chars().skip(start_col).collect());
        for row in start_row + 1..end_row {
            parts.push(self.lines[row].clone());
        }
        parts.push(self.lines[end_row].chars().take(end_col).collect());
        parts.join("\n")
    }

    /// 全选（Ctrl+A）。
    fn select_all(&mut self) {
        self.anchor = Some((0, 0));
        let last = self.lines.len().saturating_sub(1);
        self.set_caret(last, self.lines[last].chars().count());
    }

    /// 在当前光标处插入一段文字（变量、粘贴的内容）。
    fn insert(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        // 有选区：输入先顶替掉选中的内容。
        self.delete_selection();
        let line = self.lines.get(self.row).cloned().unwrap_or_default();
        let chars: Vec<char> = line.chars().collect();
        let col = self.col.min(chars.len());
        let before: String = chars[..col].iter().collect();
        let after: String = chars[col..].iter().collect();
        let room = MAX_TOOLTIP_LINE_CHARS.saturating_sub(chars.len());
        if room == 0 {
            return;
        }
        let mut inserted = String::new();
        for ch in text.chars().filter(|ch| !ch.is_control()) {
            if inserted.chars().count() >= room {
                break;
            }
            inserted.push(ch);
        }
        let merged = format!("{before}{inserted}{after}");
        self.col = col + inserted.chars().count();
        if let Some(slot) = self.lines.get_mut(self.row) {
            *slot = merged;
        }
    }

    fn insert_char(&mut self, ch: char) {
        if ch.is_control() {
            return;
        }
        let mut buffer = [0u8; 4];
        self.insert(ch.encode_utf8(&mut buffer));
    }

    /// 粘贴：换行拆成多行，超长由 `insert` / `split_line` 各自夹住。
    fn paste(&mut self, text: &str) {
        let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
        for (index, part) in normalized.split('\n').enumerate() {
            if index > 0 {
                self.split_line();
            }
            self.insert(part);
        }
    }

    /// 回车：在光标处拆行（到上限就不再拆，免得按下去没反应还错乱）。
    fn split_line(&mut self) {
        // 有选区就先把选中的删掉，再在删除处拆行。
        self.delete_selection();
        if self.lines.len() >= MAX_TOOLTIP_LINES {
            return;
        }
        let line = self.lines.get(self.row).cloned().unwrap_or_default();
        let chars: Vec<char> = line.chars().collect();
        let col = self.col.min(chars.len());
        let head: String = chars[..col].iter().collect();
        let tail: String = chars[col..].iter().collect();
        self.lines[self.row] = head;
        self.lines.insert(self.row + 1, tail);
        self.row += 1;
        self.col = 0;
    }

    /// 退格：行首时把这一行接回上一行尾部。
    fn backspace(&mut self) {
        // 有选区：一次删掉整块，别再往左啃一格。
        if self.delete_selection() {
            return;
        }
        let line = self.lines.get(self.row).cloned().unwrap_or_default();
        let chars: Vec<char> = line.chars().collect();
        if self.col > 0 {
            let col = self.col.min(chars.len());
            let mut merged: String = chars[..col - 1].iter().collect();
            merged.extend(chars[col..].iter());
            self.lines[self.row] = merged;
            self.col = col - 1;
            return;
        }
        if self.row == 0 {
            return;
        }
        let previous_len = self.lines[self.row - 1].chars().count();
        let tail = self.lines.remove(self.row);
        self.row -= 1;
        self.lines[self.row].push_str(&tail);
        self.col = previous_len;
    }

    /// Delete：行尾时把下一行接上来。
    fn delete_forward(&mut self) {
        if self.delete_selection() {
            return;
        }
        let line = self.lines.get(self.row).cloned().unwrap_or_default();
        let chars: Vec<char> = line.chars().collect();
        if self.col < chars.len() {
            let col = self.col;
            let mut merged: String = chars[..col].iter().collect();
            merged.extend(chars[col + 1..].iter());
            self.lines[self.row] = merged;
            return;
        }
        if self.row + 1 >= self.lines.len() {
            return;
        }
        let tail = self.lines.remove(self.row + 1);
        self.lines[self.row].push_str(&tail);
    }

    fn move_up(&mut self) {
        if self.row == 0 {
            return;
        }
        self.row -= 1;
        self.clamp_col();
    }

    fn move_down(&mut self) {
        if self.row + 1 >= self.lines.len() {
            return;
        }
        self.row += 1;
        self.clamp_col();
    }

    /// 左移一格，行首时退到上一行末尾。
    fn move_left(&mut self) {
        if self.col > 0 {
            self.col -= 1;
        } else if self.row > 0 {
            self.row -= 1;
            self.col = self.current().chars().count();
        }
    }

    /// 右移一格，行尾时进到下一行行首。
    fn move_right(&mut self) {
        let len = self.current().chars().count();
        if self.col < len {
            self.col += 1;
        } else if self.row + 1 < self.lines.len() {
            self.row += 1;
            self.col = 0;
        }
    }

    fn clamp_col(&mut self) {
        self.col = self.col.min(self.current().chars().count());
    }

    /// 提交前收拾一下：去掉末尾的空行（中间的空行渲染时也会被丢掉，
    /// 但留着一串空行会让下次进来光标停在看不见的地方）。
    fn trimmed(&self) -> Vec<String> {
        let mut lines = self.lines.clone();
        while lines.len() > 1 && lines.last().map(|line| line.trim().is_empty()) == Some(true) {
            lines.pop();
        }
        lines
    }
}

/// 某个状态下要编辑的行（配置里没写过就用当前语言的默认内容）。
fn tooltip_lines_for(config: &Config, target: TooltipTarget, lang: Lang) -> Vec<String> {
    let stored = match target {
        TooltipTarget::Normal => &config.tooltip.lines,
        TooltipTarget::Muted => &config.tooltip.muted_lines,
    };
    if !stored.is_empty() {
        return stored.clone();
    }
    default_tooltip_lines(lang, target)
}

/// 默认模板：第一行读数（静音那份显示状态词）、第二行当前播放设备。
///
/// 和自定义关闭时的内置内容对齐（`tray::default_template`），只是静音那份
/// 换成状态词 —— 独立的一套内容，更像「另一种状态」。
fn default_tooltip_lines(_lang: Lang, target: TooltipTarget) -> Vec<String> {
    let template = crate::tray::default_template();
    match target {
        TooltipTarget::Normal => template.iter().map(|line| line.to_string()).collect(),
        TooltipTarget::Muted => vec!["{name} {state}".to_string(), template[1].to_string()],
    }
}

/// 拖动中把光标那一头挪到 (row, col)。
///
/// 只认「在编辑区里按下左键」的那次拖动：
/// - 右键 / 中键拖着经过编辑区，不许改选区；
/// - 从别处（滚动条、窗口边框）拖过来时左键也压着，同样不许改 ——
///   以前就是靠「左键压着」判断，才会出现鼠标滑过就把选区带跑的怪现象；
/// - 松手事件万一来不及收到，下一次移动看到左键已经不压着就自动收尾。
fn drag_caret(
    this: &mut Settings,
    target: TooltipTarget,
    row: usize,
    col: usize,
    event: &gpui::MouseMoveEvent,
    window: &mut Window,
    cx: &mut Context<Settings>,
) {
    let Some(editor) = this.tooltip_edit.as_mut() else {
        return;
    };
    if editor.target != target {
        return;
    }
    if !matches!(event.pressed_button, Some(gpui::MouseButton::Left)) {
        editor.drag = false;
        return;
    }
    if !editor.drag {
        return;
    }
    // 命中区有重叠时（字块嵌在行里），先到的那层说了算，别让外层把端点拉回去。
    cx.stop_propagation();
    if (editor.row, editor.col) == (row, col) {
        return;
    }
    editor.drag_to(row, col);
    refresh_ui(window, cx);
}

/// 预览用的设备：优先当前默认设备，没有设备时编一台像样的出来，
/// 免得设置页在没插耳机 / 无音频设备时看不见预览效果。
fn preview_device(snapshot: &Snapshot, lang: Lang) -> DeviceInfo {
    snapshot
        .devices
        .iter()
        .find(|device| device.is_default)
        .or_else(|| snapshot.devices.first())
        .cloned()
        .unwrap_or_else(|| DeviceInfo {
            id: String::new(),
            name: lang.pick("扬声器", "Speakers").to_string(),
            description: String::new(),
            is_default: true,
            enabled: true,
            // 62% 是个能同时看出百分比和 dB 的中间值。
            volume: 0.62,
            muted: false,
            peak: 0.0,
            kind: crate::audio::DeviceKind::Speakers,
        })
}

/// OSD 预览里的示例内容：用固定的 42%，和实际音量无关，
/// 免得一调音量预览就跟着跳。
///
/// 内容照着**这套模板平时那一次操作**来：音量 / 媒体键 / 亮度各是什么字、有没有条，
/// 都和真机上一致（`function` 只影响这里的样例，不影响配置）。
fn preview_osd_state(lang: Lang, cfg: &OsdConfig, function: OsdFunction) -> OsdState {
    let mut state = OsdState {
        function,
        target: OsdTarget::Device,
        icon: cfg.icon_style().icon,
        title: String::new(),
        subtitle: String::new(),
        // 示例直接用最长的那一档：100% 能放下，42% 自然也没问题。
        percent: String::new(),
        db: String::new(),
        fraction: 0.42,
        show_bar: true,
        muted: false,
        playing: None,
    };
    match function {
        // 亮度只有百分比、没有分贝，图标固定是太阳。
        OsdFunction::Brightness => {
            state.icon = "sun";
            state.title = lang.pick("亮度", "Brightness").to_string();
            state.percent = cfg.percent_display("100%");
        }
        // 媒体键只报动作，不画条也不报读数。
        f if f.is_media() => {
            state.icon = "play";
            state.title = f.label(lang).to_string();
            state.fraction = 0.0;
            state.show_bar = false;
        }
        _ => {
            state.title = lang.pick("音量", "Volume").to_string();
            state.percent = cfg.percent_display("100%");
            state.db = "-12.0 dB".to_string();
        }
    }
    // 用户填了标题就盖掉自动内容，和真实提示条一致。
    if !cfg.title_text.trim().is_empty() {
        state.title = cfg.title_text.clone();
    }
    state
}

/// 预览下面那行小字：这是哪一套模板。
fn template_label(template: crate::config::OsdTemplate, lang: Lang) -> &'static str {
    match template {
        crate::config::OsdTemplate::Volume => lang.pick("音量", "Volume"),
        crate::config::OsdTemplate::Media => lang.pick("媒体键", "Media keys"),
        crate::config::OsdTemplate::Brightness => lang.pick("亮度", "Brightness"),
    }
}

/// 托盘数字某一档位数的某一项外观（PS 字符面板那四件套）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[allow(dead_code)]
enum TrayDigitMetric {
    /// 字号（点，0 = 自动）。
    SizePt,
    /// 水平缩放（%）。
    Width,
    /// 垂直缩放（%）。
    Height,
    /// 字距（1/1000 em，可为负）。
    Tracking,
}

/// 设置页里可以手动输入的数值字段。
///
/// 这些都是 uint 小数字（音量步进、托盘数字字号），没有自由文本输入控件
/// 也能做：点一下数字框就进入「打字」状态，按键由 `handle_edit_key` 接手。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
#[allow(dead_code)]
enum NumberField {
    /// 快捷键音量步进（百分比 / dB）。
    HotkeyStep,
    /// 百分比滚轮步进（线性刻度）。
    WheelStep,
    /// 对数滚轮步进，**单位 dB，内部按十分之一 dB 存**（`value = dB × 10`）。
    LogWheelStep,
    /// 托盘数字：第 `digits` 档（1 / 2 / 3 位）的某一项外观。
    TrayDigit(u8, TrayDigitMetric),
    /// 启动时设置的主音量（百分比）。
    StartupVolume,
    /// 设备最大音量上限（百分比）。
    VolumeCap,
    /// 快捷键步进，**单位 dB**（和 `HotkeyStep` 是同一个字段，只换显示单位）。
    HotkeyStepDb,
    /// 当前应用的快捷键音量步进（百分比）。
    AppHotkeyStep,
    /// 当前应用的快捷键音量步进（dB，和 `AppHotkeyStep` 同一个字段）。
    AppHotkeyStepDb,
    /// 显示器亮度一次 +/− 的步进（百分点）。
    BrightnessStep,
    /// 亮度滚轮步进（一格滚轮走多少个百分点）。
    BrightnessScrollStep,
    /// 亮度面板宽度（逻辑像素，0 = 跟随音量面板）。
    BrightnessWidth,
    /// 亮度面板竖向高度（逻辑像素，0 = 跟随音频面板）。
    BrightnessVerticalHeight,
    /// 音量面板宽度（逻辑像素）。
    FlyoutWidth,
    /// 音量面板竖向高度（逻辑像素，0 = 自动按内容算）。
    FlyoutVerticalHeight,
    /// 面板里音量条的粗细（逻辑像素）。
    PanelBarThickness,
    /// 面板里音量条的圆角（半高的百分比，100 = 全圆头）。
    PanelBarRound,
    /// OSD 卡片宽度（逻辑像素）。
    OsdWidth,
    /// OSD 卡片高度（逻辑像素）。
    OsdHeight,
    /// OSD 圆角（逻辑像素）。
    OsdCorner,
    /// OSD 不透明度（百分比）。
    OsdOpacity,
    /// OSD 指定坐标：相对显示器左上角的水平 / 垂直偏移。
    OsdX,
    OsdY,
    /// OSD 淡入 / 淡出 / 停留时长（毫秒）。
    OsdFadeIn,
    OsdFadeOut,
    OsdHold,
    /// OSD 里某个元素自己的尺寸（图标边长 / 字号 / 音量条厚度，逻辑像素）。
    OsdSize(OsdPart),
    /// 面板里设备名的字号（逻辑像素）。
    DeviceNameSize,
    /// 音量面板的字号（逻辑像素，内部按十分之一像素存：`value = px × 10`）。
    PanelFontSize,
    /// 音量面板的字间距（逻辑像素，内部按十分之一存；**唯一允许负数的字段**）。
    PanelLetterSpacing,
    /// 音量面板的行高（逻辑像素，0 = 跟着字号自动算）。
    PanelLineHeight,
    /// 面板弹出动画时长（毫秒）。
    PopupAnimation,
    /// 设置窗口的字号缩放（百分比）。
    UiFontScale,
    /// 界面不透明度（百分比）。
    UiOpacity,
    /// 界面对比度（百分比，100 = 主题原样）。
    UiContrast,
    /// 托盘右键菜单（自绘）的最大宽度（逻辑像素）。
    TrayMenuWidth,
    /// 动态图标的灵敏度（百分比）。
    TrayMotionGain,
    /// 按住连发的间隔（毫秒）。
    HotkeyRepeatInterval,
    /// 按住多久才开始连发（毫秒，0 = 不等）。
    HotkeyRepeatDelay,
    /// 「指定显示器」那组亮度快捷键调第几台（1 起）。
    BrightnessHotkeyMonitor,
    /// 亮度快捷键自己的步进（百分点，0 = 跟面板的「亮度步进」）。
    BrightnessHotkeyStep,
    /// 响度均衡：最大放大级别（百分比，1–100）。
    LoudnessLevel,
    /// 响度均衡：音量变化级别（1–10）。
    LoudnessSmooth,
    /// 音量音效自己的响度（百分比）。
    VolumeSound,
}

/// 设置页里那几格 OSD 数值（尺寸 / 圆角 / 元素大小 / 文字）对准的一直是**音量模板**：
/// 页面上那块预览就是按音量模板画的。编辑器停在媒体键或亮度模板时不能跟着它走，
/// 否则「看着是音量卡，改的是另一张」。要调另外两套，用编辑器的参数栏。
fn with_volume_osd<R>(
    config: &mut crate::config::Config,
    edit: impl FnOnce(&mut crate::config::OsdConfig) -> R,
) -> R {
    let saved = config.osd.template();
    if saved != crate::config::OsdTemplate::Volume {
        config.osd.set_template(crate::config::OsdTemplate::Volume);
    }
    let out = edit(&mut config.osd);
    if saved != crate::config::OsdTemplate::Volume {
        config.osd.set_template(saved);
    }
    out
}

impl NumberField {
    /// 数字框里显示的值（有符号：面板字间距能填负数）。
    ///
    /// 面板那三个「字体字号」字段和其他字段的单位不一样，单独在这儿换算：
    /// 字号 / 字间距内部按**十分之一像素**存（好让 +/− 走 0.5px），
    /// 行高就是整数像素。其余字段全是像素 / 百分比 / 毫秒，恒为非负。
    fn value(self, config: &Config) -> i32 {
        match self {
            Self::PanelFontSize => (config.panel_font_size * 10.0).round() as i32,
            Self::PanelLetterSpacing => (config.panel_letter_spacing * 10.0).round() as i32,
            Self::PanelLineHeight => config.panel_line_height.round() as i32,
            Self::TrayDigit(digits, metric) => {
                let metrics = tray_digit_metrics(config, digits);
                match metric {
                    TrayDigitMetric::SizePt => metrics.size_pt as i32,
                    TrayDigitMetric::Width => metrics.width as i32,
                    TrayDigitMetric::Height => metrics.height as i32,
                    TrayDigitMetric::Tracking => metrics.tracking,
                }
            }
            _ => self.value_nonneg(config) as i32,
        }
    }

    /// 非负字段的原值。
    fn value_nonneg(self, config: &Config) -> u32 {
        match self {
            // 面板那三个字段在上面的有符号版本里就截住了，走不到这儿。
            Self::PanelFontSize | Self::PanelLetterSpacing | Self::PanelLineHeight => {
                unreachable!("面板字体字段走有符号的那一层")
            }
            // 托盘数字四件套也走有符号的那一层（字距能填负数）。
            Self::TrayDigit(..) => unreachable!("托盘数字字段走有符号的那一层"),
            Self::HotkeyStep => config.hotkey_volume_step,
            Self::WheelStep => config.scroll_wheel_step,
            // 十分之一 dB：0.1 dB 是读数（一位小数）能表达的最小变化。
            Self::LogWheelStep => (config.log_scroll_wheel_step * 10.0).round().max(0.0) as u32,
            Self::StartupVolume => config.startup.master_volume,
            Self::VolumeCap => config.volume_cap.percent,
            Self::HotkeyStepDb => config.hotkey_volume_step,
            Self::AppHotkeyStep | Self::AppHotkeyStepDb => config.hotkey_app_volume_step,
            Self::BrightnessStep => config.brightness.step,
            Self::BrightnessScrollStep => config.brightness.scroll_step,
            Self::BrightnessWidth => config.brightness.width,
            Self::BrightnessVerticalHeight => config.brightness.vertical_height,
            Self::FlyoutWidth => config.flyout_width,
            Self::FlyoutVerticalHeight => config.vertical_height,
            Self::PanelBarThickness => config.panel_bar_thickness,
            Self::PanelBarRound => config.panel_bar_round,
            Self::OsdWidth => config.osd.width,
            Self::OsdHeight => config.osd.height,
            Self::OsdCorner => config.osd.corner,
            Self::OsdOpacity => config.osd.opacity,
            Self::OsdX => config.osd.custom_x,
            Self::OsdY => config.osd.custom_y,
            Self::OsdFadeIn => config.osd.fade_in_ms,
            Self::OsdFadeOut => config.osd.fade_out_ms,
            Self::OsdHold => config.osd.hold_ms,
            Self::OsdSize(part) => config.osd.part(part).size,
            Self::DeviceNameSize => config.device_name_size,
            Self::PopupAnimation => config.popup_animation_ms,
            Self::UiFontScale => config.ui_font_scale,
            Self::UiOpacity => config.ui_opacity,
            Self::UiContrast => config.ui_contrast,
            Self::TrayMenuWidth => config.tray_menu_max_width,
            Self::TrayMotionGain => config.tray_motion_gain,
            Self::HotkeyRepeatInterval => config.hotkey_repeat_interval,
            Self::HotkeyRepeatDelay => config.hotkey_repeat_delay,
            Self::BrightnessHotkeyMonitor => config.brightness.hotkey_monitor,
            Self::BrightnessHotkeyStep => config.brightness.hotkey_step,
            // 两个都是整数百分比 / 级别，直接就是显示值。
            Self::LoudnessLevel => config.loudness.level.clamp(1, 100),
            Self::LoudnessSmooth => config.loudness.smoothness.clamp(1, 10),
            Self::VolumeSound => config.volume_sound.volume,
        }
    }

    /// 可填范围。负数只给面板字间距一个字段。
    fn range(self) -> (i32, i32) {
        match self {
            // 6.0–72.0px，按十分之一存。
            Self::PanelFontSize => (
                (crate::config::PANEL_FONT_SIZE_RANGE.0 * 10.0).round() as i32,
                (crate::config::PANEL_FONT_SIZE_RANGE.1 * 10.0).round() as i32,
            ),
            // −5.0–20.0px（负 = 紧排），按十分之一存。
            Self::PanelLetterSpacing => (
                (crate::config::PANEL_LETTER_SPACING_RANGE.0 * 10.0).round() as i32,
                (crate::config::PANEL_LETTER_SPACING_RANGE.1 * 10.0).round() as i32,
            ),
            // 0 = 跟着字号自动算；上限只是挡天文数字，正常想填多少填多少。
            Self::PanelLineHeight => (0, crate::config::PANEL_LINE_HEIGHT_MAX as i32),
            Self::TrayDigit(_, metric) => match metric {
                TrayDigitMetric::SizePt => (
                    crate::config::TrayDigitMetrics::SIZE_RANGE.0 as i32,
                    crate::config::TrayDigitMetrics::SIZE_RANGE.1 as i32,
                ),
                TrayDigitMetric::Width | TrayDigitMetric::Height => (
                    crate::config::TrayDigitMetrics::SCALE_RANGE.0 as i32,
                    crate::config::TrayDigitMetrics::SCALE_RANGE.1 as i32,
                ),
                TrayDigitMetric::Tracking => crate::config::TrayDigitMetrics::TRACKING_RANGE,
            },
            _ => {
                let (low, high) = self.range_nonneg();
                (low as i32, high as i32)
            }
        }
    }

    fn range_nonneg(self) -> (u32, u32) {
        match self {
            // 同上：这三个字段的范围在 `range()` 里给（有符号）。
            Self::PanelFontSize | Self::PanelLetterSpacing | Self::PanelLineHeight => {
                unreachable!("面板字体字段走有符号的那一层")
            }
            Self::TrayDigit(..) => unreachable!("托盘数字字段走有符号的那一层"),
            Self::HotkeyStep => (1, 100),
            Self::WheelStep => (1, 100),
            Self::LogWheelStep => (
                (crate::config::LOG_SCROLL_STEP_RANGE_DB.0 * 10.0).round() as u32,
                (crate::config::LOG_SCROLL_STEP_RANGE_DB.1 * 10.0).round() as u32,
            ),
            Self::StartupVolume => (0, 100),
            // 上限压到 0 就等于常驻静音，没有意义；最低给 5%。
            Self::VolumeCap => (5, 100),
            // 参考实现的 increase / decrease 就是 2，这里允许 1..20。
            Self::HotkeyStepDb | Self::AppHotkeyStep | Self::AppHotkeyStepDb => (1, 100),
            Self::BrightnessStep => (1, 20),
            Self::BrightnessScrollStep => crate::config::BRIGHTNESS_SCROLL_STEP_RANGE,
            Self::BrightnessWidth => crate::config::BRIGHTNESS_WIDTH_RANGE,
            Self::BrightnessVerticalHeight | Self::FlyoutVerticalHeight => {
                crate::config::VERTICAL_HEIGHT_RANGE
            }
            Self::FlyoutWidth => crate::config::FLYOUT_WIDTH_RANGE,
            Self::PanelBarThickness => crate::config::PANEL_BAR_THICKNESS_RANGE,
            Self::PanelBarRound => (0, 100),
            Self::OsdWidth => (160, 900),
            Self::OsdHeight => (48, 320),
            // 圆角最大到高度一半就够了（再大看不出变化）。
            Self::OsdCorner => (0, 160),
            // 太透明就看不见字了，最低给 20%。
            Self::OsdOpacity => (20, 100),
            Self::OsdX | Self::OsdY => (0, 4000),
            Self::OsdFadeIn | Self::OsdFadeOut => (0, 2000),
            Self::OsdHold => (200, 10000),
            Self::OsdSize(part) => part.size_range(),
            // 比 9 号再小就糊成一团，20 号以外设备名会溢出成三行。
            Self::DeviceNameSize => (9, 20),
            // 0 = 不动画（面板直接出现）；再长就慢得难受了。
            Self::PopupAnimation => (0, 2000),
            // 再小字就看不清了，再大整个布局会开始挤。
            Self::UiFontScale => (80, 150),
            // 低于 30% 连字都读不出来，不如不给。
            Self::UiOpacity => (30, 100),
            Self::UiContrast => crate::config::UI_CONTRAST_RANGE,
            Self::TrayMenuWidth => crate::config::TRAY_MENU_WIDTH_RANGE,
            Self::TrayMotionGain => crate::config::TRAY_MOTION_GAIN_RANGE,
            Self::HotkeyRepeatInterval => crate::config::HOTKEY_REPEAT_RANGE,
            Self::HotkeyRepeatDelay => crate::config::HOTKEY_REPEAT_DELAY_RANGE,
            // 台数上限按 8 给：再多的场面亮度面板本来也放不下。
            Self::BrightnessHotkeyMonitor => (1, 8),
            Self::BrightnessHotkeyStep => crate::config::BRIGHTNESS_HOTKEY_STEP_RANGE,
            // 目标电平 / 上限都是线性幅度，1% 就已经很小了，不给 0（0 等于全程静音）。
            Self::LoudnessLevel => (1, 100),
            // 参考实现的 `CompRate` 没有硬上限；给到 10.0（= 20 dB 斜率）足够，
            // 再大就是「所有内容一样响」的死压，听感会喘。
            // 1 = 一块走 50%，10 = 一块走千分之一。
            Self::LoudnessSmooth => (1, 10),
            // 0 就是「开了但不出声」，容易让人以为坏了，最低给 5%。
            Self::VolumeSound => (5, 100),
            // 参考实现默认 3000ms；少于 100ms 窗口里连一块都攒不满。

            // 0 dB = 不提升（参考实现的行为）；+18 dB（8 倍）再往上就没意义了。

        }
    }

    fn apply(self, config: &mut Config, value: i32) {
        match self {
            Self::PanelFontSize => config.panel_font_size = value as f32 / 10.0,
            Self::PanelLetterSpacing => config.panel_letter_spacing = value as f32 / 10.0,
            Self::PanelLineHeight => config.panel_line_height = value as f32,
            // 字距能填负数，所以托盘数字这几项走有符号这一层。
            Self::TrayDigit(digits, metric) => {
                let metrics = tray_digit_metrics_mut(config, digits);
                match metric {
                    TrayDigitMetric::SizePt => metrics.size_pt = value.max(0) as u32,
                    TrayDigitMetric::Width => metrics.width = value.max(0) as u32,
                    TrayDigitMetric::Height => metrics.height = value.max(0) as u32,
                    TrayDigitMetric::Tracking => metrics.tracking = value,
                }
            }
            // 一行都不给负数：夹一下再往下传，免得手改配置文件写出负数。
            _ => self.apply_nonneg(config, value.max(0) as u32),
        }
    }

    fn apply_nonneg(self, config: &mut Config, value: u32) {
        match self {
            // 同上：面板字体字段走 `apply()`。
            Self::PanelFontSize | Self::PanelLetterSpacing | Self::PanelLineHeight => {
                unreachable!("面板字体字段走有符号的那一层")
            }
            Self::HotkeyStep => config.hotkey_volume_step = value,
            Self::WheelStep => config.scroll_wheel_step = value,
            Self::LogWheelStep => config.log_scroll_wheel_step = value as f32 / 10.0,
            Self::TrayDigit(..) => unreachable!("托盘数字字段走有符号的那一层"),
            Self::StartupVolume => config.startup.master_volume = value,
            Self::VolumeCap => config.volume_cap.percent = value,
            Self::HotkeyStepDb => config.hotkey_volume_step = value,
            Self::AppHotkeyStep | Self::AppHotkeyStepDb => config.hotkey_app_volume_step = value,
            Self::BrightnessStep => config.brightness.step = value,
            Self::BrightnessScrollStep => config.brightness.scroll_step = value,
            Self::BrightnessWidth => config.brightness.width = value,
            Self::BrightnessVerticalHeight => config.brightness.vertical_height = value,
            Self::FlyoutWidth => config.flyout_width = value,
            Self::FlyoutVerticalHeight => config.vertical_height = value,
            Self::PanelBarThickness => config.panel_bar_thickness = value,
            Self::PanelBarRound => config.panel_bar_round = value,
            // 这几格读的一直是音量模板那份（上面那块预览也是按音量模板画的），
            // 写的时候也必须落在同一份上 —— 否则编辑器停在亮度模板时，
            // 「预览是音量卡，改的却是亮度卡」。
            Self::OsdWidth => with_volume_osd(config, |osd| osd.set_card(|c| c.width = value)),
            Self::OsdHeight => with_volume_osd(config, |osd| osd.set_card(|c| c.height = value)),
            Self::OsdCorner => with_volume_osd(config, |osd| osd.set_card(|c| c.corner = value)),
            Self::OsdOpacity => with_volume_osd(config, |osd| osd.set_card(|c| c.opacity = value)),
            Self::OsdX => config.osd.custom_x = value,
            Self::OsdY => config.osd.custom_y = value,
            Self::OsdFadeIn => config.osd.fade_in_ms = value,
            Self::OsdFadeOut => config.osd.fade_out_ms = value,
            Self::OsdHold => config.osd.hold_ms = value,
            Self::OsdSize(part) => with_volume_osd(config, |osd| osd.set_part_size(part, value)),
            Self::DeviceNameSize => config.device_name_size = value,
            Self::PopupAnimation => config.popup_animation_ms = value,
            Self::UiFontScale => config.ui_font_scale = value,
            Self::UiOpacity => config.ui_opacity = value,
            Self::UiContrast => config.ui_contrast = value,
            Self::TrayMenuWidth => config.tray_menu_max_width = value,
            Self::TrayMotionGain => config.tray_motion_gain = value,
            Self::HotkeyRepeatInterval => config.hotkey_repeat_interval = value,
            Self::HotkeyRepeatDelay => config.hotkey_repeat_delay = value,
            Self::BrightnessHotkeyMonitor => config.brightness.hotkey_monitor = value,
            Self::BrightnessHotkeyStep => config.brightness.hotkey_step = value,
            Self::LoudnessLevel => config.loudness.level = value.clamp(1, 100),
            Self::LoudnessSmooth => config.loudness.smoothness = value.clamp(1, 10),
            Self::VolumeSound => config.volume_sound.volume = value.clamp(5, 100),
        }
    }

    /// 数值框里显示的文字（自动大小写「自动」）。
    fn display(self, value: i32, lang: Lang) -> String {
        match self {
            // 「字体字号」那套：11.5 这种一位小数，整数就不带尾巴。
            Self::PanelFontSize => format!("{} px", tenths_text(value)),
            // 字间距带正负号：+2 一眼看出是放松，−1.5 是收紧。
            Self::PanelLetterSpacing => {
                let text = tenths_text(value);
                if value > 0 { format!("+{text}") } else { text }
            }
            // 0 = 跟着字号自动算（和高度类字段一个写法）。
            Self::PanelLineHeight => {
                if value == 0 {
                    lang.pick("自动", "Auto").to_string()
                } else {
                    format!("{value} px")
                }
            }
            _ => self.display_nonneg(value.max(0) as u32, lang),
        }
    }

    fn display_nonneg(self, value: u32, lang: Lang) -> String {
        match self {
            // 同上：面板字体字段的显示在 `display()` 里（带小数 / 正负号）。
            Self::PanelFontSize | Self::PanelLetterSpacing | Self::PanelLineHeight => {
                unreachable!("面板字体字段走有符号的那一层")
            }
            Self::HotkeyStep
            | Self::WheelStep
            | Self::StartupVolume
            | Self::VolumeCap
            | Self::VolumeSound
            | Self::BrightnessStep => format!("{value}%"),
            Self::HotkeyStepDb | Self::AppHotkeyStepDb => format!("{value} dB"),
            Self::AppHotkeyStep => format!("{value}%"),
            Self::BrightnessScrollStep => format!("{value}%"),
            // 存的是十分之一 dB，显示成一位小数。
            Self::LogWheelStep => format!("{:.1} dB", value as f32 / 10.0),
            Self::TrayDigit(_, metric) => match metric {
                // 0 是「自动」，比一个具体点数更好懂。
                TrayDigitMetric::SizePt => {
                    if value == 0 {
                        lang.pick("自动", "Auto").to_string()
                    } else if lang.is_en() {
                        format!("{value} pt")
                    } else {
                        // 和 Photoshop 的中文界面一致：字号叫「点」。
                        format!("{value} 点")
                    }
                }
                TrayDigitMetric::Width | TrayDigitMetric::Height => format!("{value}%"),
                TrayDigitMetric::Tracking => format!("{value}"),
            },
            // 0 = 跟随音量面板，比写死一个数字更好懂。
            Self::BrightnessWidth => {
                if value == 0 {
                    lang.pick("跟随", "Match").to_string()
                } else {
                    format!("{value} px")
                }
            }
            Self::FlyoutWidth => format!("{value} px"),
            // 0 在这一对字段里是「跟谁一样高」，写清楚比显示 0 px 好懂。
            Self::BrightnessVerticalHeight => {
                if value == 0 {
                    lang.pick("跟随", "Match").to_string()
                } else {
                    format!("{value} px")
                }
            }
            Self::FlyoutVerticalHeight => {
                if value == 0 {
                    lang.pick("自动", "Auto").to_string()
                } else {
                    format!("{value} px")
                }
            }
            Self::PanelBarThickness => format!("{value} px"),
            Self::PanelBarRound => format!("{value}%"),
            Self::OsdWidth | Self::OsdHeight | Self::OsdCorner => format!("{value} px"),
            Self::OsdOpacity => format!("{value}%"),
            Self::OsdX | Self::OsdY => format!("{value} px"),
            Self::OsdFadeIn | Self::OsdFadeOut => format!("{value} ms"),
            Self::OsdHold => format!("{:.1} s", value as f32 / 1000.0),
            Self::OsdSize(_) => format!("{value} px"),
            Self::DeviceNameSize => format!("{value} px"),
            Self::PopupAnimation => format!("{value} ms"),
            Self::UiFontScale | Self::UiOpacity | Self::UiContrast => format!("{value}%"),
            Self::TrayMenuWidth => format!("{value} px"),
            Self::TrayMotionGain => format!("{value}%"),
            Self::HotkeyRepeatInterval | Self::HotkeyRepeatDelay => format!("{value} ms"),
            Self::BrightnessHotkeyMonitor => format!("#{value}"),
            Self::BrightnessHotkeyStep => {
                if value == 0 {
                    lang.pick("跟随", "Match").to_string()
                } else {
                    format!("{value}%")
                }
            }
            Self::LoudnessLevel => format!("{value}%"),
            Self::LoudnessSmooth => format!("{value}"),
        }
    }

    /// 这个字段能不能输入小数点（dB 步进和面板字号 / 字间距都按十分之一存）。
    fn allows_decimal(self) -> bool {
        matches!(
            self,
            Self::LogWheelStep | Self::PanelFontSize | Self::PanelLetterSpacing
        )
    }

    /// 这个字段能不能填负数（目前只有面板字间距：负 = 紧排）。
    fn allows_negative(self) -> bool {
        matches!(self, Self::PanelLetterSpacing)
    }

    /// 手动输入框最多几个字符。
    fn edit_len(self) -> usize {
        if matches!(self, Self::PanelFontSize | Self::PanelLetterSpacing) {
            // 字号最长 "72.0"，字间距还要给负号留一位。
            4 + usize::from(self.allows_negative())
        } else if self.allows_decimal() {
            5
        } else if matches!(self, Self::OsdHold) {
            // 最长 20000ms，五位。
            5
        } else if matches!(
            self,
            Self::OsdX
                | Self::OsdY
                | Self::OsdFadeIn
                | Self::OsdFadeOut
                | Self::OsdWidth
                | Self::OsdHeight
                | Self::PopupAnimation
                | Self::HotkeyRepeatInterval
                | Self::HotkeyRepeatDelay
        ) {
            4
        } else {
            3
        }
    }

    /// 手动输入落成内部整数（dB 步进把 `0.5` 变成 5）。
    fn parse_edit(self, buffer: &str, current: i32) -> i32 {
        if self.allows_decimal() {
            buffer
                .trim()
                .parse::<f32>()
                .map(|value| (value * 10.0).round().clamp(-10_000.0, 10_000.0) as i32)
                .unwrap_or(current)
        } else {
            buffer.trim().parse::<i32>().unwrap_or(current)
        }
    }

    /// 按一下 +/−。
    ///
    /// 「自动」(0) 是个占位值，本身等于 70%：从它往上给 80%、往下给 60%，
    /// 免得按一下只挪 1 个百分点、看不出变化。
    fn stepped(self, current: i32, direction: i32) -> i32 {
        let (low, high) = self.range();
        // 连发延迟的 0 是正经值（「不等」），不是占位值：往上按一格就是 50ms。
        if matches!(self, Self::HotkeyRepeatDelay) {
            return (current + direction * self.step_size()).clamp(low, high);
        }
        if direction > 0 && current == low && low == 0 {
            // 高度类字段的 0 是「自动 / 跟随」，往上按一下直接给一个像样的高度。
            if matches!(
                self,
                Self::FlyoutVerticalHeight | Self::BrightnessVerticalHeight
            ) {
                return 200.min(high);
            }
            // 托盘数字的字号 0 是「自动」（≈旧版观感）；往上一次给 12pt（16px 画布上正好）。
            if matches!(self, Self::TrayDigit(_, TrayDigitMetric::SizePt)) {
                return 12.min(high);
            }
            return 80.min(high);
        }
        let next = current + direction * self.step_size();
        next.clamp(low, high)
    }

    /// 按一次 +/− 走多少（单位就是 `value()` 那个内部单位）。
    ///
    /// 像素和毫秒类的字段按 1 走太细了（1400ms 要点 1400 下），
    /// 给一个「一眼能看出来」的步长；要精确值直接点数字输入。
    fn step_size(self) -> i32 {
        match self {
            Self::OsdWidth | Self::OsdHeight | Self::OsdX | Self::OsdY => 8,
            // 面板宽度一像素一像素地调才有意义（滑条长度直接跟着变）。
            Self::BrightnessWidth => 1,
            Self::FlyoutWidth | Self::TrayMenuWidth => 8,
            Self::TrayMotionGain => 10,
            // 高度按 8px 走：1px 一按要点太多次，精确值直接点数字输入。
            Self::BrightnessVerticalHeight | Self::FlyoutVerticalHeight => 8,
            Self::OsdCorner => 2,
            Self::OsdOpacity => 5,
            Self::OsdFadeIn | Self::OsdFadeOut => 20,
            Self::OsdHold => 100,
            // 字号和不透明度按 5% 走：1% 一按看不出来，精确值直接点数字输入。
            Self::UiFontScale | Self::UiOpacity => 5,
            // 对比度也是 1% 看不出来，按 5% 走。
            Self::UiContrast => 5,
            // 动画时长按 25ms 走（原来只有 200/300/400/500 四档）。
            Self::PopupAnimation => 25,
            // 面板字号 / 字间距内部是「十分之一像素」，走 5 = 半像素，
            // 正好是字体字号框里最常用的一档（11.5 / 12 / 12.5）。
            Self::PanelFontSize | Self::PanelLetterSpacing => 5,
            // 托盘数字：字号按 1pt 走（16px 画布上一档约 1.3px），宽高按 5%，
            // 字距按 10（= 1/100 em，16px 下约 0.16px 一档）。
            Self::TrayDigit(_, TrayDigitMetric::SizePt) => 1,
            Self::TrayDigit(_, TrayDigitMetric::Width | TrayDigitMetric::Height) => 5,
            Self::TrayDigit(_, TrayDigitMetric::Tracking) => 10,
            // 响度那两个都是小整数，一档一格。
            Self::LoudnessLevel | Self::LoudnessSmooth => 1,
            _ => 1,
        }
    }

    /// 改这个字段要不要重画托盘图标（数字相关的都要）。
    fn touches_tray(self) -> bool {
        matches!(self, Self::TrayDigit(..))
    }
}

/// 取某一档位数（1 / 2 / 3 位）的托盘数字外观。
fn tray_digit_metrics(config: &Config, digits: u8) -> crate::config::TrayDigitMetrics {
    match digits {
        1 => config.tray_digit_one,
        2 => config.tray_digit_two,
        _ => config.tray_digit_three,
    }
}

/// 同上，可变引用（写配置用）。
fn tray_digit_metrics_mut(
    config: &mut Config,
    digits: u8,
) -> &mut crate::config::TrayDigitMetrics {
    match digits {
        1 => &mut config.tray_digit_one,
        2 => &mut config.tray_digit_two,
        _ => &mut config.tray_digit_three,
    }
}

/// 内部值（十分之一）→ 显示文字：`115` → `11.5`，`120` → `12`。
fn tenths_text(value: i32) -> String {    let sign = if value < 0 { "-" } else { "" };
    let abs = value.unsigned_abs();
    if abs % 10 == 0 {
        format!("{sign}{}", abs / 10)
    } else {
        format!("{sign}{}.{}", abs / 10, abs % 10)
    }
}

/// 字体下拉框在改哪个字体。
///
/// 三个字体是分开存的：`tray_number_font` 只影响托盘上那几个数字（光栅化时用），
/// `ui_font` 是整个设置窗的字体族，`panel_font` 是音量面板的
/// （两个窗口各自在根节点上挂一次，全树继承，互不影响）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FontTarget {
    /// 托盘数字。
    TrayNumber,
    /// 整个设置窗。
    Ui,
    /// 音量面板。
    Panel,
}

impl FontTarget {
    fn current(self, config: &Config) -> String {
        match self {
            Self::TrayNumber => config.tray_number_font.clone(),
            Self::Ui => config.ui_font.clone(),
            Self::Panel => config.panel_font.clone(),
        }
    }

    fn set(self, config: &mut Config, family: String) {
        match self {
            Self::TrayNumber => config.tray_number_font = family,
            Self::Ui => config.ui_font = family,
            Self::Panel => config.panel_font = family,
        }
    }

    fn set_weight(self, config: &mut Config, weight: u32) {
        match self {
            // 托盘数字从 rev 67 起不再有字重设置（笔画粗细固定 600）。
            Self::TrayNumber => {}
            Self::Ui => config.ui_font_weight = weight,
            Self::Panel => config.panel_font_weight = weight,
        }
    }

    fn title(self, lang: Lang) -> &'static str {
        match self {
            Self::TrayNumber => lang.pick("数字字体", "Number font"),
            Self::Ui => lang.pick("界面字体", "Interface font"),
            Self::Panel => lang.pick("面板字体", "Panel font"),
        }
    }

    fn description(self, lang: Lang) -> &'static str {
        match self {
            Self::TrayNumber => lang.pick(
                "托盘上的数字用哪个字形",
                "Which typeface the tray digits use",
            ),
            Self::Ui => lang.pick(
                "设置窗口里所有文字用哪个字形；「默认」是系统字体",
                "Typeface for every text in this window; Default follows the system",
            ),
            Self::Panel => lang.pick(
                "音量面板里所有文字用哪个字形；「默认」是系统字体（和设置窗分开存）",
                "Typeface for every text in the volume panel; Default follows the system",
            ),
        }
    }
}

/// 「跟随系统」模式下的两档槽位：系统亮色用哪一套底色 / 强调色、深色用哪一套。
///
/// 两档各自占两个配置字段 —— 切到不跟随系统时它们还留着，
/// 下次打开「跟随系统」还是原来配的那两套。
#[derive(Clone, Copy, PartialEq, Eq)]
enum ThemeSlot {
    Light,
    Dark,
}

/// 自选颜色作用在哪一项上。
///
/// 前两项是「固定主题」那一套的自选色；后四项是「跟随系统」时亮 / 暗两档
/// 各自的自选色 —— 每档一套，跟随时两档才能长得不一样。
/// 存的都是十六进制串（空 = 用那套主题自己的颜色），配置里各占一个字段。
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
enum ColorTarget {
    /// 背景 / 卡片底色，连带决定文字和描边用深色还是浅色。
    Surface,
    /// 强调色：选中态、滑条、角标。
    Accent,
    /// 跟随系统 → 亮色档的自选底色。
    LightSurface,
    /// 跟随系统 → 亮色档的自选强调色。
    LightAccent,
    /// 跟随系统 → 深色档的自选底色。
    DarkSurface,
    /// 跟随系统 → 深色档的自选强调色。
    DarkAccent,
    /// 托盘图标的墨色。跟主题无关，是任务栏上那个小图标的颜色。
    ///
    /// 空 = 跟随任务栏深浅（深色任务栏画浅墨、浅色画深墨），和系统图标一致。
    Tray,
}

impl ColorTarget {
    /// 是不是底色。底色不给「跟随系统」那个空心块：底色的「跟随」= 打底模板自己的
    /// 底色，再摆一个空心块只是多占一格（点掉已选中的色块也能回到模板）。
    fn is_surface(self) -> bool {
        matches!(self, Self::Surface | Self::LightSurface | Self::DarkSurface)
    }

    fn current(self, config: &Config) -> &str {
        match self {
            Self::Surface => &config.theme_surface,
            Self::Accent => &config.theme_accent,
            Self::LightSurface => &config.theme_light_surface,
            Self::LightAccent => &config.theme_light_accent,
            Self::DarkSurface => &config.theme_dark_surface,
            Self::DarkAccent => &config.theme_dark_accent,
            Self::Tray => &config.tray_icon_color,
        }
    }

    fn apply(self, config: &mut Config, value: String) {
        match self {
            Self::Surface => config.theme_surface = value,
            Self::Accent => config.theme_accent = value,
            Self::LightSurface => config.theme_light_surface = value,
            Self::LightAccent => config.theme_light_accent = value,
            Self::DarkSurface => config.theme_dark_surface = value,
            Self::DarkAccent => config.theme_dark_accent = value,
            Self::Tray => config.tray_icon_color = value,
        }
    }

    /// 取色器面板 / 行首的标签。
    fn label(self, lang: Lang) -> &'static str {
        match self {
            Self::Tray => lang.pick("图标颜色", "Icon color"),
            _ if self.is_surface() => lang.pick("背景色", "Background"),
            _ => lang.pick("强调色", "Accent"),
        }
    }

    /// 预设色块。
    ///
    /// 底色给的是「深色几档 + 浅色几档 + 几个低饱和色相」：这几档换上去之后
    /// 文字和描边的对比度都还够。强调色给的是饱和度足够高、当高亮不刺眼的一批。
    /// 托盘图标是 16px 的单色小图标，先给四个中性档（深色任务栏用白、浅色用黑），
    /// 再给一批中等明度的彩色 —— 太暗的在深色任务栏上会糊成一团。
    fn presets(self) -> &'static [&'static str] {
        match self {
            Self::Tray => &[
                "#ffffff", "#c8c8c8", "#8a8a8a", "#101010", "#7c9cff", "#22d3ee", "#34d399",
                "#fbbf24", "#fb923c", "#f87171", "#c084fc", "#f472b6",
            ],
            _ if self.is_surface() => &[
                "#151519", "#1b1f24", "#20252c", "#262b33", "#2d2a36", "#22303f", "#2b3a2f",
                "#3a2b2b", "#f6f6fa", "#ececf2", "#e8eef6", "#f4efe6",
            ],
            _ => &[
                "#7c9cff", "#5b8def", "#6ea8fe", "#a78bfa", "#c084fc", "#34d399", "#22d3ee",
                "#fbbf24", "#fb923c", "#f87171", "#f472b6", "#94a3b8",
            ],
        }
    }
}

/// 改完颜色后让托盘重画一张 —— 只对「图标颜色」那一项有必要。
///
/// 主题那几项改完，窗口和面板跟着重画就完事；托盘图标是系统托盘里的一张位图，
/// 不跟着任何窗口重绘，换了色号必须显式推一次新图，否则要等到下次调音量才变。
fn refresh_tray_for(target: ColorTarget, cx: &mut gpui::App) {
    if matches!(target, ColorTarget::Tray) {
        crate::gui::refresh_tray_icon(cx);
    }
}

/// 色块上的「已选中」圆点用什么颜色：亮底配深色点，暗底配浅色点。
///
/// 用感知亮度（0.299R + 0.587G + 0.114B）而不是 HSL 的 L：同样是 50% 的 L，
/// 黄色看着比蓝色亮得多，按 L 判断会在黄色上配出看不见的白点。
fn dot_on(color: gpui::Hsla) -> gpui::Hsla {
    let rgb = gpui::hsla_to_rgba(color);
    let luminance = 0.299 * rgb.red + 0.587 * rgb.green + 0.114 * rgb.blue;
    // `rgba()` 给的是 RGB，这里统一转成 Hsla（界面里所有颜色都是 Hsla）。
    if luminance > 0.6 {
        gpui::rgb_to_hsla(rgba(0x16161aff))
    } else {
        gpui::rgb_to_hsla(rgba(0xf7f7faff))
    }
}

/// 自选颜色的取色器：点色块行末尾的「+」打开。
///
/// 方块 / 色相条 / 拖拽全在 `ui::picker` 里（和 OSD 编辑器共用同一份），
/// 这里只存「改的是哪一项」和面板贴在哪儿。
struct ColorWheel {
    target: ColorTarget,
    /// 点开时的窗口坐标（面板按它定位）。
    x: f32,
    y: f32,
    /// HSV / 色号 / 拖拽状态。
    picker: Picker,
    /// 正在屏幕上取色（窗口已藏起来）。
    picking: bool,
}

impl ColorWheel {
    /// 按当前配置里的色号造一个：空串（跟随）就用当前模板那一项的色号当起点。
    fn new(target: ColorTarget, x: f32, y: f32, value: &str) -> Self {
        let fallback = if target.is_surface() {
            (0x15, 0x15, 0x19)
        } else {
            (0x7c, 0x9c, 0xff)
        };
        let rgb = parse_rgb(value).unwrap_or(fallback);
        let (h, s, v) = rgb_to_hsv(rgb.0, rgb.1, rgb.2);
        Self {
            target,
            x,
            y,
            picker: Picker::new(h, s, v, hex_of(rgb)),
            picking: false,
        }
    }
}

/// 页签属于哪一页（决定点下去改哪个字段）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum TabTarget {
    Shortcuts,
    Devices,
    Volume,
}

/// 可自定义的全局快捷键，对应配置里的十七个字段（每个字段三组）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum HotkeySlot {
    Flyout,
    Settings,
    VolumeUp,
    VolumeDown,
    AppVolumeUp,
    AppVolumeDown,
    MediaPrev,
    MediaNext,
    MediaPlayPause,
    MediaMute,
    MediaStop,
    BrightnessUp,
    BrightnessDown,
    BrightnessAllUp,
    BrightnessAllDown,
    BrightnessMonitorUp,
    BrightnessMonitorDown,
}

impl HotkeySlot {
    fn set(self, config: &mut Config, index: usize, hotkey: Hotkey) {
        let slots = match self {
            Self::Flyout => &mut config.hotkey_flyout,
            Self::Settings => &mut config.hotkey_settings,
            Self::VolumeUp => &mut config.hotkey_volume_up,
            Self::VolumeDown => &mut config.hotkey_volume_down,
            Self::AppVolumeUp => &mut config.hotkey_app_volume_up,
            Self::AppVolumeDown => &mut config.hotkey_app_volume_down,
            Self::MediaPrev => &mut config.hotkey_media_prev,
            Self::MediaNext => &mut config.hotkey_media_next,
            Self::MediaPlayPause => &mut config.hotkey_media_play_pause,
            Self::MediaMute => &mut config.hotkey_media_mute,
            Self::MediaStop => &mut config.hotkey_media_stop,
            Self::BrightnessUp => &mut config.hotkey_brightness_up,
            Self::BrightnessDown => &mut config.hotkey_brightness_down,
            Self::BrightnessAllUp => &mut config.hotkey_brightness_all_up,
            Self::BrightnessAllDown => &mut config.hotkey_brightness_all_down,
            Self::BrightnessMonitorUp => &mut config.hotkey_brightness_monitor_up,
            Self::BrightnessMonitorDown => &mut config.hotkey_brightness_monitor_down,
        };
        slots.set(index, hotkey);
    }
}

impl Drop for Settings {
    fn drop(&mut self) {
        // 窗口关掉时别把「正在录制」留在开启状态，否则媒体键会被钩子一直吞掉。
        crate::platform::hotkey::set_capturing(false);
        // 编辑器里还有没提交的内容（写完直接关窗口）也要落盘，
        // 不能因为用户没点空白处就把刚写的模板丢了。
        if let Some(editor) = self.tooltip_edit.take() {
            let target = editor.target;
            let lines = editor.trimmed();
            let _ = self.config.borrow_mut().update(|config| match target {
                TooltipTarget::Normal => config.tooltip.lines = lines,
                TooltipTarget::Muted => config.tooltip.muted_lines = lines,
            });
        }
        // 应用音量也一样：边输边关窗口别把刚敲的数字丢了。
        if let Some((index, buffer)) = self.app_edit.take() {
            if let Ok(percent) = buffer.trim().parse::<u32>() {
                let _ = self.config.borrow_mut().update(|config| {
                    if let Some(app) = config.startup.app_volumes.get_mut(index) {
                        app.percent = percent.min(100);
                    }
                });
            }
        }
    }
}

/// 托盘菜单条目在设置页里显示的名字。
fn tray_node_label(lang: Lang, id: &str) -> String {
    if id == "devices" {
        return lang.pick("播放设备列表", "Playback devices").to_string();
    }
    crate::gui::tray_action_label(lang, id)
        .unwrap_or(id)
        .to_string()
}

impl Settings {
    pub fn new(
        config: Rc<RefCell<ConfigStore>>,
        config_path: impl Into<SharedString>,
        snapshot: Rc<RefCell<Snapshot>>,
        engine: Rc<AudioEngine>,
        cx: &mut Context<Self>,
    ) -> Self {
        // 设置页开着的时候是「草稿模式」：改动只留内存，按「应用 / 保存」才落盘。
        // 参照点（`恢复更改` 要退回的那一份）由 `ConfigStore` 自己记。
        config.borrow_mut().begin_draft();
        Self {
            config,
            config_path: config_path.into(),
            snapshot,
            engine,
            page: Page::default(),
            scrolls: std::array::from_fn(|_| gpui::ScrollHandle::new()),
            last_size: std::cell::Cell::new((0.0, 0.0)),
            size_generation: Rc::new(std::cell::Cell::new(0)),
            focus: cx.focus_handle(),
            capturing: None,
            repeat_tip: None,
            just_cleared: None,
            shortcut_tab: 0,
            device_tab: 0,
            volume_tab: 0,
            page_anim: 0,
            editing: None,
            font_menu: None,
            font_scroll: gpui::ScrollHandle::new(),
            font_filter: String::new(),
            font_pending_scroll: None,
            picker: None,
            icon_picker: None,
            picker_scroll: gpui::ScrollHandle::new(),
            scroll_drag: ScrollDragState::new(),
            tooltip_edit: None,
            footer_preview: crate::ui::footer_preview::FooterPreviewState::default(),
            app_edit: None,
            tray_menu_open: false,
            tray_menu_scroll: gpui::ScrollHandle::new(),
            tray_drag: None,
            tray_drag_is_folder: false,
            tray_drop: None,
            tray_rename: None,
            device_rename: None,
            display_rename: None,
            rename_cursor: 0,
            rename_bounds: Rc::new(Cell::new(None)),
            rename_commit_at: None,
            rename_commit_key: None,
            text_undo: Vec::new(),
            text_undo_owner: None,
            text_marked: None,
            osd_text_edit: None,
            osd_pinned: false,
            titlebar_armed: false,
            window_closed: None,
            block_pick: None,
            block_name_edit: None,
            block_name_focus: false,
            block_exempt_edit: None,
            block_exempt_focus: false,
            block_notice: None,
            color_wheel: None,
            color_memory: std::collections::HashMap::new(),
            donate_open: false,
            donate_image: RefCell::new(None),
            restore_menu_open: false,
            confirm_close: false,
        }
    }

    /// 记住窗口尺寸：拖动停下 0.6 秒后写进配置，拖的过程中不落盘。
    fn remember_size(&self, window: &Window, cx: &mut Context<Self>) {
        // `viewport_size()` 就是客户区（内容区）尺寸，和创建时传给
        // `WindowOptions::window_bounds` 的是同一个量，这里不能再扣边框：
        // 扣了就会「启动一次小一圈」。窗口外框由 gpui 自己按 border_offset 补。
        let size = window.viewport_size();
        let (width, height) = (size.width.as_f32(), size.height.as_f32());
        if width < 200.0 || height < 200.0 {
            return;
        }
        // 最大化（或还原中）拿到的不是用户挑的尺寸：记下来下次打开就顶满一屏。
        if window.is_maximized() {
            return;
        }
        let (last_width, last_height) = self.last_size.get();
        if (width - last_width).abs() < 2.0 && (height - last_height).abs() < 2.0 {
            return;
        }
        self.last_size.set((width, height));

        let generation = self.size_generation.get() + 1;
        self.size_generation.set(generation);
        let generation_handle = self.size_generation.clone();
        let store = self.config.clone();
        cx.spawn(async move |_, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(600))
                .await;
            // 期间又拖过：交给更晚的那次写。
            if generation_handle.get() != generation {
                return;
            }
            // 窗口尺寸不是用户在设置页做的改动：走 `update_now` 立刻落盘，
            // 不进草稿 —— 否则拖一下窗口就变成「有未保存的更改」，还得按应用才记住。
            let _ = store.borrow_mut().update_now(|config| {
                config.settings_width = width;
                config.settings_height = height;
            });
        })
        .detach();
    }

    /// 滚动条（可拖动）。轨道和滑块由 `Scrollbar` 自己画、自己收鼠标事件；
    /// 这一列挂在滚动区**外面**的槽里，所以永远压不到内容上。
    ///
    /// `interactive` 关掉时只画不收事件：浮层打开时下面那根要停掉，
    /// 否则点在遮罩上会顺着穿过去，弹层关掉的同时页面也翻了一屏。
    fn scrollbar_column(
        &self,
        scroll: &gpui::ScrollHandle,
        token: u64,
        interactive: bool,
    ) -> AnyElement {
        Scrollbar::new(
            SharedString::from(format!("settings-scrollbar-{token}")),
            token,
            scroll.clone(),
            self.scroll_drag.clone(),
            SCROLLBAR_WIDTH,
            interactive,
        )
        .into_any_element()
    }

    fn bind(
        &self,
        apply: impl Fn(&mut Config) + 'static,
    ) -> impl Fn(&mut Window, &mut App) + 'static {
        let store = self.config.clone();
        move |window, cx| {
            let _ = store.borrow_mut().update(|config| apply(config));
            refresh_ui(window, cx);
        }
    }

    /// 跟 `bind` 一样，但改完还要把新值推给音频线程（响度均衡是引擎里跑的）。
    fn bind_loudness(
        &self,
        apply: impl Fn(&mut Config) + 'static,
    ) -> impl Fn(&mut Window, &mut App) + 'static {
        let store = self.config.clone();
        move |window, cx| {
            let _ = store.borrow_mut().update(|config| apply(config));
            crate::gui::refresh_loudness(cx);
            refresh_ui(window, cx);
        }
    }

    /// 解除静音矩阵的表头：左边那列留白，右边写着「增大 / 减小」。
    ///
    /// 列宽要和 `switch_cell` 对齐（46），否则表头的小字会飘到开关外面。
    fn unmute_matrix_header(&self, lang: Lang) -> AnyElement {
        let label = |text: &'static str| {
            div()
                .w(px(46.))
                .flex()
                .justify_center()
                .text_size(theme::fs(10.))
                .text_color(theme::text_tertiary())
                .child(text)
                .into_any_element()
        };
        div()
            .flex()
            .items_center()
            .gap(px(4.))
            .px(px(6.))
            .pb(px(3.))
            .child(div().flex_1())
            .child(label(lang.pick("增大", "Up")))
            .child(label(lang.pick("减小", "Down")))
            .into_any_element()
    }

    /// 矩阵的一行：左边操作方式，右边两格开关（左 = 增大，右 = 减小）。
    ///
    /// 行高只有一条小开关那么高（不再有第二行描述），所以 7 行加起来才跟
    /// 原来两行「一格一行」差不多高。
    fn unmute_matrix_row(&self, title: &'static str, up: UnmuteCell, down: UnmuteCell) -> AnyElement {
        div()
            .flex()
            .items_center()
            .gap(px(4.))
            .px(px(6.))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(theme::fs(12.))
                    .text_color(theme::text_secondary())
                    .child(title),
            )
            .child(self.switch_cell(up.0, up.1, self.bind(up.2)))
            .child(self.switch_cell(down.0, down.1, self.bind(down.2)))
            .into_any_element()
    }

    /// 矩阵里的一格：一个小开关，点它翻转，悬停有底色。
    ///
    /// 不走 `toggle_row` —— 那里是「标题 + 描述 + 开关」的一整行，14 格就是十四行。
    /// 矩阵里标签在左边、方向在列头，格子里只留开关。
    fn switch_cell(
        &self,
        id: &'static str,
        value: bool,
        on_toggle: impl Fn(&mut Window, &mut App) + 'static,
    ) -> AnyElement {
        div()
            .id(SharedString::from(id))
            .w(px(46.))
            .h(px(24.))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(theme::RADIUS_SMALL))
            .cursor_pointer()
            .hover(|style| style.bg(theme::surface_hover()))
            .on_click(move |_, window, cx| on_toggle(window, cx))
            .child(self.mini_switch(value))
            .into_any_element()
    }

    /// 小号开关（30×17）：矩阵里一格一个，尺寸比 `toggle_row` 里的小一圈。
    fn mini_switch(&self, value: bool) -> AnyElement {
        let knob_x = if value { 15.0 } else { 2.0 };
        let track_color = if value {
            theme::accent()
        } else {
            theme::track()
        };
        div()
            .w(px(30.))
            .h(px(17.))
            .flex_none()
            .rounded(px(8.5))
            .bg(track_color)
            .relative()
            .child(
                div()
                    .absolute()
                    .top(px(2.))
                    .left(px(knob_x))
                    .size(px(13.))
                    .rounded(px(6.5))
                    .bg(theme::text_primary()),
            )
            .into_any_element()
    }

    /// 紧凑开关行（子项专用）：一行一条，标题 + 可选的小字提示 + 小开关。
    ///
    /// 子项天生是一长串（OSD 那六种操作、解除静音那十四格…），用 `toggle_row` 的
    /// 两行版式会撑成半页，而且同一句说明会在每一行重复。这里把说明压回标题
    /// 同一行的后面（用小字、不够就截断），行高只有 26 —— 和矩阵那一片同一个密度。
    fn compact_toggle_row(
        &self,
        id: &'static str,
        title: &'static str,
        hint: &'static str,
        value: bool,
        on_toggle: impl Fn(&mut Window, &mut App) + 'static,
    ) -> AnyElement {
        div()
            .id(SharedString::from(id))
            .flex()
            .items_center()
            .gap(px(8.))
            .px(px(7.))
            .py(px(3.))
            .rounded(px(theme::RADIUS_SMALL))
            .cursor_pointer()
            .hover(|style| style.bg(theme::surface_hover()))
            .on_click(move |_, window, cx| on_toggle(window, cx))
            .child(
                div()
                    .flex_none()
                    .text_size(theme::fs(12.))
                    .text_color(theme::text_primary())
                    .child(title),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(theme::fs(10.5))
                    .text_color(theme::text_tertiary())
                    .child(hint),
            )
            .child(self.mini_switch(value))
            .into_any_element()
    }

    /// 矩阵里的分隔线：主音量那四行和应用音量条那三行之间。
    fn unmute_matrix_divider(&self) -> AnyElement {
        div()
            .mx(px(6.))
            .my(px(4.))
            .h(px(1.))
            .bg(theme::border())
            .into_any_element()
    }

    fn toggle_row(
        &self,
        id: &'static str,
        title: &'static str,
        description: &'static str,
        value: bool,
        on_toggle: impl Fn(&mut Window, &mut App) + 'static,
    ) -> AnyElement {
        let knob_x = if value { 16.0 } else { 2.0 };
        let track_color = if value {
            theme::accent()
        } else {
            theme::track()
        };

        div()
            .id(SharedString::from(id))
            .flex()
            .items_center()
            .gap(px(10.))
            .px(px(9.))
            .py(px(5.))
            .rounded(px(theme::RADIUS_ROW))
            .cursor_pointer()
            .hover(|style| style.bg(theme::surface_hover()))
            .on_click(move |_, window, cx| on_toggle(window, cx))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .gap(px(2.))
                    .child(
                        div()
                            .text_size(theme::fs(13.))
                            .text_color(theme::text_primary())
                            .child(title),
                    )
                    // 描述留空就不占位置：有些开关一眼就懂，不需要第二行解释。
                    .when(!description.is_empty(), |column| {
                        column.child(
                            div()
                                .text_size(theme::fs(10.5))
                                .text_color(theme::text_tertiary())
                                .child(description),
                        )
                    }),
            )
            .child(
                div()
                    .w(px(34.))
                    .h(px(20.))
                    .flex_none()
                    .rounded(px(10.))
                    .bg(track_color)
                    .relative()
                    .child(
                        div()
                            .absolute()
                            .top(px(2.))
                            .left(px(knob_x))
                            .size(px(16.))
                            .rounded(px(8.))
                            .bg(theme::text_primary()),
                    ),
            )
            .into_any_element()
    }

    /// 字重的四档选项（托盘数字 / 界面共用）。
    ///
    /// 只给四档、且跨度拉开：400 和 500 在 16px 的托盘上几乎没差别，
    /// 留着两档只会让人以为没生效。要更细的笔画就往下取 300。
    fn weight_options(lang: Lang) -> &'static [(u32, &'static str)] {
        if lang.is_en() {
            &[
                (300, "Light"),
                (400, "Regular"),
                (600, "Semibold"),
                (700, "Bold"),
            ]
        } else {
            &[(300, "细"), (400, "常规"), (600, "半粗"), (700, "粗")]
        }
    }

    /// 分段选择：用于滚轮步进这类枚举值。
    fn choice_row(
        &self,
        id: &'static str,
        title: &'static str,
        description: &str,
        options: &'static [(u32, &'static str)],
        value: u32,
        on_pick: impl Fn(u32, &mut Window, &mut App) + 'static,
    ) -> AnyElement {
        let options: Vec<(u32, String)> = options
            .iter()
            .map(|(option_value, label)| (*option_value, (*label).to_string()))
            .collect();
        self.choice_row_owned(id, title, description, options, value, on_pick)
    }

    /// 分段选择控件本身（不带标题那一列）。
    ///
    /// 抽出来给「音效」那一行用：它要在分段选择**右边**再挂一个「试听」按钮，
    /// 两行列不下，只能自己排。
    fn segmented(
        &self,
        id: &'static str,
        options: Vec<(u32, String)>,
        value: u32,
        on_pick: impl Fn(u32, &mut Window, &mut App) + 'static,
    ) -> AnyElement {
        let on_pick = Rc::new(on_pick);
        let mut group = div()
            .flex()
            .flex_wrap()
            .items_center()
            .justify_end()
            .gap(px(2.))
            .p(px(2.))
            .rounded(px(theme::RADIUS_SMALL))
            .bg(theme::track());

        for (option_value, label) in &options {
            let selected = *option_value == value;
            let on_pick = on_pick.clone();
            let option_value = *option_value;
            group = group.child(
                div()
                    .id(SharedString::from(format!("{id}-{option_value}")))
                    .px(px(7.))
                    .py(px(2.))
                    .rounded(px(4.))
                    .text_size(theme::fs(11.))
                    .cursor_pointer()
                    .when(selected, |style| {
                        style.bg(theme::accent()).text_color(theme::text_primary())
                    })
                    .when(!selected, |style| style.text_color(theme::text_tertiary()))
                    .on_click(move |_, window, cx| on_pick(option_value, window, cx))
                    .child(label.clone()),
            );
        }
        group.into_any_element()
    }

    /// 分段选择（选项是运行时拼出来的，例如设备列表）。
    ///
    /// 选项多了会自动折行——设备数量不固定，硬挤在一行会把标题压没。
    fn choice_row_owned(
        &self,
        id: &'static str,
        title: &'static str,
        description: &str,
        options: Vec<(u32, String)>,
        value: u32,
        on_pick: impl Fn(u32, &mut Window, &mut App) + 'static,
    ) -> AnyElement {
        let desc = description.to_string();
        let group = self.segmented(id, options, value, on_pick);

        div()
            .flex()
            .items_center()
            .gap(px(10.))
            .px(px(9.))
            .py(px(5.))
            .rounded(px(theme::RADIUS_ROW))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .gap(px(2.))
                    .child(
                        div()
                            .text_size(theme::fs(13.))
                            .text_color(theme::text_primary())
                            .child(title),
                    )
                    .when(!desc.is_empty(), move |row| {
                        row.child(
                            div()
                                .text_size(theme::fs(10.5))
                                .text_color(theme::text_tertiary())
                                .child(desc),
                        )
                    }),
            )
            .child(group)
            .into_any_element()
    }

    /// 动作行：标题 + 说明 + 右边一个按钮（用于「记录当前状态」这类一次性操作）。
    fn action_row(
        &self,
        id: &'static str,
        title: &'static str,
        description: String,
        label: &'static str,
        on_click: impl Fn(&mut Window, &mut App) + 'static,
    ) -> AnyElement {
        let desc = description;
        div()
            .flex()
            .items_center()
            .gap(px(10.))
            .px(px(9.))
            .py(px(5.))
            .rounded(px(theme::RADIUS_ROW))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .gap(px(2.))
                    .child(
                        div()
                            .text_size(theme::fs(13.))
                            .text_color(theme::text_primary())
                            .child(title),
                    )
                    .when(!desc.is_empty(), move |row| {
                        row.child(
                            div()
                                .text_size(theme::fs(10.5))
                                .text_color(theme::text_tertiary())
                                .child(desc),
                        )
                    }),
            )
            .child(
                div()
                    .id(SharedString::from(format!("{id}-button")))
                    .flex_none()
                    .px(px(9.))
                    .py(px(3.))
                    .rounded(px(theme::RADIUS_SMALL))
                    .bg(theme::track())
                    .cursor_pointer()
                    .text_size(theme::fs(11.))
                    .text_color(theme::text_secondary())
                    .hover(|style| style.bg(theme::surface_hover()))
                    .on_click(move |_, window, cx| on_click(window, cx))
                    .child(label),
            )
            .into_any_element()
    }

    /// 设备下拉行：标题 + 说明 + 右边一个「当前设备」按钮。
    ///
    /// 以前是把所有设备铺成一排标签，七台机器配长名字能折出五六行，
    /// 整页被拉得很长、标题列也被挤没 —— 设备列表用下拉，一行就够。
    fn device_picker_row(
        &self,
        id: &'static str,
        title: &'static str,
        description: &str,
        current: &str,
        hint: &'static str,
        which: DevicePicker,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let open = matches!(self.picker, Some((kind, _, _)) if kind == which);
        let shown = if current.is_empty() {
            SharedString::from(hint)
        } else {
            SharedString::from(current.to_string())
        };
        let dim = current.is_empty();

        div()
            .flex()
            .items_center()
            .gap(px(10.))
            .px(px(9.))
            .py(px(5.))
            .rounded(px(theme::RADIUS_ROW))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .gap(px(2.))
                    .child(
                        div()
                            .text_size(theme::fs(13.))
                            .text_color(theme::text_primary())
                            .child(title),
                    )
                    .child(
                        div()
                            .text_size(theme::fs(10.5))
                            .truncate()
                            .text_color(theme::text_tertiary())
                            .child(description.to_string()),
                    ),
            )
            .child(
                div()
                    .id(SharedString::from(id))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .flex_none()
                    .pl(px(9.))
                    .pr(px(7.))
                    .py(px(4.))
                    .rounded(px(theme::RADIUS_SMALL))
                    .bg(theme::track())
                    .cursor_pointer()
                    .hover(|style| style.bg(theme::surface_hover()))
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                            // 根节点那条「点别处」不该把刚打开的下拉又关掉。
                            cx.stop_propagation();
                            if matches!(this.picker, Some((kind, _, _)) if kind == which) {
                                this.picker = None;
                            } else {
                                this.picker_scroll = gpui::ScrollHandle::new();
                                this.picker = Some((
                                    which,
                                    event.position.x.as_f32(),
                                    event.position.y.as_f32(),
                                ));
                            }
                            refresh_ui(window, cx);
                        }),
                    )
                    .child(
                        div()
                            .max_w(px(200.))
                            .min_w_0()
                            .truncate()
                            .text_size(theme::fs(11.5))
                            .text_color(if dim {
                                theme::text_tertiary()
                            } else {
                                theme::text_primary()
                            })
                            .child(shown),
                    )
                    .child(icon(
                        if open { "chevron-up" } else { "chevron-down" },
                        11.0,
                        theme::text_tertiary(),
                    )),
            )
            .into_any_element()
    }

    /// 步进行：标题 + [−] 数值 [+]。
    ///
    /// 对数下限这种「五档数值」用分段选择会在窄列里挤成好几行，
    /// 换成步进器一行就能放下。
    fn stepper_row(
        &self,
        id: &'static str,
        title: &'static str,
        value_text: String,
        on_step: impl Fn(i32, &mut Window, &mut App) + 'static,
    ) -> AnyElement {
        let on_step = Rc::new(on_step);
        let button = |slot: u32, glyph: &'static str, delta: i32| {
            let on_step = on_step.clone();
            div()
                .id(SharedString::from(format!("{id}-{slot}")))
                .size(px(20.))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(4.))
                .cursor_pointer()
                .text_size(theme::fs(12.))
                .text_color(theme::text_secondary())
                .hover(|style| style.bg(theme::surface_hover()))
                .on_click(move |_, window, cx| on_step(delta, window, cx))
                .child(glyph)
        };

        div()
            .flex()
            .items_center()
            .gap(px(10.))
            .px(px(9.))
            .py(px(5.))
            .rounded(px(theme::RADIUS_ROW))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(theme::fs(12.5))
                    .text_color(theme::text_primary())
                    .truncate()
                    .child(title),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(1.))
                    .p(px(2.))
                    .rounded(px(theme::RADIUS_SMALL))
                    .bg(theme::track())
                    .child(button(0, "−", -1))
                    .child(
                        div()
                            .w(px(48.))
                            .flex_none()
                            .text_size(theme::fs(11.))
                            .text_color(theme::text_primary())
                            .text_center()
                            .child(value_text),
                    )
                    .child(button(1, "+", 1)),
            )
            .into_any_element()
    }

    /// 可自由填写的数值行：标题 + [−] 数值 [+]。
    ///
    /// 中间那个数字框点一下就能直接敲：0-9 追加、Backspace 退格、
    /// Enter 确认、Esc 放弃，点别处也算确认。之所以自己做，是因为这个
    /// gpui 版本没有输入框控件，而步进器只给固定几档又满足不了自由填写。
    fn number_row(
        &self,
        id: &'static str,
        title: &'static str,
        description: &'static str,
        field: NumberField,
        config: &Config,
        lang: Lang,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .flex()
            .items_center()
            .gap(px(10.))
            .px(px(9.))
            .py(px(5.))
            .rounded(px(theme::RADIUS_ROW))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .gap(px(2.))
                    .child(
                        div()
                            .text_size(theme::fs(13.))
                            .text_color(theme::text_primary())
                            .child(title),
                    )
                    .when(!description.is_empty(), |row| {
                        row.child(
                            div()
                                .text_size(theme::fs(10.5))
                                .text_color(theme::text_tertiary())
                                .child(description),
                        )
                    }),
            )
            .child(self.number_stepper(id, field, field.value(config), lang, cx))
            .into_any_element()
    }

    /// 一个数字控件（− 值 +），不带标题和说明。
    ///
    /// 从 `number_row` 里抽出来给顶部那行「连续触发」用：那行要塞三样东西，
    /// 用不了「标题一行 + 说明一行 + 控件居右」的两行版式。
    fn number_stepper(
        &self,
        id: &'static str,
        field: NumberField,
        current: i32,
        lang: Lang,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let editing = match &self.editing {
            Some((active, buffer)) if *active == field => Some(buffer.as_str()),
            _ => None,
        };
        let text = match editing {
            // 没有真输入框，用一个细竖条当光标，让人看出正在输。
            Some(buffer) => format!("{buffer}▏"),
            None => field.display(current, lang),
        };
        let active = editing.is_some();

        let step = |slot: u32, glyph: &'static str, direction: i32, cx: &mut Context<Self>| {
            div()
                .id(SharedString::from(format!("{id}-{slot}")))
                .size(px(22.))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(4.))
                .cursor_pointer()
                .text_size(theme::fs(14.))
                .text_color(theme::text_secondary())
                .hover(|style| style.bg(theme::surface_hover()))
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    let value = field.stepped(field.value(this.config.borrow().get()), direction);
                    this.set_number(field, value, window, cx);
                }))
                .child(glyph)
        };

        let input = div()
            .id(SharedString::from(format!("{id}-value")))
            // 「20.0 dB」比「100%」宽，带小数的字段给宽一点，别被截成省略号；
            // 「20000 ms」比两者都长，再单给一档。
            .w(px(match field {
                other if other.allows_decimal() => 80.,
                _ => 68.,
            }))
            .flex_none()
            .py(px(2.))
            .rounded(px(4.))
            .cursor_pointer()
            .when(active, |style| style.bg(theme::accent()))
            .text_size(theme::fs(14.))
            .text_color(theme::text_primary())
            .text_center()
            .truncate()
            .hover(|style| style.opacity(0.8))
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.begin_edit(field, window, cx)
            }))
            .child(text);

        div()
            .flex()
            .items_center()
            .gap(px(1.))
            .p(px(2.))
            .rounded(px(theme::RADIUS_SMALL))
            .bg(theme::track())
            .child(step(0, "−", -1, cx))
            .child(input)
            .child(step(1, "+", 1, cx))
            .into_any_element()
    }

    /// 托盘数字某一档位数的一整组外观（字号 / 宽度 / 高度 / 字距）。
    ///
    /// 一行放四个小格子而不是四行 —— 三档位数一共十二项，按 `number_row` 那样
    /// 一行一个要占一整屏。
    fn digit_metrics_group(
        &self,
        digits: u8,
        title: &'static str,
        config: &Config,
        lang: Lang,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let cells = [
            (TrayDigitMetric::SizePt, lang.pick("字号", "Size")),
            (TrayDigitMetric::Width, lang.pick("宽度", "Width")),
            (TrayDigitMetric::Height, lang.pick("高度", "Height")),
            (TrayDigitMetric::Tracking, lang.pick("字距", "Tracking")),
        ];
        let mut row = div()
            .flex()
            .items_center()
            .gap(px(10.))
            .px(px(9.))
            .py(px(5.))
            .rounded(px(theme::RADIUS_ROW));
        for (metric, label) in cells {
            row = row.child(self.digit_metric_cell(digits, metric, label, config, lang, cx));
        }
        div()
            .flex()
            .flex_col()
            .gap(px(2.))
            .child(
                div()
                    .text_size(theme::fs(12.))
                    .text_color(theme::text_secondary())
                    .child(title),
            )
            .child(row)
            .into_any_element()
    }

    /// 上面那一组里的一个小格子：`标签 + 数字框`（点数字进入输入）。
    fn digit_metric_cell(
        &self,
        digits: u8,
        metric: TrayDigitMetric,
        label: &'static str,
        config: &Config,
        lang: Lang,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let field = NumberField::TrayDigit(digits, metric);
        let current = field.value(config);
        let editing = match &self.editing {
            Some((active, buffer)) if *active == field => Some(buffer.as_str()),
            _ => None,
        };
        let text = match editing {
            Some(buffer) => format!("{buffer}▏"),
            None => field.display(current, lang),
        };
        let active = editing.is_some();
        div()
            .flex()
            .items_center()
            .gap(px(4.))
            .child(
                div()
                    .text_size(theme::fs(11.))
                    .text_color(theme::text_tertiary())
                    .child(label),
            )
            .child(
                div()
                    .id(SharedString::from(format!(
                        "tray-digit-{digits}-{metric:?}"
                    )))
                    .w(px(64.))
                    .flex_none()
                    .py(px(2.))
                    .rounded(px(4.))
                    .bg(theme::track())
                    .cursor_pointer()
                    .when(active, |style| style.bg(theme::accent()))
                    .text_size(theme::fs(12.5))
                    .text_color(theme::text_primary())
                    .text_center()
                    .truncate()
                    .hover(|style| style.opacity(0.8))
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.begin_edit(field, window, cx)
                    }))
                    .child(text),
            )
            .into_any_element()
    }

    /// 落一个数值：夹到合法区间、写盘、立刻重画。
    fn set_number(        &mut self,
        field: NumberField,
        value: i32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let (low, high) = field.range();
        let value = value.clamp(low, high);
        if field.value(self.config.borrow().get()) == value {
            refresh_ui(window, cx);
            return;
        }
        let store = self.config.clone();
        let _ = store
            .borrow_mut()
            .update(|config| field.apply(config, value));
        if field.touches_tray() {
            // 托盘数字的外观变了，图标得照新的字号 / 宽高 / 字距重新光栅化。
            crate::gui::refresh_tray_icon(cx);
        }
        if field == NumberField::VolumeCap {
            // 上限变了要立刻压一遍，别等到下一次 tick。
            crate::gui::refresh_volume_cap(cx);
        }
        if matches!(
            field,
            NumberField::LoudnessLevel | NumberField::LoudnessSmooth
        ) {
            // 响度参数只活在音频线程里，得推一份新的过去；累积的平均值会被清空重攒。
            crate::gui::refresh_loudness(cx);
        }
        if field == NumberField::BrightnessWidth {
            // 宽度变了：亮度窗要重排（滑条长度、预设折行都跟着变）；
            // 贴在面板右侧时，面板本身也得重新让位。
            crate::gui::refresh_flyout_size(cx);
        }
        if field == NumberField::BrightnessVerticalHeight {
            // 竖向高度变了：亮度窗重排（推子长度跟着变），面板可能也得让位。
            crate::gui::refresh_flyout_size(cx);
        }
        if field == NumberField::FlyoutVerticalHeight {
            // 音量面板高度变了；亮度面板选「跟随」时也跟着一起变。
            crate::gui::refresh_flyout_size(cx);
        }
        if field == NumberField::FlyoutWidth {
            // 音量面板宽度变了：整块重排，亮度窗如果跟着它也得挪一下。
            crate::gui::refresh_flyout_size(cx);
        }
        if matches!(
            field,
            NumberField::PanelFontSize
                | NumberField::PanelLetterSpacing
                | NumberField::PanelLineHeight
        ) {
            // 面板字号 / 行高直接决定面板宽高，整块要重排（亮度面板跟着挪）；
            // 字间距只影响文字宽度，但重排一次不花什么，一起刷掉。
            crate::gui::refresh_flyout_size(cx);
        }
        if matches!(
            field,
            NumberField::UiFontScale | NumberField::UiOpacity | NumberField::UiContrast
        ) {
            // 字号 / 不透明度 / 对比度是「界面级」的：先推进主题层，再让这扇窗自己立刻变。
            //
            // 字号变了整窗都要重排（行高跟着字号走）。面板有自己的一套字号
            // （`panel_font_scale`），这里顺手刷一次只是让面板按最新配置重排，不影响它的字号。
            crate::gui::apply_theme_extras(&self.config.borrow().get().clone());
            crate::windows::platform::set_opacity(window, theme::ui_opacity());
            if field == NumberField::UiFontScale {
                crate::gui::refresh_flyout_size(cx);
            }
            if field == NumberField::UiContrast {
                // 调色板是全局的：面板、OSD 也得跟着换色。
                cx.refresh_windows();
            }
        }
        refresh_ui(window, cx);
    }

    /// 点数字框：进入手动输入状态。
    fn begin_edit(&mut self, field: NumberField, window: &mut Window, cx: &mut Context<Self>) {
        if self.capturing.is_some() {
            self.cancel_capture(window, cx);
        }
        self.editing = Some((field, String::new()));
        self.focus.focus(window, cx);
        refresh_ui(window, cx);
    }

    /// 确认手动输入。空输入（直接点走）按「没改」处理。
    fn commit_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((field, buffer)) = self.editing.take() else {
            return;
        };
        let current = field.value(self.config.borrow().get());
        let value = field.parse_edit(&buffer, current);
        self.set_number(field, value, window, cx);
    }

    /// 放弃手动输入（Esc）。
    fn cancel_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.editing.take().is_none() {
            return;
        }
        refresh_ui(window, cx);
    }

    /// 手动输入状态的按键；返回 true 表示这个键已经被吃掉。
    fn handle_edit_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.editing.is_none() {
            return false;
        }
        let key = event.keystroke.key.as_str();
        match key {
            "enter" => {
                self.commit_edit(window, cx);
                return true;
            }
            "escape" => {
                self.cancel_edit(window, cx);
                return true;
            }
            _ => {}
        }

        let Some((field, buffer)) = self.editing.as_mut() else {
            return false;
        };
        let decimal = field.allows_decimal();
        let max_len = field.edit_len();
        let handled = if key == "backspace" {
            buffer.pop();
            true
        } else {
            let mut chars = key.chars();
            match (chars.next(), chars.next()) {
                (Some(digit), None) if digit.is_ascii_digit() => {
                    // 百分比 / 像素三位数够用，dB 步进还要留出 "20.0" 四个字符。
                    if buffer.len() < max_len {
                        buffer.push(digit);
                    }
                    true
                }
                // dB 步进允许一个小数点："0.5" → 5（十分之一 dB）。
                (Some('.'), None) if decimal && !buffer.contains('.') && !buffer.is_empty() => {
                    buffer.push('.');
                    true
                }
                // 字间距可以收紧到负数：负号只能打在最前面，长度也算进额度。
                (Some('-'), None) if field.allows_negative() && buffer.is_empty() => {
                    buffer.push('-');
                    true
                }
                _ => false,
            }
        };
        if handled {
            refresh_ui(window, cx);
        }
        handled
    }

    /// 点应用音量那一格的数字：进入就地输入。
    fn begin_app_edit(&mut self, index: usize, window: &mut Window, cx: &mut Context<Self>) {
        if self.capturing.is_some() {
            self.cancel_capture(window, cx);
        }
        // 同一时刻只留一个输入框：数值框那边先确认掉。
        self.commit_edit(window, cx);
        self.app_edit = Some((index, String::new()));
        self.focus.focus(window, cx);
        refresh_ui(window, cx);
    }

    /// 确认应用音量：空输入 / 停在同样的数字都按「没改」处理。
    fn commit_app_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some((index, buffer)) = self.app_edit.take() else {
            return;
        };
        let percent = buffer
            .trim()
            .parse::<u32>()
            .ok()
            .map(|value| value.min(100));
        if let Some(percent) = percent {
            let store = self.config.clone();
            let _ = store.borrow_mut().update(|config| {
                if let Some(app) = config.startup.app_volumes.get_mut(index) {
                    app.percent = percent;
                }
            });
        }
        refresh_ui(window, cx);
    }

    /// 放弃应用音量的手动输入（Esc）。
    fn cancel_app_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.app_edit.take().is_none() {
            return;
        }
        refresh_ui(window, cx);
    }

    /// 应用音量就地输入的按键；返回 true 表示这个键已经被吃掉。
    fn handle_app_edit_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.app_edit.is_none() {
            return false;
        }
        let key = event.keystroke.key.as_str();
        match key {
            "enter" => {
                self.commit_app_edit(window, cx);
                return true;
            }
            "escape" => {
                self.cancel_app_edit(window, cx);
                return true;
            }
            _ => {}
        }

        let Some((_, buffer)) = self.app_edit.as_mut() else {
            return false;
        };
        let handled = if key == "backspace" {
            buffer.pop();
            true
        } else {
            let mut chars = key.chars();
            match (chars.next(), chars.next()) {
                (Some(digit), None) if digit.is_ascii_digit() => {
                    if buffer.len() < 3 {
                        buffer.push(digit);
                    }
                    true
                }
                _ => false,
            }
        };
        if handled {
            refresh_ui(window, cx);
        }
        handled
    }

    /// 点托盘提示的编辑区：进入编辑（光标停在最后一行的末尾）。
    fn begin_tooltip_edit(
        &mut self,
        target: TooltipTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.capturing.is_some() {
            self.cancel_capture(window, cx);
        }
        let (config, lang) = {
            let store = self.config.borrow();
            (store.get().clone(), store.get().language)
        };
        self.tooltip_edit = Some(TooltipEditor::new(
            target,
            tooltip_lines_for(&config, target, lang),
        ));
        self.focus.focus(window, cx);
        refresh_ui(window, cx);
    }

    /// 提交模板：写盘、立刻按新内容刷新托盘提示。
    fn commit_tooltip_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(editor) = self.tooltip_edit.take() else {
            return;
        };
        let target = editor.target;
        let lines = editor.trimmed();
        let store = self.config.clone();
        let changed = {
            let config = store.borrow();
            let current = match target {
                TooltipTarget::Normal => &config.get().tooltip.lines,
                TooltipTarget::Muted => &config.get().tooltip.muted_lines,
            };
            current != &lines
        };
        if changed {
            let _ = store.borrow_mut().update(|config| match target {
                TooltipTarget::Normal => config.tooltip.lines = lines,
                TooltipTarget::Muted => config.tooltip.muted_lines = lines,
            });
            crate::gui::refresh_tray_icon(cx);
        }
        refresh_ui(window, cx);
    }

    /// 放弃这次编辑（Esc）。
    fn cancel_tooltip_edit(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.tooltip_edit.take().is_none() {
            return;
        }
        refresh_ui(window, cx);
    }

    /// 往模板里插一段文字（变量按钮 / 粘贴）。
    fn insert_tooltip_text(&mut self, text: &str, window: &mut Window, cx: &mut Context<Self>) {
        if self.tooltip_edit.is_none() {
            self.begin_tooltip_edit(TooltipTarget::Normal, window, cx);
        }
        if let Some(editor) = self.tooltip_edit.as_mut() {
            editor.insert(text);
        }
        self.focus.focus(window, cx);
        refresh_ui(window, cx);
    }

    /// 模板编辑器的按键；返回 true 表示这个键已经被吃掉。
    ///
    /// 这里接管的是「文本编辑」而不是「快捷键」：回车换行、方向键挪光标、
    /// Ctrl+V 粘贴、Esc 放弃，其余可打印字符照 `key_char` 插进来。
    fn handle_tooltip_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.tooltip_edit.is_none() {
            return false;
        }
        let keystroke = &event.keystroke;
        let key = keystroke.key.as_str();
        let control = keystroke.modifiers.control;
        let shift = keystroke.modifiers.shift;

        // Ctrl+A 全选 / Ctrl+C 复制选区：拖动选完总要能整块搬走。
        if control && key == "a" {
            if let Some(editor) = self.tooltip_edit.as_mut() {
                editor.select_all();
            }
            refresh_ui(window, cx);
            return true;
        }
        // Ctrl+Z 撤销（Shift+Ctrl+Z 留给「重做」，暂时不做，别把它当撤销吞掉）。
        if control && !shift && key == "z" {
            if let Some(editor) = self.tooltip_edit.as_mut() {
                if editor.undo() {
                    refresh_ui(window, cx);
                }
            }
            return true;
        }
        if control && key == "c" {
            if let Some(text) = self
                .tooltip_edit
                .as_ref()
                .map(|editor| editor.selected_text())
                .filter(|text| !text.is_empty())
            {
                cx.write_to_clipboard(gpui::ClipboardItem::new_string(text));
            }
            return true;
        }

        if control && key == "v" {
            let text = cx
                .read_from_clipboard()
                .and_then(|item| item.text().map(|text| text.to_string()));
            if let Some(text) = text {
                if let Some(editor) = self.tooltip_edit.as_mut() {
                    editor.snapshot();
                    editor.paste(&text);
                }
                refresh_ui(window, cx);
            }
            return true;
        }

        let handled = match key {
            "escape" => {
                self.cancel_tooltip_edit(window, cx);
                return true;
            }
            // 下面几支都会改字：进去先记一步，Ctrl+Z 才好退。
            "enter" => {
                if let Some(editor) = self.tooltip_edit.as_mut() {
                    editor.snapshot();
                    editor.split_line();
                }
                true
            }
            "backspace" => {
                if let Some(editor) = self.tooltip_edit.as_mut() {
                    editor.snapshot();
                    editor.backspace();
                }
                true
            }
            "delete" => {
                if let Some(editor) = self.tooltip_edit.as_mut() {
                    editor.snapshot();
                    editor.delete_forward();
                }
                true
            }
            "up" => {
                if let Some(editor) = self.tooltip_edit.as_mut() {
                    if shift {
                        editor.extend_selection();
                    } else if editor.collapse_selection(false) {
                        // 有选区时按方向键只是把选区收掉。
                    } else {
                        editor.move_up();
                    }
                }
                true
            }
            "down" => {
                if let Some(editor) = self.tooltip_edit.as_mut() {
                    if shift {
                        editor.extend_selection();
                    } else if editor.collapse_selection(true) {
                    } else {
                        editor.move_down();
                    }
                }
                true
            }
            "left" => {
                if let Some(editor) = self.tooltip_edit.as_mut() {
                    if shift {
                        editor.extend_selection();
                        editor.move_left();
                    } else if !editor.collapse_selection(false) {
                        editor.move_left();
                    }
                }
                true
            }
            "right" => {
                if let Some(editor) = self.tooltip_edit.as_mut() {
                    if shift {
                        editor.extend_selection();
                        editor.move_right();
                    } else if !editor.collapse_selection(true) {
                        editor.move_right();
                    }
                }
                true
            }
            "home" => {
                if let Some(editor) = self.tooltip_edit.as_mut() {
                    if shift {
                        editor.extend_selection();
                        editor.col = 0;
                    } else if !editor.collapse_selection(false) {
                        editor.col = 0;
                    }
                }
                true
            }
            "end" => {
                if let Some(editor) = self.tooltip_edit.as_mut() {
                    let len = editor.current().chars().count();
                    if shift {
                        editor.extend_selection();
                        editor.col = len;
                    } else if !editor.collapse_selection(true) {
                        editor.col = len;
                    }
                }
                true
            }
            // Ctrl / Alt 组合留给全局快捷键，别被模板编辑器吞掉。
            _ if control || keystroke.modifiers.alt => false,
            _ => {
                let typed = keystroke.key_char.clone();
                match typed {
                    Some(text) if !text.is_empty() => {
                        if let Some(editor) = self.tooltip_edit.as_mut() {
                            editor.snapshot();
                            for ch in text.chars() {
                                editor.insert_char(ch);
                            }
                        }
                        true
                    }
                    _ => false,
                }
            }
        };
        if handled {
            refresh_ui(window, cx);
        }
        handled
    }

    /// 模板编辑区：多行、带光标、点行定位。
    fn tooltip_editor(
        &self,
        target: TooltipTarget,
        config: &Config,
        lang: Lang,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let editing = matches!(&self.tooltip_edit, Some(editor) if editor.target == target);
        let lines = match (&self.tooltip_edit, editing) {
            (Some(editor), true) => editor.lines.clone(),
            _ => tooltip_lines_for(config, target, lang),
        };
        let caret = match (&self.tooltip_edit, editing) {
            (Some(editor), true) => Some((editor.row, editor.col)),
            _ => None,
        };
        // 选区（前 → 后），没在编辑 / 没选中时是 None。
        let selection = match (&self.tooltip_edit, editing) {
            (Some(editor), true) => editor.selection(),
            _ => None,
        };

        let rows: Vec<AnyElement> = lines
            .iter()
            .enumerate()
            .map(|(index, text)| {
                let col = caret.filter(|(row, _)| *row == index).map(|(_, col)| col);
                let tag = match target {
                    TooltipTarget::Normal => "n",
                    TooltipTarget::Muted => "m",
                };
                let chars: Vec<char> = text.chars().collect();
                let len = chars.len();
                let col = col.map(|col| col.min(len));
                let active = col.is_some();
                let caret_bar = || {
                    div()
                        .w(px(1.5))
                        .h(px(13.))
                        .flex_none()
                        .bg(theme::accent())
                        .into_any_element()
                };
                let is_selected = |glyph: usize| {
                    selection.is_some_and(|(start, end)| {
                        start <= (index, glyph) && (index, glyph) < end
                    })
                };
                // 选中跨行时，行尾那段空白也算在选区里 —— 不涂上看着是断开的。
                let tail_selected = selection.is_some_and(|(start, end)| {
                    let tail = (index, len);
                    start <= tail && tail < end
                });

                // 每行从左到右铺满：行首内边距（6px）→ 每个字的左右两半 → 行尾空白。
                // 中间不留接不到鼠标的缝，否则拖到缝上端点就不动了，像是「拖着拖着卡住」。
                // 只在左边留内边距：右边的留白交给行尾那块，不然那 6px 也会变成缝。
                let mut row = div()
                    .id(SharedString::from(format!("tooltip-line-{tag}-{index}")))
                    .flex()
                    .h(px(19.))
                    .pl(px(6.))
                    .rounded(px(3.))
                    .when(active, |style| style.bg(theme::surface_hover()))
                    .text_size(theme::fs(11.5))
                    .text_color(if text.is_empty() {
                        theme::text_tertiary()
                    } else {
                        theme::text_primary()
                    })
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                            // 根节点那条「点别处提交」不该在点编辑区时触发。
                            cx.stop_propagation();
                            if !matches!(&this.tooltip_edit, Some(editor) if editor.target == target)
                            {
                                this.begin_tooltip_edit(target, window, cx);
                            }
                            if let Some(editor) = this.tooltip_edit.as_mut() {
                                // 命中行本身只会是行首那 6px 内边距（字和行尾空白各有自己的
                                // 命中区），所以这里按「行首」算，别把锚点丢到行尾。
                                editor.begin_selection(index, 0, event.modifiers.shift);
                            }
                            refresh_ui(window, cx);
                        }),
                    )
                    .on_mouse_move(cx.listener(
                        move |this, event: &gpui::MouseMoveEvent, window, cx| {
                            // 拖到行首那片留白上：端点收到行首。字和行尾各有命中区，
                            // 拖动时它们会先吃掉事件，轮不到这里。
                            drag_caret(this, target, index, 0, event, window, cx);
                        },
                    ));

                // 每个字都是一个小块，点到哪个字就把光标放到那个字前面 ——
                // 「点哪站哪」，不用字体度量（这套自绘文本拿不到每个字的坐标）。
                // 没在编辑时也照这样铺，否则「第一下点击」只能落到行尾。
                //
                // 拖动选择也靠它：一个字再切成左右两半（透明命中区），
                // 左半 = 光标在这个字前面、右半 = 这个字后面，按住拖过去就是选它。
                // 字块撑满整行高度（19px），这样上下不留缝，鼠标斜着拖不会突然卡住。
                for (glyph_index, ch) in chars.iter().enumerate() {
                    if col == Some(glyph_index) {
                        row = row.child(caret_bar());
                    }
                    let selected = is_selected(glyph_index);
                    let mut glyph = div()
                        .id(SharedString::from(format!(
                            "tooltip-glyph-{tag}-{index}-{glyph_index}"
                        )))
                        .relative()
                        .flex_none()
                        .flex()
                        .items_center()
                        .h(px(19.))
                        .when(selected, |style| style.bg(theme::accent_glow()))
                        .child(ch.to_string());

                    for right in [false, true] {
                        let to_col = glyph_index + usize::from(right);
                        glyph = glyph.child(
                            div()
                                .id(SharedString::from(format!(
                                    "tooltip-hit-{tag}-{index}-{glyph_index}-{}",
                                    u8::from(right)
                                )))
                                .absolute()
                                .top_0()
                                .bottom_0()
                                .left(relative(if right { 0.5 } else { 0.0 }))
                                .w(relative(0.5))
                                .cursor(gpui::CursorStyle::IBeam)
                                .on_mouse_down(
                                    gpui::MouseButton::Left,
                                    cx.listener(
                                        move |this, event: &gpui::MouseDownEvent, window, cx| {
                                            cx.stop_propagation();
                                            if !matches!(
                                                &this.tooltip_edit,
                                                Some(editor) if editor.target == target
                                            ) {
                                                this.begin_tooltip_edit(target, window, cx);
                                            }
                                            if let Some(editor) = this.tooltip_edit.as_mut() {
                                                editor.begin_selection(
                                                    index,
                                                    to_col,
                                                    event.modifiers.shift,
                                                );
                                            }
                                            refresh_ui(window, cx);
                                        },
                                    ),
                                )
                                .on_mouse_move(cx.listener(
                                    move |this, event: &gpui::MouseMoveEvent, window, cx| {
                                        drag_caret(
                                            this, target, index, to_col, event, window, cx,
                                        );
                                    },
                                )),
                        );
                    }
                    row = row.child(glyph);
                }
                if col == Some(len) {
                    row = row.child(caret_bar());
                }
                // 行尾空白：撑满这一行剩下的宽度，点这里等于按 End。
                // 顺带负责「点空白也能进编辑」—— 一行字很短时，这点空白才是大头。
                let end = len;
                row = row.child(
                    div()
                        .id(SharedString::from(format!("tooltip-tail-{tag}-{index}")))
                        .flex_1()
                        .min_w(px(0.))
                        .h(px(19.))
                        .when(tail_selected, |style| style.bg(theme::accent_glow()))
                        .on_mouse_down(
                            gpui::MouseButton::Left,
                            cx.listener(
                                move |this, event: &gpui::MouseDownEvent, window, cx| {
                                    cx.stop_propagation();
                                    if !matches!(
                                        &this.tooltip_edit,
                                        Some(editor) if editor.target == target
                                    ) {
                                        this.begin_tooltip_edit(target, window, cx);
                                    }
                                    if let Some(editor) = this.tooltip_edit.as_mut() {
                                        editor.begin_selection(index, end, event.modifiers.shift);
                                    }
                                    refresh_ui(window, cx);
                                },
                            ),
                        )
                        .on_mouse_move(cx.listener(
                            move |this, event: &gpui::MouseMoveEvent, window, cx| {
                                drag_caret(this, target, index, end, event, window, cx);
                            },
                        )),
                );
                row.into_any_element()
            })
            .collect();

        div()
            .id(SharedString::from(format!(
                "tooltip-box-{}",
                match target {
                    TooltipTarget::Normal => "n",
                    TooltipTarget::Muted => "m",
                }
            )))
            .mx(px(9.))
            .mb(px(4.))
            .py(px(3.))
            .flex()
            .flex_col()
            .rounded(px(theme::RADIUS_SMALL))
            .bg(theme::surface())
            .border_1()
            .border_color(theme::border())
            .cursor(gpui::CursorStyle::IBeam)
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, _event, window, cx| {
                    cx.stop_propagation();
                    // 已经在编辑就别重来一遍 —— 那会把刚拖好的选区清掉。
                    if !matches!(&this.tooltip_edit, Some(editor) if editor.target == target) {
                        this.begin_tooltip_edit(target, window, cx);
                    }
                }),
            )
            // 松手就结束这次拖动：之后鼠标再滑过编辑区也不该改选区。
            .on_mouse_up(
                gpui::MouseButton::Left,
                cx.listener(move |this, _event: &gpui::MouseUpEvent, _window, _cx| {
                    if let Some(editor) = this.tooltip_edit.as_mut() {
                        editor.drag = false;
                    }
                }),
            )
            .children(rows)
            .into_any_element()
    }

    /// 变量按钮：点一下插到光标处。
    ///
    /// 头三个不是变量，是**对齐标记**：插到某一行里（写哪一行算哪一行），
    /// 那一行就按标记对齐，托盘的全局对齐方式对其它行照旧。标记本身不上屏。
    fn tooltip_chips(&self, lang: Lang, cx: &mut Context<Self>) -> AnyElement {
        let tokens = [
            "{left}",
            "{center}",
            "{right}",
            "{detail}",
            "{volume}",
            "{percent}",
            "{db}",
            "{muted}",
            "{device}",
            "{device_desc}",
            "{state}",
            "{apps}",
            "{app_count}",
            "{mode}",
            "{cap}",
            "{time}",
            "{name}",
        ];
        let chips: Vec<AnyElement> = tokens
            .iter()
            .enumerate()
            .map(|(index, token)| {
                let token = *token;
                // 前三个是对齐标记：换个底色，一眼能看出它们不是内容。
                let is_align = index < 3;
                div()
                    .id(SharedString::from(format!(
                        "tooltip-chip-{}",
                        token.trim_matches(['{', '}'])
                    )))
                    .px(px(6.))
                    .py(px(2.))
                    .rounded(px(theme::RADIUS_SMALL))
                    .bg(if is_align {
                        theme::accent_glow()
                    } else {
                        theme::track()
                    })
                    .cursor_pointer()
                    .text_size(theme::fs(10.))
                    .text_color(if is_align {
                        theme::accent()
                    } else {
                        theme::text_secondary()
                    })
                    .hover(|style| style.bg(theme::surface_hover()))
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(move |this, _event, window, cx| {
                            // 别让根节点把这次点击当成「点别处」而提交。
                            cx.stop_propagation();
                            this.insert_tooltip_text(token, window, cx);
                        }),
                    )
                    .child(token)
                    .into_any_element()
            })
            .collect();

        div()
            .flex()
            .flex_wrap()
            .gap(px(4.))
            .px(px(9.))
            .py(px(4.))
            .children(chips)
            .child(
                div()
                    .text_size(theme::fs(9.5))
                    .text_color(theme::text_tertiary())
                    .child(lang.pick(
                        "点一下插到光标处；{} 之外的字原样保留。{left} {center} {right} 写在哪一行，哪一行就单独按它对齐",
                        "Click to insert at the caret; text outside {} is kept. {left} {center} {right} align just the line they sit on",
                    )),
            )
            .into_any_element()
    }

    /// 模板的实时预览（用当前设备、当前正在发声的应用算）。
    ///
    /// 没在编辑时预览配置里已经存的内容，编辑时才预览手上这份 —— 这样
    /// 刚打开这一页也能立刻看到效果，而不是空着一片。
    fn tooltip_preview_text(
        &self,
        target: TooltipTarget,
        config: &Config,
        snapshot: &Snapshot,
        lang: Lang,
    ) -> String {
        let editing = matches!(&self.tooltip_edit, Some(editor) if editor.target == target);
        let lines = match (&self.tooltip_edit, editing) {
            (Some(editor), true) => editor.trimmed(),
            _ => match target {
                TooltipTarget::Normal => config.tooltip.lines.clone(),
                TooltipTarget::Muted => config.tooltip.muted_lines.clone(),
            },
        };
        let device = preview_device(snapshot, lang);
        let mut facts = crate::gui::tooltip_facts(snapshot, config, &device);
        let mut preview = config.tooltip.clone();
        preview.enabled = true;
        match target {
            TooltipTarget::Normal => preview.lines = lines,
            TooltipTarget::Muted => {
                preview.muted_enabled = true;
                preview.muted_lines = lines;
                facts.muted = true;
            }
        }
        render_tooltip(&preview, &facts, lang)
    }

    /// 一段预览：小标题 + 按行拆开的实际内容（空内容也明确写出来）。
    fn tooltip_preview(&self, text: String, lang: Lang) -> AnyElement {
        let lines: Vec<AnyElement> = if text.is_empty() {
            vec![
                div()
                    .text_size(theme::fs(11.))
                    .text_color(theme::text_tertiary())
                    .child(lang.pick("（空，托盘上不显示文字）", "(empty — nothing shown)"))
                    .into_any_element(),
            ]
        } else {
            text.split("\r\n")
                .map(|line| {
                    // 居中靠补前导空格，而排版会把行首空格吃掉：预览里换成左内边距，
                    // 这样看到的缩进和托盘上那两行是一致的。
                    let lead = line.chars().take_while(|ch| *ch == ' ').count();
                    let body: String = line.chars().skip(lead).collect();
                    div()
                        .flex()
                        .items_center()
                        .h(px(15.))
                        .when(lead > 0, |row| row.pl(px(lead as f32 * 4.6)))
                        .text_size(theme::fs(11.))
                        .text_color(theme::text_secondary())
                        .child(body)
                        .into_any_element()
                })
                .collect()
        };
        div()
            .flex()
            .flex_col()
            .gap(px(1.))
            .px(px(9.))
            .py(px(5.))
            .child(
                div()
                    .text_size(theme::fs(9.5))
                    .text_color(theme::text_tertiary())
                    .child(lang.pick("预览", "Preview")),
            )
            .children(lines)
            .into_any_element()
    }

    /// 「托盘提示」一整段：开关 + 变量按钮 + 两个多行模板（正常 / 静音）。
    fn tooltip_rows(
        &self,
        config: &Config,
        lang: Lang,
        snapshot: &Snapshot,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let mut rows: Vec<AnyElement> = Vec::new();

        let toggle = {
            let store = self.config.clone();
            self.toggle_row(
                "tooltip-enabled",
                lang.pick("自定义悬浮提示", "Custom tooltip"),
                // 打开后展开的编辑区已经说明了一切，这句解释是多余的。
                "",
                config.tooltip.enabled,
                move |window, cx| {
                    let lang = store.borrow().get().language;
                    let _ = store.borrow_mut().update(|config| {
                        config.tooltip.enabled = !config.tooltip.enabled;
                        // 第一次打开：把默认两行填进去，用户一进来就有东西可改。
                        if config.tooltip.enabled && config.tooltip.lines.is_empty() {
                            config.tooltip.lines =
                                default_tooltip_lines(lang, TooltipTarget::Normal);
                        }
                    });
                    crate::gui::refresh_tray_icon(cx);
                    refresh_ui(window, cx);
                },
            )
        };
        rows.push(toggle);
        if !config.tooltip.enabled {
            return rows;
        }

        // 对齐：系统提示框没有对齐参数，居中 / 靠右靠逐行补空格做近似。
        // 想单独调某一行，就在那行里插一个对齐标记（下面变量条里那三个）。
        let align_row = {
            let store = self.config.clone();
            self.choice_row_owned(
                "tooltip-align",
                lang.pick("对齐方式", "Alignment"),
                lang.pick(
                    "居中 / 靠右会给短行补空格；自定义完全照编辑框里的空格显示",
                    "Center / right pad short lines with spaces; custom keeps your spacing as typed",
                ),
                vec![
                    (0u32, lang.pick("靠左", "Left").to_string()),
                    (1u32, lang.pick("居中", "Center").to_string()),
                    (2u32, lang.pick("靠右", "Right").to_string()),
                    (3u32, lang.pick("自定义", "Custom").to_string()),
                ],
                config.tooltip.align.index(),
                move |value, window, cx| {
                    let align = crate::config::TooltipAlign::from_index(value);
                    let _ = store
                        .borrow_mut()
                        .update(|config| config.tooltip.align = align);
                    crate::gui::refresh_tray_icon(cx);
                    refresh_ui(window, cx);
                },
            )
        };
        // 模板编辑这一套（对齐 / 变量条 / 编辑框 / 预览）收进一张内嵌卡片，
        // 和「启动套用」那边开关展开的子项保持同一套版式。
        let preview = self.tooltip_preview_text(TooltipTarget::Normal, config, snapshot, lang);
        rows.push(self.sub_group(vec![
            align_row,
            self.tooltip_chips(lang, cx),
            self.tooltip_editor(TooltipTarget::Normal, config, lang, cx),
            self.tooltip_preview(preview, lang),
        ]));

        // 静音是另一种状态，可以单独写一套。
        let muted_toggle = {
            let store = self.config.clone();
            self.toggle_row(
                "tooltip-muted",
                lang.pick("静音时单独显示", "Separate text when muted"),
                lang.pick(
                    "关掉就沿用上面那一套；打开后静音时用下面这份",
                    "Off reuses the text above; on shows the text below while muted",
                ),
                config.tooltip.muted_enabled,
                move |window, cx| {
                    let lang = store.borrow().get().language;
                    let _ = store.borrow_mut().update(|config| {
                        config.tooltip.muted_enabled = !config.tooltip.muted_enabled;
                        if config.tooltip.muted_enabled && config.tooltip.muted_lines.is_empty() {
                            config.tooltip.muted_lines =
                                default_tooltip_lines(lang, TooltipTarget::Muted);
                        }
                    });
                    crate::gui::refresh_tray_icon(cx);
                    refresh_ui(window, cx);
                },
            )
        };
        rows.push(muted_toggle);
        if config.tooltip.muted_enabled {
            let preview = self.tooltip_preview_text(TooltipTarget::Muted, config, snapshot, lang);
            rows.push(self.sub_group(vec![
                self.tooltip_editor(TooltipTarget::Muted, config, lang, cx),
                self.tooltip_preview(preview, lang),
            ]));
        }

        // 变量表：一行小字，说明每个变量是什么意思。
        rows.push(
            div()
                .px(px(9.))
                .pb(px(5.))
                .text_size(theme::fs(9.5))
                .text_color(theme::text_tertiary())
                .child(lang.pick(
                    "{detail} 读数（对数模式带百分比） · {volume} 读数 · {percent} 百分比 · {db} 分贝 · {muted} 静音标记 · {device} 当前播放设备 · {device_desc} 设备全名 · {state} 静音/正常 · {apps} 正在发声的应用 · {app_count} 应用数 · {mode} 刻度模式 · {cap} 音量上限 · {time} 时间 · {name} Bolume 音量 · {left}/{center}/{right} 只对齐所在那一行",
                    "{detail} reading (dB with percent) · {volume} reading · {percent} percent · {db} dB · {muted} muted marker · {device} current device · {device_desc} full device name · {state} muted/normal · {apps} playing apps · {app_count} app count · {mode} scale · {cap} volume cap · {time} clock · {name} Bolume Volume · {left}/{center}/{right} align just their own line",
                ))
                .into_any_element(),
        );

        // 恢复默认：清空模板，再按语言填回内置内容。
        rows.push({
            let store = self.config.clone();
            let limit = if lang.is_en() {
                format!("Up to {MAX_TOOLTIP_LINES} lines, {MAX_TOOLTIP_LINE_CHARS} chars each")
            } else {
                format!("最多 {MAX_TOOLTIP_LINES} 行，每行 {MAX_TOOLTIP_LINE_CHARS} 个字符")
            };
            self.action_row(
                "tooltip-reset",
                lang.pick("恢复默认内容", "Restore default text"),
                limit,
                lang.pick("恢复", "Restore"),
                move |window, cx| {
                    let lang = store.borrow().get().language;
                    let _ = store.borrow_mut().update(|config| {
                        config.tooltip.lines = default_tooltip_lines(lang, TooltipTarget::Normal);
                        if config.tooltip.muted_enabled {
                            config.tooltip.muted_lines =
                                default_tooltip_lines(lang, TooltipTarget::Muted);
                        }
                    });
                    crate::gui::refresh_tray_icon(cx);
                    refresh_ui(window, cx);
                },
            )
        });

        rows
    }

    /// 设备行：点一下切换默认播放设备，右侧小按钮禁用 / 启用。
    fn device_row(
        &self,
        device: &DeviceInfo,
        selected: bool,
        lang: Lang,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let engine = self.engine.clone();
        let device_id = device.id.clone();
        let enabled = device.enabled;
        let name_color = if !enabled {
            theme::text_tertiary()
        } else if selected {
            theme::text_primary()
        } else {
            theme::text_secondary()
        };
        // 自定义名字之后，副标题换成系统原名（原名只在设置页里露出）。
        let (label, aliased, icon_name) = {
            let store = self.config.borrow();
            let cfg = store.get();
            (
                cfg.device_label(&device.id, &device.name),
                cfg.device_alias(&device.id).is_some(),
                device_icon_resolved(cfg, &device.id, device.kind).into_owned(),
            )
        };
        let renaming = self
            .device_rename
            .as_ref()
            .is_some_and(|(id, _)| *id == device.id);
        let subtitle = if aliased {
            DeviceLabel::Description(format!(
                "{}{}",
                lang.pick("原名：", "Original: "),
                device.name
            ))
        } else if device.description.is_empty() {
            DeviceLabel::Default
        } else {
            DeviceLabel::Description(device.description.clone())
        };

        // 换图标：和「改名」并排的小按钮。
        //
        // 左边那颗图标本身也能点（最直观），但 22px 的热区在一整行里太窄，
        // 点歪一下就把默认设备换掉了 —— 再给一个带文字的明确入口。
        let pick_icon = {
            let id = device.id.clone();
            let opening = self
                .icon_picker
                .as_ref()
                .is_some_and(|(other, _, _)| *other == device.id);
            div()
                .id(SharedString::from(format!(
                    "settings-device-icon-pick-{}",
                    device.id
                )))
                .flex_none()
                .px(px(6.))
                .py(px(2.))
                .rounded(px(4.))
                .text_size(theme::fs(10.5))
                .cursor_pointer()
                .text_color(if opening {
                    theme::accent()
                } else {
                    theme::text_tertiary()
                })
                .hover(|style| style.bg(theme::surface_hover()))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                        this.icon_picker = Some((
                            id.clone(),
                            event.position.x.as_f32(),
                            event.position.y.as_f32(),
                        ));
                        cx.notify();
                    }),
                )
                .child(lang.pick("图标", "Icon"))
        };

        // 改名：只在设置页里出现，点了就地编辑（中文走输入法）。
        let rename = {
            let id = device.id.clone();
            div()
                .id(SharedString::from(format!(
                    "settings-device-rename-{}",
                    device.id
                )))
                .flex_none()
                .px(px(6.))
                .py(px(2.))
                .rounded(px(4.))
                .text_size(theme::fs(10.5))
                .cursor_pointer()
                .text_color(if renaming {
                    theme::accent()
                } else {
                    theme::text_tertiary()
                })
                .hover(|style| style.bg(theme::surface_hover()))
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(move |this, _: &gpui::MouseDownEvent, window, cx| {
                        // 别让这一下同时把设备切成默认设备。
                        cx.stop_propagation();
                        // 已经在改名：别把刚敲的字清空重来（换一行点的时候捕获相位
                        // 已经替上一行落过盘，这里 `None`）。
                        if this.device_rename.is_some() || this.rename_just_committed(&id) {
                            return;
                        }
                        // 从空白开始输入：系统原名往往几十个字符，留成前缀的话
                        // 光标在末尾，用户得先退格一路删完才能改名。
                        this.device_rename = Some((id.clone(), String::new()));
                        this.rename_cursor = 0;
                        this.text_marked = None;
                        this.focus.focus(window, cx);
                        cx.notify();
                    }),
                )
                .child(lang.pick("改名", "Rename"))
        };

        // 禁用 / 启用：走 IPolicyConfig::SetEndpointVisibility，和系统「声音」
        // 面板里右键「禁用设备」是同一件事。
        let power = {
            let engine = self.engine.clone();
            let power_id = device.id.clone();
            div()
                .id(SharedString::from(format!(
                    "settings-device-power-{}",
                    device.id
                )))
                .flex_none()
                .px(px(6.))
                .py(px(2.))
                .rounded(px(4.))
                .text_size(theme::fs(10.5))
                .cursor_pointer()
                .text_color(if enabled {
                    theme::text_tertiary()
                } else {
                    theme::accent()
                })
                .hover(|style| style.bg(theme::surface_hover()))
                .on_click(cx.listener(move |_, _: &ClickEvent, window, cx| {
                    // 按钮长在整行里：这一下只切启用状态，别顺手换掉默认设备。
                    cx.stop_propagation();
                    engine.send(AudioCommand::SetDeviceEnabled {
                        device_id: power_id.clone(),
                        enabled: !enabled,
                    });
                    refresh_ui(window, cx);
                }))
                .child(if enabled {
                    lang.pick("禁用", "Disable")
                } else {
                    lang.pick("启用", "Enable")
                })
        };

        div()
            .id(SharedString::from(format!("settings-device-{}", device.id)))
            .flex()
            .items_center()
            .gap(px(9.))
            .px(px(9.))
            .py(px(5.))
            .rounded(px(theme::RADIUS_ROW))
            .cursor_pointer()
            .when(selected, |style| style.bg(theme::surface_hover()))
            .hover(|style| style.bg(theme::surface_hover()))
            .on_click(cx.listener(move |this, _: &ClickEvent, _, _| {
                // 已禁用的设备接不上音量接口，点它没有意义；
                // 正在改名的时候这一下也别把设备切走。
                if !enabled || this.device_rename.is_some() {
                    return;
                }
                // 还有一点：点别处落盘发生在这一下的**按下**阶段，等 `click` 来了
                // 改名态已经收掉了 —— 不拦一下，用户「点这一行来收尾改名」就顺手
                // 把默认播放设备换了。刚提交过的这一下不算切换。
                if this
                    .rename_commit_at
                    .is_some_and(|at| at.elapsed() < RENAME_CLICK_GRACE)
                {
                    return;
                }
                engine.send(AudioCommand::SetDefaultDevice {
                    device_id: device_id.clone(),
                });
            }))
            .child({
                let id = device.id.clone();
                div()
                    .id(SharedString::from(format!(
                        "settings-device-icon-{}",
                        device.id
                    )))
                    .size(px(24.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(4.))
                    .cursor_pointer()
                    .hover(|style| style.bg(theme::surface_hover()))
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |this, event: &gpui::MouseDownEvent, _, cx| {
                            // 这一下只开图标选择器，别顺手把设备切成默认设备。
                            cx.stop_propagation();
                            this.icon_picker = Some((
                                id.clone(),
                                event.position.x.as_f32(),
                                event.position.y.as_f32(),
                            ));
                            cx.notify();
                        }),
                    )
                    .child(icon(
                        &icon_name,
                        18.0,
                        if selected {
                            theme::accent()
                        } else {
                            theme::text_secondary()
                        },
                    ))
            })
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .gap(px(1.))
                    .child(if renaming {
                        self.rename_text_field(label.clone(), cx)
                    } else {
                        div()
                            .text_size(theme::fs(12.5))
                            .text_color(name_color)
                            .truncate()
                            .child(label.clone())
                            .into_any_element()
                    })
                    .child(
                        div()
                            .text_size(theme::fs(10.5))
                            .text_color(theme::text_tertiary())
                            .truncate()
                            .child(match subtitle {
                                DeviceLabel::Default => {
                                    if enabled {
                                        lang.pick("默认播放设备", "Default playback device")
                                            .to_string()
                                    } else {
                                        lang.pick("已禁用", "Disabled").to_string()
                                    }
                                }
                                DeviceLabel::Description(text) => text,
                            }),
                    ),
            )
            .when(selected, |style| {
                style.child(icon("check", 15.0, theme::accent()))
            })
            .child(pick_icon)
            .child(rename)
            .child(power)
            .into_any_element()
    }

    /// 快捷键行：连发勾选 + 标题 + 三组按键框，点哪一组就录哪一组
    /// （Backspace 解除绑定、Esc 放弃；媒体键也认）。
    ///
    /// 最前面那个方框管「按住不松手要不要连发」：勾上写进
    /// `hotkey_repeat_actions`，总闸（`hotkey_hold_repeat`）关着时一律不连发。
    fn hotkey_row(
        &self,
        id: &'static str,
        title: &'static str,
        slots: HotkeySlots,
        slot: HotkeySlot,
        action: crate::platform::hotkey::Action,
        lang: Lang,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut chips = div().flex().items_center().gap(px(4.)).flex_none();
        for index in 0..HOTKEY_SLOTS {
            let hotkey = slots.get(index);
            let capturing = self.capturing == Some((slot, index));
            let text = if capturing {
                lang.pick("按键…", "Press…").to_string()
            } else {
                crate::platform::hotkey::display_text(hotkey)
                    .unwrap_or_else(|| lang.pick("未设置", "Not set").to_string())
            };
            let mut chip = div()
                .id(SharedString::from(format!("{id}-{index}")))
                // 三格等宽、铺满右侧那一段：左边名字区本来就很空，
                // 按钮窄了反而难认（「Ctrl+Alt+Num0」要完整放得下）。
                .w(px(124.))
                .px(px(4.))
                .py(px(3.))
                .rounded(px(theme::RADIUS_SMALL))
                .bg(if capturing {
                    theme::accent()
                } else {
                    theme::track()
                })
                .text_size(theme::fs(10.))
                .text_color(if capturing || !hotkey.is_empty() {
                    theme::text_primary()
                } else {
                    theme::text_tertiary()
                })
                .cursor_pointer()
                .hover(|style| style.opacity(0.8))
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.begin_capture(slot, index, window, cx)
                }));

            // 录入中：右边贴一个小 ×，点它直接清空格子 —— 比记「按 Backspace」好找，
            // 也不用先退出录制。
            //
            // 用绝对定位 + `occlude()` 叠在格子上：格子自己挂着 `on_click`（点了开始录），
            // 里面再嵌一个可点的子元素，命中会被外层吃掉（这个项目里踩过好几次）。
            // 按下就处理、顺手 stop_propagation，外层就不会又开一次录制。
            if capturing {
                // 右边留出 × 的位置，别让「按键…」压到它底下。
                chip = chip.relative().pr(px(18.)).child(
                    div()
                        .id(SharedString::from(format!("{id}-{index}-clear")))
                        .absolute()
                        .right(px(2.))
                        .top(px(1.))
                        .size(px(14.))
                        .occlude()
                        .flex()
                        .items_center()
                        .justify_center()
                        .rounded(px(3.))
                        .cursor_pointer()
                        .text_size(theme::fs(10.5))
                        .text_color(theme::text_primary())
                        .hover(|style| {
                            style
                                .bg(theme::surface_active())
                                .text_color(theme::danger())
                        })
                        .on_mouse_down(
                            gpui::MouseButton::Left,
                            cx.listener(move |this, _, window, cx| {
                                cx.stop_propagation();
                                this.just_cleared = Some((slot, index, std::time::Instant::now()));
                                this.commit_capture(Hotkey::default(), window, cx);
                            }),
                        )
                        .child("×"),
                );
            }
            chip = chip.text_center().truncate().child(text);
            chips = chips.child(chip);
        }

        // 连发勾选：配置里存的是动作名（`Action::key`），点一下增删一项。
        //
        // 「勾没勾」只看名单里有没有这个动作，跟总闸（`hotkey_hold_repeat`）无关 ——
        // 总闸只是把它暂停，不该顺手把每个人挑过的键清掉；关掉再打开，原来勾的还在。
        let hold_on = self.config.borrow().get().hotkey_hold_repeat;
        let repeat_on = self
            .config
            .borrow()
            .get()
            .hotkey_repeat_actions
            .iter()
            .any(|name| name == action.key());
        let toggle_repeat = self.bind(move |config| {
            let name = action.key().to_string();
            match config
                .hotkey_repeat_actions
                .iter()
                .position(|item| *item == name)
            {
                Some(index) => {
                    config.hotkey_repeat_actions.remove(index);
                }
                None => config.hotkey_repeat_actions.push(name),
            }
        });
        let check_box = div()
            .id(SharedString::from(format!("{id}-repeat")))
            .flex_none()
            .size(px(14.))
            .rounded(px(3.5))
            .border(px(1.))
            .border_color(if repeat_on {
                theme::accent()
            } else {
                theme::border_strong()
            })
            .bg(if repeat_on {
                theme::accent()
            } else {
                theme::surface_sunken()
            })
            .flex()
            .items_center()
            .justify_center()
            .cursor_pointer()
            .hover(|style| style.border_color(theme::accent()))
            .on_click(move |_, window, cx| toggle_repeat(window, cx))
            .on_hover(cx.listener(move |this, hovered: &bool, _window, cx| {
                let next = if *hovered {
                    Some(id)
                } else if this.repeat_tip == Some(id) {
                    None
                } else {
                    this.repeat_tip
                };
                if this.repeat_tip != next {
                    this.repeat_tip = next;
                    cx.notify();
                }
            }))
            .when(repeat_on, |style| {
                style.child(icon("check", 11.0, theme::text_primary()))
            });

        // 悬浮气泡：光标停在方框上才画出来。
        //
        // 位置跟着行内坐标走（`left(26)` = 方框右边留 4px），高度顶满整行 ——
        // 不越界才不会被下一行或卡片的 `overflow_hidden` 盖掉。
        let tip_on = self.repeat_tip == Some(id);
        let tip = div()
            .absolute()
            .left(px(26.))
            .top(px(0.))
            .bottom(px(0.))
            .pl(px(7.))
            .pr(px(9.))
            .flex()
            .items_center()
            .rounded(px(theme::RADIUS_SMALL))
            .bg(theme::surface_elevated())
            .border(px(1.))
            .border_color(theme::border())
            .text_size(theme::fs(10.))
            .text_color(theme::text_secondary())
            .child(lang.pick(
                "按住这个键不松手时连续触发",
                "Repeats while the key is held down",
            ));

        div()
            .id(SharedString::from(id))
            .relative()
            .flex()
            .items_center()
            .gap(px(8.))
            .px(px(9.))
            .py(px(5.))
            .rounded(px(theme::RADIUS_ROW))
            // 勾选这一列只在「按住连续触发」开着时出现：关掉之后整列收起，
            // 行也回到原来那样紧凑。
            .when(hold_on, |row| row.child(check_box))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(theme::fs(12.5))
                    .text_color(theme::text_primary())
                    .truncate()
                    .child(title),
            )
            .child(chips)
            // 最后画：压在标题上面，才像一条浮出来的提示。
            .when(hold_on && tip_on, |row| row.child(tip))
            .into_any_element()
    }

    /// 开始录制某个快捷键。
    ///
    /// 录之前先把手上的全局热键全撤掉：已经注册的组合会被系统直接截走，
    /// 设置窗口根本收不到（EarTrumpet 的 `HotkeyManager.Pause` 同理）。
    /// 媒体键则相反：`RegisterHotKey` 收不到它们，让低级钩子在录制期间接管。
    fn begin_capture(
        &mut self,
        slot: HotkeySlot,
        index: usize,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 刚被 × 清掉的那一格：紧接着冒上来的 click 别再开录制。
        if let Some((last_slot, last_index, at)) = self.just_cleared {
            if last_slot == slot
                && last_index == index
                && at.elapsed() < std::time::Duration::from_millis(400)
            {
                return;
            }
        }
        self.capturing = Some((slot, index));
        self.focus.focus(window, cx);
        crate::platform::hotkey::apply(&[]);
        crate::platform::hotkey::set_capturing(true);
        refresh_ui(window, cx);
    }

    /// 放弃录制：热键注册按配置原样恢复。
    fn cancel_capture(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.capturing.take().is_none() {
            return;
        }
        crate::platform::hotkey::set_capturing(false);
        let config = self.config.borrow().get().clone();
        crate::platform::hotkey::apply_config(&config);
        window.blur();
        refresh_ui(window, cx);
    }

    /// 当前是否在录快捷键（全局快捷键派发时先问这个）。
    pub fn is_capturing(&self) -> bool {
        self.capturing.is_some()
    }

    /// 收下一个按键并落子（媒体键由钩子送进来时走这条路）。
    pub fn accept_captured(&mut self, hotkey: Hotkey, window: &mut Window, cx: &mut Context<Self>) {
        self.commit_capture(hotkey, window, cx);
    }

    /// 记下组合、落盘、重新注册，退出录制。
    fn commit_capture(&mut self, hotkey: Hotkey, window: &mut Window, cx: &mut Context<Self>) {
        let Some((slot, index)) = self.capturing.take() else {
            return;
        };
        crate::platform::hotkey::set_capturing(false);
        let store = self.config.clone();
        let _ = store
            .borrow_mut()
            .update(|config| slot.set(config, index, hotkey));
        let config = store.borrow().get().clone();
        crate::platform::hotkey::apply_config(&config);
        window.blur();
        refresh_ui(window, cx);
    }

    /// 录制状态下的按键：照搬 EarTrumpet 的 `HotkeyViewModel.OnPreviewKeyDown`。
    fn handle_capture_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.capturing.is_none() {
            return;
        }
        let keystroke = &event.keystroke;

        // Tab 永远让出去（EarTrumpet 同样直接 return，不消费）。
        if keystroke.key == "tab" {
            return;
        }
        // 其余按键全部吃掉，别让 gpui 把它当成快捷键或文本输入。
        cx.stop_propagation();

        // Backspace：解除这一格的绑定（界面上的提示也是这么写的）。
        if keystroke.key == "backspace" {
            self.commit_capture(Hotkey::default(), window, cx);
            return;
        }
        // Esc：放弃本次录制，原来绑着的键保持不动。
        if keystroke.key == "escape" {
            self.cancel_capture(window, cx);
            return;
        }

        let Some((key, needs_shift)) = crate::platform::hotkey::vk_for_key_name(&keystroke.key)
        else {
            // 只按修饰键（或认不出的键）：不落子，继续等主键。
            return;
        };
        // gpui 只给字符，`Numpad0` 和主键盘的 `0` 都是 `"0"` —— 再问一次系统，
        // 按住的是小键盘那颗就记小键盘的键码，两者不会互相顶掉。
        let (key, needs_shift) =
            crate::platform::hotkey::resolve_numpad(&keystroke.key, key, needs_shift);
        let mut mods = modifiers_bits(&keystroke.modifiers);
        if needs_shift {
            mods |= MOD_SHIFT;
        }
        self.commit_capture(Hotkey::new(mods, key), window, cx);
    }

    /// 开关打开后展开的子项统一包成这种内嵌小卡片。
    ///
    /// 版式和启动页的设备名单一致：比 section 的 `surface_sunken` 亮一档 + 一圈描边，
    /// 左右各缩进 5px，形成「卡片里的卡片」—— 一眼能看出这些行属于上面那个开关，
    /// 而不是和开关平级的另一项设置。
    fn sub_group(&self, children: Vec<AnyElement>) -> AnyElement {
        div()
            .mx(px(5.))
            .my(px(4.))
            .p(px(3.))
            .flex()
            .flex_col()
            .rounded(px(theme::RADIUS_SMALL))
            .bg(theme::surface_elevated())
            .border_1()
            .border_color(theme::border_strong())
            .children(children)
            .into_any_element()
    }

    /// 页面内的分页签（快捷键：音频 / 屏幕；外观：界面 / 面板 / 托盘）。
    ///
    /// 样式是分段控件：一整条 `track` 底，选中的那格升到 `accent_soft`。
    /// 页签只是界面状态，不落进配置 —— 每次打开都从第一页开始。
    fn tab_bar(
        &self,
        id: &'static str,
        tabs: &'static [(&'static str, &'static str)],
        active: usize,
        target: TabTarget,
        lang: Lang,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let mut row = div()
            .flex()
            .items_center()
            .gap(px(2.))
            .p(px(2.))
            .flex_none()
            .rounded(px(theme::RADIUS_SMALL))
            .bg(theme::track());
        for (index, &(zh, en)) in tabs.iter().enumerate() {
            let on = index == active;
            row = row.child(
                div()
                    .id(SharedString::from(format!("{id}-{index}")))
                    .flex_1()
                    .h(px(24.))
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(4.))
                    .cursor_pointer()
                    .text_size(theme::fs(11.5))
                    .when(on, |style| {
                        style
                            .bg(theme::accent_soft())
                            .text_color(theme::text_primary())
                    })
                    .when(!on, |style| style.text_color(theme::text_secondary()))
                    .hover(|style| style.bg(theme::surface_hover()))
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        let page = match target {
                            TabTarget::Shortcuts => {
                                this.shortcut_tab = index;
                                Page::Shortcuts
                            }
                            TabTarget::Devices => {
                                this.device_tab = index;
                                // 显示器那个页签先把亮度重读一遍：别把开机时的那次
                                // 失败一直挂在页面上。
                                if index == 1 {
                                    crate::gui::refresh_displays(cx);
                                }
                                Page::Devices
                            }
                            TabTarget::Volume => {
                                this.volume_tab = index;
                                Page::Volume
                            }
                        };
                        // 换页签等于换一整页内容：滚动位置跟着回到顶部，
                        // 否则新内容一上来就停在半截。
                        this.scrolls[page.index()].set_offset(gpui::point(px(0.), px(0.)));
                        this.page_anim = this.page_anim.wrapping_add(1);
                        refresh_ui(window, cx);
                    }))
                    .child(lang.pick(zh, en)),
            );
        }
        row.into_any_element()
    }

    fn section(
        &self,
        title: &'static str,
        icon_name: &'static str,
        children: Vec<AnyElement>,
    ) -> AnyElement {
        // 整个大类套一层外框（标题也圈在里面）：
        //
        //   页面（surface_window）
        //   └ 大类（surface_sunken + 细边框）
        //     └ 子卡片（surface_elevated + 亮边框，见 `sub_group`）
        //
        // 以前只有子卡片有底色，大类之间就靠标题那一行小字分界，扫一眼看不出
        // 哪几行属于同一组。外框一加，「开关 + 它展开的子项」自然成了一个小盒。
        div()
            .flex()
            .flex_col()
            .flex_none()
            .gap(px(5.))
            .rounded(px(theme::RADIUS_CARD))
            .border_1()
            .border_color(theme::border())
            .bg(theme::surface_sunken())
            .p(px(7.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .px(px(2.))
                    .child(icon(icon_name, 12.0, theme::text_tertiary()))
                    .child(
                        div()
                            .text_size(theme::fs(11.))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme::text_tertiary())
                            .child(title),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .rounded(px(theme::RADIUS_ROW))
                    .p(px(3.))
                    .children(children),
            )
            .into_any_element()
    }
}

enum DeviceLabel {
    Default,
    Description(String),
}

/// GPUI 的修饰符状态转成 `RegisterHotKey` 的 MOD_* 位。
fn modifiers_bits(modifiers: &Modifiers) -> u32 {
    let mut bits = 0;
    if modifiers.control {
        bits |= MOD_CONTROL;
    }
    if modifiers.alt {
        bits |= MOD_ALT;
    }
    if modifiers.shift {
        bits |= MOD_SHIFT;
    }
    if modifiers.platform {
        bits |= MOD_WIN;
    }
    bits
}

/// 改完配置后立刻重画。
///
/// `App::refresh_windows()` 只是把窗口内部标记置脏；这个 gpui 版本在 Windows 上
/// 还要一条 WM_PAINT 才会真的走到绘制，否则界面要等到下一次尺寸/位置变化
/// （也就是用户拖一下窗口）才更新。`post_repaint` 补的就是这条消息。
fn refresh_ui(window: &mut Window, cx: &mut App) {
    cx.refresh_windows();
    crate::windows::platform::post_repaint(window);
}

/// 内置音效的名字：就是听感，不用再解释。
fn sound_style_label(style: crate::config::VolumeSoundStyle, lang: Lang) -> &'static str {
    use crate::config::VolumeSoundStyle;
    match style {
        VolumeSoundStyle::Click => lang.pick("轻点", "Click"),
        VolumeSoundStyle::Tick => lang.pick("滴答", "Tick"),
        VolumeSoundStyle::Pop => lang.pick("气泡", "Pop"),
        VolumeSoundStyle::Wood => lang.pick("木鱼", "Wood"),
        VolumeSoundStyle::Ratchet => lang.pick("棘轮", "Ratchet"),
        VolumeSoundStyle::Blip => lang.pick("电子", "Blip"),
        VolumeSoundStyle::Chime => lang.pick("铃音", "Chime"),
        VolumeSoundStyle::Custom => lang.pick("自定义", "Custom"),
    }
}

/// 会话 key → 能看的短名字。
///
/// 响度均衡的设置页要列出「单独设置过」的应用，应用没在跑时快照里就查不到名字，
/// 只能从 key 里抠：`exe:c:\a\b\player.exe` → `player.exe`。
fn app_key_file_name(key: &str) -> String {
    key.strip_prefix("exe:")
        .and_then(|path| path.rsplit(['\\', '/']).next())
        .filter(|name| !name.is_empty())
        .unwrap_or(key)
        .to_string()
}

/// 换配色之后重画**所有**窗口：设置窗、音量面板、亮度窗、托盘菜单、OSD。
///
/// `refresh_ui` 只补自己这一条 WM_PAINT —— 换主题时开着的音量面板 / 提示条
/// 会停在旧配色上，看着就像「点了没反应」。
fn repaint_theme(window: &mut Window, cx: &mut App) {
    crate::gui::repaint_windows(cx);
    crate::windows::platform::post_repaint(window);
}

/// 小节标题：一行粗一点的标题 + 一句说明，右边不挂控件。
///
/// 主题预设删掉之后，「亮色主题 / 深色主题」只剩下面那两行色块，
/// 得有个东西把两组分开，否则两行「背景色 / 强调色」看着像重复了。
fn section_row(title: &str, description: &str) -> AnyElement {
    div()
        .flex()
        .flex_col()
        .gap(px(2.))
        .px(px(9.))
        .pt(px(10.))
        .pb(px(2.))
        .child(
            div()
                .text_size(theme::fs(13.))
                .text_color(theme::text_primary())
                .child(title.to_string()),
        )
        .child(
            div()
                .text_size(theme::fs(10.5))
                .text_color(theme::text_tertiary())
                .child(description.to_string()),
        )
        .into_any_element()
}

/// 黑名单改了以后同步给钩子线程。
///
/// 钩子里拿不到 gpui 的全局配置，它自己存了一份快照；不刷新的话
/// 「名单里加了程序、低级钩子照样吞键」这种症状看着像没生效。
fn sync_blocklist(store: &ConfigStore) {
    let config = store.get().clone();
    crate::platform::hotkey::apply_config(&config);
}

impl Settings {
    /// 左栏一格：图标 + 名称，选中的那格底色亮起来。
    fn nav_button(&self, page: Page, lang: Lang, cx: &mut Context<Self>) -> AnyElement {
        let active = self.page == page;
        div()
            .id(SharedString::from(format!("settings-nav-{}", page.id())))
            .h(px(NAV_HEIGHT))
            .w_full()
            .flex_none()
            .flex()
            .items_center()
            .gap(px(9.))
            .px(px(10.))
            .rounded(px(theme::RADIUS_ROW))
            .cursor_pointer()
            .when(active, |style| style.bg(theme::accent_soft()))
            .when(!active, |style| {
                style.hover(|style| style.bg(theme::surface_hover()))
            })
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                if this.page == page {
                    return;
                }
                this.page = page;
                this.page_anim = this.page_anim.wrapping_add(1);
                // 显示器那一签先把亮度重读一遍：别把开机时的那次失败一直挂在页面上。
                if page == Page::Devices && this.device_tab == 1 {
                    crate::gui::refresh_displays(cx);
                }
                refresh_ui(window, cx);
            }))
            .child(icon(
                page.icon(),
                15.0,
                if active {
                    theme::accent()
                } else {
                    theme::text_tertiary()
                },
            ))
            .child(
                div()
                    .text_size(theme::fs(12.5))
                    .text_color(if active {
                        theme::text_primary()
                    } else {
                        theme::text_secondary()
                    })
                    .truncate()
                    .child(page.title(lang)),
            )
            .into_any_element()
    }

    /// 左侧导航栏：上面是分页，底部是程序名与版本。
    fn sidebar(&self, lang: Lang, cx: &mut Context<Self>) -> AnyElement {
        let mut nav = div().flex().flex_col().gap(px(NAV_GAP)).w_full();
        for page in Page::ALL {
            nav = nav.child(self.nav_button(page, lang, cx));
        }

        div()
            .id("settings-sidebar")
            .w(px(SIDEBAR_WIDTH))
            .h_full()
            .flex_none()
            .flex()
            .flex_col()
            .border_r_1()
            .border_color(theme::border())
            .bg(theme::surface_sunken())
            .p(px(8.))
            .child(nav)
            .child(div().flex_1())
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap(px(1.))
                    .px(px(8.))
                    .py(px(4.))
                    .child(
                        div()
                            .text_size(theme::fs(11.))
                            .text_color(theme::text_secondary())
                            .child(format!("Bolume {}", crate::version())),
                    )
                    .child(
                        div()
                            .text_size(theme::fs(10.))
                            .text_color(theme::text_tertiary())
                            .child(lang.pick("音量合成器", "Volume mixer")),
                    ),
            )
            .into_any_element()
    }

    /// 设备列表（没设备时给一行占位）。
    /// 设备列表：活动设备一组，已禁用的单独放一组（方便再启用回来）。
    fn device_list(
        &self,
        snapshot: &Snapshot,
        lang: Lang,
        cx: &mut Context<Self>,
    ) -> (Vec<AnyElement>, Vec<AnyElement>) {
        let mut active = Vec::new();
        let mut disabled = Vec::new();
        for device in &snapshot.devices {
            if device.enabled {
                let selected = Some(device.id.as_str()) == snapshot.current_device_id.as_deref();
                active.push(self.device_row(device, selected, lang, cx));
            } else {
                disabled.push(self.device_row(device, false, lang, cx));
            }
        }
        if active.is_empty() {
            active.push(
                div()
                    .px(px(12.))
                    .py(px(12.))
                    .text_size(theme::fs(11.5))
                    .text_color(theme::text_tertiary())
                    .child(lang.pick("没有检测到播放设备", "No playback devices found"))
                    .into_any_element(),
            );
        }
        (active, disabled)
    }

    fn general_page(
        &self,
        config: &Config,
        lang: Lang,
        snapshot: &Snapshot,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        // 开机自启显示的是系统里的**实际**状态，不是配置里那个布尔值：
        // 任务管理器「启动」页里改过、或者在任务计划程序里删过任务之后，
        // 配置并不知道 —— 用户关掉再打开，这里显示的还是老样子就成了谎话。
        // 开关的翻转方向也按实际状态算，免得配置落后一格时点一下「反着来」。
        //
        // 方式同理：实际登记在哪边就显示哪边（都没有时按配置里记的那个）。
        let method = crate::platform::autostart::detected().unwrap_or(config.launch_method);
        let autostart_on = crate::platform::autostart::is_enabled(method);
        // 启动时提权：任务那两档把它写进任务定义的 RunLevel，注册表那档往 Run 值里加参数。
        let admin = config.run_as_admin;
        let autostart = self.toggle_row(
            "toggle-autostart",
            lang.pick("开机自动启动", "Launch at login"),
            lang.pick(
                "登录 Windows 后自动在托盘运行",
                "Start in the tray after you sign in",
            ),
            autostart_on,
            self.bind(move |config| {
                let enable = !autostart_on;
                config.launch_at_login = enable;
                // 顺手把方式也按实际登记的那个写回去：配置落后一格时（系统里是
                // 注册表、配置记着计划任务），照配置开就会在系统里留下两条。
                config.launch_method = method;
                crate::platform::autostart::set_enabled(method, enable, config.run_as_admin);
            }),
        );

        // 两档：注册表 / 计划任务（登录）。「系统启动时启动」已下线 —— 那一档要管理员
        // 注册、还要跨会话交接，远端机器上问题多，不做了。
        let launch_method = {
            let store = self.config.clone();
            self.choice_row_owned(
                "launch-method",
                lang.pick("选择开机自启方式", "Pick the launch method"),
                lang.pick(
                    "用户登录时启动由任务计划程序在会话建立时拉起，比普通开机启动快一两秒",
                    "Sign-in start is launched by Task Scheduler as soon as your session exists — a second or two ahead of the standard startup entry",
                ),
                vec![
                    (0u32, lang.pick("普通开机启动", "Standard start").to_string()),
                    (
                        1u32,
                        lang.pick("用户登录时启动[任务计划程序]", "At sign-in [Task Scheduler]")
                            .to_string(),
                    ),
                ],
                method.index(),
                move |value, window, cx| {
                    let next = crate::config::LaunchMethod::from_index(value);
                    let now = crate::platform::autostart::detected()
                        .unwrap_or(store.borrow().get().launch_method);
                    if next == now {
                        return;
                    }
                    let (was_on, admin) = {
                        let store = store.borrow();
                        (
                            crate::platform::autostart::is_enabled(now),
                            store.get().run_as_admin,
                        )
                    };
                    // 三档互斥：先把原来那条彻底撤掉，开着的话再在新方式上建一条。
                    // 顺序不能反 —— 中间那一下会两条同时在，启动两次。
                    let _ = store
                        .borrow_mut()
                        .update(|config| config.launch_method = next);
                    crate::platform::autostart::set_enabled(now, false, admin);
                    if was_on {
                        crate::platform::autostart::set_enabled(next, true, admin);
                    }
                    refresh_ui(window, cx);
                },
            )
        };

        // 启动时以管理员运行。计划任务那一档靠 RunLevel（任务计划程序静默提权），
        // 注册表那档只能自己 runas 重开一次。
        let run_as_admin = {
            let store = self.config.clone();
            self.toggle_row(
                "toggle-run-as-admin",
                lang.pick("启动时以管理员运行", "Run as administrator at startup"),
                lang.pick(
                    "以最高权限启动；计划任务方式静默提权，注册表方式需 UAC 确认",
                    "Start with highest privileges; scheduled tasks raise it silently, a Run key asks for UAC once",
                ),
                admin,
                move |window, cx| {
                    let (on, method, enabled) = {
                        let mut store = store.borrow_mut();
                        let on = !store.get().run_as_admin;
                        let _ = store.update(|config| config.run_as_admin = on);
                        let config = store.get();
                        (on, config.launch_method, config.launch_at_login)
                    };
                    // 任务里的 RunLevel 和 Run 值里的参数都是登记时写死的，改完得重登一次。
                    if enabled {
                        crate::platform::autostart::set_enabled(method, true, on);
                    }
                    refresh_ui(window, cx);
                },
            )
        };

        // 没提权时的提示：只在「刚试过需要管理员权限的那件事、而且失败了」时出现，
        // 换成不需要权限的选项之后自动消失。用警示色 —— 这一句是「刚才没成功」，
        // 不是那种可以略过的小字说明。
        let admin_hint = crate::platform::autostart::admin_denied().then(|| {
            div()
                .px(px(9.))
                .pb(px(3.))
                .pt(px(1.))
                .text_size(theme::fs(10.5))
                .text_color(theme::danger())
                .child(lang.pick(
                    "需要管理员权限：请以管理员身份重新运行 Bolume",
                    "Administrator rights required — restart Bolume as administrator",
                ))
                .into_any_element()
        });

        // 「失去焦点时收起面板」和「应用列表」那两个开关都属于面板，
        // 摆在「面板」页里（是「这块面板怎么表现」，不是「软件怎么表现」）。

        let language = {
            let store = self.config.clone();
            self.choice_row(
                "language",
                lang.pick("语言", "Language"),
                lang.pick("界面语言", "Interface language"),
                &[(0, "中文"), (1, "English")],
                if config.language.is_en() { 1 } else { 0 },
                move |value, window, cx| {
                    let next = if value == 1 { Lang::En } else { Lang::Zh };
                    let _ = store.borrow_mut().update(|config| config.language = next);
                    crate::gui::refresh_tray_menu(cx);
                    refresh_ui(window, cx);
                },
            )
        };

        let mut general_rows = vec![autostart, launch_method, run_as_admin];
        if let Some(hint) = admin_hint {
            general_rows.push(hint);
        }
        general_rows.push(language);

        // 「启动时套用」和「应用音量」两张卡并进常规页：开机那一套本来就是一回事，
        // 单占一页反而不值当（内容也就两张卡）。
        let mut sections = vec![self.section(
            lang.pick("常规", "General"),
            "settings",
            general_rows,
        )];
        sections.extend(self.startup_page(config, lang, snapshot, cx));
        sections
    }

    /// 设备页：页签一是音频设备，页签二是显示器（亮度）。
    ///
    /// 两块都是「这台机器上有什么硬件、各自怎么用」，并成一页省一个导航项。
    fn devices_page(
        &self,
        config: &Config,
        lang: Lang,
        snapshot: &Snapshot,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let tabs = self.tab_bar(
            "device-tabs",
            &[("设备", "Devices"), ("显示器", "Displays")],
            self.device_tab,
            TabTarget::Devices,
            lang,
            cx,
        );
        let mut out = vec![tabs];
        if self.device_tab == 1 {
            out.extend(self.display_page(config, lang, cx));
            return out;
        }
        let (active, disabled) = self.device_list(snapshot, lang, cx);
        let mut sections = vec![self.section(
            lang.pick("播放设备", "Playback devices"),
            "headphones",
            active,
        )];
        if !disabled.is_empty() {
            sections.push(self.section(
                lang.pick("已禁用设备", "Disabled devices"),
                "volume-off",
                disabled,
            ));
        }
        out.extend(sections);
        out
    }

    /// 启动页：这些开关只影响「下次启动那一次」，之后怎么调都不会被改回去。
    fn startup_page(
        &self,
        config: &Config,
        lang: Lang,
        snapshot: &Snapshot,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let profile = &config.startup;

        // 默认设备
        let set_default_device = self.toggle_row(
            "startup-default-device",
            lang.pick("启动时设置默认设备", "Set the default device at startup"),
            lang.pick(
                "启动后自动把默认播放设备切到下面这台",
                "Switch the default playback device to the one below",
            ),
            profile.set_default_device,
            self.bind(|config| {
                config.startup.set_default_device = !config.startup.set_default_device
            }),
        );

        // 设备选择：选项是运行时来的（设备列表不固定），用下拉——铺成一排标签
        // 会把整行折出好几层，页面立刻变得很长。
        let device_picker = if profile.set_default_device {
            Some(self.device_picker_row(
                "startup-default-device-pick",
                lang.pick("启动设备", "Startup device"),
                lang.pick(
                    "启动后把默认设备切成这台",
                    "Switch the default device to this one at launch",
                ),
                &self.stored_device_label(&profile.default_device_id, &profile.default_device_name),
                lang.pick("选一台设备", "Pick a device"),
                DevicePicker::StartupDefault,
                cx,
            ))
        } else {
            None
        };

        // 设备开关规则：禁用 / 手动 / 黑名单 / 白名单，四选一。
        let device_mode = {
            let store = self.config.clone();
            let options: &'static [(u32, &'static str)] = if lang.is_en() {
                &[
                    (0, "Disabled"),
                    (1, "Manual"),
                    (2, "Blacklist"),
                    (3, "Whitelist"),
                ]
            } else {
                &[(0, "禁用"), (1, "手动"), (2, "黑名单"), (3, "白名单")]
            };
            let description = match profile.device_state_mode {
                crate::config::DeviceStateMode::Disabled => lang.pick(
                    "这套功能整个关掉，启动时一台设备都不动",
                    "The whole feature is off: no device is touched at startup",
                ),
                crate::config::DeviceStateMode::Manual => lang.pick(
                    "名单里每台自己选启用 / 禁用；名单外的设备保持系统原样",
                    "Pick on/off per device in the list; anything not listed stays as the system left it",
                ),
                crate::config::DeviceStateMode::Blacklist => lang.pick(
                    "名单里的设备启动时禁用；不在名单里的（含新接的）保持系统原样",
                    "Devices on the list are disabled at startup; anything else stays as the system left it",
                ),
                crate::config::DeviceStateMode::Whitelist => lang.pick(
                    "只有名单里的设备保留，其它一律禁用 —— 以后新接的设备也会被禁用",
                    "Only the listed devices survive; everything else is disabled, including devices plugged in later",
                ),
            };
            self.choice_row(
                "startup-device-mode",
                lang.pick("设备开关规则", "Device switch rule"),
                description,
                options,
                profile.device_state_mode.index(),
                move |value, window, cx| {
                    let mode = crate::config::DeviceStateMode::from_index(value);
                    let _ = store
                        .borrow_mut()
                        .update(|config| config.startup.device_state_mode = mode);
                    refresh_ui(window, cx);
                },
            )
        };
        let device_mode = Some(device_mode);
        let record_states = if profile.device_state_mode.uses_list() {
            // 每份名单各算各的，切模式就是在换一份数据。
            let list = profile.device_list(profile.device_state_mode);
            let enabled = list.iter().filter(|state| state.enabled).count();
            let disabled = list.len() - enabled;
            let store = self.config.clone();
            let current: Vec<(String, String, bool)> = snapshot
                .devices
                .iter()
                .map(|device| (device.id.clone(), device.name.clone(), device.enabled))
                .collect();
            let mode = profile.device_state_mode;
            // 三种模式的「记录」记的不是同一批设备：
            // 手动 = 全部设备（带各自的开关）；黑名单 = 现在被禁用的；白名单 = 现在开着的。
            let description = if current.is_empty() {
                // 一台设备都没扫到（音频服务刚起来 / 刚刚重扫）：
                // 这时候按「记录」会把名单清空，先说清楚。
                lang.pick("当前没有可记录的设备", "No devices to record right now")
                    .to_string()
            } else if list.is_empty() {
                lang.pick("还没记录过", "Nothing recorded yet").to_string()
            } else {
                match mode {
                    crate::config::DeviceStateMode::Whitelist => {
                        if lang.is_en() {
                            format!("{} devices allowed", list.len())
                        } else {
                            format!("白名单里 {} 台", list.len())
                        }
                    }
                    crate::config::DeviceStateMode::Blacklist => {
                        if lang.is_en() {
                            format!("{} devices blocked", list.len())
                        } else {
                            format!("黑名单里 {} 台", list.len())
                        }
                    }
                    _ => {
                        if lang.is_en() {
                            format!(
                                "Recorded {} ({} on / {} off)",
                                list.len(),
                                enabled,
                                disabled
                            )
                        } else {
                            format!(
                                "已记录 {} 台（启用 {} / 禁用 {}）",
                                list.len(),
                                enabled,
                                disabled
                            )
                        }
                    }
                }
            };
            let record_title = match mode {
                crate::config::DeviceStateMode::Blacklist => {
                    lang.pick("记录当前禁用的设备", "Record the disabled devices")
                }
                crate::config::DeviceStateMode::Whitelist => {
                    lang.pick("记录当前启用的设备", "Record the enabled devices")
                }
                _ => lang.pick("记录当前设备开关", "Record current device switches"),
            };
            Some(self.action_row(
                "startup-record-devices",
                record_title,
                description,
                lang.pick("记录", "Record"),
                move |window, cx| {
                    // 空快照下按记录不该把已有名单抹掉。
                    if !current.is_empty() {
                        let states: Vec<crate::config::StartupDeviceState> = current
                            .iter()
                            .filter(|(_, _, enabled)| match mode {
                                crate::config::DeviceStateMode::Blacklist => !enabled,
                                crate::config::DeviceStateMode::Whitelist => *enabled,
                                _ => true,
                            })
                            .map(|(id, name, enabled)| crate::config::StartupDeviceState {
                                id: id.clone(),
                                name: name.clone(),
                                // 黑名单 / 白名单里 enabled 只是占位：成员资格本身决定含义。
                                enabled: match mode {
                                    crate::config::DeviceStateMode::Blacklist => false,
                                    crate::config::DeviceStateMode::Whitelist => true,
                                    _ => *enabled,
                                },
                            })
                            .collect();
                        let _ = store.borrow_mut().update(|config| {
                            *config.startup.device_list_mut(mode) = states;
                        });
                    }
                    refresh_ui(window, cx);
                },
            ))
        } else {
            None
        };

        // 主音量
        let set_master_volume = self.toggle_row(
            "startup-master-volume",
            lang.pick("启动时设置主音量", "Set the master volume at startup"),
            lang.pick(
                "启动后把默认设备的音量设成下面这个值",
                "Set the default device volume to the value below",
            ),
            profile.set_master_volume,
            self.bind(|config| {
                config.startup.set_master_volume = !config.startup.set_master_volume
            }),
        );
        let master_volume = if profile.set_master_volume {
            Some(self.number_row(
                "startup-master-volume-value",
                lang.pick("启动音量", "Startup volume"),
                lang.pick("点数字可以直接填", "Click the number to type"),
                NumberField::StartupVolume,
                config,
                lang,
                cx,
            ))
        } else {
            None
        };

        let mut device_section = vec![set_default_device];
        if let Some(picker) = device_picker {
            device_section.push(self.sub_group(vec![picker]));
        }
        if let Some(row) = device_mode {
            device_section.push(row);
        }

        // 名单单独包成一张内嵌小卡片：它和上面的「规则选择」不是一类东西
        // （一个是策略，一个是策略的数据），平铺在一起会像两个功能混在一块。
        let mut list_block: Vec<AnyElement> = Vec::new();
        if let Some(row) = record_states {
            list_block.push(row);
        }
        // 名单本身可以手动改：每台设备单独切「启动时启用 / 禁用」，也能删掉、加新的。
        if profile.device_state_mode.uses_list() {
            let list = profile.device_list(profile.device_state_mode);
            if list.is_empty() {
                list_block.push(self.note_row(if profile.device_state_mode
                    == crate::config::DeviceStateMode::Whitelist
                {
                    // 白名单空着是「什么都不做」而不是「禁用一切」，先说清楚免得以为坏了。
                    lang.pick(
                        "名单还是空的：白名单为空时一台都不会被动，请先点「记录」",
                        "The list is empty: an empty whitelist leaves every device alone, press Record first",
                    )
                } else {
                    lang.pick(
                        "名单还是空的：点上面的「记录」，或者从下面手动加一台",
                        "The list is empty: use Record above, or add a device below",
                    )
                }));
            } else {
                for (index, state) in list.iter().enumerate() {
                    list_block.push(self.startup_device_row(
                        index,
                        state,
                        snapshot,
                        profile.device_state_mode,
                        lang,
                        cx,
                    ));
                }
            }
            list_block.push(self.device_picker_row(
                "startup-add-device",
                lang.pick("手动添加设备", "Add a device"),
                lang.pick(
                    "从当前检测到的设备里挑一台加进名单",
                    "Pick one of the detected devices to add",
                ),
                "",
                lang.pick("添加设备", "Add device"),
                DevicePicker::AddDeviceState,
                cx,
            ));
        }

        if !list_block.is_empty() {
            let count = profile.device_list(profile.device_state_mode).len();
            device_section.push(
                div()
                    .mx(px(5.))
                    .my(px(4.))
                    .p(px(3.))
                    .flex()
                    .flex_col()
                    .rounded(px(theme::RADIUS_SMALL))
                    // 比外层的 `surface_sunken` 亮明显一档 + 一圈描边：
                    // 用 `surface` 只差 3% 亮度，实测在深色主题里根本看不出来。
                    .bg(theme::surface_elevated())
                    .border_1()
                    .border_color(theme::border_strong())
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .gap(px(8.))
                            .px(px(7.))
                            .pt(px(2.))
                            .pb(px(4.))
                            .child(
                                div()
                                    .text_size(theme::fs(10.))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(theme::text_tertiary())
                                    .child(match profile.device_state_mode {
                                        crate::config::DeviceStateMode::Blacklist => {
                                            lang.pick("黑名单", "Blacklist")
                                        }
                                        crate::config::DeviceStateMode::Whitelist => {
                                            lang.pick("白名单", "Whitelist")
                                        }
                                        _ => lang.pick("名单", "List"),
                                    }),
                            )
                            .child(
                                div()
                                    .text_size(theme::fs(10.))
                                    .text_color(theme::text_tertiary())
                                    .child(format!("{count}{}", lang.pick(" 台", ""))),
                            ),
                    )
                    .children(list_block)
                    .into_any_element(),
            );
        }

        device_section.push(set_master_volume);
        if let Some(row) = master_volume {
            device_section.push(self.sub_group(vec![row]));
        }

        // 应用音量
        let set_app_volumes = self.toggle_row(
            "startup-app-volumes",
            lang.pick("启动时设置应用音量", "Set app volumes at startup"),
            lang.pick(
                "启动后的两分钟里，记过的应用一出现就套上它的音量",
                "Within two minutes after launch, recorded apps get their volume applied",
            ),
            profile.set_app_volumes,
            self.bind(|config| config.startup.set_app_volumes = !config.startup.set_app_volumes),
        );
        let record_apps = if profile.set_app_volumes {
            let store = self.config.clone();
            let current: Vec<(String, String, u32)> = snapshot
                .apps
                .iter()
                .filter(|app| !app.is_system)
                .map(|app| {
                    (
                        app.key.clone(),
                        app.name.clone(),
                        (app.volume * 100.0).round().clamp(0.0, 100.0) as u32,
                    )
                })
                .collect();
            let description = if current.is_empty() {
                // 没有任何会话时按「记录」会把名单清空，先说清楚。
                lang.pick(
                    "当前没有可记录的应用会话",
                    "No audio sessions to record right now",
                )
                .to_string()
            } else if profile.app_volumes.is_empty() {
                lang.pick("还没记录过", "Nothing recorded yet").to_string()
            } else {
                format!(
                    "{} {} {}",
                    lang.pick("已记录", "Recorded"),
                    profile.app_volumes.len(),
                    lang.pick("个应用", "apps")
                )
            };
            Some(self.action_row(
                "startup-record-apps",
                lang.pick("记录当前应用音量", "Record current app volumes"),
                description,
                lang.pick("记录", "Record"),
                move |window, cx| {
                    if current.is_empty() {
                        refresh_ui(window, cx);
                        return;
                    }
                    let apps: Vec<crate::config::StartupAppVolume> = current
                        .iter()
                        .map(|(key, name, percent)| crate::config::StartupAppVolume {
                            key: key.clone(),
                            name: name.clone(),
                            percent: *percent,
                        })
                        .collect();
                    let _ = store
                        .borrow_mut()
                        .update(|config| config.startup.app_volumes = apps);
                    refresh_ui(window, cx);
                },
            ))
        } else {
            None
        };

        let mut app_section = vec![set_app_volumes];
        // 展开的子项和「启动设备」那边一样收进一张内嵌卡片。
        let mut app_block: Vec<AnyElement> = Vec::new();
        if let Some(row) = record_apps {
            app_block.push(row);
        }
        if profile.set_app_volumes {
            if profile.app_volumes.is_empty() {
                app_block.push(self.note_row(lang.pick(
                    "名单还是空的：点上面的「记录」，或者从下面手动加一个应用",
                    "The list is empty: use Record above, or add an app below",
                )));
            } else {
                for (index, app) in profile.app_volumes.iter().enumerate() {
                    app_block.push(self.startup_app_row(index, app, lang, cx));
                }
            }
            app_block.push(self.device_picker_row(
                "startup-add-app",
                lang.pick("手动添加应用", "Add an app"),
                lang.pick(
                    "从当前有声会话里挑一个加进名单",
                    "Pick one of the current audio sessions to add",
                ),
                "",
                lang.pick("添加应用", "Add app"),
                DevicePicker::AddAppVolume,
                cx,
            ));
        }
        if !app_block.is_empty() {
            app_section.push(self.sub_group(app_block));
        }

        vec![
            self.section(
                lang.pick("启动时套用", "Apply at startup"),
                "power",
                device_section,
            ),
            self.section(lang.pick("应用音量", "App volumes"), "apps", app_section),
        ]
    }

    /// 一行灰色小字：名单为空之类的说明。
    /// 外观页里的「托盘菜单」入口行：点开才进弹层编辑。
    fn tray_menu_entry_row(
        &self,
        config: &Config,
        lang: Lang,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let root = &config.tray_menu.root;
        let folders = root.iter().filter(|node| node.is_folder()).count();
        let top = root.len();
        let separators = root.iter().filter(|node| node.is_separator()).count();
        let hidden = root.iter().filter(|node| !node.visible).count();
        let description = if lang.is_en() {
            format!(
                "{top} top-level items ({separators} separators), {folders} submenus, {hidden} hidden",
            )
        } else {
            format!(
                "{top} 个顶层条目（{separators} 条分隔线）· {folders} 个子菜单 · {hidden} 个已隐藏"
            )
        };

        div()
            .id("tray-menu-open")
            .flex()
            .items_center()
            .gap(px(10.))
            .px(px(9.))
            .py(px(5.))
            .rounded(px(theme::RADIUS_ROW))
            .cursor_pointer()
            .hover(|style| style.bg(theme::surface_hover()))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _: &gpui::MouseDownEvent, _, cx| {
                    cx.stop_propagation();
                    this.tray_menu_open = true;
                    this.tray_menu_scroll.scroll_to_item(0);
                    cx.notify();
                }),
            )
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .gap(px(2.))
                    .child(
                        div()
                            .text_size(theme::fs(13.))
                            .text_color(theme::text_primary())
                            .child(lang.pick("编辑托盘菜单", "Edit tray menu")),
                    )
                    .child(
                        div()
                            .text_size(theme::fs(10.5))
                            .text_color(theme::text_tertiary())
                            .child(description),
                    ),
            )
            .child(
                div()
                    .flex_none()
                    .px(px(9.))
                    .py(px(3.))
                    .rounded(px(theme::RADIUS_SMALL))
                    .bg(theme::track())
                    .text_size(theme::fs(11.))
                    .text_color(theme::text_secondary())
                    .child(lang.pick("编辑", "Edit")),
            )
            .into_any_element()
    }

    /// 托盘菜单编辑器弹层（盖在设置页上，不改窗口尺寸）。
    fn tray_menu_overlay(
        &self,
        config: &Config,
        lang: Lang,
        viewport: gpui::Size<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.tray_menu_open {
            return None;
        }

        let width = 420.0_f32;
        let height = (viewport.height.as_f32() - 110.0).clamp(260.0, 560.0);
        let left = ((viewport.width.as_f32() - width) / 2.0).max(12.0);
        let top = ((viewport.height.as_f32() - height) / 2.0).max(44.0);

        let menu = config.tray_menu.clone();
        let mut list = div()
            .id("tray-menu-list")
            .flex()
            .flex_col()
            .flex_1()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.tray_menu_scroll)
            .on_scroll_wheel(cx.listener(|_, _, _, cx| cx.notify()))
            .py(px(2.));

        for (index, node) in menu.root.iter().enumerate() {
            if node.is_folder() {
                list = list.child(self.tray_menu_folder_card(index, node, lang, cx));
            } else {
                list = list.child(self.tray_menu_overlay_row(
                    None,
                    index,
                    menu.root.len(),
                    node,
                    lang,
                    cx,
                ));
            }
        }

        // 根菜单末尾的插入指示：拖到最后一行下半格时出现（没有落点盒子去点了）。
        let drop_at_root_end = self.tray_drag.is_some()
            && self.tray_drop
                == Some(TrayDrop::Insert {
                    folder: None,
                    index: menu.root.len(),
                });
        if drop_at_root_end {
            list = list.child(Self::tray_drop_bar());
        }

        let add_folder = {
            let label = lang.pick("子菜单", "Submenu").to_string();
            let apply = self.edit_tray_menu(move |menu| {
                let id = menu.next_folder_id();
                let number = menu.root.iter().filter(|node| node.is_folder()).count() + 1;
                let folder = crate::config::TrayMenuNode::folder(
                    &id,
                    &format!("{label} {number}"),
                    Vec::new(),
                );
                match menu.root.iter().position(|node| node.id == "quit") {
                    Some(index) => menu.root.insert(index, folder),
                    None => menu.root.push(folder),
                }
            });
            let apply = std::rc::Rc::new(apply);
            let apply = apply.clone();
            div()
                .id("tray-add-folder")
                .flex_none()
                .px(px(10.))
                .py(px(3.))
                .rounded(px(theme::RADIUS_SMALL))
                .bg(theme::track())
                .cursor_pointer()
                .text_size(theme::fs(11.))
                .text_color(theme::text_secondary())
                .hover(|style| style.bg(theme::surface_hover()))
                .on_click(cx.listener(move |_, _: &ClickEvent, window, cx| apply(window, cx)))
                .child(lang.pick("＋ 新建子菜单", "+ New submenu"))
        };

        let add_separator = {
            let apply = self.edit_tray_menu(|menu| {
                // 默认插在「退出」前面（最后一段），没有退出就放到末尾。
                let index = menu
                    .root
                    .iter()
                    .position(|node| node.id == "quit")
                    .unwrap_or(menu.root.len());
                menu.insert_separator(None, index);
            });
            let apply = std::rc::Rc::new(apply);
            let apply = apply.clone();
            div()
                .id("tray-add-separator")
                .flex_none()
                .px(px(10.))
                .py(px(3.))
                .rounded(px(theme::RADIUS_SMALL))
                .bg(theme::track())
                .cursor_pointer()
                .text_size(theme::fs(11.))
                .text_color(theme::text_secondary())
                .hover(|style| style.bg(theme::surface_hover()))
                .on_click(cx.listener(move |_, _: &ClickEvent, window, cx| apply(window, cx)))
                .child(lang.pick("＋ 分隔线", "+ Separator"))
        };

        Some(
            div()
                .id("tray-menu-backdrop")
                .absolute()
                .left(px(0.))
                .top(px(0.))
                .size_full()
                .occlude()
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(|this, _: &gpui::MouseDownEvent, window, cx| {
                        // 点弹层外面 = 关掉（顺便结束正在进行的重命名）。
                        this.tray_menu_open = false;
                        this.tray_drag = None;
                        this.tray_drop = None;
                        this.commit_tray_rename(window, cx);
                        cx.notify();
                    }),
                )
                .child(
                    div()
                        .id("tray-menu-panel")
                        .absolute()
                        .left(px(left))
                        .top(px(top))
                        .w(px(width))
                        .h(px(height))
                        .flex()
                        .flex_col()
                        // 挡住滚轮：不然弹层后面的设置页会跟着滚。
                        .occlude()
                        .rounded(px(theme::RADIUS_WINDOW))
                        .border_1()
                        .border_color(theme::border_strong())
                        .bg(theme::surface_elevated())
                        .p(px(8.))
                        .gap(px(6.))
                        // 点在面板上不算「点空白处」，否则一点就关。
                        .on_mouse_down(
                            gpui::MouseButton::Left,
                            cx.listener(|_, _: &gpui::MouseDownEvent, _, cx| cx.stop_propagation()),
                        )
                        .on_mouse_up(
                            gpui::MouseButton::Left,
                            cx.listener(|this, _: &gpui::MouseUpEvent, window, cx| {
                                this.finish_tray_drag(window, cx);
                            }),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .gap(px(8.))
                                .px(px(4.))
                                .child(
                                    div()
                                        .flex_1()
                                        .text_size(theme::fs(12.))
                                        .font_weight(gpui::FontWeight::SEMIBOLD)
                                        .text_color(theme::text_primary())
                                        .child(lang.pick("托盘菜单", "Tray menu")),
                                )
                                .child(add_folder)
                                .child(add_separator)
                                .child(
                                    div()
                                        .id("tray-menu-close")
                                        .size(px(22.))
                                        .flex_none()
                                        .flex()
                                        .items_center()
                                        .justify_center()
                                        .rounded(px(theme::RADIUS_SMALL))
                                        .cursor_pointer()
                                        .hover(|style| style.bg(theme::surface_hover()))
                                        .on_click(cx.listener(
                                            |this, _: &ClickEvent, window, cx| {
                                                this.tray_menu_open = false;
                                                this.tray_drag = None;
                                                this.tray_drop = None;
                                                this.commit_tray_rename(window, cx);
                                                cx.notify();
                                            },
                                        ))
                                        .child(icon("close", 12.0, theme::text_secondary())),
                                ),
                        )
                        .child(list)
                        .child(
                            div()
                                .px(px(6.))
                                .text_size(theme::fs(9.5))
                                .text_color(theme::text_tertiary())
                                .child(lang.pick(
                                    "⠿ 拖条目上下换位置、拖进子菜单卡片里 · 点子菜单名字改名 · × 删除子菜单 / 分隔线",
                                    "⠿ drag an item up/down, or onto a submenu card · click a submenu name to rename · × deletes a submenu or separator",
                                )),
                        ),
                )
                .into_any_element(),
        )
    }

    /// 子菜单卡片：标题行 + 里面的条目。拖动时整张卡片高亮，表示「放进这个子菜单」。
    fn tray_menu_folder_card(
        &self,
        index: usize,
        node: &crate::config::TrayMenuNode,
        lang: Lang,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // 子菜单不能再嵌子菜单：拖的是子菜单时，卡片里不放落点。
        let accepts = self.tray_drag.is_some() && !self.tray_drag_is_folder;
        let hot = accepts
            && matches!(
                self.tray_drop,
                Some(TrayDrop::Insert {
                    folder: Some(folder),
                    ..
                }) if folder == index
            );
        let drop_at_end = accepts
            && self.tray_drop
                == Some(TrayDrop::Insert {
                    folder: Some(index),
                    index: node.children.len(),
                });

        let mut card = div()
            .id(SharedString::from(format!("tray-folder-{index}")))
            .mx(px(5.))
            .my(px(4.))
            .p(px(3.))
            .flex()
            .flex_col()
            .rounded(px(theme::RADIUS_SMALL))
            .bg(if hot {
                theme::accent_soft()
            } else {
                theme::surface_sunken()
            })
            .border_1()
            .border_color(if hot {
                theme::accent()
            } else {
                theme::border()
            })
            .child(self.tray_menu_overlay_row(None, index, usize::MAX, node, lang, cx));

        if node.children.is_empty() {
            card = card.child(
                div()
                    .id(SharedString::from(format!("tray-folder-{index}-empty")))
                    .pl(px(20.))
                    .py(px(3.))
                    .text_size(theme::fs(9.5))
                    .text_color(if hot {
                        theme::accent()
                    } else {
                        theme::text_tertiary()
                    })
                    // 空卡片也得能接住拖进来的条目。
                    .when(accepts, |hint| {
                        hint.on_mouse_move(cx.listener(
                            move |this, _: &gpui::MouseMoveEvent, _, cx| {
                                this.set_tray_drop(
                                    TrayDrop::Insert {
                                        folder: Some(index),
                                        index: 0,
                                    },
                                    cx,
                                );
                            },
                        ))
                    })
                    .child(lang.pick("空的：把条目拖进来", "Empty — drag an item here")),
            );
        }
        if drop_at_end {
            card = card.child(Self::tray_drop_bar());
        }
        for (child_index, child) in node.children.iter().enumerate() {
            card = card.child(self.tray_menu_overlay_row(
                Some(index),
                child_index,
                node.children.len(),
                child,
                lang,
                cx,
            ));
        }
        card.into_any_element()
    }

    /// 就地编辑框的输入法通道：绘制时把 `ElementInputHandler` 挂给平台层，
    /// 输入法提交的字就会走 `EntityInputHandler` 落进当前编辑缓冲。
    fn text_edit_canvas(&self, cx: &mut Context<Self>) -> AnyElement {
        let focus = self.focus.clone();
        let entity = cx.entity();
        gpui::canvas(
            |_bounds, _window, _cx| {},
            move |bounds, _, window, cx| {
                window.handle_input(
                    &focus,
                    gpui::ElementInputHandler::new(bounds, entity.clone()),
                    cx,
                );
            },
        )
        .absolute()
        .left_0()
        .top_0()
        .size_full()
        .into_any_element()
    }

    /// 和 `text_edit_canvas` 一样，但顺手把这块矩形记下来 ——
    /// 点击输入区时要按它把 x 换算成「第几个字」。
    fn text_edit_canvas_measured(
        &self,
        cell: Rc<Cell<Option<Bounds<Pixels>>>>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let focus = self.focus.clone();
        let entity = cx.entity();
        gpui::canvas(
            |_bounds, _window, _cx| {},
            move |bounds, _, window, cx| {
                cell.set(Some(bounds));
                window.handle_input(
                    &focus,
                    gpui::ElementInputHandler::new(bounds, entity.clone()),
                    cx,
                );
            },
        )
        .absolute()
        .left_0()
        .top_0()
        .size_full()
        .into_any_element()
    }

    /// 插入位置的指示条：两像素高的一条亮线，插在哪两行之间就画在哪。
    fn tray_drop_bar() -> AnyElement {
        div()
            .mx(px(6.))
            .my(px(1.))
            .h(px(2.))
            .flex_none()
            .rounded(px(1.))
            .bg(theme::accent())
            .into_any_element()
    }

    /// 弹层里的一行：拖动柄 + 显隐 + 名字 + 顺序按钮（子菜单 / 分隔线还多一个删除）。
    fn tray_menu_overlay_row(
        &self,
        folder: Option<usize>,
        index: usize,
        total: usize,
        node: &crate::config::TrayMenuNode,
        lang: Lang,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let is_folder = node.is_folder();
        let is_separator = node.is_separator();
        let visible = node.visible;
        let id = match folder {
            None => format!("tray-row-{index}"),
            Some(f) => format!("tray-row-{f}-{index}"),
        };
        let dragging = self.tray_drag == Some((folder, index));

        let handle = {
            let folder_for_drag = folder;
            div()
                .id(SharedString::from(format!("{id}-handle")))
                .size(px(16.))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(3.))
                .cursor_pointer()
                .text_size(theme::fs(10.))
                .text_color(if dragging {
                    theme::accent()
                } else {
                    theme::text_tertiary()
                })
                .hover(|style| style.bg(theme::surface_hover()))
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(move |this, _: &gpui::MouseDownEvent, _, cx| {
                        cx.stop_propagation();
                        this.tray_drag = Some((folder_for_drag, index));
                        this.tray_drag_is_folder = is_folder;
                        this.tray_drop = None;
                        cx.notify();
                    }),
                )
                .child("⠿")
        };

        let eye = {
            let apply = self.edit_tray_menu(move |menu| menu.set_visible(folder, index, !visible));
            let apply = std::rc::Rc::new(apply);
            let apply = apply.clone();
            div()
                .id(SharedString::from(format!("{id}-vis")))
                .size(px(16.))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(3.))
                .cursor_pointer()
                .text_size(theme::fs(11.))
                .text_color(if visible {
                    theme::text_secondary()
                } else {
                    theme::text_tertiary()
                })
                .hover(|style| style.bg(theme::surface_hover()))
                .on_click(cx.listener(move |_, _: &ClickEvent, window, cx| apply(window, cx)))
                .child(if visible { "●" } else { "○" })
        };

        let up = {
            let apply = self.edit_tray_menu(move |menu| menu.shift(folder, index, true));
            let apply = std::rc::Rc::new(apply);
            let apply = apply.clone();
            let enabled = index > 0;
            div()
                .id(SharedString::from(format!("{id}-up")))
                .size(px(16.))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(3.))
                .text_size(theme::fs(11.))
                .when(enabled, |style| {
                    style
                        .cursor_pointer()
                        .text_color(theme::text_secondary())
                        .hover(|style| style.bg(theme::surface_hover()))
                })
                .when(!enabled, |style| style.text_color(theme::text_tertiary()))
                .on_click(cx.listener(move |_, _: &ClickEvent, window, cx| {
                    if enabled {
                        apply(window, cx);
                    }
                }))
                .child("↑")
        };

        let down = {
            let apply = self.edit_tray_menu(move |menu| menu.shift(folder, index, false));
            let apply = std::rc::Rc::new(apply);
            let apply = apply.clone();
            let enabled = index + 1 < total;
            div()
                .id(SharedString::from(format!("{id}-down")))
                .size(px(16.))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(3.))
                .text_size(theme::fs(11.))
                .when(enabled, |style| {
                    style
                        .cursor_pointer()
                        .text_color(theme::text_secondary())
                        .hover(|style| style.bg(theme::surface_hover()))
                })
                .when(!enabled, |style| style.text_color(theme::text_tertiary()))
                .on_click(cx.listener(move |_, _: &ClickEvent, window, cx| {
                    if enabled {
                        apply(window, cx);
                    }
                }))
                .child("↓")
        };

        let remove = if is_folder || is_separator {
            let apply = self.edit_tray_menu(move |menu| {
                if is_folder {
                    menu.delete_folder(index);
                } else {
                    menu.remove_node(folder, index);
                }
            });
            let apply = std::rc::Rc::new(apply);
            let apply = apply.clone();
            Some(
                div()
                    .id(SharedString::from(format!("{id}-del")))
                    .size(px(16.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(3.))
                    .cursor_pointer()
                    .text_size(theme::fs(11.))
                    .text_color(theme::text_tertiary())
                    .hover(|style| style.bg(theme::surface_hover()))
                    // 弹层里（`.occlude()` + stop_propagation）子元素的 `on_click` 会被吞掉，
                    // 删除一律挂 `on_mouse_down`（和拖动柄、弹层开关一个做法）。
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(move |_, _: &gpui::MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            apply(window, cx);
                        }),
                    )
                    .child("×"),
            )
        } else {
            None
        };

        // 名字：子菜单点一下进重命名。
        let renaming = is_folder
            && self
                .tray_rename
                .as_ref()
                .is_some_and(|(rename_id, _)| *rename_id == node.id);
        let name: AnyElement = if renaming {
            let buffer = self
                .tray_rename
                .as_ref()
                .map(|(_, buffer)| buffer.clone())
                .unwrap_or_default();
            // 输入法要靠平台层的 `PlatformInputHandler` 才会把候选字送进来：
            // 这里在绘制时挂一个，`Settings` 的 `EntityInputHandler` 负责接住。
            let canvas = self.text_edit_canvas(cx);
            div()
                .relative()
                .flex_1()
                .min_w(px(0.))
                .flex()
                .items_center()
                .gap(px(1.))
                .text_size(theme::fs(12.))
                .text_color(theme::text_primary())
                .child(buffer)
                .child(
                    div()
                        .w(px(1.5))
                        .h(px(12.))
                        .flex_none()
                        .bg(theme::text_primary()),
                )
                .child(canvas)
                .into_any_element()
        } else {
            let click_target = node.id.clone();
            let fallback_title = node.title.clone();
            let label = if is_folder {
                node.title.clone()
            } else {
                tray_node_label(lang, &node.id)
            };
            if is_separator {
                // 分隔线在列表里就画成一条线：一眼看出「这条是线，删了就没线」。
                div()
                    .id(SharedString::from(format!("{id}-name")))
                    .flex_1()
                    .min_w(px(0.))
                    .py(px(4.))
                    .child(div().h(px(1.)).w_full().bg(if visible {
                        theme::border_strong()
                    } else {
                        theme::track()
                    }))
                    .into_any_element()
            } else {
                div()
                    .id(SharedString::from(format!("{id}-name")))
                    .flex_1()
                    .min_w(px(0.))
                    .truncate()
                    .text_size(theme::fs(12.))
                    .text_color(if visible {
                        theme::text_primary()
                    } else {
                        theme::text_tertiary()
                    })
                    .when(is_folder, |style| style.cursor_pointer())
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        if is_folder {
                            this.tray_rename = Some((click_target.clone(), fallback_title.clone()));
                            // 键盘事件只在焦点元素上派发：没焦点的话敲字没反应。
                            this.focus.focus(window, cx);
                            cx.notify();
                        }
                    }))
                    .child(label)
                    .into_any_element()
            }
        };

        let drop_above = self.tray_drop == Some(TrayDrop::Insert { folder, index });
        // 拖子菜单的时候，子菜单内部不能当落点（只有一层）。
        let drag_active =
            self.tray_drag.is_some() && !(self.tray_drag_is_folder && folder.is_some());

        let mut row = div()
            .id(SharedString::from(format!("{id}-row")))
            .relative()
            .flex()
            .items_center()
            .gap(px(3.))
            .when(folder.is_some(), |row| row.pl(px(14.)))
            .px(px(4.))
            .py(px(2.))
            .rounded(px(3.))
            .when(dragging, |row| row.bg(theme::surface_hover()))
            .child(handle)
            .child(eye)
            .child(name)
            .child(up)
            .child(down)
            .children(remove);

        // 落点标记：松开就会插到这一行上面。
        if drop_above {
            row = row.child(
                div()
                    .absolute()
                    .left(px(1.))
                    .right(px(1.))
                    .top(px(0.))
                    .h(px(2.))
                    .rounded(px(1.))
                    .bg(theme::accent()),
            );
        }

        // 拖动中把行切成上下两半：上半格 = 插到这一行前面，下半格 = 插到它后面。
        if drag_active {
            row = row
                .child(
                    div()
                        .id(SharedString::from(format!("{id}-drop-top")))
                        .absolute()
                        .left(px(0.))
                        .right(px(0.))
                        .top(px(0.))
                        .h(relative(0.5))
                        .on_mouse_move(cx.listener(
                            move |this, _: &gpui::MouseMoveEvent, _, cx| {
                                this.set_tray_drop(TrayDrop::Insert { folder, index }, cx);
                            },
                        )),
                )
                .child(
                    div()
                        .id(SharedString::from(format!("{id}-drop-bottom")))
                        .absolute()
                        .left(px(0.))
                        .right(px(0.))
                        .bottom(px(0.))
                        .h(relative(0.5))
                        .on_mouse_move(cx.listener(
                            move |this, _: &gpui::MouseMoveEvent, _, cx| {
                                this.set_tray_drop(
                                    TrayDrop::Insert {
                                        folder,
                                        index: index + 1,
                                    },
                                    cx,
                                );
                            },
                        )),
                );
        }

        row.into_any_element()
    }

    /// 更新拖动落点（没在拖就忽略），变了才重画。
    fn set_tray_drop(&mut self, target: TrayDrop, cx: &mut Context<Self>) {
        if self.tray_drag.is_none() || self.tray_drop == Some(target) {
            return;
        }
        self.tray_drop = Some(target);
        cx.notify();
    }

    /// 松手：把拖着的条目搬到当前落点。
    fn finish_tray_drag(&mut self, window: &mut Window, cx: &mut App) {
        let (Some(from), Some(TrayDrop::Insert { folder, index })) =
            (self.tray_drag, self.tray_drop)
        else {
            self.tray_drag = None;
            self.tray_drop = None;
            return;
        };
        // 原位放下（放在自己身上 / 自己后面）就不动，免得白写一次配置。
        let same = from.0 == folder && (index == from.1 || index == from.1 + 1);
        if !same {
            let _ = self
                .config
                .borrow_mut()
                .update(|config| config.tray_menu.move_node(from, folder, index));
            crate::gui::refresh_tray_menu(cx);
        }
        self.tray_drag = None;
        self.tray_drag_is_folder = false;
        self.tray_drop = None;
        refresh_ui(window, cx);
    }

    /// 重命名输入：回车确认、Esc 取消、退格删字。
    ///
    /// 文字本身不在这里插 —— 输入法 / `WM_CHAR` 走 `EntityInputHandler`
    /// （见本文件底部的 `impl EntityInputHandler for Settings`），
    /// 这样中文能直接打，也不会和这里的按键插一遍重复。
    fn handle_tray_rename_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.tray_rename.is_none() {
            return false;
        }
        let key = event.keystroke.key.as_str();
        // 输入法正在拼字：回车 / Esc / 退格都该交给输入法（确认候选、取消、删候选字），
        // 我们这一层不能抢。
        if self.text_marked.is_some() {
            return false;
        }
        if key == "enter" {
            cx.stop_propagation();
            self.commit_tray_rename(window, cx);
            return true;
        }
        if key == "escape" {
            cx.stop_propagation();
            self.tray_rename = None;
            self.text_marked = None;
            cx.notify();
            return true;
        }
        if key == "backspace" {
            cx.stop_propagation();
            self.backspace_text_edit(cx);
            return true;
        }
        if key == "v" && event.keystroke.modifiers.control {
            cx.stop_propagation();
            let text = cx
                .read_from_clipboard()
                .and_then(|item| item.text().map(|text| text.to_string()));
            if let Some(text) = text {
                self.insert_rename_text(&text);
                cx.notify();
            }
            return true;
        }
        false
    }

    /// 当前正在就地编辑的那段文字（字体搜索 → 设备改名 → 子菜单改名 → OSD 文字 → 黑名单输入）。
    ///
    /// 字体弹层排在第一位：它一开着就是模态的，键盘全归搜索框。
    fn text_edit(&self) -> Option<&str> {
        if self.font_menu.is_some() {
            return Some(self.font_filter.as_str());
        }
        self.device_rename
            .as_ref()
            .or(self.display_rename.as_ref())
            .or(self.tray_rename.as_ref())
            .map(|(_, text)| text.as_str())
            .or_else(|| self.osd_text_edit.as_ref().map(|(_, text)| text.as_str()))
            .or_else(|| {
                self.active_block_list()
                    .and_then(|list| self.block_draft(list).as_deref())
            })
    }

    /// 正在用键盘的那块名单：先看焦点，没焦点时退到「有草稿的那块」。
    fn active_block_list(&self) -> Option<BlockList> {
        if self.focused_block_list().is_some() {
            return self.focused_block_list();
        }
        if self.block_name_edit.is_some() {
            Some(BlockList::Block)
        } else if self.block_exempt_edit.is_some() {
            Some(BlockList::Exempt)
        } else {
            None
        }
    }

    /// 同上，拿可改的那份。
    fn text_edit_mut(&mut self) -> Option<&mut String> {
        if self.font_menu.is_some() {
            return Some(&mut self.font_filter);
        }
        if self.device_rename.is_some() {
            self.device_rename.as_mut().map(|(_, text)| text)
        } else if self.display_rename.is_some() {
            self.display_rename.as_mut().map(|(_, text)| text)
        } else if self.tray_rename.is_some() {
            self.tray_rename.as_mut().map(|(_, text)| text)
        } else if self.osd_text_edit.is_some() {
            self.osd_text_edit.as_mut().map(|(_, text)| text)
        } else {
            match self.active_block_list() {
                Some(BlockList::Exempt) => self.block_exempt_edit.as_mut(),
                _ => self.block_name_edit.as_mut(),
            }
        }
    }

    /// 从外部（剪贴板）塞一段文字到重命名缓冲的末尾。
    fn insert_rename_text(&mut self, text: &str) {
        self.push_text_undo();
        let Some(buffer) = self.text_edit() else {
            return;
        };
        let max = if self.font_menu.is_some() {
            // 字体名都不长，24 个字足够筛了。
            24
        } else if self.caret_rename() {
            MAX_DEVICE_NAME_CHARS
        } else if self.osd_text_edit.is_some() {
            // OSD 一行最多几十个字，给 32 够放一整句话了。
            32
        } else if self.block_name_edit.is_some() || self.block_exempt_edit.is_some() {
            // 粘一整条路径也放得下。
            260
        } else {
            MAX_NAME_CHARS
        };
        let (next, marked) = rename_apply(buffer, self.text_marked.clone(), None, text, false, max);
        if let Some(buffer) = self.text_edit_mut() {
            *buffer = next;
        }
        self.text_marked = marked;
    }

    /// 现在是不是「带光标的就地改名」：音频设备和显示器共用这一套。
    fn caret_rename(&self) -> bool {
        self.device_rename.is_some() || self.display_rename.is_some()
    }

    /// 当前就地输入框的身份。换一格就要清空撤销栈，不能跨框回退。
    fn undo_slot(&self) -> Option<u8> {
        if self.font_menu.is_some() {
            Some(5)
        } else if self.device_rename.is_some() {
            Some(0)
        } else if self.display_rename.is_some() {
            Some(1)
        } else if self.tray_rename.is_some() {
            Some(2)
        } else if self.osd_text_edit.is_some() {
            Some(3)
        } else if self.block_name_edit.is_some() {
            Some(4)
        } else if self.block_exempt_edit.is_some() {
            // 第二块名单单独一格：两框之间来回点不该互相回退。
            Some(6)
        } else {
            None
        }
    }

    /// 改字之前记一步（Ctrl+Z 用）。换了一格就重新开始记。
    fn push_text_undo(&mut self) {
        let Some(slot) = self.undo_slot() else {
            return;
        };
        let Some(text) = self.text_edit().map(|text| text.to_string()) else {
            return;
        };
        let len = utf16_len(&text);
        // 只有设备 / 显示器改名有真光标，其余几个都是「接在末尾」。
        let caret = if self.caret_rename() {
            self.rename_cursor.min(len)
        } else {
            len
        };
        if self.text_undo_owner != Some(slot) {
            self.text_undo.clear();
            self.text_undo_owner = Some(slot);
        }
        if self.text_undo.last() == Some(&(text.clone(), caret)) {
            return;
        }
        if self.text_undo.len() >= 100 {
            self.text_undo.remove(0);
        }
        self.text_undo.push((text, caret));
    }

    /// Ctrl+Z：把当前输入框退回上一步。没得退就返回 false（让别处接手）。
    fn undo_text_edit(&mut self, cx: &mut Context<Self>) -> bool {
        let Some(slot) = self.undo_slot() else {
            return false;
        };
        if self.text_undo_owner != Some(slot) {
            return false;
        }
        let Some((text, caret)) = self.text_undo.pop() else {
            return false;
        };
        if let Some(buffer) = self.text_edit_mut() {
            *buffer = text;
        }
        self.rename_cursor = caret;
        self.text_marked = None;
        cx.notify();
        true
    }

    /// 就地输入框退格：先记一步再删（不然退完就没法撤销）。
    fn backspace_text_edit(&mut self, cx: &mut Context<Self>) {
        self.push_text_undo();
        if let Some(buffer) = self.text_edit_mut() {
            buffer.pop();
        }
        cx.notify();
    }

    /// 当前在改名的那一格：(id / key, 文本)。
    fn rename_slot(&self) -> Option<(String, String)> {
        self.device_rename
            .clone()
            .or_else(|| self.display_rename.clone())
    }

    /// 把文本写回当前那一格（设备 / 显示器）。
    fn set_rename_slot(&mut self, text: String) {
        if let Some((_, buffer)) = self.device_rename.as_mut() {
            *buffer = text;
        } else if let Some((_, buffer)) = self.display_rename.as_mut() {
            *buffer = text;
        }
    }

    /// 回车：按当前改的是哪一种分别落盘（空内容 = 恢复系统原名）。
    fn commit_rename(&mut self, window: &mut Window, cx: &mut App) {
        self.finish_rename(true, window, cx);
    }

    /// 点别处（失焦）：一样落盘，但**一个字都没敲就不动配置**。
    ///
    /// 以前失焦和回车完全等价，而「空 = 恢复原名」，于是「点开改名 → 没敲字 →
    /// 随手点一下别处」会把已有别名清掉。失焦的语义是「收下我写的这些」，
    /// 什么都没写就该什么都不改。
    fn blur_rename(&mut self, window: &mut Window, cx: &mut App) {
        self.finish_rename(false, window, cx);
    }

    /// 这一下刚把 `key` 这格存下去吗（由捕获相位顺手落的盘）。
    fn rename_just_committed(&self, key: &str) -> bool {
        self.rename_commit_key.as_deref() == Some(key)
            && self
                .rename_commit_at
                .is_some_and(|at| at.elapsed() < RENAME_CLICK_GRACE)
    }

    fn finish_rename(&mut self, explicit: bool, window: &mut Window, cx: &mut App) {
        if self.device_rename.is_some() {
            self.commit_device_rename(explicit, window, cx);
        } else {
            self.commit_display_rename(explicit, window, cx);
        }
    }

    /// 就地改名的输入区（音频设备 / 显示器共用）：已输入的文字 + 光标 + 输入法落点。
    ///
    /// `placeholder` 是内容为空时的灰字 —— 传系统原名，删干净了也知道原来叫什么。
    fn rename_text_field(&self, placeholder: String, cx: &mut Context<Self>) -> AnyElement {
        let buffer = self.rename_slot().map(|(_, text)| text).unwrap_or_default();
        let canvas = self.text_edit_canvas_measured(self.rename_bounds.clone(), cx);
        let empty = buffer.is_empty();
        let len = utf16_len(&buffer);
        let cursor = self.rename_cursor.min(len);
        let before = utf16_slice(&buffer, 0..cursor);
        let after = utf16_slice(&buffer, cursor..len);
        let click_buffer = buffer.clone();
        let caret = || {
            div()
                .w(px(1.5))
                .h(px(12.))
                .flex_none()
                .bg(theme::text_primary())
        };
        div()
            .relative()
            .flex()
            .items_center()
            .gap(px(1.))
            .text_size(theme::fs(12.5))
            .text_color(theme::text_primary())
            // 失焦落盘统一由根节点的捕获相位负责（见 `render` 里的
            // `capture_any_mouse_down`）。早期试过 `on_mouse_down_out`，它只在
            // 「这一下的事件路径经过本元素」时才触发，点侧栏 / 点别页时路径根本
            // 不经过输入框，照样漏。
            // 点文字就是把光标放到那一格：这一下既不算「点别处」（不提交名字），
            // 也不能冒泡到整行去（不然行上的点击会把这一格顶掉）。
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    let Some(bounds) = this.rename_bounds.get() else {
                        return;
                    };
                    let x = event.position.x.as_f32() - bounds.origin.x.as_f32();
                    this.rename_cursor =
                        index_at_x(&click_buffer, x, theme::fs(12.5).as_f32(), window);
                    cx.notify();
                }),
            )
            // 空白时把当前名字当占位提示，删干净了也知道原来叫什么。
            .when(empty, |row| {
                row.child(caret()).child(
                    div()
                        .text_color(theme::text_tertiary())
                        .child(placeholder.clone()),
                )
            })
            .when(!empty, |row| row.child(before).child(caret()).child(after))
            .child(canvas)
            .into_any_element()
    }

    /// 挂上「窗口失焦就落盘」。
    ///
    /// 点窗口里的别处已经由根节点的捕获相位兜住了（见 `render`），但切到别的
    /// 程序、Alt+Tab 走开这类操作不会往我们窗口里送鼠标事件，只能等激活事件。
    pub fn watch_rename_blur(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let mut was_active = window.is_window_active();
        cx.observe_window_activation(window, move |this, window, cx| {
            let active = window.is_window_active();
            if was_active && !active {
                this.blur_rename(window, cx);
            }
            was_active = active;
        })
        .detach();
    }

    /// 关窗时把「边看边调」的东西一起收走：钉住的提示条 + 弹出来预览的面板。
    ///
    /// 这些都属于这一扇设置窗：钉住是为了对着调外观，预览面板是改外观时自动弹的。
    /// 窗口一关就没法再取消 / 再关掉，留它们在屏幕上只能挡住别的窗口。所以这里挂
    /// 一个窗口关闭回调，和用户关窗这个动作同步 —— 提示条解除钉住并淡出，面板收起。
    ///
    /// 订阅要存在自己身上：`Subscription` 一被 drop 回调就失效了。
    pub fn watch_preview_release(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let window_id = window.window_handle().window_id();
        let store = self.config.clone();
        let sub = cx.on_window_closed(move |cx, closed| {
            if closed == window_id {
                // 窗口没了，草稿模式也要收：留着的话以后所有写配置都只进内存、
                // 永远不落盘（托盘菜单里改的东西也会跟着一起丢）。
                if store.borrow_mut().discard_draft() {
                    // 预览出去的东西（主题色、托盘图标、面板宽度）退回生效中的那份。
                    let config = store.borrow().get().clone();
                    crate::gui::apply_theme_extras(&config);
                    crate::gui::refresh_tray_icon(cx);
                    crate::gui::refresh_tray_menu(cx);
                    crate::gui::sync_tray_motion(cx);
                    // 锁在屏幕上的那个预览面板要按退回后的配置重排；没开着的就别去
                    // 预览了 —— 关设置窗时突然蹦一块面板，比不改还怪。
                    crate::gui::refresh_flyout_size_if_visible(cx);
                    crate::gui::repaint_windows(cx);
                }
                crate::gui::release_osd_pins(cx);
                crate::gui::release_flyout(cx);
            }
        });
        self.window_closed = Some(sub);
    }

    /// 显示器改名：回车或点别处落盘（规则见 `rename_applies`）。
    fn commit_display_rename(&mut self, explicit: bool, window: &mut Window, cx: &mut App) {
        self.text_marked = None;
        self.rename_cursor = 0;
        let Some((key, name)) = self.display_rename.take() else {
            return;
        };
        if !rename_applies(&name, explicit) {
            // 只收掉编辑态，配置一个字都不碰。
            refresh_ui(window, cx);
            return;
        }
        self.rename_commit_at = Some(std::time::Instant::now());
        self.rename_commit_key = Some(key.clone());
        let mut changed = false;
        let _ = self
            .config
            .borrow_mut()
            .update(|config| changed = config.set_display_alias(&key, &name));
        if changed {
            // 亮度窗口那边是另一扇窗，得喊它重画一次。
            crate::gui::refresh_brightness_window(cx);
        }
        refresh_ui(window, cx);
    }

    /// 设备改名：写进配置的 `device_names`（规则同 `rename_applies`）。
    fn commit_device_rename(&mut self, explicit: bool, window: &mut Window, cx: &mut App) {
        self.text_marked = None;
        self.rename_cursor = 0;
        let Some((id, name)) = self.device_rename.take() else {
            return;
        };
        if !rename_applies(&name, explicit) {
            refresh_ui(window, cx);
            return;
        }
        self.rename_commit_at = Some(std::time::Instant::now());
        self.rename_commit_key = Some(id.clone());
        let mut changed = false;
        let _ = self.config.borrow_mut().update(|config| {
            changed = config.set_device_alias(&id, &name);
        });
        if changed {
            // 托盘菜单与悬浮提示都是原生 / 预渲染的，得主动刷一次；
            // 面板每次重画都会重新读配置，不用管。
            crate::gui::refresh_tray_menu(cx);
            crate::gui::refresh_tray_icon(cx);
        }
        refresh_ui(window, cx);
    }

    /// 设备改名的按键：和子菜单改名一套规则，外加**光标**（点哪改哪、按位置删）。
    fn handle_device_rename_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if !self.caret_rename() {
            return false;
        }
        let key = event.keystroke.key.as_str();
        // 输入法正在拼字时全部放行（选候选、取消、删候选）。
        if self.text_marked.is_some() {
            return false;
        }
        if key == "enter" {
            cx.stop_propagation();
            self.commit_rename(window, cx);
            return true;
        }
        if key == "escape" {
            cx.stop_propagation();
            self.device_rename = None;
            self.display_rename = None;
            self.text_marked = None;
            self.rename_cursor = 0;
            cx.notify();
            return true;
        }
        if key == "backspace" || key == "delete" {
            cx.stop_propagation();
            self.delete_rename_at_cursor(key == "delete", cx);
            return true;
        }
        if matches!(key, "left" | "right" | "home" | "end") {
            cx.stop_propagation();
            self.move_rename_cursor(key, cx);
            return true;
        }
        if key == "v" && event.keystroke.modifiers.control {
            cx.stop_propagation();
            let text = cx
                .read_from_clipboard()
                .and_then(|item| item.text().map(|text| text.to_string()));
            if let Some(text) = text {
                self.insert_device_rename(&text, cx);
            }
            return true;
        }
        if event.keystroke.modifiers.control || event.keystroke.modifiers.platform {
            return false;
        }
        // 英文 / 数字直接落进光标处（中文走输入法那条路，见 EntityInputHandler）。
        if let Some(text) = event.keystroke.key_char.as_ref() {
            cx.stop_propagation();
            self.insert_device_rename(text, cx);
            return true;
        }
        false
    }

    /// 在光标处插入一段文字（设备改名专用）。
    fn insert_device_rename(&mut self, text: &str, cx: &mut Context<Self>) {
        self.push_text_undo();
        let Some((_, buffer)) = self.rename_slot() else {
            return;
        };
        let (next, cursor) =
            rename_insert_at(&buffer, self.rename_cursor, text, MAX_DEVICE_NAME_CHARS);
        self.set_rename_slot(next);
        self.rename_cursor = cursor;
        cx.notify();
    }

    /// 按光标删一个字：`forward` 真 = Delete（删后面），假 = 退格（删前面）。
    fn delete_rename_at_cursor(&mut self, forward: bool, cx: &mut Context<Self>) {
        self.push_text_undo();
        let Some((_, buffer)) = self.rename_slot() else {
            return;
        };
        let Some((next, cursor)) = rename_delete_at(&buffer, self.rename_cursor, forward) else {
            return;
        };
        self.set_rename_slot(next);
        self.rename_cursor = cursor;
        cx.notify();
    }

    /// 左右 / Home / End 挪光标。
    fn move_rename_cursor(&mut self, key: &str, cx: &mut Context<Self>) {
        let Some((_, buffer)) = self.rename_slot() else {
            return;
        };
        let len = utf16_len(&buffer);
        let at = self.rename_cursor.min(len);
        self.rename_cursor = match key {
            "home" => 0,
            "end" => len,
            "left" if at > 0 => at - prev_char_len(&buffer, at),
            "right" if at < len => at + next_char_len(&buffer, at),
            _ => at,
        };
        cx.notify();
    }

    /// 确认重命名（空白名字就当取消）。
    fn commit_tray_rename(&mut self, window: &mut Window, cx: &mut App) {
        self.text_marked = None;
        let Some((id, title)) = self.tray_rename.take() else {
            return;
        };
        let title = title.trim().to_string();
        if !title.is_empty() {
            let _ = self.config.borrow_mut().update(|config| {
                if let Some(node) = config.tray_menu.root.iter_mut().find(|node| node.id == id) {
                    node.title = title;
                }
            });
            crate::gui::refresh_tray_menu(cx);
        }
        refresh_ui(window, cx);
    }

    /// OSD 文字编辑：回车确认，写进配置。
    fn commit_osd_text(&mut self, window: &mut Window, cx: &mut App) {
        self.text_marked = None;
        let Some((slot, text)) = self.osd_text_edit.take() else {
            return;
        };
        let text = text.trim().to_string();
        let part = if slot == 0 {
            OsdPart::Title
        } else {
            OsdPart::Text(u16::from(slot - 1))
        };
        let _ = self.config.borrow_mut().update(|config| {
            // 这一页只有一套文字框，对准音量模板（预览也是它）。
            with_volume_osd(config, |osd| {
                osd.set_text_for(part, text.clone());
                // 填了字就把这个元素打开：标题默认是关着的，不打开会让人以为没生效。
                if !text.is_empty() {
                    osd.set_part_visible(part, true);
                }
            });
        });
        refresh_ui(window, cx);
    }

    /// OSD 文字编辑时的按键（和子菜单改名同一套手感）。
    fn handle_osd_text_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.osd_text_edit.is_none() {
            return false;
        }
        let key = event.keystroke.key.as_str();
        // 输入法正在拼字：这些键都该交给输入法。
        if self.text_marked.is_some() {
            return false;
        }
        if key == "enter" {
            cx.stop_propagation();
            self.commit_osd_text(window, cx);
            return true;
        }
        if key == "escape" {
            cx.stop_propagation();
            self.osd_text_edit = None;
            self.text_marked = None;
            cx.notify();
            return true;
        }
        if key == "backspace" {
            cx.stop_propagation();
            self.backspace_text_edit(cx);
            return true;
        }
        if key == "v" && event.keystroke.modifiers.control {
            cx.stop_propagation();
            let text = cx
                .read_from_clipboard()
                .and_then(|item| item.text().map(|text| text.to_string()));
            if let Some(text) = text {
                self.insert_rename_text(&text);
                cx.notify();
            }
            return true;
        }
        if event.keystroke.modifiers.control || event.keystroke.modifiers.platform {
            return false;
        }
        if let Some(text) = event.keystroke.key_char.as_ref() {
            cx.stop_propagation();
            self.insert_rename_text(text);
            cx.notify();
            return true;
        }
        false
    }

    /// 黑名单「手动输入 exe 名」：回车或点「添加」才落名单，Esc 放弃。
    ///
    /// 点别处**不算**确认（以前是「失焦即保存」，随手点一下就多出一条）。
    /// 某块名单的手输草稿。
    fn block_draft(&self, list: BlockList) -> &Option<String> {
        match list {
            BlockList::Block => &self.block_name_edit,
            BlockList::Exempt => &self.block_exempt_edit,
        }
    }

    fn block_draft_mut(&mut self, list: BlockList) -> &mut Option<String> {
        match list {
            BlockList::Block => &mut self.block_name_edit,
            BlockList::Exempt => &mut self.block_exempt_edit,
        }
    }

    fn block_focused(&self, list: BlockList) -> bool {
        match list {
            BlockList::Block => self.block_name_focus,
            BlockList::Exempt => self.block_exempt_focus,
        }
    }

    fn set_block_focused(&mut self, list: BlockList, focused: bool) {
        match list {
            BlockList::Block => self.block_name_focus = focused,
            BlockList::Exempt => self.block_exempt_focus = focused,
        }
    }

    /// 正拿着键盘的那块名单（两块不会同时拿着，但顺序写死更省心）。
    fn focused_block_list(&self) -> Option<BlockList> {
        if self.block_name_focus {
            Some(BlockList::Block)
        } else if self.block_exempt_focus {
            Some(BlockList::Exempt)
        } else {
            None
        }
    }

    fn commit_block_name(&mut self, list: BlockList, window: &mut Window, cx: &mut Context<Self>) {
        self.text_marked = None;
        self.set_block_focused(list, false);
        let Some(input) = self.block_draft_mut(list).take() else {
            refresh_ui(window, cx);
            return;
        };
        let input = input.trim().to_string();
        if input.is_empty() {
            refresh_ui(window, cx);
            return;
        }
        let lang = self.config.borrow().get().language;
        let mut outcome = BlockAdd::Invalid;
        let _ = self.config.borrow_mut().update(|config| {
            outcome = match AppBlockEntry::from_input(&input) {
                None => BlockAdd::Invalid,
                Some(entry) => match list {
                    BlockList::Block if config.app_block.contains(&entry) => BlockAdd::Duplicate,
                    BlockList::Block => {
                        config.app_block.entries.push(entry);
                        BlockAdd::Added
                    }
                    BlockList::Exempt if config.app_block.contains_exempt(&entry) => {
                        BlockAdd::Duplicate
                    }
                    BlockList::Exempt => {
                        config.app_block.fullscreen_exempt.push(entry);
                        BlockAdd::Added
                    }
                },
            };
        });
        // 名单变了钩子那边得知道（不然只在设置页里好看，实际不生效）。
        sync_blocklist(&self.config.borrow());
        match outcome {
            BlockAdd::Added => {}
            BlockAdd::Duplicate => self.show_block_notice(
                lang.pick("已经在名单里了", "Already on the list")
                    .to_string(),
                cx,
            ),
            BlockAdd::Invalid => self.show_block_notice(
                lang.pick(
                    "看不出这是哪个程序，写个 exe 名或完整路径",
                    "That does not look like an app — type an exe name or a full path",
                )
                .to_string(),
                cx,
            ),
        }
        refresh_ui(window, cx);
    }

    /// 点别处：只把键盘交回去，草稿留着（想接着写点回输入框就行）。
    fn blur_block_name(&mut self, cx: &mut Context<Self>) {
        let Some(list) = self.focused_block_list() else {
            return;
        };
        self.set_block_focused(list, false);
        self.text_marked = None;
        cx.notify();
    }

    /// 点一下已经有的条目：提示「已存在」，别让它悄无声息地什么都没发生。
    fn notify_block_duplicate(&mut self, cx: &mut Context<Self>) {
        let lang = self.config.borrow().get().language;
        self.show_block_notice(
            lang.pick("已经在名单里了", "Already on the list")
                .to_string(),
            cx,
        );
    }

    /// 黑名单页的一条临时提示：2.6 秒后自己消失。
    fn show_block_notice(&mut self, text: String, cx: &mut Context<Self>) {
        let seq = self.block_notice.as_ref().map(|(_, seq)| *seq).unwrap_or(0) + 1;
        self.block_notice = Some((text, seq));
        cx.notify();
        cx.spawn(async move |this, cx| {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(2600))
                .await;
            let _ = this.update(cx, |this, cx| {
                // 期间又冒出一条新的提示就别清（序号对不上）。
                if this.block_notice.as_ref().is_some_and(|(_, id)| *id == seq) {
                    this.block_notice = None;
                    cx.notify();
                }
            });
        })
        .detach();
    }

    /// 手动输入框的按键（和子菜单改名同一套手感）。两块名单共用。
    fn handle_block_name_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(list) = self.focused_block_list() else {
            return false;
        };
        let key = event.keystroke.key.as_str();
        // 输入法正在拼字：这些键都该交给输入法。
        if self.text_marked.is_some() {
            return false;
        }
        if key == "enter" {
            cx.stop_propagation();
            self.commit_block_name(list, window, cx);
            return true;
        }
        if key == "escape" {
            cx.stop_propagation();
            // Esc 连草稿一起丢掉（点别处只是收起，草稿还留着）。
            *self.block_draft_mut(list) = None;
            self.set_block_focused(list, false);
            self.text_marked = None;
            cx.notify();
            return true;
        }
        if key == "backspace" {
            cx.stop_propagation();
            self.backspace_text_edit(cx);
            return true;
        }
        if key == "v" && event.keystroke.modifiers.control {
            cx.stop_propagation();
            let text = cx
                .read_from_clipboard()
                .and_then(|item| item.text().map(|text| text.to_string()));
            if let Some(text) = text {
                self.insert_rename_text(&text);
                cx.notify();
            }
            return true;
        }
        if event.keystroke.modifiers.control || event.keystroke.modifiers.platform {
            return false;
        }
        if let Some(text) = event.keystroke.key_char.as_ref() {
            cx.stop_propagation();
            self.insert_rename_text(text);
            cx.notify();
            return true;
        }
        false
    }

    /// 改托盘菜单布局：存盘 + 重建原生菜单 + 重画设置页。
    fn edit_tray_menu(
        &self,
        f: impl Fn(&mut crate::config::TrayMenuConfig) + 'static,
    ) -> impl Fn(&mut Window, &mut App) + 'static {
        let store = self.config.clone();
        move |window, cx| {
            let _ = store.borrow_mut().update(|config| f(&mut config.tray_menu));
            crate::gui::refresh_tray_menu(cx);
            refresh_ui(window, cx);
        }
    }

    /// 配置里存下来的设备名套上自定义名：改名之后，旧记录也显示成新名字。
    fn stored_device_label(&self, id: &str, name: &str) -> String {
        let store = self.config.borrow();
        store.get().device_label(id, name)
    }

    /// 这台设备该画的图标：自定义优先，没设就按设备类型猜。
    fn device_icon(&self, id: &str, kind: crate::audio::DeviceKind) -> String {
        let store = self.config.borrow();
        device_icon_resolved(store.get(), id, kind).into_owned()
    }

    fn note_row(&self, text: &'static str) -> AnyElement {
        div()
            .px(px(9.))
            .pb(px(3.))
            .pt(px(1.))
            .text_size(theme::fs(10.5))
            .text_color(theme::text_tertiary())
            .child(text)
            .into_any_element()
    }

    /// 启动名单里的一台设备：名字（没接上的会标出来）+ 启动时启用 / 禁用 + 移除。
    ///
    /// 「记录当前设备开关」是批量照抄现场，这一行是逐个手动改：录制完发现
    /// 有一台记反了，不用整份重录。
    fn startup_device_row(
        &self,
        index: usize,
        state: &crate::config::StartupDeviceState,
        snapshot: &Snapshot,
        mode: crate::config::DeviceStateMode,
        lang: Lang,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let show_flags = mode.per_device_flags();
        let found = snapshot.devices.iter().find(|device| device.id == state.id);
        let kind = found
            .map(|device| device.kind)
            .unwrap_or(crate::audio::DeviceKind::Unknown);
        let missing = found.is_none();
        let enabled = state.enabled;
        // 名单里存的是录制当时的系统名，显示时优先用现在的自定义名。
        let display_name = {
            let store = self.config.borrow();
            let cfg = store.get();
            match found {
                Some(device) => cfg.device_label(&device.id, &device.name),
                None => cfg.device_label(&state.id, &state.name),
            }
        };

        let mode_chip =
            |cx: &mut Context<Self>, slot: &'static str, want: bool, label: &'static str| {
                let store = self.config.clone();
                let selected = enabled == want;
                div()
                    .id(SharedString::from(format!("startup-device-{index}-{slot}")))
                    .px(px(7.))
                    .py(px(2.))
                    .rounded(px(4.))
                    .text_size(theme::fs(11.))
                    .cursor_pointer()
                    .when(selected, |style| {
                        style.bg(theme::accent()).text_color(theme::text_primary())
                    })
                    .when(!selected, |style| style.text_color(theme::text_tertiary()))
                    .on_click(cx.listener(move |_this, _: &ClickEvent, window, cx| {
                        let _ = store.borrow_mut().update(|config| {
                            if let Some(state) = config.startup.device_list_mut(mode).get_mut(index)
                            {
                                state.enabled = want;
                            }
                        });
                        refresh_ui(window, cx);
                    }))
                    .child(label)
            };

        let remove = {
            let store = self.config.clone();
            div()
                .id(SharedString::from(format!("startup-device-{index}-remove")))
                .size(px(18.))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(3.))
                .cursor_pointer()
                .text_size(theme::fs(12.))
                .text_color(theme::text_tertiary())
                .hover(|style| style.bg(theme::surface_hover()))
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    let _ = store.borrow_mut().update(|config| {
                        let list = config.startup.device_list_mut(mode);
                        if index < list.len() {
                            list.remove(index);
                        }
                    });
                    this.clamp_app_edit();
                    refresh_ui(window, cx);
                }))
                .child("×")
        };

        div()
            .flex()
            .items_center()
            .gap(px(8.))
            .px(px(9.))
            .py(px(4.))
            .rounded(px(theme::RADIUS_ROW))
            .child(icon(
                &self.device_icon(&state.id, kind),
                14.0,
                theme::text_tertiary(),
            ))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(
                        div()
                            .min_w(px(0.))
                            .truncate()
                            .text_size(theme::fs(12.))
                            .text_color(if missing {
                                theme::text_tertiary()
                            } else {
                                theme::text_primary()
                            })
                            .child(display_name.clone()),
                    )
                    .when(missing, |row| {
                        row.child(
                            div()
                                .flex_none()
                                .text_size(theme::fs(9.5))
                                .text_color(theme::text_tertiary())
                                .child(lang.pick("未连接", "Not connected")),
                        )
                    }),
            )
            // 黑名单 / 白名单只看成员资格，这两个 chip 摆着只会让人以为选了有用。
            .when(show_flags, |row| {
                row.child(
                    div()
                        .flex()
                        .items_center()
                        .gap(px(1.))
                        .p(px(2.))
                        .rounded(px(theme::RADIUS_SMALL))
                        .bg(theme::track())
                        .child(mode_chip(cx, "on", true, lang.pick("启用", "On")))
                        .child(mode_chip(cx, "off", false, lang.pick("禁用", "Off"))),
                )
            })
            .child(remove)
            .into_any_element()
    }

    /// 启动名单里的一个应用：名字 + 音量（点数字改、± 每次 5%）+ 移除。
    fn startup_app_row(
        &self,
        index: usize,
        app: &crate::config::StartupAppVolume,
        _lang: Lang,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let editing = matches!(&self.app_edit, Some((row, _)) if *row == index);
        let text = match (&self.app_edit, editing) {
            (Some((_, buffer)), true) => format!("{buffer}▏"),
            _ => format!("{}%", app.percent),
        };

        let step =
            |cx: &mut Context<Self>, slot: &'static str, glyph: &'static str, direction: i32| {
                let store = self.config.clone();
                div()
                    .id(SharedString::from(format!("startup-app-{index}-{slot}")))
                    .size(px(18.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(3.))
                    .cursor_pointer()
                    .text_size(theme::fs(11.))
                    .text_color(theme::text_secondary())
                    .hover(|style| style.bg(theme::surface_hover()))
                    .on_click(cx.listener(move |_this, _: &ClickEvent, window, cx| {
                        let _ = store.borrow_mut().update(|config| {
                            if let Some(app) = config.startup.app_volumes.get_mut(index) {
                                let next = app.percent as i32 + direction * 5;
                                app.percent = next.clamp(0, 100) as u32;
                            }
                        });
                        refresh_ui(window, cx);
                    }))
                    .child(glyph)
            };

        let value = {
            let _ = self.config.clone();
            div()
                .id(SharedString::from(format!("startup-app-{index}-value")))
                .w(px(48.))
                .flex_none()
                .py(px(1.))
                .rounded(px(4.))
                .cursor_pointer()
                .when(editing, |style| style.bg(theme::accent()))
                .text_size(theme::fs(11.))
                .text_color(theme::text_primary())
                .text_center()
                .truncate()
                .hover(|style| style.opacity(0.8))
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    this.begin_app_edit(index, window, cx)
                }))
                .child(text)
        };

        let remove = {
            let store = self.config.clone();
            div()
                .id(SharedString::from(format!("startup-app-{index}-remove")))
                .size(px(18.))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(3.))
                .cursor_pointer()
                .text_size(theme::fs(12.))
                .text_color(theme::text_tertiary())
                .hover(|style| style.bg(theme::surface_hover()))
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    let _ = store.borrow_mut().update(|config| {
                        if index < config.startup.app_volumes.len() {
                            config.startup.app_volumes.remove(index);
                        }
                    });
                    this.clamp_app_edit();
                    refresh_ui(window, cx);
                }))
                .child("×")
        };

        div()
            .flex()
            .items_center()
            .gap(px(8.))
            .px(px(9.))
            .py(px(4.))
            .rounded(px(theme::RADIUS_ROW))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .truncate()
                    .text_size(theme::fs(12.))
                    .text_color(theme::text_primary())
                    .child(app.name.clone()),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(1.))
                    .p(px(2.))
                    .rounded(px(theme::RADIUS_SMALL))
                    .bg(theme::track())
                    .child(step(cx, "down", "−", -1))
                    .child(value)
                    .child(step(cx, "up", "+", 1)),
            )
            .child(remove)
            .into_any_element()
    }

    /// 名单变短之后，正在编辑的行号可能已经越界：直接放弃这次输入。
    fn clamp_app_edit(&mut self) {
        let Some((index, _)) = &self.app_edit else {
            return;
        };
        let len = self.config.borrow().get().startup.app_volumes.len();
        if *index >= len {
            self.app_edit = None;
        }
    }

    fn volume_page(
        &self,
        config: &Config,
        lang: Lang,
        snapshot: &Snapshot,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        // 第二个页签整页换成响度均衡：两边内容差得远（这边一串开关、那边一长串
        // 应用），拼成一张长页面反而两边都难找。
        if self.volume_tab == 1 {
            let mut out = vec![self.tab_bar(
                "volume-tabs",
                &[("音量", "Volume"), ("响度均衡", "Loudness")],
                self.volume_tab,
                TabTarget::Volume,
                lang,
                cx,
            )];
            out.extend(self.loudness_page(config, lang, snapshot, cx));
            return out;
        }

        let logarithmic = self.toggle_row(
            "toggle-log",
            lang.pick("使用对数音量刻度", "Use logarithmic volume scale"),
            lang.pick(
                "滑条按 dB 等间隔分布，读数同时显示 dB 与百分比",
                "Slider steps evenly in dB; the readout shows both dB and percent",
            ),
            config.use_logarithmic_volume,
            self.bind(|config| config.use_logarithmic_volume = !config.use_logarithmic_volume),
        );

        let min_db = {
            let store = self.config.clone();
            let current = config.logarithmic_min_db;
            self.stepper_row(
                "log-min-db",
                lang.pick("对数刻度下限", "Log scale bottom"),
                format!("{current} dB"),
                move |direction, window, cx| {
                    let next = (current + direction * 10).clamp(-60, -20);
                    if next == current {
                        return;
                    }
                    let _ = store
                        .borrow_mut()
                        .update(|config| config.logarithmic_min_db = next);
                    refresh_ui(window, cx);
                },
            )
        };

        // 滚轮步进有两套算法：按百分比（固定行程）或按分贝数（固定真实 dB 变化）。
        // 选哪套是独立开关 —— 刻度只决定滑条怎么画，不该顺带决定滚轮怎么走。
        let wheel_in_db = self.toggle_row(
            "toggle-wheel-db",
            lang.pick("滚轮按分贝数步进", "Wheel steps in dB"),
            lang.pick(
                "每格按固定 dB 数调整，与「使用对数音量刻度」无关；关闭则按下面的百分比步进",
                "Each notch changes a fixed number of dB, independent of the log scale; \
                 otherwise the percent step below is used",
            ),
            config.scroll_wheel_in_db,
            self.bind(|config| config.scroll_wheel_in_db = !config.scroll_wheel_in_db),
        );

        let percent_step = self.number_row(
            "wheel-step",
            lang.pick("百分比滚轮步进", "Wheel step (percent)"),
            lang.pick(
                "按百分比步进时，每个滚轮格子改变多少个百分点；点数字可直接填",
                "How many percent points one notch moves when stepping by percent",
            ),
            NumberField::WheelStep,
            config,
            lang,
            cx,
        );

        let log_step = self.number_row(
            "wheel-step-log",
            lang.pick("分贝数滚轮步进", "Wheel step (dB)"),
            lang.pick(
                "按分贝数步进时，每个滚轮格子改变多少 dB（真实音量变化），最小 0.1 dB",
                "How many dB one notch changes when stepping in dB, as low as 0.1 dB",
            ),
            NumberField::LogWheelStep,
            config,
            lang,
            cx,
        );

        let release_sound = self.toggle_row(
            "toggle-release-sound",
            lang.pick(
                "松开滑条时播放提示音",
                "Play a sound when releasing the slider",
            ),
            lang.pick(
                "拖动音量条松手时发出的一声轻响，默认开启",
                "A soft beep when you let go of a volume slider",
            ),
            config.release_sound,
            self.bind(|config| config.release_sound = !config.release_sound),
        );
        // 子项：只在主开关开着时出现（关掉主开关以后这一项没有意义，
        // 留着只会让人以为它还能单独生效）。
        let release_sound_osd = if config.release_sound {
            Some(self.compact_toggle_row(
                "toggle-release-sound-osd",
                lang.pick(
                    "屏幕提示（OSD）里也响",
                    "Also in the on-screen display",
                ),
                lang.pick(
                    "拖完那根滑条松手也响一声",
                    "Beep when you let go of the OSD slider",
                ),
                config.release_sound_osd,
                self.bind(|config| config.release_sound_osd = !config.release_sound_osd),
            ))
        } else {
            None
        };

        let tray_wheel = self.toggle_row(
            "toggle-tray-wheel",
            lang.pick(
                "在托盘图标上滚动滚轮调节音量",
                "Scroll on the tray icon to change volume",
            ),
            lang.pick(
                "鼠标停在托盘图标上滚动即可调整音量",
                "Hover the tray icon and scroll to adjust the volume",
            ),
            config.use_scroll_wheel_in_tray,
            self.bind(|config| config.use_scroll_wheel_in_tray = !config.use_scroll_wheel_in_tray),
        );

        let flyout_wheel = self.toggle_row(
            "toggle-flyout-wheel",
            lang.pick(
                "面板内滚动滚轮调节音量",
                "Scroll inside the panel to change volume",
            ),
            lang.pick(
                "在面板的设备行上滚动滚轮即可调整音量",
                "Scroll on the panel to adjust the volume",
            ),
            config.use_global_mouse_wheel_hook,
            self.bind(|config| {
                config.use_global_mouse_wheel_hook = !config.use_global_mouse_wheel_hook
            }),
        );

        let app_wheel = self.toggle_row(
            "toggle-app-wheel",
            lang.pick(
                "在应用行上滚动滚轮调节该应用音量",
                "Scroll on an app row to change its volume",
            ),
            lang.pick(
                "把光标停在某个应用上滚动滚轮，只调整这一个应用",
                "Hover an app row and scroll to adjust only that app",
            ),
            config.use_scroll_wheel_in_apps,
            self.bind(|config| config.use_scroll_wheel_in_apps = !config.use_scroll_wheel_in_apps),
        );

        // 最大音量限制
        let cap_enabled = {
            let store = self.config.clone();
            self.toggle_row(
                "toggle-volume-cap",
                lang.pick("限制最大音量", "Limit the maximum volume"),
                lang.pick(
                    "滑条、滚轮、快捷键和硬件音量键都不会超过上限",
                    "Slider, wheel, hotkeys and hardware keys all stop at the cap",
                ),
                config.volume_cap.enabled,
                move |window, cx| {
                    let _ = store
                        .borrow_mut()
                        .update(|config| config.volume_cap.enabled = !config.volume_cap.enabled);
                    crate::gui::refresh_volume_cap(cx);
                    refresh_ui(window, cx);
                },
            )
        };

        let cap_percent = if config.volume_cap.enabled {
            Some(self.number_row(
                "volume-cap-value",
                lang.pick("音量上限", "Volume cap"),
                lang.pick("点数字可以直接填", "Click the number to type"),
                NumberField::VolumeCap,
                config,
                lang,
                cx,
            ))
        } else {
            None
        };

        let scope = if config.volume_cap.enabled {
            let store = self.config.clone();
            let current_id = snapshot.current_device_id.clone().unwrap_or_default();
            let current_name = snapshot
                .devices
                .iter()
                .find(|device| Some(&device.id) == snapshot.current_device_id.as_ref())
                .map(|device| device.name.clone())
                .unwrap_or_default();
            let scope_options: Vec<(u32, String)> = vec![
                (0, lang.pick("所有设备", "All devices").to_string()),
                (1, lang.pick("指定设备", "One device").to_string()),
            ];
            Some(self.choice_row_owned(
                "volume-cap-scope",
                lang.pick("作用范围", "Applies to"),
                lang.pick(
                    "「指定设备」会把上限记住在那台设备上",
                    "“One device” remembers the cap on that device",
                ),
                scope_options,
                if config.volume_cap.device_id.is_empty() {
                    0
                } else {
                    1
                },
                move |value, window, cx| {
                    let _ = store.borrow_mut().update(|config| {
                        if value == 0 {
                            config.volume_cap.device_id.clear();
                            config.volume_cap.device_name.clear();
                        } else if config.volume_cap.device_id.is_empty() {
                            // 头一次绑：直接用当前默认设备，省得还要再选一遍。
                            config.volume_cap.device_id = current_id.clone();
                            config.volume_cap.device_name = current_name.clone();
                        }
                    });
                    crate::gui::refresh_volume_cap(cx);
                    refresh_ui(window, cx);
                },
            ))
        } else {
            None
        };

        // 绑定到哪台设备：只在「指定设备」时才展开，选项来自真实设备列表。
        let cap_device = if config.volume_cap.enabled && !config.volume_cap.device_id.is_empty() {
            Some(self.device_picker_row(
                "volume-cap-device",
                lang.pick("绑定设备", "Bound device"),
                lang.pick(
                    "上限记住在这台设备上",
                    "The cap is remembered on this device",
                ),
                &self.stored_device_label(
                    &config.volume_cap.device_id,
                    &config.volume_cap.device_name,
                ),
                lang.pick("选一台设备", "Pick a device"),
                DevicePicker::BoundDevice,
                cx,
            ))
        } else {
            None
        };

        // 「调音量时解除静音」：总开关 + 方向 × 操作方式（主设备四条、应用三条）。
        let unmute_master = self.toggle_row(
            "toggle-unmute-on-change",
            lang.pick("调音量时解除静音", "Unmute when the volume changes"),
            lang.pick(
                "静音状态下动一下音量就恢复出声",
                "Changing the volume while muted turns the sound back on",
            ),
            config.unmute.enabled,
            self.bind(|config| config.unmute.enabled = !config.unmute.enabled),
        );

        let mut unmute_rows = vec![unmute_master];
        if config.unmute.enabled {
            // 行 = 操作方式，列 = 增大 / 减小。
            //
            // 原来是一格一行、每行都带一句「用这种方式调音量时，顺手把静音解掉」：
            // 14 格就是十四行、十四句同一句话，一个开关被撑成了半页。改成矩阵后
            // 那句话只在主开关下面说一遍，14 个开关压成 7 行，方向交给列头。
            let rows: [(&'static str, UnmuteCell, UnmuteCell); 7] = [
                (
                    lang.pick("快捷键", "Hotkey"),
                    (
                        "toggle-unmute-hotkey-up",
                        config.unmute.increase_hotkey,
                        |config| config.unmute.increase_hotkey = !config.unmute.increase_hotkey,
                    ),
                    (
                        "toggle-unmute-hotkey-down",
                        config.unmute.decrease_hotkey,
                        |config| config.unmute.decrease_hotkey = !config.unmute.decrease_hotkey,
                    ),
                ),
                (
                    lang.pick("悬停滚动", "Hover scroll"),
                    (
                        "toggle-unmute-hover-up",
                        config.unmute.increase_hover,
                        |config| config.unmute.increase_hover = !config.unmute.increase_hover,
                    ),
                    (
                        "toggle-unmute-hover-down",
                        config.unmute.decrease_hover,
                        |config| config.unmute.decrease_hover = !config.unmute.decrease_hover,
                    ),
                ),
                (
                    lang.pick("拖动滑条", "Slider drag"),
                    (
                        "toggle-unmute-drag-up",
                        config.unmute.increase_drag,
                        |config| config.unmute.increase_drag = !config.unmute.increase_drag,
                    ),
                    (
                        "toggle-unmute-drag-down",
                        config.unmute.decrease_drag,
                        |config| config.unmute.decrease_drag = !config.unmute.decrease_drag,
                    ),
                ),
                (
                    lang.pick("托盘滚动", "Tray scroll"),
                    (
                        "toggle-unmute-tray-up",
                        config.unmute.increase_tray,
                        |config| config.unmute.increase_tray = !config.unmute.increase_tray,
                    ),
                    (
                        "toggle-unmute-tray-down",
                        config.unmute.decrease_tray,
                        |config| config.unmute.decrease_tray = !config.unmute.decrease_tray,
                    ),
                ),
                (
                    lang.pick("应用音量条 · 悬停滚动", "App slider · hover scroll"),
                    (
                        "toggle-unmute-app-hover-up",
                        config.unmute.increase_app_hover,
                        |config| config.unmute.increase_app_hover = !config.unmute.increase_app_hover,
                    ),
                    (
                        "toggle-unmute-app-hover-down",
                        config.unmute.decrease_app_hover,
                        |config| config.unmute.decrease_app_hover = !config.unmute.decrease_app_hover,
                    ),
                ),
                (
                    lang.pick("应用音量条 · 拖动", "App slider · drag"),
                    (
                        "toggle-unmute-app-drag-up",
                        config.unmute.increase_app_drag,
                        |config| config.unmute.increase_app_drag = !config.unmute.increase_app_drag,
                    ),
                    (
                        "toggle-unmute-app-drag-down",
                        config.unmute.decrease_app_drag,
                        |config| config.unmute.decrease_app_drag = !config.unmute.decrease_app_drag,
                    ),
                ),
                (
                    lang.pick("应用音量条 · 快捷键", "App slider · hotkey"),
                    (
                        "toggle-unmute-app-hotkey-up",
                        config.unmute.increase_app_hotkey,
                        |config| config.unmute.increase_app_hotkey = !config.unmute.increase_app_hotkey,
                    ),
                    (
                        "toggle-unmute-app-hotkey-down",
                        config.unmute.decrease_app_hotkey,
                        |config| config.unmute.decrease_app_hotkey = !config.unmute.decrease_app_hotkey,
                    ),
                ),
            ];

            let mut matrix: Vec<AnyElement> = vec![self.unmute_matrix_header(lang)];
            for (index, (title, up, down)) in rows.into_iter().enumerate() {
                // 前四行管主音量，后三行管应用音量条：中间一条细线分开。
                if index == 4 {
                    matrix.push(self.unmute_matrix_divider());
                }
                matrix.push(self.unmute_matrix_row(title, up, down));
            }
            unmute_rows.push(self.sub_group(matrix));
        }

        let sound = self.sound_rows(config, lang, cx);
        vec![
            // 页签永远排在第一行，滚出视野也看得见自己在哪一签。
            self.tab_bar(
                "volume-tabs",
                &[("音量", "Volume"), ("响度均衡", "Loudness")],
                self.volume_tab,
                TabTarget::Volume,
                lang,
                cx,
            ),
            self.section(lang.pick("音量", "Volume"), "volume-high", {
                let mut rows = vec![logarithmic, min_db, release_sound];
                if let Some(row) = release_sound_osd {
                    rows.push(self.sub_group(vec![row]));
                }
                rows
            }),
            // 音效紧跟在「音量」后面：它管的就是调音量时的那一下反馈。
            self.section(
                lang.pick("音效", "Sound effects"),
                "volume-arc-high",
                sound,
            ),
            self.section(lang.pick("最大音量", "Volume cap"), "volume-off", {
                let mut rows = vec![cap_enabled];
                let mut expanded = Vec::new();
                if let Some(row) = cap_percent {
                    expanded.push(row);
                }
                if let Some(row) = scope {
                    expanded.push(row);
                }
                if let Some(row) = cap_device {
                    expanded.push(row);
                }
                if !expanded.is_empty() {
                    rows.push(self.sub_group(expanded));
                }
                rows
            }),
            self.section(lang.pick("静音", "Mute"), "volume-off", unmute_rows),
            self.section(
                lang.pick("滚轮", "Scroll wheel"),
                "music",
                vec![
                    wheel_in_db,
                    percent_step,
                    log_step,
                    tray_wheel,
                    flyout_wheel,
                    app_wheel,
                ],
            ),
        ]
    }

    /// 响度均衡这一页：全局开关 + 两个参数 + 已单独设置的应用。
    ///
    /// 算法和这两个参数的语义照 TomSteady（`dsp_tom.dll` v1.4，Winamp 的 AGC 插件）。
    /// 它面板上就两个滑块：`Desired Real Volume`（= 这里说「最大放大级别」）
    /// 与 `Volume Slide Smoothness`（INI 里叫 `stepping`，= 「音量变化级别」，默认 8）。
    /// 插件自己的说明文件原话是「这个插件音量控制得很细，它会自动调，没有需要设置的东西」。
    fn loudness_page(
        &self,
        config: &Config,
        lang: Lang,
        snapshot: &Snapshot,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let loud = config.loudness;
        let global_on = loud.enabled;

        let enabled = self.toggle_row(
            "toggle-loudness",
            lang.pick("全局响度均衡", "Global loudness equalization"),
            lang.pick(
                "所有应用都按这一套来；单个应用可以在面板里右键单独开或关",
                "Applies to every app; override any single app from its right-click menu",
            ),
            global_on,
            self.bind_loudness(|config| config.loudness.enabled = !config.loudness.enabled),
        );

        // 两个参数对应 TomSteady 面板上那两个滑块，别的都没有 —— 它自己的
        // 说明文件还写着「你不用调，它会自己调」。
        let level = self.number_row(
            "loudness-level",
            lang.pick("最大放大级别", "Max amplification"),
            lang.pick(
                "最多把声音推到满刻度的多少；100% 会落在九成左右，再高就削顶了",
                "How loud the app may get, as a share of full scale; 100% lands around 90% so nothing clips",
            ),
            NumberField::LoudnessLevel,
            config,
            lang,
            cx,
        );

        let smoothness = self.number_row(
            "loudness-smoothness",
            lang.pick("音量变化级别", "Volume change level"),
            lang.pick(
                "越大越稳、越小越跟手；10 大约三四十秒走完一圈，1 是一下就到",
                "Higher is steadier, lower follows faster; 10 takes tens of seconds to sweep",
            ),
            NumberField::LoudnessSmooth,
            config,
            lang,
            cx,
        );

        // 单独设置过的应用：行上点一下就是切换，切回和全局一致时这一行会自己消失
        // （`set_app_loudness` 会把「跟全局一样」的条目删掉）。
        let mut app_rows: Vec<AnyElement> = Vec::new();
        if config.app_loudness.is_empty() {
            app_rows.push(
                div()
                    .px(px(9.))
                    .py(px(5.))
                    .text_size(theme::fs(10.5))
                    .text_color(theme::text_tertiary())
                    .child(lang.pick(
                        "还没有单独设置的应用。在面板里右键任一应用 →「响度均衡」就能单独开关它。",
                        "Nothing set per app yet. Right-click an app in the panel and toggle “Loudness equalization”.",
                    ))
                    .into_any_element(),
            );
        } else {
            for entry in &config.app_loudness {
                let name = snapshot
                    .apps
                    .iter()
                    .find(|app| app.key == entry.key)
                    .map(|app| config.app_label(&entry.key, &app.name))
                    .unwrap_or_else(|| app_key_file_name(&entry.key));
                let hint = match (entry.enabled, global_on) {
                    (true, false) => lang.pick("单独开启（全局是关的）", "On for this app only"),
                    (false, true) => lang.pick("单独关闭（全局是开的）", "Off for this app only"),
                    // 和全局一样：条目只是「记住这个应用的选择」，等全局翻面才起作用。
                    (true, true) => lang.pick("开着（和全局一致）", "On, same as global"),
                    (false, false) => lang.pick("关着（和全局一致）", "Off, same as global"),
                };
                app_rows.push(self.loudness_app_row(&entry.key, name, hint, entry.enabled));
            }
        }

        // 参数一直摆着，开关关着也照样能调：这一页本身就是「响度均衡」，
        // 一进来只看到一个空开关、看不到均衡到底在调什么，反而像是没做完。
        //
        // 以前只在「全局开 || 有应用单独开」时才展开，于是「全局关、只给某个
        // 应用单独开」的人根本找不到这两个参数 —— 明明那一路正在用它们。
        let mut params: Vec<AnyElement> = Vec::new();
        if !global_on {
            params.push(
                div()
                    .px(px(9.))
                    .py(px(2.))
                    .text_size(theme::fs(10.5))
                    .text_color(theme::text_tertiary())
                    .child(lang.pick(
                        "全局是关的：下面这几个参数只对已单独开启的应用生效。",
                        "Global is off; these values apply only to the apps enabled individually below.",
                    ))
                    .into_any_element(),
            );
        }
        params.extend([level, smoothness]);

        let takeover_note = self.sub_group(vec![
            div()
                .px(px(9.))
                .py(px(2.))
                .text_size(theme::fs(10.5))
                .text_color(theme::text_tertiary())
                .child(lang.pick(
                    "需要的放大倍数超过系统音量上限（100% 还不够响）时，会单独接管这个应用的声音补上差额，大约晚 40 毫秒；受保护的内容（DRM）和独占模式的应用接管不了。",
                    "When the app needs more than the system's 100% ceiling, its audio is taken over to supply the rest — about 40 ms later. DRM-protected and exclusive-mode apps cannot be taken over.",
                ))
                .into_any_element(),
        ]);

        vec![
            self.section(
                lang.pick("响度均衡", "Loudness equalization"),
                "levels",
                vec![enabled, self.sub_group(params), takeover_note],
            ),
            self.section(
                lang.pick("单独设置的应用", "Per-app overrides"),
                "apps",
                vec![self.sub_group(app_rows)],
            ),
        ]
    }

    /// 单应用那一行：点一下切换开关，切回和全局一致时条目会被删掉。
    fn loudness_app_row(
        &self,
        key: &str,
        title: String,
        hint: &'static str,
        value: bool,
    ) -> AnyElement {
        let store = self.config.clone();
        let key_owned = key.to_string();
        div()
            .id(SharedString::from(format!("loudness-app-{key_owned}")))
            .flex()
            .items_center()
            .gap(px(8.))
            .px(px(7.))
            .py(px(3.))
            .rounded(px(theme::RADIUS_SMALL))
            .cursor_pointer()
            .hover(|style| style.bg(theme::surface_hover()))
            .on_click(move |_, window, cx| {
                let target = !value;
                let key = key_owned.clone();
                let _ = store.borrow_mut().update(|config| {
                    config.set_app_loudness(&key, Some(target));
                });
                crate::gui::refresh_loudness(cx);
                refresh_ui(window, cx);
            })
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(theme::fs(12.))
                    .text_color(theme::text_primary())
                    .child(title),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(theme::fs(10.5))
                    .text_color(theme::text_tertiary())
                    .child(hint),
            )
            .child(self.mini_switch(value))
            .into_any_element()
    }

    fn shortcuts_page(
        &self,
        config: &Config,
        lang: Lang,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let device_db = config.hotkey_step_is_db(crate::config::HotkeyStepTarget::Device);
        let hotkey_step = self.number_row(
            "hotkey-volume-step",
            lang.pick("快捷键音量步进[设备]", "Hotkey volume step [device]"),
            lang.pick(
                "按一次音量键改变多少；点数字可直接填",
                "How much one key press changes; click the number to type",
            ),
            // 单位跟着模式走：dB 模式显示 dB，否则显示 %。
            if device_db {
                NumberField::HotkeyStepDb
            } else {
                NumberField::HotkeyStep
            },
            config,
            lang,
            cx,
        );

        let hotkey_follow_log = self.toggle_row(
            "hotkey-follow-log",
            lang.pick(
                "快捷键跟随对数刻度[设备]",
                "Hotkeys follow the log scale [device]",
            ),
            lang.pick(
                "对数刻度下按 dB 走，关闭则按固定百分点",
                "Steps in dB on the log scale, otherwise fixed percent",
            ),
            config.hotkey_volume_follow_log,
            self.bind(|config| config.hotkey_volume_follow_log = !config.hotkey_volume_follow_log),
        );

        let hotkey_in_db = self.toggle_row(
            "hotkey-in-db",
            lang.pick("切换为固定 dB 模式[设备]", "Fixed dB step [device]"),
            lang.pick(
                "与刻度怎么画无关，每按一次大一档固定 dB",
                "Always moves a fixed number of dB, whatever the slider scale",
            ),
            config.hotkey_volume_in_db,
            self.bind(|config| config.hotkey_volume_in_db = !config.hotkey_volume_in_db),
        );

        let app_db = config.hotkey_step_is_db(crate::config::HotkeyStepTarget::App);
        let app_step = self.number_row(
            "hotkey-app-volume-step",
            lang.pick(
                "快捷键音量步进[当前应用]",
                "Hotkey volume step [current app]",
            ),
            lang.pick(
                "只作用于正在发声的那个应用；点数字可直接填",
                "Only affects the app that is currently playing; click the number to type",
            ),
            if app_db {
                NumberField::AppHotkeyStepDb
            } else {
                NumberField::AppHotkeyStep
            },
            config,
            lang,
            cx,
        );

        let app_follow_log = self.toggle_row(
            "hotkey-app-follow-log",
            lang.pick(
                "快捷键跟随对数刻度[当前应用]",
                "Hotkeys follow the log scale [current app]",
            ),
            lang.pick(
                "对数刻度下按 dB 走，关闭则按固定百分点",
                "Steps in dB on the log scale, otherwise fixed percent",
            ),
            config.hotkey_app_volume_follow_log,
            self.bind(|config| {
                config.hotkey_app_volume_follow_log = !config.hotkey_app_volume_follow_log
            }),
        );

        let app_in_db = self.toggle_row(
            "hotkey-app-in-db",
            lang.pick(
                "切换为固定 dB 模式[当前应用]",
                "Fixed dB step [current app]",
            ),
            lang.pick(
                "与刻度怎么画无关，每按一次大一档固定 dB",
                "Always moves a fixed number of dB, whatever the slider scale",
            ),
            config.hotkey_app_volume_in_db,
            self.bind(|config| config.hotkey_app_volume_in_db = !config.hotkey_app_volume_in_db),
        );

        let rows = vec![
            self.hotkey_row(
                "hotkey-flyout",
                lang.pick("打开 / 收起面板", "Open / close panel"),
                config.hotkey_flyout,
                HotkeySlot::Flyout,
                crate::platform::hotkey::Action::Flyout,
                lang,
                cx,
            ),
            self.hotkey_row(
                "hotkey-settings",
                lang.pick("打开设置", "Open settings"),
                config.hotkey_settings,
                HotkeySlot::Settings,
                crate::platform::hotkey::Action::Settings,
                lang,
                cx,
            ),
            self.hotkey_row(
                "hotkey-volume-up",
                lang.pick("音量增加[设备]", "Volume up [device]"),
                config.hotkey_volume_up,
                HotkeySlot::VolumeUp,
                crate::platform::hotkey::Action::VolumeUp,
                lang,
                cx,
            ),
            self.hotkey_row(
                "hotkey-volume-down",
                lang.pick("音量减少[设备]", "Volume down [device]"),
                config.hotkey_volume_down,
                HotkeySlot::VolumeDown,
                crate::platform::hotkey::Action::VolumeDown,
                lang,
                cx,
            ),
            self.hotkey_row(
                "hotkey-app-volume-up",
                lang.pick("音量增加[当前应用]", "Volume up [current app]"),
                config.hotkey_app_volume_up,
                HotkeySlot::AppVolumeUp,
                crate::platform::hotkey::Action::AppVolumeUp,
                lang,
                cx,
            ),
            self.hotkey_row(
                "hotkey-app-volume-down",
                lang.pick("音量减少[当前应用]", "Volume down [current app]"),
                config.hotkey_app_volume_down,
                HotkeySlot::AppVolumeDown,
                crate::platform::hotkey::Action::AppVolumeDown,
                lang,
                cx,
            ),
            self.hotkey_row(
                "hotkey-media-prev",
                lang.pick("上一曲", "Previous track"),
                config.hotkey_media_prev,
                HotkeySlot::MediaPrev,
                crate::platform::hotkey::Action::MediaPrev,
                lang,
                cx,
            ),
            self.hotkey_row(
                "hotkey-media-play-pause",
                lang.pick("播放 / 暂停", "Play / pause"),
                config.hotkey_media_play_pause,
                HotkeySlot::MediaPlayPause,
                crate::platform::hotkey::Action::MediaPlayPause,
                lang,
                cx,
            ),
            self.hotkey_row(
                "hotkey-media-next",
                lang.pick("下一曲", "Next track"),
                config.hotkey_media_next,
                HotkeySlot::MediaNext,
                crate::platform::hotkey::Action::MediaNext,
                lang,
                cx,
            ),
            self.hotkey_row(
                "hotkey-media-stop",
                lang.pick("停止播放", "Stop"),
                config.hotkey_media_stop,
                HotkeySlot::MediaStop,
                crate::platform::hotkey::Action::MediaStop,
                lang,
                cx,
            ),
            self.hotkey_row(
                "hotkey-media-mute",
                lang.pick("静音", "Mute"),
                config.hotkey_media_mute,
                HotkeySlot::MediaMute,
                crate::platform::hotkey::Action::MediaMute,
                lang,
                cx,
            ),
        ];

        // 屏幕亮度这三组单独一页签：它们跟音量不是一个设备类别。
        let screen_rows = vec![
            self.hotkey_row(
                "hotkey-brightness-up",
                lang.pick("亮度增加[当前屏幕]", "Brightness up [current screen]"),
                config.hotkey_brightness_up,
                HotkeySlot::BrightnessUp,
                crate::platform::hotkey::Action::BrightnessUp,
                lang,
                cx,
            ),
            self.hotkey_row(
                "hotkey-brightness-down",
                lang.pick("亮度减少[当前屏幕]", "Brightness down [current screen]"),
                config.hotkey_brightness_down,
                HotkeySlot::BrightnessDown,
                crate::platform::hotkey::Action::BrightnessDown,
                lang,
                cx,
            ),
            self.hotkey_row(
                "hotkey-brightness-all-up",
                lang.pick("亮度增加[全部屏幕]", "Brightness up [all screens]"),
                config.hotkey_brightness_all_up,
                HotkeySlot::BrightnessAllUp,
                crate::platform::hotkey::Action::BrightnessAllUp,
                lang,
                cx,
            ),
            self.hotkey_row(
                "hotkey-brightness-all-down",
                lang.pick("亮度减少[全部屏幕]", "Brightness down [all screens]"),
                config.hotkey_brightness_all_down,
                HotkeySlot::BrightnessAllDown,
                crate::platform::hotkey::Action::BrightnessAllDown,
                lang,
                cx,
            ),
            self.hotkey_row(
                "hotkey-brightness-monitor-up",
                lang.pick("亮度增加[指定屏幕]", "Brightness up [set screen]"),
                config.hotkey_brightness_monitor_up,
                HotkeySlot::BrightnessMonitorUp,
                crate::platform::hotkey::Action::BrightnessMonitorUp,
                lang,
                cx,
            ),
            self.hotkey_row(
                "hotkey-brightness-monitor-down",
                lang.pick("亮度减少[指定屏幕]", "Brightness down [set screen]"),
                config.hotkey_brightness_monitor_down,
                HotkeySlot::BrightnessMonitorDown,
                crate::platform::hotkey::Action::BrightnessMonitorDown,
                lang,
                cx,
            ),
        ];

        // NumLock 关着的时候小键盘发的是方向 / Ins / Del，`RegisterHotKey` 那一路
        // 认不出来；勾上就由键盘钩子按扫描码把它们认回数字再匹配。
        let numpad_raw = self.toggle_row(
            "hotkey-numpad-raw",
            lang.pick(
                "NumLock 关闭时仍识别小键盘数字",
                "Read the numpad as digits when NumLock is off",
            ),
            lang.pick(
                "关着 NumLock 时小键盘发出来的是方向 / 删除键；勾上后录过的 Num4 之类照样触发，没配成快捷键的键在你别的程序里还是方向键",
                "With NumLock off the numpad sends arrows and Delete. Turn this on and Num4-style bindings still fire; keys you never bound stay arrows in other apps",
            ),
            config.hotkey_numpad_raw,
            self.bind(|config| {
                config.hotkey_numpad_raw = !config.hotkey_numpad_raw;
                crate::platform::hotkey::set_numpad_raw(config.hotkey_numpad_raw);
            }),
        );

        // 亮度快捷键自己的步进：面板上的 +/− 想细，键盘按一下想大，分开配。
        let brightness_step = self.number_row(
            "hotkey-brightness-step",
            lang.pick("亮度快捷键步进", "Brightness hotkey step"),
            lang.pick(
                "按一次亮度键走多少；「跟随」= 用「显示器」页里的亮度步进",
                "How much one key press changes; Match uses the brightness step on the Display page",
            ),
            NumberField::BrightnessHotkeyStep,
            config,
            lang,
            cx,
        );

        // 「指定屏幕」调第几台：编号就是亮度面板里从左到右的顺序。
        let monitor_index = self.number_row(
            "hotkey-brightness-monitor",
            lang.pick("指定屏幕编号", "Set screen number"),
            lang.pick(
                "「指定屏幕」那两组快捷键调第几台，和亮度面板里的顺序一致",
                "Which screen the “set screen” shortcuts drive, in panel order",
            ),
            NumberField::BrightnessHotkeyMonitor,
            config,
            lang,
            cx,
        );
        let brightness_hint = div()
            .px(px(9.))
            .py(px(2.))
            .text_size(theme::fs(10.5))
            .text_color(theme::text_tertiary())
            .child(lang.pick(
                "「当前屏幕」= 鼠标所在的那块；DDC/CI 不通的屏会被跳过。按键只改亮度、不弹任何窗口。",
                "“Current screen” is the one under the pointer. Screens without DDC/CI are skipped. Keys change the brightness only — no popup.",
            ))
            .into_any_element();

        // 两页签：音频设备（音量 / 媒体键）/ 屏幕设备（亮度）。
        let tabs = self.tab_bar(
            "shortcut-tabs",
            &[("音频设备", "Audio devices"), ("屏幕设备", "Screens")],
            self.shortcut_tab,
            TabTarget::Shortcuts,
            lang,
            cx,
        );

        let mut out = vec![tabs];
        // 「按住连续触发」那一行和它管辖的勾选框圈成同一个框：开关在最上面、
        // 勾在每行最前面，不放到一起看不出「下面这些勾归上面这个开关管」。
        if self.shortcut_tab == 0 {
            out.push(self.section(
                lang.pick("快捷键", "Shortcuts"),
                "keyboard",
                vec![self.repeat_group(config, lang, rows, cx)],
            ));
            out.push(self.section(lang.pick("小键盘", "Numpad"), "keyboard", vec![numpad_raw]));
            out.push(self.section(
                lang.pick("步进", "Step"),
                "volume-low",
                vec![
                    hotkey_step,
                    hotkey_follow_log,
                    hotkey_in_db,
                    app_step,
                    app_follow_log,
                    app_in_db,
                ],
            ));
            out.push(
                div()
                    .px(px(9.))
                    .text_size(theme::fs(10.5))
                    .text_color(theme::text_tertiary())
                    .child(lang.pick(
                        "点任意一格开始录制，按下组合键即保存；Backspace 解除绑定，Esc 放弃。媒体键（播放/暂停、上一曲）可以直接录。鼠标也认：按住 Ctrl / Alt / Shift 再滚轮上下、滚轮中键、侧键、滚轮左右摆（鼠标支持的话）。裸滚轮不记录，否则全系统的滚动都会被它吃掉。",
                        "Click a slot and press the combo to save it. Backspace unbinds it, Esc cancels. Media keys work too. Mice work as well: hold Ctrl / Alt / Shift and then scroll, middle-click, side buttons or tilt the wheel. Bare scrolling is never recorded — it would eat scrolling everywhere.",
                    ))
                    .into_any_element(),
            );
        } else {
            out.push(self.section(
                lang.pick("亮度快捷键", "Brightness shortcuts"),
                "keyboard",
                vec![self.repeat_group(config, lang, screen_rows, cx)],
            ));
            out.push(self.section(
                lang.pick("亮度快捷键设置", "Brightness shortcut settings"),
                "sun",
                vec![brightness_step, monitor_index, brightness_hint],
            ));
        }
        out
    }

    /// 「按住连续触发」那一条 + 它管辖的快捷键行，圈成卡片里的一个小框。
    ///
    /// 只并成一张卡还不够：开关在最顶上、勾选框在每一行最前面，中间隔着七八行，
    /// 看着像两回事。给它们共同的一层边框和下沉底色，一眼就知道
    /// 「下面这些勾归上面这个开关管」。
    fn repeat_group(
        &self,
        config: &Config,
        lang: Lang,
        rows: Vec<AnyElement>,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        div()
            .flex()
            .flex_col()
            .gap(px(1.))
            .p(px(3.))
            .rounded(px(theme::RADIUS_ROW))
            .border(px(1.))
            .border_color(theme::border())
            .bg(theme::surface_sunken())
            .child(self.repeat_bar(config, lang, cx))
            .children(rows)
            .into_any_element()
    }

    /// 快捷键卡片顶上那一行：「按住连续触发」总闸 + 间隔 + 忽略系统长按重复。
    ///
    /// 原来这是单独一张卡片（标题一行、开关一行、子项两行），和「快捷键」卡片
    /// 上下叠着看很碎。并进来之后压成一行，具体哪个键连发交给每一行
    /// 最前面那个勾 —— 总闸只负责「全开 / 全关」。
    fn repeat_bar(&self, config: &Config, lang: Lang, cx: &mut Context<Self>) -> AnyElement {
        let toggle_on = self.bind(|config| {
            config.hotkey_hold_repeat = !config.hotkey_hold_repeat;
            crate::platform::hotkey::apply_config(config);
        });
        let toggle_ignore = self.bind(|config| {
            config.hotkey_ignore_system_repeat = !config.hotkey_ignore_system_repeat;
            crate::platform::hotkey::apply_config(config);
        });

        let knob_x = if config.hotkey_hold_repeat { 16.0 } else { 2.0 };
        let switch = div()
            .w(px(34.))
            .h(px(20.))
            .flex_none()
            .rounded(px(10.))
            .bg(if config.hotkey_hold_repeat {
                theme::accent()
            } else {
                theme::track()
            })
            .relative()
            .child(
                div()
                    .absolute()
                    .top(px(2.))
                    .left(px(knob_x))
                    .size(px(16.))
                    .rounded(px(8.))
                    .bg(theme::text_primary()),
            );

        div()
            .id("hotkey-repeat-bar")
            .flex()
            .items_center()
            .gap(px(12.))
            .px(px(9.))
            .py(px(3.))
            .rounded(px(theme::RADIUS_ROW))
            .child(
                div()
                    .id("hotkey-repeat-toggle")
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .cursor_pointer()
                    .hover(|style| style.opacity(0.85))
                    .on_click(move |_, window, cx| toggle_on(window, cx))
                    .child(
                        div()
                            .text_size(theme::fs(12.5))
                            .text_color(theme::text_primary())
                            .child(lang.pick("按住连续触发", "Hold to repeat")),
                    )
                    .child(switch),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(
                        div()
                            .text_size(theme::fs(10.5))
                            .text_color(theme::text_tertiary())
                            .child(lang.pick("延迟", "Delay")),
                    )
                    .child(self.number_stepper(
                        "hotkey-repeat-delay",
                        NumberField::HotkeyRepeatDelay,
                        config.hotkey_repeat_delay as i32,
                        lang,
                        cx,
                    )),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(
                        div()
                            .text_size(theme::fs(10.5))
                            .text_color(theme::text_tertiary())
                            .child(lang.pick("间隔", "Every")),
                    )
                    .child(self.number_stepper(
                        "hotkey-repeat-interval",
                        NumberField::HotkeyRepeatInterval,
                        config.hotkey_repeat_interval as i32,
                        lang,
                        cx,
                    )),
            )
            .child(
                div()
                    .id("hotkey-ignore-system-repeat")
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .cursor_pointer()
                    .hover(|style| style.opacity(0.85))
                    .on_click(move |_, window, cx| toggle_ignore(window, cx))
                    .child(
                        div()
                            .text_size(theme::fs(10.5))
                            .text_color(theme::text_tertiary())
                            .child(lang.pick("忽略系统长按重复", "Ignore the system repeat")),
                    )
                    .child(self.mini_switch(config.hotkey_ignore_system_repeat)),
            )
            .into_any_element()
    }

    // -----------------------------------------------------------------------
    // 音量音效
    // -----------------------------------------------------------------------

    /// 音量音效那张卡：总开关 + 音效选择 / 试听 / 自定义文件 / 响度 / 生效范围。
    ///
    /// 思路照 Volume2：每变化一档响一下。默认关 —— 这是个「想要的人才会开」的
    /// 功能，新装就让它出声太主动；打开后三种手势默认全勾（那才是它的卖点）。
    fn sound_rows(&self, config: &Config, lang: Lang, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let sound = &config.volume_sound;
        let master = self.toggle_row(
            "toggle-volume-sound",
            lang.pick(
                "调节音量时播放音效",
                "Play a sound on every volume step",
            ),
            lang.pick(
                "每变化一档响一下，调的时候不用盯着卡片",
                "A short tick for every step, so you can hear where you are",
            ),
            sound.enabled,
            self.bind(|config| config.volume_sound.enabled = !config.volume_sound.enabled),
        );
        if !sound.enabled {
            return vec![master];
        }

        // 音效：分段选择 + 右边的「试听」。选一下当场听一下 ——
        // 这一行的全部意义就是「哪个顺耳」，听完再选才叫选。
        let options: Vec<(u32, String)> = crate::config::VolumeSoundStyle::ALL
            .iter()
            .map(|style| (style.index(), sound_style_label(*style, lang).to_string()))
            .collect();
        let pick_style = {
            let store = self.config.clone();
            move |value: u32, window: &mut Window, cx: &mut App| {
                let _ = store.borrow_mut().update(|config| {
                    config.volume_sound.style = crate::config::VolumeSoundStyle::from_index(value)
                });
                crate::platform::sound::preview(&store.borrow().get().clone());
                refresh_ui(window, cx);
            }
        };
        let segmented = self.segmented("volume-sound-style", options, sound.style.index(), pick_style);
        let preview = {
            let store = self.config.clone();
            self.preview_button(
                "volume-sound-preview",
                lang,
                !sound.custom_missing(),
                move || crate::platform::sound::preview(&store.borrow().get().clone()),
            )
        };
        let style_row = div()
            .flex()
            .items_center()
            .gap(px(10.))
            .px(px(9.))
            .py(px(5.))
            .rounded(px(theme::RADIUS_ROW))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(theme::fs(13.))
                    .text_color(theme::text_primary())
                    .child(lang.pick("音效", "Sound")),
            )
            .child(segmented)
            .child(preview)
            .into_any_element();

        let mut rows: Vec<AnyElement> = vec![style_row];

        // 自定义文件：只在选了「自定义」时冒出来，不然这一行永远是空的。
        if sound.style == crate::config::VolumeSoundStyle::Custom {
            rows.push(self.sound_file_row(sound, lang, cx));
        }

        rows.push(self.number_row(
            "volume-sound-gain",
            lang.pick("音效音量", "Sound volume"),
            lang.pick(
                "音效本身有多响（设备音量照旧由音量决定）",
                "How loud the tick itself is; the device volume still does its own job",
            ),
            NumberField::VolumeSound,
            config,
            lang,
            cx,
        ));

        // 生效范围：三种手势并排，一行放下。
        let scope = |id: &'static str, label: &'static str, value: bool, toggle: fn(&mut Config)| {
            let store = self.config.clone();
            self.tick_chip(
                id,
                label,
                value,
                move |window: &mut Window, cx: &mut App| {
                    let _ = store.borrow_mut().update(toggle);
                    refresh_ui(window, cx);
                },
            )
        };
        let scope_row = div()
            .flex()
            .items_center()
            .gap(px(10.))
            .px(px(9.))
            .py(px(5.))
            .rounded(px(theme::RADIUS_ROW))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .text_size(theme::fs(13.))
                    .text_color(theme::text_primary())
                    .child(lang.pick("生效范围", "Where it plays")),
            )
            .child(scope(
                "volume-sound-hotkey",
                lang.pick("快捷键", "Hotkey"),
                sound.on_hotkey,
                |config| config.volume_sound.on_hotkey = !config.volume_sound.on_hotkey,
            ))
            .child(scope(
                "volume-sound-wheel",
                lang.pick("滚轮", "Wheel"),
                sound.on_wheel,
                |config| config.volume_sound.on_wheel = !config.volume_sound.on_wheel,
            ))
            .child(scope(
                "volume-sound-drag",
                lang.pick("拖拽", "Drag"),
                sound.on_drag,
                |config| config.volume_sound.on_drag = !config.volume_sound.on_drag,
            ))
            .into_any_element();
        rows.push(scope_row);

        rows.push(
            div()
                .px(px(9.))
                .py(px(2.))
                .text_size(theme::fs(10.5))
                .text_color(theme::text_tertiary())
                .child(lang.pick(
                    "到顶 / 到底那几下不响；连续拖动时最快 25 下 / 秒，不会糊成一片。",
                    "Nothing at 0% or 100%. While dragging it fires at most 25 ticks a second.",
                ))
                .into_any_element(),
        );

        vec![master, self.sub_group(rows)]
    }

    /// 「自定义音效」那一行：路径 + 选择…。
    ///
    /// 路径写全（程序目录下的就写相对那一截，便携版挪文件夹也不至于看不懂）；
    /// 文件不在了就标红并说明 —— 这时候试听也不出声，见 `sound::build`。
    fn sound_file_row(
        &self,
        sound: &crate::config::VolumeSoundConfig,
        lang: Lang,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let missing = sound.custom_missing();
        let shown = if sound.custom_path().is_empty() {
            lang.pick("还没选文件，先用「轻点」", "No file yet — using Click")
                .to_string()
        } else {
            crate::app_paths::display_path(sound.custom_path())
        };
        let store = self.config.clone();
        let clear_store = self.config.clone();
        div()
            .flex()
            .items_center()
            .gap(px(10.))
            .px(px(9.))
            .py(px(5.))
            .rounded(px(theme::RADIUS_ROW))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .gap(px(2.))
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(6.))
                            .child(
                                div()
                                    .flex_none()
                                    .text_size(theme::fs(13.))
                                    .text_color(theme::text_primary())
                                    .child(lang.pick("自定义音效文件", "Custom sound file")),
                            )
                            .when(missing, |style| {
                                style.child(
                                    div()
                                        .flex_none()
                                        .text_size(theme::fs(10.5))
                                        .text_color(theme::danger())
                                        .child(lang.pick("找不到文件", "File not found")),
                                )
                            }),
                    )
                    .child(
                        div()
                            .text_size(theme::fs(10.5))
                            .text_color(if missing {
                                theme::danger()
                            } else {
                                theme::text_tertiary()
                            })
                            .truncate()
                            .child(shown),
                    ),
            )
            .child(
                div()
                    .id("volume-sound-pick")
                    .flex_none()
                    .px(px(10.))
                    .py(px(4.))
                    .rounded(px(theme::RADIUS_SMALL))
                    .bg(theme::track())
                    .text_size(theme::fs(11.))
                    .text_color(theme::text_primary())
                    .cursor_pointer()
                    .hover(|style| style.bg(theme::surface_hover()))
                    // 和托盘图标那个「选择…」一样：模态框必须在别的线程上开，
                    // 而且要用 mouse_down 先拦住「点别处」那一层（见 app_pick）。
                    .on_mouse_down(
                        MouseButton::Left,
                        cx.listener(move |_, _: &gpui::MouseDownEvent, window, cx| {
                            cx.stop_propagation();
                            let store = store.clone();
                            let owner = crate::windows::platform::hwnd_of(window)
                                .map(|hwnd| hwnd.0 as isize)
                                .unwrap_or(0);
                            let receiver = crate::platform::app_pick::pick_sound_async(owner);
                            cx.spawn(async move |this, cx| {
                                let Ok(path) = receiver.recv().await else {
                                    return;
                                };
                                let Some(path) = path else {
                                    return;
                                };
                                let _ = store.borrow_mut().update(|config| {
                                    config.volume_sound.custom = path;
                                    config.volume_sound.style =
                                        crate::config::VolumeSoundStyle::Custom;
                                });
                                // 挑完先放一声：不用再点「试听」确认选对了。
                                crate::platform::sound::preview(&store.borrow().get().clone());
                                let _ = this.update_in(cx, |_, window, cx| refresh_ui(window, cx));
                            })
                            .detach();
                        }),
                    )
                    .child(lang.pick("选择…", "Choose…")),
            )
            // 移除：只在真的挑了文件时出现。点掉就回到「轻点」并收起这一行。
            .when(!sound.custom_path().is_empty(), |style| {
                style.child(
                    div()
                        .id("volume-sound-clear")
                        .flex_none()
                        .flex()
                        .items_center()
                        .justify_center()
                        .size(px(22.))
                        .rounded(px(theme::RADIUS_SMALL))
                        .cursor_pointer()
                        .hover(|style| style.bg(theme::surface_hover()))
                        .on_click(cx.listener(move |_, _: &ClickEvent, window, cx| {
                            let _ = clear_store.borrow_mut().update(|config| {
                                config.volume_sound.custom.clear();
                                // 留「自定义」的话这一行只会写着「还没选文件」，
                                // 不如干脆退回去：移除本来就等于「不要这个了」。
                                config.volume_sound.style =
                                    crate::config::VolumeSoundStyle::Click;
                            });
                            refresh_ui(window, cx);
                        }))
                        .child(icon("close", 10.0, theme::text_tertiary())),
                )
            })
            .into_any_element()
    }

    /// 「试听」按钮（音效那一行右边那个小按钮）。
    ///
    /// `enabled = false` 时灰掉、连悬停反馈也没有：自定义文件丢了的那一刻，
    /// 这一下确实没声，按钮就不该还装作能点。
    fn preview_button(
        &self,
        id: &'static str,
        lang: Lang,
        enabled: bool,
        on_click: impl Fn() + 'static,
    ) -> AnyElement {
        div()
            .id(SharedString::from(id))
            .flex_none()
            .px(px(9.))
            .py(px(2.))
            .rounded(px(4.))
            .text_size(theme::fs(11.))
            .text_color(if enabled {
                theme::text_secondary()
            } else {
                theme::text_tertiary()
            })
            .when(enabled, |style| {
                style
                    .cursor_pointer()
                    .hover(|style| style.bg(theme::surface_hover()))
                    .on_click(move |_, _, _| on_click())
            })
            .child(lang.pick("试听", "Test"))
            .into_any_element()
    }

    /// 一行里并排的小勾选（音效的生效范围就是这么排的三个）。
    fn tick_chip(
        &self,
        id: &'static str,
        label: &'static str,
        on: bool,
        on_click: impl Fn(&mut Window, &mut App) + 'static,
    ) -> AnyElement {
        div()
            .id(SharedString::from(id))
            .flex()
            .items_center()
            .gap(px(5.))
            .px(px(4.))
            .py(px(2.))
            .rounded(px(4.))
            .cursor_pointer()
            .hover(|style| style.bg(theme::surface_hover()))
            .on_click(move |_, window, cx| on_click(window, cx))
            .child(
                div()
                    .flex_none()
                    .size(px(14.))
                    .rounded(px(3.5))
                    .border(px(1.))
                    .border_color(if on {
                        theme::accent()
                    } else {
                        theme::border_strong()
                    })
                    .bg(if on {
                        theme::accent()
                    } else {
                        theme::surface_sunken()
                    })
                    .flex()
                    .items_center()
                    .justify_center()
                    .when(on, |style| style.child(icon("check", 11.0, theme::text_primary()))),
            )
            .child(
                div()
                    .text_size(theme::fs(11.5))
                    .text_color(if on {
                        theme::text_primary()
                    } else {
                        theme::text_tertiary()
                    })
                    .child(label),
            )
            .into_any_element()
    }

    /// 黑名单页：禁用 Bolume 的应用程序 + 全屏。
    ///
    /// 版式照 Quicker 的黑名单：一张卡片，上面一排动作（拖到窗口 / 添加程序 / 添加目录），
    /// 下面是一条条卡片式的名单。全屏单独一张：开关 + 它管的例外名单。
    fn blocklist_page(
        &self,
        config: &Config,
        lang: Lang,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let block = &config.app_block;

        let mut rows = vec![
            self.block_actions(BlockList::Block, lang, cx),
            self.block_name_input(BlockList::Block, lang, cx),
        ];
        // 「已经在名单里了」这类临时提示：加在输入框底下，2.6 秒后自己消失。
        if let Some((text, _)) = &self.block_notice {
            rows.push(
                div()
                    .px(px(9.))
                    .py(px(1.))
                    .text_size(theme::fs(10.5))
                    .text_color(theme::danger())
                    .child(text.clone())
                    .into_any_element(),
            );
        }
        rows.push(
            div()
                .px(px(9.))
                .py(px(2.))
                .text_size(theme::fs(10.5))
                .text_color(theme::text_tertiary())
                .child(lang.pick(
                    "这些应用在前面时，快捷键 / 滚轮 / 提示条全部让路，媒体键交还给系统；托盘图标手动点开面板照旧可用。",
                    "While these apps are in front, hotkeys, the wheel and the on-screen card stand aside and media keys go back to the system. The tray icon still opens the panel.",
                ))
                .into_any_element(),
        );
        rows.push(self.block_entry_rows(BlockList::Block, &block.entries, lang, cx));

        let fullscreen = {
            let store = self.config.clone();
            self.toggle_row(
                "block-fullscreen",
                lang.pick("全屏状态禁用", "Disable in fullscreen"),
                lang.pick(
                    "前台是全屏游戏 / 全屏视频时整个 Bolume 让路（打开设置除外）",
                    "Everything stands aside while a fullscreen game or video is in front (opening settings still works)",
                ),
                block.fullscreen,
                move |window: &mut Window, cx: &mut App| {
                    let _ = store.borrow_mut().update(|config| {
                        config.app_block.fullscreen = !config.app_block.fullscreen
                    });
                    sync_blocklist(&store.borrow());
                    refresh_ui(window, cx);
                },
            )
        };

        // 全屏这一张卡：上面一个「全屏状态禁用」，下面的例外名单归它管。
        //
        // 原来是两张卡（开关塞在让路名单末尾、例外另起一张），读起来像两件不相干的
        // 事 —— 其实「这几个例外」说的就是上面那个开关。合并之后开关和它管的名单
        // 圈进同一个框，和快捷键页「按住连续触发」那套版式一致。
        //
        // 例外名单只在开关打开时出现：关着的时候它不生效，摊在那儿只会让人以为
        // 例外名单自己就能让 Bolume 工作。名单数据不动，再打开还在。
        let mut fullscreen_rows = vec![fullscreen];
        if block.fullscreen {
            let mut exempt_rows = vec![
                div()
                    .px(px(9.))
                    .pt(px(2.))
                    .text_size(theme::fs(11.5))
                    .text_color(theme::text_secondary())
                    .child(lang.pick(
                        "以下应用全屏时照常启用",
                        "These apps keep working in fullscreen",
                    ))
                    .into_any_element(),
                self.block_actions(BlockList::Exempt, lang, cx),
                self.block_name_input(BlockList::Exempt, lang, cx),
            ];
            exempt_rows.push(
                div()
                    .px(px(9.))
                    .py(px(2.))
                    .text_size(theme::fs(10.5))
                    .text_color(theme::text_tertiary())
                    .child(lang.pick(
                        "这些应用就算全屏也照常启用 Bolume：「全屏状态禁用」对它们不生效（上面那份让路名单仍然算数）。",
                        "These apps keep Bolume working even in fullscreen — the fullscreen switch above does not apply to them (the block list still does).",
                    ))
                    .into_any_element(),
            );
            exempt_rows.push(self.block_entry_rows(
                BlockList::Exempt,
                &block.fullscreen_exempt,
                lang,
                cx,
            ));
            fullscreen_rows.push(self.sub_group(exempt_rows));
        }

        vec![
            self.section(
                lang.pick(
                    "禁用 Bolume 的应用程序",
                    "Apps where Bolume stays out of the way",
                ),
                "ban",
                rows,
            ),
            self.section(
                lang.pick("全屏", "Fullscreen"),
                "maximize",
                fullscreen_rows,
            ),
            div()
                .px(px(9.))
                .text_size(theme::fs(10.5))
                .text_color(theme::text_tertiary())
                .child(lang.pick(
                    "程序按完整路径匹配；只写 exe 名的按文件名匹配（装在哪都算，游戏把窗口交给子进程画时也能命中）；目录覆盖整个目录（含子目录）。",
                    "Apps match by full path. A bare exe name matches that file anywhere (which also covers games that draw their window from a helper process). A folder covers every program inside it, subfolders included.",
                ))
                .into_any_element(),
        ]
    }

    /// 名单页顶部那排动作：拖到窗口 / 添加… / 添加目录（两块名单各来一套）。
    fn block_actions(&self, list: BlockList, lang: Lang, cx: &mut Context<Self>) -> AnyElement {
        // `block_simple_button` 要的是 `&'static str`，id 只能按两块各写一遍。
        let (file_id, dir_id) = match list {
            BlockList::Block => ("block-add-file", "block-add-dir"),
            BlockList::Exempt => ("block-add-exempt-file", "block-add-exempt-dir"),
        };
        let add_file = self.block_simple_button(
            file_id,
            lang.pick("添加…", "Add…"),
            "apps",
            cx,
            move |this, _window, cx| this.pick_block_path(list, false, cx),
        );
        let add_dir = self.block_simple_button(
            dir_id,
            lang.pick("添加目录", "Add folder"),
            "folder",
            cx,
            move |this, _window, cx| this.pick_block_path(list, true, cx),
        );
        div()
            .flex()
            .items_center()
            .gap(px(6.))
            .px(px(5.))
            .pt(px(3.))
            .child(self.block_pick_button(list, lang, cx))
            .child(add_file)
            .child(add_dir)
            .into_any_element()
    }

    /// 一份名单的条目列表（空的就给一句提示）。
    fn block_entry_rows(
        &self,
        list: BlockList,
        entries: &[AppBlockEntry],
        lang: Lang,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        if entries.is_empty() {
            let text = match list {
                BlockList::Block => lang.pick(
                    "名单还是空的：把第一个按钮拖到目标窗口上，或者用「添加…」挑一个程序",
                    "Nothing listed yet: drag the first button onto the target window, or use “Add…”",
                ),
                BlockList::Exempt => lang.pick(
                    "还没有例外：全屏时 Bolume 一律让路",
                    "No exceptions yet: Bolume stands aside for any fullscreen app",
                ),
            };
            return div()
                .px(px(9.))
                .py(px(8.))
                .text_size(theme::fs(11.))
                .text_color(theme::text_tertiary())
                .child(text)
                .into_any_element();
        }
        let prefix = match list {
            BlockList::Block => "block-del",
            BlockList::Exempt => "block-del-exempt",
        };
        let mut column = div().flex().flex_col().gap(px(3.)).px(px(5.)).pb(px(4.));
        for (index, entry) in entries.iter().enumerate() {
            let (kind_label, kind_icon) = match entry.kind {
                AppBlockKind::Dir => (lang.pick("目录", "Folder"), "folder"),
                AppBlockKind::Name => (lang.pick("程序名", "Name"), "apps"),
                AppBlockKind::App => (lang.pick("程序", "App"), "apps"),
            };
            // 名字条目没有路径可写，第二行改说话：只按文件名匹配，装在哪都算。
            let subtitle = if entry.kind == AppBlockKind::Name {
                lang.pick("任意目录下的同名程序", "Same name in any folder")
                    .to_string()
            } else {
                entry.path.clone()
            };
            let store = self.config.clone();
            column = column.child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .px(px(9.))
                    .py(px(6.))
                    .rounded(px(theme::RADIUS_SMALL))
                    .bg(theme::surface_elevated())
                    .border_1()
                    .border_color(theme::border_strong())
                    .child(icon(kind_icon, 14.0, theme::text_secondary()))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .gap(px(1.))
                            .child(
                                div()
                                    .text_size(theme::fs(12.5))
                                    .text_color(theme::text_primary())
                                    .truncate()
                                    .child(entry.name().to_string()),
                            )
                            .child(
                                div()
                                    .text_size(theme::fs(10.))
                                    .text_color(theme::text_tertiary())
                                    .truncate()
                                    .child(subtitle),
                            ),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(theme::fs(10.))
                            .text_color(theme::text_tertiary())
                            .child(kind_label),
                    )
                    .child(
                        div()
                            .id(SharedString::from(format!("{prefix}-{index}")))
                            .flex_none()
                            .px(px(6.))
                            .py(px(2.))
                            .rounded(px(4.))
                            .cursor_pointer()
                            .text_size(theme::fs(11.))
                            .text_color(theme::text_tertiary())
                            .hover(|style| style.bg(theme::surface_hover()))
                            .on_click(cx.listener(move |_, _: &ClickEvent, window, cx| {
                                let _ = store.borrow_mut().update(|config| match list {
                                    BlockList::Block => config.app_block.remove(index),
                                    BlockList::Exempt => config.app_block.remove_exempt(index),
                                });
                                sync_blocklist(&store.borrow());
                                refresh_ui(window, cx);
                            }))
                            .child("✕"),
                    ),
            );
        }
        column.into_any_element()
    }

    /// 黑名单页第二个入口：手动输入 exe 名（或完整路径），回车添加。
    ///
    /// 拖拽拾取拿到的是「屏幕上那个窗口的进程」—— 有些游戏把界面交给子进程画
    /// （DNF 就是 `CrossProxy.exe` 画窗口），拖上去拾到的就不是主程序。
    /// 那就直接敲名字：只写 `DNF.exe` 时按文件名匹配，装在哪都算。
    fn block_name_input(&self, list: BlockList, lang: Lang, cx: &mut Context<Self>) -> AnyElement {
        let focused = self.block_focused(list);
        let draft = self.block_draft(list).clone().unwrap_or_default();
        let placeholder = match list {
            BlockList::Block => lang.pick(
                "手动输入 exe 名或完整路径，点「添加」加入（例如 DNF.exe）",
                "Type an exe name or a full path, then press Add (e.g. DNF.exe)",
            ),
            BlockList::Exempt => lang.pick(
                "手动输入全屏也想照常用的 exe 名或路径",
                "Type an exe name or path that should keep working in fullscreen",
            ),
        };
        let add_id = match list {
            BlockList::Block => "block-add-name",
            BlockList::Exempt => "block-add-exempt-name",
        };

        // 有草稿就一直显示草稿；光标只在拿着键盘的时候画。
        let text: AnyElement = if focused {
            let canvas = self.text_edit_canvas(cx);
            div()
                .relative()
                .flex_1()
                .min_w(px(0.))
                .flex()
                .items_center()
                .gap(px(1.))
                .text_size(theme::fs(12.))
                .text_color(theme::text_primary())
                .child(draft.clone())
                .child(
                    div()
                        .w(px(1.5))
                        .h(px(12.))
                        .flex_none()
                        .bg(theme::text_primary()),
                )
                .child(canvas)
                .into_any_element()
        } else if !draft.is_empty() {
            div()
                .flex_1()
                .min_w(px(0.))
                .truncate()
                .text_size(theme::fs(12.))
                .text_color(theme::text_primary())
                .child(draft.clone())
                .into_any_element()
        } else {
            div()
                .flex_1()
                .min_w(px(0.))
                .truncate()
                .text_size(theme::fs(12.))
                .text_color(theme::text_tertiary())
                .child(placeholder.to_string())
                .into_any_element()
        };

        let add = self.block_simple_button(
            add_id,
            lang.pick("添加", "Add"),
            "check",
            cx,
            move |this, window, cx| this.commit_block_name(list, window, cx),
        );

        div()
            .flex()
            .items_center()
            .gap(px(6.))
            .px(px(5.))
            .pt(px(3.))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .flex()
                    .items_center()
                    .h(px(27.))
                    .px(px(8.))
                    .rounded(px(theme::RADIUS_SMALL))
                    .bg(theme::surface_sunken())
                    .border_1()
                    .border_color(if focused {
                        theme::accent()
                    } else {
                        theme::border_strong()
                    })
                    .cursor_text()
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(move |this, _: &gpui::MouseDownEvent, window, cx| {
                            // 别让根节点那条「点别处」把刚拿到的焦点又收回去。
                            cx.stop_propagation();
                            // 两块名单不能同时吃键盘：点这块就把那块松开。
                            this.set_block_focused(
                                match list {
                                    BlockList::Block => BlockList::Exempt,
                                    BlockList::Exempt => BlockList::Block,
                                },
                                false,
                            );
                            // 草稿留着：失焦只是交回键盘，点回来接着写。
                            if this.block_draft(list).is_none() {
                                *this.block_draft_mut(list) = Some(String::new());
                            }
                            this.set_block_focused(list, true);
                            // 键盘事件只派发给焦点元素，没焦点敲字没反应。
                            this.focus.focus(window, cx);
                            cx.notify();
                        }),
                    )
                    .child(text),
            )
            .child(add)
            .into_any_element()
    }

    /// 名单页的第一个按钮：按住它，把鼠标拖到目标窗口上松手。
    fn block_pick_button(&self, list: BlockList, lang: Lang, cx: &mut Context<Self>) -> AnyElement {
        let active = self.block_pick == Some(list);
        let label: SharedString = if active {
            lang.pick("松开鼠标完成", "Release to finish").into()
        } else {
            match list {
                BlockList::Block => lang.pick(
                    "拖动到要禁用 Bolume 的窗口上",
                    "Drag onto the app to disable Bolume",
                ),
                BlockList::Exempt => lang.pick(
                    "拖动到全屏也要照常用的窗口上",
                    "Drag onto an app that should keep working",
                ),
            }
            .into()
        };
        let id = match list {
            BlockList::Block => "block-pick-window",
            BlockList::Exempt => "block-pick-exempt",
        };
        let mut button = div()
            .id(id)
            .flex_1()
            .min_w_0()
            .flex()
            .items_center()
            .justify_center()
            .gap(px(6.))
            .px(px(9.))
            .py(px(5.))
            .rounded(px(theme::RADIUS_SMALL))
            .cursor_pointer()
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(move |this, _: &gpui::MouseDownEvent, window, cx| {
                    this.begin_block_pick(list, window, cx)
                }),
            )
            .child(icon("apps", 12.0, theme::text_secondary()))
            .child(
                div()
                    .min_w_0()
                    .truncate()
                    .text_size(theme::fs(11.))
                    .text_color(theme::text_secondary())
                    .child(label),
            );
        button = if active {
            button
                .bg(theme::surface_active())
                .border_1()
                .border_color(theme::accent())
        } else {
            button
                .bg(theme::track())
                .hover(|style| style.bg(theme::surface_hover()))
        };
        button.into_any_element()
    }

    /// 黑名单页的次按钮（添加… / 添加目录）。
    fn block_simple_button(
        &self,
        id: &'static str,
        label: &'static str,
        icon_name: &'static str,
        cx: &mut Context<Self>,
        onclick: impl Fn(&mut Self, &mut Window, &mut Context<Self>) + 'static,
    ) -> AnyElement {
        div()
            .id(SharedString::from(id))
            .flex_none()
            .flex()
            .items_center()
            .gap(px(5.))
            .px(px(9.))
            .py(px(5.))
            .rounded(px(theme::RADIUS_SMALL))
            .bg(theme::track())
            .text_size(theme::fs(11.))
            .text_color(theme::text_secondary())
            .cursor_pointer()
            .hover(|style| style.bg(theme::surface_hover()))
            .on_click(
                cx.listener(move |this, _: &ClickEvent, window, cx| onclick(this, window, cx)),
            )
            .child(icon(icon_name, 12.0, theme::text_secondary()))
            .child(label)
            .into_any_element()
    }

    /// 按下「拖动到要禁用的窗口上」：自己轮询左键，松手时取光标底下的窗口。
    fn begin_block_pick(&mut self, list: BlockList, window: &mut Window, cx: &mut Context<Self>) {
        if self.block_pick.is_some() {
            return;
        }
        self.block_pick = Some(list);
        refresh_ui(window, cx);
        cx.spawn(async move |this, cx| {
            // 先等两拍再看左键：按下和松开挨太近（也就是普通点击）时，
            // 这一轮里读到的已经是「松开了」，接着下面的「光标还在设置窗里」会挡掉。
            let mut waited = 0u32;
            loop {
                cx.background_executor()
                    .timer(std::time::Duration::from_millis(30))
                    .await;
                waited += 30;
                if !crate::platform::app_pick::left_button_down() || waited > 20_000 {
                    break;
                }
            }
            let picked = crate::platform::app_pick::window_under_cursor();
            let _ = this.update_in(cx, |this, window, cx| {
                this.block_pick = None;
                // 指针还压在自己身上就不算数：那是「点了一下按钮」，不是拖到目标窗口。
                if !crate::windows::platform::cursor_in_window(window) {
                    if let Some(picked) = picked {
                        // 同一个窗口拖两遍：说一声「已存在」，别让人以为拖拽失灵。
                        if this.add_block_entry(list, AppBlockKind::App, picked.path)
                            == BlockAdd::Duplicate
                        {
                            this.notify_block_duplicate(cx);
                        }
                    }
                }
                refresh_ui(window, cx);
            });
        })
        .detach();
    }

    /// 「添加…」/「添加目录」：弹出系统选择框，选完加进名单。
    fn pick_block_path(&mut self, list: BlockList, folder: bool, cx: &mut Context<Self>) {
        let kind = if folder {
            AppBlockKind::Dir
        } else {
            AppBlockKind::App
        };
        let receiver = crate::platform::app_pick::pick_path_async(folder);
        cx.spawn(async move |this, cx| {
            // 对话框在别的线程里显示；这里只是等它把结果送回来，界面照常能画。
            let Ok(path) = receiver.recv().await else {
                return;
            };
            let Some(path) = path else {
                return;
            };
            let _ = this.update_in(cx, |this, window, cx| {
                if this.add_block_entry(list, kind, path) == BlockAdd::Duplicate {
                    this.notify_block_duplicate(cx);
                }
                refresh_ui(window, cx);
            });
        })
        .detach();
    }

    /// 往名单里加一条，顺带告诉钩子线程名单变了。
    fn add_block_entry(&mut self, list: BlockList, kind: AppBlockKind, path: String) -> BlockAdd {
        let mut added = false;
        let _ = self.config.borrow_mut().update(|config| match list {
            BlockList::Block => added = config.app_block.add(kind, path),
            BlockList::Exempt => {
                added = config.app_block.add_exempt(AppBlockEntry::new(kind, path));
            }
        });
        sync_blocklist(&self.config.borrow());
        if added {
            BlockAdd::Added
        } else {
            BlockAdd::Duplicate
        }
    }

    /// 屏幕提示（OSD）页：开关、可视化布局、位置、外观、动画、交互。
    fn osd_page(&self, config: &Config, lang: Lang, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let cfg = &config.osd;

        let enable = self.toggle_row(
            "osd-enabled",
            lang.pick("启用屏幕提示", "Enable the on-screen display"),
            lang.pick(
                "调音量、静音、切歌时在屏幕上闪一下",
                "Flash a small card when volume, mute or playback changes",
            ),
            cfg.enabled,
            self.bind(|config| config.osd.enabled = !config.osd.enabled),
        );

        let preview = self.osd_preview(config, lang, cx);

        // 「预览屏幕提示」：右边两个按钮 —— 弹一次看效果，或者钉住不消失。
        // 这两件事本来就是「看一眼」的两种做法，分成两行还各带一句说明，
        // 白占一行高度，所以并成一行。
        let pinned = self.osd_pinned;
        let preview_row = div()
            .flex()
            .items_center()
            .gap(px(10.))
            .px(px(9.))
            .py(px(5.))
            .rounded(px(theme::RADIUS_ROW))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .gap(px(2.))
                    .child(
                        div()
                            .text_size(theme::fs(13.))
                            .text_color(theme::text_primary())
                            .child(lang.pick("预览屏幕提示", "Preview the card")),
                    )
                    .child(
                        div()
                            .text_size(theme::fs(10.5))
                            .text_color(theme::text_tertiary())
                            .child(lang.pick(
                                "弹出一次看看效果；保持显示则不自动消失，便于对照调整外观",
                                "Show it once to check the look, or keep it on screen while you tweak",
                            )),
                    ),
            )
            .child(
                div()
                    .id("osd-try-button")
                    .flex_none()
                    .px(px(9.))
                    .py(px(3.))
                    .rounded(px(theme::RADIUS_SMALL))
                    .bg(theme::track())
                    .cursor_pointer()
                    .text_size(theme::fs(11.))
                    .text_color(theme::text_secondary())
                    .hover(|style| style.bg(theme::surface_hover()))
                    .on_click(cx.listener(|_, _: &ClickEvent, _window, cx| {
                        crate::gui::show_osd(
                            cx,
                            crate::windows::osd::OsdRequest::device(OsdFunction::Volume),
                        );
                    }))
                    .child(lang.pick("弹出一次", "Show once")),
            )
            .child(
                div()
                    .id("osd-pin-button")
                    .flex_none()
                    .px(px(9.))
                    .py(px(3.))
                    .rounded(px(theme::RADIUS_SMALL))
                    .bg(if pinned {
                        theme::accent_soft()
                    } else {
                        theme::track()
                    })
                    .cursor_pointer()
                    .text_size(theme::fs(11.))
                    .text_color(if pinned {
                        theme::text_primary()
                    } else {
                        theme::text_secondary()
                    })
                    .hover(|style| style.bg(theme::surface_hover()))
                    .on_click(cx.listener(|this, _: &ClickEvent, _window, cx| {
                        // 状态在设置页自己手里：按钮文案和实际命令永远一致。
                        let next = !this.osd_pinned;
                        crate::gui::set_osd_pinned(cx, next);
                        this.osd_pinned = next;
                        cx.notify();
                    }))
                    .child(if pinned {
                        lang.pick("收起", "Hide")
                    } else {
                        lang.pick("保持显示", "Keep on screen")
                    }),
            )
            .into_any_element();

        let anchor_options: &'static [(u32, &'static str)] = if lang.is_en() {
            &[
                (0, "Top left"),
                (1, "Top"),
                (2, "Top right"),
                (3, "Bottom left"),
                (4, "Bottom"),
                (5, "Bottom right"),
                (6, "Custom"),
                (7, "Cursor"),
            ]
        } else {
            &[
                (0, "左上"),
                (1, "中上"),
                (2, "右上"),
                (3, "左下"),
                (4, "中下"),
                (5, "右下"),
                (6, "指定坐标"),
                (7, "跟随鼠标"),
            ]
        };
        let position = {
            let store = self.config.clone();
            self.choice_row(
                "osd-anchor",
                lang.pick("位置", "Position"),
                lang.pick(
                    "默认下居中，和 Windows 自带的音量提示同一位置",
                    "Bottom centre by default, the same spot as the Windows volume flyout",
                ),
                anchor_options,
                config.osd.anchor.index(),
                move |value, window, cx| {
                    let anchor = OsdAnchor::from_index(value);
                    let _ = store
                        .borrow_mut()
                        .update(|config| config.osd.anchor = anchor);
                    refresh_ui(window, cx);
                },
            )
        };

        // 指定坐标只在选中「指定坐标」时展开。
        let mut coordinate_rows: Vec<AnyElement> = Vec::new();
        if config.osd.anchor.is_custom() {
            coordinate_rows.push(self.number_row(
                "osd-x",
                lang.pick("水平偏移", "Horizontal offset"),
                lang.pick(
                    "从所选显示器左上角往右多少像素",
                    "Pixels from the left edge of the chosen display",
                ),
                NumberField::OsdX,
                config,
                lang,
                cx,
            ));
            coordinate_rows.push(self.number_row(
                "osd-y",
                lang.pick("垂直偏移", "Vertical offset"),
                lang.pick(
                    "从所选显示器左上角往下多少像素",
                    "Pixels from the top edge of the chosen display",
                ),
                NumberField::OsdY,
                config,
                lang,
                cx,
            ));
        }

        // 显示器：三段固定模式 + 「手动选择」（选中后下面才展开显示器下拉）。
        //
        // 以前是把每台显示器都摊成一个分段按钮，屏一多就横向铺不下；
        // 现在固定四项，具体哪一台走下面那行下拉（`DevicePicker::OsdMonitor`）。
        // 分段按钮上用两个不会和显示器序号撞车的值表示后两个模式。
        const MONITOR_CURSOR: u32 = 1_000;
        const MONITOR_ALL: u32 = 1_001;
        const MONITOR_MANUAL: u32 = 1_002;
        // 指定了某一台（`monitor != 0`）就是「手动选择」，`monitor == 0` 是主显示器。
        let manual = config.osd.monitor_mode == crate::config::OsdMonitorMode::Single
            && config.osd.monitor != 0;
        let monitor = {
            let store = self.config.clone();
            let selected = match config.osd.monitor_mode {
                crate::config::OsdMonitorMode::Single => {
                    if config.osd.monitor == 0 { 0 } else { MONITOR_MANUAL }
                }
                crate::config::OsdMonitorMode::Cursor => MONITOR_CURSOR,
                crate::config::OsdMonitorMode::All => MONITOR_ALL,
            };
            self.choice_row_owned(
                "osd-monitor",
                lang.pick("显示器", "Display"),
                lang.pick(
                    "提示条出现在哪块屏幕上；「手动选择」选中后下面会多一行下拉，全部显示器会每块屏各弹一张",
                    "Which screen the card appears on. \"Pick a display\" opens a dropdown below; All displays shows one card per screen",
                ),
                vec![
                    (0, lang.pick("主显示器", "Primary").to_string()),
                    (
                        MONITOR_MANUAL,
                        lang.pick("手动选择", "Pick a display").to_string(),
                    ),
                    (
                        MONITOR_CURSOR,
                        lang.pick("跟随鼠标", "Follow cursor").to_string(),
                    ),
                    (
                        MONITOR_ALL,
                        lang.pick("全部显示器", "All displays").to_string(),
                    ),
                ],
                selected,
                move |value, window, cx| {
                    let _ = store.borrow_mut().update(|config| match value {
                        MONITOR_CURSOR => {
                            config.osd.monitor_mode = crate::config::OsdMonitorMode::Cursor;
                        }
                        MONITOR_ALL => {
                            config.osd.monitor_mode = crate::config::OsdMonitorMode::All;
                        }
                        MONITOR_MANUAL => {
                            config.osd.monitor_mode = crate::config::OsdMonitorMode::Single;
                            // 还没挑过具体哪台：先落到第一台，下拉才有东西可显示。
                            if config.osd.monitor == 0 {
                                if let Some((first, _)) =
                                    crate::windows::osd::display_options().first()
                                {
                                    config.osd.monitor = *first;
                                }
                            }
                        }
                        _ => {
                            config.osd.monitor_mode = crate::config::OsdMonitorMode::Single;
                            config.osd.monitor = 0;
                        }
                    });
                    refresh_ui(window, cx);
                },
            )
        };

        // 手动选择时才出现：一行下拉，显示当前那台，点开选。
        let monitor_pick = if manual {
            let current = crate::windows::osd::display_options()
                .into_iter()
                .find(|(value, _)| *value == config.osd.monitor)
                .map(|(_, name)| name)
                .unwrap_or_default();
            Some(self.device_picker_row(
                "osd-monitor-pick",
                lang.pick("选择显示器", "Choose a display"),
                lang.pick(
                    "点开挑具体哪一台；选完下拉自动收起",
                    "Click to pick the exact display; the list closes once you choose",
                ),
                &current,
                lang.pick("选择显示器", "Choose a display"),
                DevicePicker::OsdMonitor,
                cx,
            ))
        } else {
            None
        };

        let open_editor = self.action_row(
            "osd-open-editor",
            lang.pick("屏幕提示布局编辑", "On-screen display layout"),
            String::new(),
            lang.pick("编辑", "Edit"),
            |window, cx| {
                crate::gui::open_osd_editor(cx);
                let _ = window;
            },
        );

        // 接管系统媒体键：没配快捷键时也弹提示。
        let takeover = self.toggle_row(
            "osd-takeover-media",
            lang.pick("接管系统媒体键", "Take over media keys"),
            lang.pick(
                "没配快捷键时，按上一曲 / 下一曲 / 播放暂停 / 停止也弹提示，键照常转发给播放器。音量与静音键不接管。",
                "Show the card for prev / next / play-pause / stop even without a shortcut; the key is still forwarded. Volume and mute keys are left alone.",
            ),
            config.osd.takeover_media_keys,
            self.bind(|config| {
                config.osd.takeover_media_keys = !config.osd.takeover_media_keys;
                crate::platform::hotkey::set_takeover_media(config.osd.takeover_media_keys);
            }),
        );

        // 禁用系统 OSD：两半一起 —— 钩子吞音量键，另外把系统那扇 OSD 窗口最小化。
        // 后者才是 Win10 上真正管用的那半：媒体键多半走 HID，钩子根本看不见；
        // 「正在播放」那张带封面的卡片也不是按键触发的。
        let kill_osd = self.toggle_row(
            "osd-block-system",
            lang.pick("禁用 Windows 音量条", "Hide the Windows volume bar"),
            lang.pick(
                "Bolume 运行时不再出现 Windows 自己的音量条，带 Ctrl / Alt / Shift 也一样；上一曲 / 下一曲 / 播放暂停弹出的那张带封面的卡片也一起隐藏。退出 Bolume 后系统恢复原样。",
                "While Bolume is running, the Windows volume bar stays hidden, even with Ctrl / Alt / Shift held — and so does the album-art card that media keys bring up. Closing Bolume restores the default behaviour.",
            ),
            config.osd.block_system_osd,
            self.bind(|config| {
                config.osd.block_system_osd = !config.osd.block_system_osd;
                crate::platform::hotkey::set_block_system_osd(config.osd.block_system_osd);
            }),
        );

        // 六种操作各一行：本来每种都占两行（标题 + 描述），六项就是半页，
        // 描述也大半重复。现在压成一行一条，提示缩成后面那串小字。
        let function_toggles: Vec<AnyElement> = vec![
            self.compact_toggle_row(
                "osd-fn-volume",
                OsdFunction::Volume.label(lang),
                lang.pick("音量键 / 托盘滚轮", "Volume keys / tray scroll"),
                config.osd.volume,
                self.bind(|config| config.osd.volume = !config.osd.volume),
            ),
            self.compact_toggle_row(
                "osd-fn-mute",
                OsdFunction::Mute.label(lang),
                lang.pick("静音键", "Mute key"),
                config.osd.mute,
                self.bind(|config| config.osd.mute = !config.osd.mute),
            ),
            self.compact_toggle_row(
                "osd-fn-brightness",
                OsdFunction::Brightness.label(lang),
                lang.pick("亮度键 / 滚轮", "Brightness keys / wheel"),
                config.osd.brightness,
                self.bind(|config| config.osd.brightness = !config.osd.brightness),
            ),
            self.compact_toggle_row(
                "osd-fn-prev",
                OsdFunction::PrevTrack.label(lang),
                lang.pick("媒体键", "Media key"),
                config.osd.prev_track,
                self.bind(|config| config.osd.prev_track = !config.osd.prev_track),
            ),
            self.compact_toggle_row(
                "osd-fn-play",
                OsdFunction::PlayPause.label(lang),
                lang.pick("媒体键", "Media key"),
                config.osd.play_pause,
                self.bind(|config| config.osd.play_pause = !config.osd.play_pause),
            ),
            self.compact_toggle_row(
                "osd-fn-next",
                OsdFunction::NextTrack.label(lang),
                lang.pick("媒体键", "Media key"),
                config.osd.next_track,
                self.bind(|config| config.osd.next_track = !config.osd.next_track),
            ),
            self.compact_toggle_row(
                "osd-fn-stop",
                OsdFunction::Stop.label(lang),
                lang.pick("媒体键", "Media key"),
                config.osd.stop,
                self.bind(|config| config.osd.stop = !config.osd.stop),
            ),
        ];

        // 标题跟着风格走：选了「跟随系统」之后，这一项管的已经不是「淡入淡出」了。
        let animate_title = if config.osd.anim_style.is_system() {
            lang.pick("弹出动画", "Popup animation")
        } else {
            lang.pick("淡入淡出", "Fade")
        };
        let animate = self.toggle_row(
            "osd-animate",
            animate_title,
            lang.pick(
                "关掉就直接出现、直接消失",
                "Turn off to appear and disappear instantly",
            ),
            config.osd.animate,
            self.bind(|config| config.osd.animate = !config.osd.animate),
        );
        // 动画风格：就地淡入淡出，还是照系统音量条「滑上来 + 淡入」。
        let anim_style = if config.osd.animate {
            let store = self.config.clone();
            Some(self.choice_row_owned(
                "osd-anim-style",
                lang.pick("动画风格", "Animation style"),
                lang.pick(
                    "「跟随系统」和 Windows 自己的音量条一样从下方滑上来，消失时滑回去",
                    "\"Follow system\" slides up from below like the Windows volume bar, and slides back out",
                ),
                vec![
                    (0, lang.pick("淡入淡出", "Fade").to_string()),
                    (1, lang.pick("跟随系统", "Follow system").to_string()),
                ],
                config.osd.anim_style.index(),
                move |value, window, cx| {
                    let _ = store.borrow_mut().update(|config| {
                        config.osd.anim_style = OsdAnimStyle::from_index(value)
                    });
                    refresh_ui(window, cx);
                },
            ))
        } else {
            None
        };
        let mut fade_rows: Vec<AnyElement> = Vec::new();
        if config.osd.animate {
            if let Some(style_row) = anim_style {
                fade_rows.push(style_row);
            }
        }
        // 「跟随系统」的时长由系统那套定，两个数字框这时候不摆出来。
        if config.osd.animate && !config.osd.anim_style.is_system() {
            let fade_in = self.number_row(
                "osd-fade-in",
                lang.pick("淡入时长", "Fade in"),
                "",
                NumberField::OsdFadeIn,
                config,
                lang,
                cx,
            );
            let fade_out = self.number_row(
                "osd-fade-out",
                lang.pick("淡出时长", "Fade out"),
                "",
                NumberField::OsdFadeOut,
                config,
                lang,
                cx,
            );
            fade_rows.push(self.sub_group(vec![fade_in, fade_out]));
        }
        let hold = self.number_row(
            "osd-hold",
            lang.pick("停留时长", "Stay visible"),
            lang.pick(
                "完全显示后停留多久开始淡出",
                "How long the card stays before fading out",
            ),
            NumberField::OsdHold,
            config,
            lang,
            cx,
        );
        let hover_hold = self.toggle_row(
            "osd-hover-hold",
            lang.pick("鼠标悬停时不消失", "Keep open on hover"),
            lang.pick(
                "指针停在提示条上时它会一直留着",
                "The card stays as long as the pointer is on it",
            ),
            config.osd.hover_hold,
            self.bind(|config| config.osd.hover_hold = !config.osd.hover_hold),
        );
        let interactive = self.toggle_row(
            "osd-interactive",
            lang.pick("悬停时可操作", "Interactive on hover"),
            lang.pick(
                "拖动音量条调音量，点喇叭图标切换静音 / 播放暂停",
                "Drag the bar to change volume, click the speaker icon to mute or play/pause",
            ),
            config.osd.interactive,
            self.bind(|config| config.osd.interactive = !config.osd.interactive),
        );
        // 滚轮悬浮调节：整张卡片都认滚轮，不必把指针精确压在细细的音量条上。
        let wheel_hover = self.toggle_row(
            "osd-wheel-hover",
            lang.pick("滚轮悬浮调节", "Scroll to adjust on hover"),
            lang.pick(
                "光标停在提示条上任意位置，滚动滚轮就能调音量。关掉后提示条不响应滚轮（拖音量条、点图标照旧）。",
                "Scroll anywhere on the card to change the volume. Turn it off and the card ignores the wheel; dragging the bar and clicking the icon still work.",
            ),
            config.osd.wheel_hover,
            self.bind(|config| config.osd.wheel_hover = !config.osd.wheel_hover),
        );

        // 应用过滤：黑名单 / 白名单（只管提示条弹不弹；「整个 Bolume 让路」在黑名单页）。
        let filter_options: &'static [(u32, &'static str)] = if lang.is_en() {
            &[(0, "Off"), (1, "Blacklist"), (2, "Whitelist")]
        } else {
            &[(0, "不过滤"), (1, "黑名单"), (2, "白名单")]
        };
        let filter_mode = {
            let store = self.config.clone();
            self.choice_row(
                "osd-filter-mode",
                lang.pick("按应用过滤", "Filter by app"),
                lang.pick(
                    "黑名单：名单里的应用不弹；白名单：只有名单里的应用才弹",
                    "Blacklist hides the card in the listed apps; whitelist shows it only there",
                ),
                filter_options,
                config.osd.filter_mode.index(),
                move |value, window, cx| {
                    let mode = OsdFilterMode::from_index(value);
                    let _ = store
                        .borrow_mut()
                        .update(|config| config.osd.filter_mode = mode);
                    refresh_ui(window, cx);
                },
            )
        };

        let mut filter_rows = vec![filter_mode];
        if config.osd.filter_mode.uses_list() {
            let current_app = crate::platform::fullscreen::foreground_process_name();
            let add_app = current_app.clone();
            filter_rows.push(self.action_row(
                "osd-filter-add",
                lang.pick("添加当前前台应用", "Add the app in front"),
                if add_app.is_empty() {
                    lang.pick(
                        "取不到前台窗口的进程名（先切到那个应用再回来）",
                        "Could not read the front window's process (switch to it and come back)",
                    )
                    .to_string()
                } else {
                    format!(
                        "{} · {add_app}",
                        lang.pick("把前台进程加进名单", "Add the process in front")
                    )
                },
                lang.pick("添加", "Add"),
                self.bind(move |config| {
                    if !add_app.is_empty()
                        && !config
                            .osd
                            .filter_apps
                            .iter()
                            .any(|app| app.eq_ignore_ascii_case(&add_app))
                    {
                        config.osd.filter_apps.push(add_app.clone());
                        config.osd.filter_apps.sort();
                    }
                }),
            ));

            if config.osd.filter_apps.is_empty() {
                filter_rows.push(
                    div()
                        .px(px(9.))
                        .py(px(3.))
                        .text_size(theme::fs(11.))
                        .text_color(theme::text_tertiary())
                        .child(lang.pick("名单还是空的", "The list is empty yet"))
                        .into_any_element(),
                );
            } else {
                for app in &config.osd.filter_apps {
                    let name = app.clone();
                    let shown = name.clone();
                    filter_rows.push(
                        div()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .px(px(9.))
                            .py(px(3.))
                            .rounded(px(theme::RADIUS_ROW))
                            .child(
                                div()
                                    .flex_1()
                                    .min_w_0()
                                    .truncate()
                                    .text_size(theme::fs(12.))
                                    .text_color(theme::text_primary())
                                    .child(shown),
                            )
                            .child(
                                div()
                                    .id(SharedString::from(format!("osd-filter-del-{name}")))
                                    .flex_none()
                                    .px(px(7.))
                                    .py(px(2.))
                                    .rounded(px(4.))
                                    .cursor_pointer()
                                    .text_size(theme::fs(11.))
                                    .text_color(theme::text_tertiary())
                                    .hover(|style| style.bg(theme::surface_hover()))
                                    .on_click(cx.listener(
                                        move |this, _: &ClickEvent, window, cx| {
                                            let name = name.clone();
                                            let _ = this.config.borrow_mut().update(|config| {
                                                config.osd.filter_apps.retain(|app| app != &name);
                                            });
                                            refresh_ui(window, cx);
                                        },
                                    ))
                                    .child("✕"),
                            )
                            .into_any_element(),
                    );
                }
            }
        }

        let mut position_rows = vec![position, monitor];
        if let Some(pick) = monitor_pick {
            position_rows.push(pick);
        }
        position_rows.extend(coordinate_rows);

        vec![
            self.section(
                lang.pick("屏幕提示", "On-screen display"),
                "monitor",
                vec![enable, preview_row],
            ),
            self.section(
                lang.pick("布局与样式", "Layout and style"),
                "palette",
                vec![open_editor, preview],
            ),
            self.section(
                lang.pick("屏幕提示弹出位置", "On-screen display position"),
                "monitor",
                position_rows,
            ),
            self.section(
                lang.pick("显示内容", "Content"),
                "info",
                vec![takeover, kill_osd, self.sub_group(function_toggles)],
            ),
            self.section(
                lang.pick("应用过滤", "App filter"),
                "apps",
                filter_rows,
            ),
            self.section(
                lang.pick("动画", "Animation"),
                "power",
                {
                    let mut rows = vec![animate];
                    rows.extend(fade_rows);
                    rows.push(hold);
                    rows
                },
            ),
            self.section(
                lang.pick("鼠标", "Mouse"),
                "apps",
                vec![hover_hold, interactive, wheel_hover],
            ),
            div()
                .px(px(9.))
                .text_size(theme::fs(10.5))
                .text_color(theme::text_tertiary())
                .child(lang.pick(
                    "提示条不会抢焦点：全屏游戏和视频上照样能弹，游戏也不会被切到后台。\n关掉「静音」这一项时，静音键交回系统处理（会出现 Windows 自带的音量提示）。",
                    "The card never takes focus, so it can appear over fullscreen games without pulling them out of the foreground.\nWith Mute switched off, the mute key goes back to Windows and its own volume flyout appears.",
                ))
                .into_any_element(),
        ]
    }

    /// OSD 的缩略预览：三套模板各一张，点一下打开布局编辑器。
    ///
    /// 只画音量那一套的话，改完媒体键 / 亮度在这里根本看不出差别（而三套本来就各存各的）。
    /// 真正调位置 / 大小还在那扇独立的、可以放大的窗口里做 —— 这里只要
    /// 能一眼看出三套大概长什么样就够了。
    fn osd_preview(&self, config: &Config, lang: Lang, cx: &mut Context<Self>) -> AnyElement {
        use crate::config::OsdTemplate;
        use crate::windows::osd;

        // 顺序与编辑器里的模板按钮一致：音量 / 媒体键 / 亮度。
        // 样例操作各取常见的那一次（音量、播放 / 暂停、亮度）。
        let samples = [
            (OsdTemplate::Volume, OsdFunction::Volume),
            (OsdTemplate::Media, OsdFunction::PlayPause),
            (OsdTemplate::Brightness, OsdFunction::Brightness),
        ];
        let cfgs: Vec<OsdConfig> = samples
            .iter()
            .map(|(_, function)| config.osd.for_function(*function))
            .collect();
        // 尺寸按**转后的包围盒**算：卡片转 45° 之后是它占的地方变大，不是卡片变大。
        let boxes: Vec<(f32, f32)> = cfgs.iter().map(osd::rotated_bounds).collect();
        let pads: Vec<f32> = cfgs.iter().map(osd::shadow_margin).collect();

        // 三张横排放进内容区（大约 520px），按**同一个比例**缩：
        // 三套模板的尺寸大小关系才跟真机一致，不会一张 1:1、一张缩一半。
        const GAP: f32 = 14.0;
        const AVAIL: f32 = 520.0;
        // 缩略图上的影子不用留满：真机上离得远，这里挨得近，留四分之一到一半就够。
        const PAD_MAX: f32 = 12.0;
        let pad_max = pads.iter().copied().fold(0.0f32, f32::max).min(PAD_MAX);
        let total_w: f32 = boxes.iter().map(|(w, _)| *w).sum();
        let scale = ((AVAIL - GAP * (samples.len() - 1) as f32 - pad_max * 2.0 * samples.len() as f32)
            / total_w)
            .min(1.0)
            .max(0.3);

        let mut cells: Vec<AnyElement> = Vec::new();
        for (index, &(template, function)) in samples.iter().enumerate() {
            let cfg = &cfgs[index];
            let (box_w, box_h) = boxes[index];
            let pad = pads[index].min(PAD_MAX);
            let sample = preview_osd_state(lang, cfg, function);
            let content = osd::card_visual(
                cfg,
                &sample,
                scale,
                None,
                &osd::TextMeasure::approx(),
                lang,
            );
            // 背景卡的旋转靠 `card_frame` 套一层（外面按包围盒留位置，本体居中转），
            // 元素自己的旋转在 `card_visual` 里就转了 —— 两处都得走同一条路，
            // 预览才不会比真机少转一层。
            let card = osd::card_frame(cfg, scale, content);
            cells.push(
                div()
                    .id(SharedString::from(format!("osd-preview-{index}")))
                    .flex()
                    .flex_none()
                    .flex_col()
                    .items_center()
                    .gap(px(5.))
                    .cursor_pointer()
                    .child(
                        div()
                            .flex()
                            .flex_none()
                            .items_center()
                            .justify_center()
                            // 转后的包围盒 + 影子该占的一点地方。
                            .w(px(box_w * scale + pad * 2.0))
                            .h(px(box_h * scale + pad * 2.0))
                            .child(card),
                    )
                    .child(
                        div()
                            .text_size(theme::fs(10.5))
                            .text_color(theme::text_tertiary())
                            .child(template_label(template, lang)),
                    )
                    // 点哪一张就进哪一套：先把编辑器指到这套模板，再开窗口。
                    // 不 stop 的话外层的「点预览开编辑器」也会触发一次（重复开）。
                    .on_click(cx.listener(move |this, _: &ClickEvent, _window, cx| {
                        let _ = this
                            .config
                            .borrow_mut()
                            .update(|config| config.osd.set_template(template));
                        crate::gui::open_osd_editor(cx);
                        cx.stop_propagation();
                    }))
                    .into_any_element(),
            );
        }

        div()
            .id("osd-preview")
            .mx(px(5.))
            .my(px(4.))
            .px(px(10.))
            .py(px(8.))
            .flex()
            .items_end()
            .justify_center()
            .gap(px(GAP))
            .rounded(px(theme::RADIUS_SMALL))
            // 预览底色比卡片再暗一档：浅色主题下白卡片贴在浅底上也得有边。
            .bg(theme::surface_sunken())
            .border_1()
            .border_color(theme::border())
            .cursor_pointer()
            .on_click(cx.listener(|_this, _: &ClickEvent, _window, cx| {
                crate::gui::open_osd_editor(cx);
            }))
            .children(cells)
            .into_any_element()
    }

    /// OSD 文字：标题 + 三条额外文字，点一下就地编辑（走输入法那一套）。
    ///
    /// 入口已经搬到布局编辑器（`windows/osd_editor.rs`）里了 —— 那里能一边改一边看到
    /// 卡片上的实际效果。这里保留实现，设置页不再挂出来。
    #[allow(dead_code)]
    fn osd_text_rows(
        &self,
        config: &Config,
        lang: Lang,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let mut rows: Vec<AnyElement> = Vec::new();
        for slot in 0..4u8 {
            let editing = self.osd_text_edit.as_ref().map(|(slot, _)| *slot) == Some(slot);
            let part = if slot == 0 {
                OsdPart::Title
            } else {
                OsdPart::Text(u16::from(slot - 1))
            };
            let current = config.osd.text_for(part).to_string();
            let title: &'static str = match slot {
                0 => lang.pick("标题", "Title"),
                1 => lang.pick("文字 1", "Text 1"),
                2 => lang.pick("文字 2", "Text 2"),
                _ => lang.pick("文字 3", "Text 3"),
            };
            let id = match slot {
                0 => "osd-text-title",
                1 => "osd-text-1",
                2 => "osd-text-2",
                _ => "osd-text-3",
            };
            let placeholder = if slot == 0 {
                lang.pick(
                    "自动（音量 / 已静音 / 下一曲）",
                    "Automatic (Volume / Muted / Next)",
                )
            } else {
                lang.pick("（空，点这里输入）", "(empty — click to type)")
            };

            let value: AnyElement = if editing {
                let buffer = self
                    .osd_text_edit
                    .as_ref()
                    .map(|(_, text)| text.clone())
                    .unwrap_or_default();
                let canvas = self.text_edit_canvas(cx);
                div()
                    .relative()
                    .flex_1()
                    .min_w(px(0.))
                    .flex()
                    .items_center()
                    .gap(px(1.))
                    .text_size(theme::fs(12.))
                    .text_color(theme::text_primary())
                    .child(buffer)
                    .child(
                        div()
                            .w(px(1.5))
                            .h(px(12.))
                            .flex_none()
                            .bg(theme::text_primary()),
                    )
                    .child(canvas)
                    .into_any_element()
            } else {
                let typed = current.clone();
                let empty = current.is_empty();
                div()
                    .id(SharedString::from(id))
                    .flex_1()
                    .min_w(px(0.))
                    .truncate()
                    .text_size(theme::fs(12.))
                    .text_color(if empty {
                        theme::text_tertiary()
                    } else {
                        theme::text_primary()
                    })
                    .cursor_pointer()
                    .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                        this.osd_text_edit = Some((slot, typed.clone()));
                        // 键盘事件只在焦点元素上派发：没焦点的话敲字没反应。
                        this.focus.focus(window, cx);
                        cx.notify();
                    }))
                    .child(if empty {
                        placeholder.to_string()
                    } else {
                        current.clone()
                    })
                    .into_any_element()
            };

            rows.push(
                div()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .px(px(9.))
                    .py(px(4.))
                    .rounded(px(theme::RADIUS_ROW))
                    .child(
                        div()
                            .w(px(44.))
                            .flex_none()
                            .text_size(theme::fs(12.))
                            .text_color(theme::text_secondary())
                            .child(title),
                    )
                    .child(value)
                    .into_any_element(),
            );
        }
        rows
    }

    /// 自选颜色：一行预设色块，点一下立刻换。
    ///
    /// 只做预设不做色轮：这里的色块是按「当底色 / 当强调色都好看」挑过的，
    /// 用户点两下就能得到一套协调的配色，比调 HSV 轮快得多。
    fn color_row(
        &self,
        id: &'static str,
        title: &'static str,
        description: &'static str,
        target: ColorTarget,
        config: &Config,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let current = target.current(config).to_ascii_lowercase();
        let swatches = self.color_swatches(id, target, config, cx);

        div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .px(px(9.))
            .py(px(6.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .gap(px(2.))
                            .child(
                                div()
                                    .text_size(theme::fs(13.))
                                    .text_color(theme::text_primary())
                                    .child(title),
                            )
                            .child(
                                div()
                                    .text_size(theme::fs(10.5))
                                    .text_color(theme::text_tertiary())
                                    .child(description),
                            ),
                    )
                    .child(
                        div()
                            .flex_none()
                            .text_size(theme::fs(10.5))
                            .text_color(theme::text_tertiary())
                            .child(if current.is_empty() {
                                // 强调色空着 = 跟随 Windows 的强调色：把真正生效的那个色号
                                // 显示出来，用户能对上系统设置里的颜色（"—" 看不出跟到了什么）。
                                // 图标颜色空着是「跟随任务栏深浅」，没有色号可给。
                                if target.is_surface() || matches!(target, ColorTarget::Tray) {
                                    "—".to_string()
                                } else {
                                    theme::accent_hex().to_uppercase()
                                }
                            } else {
                                current.to_uppercase()
                            }),
                    ),
            )
            .child(swatches)
            .into_any_element()
    }

    /// 一行色块：「跟随系统」（只有强调色有）+ 预设色 + 自选（色轮）。
    ///
    /// 抽出来是因为「跟随系统」的亮 / 暗两档各有一行底色、一行强调色，
    /// 但它们要的不是整块标题 + 描述的排版，而是两行紧凑的色块。
    fn color_swatches(
        &self,
        id: &str,
        target: ColorTarget,
        config: &Config,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let current = target.current(config).to_ascii_lowercase();
        let store = self.config.clone();

        // 「跟随主题」：半深半浅那一块（`ui::follow_swatch`），和编辑器里同一份 ——
        // 它跟右边那些实色方块长得不一样，不看文字也知道这一格是「跟主题走」。
        let follow_selected = current.is_empty();
        let follow = {
            let store = store.clone();
            crate::ui::follow_swatch(22.0, 6.0, follow_selected)
                .id(SharedString::from(format!("{id}-follow")))
                .cursor_pointer()
                .hover(|style| style.border_color(theme::text_tertiary()))
                .on_click(cx.listener(move |_, _: &ClickEvent, window, cx| {
                    let _ = store
                        .borrow_mut()
                        .update(|config| target.apply(config, String::new()));
                    let cfg = store.borrow().get().clone();
                    crate::gui::apply_theme_extras(&cfg);
                    // 图标颜色改的是任务栏上那张位图，得让托盘单独重画一张；
                    // 主题那几项由 `repaint_theme` 管。
                    refresh_tray_for(target, cx);
                    repaint_theme(window, cx);
                }))
                .into_any_element()
        };

        // 「跟随系统」只留给强调色。底色的「跟随」就是打底模板自己那一档 ——
        // 点掉已选中的色块就能回去，再摆一个空心块只是多占一格。
        let mut swatches: Vec<AnyElement> = Vec::new();
        if !target.is_surface() {
            swatches.push(follow);
        }
        for hex in target.presets() {
            let selected = current == hex.to_ascii_lowercase();
            let color = theme::parse_hex(hex).unwrap_or_else(theme::accent);
            // 选中的那块除描边再加一个对比色圆点：22px 的小方块光靠一圈描边
            // 在深色主题里不够显眼，一眼扫过去分不出选的是哪一块。
            let dot = dot_on(color);
            let value = hex.to_string();
            let store = store.clone();
            swatches.push(
                div()
                    .id(SharedString::from(format!("{id}-{hex}")))
                    .size(px(22.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .rounded(px(6.))
                    .bg(color)
                    .border_1()
                    .border_color(if selected {
                        theme::accent()
                    } else {
                        theme::border()
                    })
                    .cursor_pointer()
                    .hover(|style| style.border_color(theme::text_tertiary()))
                    .when(selected, |style| {
                        style.child(div().size(px(6.)).flex_none().rounded(px(3.)).bg(dot))
                    })
                    .on_click(cx.listener(move |_, _: &ClickEvent, window, cx| {
                        // 再点一次已选中的色块 = 取消（底色回模板、强调色回跟随系统）。
                        let next = if selected {
                            String::new()
                        } else {
                            value.clone()
                        };
                        let _ = store
                            .borrow_mut()
                            .update(|config| target.apply(config, next));
                        let cfg = store.borrow().get().clone();
                        crate::gui::apply_theme_extras(&cfg);
                        refresh_tray_for(target, cx);
                        repaint_theme(window, cx);
                    }))
                    .into_any_element(),
            );
        }

        // 最后一格：平时是个「+」，点开色轮自己挑任意色号。
        //
        // 已经用色轮挑过一个（不在预设里）时，这一格直接显示那个颜色 + 圆点 ——
        // 否则自选色在整行色块里根本看不见，用户会以为「没生效」。
        let wheel_open = self.color_wheel.as_ref().map(|wheel| wheel.target) == Some(target);
        let custom_color = if current.is_empty()
            || target
                .presets()
                .iter()
                .any(|hex| hex.eq_ignore_ascii_case(&current))
        {
            None
        } else {
            theme::parse_hex(&current)
        };
        let custom_dot = custom_color.map(dot_on);
        swatches.push(
            div()
                .id(SharedString::from(format!("{id}-wheel")))
                .size(px(22.))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(6.))
                .bg(custom_color.unwrap_or_else(theme::surface_elevated))
                .border_1()
                .border_color(if wheel_open || custom_color.is_some() {
                    theme::accent()
                } else {
                    theme::border_strong()
                })
                .cursor_pointer()
                .hover(|style| style.border_color(theme::text_tertiary()))
                // 用 mouse_down 而不是 click：根节点那条「点别处」会先把数值输入确认掉，
                // 这里要先拦住，不然色轮刚开就被关。
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        this.toggle_color_wheel(
                            target,
                            event.position.x.as_f32(),
                            event.position.y.as_f32(),
                            window,
                            cx,
                        );
                    }),
                )
                .child(match custom_dot {
                    Some(dot) => div()
                        .size(px(6.))
                        .flex_none()
                        .rounded(px(3.))
                        .bg(dot)
                        .into_any_element(),
                    None => div()
                        .text_size(theme::fs(13.))
                        .text_color(theme::text_secondary())
                        .child("+")
                        .into_any_element(),
                })
                .into_any_element(),
        );

        div()
            .flex()
            .flex_wrap()
            .items_center()
            .gap(px(6.))
            .children(swatches)
            .into_any_element()
    }

    /// 打开 / 收起某一行颜色的取色器。
    fn toggle_color_wheel(
        &mut self,
        target: ColorTarget,
        x: f32,
        y: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        // 同一时刻只留一个输入框。
        self.commit_edit(window, cx);
        self.commit_app_edit(window, cx);
        if self.color_wheel.as_ref().map(|wheel| wheel.target) == Some(target) {
            self.color_wheel = None;
        } else {
            // 空串（跟随）就拿当前生效的那一项当起点，别用写死的默认色。
            let stored = target.current(self.config.borrow().get()).to_string();
            // 这格现在摆的是预设色 → 翻出上次在色轮里调的那一色（点预设不该把
            // 调好的颜色冲掉）；当前就是自选色时以它为准。
            let remembered = crate::ui::picker::resume_value(
                target.presets(),
                &stored,
                self.color_memory.get(&target).map(String::as_str),
            );
            let value = if let Some(hex) = remembered {
                hex
            } else if stored.is_empty() {
                // 空串（跟随）时色轮的起点：底色 / 强调色取当前生效的那一档；
                // 图标颜色没有「当前生效」的色号（跟着任务栏深浅走），从默认亮墨起步。
                match target {
                    ColorTarget::Tray => "#f8f8f8".to_string(),
                    _ if target.is_surface() => theme::surface_hex(),
                    _ => theme::accent_hex(),
                }
            } else {
                stored
            };
            self.color_wheel = Some(ColorWheel::new(target, x, y, &value));
        }
        self.focus.focus(window, cx);
        refresh_ui(window, cx);
    }

    /// 写回自选色（空串 = 跟随），并让所有窗口立刻按新配色重画。
    fn set_custom_color(
        &mut self,
        target: ColorTarget,
        hex: String,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if crate::ui::picker::remembers_value(target.presets(), &hex) {
            self.color_memory.insert(target, hex.clone());
        }
        let _ = self
            .config
            .borrow_mut()
            .update(|config| target.apply(config, hex));
        let cfg = self.config.borrow().get().clone();
        crate::gui::apply_theme_extras(&cfg);
        refresh_tray_for(target, cx);
        // 换了配色是整窗（还有音量面板 / OSD）一起换，只 notify 自己不够。
        repaint_theme(window, cx);
    }

    /// 色轮上拖出新颜色。
    fn apply_wheel_hsv(
        &mut self,
        h: f32,
        s: f32,
        v: f32,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(target) = self.color_wheel.as_ref().map(|wheel| wheel.target) else {
            return;
        };
        let hex = hex_of(hsv_to_rgb(h, s, v));
        if let Some(wheel) = self.color_wheel.as_mut() {
            wheel.picker.set_hsv(h, s, v);
            wheel.picker.hex = hex.clone();
        }
        self.set_custom_color(target, hex, window, cx);
    }

    /// 手输 / 粘贴色号时的按键；返回 true 表示这一键归取色器。
    fn handle_color_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        let Some(mut buffer) = self
            .color_wheel
            .as_ref()
            .and_then(|wheel| wheel.picker.edit.clone())
        else {
            return false;
        };
        // 刚点开输入框：第一下（打字 / 退格 / 粘贴）整串换掉，
        // 不用先退格七次才能输入新的。回车 / Esc 不算「换掉」，照原样提交 / 放弃。
        let fresh = self
            .color_wheel
            .as_ref()
            .map(|wheel| wheel.picker.edit_fresh)
            .unwrap_or(false);
        let key = event.keystroke.key.as_str();
        let typed = key.chars().count() == 1;
        let replaces = fresh && (typed || matches!(key, "backspace" | "v"));
        if replaces {
            buffer.clear();
            if let Some(wheel) = self.color_wheel.as_mut() {
                wheel.picker.edit_fresh = false;
            }
        }
        match key {
            "enter" | "escape" => {
                let apply = key == "enter";
                let target = self.color_wheel.as_ref().map(|wheel| wheel.target);
                if let Some(wheel) = self.color_wheel.as_mut() {
                    wheel.picker.edit = None;
                }
                // 空串 + 回车 = 清掉自选色，回到跟随。
                if apply {
                    if let Some(target) = target {
                        match parse_rgb(&buffer) {
                            Some(rgb) => {
                                let (h, s, v) = rgb_to_hsv(rgb.0, rgb.1, rgb.2);
                                self.apply_wheel_hsv(h, s, v, window, cx);
                                return true;
                            }
                            None if buffer.trim().is_empty() => {
                                self.set_custom_color(target, String::new(), window, cx);
                                return true;
                            }
                            None => {}
                        }
                    }
                }
                refresh_ui(window, cx);
                true
            }
            "backspace" => {
                buffer.pop();
                if let Some(wheel) = self.color_wheel.as_mut() {
                    wheel.picker.edit = Some(buffer);
                }
                refresh_ui(window, cx);
                true
            }
            "v" if event.keystroke.modifiers.control => {
                if let Some(text) = cx
                    .read_from_clipboard()
                    .and_then(|item| item.text().map(|text| text.to_string()))
                {
                    push_hex(&mut buffer, &text);
                    if let Some(wheel) = self.color_wheel.as_mut() {
                        wheel.picker.edit = Some(buffer);
                    }
                    refresh_ui(window, cx);
                }
                true
            }
            _ => {
                if let Some(text) = event.keystroke.key_char.as_ref() {
                    push_hex(&mut buffer, text);
                    if let Some(wheel) = self.color_wheel.as_mut() {
                        wheel.picker.edit = Some(buffer);
                    }
                }
                refresh_ui(window, cx);
                true
            }
        }
    }

    /// 屏幕取色：把设置窗藏起来 → 等一次左键 → 采样 → 露回来。
    fn start_color_pick(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(raw) = crate::windows::platform::raw_hwnd(window) else {
            return;
        };
        let Some(target) = self.color_wheel.as_ref().map(|wheel| wheel.target) else {
            return;
        };
        if self
            .color_wheel
            .as_ref()
            .map(|wheel| wheel.picking)
            .unwrap_or(false)
        {
            return;
        }
        if let Some(wheel) = self.color_wheel.as_mut() {
            wheel.picking = true;
            wheel.picker.edit = None;
        }
        crate::windows::platform::show_raw(raw, false);
        cx.notify();

        let (tx, rx) = std::sync::mpsc::channel::<Option<String>>();
        std::thread::spawn(move || {
            // 先等按在按钮上的那一发松开，否则会立刻当成「取色点击」。
            while crate::windows::platform::mouse_left_down() {
                std::thread::sleep(COLOR_PICK_TICK);
            }
            let mut picked = None;
            // 最多等 20 秒：用户按了取色又改主意时，窗口必须自己回来。
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
            while std::time::Instant::now() < deadline {
                if crate::windows::platform::escape_down() {
                    break;
                }
                if crate::windows::platform::mouse_left_down() {
                    let (x, y) = crate::windows::platform::cursor_screen();
                    picked = crate::windows::platform::screen_color(x, y);
                    break;
                }
                std::thread::sleep(COLOR_PICK_TICK);
            }
            // 无论如何先把窗口放回来，不能让用户面对一扇消失的设置窗。
            crate::windows::platform::show_raw(raw, true);
            let hex =
                picked.map(|value| hex_of(((value >> 16) as u8, (value >> 8) as u8, value as u8)));
            let _ = tx.send(hex);
        });

        cx.spawn(async move |this, cx| {
            let mut result = None;
            for _ in 0..1500 {
                match rx.try_recv() {
                    Ok(value) => {
                        result = value;
                        break;
                    }
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
                    Err(std::sync::mpsc::TryRecvError::Empty) => {
                        cx.background_executor().timer(COLOR_PICK_TICK).await;
                    }
                }
            }
            let _ = this.update(cx, |this, cx| {
                let rgb = result.as_ref().and_then(|text| parse_rgb(text));
                if let Some(wheel) = this.color_wheel.as_mut() {
                    wheel.picking = false;
                    if let Some(rgb) = rgb {
                        wheel.picker.set_hex(hex_of(rgb));
                    }
                }
                if let Some(rgb) = rgb {
                    // 这里拿不到 Window，所以不走 `set_custom_color`：
                    // 自己写配置 + 叠主题色的那两步，最后 notify 一次就够。
                    let hex = hex_of(rgb);
                    let _ = this
                        .config
                        .borrow_mut()
                        .update(|config| target.apply(config, hex));
                    let cfg = this.config.borrow().get().clone();
                    crate::gui::apply_theme_extras(&cfg);
                    cx.refresh_windows();
                }
                cx.notify();
            });
        })
        .detach();
    }

    /// 主题：一排三个小图标（各自用那套配色画出来），点一下即时换肤。
    ///
    /// 原来是「一行一个名字」，三套主题就吃掉三行高度；改成并排三格之后
    /// 一屏能多看见下面两项，而且不用先读名字就知道哪个是黑的、哪个是白的。
    ///
    /// 打开「跟随系统亮暗」后那一排收起来，换成两行下拉：亮色用哪套、深色用哪套 ——
    /// 一排三格代表的是「固定用这一套」，和「跟着系统切」是互斥的两种模式。
    fn theme_rows(&self, config: &Config, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let lang = config.language;
        let mut rows = Vec::new();

        // 外观模式：固定一套 / 跟着 Windows 的亮暗自动切。
        let mode = {
            let store = self.config.clone();
            let options = vec![
                (0_u32, lang.pick("固定主题", "Fixed theme").to_string()),
                (1_u32, lang.pick("跟随系统", "Follow Windows").to_string()),
            ];
            let description = if !config.theme_follow_system {
                lang.pick(
                    "固定用深色底；切成「跟随系统」后按 Windows 的亮暗换下面两套",
                    "Always dark; switch to Follow Windows to use the two sets below",
                )
            } else if crate::platform::system_theme::is_dark() {
                lang.pick(
                    "Windows 现在是深色，用下面「深色主题」那一套",
                    "Windows is dark right now; the dark set below is in use",
                )
            } else {
                lang.pick(
                    "Windows 现在是亮色，用下面「亮色主题」那一套",
                    "Windows is light right now; the light set below is in use",
                )
            };
            self.choice_row_owned(
                "theme-follow-system",
                lang.pick("外观模式", "Appearance mode"),
                description,
                options,
                u32::from(config.theme_follow_system),
                move |picked, window, cx| {
                    let follow = picked == 1;
                    if store.borrow().get().theme_follow_system == follow {
                        return;
                    }
                    let _ = store
                        .borrow_mut()
                        .update(|config| config.theme_follow_system = follow);
                    // 先套主题、再叠用户自选的底色 / 强调色（`apply_theme_extras`
                    // 里就是这个顺序），否则一换模式自定义色就没了。
                    crate::gui::apply_theme_extras(&store.borrow().get().clone());
                    repaint_theme(window, cx);
                },
            )
        };
        rows.push(mode);

        if config.theme_follow_system {
            rows.push(section_row(
                lang.pick("亮色主题", "Light theme"),
                lang.pick(
                    "Windows 切到亮色时用这一套",
                    "Used when Windows switches to its light mode",
                ),
            ));
            rows.push(self.slot_colors_block(
                "theme-light-color",
                ThemeSlot::Light,
                lang,
                config,
                cx,
            ));
            rows.push(section_row(
                lang.pick("深色主题", "Dark theme"),
                lang.pick(
                    "Windows 切到深色时用这一套",
                    "Used when Windows switches to its dark mode",
                ),
            ));
            rows.push(self.slot_colors_block(
                "theme-dark-color",
                ThemeSlot::Dark,
                lang,
                config,
                cx,
            ));
        }

        rows
    }

    /// 「跟随系统」某一档自己的自选配色：两行色块（背景色 / 强调色）。
    ///
    /// 每档各存一套（见 `Config::theme_light_surface`）：共用一份自选色的话，
    /// 系统在亮暗之间切来切去界面一模一样，跟随就没意义了。
    /// 底色点掉已选中的那格 = 回到模板自己的底色；强调色第一格 = 跟随系统强调色。
    fn slot_colors_block(
        &self,
        id: &str,
        slot: ThemeSlot,
        lang: Lang,
        config: &Config,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let (surface, accent) = match slot {
            ThemeSlot::Light => (ColorTarget::LightSurface, ColorTarget::LightAccent),
            ThemeSlot::Dark => (ColorTarget::DarkSurface, ColorTarget::DarkAccent),
        };
        // 左边一列固定宽度的标签，两行的色块起止位置才对得上。
        let line = |label: &'static str, swatches: AnyElement| {
            div()
                .flex()
                .items_center()
                .gap(px(10.))
                .child(
                    div()
                        .flex_none()
                        .w(px(46.))
                        .text_size(theme::fs(11.))
                        .text_color(theme::text_secondary())
                        .child(label),
                )
                .child(swatches)
                .into_any_element()
        };
        let surface_line = line(
            lang.pick("背景色", "Background"),
            self.color_swatches(&format!("{id}-surface"), surface, config, cx),
        );
        let accent_line = line(
            lang.pick("强调色", "Accent"),
            self.color_swatches(&format!("{id}-accent"), accent, config, cx),
        );
        div()
            .flex()
            .flex_col()
            .gap(px(6.))
            .px(px(9.))
            .pt(px(2.))
            .pb(px(8.))
            .child(surface_line)
            .child(accent_line)
            .into_any_element()
    }

    /// 页脚编辑器要改配置时走它 —— 写配置、让开着的面板立刻重画、设置页重绘。
    ///
    /// 返回两个回调：`edit` 收编辑器抛出来的改动；`refresh` 只重画不碰配置
    /// （拖动时跟着光标走的落点竖条、悬停显形的那个 × 都靠它）。
    #[allow(clippy::type_complexity)]
    fn footer_edit(
        &self,
        cx: &mut Context<Self>,
    ) -> (
        Rc<dyn Fn(FooterEdit, &mut Window, &mut App)>,
        Rc<dyn Fn(&mut Window, &mut App)>,
    ) {
        // 窗口级监听是纯闭包，拿不到实体，只能给弱引用：设置窗关掉以后
        // 别把它吊着不放。
        let weak = cx.entity().downgrade();
        let refresh: Rc<dyn Fn(&mut Window, &mut App)> = {
            let weak = weak.clone();
            Rc::new(move |window: &mut Window, cx: &mut App| {
                if let Some(this) = weak.upgrade() {
                    let _ = this.update(cx, |_, cx| cx.notify());
                }
                // × 是整块出现 / 消失的，光 notify 不够。
                window.refresh();
            })
        };
        let store = self.config.clone();
        let after = refresh.clone();
        let edit: Rc<dyn Fn(FooterEdit, &mut Window, &mut App)> =
            Rc::new(move |edit, window, cx| {
                let _ = store.borrow_mut().update(|config| match edit {
                    FooterEdit::Hide(id) => config.footer.set_visible(&id, false),
                    FooterEdit::Place { id, side, before } => {
                        config.footer.place(&id, side, before.as_deref(), true);
                    }
                    // 放回来 = 回到它原来待的位置。竖排面板不画左边那一组，
                    // 从「未放置」里点回来就是搬到右边去。
                    FooterEdit::Restore(id) => {
                        let on_left = config
                            .footer
                            .items
                            .iter()
                            .find(|item| item.id == id)
                            .is_some_and(|item| item.side.is_left());
                        if config.panel_layout.is_vertical() && on_left {
                            config.footer.place(&id, FooterSide::Right, None, true);
                        } else {
                            config.footer.set_visible(&id, true);
                        }
                    }
                });
                // 面板开着就立刻重画；关着的话下次弹出自然就是新的。
                crate::gui::refresh_flyout_footer(cx);
                after(window, cx);
                refresh_ui(window, cx);
            });
        (edit, refresh)
    }

    /// 「页脚按钮」这一整块：面板底边的 1:1 预览 + 没放上去的那些。
    fn footer_section(&self, config: &Config, lang: Lang, cx: &mut Context<Self>) -> AnyElement {
        let (edit, refresh) = self.footer_edit(cx);
        let mut rows = vec![crate::ui::footer_preview::footer_preview(
            config,
            &self.footer_preview,
            lang,
            edit.clone(),
            refresh,
        )];
        if let Some(tray) = crate::ui::footer_preview::unplaced_tray(config, lang, edit) {
            rows.push(tray);
        }
        rows.push(self.note_row(lang.pick(
            "上面那条就是面板底边的实际大小：拖按钮换位置（拖到空着的另一半就是搬到那一组），点右上角的 × 收起来，收起来的在「未放置」里点一下就能放回去。竖排面板太窄，只画右侧一组、而且只画图标",
            "The strip above is the panel footer at actual size: drag a button to move it (drop it on the empty half to switch sides), click the × to take it off; anything taken off waits under “Not shown” and one click puts it back. Vertical panels are too narrow and only show the right group, icons only",
        )));
        self.section(lang.pick("页脚按钮", "Footer buttons"), "pin", rows)
    }

    /// 外观 / 面板 / 托盘三页共用的一次性构建：一页的行交错写在一起（共用一份
    /// `config` 与 `cx`），拆开要搬几百行，所以照旧一起建、返回给三页各取一组。
    fn appearance_groups(
        &self,
        config: &Config,
        lang: Lang,
        snapshot: &Snapshot,
        cx: &mut Context<Self>,
    ) -> (Vec<AnyElement>, Vec<AnyElement>, Vec<AnyElement>) {
        let mut palette_rows = self.theme_rows(config, cx);
        // 跟随系统时，自选色是「每档各一套」，已经跟着上面两档槽位一起摆出来了；
        // 这两行全局色只在「固定主题」下才有意义（跟随时它们不参与配色）。
        if !config.theme_follow_system {
            palette_rows.push(self.color_row(
                "theme-surface",
                lang.pick("背景色", "Background"),
                lang.pick(
                    "选一个底色，文字和描边会自动换成对比色；再点一次已选的色块就回到默认底色",
                    "Pick a base colour; text and borders flip to stay readable. Click the selected swatch again to go back to the default",
                ),
                ColorTarget::Surface,
                config,
                cx,
            ));
            palette_rows.push(self.color_row(
                "theme-accent",
                lang.pick("强调色", "Accent"),
                lang.pick(
                    "选中态、滑条、应用角标用的颜色；第一个色块 = 跟随 Windows 的强调色",
                    "Used for selections, sliders and the per-app route badge; the first swatch follows the Windows accent",
                ),
                ColorTarget::Accent,
                config,
                cx,
            ));
        }

        let font_scale = self.number_row(
            "ui-font-scale",
            lang.pick("设置窗字号", "Settings font size"),
            lang.pick(
                "整个设置窗口的文字缩放，100% 是原始大小",
                "Scales every text in this window; 100% is the original size",
            ),
            NumberField::UiFontScale,
            config,
            lang,
            cx,
        );
        let ui_opacity = self.number_row(
            "ui-opacity",
            lang.pick("界面不透明度", "Interface opacity"),
            lang.pick(
                "设置窗和音量面板的通透程度，100% 完全不透明",
                "How solid the settings window and volume panel are; 100% is fully opaque",
            ),
            NumberField::UiOpacity,
            config,
            lang,
            cx,
        );

        // 托盘数字的字体族：一个下拉按钮，点开才是列表。
        // 数字是按托盘尺寸原生光栅化的，不再画大图缩小，所以换字体立刻生效。
        let number_fonts = self.font_picker_row(config, lang, FontTarget::TrayNumber, cx);
        // 整个设置窗的字体：和「设置窗字号」摆在一起。
        let ui_font = self.font_picker_row(config, lang, FontTarget::Ui, cx);

        // 界面字重：四档（细 / 常规 / 半粗 / 粗），只影响整窗文字。
        let ui_weight = {
            let store = self.config.clone();
            let weight_options = Self::weight_options(lang);
            self.choice_row(
                "ui-font-weight",
                lang.pick("界面字重", "Interface weight"),
                lang.pick(
                    "设置窗口里文字的笔画粗细（只对指定了「界面字体」时更明显）",
                    "Stroke weight of the texts in this window",
                ),
                weight_options,
                config.ui_font_weight,
                move |value, window, cx| {
                    let _ = store
                        .borrow_mut()
                        .update(|config| FontTarget::Ui.set_weight(config, value));
                    crate::gui::apply_theme_extras(&store.borrow().get().clone());
                    cx.refresh_windows();
                    refresh_ui(window, cx);
                },
            )
        };

        // 对比度：只拉次级文字 / 描边 / 悬停层的浓度，底色和强调色不动。
        let ui_contrast = self.number_row(
            "ui-contrast",
            lang.pick("界面对比度", "Interface contrast"),
            lang.pick(
                "拉开次级文字和描边的明暗差，100% 是主题原样",
                "Spacing between secondary text and borders; 100% is the theme as designed",
            ),
            NumberField::UiContrast,
            config,
            lang,
            cx,
        );

        // 抗锯齿方式：照 Photoshop 字符面板底部那七档。
        let antialias = {
            let store = self.config.clone();
            let options: &'static [(u32, &'static str)] = if lang.is_en() {
                &[
                    (0, "None"),
                    (1, "Sharp"),
                    (2, "Crisp"),
                    (3, "Strong"),
                    (4, "Smooth"),
                    (5, "Windows LCD"),
                    (6, "Windows"),
                ]
            } else {
                &[
                    (0, "无"),
                    (1, "锐利"),
                    (2, "犀利"),
                    (3, "浑厚"),
                    (4, "平滑"),
                    (5, "Windows LCD"),
                    (6, "Windows"),
                ]
            };
            self.choice_row(
                "tray-number-antialias",
                lang.pick("数字抗锯齿", "Digit antialiasing"),
                lang.pick(
                    "和 Photoshop 字符面板那七档一致；「无」是硬边，小尺寸下最清楚",
                    "The same seven levels as the Photoshop character panel; None is hard-edged",
                ),
                options,
                config.tray_number_antialias.index(),
                move |value, window, cx| {
                    let style = crate::config::TrayAntialias::from_index(value);
                    let _ = store
                        .borrow_mut()
                        .update(|config| config.tray_number_antialias = style);
                    crate::gui::refresh_tray_icon(cx);
                    refresh_ui(window, cx);
                },
            )
        };

        // 三档位数各一套字号 / 宽度 / 高度 / 字距：一位数只有「5」的时候可以放大，
        // 三位数「100」需要更小的字号才不会挤成一团 —— 分开存正是为这个。
        let digit_one = self.digit_metrics_group(
            1,
            lang.pick("一位数（0–9）", "One digit (0–9)"),
            config,
            lang,
            cx,
        );
        let digit_two = self.digit_metrics_group(
            2,
            lang.pick("两位数（10–99）", "Two digits (10–99)"),
            config,
            lang,
            cx,
        );
        let digit_three = self.digit_metrics_group(
            3,
            lang.pick("三位数（100）", "Three digits (100)"),
            config,
            lang,
            cx,
        );

        // 托盘图标样式：喇叭 / 数字 / 软件图标，默认喇叭。
        let icon_options: &'static [(u32, &'static str)] = if lang.is_en() {
            &[
                (0, "Speaker"),
                (1, "Number"),
                (2, "App icon"),
                (3, "Dynamic"),
                (4, "Custom .ico"),
            ]
        } else {
            &[
                (0, "喇叭"),
                (1, "数字"),
                (2, "软件图标"),
                (3, "动态"),
                (4, "自定义图标"),
            ]
        };
        let icon_style = {
            let store = self.config.clone();
            self.choice_row(
                "tray-icon-style",
                lang.pick("托盘图标", "Tray icon"),
                lang.pick(
                    "选「数字」才会展开字体设置；「动态」是跟着声音律动的图标",
                    "Number reveals font options; Dynamic animates with the sound",
                ),
                icon_options,
                match config.tray_icon_style {
                    TrayIconStyle::Arcs => 0,
                    TrayIconStyle::Number => 1,
                    TrayIconStyle::App => 2,
                    TrayIconStyle::Dynamic => 3,
                    TrayIconStyle::Custom => 4,
                },
                move |value, window, cx| {
                    let style = match value {
                        1 => TrayIconStyle::Number,
                        2 => TrayIconStyle::App,
                        3 => TrayIconStyle::Dynamic,
                        4 => TrayIconStyle::Custom,
                        _ => TrayIconStyle::Arcs,
                    };
                    let _ = store
                        .borrow_mut()
                        .update(|config| config.tray_icon_style = style);
                    // 动态模式要顺手把音频线程提频 / 降频。
                    crate::gui::sync_tray_motion(cx);
                    crate::gui::refresh_tray_icon(cx);
                    refresh_ui(window, cx);
                },
            )
        };

        // 图标颜色：单色墨迹（喇叭 / 数字 / 动态图案）换个色就是换墨色。
        // 软件图标和自定义 ico 是彩色原图，染色会毁掉它 —— 选它们时这一行不出现。
        let icon_color = config.tray_icon_style.is_monochrome().then(|| {
            self.color_row(
                "tray-icon-color",
                lang.pick("图标颜色", "Icon color"),
                lang.pick(
                    "留空跟随任务栏深浅；喇叭、数字、动态三种样式都会换色",
                    "Empty follows the taskbar shade; applies to the speaker, number and motion icons",
                ),
                ColorTarget::Tray,
                config,
                cx,
            )
        });

        // 自定义图标：选「自定义图标」才出现，选完立刻换掉托盘上那张图。
        let custom_icon = matches!(config.tray_icon_style, TrayIconStyle::Custom).then(|| {
            let store = self.config.clone();
            let path = config.tray_icon_custom.trim().to_string();
            let label = if path.is_empty() {
                lang.pick("还没选文件", "No file chosen").to_string()
            } else {
                std::path::Path::new(&path)
                    .file_name()
                    .map(|name| name.to_string_lossy().to_string())
                    .unwrap_or_else(|| path.clone())
            };
            div()
                .flex()
                .items_center()
                .gap(px(10.))
                .px(px(9.))
                .py(px(6.))
                .child(
                    div()
                        .flex()
                        .flex_col()
                        .flex_1()
                        .min_w_0()
                        .gap(px(2.))
                        .child(
                            div()
                                .text_size(theme::fs(13.))
                                .text_color(theme::text_primary())
                                .child(lang.pick("图标文件", "Icon file")),
                        )
                        .child(
                            div()
                                .text_size(theme::fs(10.5))
                                .text_color(theme::text_tertiary())
                                .child(label),
                        ),
                )
                .child(
                    div()
                        .id("tray-icon-custom-pick")
                        .flex_none()
                        .px(px(10.))
                        .py(px(4.))
                        .rounded(px(theme::RADIUS_SMALL))
                        .bg(theme::track())
                        .text_size(theme::fs(11.))
                        .text_color(theme::text_primary())
                        .cursor_pointer()
                        .hover(|style| style.bg(theme::surface_hover()))
                        // 用 mouse_down：根节点那条「点别处」会先把别的编辑提交掉，
                        // 这里要先拦住，不然对话框刚开就被关。
                        //
                        // 对话框必须在**别的线程**上开：模态框会在调用线程里跑消息
                        // 循环，把消息灌进 gpui 的 WndProc，而这里状态还借着 ——
                        // 见 `app_pick::pick_icon_async`。
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(move |_, _: &gpui::MouseDownEvent, window, cx| {
                                cx.stop_propagation();
                                let store = store.clone();
                                // 认主：对话框锁在设置窗前面（模态），不会被叠出第二个。
                                let owner = crate::windows::platform::hwnd_of(window)
                                    .map(|hwnd| hwnd.0 as isize)
                                    .unwrap_or(0);
                                let receiver = crate::platform::app_pick::pick_icon_async(owner);
                                cx.spawn(async move |this, cx| {
                                    let Ok(path) = receiver.recv().await else {
                                        return;
                                    };
                                    let Some(path) = path else {
                                        return;
                                    };
                                    let _ = store
                                        .borrow_mut()
                                        .update(|config| config.tray_icon_custom = path);
                                    let _ = this.update_in(cx, |_, window, cx| {
                                        crate::gui::refresh_tray_icon(cx);
                                        refresh_ui(window, cx);
                                    });
                                })
                                .detach();
                            }),
                        )
                        .child(lang.pick("选择…", "Choose…")),
                )
                .into_any_element()
        });

        // 律动效果 + 灵敏度：只在「动态」图标下才有意义。
        let motion_effect = {
            let store = self.config.clone();
            let options: &'static [(u32, &'static str)] = if lang.is_en() {
                &[(0, "Bars"), (1, "Ripple"), (2, "Orbit"), (3, "Pulse")]
            } else {
                &[(0, "律动条"), (1, "涟漪"), (2, "光环"), (3, "呼吸")]
            };
            self.choice_row(
                "tray-motion-effect",
                lang.pick("律动效果", "Motion effect"),
                lang.pick(
                    "四种都只吃峰值电平：律动条看电平表、涟漪随鼓点扩散、光环匀速转、呼吸跟着胀缩",
                    "All four read the peak level: Bars is a level meter, Ripple spreads on beats, Orbit rotates, Pulse breathes",
                ),
                options,
                config.tray_motion.index(),
                move |value, window, cx| {
                    let effect = TrayMotion::from_index(value);
                    let _ = store
                        .borrow_mut()
                        .update(|config| config.tray_motion = effect);
                    crate::gui::sync_tray_motion(cx);
                    refresh_ui(window, cx);
                },
            )
        };

        let motion_gain = self.number_row(
            "tray-motion-gain",
            lang.pick("律动灵敏度", "Motion sensitivity"),
            lang.pick(
                "把峰值放大多少倍；安静的歌也能动起来就往上调",
                "How much the peak level is amplified; raise it for quiet tracks",
            ),
            NumberField::TrayMotionGain,
            config,
            lang,
            cx,
        );

        // 彩色律动只在单色墨迹上画得出来：软件图标 / 自定义 ico 是彩色原图，
        // 染上去只会把原样毁掉，所以这两种样式下整行都不出现。
        let icon_color_by_level = config.tray_icon_style.is_monochrome().then(|| {
            let store = self.config.clone();
            self.toggle_row(
                "tray-icon-color-level",
                lang.pick("彩色律动", "Colour pulse"),
                lang.pick(
                    "颜色跟着响度走：安静偏蓝，越响越往红走",
                    "Hue follows the level: cool when quiet, red when loud",
                ),
                config.tray_color_by_level,
                move |window, cx| {
                    let _ = store
                        .borrow_mut()
                        .update(|config| config.tray_color_by_level = !config.tray_color_by_level);
                    crate::gui::sync_tray_motion(cx);
                    crate::gui::refresh_tray_icon(cx);
                    refresh_ui(window, cx);
                },
            )
        });

        // 音量条上的发声特效：画在填充之上，满音量也看得见谁在响。
        let activity_effect = {
            let store = self.config.clone();
            let options: &'static [(u32, &'static str)] = if lang.is_en() {
                &[
                    (0, "Off"),
                    (1, "Level"),
                    (2, "Shimmer"),
                    (3, "Pulse"),
                    (4, "Gradient"),
                    (5, "Drift"),
                ]
            } else {
                &[
                    (0, "关闭"),
                    (1, "电平"),
                    (2, "流光"),
                    (3, "呼吸"),
                    (4, "渐变"),
                    (5, "浮光"),
                ]
            };
            self.choice_row(
                "activity-effect",
                lang.pick("发声特效", "Activity effect"),
                lang.pick(
                    "叠在音量条上方，满音量也能看出谁在发声",
                    "Drawn on top of the volume bar, visible even at full volume",
                ),
                options,
                config.activity_effect.index(),
                move |value, window, cx| {
                    let effect = ActivityEffect::from_index(value);
                    let _ = store
                        .borrow_mut()
                        .update(|config| config.activity_effect = effect);
                    refresh_ui(window, cx);
                },
            )
        };

        // 设备名字号：原来是 11/12/13/14 四档，改成数字框 —— 点数字直接手输，
        // 其他值（比如 10 或 15）也能填。
        let device_name_size = self.number_row(
            "device-name-size",
            lang.pick("设备名字号", "Device name size"),
            lang.pick(
                "括号里的型号会自动换到第二行",
                "The part in brackets wraps to a second line",
            ),
            NumberField::DeviceNameSize,
            config,
            lang,
            cx,
        );

        // 面板宽度：所有行都跟着它走；亮度面板默认也取这个宽度。
        let panel_width = self.number_row(
            "flyout-width",
            lang.pick("面板宽度", "Panel width"),
            lang.pick(
                "音量面板有多宽（像素）；亮度面板默认跟随这个宽度",
                "How wide the volume panel is, in pixels; the brightness panel matches it by default",
            ),
            NumberField::FlyoutWidth,
            config,
            lang,
            cx,
        );

        // 滑条的粗细：面板里所有音量条 / 亮度条一起变。
        let bar_thickness = self.number_row(
            "panel-bar-thickness",
            lang.pick("滑条粗细", "Bar thickness"),
            lang.pick(
                "面板里音量条与亮度条有多粗（像素）；屏幕提示条（OSD）的条高在布局编辑器里调",
                "How thick the volume and brightness bars are, in pixels. The on-screen display's bar size lives in the layout editor",
            ),
            NumberField::PanelBarThickness,
            config,
            lang,
            cx,
        );

        // 滑条的圆角：按「条高的一半」算百分比，跟 OSD 的条圆角一个口径。
        let bar_round = self.number_row(
            "panel-bar-round",
            lang.pick("滑条圆角", "Bar corner"),
            lang.pick(
                "按条高的一半算：100% = 全圆头（胶囊形），0% = 方角",
                "Measured against half the bar height: 100% is a full pill, 0% is square",
            ),
            NumberField::PanelBarRound,
            config,
            lang,
            cx,
        );

        // 排版：横排（一路一行）/ 竖排（一路一条立起来的推子）。
        let layout_options: &'static [(u32, &'static str)] = if lang.is_en() {
            &[(0, "Horizontal"), (1, "Vertical")]
        } else {
            &[(0, "横向"), (1, "竖向")]
        };
        let layout_row = {
            let store = self.config.clone();
            self.choice_row(
                "flyout-layout",
                lang.pick("面板排版", "Panel layout"),
                lang.pick(
                    "横向是一路一行；竖向是一路一条立起来的推子（像调音台），面板宽度由路数决定",
                    "Horizontal gives each app its own row; vertical gives each one an upright fader like a mixing desk, and the width follows the number of channels",
                ),
                layout_options,
                config.panel_layout.index(),
                move |value, window, cx| {
                    let layout = PanelLayout::from_index(value);
                    let _ = store
                        .borrow_mut()
                        .update(|config| config.panel_layout = layout);
                    crate::gui::refresh_flyout_size(cx);
                    refresh_ui(window, cx);
                },
            )
        };

        // 竖向排版的面板高度：0 = 自动（按内容算），写数字推子就跟着长。
        let vertical_height = self.number_row(
            "flyout-vertical-height",
            lang.pick("竖向高度", "Vertical height"),
            lang.pick(
                "竖向排版时面板有多高（像素），多出来的都给推子；0 = 自动按内容算",
                "How tall the panel is in vertical layout, in pixels; the extra space goes to the faders. 0 = fit the content",
            ),
            NumberField::FlyoutVerticalHeight,
            config,
            lang,
            cx,
        );

        // 竖向排版下要不要写应用名：默认不写，列只有四十来像素宽。
        let vertical_names = {
            let store = self.config.clone();
            self.toggle_row(
                "flyout-vertical-names",
                lang.pick("竖向时显示应用名", "Show app names in vertical layout"),
                lang.pick(
                    "关闭时竖向只显示图标和推子；打开会在读数下面加一行小字",
                    "Off keeps only the icon and the fader; on adds a small name line under the readout",
                ),
                config.vertical_app_names,
                move |window, cx| {
                    let _ = store.borrow_mut().update(|config| {
                        config.vertical_app_names = !config.vertical_app_names
                    });
                    crate::gui::refresh_flyout_size(cx);
                    refresh_ui(window, cx);
                },
            )
        };

        let animation = {
            let store = self.config.clone();
            self.toggle_row(
                "popup-animation",
                lang.pick("弹出动画", "Popup animation"),
                lang.pick(
                    "从任务栏后面滑出来，或原地淡入淡出",
                    "Slide out from behind the taskbar, or cross-fade in place",
                ),
                config.popup_animation,
                move |window, cx| {
                    let _ = store
                        .borrow_mut()
                        .update(|config| config.popup_animation = !config.popup_animation);
                    refresh_ui(window, cx);
                },
            )
        };

        // 应用图标：从 exe / .ico 里按这一档取，也按这一档画。
        let app_icon = {
            let store = self.config.clone();
            let engine = self.engine.clone();
            self.choice_row(
                "app-icon-size",
                lang.pick("应用图标大小", "App icon size"),
                lang.pick(
                    "按这一档从图标资源里取图，也按这个尺寸显示",
                    "Extract the icon at this size and draw it at the same size",
                ),
                &[(16, "16px"), (32, "32px"), (64, "64px")],
                config.app_icon_size,
                move |value, window, cx| {
                    let _ = store
                        .borrow_mut()
                        .update(|config| config.app_icon_size = value);
                    // 音频线程按新尺寸重新提取一遍图标。
                    engine.send(AudioCommand::SetIconSize(value));
                    refresh_ui(window, cx);
                },
            )
        };

        // 应用名首字母大写：进程名就是 exe 文件名，一水儿小写（chrome / cloudmusic），
        // 列在面板里像命令行。默认开着，认不出是哪个应用时可以直接关掉看原名。
        let app_name_capitalize = {
            let store = self.config.clone();
            let engine = self.engine.clone();
            self.toggle_row(
                "app-name-capitalize",
                lang.pick("应用名首字母大写", "Capitalize app names"),
                lang.pick(
                    "进程名是文件名（chrome.exe → chrome），打开后第一个字母大写",
                    "Process names come from the file name (chrome.exe → chrome); on, the first letter is upper-cased",
                ),
                config.app_name_capitalize,
                move |window, cx| {
                    let next = !store.borrow().get().app_name_capitalize;
                    let _ = store
                        .borrow_mut()
                        .update(|config| config.app_name_capitalize = next);
                    // 音频线程按新规则重新解析名字。
                    engine.send(AudioCommand::SetNameCapitalize(next));
                    refresh_ui(window, cx);
                },
            )
        };

        // 弹出方式：照系统音量面板整块滑进来，还是原地淡入淡出。
        let animation_style = {
            let store = self.config.clone();
            self.choice_row_owned(
                "popup-anim-style",
                lang.pick("弹出方式", "Popup style"),
                lang.pick(
                    "「跟随系统」和 Windows 自己的音量面板一样整块从任务栏后面滑进来；「淡入淡出」是原地淡出淡入",
                    "\"Follow system\" slides the whole panel in from behind the taskbar, \"Fade\" just cross-fades in place",
                ),
                vec![
                    (0, lang.pick("跟随系统", "Follow system").to_string()),
                    (1, lang.pick("淡入淡出", "Fade").to_string()),
                ],
                config.popup_anim_style.index(),
                move |value, window, cx| {
                    let _ = store.borrow_mut().update(|config| {
                        config.popup_anim_style = PopupAnimStyle::from_index(value)
                    });
                    refresh_ui(window, cx);
                },
            )
        };

        // 收起动画：关掉就瞬间消失（默认开）。
        let close_animation = {
            let store = self.config.clone();
            self.toggle_row(
                "panel-close-animation",
                lang.pick("收起动画", "Close animation"),
                lang.pick(
                    "关掉的话面板一眨眼就没了",
                    "Turn off to make the panel disappear instantly",
                ),
                config.panel_close_animation,
                move |window, cx| {
                    let _ = store.borrow_mut().update(|config| {
                        config.panel_close_animation = !config.panel_close_animation
                    });
                    refresh_ui(window, cx);
                },
            )
        };

        // 弹出速度：三档通用的数字框，毫秒随便填。
        // 「跟随系统」也吃这一项 —— 缩放的是系统那条曲线的时间常数，形状不变。
        let animation_speed = self.number_row(
            "popup-animation-ms",
            lang.pick("弹出速度（毫秒）", "Popup speed (ms)"),
            lang.pick(
                "从按下到完全展开的时长，数值越小越快；「跟随系统」按同一比例缩放系统节奏",
                "Time from the hotkey to fully open; smaller is faster. With \"Follow system\" it scales the system timing",
            ),
            NumberField::PopupAnimation,
            config,
            lang,
            cx,
        );

        // 界面动效：切换设置页面（左栏分页、页内页签）时内容淡入 + 轻微上浮。
        let ui_motion = self.toggle_row(
            "toggle-ui-motion",
            lang.pick("界面动效", "Interface motion"),
            lang.pick(
                "切换设置页面时的淡入动效；关掉后新页面直接出现",
                "Fade-in when switching settings pages; off makes the new page appear right away",
            ),
            config.ui_motion,
            self.bind(|config| config.ui_motion = !config.ui_motion),
        );

        // 面板自己的字体 / 字号 / 字距 / 行高 / 字重：和设置窗那几项分开存，两个窗口互不影响。
        let panel_font = self.font_picker_row(config, lang, FontTarget::Panel, cx);
        let panel_font_size = self.number_row(
            "panel-font-size",
            lang.pick("面板字号", "Panel font size"),
            lang.pick(
                "面板里所有文字的字号（像素）；只改字号，面板宽高不动 —— 挤不下就调行高或宽度，11.5 是原始大小",
                "Font size of every text in the panel (px). Only the text grows — the panel keeps its size; raise row height or width if it does not fit. 11.5 is the original",
            ),
            NumberField::PanelFontSize,
            config,
            lang,
            cx,
        );
        let panel_letter_spacing = self.number_row(
            "panel-letter-spacing",
            lang.pick("面板字间距", "Panel letter spacing"),
            lang.pick(
                "每个字之间多留（或少留）多少像素，负数收紧；填的是实际像素，不跟字号放大",
                "Extra pixels between characters (negative tightens); absolute, does not follow the font size",
            ),
            NumberField::PanelLetterSpacing,
            config,
            lang,
            cx,
        );
        let panel_line_height = self.number_row(
            "panel-line-height",
            lang.pick("面板行高", "Panel row height"),
            lang.pick(
                "面板里每一行的高度（像素）；0 = 跟着字号自动算，填了就以它为准",
                "Height of each row in the panel (px); 0 lets it follow the font size",
            ),
            NumberField::PanelLineHeight,
            config,
            lang,
            cx,
        );
        let panel_font_weight = {
            let store = self.config.clone();
            let weight_options = Self::weight_options(lang);
            self.choice_row(
                "panel-font-weight",
                lang.pick("面板字重", "Panel weight"),
                lang.pick(
                    "面板里文字的笔画粗细（只对指定了「面板字体」时更明显）",
                    "Stroke weight of the texts in the panel",
                ),
                weight_options,
                config.panel_font_weight,
                move |value, window, cx| {
                    let _ = store
                        .borrow_mut()
                        .update(|config| FontTarget::Panel.set_weight(config, value));
                    crate::gui::refresh_flyout_size(cx);
                    refresh_ui(window, cx);
                },
            )
        };

        // 面板紧跟在「界面」后面：它也是「整个面板长什么样」这一类，
        // 中间隔着一整串托盘设置的话，找宽度 / 特效要往下翻很久。
        let panel_section = self.section(lang.pick("面板", "Panel"), "monitor", {
            // 宽度 / 行高只对横排有意义，竖向高度 / 应用名只对竖排有意义 —— 只摆那几条能用的。
            let mut rows = vec![layout_row];
            if config.panel_layout.is_vertical() {
                rows.push(vertical_height);
                rows.push(vertical_names);
            } else {
                rows.push(panel_width);
                // 行高挨着宽度：都是「这一块占多大」。竖排是一列一列摆的，没有行高这回事。
                rows.push(panel_line_height);
            }
            // 条的粗细 / 圆角两种排版都有（横排是横条、竖排是立起来的推子），一律摆上。
            rows.push(bar_thickness);
            rows.push(bar_round);
            // 字体那几项紧跟布局：改字号时宽度 / 高度会自动跟着变，摆在一起好对照。
            rows.push(panel_font);
            rows.push(panel_font_size);
            rows.push(panel_letter_spacing);
            rows.push(panel_font_weight);
            rows.push(activity_effect);
            rows.push(device_name_size);
            rows.push(app_icon);
            rows.push(app_name_capitalize);
            rows.push(animation);
            if config.popup_animation {
                rows.push(animation_style);
                rows.push(close_animation);
                // 三档都能调速度：「跟随系统」缩放的是系统曲线的时间常数。
                rows.push(animation_speed);
            }
            rows
        });

        // 页脚那一排按钮单独一张卡：放哪些、摆哪一边、谁在前谁在后。
        let footer_section = self.footer_section(config, lang, cx);

        // 三个页签三组内容：界面 / 面板 / 托盘。
        let interface_sections = vec![
            self.section(lang.pick("主题", "Theme"), "palette", palette_rows),
            self.section(
                lang.pick("界面", "Interface"),
                "settings",
                vec![
                    font_scale,
                    ui_font,
                    ui_weight,
                    ui_contrast,
                    ui_opacity,
                    ui_motion,
                ],
            ),
        ];
        let panel_sections = vec![panel_section, footer_section];
        let mut tray_rows = vec![icon_style];
        if let Some(row) = custom_icon {
            tray_rows.push(row);
        }
        if let Some(color) = icon_color {
            tray_rows.push(color);
        }
        if let Some(row) = icon_color_by_level {
            tray_rows.push(row);
        }
        let mut sections = vec![self.section(
            lang.pick("托盘图标", "Tray icon"),
            "speaker",
            tray_rows,
        )];
        // 字体 / 抗锯齿 / 三档字号宽高只对「数字」图标有意义：选喇叭时整段收起来，
        // 免得摆一堆改了也看不出变化的选项。
        if matches!(config.tray_icon_style, TrayIconStyle::Number) {
            sections.push(self.section(
                lang.pick("托盘数字", "Tray digits"),
                "speaker",
                vec![
                    number_fonts,
                    antialias,
                    digit_one,
                    digit_two,
                    digit_three,
                ],
            ));
        }
        // 律动效果 / 灵敏度只对「动态」图标有意义。
        if matches!(config.tray_icon_style, TrayIconStyle::Dynamic) {
            sections.push(self.section(
                lang.pick("托盘律动", "Tray motion"),
                "speaker",
                vec![motion_effect, motion_gain],
            ));
        }
        // 菜单外观：跟随主题 = 自绘菜单，原生 = Windows 系统菜单。
        // 宽度那一项只对自绘有意义（原生菜单的宽度由系统按文字算），摆在一起说明清楚。
        let tray_menu_style = {
            let store = self.config.clone();
            let options = vec![
                (0_u32, lang.pick("跟随主题", "Themed").to_string()),
                (
                    1_u32,
                    lang.pick("Windows 原生", "Windows native").to_string(),
                ),
            ];
            let value = match config.tray_menu_style {
                crate::config::TrayMenuStyle::Theme => 0,
                crate::config::TrayMenuStyle::Native => 1,
            };
            self.choice_row_owned(
                "tray-menu-style",
                lang.pick("菜单外观", "Menu look"),
                lang.pick(
                    "跟随主题是自己画的菜单：底色 / 字号 / 强调色都跟界面一致",
                    "Themed draws the menu itself so it follows the app colours",
                ),
                options,
                value,
                move |picked, window, cx| {
                    let _ = store.borrow_mut().update(|config| {
                        config.tray_menu_style = if picked == 0 {
                            crate::config::TrayMenuStyle::Theme
                        } else {
                            crate::config::TrayMenuStyle::Native
                        };
                    });
                    // 换回原生要把系统菜单挂回去，换到主题要把它摘掉。
                    crate::gui::refresh_tray_menu(cx);
                    refresh_ui(window, cx);
                },
            )
        };

        // 菜单最大宽度：超出的设备名用 … 收尾，免得一行横穿半个屏幕。
        let tray_menu_width = self.number_row(
            "tray-menu-width",
            lang.pick("菜单最大宽度", "Menu max width"),
            lang.pick(
                "名字太长就用 … 截断；只对「跟随主题」的菜单有效",
                "Long names get truncated with …; only for the themed menu",
            ),
            NumberField::TrayMenuWidth,
            config,
            lang,
            cx,
        );

        // 托盘右键菜单的布局：条目多，编辑器放进弹层，页面上只留一行入口。
        sections.push(self.section(
            lang.pick("托盘右键菜单", "Tray context menu"),
            "settings",
            vec![
                tray_menu_style,
                self.tray_menu_entry_row(config, lang, cx),
                tray_menu_width,
            ],
        ));

        // 悬浮提示：内容完全由用户写，每行一个模板。
        sections.push(self.section(
            lang.pick("托盘图标悬浮提示", "Tray icon tooltip"),
            "info",
            self.tooltip_rows(config, lang, snapshot, cx),
        ));

        // 这一页原本是「界面 / 面板 / 托盘」三个页签；后两组已经从外观里拆成
        // 独立大类，这里只把三组一起交出去，由三个页各取所需（见下面三个薄函数）。
        (interface_sections, panel_sections, sections)
    }

    /// 外观页：只留「界面」那一组（主题 + 界面）。面板、托盘各自是独立大类了。
    fn appearance_page(
        &self,
        config: &Config,
        lang: Lang,
        snapshot: &Snapshot,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        self.appearance_groups(config, lang, snapshot, cx).0
    }

    /// 面板页：面板怎么自动收起、长什么样、页脚放什么、列表里放什么。
    ///
    /// 「失去焦点时收起面板」和「应用列表」那两个开关原在常规页 —— 它们说的是
    /// 这块面板自己的行为，跟着面板走更顺。
    fn panel_page(
        &self,
        config: &Config,
        lang: Lang,
        snapshot: &Snapshot,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        let (_, panel_sections, _) = self.appearance_groups(config, lang, snapshot, cx);

        let hide_focus = self.toggle_row(
            "toggle-focus",
            lang.pick("失去焦点时收起面板", "Close panel when focus is lost"),
            lang.pick(
                "点击面板以外区域时自动隐藏",
                "Hide automatically when you click outside the panel",
            ),
            config.hide_on_focus_lost,
            self.bind(|config| config.hide_on_focus_lost = !config.hide_on_focus_lost),
        );

        let system = self.toggle_row(
            "toggle-system",
            lang.pick("显示系统声音", "Show system sounds"),
            lang.pick(
                "在列表中保留 Windows 的系统提示音会话",
                "Keep the Windows system sounds session in the list",
            ),
            config.show_system_sounds,
            self.bind(|config| config.show_system_sounds = !config.show_system_sounds),
        );

        // 正在发声的应用浮到最前面：只影响面板里的排列，不动音量、不动设备。
        let sounding_first = {
            let store = self.config.clone();
            self.toggle_row(
                "toggle-sounding-first",
                lang.pick("正在播放的应用排最前", "Playing apps first"),
                lang.pick(
                    "打开面板时，正在发声的应用移至列表顶部，之后不再回落",
                    "When the panel opens, apps that are playing move to the top of the list and stay there",
                ),
                config.sounding_apps_first,
                move |window, cx| {
                    let _ = store.borrow_mut().update(|config| {
                        config.sounding_apps_first = !config.sounding_apps_first
                    });
                    // 面板要是正开着，立刻按新规则重排一次。
                    cx.refresh_windows();
                    refresh_ui(window, cx);
                },
            )
        };

        let mut out = vec![self.section(
            lang.pick("行为", "Behaviour"),
            "settings",
            vec![hide_focus],
        )];
        out.extend(panel_sections);
        out.push(self.section(
            lang.pick("应用列表", "App list"),
            "apps",
            vec![system, sounding_first],
        ));
        out
    }

    /// 托盘页：图标长什么样、右键菜单、悬浮提示。
    fn tray_page(
        &self,
        config: &Config,
        lang: Lang,
        snapshot: &Snapshot,
        cx: &mut Context<Self>,
    ) -> Vec<AnyElement> {
        self.appearance_groups(config, lang, snapshot, cx).2
    }

    /// 字体弹层里当前选中的那一行在第几行（0 = 「默认」行，1.. = 字体族）。
    ///
    /// 打开弹层时用它决定滚到哪儿：两三百个字体铺开，不滚过去等于每次都要自己找。
    fn font_index_of(config: &Config, target: FontTarget) -> usize {
        let current = target.current(config);
        if current.trim().is_empty() {
            return 0;
        }
        crate::platform::fonts::families()
            .iter()
            .position(|family| family.eq_ignore_ascii_case(&current))
            .map(|index| index + 1)
            .unwrap_or(0)
    }

    /// 搜索框筛出来的字体（不区分大小写，纯包含匹配）。
    fn filtered_fonts(&self) -> Vec<String> {
        let needle = self.font_filter.trim().to_lowercase();
        crate::platform::fonts::families()
            .into_iter()
            .filter(|family| needle.is_empty() || family.to_lowercase().contains(&needle))
            .collect()
    }

    /// 搜索词变了：列表内容整个换了，滚动位置回到顶部 ——
    /// 不重置的话会停在新列表的下半截（甚至一片空白），看着像「没结果」。
    fn reset_font_scroll(&mut self) {
        self.font_pending_scroll = None;
        self.font_scroll.set_offset(gpui::point(px(0.), px(0.)));
    }

    /// 收起字体弹层，草稿（搜索词）一起丢掉。
    fn close_font_menu(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.font_menu = None;
        self.font_pending_scroll = None;
        self.font_filter.clear();
        self.text_marked = None;
        refresh_ui(window, cx);
    }

    /// 选中一个字体：点列表行和搜索框里回车都走这里。
    fn pick_font(
        &mut self,
        family: String,
        target: FontTarget,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let store = self.config.clone();
        let current = target.current(&store.borrow().get());
        if current != family {
            let value = family.clone();
            let _ = store
                .borrow_mut()
                .update(|config| target.set(config, value));
            match target {
                FontTarget::TrayNumber => crate::gui::refresh_tray_icon(cx),
                // 界面字体是全局的：先把新字体装进主题层，再让整窗重画。
                FontTarget::Ui => {
                    let cfg = store.borrow().get().clone();
                    crate::gui::apply_theme_extras(&cfg);
                    cx.refresh_windows();
                }
                // 面板自己读配置、挂在自己根节点上：重排一次尺寸 + 整窗重画就换过来了。
                FontTarget::Panel => crate::gui::refresh_flyout_size(cx),
            }
        }
        self.close_font_menu(window, cx);
    }

    /// 字体弹层搜索框的按键：打字筛、退格删、回车选第一条、Esc 收起。
    ///
    /// 弹层开着时键盘归它 —— 和页面上那些就地改名框是互斥的（同一时刻只会开一个）。
    fn handle_font_filter_key(
        &mut self,
        event: &KeyDownEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        if self.font_menu.is_none() {
            return false;
        }
        // 输入法正在拼字：这些键都该交给输入法（提交的字走 EntityInputHandler）。
        if self.text_marked.is_some() {
            return false;
        }
        match event.keystroke.key.as_str() {
            "escape" => {
                cx.stop_propagation();
                self.close_font_menu(window, cx);
                return true;
            }
            "enter" => {
                cx.stop_propagation();
                let target = self.font_menu.map(|(target, _, _)| target);
                if let (Some(target), Some(family)) =
                    (target, self.filtered_fonts().into_iter().next())
                {
                    // 搜索框空着时第一条就是「默认（系统字体）」那一行。
                    let family = if self.font_filter.trim().is_empty() {
                        String::new()
                    } else {
                        family
                    };
                    self.pick_font(family, target, window, cx);
                }
                return true;
            }
            "backspace" => {
                cx.stop_propagation();
                self.push_text_undo();
                self.font_filter.pop();
                self.reset_font_scroll();
                cx.notify();
                return true;
            }
            _ => {}
        }
        if event.keystroke.key.as_str() == "v" && event.keystroke.modifiers.control {
            cx.stop_propagation();
            // 粘一整段字体名（中文名字体用输入法，英文名常常是复制来的）。
            let text = cx
                .read_from_clipboard()
                .and_then(|item| item.text().map(|text| text.to_string()));
            if let Some(text) = text {
                self.insert_rename_text(&text);
                self.reset_font_scroll();
                cx.notify();
            }
            return true;
        }
        if event.keystroke.modifiers.control || event.keystroke.modifiers.platform {
            return false;
        }
        if let Some(text) = event.keystroke.key_char.as_ref() {
            cx.stop_propagation();
            // 字体名都不长，24 个字足够筛了。
            if self.font_filter.chars().count() < 24 {
                self.font_filter.push_str(text);
            }
            self.reset_font_scroll();
            cx.notify();
            return true;
        }
        false
    }

    /// 这个字体族是不是图标字体（Segoe MDL2 Assets / Fluent Icons / Wingdings 这些）。
    ///
    /// 它们没有正常的拉丁字形，用字体本身画名字会整行变成方框 —— 这些行改成用
    /// 界面字体画名字，只留左边的「88」当预览（那排方框本身就是它的样子）。
    fn is_symbol_font(family: &str) -> bool {
        let lower = family.to_lowercase();
        [
            "mdl2",
            "fluent icons",
            "symbol",
            "webding",
            "wingding",
            "marlett",
        ]
        .iter()
        .any(|needle| lower.contains(needle))
    }

    /// 字体行：行尾一个下拉按钮，点开才是字体列表。
    ///
    /// 系统字体族动辄两三百个，全部铺在页面里会把后面几项挤到得滚半天；
    /// 改成弹出层：固定高度、自己的滚动条，选中就收。
    fn font_picker_row(
        &self,
        config: &Config,
        lang: Lang,
        target: FontTarget,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let family = target.current(config);
        let shown = SharedString::from(if family.trim().is_empty() {
            lang.pick("默认（系统字体）", "Default (system)")
                .to_string()
        } else {
            family.clone()
        });
        let open = matches!(self.font_menu, Some((which, _, _)) if which == target);

        div()
            .flex()
            .items_center()
            .gap(px(10.))
            .px(px(9.))
            .py(px(5.))
            .rounded(px(theme::RADIUS_ROW))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .gap(px(2.))
                    .child(
                        div()
                            .text_size(theme::fs(12.5))
                            .text_color(theme::text_primary())
                            .child(target.title(lang)),
                    )
                    .child(
                        div()
                            .text_size(theme::fs(10.5))
                            .text_color(theme::text_tertiary())
                            .truncate()
                            .child(target.description(lang)),
                    ),
            )
            .child(
                div()
                    .id(SharedString::from(match target {
                        FontTarget::TrayNumber => "tray-font-picker",
                        FontTarget::Ui => "ui-font-picker",
                        FontTarget::Panel => "panel-font-picker",
                    }))
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .flex_none()
                    .pl(px(9.))
                    .pr(px(7.))
                    .py(px(4.))
                    .rounded(px(theme::RADIUS_SMALL))
                    .bg(theme::track())
                    .cursor_pointer()
                    .hover(|style| style.bg(theme::surface_hover()))
                    .on_mouse_down(
                        gpui::MouseButton::Left,
                        cx.listener(move |this, event: &gpui::MouseDownEvent, window, cx| {
                            // 根节点那条「点别处」不该把刚打开的面板又关掉。
                            cx.stop_propagation();
                            if this.font_menu.is_some() {
                                this.font_menu = None;
                                this.font_pending_scroll = None;
                                this.font_filter.clear();
                            } else {
                                // 每次打开都是空白搜索 + 从顶部重新量一遍。
                                this.font_scroll = gpui::ScrollHandle::new();
                                this.font_filter.clear();
                                this.text_marked = None;
                                // 打开就滚到当前选中的那个字体。第一帧还量不到行边界，
                                // 记个下标交给下一帧的 overlay 去滚。
                                this.font_pending_scroll = Some(Self::font_index_of(
                                    &this.config.borrow().get().clone(),
                                    target,
                                ));
                                this.font_menu = Some((
                                    target,
                                    event.position.x.as_f32(),
                                    event.position.y.as_f32(),
                                ));
                                // 键盘交给搜索框：`accepts_text_input` 那边跟着打开输入法。
                                this.focus.focus(window, cx);
                            }
                            refresh_ui(window, cx);
                        }),
                    )
                    .child(
                        div()
                            .max_w(px(150.))
                            .min_w_0()
                            .truncate()
                            .text_size(theme::fs(11.5))
                            .font_family(shown.clone())
                            .text_color(theme::text_primary())
                            .child(shown),
                    )
                    .child(icon(
                        if open { "chevron-up" } else { "chevron-down" },
                        11.0,
                        theme::text_tertiary(),
                    )),
            )
            .into_any_element()
    }

    /// 字体列表里的一行：左边用这个字体自己画两个数字当预览。
    ///
    /// `family` 为空串表示「跟随系统」—— 那一行不指定字体，预览就是系统默认的样子。
    fn font_menu_row(
        &self,
        family: &str,
        config: &Config,
        lang: Lang,
        target: FontTarget,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let selected = family.eq_ignore_ascii_case(&target.current(config));
        let value = family.to_string();
        let preview = SharedString::from(family.to_string());
        let label = if family.is_empty() {
            lang.pick("默认（系统字体）", "Default (system)")
                .to_string()
        } else {
            family.to_string()
        };

        div()
            .id(SharedString::from(format!(
                "font-row-{}-{family}",
                match target {
                    FontTarget::TrayNumber => "n",
                    FontTarget::Ui => "u",
                    FontTarget::Panel => "p",
                }
            )))
            .flex()
            .flex_none()
            .items_center()
            .gap(px(8.))
            .px(px(6.))
            .py(px(4.))
            .rounded(px(theme::RADIUS_SMALL))
            .cursor_pointer()
            .when(selected, |style| style.bg(theme::accent_soft()))
            .hover(|style| style.bg(theme::surface_hover()))
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.pick_font(value.clone(), target, window, cx);
            }))
            .child(
                div()
                    .w(px(30.))
                    .flex_none()
                    .text_size(theme::fs(13.))
                    .font_family(preview.clone())
                    .text_color(if selected {
                        theme::accent()
                    } else {
                        theme::text_tertiary()
                    })
                    .child("88"),
            )
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(theme::fs(11.5))
                    // 图标字体的名字用界面字体画，否则整行都是方框。
                    .when(!Self::is_symbol_font(family), |row| {
                        row.font_family(preview)
                    })
                    .text_color(if selected {
                        theme::accent()
                    } else {
                        theme::text_primary()
                    })
                    .child(label),
            )
            .when(selected, |style| {
                style.child(icon("check", 13.0, theme::accent()))
            })
            .into_any_element()
    }

    /// 字体弹层顶部的搜索框：左边放大镜、中间搜索词（带光标）、右边清空。
    ///
    /// 弹层开着时键盘就归它（`handle_font_filter_key`），所以输入法通道
    /// 跟着弹层一起开 —— 中文名字体（微软雅黑、思源黑体）也能打进去筛。
    fn font_search_row(&self, lang: Lang, cx: &mut Context<Self>) -> AnyElement {
        let empty = self.font_filter.is_empty();
        let text = self.font_filter.clone();
        div()
            .relative()
            .flex()
            .flex_none()
            .items_center()
            .gap(px(5.))
            .px(px(6.))
            .py(px(4.))
            .rounded(px(theme::RADIUS_SMALL))
            .bg(theme::track())
            .child(icon("search", 11.0, theme::text_tertiary()))
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_w_0()
                    .items_center()
                    .gap(px(1.))
                    .text_size(theme::fs(11.5))
                    .text_color(theme::text_primary())
                    .when(empty, |row| {
                        row.child(
                            div()
                                .text_color(theme::text_tertiary())
                                .child(lang.pick("搜索字体…", "Search fonts…")),
                        )
                    })
                    .child(text)
                    .child(
                        div()
                            .w(px(1.5))
                            .h(px(11.))
                            .flex_none()
                            .bg(theme::text_primary()),
                    ),
            )
            .when(!empty, |row| {
                row.child(
                    div()
                        .id("font-search-clear")
                        .size(px(14.))
                        .flex()
                        .flex_none()
                        .items_center()
                        .justify_center()
                        .rounded(px(7.))
                        .cursor_pointer()
                        .hover(|style| style.bg(theme::surface_hover()))
                        .on_click(cx.listener(|this, _: &ClickEvent, _, cx| {
                            this.font_filter.clear();
                            this.reset_font_scroll();
                            cx.notify();
                        }))
                        .child(icon("close", 9.0, theme::text_tertiary())),
                )
            })
            .child(self.text_edit_canvas(cx))
            .into_any_element()
    }

    /// 数字字体弹出层：绝对定位挂在根节点上，所以不会被滚动区剪掉。
    fn font_menu_overlay(
        &mut self,
        config: &Config,
        lang: Lang,
        viewport: gpui::Size<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let (target, click_x, click_y) = self.font_menu?;
        let width = viewport.width.as_f32();
        let height = viewport.height.as_f32();

        // 按钮在行的右端：让面板右边缘贴着按钮，同时不越出窗口。
        let left =
            (click_x - FONT_MENU_WIDTH + 60.0).clamp(8.0, (width - FONT_MENU_WIDTH - 8.0).max(8.0));

        // 下面塞不下就往上翻；两边都不够高时按空间最高的那侧缩一点，
        // 绝不让它压到标题栏或超出窗口。
        let top_limit = 46.0;
        let bottom_limit = height - 16.0;
        let below = (bottom_limit - (click_y + 10.0)).max(0.0);
        let above = (click_y - 6.0 - top_limit).max(0.0);
        let menu_height = FONT_MENU_HEIGHT.min(below.max(above)).max(140.0);
        let top = if below >= menu_height {
            click_y + 10.0
        } else if above >= menu_height {
            click_y - 6.0 - menu_height
        } else {
            click_y + 10.0
        };

        // 搜索框空着时第一行是「跟随系统」；有搜索词就只列匹配的字体，
        // 那一行（不是字体）也就不该再出现。
        let query = self.font_filter.trim().to_string();
        let filtering = !query.is_empty();
        let fonts = self.filtered_fonts();

        let mut list = div()
            .id("tray-font-menu-list")
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .overflow_y_scroll()
            .track_scroll(&self.font_scroll)
            .on_scroll_wheel(cx.listener(|_, _, _, cx| cx.notify()));
        if !filtering {
            // 第一行永远是「跟随系统」：不然选了别的字体就回不去了。
            list = list.child(self.font_menu_row("", config, lang, target, cx));
        }
        for family in &fonts {
            list = list.child(self.font_menu_row(family, config, lang, target, cx));
        }
        if fonts.is_empty() {
            list = list.child(
                div()
                    .px(px(8.))
                    .py(px(10.))
                    .text_size(theme::fs(11.))
                    .text_color(theme::text_tertiary())
                    .child(lang.pick("没有匹配的字体", "No matching font")),
            );
        }

        // 打开时滚到当前选中的那个字体。
        //
        // 行边界是绘制时才记进 `ScrollHandle` 的：刚打开这一帧还量不到，
        // 所以先补一帧，等下一帧拿到 `bounds_for_item` 再算偏移量。
        if let Some(index) = self.font_pending_scroll {
            let row_count = fonts.len() + if filtering { 0 } else { 1 };
            if filtering || index >= row_count {
                // 搜索中不抢滚动位置（用户在挑结果）；下标越界也别硬滚。
                self.font_pending_scroll = None;
            } else if let Some(row) = self.font_scroll.bounds_for_item(index) {
                let view = self.font_scroll.bounds();
                let max = self.font_scroll.max_offset().y.as_f32();
                if max > 0.0 {
                    // `bounds_for_item` 是内容坐标（还没叠滚动偏移），所以直接按内容坐标算：
                    // 让这一行落在列表可视区的正中间。
                    let centered = (view.top() - row.top()).as_f32()
                        + (view.size.height - row.size.height).as_f32() / 2.0;
                    self.font_scroll
                        .set_offset(gpui::point(px(0.), px(centered.clamp(-max, 0.0))));
                    self.font_pending_scroll = None;
                    cx.notify();
                } else {
                    // 列表根本不够长（全都看得见），不用滚，也别在这里空转。
                    self.font_pending_scroll = None;
                }
            } else {
                // 还没量到：请求下一帧再看一次。
                cx.notify();
            }
        }

        Some(
            div()
                .id("tray-font-backdrop")
                .absolute()
                .left(px(0.))
                .top(px(0.))
                .size_full()
                // 滚轮会派发给「鼠标下所有命中框」，不遮挡的话背后的页面会跟着滚。
                .occlude()
                // 点空白处收起。
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(|this, _: &gpui::MouseDownEvent, window, cx| {
                        if this.font_menu.is_some() {
                            this.close_font_menu(window, cx);
                        }
                    }),
                )
                .child(
                    div()
                        .id("tray-font-menu")
                        .absolute()
                        .left(px(left))
                        .top(px(top))
                        .w(px(FONT_MENU_WIDTH))
                        .h(px(menu_height))
                        // 同上：面板也要挡住，滚轮才只滚列表。
                        .occlude()
                        .rounded(px(theme::RADIUS_ROW))
                        .border_1()
                        .border_color(theme::border())
                        // 用不透明的「浮起」色：`surface` 有 3% 透光，
                        // 底下的控件会隐隐透出来，看着像重影。
                        .bg(theme::surface_elevated())
                        .p(px(4.))
                        .flex()
                        .flex_col()
                        .gap(px(4.))
                        // 点在面板上不算「点空白处」。
                        .on_mouse_down(
                            gpui::MouseButton::Left,
                            cx.listener(|_, _: &gpui::MouseDownEvent, _, cx| cx.stop_propagation()),
                        )
                        .child(
                            div()
                                .px(px(6.))
                                .py(px(2.))
                                .flex_none()
                                .text_size(theme::fs(10.5))
                                .text_color(theme::text_tertiary())
                                .child(target.title(lang)),
                        )
                        // 搜索框：两三百个字体靠翻太费劲，打几个字就筛。
                        .child(self.font_search_row(lang, cx))
                        .child(
                            div()
                                .flex()
                                .flex_1()
                                .min_h_0()
                                .child(list)
                                // 列表的滚动条也放在内容外面。
                                .child(
                                    div()
                                        .w(px(SCROLLBAR_GUTTER))
                                        .h_full()
                                        .flex_none()
                                        .flex()
                                        .justify_center()
                                        .child(self.scrollbar_column(&self.font_scroll, 2, true)),
                                ),
                        ),
                )
                .into_any_element(),
        )
    }

    /// 色轮弹出层：饱和度方块 + 色相条 + 色号输入 + 屏幕取色。
    ///
    /// 和 OSD 编辑器里那套是同一套交互（那边是固定面板，这边跟着「+」按钮弹出来）。
    fn color_wheel_overlay(
        &self,
        lang: Lang,
        viewport: gpui::Size<Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        const WHEEL_W: f32 = 216.0;
        const SV_H: f32 = 92.0;
        const HUE_H: f32 = 10.0;
        const PANEL_W: f32 = WHEEL_W + 24.0;
        const PANEL_H: f32 = 196.0;

        let wheel = self.color_wheel.as_ref()?;
        let target = wheel.target;
        let hex = wheel.picker.hex.clone();
        let edit = wheel.picker.edit.clone();
        let picking = wheel.picking;
        let click_x = wheel.x;
        let click_y = wheel.y;
        // 方块 / 色相条：走 `ui::picker`（和 OSD 编辑器同一份），这里只给尺寸和回调。
        let this = cx.entity().downgrade();
        let press = {
            let this = this.clone();
            Rc::new(move |window: &mut Window, cx: &mut App| {
                let _ = this.update(cx, |this, cx| this.focus.focus(window, cx));
            }) as Rc<dyn Fn(&mut Window, &mut App)>
        };
        let sv_press = press.clone();
        let sv_change = {
            let this = this.clone();
            Rc::new(
                move |h: f32, s: f32, v: f32, window: &mut Window, cx: &mut App| {
                    let _ = this.update(cx, |this, cx| this.apply_wheel_hsv(h, s, v, window, cx));
                },
            ) as Rc<dyn Fn(f32, f32, f32, &mut Window, &mut App)>
        };
        let hue_press = press.clone();
        let hue_change = sv_change.clone();
        let sv_box =
            crate::ui::picker::sv_box("set", &wheel.picker, WHEEL_W, SV_H, sv_press, sv_change);
        let hue_box =
            crate::ui::picker::hue_box("set", &wheel.picker, WHEEL_W, HUE_H, hue_press, hue_change);

        let width = viewport.width.as_f32();
        let height = viewport.height.as_f32();
        // 按钮在行的右端：面板右边缘贴近按钮，但不越出窗口。
        let left = (click_x - PANEL_W + 22.0).clamp(8.0, (width - PANEL_W - 8.0).max(8.0));
        // 下面放不下就往上翻；两边都不够就贴标题栏下方。
        let below = height - 16.0 - (click_y + 10.0);
        let above = click_y - 10.0 - 46.0;
        let top = if below >= PANEL_H {
            click_y + 10.0
        } else if above >= PANEL_H {
            click_y - 10.0 - PANEL_H
        } else {
            (46.0_f32).min((height - PANEL_H - 8.0).max(8.0))
        };

        let hex_text = match &edit {
            Some(buffer) => format!("{buffer}|"),
            None => hex.clone(),
        };
        let hex_box = div()
            .id("set-hex")
            .flex_1()
            .h(px(24.))
            .px(px(7.))
            .flex()
            .items_center()
            .rounded(px(4.))
            .bg(theme::track())
            .border_1()
            .border_color(if edit.is_some() {
                theme::accent()
            } else {
                theme::border()
            })
            .cursor_text()
            .text_size(theme::fs(11.))
            .text_color(theme::text_primary())
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &gpui::MouseDownEvent, window, cx| {
                    if let Some(wheel) = this.color_wheel.as_mut() {
                        if wheel.picker.edit.is_none() {
                            wheel.picker.edit = Some(wheel.picker.hex.clone());
                        }
                        // 整串默认是「选中」状态：直接打字 / 粘贴 / 退格就换掉。
                        wheel.picker.edit_fresh = true;
                    }
                    this.focus.focus(window, cx);
                    cx.stop_propagation();
                    cx.notify();
                }),
            )
            .child(hex_text);

        let footer = div()
            .flex()
            .items_center()
            .gap(px(6.))
            .child(hex_box)
            .child(
                div()
                    .id("set-eyedrop")
                    .h(px(24.))
                    .px(px(8.))
                    .flex()
                    .items_center()
                    .rounded(px(4.))
                    .bg(theme::track())
                    .cursor_pointer()
                    .text_size(theme::fs(11.))
                    .text_color(if picking {
                        theme::accent()
                    } else {
                        theme::text_secondary()
                    })
                    .hover(|style| style.bg(theme::surface_hover()))
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.start_color_pick(window, cx);
                    }))
                    .child(lang.pick("取色", "Pick")),
            );

        // 「跟随」放在面板里而不是色块行外，行首那个半深半浅的块就是它；
        // 这里再给一个带字的入口（清空色号 + 回车也行，两个都在）。
        // 强调色回的是「跟随 Windows 的强调色」，底色回的是「跟随这一套主题」。
        let reset = div()
            .id("set-color-reset")
            .flex_none()
            .px(px(6.))
            .py(px(3.))
            .rounded(px(4.))
            .cursor_pointer()
            .text_size(theme::fs(10.5))
            .text_color(theme::text_tertiary())
            .hover(|style| style.bg(theme::surface_hover()))
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                this.set_custom_color(target, String::new(), window, cx);
                // 面板里的光标 / 色号也回到主题色，否则读数跟行上的「—」对不上。
                let hex = if target.is_surface() {
                    theme::surface_hex()
                } else {
                    theme::accent_hex()
                };
                if let Some(wheel) = this.color_wheel.as_mut() {
                    if parse_rgb(&hex).is_some() {
                        wheel.picker.set_hex(hex.clone());
                    }
                    wheel.picker.edit = None;
                    wheel.picker.edit_fresh = false;
                }
                refresh_ui(window, cx);
            }))
            .child(if target.is_surface() {
                lang.pick("跟随主题", "Follow theme")
            } else {
                lang.pick("跟随系统", "Follow Windows")
            });

        Some(
            div()
                .id("set-color-backdrop")
                .absolute()
                .left(px(0.))
                .top(px(0.))
                .size_full()
                .occlude()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _: &gpui::MouseDownEvent, window, cx| {
                        if this.color_wheel.take().is_some() {
                            refresh_ui(window, cx);
                        }
                    }),
                )
                .child(
                    div()
                        .id("set-color-wheel")
                        .absolute()
                        .left(px(left))
                        .top(px(top))
                        .w(px(PANEL_W))
                        .p(px(12.))
                        .flex()
                        .flex_col()
                        .gap(px(8.))
                        .occlude()
                        .rounded(px(theme::RADIUS_ROW))
                        // 浮层要不透明：透光的话底下的色块会隐隐透上来，像重影。
                        .bg(theme::opaque(theme::surface_elevated()))
                        .border_1()
                        .border_color(theme::border_strong())
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|_, _: &gpui::MouseDownEvent, _, cx| cx.stop_propagation()),
                        )
                        .child(
                            div()
                                .flex()
                                .items_center()
                                .justify_between()
                                .child(
                                    div()
                                        .text_size(theme::fs(11.5))
                                        .text_color(theme::text_primary())
                                        .child(target.label(lang)),
                                )
                                .child(reset),
                        )
                        .child(sv_box)
                        .child(hue_box)
                        .child(footer)
                        .child(
                            div()
                                .text_size(theme::fs(9.5))
                                .text_color(theme::text_tertiary())
                                .child(lang.pick(
                                    "拖色块选色 · 点色号可粘贴 · 回车确认",
                                    "Drag to pick · click the hex to paste · Enter to apply",
                                )),
                        ),
                )
                .into_any_element(),
        )
    }

    /// 图标选择器里的一格：图标 + 名字，点一下就把这台设备的图标换成它。
    fn icon_picker_cell(
        &self,
        choice: &'static crate::ui::DeviceIconChoice,
        device_id: &str,
        selected: bool,
        lang: Lang,
        cell_w: f32,
        cell_h: f32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let store = self.config.clone();
        let id = device_id.to_string();
        div()
            .id(SharedString::from(format!("icon-pick-{}", choice.icon)))
            .w(px(cell_w))
            .h(px(cell_h))
            .flex_none()
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap(px(3.))
            .rounded(px(theme::RADIUS_SMALL))
            .cursor_pointer()
            .when(selected, |style| style.bg(theme::accent_soft()))
            .hover(|style| style.bg(theme::surface_hover()))
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &gpui::MouseDownEvent, window, cx| {
                    // 弹层内部的 on_click 会被 `occlude` + 「点别处」吃掉，
                    // 和其它弹层一样挂在按下阶段。
                    cx.stop_propagation();
                    let icon = choice.icon;
                    let _ = store.borrow_mut().update(|config| {
                        config.set_device_icon(&id, icon);
                    });
                    this.icon_picker = None;
                    refresh_ui(window, cx);
                }),
            )
            .child(icon(
                choice.icon,
                20.0,
                if selected {
                    theme::accent()
                } else {
                    theme::text_secondary()
                },
            ))
            .child(
                div()
                    .text_size(theme::fs(9.5))
                    .text_color(if selected {
                        theme::accent()
                    } else {
                        theme::text_tertiary()
                    })
                    .child(lang.pick(choice.zh, choice.en)),
            )
            .into_any_element()
    }

    /// 设备图标选择器：一格一个图标（图标 + 名字），最后一行是「跟随设备类型」。
    ///
    /// 定位规则和别的弹层一样：绝对定位挂在根节点上（不被滚动区剪掉），
    /// 从点击处往下铺，下面放不下就翻到上面。
    fn icon_picker_overlay(
        &self,
        config: &Config,
        lang: Lang,
        viewport: gpui::Size<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        const COLS: usize = 3;
        const CELL_W: f32 = 62.0;
        const CELL_H: f32 = 50.0;
        const GAP: f32 = 4.0;
        const PAD: f32 = 6.0;
        const RESET_H: f32 = 24.0;

        let (device_id, click_x, click_y) = self.icon_picker.clone()?;
        let view_w = viewport.width.as_f32();
        let view_h = viewport.height.as_f32();
        let rows = DEVICE_ICON_CHOICES.len().div_ceil(COLS);
        let panel_w = COLS as f32 * CELL_W + (COLS as f32 - 1.0) * GAP + PAD * 2.0;
        let panel_h =
            rows as f32 * CELL_H + (rows as f32 - 1.0) * GAP + PAD * 2.0 + 1.0 + 6.0 + RESET_H;

        let left = (click_x - panel_w * 0.5).clamp(8.0, (view_w - panel_w - 8.0).max(8.0));
        let top = if click_y + 10.0 + panel_h <= view_h - 12.0 {
            click_y + 10.0
        } else {
            (click_y - 10.0 - panel_h).max(46.0)
        };

        // 当前存的是哪个（空 = 跟随设备类型）。
        let current = config.device_icon(&device_id).unwrap_or("").to_string();

        let mut grid = div().flex().flex_col().gap(px(GAP));
        for row in DEVICE_ICON_CHOICES.chunks(COLS) {
            let mut line = div().flex().gap(px(GAP));
            for choice in row {
                let selected = current == choice.icon;
                line = line.child(
                    self.icon_picker_cell(choice, &device_id, selected, lang, CELL_W, CELL_H, cx),
                );
            }
            // 最后一行不满时补空位，格子不会被拉宽。
            for _ in row.len()..COLS {
                line = line.child(div().w(px(CELL_W)).h(px(CELL_H)).flex_none());
            }
            grid = grid.child(line);
        }

        // 「跟随设备类型」：清掉自定义图标，回到按 FormFactor 猜。
        let reset = {
            let store = self.config.clone();
            let id = device_id.clone();
            let active = current.is_empty();
            div()
                .id("icon-pick-auto")
                .h(px(RESET_H))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(theme::RADIUS_SMALL))
                .cursor_pointer()
                .text_size(theme::fs(10.5))
                .text_color(if active {
                    theme::accent()
                } else {
                    theme::text_tertiary()
                })
                .hover(|style| style.bg(theme::surface_hover()))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(move |this, _: &gpui::MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        let _ = store.borrow_mut().update(|config| {
                            config.set_device_icon(&id, "");
                        });
                        this.icon_picker = None;
                        refresh_ui(window, cx);
                    }),
                )
                .child(lang.pick("跟随设备类型", "Match device type"))
        };

        Some(
            div()
                .id("icon-picker-backdrop")
                .absolute()
                .left(px(0.))
                .top(px(0.))
                .size_full()
                // 滚轮 / 点击都别漏给后面的页面。
                .occlude()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _: &gpui::MouseDownEvent, window, cx| {
                        if this.icon_picker.take().is_some() {
                            refresh_ui(window, cx);
                        }
                    }),
                )
                .child(
                    div()
                        .id("icon-picker")
                        .absolute()
                        .left(px(left))
                        .top(px(top))
                        .w(px(panel_w))
                        .occlude()
                        .rounded(px(theme::RADIUS_ROW))
                        .border_1()
                        .border_color(theme::border())
                        .bg(theme::surface_elevated())
                        .p(px(PAD))
                        .flex()
                        .flex_col()
                        .gap(px(6.))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|_, _: &gpui::MouseDownEvent, _, cx| cx.stop_propagation()),
                        )
                        .child(grid)
                        .child(div().h(px(1.)).flex_none().bg(theme::border()))
                        .child(reset),
                )
                .into_any_element(),
        )
    }

    /// 设备下拉的弹出层：定位规则和字体下拉完全一致（绝对定位挂根节点、上下翻转、带背板）。
    fn device_menu_overlay(
        &self,
        config: &Config,
        lang: Lang,
        snapshot: &Snapshot,
        viewport: gpui::Size<gpui::Pixels>,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        let (which, click_x, click_y) = self.picker?;
        let width = viewport.width.as_f32();
        let height = viewport.height.as_f32();

        let left = (click_x - DEVICE_MENU_WIDTH + 40.0)
            .clamp(8.0, (width - DEVICE_MENU_WIDTH - 8.0).max(8.0));
        let top_limit = 46.0;
        let bottom_limit = height - 16.0;
        let below = (bottom_limit - (click_y + 10.0)).max(0.0);
        let above = (click_y - 6.0 - top_limit).max(0.0);
        let menu_height = DEVICE_MENU_HEIGHT.min(below.max(above)).max(120.0);
        let top = if below >= menu_height {
            click_y + 10.0
        } else if above >= menu_height {
            click_y - 6.0 - menu_height
        } else {
            click_y + 10.0
        };

        let mut list = div()
            .id("device-menu-list")
            .flex()
            .flex_col()
            .flex_1()
            .min_w_0()
            .min_h_0()
            .gap(px(2.))
            .overflow_y_scroll()
            .track_scroll(&self.picker_scroll)
            .on_scroll_wheel(cx.listener(|_, _, _, cx| cx.notify()));
        let hint = |text: &'static str| {
            div()
                .px(px(8.))
                .py(px(8.))
                .text_size(theme::fs(11.))
                .text_color(theme::text_tertiary())
                .child(text)
        };

        // 「添加到名单」、选设备、选显示器共用一套弹出层：选项来源不同而已。
        match which {
            DevicePicker::OsdMonitor => {
                let options = crate::windows::osd::display_options();
                if options.is_empty() {
                    list = list.child(hint(lang.pick("没有检测到显示器", "No displays found")));
                } else {
                    for (value, name) in options {
                        list = list.child(self.monitor_menu_row(
                            value,
                            &name,
                            value == config.osd.monitor,
                            cx,
                        ));
                    }
                }
            }
            DevicePicker::StartupDefault | DevicePicker::BoundDevice => {
                let current_id = match which {
                    DevicePicker::StartupDefault => config.startup.default_device_id.clone(),
                    _ => config.volume_cap.device_id.clone(),
                };
                let devices: Vec<(String, String, crate::audio::DeviceKind)> = snapshot
                    .devices
                    .iter()
                    .filter(|device| device.enabled)
                    .map(|device| {
                        (
                            device.id.clone(),
                            config.device_label(&device.id, &device.name),
                            device.kind,
                        )
                    })
                    .collect();
                if devices.is_empty() {
                    list = list.child(hint(lang.pick("没有可用设备", "No devices available")));
                } else {
                    for (id, name, kind) in &devices {
                        list = list.child(self.device_menu_row(
                            id,
                            name,
                            *kind,
                            *id == current_id,
                            which,
                            cx,
                        ));
                    }
                }
            }
            DevicePicker::AddDeviceState => {
                let recorded: Vec<&str> = config
                    .startup
                    .device_list(config.startup.device_state_mode)
                    .iter()
                    .map(|state| state.id.as_str())
                    .collect();
                let devices: Vec<(String, String, crate::audio::DeviceKind, bool)> = snapshot
                    .devices
                    .iter()
                    .filter(|device| !recorded.contains(&device.id.as_str()))
                    .map(|device| {
                        (
                            device.id.clone(),
                            config.device_label(&device.id, &device.name),
                            device.kind,
                            device.enabled,
                        )
                    })
                    .collect();
                if devices.is_empty() {
                    list = list.child(hint(lang.pick(
                        "名单里已经有全部设备了",
                        "Every device is already in the list",
                    )));
                } else {
                    for (id, name, kind, enabled) in &devices {
                        list = list
                            .child(self.add_device_menu_row(id, name, *kind, *enabled, lang, cx));
                    }
                }
            }
            DevicePicker::AddAppVolume => {
                let recorded: Vec<&str> = config
                    .startup
                    .app_volumes
                    .iter()
                    .map(|app| app.key.as_str())
                    .collect();
                // 一个应用可能挂好几个会话（Chromium 系常见），按名字去重。
                let mut seen: Vec<String> = Vec::new();
                let apps: Vec<(String, String, u32)> = snapshot
                    .apps
                    .iter()
                    .filter(|app| !app.is_system)
                    .filter(|app| !recorded.contains(&app.key.as_str()))
                    .filter(|app| {
                        if seen.iter().any(|name| name == &app.name) {
                            return false;
                        }
                        seen.push(app.name.clone());
                        true
                    })
                    .map(|app| {
                        (
                            app.key.clone(),
                            app.name.clone(),
                            (app.volume * 100.0).round().clamp(0.0, 100.0) as u32,
                        )
                    })
                    .collect();
                if apps.is_empty() {
                    let no_sessions = !snapshot.apps.iter().any(|app| !app.is_system);
                    list = list.child(hint(if no_sessions {
                        lang.pick("当前没有应用会话", "No audio sessions right now")
                    } else {
                        lang.pick(
                            "没有可添加的应用（列表里已经有全部会话）",
                            "No apps left to add (all sessions are listed)",
                        )
                    }));
                } else {
                    for (key, name, percent) in &apps {
                        list = list.child(self.add_app_menu_row(key, name, *percent, cx));
                    }
                }
            }
        }

        Some(
            div()
                .id("device-menu-backdrop")
                .absolute()
                .left(px(0.))
                .top(px(0.))
                .size_full()
                // 滚轮会派发给「鼠标下所有命中框」，不遮挡的话背后的页面会跟着滚。
                .occlude()
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(|this, _: &gpui::MouseDownEvent, window, cx| {
                        if this.picker.take().is_some() {
                            refresh_ui(window, cx);
                        }
                    }),
                )
                .child(
                    div()
                        .id("device-menu")
                        .absolute()
                        .left(px(left))
                        .top(px(top))
                        .w(px(DEVICE_MENU_WIDTH))
                        .h(px(menu_height))
                        .occlude()
                        .rounded(px(theme::RADIUS_ROW))
                        .border_1()
                        .border_color(theme::border())
                        .bg(theme::surface_elevated())
                        .p(px(4.))
                        .flex()
                        .flex_col()
                        .on_mouse_down(
                            gpui::MouseButton::Left,
                            cx.listener(|_, _: &gpui::MouseDownEvent, _, cx| cx.stop_propagation()),
                        )
                        .child(
                            div().flex().flex_1().min_h_0().min_w_0().child(list).child(
                                div()
                                    .w(px(SCROLLBAR_GUTTER))
                                    .h_full()
                                    .flex_none()
                                    .flex()
                                    .justify_center()
                                    .child(self.scrollbar_column(&self.picker_scroll, 3, true)),
                            ),
                        ),
                )
                .into_any_element(),
        )
    }

    /// 设备列表里的一行：图标 + 名字 +（选中的那个打勾）。
    /// 显示器下拉里的一行：名字 + 勾选，点一下定下 `osd.monitor`。
    fn monitor_menu_row(
        &self,
        value: u32,
        name: &str,
        selected: bool,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let store = self.config.clone();
        let name = SharedString::from(name.to_string());
        div()
            .id(SharedString::from(format!("osd-monitor-{value}")))
            .flex()
            .flex_none()
            .items_center()
            .gap(px(8.))
            .px(px(6.))
            .py(px(5.))
            .rounded(px(theme::RADIUS_SMALL))
            .cursor_pointer()
            .when(selected, |style| style.bg(theme::accent_soft()))
            .hover(|style| style.bg(theme::surface_hover()))
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                let _ = store.borrow_mut().update(|config| {
                    config.osd.monitor_mode = crate::config::OsdMonitorMode::Single;
                    config.osd.monitor = value;
                });
                // 选完就收起来，不用再点一下。
                this.picker = None;
                refresh_ui(window, cx);
            }))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(theme::fs(11.5))
                    .text_color(if selected {
                        theme::accent()
                    } else {
                        theme::text_primary()
                    })
                    .child(name),
            )
            .when(selected, |style| {
                style.child(icon("check", 13.0, theme::accent()))
            })
            .into_any_element()
    }

    fn device_menu_row(
        &self,
        id: &str,
        name: &str,
        kind: crate::audio::DeviceKind,
        selected: bool,
        which: DevicePicker,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let store = self.config.clone();
        let device_id = id.to_string();
        let device_name = name.to_string();
        let shown = name.to_string();
        let key = SharedString::from(format!("device-pick-{id}"));

        div()
            .id(key)
            .flex()
            .flex_none()
            .items_center()
            .gap(px(8.))
            .px(px(6.))
            .py(px(5.))
            .rounded(px(theme::RADIUS_SMALL))
            .cursor_pointer()
            .when(selected, |style| style.bg(theme::accent_soft()))
            .hover(|style| style.bg(theme::surface_hover()))
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                let (id, name) = (device_id.clone(), device_name.clone());
                let _ = store.borrow_mut().update(|config| match which {
                    DevicePicker::StartupDefault => {
                        config.startup.default_device_id = id.clone();
                        config.startup.default_device_name = name.clone();
                    }
                    DevicePicker::BoundDevice => {
                        config.volume_cap.device_id = id.clone();
                        config.volume_cap.device_name = name.clone();
                    }
                    // 「添加」两种各有一行，显示器那行也各有一份，都走不到这里。
                    DevicePicker::AddDeviceState
                    | DevicePicker::AddAppVolume
                    | DevicePicker::OsdMonitor => {}
                });
                if which == DevicePicker::BoundDevice {
                    crate::gui::refresh_volume_cap(cx);
                }
                // 选完就收起来，不用再点一下。
                this.picker = None;
                refresh_ui(window, cx);
            }))
            .child(icon(
                &self.device_icon(id, kind),
                14.0,
                theme::text_tertiary(),
            ))
            .child(
                div()
                    .flex_1()
                    .min_w_0()
                    .truncate()
                    .text_size(theme::fs(11.5))
                    .text_color(if selected {
                        theme::accent()
                    } else {
                        theme::text_primary()
                    })
                    .child(shown),
            )
            .when(selected, |style| {
                style.child(icon("check", 13.0, theme::accent()))
            })
            .into_any_element()
    }

    /// 下拉里「加进设备开关名单」的一行：图标 + 名字 + 它当前是开还是关。
    fn add_device_menu_row(
        &self,
        id: &str,
        name: &str,
        kind: crate::audio::DeviceKind,
        enabled: bool,
        lang: Lang,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let store = self.config.clone();
        let device_id = id.to_string();
        let device_name = name.to_string();
        let shown = name.to_string();

        div()
            .id(SharedString::from(format!("add-device-{id}")))
            .flex()
            .flex_none()
            .items_center()
            .gap(px(8.))
            .px(px(6.))
            .py(px(5.))
            .rounded(px(theme::RADIUS_SMALL))
            .cursor_pointer()
            .hover(|style| style.bg(theme::surface_hover()))
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                let (id, name) = (device_id.clone(), device_name.clone());
                let _ = store.borrow_mut().update(|config| {
                    let mode = config.startup.device_state_mode;
                    let list = config.startup.device_list_mut(mode);
                    if !list.iter().any(|state| state.id == id) {
                        // 黑名单 = 加进来就是「要禁用」的，白名单 = 「要保留」的；
                        // 手动模式才按它现在真实的状态落库。
                        let enabled = match mode {
                            crate::config::DeviceStateMode::Blacklist => false,
                            crate::config::DeviceStateMode::Whitelist => true,
                            _ => enabled,
                        };
                        list.push(crate::config::StartupDeviceState { id, name, enabled });
                    }
                });
                this.picker = None;
                refresh_ui(window, cx);
            }))
            .child(icon(
                &self.device_icon(id, kind),
                14.0,
                theme::text_tertiary(),
            ))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .truncate()
                    .text_size(theme::fs(11.5))
                    .text_color(theme::text_primary())
                    .child(shown),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(theme::fs(9.5))
                    .text_color(theme::text_tertiary())
                    .child(if enabled {
                        lang.pick("当前启用", "On now")
                    } else {
                        lang.pick("当前禁用", "Off now")
                    }),
            )
            .into_any_element()
    }

    /// 下拉里「加进应用音量名单」的一行：名字 + 它现在的音量。
    fn add_app_menu_row(
        &self,
        key: &str,
        name: &str,
        percent: u32,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let store = self.config.clone();
        let app_key = key.to_string();
        let app_name = name.to_string();
        let shown = name.to_string();

        div()
            .id(SharedString::from(format!("add-app-{key}")))
            .flex()
            .flex_none()
            .items_center()
            .gap(px(8.))
            .px(px(6.))
            .py(px(5.))
            .rounded(px(theme::RADIUS_SMALL))
            .cursor_pointer()
            .hover(|style| style.bg(theme::surface_hover()))
            .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                let (key, name) = (app_key.clone(), app_name.clone());
                let _ = store.borrow_mut().update(|config| {
                    if !config.startup.app_volumes.iter().any(|app| app.key == key) {
                        config
                            .startup
                            .app_volumes
                            .push(crate::config::StartupAppVolume { key, name, percent });
                    }
                });
                this.picker = None;
                refresh_ui(window, cx);
            }))
            .child(
                div()
                    .flex_1()
                    .min_w(px(0.))
                    .truncate()
                    .text_size(theme::fs(11.5))
                    .text_color(theme::text_primary())
                    .child(shown),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(theme::fs(9.5))
                    .text_color(theme::text_tertiary())
                    .child(format!("{percent}%")),
            )
            .into_any_element()
    }

    /// 显示器页：亮度调节总开关、步进、预设管理。
    ///
    /// 亮度窗口本身在音量面板旁边，这里只管「开不开」和「存了哪些预设」。
    /// 对比度之类的调节将来会挂在这一页下面（VCP 0x12 常量已经在 display 里留好）。
    /// 显示器列表里的一行：图标 + 名字（可就地改名）+ 亮度读数 + 「改名」。
    ///
    /// 交互和音频设备行是同一套：点「改名」从空白开始输入，回车或点别处落盘。
    fn display_row(
        &self,
        display: &DisplayInfo,
        index: usize,
        lang: Lang,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = display.key.clone();
        let (label, aliased) = {
            let store = self.config.borrow();
            let config = store.get();
            (
                config.display_label(&key, &display.name),
                config.display_alias(&key).is_some(),
            )
        };
        let renaming = self
            .display_rename
            .as_ref()
            .is_some_and(|(other, _)| *other == key);

        let rename = {
            let key = key.clone();
            div()
                .id(SharedString::from(format!(
                    "settings-display-rename-{index}"
                )))
                .flex_none()
                .px(px(8.))
                .py(px(4.))
                .rounded(px(4.))
                .text_size(theme::fs(10.5))
                .cursor_pointer()
                .text_color(if renaming {
                    theme::accent()
                } else {
                    theme::text_tertiary()
                })
                .hover(|style| style.bg(theme::surface_hover()))
                .on_mouse_down(
                    gpui::MouseButton::Left,
                    cx.listener(move |this, _: &gpui::MouseDownEvent, window, cx| {
                        cx.stop_propagation();
                        // 已经在改名（点的是同一个「改名」）：别把刚敲的字清空重来。
                        // 换一行点的时候这里已经是 `None` 了 —— 那一下在捕获相位
                        // 就替上一行落了盘（见 `rename_text_field`）。
                        if this.display_rename.is_some() || this.rename_just_committed(&key) {
                            return;
                        }
                        // 和音频设备一样从空白开始：原来的名字留在灰色占位里。
                        this.display_rename = Some((key.clone(), String::new()));
                        this.rename_cursor = 0;
                        this.text_marked = None;
                        this.focus.focus(window, cx);
                        cx.notify();
                    }),
                )
                .child(lang.pick("改名", "Rename"))
        };

        div()
            .id(SharedString::from(format!("settings-display-{index}")))
            .flex()
            .items_center()
            .gap(px(10.))
            .px(px(9.))
            .py(px(5.))
            .rounded(px(theme::RADIUS_ROW))
            .hover(|style| style.bg(theme::surface_hover()))
            .child(icon("monitor", 14.0, theme::text_secondary()))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .flex_1()
                    .min_w_0()
                    .gap(px(1.))
                    // 名字本身也点得动：一整块热区，比右边那个小字好瞄准。
                    // 已经在改名时不要再接一次（不然点字里会把刚设的编辑态顶掉）。
                    .when(!renaming, |row| {
                        let key = key.clone();
                        row.on_mouse_down(
                            gpui::MouseButton::Left,
                            cx.listener(move |this, _: &gpui::MouseDownEvent, window, cx| {
                                cx.stop_propagation();
                                // 换一行点：上一行已经在捕获相位落过盘了。
                                if this.display_rename.is_some() {
                                    return;
                                }
                                this.display_rename = Some((key.clone(), String::new()));
                                this.rename_cursor = 0;
                                this.text_marked = None;
                                this.focus.focus(window, cx);
                                cx.notify();
                            }),
                        )
                    })
                    .cursor_pointer()
                    .child(if renaming {
                        self.rename_text_field(label.clone(), cx)
                    } else {
                        div()
                            .text_size(theme::fs(12.5))
                            .text_color(theme::text_primary())
                            .truncate()
                            .child(label.clone())
                            .into_any_element()
                    })
                    // 改过名字之后把系统原名当副标题露出来（只在设置页里）。
                    .when(aliased, |row| {
                        row.child(
                            div()
                                .text_size(theme::fs(10.5))
                                .text_color(theme::text_tertiary())
                                .truncate()
                                .child(format!(
                                    "{}{}",
                                    lang.pick("原名：", "Original: "),
                                    display.name
                                )),
                        )
                    }),
            )
            .child(
                div()
                    .flex_none()
                    .text_size(theme::fs(11.5))
                    .text_color(if display.brightness.is_some() {
                        theme::text_secondary()
                    } else {
                        theme::text_tertiary()
                    })
                    .child(match display.brightness {
                        Some(percent) => format!("{percent}%"),
                        None => lang.pick("不支持", "N/A").to_string(),
                    }),
            )
            .child(rename)
            .into_any_element()
    }

    fn display_page(&self, config: &Config, lang: Lang, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let store = self.config.clone();
        let enabled = config.brightness.enabled;

        // 显示器列表：名字能就地改（和音频设备同款），亮度读数顺带露一眼。
        let display_list = crate::gui::displays_snapshot(cx);
        let mut display_rows: Vec<AnyElement> = Vec::new();
        if display_list.is_empty() {
            display_rows.push(
                div()
                    .px(px(9.))
                    .py(px(6.))
                    .text_size(theme::fs(10.5))
                    .text_color(theme::text_tertiary())
                    .child(lang.pick("还没有枚举到显示器", "No displays detected yet"))
                    .into_any_element(),
            );
        }
        for (index, display) in display_list.iter().enumerate() {
            display_rows.push(self.display_row(display, index, lang, cx));
        }

        let merged = config.brightness.merged;
        let toggle = self.toggle_row(
            "display-brightness",
            lang.pick("启用显示器亮度面板", "Enable display brightness panel"),
            if merged {
                lang.pick(
                    "亮度块长在音量面板里（DDC/CI）",
                    "The brightness block lives inside the volume panel (DDC/CI)",
                )
            } else {
                lang.pick(
                    "开启后，音量面板旁边会多出一个亮度面板（DDC/CI）",
                    "Adds a brightness panel beside the volume panel (DDC/CI)",
                )
            },
            enabled,
            move |window, cx| {
                let _ = store.borrow_mut().update(|config| {
                    config.brightness.enabled = !config.brightness.enabled;
                });
                // 立刻生效：面板正开着的话亮度窗口马上出现 / 消失。
                crate::gui::refresh_brightness(cx);
                refresh_ui(window, cx);
            },
        );

        // 融合：亮度块不再是旁边一扇小窗，而是长进音量面板里，排版跟着音频走。
        let merge_store = self.config.clone();
        let merge_toggle = self.toggle_row(
            "display-brightness-merge",
            lang.pick("与音量面板融合", "Merge with the volume panel"),
            lang.pick(
                "亮度块长在音量面板里，排版跟随音频：横排铺在上下，竖排贴在左右",
                "The brightness block lives inside the volume panel and follows its layout: a strip above or below when horizontal, a column beside it when vertical",
            ),
            merged,
            move |window, cx| {
                let _ = merge_store.borrow_mut().update(|config| {
                    config.brightness.merged = !config.brightness.merged;
                });
                crate::gui::refresh_merged_brightness(cx);
                refresh_ui(window, cx);
            },
        );

        let step = self.number_row(
            "brightness-step",
            lang.pick("亮度步进", "Brightness step"),
            lang.pick(
                "每按一次 + / − 改变多少个百分点",
                "How many percent each + / − click changes",
            ),
            NumberField::BrightnessStep,
            config,
            lang,
            cx,
        );

        // 面板宽度：滑条长度直接跟着它走 —— 宽度一改，「1 像素 = 多少个百分点」也变。
        let width_row = self.number_row(
            "brightness-width",
            lang.pick("面板宽度", "Panel width"),
            lang.pick(
                "0 / 跟随 = 和音量面板一样宽；写具体像素就自己说了算",
                "0 / Match = same width as the volume panel; type a pixel value to override",
            ),
            NumberField::BrightnessWidth,
            config,
            lang,
            cx,
        );

        // 竖向高度：默认跟音频面板一样高，两个面板并排看着齐。
        let vertical_height_row = self.number_row(
            "brightness-vertical-height",
            lang.pick("竖向高度", "Vertical height"),
            lang.pick(
                "竖向排版时面板有多高（像素），多出来的都给推子；0 / 跟随 = 和音频面板一样高",
                "How tall the panel is in vertical layout, in pixels; the extra space goes to the faders. 0 / Match = same height as the volume panel",
            ),
            NumberField::BrightnessVerticalHeight,
            config,
            lang,
            cx,
        );

        // 贴在音量面板的哪一侧：上下左右四选一。
        let placement_options: &'static [(u32, &'static str)] = if lang.is_en() {
            &[(0, "Left"), (1, "Right"), (2, "Above"), (3, "Below")]
        } else {
            &[(0, "左侧"), (1, "右侧"), (2, "上方"), (3, "下方")]
        };
        let placement_row = {
            let store = self.config.clone();
            self.choice_row(
                "brightness-placement",
                lang.pick("相对音量面板", "Around the volume panel"),
                lang.pick(
                    "亮度面板贴在音量面板哪一侧；那一侧贴着屏幕边放不下时会自动翻到对面",
                    "Which side of the volume panel it sits on; if that side runs off screen it flips to the other one",
                ),
                placement_options,
                config.brightness.placement.index(),
                move |value, window, cx| {
                    let placement = BrightnessPlacement::from_index(value);
                    let _ = store
                        .borrow_mut()
                        .update(|config| config.brightness.placement = placement);
                    // 位置改了立刻重摆：面板可能得往里去让地方，亮度窗再跟着贴过来。
                    crate::gui::refresh_flyout_size(cx);
                    refresh_ui(window, cx);
                },
            )
        };

        // 融合时的位置：横排只认上方 / 下方，竖排只认左侧 / 右侧 ——
        // 选项直接按音频面板的朝向出，另一轴那两档根本不出来。
        let audio_vertical = config.panel_layout.is_vertical();
        let merge_options: &'static [(u32, &'static str)] = if audio_vertical {
            if lang.is_en() {
                &[(2, "Left"), (3, "Right")]
            } else {
                &[(2, "左侧"), (3, "右侧")]
            }
        } else if lang.is_en() {
            &[(0, "Above"), (1, "Below")]
        } else {
            &[(0, "上方"), (1, "下方")]
        };
        let merge_side_row = {
            let store = self.config.clone();
            self.choice_row(
                "brightness-merge-side",
                lang.pick("融合位置", "Merged position"),
                lang.pick(
                    "亮度块摆在音频的上侧还是下侧；音频竖向排版时换成左侧还是右侧",
                    "Whether the brightness block sits above or below the audio; when the audio panel is vertical, left or right instead",
                ),
                merge_options,
                config.brightness.merge_side_of(audio_vertical).index(),
                move |value, window, cx| {
                    let side = BrightnessMergeSide::from_index(value);
                    let _ = store
                        .borrow_mut()
                        .update(|config| config.brightness.merge_side = side);
                    crate::gui::refresh_merged_brightness(cx);
                    refresh_ui(window, cx);
                },
            )
        };

        // 排版：横排（一行一台显示器）/ 竖排（一台一条立起来的推子）。
        let layout_options: &'static [(u32, &'static str)] = if lang.is_en() {
            &[(0, "Horizontal"), (1, "Vertical")]
        } else {
            &[(0, "横向"), (1, "竖向")]
        };
        let layout_row = {
            let store = self.config.clone();
            self.choice_row(
                "brightness-layout",
                lang.pick("面板排版", "Panel layout"),
                lang.pick(
                    "横向是一台显示器一行；竖向是一台显示器一条立起来的推子，亮度条长度跟着面板高度走",
                    "Horizontal gives each display its own row; vertical gives each one an upright fader whose length follows the panel height",
                ),
                layout_options,
                config.brightness.panel_layout.index(),
                move |value, window, cx| {
                    let layout = PanelLayout::from_index(value);
                    let _ = store
                        .borrow_mut()
                        .update(|config| config.brightness.panel_layout = layout);
                    crate::gui::refresh_flyout_size(cx);
                    refresh_ui(window, cx);
                },
            )
        };

        // 竖向排版时显示器名写成什么：编号 / 全名（两种都不带括号里的型号）。
        let name_options: &'static [(u32, &'static str)] = if lang.is_en() {
            &[(0, "Number"), (1, "Full name")]
        } else {
            &[(0, "编号"), (1, "全名")]
        };
        let name_row = {
            let store = self.config.clone();
            self.choice_row(
                "brightness-name-mode",
                lang.pick("显示器名", "Display name"),
                lang.pick(
                    "竖向排版时列头写什么：编号只写 1、2；全名去掉括号里的型号，放不下就横向滚动",
                    "What the column header shows in vertical layout: the number only, or the full name without the part in parentheses (scrolls when it does not fit)",
                ),
                name_options,
                config.brightness.name_mode.index(),
                move |value, window, cx| {
                    let mode = DisplayNameMode::from_index(value);
                    let _ = store
                        .borrow_mut()
                        .update(|config| config.brightness.name_mode = mode);
                    crate::gui::refresh_brightness_window(cx);
                    refresh_ui(window, cx);
                },
            )
        };

        // 悬停滚轮：和音量那三项是同一套手感，默认开。
        let scroll_toggle = self.compact_toggle_row(
            "display-scroll-wheel",
            lang.pick(
                "在亮度条上滚动滚轮调亮度",
                "Scroll on a brightness slider to change it",
            ),
            lang.pick(
                "只调整光标停着的那一台",
                "Only the display under the cursor",
            ),
            config.brightness.scroll_wheel,
            self.bind(|config| config.brightness.scroll_wheel = !config.brightness.scroll_wheel),
        );

        let scroll_step = self.number_row(
            "brightness-scroll-step",
            lang.pick("滚轮步进", "Wheel step"),
            lang.pick(
                "在亮度条上滚一格改变多少个百分点；点数字可直接填",
                "How many percent one wheel notch changes on the brightness bar; click the number to type",
            ),
            NumberField::BrightnessScrollStep,
            config,
            lang,
            cx,
        );

        let presets = config.brightness.presets.clone();
        let mut preset_rows: Vec<AnyElement> = Vec::new();
        if presets.is_empty() {
            preset_rows.push(
                div()
                    .px(px(9.))
                    .py(px(6.))
                    .text_size(theme::fs(10.5))
                    .text_color(theme::text_tertiary())
                    .child(lang.pick(
                        "还没有预设：在亮度面板里点「保存预设」即存下当前亮度",
                        "No presets yet — press Save preset in the brightness panel to store the current level",
                    ))
                    .into_any_element(),
            );
        }
        for (index, preset) in presets.iter().enumerate() {
            let store = self.config.clone();
            preset_rows.push(
                div()
                    .id(SharedString::from(format!("brightness-preset-{index}")))
                    .flex()
                    .items_center()
                    .gap(px(10.))
                    .px(px(9.))
                    .py(px(4.))
                    .rounded(px(theme::RADIUS_ROW))
                    .hover(|style| style.bg(theme::surface_hover()))
                    .child(icon("sun", 12.0, theme::text_tertiary()))
                    .child(
                        div()
                            .flex_1()
                            .min_w(px(0.))
                            .truncate()
                            .text_size(theme::fs(12.5))
                            .text_color(theme::text_primary())
                            .child(preset.name.clone()),
                    )
                    // 右键删除和亮度窗口里一致；这里再给一个显眼的 ×，鼠标不用猜。
                    .child(
                        div()
                            .id(SharedString::from(format!(
                                "brightness-preset-remove-{index}"
                            )))
                            .size(px(18.))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(theme::RADIUS_SMALL))
                            .cursor_pointer()
                            .hover(|style| style.bg(theme::surface_hover()))
                            .on_click(cx.listener(move |_this, _: &ClickEvent, window, cx| {
                                let _ = store.borrow_mut().update(|config| {
                                    if index < config.brightness.presets.len() {
                                        config.brightness.presets.remove(index);
                                    }
                                });
                                crate::gui::refresh_brightness_window(cx);
                                refresh_ui(window, cx);
                            }))
                            .child(icon("close", 9.0, theme::text_tertiary())),
                    )
                    .into_any_element(),
            );
        }

        // 步进和滚轮都跟着总开关走：没开亮度时调它们没有意义。
        let mut brightness_rows = vec![toggle];
        if enabled {
            let mut rows = vec![merge_toggle];
            if merged {
                // 融合时亮度块排在音量面板里：朝向由音频面板定，宽度 / 高度也跟着音频走，
                // 只剩「贴哪一侧」还要自己选（竖排时顺带还能挑显示器名列头怎么写）。
                rows.push(merge_side_row);
                if audio_vertical {
                    rows.push(name_row);
                }
            } else {
                // 宽度只对横排有意义（竖排的宽度由显示器台数决定），
                // 显示器名只对竖排有意义 —— 各显示各的那一条，别摆一个改不动的选项在这儿。
                rows.push(layout_row);
                if config.brightness.panel_layout.is_vertical() {
                    rows.push(vertical_height_row);
                    rows.push(name_row);
                } else {
                    rows.push(width_row);
                }
                rows.push(placement_row);
            }
            // 亮度窗口里 + / − 按一下走多少：热键页的「跟随」就是跟它。
            rows.push(step);
            rows.push(scroll_toggle);
            rows.push(scroll_step);
            brightness_rows.push(self.sub_group(rows));
        }

        vec![
            self.section(
                lang.pick("显示器", "Displays"),
                "monitor",
                display_rows,
            ),
            self.section(
                lang.pick("亮度", "Brightness"),
                "sun",
                brightness_rows,
            ),
            self.section(
                lang.pick("预设", "Presets"),
                "settings",
                preset_rows,
            ),
            div()
                .px(px(9.))
                .text_size(theme::fs(10.5))
                .text_color(theme::text_tertiary())
                .child(lang.pick(
                    "亮度通过 DDC/CI 直接写给显示器；笔记本内屏或走转接头的显示器通常不支持。\n亮度面板里点数值可以手动填写，点「保存预设」把当前亮度存成预设。预设排在面板底部：左键套给光标所在的显示器，中键套给全部，拖到某台显示器的亮度条上则套给那台，右键删除（需确认）。\n在亮度条上滚动滚轮也能调：一格走多少个百分点可以自己填。",
                    "Brightness is written to the display over DDC/CI; laptop panels and adapters often do not support it.\nClick the value in the brightness panel to type a level, press Save preset to store it. Presets sit along the bottom of the panel: left-click applies one to the display under the cursor, middle-click applies it to every display, drag it onto a display's slider to apply it there, right-click to delete it (with confirmation).\nScrolling on a brightness slider works too: one notch moves a fixed step you can set yourself.",
                ))
                .into_any_element(),
        ]
    }

    fn about_page(&self, lang: Lang, cx: &mut Context<Self>) -> Vec<AnyElement> {
        let row = |title: &'static str, value: String| -> AnyElement {
            div()
                .flex()
                .items_center()
                .gap(px(10.))
                .px(px(9.))
                .py(px(5.))
                .child(
                    div()
                        .flex_none()
                        .text_size(theme::fs(12.5))
                        .text_color(theme::text_primary())
                        .child(title),
                )
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_size(theme::fs(11.))
                        .text_color(theme::text_tertiary())
                        .truncate()
                        .child(value),
                )
                .into_any_element()
        };

        vec![
            self.section(
                lang.pick("关于", "About"),
                "info",
                vec![
                    row(
                        lang.pick("版本", "Version"),
                        crate::version().to_string(),
                    ),
                    row(
                        lang.pick("配置文件", "Config file"),
                        self.config_path.to_string(),
                    ),
                    // 源码地址写在关于里：软件是绿色版、没有官网，别人想反馈或自己编译
                    // 只能靠这一串。点「打开」用默认浏览器跳过去。
                    self.action_row(
                        "project-home",
                        lang.pick("项目主页", "Project page"),
                        PROJECT_URL.to_string(),
                        lang.pick("打开", "Open"),
                        |_, _| crate::platform::shell::open_url(PROJECT_URL),
                    ),
                    self.action_row(
                        "project-issues",
                        lang.pick("问题反馈", "Report an issue"),
                        format!("{PROJECT_URL}/issues"),
                        lang.pick("打开", "Open"),
                        |_, _| crate::platform::shell::open_url(ISSUES_URL),
                    ),
                    // 捐赠打开的是窗口里的收款码浮层，不是外链 —— 收款码是张图，
                    // 得在这儿看。闭包拿不到 `self`，走弱引用回实体里翻开关。
                    self.action_row(
                        "donate",
                        lang.pick("捐赠", "Donate"),
                        lang.pick("微信 / 支付宝", "WeChat Pay / Alipay").to_string(),
                        lang.pick("查看", "Show"),
                        {
                            let this = cx.entity().downgrade();
                            move |window: &mut Window, cx: &mut App| {
                                let _ = this.update(cx, |this, cx| {
                                    this.donate_open = true;
                                    cx.notify();
                                });
                                window.refresh();
                            }
                        },
                    ),
                ],
            ),
            div()
                .px(px(9.))
                .text_size(theme::fs(10.5))
                .text_color(theme::text_tertiary())
                .child(lang.pick(
                    "独立的 WASAPI 音量混合器，配置随程序目录走，不写注册表。",
                    "A standalone WASAPI volume mixer. Portable config, nothing written to the registry.",
                ))
                .into_any_element(),
        ]
    }

    /// 捐赠浮层：整块背板压暗，中间一张收款码卡片。
    ///
    /// 图是 367×232 的横图（微信在左、支付宝在右），按原始像素摆出来最清楚 ——
    /// 缩放插值会让二维码边缘发虚，手机上不一定扫得出来。换图时这张常量要跟着改
    /// （尺寸由 `assets/donate.png` 决定）。
    fn donate_overlay(&self, lang: Lang, cx: &mut Context<Self>) -> Option<AnyElement> {
        const QR_W: f32 = 367.0;
        const QR_H: f32 = 232.0;

        if !self.donate_open {
            return None;
        }
        // 首帧解一次就缓存：这张图 51 KB，解码完接近 340 KB，没必要每次重画都解。
        if self.donate_image.borrow().is_none() {
            *self.donate_image.borrow_mut() = crate::ui::assets::donate_image();
        }
        let qr = self.donate_image.borrow().clone();

        let qr_element: AnyElement = match qr {
            Some(qr) => img(qr)
                .w(px(QR_W))
                .h(px(QR_H))
                .rounded(px(theme::RADIUS_SMALL))
                .into_any_element(),
            // 理论上不会走到（图是编进 exe 的），真解不出来也别留一块空白。
            None => div()
                .w(px(QR_W))
                .h(px(QR_H))
                .flex()
                .items_center()
                .justify_center()
                .rounded(px(theme::RADIUS_SMALL))
                .bg(theme::track())
                .text_size(theme::fs(11.))
                .text_color(theme::text_tertiary())
                .child(lang.pick("收款码加载失败", "Could not load the code"))
                .into_any_element(),
        };

        let card = div()
            .id("donate-card")
            .flex_none()
            .w(px(QR_W + 36.))
            .p(px(18.))
            .rounded(px(theme::RADIUS_CARD))
            .border_1()
            .border_color(theme::border())
            .bg(theme::surface_elevated())
            // 卡片上的按下不再往背板传，否则点图也会把浮层关掉。
            .occlude()
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|_, _: &gpui::MouseDownEvent, _, cx| cx.stop_propagation()),
            )
            .flex()
            .flex_col()
            .gap(px(12.))
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .child(
                        div()
                            .text_size(theme::fs(13.))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme::text_primary())
                            .child(lang.pick("捐赠", "Donate")),
                    )
                    .child(
                        div()
                            .id("donate-close")
                            .flex_none()
                            .size(px(20.))
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(theme::RADIUS_SMALL))
                            .cursor_pointer()
                            .hover(|style| style.bg(theme::surface_hover()))
                            // 浮层里的子元素用 `on_mouse_down`：`on_click` 在浮层里
                            // 经常收不到（面板的提示条也踩过这个）。
                            .on_mouse_down(
                                MouseButton::Left,
                                cx.listener(|this, _: &gpui::MouseDownEvent, window, cx| {
                                    this.donate_open = false;
                                    window.refresh();
                                    cx.notify();
                                }),
                            )
                            .child(icon("close", 12.0, theme::text_tertiary())),
                    ),
            )
            .child(qr_element)
            .child(
                div()
                    .text_size(theme::fs(10.5))
                    .text_color(theme::text_tertiary())
                    .child(lang.pick(
                        "微信或支付宝扫码支持，谢谢。",
                        "Scan with WeChat Pay or Alipay to support the project. Thank you.",
                    )),
            );

        Some(
            div()
                .id("donate-backdrop")
                .absolute()
                .left(px(0.))
                .top(px(0.))
                .size_full()
                // 滚轮 / 点击都别漏给后面的页面；点背板（不是卡片）就收起来。
                .occlude()
                .flex()
                .items_center()
                .justify_center()
                .bg(gpui::rgb_to_hsla(rgba(0x00000059)))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _: &gpui::MouseDownEvent, window, cx| {
                        // 模态：这一下只用来关浮层，不再传给底下被遮住的那一页
                        // （gpui 的鼠标派发是「所有 listener 都过一遍」，不主动拦就会
                        // 顺手把下面的按钮也点掉）。
                        cx.stop_propagation();
                        if this.donate_open {
                            this.donate_open = false;
                            window.refresh();
                            cx.notify();
                        }
                    }),
                )
                .child(card)
                .into_any_element(),
        )
    }

    /// 标题栏三横杠里那份菜单：恢复更改 / 恢复出厂设置，各自再分「本页 / 全部」。
    ///
    /// 四条都只改草稿（内存），要按底下的「应用 / 保存」才落盘。好处是点错了还能
    /// 撤：参照点一直在，「恢复更改」随时能把草稿退回上一次生效的那一份。
    fn restore_menu_overlay(
        &self,
        config: &Config,
        lang: Lang,
        cx: &mut Context<Self>,
    ) -> Option<AnyElement> {
        if !self.restore_menu_open {
            return None;
        }
        let base = self.config.borrow().draft_base().cloned();
        let page_changed = base
            .as_ref()
            .is_some_and(|base| Self::fields_differ(config, base, page_fields(self.page)));
        let all_changed = self.config.borrow().is_dirty();

        // 菜单宽度给一个够用的定值：中文 / 英文的字宽差一倍，量不出一个通用的。
        let width = 208.0_f32;
        // 三颗窗口按钮各 24、间距 8、右边距 14：菜单右边缘对齐三横杠的右边缘，
        // 于是它正好「挂」在那颗按钮下面，不会顶到屏幕外。
        let right = 14.0 + 24.0 * 3.0 + 8.0 * 3.0;

        let caption = |text: &'static str| {
            div()
                .px(px(10.))
                .pt(px(6.))
                .pb(px(2.))
                .text_size(theme::fs(10.))
                .text_color(theme::text_tertiary())
                .child(text)
        };

        Some(
            div()
                .id("restore-menu-backdrop")
                .absolute()
                .left(px(0.))
                .top(px(0.))
                .size_full()
                .occlude()
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _: &gpui::MouseDownEvent, window, cx| {
                        this.restore_menu_open = false;
                        window.refresh();
                        cx.notify();
                    }),
                )
                .child(
                    div()
                        .id("restore-menu-panel")
                        .absolute()
                        .right(px(right))
                        .top(px(44.))
                        .w(px(width))
                        .flex()
                        .flex_col()
                        .p(px(5.))
                        .rounded(px(theme::RADIUS_ROW))
                        .border_1()
                        .border_color(theme::border_strong())
                        .bg(theme::surface_elevated())
                        // 点在面板上不算「点空白处」，否则点一下就关了。
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|_, _: &gpui::MouseDownEvent, _, cx| {
                                cx.stop_propagation()
                            }),
                        )
                        .child(caption(lang.pick("恢复更改", "Revert changes")))
                        .child(self.restore_item(
                            "restore-page",
                            lang.pick("恢复本页更改", "Revert this page").to_string(),
                            page_changed,
                            Restore {
                                factory: false,
                                whole: false,
                            },
                            cx,
                        ))
                        .child(self.restore_item(
                            "restore-all",
                            lang.pick("恢复全部更改", "Revert all pages").to_string(),
                            all_changed,
                            Restore {
                                factory: false,
                                whole: true,
                            },
                            cx,
                        ))
                        .child(
                            div()
                                .h(px(1.))
                                .flex_none()
                                .my(px(4.))
                                .mx(px(6.))
                                .bg(theme::border()),
                        )
                        .child(caption(lang.pick("恢复出厂设置", "Factory reset")))
                        .child(self.restore_item(
                            "reset-page",
                            lang.pick("本页恢复出厂设置", "Reset this page").to_string(),
                            true,
                            Restore {
                                factory: true,
                                whole: false,
                            },
                            cx,
                        ))
                        .child(self.restore_item(
                            "reset-all",
                            lang.pick("全部恢复出厂设置", "Reset everything").to_string(),
                            true,
                            Restore {
                                factory: true,
                                whole: true,
                            },
                            cx,
                        )),
                )
                .into_any_element(),
        )
    }

    /// 菜单里的一行。`enabled` = 这一条现在有东西可撤（没改过就点不动）。
    fn restore_item(
        &self,
        id: &'static str,
        label: String,
        enabled: bool,
        what: Restore,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let color = if enabled {
            theme::text_primary()
        } else {
            theme::text_tertiary()
        };
        div()
            .id(SharedString::from(id))
            .px(px(10.))
            .py(px(6.))
            .rounded(px(theme::RADIUS_SMALL))
            .flex()
            .items_center()
            .text_size(theme::fs(12.))
            .text_color(color)
            .when(enabled, |style| {
                style
                    .cursor_pointer()
                    .hover(|style| style.bg(theme::surface_hover()))
            })
            // 浮层里的行一律挂在按下阶段：`on_click` 会被背板的 `occlude` 吃掉。
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(move |this, _: &gpui::MouseDownEvent, window, cx| {
                    cx.stop_propagation();
                    if !enabled {
                        return;
                    }
                    this.restore(what, window, cx);
                }),
            )
            .child(label)
            .into_any_element()
    }

    /// `a` 和 `b` 在 `keys` 这些字段上有没有差别（`keys` 空 = 整份比）。
    ///
    /// 只是给菜单决定「这一条能不能点」，顺手比一遍 JSON 就够 ——
    /// `Config` 没派生 `PartialEq`，为了这个去补一个也不值当。
    fn fields_differ(a: &Config, b: &Config, keys: &[&str]) -> bool {
        let (Ok(a), Ok(b)) = (serde_json::to_value(a), serde_json::to_value(b)) else {
            return true;
        };
        if keys.is_empty() {
            return a != b;
        }
        keys.iter().any(|key| a.get(*key) != b.get(*key))
    }

    /// 执行一条恢复：退回到上一次生效的配置，或者退回出厂默认；范围本页或全部。
    ///
    /// 只改草稿 —— 屏幕上立刻能看到结果（预览），但要按「应用 / 保存」才写进文件。
    fn restore(&mut self, what: Restore, window: &mut Window, cx: &mut Context<Self>) {
        let page = self.page;
        let source = if what.factory {
            Config::default()
        } else {
            match self.config.borrow().draft_base().cloned() {
                Some(base) => base,
                // 没在草稿模式里（理论上不会）：没什么可退的。
                None => return,
            }
        };
        let keys: &[&str] = if what.whole {
            &[]
        } else {
            page_fields(page)
        };
        let applied = self
            .config
            .borrow_mut()
            .edit_draft(&source, keys, &UNTOUCHED_FIELDS);

        self.restore_menu_open = false;
        if !applied {
            window.refresh();
            cx.notify();
            return;
        }
        self.close_transient_popups();
        let config = self.config.borrow().get().clone();
        let scope = if what.whole { None } else { Some(page) };
        // 预览不碰系统里的自启登记（那条删了再建、没提权就建不回来）。
        self.apply_everything(&config, scope, false, window, cx);
    }

    /// 页面底部那条「应用 / 保存」：草稿模式的出口。
    fn apply_bar(&self, lang: Lang, cx: &mut Context<Self>) -> AnyElement {
        let dirty = self.config.borrow().is_dirty();
        let hint = if dirty {
            lang.pick("有未保存的更改", "Unsaved changes")
        } else {
            ""
        };
        div()
            .h(px(40.))
            .flex_none()
            .flex()
            .items_center()
            .gap(px(8.))
            .px(px(14.))
            .border_t_1()
            .border_color(theme::border())
            .child(
                div()
                    .flex_1()
                    .text_size(theme::fs(10.5))
                    .text_color(theme::text_tertiary())
                    .child(hint),
            )
            .child(
                div()
                    .id("settings-apply")
                    .px(px(12.))
                    .py(px(4.))
                    .rounded(px(theme::RADIUS_SMALL))
                    .bg(theme::track())
                    .text_size(theme::fs(11.5))
                    .text_color(if dirty {
                        theme::text_secondary()
                    } else {
                        theme::text_tertiary()
                    })
                    .when(dirty, |style| {
                        style
                            .cursor_pointer()
                            .hover(|style| style.bg(theme::surface_hover()))
                    })
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        if this.config.borrow().is_dirty() {
                            this.save_draft(false, window, cx);
                        }
                    }))
                    .child(lang.pick("应用", "Apply")),
            )
            .child(
                div()
                    .id("settings-save")
                    .px(px(12.))
                    .py(px(4.))
                    .rounded(px(theme::RADIUS_SMALL))
                    .bg(if dirty {
                        theme::accent()
                    } else {
                        theme::track()
                    })
                    .text_size(theme::fs(11.5))
                    .text_color(if dirty {
                        theme::text_primary()
                    } else {
                        theme::text_tertiary()
                    })
                    .cursor_pointer()
                    .hover(|style| style.opacity(0.88))
                    .on_click(cx.listener(|this, _: &ClickEvent, window, cx| {
                        this.save_draft(true, window, cx)
                    }))
                    .child(lang.pick("保存", "Save")),
            )
            .into_any_element()
    }

    /// 「应用 / 保存」：草稿写进文件，再把各子系统推一遍。
    ///
    /// `close` = 保存完顺手关窗（「保存」按钮）；否则留在原页（「应用」）。
    fn save_draft(&mut self, close: bool, window: &mut Window, cx: &mut Context<Self>) {
        // 先把还挂在输入框里的东西落进草稿：gpui 是「最上面的元素先收到事件」，
        // 点在「保存」上时根节点那个「点别处就提交」还没轮到，不补这一下，
        // 最后一次输入（数值 / 改名 / 提示文字）会漏掉。
        self.blur_rename(window, cx);
        self.commit_osd_text(window, cx);
        self.commit_tooltip_edit(window, cx);
        self.commit_app_edit(window, cx);
        self.commit_edit(window, cx);

        // 自启那三项动过才去改系统登记：别的项（颜色、字体……）不该连带把任务
        // 删了重建一遍。
        let autostart_changed = {
            let store = self.config.borrow();
            store.draft_base().is_none_or(|base| {
                Self::fields_differ(
                    store.get(),
                    base,
                    &["launch_at_login", "launch_method", "run_as_admin"],
                )
            })
        };

        let _ = self.config.borrow_mut().commit_draft();
        let config = self.config.borrow().get().clone();
        self.apply_everything(&config, None, autostart_changed, window, cx);
        if close {
            self.close_window(window, cx);
        } else {
            window.refresh();
            cx.notify();
        }
    }

    /// 把一份配置推到各个子系统。
    ///
    /// 平时是「改一项顺手推一次」，这里是一次换掉一整页（或者整份），所以得推全：
    /// 否则托盘图标 / 快捷键 / 主题会停在旧值上，看着像没生效。
    /// `scope` 限定只在某一页动过时跳过有副作用的那几项（亮度会真去写显示器）。
    fn apply_everything(
        &mut self,
        config: &Config,
        scope: Option<Page>,
        sync_autostart: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let touches = |page: Page| scope.is_none_or(|only| only == page);
        crate::platform::hotkey::apply_config(config);
        crate::gui::apply_theme_extras(config);
        crate::gui::refresh_tray_icon(cx);
        crate::gui::refresh_tray_menu(cx);
        crate::gui::sync_tray_motion(cx);
        crate::gui::refresh_volume_cap(cx);
        // 只在面板开着时重排：这里是一次换掉一整页（甚至整份），按「调宽度」那条走
        // 会把面板预览出来 —— 点一下恢复菜单就蹦一块面板，用户不知道哪来的。
        crate::gui::refresh_flyout_size_if_visible(cx);
        crate::windows::platform::set_opacity(window, crate::ui::theme::ui_opacity());
        if touches(Page::Volume) {
            crate::gui::refresh_loudness(cx);
        }
        if touches(Page::Devices) {
            // 亮度：配置里那份是「上次写下的值」，刷新一次让显示器那边重新对表。
            crate::gui::refresh_displays(cx);
            crate::gui::refresh_brightness(cx);
        }
        if touches(Page::General) && sync_autostart {
            // 自启是系统里的登记，不是配置里一个布尔：开着就按当前这份重登记，
            // 关着就把两档一起撤干净（`set_enabled` 自己管互斥）。
            //
            // 只在这一项真变过的时候动系统：`set_enabled` 是先删再建，没提权时
            // 建不回去会顺手立一个「需要管理员」的招牌 —— 为了改个颜色就弹这条
            // 提示纯属误伤。「恢复」那种预览更是一次都不碰。
            crate::platform::autostart::set_enabled(
                config.launch_method,
                config.launch_at_login,
                config.run_as_admin,
            );
        }
        repaint_theme(window, cx);
    }

    /// 配置整块换掉之后，依附在具体控件上的弹层全收掉。
    ///
    /// 设备下拉、图标选择器、字体列表、色轮都是按「点在哪一行」定位的，
    /// 那一行可能刚被恢复成别的样子，留着就是个悬空浮层。
    fn close_transient_popups(&mut self) {
        self.picker = None;
        self.icon_picker = None;
        self.font_menu = None;
        self.font_pending_scroll = None;
        self.color_wheel = None;
        self.tray_menu_open = false;
        self.donate_open = false;
        self.capturing = None;
    }

    /// 点 × 的处理：有没保存的改动先问一句，没有就直接关。
    fn request_close(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.config.borrow().is_dirty() {
            self.confirm_close = true;
            window.refresh();
            cx.notify();
        } else {
            self.close_window(window, cx);
        }
    }

    /// 真关窗：草稿模式到此结束（没提交的改动丢掉），窗口自己会触发清理订阅。
    fn close_window(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.config.borrow_mut().discard_draft();
        window.remove_window();
        let _ = cx;
    }

    /// 「有未保存的更改」确认框：保存 / 不保存 / 取消。
    fn confirm_close_overlay(&self, lang: Lang, cx: &mut Context<Self>) -> Option<AnyElement> {
        if !self.confirm_close {
            return None;
        }
        let button = |id: &'static str,
                      label: &'static str,
                      answer: CloseAnswer,
                      cx: &mut Context<Self>| {
            let primary = answer == CloseAnswer::Save;
            let mut element = div()
                .id(SharedString::from(id))
                .px(px(12.))
                .py(px(4.))
                .rounded(px(theme::RADIUS_SMALL))
                .text_size(theme::fs(11.5))
                .cursor_pointer()
                .hover(|style| style.opacity(0.88));
            element = if primary {
                element.bg(theme::accent()).text_color(theme::text_primary())
            } else {
                element
                    .bg(theme::track())
                    .text_color(theme::text_secondary())
            };
            element
                .on_click(cx.listener(move |this, _: &ClickEvent, window, cx| {
                    match answer {
                        // 取消 = 什么都不做，接着改。
                        CloseAnswer::Cancel => {
                            this.confirm_close = false;
                            window.refresh();
                            cx.notify();
                        }
                        CloseAnswer::Save => {
                            this.confirm_close = false;
                            this.save_draft(true, window, cx);
                        }
                        CloseAnswer::Discard => {
                            this.confirm_close = false;
                            // 不保存 = 把草稿丢掉，界面上预览出去的东西也跟着弹回去。
                            if this.config.borrow_mut().discard_draft() {
                                let config = this.config.borrow().get().clone();
                                this.apply_everything(&config, None, false, window, cx);
                            }
                            window.remove_window();
                        }
                    }
                }))
                .child(label)
                .into_any_element()
        };

        Some(
            div()
                .id("close-confirm-backdrop")
                .absolute()
                .left(px(0.))
                .top(px(0.))
                .size_full()
                .occlude()
                .flex()
                .items_center()
                .justify_center()
                .bg(gpui::rgb_to_hsla(rgba(0x00000059)))
                .on_mouse_down(
                    MouseButton::Left,
                    cx.listener(|this, _: &gpui::MouseDownEvent, window, cx| {
                        // 点背板 = 取消（跟「取消」那颗一样），别把窗口关掉。
                        cx.stop_propagation();
                        this.confirm_close = false;
                        window.refresh();
                        cx.notify();
                    }),
                )
                .child(
                    div()
                        .w(px(324.))
                        .flex()
                        .flex_col()
                        .gap(px(10.))
                        .p(px(16.))
                        .rounded(px(theme::RADIUS_WINDOW))
                        .border_1()
                        .border_color(theme::border_strong())
                        .bg(theme::opaque(theme::surface_elevated()))
                        .on_mouse_down(
                            MouseButton::Left,
                            cx.listener(|_, _: &gpui::MouseDownEvent, _, cx| {
                                cx.stop_propagation()
                            }),
                        )
                        .child(
                            div()
                                .text_size(theme::fs(13.))
                                .font_weight(gpui::FontWeight::SEMIBOLD)
                                .text_color(theme::text_primary())
                                .child(lang.pick("有未保存的更改", "Unsaved changes")),
                        )
                        .child(
                            div()
                                .text_size(theme::fs(11.))
                                .text_color(theme::text_secondary())
                                .child(lang.pick(
                                    "关掉设置窗，这些改动就没了。",
                                    "Closing the settings window discards them.",
                                )),
                        )
                        .child(
                            div()
                                .flex()
                                .justify_end()
                                .gap(px(8.))
                                .child(button(
                                    "close-save",
                                    lang.pick("保存", "Save"),
                                    CloseAnswer::Save,
                                    cx,
                                ))
                                .child(button(
                                    "close-discard",
                                    lang.pick("不保存", "Don't save"),
                                    CloseAnswer::Discard,
                                    cx,
                                ))
                                .child(button(
                                    "close-cancel",
                                    lang.pick("取消", "Cancel"),
                                    CloseAnswer::Cancel,
                                    cx,
                                )),
                        ),
                )
                .into_any_element(),
        )
    }

    /// 当前页的内容：每页一个滚动区，滚动位置各自记住。
    fn page_view(
        &self,
        config: &Config,
        lang: Lang,
        snapshot: &Snapshot,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let scroll = self.scrolls[self.page.index()].clone();
        // 换页 / 换页签时内容淡入并轻轻上浮一点。开关在「外观 → 界面 → 界面动效」，
        // 关掉就一帧到位，不做任何过渡。
        let motion = self.config.borrow().get().ui_motion && !cx.reduce_motion();
        let anim_id = format!("settings-page-in-{}", self.page_anim);
        let sections = match self.page {
            Page::General => self.general_page(config, lang, snapshot, cx),
            Page::Devices => self.devices_page(config, lang, snapshot, cx),
            Page::Appearance => self.appearance_page(config, lang, snapshot, cx),
            Page::Volume => self.volume_page(config, lang, snapshot, cx),
            Page::Panel => self.panel_page(config, lang, snapshot, cx),
            Page::Tray => self.tray_page(config, lang, snapshot, cx),
            Page::Shortcuts => self.shortcuts_page(config, lang, cx),
            Page::Osd => self.osd_page(config, lang, cx),
            Page::Blocklist => self.blocklist_page(config, lang, cx),
            Page::About => self.about_page(lang, cx),
        };
        let body = div()
            .id("settings-page")
            .flex_1()
            .min_w_0()
            .min_h_0()
            .relative()
            .flex()
            .child(
                div()
                    .id(SharedString::from(format!(
                        "settings-page-scroll-{}",
                        self.page.id()
                    )))
                    .flex_1()
                    .min_w_0()
                    .min_h_0()
                    .overflow_y_scroll()
                    .track_scroll(&scroll)
                    .on_scroll_wheel(cx.listener(|_, _, _, cx| cx.notify()))
                    .p(px(12.))
                    .flex()
                    .flex_col()
                    .gap(px(10.))
                    .children(sections),
            )
            // 滚动条单独占一条槽：内容在它左边结束，怎么滚都不会压上来。
            .child(
                div()
                    .w(px(SCROLLBAR_GUTTER))
                    .h_full()
                    .flex_none()
                    .flex()
                    .justify_center()
                    .child(self.scrollbar_column(&scroll, 1, self.font_menu.is_none())),
            );
        if !motion {
            return body.into_any_element();
        }
        // 200ms、三次缓出（`1-(1-t)^3`）：起手有点速度、结尾慢慢收住，
        // 整段时间里都看得见变化。gpui 自带的 quint 太前倾 —— 190ms 的动画
        // 85ms 就走完 95%，肉眼基本只剩「一下跳过去」。
        //
        // 位移用 `relative().top()`（只挪画上去的位置，不参与布局），
        // 淡入过程中不会反复折算滚动区高度，不会引发布局抖一下。
        body.with_animation(
            ElementId::Name(anim_id.into()),
            Animation::new(std::time::Duration::from_millis(200))
                .with_easing(|t: f32| 1.0 - (1.0 - t).powi(3)),
            move |element, delta| {
                element
                    .opacity(delta)
                    .relative()
                    .top(px(10.0 * (1.0 - delta)))
            },
        )
        .into_any_element()
    }
}

impl Render for Settings {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.remember_size(window, cx);
        let config = self.config.borrow().get().clone();
        let lang = config.language;
        let snapshot = self.snapshot.borrow().clone();
        let viewport = window.viewport_size();

        // 图标切回「喇叭」时数字字体那一整段会收起来，弹出层也得跟着关，
        // 否则会留一个悬空的下拉框。
        if !matches!(config.tray_icon_style, TrayIconStyle::Number)
            && matches!(self.font_menu, Some((FontTarget::TrayNumber, _, _)))
        {
            self.font_menu = None;
            self.font_pending_scroll = None;
            self.font_filter.clear();
        }
        // 设备图标选择器长在设备行上，离开这一页就没地方可依附了。
        if self.page != Page::Devices {
            self.icon_picker = None;
        }
        // 捐赠浮层开在「关于」页，翻到别页还盖着就成了遮面板的一块黑。
        if self.page != Page::About {
            self.donate_open = false;
        }
        // 设备下拉的两行都挂在开关下面：开关一关，行没了，下拉也得收。
        if let Some((which, _, _)) = self.picker {
            let alive = match which {
                DevicePicker::StartupDefault => config.startup.set_default_device,
                DevicePicker::BoundDevice => {
                    config.volume_cap.enabled && !config.volume_cap.device_id.is_empty()
                }
                DevicePicker::AddDeviceState => config.startup.device_state_mode.uses_list(),
                DevicePicker::AddAppVolume => config.startup.set_app_volumes,
                // 选了「手动选择」那一档这一行才存在。
                DevicePicker::OsdMonitor => {
                    config.osd.monitor_mode == crate::config::OsdMonitorMode::Single
                        && config.osd.monitor != 0
                }
            };
            if !alive {
                self.picker = None;
            }
        }

        // 滚动条的尺寸得等布局跑完才有（`bounds` 首帧是空的）。
        // 只要还没拿到就补一帧；拿到之后就不补了，不会一直重绘。
        if self.scrolls[self.page.index()]
            .bounds()
            .size
            .height
            .as_f32()
            <= 1.0
            || (self.font_menu.is_some() && self.font_scroll.bounds().size.height.as_f32() <= 1.0)
        {
            let this = cx.entity().downgrade();
            window.on_next_frame(move |_, cx| {
                let _ = this.update(cx, |_, cx| cx.notify());
            });
        }

        let sidebar = self.sidebar(lang, cx);
        let page = self.page_view(&config, lang, &snapshot, cx);
        let font_menu = self.font_menu_overlay(&config, lang, viewport, cx);
        let color_wheel = self.color_wheel_overlay(lang, viewport, cx);
        let device_menu = self.device_menu_overlay(&config, lang, &snapshot, viewport, cx);
        let icon_picker = self.icon_picker_overlay(&config, lang, viewport, cx);
        let tray_menu = self.tray_menu_overlay(&config, lang, viewport, cx);
        let restore_menu = self.restore_menu_overlay(&config, lang, cx);
        let confirm_close = self.confirm_close_overlay(lang, cx);
        let apply_bar = self.apply_bar(lang, cx);
        let donate = self.donate_overlay(lang, cx);

        div()
            .size_full()
            .relative()
            .flex()
            .flex_col()
            // 用户自选的界面字体：挂在根节点上，整棵树里没指定字体的文字都跟着变
            // （gpui 的文本样式是往下继承的）。空串 = 跟随系统，这时这一条不生效。
            .when_some(theme::ui_font(), |style, family| style.font_family(family))
            // 界面字重同理：常规（400）时不写这一条，省掉一层对每个文本的覆盖。
            .when_some(theme::ui_font_weight(), |style, weight| {
                style.font_weight(weight)
            })
            // 这扇窗的根底色要不透明：主题的 `surface` 留了 3% 透光，
            // 不透明度给到 100% 时还会隐隐透出桌面。窗口级透明由 `ui_opacity` 管。
            .bg(theme::surface_window())
            // 录快捷键时键盘事件只在有焦点的元素上派发，根节点就是那个焦点宿主；
            // 点别处（鼠标按下冒泡到这里）则视为放弃这一轮录制。
            .track_focus(&self.focus)
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, window, cx| {
                // Ctrl+Z：撤销当前输入框的上一步（每个输入框自己一份栈）。
                // 放在最前面 —— 不然会被下面各输入框的「接在末尾」逻辑当成普通字符。
                // 多行模板编辑器的撤销在它自己的按键处理里（按行记）。
                if event.keystroke.key == "z"
                    && event.keystroke.modifiers.control
                    && !event.keystroke.modifiers.shift
                    && this.undo_text_edit(cx)
                {
                    cx.stop_propagation();
                    return;
                }
                // 字体弹层的搜索框最先：弹层开着时键盘就是用来筛字体的。
                if this.handle_font_filter_key(event, window, cx) {
                    return;
                }
                // 设备改名 / 子菜单改名最优先：这时候按键都是名字的一部分。
                if this.handle_device_rename_key(event, window, cx) {
                    return;
                }
                if this.handle_tray_rename_key(event, window, cx) {
                    return;
                }
                if this.handle_osd_text_key(event, window, cx) {
                    return;
                }
                // 黑名单的手动输入框：回车把这一行加进名单。
                if this.handle_block_name_key(event, window, cx) {
                    return;
                }
                // 模板编辑器优先：回车是换行，不是确认。
                if this.handle_tooltip_key(event, window, cx) {
                    return;
                }
                // 应用音量名单里的就地输入（数字）。
                if this.handle_app_edit_key(event, window, cx) {
                    return;
                }
                // 色号输入框：这时候按键是十六进制字符，不是快捷键。
                if this.handle_color_key(event, window, cx) {
                    return;
                }
                // 手动填数值优先：这时候按键是数字，不是快捷键。
                if this.handle_edit_key(event, window, cx) {
                    return;
                }
                this.handle_capture_key(event, window, cx);
            }))
            // 色轮 / 色相条的拖拽**不在这里**：它挂在 `ui::picker` 的窗口级监听上，
            // 根节点这条会被子控件的 stop_propagation 截断，光标移出方块也会断。
            // 改名失焦落盘挂在**捕获相位**上。
            //
            // 冒泡相位靠不住：子控件只要 `stop_propagation()`（页签、开关、下拉、
            // 滚动条、**别的设备行 / 显示器行**都这么干），这一下就传不到根节点，
            // 于是「点某些地方名字就没存上」—— 尤其常见的是改完 A 的名字直接去点
            // B，A 那格被替换成新的空草稿，敲的字全丢。
            // 捕获相位从根往下走，谁都拦不住。点在输入框自己身上不算失焦。
            .capture_any_mouse_down(cx.listener(
                |this, event: &gpui::MouseDownEvent, window, cx| {
                    if this.device_rename.is_none() && this.display_rename.is_none() {
                        return;
                    }
                    // `rename_bounds` 由输入框的测量画布每帧写回（窗口坐标，
                    // 画布是 `size_full` 的，所以就是整格输入区的矩形）。
                    if let Some(bounds) = this.rename_bounds.get() {
                        if bounds.contains(&event.position) {
                            return;
                        }
                    }
                    this.blur_rename(window, cx);
                },
            ))
            .on_mouse_down(
                gpui::MouseButton::Left,
                cx.listener(|this, _: &gpui::MouseDownEvent, window, cx| {
                    // 点别处：数值照单全收（当作确认），快捷键录制直接放弃。
                    // 改名的落盘在捕获相位已经做过一次（见上面的 `capture_any_mouse_down`），
                    // 这里再兜一次 —— 走冒泡时它多半已经被子控件吞掉了，兜不住就算了。
                    this.blur_rename(window, cx);
                    this.commit_osd_text(window, cx);
                    // 黑名单那个框例外：点别处只交回键盘，草稿留着，
                    // 要真加进名单得点「添加」或回车。
                    this.blur_block_name(cx);
                    this.commit_tooltip_edit(window, cx);
                    this.commit_app_edit(window, cx);
                    this.commit_edit(window, cx);
                    this.cancel_capture(window, cx);
                }),
            )
            .child(
                div()
                    .h(px(42.))
                    .px(px(14.))
                    .flex_none()
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .border_b_1()
                    .border_color(theme::border())
                    .child(
                        // 窗口是无边框自绘的，标题区域自己接管拖动（不含关闭按钮）。
                        //
                        // 拖动**等鼠标真动起来才开始**：按下就走系统的标题栏拖动环，
                        // 那个模态环会把双击的第二下吞掉 —— 双击永远收不到 `click_count == 2`。
                        div()
                            .id("settings-titlebar")
                            .flex_1()
                            .h_full()
                            .flex()
                            .items_center()
                            .gap(px(8.))
                            .on_mouse_down(
                                gpui::MouseButton::Left,
                                cx.listener(|this, event: &gpui::MouseDownEvent, window, _| {
                                    if event.click_count >= 2 {
                                        // 双击标题栏 = 最大化 / 还原，跟系统标题栏一个手感。
                                        this.titlebar_armed = false;
                                        window.zoom_window();
                                    } else {
                                        this.titlebar_armed = true;
                                    }
                                }),
                            )
                            .on_mouse_move(cx.listener(
                                |this, event: &gpui::MouseMoveEvent, window, _| {
                                    if this.titlebar_armed
                                        && event.pressed_button == Some(gpui::MouseButton::Left)
                                    {
                                        this.titlebar_armed = false;
                                        window.start_window_move();
                                    }
                                },
                            ))
                            .on_mouse_up(
                                gpui::MouseButton::Left,
                                cx.listener(|this, _, _, _| this.titlebar_armed = false),
                            )
                            .child(icon("settings", 15.0, theme::text_secondary()))
                            .child(
                                div()
                                    .text_size(theme::fs(13.))
                                    .font_weight(gpui::FontWeight::SEMIBOLD)
                                    .text_color(theme::text_primary())
                                    .child(lang.pick("Bolume 设置", "Bolume Settings")),
                            ),
                    )
                    .child(
                        // 三横杠：恢复更改 / 恢复出厂设置。所有页都在同一个位置
                        // （三颗窗口按钮左边），菜单里的「本页」跟着当前页走。
                        div()
                            .id("settings-restore-menu")
                            .size(px(24.))
                            .flex_none()
                            .flex()
                            .items_center()
                            .justify_center()
                            .rounded(px(theme::RADIUS_SMALL))
                            .cursor_pointer()
                            .when(self.restore_menu_open, |style| {
                                style.bg(theme::accent_soft())
                            })
                            .hover(|style| style.bg(theme::surface_hover()))
                            .on_click(cx.listener(
                                |this, _: &ClickEvent, window, cx| {
                                    this.restore_menu_open = !this.restore_menu_open;
                                    window.refresh();
                                    cx.notify();
                                },
                            ))
                            .child(icon("menu", 14.0, theme::text_secondary())),
                    )
                    .child(
                        // 窗口按钮：最小化 / 最大化-还原 / 关闭。
                        // 窗口是 `WindowDecorations::Client`（系统标题栏被去掉），
                        // 这三颗都得自己画、自己接。
                        titlebar_button("settings-min", "minimize")
                            .on_click(|_, window, _| window.minimize_window()),
                    )
                    .child(
                        titlebar_button(
                            "settings-max",
                            if window.is_maximized() {
                                "restore"
                            } else {
                                "maximize"
                            },
                        )
                        .on_click(|_, window, _| window.zoom_window()),
                    )
                    .child(
                        titlebar_button("settings-close", "close")
                            .on_click(cx.listener(
                                |this, _: &ClickEvent, window, cx| this.request_close(window, cx),
                            )),
                    ),
            )
            .child(
                div()
                    .flex()
                    .flex_1()
                    .min_h_0()
                    .child(sidebar)
                    // 右半边是「页面 + 底部应用栏」：栏只横跨内容区，
                    // 侧栏那条从标题栏一直通到底，不被切开。
                    .child(
                        div()
                            .flex()
                            .flex_col()
                            .flex_1()
                            .min_w_0()
                            .min_h_0()
                            .child(page)
                            .child(apply_bar),
                    ),
            )
            // 弹出层永远在最后：盖在侧栏和页面上，但不改窗口尺寸。
            .children(font_menu)
            .children(device_menu)
            .children(icon_picker)
            .children(tray_menu)
            // 色轮最后挂：它就是当前焦点，别的弹层都该被它盖住。
            .children(color_wheel)
            // 恢复菜单挂在模态之下：捐赠浮层开着时不该还能点它。
            .children(restore_menu)
            // 捐赠浮层是模态，压在所有弹层之上。
            .children(donate)
            // 「有未保存的更改」是关窗前的最后一问，压在最上面。
            .children(confirm_close)
    }
}

/// 标题栏上的一颗窗口按钮（最小化 / 最大化 / 关闭）。
///
/// 三颗长得一样，只有图标和动作不同：24×24 的方形热区、悬停浅底，
/// 和面板里的其它小按钮同一套手感。
fn titlebar_button(id: &'static str, name: &'static str) -> gpui::Stateful<gpui::Div> {
    div()
        .id(SharedString::from(id))
        .size(px(24.))
        .flex_none()
        .flex()
        .items_center()
        .justify_center()
        .rounded(px(theme::RADIUS_SMALL))
        .cursor_pointer()
        .hover(|style| style.bg(theme::surface_hover()))
        .child(icon(name, 14.0, theme::text_secondary()))
}

/// 子菜单名字最多几个字。
const MAX_NAME_CHARS: usize = 24;
/// 设备名字最多几个字（系统原名经常就很长，给宽一点）。
const MAX_DEVICE_NAME_CHARS: usize = 48;

/// 按 UTF-16 计的长度（输入法 / 平台层用的都是这个单位）。
fn utf16_len(text: &str) -> usize {
    text.encode_utf16().count()
}

/// 按 UTF-16 下标切一段字符串（越界自动夹紧）。
fn utf16_slice(text: &str, range: std::ops::Range<usize>) -> String {
    let mut out = String::new();
    let mut index = 0usize;
    for ch in text.chars() {
        let len = ch.len_utf16();
        if index >= range.start && index + len <= range.end {
            out.push(ch);
        }
        index += len;
    }
    out
}

/// 一段文字在给定字号下有多宽（逻辑像素）。
fn text_width(text: &str, size: f32, window: &mut Window) -> f32 {
    if text.is_empty() {
        return 0.0;
    }
    let font = window.text_style().font();
    let run = gpui::TextRun {
        len: text.len(),
        font,
        color: gpui::black(),
        background_color: None,
        underline: None,
        strikethrough: None,
        letter_spacing: None,
    };
    window
        .text_system()
        .layout_line(text, px(size), &[run], None)
        .width
        .as_f32()
}

/// 在光标处插入一段文字，返回「新串 + 新光标」（超长按 `max_chars` 截断）。
/// 这一轮改名要不要真写进配置。
///
/// `explicit` = 用户按了回车：空内容算「恢复系统原名」，这是目前唯一能删掉别名的入口。
/// 否则（点别处 / 失焦）：只有真敲了字才写 —— 空着说明用户什么都没表达，
/// 不能因为「点了一下别的地方」就把已有别名抹掉。
fn rename_applies(name: &str, explicit: bool) -> bool {
    explicit || !name.trim().is_empty()
}

fn rename_insert_at(text: &str, cursor: usize, insert: &str, max_chars: usize) -> (String, usize) {
    let len = utf16_len(text);
    let at = cursor.min(len);
    let prefix = utf16_slice(text, 0..at);
    let suffix = utf16_slice(text, at..len);
    let room = max_chars.saturating_sub(prefix.chars().count() + suffix.chars().count());
    let inserted: String = insert
        .chars()
        .filter(|ch| !ch.is_control())
        .take(room)
        .collect();
    let inserted_len = utf16_len(&inserted);
    (format!("{prefix}{inserted}{suffix}"), at + inserted_len)
}

/// 按光标删一个字：`forward` 真 = 删光标后面（Delete），假 = 删前面（退格）。
///
/// 到头了返回 `None`（调用方什么都不用做）。
fn rename_delete_at(text: &str, cursor: usize, forward: bool) -> Option<(String, usize)> {
    let len = utf16_len(text);
    let at = cursor.min(len);
    let (start, end) = if forward {
        if at >= len {
            return None;
        }
        (at, at + next_char_len(text, at))
    } else {
        if at == 0 {
            return None;
        }
        (at - prev_char_len(text, at), at)
    };
    let next = format!(
        "{}{}",
        utf16_slice(text, 0..start),
        utf16_slice(text, end..len)
    );
    Some((next, start))
}

/// 第 `at` 个 UTF-16 位置上那个字占几位（代理对 = 2）。
fn next_char_len(text: &str, at: usize) -> usize {
    let mut index = 0;
    for ch in text.chars() {
        if index >= at {
            return ch.len_utf16();
        }
        index += ch.len_utf16();
    }
    0
}

/// 紧挨在 `at` 前面那个字占几位（退格 / 左移用）。
fn prev_char_len(text: &str, at: usize) -> usize {
    let mut index = 0;
    for ch in text.chars() {
        if index + ch.len_utf16() == at {
            return ch.len_utf16();
        }
        index += ch.len_utf16();
    }
    at.min(1)
}

/// 点击位置（相对文字左端）落在第几个 UTF-16 下标上。
/// 取「离得最近的那个字边界」：点在字的前半格就落到它前面，后半格落到它后面。
fn index_at_x(text: &str, x: f32, size: f32, window: &mut Window) -> usize {
    if x <= 0.0 {
        return 0;
    }
    let mut width = 0.0f32;
    let mut index = 0usize;
    for ch in text.chars() {
        let mut buffer = [0u8; 4];
        let width_of_char = text_width(ch.encode_utf8(&mut buffer), size, window);
        if x < width + width_of_char / 2.0 {
            return index;
        }
        width += width_of_char;
        index += ch.len_utf16();
    }
    index
}

/// 重命名输入框的输入法通道。
///
/// 这套界面是自绘的，`on_key_down` 只能拿到按键（`ToUnicode` 对输入法拼出来的字返回空），
/// 所以中文必须走平台层的 `PlatformInputHandler`：绘制时在名字那一格挂一个
/// `ElementInputHandler`，Windows 的 `WM_IME_COMPOSITION` / `WM_CHAR` 就会落到这里。
///
/// 目标只有一个：`tray_rename` 里那个字符串。光标永远在末尾，没有选区，
/// 正在拼的那一段记在 `text_marked` 里 —— 输入法每更新一次候选就从
/// 那个范围整体替换，提交时再换成最终文字。
impl gpui::EntityInputHandler for Settings {
    fn text_for_range(
        &mut self,
        range: std::ops::Range<usize>,
        adjusted_range: &mut Option<std::ops::Range<usize>>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<String> {
        let buffer = self.text_edit()?;
        let len = utf16_len(buffer);
        let start = range.start.min(len);
        let end = range.end.min(len).max(start);
        *adjusted_range = Some(start..end);
        Some(utf16_slice(buffer, start..end))
    }

    fn selected_text_range(
        &mut self,
        _ignore_disabled_input: bool,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<gpui::UTF16Selection> {
        let buffer = self.text_edit()?;
        let len = utf16_len(buffer);
        // 设备改名有光标：告诉输入法「就在这一格插入」，中文候选才会落对地方；
        // 其余几个框仍按末尾处理。
        let pos = if self.caret_rename() {
            self.rename_cursor.min(len)
        } else {
            len
        };
        Some(gpui::UTF16Selection {
            range: pos..pos,
            reversed: false,
        })
    }

    fn marked_text_range(
        &self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<std::ops::Range<usize>> {
        self.text_marked.clone()
    }

    fn unmark_text(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        if self.text_marked.take().is_some() {
            cx.notify();
        }
    }

    fn replace_text_in_range(
        &mut self,
        range: Option<std::ops::Range<usize>>,
        text: &str,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.replace_rename_range(range, text, false, cx);
    }

    fn replace_and_mark_text_in_range(
        &mut self,
        range: Option<std::ops::Range<usize>>,
        new_text: &str,
        _new_selected_range: Option<std::ops::Range<usize>>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.replace_rename_range(range, new_text, true, cx);
    }

    fn bounds_for_range(
        &mut self,
        _range_utf16: std::ops::Range<usize>,
        element_bounds: gpui::Bounds<gpui::Pixels>,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<gpui::Bounds<gpui::Pixels>> {
        Some(element_bounds)
    }

    fn character_index_for_point(
        &mut self,
        point: gpui::Point<gpui::Pixels>,
        window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        if !self.caret_rename() {
            return None;
        }
        let buffer = self.text_edit()?.to_string();
        let bounds = self.rename_bounds.get()?;
        let x = point.x.as_f32() - bounds.origin.x.as_f32();
        Some(index_at_x(&buffer, x, theme::fs(12.5).as_f32(), window))
    }

    fn text_length_utf16(
        &mut self,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) -> Option<usize> {
        self.text_edit().map(utf16_len)
    }

    fn accepts_text_input(&self, _window: &mut Window, _cx: &mut Context<Self>) -> bool {
        // 只有真的在编辑某个名字时才接管文字输入（其余时候让系统关掉输入法）。
        self.text_edit().is_some()
    }
}

/// 把一段文字换进重命名缓冲，返回新内容与「正在拼的候选字」范围。
///
/// `range` 为空时优先用 `marked`（输入法上一次的候选字范围），再退到末尾；
/// 这样输入法每更新一次候选都是整体替换，不会把拼音字母一个个堆进去。
fn rename_apply(
    current: &str,
    marked: Option<std::ops::Range<usize>>,
    range: Option<std::ops::Range<usize>>,
    text: &str,
    mark: bool,
    max_chars: usize,
) -> (String, Option<std::ops::Range<usize>>) {
    let len = utf16_len(current);
    let range = range.or(marked).unwrap_or(len..len);
    let start = range.start.min(len);
    let end = range.end.min(len).max(start);

    let prefix = utf16_slice(current, 0..start);
    let suffix = utf16_slice(current, end..len);
    let text: String = text.chars().filter(|c| !c.is_control()).collect();
    let room = max_chars.saturating_sub(prefix.chars().count() + suffix.chars().count());
    let inserted: String = text.chars().take(room).collect();
    let inserted_len = utf16_len(&inserted);

    let mut next = prefix;
    next.push_str(&inserted);
    next.push_str(&suffix);
    (next, mark.then_some(start..start + inserted_len))
}

impl Settings {
    /// 把一段文字换进重命名缓冲：`mark` 为真表示这是输入法还没提交的候选字。
    fn replace_rename_range(
        &mut self,
        range: Option<std::ops::Range<usize>>,
        text: &str,
        mark: bool,
        cx: &mut Context<Self>,
    ) {
        // 先复制一份：下面要一边写回缓冲、一边更新光标，借用不能纠缠。
        let Some(current) = self.text_edit().map(|text| text.to_string()) else {
            return;
        };
        let max = if self.font_menu.is_some() {
            24
        } else if self.caret_rename() {
            MAX_DEVICE_NAME_CHARS
        } else if self.osd_text_edit.is_some() {
            // OSD 一行最多几十个字，给 32 够放一整句话了。
            32
        } else if self.block_name_edit.is_some() {
            260
        } else {
            MAX_NAME_CHARS
        };
        let device = self.caret_rename();
        let len = utf16_len(&current);
        let at = self.rename_cursor.min(len);
        // 设备改名：没在拼字时插入点就是光标那一格（中文候选也落在这里）。
        let range = if device && self.text_marked.is_none() {
            Some(at..at)
        } else {
            range
        };
        let span = range
            .clone()
            .or(self.text_marked.clone())
            .unwrap_or(len..len);
        let start = span.start.min(len);
        let end = span.end.min(len).max(start);
        let (next, marked) =
            rename_apply(&current, self.text_marked.clone(), range, text, mark, max);
        if device {
            // 插入完光标跟到新字后面。
            let inserted = utf16_len(&next).saturating_sub(len - (end - start));
            self.rename_cursor = start + inserted;
        }
        if let Some(buffer) = self.text_edit_mut() {
            *buffer = next;
        }
        self.text_marked = marked;
        cx.notify();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 每页那份字段清单要把 `Config` 的字段**分完**，且一页只收一次。
    ///
    /// 漏一个，那一页「恢复本页」就会漏掉那一项（点了看着像没反应）；
    /// 两页都收，重置别的页时会顺手把这页也改掉。加字段时这里会直接拦下来。
    #[test]
    fn page_fields_cover_every_config_key() {
        let value = serde_json::to_value(Config::default()).unwrap();
        let object = value.as_object().unwrap();
        for key in object.keys() {
            let owners = Page::ALL
                .iter()
                .filter(|page| page_fields(**page).contains(&key.as_str()))
                .count();
            if UNTOUCHED_FIELDS.contains(&key.as_str()) {
                assert_eq!(owners, 0, "{key} 是不该被重置的字段");
            } else {
                assert_eq!(owners, 1, "{key} 正好要被一页收着，现在是 {owners} 页");
            }
        }
        // 反过来：清单里不该有 `Config` 上不存在的名字（拼错 = 那一条永远不生效）。
        for page in Page::ALL {
            for key in page_fields(page) {
                assert!(
                    object.contains_key(*key),
                    "{page:?} 的 {key} 不是 Config 的字段"
                );
            }
        }
    }

    /// 失焦落盘的规则：敲了字就一定写；空着只有按回车才算「恢复原名」。
    #[test]
    fn blur_with_empty_buffer_keeps_alias() {
        // 空着 + 点别处 → 什么都不动（以前这里会把已有别名删掉）。
        assert!(!rename_applies("", false));
        assert!(!rename_applies("   ", false));
        // 空着 + 回车 → 恢复系统原名（删别名的唯一入口）。
        assert!(rename_applies("", true));
        // 有字：怎么结束都写。
        assert!(rename_applies("客厅屏", false));
        assert!(rename_applies("客厅屏", true));
    }

    /// 设备改名：插入落在光标那一格，而不是永远追加到末尾。
    #[test]
    fn device_rename_inserts_at_cursor() {
        let (next, cursor) = rename_insert_at("桌面音箱", 2, "大", 32);
        assert_eq!(next, "桌面大音箱");
        assert_eq!(cursor, 3);
        // 光标在头 / 尾都要成立。
        let (next, cursor) = rename_insert_at("abc", 0, "X", 32);
        assert_eq!((next.as_str(), cursor), ("Xabc", 1));
        let (next, cursor) = rename_insert_at("abc", 3, "X", 32);
        assert_eq!((next.as_str(), cursor), ("abcX", 4));
    }

    /// 退格删光标前一个字，Delete 删后一个字；到头了什么都不做。
    #[test]
    fn device_rename_deletes_both_directions() {
        let (next, cursor) = rename_delete_at("abcdef", 3, false).unwrap();
        assert_eq!((next.as_str(), cursor), ("abdef", 2));
        let (next, cursor) = rename_delete_at("abcdef", 3, true).unwrap();
        assert_eq!((next.as_str(), cursor), ("abcef", 3));
        assert!(rename_delete_at("abc", 0, false).is_none());
        assert!(rename_delete_at("abc", 3, true).is_none());
        // 中文按字删，不会删半个。
        let (next, cursor) = rename_delete_at("桌面音箱", 2, false).unwrap();
        assert_eq!((next.as_str(), cursor), ("桌音箱", 1));
    }

    /// 超长按上限截断，光标停在真正插入的位置。
    #[test]
    fn device_rename_respects_max_chars() {
        let (next, cursor) = rename_insert_at("ab", 2, "xxxx", 4);
        assert_eq!((next.as_str(), cursor), ("abxx", 4));
    }

    /// dB 步进在配置里存的是浮点，在数字框里是「十分之一 dB」的整数，
    /// 来回换一次不能掉精度，也不能把 0.1 变成 0。
    #[test]
    fn log_wheel_step_round_trips_through_tenths() {
        let mut config = Config::default();
        let field = NumberField::LogWheelStep;
        assert_eq!(field.value(&config), 20, "默认 2.0 dB 应该读成 20");

        let stored = field.value(&config);
        field.apply(&mut config, stored);
        assert!((config.log_scroll_wheel_step - 2.0).abs() < 1e-6);

        // 最低 0.1 dB（= 1 个十分之一）。
        field.apply(&mut config, 1);
        assert!((config.log_scroll_wheel_step - 0.1).abs() < 1e-6);
        assert_eq!(field.display(1, Lang::Zh), "0.1 dB");
        assert_eq!(field.display(200, Lang::Zh), "20.0 dB");
    }

    /// 数字框的下限就是这个字段能到的最细步进，不能被 UI 放过更小的值。
    #[test]
    fn log_wheel_step_range_starts_at_one_tenth() {
        let (low, high) = NumberField::LogWheelStep.range();
        assert_eq!(low, 1);
        assert_eq!(high, 200);
        // 按一下就是 0.1 dB，按住不会再往 0 跑。
        assert_eq!(NumberField::LogWheelStep.stepped(1, -1), 1);
        assert_eq!(NumberField::LogWheelStep.stepped(20, 1), 21);
    }

    /// 手动输入：`0.5` → 5（十分之一 dB），`2` → 20，乱输保持原值。
    #[test]
    fn log_wheel_step_parses_decimal_typing() {
        let field = NumberField::LogWheelStep;
        assert_eq!(field.parse_edit("0.5", 20), 5);
        assert_eq!(field.parse_edit("2", 20), 20);
        assert_eq!(field.parse_edit("20.0", 20), 200);
        assert_eq!(field.parse_edit("", 20), 20);
        assert_eq!(field.parse_edit("abc", 20), 20);
        // 百分比字段还是只认整数。
        assert_eq!(NumberField::WheelStep.parse_edit("0.5", 2), 2);
        assert_eq!(NumberField::WheelStep.parse_edit("7", 2), 7);
    }

    /// 设置页那三张缩略预览各按自己那套的样例画：
    /// 音量报百分比 + 分贝并画条，媒体键只报动作，亮度只报百分比。
    #[test]
    fn preview_samples_match_each_template() {
        let cfg = OsdConfig::default();

        let volume = preview_osd_state(Lang::Zh, &cfg, OsdFunction::Volume);
        assert!(volume.show_bar);
        assert!(!volume.percent.is_empty());
        assert!(!volume.db.is_empty());

        let media = preview_osd_state(Lang::Zh, &cfg, OsdFunction::PlayPause);
        assert!(!media.show_bar);
        assert!(media.percent.is_empty());
        assert!(media.db.is_empty());

        let brightness = preview_osd_state(Lang::Zh, &cfg, OsdFunction::Brightness);
        assert!(brightness.show_bar);
        assert!(!brightness.percent.is_empty());
        // 亮度没有分贝这一说。
        assert!(brightness.db.is_empty());
    }

    /// 三张预览各自取自己模板的卡片：亮度那张必须是亮度卡的尺寸，
    /// 不能因为编辑器停在音量模板就拿音量的尺寸去画。
    #[test]
    fn preview_takes_each_template_own_card() {
        let mut cfg = OsdConfig::default();
        cfg.brightness_card = Some(crate::config::OsdCard {
            width: 200,
            height: 90,
            corner: 10,
            opacity: 100,
            color: String::new(),
            rotation: 0,
        });
        let brightness = cfg.for_function(OsdFunction::Brightness);
        assert_eq!(crate::windows::osd::card_size(&brightness), (200.0, 90.0));
        // 另外两套不受影响。
        let volume = cfg.for_function(OsdFunction::Volume);
        assert_ne!(crate::windows::osd::card_size(&volume), (200.0, 90.0));
    }

    /// 滚轮按哪套步进由开关决定，跟刻度开关是两件事。
    #[test]
    fn wheel_step_follows_the_selected_unit() {
        let mut config = Config::default();
        config.scroll_wheel_step = 5;
        config.log_scroll_wheel_step = 1.5;

        // 默认按百分比：一格 5 个百分点（跟对数刻度开不开无关）。
        assert!((config.scroll_display(0.4, 2.0) - 0.5).abs() < 1e-6);
        config.use_logarithmic_volume = true;
        assert!((config.scroll_display(0.4, 2.0) - 0.5).abs() < 1e-6);

        // 打开「按分贝数步进」：对数刻度上一格 1.5 dB，加上两格就是 3 dB。
        config.scroll_wheel_in_db = true;
        assert!((config.scroll_display(0.5, 2.0) - 0.575).abs() < 1e-4);

        // 刻度关掉也照样按 dB：0.5 的幅度是 -6 dB，加 3 dB 到 -3 dB。
        config.use_logarithmic_volume = false;
        let next = config.scroll_display(0.5, 2.0);
        assert!((crate::audio::amplitude_to_db(next) - -3.0).abs() < 0.05);
    }

    /// 静音时按 dB 往上滚一格要能起来，不能被 -140 dB 卡住。
    #[test]
    fn db_step_lifts_off_silence() {
        let mut config = Config::default();
        config.scroll_wheel_in_db = true;
        config.log_scroll_wheel_step = 2.0;
        config.use_logarithmic_volume = true;
        let next = config.scroll_display(0.0, 1.0);
        assert!((next - 2.0 / 40.0).abs() < 1e-4, "{next}");
    }

    fn editor_with(lines: &[&str]) -> TooltipEditor {
        TooltipEditor::new(
            TooltipTarget::Normal,
            lines.iter().map(|line| line.to_string()).collect(),
        )
    }

    /// 拖动选区的两头顺序不重要：从后往前拖和从前往后拖选出来的是同一块。
    #[test]
    fn drag_selection_normalizes_direction() {
        let mut editor = editor_with(&["abcd"]);
        editor.begin_selection(0, 1, false);
        editor.drag_to(0, 3);
        assert_eq!(editor.selection(), Some(((0, 1), (0, 3))));
        assert_eq!(editor.selected_text(), "bc");

        // 反向拖：光标在锚点左边。
        let mut editor = editor_with(&["abcd"]);
        editor.begin_selection(0, 3, false);
        editor.drag_to(0, 1);
        assert_eq!(editor.selection(), Some(((0, 1), (0, 3))));
        assert_eq!(editor.selected_text(), "bc");
    }

    /// 只点一下（没拖）不算选区，退格还是照常删一个字。
    #[test]
    fn click_without_drag_is_not_a_selection() {
        let mut editor = editor_with(&["abc"]);
        editor.begin_selection(0, 1, false);
        assert_eq!(editor.selection(), None);
        editor.backspace();
        assert_eq!(editor.lines, vec!["bc".to_string()]);
    }

    /// 退格 / Delete 一次删掉整块选区，光标落到选区起点。
    #[test]
    fn backspace_and_delete_remove_whole_selection() {
        let mut editor = editor_with(&["abcd"]);
        editor.begin_selection(0, 1, false);
        editor.drag_to(0, 4);
        editor.backspace();
        assert_eq!(editor.lines, vec!["a".to_string()]);
        assert_eq!((editor.row, editor.col), (0, 1));
        assert_eq!(editor.selection(), None);

        let mut editor = editor_with(&["abcd"]);
        editor.begin_selection(0, 0, false);
        editor.drag_to(0, 3);
        editor.delete_forward();
        assert_eq!(editor.lines, vec!["d".to_string()]);
        assert_eq!((editor.row, editor.col), (0, 0));
    }

    /// 跨行选区：删掉之后两行接成一行。
    #[test]
    fn selection_across_lines_merges_on_delete() {
        let mut editor = editor_with(&["abc", "def"]);
        editor.begin_selection(0, 1, false);
        editor.drag_to(1, 2);
        assert_eq!(editor.selected_text(), "bc\nde");
        editor.delete_selection();
        assert_eq!(editor.lines, vec!["af".to_string()]);
        assert_eq!((editor.row, editor.col), (0, 1));
    }

    /// 直接打字 / 粘贴：选中的内容被顶替掉。
    #[test]
    fn typing_replaces_the_selection() {
        let mut editor = editor_with(&["abcd"]);
        editor.begin_selection(0, 1, false);
        editor.drag_to(0, 3);
        editor.insert_char('X');
        assert_eq!(editor.lines, vec!["aXd".to_string()]);
        assert_eq!((editor.row, editor.col), (0, 2));

        // 粘贴多行：第一段顶替选区，其余照常拆行。
        let mut editor = editor_with(&["abcd"]);
        editor.begin_selection(0, 1, false);
        editor.drag_to(0, 3);
        editor.paste("1\n2");
        assert_eq!(editor.lines, vec!["a1".to_string(), "2d".to_string()]);
    }

    /// 回车：先删选区，再在删除处拆行。
    #[test]
    fn enter_splits_at_the_selection_start() {
        let mut editor = editor_with(&["abcd"]);
        editor.begin_selection(0, 1, false);
        editor.drag_to(0, 3);
        editor.split_line();
        assert_eq!(editor.lines, vec!["a".to_string(), "d".to_string()]);
        assert_eq!((editor.row, editor.col), (1, 0));
    }

    /// Ctrl+Z：一步步退回去，连光标位置一起还原。
    #[test]
    fn tooltip_undo_steps_back_and_restores_the_caret() {
        let mut editor = editor_with(&["ab"]);
        editor.snapshot();
        editor.insert_char('c');
        editor.snapshot();
        editor.insert_char('d');
        assert_eq!(editor.lines, vec!["abcd".to_string()]);
        assert_eq!((editor.row, editor.col), (0, 4));

        assert!(editor.undo());
        assert_eq!(editor.lines, vec!["abc".to_string()]);
        assert_eq!((editor.row, editor.col), (0, 3));
        assert!(editor.undo());
        assert_eq!(editor.lines, vec!["ab".to_string()]);
        assert_eq!((editor.row, editor.col), (0, 2));
        // 退无可退：内容原样留着。
        assert!(!editor.undo());
        assert_eq!(editor.lines, vec!["ab".to_string()]);
    }

    /// 撤销跨行：拆行 / 删行都能一步退回去。
    #[test]
    fn tooltip_undo_restores_split_lines() {
        let mut editor = editor_with(&["abcd"]);
        editor.set_caret(0, 2);
        editor.snapshot();
        editor.split_line();
        assert_eq!(editor.lines, vec!["ab".to_string(), "cd".to_string()]);
        assert!(editor.undo());
        assert_eq!(editor.lines, vec!["abcd".to_string()]);
        assert_eq!((editor.row, editor.col), (0, 2));
    }

    /// Shift+方向键扩选区；不按 Shift 的方向键先把选区收成一点。
    #[test]
    fn shift_arrows_extend_then_plain_arrows_collapse() {
        let mut editor = editor_with(&["abcd"]);
        editor.begin_selection(0, 2, false);
        editor.extend_selection();
        editor.move_right();
        editor.move_right();
        assert_eq!(editor.selection(), Some(((0, 2), (0, 4))));

        // 左键收成一头上（往前那个），光标不再动。
        assert!(editor.collapse_selection(false));
        assert_eq!(editor.selection(), None);
        assert_eq!((editor.row, editor.col), (0, 2));
    }

    /// Ctrl+A 全选：从第一行行首到最后一行行尾。
    #[test]
    fn select_all_covers_every_line() {
        let mut editor = editor_with(&["abc", "", "de"]);
        editor.select_all();
        assert_eq!(editor.selection(), Some(((0, 0), (2, 2))));
        assert_eq!(editor.selected_text(), "abc\n\nde");
    }

    /// 重命名缓冲：普通输入追加、输入法候选整体替换、提交后落成文字。
    #[test]
    fn rename_apply_handles_ime_composition() {
        // 普通字符：光标在末尾，直接追加。
        let (text, marked) = rename_apply("声音", None, None, "A", false, MAX_NAME_CHARS);
        assert_eq!(text, "声音A");
        assert!(marked.is_none());

        // 输入法开始拼：先出现候选字 `sheng` 的替换结果。
        let (text, marked) = rename_apply("", None, None, "声", true, MAX_NAME_CHARS);
        assert_eq!(text, "声");
        assert_eq!(marked, Some(0..1));

        // 候选更新：整段替换，不是往后堆。
        let (text, marked) = rename_apply(&text, marked, None, "声音", true, MAX_NAME_CHARS);
        assert_eq!(text, "声音");
        assert_eq!(marked, Some(0..2));

        // 提交：同样的字换掉候选段，标记清空。
        let (text, marked) = rename_apply(&text, marked, None, "声音", false, MAX_NAME_CHARS);
        assert_eq!(text, "声音");
        assert!(marked.is_none());

        // 已经有内容时在末尾拼，前缀不能被吃掉。
        let (text, marked) = rename_apply("菜单", None, None, "a", true, MAX_NAME_CHARS);
        assert_eq!(text, "菜单a");
        assert_eq!(marked, Some(2..3));
        let (text, marked) = rename_apply(&text, marked, None, "ab", true, MAX_NAME_CHARS);
        assert_eq!(text, "菜单ab");
        assert_eq!(marked, Some(2..4));
    }

    /// 控制字符（退格键也会发 WM_CHAR）不进缓冲，长度封在 24 个字。
    #[test]
    fn rename_apply_filters_and_caps() {
        let (text, _) = rename_apply("a", None, None, "\u{8}", false, MAX_NAME_CHARS);
        assert_eq!(text, "a", "退格的 WM_CHAR 不该当成文字");

        let (text, _) = rename_apply("", None, None, &"字".repeat(40), false, MAX_NAME_CHARS);
        assert_eq!(text.chars().count(), 24);

        // 已经满 24 个字时再输不进去，也不能截断原有的字。
        let (text, _) = rename_apply(&text, None, None, "X", false, MAX_NAME_CHARS);
        assert_eq!(text.chars().count(), 24);
    }

    /// Shift+点击不算「重新起点」：锚点留在上一次的地方。
    #[test]
    fn shift_click_keeps_the_original_anchor() {
        let mut editor = editor_with(&["abcd"]);
        editor.begin_selection(0, 1, false);
        editor.begin_selection(0, 3, true);
        assert_eq!(editor.selection(), Some(((0, 1), (0, 3))));
        // 再 shift 一次会接着扩，不会把锚点挪到 3。
        editor.begin_selection(0, 4, true);
        assert_eq!(editor.selection(), Some(((0, 1), (0, 4))));
    }
}
