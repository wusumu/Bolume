//! 音量面板：设备列表（常驻平铺，无折叠）+ 应用会话列表。
//!
//! 布局高度完全由内容决定，窗口高度与内容高度严格一致，避免顶部出现空白带。

use std::{
    cell::RefCell,
    collections::HashMap,
    path::PathBuf,
    rc::Rc,
    sync::Arc,
    time::{Duration, Instant},
};

use gpui::prelude::*;
use gpui::{
    Animation, AnimationExt, AnyElement, App, Context, ElementId, IntoElement, MouseButton,
    MouseDownEvent, Pixels, Render, RenderImage, SharedString, TextRun, Window, div, img, point,
    px, size,
};

use crate::windows::platform::TaskbarEdge;
use crate::{
    audio::{
        self, AppInfo, AudioCommand, DeviceInfo, Snapshot, to_display_volume, to_log_volume,
        volume_label,
    },
    config::{Config, ConfigStore},
    i18n::Lang,
    ui::{
        ActivityState, Entrance, SliderDragState, VolumeSlider, anim, device_icon_resolved, icon,
        load_png_rgba, theme, volume_icon_name, wheel_direction,
    },
};

/// 应用条目右键菜单的尺寸（用于把菜单夹在窗口内）。
///
/// 菜单里除了「折叠 / 展开」还要摆一整串播放设备，168 太窄，
/// 设备名（`Headphones (Meizu HiFi DAC…)`）会全被截掉。
const MENU_WIDTH: f32 = 216.0;
/// 菜单每一项的高度 / 上下内边距 / 分隔线与小标题占的高度。
const MENU_ITEM: f32 = 28.0;
const MENU_PAD: f32 = 4.0;
const MENU_SEPARATOR: f32 = 9.0;
const MENU_CAPTION: f32 = 20.0;
/// 应用图标右上角的「已单独指定设备」标记直径。
const ROUTE_BADGE: f32 = 6.0;
/// 算「正在发声」的电平门槛。
///
/// 比面板整体是否活跃用的 0.004 高一档：底噪（尤其浏览器暂停后）常在 0.005 上下飘，
/// 照那个门槛排会让列表一直微微跳。过了这道门槛的应用会被记住（见 `promoted`），
/// 之后哪怕声音落回去也不再挪位 —— 门槛只决定「什么时候挤进前排」。
const SOUNDING_PEAK: f32 = 0.012;

/// 竖向排版（一路一条推子，像调音台）的尺寸。
///
/// 列宽压得很窄：只放图标、一条竖推子和一个百分比。名字默认不写
/// （`config.vertical_app_names` 打开才写一行小字）。
const V_COLUMN_WIDTH: f32 = 46.0;
const V_COLUMN_GAP: f32 = 2.0;
const V_PAD: f32 = 8.0;
/// 推子那一截有多长（触摸区高度）。
const V_FADER_HEIGHT: f32 = 96.0;
const V_ICON_ROW: f32 = 24.0;
const V_READOUT_ROW: f32 = 13.0;
const V_NAME_ROW: f32 = 12.0;
/// 列内各段之间的缝。
const V_GAP: f32 = 4.0;
/// 推子最短也留这么长，否则配一个很矮的面板会把推子压成一条线、拖都拖不动。
const V_FADER_MIN: f32 = 40.0;

/// 面板字号缩放（由 `config.panel_font_size` 和正文基准字号比出来）。
///
/// 竖排那套尺寸是自由函数算的（亮度面板也要读），所以这个也做成自由函数。
/// 上下限只是兜住手改配置文件的极端值（空 / NaN / 天文数字），界面那边有自己的范围。
pub fn panel_scale(cfg: &Config) -> f32 {
    let size = cfg.panel_font_size;
    let size = if size.is_finite() && size > 0.5 {
        size
    } else {
        crate::config::PANEL_BASE_FONT_SIZE
    };
    (size / crate::config::PANEL_BASE_FONT_SIZE).clamp(0.3, 8.0)
}

/// 面板字体族：空串 = 跟随系统默认字体（`None` 就不写这一条）。
fn panel_font_family(cfg: &Config) -> Option<SharedString> {
    let value = cfg.panel_font.trim();
    (!value.is_empty()).then(|| SharedString::from(value.to_string()))
}

/// 面板字重：400 是常规，不写这一条（少一层逐文本覆盖）。
fn panel_font_weight(cfg: &Config) -> Option<gpui::FontWeight> {
    let weight = cfg.panel_font_weight.clamp(100, 900);
    (weight != 400).then(|| gpui::FontWeight(weight as f32))
}

/// 竖排面板的总高度（只跟配置有关，和路数无关）。
///
/// `config.vertical_height` 写了数字就照它来（推子吃掉多出来的那一截），
/// 写 0（默认）就按内容算。亮度面板选「跟随音频面板」时读的就是这个函数，
/// 所以做成自由函数、不挂在实例上。
pub fn vertical_panel_height(cfg: &Config) -> f32 {
    let fixed = vertical_fixed_height(cfg);
    match cfg.vertical_panel_height() {
        Some(height) => height.max(fixed + V_FADER_MIN),
        None => fixed + V_FADER_HEIGHT,
    }
}

/// 竖排一列里**除推子之外**占掉的高度：内边距 / 图标 / 读数 /（可选名字）/ 分隔线 / 页脚。
///
/// 这些段是固定的布局尺寸，不跟面板字号缩放；推子那截也不缩放（它是交互长度）。
fn vertical_fixed_height(cfg: &Config) -> f32 {
    let name = if cfg.vertical_app_names {
        V_NAME_ROW + V_GAP
    } else {
        0.0
    };
    V_PAD * 2.0 + V_ICON_ROW + V_GAP + V_GAP + V_READOUT_ROW + theme::FOOTER_HEIGHT + 1.0 + name
}

/// 推子那一截多长：面板高度扣掉固定那几段，剩下的都给它。
fn vertical_fader_height(cfg: &Config) -> f32 {
    (vertical_panel_height(cfg) - vertical_fixed_height(cfg)).max(V_FADER_MIN)
}

// 弹出动画的曲线与状态机在 `ui::anim`：显示器窗口用的是同一套，
// 两扇窗一起弹出来，曲线差一点都会看出「一个先落、一个后落」。

/// 托盘图标那一下点击会先让面板失焦收起、紧接着才轮到 Toggle，
/// 两次事件的时间差在这之内就当成同一次点击，不再重新展开。
const TOGGLE_SUPPRESS: Duration = Duration::from_millis(250);

/// 「先声夺人」的静音覆盖最多撑这么久。
///
/// 正常的交还条件是「快照跟用户要的值对上」，这个上限只是兜底：万一音频那边
/// 始终没切成（设备拒绝 / 写入失败），面板不能一直挂着一个跟系统不符的图标。
/// 留 3 秒是因为写入本身要阻塞约 740ms、连按时还要排队一轮。
const MUTE_OVERRIDE_FOR: Duration = Duration::from_millis(3000);

/// 松手之后「按住用户要的那个音量」多久（只在写入被设备拒绝时才会撞上）。
///
/// 写入之后要绕一圈才回读得到：音频线程 30Hz 一帧快照，面板忙的时候还会更慢。
/// 从前固定 240ms 就交还真实值，快一点的两帧内就追上、慢一拍就会先弹回旧值
/// 再跳回来 —— 按住不动感觉不出来，按下马上抬起就是一次明显的「横跳」。
/// 现在等「快照跟用户要的值对上」再交还，下面的时长只是兜底。
const VOLUME_HOLD_FOR: Duration = Duration::from_millis(1000);

/// 手势之后无条件顶住的时长。
///
/// 这一小段用来吃「命令队列里的中间态」：连点两下静音（静音 → 立刻解除）时，
/// 前一条命令会晚一步生效，快照里先冒出 `true`、再冒回 `false`。手势刚完就认
/// 快照，那一下中间态会先把图标顶回去、下一秒再跳回来 —— 一个可见的显示循环。
///
/// 和 `osd::HOLD_SETTLE` 同一个数、同一个理由。
const HOLD_SETTLE: Duration = Duration::from_millis(260);

/// 松手后继续按用户要的值画的记录。
struct ReleaseNote {
    want: f32,
    at: Instant,
}

impl ReleaseNote {
    fn new(want: f32) -> Self {
        Self {
            want,
            at: Instant::now(),
        }
    }
}

/// 静音图标「先声夺人」的记录，判据和 `ReleaseNote` 一样。
pub(crate) struct MuteNote {
    want: bool,
    at: Instant,
}

impl MuteNote {
    fn new(want: bool) -> Self {
        Self {
            want,
            at: Instant::now(),
        }
    }
}

/// 松手之后这一行画什么：`Some(v)` 继续顶着用户要的 `v`，`None` 交还快照值。
///
/// 判据只有时间：手势之后先无条件顶 `HOLD_SETTLE`（吃掉命令队列里的中间态），
/// 之后顶到上限为止。**不要**拿「快照已经等于目标了」来提前交还 ——
/// 写入在 USB / 蓝牙端点上会晚几百毫秒才落地，中间那段时间快照「看起来已经
/// 到位」是假象，一交还就正好撞上迟到的旧值。
fn resolve_held_volume(note: &mut ReleaseNote, _real: f32) -> Option<f32> {
    if note.at.elapsed() < HOLD_SETTLE {
        return Some(note.want);
    }
    if note.at.elapsed() > VOLUME_HOLD_FOR {
        return None;
    }
    Some(note.want)
}

/// 静音图标这一行画什么：`Some(v)` 顶着，`None` 交还快照。
///
/// 交还判据是「快照已经等于用户要的值」，而不是顶满多久。音频线程处理一条静音
/// 命令要**同步阻塞约 740ms**，连按 N 下时最后一条要排队等前面几条写完才轮到
/// —— 固定时长按秒数去猜，两下够用、三下就露馅（露出来的那段是旧快照）。
///
/// 为什么「对上就交还」不会误判成提前交还：快照只会在引擎**受理**命令时翻
/// （受理前它一直是上一轮的值，跟新意图对不上；受理时连着按的那串命令已经合并
/// 成最后一个意图了），而 `want` 只会被更新的一下改写 —— 对上了就说明那一下
/// 已经被引擎收下，剩下的落地由引擎的 `MuteGuard` 继续管。
fn resolve_muted(note: &mut MuteNote, real: bool) -> Option<bool> {
    if note.want == real && note.at.elapsed() >= HOLD_SETTLE {
        return None;
    }
    if note.at.elapsed() > MUTE_OVERRIDE_FOR {
        return None;
    }
    Some(note.want)
}

/// 这一行现在该按哪种静音态画（顺手清掉过期的记录）。
///
/// 面板和快捷键共用同一张表 —— 快捷键是 toggle，基准必须是「用户最后按下时的
/// 意图」。只看快照的话，快照 30Hz 才回读一次，连按两下时第二下读到的还是旧值，
/// 于是连发两条一样的命令：按两下依旧是静音，用户读作「解除不掉」。
pub(crate) fn shown_muted(
    map: &Rc<RefCell<HashMap<String, MuteNote>>>,
    key: &str,
    real: bool,
) -> bool {
    let mut map = map.borrow_mut();
    let Some(note) = map.get_mut(key) else {
        return real;
    };
    match resolve_muted(note, real) {
        Some(want) => want,
        None => {
            map.remove(key);
            real
        }
    }
}

/// 点静音图标时算「这次要变成什么」，顺手记一笔预测。
///
/// 基准必须是**现在显示的那个值**，不能是闭包里捕获的渲染值：面板 33ms 才重画一帧，
/// 一帧里点两下（手快 / 连点器）时渲染值还是上一帧的，两次会发出同一个目标 ——
/// 用户点两下等于点一下。和快捷键 toggle 走的是同一条基准。
fn toggle_muted(map: &Rc<RefCell<HashMap<String, MuteNote>>>, key: &str, shown: bool) -> bool {
    let want = !shown_muted(map, key, shown);
    note_muted(map, key, want);
    want
}

/// 记一笔「用户要这个静音态」（面板与快捷键共用）。
pub(crate) fn note_muted(map: &Rc<RefCell<HashMap<String, MuteNote>>>, key: &str, want: bool) {
    map.borrow_mut()
        .insert(key.to_string(), MuteNote::new(want));
}

/// 滚轮这一路没有「松手」事件：也记一笔预览，等快照动过自动交还。
///
/// 只写 `release_hold` 不写 `drag_values`：后者是「手还按着」，写了就再也不跟
/// 系统走了（滚轮没有松手可以把它们清掉）。
fn note_scroll(
    hold: &Rc<RefCell<HashMap<String, ReleaseNote>>>,
    muted_slot: &Rc<RefCell<HashMap<String, MuteNote>>>,
    key: &str,
    display: f32,
    unmute: bool,
) {
    hold.borrow_mut()
        .insert(key.to_string(), ReleaseNote::new(display));
    if unmute {
        muted_slot
            .borrow_mut()
            .insert(key.to_string(), MuteNote::new(false));
    }
}

/// 滚轮连滚几格时的基准值。
///
/// 面板要等下一次重绘才会把新值画上去，闭包里捕获的 `display` 还是上一帧的；
/// 连滚三格就会三格都从同一个旧值算，只前进一格。预览表里有值就先按它算。
fn scroll_base(hold: &Rc<RefCell<HashMap<String, ReleaseNote>>>, key: &str, fallback: f32) -> f32 {
    hold.borrow().get(key).map_or(fallback, |note| note.want)
}

