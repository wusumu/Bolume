//! 屏幕提示条（OSD）：调音量 / 静音 / 切歌时在屏幕上闪一下。
//!
//! 和面板不是一回事：这扇窗**永远不抢焦点**（`WS_EX_NOACTIVATE`）、不进 Alt+Tab、
//! 始终置顶，所以放在游戏上面也不会让游戏失焦。
//!
//! 样式全部来自 `config.osd`：卡片尺寸、圆角、透明度、贴哪儿、元素怎么摆，
//! 都由设置页里的可视化编辑器直接改。这里的 `card_visual` 同时给真实窗口和
//! 设置页的预览用 —— 两处必须长得一模一样，否则「所见非所得」。

use std::{
    cell::RefCell,
    rc::Rc,
    time::{Duration, Instant},
};

use gpui::prelude::*;
use gpui::{
    AnyElement, App, Context, Div, Pixels, Render, ScrollWheelEvent, Window, div, point, px, size,
};

use crate::{
    audio::{
        AppInfo, AudioCommand, AudioEngine, DeviceInfo, Snapshot, amplitude_to_db,
        to_display_volume, to_log_volume,
    },
    config::{Config, ConfigStore, OsdAlign, OsdAnchor, OsdConfig, OsdFunction, OsdPart},
    i18n::Lang,
    ui::{
        SliderDragState, VolumeSlider, anim::SystemMotion, icon, theme, wheel_direction,
    },
};

/// 卡片里长文本（标题 / 副标题）右边至少留多少空隙。
/// 卡片右侧留白：元素最多铺到「卡片宽 - 这个值」。
///
/// 编辑器的拖动 / 吸附都按它算上限 —— 否则拖到最右边时
/// `part_rect` 会用更小的 `available` 把宽度压窄，看起来就是「挤压」。
pub const PAD_RIGHT: f32 = 10.0;

/// 动画的推进间隔（250Hz 的定时器，实际受系统计时精度限制）。
///
/// 以前是 16ms（≈60Hz），但那种节奏下「滑上来」看着明显一格一格的 —— 系统那条
/// 音量条是按合成帧给的，比 60Hz 顺得多。这里 4ms 一拍，且动画期间只挪窗口 +
/// 改整窗不透明度（不重绘内容），所以加密不会带来额外绘制开销。
const TICK: Duration = Duration::from_millis(4);

/// 这一次要不要走入场动画。
///
/// 系统风格不看两个时长设置（节奏由系统那套定），只看总开关；
/// 自己那套还是「时长为 0 就别卡在 In 里」。
fn animates_in(cfg: &OsdConfig) -> bool {
    cfg.animate && (cfg.anim_style.is_system() || cfg.fade_in_ms > 0)
}

/// 这块屏的整屏底部（逻辑像素，含任务栏占的那一条）。
fn screen_bottom(monitor: u32) -> f32 {
    let list = crate::platform::monitors::monitors();
    let chosen = if monitor == 0 {
        list.iter()
            .find(|info| info.is_primary)
            .or_else(|| list.first())
    } else {
        list.get(monitor as usize - 1)
            .or_else(|| list.iter().find(|info| info.is_primary))
    };
    match chosen {
        Some(info) => (info.bounds.1 + info.bounds.3).max(info.work.1 + info.work.3),
        None => 1080.0,
    }
}

/// 系统风格下这一轮要从屏幕底往上滑多远（不是系统风格就是 0）。
///
/// 起点让**窗口顶边贴到屏幕底**：整块卡片完全在屏幕外，往上滑的路上被任务栏
/// 挡住（配合 `behind_taskbar`），才是系统那种「从任务栏后面出来」的样子。
/// 以前按「卡片高度 + 10」算，卡片矮的时候起点还留在屏幕内，看起来就是从半空冒出来。
fn system_slide(cfg: &OsdConfig, monitor: u32, window_top: f32) -> f32 {
    if cfg.anim_style.is_system() {
        (screen_bottom(monitor) - window_top).max(0.0)
    } else {
        0.0
    }
}

/// OSD 当前显示的是哪一路音量。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum OsdTarget {
    /// 默认播放设备。
    Device,
    /// 某个应用的会话（快捷键调「当前应用」时用）。
    App { key: String, name: String },
}

/// 一次提示请求。
#[derive(Clone, Debug, PartialEq)]
pub struct OsdRequest {
    pub function: OsdFunction,
    pub target: OsdTarget,
    /// 调用方算好的新读数（滑条位置 0..1）。
    ///
    /// 快捷键 / 滚轮调完音量时，音频快照还要 33ms 才回来；不传这个的话
    /// 提示条会先显示旧值、下一帧再跳一下。传了就一次到位。
    pub predicted: Option<f32>,
    /// 调用方刚切的静音状态：静音键按下时快照还没回来，
    /// 不传的话提示条会先显示成「取消静音」再翻过来。
    pub predicted_muted: Option<bool>,
    /// 播放 / 暂停键这次按下去之后是「在播」还是「停了」。
    ///
    /// 两个动作共用一个键，光看键分不出来，得靠系统媒体会话（SMTC）问一次。
    /// `None` 就是问不出来，退回中性的「播放 / 暂停」。
    pub playing: Option<bool>,
}

impl OsdRequest {
    pub fn device(function: OsdFunction) -> Self {
        Self {
            function,
            target: OsdTarget::Device,
            predicted: None,
            predicted_muted: None,
            playing: None,
        }
    }

    pub fn app(function: OsdFunction, key: String, name: String) -> Self {
        Self {
            function,
            target: OsdTarget::App { key, name },
            predicted: None,
            predicted_muted: None,
            playing: None,
        }
    }

    /// 带上「刚切成什么静音状态」。
    pub fn muted(mut self, muted: bool) -> Self {
        self.predicted_muted = Some(muted);
        self
    }

    /// 带上「这一键按完是在播还是停了」。
    pub fn playing(mut self, playing: bool) -> Self {
        self.playing = Some(playing);
        self
    }

    /// 带上「调完之后应该是什么读数」。
    pub fn predicted(mut self, display: f32) -> Self {
        self.predicted = Some(display.clamp(0.0, 1.0));
        self
    }
}

/// 提示条此刻要画的内容。
#[derive(Clone, Debug, PartialEq)]
pub struct OsdState {
    pub function: OsdFunction,
    pub target: OsdTarget,
    pub icon: &'static str,
    /// 主行：总体信息模式是设备名 / 应用名，操作模式是动作名。
    pub title: String,
    /// 副行（总体信息模式才有）：正在发声的应用、静音状态等。
    pub subtitle: String,
    /// 百分比读数：「42%」。和分贝是两个独立元素，各自显示。
    pub percent: String,
    /// 分贝读数：「-12.0 dB」。
    pub db: String,
    /// 音量条进度（0..1）。
    pub fraction: f32,
    /// 画不画音量条。
    pub show_bar: bool,
    /// 当前是不是静音（决定点击时切到哪一边）。
    pub muted: bool,
    /// 播放 / 暂停键这次按完是在播还是停了（`None` = 问不出来）。
    pub playing: Option<bool>,
}

impl Default for OsdState {
    fn default() -> Self {
        Self {
            function: OsdFunction::Volume,
            target: OsdTarget::Device,
            icon: "volume-high",
            title: String::new(),
            subtitle: String::new(),
            percent: String::new(),
            db: String::new(),
            fraction: 0.0,
            show_bar: true,
            muted: false,
            playing: None,
        }
    }
}

/// 音量条的交互绑定：真实窗口给一份，设置页的预览不给。
///
/// 这里的拖拽 / 滚轮直接复用面板那套 `VolumeSlider`（同一份按下、拖动、
/// 抬起逻辑，同一套对数换算），只是不画发声电平。
pub struct OsdBar {
    pub drag: SliderDragState,
    pub on_change: Rc<dyn Fn(f32, &mut Window, &mut App)>,
    pub on_release: Rc<dyn Fn(&mut Window, &mut App)>,
}

/// 滑条自己的触摸高度（`ui::slider` 里固定 24），摆的时候要按它居中。
const BAR_TOUCH_HEIGHT: f32 = 24.0;

/// 卡片尺寸（逻辑像素）。
///
/// 上下限和编辑器里的数值框、拖边手柄**必须一致**：以前这里单独夹到 160×40，
/// 于是把宽度拖到 120 时界面显示 120、实际画出来却是 160，尺寸读数全是假话。
pub fn card_size(cfg: &OsdConfig) -> (f32, f32) {
    (
        cfg.width.clamp(120, 900) as f32,
        cfg.height.clamp(28, 320) as f32,
    )
}

/// 元素高度与它自己的 `size` 之间的倍数。
///
/// 图标 / 音量条的 `size` 就是高度本身；标题是两行（标题 + 副标题），
/// 读数是单行但留了行距。设置页拖角改大小时靠它把「拖了多少像素」
/// 换算成「字号 / 厚度该加多少」，和 `part_rect` 用的是同一组数字。
pub fn height_factor(part: OsdPart) -> f32 {
    match part {
        OsdPart::Icon | OsdPart::Bar => 1.0,
        OsdPart::Title => 2.2,
        OsdPart::Percent | OsdPart::Db | OsdPart::Text(_) => 1.6,
    }
}

/// 元素的高度（逻辑像素）：图标 / 音量条就是 size，文字是字号 × 倍数。
pub fn part_height(part: OsdPart, size: f32) -> f32 {
    match part {
        OsdPart::Icon | OsdPart::Bar => size,
        OsdPart::Title => (size * height_factor(part)).max(24.0),
        OsdPart::Percent | OsdPart::Db | OsdPart::Text(_) => (size * height_factor(part)).max(12.0),
    }
}

/// 某个元素在卡片里的矩形（左上角 x/y + 宽高，未缩放）。
///
/// 尺寸全部来自该元素自己的 `size`（图标边长 / 字号 / 音量条厚度），
/// 位置来自归一化坐标。标题 / 读数 / 音量条从各自的 x 一直铺到卡片右边距：
/// 拖动改的是左边缘，「往右拖」不会把内容拖出卡片。
pub fn part_rect(cfg: &OsdConfig, part: OsdPart) -> (f32, f32, f32, f32) {
    let (width, height) = card_size(cfg);
    let position = cfg.part(part);
    let x = position.x.clamp(0.0, 1.0) * width;
    let y = position.y.clamp(0.0, 1.0) * height;
    let size = position.size as f32;
    // 自动宽度铺到「右内边距」为止；**用户拖过的固定宽度可以一直贴到卡片右边缘**
    // —— 左边能贴 0，右边也得能贴满，否则拖到最右会有一条看不见的线拦着。
    let available_auto = (width - x - PAD_RIGHT).max(30.0);
    let available_explicit = (width - x).max(30.0);
    // width = 0 是「自动」：铺到卡片右边距。用户拖过之后就是一个固定宽度，
    // 这样拖动框体才是整块平移，而不是一直被右边距拉着拉伸。
    let explicit = position.width > 0;
    let sized = if explicit {
        (position.width as f32).min(available_explicit)
    } else {
        available_auto
    };
    let available = if explicit {
        available_explicit
    } else {
        available_auto
    };
    match part {
        OsdPart::Icon => (x, y, size, size),
        OsdPart::Title => {
            // 自动宽度时才给读数让位：用户自己拖过的宽度就是他的决定。
            let width = if explicit {
                sized
            } else {
                let limit = [OsdPart::Percent, OsdPart::Db]
                    .iter()
                    .map(|other| cfg.part(*other))
                    .filter(|other| other.visible)
                    .map(|other| other.x.clamp(0.0, 1.0) * width)
                    .filter(|other_x| *other_x > x + 40.0)
                    .fold(f32::MAX, f32::min);
                if limit.is_finite() {
                    sized.min(limit - x - 8.0)
                } else {
                    sized
                }
            };
            (x, y, width.max(30.0), part_height(part, size))
        }
        OsdPart::Percent | OsdPart::Db => {
            let width = if explicit { sized } else { sized.min(140.0) };
            // 100% 比 42% 多一位：宽度至少得放得下最长的那个读数，
            // 否则数值一变就被截掉半个字。
            let worst = size * if part == OsdPart::Percent { 2.6 } else { 5.2 };
            (
                x,
                y,
                width.max(worst).min(available),
                part_height(part, size),
            )
        }
        OsdPart::Bar => (x, y, sized, part_height(part, size)),
        // 自定义文字：单行，宽度就是自己的宽度（自动 = 铺到右边距）。
        OsdPart::Text(_) => {
            let width = if explicit { sized } else { sized.min(200.0) };
            (x, y, width.max(30.0), part_height(part, size))
        }
    }
}

/// 设置页编辑器里的虚线框：比元素本身更贴近看得见的范围。
///
/// 标题 / 读数在布局里是「从 x 一直铺到右边距」的容器，直接拿它画框会两个框叠在一起，
/// 拖起来分不清谁是谁。这里按内容宽度收窄；音量条上下各放宽 3px 方便抓。
pub fn editor_rect(cfg: &OsdConfig, part: OsdPart) -> (f32, f32, f32, f32) {
    let (x, y, width, height) = part_rect(cfg, part);
    let position = cfg.part(part);
    let size = position.size as f32;
    // 自动宽度时才按内容收窄 —— 两个框才不会叠在一起、分不清拖的是谁。
    // 宽度被用户拖过之后，框就老老实实等于元素本身。
    let auto = position.width == 0;
    match part {
        // 音量条本体太细（默认 6px），四角各放一个手柄后中间就空了；
        // 编辑框上下各让 6px，中间才留得出一条能抓的位置。
        OsdPart::Bar => {
            let (_, card_h) = card_size(cfg);
            let top = (y - 6.0).max(0.0);
            let bottom = (y + height + 6.0).min(card_h);
            (x, top, width, (bottom - top).max(12.0))
        }
        OsdPart::Title => (x, y, if auto { width.min(170.0) } else { width }, height),
        OsdPart::Percent => (
            x,
            y,
            if auto {
                width.min(size * 4.2).max(48.0)
            } else {
                width
            },
            height,
        ),
        OsdPart::Db => (
            x,
            y,
            if auto {
                width.min(size * 5.0).max(48.0)
            } else {
                width
            },
            height,
        ),
        OsdPart::Text(_) => (
            x,
            y,
            if auto {
                width.min(size * 8.0).max(40.0)
            } else {
                width
            },
            height,
        ),
        OsdPart::Icon => (x, y, width, height),
    }
}

/// 元素在当前位置还能有多宽（拉到卡片右边缘为止；左边能贴 0，右边也对称）。
pub fn max_part_width(cfg: &OsdConfig, part: OsdPart) -> f32 {
    let (width, _) = card_size(cfg);
    let x = cfg.part(part).x.clamp(0.0, 1.0) * width;
    (width - x).max(part.min_width())
}