pub struct Flyout {
    pub snapshot: Snapshot,
    pub config: Rc<RefCell<ConfigStore>>,
    pub engine: Rc<crate::audio::AudioEngine>,
    pub on_open_settings: Option<Rc<dyn Fn(&mut Window, &mut App)>>,
    /// 面板钉住了没有：钉住时不跟失焦自动收起，一直挂在最前面。
    ///
    /// 和托盘的「点一下收起」不冲突 —— 那是用户主动关，钉住也照收。
    pinned: bool,
    icons: RefCell<HashMap<PathBuf, Arc<RenderImage>>>,
    /// 拖动滑条期间的本地预览值（key → 0..1 显示音量）。
    ///
    /// 音频接口（尤其是 USB DAC、蓝牙）写入音量后回读有延迟，如果滑条位置
    /// 完全等引擎回传再更新，手感会明显滞后。这里在拖动时先用本地值渲染，
    /// 松手后再交还给真实值。
    drag_values: Rc<RefCell<HashMap<String, f32>>>,
    /// 刚被「调音量顺手解除静音」的条目（key → 手势时刻）。
    ///
    /// 解除静音在音频线程里是**立刻**生效的（SetMute(false)），但面板画的是快照里
    /// 回读出来的 `muted`：快照 30Hz 一帧，还要排队，于是耳朵先听到声音、
    /// 图标过一会儿才变。这里记一笔，手势当下就把那一行按「未静音」画，
    /// 等快照真的说「没静音」就把记录删掉 —— 只是提前，不会把状态搞错。
    muted_override: Rc<RefCell<HashMap<String, MuteNote>>>,
    /// 刚松手、还在等快照动起来的那笔音量（key → 记录）。
    ///
    /// `drag_values` 是「手还按着」，这个是「手松了但别急着交还」：
    /// 快照一离开松手时的基准值就删，最多撑 `VOLUME_HOLD_FOR`。
    release_hold: Rc<RefCell<HashMap<String, ReleaseNote>>>,
    drag: SliderDragState,
    /// 音量条电平特效的平滑状态（所有行共用，按元素令牌区分）。
    activity: ActivityState,
    /// 特效补帧循环是否在跑：只在这一帧真有声音时跑，静音就停。
    activity_ticker: bool,
    /// 当前鼠标悬停的应用 key（长名字悬停滚动用）。
    hovered_name: Rc<RefCell<Option<String>>>,
    /// 应用条目右键菜单（窗口坐标），None 表示没打开。
    menu: Rc<RefCell<Option<AppMenu>>>,
    /// 设备切换菜单（窗口坐标），None 表示没打开。
    device_menu: Rc<RefCell<Option<DeviceMenu>>>,
    /// 是否把「已折叠」的应用也展开出来。只在本次打开面板期间有效。
    show_hidden: bool,
    /// 上一次设定的窗口高度，避免每帧无条件 SetWindowPos。
    last_height: f32,
    /// 最近一次收起的时间：托盘图标那一下点击先触发失焦收起，
    /// 紧接着才轮到 Toggle，不挡一下就会「收起又立刻打开」。
    last_hidden: Option<Instant>,
    /// 入场动画（与显示器窗口共用 `ui::anim::Entrance`，两扇窗必须同步长出来）。
    anim: Entrance,
    /// 任务栏贴在哪条边：面板贴同一条边，并从那一侧向屏幕内展开。
    edge: crate::windows::platform::TaskbarEdge,
    visible: bool,
    /// 应用列表的滚动句柄：滚轮要根据它算光标落在第几行。
    apps_scroll: gpui::ScrollHandle,
    /// 上一次处理滚轮的时刻：`on_scroll_wheel` 在 gpui 里可能被同一帧注册多份，
    /// 一格滚轮会连着回调好几次，用时间戳把它压成一次。
    wheel_last_at: Option<Instant>,
    /// 本次打开面板期间「已经靠过前」的应用 key，按首次发声的先后排。
    ///
    /// 一旦进来就不出去：声音小的应用电平常在门槛上下抖，跟着它来回挪会让条目
    /// 在列表里一直窜。这里只增不减，面板每次弹出时清空。
    promoted: RefCell<Vec<String>>,
}

/// 应用条目的右键菜单。
#[derive(Clone)]
struct AppMenu {
    key: String,
    name: String,
    /// 当前是否已被折叠（决定菜单显示「折叠」还是「展开」）。
    hidden: bool,
    /// 这个应用被单独指定到的设备；`None` = 跟随系统默认设备。
    device_id: Option<String>,
    /// 应用进程 id（改路由按 pid 下发）。
    pid: u32,
    /// 能不能改：系统声音没有独立会话，未公开接口拿不到时也不给改。
    movable: bool,
    x: f32,
    y: f32,
    /// 菜单实际宽度：竖向排版的面板只有一百多像素宽，写死 216 会伸到窗口外被裁掉。
    width: f32,
}

/// 设备名点开的「更换播放设备」菜单（浮层，不占布局）。
#[derive(Clone)]
struct DeviceMenu {
    x: f32,
    y: f32,
}

/// 设备菜单每行高度、宽度与上下内边距。
const DEVICE_MENU_ROW: f32 = 32.0;
const DEVICE_MENU_WIDTH: f32 = 236.0;
const DEVICE_MENU_PAD: f32 = 4.0;

impl Flyout {
    pub(crate) fn new(
        snapshot: Snapshot,
        config: Rc<RefCell<ConfigStore>>,
        engine: Rc<crate::audio::AudioEngine>,
        muted_override: Rc<RefCell<HashMap<String, MuteNote>>>,
    ) -> Self {
        Self {
            snapshot,
            config,
            engine,
            on_open_settings: None,
            pinned: false,
            icons: RefCell::new(HashMap::new()),
            drag_values: Rc::new(RefCell::new(HashMap::new())),
            muted_override,
            release_hold: Rc::new(RefCell::new(HashMap::new())),
            drag: SliderDragState::new(),
            activity: ActivityState::new(),
            activity_ticker: false,
            hovered_name: Rc::new(RefCell::new(None)),
            menu: Rc::new(RefCell::new(None)),
            device_menu: Rc::new(RefCell::new(None)),
            show_hidden: false,
            last_height: 0.0,
            last_hidden: None,
            anim: Entrance::new(),
            edge: crate::windows::platform::taskbar_edge(),
            visible: false,
            apps_scroll: gpui::ScrollHandle::new(),
            wheel_last_at: None,
            promoted: RefCell::new(Vec::new()),
        }
    }

    pub fn apply_snapshot(&mut self, snapshot: Snapshot) -> bool {
        if self.snapshot == snapshot {
            return false;
        }
        self.snapshot = snapshot;
        true
    }

    /// 这一行现在按哪种静音状态画。
    ///
    /// `real` 是快照里回读出来的值。手势刚发生的那一下快照还没跟上，而耳朵
    /// 已经听到声音了 —— 就用「期望值」顶上；等快照真的对上了或者超时，
    /// 就把记录删掉，之后完全以系统为准，不会把状态带偏。
    fn muted_for(&self, key: &str, real: bool) -> bool {
        shown_muted(&self.muted_override, key, real)
    }

    fn cfg(&self) -> Config {
        self.config.borrow().get().clone()
    }

    fn display_volume(&self, raw: f32) -> f32 {
        let cfg = self.cfg();
        if cfg.use_logarithmic_volume {
            to_display_volume(raw, cfg.logarithmic_min_db as f32)
        } else {
            raw
        }
    }

    /// 松手之后要不要继续按「用户要的值」画：要就返回它。
    ///
    /// 快照离开松手时的基准值（说明写入已经反映到系统里了）就交还真实值 ——
    /// 这样「按下马上抬起」也不会先弹回旧值再跳回来。
    fn held_volume(&self, key: &str, real: f32) -> Option<f32> {
        let mut map = self.release_hold.borrow_mut();
        let Some(note) = map.get_mut(key) else {
            return None;
        };
        let held = resolve_held_volume(note, real);
        if held.is_none() {
            map.remove(key);
        }
        held
    }

    fn current_device(&self) -> Option<&DeviceInfo> {
        let id = self.snapshot.current_device_id.as_deref()?;
        self.snapshot.devices.iter().find(|device| device.id == id)
    }

    /// 列出的应用会话：默认全部列出（含当前没发声的），只按设置隐藏「系统声音」。
    /// 被右键隐藏的应用不在这里，它们折叠到列表底部的「已隐藏」分组里。
    ///
    /// 打开「正在播放的应用排最前」时按 `promoted` 排：一旦某个应用在面板开着
    /// 的这段时间里发过声，它就固定留在前面那一组，不再因为声音变小而退回去。
    /// 排序只发生在面板重绘这一帧里（快照本来就是面板开着时才按 33ms 刷），
    /// 面板收起后没人碰这份顺序，后台不会为了它多跑一个线程或定时器。
    fn visible_apps(&self) -> Vec<&AppInfo> {
        let cfg = self.cfg();
        let mut apps: Vec<&AppInfo> = self
            .snapshot
            .apps
            .iter()
            .filter(|app| cfg.show_system_sounds || !app.is_system)
            .filter(|app| !cfg.hidden_apps.iter().any(|key| key == &app.key))
            .collect();
        if cfg.sounding_apps_first {
            // 只往队尾追加：先响的排在更前面，已排过的顺序永远不会被打乱。
            let mut promoted = self.promoted.borrow_mut();
            for app in &apps {
                if app.peak > SOUNDING_PEAK && !promoted.iter().any(|key| key == &app.key) {
                    promoted.push(app.key.clone());
                }
            }
            // 稳定排序：没进过 `promoted` 的一律 `usize::MAX`，它们之间保持系统给的
            // 顺序，只有「靠过前 / 没靠过前」跨组时才动，所以不会每帧都抖。
            apps.sort_by_key(|app| {
                promoted
                    .iter()
                    .position(|key| key == &app.key)
                    .unwrap_or(usize::MAX)
            });
        } else {
            // 关掉这个功能时把记忆清掉，下次打开从头开始，不会带着上次的顺序。
            self.promoted.borrow_mut().clear();
        }
        apps
    }

    /// 被隐藏、但当前还在跑的应用（折叠分组里的内容）。
    fn hidden_apps(&self) -> Vec<&AppInfo> {
        let cfg = self.cfg();
        self.snapshot
            .apps
            .iter()
            .filter(|app| cfg.show_system_sounds || !app.is_system)
            .filter(|app| cfg.hidden_apps.iter().any(|key| key == &app.key))
            .collect()
    }

    fn is_hidden(&self, key: &str) -> bool {
        self.cfg().hidden_apps.iter().any(|hidden| hidden == key)
    }

    /// 这个应用能不能改输出设备。
    ///
    /// 系统声音没有自己的进程（pid 0），未公开接口对它无效；
    /// 拿不到 AudioPolicyConfig 的系统上干脆整组不给改。
    fn is_movable(&self, app: &AppInfo) -> bool {
        !app.is_system && self.snapshot.app_routing
    }

    /// 右键菜单的高度：折叠项 +（可改时）分隔线 / 小标题 / 设备列表。
    ///
    /// 用来把菜单夹在面板里 —— 设备多的机器上它会比面板还高，
    /// 只能保证菜单顶边不越界（gpui 的浮层不做滚动）。
    fn app_menu_height(&self, movable: bool) -> f32 {
        // 菜单行高是固定值（不跟面板字号缩放），这里照着常量算就行。
        let base = MENU_PAD * 2.0 + MENU_ITEM;
        if !movable {
            return base;
        }
        let devices = self
            .snapshot
            .devices
            .iter()
            .filter(|device| device.enabled)
            .count() as f32;
        base + MENU_SEPARATOR + MENU_CAPTION + MENU_ITEM * (1.0 + devices)
    }

    fn set_hidden(&mut self, key: &str, hidden: bool) {
        let key = key.to_string();
        let _ = self.config.borrow_mut().update(|config| {
            config.hidden_apps.retain(|existing| existing != &key);
            if hidden {
                config.hidden_apps.push(key.clone());
            }
        });
    }

    fn icon_for(&self, app: &AppInfo) -> Option<Arc<RenderImage>> {
        let path = app.icon_path.as_ref()?;
        if let Some(image) = self.icons.borrow().get(path) {
            return Some(image.clone());
        }
        let (width, height, mut rgba) = load_png_rgba(path)?;
        for pixel in rgba.chunks_exact_mut(4) {
            pixel.swap(0, 2);
        }
        let buffer = image::RgbaImage::from_raw(width, height, rgba)?;
        let image = Arc::new(RenderImage::new(vec![image::Frame::new(buffer)]));
        self.icons.borrow_mut().insert(path.clone(), image.clone());
        Some(image)
    }

    /// 应用图标的显示边长 = 从 ICO 里取的那一档（1:1 画出来最清楚）。
    fn icon_size(&self) -> f32 {
        self.cfg().app_icon_size.clamp(16, 64) as f32
    }

    /// 面板字号缩放（`panel_font_size / 基准字号`）。
    ///
    /// 直接读配置、不 clone 整份 `Config`：渲染一帧里要用几十次。
    fn font_scale(&self) -> f32 {
        panel_scale(&self.config.borrow().get())
    }

    /// 面板里的**文字大小**，只有它跟着「面板字号」走。
    ///
    /// 字号是唯一还会跟着配置变的尺寸：面板宽高、行高、图标、内边距全部固定 ——
    /// 用户把字调大只是字变大，窗口一个像素都不动（放不下就自己调「行高 / 面板宽度」）。
    fn fs(&self, base: f32) -> Pixels {
        px(base * self.font_scale())
    }

    /// 面板里**固定在布局里的尺寸**都过它（行高、内边距、图标行、页脚……）。
    ///
    /// 不再乘字号缩放：这就是「面板字号不影响面板大小」的落点。
    fn su(&self, base: f32) -> Pixels {
        px(base)
    }

    /// 面板文字的字间距（像素，可负）。
    ///
    /// 不乘缩放：填的就是像素，字号变了字距不变是正常的排版行为
    /// （要把字距也一起放大，就往正数方向再调一点）。
    fn spacing_value(&self) -> f32 {
        let value = self.config.borrow().get().panel_letter_spacing;
        if value.is_finite() { value } else { 0.0 }
    }

    fn letter_spacing(&self) -> Pixels {
        px(self.spacing_value())
    }

    /// 横排那一格读数的宽度：字宽 + 每个字多占的那一份字间距。
    ///
    /// 这一格**跟着字号长**：它夹在滑条和静音图标中间，不让出位置的话
    /// 「100%」会压到图标上。挤掉的是滑条的长度，面板宽度不变（`panel_width()` 不读字号）。
    fn readout_width(&self, log: bool) -> Pixels {
        let (base, chars) = if log { (88.0, 8.0) } else { (34.0, 4.0) };
        px((base * self.font_scale() + self.spacing_value() * chars).max(base * 0.6))
    }

    /// 竖排一列的宽度：同样要给字间距留位置（读数 / 可选的应用名）。
    fn column_width(&self) -> f32 {
        (V_COLUMN_WIDTH + self.spacing_value() * 5.0).max(V_COLUMN_WIDTH * 0.6)
    }

    /// 应用行高：图标选小了还是原来的 44，选大了跟着长高，不然会压到旁边的控件。
    ///
    /// 用户自己填了「行高」就以它为准。**不跟面板字号走**：字号只改文字大小，
    /// 行高不够就自己填一个 —— 这是「面板字号不影响面板大小」的另一半。
    fn row_height(&self) -> f32 {
        let custom = self.cfg().panel_line_height;
        if custom.is_finite() && custom > 0.5 {
            return custom;
        }
        (self.icon_size() + 20.0).max(theme::ROW_HEIGHT)
    }

    /// 窗口高度 = 内容高度，逐项累加，不留弹性空位。
    ///
    /// 面板只显示当前默认设备一行；切换设备在设置页里做，所以高度恒定。
    pub fn desired_height(&self) -> f32 {
        if self.cfg().panel_layout.is_vertical() {
            return self.column_height();
        }
        let visible = self.visible_apps().len();
        let hidden = self.hidden_apps().len();
        // 有折叠项时多一行「已折叠」标题；展开后再加上被折叠的行。
        let extra = if hidden > 0 {
            1 + if self.show_hidden { hidden } else { 0 }
        } else {
            0
        };
        let apps = (visible + extra).clamp(1, theme::MAX_VISIBLE_APPS) as f32;

        // 三段固定高度是布局常量，不跟面板字号走。
        (theme::DEVICE_ROW_HEIGHT + theme::APPS_HEADER_HEIGHT + theme::FOOTER_HEIGHT)
            + 1.0
            + apps * self.row_height()
    }

    /// 竖向排版一共几列：设备占一列，其余每路应用一列。
    pub fn channel_count(&self) -> usize {
        self.visible_apps().len().clamp(1, theme::MAX_VISIBLE_APPS) + 1
    }