/// 拖动时的吸附：把元素吸到别的元素 / 卡片边距的同一条线上。
///
/// 返回（吸附后的 x, 吸附后的 y, 竖直参考线, 水平参考线），单位都是卡片内的像素。
/// 参考线只在真的吸上时给，设置页拿它画那两条高亮线。
///
/// 候选线分三档，前档吸上了就不看后档：
/// 1. **卡片正中**（中线，判定半径 9px）—— 用户说「居中」指的就是这条；
/// 2. 别的可见元素的中心线（同样 9px），中心对中心；
/// 3. 边距与别的元素的左 / 右沿（6px）。
///
/// 卡片中线排在邻居中心之前是有意的：默认布局里音量条的中心线离卡片中线只有十来像素，
/// 两档混在一起比距离，正中间反而吸不住 —— 那样「居中吸附」就等于没有。
pub fn snap_position(
    cfg: &OsdConfig,
    part: OsdPart,
    x: f32,
    y: f32,
) -> (f32, f32, Option<f32>, Option<f32>) {
    /// 离边线多远之内就吸过去（卡片像素）。
    ///
    /// 3px：只够「差一点点」的时候兜住，稍微想离开就离开。以前是 6px，
    /// 拖到附近就被拽走，手感像粘住。
    const SNAP: f32 = 3.0;
    /// 居中吸附的判定半径。跟边线用同一个半径，不然中线比边线贪心，更抢。
    const SNAP_CENTER: f32 = 3.0;

    let (width, height) = card_size(cfg);
    let (_, _, w, h) = editor_rect(cfg, part);

    // 卡片自己的两条中线。
    let card_x = ((width - w) / 2.0, width / 2.0);
    let card_y = ((height - h) / 2.0, height / 2.0);

    // (目标位置, 参考线位置)
    // 右边线用 `PAD_RIGHT`：吸过去之后 `part_rect` 算出来的可用宽度刚好等于元素宽度，
    // 不会被 `available` 压窄（用 13 的话会硬压 3px）。
    // 右边线就是卡片边缘本身：和左边能贴 0 对称。
    let mut x_edges: Vec<(f32, f32)> = vec![(13.0, 13.0), (width - w, width)];
    let mut y_edges: Vec<(f32, f32)> = vec![(8.0, 8.0), (height - 10.0 - h, height - 10.0)];
    let mut x_centers: Vec<(f32, f32)> = Vec::new();
    let mut y_centers: Vec<(f32, f32)> = Vec::new();
    for other in cfg.parts() {
        if other == part || !cfg.shows_part(other) {
            continue;
        }
        let (ox, oy, ow, oh) = editor_rect(cfg, other);
        x_edges.push((ox, ox));
        x_edges.push((ox + ow - w, ox + ow));
        y_edges.push((oy, oy));
        y_edges.push((oy + oh - h, oy + oh));
        // 我的中心 ↔ 它的中心。
        x_centers.push((ox + ow / 2.0 - w / 2.0, ox + ow / 2.0));
        y_centers.push((oy + oh / 2.0 - h / 2.0, oy + oh / 2.0));
    }

    let nearest = |value: f32, pairs: &[(f32, f32)], limit: f32| -> Option<(f32, f32)> {
        pairs
            .iter()
            .map(|(target, guide)| ((target - value).abs(), *target, *guide))
            .filter(|(distance, _, _)| *distance <= limit)
            .min_by(|left, right| left.0.total_cmp(&right.0))
            .map(|(_, target, guide)| (target, guide))
    };
    let resolve = |value: f32, own: (f32, f32), centers: &[(f32, f32)], edges: &[(f32, f32)]| {
        nearest(value, std::slice::from_ref(&own), SNAP_CENTER)
            .or_else(|| nearest(value, centers, SNAP_CENTER))
            .or_else(|| nearest(value, edges, SNAP))
    };

    let (x, guide_x) = match resolve(x, card_x, &x_centers, &x_edges) {
        Some((target, guide)) => (target, Some(guide)),
        None => (x, None),
    };
    let (y, guide_y) = match resolve(y, card_y, &y_centers, &y_edges) {
        Some((target, guide)) => (target, Some(guide)),
        None => (y, None),
    };
    (x, y, guide_x, guide_y)
}

/// 媒体键那一套状态下要显示的文字与图标。
///
/// 上一曲 / 下一曲 / 停止都是「一个动作一个样子」；播放 / 暂停共用一个键，
/// 得靠 `playing` 指认这次按完是哪一个 —— 否则两个动作在屏幕上分不出来。
fn media_action_text(
    function: OsdFunction,
    playing: Option<bool>,
    lang: Lang,
) -> (&'static str, &'static str) {
    match function {
        OsdFunction::PlayPause => match playing {
            Some(true) => (lang.pick("播放", "Play"), "play"),
            Some(false) => (lang.pick("暂停", "Pause"), "pause"),
            // 问不出系统状态（老播放器不汇报）：不猜，写成两个都认的「播放 / 暂停」。
            None => (function.label(lang), "play"),
        },
        OsdFunction::PrevTrack => (function.label(lang), "prev"),
        OsdFunction::NextTrack => (function.label(lang), "next"),
        OsdFunction::Stop => (function.label(lang), "stop"),
        _ => (function.label(lang), "music"),
    }
}

/// 把「喇叭样式 / 百分比带不带 `%`」这两项用户选择套到一份状态上。
///
/// 图标只在本来就是音量图标时替换 —— 媒体键那几个（上一曲 / 播放）
/// 不该被用户选的喇叭样式盖掉。
fn apply_style(cfg: &OsdConfig, state: &mut OsdState) {
    // 只替换「本来就是音量图标」的那种：媒体键那几个（上一曲 / 播放）不该被盖掉。
    if state.icon.starts_with("volume-") || state.icon == "speaker" {
        let style = cfg.icon_style();
        state.icon = match style.states {
            // 动态样式：看现在处在哪一档，换成同一套里的那张。
            Some(states) => states[volume_slot(state.icon)],
            None => style.icon,
        };
    }
    state.percent = cfg.percent_display(&state.percent);
}

/// 图标名落在三态里的哪一档：0 = 静音 / 1 = 小声 / 2 = 大声。
///
/// 名字只认后缀（`volume-off` / `volume-bar-low` / `volume-dot-high` …），
/// 这样同一个函数对四套图标都成立。
pub(crate) fn volume_slot(icon: &str) -> usize {
    if icon.ends_with("off") || icon.ends_with("mute") {
        0
    } else if icon.ends_with("low") {
        1
    } else {
        2
    }
}

/// 一路音量对应的两个读数：百分比 + 分贝。
///
/// 拆分元素之后它们不再二选一 —— 想只留一个就把另一个元素删掉。
fn readouts(display: f32, logarithmic: bool, min_db: f32) -> (String, String) {
    let amplitude = if logarithmic {
        to_log_volume(display, min_db)
    } else {
        display
    };
    let percent = (amplitude * 100.0).round().clamp(0.0, 100.0) as u32;
    let db = if amplitude <= 0.0005 {
        "-∞ dB".to_string()
    } else {
        let mut value = amplitude_to_db(amplitude);
        if value > -0.05 {
            value = 0.0;
        }
        format!("{value:.1} dB")
    };
    (format!("{percent}%"), db)
}

/// 和 `positioned` 一样，只是额外把「这是哪个元素」带出来，好按图层顺序排。
fn layered(part: OsdPart, cfg: &OsdConfig, scale: f32, child: AnyElement) -> (OsdPart, AnyElement) {
    (part, positioned(part, cfg, scale, child))
}

fn positioned(part: OsdPart, cfg: &OsdConfig, scale: f32, child: AnyElement) -> AnyElement {
    let (x, y, w, h) = part_rect(cfg, part);
    div()
        .absolute()
        .left(px(x * scale))
        .top(px(y * scale))
        .w(px(w * scale))
        .h(px(h * scale))
        .child(child)
        .into_any_element()
}

/// 投影要在卡片外面画，窗口 / 预览框得多让出多少边（逻辑像素）。
///
/// 渲染器给阴影铺的四边形是 `bounds` 向外扩 **3×模糊半径**（`shaders.hlsl` 里
/// `margin = 3.0 * blur_radius`），再叠上偏移。以前这里只留了 1.5 倍，超出的那部分
/// 就等于被窗口边缘切掉了 —— 扩散越大越明显，看着像外面套了个框。
pub fn shadow_margin(cfg: &OsdConfig) -> f32 {
    if !cfg.shadow.enabled {
        return 0.0;
    }
    let blur = cfg.shadow.blur as f32;
    let offset = cfg.shadow.offset_x.abs().max(cfg.shadow.offset_y.abs()) as f32;
    (blur * 3.0 + offset + 4.0).ceil()
}

/// 卡片下面那层影子。关着就返回 `None`（不多一个空元素）。
fn shadow_layer(cfg: &OsdConfig, scale: f32) -> Option<gpui::AnyElement> {
    let shadow = &cfg.shadow;
    if !shadow.enabled {
        return None;
    }
    let (width, height) = card_size(cfg);
    let base = theme::parse_hex(&shadow.color).unwrap_or_else(|| gpui::black());
    let color = gpui::Hsla {
        alpha: (shadow.opacity.min(100) as f32 / 100.0).clamp(0.0, 1.0),
        ..base
    };
    let box_shadow = gpui::BoxShadow::new(
        px(shadow.offset_x as f32 * scale),
        px(shadow.offset_y as f32 * scale),
        color,
    )
    .blur_radius(px(shadow.blur as f32 * scale));
    let corners = gpui::Corners::all(px((cfg.corner as f32).min(height / 2.0) * scale));
    let _ = width;
    Some(
        gpui::canvas(
            |_, _, _| {},
            move |bounds, _, window, _| {
                window.paint_drop_shadows(bounds, corners, std::slice::from_ref(&box_shadow));
            },
        )
        .absolute()
        .left_0()
        .top_0()
        .size_full()
        .into_any_element(),
    )
}

/// 量文字宽度用的东西（拿一次 TextSystem + 当前字体，之后反复量）。
///
/// 断句逻辑在平台层，这里只关心「这段字在给定字号下有多宽」——
/// 读数的右/左锚定、编辑器的元素边界都靠它。
pub enum TextMeasure {
    /// 真窗口里：交给平台排版量（精确）。
    Shaped {
        system: std::sync::Arc<gpui::WindowTextSystem>,
        font: gpui::Font,
    },
    /// 拿不到窗口时（设置页缩略图）按字符估：中文算一个字号宽，ASCII 算半个多一点。
    Approx,
}

impl TextMeasure {
    pub fn new(window: &gpui::Window) -> Self {
        Self::Shaped {
            system: window.text_system().clone(),
            font: window.text_style().font(),
        }
    }

    pub fn approx() -> Self {
        Self::Approx
    }

    /// 一行文字实际占多高（编辑器里画元素框要用它，不然框会上下各空一截）。
    pub fn height(&self, text: &str, size: f32) -> f32 {
        if text.is_empty() {
            return size * 1.2;
        }
        match self {
            Self::Shaped { system, font } => {
                let run = gpui::TextRun {
                    len: text.len(),
                    font: font.clone(),
                    color: gpui::black(),
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                    letter_spacing: None,
                };
                let line = system.layout_line(text, px(size), &[run], None);
                line.ascent.as_f32() + line.descent.as_f32()
            }
            Self::Approx => size * 1.25,
        }
    }

    pub fn width(&self, text: &str, size: f32) -> f32 {
        if text.is_empty() {
            return 0.0;
        }
        match self {
            Self::Shaped { system, font } => {
                let run = gpui::TextRun {
                    len: text.len(),
                    font: font.clone(),
                    color: gpui::black(),
                    background_color: None,
                    underline: None,
                    strikethrough: None,
                    letter_spacing: None,
                };
                system
                    .layout_line(text, px(size), &[run], None)
                    .width
                    .as_f32()
            }
            Self::Approx => text
                .chars()
                .map(|ch| if ch.is_ascii() { size * 0.56 } else { size })
                .sum(),
        }
    }
}

/// 文字元素按「对齐方式」定锚：内容比用户框宽时向外扩，锚边不动。
///
/// 读数右对齐时就以框的**右边**为基准向左长 —— 从 `9%` 变 `100%` 不会把整块往右挤；
/// 左对齐以左边、居中就以中心为基准。
fn anchored_text(
    cfg: &OsdConfig,
    part: OsdPart,
    scale: f32,
    content_w: f32,
    child: AnyElement,
) -> AnyElement {
    let (x, y, w, h) = part_rect(cfg, part);
    let frame_w = (w * scale).max(1.0);
    let width = frame_w.max(content_w);
    let left = match cfg.align_of(part) {
        OsdAlign::Right => x * scale + frame_w - width,
        OsdAlign::Center => x * scale + (frame_w - width) / 2.0,
        OsdAlign::Left => x * scale,
    };
    div()
        .absolute()
        .left(px(left))
        .top(px(y * scale))
        .w(px(width))
        .h(px(h * scale))
        .child(child)
        .into_any_element()
}

/// 整张提示条。真实窗口与设置页预览共用，保证所见即所得。
///
/// `scale` 只给预览缩放用，真实窗口永远是 1.0。
/// `lang` 是给文字里的变量用的（`{ampm}` / `{week}` 要按界面语言出中文还是英文）。
pub fn card_visual(
    cfg: &OsdConfig,
    state: &OsdState,
    scale: f32,
    bar: Option<&OsdBar>,
    measure: &TextMeasure,
    lang: Lang,
) -> Div {
    let (width, height) = card_size(cfg);
    // 文字里的 `{hh}` / `{date}` 这类变量按「这一刻」展开，整张卡片共用一份时间：
    // 同一帧里不会出现前半秒旧、后半秒新的跳字。
    let clock = crate::textvars::Clock::local();
    // 配色：用户设了就用自己的，没设跟随主题。
    let bg = theme::parse_hex(&cfg.colors.card).unwrap_or_else(theme::surface);
    let ink = theme::parse_hex(&cfg.colors.text).unwrap_or_else(theme::text_primary);
    // 副标题（状态下面那行小字）用主题的三级文字色；分贝不再算「次要信息」——
    // 它现在跟别的元素一样跟随文字色，要淡就单独给它挑个色。
    let dim = theme::text_tertiary();
    // 每个元素都可以单独改色：没设过就跟随「文字色」。
    let part_ink = |part: OsdPart| -> gpui::Hsla {
        let raw = cfg.part_color(part);
        theme::parse_hex(&raw).unwrap_or(ink)
    };
    let accent = theme::parse_hex(&cfg.colors.accent).unwrap_or_else(theme::accent);
    let bar_ink = {
        let raw = cfg.part_color(OsdPart::Bar);
        theme::parse_hex(&raw).unwrap_or(accent)
    };
    // 音量条的另两段颜色：轨道底色、静音时的填充。空 = 主题原来那两个。
    let bar_track = theme::parse_hex(&cfg.colors.bar_track);
    let bar_muted = theme::parse_hex(&cfg.colors.bar_muted);
    let mut card = div()
        .relative()
        .flex_none()
        .w(px(width * scale))
        .h(px(height * scale))
        // 圆角超过高度一半就只剩个胶囊形，再大视觉上没差别 —— 渲染时先夹住。
        .rounded(px((cfg.corner as f32).min(height / 2.0) * scale))
        .bg(bg)
        .border_1()
        .border_color(theme::border_strong())
        .overflow_hidden();

    // 各元素先攒起来，最后按 `parts()` 的图层顺序落笔（靠后的盖在上面）。
    let mut layers: Vec<(OsdPart, AnyElement)> = Vec::new();

    // 图标：静音 / 播放暂停这类「状态」由调用方算好 icon 名字。
    if cfg.shows_part(OsdPart::Icon) {
        let (_, _, icon_w, _) = part_rect(cfg, OsdPart::Icon);
        layers.push(layered(
            OsdPart::Icon,
            cfg,
            scale,
            icon(state.icon, icon_w * scale, part_ink(OsdPart::Icon)).into_any_element(),
        ));
    }

    // 标题 + 副标题：字号跟着这个元素自己的 size 走。
    // 标题里的变量同样要展开 —— 用户可以把「状态」整条换成 `{time}` 当钟用。
    let title_text = crate::textvars::resolve(&state.title, &clock, lang);
    if cfg.shows_part(OsdPart::Title) && !title_text.is_empty() {
        let (_, _, _, title_h) = part_rect(cfg, OsdPart::Title);
        let title_size = cfg.part(OsdPart::Title).size as f32;
        let title_align = cfg.align_of(OsdPart::Title);
        let mut title_column = div()
            .flex()
            .flex_col()
            .gap(px(2.0 * scale))
            .w_full()
            .h(px(title_h * scale))
            .justify_center()
            .when(title_align == OsdAlign::Center, |style| {
                style.items_center()
            })
            .when(title_align == OsdAlign::Right, |style| style.items_end())
            .child(
                div()
                    .text_size(px(title_size * scale))
                    .text_color(part_ink(OsdPart::Title))
                    .truncate()
                    .child(title_text.clone()),
            );
        if !state.subtitle.is_empty() {
            title_column = title_column.child(
                div()
                    .text_size(px((title_size * 0.8).max(8.0) * scale))
                    .text_color(dim)
                    .truncate()
                    .child(state.subtitle.clone()),
            );
        }
        // 内容比框宽时按对齐方式向外扩（右对齐就向左长），锚边不动。
        let title_w =
            measure
                .width(&title_text, title_size * scale)
                .max(if state.subtitle.is_empty() {
                    0.0
                } else {
                    measure.width(&state.subtitle, (title_size * 0.8).max(8.0) * scale)
                });
        layers.push((
            OsdPart::Title,
            anchored_text(
                cfg,
                OsdPart::Title,
                scale,
                title_w,
                title_column.into_any_element(),
            ),
        ));
    }

    // 自定义文字：内容来自配置，空着就不画。
    // `{hh}` / `{date}` 这类变量在这里展开 —— 一条文字既可以写死，也可以当时钟。
    for index in 0..cfg.texts.len() {
        let part = OsdPart::Text(index as u16);
        if !cfg.shows_part(part) {
            continue;
        }
        let text = crate::textvars::resolve(cfg.text_at(index).trim(), &clock, lang);
        if text.is_empty() {
            continue;
        }
        let size = cfg.part(part).size as f32;
        let text_w = measure.width(&text, size * scale);
        layers.push((
            part,
            anchored_text(
                cfg,
                part,
                scale,
                text_w,
                div()
                    .flex()
                    .items_center()
                    .when(cfg.align_of(part) == OsdAlign::Center, |style| {
                        style.justify_center()
                    })
                    .when(cfg.align_of(part) == OsdAlign::Right, |style| {
                        style.justify_end()
                    })
                    .w_full()
                    .h_full()
                    .text_size(px(size * scale))
                    .text_color(part_ink(part))
                    .truncate()
                    .child(text)
                    .into_any_element(),
            ),
        ));
    }

    // 百分比与分贝：两个独立元素，各自有字号、各自能删。
    // 「媒体键」模板只留图标 + 一句话，数值和音量条都不画。
    for (part, text) in [(OsdPart::Percent, &state.percent), (OsdPart::Db, &state.db)] {
        if text.is_empty() || !cfg.shows_part(part) {
            continue;
        }
        let size = cfg.part(part).size as f32;
        let color = part_ink(part);
        let text_w = measure.width(text, size * scale);
        layers.push((
            part,
            anchored_text(
                cfg,
                part,
                scale,
                text_w,
                div()
                    .flex()
                    .items_center()
                    // 对齐由每个元素自己定（读数默认靠右：位数变化时数字不左右跳）。
                    .when(cfg.align_of(part) == OsdAlign::Center, |style| {
                        style.justify_center()
                    })
                    .when(cfg.align_of(part) == OsdAlign::Right, |style| {
                        style.justify_end()
                    })
                    .w_full()
                    .h_full()
                    .text_size(px(size * scale))
                    .text_color(color)
                    .truncate()
                    .child(text.clone())
                    .into_any_element(),
            ),
        ));
    }

    // 音量条：有绑定时换成可拖动的滑条（和面板里那根是同一种元素）。
    if state.show_bar && cfg.shows_part(OsdPart::Bar) {
        let (bar_x, bar_y, bar_w, bar_h) = part_rect(cfg, OsdPart::Bar);
        // 音量条圆角：`bar_round` 是半高的百分比（100 = 全圆头），跟着条高一起缩放。
        let bar_corner = bar_h * cfg.bar_round_ratio() / 2.0;
        let radius = bar_corner * scale;
        // 滑条自己带一段触摸高度，往上让一截让轨道落在原来的位置。
        let touch = (bar_h + 8.0).max(BAR_TOUCH_HEIGHT);
        let track = match bar {
            Some(binding) => {
                let top = (bar_y - (touch - bar_h) / 2.0).max(0.0);
                let change = binding.on_change.clone();
                let release = binding.on_release.clone();
                let drag = binding.drag.clone();
                div()
                    .absolute()
                    .left(px(bar_x * scale))
                    .top(px(top * scale))
                    .w(px(bar_w * scale))
                    .h(px(touch * scale))
                    .child(
                        VolumeSlider::new("osd-volume-bar", state.fraction.clamp(0.0, 1.0), drag)
                            // 提示条只是「顺手一调」，不画电平特效：那会让卡片闪。
                            .effect(crate::config::ActivityEffect::Off)
                            .fill(bar_ink)
                            .track_color(bar_track)
                            .muted_color(bar_muted)
                            .corner(bar_corner)
                            .thickness(bar_h)
                            .muted(state.muted)
                            .enabled(cfg.interactive)
                            .on_change(move |value, window, cx| change(value, window, cx))
                            .on_release(move |window, cx| release(window, cx)),
                    )
                    .into_any_element()
            }
            // 预览：静态的一条，跟着滑条的样子画（圆头轨道 + 圆头填充）。
            None => positioned(
                OsdPart::Bar,
                cfg,
                scale,
                div()
                    .relative()
                    .w(px(bar_w * scale))
                    .h(px(bar_h * scale))
                    .rounded(px(radius))
                    .bg(bar_track.unwrap_or_else(theme::track))
                    .child(
                        div()
                            .absolute()
                            .left_0()
                            .top_0()
                            .h_full()
                            .w(px(bar_w * scale * state.fraction.clamp(0.0, 1.0)))
                            .rounded(px(radius))
                            .bg(if state.muted {
                                bar_muted.unwrap_or_else(theme::muted_fill)
                            } else {
                                bar_ink
                            }),
                    )
                    .into_any_element(),
            ),
        };
        layers.push((OsdPart::Bar, track));
    }

    // 按图层顺序落笔：`parts()` 里靠后的画在上面。
    for part in cfg.parts() {
        if let Some(index) = layers.iter().position(|(slot, _)| *slot == part) {
            let (_, element) = layers.remove(index);
            card = card.child(element);
        }
    }
    // 兜底：清单里没提到的（理论上不会发生）也别丢。
    for (_, element) in layers {
        card = card.child(element);
    }

    // 投影画在卡片外面，所以套一层不裁剪的外壳：外壳给阴影留位置，
    // 卡片自己继续 `overflow_hidden` 管圆角。
    let mut shell = div()
        .relative()
        .flex_none()
        .w(px(width * scale))
        .h(px(height * scale));
    if let Some(shadow) = shadow_layer(cfg, scale) {
        shell = shell.child(shadow);
    }
    shell.child(card)
}

/// 屏幕上的提示条窗口。
pub struct Osd {
    config: Rc<RefCell<ConfigStore>>,
    engine: Rc<AudioEngine>,
    snapshot: Rc<RefCell<Snapshot>>,
    state: OsdState,
    phase: Phase,
    alpha: f32,
    last_alpha: f32,
    hovered: bool,
    visible: bool,
    ticker: bool,
    /// 上一次设定的卡片尺寸，尺寸变了才重建窗口几何。
    frame: (f32, f32),
    /// 滚轮去重：同一格滚轮 gpui 可能连着回调好几遍。
    wheel_last_at: Option<Instant>,
    /// 音量条滑条的拖拽 / 悬停状态（和面板里那根共用一套逻辑，状态各存一份）。
    slider: SliderDragState,
    /// 正在拖音量条：这时候别让 33ms 一来的快照把读数拽回旧值。
    dragging: bool,
    /// 手势刚写下去、快照还没确认的读数（连滚 / 拖条 / 快捷键都记一笔）。
    hold: Option<OsdHold>,
    /// 「保持显示」：钉住时不自动淡出（设置页里对比外观用）。
    pinned: bool,
    /// 正在收「保持显示」的预览：这一轮淡出连悬停也不理。
    ///
    /// 设置页关掉时鼠标常常正停在卡片上（刚点完按钮就关窗），若照常走
    /// 「悬停就冻住」，卡片会一直挂在那儿 —— 用户要的是「关了设置它就没了」。
    dismissing: bool,
    /// 上一次画出来的时间文字（元素 → 展开后的内容）。
    ///
    /// 带 `{ss}` 这种变量的文字每帧都可能不一样，但一秒才变一次；
    /// 存一份上次的样子，只有真变了才重画，别拿 16ms 一帧去重绘时钟。
    time_text: Vec<(OsdPart, String)>,
    /// 系统风格下卡片当前要往下让多远（逻辑像素）；0 = 在位。
    ///
    /// 跟系统的音量条一样：出现时从下方滑上来、消失时滑回去，
    /// 直接挪窗口位置（卡片和影子一起走，不会被窗口裁掉一块）。
    offset: f32,
    /// 窗口贴哪儿（左上角 + 尺寸）：`place` 算好存这儿，位移每帧在它上面加减。
    base: (f32, f32, f32, f32),
    /// 上一次真正写进窗口的位移，用来跳过没变化的帧。
    last_offset: f32,
    /// 系统风格下这一轮的起始位移（窗口顶边贴到屏幕底，整块卡片在屏幕外）。
    slide: f32,
    /// 正在跑的那段滑动（独立线程按合成帧推进）；None = 没有动画在跑。
    slide_run: Option<crate::windows::platform::SlideHandle>,
    /// 这一轮淡入从哪个不透明度开始。
    ///
    /// 正常是从 0（全透明露头），「淡出演到一半又被呼出」时是那一瞬间的读数 ——
    /// 于是卡片从半透明接着变实，不会先跳成全透明再涨一遍。
    fade_from: f32,
    /// 这扇窗固定贴在哪块屏上（`None` = 照配置里的 `monitor` 走）。
    ///
    /// 「全部显示器」时每块屏一扇窗，各自钉在自己那块上；
    /// 「跟随鼠标」时由调用方现算出序号再钉上来。
    monitor: Option<u32>,
    /// 上一次把窗口提到 TOPMOST 组最前的时间。
    ///
    /// 停留期间隔一段重申一次：有些置顶软件（播放器、悬浮窗、游戏内覆盖层）
    /// 会周期性调 `SetWindowPos(HWND_TOPMOST)` 把自己抢回组首，一被抢走
    /// 提示条就整块埋在它下面。只动 z 序、不激活，看不出闪烁。
    last_raise: Instant,
}

/// 显示阶段：等首帧 / 淡入 / 停留 / 淡出。
#[derive(Clone, Copy, Debug)]
enum Phase {
    Hidden,
    /// 窗口刚从隐藏状态重新露头：等新内容真的画出来再开始淡入。
    ///
    /// 少了这一段，窗口一 Show，DWM 会先把**上一帧的旧 surface** 顶上来
    /// （上一次的音量卡片），淡入过程中它跟着 alpha 一路变清楚 ——
    /// 表现就是「先显示音量，再闪成媒体键」。
    Warmup {
        until: Instant,
    },
    In(Instant),
    Hold {
        until: Instant,
    },
    Out(Instant),
}

/// 手势刚写下去、快照还没确认的那次读数。
///
/// 从前只靠 `dragging`（手按着）挡住快照；手一松，快照里还是旧值，
/// `sync_snapshot` 就把读数和喇叭图标一起拽回去，下一帧再跳回来 ——
/// 就是「松手之后喇叭和百分比还跟着跳一下」。
///
/// 现在只看时间：手势之后顶住 `HOLD_SETTLE` + 一段时间，期间快照一律不认。
struct OsdHold {
    /// 最后一次手势的时刻（连续拖动 / 连滚时每次刷新）。
    at: Instant,
}

/// 手势之后无条件顶住的时长。
///
/// 这一小段是用来吃「命令队列里的中间态」的：连点两下静音（静音 → 立刻解除）
/// 时，前一条命令会晚一步生效，快照里先冒出 `true`、再冒回 `false`。
/// 判据只要不是「手势之后过去足够久」，那一下中间态就会先把读数顶掉、
/// 下一秒再跳回来 —— 就是用户看到的「反复跳一个显示循环」。
///
/// 260ms：比两帧快照（30Hz）宽，也比托盘那一下的 250ms 双击抑制略长。
const HOLD_SETTLE: Duration = Duration::from_millis(260);