    /// 面板宽度：横排听用户配的（`flyout_width`），竖排由列数算出来。
    /// 两种都**不乘面板字号** —— 字号只改文字，窗口宽度不会跟着变。
    pub fn panel_width(&self) -> f32 {
        if self.cfg().panel_layout.is_vertical() {
            let columns = self.channel_count() as f32;
            // 和渲染那边对齐：左右内边距、列宽都是固定值，列间距也是。
            V_PAD * 2.0 + columns * self.column_width() + (columns - 1.0) * V_COLUMN_GAP
        } else {
            self.cfg().flyout_width()
        }
    }

    /// 竖排的一列有多高：图标 / 推子 / 读数（可选名字）+ 页脚。
    fn column_height(&self) -> f32 {
        vertical_panel_height(&self.cfg())
    }

    /// 窗口尺寸：(宽, 高)。
    pub fn panel_size(&self) -> (f32, f32) {
        (self.panel_width(), self.desired_height())
    }

    fn mute_button(
        &self,
        id: String,
        muted: bool,
        size: f32,
        on_click: impl Fn(&mut Window, &mut App) + 'static,
    ) -> AnyElement {
        let color = if muted {
            theme::danger()
        } else {
            theme::text_secondary()
        };
        let name = if muted { "volume-off" } else { "volume-high" };
        div()
            .id(SharedString::from(id))
            .size(px(size))
            .flex_none()
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(theme::RADIUS_SMALL))
            .cursor_pointer()
            .hover(|style| style.bg(theme::surface_hover()))
            .on_click(move |_, window, cx| {
                on_click(window, cx);
                // 图标、灰条、读数颜色都跟着这一下变，别等下一次快照重绘。
                window.refresh();
            })
            .child(icon(name, size * 0.58, color))
            .into_any_element()
    }

    /// 当前默认设备行：图标 + 名称 + 音量条 + 百分比 + 静音。
    /// 面板里不切换设备（设备选择在设置页）。
    fn device_row(&self, device: &DeviceInfo, cx: &mut Context<Self>) -> AnyElement {
        let drag_key = format!("device:{}", device.id);
        // 静音状态的「先声夺人」表：手势当下就按期望值画，不等快照回读。
        let muted_slot = self.muted_override.clone();
        let muted_key = drag_key.clone();
        // 两个闭包各拿一份：Rc / String 都不实现 Copy，不能同时 move 进去。
        let unmute_slot = muted_slot.clone();
        let unmute_key = muted_key.clone();
        let mute_slot = muted_slot.clone();
        let mute_key = muted_key.clone();
        let real = self.display_volume(device.volume);
        let display = match self.drag_values.borrow().get(&drag_key).copied() {
            Some(value) => value,
            // 松手之后还没交还：快照追上来之前继续按用户要的值画。
            None => self.held_volume(&drag_key, real).unwrap_or(real),
        };
        let peak = self.display_volume(device.peak);
        let muted = self.muted_for(&drag_key, device.muted);
        let effect = self.cfg().activity_effect;
        let log = self.cfg().use_logarithmic_volume;
        let min_db = self.cfg().logarithmic_min_db as f32;
        // 设备最大音量限制：滑条拖到头也就停在这里。
        let ceiling = crate::gui::cap_display(
            self.cfg().volume_cap.limit_for(&device.id),
            &self.cfg(),
            min_db,
        );
        let label = volume_readout(display, log, min_db);
        let play_sound = self.cfg().release_sound;
        // 面板内的滚轮由「面板内滚动滚轮调节音量」控制，步进取设置里的滚轮步进。
        let scroll_cfg = self.cfg();
        let scroll_enabled = scroll_cfg.use_global_mouse_wheel_hook;

        let engine = self.engine.clone();
        let engine_for_slider = engine.clone();
        let id_for_slider = device.id.clone();
        let engine_for_mute = engine.clone();
        let id_for_mute = device.id.clone();
        let id_for_scroll = device.id.clone();
        let engine_for_scroll = engine.clone();
        let drag_values = self.drag_values.clone();
        let drag_values_release = self.drag_values.clone();
        let release_hold_release = self.release_hold.clone();
        let drag_key_change = drag_key.clone();
        let drag_key_release = drag_key.clone();
        // 滚轮那一路没有松手事件：靠同一张预览表把这一格当场画出来。
        let wheel_hold = self.release_hold.clone();
        let wheel_muted = self.muted_override.clone();
        let drag_key_scroll = drag_key.clone();
        // 拖动也要按「增 / 减」分开决定解不解除静音，所以单独克隆一份配置。
        let drag_cfg = self.cfg();

        let slider = VolumeSlider::new(
            SharedString::from(format!("device-volume-{}", device.id)),
            display,
            self.drag.clone(),
        )
        .peak(peak)
        .muted(muted)
        .fill(theme::accent())
        .effect(effect)
        .activity(self.activity.clone())
        .limit(ceiling)
        .on_change(move |value, window, _| {
            // 先拿到上一次的位置：方向（增大 / 减小）是拿这两次比出来的。
            let previous = drag_values
                .borrow_mut()
                .insert(drag_key_change.clone(), value);
            window.refresh();
            let raw = if log {
                to_log_volume(value, min_db)
            } else {
                value
            };
            let unmute = slider_unmute(
                &drag_cfg,
                crate::config::VolumeGesture::Drag,
                previous,
                display,
                value,
            );
            if unmute {
                // 耳朵已经听到声音了，面板这一格不该还挂着静音。
                unmute_slot
                    .borrow_mut()
                    .insert(unmute_key.clone(), MuteNote::new(false));
            }
            engine_for_slider.send(AudioCommand::SetDeviceVolume {
                device_id: id_for_slider.clone(),
                volume: raw,
                unmute,
            });
        })
        .on_release(move |window, _cx| {
            // 松手别马上交还真实值：写入要过音频线程那 30Hz 的快照才回读得到，
            // 交还早了一步，读数会先弹回旧值、再跳回来 —— 按住不动看不出来，
            // 按下马上抬起就是一次清楚的「横跳」。这里把值挂进 release_hold，
            // 等快照真的追上（或等满 `VOLUME_HOLD_FOR`）再放开。
            let values = drag_values_release.clone();
            let hold = release_hold_release.clone();
            let key = drag_key_release.clone();
            if let Some(value) = values.borrow_mut().remove(&key) {
                hold.borrow_mut().insert(key, ReleaseNote::new(value));
            }
            window.refresh();
            if play_sound {
                crate::platform::sound::tick(window);
            }
        });

        let mute = self.mute_button(
            format!("device-mute-{}", device.id),
            muted,
            26.0,
            move |_, _| {
                let want = toggle_muted(&mute_slot, &mute_key, muted);
                engine_for_mute.send(AudioCommand::SetDeviceMute {
                    device_id: id_for_mute.clone(),
                    muted: want,
                });
            },
        );

        let name_size = self.cfg().device_name_size.clamp(9, 20) as f32;
        // 自定义过名字就只显示自定义名（系统原名只在设置页里看得到）。
        let (name_head, name_tail) =
            split_device_name(&self.cfg().device_label(&device.id, &device.name));

        // 设备名点一下弹出设备菜单（浮层，窗口尺寸不变）。
        let device_menu_slot = self.device_menu.clone();
        let device_count = self
            .snapshot
            .devices
            .iter()
            .filter(|device| device.enabled)
            .count();
        // 行高 / 图标行都是固定尺寸（不跟面板字号走）：浮层要刚好挂在它下面。
        let open_device_menu = cx.listener(
            move |_: &mut Self, _: &MouseDownEvent, window: &mut Window, cx| {
                // 再点一下设备名就收起来（同一个按钮一开一关）。
                if device_menu_slot.borrow().is_some() {
                    *device_menu_slot.borrow_mut() = None;
                    cx.notify();
                    return;
                }
                let size = window.viewport_size();
                let height = device_count as f32 * DEVICE_MENU_ROW + DEVICE_MENU_PAD * 2.0;
                // 左边对齐设备行自己的 6px 内缩，看着像从设备行里长出来的。
                let x = 6.0;
                // 挂在整条设备行的下方（行内还有滑块，贴在点击处会把自己盖住）；
                // 下方塞不下就往上收，但至少留 6px。
                let y = (theme::DEVICE_ROW_HEIGHT + 1.0)
                    .min(size.height.as_f32() - height - 6.0)
                    .max(6.0);
                *device_menu_slot.borrow_mut() = Some(DeviceMenu { x, y });
                cx.notify();
            },
        );

        div()
            .id("current-device")
            .h(self.su(theme::DEVICE_ROW_HEIGHT))
            .px(px(8.))
            .mx(px(6.))
            .py(px(10.))
            .flex()
            .flex_col()
            .flex_none()
            .justify_center()
            .gap(px(6.))
            .rounded(px(theme::RADIUS_ROW))
            .on_scroll_wheel(move |event, window, _| {
                if !scroll_enabled {
                    return;
                }
                let steps = wheel_direction(event.delta);
                if steps == 0.0 {
                    return;
                }
                // 连滚时按预览值算，别拿上一帧的读数当基准（否则几格只走一格）。
                let base = scroll_base(&wheel_hold, &drag_key_scroll, display);
                let next = scroll_cfg.scroll_display(base, steps).min(ceiling);
                let raw = if log {
                    to_log_volume(next, min_db)
                } else {
                    next
                };
                let unmute = scroll_cfg
                    .unmute
                    .allows(crate::config::VolumeGesture::HoverScroll, steps > 0.0);
                // 滚轮也「先声夺人」：声音和画面对齐，别让这一格等快照回读。
                note_scroll(&wheel_hold, &wheel_muted, &drag_key_scroll, next, unmute);
                window.refresh();
                engine_for_scroll.send(AudioCommand::SetDeviceVolume {
                    device_id: id_for_scroll.clone(),
                    volume: raw,
                    unmute,
                });
            })
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(9.))
                    .child(icon(
                        &device_icon_resolved(&self.cfg(), &device.id, device.kind),
                        18.0,
                        theme::accent(),
                    ))
                    .child(
                        div()
                            .id("device-name-button")
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(1.))
                            .px(px(5.))
                            .mx(px(-5.))
                            .rounded(px(theme::RADIUS_SMALL))
                            .cursor_pointer()
                            .hover(|style| style.bg(theme::surface_hover()))
                            .on_mouse_down(MouseButton::Left, open_device_menu)
                            .child(
                                div()
                                    .text_size(self.fs(name_size))
                                    .text_color(theme::text_primary())
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .child(name_head),
                            )
                            .when_some(name_tail, |style, tail| {
                                style.child(
                                    div()
                                        .text_size(self.fs((name_size - 1.5).max(9.0)))
                                        .text_color(theme::text_tertiary())
                                        .whitespace_nowrap()
                                        .text_ellipsis()
                                        .child(tail),
                                )
                            }),
                    )
                    .child(mute),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(9.))
                    .pl(px(27.))
                    .child(div().flex_1().min_w_0().child(slider))
                    .child(
                        div()
                            .w(self.readout_width(log))
                            .flex_none()
                            .whitespace_nowrap()
                            .text_size(self.fs(11.5))
                            .text_color(if muted {
                                theme::text_tertiary()
                            } else {
                                theme::text_secondary()
                            })
                            .text_right()
                            .child(label),
                    ),
            )
            .into_any_element()
    }

    /// 应用名：放不下就先省略号；鼠标悬停时横向滚一遍，看全名。
    fn app_name(
        &self,
        key: &str,
        name: String,
        color: gpui::Hsla,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // 名字区宽度是固定值（不跟面板字号走），只有字间距会给每个字多占一份，
        // 宽度不跟就得少看几个字。
        let visible_width = (104.0 + self.spacing_value() * 4.0).max(72.0);
        // 只在悬停时才量文字宽度，平时不白算。
        let hovered = self.hovered_name.borrow().as_deref() == Some(key);
        let overhang = if hovered {
            (measure_text_width(&name, window) - visible_width).max(0.0)
        } else {
            0.0
        };

        let text = div()
            .whitespace_nowrap()
            .text_size(self.fs(12.5))
            .text_color(color)
            .child(name);

        let inner: AnyElement = if overhang > 0.5 {
            // 前后各留一成时间停顿，中间匀速滚过去（整周期 2.8s）。
            let travel = overhang + 10.0;
            text.with_animation(
                ElementId::Name(format!("name-scroll-{key}").into()),
                Animation::new(std::time::Duration::from_millis(2800)).repeat(),
                move |element, delta| {
                    let progress = ((delta - 0.1) / 0.8).clamp(0.0, 1.0);
                    element.ml(px(-travel * progress))
                },
            )
            .into_any_element()
        } else {
            text.text_ellipsis().into_any_element()
        };

        let hovered_name = self.hovered_name.clone();
        let key_for_hover = key.to_string();
        div()
            .id(ElementId::Name(format!("name-hover-{key}").into()))
            .w(px(visible_width))
            .flex_none()
            .overflow_hidden()
            .on_hover(cx.listener(move |_, is_hovered: &bool, window, cx| {
                let key = key_for_hover.as_str();
                let mut current = hovered_name.borrow_mut();
                if *is_hovered {
                    if current.as_deref() == Some(key) {
                        return;
                    }
                    *current = Some(key.to_string());
                } else {
                    // 从 A 划到 B 时，A 的移出事件可能晚于 B 的移入，别把 B 清掉。
                    if current.as_deref() != Some(key) {
                        return;
                    }
                    *current = None;
                }
                drop(current);
                cx.notify();
                window.refresh();
            }))
            .child(inner)
            .into_any_element()
    }

    fn empty_device_row(&self, lang: Lang) -> AnyElement {
        div()
            .h(self.su(theme::DEVICE_ROW_HEIGHT))
            .px(px(16.))
            .flex()
            .items_center()
            .text_size(self.fs(12.))
            .text_color(theme::text_tertiary())
            .child(lang.pick("没有可用的播放设备", "No playback devices"))
            .into_any_element()
    }

    /// 应用列表上的滚轮：调整光标所在那一行的应用音量。
    ///
    /// 监听挂在列表容器而不是每一行上：行都躺在 `overflow_y_scroll` 里，
    /// 行级 `on_scroll_wheel` 实测收不到（容器把滚轮算作自己的滚动）。
    /// 这里按光标 y 反推行号，命中就吃掉这次滚动，免得列表跟着一起滚。
    /// 应用列表上的滚轮：调整光标所在那一行的应用音量。
    ///
    /// 挂 `apps-list` 这个 div 就行：元素上的监听每帧重建，数量恒定；
    /// 反过来在 `render` 里调 `window.on_mouse_event` 会往下一帧的监听表里
    /// 反复追加（同一帧 render 可能跑好几次），一格滚轮会触发好几档。
    fn on_apps_wheel(
        &mut self,
        event: &gpui::ScrollWheelEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let cfg = self.cfg();
        if !(cfg.use_global_mouse_wheel_hook && cfg.use_scroll_wheel_in_apps) {
            return;
        }
        let steps = wheel_direction(event.delta);
        if steps == 0.0 {
            return;
        }
        // 同一次滚轮可能连着重入好几遍，只认第一遍。
        let now = Instant::now();
        if let Some(last) = self.wheel_last_at {
            if now.duration_since(last) < Duration::from_millis(3) {
                return;
            }
        }
        self.wheel_last_at = Some(now);
        let Some(index) = self.app_row_at(event.position.y) else {
            return;
        };
        let app = match self.visible_apps().get(index) {
            Some(app) => (*app).clone(),
            None => return,
        };

        let key = format!("app:{}", app.key);
        // 连滚时按预览值算：面板还没重绘，`app.volume` 是旧的。
        let display = scroll_base(&self.release_hold, &key, self.display_volume(app.volume));
        let min_db = cfg.logarithmic_min_db as f32;
        let next = cfg.scroll_display(display, steps);
        let raw = if cfg.use_logarithmic_volume {
            to_log_volume(next, min_db)
        } else {
            next
        };
        let unmute = cfg
            .unmute
            .allows(crate::config::VolumeGesture::AppHoverScroll, steps > 0.0);
        // 滚轮也「先声夺人」：这一格不等快照回读。
        note_scroll(&self.release_hold, &self.muted_override, &key, next, unmute);
        self.engine.send(AudioCommand::SetAppVolume {
            key: app.key.clone(),
            volume: raw,
            unmute,
        });
        // 命中某一行就别让列表跟着滚。
        cx.stop_propagation();
        cx.notify();
    }

    /// 光标落在应用列表的第几行（用滚动句柄记下来的行矩形，不靠行高推算）。
    fn app_row_at(&self, y: Pixels) -> Option<usize> {
        let y = y.as_f32();
        let count = self.visible_apps().len();
        (0..count).find(|&ix| {
            self.apps_scroll
                .bounds_for_item(ix)
                .map(|bounds| {
                    y >= bounds.origin.y.as_f32()
                        && y < bounds.origin.y.as_f32() + bounds.size.height.as_f32()
                })
                .unwrap_or(false)
        })
    }

    fn app_row(&self, app: &AppInfo, window: &mut Window, cx: &mut Context<Self>) -> AnyElement {
        let key = app.key.clone();
        let drag_key = format!("app:{key}");
        // 同上：应用这一路也按手势先画。
        let muted_slot = self.muted_override.clone();
        let muted_key = drag_key.clone();
        // 两个闭包各拿一份：Rc / String 都不实现 Copy，不能同时 move 进去。
        let unmute_slot = muted_slot.clone();
        let unmute_key = muted_key.clone();
        let mute_slot = muted_slot.clone();
        let mute_key = muted_key.clone();
        let real = self.display_volume(app.volume);
        let display = match self.drag_values.borrow().get(&drag_key).copied() {
            Some(value) => value,
            // 松手之后还没交还：快照追上来之前继续按用户要的值画。
            None => self.held_volume(&drag_key, real).unwrap_or(real),
        };
        let peak = self.display_volume(app.peak);
        let muted = self.muted_for(&drag_key, app.muted);
        let effect = self.cfg().activity_effect;
        let lang = self.cfg().language;
        let log = self.cfg().use_logarithmic_volume;
        let min_db = self.cfg().logarithmic_min_db as f32;
        let label = volume_readout(display, log, min_db);
        let play_sound = self.cfg().release_sound;
        let playing = app.state == 1;

        let engine = self.engine.clone();
        let engine_for_slider = engine.clone();
        let engine_for_mute = engine.clone();
        let engine_for_middle = engine.clone();
        let key_for_slider = key.clone();
        let key_for_mute = key.clone();
        let key_for_middle = key.clone();
        let drag_values = self.drag_values.clone();
        let drag_values_release = self.drag_values.clone();
        let release_hold_release = self.release_hold.clone();
        let drag_key_change = drag_key.clone();
        let drag_key_release = drag_key.clone();
        // 滚轮那一路没有松手事件：靠同一张预览表把这一格当场画出来。
        let wheel_hold = self.release_hold.clone();
        let wheel_muted = self.muted_override.clone();
        let drag_key_scroll = drag_key.clone();
        // 拖动要按「增 / 减」决定解不解除静音，所以单独克隆一份配置。
        let drag_cfg = self.cfg();

        let icon_side = self.icon_size();
        let icon_element: AnyElement = match self.icon_for(app) {
            Some(image) => img(image)
                .size(px(icon_side))
                .rounded(px(5.))
                .into_any_element(),
            None => div()
                .size(px(icon_side))
                .flex_none()
                .flex()
                .items_center()
                .justify_center()
                .child(icon("music", icon_side * 0.85, theme::text_tertiary()))
                .into_any_element(),
        };
        // 被单独指定过播放设备的应用，图标右上角打一个小点。
        let icon_element = with_route_badge(icon_element, app.is_routed());

        let mute = self.mute_button(format!("app-mute-{key}"), muted, 26.0, move |_, _| {
            let want = toggle_muted(&mute_slot, &mute_key, muted);
            engine_for_mute.send(AudioCommand::SetAppMute {
                key: key_for_mute.clone(),
                muted: want,
            });
        });

        let slider = VolumeSlider::new(
            SharedString::from(format!("app-volume-{key}")),
            display,
            self.drag.clone(),
        )
        .peak(peak)
        .muted(muted)
        .fill(theme::accent())
        .effect(effect)
        .activity(self.activity.clone())
        .on_change(move |value, window, _| {
            let previous = drag_values
                .borrow_mut()
                .insert(drag_key_change.clone(), value);
            window.refresh();
            let raw = if log {
                to_log_volume(value, min_db)
            } else {
                value
            };
            // 和应用自己的静音按钮分开算：走「拖动应用滑条」那两条开关。
            let unmute = slider_unmute(
                &drag_cfg,
                crate::config::VolumeGesture::AppDrag,
                previous,
                display,
                value,
            );
            if unmute {
                // 耳朵已经听到声音了，面板这一格不该还挂着静音。
                unmute_slot
                    .borrow_mut()
                    .insert(unmute_key.clone(), MuteNote::new(false));
            }
            engine_for_slider.send(AudioCommand::SetAppVolume {
                key: key_for_slider.clone(),
                volume: raw,
                unmute,
            });
        })
        .on_release(move |window, _cx| {
            // 松手别马上交还真实值：写入要过音频线程那 30Hz 的快照才回读得到，
            // 交还早了一步，读数会先弹回旧值、再跳回来 —— 按住不动看不出来，
            // 按下马上抬起就是一次清楚的「横跳」。这里把值挂进 release_hold，
            // 等快照真的追上（或等满 `VOLUME_HOLD_FOR`）再放开。
            let values = drag_values_release.clone();
            let hold = release_hold_release.clone();
            let key = drag_key_release.clone();
            if let Some(value) = values.borrow_mut().remove(&key) {
                hold.borrow_mut().insert(key, ReleaseNote::new(value));
            }
            window.refresh();
            if play_sound {
                crate::platform::sound::tick(window);
            }
        });

        let name = if app.is_system {
            lang.pick("系统声音", "System sounds").to_string()
        } else {
            app.name.clone()
        };
        let hidden = self.is_hidden(&key);
        let name_color = if hidden || muted || !playing {
            theme::text_secondary()
        } else {
            theme::text_primary()
        };
        let name_element = self.app_name(&key, name.clone(), name_color, window, cx);

        // 右键菜单：把光标位置和当前隐藏状态记下来，菜单内容在 render 里画。
        let menu_slot = self.menu.clone();
        let key_for_menu = key.clone();
        let hidden_for_menu = hidden;
        let device_for_menu = app.device_id.clone();
        let pid_for_menu = app.pid;
        let movable_for_menu = self.is_movable(app);
        let menu_height = self.app_menu_height(movable_for_menu);
        let open_menu = cx.listener(
            move |this: &mut Self, event: &MouseDownEvent, window: &mut Window, cx| {
                // 用真实窗口尺寸而不是 viewport_size：面板淡出时 viewport 报得比窗口高，
                // 按它夹会把菜单放到窗口外面去（下半截被裁掉）。
                let size = window.bounds().size;
                // 竖向排版的面板只有一百多像素宽，固定 216 会伸到窗口外被裁掉，宽度跟着窗口走。
                let menu_width = MENU_WIDTH.min((size.width.as_f32() - 8.0).max(80.0));
                let x = event
                    .position
                    .x
                    .as_f32()
                    .min((size.width.as_f32() - menu_width - 4.0).max(4.0))
                    .max(4.0);
                let click_y = event.position.y.as_f32();
                let height = size.height.as_f32();
                // 默认在光标下方展开；下方放不下就翻到上方，两边都不够就贴顶。
                let y = if click_y + menu_height + 6.0 <= height {
                    click_y + 2.0
                } else {
                    (click_y - menu_height - 2.0).max(4.0)
                };
                *menu_slot.borrow_mut() = Some(AppMenu {
                    key: key_for_menu.clone(),
                    name: name.clone(),
                    hidden: hidden_for_menu,
                    device_id: device_for_menu.clone(),
                    pid: pid_for_menu,
                    movable: movable_for_menu,
                    x,
                    y,
                    width: menu_width,
                });
                // 打开菜单即视为不再悬停，免得名字继续滚动。
                *this.hovered_name.borrow_mut() = None;
                cx.notify();
            },
        );

        let mut row = div()
            .id(SharedString::from(format!("row-{key}")))
            .h(px(self.row_height()))
            .px(px(8.))
            .mx(px(6.))
            .flex()
            .items_center()
            .gap(px(9.))
            .rounded(px(theme::RADIUS_ROW))
            .hover(|style| style.bg(theme::surface_hover()))
            .on_mouse_down(MouseButton::Right, open_menu)
            .on_mouse_down(MouseButton::Middle, move |_, _, _| {
                engine_for_middle.send(AudioCommand::SetAppMute {
                    key: key_for_middle.clone(),
                    muted: true,
                });
            });
        if hidden {
            // 被折叠的应用在展开后用弱化样式区分。
            row = row.opacity(0.55);
        }
        row.child(icon_element)
            .child(name_element)
            .child(div().flex_1().min_w_0().child(slider))
            .child(
                div()
                    .w(self.readout_width(log))
                    .flex_none()
                    .whitespace_nowrap()
                    .text_size(self.fs(11.5))
                    .text_color(theme::text_tertiary())
                    .text_right()
                    .child(label),
            )
            .child(mute)
            .into_any_element()
    }

    // ---------- 竖向排版（调音台样式） ----------
    //
    // 一路一列：图标在上、竖推子居中、百分比在下。名字默认不写，
    // 因为列只有 46 宽；`config.vertical_app_names` 打开才在读数下面加一行小字。
    // 左右键的分工和横排一致：左键点图标 = 切静音，右键 = 菜单。

    /// 一列的壳子：图标 / 推子 / 读数 /（可选）名字。
    fn channel_shell(
        &self,
        id: String,
        icon_element: AnyElement,
        slider: VolumeSlider,
        label: String,
        name: Option<String>,
    ) -> AnyElement {
        // 面板高度可配（`config.vertical_height`），推子吃掉多出来那一截。
        let fader_height = vertical_fader_height(&self.cfg());
        div()
            .id(SharedString::from(id))
            .w(px(self.column_width()))
            .flex_none()
            .flex()
            .flex_col()
            .items_center()
            .gap(self.su(V_GAP))
            .child(
                div()
                    .h(self.su(V_ICON_ROW))
                    .flex_none()
                    .flex()
                    .items_center()
                    .justify_center()
                    .child(icon_element),
            )
            .child(
                div()
                    .w_full()
                    .h(px(fader_height))
                    .flex_none()
                    .px(px(6.))
                    .child(slider),
            )
            .child(
                div()
                    .h(self.su(V_READOUT_ROW))
                    .w_full()
                    .flex_none()
                    .text_size(self.fs(10.))
                    .text_center()
                    .whitespace_nowrap()
                    .text_color(theme::text_secondary())
                    .child(label),
            )
            .when_some(name, |column, name| {
                column.child(
                    div()
                        .h(self.su(V_NAME_ROW))
                        .w_full()
                        .flex_none()
                        .text_size(self.fs(9.5))
                        .text_color(theme::text_tertiary())
                        .text_center()
                        .truncate()
                        .child(name),
                )
            })
            .into_any_element()
    }

    /// 图标按钮：左键切静音，图标本身按静音状态变色 / 变淡。
    fn channel_icon(
        &self,
        id: String,
        element: AnyElement,
        muted: bool,
        on_click: impl Fn(&mut Window, &mut App) + 'static,
    ) -> AnyElement {
        div()
            .id(SharedString::from(id))
            .size(self.su(V_ICON_ROW))
            .flex()
            .items_center()
            .justify_center()
            .rounded(px(5.))
            .cursor_pointer()
            .when(muted, |style| style.opacity(0.55))
            .hover(|style| style.bg(theme::surface_hover()))
            .on_mouse_down(MouseButton::Left, move |_, window, cx| {
                cx.stop_propagation();
                on_click(window, cx);
                // 竖排的图标是自己画的一层，不刷新就要等下一帧快照才变
                // —— 而音频线程写静音时可能正卡着几百毫秒。
                window.refresh();
            })
            .child(element)
            .into_any_element()
    }

    /// 竖向排版：设备（主音量）那一列。
    fn device_channel(&self, device: &DeviceInfo, cx: &mut Context<Self>) -> AnyElement {
        let drag_key = format!("device:{}", device.id);
        // 静音状态的「先声夺人」表：手势当下就按期望值画，不等快照回读。
        let muted_slot = self.muted_override.clone();
        let muted_key = drag_key.clone();
        // 两个闭包各拿一份：Rc / String 都不实现 Copy，不能同时 move 进去。
        let unmute_slot = muted_slot.clone();
        let unmute_key = muted_key.clone();
        let mute_slot = muted_slot.clone();
        let mute_key = muted_key.clone();
        let real = self.display_volume(device.volume);
        let display = match self.drag_values.borrow().get(&drag_key).copied() {
            Some(value) => value,
            // 松手之后还没交还：快照追上来之前继续按用户要的值画。
            None => self.held_volume(&drag_key, real).unwrap_or(real),
        };
        let peak = self.display_volume(device.peak);
        let muted = self.muted_for(&drag_key, device.muted);
        let log = self.cfg().use_logarithmic_volume;
        let min_db = self.cfg().logarithmic_min_db as f32;
        let ceiling = crate::gui::cap_display(
            self.cfg().volume_cap.limit_for(&device.id),
            &self.cfg(),
            min_db,
        );
        // 列太窄，读数只写一个量（对数模式下写 dB，不并百分比）。
        let label = volume_label(display, log, min_db);
        let play_sound = self.cfg().release_sound;
        let scroll_cfg = self.cfg();
        let scroll_enabled = scroll_cfg.use_global_mouse_wheel_hook;

        let engine = self.engine.clone();
        let engine_for_slider = engine.clone();
        let engine_for_mute = engine.clone();
        let engine_for_scroll = engine.clone();
        let id_for_slider = device.id.clone();
        let id_for_mute = device.id.clone();
        let id_for_scroll = device.id.clone();
        let drag_values = self.drag_values.clone();
        let drag_values_release = self.drag_values.clone();
        let release_hold_release = self.release_hold.clone();
        let drag_key_change = drag_key.clone();
        let drag_key_release = drag_key.clone();
        // 滚轮那一路没有松手事件：靠同一张预览表把这一格当场画出来。
        let wheel_hold = self.release_hold.clone();
        let wheel_muted = self.muted_override.clone();
        let drag_key_scroll = drag_key.clone();
        let drag_cfg = self.cfg();

        let slider = VolumeSlider::new(
            SharedString::from(format!("device-fader-{}", device.id)),
            display,
            self.drag.clone(),
        )
        .peak(peak)
        .muted(muted)
        .fill(theme::accent())
        .effect(self.cfg().activity_effect)
        .activity(self.activity.clone())
        .limit(ceiling)
        .vertical(true)
        .on_change(move |value, window, _| {
            let previous = drag_values
                .borrow_mut()
                .insert(drag_key_change.clone(), value);
            window.refresh();
            let raw = if log {
                to_log_volume(value, min_db)
            } else {
                value
            };
            let unmute = slider_unmute(
                &drag_cfg,
                crate::config::VolumeGesture::Drag,
                previous,
                display,
                value,
            );
            if unmute {
                // 耳朵已经听到声音了，面板这一格不该还挂着静音。
                unmute_slot
                    .borrow_mut()
                    .insert(unmute_key.clone(), MuteNote::new(false));
            }
            engine_for_slider.send(AudioCommand::SetDeviceVolume {
                device_id: id_for_slider.clone(),
                volume: raw,
                unmute,
            });
        })
        .on_release(move |window, _cx| {
            // 松手别马上交还真实值：写入要过音频线程那 30Hz 的快照才回读得到，
            // 交还早了一步，读数会先弹回旧值、再跳回来 —— 按住不动看不出来，
            // 按下马上抬起就是一次清楚的「横跳」。这里把值挂进 release_hold，
            // 等快照真的追上（或等满 `VOLUME_HOLD_FOR`）再放开。
            let values = drag_values_release.clone();
            let hold = release_hold_release.clone();
            let key = drag_key_release.clone();
            if let Some(value) = values.borrow_mut().remove(&key) {
                hold.borrow_mut().insert(key, ReleaseNote::new(value));
            }
            window.refresh();
            if play_sound {
                crate::platform::sound::tick(window);
            }
        });

        let icon_side = self.icon_size().min(V_COLUMN_WIDTH - 12.0).max(14.0);
        let icon_color = if muted {
            theme::danger()
        } else {
            theme::accent()
        };
        let icon_element = icon(
            &device_icon_resolved(&self.cfg(), &device.id, device.kind),
            icon_side,
            icon_color,
        )
        .into_any_element();
        let icon_button = self.channel_icon(
            format!("device-icon-{}", device.id),
            icon_element,
            muted,
            move |_, _| {
                let want = toggle_muted(&mute_slot, &mute_key, muted);
                engine_for_mute.send(AudioCommand::SetDeviceMute {
                    device_id: id_for_mute.clone(),
                    muted: want,
                });
            },
        );

        // 右键弹设备菜单：竖排里没有设备名可点，图标右键是唯一入口（和应用的右键一致）。
        let device_menu_slot = self.device_menu.clone();
        let device_count = self
            .snapshot
            .devices
            .iter()
            .filter(|device| device.enabled)
            .count();
        // 行高 / 图标行都是固定尺寸（不跟面板字号走）：浮层要刚好挂在它下面。
        let open_device_menu = cx.listener(
            move |_: &mut Self, event: &MouseDownEvent, window: &mut Window, cx| {
                cx.stop_propagation();
                let size = window.viewport_size();
                let height = device_count as f32 * DEVICE_MENU_ROW + DEVICE_MENU_PAD * 2.0;
                // 贴着图标往下弹；下方放不下就往上收。
                let x = 4.0;
                let y = V_ICON_ROW + V_PAD.min(size.height.as_f32() - height - 6.0).max(6.0);
                let _ = event;
                *device_menu_slot.borrow_mut() = Some(DeviceMenu { x, y });
                cx.notify();
            },
        );

        let id_for_wheel = device.id.clone();
        div()
            .id(SharedString::from("device-channel"))
            .on_mouse_down(MouseButton::Right, open_device_menu)
            .on_scroll_wheel(move |event, window, _| {
                if !scroll_enabled {
                    return;
                }
                let steps = wheel_direction(event.delta);
                if steps == 0.0 {
                    return;
                }
                // 连滚时按预览值算，别拿上一帧的读数当基准（否则几格只走一格）。
                let base = scroll_base(&wheel_hold, &drag_key_scroll, display);
                let next = scroll_cfg.scroll_display(base, steps).min(ceiling);
                let raw = if log {
                    to_log_volume(next, min_db)
                } else {
                    next
                };
                let unmute = scroll_cfg
                    .unmute
                    .allows(crate::config::VolumeGesture::HoverScroll, steps > 0.0);
                // 滚轮也「先声夺人」：声音和画面对齐，别让这一格等快照回读。
                note_scroll(&wheel_hold, &wheel_muted, &drag_key_scroll, next, unmute);
                window.refresh();
                engine_for_scroll.send(AudioCommand::SetDeviceVolume {
                    device_id: id_for_wheel.clone(),
                    volume: raw,
                    unmute,
                });
            })
            .child(self.channel_shell(
                "device-channel-shell".to_string(),
                icon_button,
                slider,
                label,
                None,
            ))
            .into_any_element()
    }

    /// 竖向排版：一路应用的列。
    fn app_channel(
        &self,
        app: &AppInfo,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let key = app.key.clone();
        let drag_key = format!("app:{key}");
        // 同上：应用这一路也按手势先画。
        let muted_slot = self.muted_override.clone();
        let muted_key = drag_key.clone();
        // 两个闭包各拿一份：Rc / String 都不实现 Copy，不能同时 move 进去。
        let unmute_slot = muted_slot.clone();
        let unmute_key = muted_key.clone();
        let mute_slot = muted_slot.clone();
        let mute_key = muted_key.clone();
        let real = self.display_volume(app.volume);
        let display = match self.drag_values.borrow().get(&drag_key).copied() {
            Some(value) => value,
            // 松手之后还没交还：快照追上来之前继续按用户要的值画。
            None => self.held_volume(&drag_key, real).unwrap_or(real),
        };
        let peak = self.display_volume(app.peak);
        let muted = self.muted_for(&drag_key, app.muted);
        let log = self.cfg().use_logarithmic_volume;
        let min_db = self.cfg().logarithmic_min_db as f32;
        let label = volume_label(display, log, min_db);
        let play_sound = self.cfg().release_sound;
        let scroll_cfg = self.cfg();
        let scroll_enabled = scroll_cfg.use_global_mouse_wheel_hook;
        let lang = self.cfg().language;

        let engine = self.engine.clone();
        let engine_for_slider = engine.clone();
        let engine_for_mute = engine.clone();
        let engine_for_scroll = engine.clone();
        let key_for_slider = key.clone();
        let key_for_mute = key.clone();
        let key_for_scroll = key.clone();
        let drag_values = self.drag_values.clone();
        let drag_values_release = self.drag_values.clone();
        let release_hold_release = self.release_hold.clone();
        let drag_key_change = drag_key.clone();
        let drag_key_release = drag_key.clone();
        // 滚轮那一路没有松手事件：靠同一张预览表把这一格当场画出来。
        let wheel_hold = self.release_hold.clone();
        let wheel_muted = self.muted_override.clone();
        let drag_key_scroll = drag_key.clone();
        // 拖动要按「增 / 减」决定解不解除静音。
        let drag_cfg = self.cfg();

        let slider = VolumeSlider::new(
            SharedString::from(format!("app-fader-{key}")),
            display,
            self.drag.clone(),
        )
        .peak(peak)
        .muted(muted)
        .fill(theme::accent())
        .effect(self.cfg().activity_effect)
        .activity(self.activity.clone())
        .vertical(true)
        .on_change(move |value, window, _| {
            let previous = drag_values
                .borrow_mut()
                .insert(drag_key_change.clone(), value);
            window.refresh();
            let raw = if log {
                to_log_volume(value, min_db)
            } else {
                value
            };
            // 和应用自己的静音按钮分开算：走「拖动应用滑条」那两条开关。
            let unmute = slider_unmute(
                &drag_cfg,
                crate::config::VolumeGesture::AppDrag,
                previous,
                display,
                value,
            );
            if unmute {
                // 耳朵已经听到声音了，面板这一格不该还挂着静音。
                unmute_slot
                    .borrow_mut()
                    .insert(unmute_key.clone(), MuteNote::new(false));
            }
            engine_for_slider.send(AudioCommand::SetAppVolume {
                key: key_for_slider.clone(),
                volume: raw,
                unmute,
            });
        })
        .on_release(move |window, _cx| {
            // 松手别马上交还真实值：写入要过音频线程那 30Hz 的快照才回读得到，
            // 交还早了一步，读数会先弹回旧值、再跳回来 —— 按住不动看不出来，
            // 按下马上抬起就是一次清楚的「横跳」。这里把值挂进 release_hold，
            // 等快照真的追上（或等满 `VOLUME_HOLD_FOR`）再放开。
            let values = drag_values_release.clone();
            let hold = release_hold_release.clone();
            let key = drag_key_release.clone();
            if let Some(value) = values.borrow_mut().remove(&key) {
                hold.borrow_mut().insert(key, ReleaseNote::new(value));
            }
            window.refresh();
            if play_sound {
                crate::platform::sound::tick(window);
            }
        });

        let icon_side = self.icon_size().min(V_COLUMN_WIDTH - 12.0).max(14.0);
        let icon_element: AnyElement = match self.icon_for(app) {
            Some(image) => img(image)
                .size(px(icon_side))
                .rounded(px(4.))
                .into_any_element(),
            None => icon("music", icon_side, theme::text_tertiary()).into_any_element(),
        };
        let icon_element = with_route_badge(icon_element, app.is_routed());
        let icon_button = self.channel_icon(
            format!("app-icon-{key}"),
            icon_element,
            muted,
            move |_, _| {
                let want = toggle_muted(&mute_slot, &mute_key, muted);
                engine_for_mute.send(AudioCommand::SetAppMute {
                    key: key_for_mute.clone(),
                    muted: want,
                });
            },
        );

        let name = if self.cfg().vertical_app_names {
            Some(if app.is_system {
                lang.pick("系统声音", "System sounds").to_string()
            } else {
                app.name.clone()
            })
        } else {
            None
        };

        // 右键菜单：和横排同一个浮层，位置按点击点算。
        let menu_slot = self.menu.clone();
        let key_for_menu = key.clone();
        let hidden = self.is_hidden(&key);
        let name_for_menu = name.clone().unwrap_or_else(|| app.name.clone());
        let device_for_menu = app.device_id.clone();
        let pid_for_menu = app.pid;
        let movable_for_menu = self.is_movable(app);
        let menu_height = self.app_menu_height(movable_for_menu);
        let open_menu = cx.listener(
            move |this: &mut Self, event: &MouseDownEvent, window: &mut Window, cx| {
                cx.stop_propagation();
                // 用真实窗口尺寸而不是 viewport_size：面板淡出时 viewport 报得比窗口高，
                // 按它夹会把菜单放到窗口外面去（下半截被裁掉）。
                let size = window.bounds().size;
                // 竖向排版的面板只有一百多像素宽，固定 216 会伸到窗口外被裁掉，宽度跟着窗口走。
                let menu_width = MENU_WIDTH.min((size.width.as_f32() - 8.0).max(80.0));
                let x = event
                    .position
                    .x
                    .as_f32()
                    .min((size.width.as_f32() - menu_width - 4.0).max(4.0))
                    .max(4.0);
                let click_y = event.position.y.as_f32();
                let height = size.height.as_f32();
                // 默认在光标下方展开；下方放不下就翻到上方，两边都不够就贴顶。
                let y = if click_y + menu_height + 6.0 <= height {
                    click_y + 2.0
                } else {
                    (click_y - menu_height - 2.0).max(4.0)
                };
                *menu_slot.borrow_mut() = Some(AppMenu {
                    key: key_for_menu.clone(),
                    name: name_for_menu.clone(),
                    hidden,
                    device_id: device_for_menu.clone(),
                    pid: pid_for_menu,
                    movable: movable_for_menu,
                    x,
                    y,
                    width: menu_width,
                });
                *this.hovered_name.borrow_mut() = None;
                cx.notify();
            },
        );

        let id_for_wheel = key.clone();
        div()
            .id(SharedString::from(format!("app-channel-{key}")))
            .when(hidden, |style| style.opacity(0.55))
            .on_mouse_down(MouseButton::Right, open_menu)
            .on_scroll_wheel(move |event, window, _| {
                if !scroll_enabled {
                    return;
                }
                let steps = wheel_direction(event.delta);
                if steps == 0.0 {
                    return;
                }
                // 连滚时按预览值算，别拿上一帧的读数当基准。
                let base = scroll_base(&wheel_hold, &drag_key_scroll, display);
                let next = scroll_cfg.scroll_display(base, steps);
                let raw = if log {
                    to_log_volume(next, min_db)
                } else {
                    next
                };
                let unmute = scroll_cfg
                    .unmute
                    .allows(crate::config::VolumeGesture::HoverScroll, steps > 0.0);
                // 滚轮也「先声夺人」：声音和画面对齐，别让这一格等快照回读。
                note_scroll(&wheel_hold, &wheel_muted, &drag_key_scroll, next, unmute);
                window.refresh();
                engine_for_scroll.send(AudioCommand::SetAppVolume {
                    key: id_for_wheel.clone(),
                    volume: raw,
                    unmute,
                });
            })
            .child(self.channel_shell(
                format!("app-channel-shell-{key}"),
                icon_button,
                slider,
                label,
                name,
            ))
            .into_any_element()
    }

    /// 列表底部的「已折叠 n」标题：点一下展开/收起被折叠的应用。
    fn hidden_toggle_row(&self, count: usize, cx: &mut Context<Self>) -> AnyElement {
        let lang = self.cfg().language;
        let expanded = self.show_hidden;
        let onclick = cx.listener(|this: &mut Self, _, window: &mut Window, cx| {
            this.show_hidden = !this.show_hidden;
            this.update_frame(window, cx);
            cx.notify();
        });

        div()
            .id("hidden-apps-toggle")
            .h(px(28.))
            .mx(px(6.))
            .px(px(8.))
            .flex()
            .flex_none()
            .items_center()
            .gap(px(6.))
            .rounded(px(theme::RADIUS_ROW))
            .cursor_pointer()
            .hover(|style| style.bg(theme::surface_hover()))
            .on_click(onclick)
            .child(icon(
                if expanded {
                    "chevron-up"
                } else {
                    "chevron-down"
                },
                11.0,
                theme::text_tertiary(),
            ))
            .child(
                div()
                    .text_size(self.fs(11.))
                    .text_color(theme::text_tertiary())
                    .child(format!(
                        "{} {count}",
                        lang.pick("已折叠的应用", "Collapsed apps")
                    )),
            )
            .into_any_element()
    }

    fn apps_header(&self) -> AnyElement {
        let lang = self.cfg().language;
        let count = self.visible_apps().len();

        // 设置按钮挪到页脚了（混音器 · 设置 · 置顶），标题行只留「应用 n」。
        div()
            .h(self.su(theme::APPS_HEADER_HEIGHT))
            .px(px(14.))
            .flex()
            .flex_none()
            .items_center()
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap(px(6.))
                    .child(
                        div()
                            .text_size(self.fs(11.))
                            .font_weight(gpui::FontWeight::SEMIBOLD)
                            .text_color(theme::text_tertiary())
                            .child(lang.pick("应用", "Apps")),
                    )
                    .child(
                        // 计数与标题同字号：字号不同的话基线对不齐，数字会上下错位。
                        div()
                            .text_size(self.fs(11.))
                            .text_color(theme::text_tertiary())
                            .child(format!("{count}")),
                    ),
            )
            .into_any_element()
    }

    /// 应用条目的右键菜单：一层可点掉的遮罩 + 浮在上面的小卡片。
    ///
    /// 上半是折叠 / 展开，下半（能改路由时）是这台应用用哪个播放设备 ——
    /// 和 EarTrumpet 的「移动」按钮同一套语义：`默认设备` + 分隔线 + 每台设备，
    /// 当前生效的那一项打勾。
    fn context_menu(&self, menu: AppMenu, cx: &mut Context<Self>) -> AnyElement {
        let lang = self.cfg().language;
        let (label, next_hidden) = if menu.hidden {
            (lang.pick("展开此应用", "Expand this app"), false)
        } else {
            (lang.pick("折叠此应用", "Collapse this app"), true)
        };
        let key = menu.key.clone();

        let dismiss = cx.listener(|this: &mut Self, _, _, cx| {
            *this.menu.borrow_mut() = None;
            cx.notify();
        });
        let activate = cx.listener(move |this: &mut Self, _, window: &mut Window, cx| {
            this.set_hidden(&key, next_hidden);
            *this.menu.borrow_mut() = None;
            // 折叠/展开会改变列表行数，窗口高度得跟着重新算。
            this.update_frame(window, cx);
            cx.notify();
        });

        let mut items = div().flex().flex_col().child(self.menu_row(
            "app-menu-hide",
            label,
            None,
            false,
            activate,
        ));

        if menu.movable {
            items = items
                .child(
                    div()
                        .h(px(1.))
                        .mx(px(6.))
                        .my(self.su((MENU_SEPARATOR - 1.0) / 2.0))
                        .bg(theme::border()),
                )
                .child(
                    div()
                        .h(self.su(MENU_CAPTION))
                        .px(px(10.))
                        .flex()
                        .items_center()
                        .text_size(self.fs(10.))
                        .text_color(theme::text_tertiary())
                        .child(lang.pick("播放设备", "Playback device")),
                );

            // 第一项永远是「跟随默认设备」—— 和 EarTrumpet 一样，
            // 选它就是把这个应用的持久化设置清掉。
            let engine = self.engine.clone();
            let slot = self.menu.clone();
            let pid = menu.pid;
            let follow_default = cx.listener(move |this: &mut Self, _, _, cx| {
                engine.send(AudioCommand::SetAppDevice {
                    pid,
                    device_id: None,
                });
                // 菜单留着：用户要能看见勾选跑到「跟随默认设备」上。
                *slot.borrow_mut() = None;
                cx.notify();
                let _ = this;
            });
            items = items.child(self.menu_row(
                "app-route-default",
                lang.pick("跟随默认设备", "Default device"),
                None,
                menu.device_id.is_none(),
                follow_default,
            ));

            let cfg = self.cfg();
            for device in self.snapshot.devices.iter().filter(|device| device.enabled) {
                let engine = self.engine.clone();
                let slot = self.menu.clone();
                let pid = menu.pid;
                let device_id = device.id.clone();
                let checked = menu.device_id.as_deref() == Some(device.id.as_str());
                let select = cx.listener(move |_: &mut Self, _, _, cx| {
                    engine.send(AudioCommand::SetAppDevice {
                        pid,
                        device_id: Some(device_id.clone()),
                    });
                    *slot.borrow_mut() = None;
                    cx.notify();
                });
                items = items.child(self.menu_row(
                    &format!("app-route-{}", device.id),
                    &cfg.device_label(&device.id, &device.name),
                    Some(device_icon_resolved(&cfg, &device.id, device.kind)),
                    checked,
                    select,
                ));
            }
        }

        div()
            .absolute()
            .size_full()
            .on_mouse_down(MouseButton::Left, dismiss)
            .child(
                div()
                    .absolute()
                    .left(px(menu.x))
                    .top(px(menu.y))
                    .w(px(menu.width))
                    .p(self.su(MENU_PAD))
                    .flex()
                    .flex_col()
                    .rounded(px(theme::RADIUS_SMALL + 2.0))
                    .bg(theme::surface_elevated())
                    .border_1()
                    .border_color(theme::border_strong())
                    // 卡片自己吃掉按下事件：外面那层遮罩靠 `on_mouse_down` 收起菜单，
                    // 不挡住的话点菜单项会先被遮罩清掉，菜单项的点击永远等不到。
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(items),
            )
            .into_any_element()
    }

    /// 菜单里的一行：可选图标 + 文字 +（选中时）右侧小圆点。
    fn menu_row(
        &self,
        id: &str,
        label: &str,
        icon_name: Option<std::borrow::Cow<'static, str>>,
        checked: bool,
        on_click: impl Fn(&gpui::MouseDownEvent, &mut Window, &mut App) + 'static,
    ) -> AnyElement {
        let mut row = div()
            .id(SharedString::from(id.to_string()))
            .h(self.su(MENU_ITEM))
            .flex_none()
            .px(px(8.))
            .flex()
            .items_center()
            .gap(px(7.))
            .rounded(px(theme::RADIUS_SMALL))
            .cursor_pointer()
            .hover(|style| style.bg(theme::surface_hover()))
            .on_mouse_down(MouseButton::Left, on_click);
        if let Some(name) = icon_name {
            let color = if checked {
                theme::accent()
            } else {
                theme::text_tertiary()
            };
            row = row.child(icon(&name, 13.0, color));
        }
        row.child(
            div()
                .flex_1()
                .min_w_0()
                .text_size(self.fs(12.))
                .text_color(if checked {
                    theme::accent()
                } else {
                    theme::text_primary()
                })
                .whitespace_nowrap()
                .text_ellipsis()
                .child(label.to_string()),
        )
        .when(checked, |style| {
            style.child(
                div()
                    .size(px(6.))
                    .flex_none()
                    .rounded_full()
                    .bg(theme::accent()),
            )
        })
        .into_any_element()
    }

    /// 「更换播放设备」浮层：点设备名弹出，选一个就设为默认设备。
    ///
    /// 完全是绝对定位的浮层，不改面板尺寸，也不挤动下面任何一行。
    fn device_menu_view(&self, menu: DeviceMenu, cx: &mut Context<Self>) -> AnyElement {
        let lang = self.cfg().language;

        let dismiss = cx.listener(|this: &mut Self, _, _, cx| {
            *this.device_menu.borrow_mut() = None;
            cx.notify();
        });

        let mut items = div().flex().flex_col();
        let devices: Vec<&DeviceInfo> = self
            .snapshot
            .devices
            .iter()
            .filter(|device| device.enabled)
            .collect();
        if devices.is_empty() {
            items = items.child(
                div()
                    .h(self.su(DEVICE_MENU_ROW))
                    .px(px(8.))
                    .flex()
                    .items_center()
                    .text_size(self.fs(11.5))
                    .text_color(theme::text_tertiary())
                    .child(lang.pick("没有可用的播放设备", "No playback devices")),
            );
        }

        for device in &devices {
            let device_id = device.id.clone();
            let is_default = device.is_default;
            let engine = self.engine.clone();
            let slot = self.device_menu.clone();
            let select = cx.listener(move |_: &mut Self, _, _, cx| {
                engine.send(AudioCommand::SetDefaultDevice {
                    device_id: device_id.clone(),
                });
                *slot.borrow_mut() = None;
                cx.notify();
            });

            let name_color = if is_default {
                theme::accent()
            } else {
                theme::text_primary()
            };
            // 自定义过名字的那台只显示自定义名：描述里往往带着系统原名的型号，
            // 摆出来等于把原名漏回面板。
            let label = self.cfg().device_label(&device.id, &device.name);
            let aliased = self.cfg().device_alias(&device.id).is_some();
            // 描述常常就是设备名的重复，重复时不再占一行。
            let description = (!aliased && device.description != device.name)
                .then(|| device.description.clone())
                .filter(|text| !text.is_empty());

            items = items.child(
                div()
                    .id(SharedString::from(format!("device-pick-{}", device.id)))
                    .h(self.su(DEVICE_MENU_ROW))
                    .px(px(8.))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .rounded(px(theme::RADIUS_SMALL))
                    .cursor_pointer()
                    .hover(|style| style.bg(theme::surface_hover()))
                    .on_mouse_down(MouseButton::Left, select)
                    .child(icon(
                        &device_icon_resolved(&self.cfg(), &device.id, device.kind),
                        15.0,
                        name_color,
                    ))
                    .child(
                        div()
                            .flex_1()
                            .min_w_0()
                            .flex()
                            .flex_col()
                            .gap(px(1.))
                            .child(
                                div()
                                    .text_size(self.fs(12.))
                                    .text_color(name_color)
                                    .whitespace_nowrap()
                                    .text_ellipsis()
                                    .child(label.clone()),
                            )
                            .when_some(description, |style, text| {
                                style.child(
                                    div()
                                        .text_size(self.fs(10.5))
                                        .text_color(theme::text_tertiary())
                                        .whitespace_nowrap()
                                        .text_ellipsis()
                                        .child(text),
                                )
                            }),
                    )
                    .when(is_default, |style| {
                        style.child(
                            div()
                                .size(px(6.))
                                .flex_none()
                                .rounded_full()
                                .bg(theme::accent()),
                        )
                    }),
            );
        }

        div()
            .absolute()
            .size_full()
            .on_mouse_down(MouseButton::Left, dismiss)
            .child(
                div()
                    .absolute()
                    .left(px(menu.x))
                    .top(px(menu.y))
                    .w(px(DEVICE_MENU_WIDTH))
                    .p(self.su(DEVICE_MENU_PAD))
                    .flex()
                    .flex_col()
                    .rounded(px(theme::RADIUS_SMALL + 2.0))
                    .bg(theme::surface_elevated())
                    .border_1()
                    .border_color(theme::border_strong())
                    .on_mouse_down(MouseButton::Left, |_, _, cx| cx.stop_propagation())
                    .child(items),
            )
            .into_any_element()
    }

    /// 底部的小图标按钮（22×22）。`active` 时图标用强调色、垫一层淡底。
    fn footer_icon_button(
        id: &'static str,
        name: &'static str,
        active: bool,
        onclick: impl Fn(&gpui::ClickEvent, &mut Window, &mut App) + 'static,
    ) -> AnyElement {
        let color = if active {
            theme::accent()
        } else {
            theme::text_tertiary()
        };
        let mut button = div()
            .id(id)
            .size(px(22.))
            .flex()
            .flex_none()
            .items_center()
            .justify_center()
            .rounded(px(theme::RADIUS_SMALL))
            .cursor_pointer()
            .hover(|style| style.bg(theme::surface_hover()))
            .on_click(onclick)
            .child(icon(name, 13.0, color));
        if active {
            button = button.bg(theme::accent_soft());
        }
        button.into_any_element()
    }

    fn footer(&self, cx: &mut Context<Self>) -> AnyElement {
        let lang = self.cfg().language;
        // 从左到右：混音器 · 设置 · 置顶。
        let mut actions = div().flex().items_center().gap(px(2.));
        actions = actions.child(
            div()
                .id("open-mixer")
                .h(px(22.))
                .px(px(8.))
                .flex()
                .items_center()
                .gap(px(5.))
                .rounded(px(theme::RADIUS_SMALL))
                .cursor_pointer()
                .hover(|style| style.bg(theme::surface_hover()))
                .on_click(|_, _, _| {
                    let _ = std::process::Command::new("sndvol.exe").spawn();
                })
                .child(icon("apps", 13.0, theme::text_tertiary()))
                .child(
                    div()
                        .text_size(self.fs(11.))
                        .text_color(theme::text_tertiary())
                        .child(lang.pick("混音器", "Mixer")),
                ),
        );
        if let Some(callback) = self.on_open_settings.clone() {
            actions = actions.child(Self::footer_icon_button(
                "open-settings",
                "settings",
                false,
                move |_, window, cx| callback(window, cx),
            ));
        }
        actions = actions.child(Self::footer_icon_button(
            "pin-flyout",
            "pin",
            self.pinned,
            cx.listener(|this: &mut Self, _, window, cx| {
                this.set_pinned(!this.pinned, window, cx);
            }),
        ));

        div()
            .h(self.su(theme::FOOTER_HEIGHT))
            .px(px(14.))
            .flex()
            .flex_none()
            .items_center()
            .justify_end()
            .border_t_1()
            .border_color(theme::border())
            .child(actions)
            .into_any_element()
    }
}