/// 手势之后最多顶这么久，只在写入被设备拒绝 / 命令被合并掉时才会撞上。
const OSD_HOLD_FOR: Duration = Duration::from_millis(1000);

/// 这次快照能不能盖掉手势读数：`true` 表示还得继续顶住。
///
/// 判据只有时间：手势后先无条件顶 `HOLD_SETTLE`（吃掉命令队列里的中间态），
/// 之后顶到上限。**不要**拿「快照已经等于手势值了」提前交还 —— 写入在
/// USB / 蓝牙端点上会晚几百毫秒落地，这期间快照「看起来到位」是假象，
/// 一交还就正好撞上迟到的旧值，显示就来回跳一圈。
fn resolve_hold(hold: &mut OsdHold, real: (f32, bool)) -> bool {
    let _ = real;
    hold.at.elapsed() <= OSD_HOLD_FOR
}

/// 等首帧的时长：两个 16ms 的 tick，足够 gpui 走完一次绘制。
const WARMUP: Duration = Duration::from_millis(32);

/// 停留期间多久重申一次「我在最前」。
///
/// 间隔取 250ms：够快，抢层级的置顶软件（多数按帧或按几百毫秒重设）
/// 撑不满两次调整之间的空档；又比每帧重申省得多 —— `SetWindowPos` 只动 z 序、
/// 不带 `SWP_NOZORDER` 之外的重活，一秒钟四次完全看不出闪烁。
const RAISE_INTERVAL: Duration = Duration::from_millis(250);

impl Osd {
    pub fn new(
        config: Rc<RefCell<ConfigStore>>,
        engine: Rc<AudioEngine>,
        snapshot: Rc<RefCell<Snapshot>>,
    ) -> Self {
        Self {
            config,
            engine,
            snapshot,
            state: OsdState::default(),
            phase: Phase::Hidden,
            alpha: 0.0,
            last_alpha: 0.0,
            hovered: false,
            visible: false,
            ticker: false,
            frame: (0.0, 0.0),
            offset: 0.0,
            base: (0.0, 0.0, 0.0, 0.0),
            last_offset: f32::NAN,
            slide: 0.0,
            slide_run: None,
            fade_from: 0.0,
            wheel_last_at: None,
            slider: SliderDragState::new(),
            dragging: false,
            hold: None,
            pinned: false,
            dismissing: false,
            time_text: Vec::new(),
            monitor: None,
            last_raise: Instant::now(),
        }
    }

    /// 把这扇窗钉到某块屏上（序号同 `target_area`：0 = 主显示器，n = 第 n 台）。
    pub fn set_monitor(&mut self, monitor: u32) {
        self.monitor = Some(monitor);
    }

    pub fn is_visible(&self) -> bool {
        self.visible
    }

    /// 当前这次操作该用的配置（模板已经按操作挑好）。
    fn cfg(&self) -> OsdConfig {
        self.cfg_for(self.state.function)
    }

    /// 指定操作用的配置：音量 / 静音 → 音量模板，媒体键 → 媒体键模板。
    ///
    /// 模板跟着「是谁触发的」走，不看配置里存的 `detailed` —— 那个只是编辑器在改哪套。
    fn cfg_for(&self, function: OsdFunction) -> OsdConfig {
        self.config.borrow().get().osd.for_function(function)
    }

    fn lang(&self) -> Lang {
        self.config.borrow().get().language
    }

    /// 显示一条提示。
    ///
    /// 已经显示着就只换内容、把停留时间往后推：连按音量键时不该每次都重新淡入，
    /// 也不该在两次按键之间闪掉 —— 手一停，照常倒计时淡出。
    /// 记一笔「这是手势要的读数，等快照确认再交还」。
    ///
    /// 连续调节（连滚、一直拖）会不停进来：目标值和计时每次刷新 —— 手不停就一直等，
    /// 手一停再过 `HOLD_SETTLE` 才开始认快照。
    ///
    /// `muted` 要传**手势预测的**静音态（调用点都排在 `state.muted` 更新之后）。
    /// 传快照里那个旧值的话，解除静音时这里记的是「静音」，快照再怎么变都对不上。
    fn note_gesture(&mut self) {
        self.hold = Some(OsdHold { at: Instant::now() });
    }

    /// 这次快照能不能盖掉手势读数：能返回 true，不能（还得继续顶）返回 false。
    fn hold_blocks(&mut self, real: (f32, bool)) -> bool {
        let Some(hold) = self.hold.as_mut() else {
            return false;
        };
        if resolve_hold(hold, real) {
            return true;
        }
        self.hold = None;
        false
    }

    pub fn present(&mut self, request: OsdRequest, window: &mut Window, cx: &mut Context<Self>) {
        // 新的一次显示：上一轮「收预览」的强制淡出到此为止（用户又按了音量键之类）。
        self.dismissing = false;
        // 入场（预热 / 滑入）还没走完又来了一次：这一下只换内容，
        // 位置、透明度、相位一律不动 —— 否则每按一下卡片都会从底下重新升一次。
        // 判定条件是「相位还在入场里」且「滑动线程确实在跑」。
        let entering = matches!(self.phase, Phase::Warmup { .. } | Phase::In(_))
            && self.slide_run.is_some();
        // 出场（滑走 / 淡出）演到一半又来了一次：从**这一刻的位置和不透明度**接着
        // 反向回来，不能跳回原位、更不能把 alpha 一把拉到 1 —— 那一下就是「卡一下」。
        let leaving = matches!(self.phase, Phase::Out(_));
        let resume = if leaving {
            let (offset, alpha) = match self.slide_run.as_ref() {
                Some(handle) => (handle.offset(), handle.alpha()),
                None => (self.offset, self.alpha),
            };
            // 旧线程可能还在往屏幕外拽，先叫停；位置由下面这一段接管。
            if let Some(old) = self.slide_run.take() {
                old.cancel();
            }
            Some((offset.max(0.0), alpha.clamp(0.0, 1.0)))
        } else {
            // 上一次的滑动线程停掉 —— 它可能正往屏幕外滑，不取消的话会把窗口
            // 继续往下拽。入场期间例外：那一轮要继续跑完。
            if !entering {
                if let Some(old) = self.slide_run.take() {
                    old.cancel();
                }
            }
            None
        };
        // 模板按这次触发的操作选：媒体键弹媒体键模板，音量 / 静音弹音量模板。
        let cfg = self.cfg_for(request.function);
        self.state = self.build_state(&request, &cfg);
        // 带预测值的这次是手势（快捷键 / 媒体键）：快照回头还慢，先记一笔顶住它。
        // 不带预测值说明这次就是照快照显示，直接以系统为准，别留旧记录。
        if request.predicted.is_some() || request.predicted_muted.is_some() {
            self.note_gesture();
        } else {
            self.hold = None;
        }
        // 先把时间文字记成「现在这一份」，免得第一帧就被判成变化多画一次。
        let _ = self.refresh_time_text(&cfg);
        let now = Instant::now();
        let reactivate = !self.visible || matches!(self.phase, Phase::Hidden);
        if reactivate {
            // 和 `dispatch` 那条一起看，就能分清「按键到弹窗」慢在哪一段。
            crate::platform::hotkey::trace("osd present (reactivate)");
        }
        // 只有「从隐藏状态重新露头」才需要等首帧：卡片还在屏幕上时换内容，
        // 同一次绘制就把新内容提交了，不会露出旧 surface。
        let warmup = if reactivate { WARMUP } else { Duration::ZERO };
        let guarded = !warmup.is_zero();
        // 系统风格：这一次从「卡片整个在下方」的位置开始，窗口先摆到起点再显示。
        let enters = animates_in(&cfg);
        if guarded {
            // 先保持全透明，等首帧画完再淡入。
            self.alpha = 0.0;
            self.last_alpha = 0.0;
            self.fade_from = 0.0;
            self.phase = Phase::Warmup {
                until: now + warmup,
            };
        } else if let Some((offset, alpha)) = resume {
            // 出场中被打断：位置和不透明度都留在刚才那一刻，交给 `tick` / 新的
            // 滑动从这里往回走。
            self.offset = offset;
            self.alpha = alpha;
            self.fade_from = alpha;
            self.phase = Phase::In(now);
        } else if reactivate {
            self.phase = if self.slide > 0.0 {
                Phase::In(now)
            } else {
                self.offset = 0.0;
                Phase::Hold {
                    until: now + Duration::from_millis(cfg.hold_ms as u64),
                }
            };
            self.fade_from = 0.0;
            self.alpha = if enters { 0.0 } else { 1.0 };
        } else if matches!(self.phase, Phase::Warmup { .. }) {
            // 首帧还没画完又来了一次：内容已经换好，别把淡入再往后推。
            self.alpha = 0.0;
        } else {
            // 连续调节：不重新淡入，只把停留时间往后推。
            self.offset = 0.0;
            self.alpha = 1.0;
            self.fade_from = 1.0;
            self.phase = Phase::Hold {
                until: now + Duration::from_millis(cfg.hold_ms as u64),
            };
        }
        if entering {
            // 入场还在走：只保留上面换好的内容，窗口位置和整窗透明度都交给
            // 那一轮的滑动线程，别插手。
        } else if let Some((offset, alpha)) = resume {
            // 接力：`place` 按「目标位置 + 刚停下的位移」摆，一次 SetWindowPos
            // 就落在当前的画面上（尺寸不变时等于没挪），不会闪回原位。
            self.place(window, cx, &cfg);
            // 上一轮没记过「到屏幕底多远」（比如刚切到系统风格）就现算一个：
            // 这一段接力和它之后的出场都要按这个距离走。
            if self.slide <= 0.5 {
                self.slide = system_slide(&cfg, self.monitor.unwrap_or(cfg.monitor), self.base.1);
            }
            let slide = self.slide.max(offset);
            if enters && slide > 0.5 {
                // 动画期间压在任务栏后面：滑过任务栏的那一段被它挡住。
                crate::windows::platform::place_osd(window);
                self.start_slide(
                    window,
                    &cfg,
                    crate::windows::platform::SlideMode::Enter,
                    offset,
                    alpha,
                );
            } else {
                // 淡入淡出档：原地淡回来，位置交给 `tick` 归零。
                self.offset = 0.0;
                self.last_offset = f32::NAN;
                self.alpha = if enters { alpha } else { 1.0 };
                self.apply_alpha(window, &cfg);
                // 不滑动的这一档没有「被任务栏遮住」的需求，直接提到最前 ——
                // 压回任务栏之后的话，别的置顶窗口会把它盖掉。
                crate::windows::platform::raise_osd(window);
            }
        } else {
            // 位置和透明度这一轮归滑动线程管，`place` 先按目标位置摆好。
            self.offset = 0.0;
            self.place(window, cx, &cfg);
            // **摆好之后**才知道「从屏幕底到这里有多远」—— 提前算会拿到上一次的位置。
            let slide = system_slide(&cfg, self.monitor.unwrap_or(cfg.monitor), self.base.1);
            // 「这一轮从多远起步」：系统风格下恒按「卡片到屏幕底」算。
            // **出场也用同一个量**滑出去 —— 以前连着按第二下时把它清成 0，
            // 出场就退化成纯淡出（用户：「触发完再按一次，消失动画变成淡出了」）。
            self.slide = if enters && cfg.anim_style.is_system() {
                slide
            } else {
                0.0
            };
            if guarded {
                // 显示之前先把窗口压成全透明：DWM 在 Show 的一瞬间就会把旧 surface
                // 摆出来，哪怕只有一帧，也会在淡入开头看出上一次的内容。
                crate::windows::platform::set_opacity(window, 0.0);
                self.last_alpha = 0.0;
            }
            crate::windows::platform::set_visible(window, true);
            // 只有「从隐藏状态重新露头」才滑进来；连续调节时卡片就在屏幕上，
            // 再滑一次就变成每按一下都从底下冒上来了（`self.slide` 仍然记着，
            // 出场还要按它滑下去）。
            if self.slide > 0.0 && reactivate {
                // 动画期间压在任务栏后面：滑过任务栏的那一段被它挡住，
                // 才像系统那样「从任务栏背后升起来」。到位后再提到最上层。
                crate::windows::platform::place_osd(window);
                self.start_slide(
                    window,
                    &cfg,
                    crate::windows::platform::SlideMode::Enter,
                    self.slide,
                    0.0,
                );
            } else {
                // 连续调节（卡片已经在屏幕上）：没在滑动，直接提到最前。
                // 出场前（`Phase::Hold` 结束时）会重新压回任务栏之后，
                // 所以「滑下去那一段被任务栏挡住」照样成立。
                crate::windows::platform::raise_osd(window);
            }
            self.apply_alpha(window, &cfg);
        }
        self.visible = true;
        cx.notify();
        // 立刻要一帧：OSD 窗口是 NOACTIVATE 的，不给它一条 WM_PAINT，
        // 新内容可能要等下一次输入才提交，旧 surface 就一直顶着。
        crate::windows::platform::post_repaint(window);
        self.kick_ticker(cx);
    }

    /// 被音频快照刷新时调用：只更新读数，不动动画。
    pub fn sync_snapshot(&mut self, snapshot: Snapshot, cx: &mut Context<Self>) {
        if !self.visible || self.dragging {
            return;
        }
        *self.snapshot.borrow_mut() = snapshot;
        if !matches!(self.state.function, OsdFunction::Volume | OsdFunction::Mute) {
            return;
        }
        let cfg = self.cfg();
        let request = OsdRequest {
            function: self.state.function,
            target: self.state.target.clone(),
            predicted: None,
            predicted_muted: None,
            playing: self.state.playing,
        };
        let next = self.build_state(&request, &cfg);
        // 手势的读数还在等快照确认：快照压根还没动过，就别把它拽回旧值。
        if self.hold_blocks((next.fraction, next.muted)) {
            return;
        }
        // 只更数值，标题不跟着抖动（比如正在发声的应用切来切去）。
        if next.percent != self.state.percent
            || next.db != self.state.db
            || next.fraction != self.state.fraction
            || next.muted != self.state.muted
        {
            self.state.percent = next.percent;
            self.state.db = next.db;
            self.state.fraction = next.fraction;
            self.state.icon = next.icon;
            self.state.muted = next.muted;
            cx.notify();
        }
    }

    /// 按请求算出要显示的文字与图标。
    fn build_state(&self, request: &OsdRequest, cfg: &OsdConfig) -> OsdState {
        let snapshot = self.snapshot.borrow().clone();
        let config = self.config.borrow().get().clone();
        let lang = config.language;
        let min_db = config.logarithmic_min_db as f32;
        let logarithmic = config.use_logarithmic_volume;
        let mut state = OsdState {
            function: request.function,
            target: request.target.clone(),
            ..OsdState::default()
        };

        match request.function {
            OsdFunction::Volume | OsdFunction::Mute => {
                let (volume, snapshot_muted, owner) = match &request.target {
                    OsdTarget::Device => {
                        let device = snapshot
                            .devices
                            .iter()
                            .find(|device| device.is_default)
                            .or_else(|| snapshot.devices.iter().find(|device| device.enabled));
                        match device {
                            Some(device) => {
                                (device.volume, device.muted, device_name(&config, device))
                            }
                            None => (0.0, false, String::new()),
                        }
                    }
                    OsdTarget::App { key, name } => match find_app(&snapshot, key) {
                        Some(app) => (app.volume, app.muted, name.clone()),
                        None => (0.0, false, name.clone()),
                    },
                };
                // 调用方刚切过静音就以它为准：快照还要 33ms 才回来，
                // 不然静音键按下去那一瞬间显示的是「取消静音」。
                let muted = request.predicted_muted.unwrap_or(snapshot_muted);
                // 同理，调完音量直接用它算好的读数，避免先显示旧值再跳。
                let display = match request.predicted {
                    Some(predicted) => predicted.clamp(0.0, 1.0),
                    None if logarithmic => to_display_volume(volume, min_db),
                    None => volume,
                };
                let amplitude = if logarithmic {
                    to_log_volume(display, min_db)
                } else {
                    display
                };
                let (percent, db) = readouts(display, logarithmic, min_db);
                state.percent = percent;
                state.db = db;
                state.fraction = display;
                state.muted = muted;
                state.show_bar = true;
                state.icon = match request.function {
                    OsdFunction::Mute if muted => "volume-off",
                    _ => crate::ui::volume_icon_name(amplitude, muted),
                };
                let muted_text = lang.pick("已静音", "Muted");
                // 两种模板都只报「这一次操作」：设备名、正在播放的应用一律不显示。
                let _ = &owner;
                state.title = match request.function {
                    OsdFunction::Mute if muted => muted_text,
                    OsdFunction::Mute => lang.pick("取消静音", "Unmuted"),
                    _ if matches!(request.target, OsdTarget::App { .. }) => {
                        lang.pick("应用音量", "App volume")
                    }
                    _ => lang.pick("音量", "Volume"),
                }
                .to_string();
                state.subtitle = String::new();
            }
            OsdFunction::Brightness => {
                // 亮度没有音频快照可等：调用方算好的比例（0..1）就是真值。
                // 滑条、读数都按线性百分比算，别套音量的对数那一套。
                let level = request.predicted.unwrap_or(0.0).clamp(0.0, 1.0);
                let (percent, _) = readouts(level, false, min_db);
                state.percent = percent;
                state.fraction = level;
                state.show_bar = true;
                state.icon = "sun";
                state.title = lang.pick("亮度", "Brightness").to_string();
                state.subtitle = String::new();
            }
            function => {
                state.percent = String::new();
                state.db = String::new();
                state.show_bar = false;
                // 播放 / 暂停共用一个键：按完到底是「开始播」还是「停下来」，
                // 由调用方问过系统媒体会话之后塞进 `playing`。
                let (title, icon) = media_action_text(function, request.playing, lang);
                state.title = title.to_string();
                state.icon = icon;
                state.playing = request.playing;
                // 只报动作（上一曲 / 下一曲 / 暂停），不带应用名。
                state.subtitle = String::new();
            }
        }
        // 用户自己填了标题就盖掉自动内容（自动副标题也不显示了）。
        if !cfg.title_text.trim().is_empty() {
            state.title = cfg.title_text.clone();
            state.subtitle = String::new();
        }
        apply_style(cfg, &mut state);
        state
    }

    /// 摆位置 + 调窗口尺寸。
    fn place(&mut self, window: &mut Window, cx: &mut Context<Self>, cfg: &OsdConfig) {
        // 窗口比卡片大一圈：投影要画到卡片外面去，窗口正好等于卡片的话会被裁掉。
        let pad = shadow_margin(cfg);
        let (card_w, card_h) = card_size(cfg);
        let (width, height) = (card_w + pad * 2.0, card_h + pad * 2.0);
        let area = target_area(self.monitor.unwrap_or(cfg.monitor));
        let margin = 24.0;
        let (ax, ay) = (area.origin.x.as_f32(), area.origin.y.as_f32());
        let (aw, ah) = (area.size.width.as_f32(), area.size.height.as_f32());
        let (mut x, mut y) = match cfg.anchor {
            OsdAnchor::TopLeft => (ax + margin, ay + margin),
            OsdAnchor::TopCenter => (ax + (aw - card_w) / 2.0, ay + margin),
            OsdAnchor::TopRight => (ax + aw - card_w - margin, ay + margin),
            OsdAnchor::BottomLeft => (ax + margin, ay + ah - card_h - margin),
            OsdAnchor::BottomCenter => (ax + (aw - card_w) / 2.0, ay + ah - card_h - margin),
            OsdAnchor::BottomRight => (ax + aw - card_w - margin, ay + ah - card_h - margin),
            OsdAnchor::Custom => (ax + cfg.custom_x as f32, ay + cfg.custom_y as f32),
            OsdAnchor::Cursor => {
                let (cursor_x, cursor_y) =
                    crate::windows::platform::cursor_logical(window.scale_factor());
                // 摆在光标的右下方，不挡住指针本身。
                (cursor_x + 18.0, cursor_y + 22.0)
            }
        };
        // 夹的是**卡片本体**，不是「卡片 + 影子」：影子是透明的，宁可让它在屏幕边上
        // 被裁一点，也不能因为扩散变大就把卡片往里推（以前 pad 一涨，卡片就跳着走）。
        x = x.clamp(ax, (ax + aw - card_w).max(ax));
        y = y.clamp(ay, (ay + ah - card_h).max(ay));
        // 卡片在窗口里居中，所以窗口再往左上让出 pad，卡片本体才落在锚点上。
        let (x, y) = (x - pad, y - pad);
        self.base = (x, y, width, height);
        // 强制写一遍：新位置可能刚好和上一次的位移抵消成同一个值。
        self.last_offset = f32::NAN;
        self.sync_position(window);
        if (width, height) != self.frame {
            self.frame = (width, height);
            window.bounds_changed(cx);
        }
    }

    /// 把「最终位置 + 当前位移」写进窗口。
    ///
    /// 系统风格下卡片是从下方滑上来的：直接把整扇窗往下挪，卡片和投影一起走，
    /// 不用在窗口里裁一块（裁的话影子会被切掉一截）。
    fn sync_position(&mut self, window: &Window) {
        let (x, y, width, height) = self.base;
        if width <= 0.0 || height <= 0.0 {
            return;
        }
        if !self.last_offset.is_nan() && (self.offset - self.last_offset).abs() < 0.3 {
            return;
        }
        self.last_offset = self.offset;
        crate::windows::platform::set_frame(
            window,
            gpui::Bounds::new(point(px(x), px(y + self.offset)), size(px(width), px(height))),
            window.scale_factor(),
        );
    }

    /// 起一段滑动（入场 / 出场）。
    ///
    /// 位移和整窗不透明度都交给独立线程按合成帧推进 —— gpui 的定时器实际是 16ms
    /// 粒度，165Hz 屏上看着就是一格一格。线程「跑完了」的标志存在 `slide_run` 里，
    /// 由 `tick` 收尾（转 Hold / 收起来）。
    ///
    /// `from` / `from_alpha` 是这一段的起点：正常入场是「整个在下面的距离、完全透明」；
    /// 出场演到一半被呼出时就是那一刻的位置和不透明度，于是它是接着往回走，
    /// 不是重新从底下升一遍。
    fn start_slide(
        &mut self,
        window: &Window,
        cfg: &OsdConfig,
        mode: crate::windows::platform::SlideMode,
        from: f32,
        from_alpha: f32,
    ) {
        use crate::windows::platform::{SlideMode, SlideRun};
        let motion = SystemMotion::osd(self.slide);
        // 接力入场（出场演到一半又按了一下）：位置和不透明度都只剩一小截，
        // 还按整段节奏走就成了「卡片慢慢蹭回来」—— 按剩多少把时间常数和淡入
        // 时长一起收短，回来的手感和出去的 140ms 对得上。
        let (tau_ms, fade_delay_ms, fade_ms, total_ms) =
            if mode == SlideMode::Enter && from_alpha > 0.01 {
                let remain = (1.0 - from_alpha).clamp(0.0, 1.0);
                let span = if self.slide > 0.5 {
                    (from / self.slide).clamp(0.0, 1.0)
                } else {
                    1.0
                };
                let resume = SystemMotion {
                    slide: motion.slide,
                    tau_ms: motion.tau_ms * span.clamp(0.35, 1.0),
                    fade_delay_ms: 0.0,
                    fade_ms: motion.fade_ms * remain.clamp(0.3, 1.0),
                    exit_ms: motion.exit_ms,
                };
                (
                    resume.tau_ms,
                    resume.fade_delay_ms,
                    resume.fade_ms,
                    resume.enter_total_ms(),
                )
            } else {
                (
                    motion.tau_ms,
                    motion.fade_delay_ms,
                    motion.fade_ms,
                    match mode {
                        SlideMode::Enter => motion.enter_total_ms(),
                        SlideMode::Exit => motion.exit_ms,
                    },
                )
            };
        let run = SlideRun {
            mode,
            to: (self.base.0, self.base.1),
            direction: (0.0, 1.0),
            from: match mode {
                SlideMode::Enter => from,
                SlideMode::Exit => 0.0,
            },
            end: match mode {
                SlideMode::Enter => 0.0,
                SlideMode::Exit => self.slide,
            },
            tau_ms,
            fade_delay_ms,
            fade_ms,
            total_ms,
            opacity: cfg.alpha(),
            from_alpha,
        };
        self.slide_run = Some(crate::windows::platform::run_slide(window, run));
    }

    /// 收起来：藏窗口 + 停 ticker（淡出 / 滑出都走这里）。
    fn hide_now(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        self.phase = Phase::Hidden;
        self.visible = false;
        self.dismissing = false;
        self.hold = None;
        self.fade_from = 0.0;
        // 不 `set_visible(false)`：隐藏再显示会让 DWM 给窗口补一段出现淡入，
        // 那一次滑入就变半透明。直接把窗口挪到屏幕外（下面）就是「看不见」。
        self.offset = if self.slide > 0.5 { self.slide } else { 0.0 };
        crate::windows::platform::set_opacity(window, 0.0);
        self.last_alpha = 0.0;
        self.sync_position(window);
        self.ticker = false;
        cx.notify();
        false
    }

    fn apply_alpha(&mut self, window: &Window, cfg: &OsdConfig) {
        let alpha = (self.alpha.clamp(0.0, 1.0) * cfg.alpha()).clamp(0.0, 1.0);
        if (alpha - self.last_alpha).abs() > 0.004 {
            self.last_alpha = alpha;
            crate::windows::platform::set_opacity(window, alpha);
        }
        self.sync_position(window);
    }

    fn kick_ticker(&mut self, cx: &mut Context<Self>) {
        if self.ticker || !self.visible {
            return;
        }
        self.ticker = true;
        cx.spawn(async move |this, cx| {
            loop {
                cx.background_executor().timer(TICK).await;
                let running = this
                    .update_in(cx, |this, window, cx| this.tick(window, cx))
                    .unwrap_or(false);
                if !running {
                    break;
                }
            }
        })
        .detach();
    }

    /// 文字里的时间变量是不是变了；变了就得重画一张。
    ///
    /// 只在显示期间有意义（`tick` 隐藏时会停），所以每秒一次的重绘
    /// 和淡入淡出那几帧比起来可以忽略。
    fn refresh_time_text(&mut self, cfg: &OsdConfig) -> bool {
        let lang = self.lang();
        let clock = crate::textvars::Clock::local();
        let mut next: Vec<(OsdPart, String)> = Vec::new();
        // 和 `card_visual` 画的是同一批：状态文字 + 每一条自定义文字。
        // 这里**不能**按 `cfg.parts()` 走 —— 那个清单只看当前模板的排布，
        // 排布里没登记的文字照样会画出来（按兜底位置），漏掉它就永远不刷新。
        let mut parts = vec![OsdPart::Title];
        for index in 0..cfg.texts.len() {
            parts.push(OsdPart::Text(index as u16));
        }
        for part in parts {
            if !cfg.shows_part(part) {
                continue;
            }
            let raw = cfg.text_for(part);
            if !crate::textvars::has_variable(raw) {
                continue;
            }
            next.push((part, crate::textvars::resolve(raw, &clock, lang)));
        }
        if next == self.time_text {
            return false;
        }
        self.time_text = next;
        true
    }