impl Flyout {
    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// 面板钉住了没有（钉住时失焦不自动收起）。
    pub fn is_pinned(&self) -> bool {
        self.pinned
    }

    /// 钉住 / 取消钉住。
    ///
    /// 钉住时把窗口压在最前（TOPMOST），取消时降回普通层级 —— 和按钮的外观
    /// （强调色 + 淡底）是同一份状态，不存在「看着钉着其实没钉」。
    fn set_pinned(&mut self, pinned: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.pinned = pinned;
        crate::windows::platform::set_topmost(window, pinned);
        if pinned {
            // 已经开着也要重新顶一下：别的置顶窗口可能正压在它上面。
            crate::windows::platform::bring_to_front(window);
        }
        cx.notify();
    }

    pub fn apply_and_refresh(
        &mut self,
        snapshot: Snapshot,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let changed = self.apply_snapshot(snapshot);
        if changed {
            if self.visible {
                let height = self.desired_height();
                // 只在高度真的变了才动窗口：拖动滑条时 snapshot 每 33ms 来一次，
                // 无条件 SetWindowPos 会让面板跟着抖。
                if (height - self.last_height).abs() > 0.5 {
                    self.update_frame(window, cx);
                }
            }
            cx.notify();
        }
        // 快照只有 30Hz，特效要 60Hz 才顺——有声音时补帧，静音自动停。
        self.kick_activity_ticker(cx);
    }