    /// 推进一帧淡入淡出；返回「还要不要继续跑」。
    fn tick(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        if !self.visible {
            self.ticker = false;
            return false;
        }
        let cfg = self.cfg();
        let now = Instant::now();

        // 滑动线程在跑：位置和整窗不透明度都归它管，这里只等它跑完再收尾。
        // （自己那套淡入淡出走下面的状态机，不经过这里。）
        if let Some(flag) = self.slide_run.clone() {
            if !flag.is_done() {
                return true;
            }
            self.slide_run = None;
            if matches!(self.phase, Phase::Out(_)) {
                return self.hide_now(window, cx);
            }
            self.offset = 0.0;
            self.alpha = 1.0;
            self.phase = Phase::Hold {
                until: now + Duration::from_millis(cfg.hold_ms as u64),
            };
            self.apply_alpha(window, &cfg);
            return true;
        }

        // 自定义 / 状态文字里有 `{ss}` 这类变量：这一格跳了就重画一张。
        if self.refresh_time_text(&cfg) {
            cx.notify();
            crate::windows::platform::post_repaint(window);
        }

        // 钉住：一直显示，不进入淡出，也不用管悬停。
        if self.pinned {
            self.offset = 0.0;
            self.alpha = 1.0;
            self.phase = Phase::Hold {
                until: now + Duration::from_secs(3600),
            };
            self.apply_alpha(window, &cfg);
            return true;
        }

        // 等首帧：这段时间窗口是全透明的，新内容画完才开始淡入。
        // 放在悬停判断之前 —— 否则指针正好停在卡片上会把 warmup 抢成「直接实心显示」。
        if let Phase::Warmup { until } = self.phase {
            // 等首帧的时间窗口停在起点，还没开始往上滑。
            self.offset = self.slide;
            self.alpha = 0.0;
            if now >= until {
                if animates_in(&cfg) {
                    self.phase = Phase::In(now);
                } else {
                    self.offset = 0.0;
                    self.alpha = 1.0;
                    self.phase = Phase::Hold {
                        until: now + Duration::from_millis(cfg.hold_ms as u64),
                    };
                }
            }
            self.apply_alpha(window, &cfg);
            return true;
        }

        // 悬停状态自己轮询：gpui 的 `on_hover` 收不到「直接划出窗口」那一下，
        // 只靠它的话指针一离开提示条就再也不淡出了。
        // 只认卡片本体：窗口为了画投影比卡片大一圈，手指停在影子上不算「指着它」。
        let inside = crate::windows::platform::cursor_in_window_inset(window, shadow_margin(&cfg));
        if inside != self.hovered {
            self.hovered = inside;
            // 指针刚移开：从这一刻重新计时，然后照常淡出。
            if !inside {
                self.phase = Phase::Hold {
                    until: now + Duration::from_millis(cfg.hold_ms as u64),
                };
            }
        }

        // 悬停 / 正在拖音量条时冻在完全显示，并且把停留计时一直往后推：
        // 用户指着它看、或者手还按着的时候不该突然消失。
        // **松手 / 移开之后照常倒计时淡出** —— 只有「操作期间」才不算。
        // 「收预览」（设置页关窗）这轮例外：鼠标就算停在卡片上也照常走完淡出。
        if !self.dismissing && ((self.hovered && cfg.hover_hold) || self.dragging) {
            self.offset = 0.0;
            self.alpha = 1.0;
            self.phase = Phase::Hold {
                until: now + Duration::from_millis(cfg.hold_ms as u64),
            };
            self.apply_alpha(window, &cfg);
            // 循环要继续跑：随时可能移开，得靠它发现。
            return true;
        }

        match self.phase {
            Phase::Hidden => {
                self.ticker = false;
                return false;
            }
            // 上面已经单独处理并 return 了，这里只为穷尽匹配。
            Phase::Warmup { .. } => {}
            Phase::In(start) => {
                let elapsed = now.duration_since(start).as_secs_f32();
                // 这一轮从哪涨上来：正常是 0，出场被打断时是那一刻的读数。
                let from = self.fade_from.clamp(0.0, 1.0);
                if cfg.anim_style.is_system() {
                    // 系统那套：位移指数缓出 + 同时淡入，两条曲线各走各的。
                    let motion = SystemMotion::osd(self.slide);
                    let (offset, alpha) = motion.enter(elapsed * 1000.0);
                    self.offset = offset;
                    self.alpha = from + (1.0 - from) * alpha;
                    if elapsed * 1000.0 >= motion.enter_total_ms() {
                        self.offset = 0.0;
                        self.alpha = 1.0;
                        self.phase = Phase::Hold {
                            until: now + Duration::from_millis(cfg.hold_ms as u64),
                        };
                        // 滑到位了：这一轮「被任务栏挡住」的需求结束，提到最前。
                        // 出场前 `Phase::Hold` 结束时会再压回任务栏之后，
                        // 所以收起来那一段照样是「从任务栏背后滑下去」。
                        crate::windows::platform::raise_osd(window);
                    }
                } else {
                    self.offset = 0.0;
                    let duration = cfg.fade_in_ms as f32 / 1000.0;
                    if duration <= 0.0 || elapsed >= duration {
                        self.alpha = 1.0;
                        self.phase = Phase::Hold {
                            until: now + Duration::from_millis(cfg.hold_ms as u64),
                        };
                    } else {
                        self.alpha = from + (1.0 - from) * (elapsed / duration).clamp(0.0, 1.0);
                    }
                }
            }
            Phase::Hold { until } => {
                self.offset = 0.0;
                self.alpha = 1.0;
                // 停留期间守住组首：会有置顶软件周期性把自己抢回最前，
                // 被抢走的话提示条就整块埋在它下面（用户：「置顶的软件就抢不过了」）。
                if self.last_raise.elapsed() >= RAISE_INTERVAL {
                    crate::windows::platform::raise_osd(window);
                    self.last_raise = now;
                }
                if now >= until {
                    // 出场前再压一次：整段动画里只有这一步会「从任务栏前面滑下去」，
                    // 谁在这之前把它提上去了（首帧 resize、激活、系统重排）在这儿兜住。
                    crate::windows::platform::place_osd(window);
                    // 系统风格：出场也交给滑动线程（滑下去 + 快淡出）。
                    if cfg.anim_style.is_system() && cfg.animate && self.slide > 0.5 {
                        self.start_slide(
                            window,
                            &cfg,
                            crate::windows::platform::SlideMode::Exit,
                            0.0,
                            self.alpha,
                        );
                    }
                    self.phase = Phase::Out(now);
                }
            }
            Phase::Out(start) => {
                // 关掉「淡入淡出」之后这一幕必须立刻收：以前这里不看开关，
                // 出来是「啪」一下、消失却还慢慢淡 240ms，一快一慢很别扭
                // （`fade_in` 那边三处都判了 `animate`，只有这里漏了）。
                let elapsed = now.duration_since(start).as_secs_f32();
                if cfg.anim_style.is_system() && cfg.animate {
                    // 系统那套：滑回下方 + 快淡出（实测约 140ms）。
                    let motion = SystemMotion::osd(self.slide);
                    let (offset, alpha) = motion.exit(elapsed * 1000.0);
                    self.offset = offset;
                    self.alpha = alpha;
                } else {
                    self.offset = 0.0;
                    let duration = if cfg.animate {
                        cfg.fade_out_ms as f32 / 1000.0
                    } else {
                        0.0
                    };
                    if duration <= 0.0 || elapsed >= duration {
                        self.alpha = 0.0;
                    } else {
                        self.alpha = (1.0 - elapsed / duration).clamp(0.0, 1.0);
                    }
                }
            }
        }

        self.apply_alpha(window, &cfg);
        if matches!(self.phase, Phase::Out(_)) && self.alpha <= 0.0 {
            // 收起来了，这一轮「关窗即收」的任务也算完成；这轮的手势记录一并清掉
            // （下次弹出来可能很久以后，沿用旧基准会把判据一下子判成「快照动过」）。
            return self.hide_now(window, cx);
        }
        true
    }

    /// 滚轮：直接调音量，读数当场变（不等音频线程那 33ms）。
    fn on_hover_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let cfg = self.cfg();
        // `wheel_hover` 就是「悬浮滚轮调节」这个开关：关掉整张卡片都不认滚轮。
        if !cfg.interactive || !cfg.wheel_hover || !self.visible {
            return;
        }
        if !matches!(self.state.function, OsdFunction::Volume | OsdFunction::Mute) {
            return;
        }
        // 事件是挂在**整张卡片**上的，所以走到这里光标一定在卡片里（窗口为投影留的
        // 那一圈留白不算卡片，滚不到）。早先只认音量条那一条，用户得把指针精确压在
        // 细条上才好使 —— 现在悬浮即可，条上条下都行。
        let notches = wheel_direction(event.delta);
        if notches == 0.0 {
            return;
        }
        let now = Instant::now();
        if let Some(last) = self.wheel_last_at {
            if now.duration_since(last) < Duration::from_millis(3) {
                return;
            }
        }
        self.wheel_last_at = Some(now);

        let config = self.config.borrow().get().clone();
        let min_db = config.logarithmic_min_db as f32;
        // 用当前读数当起点，别等快照（否则连滚几格会用同一个旧值算）。
        let display = self.state.fraction;
        let next = config.scroll_display(display, notches);
        let raw = if config.use_logarithmic_volume {
            to_log_volume(next, min_db)
        } else {
            next
        };
        match self.state.target.clone() {
            OsdTarget::Device => {
                let unmute = config
                    .unmute
                    .allows(crate::config::VolumeGesture::HoverScroll, notches > 0.0);
                if let Some(device_id) = self.default_device_id() {
                    self.engine.send(AudioCommand::SetDeviceVolume {
                        device_id,
                        volume: raw,
                        unmute,
                    });
                }
            }
            OsdTarget::App { key, .. } => {
                let unmute = config
                    .unmute
                    .allows(crate::config::VolumeGesture::HoverScroll, notches > 0.0);
                self.engine.send(AudioCommand::SetAppVolume {
                    key,
                    volume: raw,
                    unmute,
                });
            }
        }
        if (next - display).abs() > 0.0001 {
            crate::platform::sound::step(
                &config,
                crate::config::VolumeGesture::HoverScroll,
            );
        }
        let (percent, db) = readouts(next, config.use_logarithmic_volume, min_db);
        self.state.percent = percent;
        self.state.db = db;
        self.state.fraction = next;
        if config
            .unmute
            .allows(crate::config::VolumeGesture::HoverScroll, notches > 0.0)
        {
            self.state.muted = false;
        }
        // 滚轮也是手势：快照回头还是旧值，先记一笔顶住。
        self.note_gesture();
        // 调节期间重新计时，手不停它就不消失。
        self.phase = Phase::Hold {
            until: Instant::now() + Duration::from_millis(self.cfg().hold_ms as u64),
        };
        self.alpha = 1.0;
        self.apply_alpha(window, &self.cfg());
        cx.notify();
        self.kick_ticker(cx);
    }

    /// 拖动音量条：和面板里那根滑条同一套逻辑 —— 按位置写音量、
    /// 上限由音频线程兜底、往上调顺手解除静音（走「拖动」那一路开关）。
    fn on_bar_change(&mut self, display: f32, window: &mut Window, cx: &mut Context<Self>) {
        let cfg = self.cfg();
        if !cfg.interactive || !self.visible {
            return;
        }
        if !matches!(self.state.function, OsdFunction::Volume | OsdFunction::Mute) {
            return;
        }
        let config = self.config.borrow().get().clone();
        let min_db = config.logarithmic_min_db as f32;
        let display = display.clamp(0.0, 1.0);
        let raw = if config.use_logarithmic_volume {
            to_log_volume(display, min_db)
        } else {
            display
        };
        let increase = display > self.state.fraction;
        let unmute = config
            .unmute
            .allows(crate::config::VolumeGesture::Drag, increase);
        match self.state.target.clone() {
            OsdTarget::Device => {
                if let Some(device_id) = self.default_device_id() {
                    self.engine.send(AudioCommand::SetDeviceVolume {
                        device_id,
                        volume: raw,
                        unmute,
                    });
                }
            }
            OsdTarget::App { key, .. } => {
                self.engine.send(AudioCommand::SetAppVolume {
                    key,
                    volume: raw,
                    unmute,
                });
            }
        }
        if (display - self.state.fraction).abs() > 0.0001 {
            crate::platform::sound::step(&config, crate::config::VolumeGesture::Drag);
        }
        let (percent, db) = readouts(display, config.use_logarithmic_volume, min_db);
        self.state.percent = percent;
        self.state.db = db;
        self.state.fraction = display;
        if unmute {
            self.state.muted = false;
        }
        self.state.icon = crate::ui::volume_icon_name(raw, self.state.muted);
        apply_style(&cfg, &mut self.state);
        // 手按着靠 `dragging` 挡快照；松手之后还得靠这一笔 —— 不然快照一帧
        // 就把读数和图标拽回旧值，再跳回来。
        self.note_gesture();
        self.dragging = true;
        // 手按着的时候绝不淡出。
        self.phase = Phase::Hold {
            until: Instant::now() + Duration::from_millis(cfg.hold_ms as u64),
        };
        self.alpha = 1.0;
        self.apply_alpha(window, &cfg);
        cx.notify();
    }

    /// 松开滑条：手一停就重新开始倒计时，到点照常淡出。
    ///
    /// 松手那一声提示音跟面板同一个开关，OSD 这一侧另外还要过 `release_sound_osd`
    /// 这个子开关（面板里是主开关直接管）。亮度 / 低音那几条虽然也画着滑条，
    /// 拖它并不改设置（`on_bar_change` 早退），所以不跟着响。
    fn on_bar_release(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.dragging = false;
        if !self.visible {
            return;
        }
        let cfg = self.cfg();
        self.phase = Phase::Hold {
            until: Instant::now() + Duration::from_millis(cfg.hold_ms as u64),
        };
        // 声音开关在全局配置里（`cfg` 是 OSD 自己的外观那一套）。
        let sound = self.config.borrow().get().clone();
        if sound.release_sound
            && sound.release_sound_osd
            // 同上：拖拽音效管着这根滑条，末一格已经响过。
            && !sound.volume_sound.covers(crate::config::VolumeGesture::Drag)
            && matches!(self.state.function, OsdFunction::Volume | OsdFunction::Mute)
        {
            crate::platform::sound::tick(window);
        }
        self.kick_ticker(cx);
    }

    /// 钉住 / 取消钉住。
    ///
    /// 钉住时提示条一直显示（设置页里对着调外观用）；再点一下**立刻**淡出收起 ——
    /// 不等停留时间，点完就该走。还没显示过就先按当前音量弹一条。
    pub fn set_pinned(&mut self, pinned: bool, window: &mut Window, cx: &mut Context<Self>) {
        self.pinned = pinned;
        // 重新钉住说明用户又要长期看着它，把「收预览」的强制淡出撤掉。
        self.dismissing = false;
        let cfg = self.cfg();
        if pinned {
            if !self.visible {
                self.state = self.build_state(&OsdRequest::device(OsdFunction::Volume), &cfg);
                self.place(window, cx, &cfg);
                crate::windows::platform::set_visible(window, true);
                // 预览也一样要压在设置窗上面（设置窗自己也是置顶的）。
                crate::windows::platform::raise_osd(window);
                self.visible = true;
            }
            self.alpha = 1.0;
            self.phase = Phase::Hold {
                until: Instant::now() + Duration::from_secs(3600),
            };
        } else if self.visible {
            // 再点一下是「收起来」：立刻淡出，不再等一轮停留时间。
            // 用户已经盯着它调完了，多出来的那两秒只像「点了没反应」。
            self.dismissing = true;
            self.phase = Phase::Out(Instant::now());
        }
        self.apply_alpha(window, &cfg);
        cx.notify();
        self.kick_ticker(cx);
    }

    pub fn is_pinned(&self) -> bool {
        self.pinned
    }

    /// 收掉「保持显示」的预览（设置页关窗时用）。
    ///
    /// 解除钉住并**立刻**开始淡出：不等 `hold_ms`。窗口都关了，那张卡片再挂一两秒
    /// 也没人看，只是挡在别的窗口前面 —— 用户的感觉是「关设置，预览跟着走」。
    pub fn release_pin(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        if !self.pinned {
            return;
        }
        self.pinned = false;
        if self.visible {
            self.dismissing = true;
            self.phase = Phase::Out(Instant::now());
            let cfg = self.cfg();
            self.apply_alpha(window, &cfg);
        }
        cx.notify();
        self.kick_ticker(cx);
    }

    /// 点击喇叭图标：音量 / 静音切静音，播放暂停再发一次播放暂停键。
    ///
    /// 只有图标这一小块可点 —— 整张卡片都能点的话，想拖音量条时点空处就静音了。
    fn on_click(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let cfg = self.cfg();
        if !cfg.interactive || !self.visible {
            return;
        }
        match self.state.function {
            OsdFunction::Volume | OsdFunction::Mute => {
                let muted = !self.state.muted;
                match self.state.target.clone() {
                    OsdTarget::Device => {
                        if let Some(device_id) = self.default_device_id() {
                            self.engine
                                .send(AudioCommand::SetDeviceMute { device_id, muted });
                        }
                    }
                    OsdTarget::App { key, .. } => {
                        self.engine.send(AudioCommand::SetAppMute { key, muted });
                    }
                }
                self.state.muted = muted;
                self.state.icon = if muted {
                    "volume-off"
                } else {
                    crate::ui::volume_icon_name(self.state.fraction, false)
                };
                apply_style(&cfg, &mut self.state);
                let lang = self.lang();
                self.state.title = if muted {
                    lang.pick("已静音", "Muted")
                } else {
                    lang.pick("取消静音", "Unmuted")
                }
                .to_string();
                self.phase = Phase::Hold {
                    until: Instant::now() + Duration::from_millis(cfg.hold_ms as u64),
                };
                self.alpha = 1.0;
                self.apply_alpha(window, &cfg);
                cx.notify();
                self.kick_ticker(cx);
            }
            OsdFunction::PlayPause => {
                // 和键盘那条路一个道理：先问系统现在在不在放，翻过来才是这一下
                // 按完的状态；问不到就翻卡片上正显示的（至少看起来是连贯的）。
                let playing = crate::platform::media::is_playing()
                    .map(|now| !now)
                    .or_else(|| self.state.playing.map(|shown| !shown));
                crate::platform::hotkey::send_media_key(
                    crate::platform::hotkey::VK_MEDIA_PLAY_PAUSE,
                );
                let lang = self.lang();
                let (title, icon) = media_action_text(OsdFunction::PlayPause, playing, lang);
                self.state.title = title.to_string();
                self.state.icon = icon;
                self.state.playing = playing;
                // 点完把提示条再挂一会儿，让人看清状态变了。
                self.phase = Phase::Hold {
                    until: Instant::now() + Duration::from_millis(cfg.hold_ms as u64),
                };
                self.alpha = 1.0;
                self.apply_alpha(window, &cfg);
                cx.notify();
                self.kick_ticker(cx);
            }
            _ => {}
        }
    }

    fn default_device_id(&self) -> Option<String> {
        let snapshot = self.snapshot.borrow();
        snapshot
            .devices
            .iter()
            .find(|device| device.is_default)
            .or_else(|| snapshot.devices.iter().find(|device| device.enabled))
            .map(|device| device.id.clone())
    }
}