    /// 有没有任何一路在出声。特效层只在有声音时画，没声音就不用补帧。
    fn has_activity(&self) -> bool {
        if !self.config.borrow().get().activity_effect.animates() {
            return false;
        }
        self.snapshot
            .devices
            .iter()
            .any(|device| device.enabled && device.peak > 0.004)
            || self.snapshot.apps.iter().any(|app| app.peak > 0.004)
    }

    /// 特效补帧循环：只在面板可见且有声音时跑，其余时间没有任何定时器。
    ///
    /// 音频线程的快照是 33ms 一帧，直接拿它画，扫光 / 呼吸这类跟时间有关的特效
    /// 会一格一格地动；这里按 ~60Hz 补帧，电平本身也顺带平滑了。
    fn kick_activity_ticker(&mut self, cx: &mut Context<Self>) {
        if self.activity_ticker || !self.visible || !self.has_activity() {
            return;
        }
        self.activity_ticker = true;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor()
                    .timer(Duration::from_millis(16))
                    .await;
                let running = this
                    .update(cx, |this, cx| {
                        let running = this.visible && this.has_activity();
                        if !running {
                            this.activity_ticker = false;
                        }
                        cx.notify();
                        running
                    })
                    .unwrap_or(false);
                if !running {
                    break;
                }
            }
        })
        .detach();
    }

    pub fn show(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.visible = true;
        self.activity.reset();
        // 每次弹出都从列表顶部开始，别接着上次的滚动位置。
        self.apps_scroll.scroll_to_item(0);
        // 排序记忆也从头开始：这次一打开就正在响的应用，在下面第一次算列表时
        // 按系统顺序一次性挤到前面，之后新响的再往队尾追加。
        self.promoted.borrow_mut().clear();
        self.engine.send(AudioCommand::SetVisible(true));
        // 动画开关：设置里「弹出动画」关掉，或者系统要求减少动态效果，都直接铺满。
        let animate = self.config.borrow().get().popup_animation && !cx.reduce_motion();
        let config_ms = self.config.borrow().get().popup_animation_ms;
        self.anim.begin(animate, config_ms);
        // 窗口几何变更必须放到本帧绘制结束之后：鼠标事件是在绘制期间派发的，
        // 此时直接 SetWindowPos 会撞上 gpui 内部正在借用的渲染器。
        cx.defer_in(window, move |this, window, cx| {
            this.update_frame(window, cx);
            // 先整窗透明再显示：窗口一露头 DWM 就会画出圆角框和阴影，
            // 此时内容（动画第一帧）还没提交，不透明的话会闪一下空框。
            // 透明度随后跟着入场进度拉起来，见 `Entrance::tick`。
            this.anim.set_alpha(window, 0.0);
            crate::windows::platform::set_visible(window, true);
            crate::windows::platform::bring_to_front(window);
            crate::windows::platform::activate(window);
            if animate {
                // 从「真的显示出来」那一刻开始计时。
                this.anim.start();
                let session = this.anim.session();
                // 兜底：正常情况由逐帧回调推进，但窗口被遮挡 / 合成器不给帧时
                // 回调可能不续，到点强制收尾，绝不让面板停在半截高度。
                let deadline = this.anim.fallback_after();
                let view = cx.entity().downgrade();
                cx.spawn(async move |_, cx| {
                    cx.background_executor().timer(deadline).await;
                    let _ = view.update(cx, |this, cx| {
                        if this.anim.settle(session) {
                            cx.notify();
                        }
                    });
                })
                .detach();
            } else {
                // 关掉动画：内容已经完整摆好、缓存里就是完整画面，直接给满不透明度。
                // 顺序不能反 —— 反过来就又回到「不透明空窗先出现」。
                this.anim.set_alpha(window, 1.0);
            }
            cx.notify();
        });
    }

    pub fn hide(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.visible {
            return;
        }
        self.visible = false;
        self.last_hidden = Some(Instant::now());
        // 收起就解除钉住：下次打开回到普通面板，不会莫名其妙又压在最前面。
        if self.pinned {
            self.pinned = false;
            crate::windows::platform::set_topmost(window, false);
        }
        // 电平状态跟着面板一起清掉，下次打开从头开始量。
        self.activity.reset();
        // 「等快照追上」的挂起值同理：面板都不在了，留着只会在下次弹出时
        // 压着一条旧读数不放。
        self.release_hold.borrow_mut().clear();
        self.drag_values.borrow_mut().clear();
        *self.menu.borrow_mut() = None;
        *self.device_menu.borrow_mut() = None;
        self.engine.send(AudioCommand::SetVisible(false));
        // 隐藏前先把内容收成 1px 再画一帧：系统是拿「上一次提交的画面」
        // 来显示窗口的，缓存里若是完整面板，下次弹出会先闪一整块再开始动画。
        // 关掉动画（或系统要求减少动态效果）时反之要留完整画面，
        // 否则显示瞬间反而只有一条边，还得等下一帧才铺满。
        let animate = self.config.borrow().get().popup_animation && !cx.reduce_motion();
        let config_ms = self.config.borrow().get().popup_animation_ms;
        self.anim.begin(animate, config_ms);
        cx.notify();
        cx.defer_in(window, |this, window, _| {
            crate::windows::platform::set_visible(window, false);
            // 藏好之后保持全透明，**不要**复位成 1.0：
            // 窗口隐藏的这段时间里，DWM 手上就是「alpha = 0」这个属性；
            // 下次 ShowWindow 的第一帧因此也是全透明的。
            // 若复位成 1.0，DWM 会先用不透明状态合成一帧再收到新的 alpha，
            // 那一帧就是用户看到的「先闪一道黑边框」。
            // QcClip 的隐藏动画最后一帧同样停在 0（`visibility_animation_opacity`
            // 显示/隐藏都收敛到 0/1），从不复位。
            this.anim.set_alpha(window, 0.0);
        });
    }

    pub fn toggle(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if self.visible {
            self.hide(window, cx);
            return;
        }
        // 托盘图标那一下点击：失焦先把面板收起来，紧接着 Toggle 才到，
        // 此时 visible 已经是 false，不再挡一下就会「收不起来、又弹回去」。
        if let Some(hidden) = self.last_hidden {
            if hidden.elapsed() < TOGGLE_SUPPRESS {
                return;
            }
        }
        self.show(window, cx);
    }

    /// 面板贴右下角（任务栏上沿）定位，高度随内容精确变化。
    pub fn update_frame(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let (width, height) = self.panel_size();
        self.last_height = height;
        let area = cx
            .primary_display()
            .map(|display| display.visible_bounds())
            .unwrap_or_else(|| {
                gpui::Bounds::new(point(px(0.), px(0.)), size(px(1920.), px(1080.)))
            });
        // 面板贴任务栏所在的边，并沿垂直/水平方向对齐到屏幕右下角，
        // 与 EarTrumpet 在各任务栏位置下的表现一致。
        self.edge = crate::windows::platform::taskbar_edge();
        const MARGIN: f32 = 12.0;
        let right = area.origin.x + area.size.width;
        let bottom = area.origin.y + area.size.height;
        let left = area.origin.x;
        let top = area.origin.y;
        let mut origin = match self.edge {
            crate::windows::platform::TaskbarEdge::Top => {
                point(right - px(width) - px(MARGIN), top + px(MARGIN))
            }
            crate::windows::platform::TaskbarEdge::Bottom => point(
                right - px(width) - px(MARGIN),
                bottom - px(height) - px(MARGIN),
            ),
            crate::windows::platform::TaskbarEdge::Left => {
                point(left + px(MARGIN), bottom - px(height) - px(MARGIN))
            }
            crate::windows::platform::TaskbarEdge::Right => point(
                right - px(width) - px(MARGIN),
                bottom - px(height) - px(MARGIN),
            ),
        };
        // 亮度面板挨着面板摆的时候，音量面板得往「屏幕内侧」让出那块地方 ——
        // 但只有亮度窗要占的那一侧正好是本面板贴着的屏幕边时才需要让：
        //   任务栏在下 / 右 → 面板贴右下角，「右 / 下」外面没地方，往里让；
        //   任务栏在上     → 面板贴右上角，「上」外面没地方，往下让（亮度窗占上格）；
        //   任务栏在左     → 面板贴左下角，「左」外面没地方，往右让。
        // 反过来的情况（任务栏在上、亮度窗放下面）一格都不用挪：挪了反而会把它
        // 顶到工作区外面去（上半截被任务栏挡住）。
        let cfg = self.cfg();
        if cfg.brightness.enabled {
            const GAP: f32 = 8.0;
            let displays = crate::gui::display_count(cx);
            // 亮度面板多大由它自己那套排版算，这里只负责让出这块地方。
            let (brightness_width, brightness_height) =
                crate::windows::brightness::panel_size(&cfg, displays);
            // 面板贴哪两个边：任务栏在上 / 下 / 右时贴右边（右上 / 右下角），
            // 任务栏在左时贴左边；任务栏在上时贴的是上边，其余贴下边。
            let at_right = self.edge != TaskbarEdge::Left;
            let at_bottom = self.edge != TaskbarEdge::Top;
            match cfg.brightness.placement {
                crate::config::BrightnessPlacement::Right if at_right => {
                    origin.x -= px(brightness_width + GAP);
                }
                crate::config::BrightnessPlacement::Bottom if at_bottom => {
                    origin.y -= px(brightness_height + GAP);
                }
                // 任务栏在上：亮度窗要占最上面那格，音量面板往下让同样的距离。
                crate::config::BrightnessPlacement::Top if self.edge == TaskbarEdge::Top => {
                    origin.y += px(brightness_height + GAP);
                }
                _ => {}
            }
        }
        crate::windows::platform::set_frame(
            window,
            gpui::Bounds::new(origin, size(px(width), px(height))),
            window.scale_factor(),
        );
        // 记一笔自己的位置：亮度面板要贴着这一侧摆。
        crate::gui::set_flyout_frame(cx, (origin.x.as_f32(), origin.y.as_f32(), width, height));
        // 原生窗口尺寸变了 gpui 的 viewport 缓存不一定跟着变：不显式通知，
        // 面板下半截会留在上一次的尺寸里不重绘（QcClip 在同一位置也是这么做的）。
        window.bounds_changed(cx);
    }
}

impl Render for Flyout {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let lang = self.cfg().language;
        let vertical = self.cfg().panel_layout.is_vertical();
        let current = self.current_device().cloned();
        let device_row = match &current {
            Some(device) => self.device_row(device, cx),
            None => self.empty_device_row(lang),
        };

        let apps = self.visible_apps();
        let mut rows: Vec<AnyElement> = if vertical {
            Vec::new()
        } else {
            apps.iter()
                .map(|app| self.app_row(app, window, cx))
                .collect()
        };

        // 被折叠的应用收在列表最下面，点标题展开。
        let hidden = self.hidden_apps();
        if !vertical && !hidden.is_empty() {
            rows.push(self.hidden_toggle_row(hidden.len(), cx));
            if self.show_hidden {
                rows.extend(hidden.iter().map(|app| self.app_row(app, window, cx)));
            }
        }

        let rows: Vec<AnyElement> = if rows.is_empty() {
            vec![
                div()
                    .h(px(self.row_height()))
                    .px(px(16.))
                    .flex()
                    .items_center()
                    .gap(px(8.))
                    .child(icon("music", 15.0, theme::text_tertiary()))
                    .child(
                        div()
                            .text_size(self.fs(11.5))
                            .text_color(theme::text_tertiary())
                            .child(lang.pick("没有可调节的应用会话", "No app sessions")),
                    )
                    .into_any_element(),
            ]
        } else {
            rows
        };