impl Render for Osd {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let cfg = self.cfg();

        // 滑条要的回调：全都是「回到实体上再处理」，所以这里先拿到弱引用。
        let change_handle = cx.entity().downgrade();
        let release_handle = cx.entity().downgrade();
        let bar = OsdBar {
            drag: self.slider.clone(),
            on_change: Rc::new(move |value: f32, window, cx| {
                let _ = change_handle.update(cx, |this, cx| {
                    this.on_bar_change(value, window, cx);
                });
            }),
            on_release: Rc::new(move |window: &mut Window, cx| {
                let _ = release_handle.update(cx, |this, cx| this.on_bar_release(window, cx));
            }),
        };

        let measure = TextMeasure::new(window);
        let mut card = card_visual(&cfg, &self.state, 1.0, Some(&bar), &measure, self.lang())
            .id("bolume-osd")
            // 滚轮挂在整张卡片上（滑条元素自己的命中框不一定让事件冒出来）；
            // 是否真的动手由 `on_hover_wheel` 按「悬浮滚轮调节」开关决定。
            .on_scroll_wheel(cx.listener(|this, event: &ScrollWheelEvent, window, cx| {
                this.on_hover_wheel(event, window, cx);
            }));

        // 只有喇叭 / 播放暂停那一小块可点。
        if matches!(
            self.state.function,
            OsdFunction::Volume | OsdFunction::Mute | OsdFunction::PlayPause
        ) {
            let (x, y, w, h) = part_rect(&cfg, OsdPart::Icon);
            card = card.child(
                div()
                    .id("osd-icon-zone")
                    .absolute()
                    .left(px((x - 4.0).max(0.0)))
                    .top(px((y - 4.0).max(0.0)))
                    .w(px(w + 8.0))
                    .h(px(h + 8.0))
                    .rounded(px(6.0))
                    .cursor_pointer()
                    .on_click(cx.listener(|this, _: &gpui::ClickEvent, window, cx| {
                        this.on_click(window, cx);
                    })),
            );
        }

        div()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .child(card)
            .into_any_element()
    }
}

/// 设备该显示的名字（有自定义名就只显示自定义名）。
fn device_name(config: &Config, device: &DeviceInfo) -> String {
    let raw = if device.name.trim().is_empty() {
        device.id.as_str()
    } else {
        device.name.as_str()
    };
    config.device_label(&device.id, raw)
}

fn find_app<'a>(snapshot: &'a Snapshot, key: &str) -> Option<&'a AppInfo> {
    snapshot.apps.iter().find(|app| app.key == key)
}

/// 正在发声的应用名（总体信息模式的副行）。
fn current_app_name(snapshot: &Snapshot) -> String {
    let Some(app) = snapshot.current_app() else {
        return String::new();
    };
    app.name.clone()
}

/// OSD 要贴在哪个显示器上（`monitor`：0 = 主显示器，n = 第 n 台）。
///
/// 工作区已经从系统拿到（排除了任务栏），坐标和 gpui 的逻辑像素一致。
pub fn target_area(monitor: u32) -> gpui::Bounds<Pixels> {
    let fallback = || gpui::Bounds::new(point(px(0.), px(0.)), size(px(1920.), px(1080.)));
    let list = crate::platform::monitors::monitors();
    let chosen = if monitor == 0 {
        list.iter()
            .find(|info| info.is_primary)
            .or_else(|| list.first())
    } else {
        list.get(monitor as usize - 1)
            .or_else(|| list.iter().find(|info| info.is_primary))
            .or_else(|| list.first())
    };
    match chosen {
        Some(info) => gpui::Bounds::new(
            point(px(info.work.0), px(info.work.1)),
            size(px(info.work.2), px(info.work.3)),
        ),
        None => fallback(),
    }
}

/// 这次该在哪几块屏上弹（每台一扇窗，返回值就是窗口的「显示器键」）。
///
/// - 指定一台 → 就那台（0 = 主显示器，n = 第 n 台）；
/// - 跟随鼠标 → 光标此刻所在那台（取不到退回配置里那台）；
/// - 全部显示器 → 每台一个，`1..=n`。
pub fn monitor_keys(cfg: &OsdConfig) -> Vec<u32> {
    match cfg.monitor_mode {
        crate::config::OsdMonitorMode::Single => vec![cfg.monitor],
        crate::config::OsdMonitorMode::Cursor => {
            vec![crate::platform::monitors::index_at_cursor().unwrap_or(cfg.monitor)]
        }
        crate::config::OsdMonitorMode::All => {
            let count = crate::platform::monitors::monitors().len() as u32;
            if count == 0 {
                vec![cfg.monitor]
            } else {
                (1..=count).collect()
            }
        }
    }
}

/// 供设置页展示的显示器：`(序号, 名字)` —— 显示器的友好名（`XG279Q` 这种），
/// 不再拿分辨率凑数。
pub fn display_options() -> Vec<(u32, String)> {
    crate::platform::monitors::monitors()
        .iter()
        .enumerate()
        .map(|(index, info)| {
            let name = if info.is_primary {
                format!("{} ★", info.name)
            } else {
                info.name.clone()
            };
            (index as u32 + 1, name)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 投影的留白要够渲染器铺满的那块四边形（3×模糊半径）。
    ///
    /// 以前这里只留 1.5 倍、还把 blur 夹在 60，扩散一大影子就被窗口边缘切掉。
    #[test]
    fn shadow_margin_covers_the_whole_blur() {
        let mut cfg = OsdConfig::default();
        cfg.shadow.enabled = true;
        cfg.shadow.blur = 120;
        cfg.shadow.offset_x = -30;
        // 3×120 + 30 的偏移还要再加余量。
        assert!(shadow_margin(&cfg) >= 120.0 * 3.0 + 30.0);
        // 关着就不留白。
        cfg.shadow.enabled = false;
        assert_eq!(shadow_margin(&cfg), 0.0);
    }

    /// 动态图标样式：同一套里按档位换图，静音档四套共用一张。
    #[test]
    fn dynamic_icon_styles_pick_the_right_state() {
        // 档位名只认后缀，四套图标都成立。
        for icon in ["volume-off", "volume-mute"] {
            assert_eq!(volume_slot(icon), 0, "{icon}");
        }
        assert_eq!(volume_slot("volume-low"), 1);
        assert_eq!(volume_slot("volume-bar-low"), 1);
        assert_eq!(volume_slot("volume-dot-low"), 1);
        assert_eq!(volume_slot("volume-high"), 2);
        assert_eq!(volume_slot("volume-arc-high"), 2);

        let mut cfg = OsdConfig::default();
        for (style, want) in [
            ("bar", ["volume-off", "volume-bar-low", "volume-bar-high"]),
            ("dot", ["volume-off", "volume-dot-low", "volume-dot-high"]),
            ("arc", ["volume-off", "volume-arc-low", "volume-arc-high"]),
            ("auto", ["volume-off", "volume-low", "volume-high"]),
        ] {
            cfg.icon_style = style.to_string();
            let entry = cfg.icon_style();
            assert_eq!(entry.key, style);
            let states = entry.states.expect("这几套都是动态的");
            assert_eq!(states, want, "{style}");
        }
        // 光喇叭：静音有自己单独一张（喇叭 + 叉），小声 / 大声共用喇叭本身。
        cfg.icon_style = "speaker".to_string();
        assert_eq!(
            cfg.icon_style().states,
            Some(["volume-off", "speaker", "speaker"]),
            "音箱这套也要有静音态"
        );
        assert_eq!(cfg.fixed_icon(), None);
        // 认不出来的值回落到 auto，不至于没有图标。
        cfg.icon_style = "whatever".to_string();
        assert_eq!(cfg.icon_style().key, "auto");
    }

    /// 拖到别人同一条竖线 / 卡片边距附近要吸过去，远的地方不吸。
    #[test]
    fn snapping_aligns_to_neighbours_and_margins() {
        let cfg = OsdConfig::default();

        // 左边距 13px（默认布局里图标也挨着这条线）：标题拖到 15 应吸过去。
        let (x, _, guide_x, _) = snap_position(&cfg, OsdPart::Title, 15.0, 200.0);
        assert!((x - 13.0).abs() < 0.05, "应该吸到左边距 13px，实际 {x}");
        assert_eq!(guide_x, Some(x));

        // y 已经出了卡片，谁都不挨着，不许动、也不给参考线。
        let (_, y, _, guide_y) = snap_position(&cfg, OsdPart::Title, 200.0, 200.0);
        assert!((y - 200.0).abs() < 0.01);
        assert_eq!(guide_y, None);

        // 垂直居中：只留标题，中线 = (卡片高 - 标题高) / 2。
        let mut centered = OsdConfig::default();
        for part in [OsdPart::Percent, OsdPart::Db, OsdPart::Bar, OsdPart::Icon] {
            centered.set_part_visible(part, false);
        }
        let height = editor_rect(&centered, OsdPart::Title).3;
        let middle = (card_size(&centered).1 - height) / 2.0;
        let (_, y, _, guide_y) = snap_position(&centered, OsdPart::Title, 200.0, middle + 0.5);
        assert!((y - middle).abs() < 0.2, "应该吸到垂直中线，实际 {y}");
        assert_eq!(guide_y, Some(card_size(&centered).1 / 2.0));
    }

    /// 吸附半径只有 3px：差一点点才兜住，稍微想离开就离开。
    #[test]
    fn snapping_reach_is_three_pixels() {
        let cfg = OsdConfig::default();
        let (width, _) = card_size(&cfg);
        let (_, _, w, _) = editor_rect(&cfg, OsdPart::Icon);
        let centered = (width - w) / 2.0;

        for offset in [-2.5, -1.0, 0.0, 2.5] {
            let (x, _, _, _) = snap_position(&cfg, OsdPart::Icon, centered + offset, 30.0);
            assert!(
                (x - centered).abs() < 0.01,
                "差 {offset} 应该在 3px 内被吸住"
            );
        }
        // 超出 3px 就不该再被中线拽住（别的候选线不管，只看中线）。
        for offset in [-6.0, 4.0] {
            let (_, _, guide, _) = snap_position(&cfg, OsdPart::Icon, centered + offset, 30.0);
            assert_ne!(guide, Some(width / 2.0), "差 {offset} 不该吸到中线");
        }
    }

    /// 居中吸附要压过旁边的边线：离中线够近时，就算某条左沿只差 1px，也吸到中线。
    #[test]
    fn snapping_prefers_the_center_lines() {
        let cfg = OsdConfig::default();
        let (width, _) = card_size(&cfg);
        let (_, _, w, _) = editor_rect(&cfg, OsdPart::Icon);
        let centered = (width - w) / 2.0;

        // 中线附近 ±3 都算「想摆正」。
        for offset in [-2.0, -1.0, 0.0, 2.0] {
            let (x, _, guide, _) = snap_position(&cfg, OsdPart::Icon, centered + offset, 30.0);
            assert!(
                (x - centered).abs() < 0.01,
                "偏离 {offset} 应该吸到中线 {centered}，实际 {x}"
            );
            assert_eq!(guide, Some(width / 2.0));
        }

        // 离得远了就不许乱吸（+40 已经越过中线、也挨不着任何一条候选线）。
        let (x, _, guide, _) = snap_position(&cfg, OsdPart::Icon, centered + 40.0, 30.0);
        assert!((x - (centered + 40.0)).abs() < 0.01);
        assert_eq!(guide, None);
    }

    /// 两个元素可以「中心对中心」，不只是边对边。
    #[test]
    fn snapping_matches_neighbour_centers() {
        let mut cfg = OsdConfig::default();
        // 只留分贝，摆到卡片正中，再拖音量条的左沿去贴它的中心。
        for part in [OsdPart::Icon, OsdPart::Title, OsdPart::Percent] {
            cfg.set_part_visible(part, false);
        }
        let (_, _, dw, _) = editor_rect(&cfg, OsdPart::Db);
        let (width, _) = card_size(&cfg);
        cfg.set_part(OsdPart::Db, (width - dw) / 2.0 / width, 0.2);

        let (_, _, bw, _) = editor_rect(&cfg, OsdPart::Bar);
        let db_center = editor_rect(&cfg, OsdPart::Db).0 + dw / 2.0;
        // 我的中心落到它的中心上需要的位置。
        let target = db_center - bw / 2.0;
        let (x, _, guide, _) = snap_position(&cfg, OsdPart::Bar, target + 3.0, 40.0);
        assert!((x - target).abs() < 0.01, "应该吸到邻居中心，实际 {x}");
        assert_eq!(guide, Some(db_center));
    }

    /// 关掉的元素不参与吸附：删了音量条就别再吸到它身上。
    #[test]
    fn hidden_parts_are_ignored_when_snapping() {
        // 给一块宽大的卡片，免得各个候选线挤在一起、分不清是谁吸的。
        let mut cfg = OsdConfig::default();
        cfg.width = 400;
        cfg.height = 300;
        // 音量条摆到靠上（0.2），远离卡片中线 —— 否则会被「居中吸附」先接住。
        cfg.set_part(OsdPart::Bar, 0.2, 0.2);
        // 用编辑框的顶边：音量条的编辑框上下各让了 6px，拿元素本身的 y 会有 6px 的差，
        // 超出 3px 的吸附半径。
        let bar_y = editor_rect(&cfg, OsdPart::Bar).1;
        // 分贝摆到音量条那条线上：音量条还在，应该给出一条水平参考线。
        let (_, _, _, guide) = snap_position(&cfg, OsdPart::Db, 250.0, bar_y);
        assert!(guide.is_some(), "音量条还在时应该吸到它");

        // 删掉音量条之后，同一条线不再是候选。
        cfg.set_part_visible(OsdPart::Bar, false);
        let (_, _, _, guide) = snap_position(&cfg, OsdPart::Db, 250.0, bar_y);
        assert!(guide.is_none(), "已删除的元素不该再吸");
    }

    /// 元素尺寸缺省时回落到各自的默认值，越界会被夹住。
    #[test]
    fn part_sizes_fall_back_and_clamp() {
        let mut cfg = OsdConfig::default();
        cfg.layout.clear();
        // 清空布局后回落到 fallback：额外文字的默认字号就是 13。
        assert_eq!(
            cfg.part(OsdPart::Text(0)).size,
            OsdPart::Text(0).default_size()
        );
        cfg.set_part_size(OsdPart::Db, 999);
        assert_eq!(cfg.part(OsdPart::Db).size, OsdPart::Db.size_range().1);
        cfg.set_part_size(OsdPart::Db, 1);
        assert_eq!(cfg.part(OsdPart::Db).size, OsdPart::Db.size_range().0);
    }

    /// 播放 / 暂停共用一个键：按完的状态要分别显示「播放」和「暂停」，
    /// 图标也得跟着换 —— 否则按了哪一下在屏幕上看不出来。
    #[test]
    fn play_and_pause_are_told_apart() {
        let zh = Lang::Zh;
        assert_eq!(
            media_action_text(OsdFunction::PlayPause, Some(true), zh),
            ("播放", "play")
        );
        assert_eq!(
            media_action_text(OsdFunction::PlayPause, Some(false), zh),
            ("暂停", "pause")
        );
        // 问不出系统状态时退回中性的「播放 / 暂停」，图标保持 play。
        assert_eq!(
            media_action_text(OsdFunction::PlayPause, None, zh),
            (OsdFunction::PlayPause.label(zh), "play")
        );
        // 其它媒体键各是各的样子，不受影响。
        assert_eq!(
            media_action_text(OsdFunction::NextTrack, None, zh),
            ("下一曲", "next")
        );
        assert_eq!(
            media_action_text(OsdFunction::PrevTrack, None, zh),
            ("上一曲", "prev")
        );
        assert_eq!(
            media_action_text(OsdFunction::Stop, None, zh),
            ("停止", "stop")
        );
    }

    /// 读数是一对：百分比与分贝各自算，互不影响。
    #[test]
    fn readouts_split_percent_and_db() {
        let (percent, db) = readouts(0.5, false, -40.0);
        assert_eq!(percent, "50%");
        assert_eq!(db, "-6.0 dB");
        let (percent, db) = readouts(0.0, true, -40.0);
        assert_eq!(percent, "0%");
        assert_eq!(db, "-∞ dB");
        let (percent, db) = readouts(1.0, false, -40.0);
        assert_eq!(percent, "100%");
        assert_eq!(db, "0.0 dB");
    }

    /// 宽度：0 = 自动铺到右边距，写了值就按值来（但不会超出可用宽度）。
    #[test]
    fn part_width_auto_and_explicit() {
        let mut cfg = OsdConfig::default();
        // 默认外观里音量条是固定宽度，先掰回「自动」再测。
        let card_w = cfg.width as f32;
        cfg.set_part_width(OsdPart::Bar, 0);
        let (x, _, w, _) = part_rect(&cfg, OsdPart::Bar);
        // 自动：从 x 一直铺到右边距（卡片宽 - 右边距）。
        assert!(
            (w - (card_w - x - PAD_RIGHT)).abs() < 0.01,
            "自动宽度应铺满，实际 {w}"
        );

        cfg.set_part_width(OsdPart::Bar, 120);
        let (_, _, w, _) = part_rect(&cfg, OsdPart::Bar);
        assert!((w - 120.0).abs() < 0.01);

        // 写一个超过卡片的宽度：渲染时被可用宽度截住，但不会越界。
        // 显式宽度可以一直贴到卡片右边缘（左边能贴 0，右边也对称），不再留右边距。
        cfg.set_part_width(OsdPart::Bar, 999);
        let (_, _, w, _) = part_rect(&cfg, OsdPart::Bar);
        assert!(w <= card_w - x + 0.01);

        // 传 0 回到自动。
        cfg.set_part_width(OsdPart::Bar, 0);
        assert_eq!(cfg.part(OsdPart::Bar).width, 0);
    }

    /// 宽度定下来之后，拖框体 = 整块平移（左右两边一起走）。
    #[test]
    fn explicit_width_keeps_the_block_intact() {
        let mut cfg = OsdConfig::default();
        // 卡片宽一点，100px 的块挪 30px 之后不会被右边距截住（截住就成了改宽度）。
        cfg.width = 400;
        let card_w = cfg.width as f32;
        cfg.set_part(OsdPart::Bar, 0.30, 0.75);
        cfg.set_part_width(OsdPart::Bar, 100);
        let (x0, _, w0, _) = part_rect(&cfg, OsdPart::Bar);
        // 挪 30px 之后宽度不变，只有 x 变。
        cfg.set_part(OsdPart::Bar, (x0 + 30.0) / card_w, 0.75);
        let (x1, _, w1, _) = part_rect(&cfg, OsdPart::Bar);
        assert!((w1 - w0).abs() < 0.01, "宽度不该跟着位置变");
        assert!(
            (x1 - x0 - 30.0).abs() < 0.5,
            "应该整块平移，实际 {x0} → {x1}"
        );
    }

    /// 两个模板的排布互不影响：改一个，另一个原样。
    #[test]
    fn media_layout_is_independent_from_volume() {
        let mut cfg = OsdConfig::default();
        let media_icon_x = {
            let mut media = OsdConfig::default();
            media.detailed = true;
            media.part(OsdPart::Icon).x
        };
        cfg.set_part(OsdPart::Icon, 0.5, 0.5);
        cfg.detailed = true;
        let icon = cfg.part(OsdPart::Icon);
        assert!(
            (icon.x - media_icon_x).abs() < 0.001,
            "媒体键模板该用自己的默认位置，实际 {}（应为 {media_icon_x}）",
            icon.x
        );
        cfg.set_part(OsdPart::Icon, 0.8, 0.8);
        cfg.detailed = false;
        assert!((cfg.part(OsdPart::Icon).x - 0.5).abs() < 0.001);
    }

    /// 「指定一台」时窗口键就是配置里那台（0 = 主显示器，n = 第 n 台）。
    ///
    /// 另外两种模式要问系统要显示器 / 光标位置，放真机探针里验。
    #[test]
    fn single_monitor_mode_uses_the_configured_screen() {
        let mut cfg = OsdConfig::default();
        assert_eq!(cfg.monitor_mode, crate::config::OsdMonitorMode::Single);
        assert_eq!(monitor_keys(&cfg), vec![0]);
        cfg.monitor = 2;
        assert_eq!(monitor_keys(&cfg), vec![2]);
    }

    /// 对齐：读数默认靠右，文字默认靠左，可以各自改。
    #[test]
    fn align_defaults_per_part_and_sticks() {
        let mut cfg = OsdConfig::default();
        assert_eq!(cfg.align_of(OsdPart::Percent), OsdAlign::Right);
        assert_eq!(cfg.align_of(OsdPart::Title), OsdAlign::Left);
        cfg.set_part_align(OsdPart::Title, OsdAlign::Center);
        assert_eq!(cfg.align_of(OsdPart::Title), OsdAlign::Center);
        // 只动了标题，读数还是靠右。
        assert_eq!(cfg.align_of(OsdPart::Percent), OsdAlign::Right);
    }

    /// 造一笔「已经过去 `age`」的手势记录。
    fn hold_age(age: Duration) -> OsdHold {
        OsdHold {
            at: Instant::now() - age,
        }
    }

    /// 手势刚写完、快照还停在旧值时，卡片不能被拽回旧读数。
    #[test]
    fn hold_keeps_reading_while_snapshot_is_stale() {
        let mut hold = hold_age(HOLD_SETTLE + Duration::from_millis(20));
        assert!(resolve_hold(&mut hold, (0.5, false)));
        assert!(resolve_hold(&mut hold, (0.5, false)));
    }

    /// 快照「看起来已经到位」也不算交还 —— 写入在 USB / 蓝牙端点上会晚几百
    /// 毫秒落地，这段时间的快照是假的，一交还就正好撞上迟到的旧值。
    #[test]
    fn hold_ignores_snapshot_that_looks_settled() {
        let mut hold = hold_age(HOLD_SETTLE + Duration::from_millis(20));
        assert!(resolve_hold(&mut hold, (0.7, false)));
        assert!(resolve_hold(&mut hold, (0.7, false)));
    }

    /// 静音 → **立刻**解除静音：队列里前一条命令晚一步生效，快照会先冒出
    /// `true`（静音）再冒回 `false`。这一整段都得顶住不显示 —— 否则就是
    /// 一个「静音 ↔ 未静音」的可见循环。
    #[test]
    fn hold_swallows_command_queue_bounce() {
        // 最后一次手势是「解除静音」，刚发生。
        let mut hold = hold_age(Duration::ZERO);
        // 前一条静音命令落地的中间态。
        assert!(resolve_hold(&mut hold, (0.5, true)));
        // 解除命令落地。
        assert!(resolve_hold(&mut hold, (0.5, false)));
        // 静默期过后也还顶着，直到上限。
        hold.at = Instant::now() - HOLD_SETTLE - Duration::from_millis(20);
        assert!(resolve_hold(&mut hold, (0.5, false)));
    }

    /// 写入被拒、快照一动不动：顶到上限就让位，卡片不能一直挂着假值。
    #[test]
    fn hold_expires() {
        let mut hold = hold_age(OSD_HOLD_FOR + Duration::from_millis(1));
        assert!(!resolve_hold(&mut hold, (0.2, false)));
    }
}

/// OSD 预览里框体的手柄：四角 + 四条边的中点。
///
/// - 拖角：宽高一起改；
/// - 拖左右边中点：只改宽度；
/// - 拖上下边中点：只改字号（图标 / 音量条就是边长 / 粗细）。
///
/// 拖「左 / 上」那一侧的手柄时，对边是锚点（右 / 下边缘不动）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OsdHandle {
    TopLeft,
    Top,
    TopRight,
    Right,
    BottomRight,
    Bottom,
    BottomLeft,
    Left,
}

impl OsdHandle {
    pub const ALL: [OsdHandle; 8] = [
        OsdHandle::TopLeft,
        OsdHandle::Top,
        OsdHandle::TopRight,
        OsdHandle::Right,
        OsdHandle::BottomRight,
        OsdHandle::Bottom,
        OsdHandle::BottomLeft,
        OsdHandle::Left,
    ];

    pub fn index(self) -> u32 {
        match self {
            Self::TopLeft => 0,
            Self::Top => 1,
            Self::TopRight => 2,
            Self::Right => 3,
            Self::BottomRight => 4,
            Self::Bottom => 5,
            Self::BottomLeft => 6,
            Self::Left => 7,
        }
    }

    pub fn is_corner(self) -> bool {
        matches!(
            self,
            Self::TopLeft | Self::TopRight | Self::BottomLeft | Self::BottomRight
        )
    }

    pub fn left(self) -> bool {
        matches!(self, Self::TopLeft | Self::BottomLeft | Self::Left)
    }

    pub fn right(self) -> bool {
        matches!(self, Self::TopRight | Self::BottomRight | Self::Right)
    }

    pub fn top(self) -> bool {
        matches!(self, Self::TopLeft | Self::TopRight | Self::Top)
    }

    pub fn bottom(self) -> bool {
        matches!(self, Self::BottomLeft | Self::BottomRight | Self::Bottom)
    }

    /// 这一侧的手柄会改宽度。
    pub fn moves_x(self) -> bool {
        self.left() || self.right()
    }

    /// 这一侧的手柄会改高度（字号 / 粗细）。
    pub fn moves_y(self) -> bool {
        self.top() || self.bottom()
    }
}

/// 一次「拖手柄改大小」的起点。
///
/// 尺寸和位置都只在按下那一刻取一次，之后按累计位移算 —— 中途重绘不会让数字飘。
#[derive(Clone, Copy, Debug)]
pub struct OsdResize {
    pub part: OsdPart,
    /// 按下时的光标位置（窗口坐标）。
    pub start: gpui::Point<gpui::Pixels>,
    /// 按下时元素的矩形（卡片像素）：左上角 + 宽高。
    pub origin: (f32, f32, f32, f32),
    /// 按下时的尺寸与宽度（配置里的值，用于换算）。
    pub origin_size: u32,
    pub origin_width: u32,
    pub handle: OsdHandle,
    /// 预览缩放：窗口像素 ÷ 这个数 = 卡片像素。
    pub scale: f32,
}