        // 竖向排版：设备一列 + 各应用一列，横着排开。
        let columns: Vec<AnyElement> = if vertical {
            let mut columns: Vec<AnyElement> = Vec::new();
            match &current {
                Some(device) => {
                    columns.push(self.device_channel(device, cx));
                    // 设备和其他应用之间拉一条竖线，跟横排那条分隔线一个意思。
                    columns.push(
                        div()
                            .w(px(1.))
                            .h_full()
                            .flex_none()
                            .bg(theme::border())
                            .into_any_element(),
                    );
                }
                None => columns.push(self.empty_device_row(lang)),
            }
            columns.extend(apps.iter().map(|app| self.app_channel(app, window, cx)));
            columns
        } else {
            Vec::new()
        };

        let overlay = self
            .menu
            .borrow()
            .clone()
            .map(|menu| self.context_menu(menu, cx));

        // 设备切换菜单也是浮层：挂在内容最上层，不改窗口尺寸，面板位置纹丝不动。
        let device_overlay = self
            .device_menu
            .borrow()
            .clone()
            .map(|menu| self.device_menu_view(menu, cx));

        // 入场动画：窗口始终按完整高度摆好，只让内容在窗口里从下往上「长」出来。
        // 不去改原生窗口尺寸，是因为 gpui 的 viewport 缓存会慢一帧，
        // 边改尺寸边绘制时面板下半截会被裁在上一次的尺寸里。
        // 面板没显示时不动窗口透明度，免得和 hide 里的复位互相打架。
        let progress = if self.visible {
            self.anim.tick(window)
        } else {
            self.anim.progress()
        };
        let full_height = self.desired_height();
        let visible_height = anim::anim_height(progress, full_height);
        // 任务栏在左/右时改成从那一侧横向拉开：宽度插值，内容按原尺寸被裁切。
        let full_width = self.panel_width();
        let visible_width = (full_width * progress).clamp(0.0, full_width);

        // 圆角由内容自己裁：关掉 DWM 非客户区渲染后系统不再做窗口圆角，
        // 这里用 rounded + overflow_hidden 顶上，四角外保持透明。
        // 字体族 / 字重挂在根节点上，靠继承往下传（和设置窗根节点一个做法）。
        // 只在用户真的选了非默认值时才写这一条，少一层逐文本覆盖。
        let panel_cfg = self.cfg();
        let spacing = self.letter_spacing();
        let content = div()
            .size_full()
            .flex()
            .flex_col()
            // 根底色用不透明版：主题的 `surface` 留了 3% 透光，不透明度给到 100%
            // 时还会隐隐透出桌面。要透多少由 `ui_opacity` 一个人说了算。
            .bg(theme::surface_window())
            .rounded(px(theme::RADIUS_WINDOW))
            .overflow_hidden()
            .when_some(panel_font_family(&panel_cfg), |style, family| {
                style.font_family(family)
            })
            .when_some(panel_font_weight(&panel_cfg), |style, weight| {
                style.font_weight(weight)
            })
            // 字间距也是文本样式，挂根节点整棵树继承（子节点只覆盖字号，不会盖掉它）。
            .when(spacing != px(0.), |style| style.letter_spacing(spacing));

        let content = if vertical {
            content
                .child(
                    div()
                        .flex_1()
                        .min_h_0()
                        .flex()
                        .flex_row()
                        .items_start()
                        .gap(px(V_COLUMN_GAP))
                        .px(self.su(V_PAD))
                        .py(self.su(V_PAD))
                        .children(columns),
                )
                .child(self.footer(cx))
                .children(overlay)
                .children(device_overlay)
        } else {
            content
                .child(device_row)
                .child(div().h(px(1.)).flex_none().bg(theme::border()))
                .child(self.apps_header())
                .child(
                    div()
                        .id("apps-list")
                        .flex_1()
                        .min_h_0()
                        .flex()
                        .flex_col()
                        .overflow_y_scroll()
                        .track_scroll(&self.apps_scroll)
                        .on_scroll_wheel(cx.listener(Self::on_apps_wheel))
                        .children(rows),
                )
                .child(self.footer(cx))
                .children(overlay)
                .children(device_overlay)
        };

        // 面板本体：垂直布局按高度展开，水平布局按宽度展开。
        let panel = match self.edge {
            TaskbarEdge::Bottom => div()
                .absolute()
                .left_0()
                .right_0()
                .top(px(visible_height - full_height))
                .h(px(full_height))
                .child(content),
            TaskbarEdge::Top => div()
                .absolute()
                .left_0()
                .right_0()
                .top_0()
                .h(px(full_height))
                .child(content),
            TaskbarEdge::Left => div()
                .absolute()
                .top_0()
                .bottom_0()
                .left_0()
                .w(px(full_width))
                .child(content),
            TaskbarEdge::Right => div()
                .absolute()
                .top_0()
                .bottom_0()
                .left(px(visible_width - full_width))
                .w(px(full_width))
                .child(content),
        };

        // 裁切层：贴住任务栏那条边，另一侧随进度让开。
        let clip = match self.edge {
            TaskbarEdge::Bottom => div()
                .absolute()
                .left_0()
                .right_0()
                .bottom_0()
                .h(px(visible_height)),
            TaskbarEdge::Top => div()
                .absolute()
                .left_0()
                .right_0()
                .top_0()
                .h(px(visible_height)),
            TaskbarEdge::Left => div()
                .absolute()
                .top_0()
                .bottom_0()
                .left_0()
                .w(px(visible_width)),
            TaskbarEdge::Right => div()
                .absolute()
                .top_0()
                .bottom_0()
                .right_0()
                .w(px(visible_width)),
        };

        div()
            .size_full()
            .relative()
            .child(clip.overflow_hidden().child(panel))
    }
}

/// 面板上的音量读数。
///
/// 线性模式只有一个百分比；对数模式照 EarTrumpet 显示 dB，但百分比也要能看见，
/// 所以并排写在 dB 后面：`-20.0 dB · 10%`。
fn volume_readout(display: f32, logarithmic: bool, min_db: f32) -> String {
    if !logarithmic {
        return volume_label(display, false, min_db);
    }
    let percent = audio::percent_from_display(display, true, min_db);
    format!("{} · {percent}%", volume_label(display, true, min_db))
}

/// 把滚轮事件折算成「方向」：+1 / -1 / 0。
///
/// gpui 在 Windows 上把 `WM_MOUSEWHEEL` 换算成「行」时已经乘过系统的每次滚动
/// 行数（默认 3），一格滚轮拿到的是 3.0 而不是 1.0 —— 直接当格数用，一格就滚了
/// 3 个步进。所以只取方向，步进交给配置里的滚轮步进。
/// 设备名按括号拆成两行：`扬声器 (Realtek(R) Audio)` →
/// 第一行 `扬声器`，第二行 `(Realtek(R) Audio)`。
///
/// 从**第一个**左括号切：型号描述总是从那个括号开始的，里面再套括号（Realtek(R)）
/// 都属于第二行。全角括号一并认。
/// 给应用图标右上角加一个小圆点：这个应用被单独指定了播放设备。
///
/// EarTrumpet 没有这个标记（它只在右键菜单里打勾），这里补一个 ——
/// 面板上一眼就能看出「哪几个应用没走默认设备」。gpui 的父盒子不裁子元素，
/// 圆点正好跨在图标边线上，看着像个角标。
fn with_route_badge(element: AnyElement, routed: bool) -> AnyElement {
    if !routed {
        return element;
    }
    div()
        .relative()
        .flex_none()
        .child(element)
        .child(
            div()
                .absolute()
                .right(px(-1.))
                .top(px(-1.))
                .size(px(ROUTE_BADGE))
                .rounded_full()
                .bg(theme::accent())
                .border_1()
                .border_color(theme::surface_elevated()),
        )
        .into_any_element()
}

fn split_device_name(name: &str) -> (String, Option<String>) {
    let Some(index) = name.find(['(', '（']) else {
        return (name.trim().to_string(), None);
    };
    let head = name[..index].trim_end();
    let tail = name[index..].trim();
    if head.is_empty() || tail.len() <= 1 {
        return (name.trim().to_string(), None);
    }
    (head.to_string(), Some(tail.to_string()))
}

/// 量一段文字在面板里的实际宽度（用窗口当前字体，字号与应用名一致）。
fn measure_text_width(text: &str, window: &mut Window) -> f32 {
    let font = window.text_style().font();
    let run = TextRun {
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
        .layout_line(text, px(12.5), &[run], None)
        .width
        .as_f32()
}

/// 滑条取值之后要不要「顺手解除静音」。
///
/// `previous` 是**同一次手势里**上一次的位置（拖动中才有），`current` 是这次手势
/// 开始前滑条显示的读数。一次手势的第一下（鼠标按下、点一下跳到某处）还没有
/// `previous`，就拿 `current` 当基准 —— 点滑条同样是调音量，静音状态下该解除就
/// 得解除；只有真的没挪动（按在原处又原地松手）才不动静音。
///
/// 以前这里要求 `previous` **必须存在**，于是「点一下」永远算不出方向、
/// 永远不解除静音：拖动能出声、点一下不出声，正是用户报的那个不一致。
fn slider_unmute(
    cfg: &Config,
    gesture: crate::config::VolumeGesture,
    previous: Option<f32>,
    current: f32,
    value: f32,
) -> bool {
    let from = previous.unwrap_or(current);
    from != value && cfg.unmute.allows(gesture, value > from)
}

#[cfg(test)]
mod tests {
    use super::{
        HOLD_SETTLE, MUTE_OVERRIDE_FOR, MuteNote, ReleaseNote, V_FADER_HEIGHT, V_FADER_MIN,
        VOLUME_HOLD_FOR, note_muted, resolve_held_volume, resolve_muted, shown_muted,
        slider_unmute, split_device_name, vertical_fader_height, vertical_panel_height,
        volume_readout,
    };
    use crate::config::{Config, VolumeGesture};

    /// 点一下滑条（同一次手势的第一下，没有「上一次」）也要按方向解除静音。
    ///
    /// 这就是用户报的「拖动能解除静音、点击不能」：以前这里要求 `previous` 存在，
    /// 单击永远算不出方向，于是静音一直留着。
    #[test]
    fn click_on_slider_unmutes_like_drag() {
        let mut cfg = Config::default();
        cfg.unmute.enabled = true;
        // 静音中、读数 30%：点在 60% 处 = 往上调，解除静音。
        assert!(slider_unmute(&cfg, VolumeGesture::Drag, None, 0.3, 0.6));
        // 点回原处：没挪动，不动静音。
        assert!(!slider_unmute(&cfg, VolumeGesture::Drag, None, 0.3, 0.3));
    }

    /// 拖动中仍按「上一次的位置」判方向，不受手势起点影响。
    #[test]
    fn drag_direction_uses_previous_step() {
        let mut cfg = Config::default();
        cfg.unmute.enabled = true;
        cfg.unmute.increase_drag = true;
        cfg.unmute.decrease_drag = false;
        // 50% → 60%：往上，解除。
        assert!(slider_unmute(
            &cfg,
            VolumeGesture::Drag,
            Some(0.5),
            0.3,
            0.6
        ));
        // 50% → 40%：往下，这条开关关着，不解。
        assert!(!slider_unmute(
            &cfg,
            VolumeGesture::Drag,
            Some(0.5),
            0.3,
            0.4
        ));
    }

    /// 应用滑条走自己那两条开关，别和设备的串了。
    #[test]
    fn app_slider_uses_its_own_switches() {
        let mut cfg = Config::default();
        cfg.unmute.enabled = true;
        cfg.unmute.increase_app_drag = false;
        assert!(!slider_unmute(&cfg, VolumeGesture::AppDrag, None, 0.2, 0.8));
        assert!(slider_unmute(&cfg, VolumeGesture::Drag, None, 0.2, 0.8));
    }

    /// 总开关关掉就一律不解除。
    #[test]
    fn disabled_unmute_never_fires() {
        let mut cfg = Config::default();
        cfg.unmute.enabled = false;
        assert!(!slider_unmute(&cfg, VolumeGesture::Drag, None, 0.3, 0.9));
        assert!(!slider_unmute(
            &cfg,
            VolumeGesture::Drag,
            Some(0.3),
            0.3,
            0.9
        ));
    }

    #[test]
    fn vertical_height_is_optional_and_grows_the_fader() {
        let mut config = Config::default();
        // 0 = 自动：按内容算，推子就是默认长度。
        let auto = vertical_panel_height(&config);
        assert!((vertical_fader_height(&config) - V_FADER_HEIGHT).abs() < 0.01);
        assert!(auto > V_FADER_HEIGHT);

        // 写数字：面板就是那么高，多出来的全给推子。
        config.vertical_height = 300;
        assert_eq!(vertical_panel_height(&config), 300.0);
        assert!((vertical_fader_height(&config) - (V_FADER_HEIGHT + (300.0 - auto))).abs() < 0.01);

        // 写得太小也留得住一条推子（高度被抬到下限附近）。
        config.vertical_height = 1;
        assert!(vertical_fader_height(&config) >= V_FADER_MIN);
        assert!(vertical_panel_height(&config) >= 140.0);
    }

    #[test]
    fn device_name_splits_after_parenthesis() {
        assert_eq!(
            split_device_name("ROG XG27AQNGV (High Definition Audio Device)"),
            (
                "ROG XG27AQNGV".to_string(),
                Some("(High Definition Audio Device)".to_string())
            )
        );
        // 全角括号也认。
        assert_eq!(
            split_device_name("扬声器（Realtek(R) Audio）"),
            (
                "扬声器".to_string(),
                Some("（Realtek(R) Audio）".to_string())
            )
        );
        // 没有括号 / 括号在开头：保持一行。
        assert_eq!(
            split_device_name("Speakers"),
            ("Speakers".to_string(), None)
        );
        assert_eq!(
            split_device_name("(USB Audio)"),
            ("(USB Audio)".to_string(), None)
        );
    }

    #[test]
    fn logarithmic_readout_keeps_percent_next_to_db() {
        assert_eq!(volume_readout(0.62, false, -40.0), "62%");
        assert_eq!(volume_readout(0.5, true, -40.0), "-20.0 dB · 10%");
        assert_eq!(volume_readout(1.0, true, -40.0), "0.0 dB · 100%");
    }

    /// 造一笔「已经过了静默期」的音量记录。
    fn settled_release(want: f32) -> ReleaseNote {
        let mut note = ReleaseNote::new(want);
        note.at = std::time::Instant::now() - HOLD_SETTLE - std::time::Duration::from_millis(20);
        note
    }

    /// 造一笔「已经过了静默期」的静音记录。
    fn settled_mute(want: bool) -> MuteNote {
        let mut note = MuteNote::new(want);
        note.at = std::time::Instant::now() - HOLD_SETTLE - std::time::Duration::from_millis(20);
        note
    }

    /// 松手后快照还停在旧值时，必须继续顶着用户要的值 —— 这一条就是「横跳」本身。
    #[test]
    fn held_volume_keeps_target_while_snapshot_is_stale() {
        let mut note = settled_release(0.65);
        assert_eq!(resolve_held_volume(&mut note, 0.49), Some(0.65));
        assert_eq!(resolve_held_volume(&mut note, 0.49), Some(0.65));
    }

    /// 设备量化（只报 1% 一档）：回读 0.65、目标 0.654，也得继续顶。
    ///
    /// 拿「接近了」当交还条件，就会在迟到的旧命令落地后跳一下 ——
    /// 快照「看起来到位」在写入还没真正落地时说明不了什么。
    #[test]
    fn held_volume_keeps_target_on_quantized_snapshot() {
        let mut note = settled_release(0.654);
        assert_eq!(resolve_held_volume(&mut note, 0.65), Some(0.654));
        // 哪怕快照恰好等于目标值，记录也留到超时：那段窗口是用来兜迟到命令的。
        assert_eq!(resolve_held_volume(&mut note, 0.654), Some(0.654));
    }

    /// 快照「动了一半」（0.49 → 0.5）也不交还。
    #[test]
    fn held_volume_swallows_midway_snapshot() {
        let mut note = settled_release(0.65);
        assert_eq!(resolve_held_volume(&mut note, 0.49), Some(0.65));
        assert_eq!(resolve_held_volume(&mut note, 0.5), Some(0.65));
    }

    /// 手势刚完的那一小段，快照说什么都不认（命令还在队列里）。
    #[test]
    fn held_volume_ignores_snapshot_during_settle() {
        let mut note = ReleaseNote::new(0.65);
        assert_eq!(resolve_held_volume(&mut note, 0.65), Some(0.65));
    }

    /// 写入被设备拒绝、快照一动不动：顶到上限就让位，面板不能一直挂着假值。
    #[test]
    fn held_volume_expires_when_snapshot_never_moves() {
        let mut note = ReleaseNote::new(0.9);
        note.at = std::time::Instant::now() - VOLUME_HOLD_FOR - std::time::Duration::from_millis(1);
        assert_eq!(resolve_held_volume(&mut note, 0.2), None);
    }

    /// 交还判据是「快照对上了」：对不上就一直顶（快照还是旧的），
    /// 一但引擎受理、快照翻成用户要的值，立刻让位 —— 之后两边画的是同一个值。
    #[test]
    fn mute_override_releases_once_snapshot_agrees() {
        let mut note = settled_mute(false);
        assert_eq!(resolve_muted(&mut note, true), Some(false));
        assert_eq!(resolve_muted(&mut note, false), None);
    }

    /// 静音 → **立刻**解除：前一条命令晚一步才被受理，中间快照会一直停在 `true`。
    /// 整段都得顶着 —— 交还一次就是一个显示循环。
    #[test]
    fn mute_override_swallows_quick_toggle_bounce() {
        let mut note = MuteNote::new(false);
        for _ in 0..10 {
            assert_eq!(resolve_muted(&mut note, true), Some(false));
        }
        // 引擎受理之后快照才是新值。
        note.at = std::time::Instant::now() - HOLD_SETTLE - std::time::Duration::from_millis(20);
        assert_eq!(resolve_muted(&mut note, false), None);
    }

    /// 连按五下：快照在这期间一动不动，显示必须跟着**最后一下**的意图走，
    /// 而且 toggle 的基准也得是显示值（拿快照当基准会连发五条一样的命令）。
    #[test]
    fn rapid_burst_display_follows_last_click() {
        use std::cell::RefCell;
        use std::rc::Rc;
        let map: Rc<RefCell<std::collections::HashMap<String, MuteNote>>> =
            Rc::new(RefCell::new(std::collections::HashMap::new()));
        let real = false; // 引擎还在忙，快照停在「未静音」
        let mut shown = real;
        for _ in 0..5 {
            let want = !shown;
            note_muted(&map, "dev", want);
            shown = shown_muted(&map, "dev", real);
            assert_eq!(shown, want);
        }
    }

    /// 手势刚完的那一小段，连「快照对上了」都不认（命令可能还在队列里）。
    #[test]
    fn mute_override_ignores_agreement_during_settle() {
        let mut note = MuteNote::new(false);
        assert_eq!(resolve_muted(&mut note, false), Some(false));
    }

    #[test]
    fn mute_override_expires() {
        let mut note = MuteNote::new(false);
        note.at =
            std::time::Instant::now() - MUTE_OVERRIDE_FOR - std::time::Duration::from_millis(1);
        assert_eq!(resolve_muted(&mut note, true), None);
    }
}
