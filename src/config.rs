//! 用户配置：JSON 持久化，改动即时落盘。

use std::{
    io,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::i18n::Lang;

/// 托盘图标样式：只有「喇叭」和「纯数字」两种，默认喇叭。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum TrayIconStyle {
    /// Win11 系统同款喇叭：本体 + 随音量增减的音量弧线。
    /// `legacy` 是历史值（已移除的旧版图标），旧配置读进来一并归到喇叭。
    #[default]
    #[serde(rename = "arcs", alias = "legacy")]
    Arcs,
    /// 只显示音量数字（百分比），字体与 Win11 托盘一致。
    #[serde(rename = "number")]
    Number,
    /// Bolume 自己的图标（`assets/Bolume.ico` 里的 16px 那一档）。
    #[serde(rename = "app")]
    App,
    /// 动态律动：跟着正在播放的声音实时重绘（效果见 `TrayMotion`）。
    #[serde(rename = "dynamic")]
    Dynamic,
    /// 用户自己选的本地 `.ico`（路径在 `tray_icon_custom`）。
    ///
    /// 读不到文件时退回数字图标 —— 托盘上不能出现一个空白格子。
    #[serde(rename = "custom")]
    Custom,
}

impl TrayIconStyle {
    /// 是不是单色墨迹画的（字母、喇叭、律动图案）。
    ///
    /// 「彩色律动」只染得动这一类；软件图标和自定义 ico 是彩色原图，染色会毁掉原样。
    pub fn is_monochrome(self) -> bool {
        !matches!(self, Self::App | Self::Custom)
    }
}

/// 动态模式画哪一种律动。
///
/// 这些都只吃一个信号 —— 播放设备的峰值电平 —— 所以一律按「节拍 + 包络」来编：
/// 包络跟音量走（快攻慢放），节拍由「电平突然抬头」触发，特效各自把它翻译成
/// 伸缩 / 扩散 / 旋转。没有 FFT 也不影响「看得出在跟着音乐动」。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrayMotion {
    /// 四根律动条：高度跟响度，四根用不同的回落速度，看着像电平表。
    #[default]
    Bars,
    /// 涟漪：中心一个实心点，节拍来了向外扩一圈，边扩边淡。
    Ripple,
    /// 光环：一圈留了缺口的环匀速转，转速和亮度跟着响度走。
    Orbit,
    /// 呼吸：一个圆随包络胀缩，外面套一圈固定细环。
    Pulse,
}

impl TrayMotion {
    /// 设置页分段按钮的下标。
    pub fn index(self) -> u32 {
        match self {
            Self::Bars => 0,
            Self::Ripple => 1,
            Self::Orbit => 2,
            Self::Pulse => 3,
        }
    }

    pub fn from_index(index: u32) -> Self {
        match index {
            1 => Self::Ripple,
            2 => Self::Orbit,
            3 => Self::Pulse,
            _ => Self::Bars,
        }
    }
}

/// 托盘右键菜单长什么样。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum TrayMenuStyle {
    /// Windows 原生菜单：跟系统配色、字号固定、宽度随文字自己长。
    Native,
    /// 自绘菜单：底色 / 文字 / 强调色 / 字号全跟主题，宽度受 `tray_menu_max_width` 限制。
    #[default]
    Theme,
}

/// 音量条上「这个应用正在出声」的动态特效。
///
/// 这些特效都画在音量填充**之上**，所以音量拉满时依然能看出谁在发声。
/// 顺序就是设置页里分段按钮的顺序（`index` / `from_index`）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ActivityEffect {
    /// 关闭：只有音量填充。
    Off,
    /// 经典电平表：电平段两侧压两条亮边（EarTrumpet 的做法）。
    #[default]
    Level,
    /// 流光：一道柔光在电平段里来回扫。
    Shimmer,
    /// 呼吸：电平头上一团会呼吸的辉光。
    Pulse,
    /// 渐变：从透明亮到电平头的尾巴。
    Gradient,
    /// 浮光：流光 + 呼吸同时进行 —— 光带照常来回扫，底光另外自己一张一缩。
    ///
    /// 两条动画各用各的时间轴、各改各的维度（流光只改位置，呼吸只改亮度），
    /// 所以叠在一起也不会互相拖拽，看不出「一抖一抖」。
    Drift,
}

impl ActivityEffect {
    pub fn index(self) -> u32 {
        match self {
            Self::Off => 0,
            Self::Level => 1,
            Self::Shimmer => 2,
            Self::Pulse => 3,
            Self::Gradient => 4,
            Self::Drift => 5,
        }
    }

    pub fn from_index(value: u32) -> Self {
        match value {
            0 => Self::Off,
            2 => Self::Shimmer,
            3 => Self::Pulse,
            4 => Self::Gradient,
            5 => Self::Drift,
            _ => Self::Level,
        }
    }

    /// 会不会动（关掉就不需要为了特效额外补帧）。
    pub fn animates(self) -> bool {
        !matches!(self, Self::Off)
    }
}

/// 设备最大音量限制：拉满也只到 `percent`。
///
/// 可以只管一台设备（`device_id` 里记住那台的 id），也可以所有设备一起管。
/// 钳制点在音频线程里：不管音量是滑条拖的、滚轮滚的、快捷键按的、
/// 还是硬件音量键拧上去的，超过上限都会被按回上限。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct VolumeCap {
    pub enabled: bool,
    /// 上限百分比（1..100）。
    pub percent: u32,
    /// 绑定到的设备 id；空 = 所有设备。
    pub device_id: String,
    /// 绑定设备的显示名（只为了在设置里显示，认不出设备时也不至于只剩一串 id）。
    pub device_name: String,
}

impl Default for VolumeCap {
    fn default() -> Self {
        Self {
            enabled: false,
            percent: 80,
            device_id: String::new(),
            device_name: String::new(),
        }
    }
}

impl VolumeCap {
    /// 这台设备的上限（0..1）；不受限制时返回 `None`。
    pub fn limit_for(&self, device_id: &str) -> Option<f32> {
        if !self.enabled {
            return None;
        }
        if !self.device_id.is_empty() && self.device_id != device_id {
            return None;
        }
        Some(self.percent.clamp(1, 100) as f32 / 100.0)
    }
}

/// 调音量时解除静音。
///
/// 维度是两维的：**方向**（增大 / 减小）× **操作方式**（快捷键 / 面板悬停滚动 / 托盘图标滚动）。
/// 只给一个总开关不够用 —— 有人只想「往上滚就出声、往下滚保持静音」，
/// 也有人只认硬件音量键，滚轮照旧别碰静音，所以每个格子都要能各调各的。
///
/// 主设备音量的四种调法（快捷键 / 悬停滚动 / 拖滑条 / 托盘滚动）各占两格；
/// **单个应用**的音量另有三种调法（悬停滚动 / 拖滑条 / 快捷键调当前应用），
/// 也各自两格 —— 应用滑条常被当成「临时压一下」用，不该跟着主音量一起解锁。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UnmuteOnChange {
    /// 总开关：关掉就完全不解除静音（老行为）。
    pub enabled: bool,
    /// 快捷键音量增大时解除静音。
    pub increase_hotkey: bool,
    /// 快捷键音量减小时解除静音。
    pub decrease_hotkey: bool,
    /// 面板里悬停滚动、音量增大时解除静音。
    pub increase_hover: bool,
    /// 面板里悬停滚动、音量减小时解除静音。
    pub decrease_hover: bool,
    /// 拖动滑条、音量增大时解除静音。
    pub increase_drag: bool,
    /// 拖动滑条、音量减小时解除静音。
    pub decrease_drag: bool,
    /// 托盘图标上滚动、音量增大时解除静音。
    pub increase_tray: bool,
    /// 托盘图标上滚动、音量减小时解除静音。
    pub decrease_tray: bool,
    /// 面板里悬停在**单个应用**音量条上滚动、音量增大时解除静音。
    pub increase_app_hover: bool,
    /// 面板里悬停在**单个应用**音量条上滚动、音量减小时解除静音。
    pub decrease_app_hover: bool,
    /// 面板里拖动**单个应用**的音量条、音量增大时解除静音。
    pub increase_app_drag: bool,
    /// 面板里拖动**单个应用**的音量条、音量减小时解除静音。
    pub decrease_app_drag: bool,
    /// 快捷键调**当前应用**音量、音量增大时解除静音。
    pub increase_app_hotkey: bool,
    /// 快捷键调**当前应用**音量、音量减小时解除静音。
    pub decrease_app_hotkey: bool,
}

impl Default for UnmuteOnChange {
    fn default() -> Self {
        // 总开关默认关、各格子默认开：想用的人打开总开关就是「全场景都解除」，
        // 不想用的人（只想安静调一格）不会被静音自己跳掉。
        Self {
            enabled: false,
            increase_hotkey: true,
            decrease_hotkey: true,
            increase_hover: true,
            decrease_hover: true,
            increase_drag: true,
            decrease_drag: true,
            increase_tray: true,
            decrease_tray: true,
            increase_app_hover: true,
            decrease_app_hover: true,
            increase_app_drag: true,
            decrease_app_drag: true,
            increase_app_hotkey: true,
            decrease_app_hotkey: true,
        }
    }
}

/// 调音量时的音效：每变化一档响一下。
///
/// 思路照 Volume2 —— 调的时候耳朵有个准信儿，不用一直盯着卡片。音源分两类：
/// 代码里合成的内置音效（不占体积、不依赖采样率）和用户自选的本地 WAV。
/// 生效范围按**手势**分开勾：很多人只想要键盘那一下，拖滑条时宁可安静。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct VolumeSoundConfig {
    /// 总开关。
    pub enabled: bool,
    /// 用哪个音源。
    pub style: VolumeSoundStyle,
    /// 自定义音效文件（WAV）；空 = 没选（这时退回「轻点」）。
    pub custom: String,
    /// 音效本身有多响（0..=100，相对满幅）。设备音量照旧由音量决定。
    pub volume: u32,
    /// 快捷键（含硬件音量键、调当前应用的快捷键）也响。
    pub on_hotkey: bool,
    /// 滚轮（面板悬停滚动 / 托盘图标滚动 / 应用音量条滚动）也响。
    pub on_wheel: bool,
    /// 拖滑条（主音量 / 应用音量 / 屏幕提示里那根）也响。
    pub on_drag: bool,
}

impl Default for VolumeSoundConfig {
    fn default() -> Self {
        // 默认关：这是个「想要的人才会开」的功能，新装就让它出声音太主动。
        // 范围默认三种全勾 —— 打开总开关就是「调什么都响」，这才是它的卖点。
        Self {
            enabled: false,
            style: VolumeSoundStyle::Click,
            custom: String::new(),
            volume: 60,
            on_hotkey: true,
            on_wheel: true,
            on_drag: true,
        }
    }
}

impl VolumeSoundConfig {
    /// 这次操作该不该响：总开关 + 手势范围。
    pub fn covers(&self, gesture: VolumeGesture) -> bool {
        if !self.enabled {
            return false;
        }
        match gesture {
            VolumeGesture::Hotkey | VolumeGesture::AppHotkey => self.on_hotkey,
            VolumeGesture::HoverScroll
            | VolumeGesture::TrayScroll
            | VolumeGesture::AppHoverScroll => self.on_wheel,
            VolumeGesture::Drag | VolumeGesture::AppDrag => self.on_drag,
        }
    }

    /// 自定义文件的路径（没选就空串）。
    pub fn custom_path(&self) -> &str {
        self.custom.trim()
    }

    /// 要不要用自定义文件：选了「自定义」并且真的挑了一个文件。
    pub fn uses_custom_file(&self) -> bool {
        self.style == VolumeSoundStyle::Custom && !self.custom_path().is_empty()
    }

    /// 挑过文件，但那个文件现在不在了。
    ///
    /// 这一刻不该拿别的音效顶包 —— 用户选的是自己的那一声，给它换成「轻点」
    /// 只会让人以为「设置没生效」。设置页拿它标红，播放拿它直接静音。
    pub fn custom_missing(&self) -> bool {
        self.uses_custom_file() && std::fs::metadata(self.custom_path()).is_err()
    }

    /// 音源标识（含文件时间戳）：音效缓存拿它当键，换了文件 / 改了文件就会重读。
    pub fn source_key(&self) -> String {
        if self.uses_custom_file() {
            let path = self.custom_path();
            let stamp = std::fs::metadata(path)
                .and_then(|meta| meta.modified())
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|elapsed| elapsed.as_secs())
                .unwrap_or(0);
            format!("file:{path}:{stamp}")
        } else if self.style == VolumeSoundStyle::Custom {
            // 选了「自定义」但还没挑文件：退回默认那一声，总比按了没反应好。
            format!("builtin:{}", VolumeSoundStyle::Click.key())
        } else {
            format!("builtin:{}", self.style.key())
        }
    }
}

/// 内置音效 + 「自定义」。
///
/// 都是代码里合成的一段短音，各有个性：轻点短促、滴答清亮、气泡上滑、
/// 木鱼发闷、棘轮干硬、电子带方波味、铃音收得比早先短（连着响才不拖）。
/// 名字就是听感，不用再解释。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum VolumeSoundStyle {
    /// 轻点：最短促的一下「嗒」。
    #[default]
    Click,
    /// 滴答：高一点的「滴」。
    Tick,
    /// 气泡：「啵」，带点上滑。
    Pop,
    /// 木鱼：闷一点的木质敲击。
    Wood,
    /// 棘轮：一记干硬的金属齿「咔」，尾巴带一点回弹。
    Ratchet,
    /// 电子：方波味的「哔」。
    Blip,
    /// 铃音：拖得长一点的「叮」。
    Chime,
    /// 自定义：播 `custom` 指的那个文件。
    Custom,
}

impl VolumeSoundStyle {
    /// 界面里的顺序（分段选择按这个排）。
    pub const ALL: [Self; 8] = [
        Self::Click,
        Self::Tick,
        Self::Pop,
        Self::Wood,
        Self::Ratchet,
        Self::Blip,
        Self::Chime,
        Self::Custom,
    ];

    /// 配置里认的那个名字（也当音效缓存的键）。
    pub fn key(self) -> &'static str {
        match self {
            Self::Click => "click",
            Self::Tick => "tick",
            Self::Pop => "pop",
            Self::Wood => "wood",
            Self::Ratchet => "ratchet",
            Self::Blip => "blip",
            Self::Chime => "chime",
            Self::Custom => "custom",
        }
    }

    /// 分段选择控件用的序号。
    pub fn index(self) -> u32 {
        Self::ALL.iter().position(|item| *item == self).unwrap_or(0) as u32
    }

    pub fn from_index(index: u32) -> Self {
        Self::ALL.get(index as usize).copied().unwrap_or_default()
    }
}

/// 一次调音量的操作来自哪里。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum VolumeGesture {
    /// 全局快捷键（含硬件音量键）。
    Hotkey,
    /// 面板里悬停在音量条上滚动。
    HoverScroll,
    /// 按住滑条拖动。
    Drag,
    /// 托盘图标上滚动。
    TrayScroll,
    /// 面板里悬停在某个应用自己的音量条上滚动。
    AppHoverScroll,
    /// 面板里拖动某个应用自己的音量条。
    AppDrag,
    /// 快捷键调「当前应用」的音量。
    AppHotkey,
}

impl UnmuteOnChange {
    /// 这次操作要不要顺带解除静音。
    pub fn allows(&self, gesture: VolumeGesture, increase: bool) -> bool {
        if !self.enabled {
            return false;
        }
        match (gesture, increase) {
            (VolumeGesture::Hotkey, true) => self.increase_hotkey,
            (VolumeGesture::Hotkey, false) => self.decrease_hotkey,
            (VolumeGesture::HoverScroll, true) => self.increase_hover,
            (VolumeGesture::HoverScroll, false) => self.decrease_hover,
            (VolumeGesture::Drag, true) => self.increase_drag,
            (VolumeGesture::Drag, false) => self.decrease_drag,
            (VolumeGesture::TrayScroll, true) => self.increase_tray,
            (VolumeGesture::TrayScroll, false) => self.decrease_tray,
            (VolumeGesture::AppHoverScroll, true) => self.increase_app_hover,
            (VolumeGesture::AppHoverScroll, false) => self.decrease_app_hover,
            (VolumeGesture::AppDrag, true) => self.increase_app_drag,
            (VolumeGesture::AppDrag, false) => self.decrease_app_drag,
            (VolumeGesture::AppHotkey, true) => self.increase_app_hotkey,
            (VolumeGesture::AppHotkey, false) => self.decrease_app_hotkey,
        }
    }
}

/// 托盘右键菜单里的一个条目。
///
/// 只有一层：根菜单上的动作 / 文件夹，以及文件夹里的动作。
/// `id` 就是菜单事件里的那个 id（`device:` 前缀的是动态设备项，不在这里）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TrayMenuNode {
    /// 动作 id（`toggle` / `settings` / `quit` / `devices` / 那几个 legacy 入口），
    /// 或者文件夹 id（`folder:1` 这种）。
    pub id: String,
    #[serde(default = "default_true")]
    pub visible: bool,
    /// 文件夹标题；只有文件夹用。
    #[serde(default)]
    pub title: String,
    /// 文件夹里的条目；只有文件夹用，且子项不能再是文件夹。
    #[serde(default)]
    pub children: Vec<TrayMenuNode>,
}

fn default_true() -> bool {
    true
}

/// 「按住连续触发」的默认勾选集合（音量四个 + 亮度六个）。
fn default_repeat_actions() -> Vec<String> {
    DEFAULT_REPEAT_ACTIONS
        .iter()
        .map(|name| (*name).to_string())
        .collect()
}

impl TrayMenuNode {
    pub fn action(id: &str) -> Self {
        Self {
            id: id.to_string(),
            visible: true,
            title: String::new(),
            children: Vec::new(),
        }
    }

    pub fn folder(id: &str, title: &str, children: Vec<TrayMenuNode>) -> Self {
        Self {
            id: id.to_string(),
            visible: true,
            title: title.to_string(),
            children,
        }
    }

    /// 默认不出现在右键里的动作（「重启」这种偶发操作）：
    /// 先在设置页的托盘菜单编辑器里把眼睛点开，才会真的进菜单。
    pub fn hidden_action(id: &str) -> Self {
        Self {
            visible: false,
            ..Self::action(id)
        }
    }

    /// 一条分隔线：只在菜单里画一条线，没有点击行为。
    ///
    /// 以前分隔线是渲染时自动补的（设备块之后、`quit` 之前），现在完全由用户自己插 ——
    /// 想隔哪就隔哪，不想隔就一条都没有。
    pub fn separator() -> Self {
        Self {
            id: SEPARATOR_ID.to_string(),
            visible: true,
            title: String::new(),
            children: Vec::new(),
        }
    }

    pub fn is_folder(&self) -> bool {
        self.id.starts_with("folder:")
    }

    pub fn is_separator(&self) -> bool {
        self.id == SEPARATOR_ID
    }
}

/// 分隔线节点的 id。可以出现任意多条（渲染时不看 id、只看顺序）。
pub const SEPARATOR_ID: &str = "separator";

/// 托盘右键菜单的布局：顺序、显隐、子菜单都由用户自己排。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TrayMenuConfig {
    pub root: Vec<TrayMenuNode>,
}

impl Default for TrayMenuConfig {
    fn default() -> Self {
        Self {
            // 默认全部平铺，按「设备 → 面板 → 系统入口 → 设备设置 → 收尾」分四段，
            // 段与段之间是分隔线。分隔线从 rev 54 起是**配置里的条目**（`separator`），
            // 渲染时不再自动补 —— 所以默认这一份里写了几条就是几条。
            root: vec![
                TrayMenuNode::action("devices"),
                TrayMenuNode::separator(),
                TrayMenuNode::action("toggle"),
                TrayMenuNode::separator(),
                TrayMenuNode::action("legacy-mixer"),
                TrayMenuNode::action("apps-volume"),
                // 新版入口在前、旧版垫后（跟上面「音量合成器」那一对保持一致）。
                TrayMenuNode::action("legacy-sounds"),
                TrayMenuNode::action("sound-settings"),
                TrayMenuNode::separator(),
                TrayMenuNode::action("legacy-playback"),
                TrayMenuNode::action("legacy-recording"),
                TrayMenuNode::separator(),
                // 重启摆在设置 / 退出前面（复查要重启一次很常见，默认就露出来）。
                TrayMenuNode::action("restart"),
                TrayMenuNode::action("settings"),
                TrayMenuNode::action("quit"),
            ],
        }
    }
}

impl TrayMenuConfig {
    /// 这个 id 在菜单树里（含子菜单）有没有出现过。
    pub fn contains(&self, id: &str) -> bool {
        fn walk(nodes: &[TrayMenuNode], id: &str) -> bool {
            nodes
                .iter()
                .any(|node| node.id == id || walk(&node.children, id))
        }
        walk(&self.root, id)
    }

    /// 老配置补一个新动作：默认隐藏，插在「退出」前面（没有退出就放末尾）。
    ///
    /// 已经存在就一个字都不动 —— 用户自己排过位置、或者已经手动藏起来的，不能被覆盖。
    pub fn ensure_action(&mut self, id: &str) {
        if self.contains(id) {
            return;
        }
        let index = self
            .root
            .iter()
            .position(|node| node.id == "quit")
            .unwrap_or(self.root.len());
        self.root.insert(index, TrayMenuNode::hidden_action(id));
    }

    /// 某个动作在菜单里**每一处**的显隐一起改（根菜单、子菜单都算）。
    ///
    /// 迁移用：默认值变了以后要把老配置里那一项也打开，否则用户升级完看到的还是旧样子。
    pub fn set_action_visible(&mut self, id: &str, visible: bool) {
        fn walk(nodes: &mut [TrayMenuNode], id: &str, visible: bool) {
            for node in nodes {
                if node.id == id {
                    node.visible = visible;
                }
                walk(&mut node.children, id, visible);
            }
        }
        walk(&mut self.root, id, visible);
    }

    /// 某个条目在它所在容器里的可变引用（`folder = None` 表示右键根菜单）。
    fn node_mut(&mut self, folder: Option<usize>, index: usize) -> Option<&mut TrayMenuNode> {
        match folder {
            None => self.root.get_mut(index),
            Some(folder) => self.root.get_mut(folder)?.children.get_mut(index),
        }
    }

    /// 改显隐。
    pub fn set_visible(&mut self, folder: Option<usize>, index: usize, visible: bool) {
        if let Some(node) = self.node_mut(folder, index) {
            node.visible = visible;
        }
    }

    /// 在同一个容器里上移 / 下移一格。
    pub fn shift(&mut self, folder: Option<usize>, index: usize, up: bool) {
        let list = match folder {
            None => &mut self.root,
            Some(folder) => {
                let Some(node) = self.root.get_mut(folder) else {
                    return;
                };
                &mut node.children
            }
        };
        if up && index > 0 {
            list.swap(index, index - 1);
        } else if !up && index + 1 < list.len() {
            list.swap(index, index + 1);
        }
    }

    /// 把子菜单里的条目挪回右键（追加到末尾）。
    pub fn move_child_to_root(&mut self, folder: usize, index: usize) {
        let Some(parent) = self.root.get_mut(folder) else {
            return;
        };
        if index < parent.children.len() {
            let child = parent.children.remove(index);
            self.root.push(child);
        }
    }

    /// 把右键上的条目收进最后那个子菜单（没有子菜单就什么都不做）。
    pub fn move_root_into_last_folder(&mut self, index: usize) {
        let Some(folder) = self.root.iter().rposition(|node| node.is_folder()) else {
            return;
        };
        if index < self.root.len() && index != folder {
            let node = self.root.remove(index);
            // 删掉一个条目后，folder 的下标可能要往前挪一位。
            let folder = if index < folder { folder - 1 } else { folder };
            if let Some(target) = self.root.get_mut(folder) {
                target.children.push(node);
            }
        }
    }

    /// 拖拽落点：把 `from` 处的条目搬到 `to_folder`（`None` = 右键根菜单）的第 `to_index` 个位置。
    ///
    /// `to_index` 按**搬之前**的列表算，和界面上看到的行号一致：
    /// 放到某一行上面就是那一行的下标，放到最底下就是 `len`。
    /// 同一容器内往下搬时，这里会把「自己先被抽走」造成的位移补回来。
    pub fn move_node(
        &mut self,
        from: (Option<usize>, usize),
        to_folder: Option<usize>,
        to_index: usize,
    ) {
        // 先把条目从原位置取出来。
        let node = match from.0 {
            None => {
                if from.1 >= self.root.len() {
                    return;
                }
                self.root.remove(from.1)
            }
            Some(folder) => {
                let Some(parent) = self.root.get_mut(folder) else {
                    return;
                };
                if from.1 >= parent.children.len() {
                    return;
                }
                parent.children.remove(from.1)
            }
        };

        let mut to_folder = to_folder;
        // 从根菜单里删掉一项后，它后面那些子菜单的下标要往前挪一位。
        if let (None, Some(folder)) = (from.0, to_folder) {
            if from.1 < folder {
                to_folder = Some(folder - 1);
            }
        }
        // 同一个容器里往下搬：目标下标要减掉被自己挤掉的那一格。
        let mut to_index = to_index;
        if from.0 == to_folder && from.1 < to_index {
            to_index -= 1;
        }

        match to_folder {
            // 子菜单只有一层：文件夹不能塞进文件夹，遇到这种落点就当放回根菜单。
            Some(folder) if !node.is_folder() => {
                let valid = self
                    .root
                    .get(folder)
                    .is_some_and(|parent| parent.is_folder());
                if valid {
                    let parent = self.root.get_mut(folder).expect("已确认是子菜单");
                    let index = to_index.min(parent.children.len());
                    parent.children.insert(index, node);
                } else {
                    let index = to_index.min(self.root.len());
                    self.root.insert(index, node);
                }
            }
            _ => {
                let index = to_index.min(self.root.len());
                self.root.insert(index, node);
            }
        }
    }

    /// 删掉子菜单：里面的条目回到右键末尾，不丢。
    pub fn delete_folder(&mut self, index: usize) {
        if index < self.root.len() && self.root[index].is_folder() {
            let folder = self.root.remove(index);
            self.root.extend(folder.children);
        }
    }

    /// 新建子菜单时用的 id（`folder:N` 里取最大 N 再加一）。
    pub fn next_folder_id(&self) -> String {
        let max = self
            .root
            .iter()
            .filter(|node| node.is_folder())
            .filter_map(|node| node.id.strip_prefix("folder:"))
            .filter_map(|n| n.parse::<u32>().ok())
            .max()
            .unwrap_or(0);
        format!("folder:{}", max + 1)
    }

    /// 在下标 `index` 前插一条分隔线（`folder = None` = 右键根菜单）。
    ///
    /// 分隔线可以连着插两条，也可以插在最前面 / 最后面 —— 不做任何「智能」合并，
    /// 用户插了什么就是什么（真嫌多，点行尾的 × 删掉就行）。
    pub fn insert_separator(&mut self, folder: Option<usize>, index: usize) {
        match folder {
            None => {
                let index = index.min(self.root.len());
                self.root.insert(index, TrayMenuNode::separator());
            }
            Some(folder) => {
                let Some(parent) = self.root.get_mut(folder) else {
                    return;
                };
                let index = index.min(parent.children.len());
                parent.children.insert(index, TrayMenuNode::separator());
            }
        }
    }

    /// 删掉一个条目（子菜单会先把内容还给上层）。分隔线走的就是这条。
    pub fn remove_node(&mut self, folder: Option<usize>, index: usize) {
        match folder {
            None => {
                if index >= self.root.len() {
                    return;
                }
                if self.root[index].is_folder() {
                    self.delete_folder(index);
                } else {
                    self.root.remove(index);
                }
            }
            Some(folder) => {
                let Some(parent) = self.root.get_mut(folder) else {
                    return;
                };
                if index < parent.children.len() {
                    parent.children.remove(index);
                }
            }
        }
    }

    /// rev 54 之前的默认布局（设备 + 八个入口 + 重启 + 退出，一条分隔线都没有）。
    const LEGACY_DEFAULT_IDS: [&'static str; 11] = [
        "devices",
        "toggle",
        "settings",
        "legacy-mixer",
        "legacy-playback",
        "legacy-recording",
        "legacy-sounds",
        "sound-settings",
        "apps-volume",
        "restart",
        "quit",
    ];

    /// 这份布局是不是「原样没动过的老默认」。
    ///
    /// 迁移只认这一种情况：用户自己排过顺序 / 建过子菜单 / 删过条目的，一个都不碰。
    fn is_legacy_default(&self) -> bool {
        self.root.len() == Self::LEGACY_DEFAULT_IDS.len()
            && self
                .root
                .iter()
                .zip(Self::LEGACY_DEFAULT_IDS)
                .all(|(node, id)| node.id == id)
    }

    /// 把「原样没动过的老默认」或「顺序已经和新默认一致、只是缺分隔线」的布局换成新默认。
    ///
    /// 两种情况才动：
    /// 1. 就是 rev 53 的那份老默认（顺序完全一样，一条线都没有）；
    /// 2. 顺序已经是新默认的顺序，只是没有分隔线（升级前用户手动排成了这个样子）。
    ///
    /// 每一项的显隐按 id 带过去：老配置里被眼睛关掉的，新布局里依然是关着的。
    /// 用户排过别的顺序 / 建过子菜单的，一个字都不动 —— 想加线自己插。
    pub fn rebase_legacy_default(&mut self) {
        let target = TrayMenuConfig::default();
        if self
            .root
            .iter()
            .any(|node| node.is_folder() || node.is_separator())
        {
            return;
        }
        let strip = |nodes: &[TrayMenuNode]| -> Vec<String> {
            nodes
                .iter()
                .filter(|node| !node.is_separator())
                .map(|node| node.id.clone())
                .collect()
        };
        let same_order = strip(&self.root) == strip(&target.root);
        if !same_order && !self.is_legacy_default() {
            return;
        }
        let hidden: Vec<String> = self
            .root
            .iter()
            .filter(|node| !node.visible)
            .map(|node| node.id.clone())
            .collect();
        self.root = target
            .root
            .into_iter()
            .map(|mut node| {
                if !node.is_separator() && hidden.contains(&node.id) {
                    node.visible = false;
                }
                node
            })
            .collect();
    }

    /// rev 55 的默认顺序（`sound-settings` 在 `legacy-sounds` 前面）。
    ///
    /// rev 56 把这两条对调了，这里留一份老顺序用来认「没动过的默认」。
    const REV55_ORDER: [&'static str; 11] = [
        "devices",
        "toggle",
        "legacy-mixer",
        "apps-volume",
        "sound-settings",
        "legacy-sounds",
        "legacy-playback",
        "legacy-recording",
        "restart",
        "settings",
        "quit",
    ];

    /// rev 56：把「声音设置」和「声音设置 [旧]」对调位置。
    ///
    /// 只认 rev 55 那份默认顺序（分隔线不参与比较，用户插几条都不影响判断）——
    /// 自己排过别的顺序的，一个字不动。整条节点一起换，显隐跟着条目走。
    pub fn swap_sound_entries(&mut self) -> bool {
        let strip: Vec<&str> = self
            .root
            .iter()
            .filter(|node| !node.is_separator())
            .map(|node| node.id.as_str())
            .collect();
        if strip != Self::REV55_ORDER {
            return false;
        }
        let (Some(a), Some(b)) = (
            self.root
                .iter()
                .position(|node| node.id == "sound-settings"),
            self.root.iter().position(|node| node.id == "legacy-sounds"),
        ) else {
            return false;
        };
        self.root.swap(a, b);
        true
    }
}

// ---------------------------------------------------------------------------
// 面板页脚那一排按钮：放哪些、摆哪一边、谁在前谁在后
// ---------------------------------------------------------------------------

/// 页脚按钮靠哪一边。
///
/// 页脚本来就是两头对齐的（`justify_between`）：一边是系统状态（WiFi / 蓝牙），
/// 一边是面板自己的动作（响度均衡 / 音量合成器 / 设置 / 置顶）。所以「位置」有两层意思 ——
/// 摆在哪一头，以及那一头里的第几个。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum FooterSide {
    #[default]
    Left,
    Right,
}

impl FooterSide {
    pub fn is_left(self) -> bool {
        matches!(self, FooterSide::Left)
    }
}

/// 页脚上能放的一个按钮（注册表条目）。
///
/// **加一个新按钮只要两步**：这里加一条，`windows::flyout` 的 `footer_button`
/// 里加一个 `match` 分支。设置页那张列表、显隐、排序、以及老配置的补全全是按这张
/// 表生成的，不用再改别处。
pub struct FooterItemSpec {
    /// 配置里存的 id。改了就等于换了一个按钮（老配置里那条会变成不认识的条目）。
    pub id: &'static str,
    pub zh: &'static str,
    pub en: &'static str,
    /// 面板上的图标（`assets/icons` 里的名字）。
    pub icon: &'static str,
    /// 横排时带文字（响度均衡 / 音量合成器这种）。竖排列太窄，一律只画图标。
    pub label: bool,
    /// 第一次进配置时摆在哪一边、默认显不显示。
    pub side: FooterSide,
}

/// 页脚按钮注册表。**顺序就是设置页列表里的顺序**，也是新按钮插进老配置时的锚点。
pub const FOOTER_ITEMS: &[FooterItemSpec] = &[
    FooterItemSpec {
        id: "wifi",
        zh: "WiFi",
        en: "Wi-Fi",
        icon: "wifi",
        label: false,
        side: FooterSide::Left,
    },
    FooterItemSpec {
        id: "bluetooth",
        zh: "蓝牙",
        en: "Bluetooth",
        icon: "bluetooth",
        label: false,
        side: FooterSide::Left,
    },
    FooterItemSpec {
        id: "loudness",
        zh: "响度均衡",
        en: "Loudness",
        icon: "levels",
        label: true,
        side: FooterSide::Right,
    },
    FooterItemSpec {
        id: "mixer",
        zh: "音量合成器",
        en: "Volume mixer",
        icon: "adjustments",
        label: true,
        side: FooterSide::Right,
    },
    FooterItemSpec {
        id: "settings",
        zh: "设置",
        en: "Settings",
        icon: "settings",
        label: false,
        side: FooterSide::Right,
    },
    FooterItemSpec {
        id: "pin",
        zh: "置顶",
        en: "Pin",
        icon: "pin",
        label: false,
        side: FooterSide::Right,
    },
];

/// 按 id 找注册表条目。认不出来（用户手改过 / 从新版降级）就当没这个按钮。
pub fn footer_spec(id: &str) -> Option<&'static FooterItemSpec> {
    FOOTER_ITEMS.iter().find(|spec| spec.id == id)
}

/// 页脚上的一个按钮：哪个、显不显示、靠哪一边。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FooterItem {
    pub id: String,
    #[serde(default = "default_true")]
    pub visible: bool,
    #[serde(default)]
    pub side: FooterSide,
}

impl FooterItem {
    fn from_spec(spec: &FooterItemSpec) -> Self {
        Self {
            id: spec.id.to_string(),
            visible: true,
            side: spec.side,
        }
    }
}

/// 面板页脚的布局。`items` 的顺序就是**同一侧里的显示顺序**（两侧混排在一份列表里，
/// 渲染时按 `side` 分两拨、各自保序）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct FooterConfig {
    pub items: Vec<FooterItem>,
}

impl Default for FooterConfig {
    fn default() -> Self {
        Self {
            items: FOOTER_ITEMS.iter().map(FooterItem::from_spec).collect(),
        }
    }
}

impl FooterConfig {
    pub fn index_of(&self, id: &str) -> Option<usize> {
        self.items.iter().position(|item| item.id == id)
    }

    pub fn is_visible(&self, id: &str) -> bool {
        self.items
            .iter()
            .find(|item| item.id == id)
            .is_some_and(|item| item.visible)
    }

    pub fn set_visible(&mut self, id: &str, visible: bool) {
        if let Some(item) = self.items.iter_mut().find(|item| item.id == id) {
            item.visible = visible;
        }
    }

    /// 把 `id` 挪到 `side` 那一组里、插在 `before` 前面（`None` = 放进这组的末尾）。
    ///
    /// 设置页那个可视化面板拖一下就调这个：屏幕上的「插在第几个」先换算成
    /// 「插在谁的签名儿上」，跨组搬家和组内换序都是同一条路。
    ///
    /// `show` 为真时顺手取消隐藏 —— 从「未放置」把按钮放回面板走的就是它。
    ///
    /// 返回真的动了没有：把按钮拖回原处不算改动，省一次落盘。
    pub fn place(
        &mut self,
        id: &str,
        side: FooterSide,
        before: Option<&str>,
        show: bool,
    ) -> bool {
        let Some(from) = self.index_of(id) else {
            return false;
        };
        // 落在自己身上 = 没动。不先挡掉的话「锚点被移除」会让它顺势滑到组尾。
        if before == Some(id) {
            return false;
        }
        // 先在副本上做完再比：拖回原位（含「插到紧挨着自己后面」）就是一次空操作。
        let mut next = self.clone();
        let mut item = next.items.remove(from);
        item.side = side;
        item.visible = item.visible || show;
        // 锚点在移除之后整体挪了一位，按 id 重新找。
        let at = before
            .and_then(|anchor| next.items.iter().position(|other| other.id == anchor))
            .unwrap_or(next.items.len());
        next.items.insert(at, item);
        if next == *self {
            return false;
        }
        *self = next;
        true
    }

    /// 老配置补新按钮：注册表里有、配置里没有的，按注册表顺序插进去，默认显示。
    ///
    /// 每次载入都跑（幂等）。这样以后加按钮，老用户升级完就能看见，不用自己找；
    /// 认不出来的旧 id 留着不删 —— 渲染时跳过，用户万一降级回去也还在。
    pub fn ensure_all(&mut self) -> bool {
        let mut changed = false;
        for (order, spec) in FOOTER_ITEMS.iter().enumerate() {
            if self.items.iter().any(|item| item.id == spec.id) {
                continue;
            }
            // 插到「注册表里排在它后面的第一项」前面，保持注册表的相对顺序；
            // 没有这样的项就放末尾。
            let at = self
                .items
                .iter()
                .position(|item| {
                    FOOTER_ITEMS
                        .iter()
                        .position(|known| known.id == item.id)
                        .is_some_and(|known_order| known_order > order)
                })
                .unwrap_or(self.items.len());
            self.items.insert(at, FooterItem::from_spec(spec));
            changed = true;
        }
        changed
    }
}

/// 托盘提示的对齐方式。
///
/// 系统的通知区域提示本身没有对齐参数（文字固定靠左），「居中」「靠右」都是靠
/// 逐行补前导空格做出来的近似效果，详见 `tray::align_rows`。
///
/// `Verbatim`（自定义）不补也不裁：编辑框里怎么排、有几个空格，托盘上就照原样。
/// 单行想脱离全局设置时，在该行任意位置写 `{left}` / `{center}` / `{right}`，
/// 见 `tray::take_align_marker`。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum TooltipAlign {
    #[default]
    #[serde(rename = "left")]
    Left,
    #[serde(rename = "center")]
    Center,
    #[serde(rename = "right")]
    Right,
    /// 自定义：完全遵循编辑框内容（含空格），不补也不裁。
    #[serde(rename = "verbatim")]
    Verbatim,
}

impl TooltipAlign {
    pub fn is_center(self) -> bool {
        matches!(self, Self::Center)
    }

    pub fn is_verbatim(self) -> bool {
        matches!(self, Self::Verbatim)
    }

    /// 对齐选项在界面上的位置（靠左 / 居中 / 靠右 / 自定义）。
    pub fn index(self) -> u32 {
        match self {
            Self::Left => 0,
            Self::Center => 1,
            Self::Right => 2,
            Self::Verbatim => 3,
        }
    }

    pub fn from_index(value: u32) -> Self {
        match value {
            1 => Self::Center,
            2 => Self::Right,
            3 => Self::Verbatim,
            _ => Self::Left,
        }
    }
}

/// 托盘悬浮提示（tooltip）的自定义内容。
///
/// 每行一个模板，`{变量}` 在运行时替换成实时值（见 `tray` 里的变量表），
/// 空行不显示。为了照顾「静音是一种单独的状态」，静音时可以另用一套内容：
/// `muted_enabled` 关着（默认）就沿用正常内容。
///
/// 模板里可以写字面文字和变量混排，比如 `音量 {volume} · {device}`。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct TooltipConfig {
    /// 自定义开关：关着就用内置的默认内容（第一行读数、第二行设备名）。
    pub enabled: bool,
    /// 正常状态下的内容，一行一个模板。
    pub lines: Vec<String>,
    /// 静音时换个内容（关着则沿用 `lines`）。
    pub muted_enabled: bool,
    pub muted_lines: Vec<String>,
    /// 对齐：靠左 / 居中 / 靠右 / 自定义（完全照编辑框）。
    ///
    /// 单行可以在模板里写 `{left}` / `{center}` / `{right}` 覆盖它。
    pub align: TooltipAlign,
}

impl Default for TooltipConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            // 空模板 = 「没写」，渲染时回落到按当前语言生成的内置内容
            // （见 `tray::default_template`、`tooltip_lines_for`）。
            lines: Vec::new(),
            muted_enabled: false,
            muted_lines: Vec::new(),
            align: TooltipAlign::Left,
        }
    }
}

impl TooltipConfig {
    /// 当前状态下真正要渲染的模板行。
    ///
    /// 静音且单独配了内容才用静音那套；否则一律用正常那套 —— 关掉开关
    /// 又保留着旧内容，不应该在静音时突然换成一份空提示。
    pub fn active_lines(&self, muted: bool) -> &[String] {
        if muted && self.muted_enabled && !self.muted_lines.is_empty() {
            &self.muted_lines
        } else {
            &self.lines
        }
    }
}

/// 启动时要恢复的一台设备的开关状态。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct StartupDeviceState {
    pub id: String,
    pub name: String,
    pub enabled: bool,
}

/// 启动时要恢复的一个应用的音量。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(default)]
pub struct StartupAppVolume {
    /// 会话分组键（`exe:<路径>`），和面板里「折叠此应用」用的是同一套 key。
    pub key: String,
    pub name: String,
    pub percent: u32,
}

/// 启动时怎么管设备开关：四选一，同时只生效一种规则。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DeviceStateMode {
    /// 禁用：这套功能整个关掉，启动时一台设备都不碰（默认）。
    #[default]
    Disabled,
    /// 手动模式：名单里每台设备自己选「启用 / 禁用」，名单外的不动。
    Manual,
    /// 黑名单模式：**名单里的设备是黑的** —— 启动时禁用它们，其它设备保持系统原样。
    Blacklist,
    /// 白名单模式：**名单里的设备是白的** —— 只启用它们，其它一律禁用；
    /// 以后新插进来的设备不在名单里，自然也被禁用。
    Whitelist,
}

impl DeviceStateMode {
    pub fn index(self) -> u32 {
        match self {
            Self::Disabled => 0,
            Self::Manual => 1,
            Self::Blacklist => 2,
            Self::Whitelist => 3,
        }
    }

    pub fn from_index(value: u32) -> Self {
        match value {
            1 => Self::Manual,
            2 => Self::Blacklist,
            3 => Self::Whitelist,
            _ => Self::Disabled,
        }
    }

    /// 要不要显示名单（禁用模式整块不显示）。
    pub fn uses_list(self) -> bool {
        !matches!(self, Self::Disabled)
    }

    /// 名单里每台设备要不要单独选「启用 / 禁用」。
    ///
    /// 只有手动模式需要：黑名单 / 白名单的**成员资格本身就是结论**，
    /// 再摆一排「启用 / 禁用」按钮只会让人以为选它有用。
    pub fn per_device_flags(self) -> bool {
        matches!(self, Self::Manual)
    }
}

/// 启动时自动套用的状态（全部默认关闭，开了才动）。
///
/// 只在**启动那一次**生效：之后用户在面板里怎么调都不会被拉回去。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct StartupProfile {
    /// 启动时把默认播放设备切过去。
    pub set_default_device: bool,
    pub default_device_id: String,
    pub default_device_name: String,

    /// 启动时的设备开关规则（禁用 / 手动 / 黑名单 / 白名单）。
    pub device_state_mode: DeviceStateMode,
    /// 手动模式的名单：每台记自己的启用 / 禁用。
    pub device_states: Vec<StartupDeviceState>,
    /// 黑名单：在名单里 = 启动时禁用。
    pub device_blacklist: Vec<StartupDeviceState>,
    /// 白名单：在名单里 = 启动时保留，其余禁用。
    pub device_whitelist: Vec<StartupDeviceState>,

    /// 启动时设置主音量。
    pub set_master_volume: bool,
    /// 主音量百分比（0..100）。
    pub master_volume: u32,

    /// 启动时设置各应用音量。
    pub set_app_volumes: bool,
    pub app_volumes: Vec<StartupAppVolume>,
}

impl Default for StartupProfile {
    fn default() -> Self {
        Self {
            set_default_device: false,
            default_device_id: String::new(),
            default_device_name: String::new(),
            device_state_mode: DeviceStateMode::Disabled,
            device_states: Vec::new(),
            device_blacklist: Vec::new(),
            device_whitelist: Vec::new(),
            set_master_volume: false,
            // 开关默认关着，这个值只是让人打开开关时不至于一上手就是静音。
            master_volume: 50,
            set_app_volumes: false,
            app_volumes: Vec::new(),
        }
    }
}

impl StartupProfile {
    /// 当前规则对应的那份名单。三种模式的名单**各存各的**：
    /// 白名单里加过的设备不该在黑名单里冒出来。
    pub fn device_list(&self, mode: DeviceStateMode) -> &[StartupDeviceState] {
        match mode {
            DeviceStateMode::Manual => &self.device_states,
            DeviceStateMode::Blacklist => &self.device_blacklist,
            DeviceStateMode::Whitelist => &self.device_whitelist,
            DeviceStateMode::Disabled => &[],
        }
    }

    /// 同上，可改的那一份。
    pub fn device_list_mut(&mut self, mode: DeviceStateMode) -> &mut Vec<StartupDeviceState> {
        match mode {
            DeviceStateMode::Blacklist => &mut self.device_blacklist,
            DeviceStateMode::Whitelist => &mut self.device_whitelist,
            // 禁用模式没有名单；写成手动，避免调用方拿到一个立刻被丢掉的临时值。
            _ => &mut self.device_states,
        }
    }

    /// 有没有任何一项要套用。
    #[allow(dead_code)]
    pub fn is_active(&self) -> bool {
        self.set_default_device
            || self.device_state_mode.uses_list()
            || self.set_master_volume
            || self.set_app_volumes
    }
}

/// 配置格式版本：默认值变过就 +1，老配置读进来时按新默认补齐。
///
/// rev 28：亮度滚轮从「段数」改成「步进百分比」（换算在 `Config::load` 里做）。
/// rev 29：新增应用黑名单；OSD 的「全屏时不弹」与快捷键的「全屏时禁用」合并成
/// 黑名单里的「全屏状态禁用」（老配置任一为真就打开）。
/// rev 33：界面字体多了字重、界面外观多了对比度；两个字段都有 serde 默认值，不用搬数据。
/// rev 34：面板多了「正在发声的应用排最前」开关（rev 52 起默认关）。
/// rev 35：快捷键多了「按住连发」与间隔、亮度调节三组快捷键；都有 serde 默认值。
/// rev 36：亮度快捷键可以单独设步进（`brightness.hotkey_step`，0 = 跟随面板步进）。
/// rev 37：界面多了「动效」开关（`ui_motion`，默认开）—— 设置页切换时的淡入。
/// rev 38：文本元素改成按需新建、条数不限（`OsdPart::Text(n)`）；默认不再带三条空文字。
/// rev 39：多了元素图层顺序 `z_order`（空 = 内置顺序），有 serde 默认值，不用搬数据。
/// rev 40：OSD 卡片投影（`osd.shadow`，默认关）+ 应用滑条拖动解锁静音的两个开关。
/// rev 45：`hotkey_numpad_raw` —— NumLock 关着也按小键盘数字匹配快捷键。
/// rev 46：主题可以「跟随系统亮暗」（`theme_follow_system` + 亮 / 暗两个槽位），
/// 默认关闭、两槽各给一档默认主题，老配置读进来行为不变。
/// rev 47：跟随系统的亮 / 暗两档各有自己的自选底色与强调色（四个空串默认）。
/// rev 48：音量条的轨道底色与静音填充色也能单独设（`colors.bar_track` / `colors.bar_muted`，
/// 空串 = 用主题原来的那两个，老配置读进来行为不变）。
/// rev 49：音量条的圆角可调（`bar_round`，存半高的百分比，默认 100 = 全圆头）。
/// rev 50：布局编辑器多了「画布底色」（`editor_bg`）与可拖的右栏宽度（`editor_panel_w`）——
/// 都只是编辑器自己的外观，不影响提示条；音量条自己那条元素色搬进 `colors.accent`。
/// rev 51：黑名单多了「全屏例外」名单（`app_block.fullscreen_exempt`，空 = 老行为），
/// 喇叭图标样式改成「弧线1 / 弧线2 / 竖条 / 圆点 / 音箱」且音箱也有静音态。
/// rev 52：托盘右键的「重启」改成默认显示（rev 30 加它时是默认藏着的），老配置一并打开。
/// rev 53：面板字号从「百分比缩放」改成「字体字号（`panel_font_size`，逻辑像素）」，
/// 并新增字间距 / 行高（老配置的百分比按 100% = 11.5px 换算过来）。
/// rev 54：托盘右键的分隔线改成配置里的条目（`separator`），渲染时不再自动补两条；
/// 默认顺序重排成「设备 / 面板 / 系统入口 / 设备设置 / 收尾」五段。
/// rev 55：播放设备可以自定义图标（`device_icons`，按设备 id）；新字段默认空 = 跟随设备类型。
/// rev 56：默认菜单里「声音设置 [旧]」挪到「声音设置」前面，跟「音量合成器 [旧] → 音量合成器」一致。
/// rev 57：应用名多了一个「首字母大写」开关（`app_name_capitalize`，默认开）。
/// rev 58：「松开滑条时播放提示音」改成默认开；「调音量时解除静音」的总开关改成默认关。
/// rev 59：外观模式默认改成「跟随系统」；强调色空着 = 跟随 Windows 的强调色。
/// rev 60：「松开滑条时播放提示音」多一个子开关 —— 屏幕提示（OSD）里也响（`release_sound_osd`，默认开）。
/// rev 61：「按住连续触发」多一个子开关 —— 不理会系统的长按重复（`hotkey_ignore_system_repeat`，默认开）。
/// rev 62：面板里的音量条粗细（`panel_bar_thickness`）与圆角（`panel_bar_round`）可调，
/// 默认 6px / 100%（全圆头），老配置读进来画出来和以前一模一样。
/// rev 63：弹出动画可以选「跟随系统」（`popup_anim_style` / `osd.anim_style`），
/// 默认仍是各自原来那套，老配置读进来行为不变。
/// rev 64：动画方式各多一档（面板加「淡入淡出」、屏幕提示加「向上展开」），
/// 面板收起也加了动画（`panel_close_animation`，默认开）。
/// rev 65：默认弹出时长 400 → 200ms（淡入淡出那档太拖），动画支持打断。
/// rev 66：删掉面板的「从任务栏展开」和屏幕提示的「向上展开」两档动画
/// （老配置靠 serde alias 落到 system），屏幕提示的显示器改成「手动选择 + 下拉」。
/// rev 67：托盘数字重做 —— 去掉「字重 / 数字大小」，改成照 PS 字符面板那套
/// （`tray_number_antialias` + 三位数的 `tray_digit_one/two/three`：
/// 字号 / 宽度 / 高度 / 字距）。老配置的「数字大小」百分比换算成一个字号搬过去。
/// rev 68：应用可以自定义名字（`app_names`，面板右键「重命名」）。
/// 纯新增字段，默认空表 = 全部显示进程名，不需要迁移老配置。
/// rev 69：响度均衡（`loudness` + `app_loudness`）。默认关闭，纯新增字段，
/// 不需要迁移：老配置读进来就是「没开」，音量行为跟以前一模一样。
/// rev 70：响度均衡加「最大增益」（`loudness.max_gain`，默认 1.0 = 不提升）。
/// 超过 1.0 的部分要接管该应用的音频流才输出得出来，默认不开，
/// 所以老配置的行为同样不变。
/// rev 71：响度均衡换成 TomSteady 的算法，参数从五个（目标电平 / 上限 /
/// 压缩比 / 平均窗口 / 最大增益）收成两个（`loudness.level` 最大放大级别、
/// `loudness.smoothness` 音量变化级别）。老配置里那五个字段反序列化时被忽略，
/// 落到新默认值 —— 默认仍然是「关」，所以老配置读进来音量行为照旧。
/// rev 72：主题去掉黑 / 灰 / 白三套预设（`theme` / `theme_light` / `theme_dark` 三个
/// 字段一起删），打底只剩深色 / 浅色两套模板，外观全由底色 + 强调色决定。
/// 亮暗两档的底色给了默认值（浅色档倒数第一个、深色档第四个），老配置里空着
/// 的那两格一并填上 —— 空着的话亮色档会落到深色模板上。
/// rev 73：调音量的音效（`volume_sound`）是新字段，默认关 —— serde 默认就是
/// 「关 + 轻点 + 三种手势全勾」，老配置读进来什么都不用搬，这里只留个记号。
const CONFIG_REV: u32 = 73;

/// 面板弹出动画的风格。
///
/// `System` 照 Windows 自己弹音量面板的样子：整块从任务栏背后滑进来
/// （Windows 11 上实测是「指数缓出的位移」，见 `ui::anim::SystemMotion`）。
/// `Fade` 是就地淡入淡出。
///
/// rev 66 删掉了原来那档「从任务栏展开」（裁内容那种）。老配置里写着
/// `unfold` 的，反序列化时按 `alias` 落到 `System`（最接近的观感），
/// 下次保存就写成 `system`。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PopupAnimStyle {
    /// 跟系统的音量面板一样（整块从屏幕底、任务栏背后滑上来），
    /// 曲线照系统的指数衰减，速度按设置里的毫秒等比缩放。
    #[default]
    #[serde(alias = "unfold")]
    System,
    /// 就地淡入淡出：位置不动，只把整窗不透明度拉起来。
    Fade,
}

impl PopupAnimStyle {
    pub fn index(self) -> u32 {
        match self {
            Self::System => 0,
            Self::Fade => 1,
        }
    }

    pub fn from_index(value: u32) -> Self {
        match value {
            1 => Self::Fade,
            _ => Self::System,
        }
    }


    /// 这一档是不是「照系统来」。
    pub fn is_system(self) -> bool {
        matches!(self, Self::System)
    }
}

/// OSD 的出现 / 消失动画风格。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OsdAnimStyle {
    /// 就地淡入淡出（默认，和以前一样）。
    #[default]
    Fade,
    /// 跟系统的音量条一模一样：从下方滑上来 + 淡入，消失时滑回去 + 淡出。
    #[serde(alias = "unfold")]
    System,
}

impl OsdAnimStyle {
    pub fn index(self) -> u32 {
        match self {
            Self::Fade => 0,
            Self::System => 1,
        }
    }

    pub fn from_index(value: u32) -> Self {
        match value {
            1 => Self::System,
            _ => Self::Fade,
        }
    }

    /// 这一档是不是「整扇窗平移」（滑上来 / 滑下去）。
    pub fn is_system(self) -> bool {
        matches!(self, Self::System)
    }
}

/// 托盘提示最多几行。
///
/// Windows 的通知区域提示是一个 128 个 UTF-16 单元的固定缓冲区
/// （`NOTIFYICONDATAW.szTip`，含结尾的 0），多行用 `\r\n` 分隔也照样占这个额度；
/// 六行已经能把音量 / 设备 / 应用都摆开，再多只会被系统截掉。
pub const MAX_TOOLTIP_LINES: usize = 6;

/// 单行最多几个字符（防止一行写到天荒地老、把后面的行全挤掉）。
///
/// 比总上限宽一点：只写一行时能放下「设备名 + 读数」，两行以上由
/// `MAX_TOOLTIP_CHARS` 兜底截断。
pub const MAX_TOOLTIP_LINE_CHARS: usize = 64;

/// 托盘提示的总字符上限（UTF-16 单元，留一个给结尾的 0）。
pub const MAX_TOOLTIP_CHARS: usize = 127;

/// 对数模式下一格滚轮的默认步进（dB）。
///
/// 和 EarTrumpet 的滑条一致：`VolumeSlider.OnMouseWheel` 里是 `sign(delta) * 2.0`，
/// 对数刻度下 `Minimum = LogarithmicVolumeMinDb(-40)`、`Maximum = 0`，
/// 所以那 2.0 就是 2 dB。
pub const DEFAULT_LOG_SCROLL_STEP_DB: f32 = 2.0;

/// 对数滚轮步进的调节范围（dB）。
///
/// 下限 0.1 dB 是「还能看出来」的极限：面板读数本身就是一位小数
/// （`-20.0 dB`），再细的步进按一次读数根本不变，滑块位移也低于一个像素。
/// 顺带一提，实测这台机器上各设备的硬件步进是 1 dB / 1.5 dB
/// （`IAudioEndpointVolume::GetVolumeRange` 的 increment），比 0.1 dB 粗得多。
pub const LOG_SCROLL_STEP_RANGE_DB: (f32, f32) = (0.1, 20.0);

/// 弹出动画默认时长（毫秒）。对照 QcClip 的 `NORMAL_SHOW_ANIMATION_DURATION`。
pub const DEFAULT_POPUP_ANIMATION_MS: u32 = 200;

/// 数字图标默认字体族。
///
/// 2026-10-25 起按用户自己调好的那套（Arial）当默认：Arial 的数字笔画粗细
/// 在 16px 托盘上比 Segoe UI 更「立」，配下面的 20 点 + 宽高缩放默认值正好。
pub const DEFAULT_NUMBER_FONT: &str = "Arial";

/// 托盘数字的抗锯齿方式，照 Photoshop 字符面板底部那七档。
///
/// GDI 本身只有「不抗锯齿 / 灰度抗锯齿 / ClearType」三类质量档，
/// 这里七档是「质量档 + 灰度曲线」拼出来的近似：`Sharp` 把边缘压陡、
/// `Strong` 让笔画更实（PS 的「浑厚」就是把笔画加粗一档）。
///
/// 默认是 `Sharp`（灰度抗锯齿 + 边缘压陡）：16px 的托盘上比「平滑」清楚一档，
/// 而且灰度抗锯齿逐像素稳定 —— ClearType（`WindowsLcd`）会让同一字形随子像素
/// 相位微变，滑动音量条时数字会轻轻抖。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TrayAntialias {
    /// 无：硬边，只有全黑 / 全透明两种像素。
    None,
    /// 锐利：抗锯齿但边缘压陡（默认）。
    #[default]
    Sharp,
    /// 犀利：抗锯齿，边缘比「平滑」略细。
    Crisp,
    /// 浑厚：笔画更实、更粗。
    Strong,
    /// 平滑：普通灰度抗锯齿。
    Smooth,
    /// Windows LCD：ClearType 子像素抗锯齿。
    WindowsLcd,
    /// Windows：交给 GDI 自己挑（`DEFAULT_QUALITY`）。
    Windows,
}

impl TrayAntialias {
    /// 界面上那七档的顺序（存的是下标）。
    pub const ALL: [TrayAntialias; 7] = [
        TrayAntialias::None,
        TrayAntialias::Sharp,
        TrayAntialias::Crisp,
        TrayAntialias::Strong,
        TrayAntialias::Smooth,
        TrayAntialias::WindowsLcd,
        TrayAntialias::Windows,
    ];

    pub fn index(self) -> u32 {
        Self::ALL
            .iter()
            .position(|item| *item == self)
            .unwrap_or(1) as u32
    }

    pub fn from_index(index: u32) -> Self {
        Self::ALL.get(index as usize).copied().unwrap_or_default()
    }

    /// 这一档用的 GDI 质量值（`CreateFontW` 的 `fdwQuality`）。
    pub fn quality(self) -> i32 {
        match self {
            // 硬边：让 GDI 直接给 1-bit 字形。
            Self::None => 1, // NONANTIALIASED_QUALITY
            // 灰度抗锯齿，边缘浓淡由 `gamma` 再调。
            Self::Sharp | Self::Crisp | Self::Smooth => 4, // ANTIALIASED_QUALITY
            // 子像素抗锯齿（取最大通道当覆盖率，不会有彩边）。
            Self::Strong | Self::WindowsLcd => 5, // CLEARTYPE_QUALITY
            Self::Windows => 0,                   // DEFAULT_QUALITY
        }
    }

    /// 覆盖率的灰度曲线指数：>1 让边缘更快变黑（更锐），<1 让边缘更实（更厚）。
    pub fn gamma(self) -> f32 {
        match self {
            Self::Sharp => 1.40,
            Self::Crisp => 1.15,
            Self::Strong => 0.78,
            Self::Smooth => 1.0,
            _ => 1.0,
        }
    }

    /// 是否把覆盖率阈值化成 0 / 1（「无」这一档）。
    pub fn threshold(self) -> bool {
        matches!(self, Self::None)
    }
}

/// 某一档位数（一位数 / 两位数 / 三位数）的托盘数字外观。
///
/// 单位尽量照 Photoshop 字符面板：字号是**点**，宽度 / 高度是**百分比**
/// （100% 就是字形本来的宽高），字距是 **1/1000 em**（PS 的 VA 值）。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct TrayDigitMetrics {
    /// 字号（点）。显示时按 `点 × DPI / 72` 换算成像素，0 = 自动
    /// （墨迹高度撑到画布的 70%，这是旧版「自动」的观感）。
    #[serde(default)]
    pub size_pt: u32,
    /// 水平缩放（%）：100 是字形原本的宽度。
    #[serde(default = "default_hundred")]
    pub width: u32,
    /// 垂直缩放（%）。
    #[serde(default = "default_hundred")]
    pub height: u32,
    /// 字距（1/1000 em，PS 的 VA 值，可为负）。
    #[serde(default)]
    pub tracking: i32,
}

fn default_hundred() -> u32 {
    100
}

impl Default for TrayDigitMetrics {
    fn default() -> Self {
        Self {
            size_pt: 0,
            width: 100,
            height: 100,
            tracking: 0,
        }
    }
}

/// 一位数（0–9）的默认外观：20 点、宽高各 60%。
///
/// 这套是用户 2026-10-25 调好、要求当默认的：字号统一 20 点，靠宽高缩放把
/// 一位数 / 两位数 / 三位数各自压到合适的宽度，比逐档改字号好维护。
fn default_tray_digit_one() -> TrayDigitMetrics {
    TrayDigitMetrics {
        size_pt: 20,
        width: 60,
        height: 60,
        tracking: 0,
    }
}

/// 两位数的默认外观（20 点、45%）。
fn default_tray_digit_two() -> TrayDigitMetrics {
    TrayDigitMetrics {
        size_pt: 20,
        width: 45,
        height: 45,
        tracking: 0,
    }
}

/// 三位数（100）的默认外观（20 点、33%）。
fn default_tray_digit_three() -> TrayDigitMetrics {
    TrayDigitMetrics {
        size_pt: 20,
        width: 33,
        height: 33,
        tracking: 0,
    }
}

impl TrayDigitMetrics {
    /// 字号范围（点）：0 = 自动，1..=200。16px 的画布上 12 点就满了。
    pub const SIZE_RANGE: (u32, u32) = (0, 200);
    /// 宽 / 高缩放范围（%）。
    pub const SCALE_RANGE: (u32, u32) = (10, 300);
    /// 字距范围（1/1000 em）。
    pub const TRACKING_RANGE: (i32, i32) = (-500, 1000);
}

/// 旧版本写进配置文件的默认值：读到它就说明用户没主动改过，迁移到新默认。
const LEGACY_POPUP_ANIMATION_MS: u32 = 140;

/// rev 65 之前的默认值。4xx 那档在淡入淡出上明显拖沓，读到它同样当「没改过」。
const LEGACY_POPUP_ANIMATION_MS_400: u32 = 400;

/// 一个全局快捷键：修饰符位 + 虚拟键码。
///
/// 字段取值和 Win32 的 `RegisterHotKey` 一致（mods 用 MOD_ALT/CONTROL/SHIFT/WIN
/// 那套位，key 用 VK_*），所以注册时不用再做一次翻译，配置文件里也能直接看懂。
/// `key == 0` 表示未设置。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hotkey {
    /// 修饰符位：Alt = 1、Ctrl = 2、Shift = 4、Win = 8（可叠加）。
    #[serde(default)]
    pub mods: u32,
    /// 虚拟键码（`VK_*`），0 表示未设置。
    #[serde(default)]
    pub key: u32,
}

impl Hotkey {
    pub fn new(mods: u32, key: u32) -> Self {
        Self { mods, key }
    }

    pub fn is_empty(&self) -> bool {
        self.key == 0
    }
}

/// 一个动作最多可以录三组快捷键。
///
/// 序列化写成数组（`[{...}, {...}, {...}]`），反序列化既认数组也认单个对象，
/// 所以升级前写下的 `"hotkey_flyout": {"mods":..,"key":..}` 依然能读进来。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HotkeySlots(pub [Hotkey; 3]);

impl HotkeySlots {
    pub fn get(&self, index: usize) -> Hotkey {
        self.0.get(index).copied().unwrap_or_default()
    }

    pub fn set(&mut self, index: usize, hotkey: Hotkey) {
        if let Some(slot) = self.0.get_mut(index) {
            *slot = hotkey;
        }
    }

    pub fn all(&self) -> impl Iterator<Item = Hotkey> + '_ {
        self.0.iter().copied()
    }
}

impl serde::Serialize for HotkeySlots {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeSeq;
        let mut seq = serializer.serialize_seq(Some(3))?;
        for hotkey in self.0 {
            seq.serialize_element(&hotkey)?;
        }
        seq.end()
    }
}

impl<'de> serde::Deserialize<'de> for HotkeySlots {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        #[derive(serde::Deserialize)]
        #[serde(untagged)]
        enum Repr {
            Many(Vec<Hotkey>),
            One(Hotkey),
        }
        let mut slots = HotkeySlots::default();
        match Repr::deserialize(deserializer)? {
            Repr::Many(list) => {
                for (index, hotkey) in list.into_iter().take(3).enumerate() {
                    slots.set(index, hotkey);
                }
            }
            Repr::One(hotkey) => slots.set(0, hotkey),
        }
        Ok(slots)
    }
}

/// 显示器亮度调节（DDC/CI）。
///
/// 默认**关闭**：读一次 DDC/CI 要几十毫秒，没接支持 DDC/CI 的显示器时
/// 那些调用全是白费，所以由用户自己开。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BrightnessConfig {
    /// 启用后，音量面板旁边会多出一个显示器调节窗口。
    pub enabled: bool,
    /// 一次「+ / −」的步进（百分点）。默认 2，跟参考实现
    /// （ddc-control 的 `increase`/`decrease` 每次 2）一致。
    pub step: u32,
    /// 保存下来的亮度预设，面板旁边的窗口里点一下就能套用。
    pub presets: Vec<BrightnessPreset>,
    /// 悬停在某台显示器的亮度条上滚动滚轮调它的亮度。
    ///
    /// 和音量那套「悬停滚轮」是同一份逻辑：只看位移的符号（一格算一档）、
    /// 边界截断、节流去重。默认开，跟音量面板的滚轮开关保持一致。
    pub scroll_wheel: bool,
    /// 滚轮步进：一格滚轮改变多少个百分点（1..=50）。
    ///
    /// 以前存的是「段数」（整条 0–100 切几段、一格走一段），那个方向是反的 ——
    /// 数字越大每格越小，谁都猜不到。现在直接就是百分比，和「亮度步进」同一个口径。
    /// 老配置的 `scroll_segments` 在 `Config::load` 里换算过来（100 / 段数）。
    pub scroll_step: u32,
    /// 面板宽度（逻辑像素）。
    ///
    /// 滑条长度 = 面板宽度 - 两侧步进钮 - 内边距，所以这个数字直接决定
    /// 「鼠标走 1 像素 = 多少个百分点」。**0 = 跟随音量面板宽度**（默认）。
    pub width: u32,
    /// 亮度面板贴在音量面板的哪一侧。
    pub placement: BrightnessPlacement,
    /// 亮度面板自己怎么排：横向（一行一台显示器）/ 竖向（一台显示器一条推子）。
    pub panel_layout: PanelLayout,
    /// 竖向排版时显示器名怎么显示：编号 / 全名（放不下就滚动）。
    pub name_mode: DisplayNameMode,
    /// 竖向排版的面板高度（逻辑像素）。**0 = 跟随音频面板**（默认）。
    ///
    /// 跟的是「音频面板也竖向排的时候的高度」，两个面板并排看着一样齐；
    /// 显式写了数字就自己定，推子吃掉多出来的那一截。
    pub vertical_height: u32,
    /// 亮度快捷键一次走多少个百分点。**0 = 跟随上面的「亮度步进」**（默认）。
    ///
    /// 面板上的 +/− 是精细活儿（2% 一档），键盘上按一下往往想要大一点（5%~10%），
    /// 两者分开配，谁也碍不着谁。
    pub hotkey_step: u32,
    /// 「指定显示器」那组亮度快捷键调第几台（1 起，按枚举顺序）。
    ///
    /// 超出实际台数时按最后一台算 —— 换了个少一台的环境不至于按了没反应。
    pub hotkey_monitor: u32,
    /// 亮度面板与音量面板**融合**成一个窗口。
    ///
    /// 融合时亮度块不再是一扇独立的小窗，而是长在音量面板里，排版跟着音频走：
    /// 音频横排就整条铺在它上 / 下方，音频竖排就整列贴在它左 / 右侧。
    pub merged: bool,
    /// 融合时亮度块贴在音频的哪一侧（横排只看上 / 下，竖排只看左 / 右）。
    pub merge_side: BrightnessMergeSide,
}

/// 面板排版。
///
/// 横向 = 一路一行，名字 + 横条 + 读数都摊开（默认，和最早的样子一致）。
/// 竖向 = 一路一条立起来的推子，图标在上、条在中间，像调音台。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PanelLayout {
    #[default]
    Horizontal,
    Vertical,
}

impl PanelLayout {
    pub fn is_vertical(self) -> bool {
        matches!(self, Self::Vertical)
    }

    pub fn index(self) -> u32 {
        match self {
            Self::Horizontal => 0,
            Self::Vertical => 1,
        }
    }

    pub fn from_index(value: u32) -> Self {
        match value {
            1 => Self::Vertical,
            _ => Self::Horizontal,
        }
    }

    pub fn label(self, lang: Lang) -> &'static str {
        match self {
            Self::Horizontal => lang.pick("横向", "Horizontal"),
            Self::Vertical => lang.pick("竖向", "Vertical"),
        }
    }
}

/// 竖向排版时显示器名显示到哪一步。
///
/// 列的宽度只够写下几个字，所以默认只留编号；要看名字就选全名，
/// 放不下时横向滚一遍。两种模式都**不显示括号里的型号**。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DisplayNameMode {
    /// 只显示显示器编号（1、2、3…）。
    #[default]
    Number,
    /// 显示全名，去掉括号里的型号；超出列宽时滚动。
    FullName,
}

impl DisplayNameMode {
    pub fn index(self) -> u32 {
        match self {
            Self::Number => 0,
            Self::FullName => 1,
        }
    }

    pub fn from_index(value: u32) -> Self {
        match value {
            1 => Self::FullName,
            _ => Self::Number,
        }
    }

    pub fn label(self, lang: Lang) -> &'static str {
        match self {
            Self::Number => lang.pick("编号", "Number"),
            Self::FullName => lang.pick("全名", "Full name"),
        }
    }
}

/// 亮度面板相对音量面板放在哪一侧。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BrightnessPlacement {
    /// 左侧（默认，和最早的行为一致）。
    #[default]
    Left,
    Right,
    Top,
    Bottom,
}

impl BrightnessPlacement {
    pub fn index(self) -> u32 {
        match self {
            Self::Left => 0,
            Self::Right => 1,
            Self::Top => 2,
            Self::Bottom => 3,
        }
    }

    pub fn from_index(value: u32) -> Self {
        match value {
            1 => Self::Right,
            2 => Self::Top,
            3 => Self::Bottom,
            _ => Self::Left,
        }
    }

    pub fn label(self, lang: Lang) -> &'static str {
        match self {
            Self::Left => lang.pick("左侧", "Left"),
            Self::Right => lang.pick("右侧", "Right"),
            Self::Top => lang.pick("上方", "Above"),
            Self::Bottom => lang.pick("下方", "Below"),
        }
    }
}

/// 融合时亮度块摆在音频的哪一侧。
///
/// 横排的音量面板只有上下两条边可以长，竖排的只有左右两条 —— 所以同一个值
/// 只在对应那一轴上有效，换到另一轴时按该轴的默认（横排下方 / 竖排右侧）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BrightnessMergeSide {
    /// 音频上方。
    Above,
    /// 音频下方（默认）。
    #[default]
    Below,
    /// 音频左侧。
    Left,
    /// 音频右侧。
    Right,
}

impl BrightnessMergeSide {
    pub fn index(self) -> u32 {
        match self {
            Self::Above => 0,
            Self::Below => 1,
            Self::Left => 2,
            Self::Right => 3,
        }
    }

    pub fn from_index(value: u32) -> Self {
        match value {
            0 => Self::Above,
            2 => Self::Left,
            3 => Self::Right,
            _ => Self::Below,
        }
    }

    pub fn label(self, lang: Lang) -> &'static str {
        match self {
            Self::Above => lang.pick("上方", "Above"),
            Self::Below => lang.pick("下方", "Below"),
            Self::Left => lang.pick("左侧", "Left"),
            Self::Right => lang.pick("右侧", "Right"),
        }
    }

    /// 是不是横排那一轴的取值（上 / 下）。
    pub fn is_horizontal_axis(self) -> bool {
        matches!(self, Self::Above | Self::Below)
    }
}

impl Default for BrightnessConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            step: 2,
            presets: Vec::new(),
            scroll_wheel: true,
            scroll_step: DEFAULT_BRIGHTNESS_SCROLL_STEP,
            width: DEFAULT_BRIGHTNESS_WIDTH,
            placement: BrightnessPlacement::default(),
            panel_layout: PanelLayout::Horizontal,
            name_mode: DisplayNameMode::Number,
            vertical_height: 0,
            hotkey_step: 0,
            hotkey_monitor: 1,
            merged: false,
            merge_side: BrightnessMergeSide::default(),
        }
    }
}

impl BrightnessConfig {
    /// 面板宽度（逻辑像素）。
    ///
    /// `0` 表示跟随音量面板（默认），这里只负责把显式写下的宽度钳进可调范围。
    pub fn panel_width(&self, flyout_width: f32) -> f32 {
        if self.width == 0 {
            return flyout_width;
        }
        self.width.clamp(120, BRIGHTNESS_WIDTH_RANGE.1) as f32
    }

    /// 竖向排版时的高度。`None` = 跟随音频面板。
    pub fn vertical_panel_height(&self) -> Option<f32> {
        vertical_height_value(self.vertical_height)
    }

    /// 融合模式生效了没有：两个开关都开着才算。
    pub fn merged_with_audio(&self) -> bool {
        self.enabled && self.merged
    }

    /// 融合时亮度块在音频的哪一侧，已按当前排版归一到对应那一轴：
    /// 横排只认上 / 下，竖排只认左 / 右；存的值属于另一轴时用该轴的默认。
    pub fn merge_side_of(&self, vertical: bool) -> BrightnessMergeSide {
        if vertical == self.merge_side.is_horizontal_axis() {
            // 轴不匹配（横排配了左 / 右，或竖排配了上 / 下）：用该轴默认。
            if vertical {
                BrightnessMergeSide::Right
            } else {
                BrightnessMergeSide::Below
            }
        } else {
            self.merge_side
        }
    }

    /// 记一个预设（同名同值的不重复记；超过上限就丢掉最老的一个）。
    ///
    /// 名字默认就是「80%」这种：预设本质是「记住这个亮度」，
    /// 用户点一下能看出套用的是多少，比「预设 1 / 预设 2」好认。
    pub fn add_preset(&mut self, brightness: u32) {
        let brightness = brightness.min(100);
        if self
            .presets
            .iter()
            .any(|preset| preset.brightness == brightness && preset.contrast.is_none())
        {
            return;
        }
        if self.presets.len() >= MAX_BRIGHTNESS_PRESETS {
            self.presets.remove(0);
        }
        self.presets.push(BrightnessPreset {
            name: format!("{brightness}%"),
            brightness,
            contrast: None,
        });
    }
}

/// 按前台应用过滤提示条。
///
/// 黑名单 = 名单里的应用不弹；白名单 = 只有名单里的应用才弹。
/// 两种都是按进程名比（`chrome.exe` 这种，不区分大小写）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OsdFilterMode {
    /// 不过滤，所有应用都弹。
    #[default]
    Off,
    /// 名单里的应用不弹。
    Blacklist,
    /// 只有名单里的应用才弹。
    Whitelist,
}

impl OsdFilterMode {
    pub fn index(self) -> u32 {
        match self {
            Self::Off => 0,
            Self::Blacklist => 1,
            Self::Whitelist => 2,
        }
    }

    pub fn from_index(value: u32) -> Self {
        match value {
            1 => Self::Blacklist,
            2 => Self::Whitelist,
            _ => Self::Off,
        }
    }

    pub fn label(self, lang: Lang) -> &'static str {
        match self {
            Self::Off => lang.pick("不过滤", "Off"),
            Self::Blacklist => lang.pick("黑名单", "Blacklist"),
            Self::Whitelist => lang.pick("白名单", "Whitelist"),
        }
    }

    /// 两种模式都要用那份名单。
    pub fn uses_list(self) -> bool {
        !matches!(self, Self::Off)
    }
}

/// 黑名单条目的类型。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum AppBlockKind {
    /// 单个程序：`path` 是 exe 的完整路径。
    #[default]
    App,
    /// 整个目录：`path` 是目录，里面所有程序都算。
    Dir,
    /// 只按 exe 文件名匹配，不管装在哪：`path` 存的就是文件名（`dnf.exe`）。
    ///
    /// 给「手动输入」用 —— 游戏这种拿不到路径（或者窗口根本不在主进程上）的场景，
    /// 敲个 `DNF.exe` 就够了。
    Name,
}

/// 黑名单里的一条。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppBlockEntry {
    pub kind: AppBlockKind,
    /// 程序 / 目录的完整路径；`Name` 类型存文件名。
    pub path: String,
}

impl AppBlockEntry {
    pub fn new(kind: AppBlockKind, path: impl Into<String>) -> Self {
        let path = path.into();
        let path = match kind {
            // 手动输入的名字：只留文件名、小写，没写扩展名就补 `.exe`
            // （用户敲 `DNF` 的比敲 `DNF.exe` 的多）。
            AppBlockKind::Name => normalize_exe_name(&path),
            // 统一成 Windows 的反斜杠、去掉末尾多余的斜杠：比路径时少一层特判。
            _ => normalize_path(&path),
        };
        Self { kind, path }
    }

    /// 手动输入一行：带路径分隔符的当完整路径，否则当 exe 名。
    pub fn from_input(input: &str) -> Option<Self> {
        let trimmed = input.trim().trim_matches('"');
        if trimmed.is_empty() {
            return None;
        }
        let kind = if trimmed.contains('\\') || trimmed.contains('/') {
            AppBlockKind::App
        } else {
            AppBlockKind::Name
        };
        let entry = Self::new(kind, trimmed);
        (!entry.path.is_empty()).then_some(entry)
    }

    /// 界面上显示的名字：程序是文件名（`Quicker.exe`），目录是目录名。
    pub fn name(&self) -> &str {
        if self.kind == AppBlockKind::Name {
            return &self.path;
        }
        let trimmed = self.path.trim_end_matches('\\');
        trimmed.rsplit('\\').next().unwrap_or(trimmed)
    }
}

/// exe 名的归一化：去掉路径、转小写、补 `.exe`。
fn normalize_exe_name(name: &str) -> String {
    let trimmed = name.trim().trim_matches('"');
    let base = trimmed.rsplit(['\\', '/']).next().unwrap_or(trimmed);
    let lower = base.to_ascii_lowercase();
    if lower.is_empty() {
        return String::new();
    }
    if lower.contains('.') {
        lower
    } else {
        format!("{lower}.exe")
    }
}

/// 黑名单：这些应用在前台时 Bolume 让路（快捷键 / 滚轮 / 提示条都不响应）。
///
/// 和 OSD 自己的「按应用过滤」不是一回事 —— 那个只决定提示条弹不弹，
/// 这里连快捷键和滚轮一起让出去（托盘图标手动点开面板仍然可用）。
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
pub struct AppBlockConfig {
    /// 前台是全屏游戏 / 全屏视频时也让路（原来 OSD 与快捷键各有一个开关，合并到这里）。
    #[serde(default)]
    pub fullscreen: bool,
    /// 全屏例外：这些程序就算全屏也照常启用 Bolume（上面的 `fullscreen` 对它们不生效）。
    ///
    /// 和 `entries` 是两份互不相干的名单：`entries` 管「前台是它就让路」，
    /// 这份只管「全屏时别拦我」。所以两边都写同一个程序时，它全屏不拦、
    /// 但一进前台仍然让路 —— 那是用户更早也更强的意思。
    #[serde(default)]
    pub fullscreen_exempt: Vec<AppBlockEntry>,
    /// 名单，按添加顺序排。
    #[serde(default)]
    pub entries: Vec<AppBlockEntry>,
}

impl AppBlockConfig {
    /// 这个 exe 路径要不要让路。
    ///
    /// - 程序条目：完整路径相等（忽略大小写）；
    /// - 目录条目：exe 就在这个目录里（含子目录）；
    /// - 名字条目：文件名相等，不管装在哪个目录。
    pub fn blocks_exe(&self, exe_path: &str) -> bool {
        matches_entry(&self.entries, exe_path)
    }

    /// 这条进程链里有谁在「全屏例外」名单里。
    ///
    /// 判定范围和黑名单一致（前台进程 + 祖先链）：游戏把窗口交给子进程画时，
    /// 用户写的往往是主程序名，只看前台那一个会漏。
    pub fn exempts_fullscreen(&self, chain: &[String]) -> bool {
        chain
            .iter()
            .any(|exe| matches_entry(&self.fullscreen_exempt, exe))
    }

    /// 「全屏例外」名单里有没有这一条（同类型 + 同路径）。
    pub fn contains_exempt(&self, entry: &AppBlockEntry) -> bool {
        contains_entry(&self.fullscreen_exempt, entry)
    }

    /// 往「全屏例外」名单里加一条；已在名单里返回 false。
    pub fn add_exempt(&mut self, entry: AppBlockEntry) -> bool {
        if entry.path.is_empty() || self.contains_exempt(&entry) {
            return false;
        }
        self.fullscreen_exempt.push(entry);
        true
    }

    /// 手动输入一条进「全屏例外」（exe 名或完整路径）。
    pub fn add_exempt_input(&mut self, input: &str) -> bool {
        match AppBlockEntry::from_input(input) {
            Some(entry) => self.add_exempt(entry),
            None => false,
        }
    }

    pub fn remove_exempt(&mut self, index: usize) {
        if index < self.fullscreen_exempt.len() {
            self.fullscreen_exempt.remove(index);
        }
    }

    /// 这条已经在名单里了？（同类型 + 同路径，忽略大小写）
    ///
    /// 界面靠它给「已存在」的提示：同一个程序拖两次不该悄悄多出一行，
    /// 但也不能一声不响什么都不做（看着像拖拽失灵）。
    pub fn contains(&self, entry: &AppBlockEntry) -> bool {
        contains_entry(&self.entries, entry)
    }

    /// 手动输入一条（exe 名或完整路径）。
    pub fn add_input(&mut self, input: &str) -> bool {
        match AppBlockEntry::from_input(input) {
            Some(entry) => {
                if self.contains(&entry) {
                    return false;
                }
                self.entries.push(entry);
                true
            }
            None => false,
        }
    }

    /// 加一条（重复的不再加）。返回是否真的加进去了。
    pub fn add(&mut self, kind: AppBlockKind, path: impl Into<String>) -> bool {
        let entry = AppBlockEntry::new(kind, path);
        if entry.path.is_empty() || self.contains(&entry) {
            return false;
        }
        self.entries.push(entry);
        true
    }

    pub fn remove(&mut self, index: usize) {
        if index < self.entries.len() {
            self.entries.remove(index);
        }
    }
}

/// 一条 exe 路径命中名单里的某一条没有（两份名单共用同一套匹配规则）。
fn matches_entry(list: &[AppBlockEntry], exe_path: &str) -> bool {
    if exe_path.is_empty() {
        return false;
    }
    let exe = normalize_path(exe_path).to_lowercase();
    let file = exe.rsplit('\\').next().unwrap_or(&exe).to_string();
    list.iter().any(|entry| {
        let path = normalize_path(&entry.path).to_lowercase();
        match entry.kind {
            AppBlockKind::App => path == exe,
            AppBlockKind::Dir => {
                let dir = path.trim_end_matches('\\');
                !dir.is_empty() && (exe == dir || exe.starts_with(&format!("{dir}\\")))
            }
            AppBlockKind::Name => path == file,
        }
    })
}

/// 名单里有没有这一条（同类型 + 同路径，忽略大小写）。
fn contains_entry(list: &[AppBlockEntry], entry: &AppBlockEntry) -> bool {
    list.iter().any(|existing| {
        existing.kind == entry.kind && existing.path.eq_ignore_ascii_case(&entry.path)
    })
}

/// 路径归一化：反斜杠、合并重复分隔符、去尾部斜杠（盘根 `C:\` 与 UNC 前缀保留）。
fn normalize_path(path: &str) -> String {
    let replaced = path.trim().replace('/', "\\");
    // 连续的分隔符压成一个（手改配置时很容易写成 `C:\\Tools\\\\App.exe`），
    // 开头的两个反斜杠是 UNC 前缀，不能动。
    let mut compact = String::with_capacity(replaced.len());
    for (index, ch) in replaced.chars().enumerate() {
        if ch == '\\' && compact.ends_with('\\') && index > 1 {
            continue;
        }
        compact.push(ch);
    }
    let trimmed = compact.trim_end_matches('\\');
    if trimmed.is_empty() {
        compact
    } else if trimmed.len() == 2 && trimmed.ends_with(':') {
        format!("{trimmed}\\")
    } else {
        trimmed.to_string()
    }
}

/// OSD 卡片贴在屏幕的哪个位置。
///
/// 除了八个固定位置，还有「指定坐标」（相对所选显示器左上角的像素偏移）
/// 和「跟随鼠标指针」。默认下居中，和 Windows 自带的音量 OSD 一样。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OsdAnchor {
    TopLeft,
    TopCenter,
    TopRight,
    BottomLeft,
    /// 下居中（默认，和 Windows 自带 OSD 同一位置）。
    #[default]
    BottomCenter,
    BottomRight,
    /// 指定坐标：`custom_x` / `custom_y` 是相对显示器左上角的像素偏移。
    Custom,
    /// 跟随鼠标指针：卡片摆在光标右下角，超出屏幕时自动往回收。
    Cursor,
}

impl OsdAnchor {
    pub fn index(self) -> u32 {
        match self {
            Self::TopLeft => 0,
            Self::TopCenter => 1,
            Self::TopRight => 2,
            Self::BottomLeft => 3,
            Self::BottomCenter => 4,
            Self::BottomRight => 5,
            Self::Custom => 6,
            Self::Cursor => 7,
        }
    }

    pub fn from_index(value: u32) -> Self {
        match value {
            0 => Self::TopLeft,
            1 => Self::TopCenter,
            2 => Self::TopRight,
            3 => Self::BottomLeft,
            5 => Self::BottomRight,
            6 => Self::Custom,
            7 => Self::Cursor,
            _ => Self::BottomCenter,
        }
    }

    pub fn is_custom(self) -> bool {
        matches!(self, Self::Custom)
    }
}

/// OSD 卡片里可以单独摆位置的元素。
///
/// 文字类元素是**按需新建、条数不限**的：`Text(n)` 指第 n 条自定义文字，
/// 内容存在 `OsdConfig.texts[n]`。老配置里的 `text0` / `text1` / `text2`
/// 读进来就是 `Text(0..2)`，序列化回去还是同名，格式不变。
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum OsdPart {
    /// 功能图标（喇叭 / 静音 / 上一曲 / 播放暂停…）。
    Icon,
    /// 状态文字：总体信息模式是设备名，操作模式是动作名；写了固定文字就用固定文字。
    Title,
    /// 百分比读数。老配置里的 `value` 就是它（后来才和分贝拆开）。
    Percent,
    /// 分贝读数。和百分比是**两个独立元素**：各自摆位置、各自调字号、各自删。
    Db,
    /// 音量条。
    Bar,
    /// 第 n 条自定义文字（0 起），内容在 `OsdConfig.texts` 里。
    Text(u16),
}

impl Serialize for OsdPart {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let name = match self {
            Self::Icon => "icon".to_string(),
            Self::Title => "title".to_string(),
            Self::Percent => "percent".to_string(),
            Self::Db => "db".to_string(),
            Self::Bar => "bar".to_string(),
            Self::Text(index) => format!("text{index}"),
        };
        serializer.serialize_str(&name)
    }
}

impl<'de> Deserialize<'de> for OsdPart {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let raw = String::deserialize(deserializer)?;
        Ok(match raw.as_str() {
            "icon" => Self::Icon,
            "title" => Self::Title,
            // 老配置里百分比叫 `value`。
            "percent" | "value" => Self::Percent,
            "db" => Self::Db,
            "bar" => Self::Bar,
            other => match other.strip_prefix("text") {
                Some(number) => Self::Text(number.parse().unwrap_or(0)),
                // 认不出来的（手改坏了）当图标，不至于整份配置读不出来。
                None => Self::Icon,
            },
        })
    }
}

impl OsdPart {
    /// 固定的五个元素（文字不在里面：它们是按需新建的）。
    pub const FIXED: [OsdPart; 5] = [
        OsdPart::Icon,
        OsdPart::Title,
        OsdPart::Percent,
        OsdPart::Db,
        OsdPart::Bar,
    ];

    /// 是不是「文字」这一类（状态文字 + 自定义文字）。
    pub fn is_text(self) -> bool {
        matches!(self, Self::Title | Self::Text(_))
    }

    /// 自定义文字在 `texts` 里的下标；状态文字不是这里的文本（它有自己的自动内容）。
    pub fn text_index(self) -> Option<usize> {
        match self {
            Self::Text(index) => Some(index as usize),
            _ => None,
        }
    }

    pub fn index(self) -> u32 {
        match self {
            Self::Icon => 0,
            Self::Title => 1,
            Self::Percent => 2,
            Self::Db => 3,
            Self::Bar => 4,
            Self::Text(index) => 5 + index as u32,
        }
    }

    pub fn from_index(value: u32) -> Self {
        match value {
            1 => Self::Title,
            2 => Self::Percent,
            3 => Self::Db,
            4 => Self::Bar,
            5.. => Self::Text((value - 5) as u16),
            _ => Self::Icon,
        }
    }

    pub fn label(self, lang: Lang) -> String {
        match self {
            Self::Icon => lang.pick("图标", "Icon").to_string(),
            Self::Title => lang.pick("状态", "Status").to_string(),
            Self::Percent => lang.pick("百分比", "Percent").to_string(),
            Self::Db => lang.pick("分贝", "dB").to_string(),
            Self::Bar => lang.pick("音量条", "Bar").to_string(),
            Self::Text(index) => format!("{} {}", lang.pick("文本", "Text"), index as u32 + 1),
        }
    }

    /// 默认尺寸（逻辑像素）：图标是边长，文字是字号，音量条是厚度。
    pub fn default_size(self) -> u32 {
        match self {
            Self::Icon => 20,
            Self::Title => 13,
            Self::Percent => 13,
            Self::Db => 10,
            Self::Bar => 6,
            Self::Text(_) => 13,
        }
    }

    /// 默认对齐：读数是数字，靠右最稳；图标 / 音量条居中；文字靠左。
    pub fn default_align(self) -> OsdAlign {
        match self {
            Self::Percent | Self::Db => OsdAlign::Right,
            Self::Icon | Self::Bar => OsdAlign::Center,
            _ => OsdAlign::Left,
        }
    }

    /// 宽度下限（逻辑像素）：再窄就放不下内容了。
    pub fn min_width(self) -> f32 {
        match self {
            Self::Icon => 8.0,
            Self::Title => 40.0,
            Self::Percent | Self::Db => 24.0,
            Self::Bar => 30.0,
            Self::Text(_) => 30.0,
        }
    }

    /// 尺寸的可调范围（逻辑像素）。
    pub fn size_range(self) -> (u32, u32) {
        match self {
            Self::Icon => (8, 64),
            Self::Title | Self::Percent => (8, 40),
            Self::Db => (6, 40),
            Self::Bar => (2, 24),
            Self::Text(_) => (8, 40),
        }
    }
}

/// 元素内容的水平对齐：框不动，里面那行东西贴哪一边。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum OsdAlign {
    #[default]
    Left,
    Center,
    Right,
}

impl OsdAlign {
    pub fn index(self) -> u32 {
        match self {
            Self::Left => 0,
            Self::Center => 1,
            Self::Right => 2,
        }
    }

    pub fn label(self, lang: Lang) -> &'static str {
        match self {
            Self::Left => lang.pick("靠左", "Left"),
            Self::Center => lang.pick("居中", "Center"),
            Self::Right => lang.pick("靠右", "Right"),
        }
    }
}

/// OSD 卡片的自定义配色。空串 = 跟随当前主题。
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OsdColors {
    /// 卡片背景。
    #[serde(default)]
    pub card: String,
    /// 元素颜色（文字 / 图标）。
    #[serde(default)]
    pub text: String,
    /// 强调色（音量条填充、选中框）。
    #[serde(default)]
    pub accent: String,
    /// 音量条的轨道底色；空 = 主题的轨道色。
    ///
    /// 音量条一共三段颜色：轨道底色（这根）、静音时的填充（`bar_muted`）、
    /// 平时（未静音）的填充 —— 后者跟强调色走，也可以按元素单独改。
    #[serde(default)]
    pub bar_track: String,
    /// 静音时音量条的填充色；空 = 主题那层半透明的淡色。
    #[serde(default)]
    pub bar_muted: String,
}

/// OSD 卡片的投影。默认关着 —— 有的人就喜欢干净的直角卡片。
///
/// `offset_x` / `offset_y` 是偏移（逻辑像素，负数往左/上），`blur` 是扩散（0 = 硬边），
/// `opacity` 是整块影子的不透明度。颜色留空就用文字色（深色主题下是浅色 -> 不像影子，
/// 所以默认给一个纯黑）。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct OsdShadow {
    pub enabled: bool,
    /// 颜色（`#rrggbb`）；空 = 纯黑。
    #[serde(default)]
    pub color: String,
    #[serde(default)]
    pub offset_x: i32,
    #[serde(default)]
    pub offset_y: i32,
    #[serde(default)]
    pub blur: u32,
    #[serde(default)]
    pub opacity: u32,
}

impl Default for OsdShadow {
    fn default() -> Self {
        Self {
            enabled: false,
            color: String::new(),
            // 往右下偏 4px、扩散 12：贴在浅色桌面上最像「浮起来」的一档。
            offset_x: 0,
            offset_y: 4,
            blur: 12,
            opacity: 45,
        }
    }
}

/// 阴影偏移的可调范围（逻辑像素）。
pub const OSD_SHADOW_OFFSET_RANGE: (i32, i32) = (-40, 40);
/// 阴影扩散 / 不透明度的可调范围。
///
/// 扩散上限给得很大：窗口那圈留白是按 blur 算出来的，不再是固定容器，
/// 想画一朵「云」也随你（再大的时候窗口会自己撑开）。
pub const OSD_SHADOW_BLUR_RANGE: (u32, u32) = (0, 200);
pub const OSD_SHADOW_OPACITY_RANGE: (u32, u32) = (0, 100);

/// 一个元素在卡片里的位置、尺寸与显隐。
///
/// 位置存比例而不是像素：卡片宽高可以改，元素跟着一起动，不用重排。
/// 尺寸存**逻辑像素**（图标边长 / 字号 / 音量条厚度），和位置是两回事 ——
/// 卡片拉大时位置按比例走，字号保持不变（文字不会跟着糊成一片）。
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct OsdPartPos {
    pub part: OsdPart,
    pub x: f32,
    pub y: f32,
    /// 尺寸（逻辑像素）。0 = 用这个元素的默认值。
    #[serde(default)]
    pub size: u32,
    /// 宽度（逻辑像素）。0 = 自动：从 x 一直铺到卡片右边距。
    ///
    /// 拖动框体时会把「自动」固化成当时看到的宽度，这样整块是平移的 ——
    /// 不然右边缘永远贴着右边距，拖起来只会左右拉伸。
    #[serde(default)]
    pub width: u32,
    /// 内容在框里的水平对齐。缺省 = 该元素自己的默认（读数靠右、文字靠左）。
    #[serde(default)]
    pub align: Option<OsdAlign>,
    /// 关掉 = 这个元素不画（模块化「删掉」它）。
    #[serde(default = "default_true")]
    pub visible: bool,
}

/// 某个元素自己的颜色。
///
/// 单独一张表而不是塞进 `OsdPartPos`：那边是 `Copy` 的小结构（每帧到处克隆），
/// 放字符串会把整条链路都变成克隆字符串。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PartColor {
    pub part: OsdPart,
    /// `#rrggbb`。
    pub color: String,
}

/// 媒体键模板的默认排布：喇叭在左，标题居中横排在右上（用户调好的那套）。
///
/// 它和音量模板是两套独立的数据 —— 用户在媒体键页拖动，只改这一份。
/// 媒体键没有数值，也没有音量条：默认关掉，需要的话用户可以自己打开。
fn default_media_layout() -> Vec<OsdPartPos> {
    vec![
        OsdPartPos {
            part: OsdPart::Icon,
            x: 0.0681,
            y: 0.2858,
            size: 19,
            width: 0,
            visible: true,
            align: None,
        },
        OsdPartPos {
            part: OsdPart::Title,
            x: 0.3325,
            y: 0.1478,
            size: 14,
            width: 64,
            visible: true,
            align: Some(OsdAlign::Center),
        },
        OsdPartPos {
            part: OsdPart::Percent,
            x: 0.81,
            y: 0.1667,
            size: 13,
            width: 36,
            visible: false,
            align: None,
        },
        OsdPartPos {
            part: OsdPart::Db,
            x: 0.15,
            y: 0.1667,
            size: 13,
            width: 60,
            visible: false,
            align: None,
        },
        OsdPartPos {
            part: OsdPart::Bar,
            x: 0.15,
            y: 0.6354,
            size: 4,
            width: 234,
            visible: false,
            align: None,
        },
    ]
}

/// 哪几种操作会弹 OSD。
///
/// 每个功能单独一个开关：有人只想看音量，有人连上一曲都要提示。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OsdFunction {
    Volume,
    Mute,
    PrevTrack,
    NextTrack,
    PlayPause,
    Stop,
}

impl OsdFunction {
    pub const ALL: [OsdFunction; 6] = [
        OsdFunction::Volume,
        OsdFunction::Mute,
        OsdFunction::PrevTrack,
        OsdFunction::NextTrack,
        OsdFunction::PlayPause,
        OsdFunction::Stop,
    ];

    /// 媒体键（上一曲 / 下一曲 / 播放暂停 / 停止）—— 剩下的就是音量操作（音量 / 静音）。
    ///
    /// 提示条按这个分两套模板：音量操作走音量模板，其余一律走媒体键模板。
    pub fn is_media(self) -> bool {
        !matches!(self, Self::Volume | Self::Mute)
    }

    pub fn label(self, lang: Lang) -> &'static str {
        match self {
            Self::Volume => lang.pick("音量", "Volume"),
            Self::Mute => lang.pick("静音", "Mute"),
            Self::PrevTrack => lang.pick("上一曲", "Previous track"),
            Self::NextTrack => lang.pick("下一曲", "Next track"),
            Self::PlayPause => lang.pick("播放 / 暂停", "Play / pause"),
            Self::Stop => lang.pick("停止", "Stop"),
        }
    }
}

/// OSD 贴到哪块屏上。
///
/// `Single` 时用 `OsdConfig::monitor`（0 = 主显示器，n = 按左上角排序的第 n 台）；
/// 另外两种不看那个字段。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub enum OsdMonitorMode {
    /// 指定一台。
    #[default]
    #[serde(rename = "single")]
    Single,
    /// 跟随鼠标所在的那块屏（每次弹的时候现算）。
    #[serde(rename = "cursor")]
    Cursor,
    /// 每块屏各弹一张。
    #[serde(rename = "all")]
    All,
}

/// 屏幕上的提示条（OSD）。
///
/// 只显示「刚刚发生了什么」：调音量、静音、切歌。样式（圆角 / 透明度 /
/// 位置 / 元素排布）、动画、悬停交互全在这里配。
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct OsdConfig {
    /// 总开关。
    pub enabled: bool,

    /// 下面六个是「哪一种操作要弹提示」的独立开关。
    pub volume: bool,
    pub mute: bool,
    pub prev_track: bool,
    pub next_track: bool,
    pub play_pause: bool,
    pub stop: bool,

    /// 卡片尺寸（逻辑像素）。
    pub width: u32,
    pub height: u32,
    /// 圆角（逻辑像素）。
    pub corner: u32,
    /// 音量条的圆角，存「半高的百分之几」：100 = 全圆头（默认，跟以前一样），0 = 方角。
    ///
    /// 不存逻辑像素：条高本身是可调的（元素尺寸），存 px 的话改条高圆角就变形了；
    /// 百分比跟着条高一起缩放，才调得出「一直圆头 / 一直方角」。
    pub bar_round: u32,
    /// 不透明度（30..100）。
    pub opacity: u32,

    /// 贴哪儿。
    pub anchor: OsdAnchor,
    /// 指定坐标用：相对显示器左上角的偏移（逻辑像素）。
    pub custom_x: u32,
    pub custom_y: u32,
    /// 0 = 主显示器；n = 按左上角排序后的第 n 台（从 1 开始）。
    pub monitor: u32,
    /// 显示器怎么选：指定一台 / 跟随鼠标 / 全部显示器。
    pub monitor_mode: OsdMonitorMode,

    /// 现在编辑 / 预览的是哪一套排布：`false` = 音量模板，`true` = 媒体键模板。
    ///
    /// **它只管编辑器里正在改哪一套**，不决定运行时画哪套 —— 运行时是按这次触发的
    /// 操作现场选的（音量 / 静音用音量模板，上一曲 / 下一曲 / 播放暂停 / 停止用媒体键模板），
    /// 见 [`OsdConfig::for_function`]。
    pub detailed: bool,

    /// 动画开关；关掉就直接出现、直接消失。
    pub animate: bool,
    /// 动画风格：就地淡入淡出，还是照系统音量条「滑上来 + 淡入」。
    ///
    /// 选 `System` 时下面两个时长不再生效（用系统那套节奏）。
    #[serde(default)]
    pub anim_style: OsdAnimStyle,
    /// 淡入 / 淡出时长（毫秒）。
    pub fade_in_ms: u32,
    pub fade_out_ms: u32,
    /// 完全显示后停留多久（毫秒）。
    ///
    /// 连续调节（一直按音量键 / 一直滚轮 / 正在拖）期间只续时、不重新淡入，
    /// 也不会淡出；手一停就照常倒计时收掉 —— 这条行为不提供开关。
    pub hold_ms: u32,

    /// 鼠标悬停在提示条上时不消失。
    pub hover_hold: bool,
    /// 悬停时可交互：滚轮调音量、点图标切静音 / 播放暂停。
    pub interactive: bool,

    /// 悬停时滚轮调音量（默认开）。
    ///
    /// 打开时**整张卡片**都认滚轮 —— 光标停在卡片任意位置转一下滚轮就调音量，
    /// 不必精确压在细细的那条音量条上。关掉之后卡片完全不响应滚轮（拖动音量条、
    /// 点图标切静音仍然可用，那两个归 `interactive` 管）。
    pub wheel_hover: bool,

    /// 拖动元素时自动吸附对齐（默认开）。
    pub snap: bool,

    /// 没配快捷键的系统媒体键（上一曲 / 下一曲 / 播放暂停 / 停止）也接管：
    /// 弹一条提示，再把键原样转发给播放器。默认关（保持系统原生行为）。
    pub takeover_media_keys: bool,

    /// 禁用 Windows 自己的音量 OSD（那条按音量键弹出来的音量条）。
    ///
    /// **只在 Bolume 运行时生效**：主程序的低级键盘钩子把音量键在到达 shell 之前
    /// 吞掉（带 `Ctrl / Alt / Shift` 也吞 —— 系统那条条不认修饰键），改由 Bolume
    /// 弹自己的卡片。退出 Bolume 之后系统行为恢复原样。
    pub block_system_osd: bool,

    /// 标题文字。空 = 用自动标题（「音量」「已静音」「下一曲」这种）。
    pub title_text: String,
    /// 自定义文字（一条都没有 = 不显示任何自定义文字）。
    pub texts: Vec<String>,

    /// 元素的图层顺序：**后面的画在上面**（后画的盖住先画的）。
    ///
    /// 空 = 按内置顺序（图标 → 状态 → 百分比 → 分贝 → 音量条 → 文字）。编辑器里
    /// 拖动元素列表就是在改这个；只存「排过序的清单」，认不出的元素读的时候忽略，
    /// 清单里没提到的按内置顺序补在最后。
    pub z_order: Vec<OsdPart>,

    /// 卡片投影。
    #[serde(default)]
    pub shadow: OsdShadow,

    /// 喇叭图标样式：`auto` = 跟着音量变（默认），其余是固定的一种。
    ///
    /// 取值就是内嵌图标名（`speaker` / `volume-high` / `volume-low` / `volume-off`），
    /// 认不出来的一律按 `auto` 处理。
    pub icon_style: String,

    /// 百分比后面带不带 `%`（默认带）。
    pub percent_sign: bool,

    /// 按前台应用过滤（黑名单 / 白名单）。
    pub filter_mode: OsdFilterMode,
    /// 名单里的进程名（小写、含 `.exe`）。
    pub filter_apps: Vec<String>,

    /// 卡片配色（空串 = 跟随主题）。
    #[serde(default)]
    pub colors: OsdColors,

    /// 元素各自的颜色（没列进来的跟随 `colors.text`）。
    #[serde(default)]
    pub part_colors: Vec<PartColor>,

    /// 音量模板的元素排布。
    pub layout: Vec<OsdPartPos>,
    /// 媒体键模板的元素排布（和音量模板完全独立，各调各的）。
    #[serde(default = "default_media_layout")]
    pub media_layout: Vec<OsdPartPos>,
}

/// 默认的元素排布：喇叭在左、读数在右、音量条在底（卡片 191×50）。
///
/// 这是用户 2026-10-20 自己调好后固化下来的那套外观：
/// - 喇叭 19px，纵向略偏上一点（0.2858）；
/// - 百分比 13px 靠右（0.7382），和喇叭同一水平带；
/// - 音量条 4px 粗，从 0.2147 起、固定 93px 宽，横在卡片下方（0.44）；
/// - 分贝、标题默认不显示，需要时在布局编辑器里打开（自定义文字按需新建）。
///
/// 坐标是归一化的，卡片尺寸变了按比例跟着走；只有音量条宽度是固定像素。
fn default_osd_layout() -> Vec<OsdPartPos> {
    vec![
        OsdPartPos {
            part: OsdPart::Icon,
            x: 0.0681,
            y: 0.2858,
            size: 19,
            width: 0,
            visible: true,
            align: None,
        },
        OsdPartPos {
            part: OsdPart::Title,
            x: 0.15,
            y: 0.2078,
            size: 17,
            width: 85,
            visible: false,
            align: None,
        },
        OsdPartPos {
            part: OsdPart::Percent,
            x: 0.7382,
            y: 0.272,
            size: 13,
            width: 37,
            visible: true,
            align: None,
        },
        OsdPartPos {
            part: OsdPart::Db,
            x: 0.15,
            y: 0.1667,
            size: 13,
            width: 60,
            visible: false,
            align: Some(OsdAlign::Left),
        },
        OsdPartPos {
            part: OsdPart::Bar,
            x: 0.2147,
            y: 0.44,
            size: 4,
            width: 93,
            visible: true,
            align: None,
        },
        // 自定义文字不放默认排布里：编辑器里点「＋ 文字」才新建一条（条数不限）。
    ]
}

/// 喇叭图标可选的样式：`(配置值, 图标名, 中文, 英文)`。
///
/// 第一项是「自动」——跟着音量变（小声 / 大声 / 静音），其余是固定的一种。
/// 图标全部来自内嵌的 SVG 资源，不额外画图。
/// 一套喇叭图标样式。
///
/// `states` 是「静音 / 小声 / 大声」三张图：有它就是**动态**的（跟着音量换），
/// 没有就是**固定**的一张（`icon`）。原先「满格 / 小声 / 静音」也被列成三个可选项，
/// 可那三个本来就是同一套动态图标的三张静态图 —— 选哪个都只是冻在某一格，
/// 所以现在只留动态的几套，外加一个固定的光喇叭。
pub struct IconStyle {
    /// 配置里存的值。
    pub key: &'static str,
    /// 编辑器格子里的缩略图 / 固定样式用的那张。
    pub icon: &'static str,
    /// 三态（静音 / 小声 / 大声）。`None` = 不随音量变。
    pub states: Option<[&'static str; 3]>,
    pub zh: &'static str,
    pub en: &'static str,
}

/// 顺序 = 下拉里的顺序。第一套也是「认不出来时」的兜底，别把别的挪到最前面。
///
/// 前两套是同一颗喇叭配两种弧线画法（`弧线1` 是系统那种三条弧、`弧线2` 是等距
/// 细弧），接着是竖条 / 圆点，最后是光身子喇叭。
pub const ICON_STYLES: &[IconStyle] = &[
    IconStyle {
        key: "auto",
        icon: "volume-high",
        states: Some(["volume-off", "volume-low", "volume-high"]),
        zh: "弧线1",
        en: "Arcs 1",
    },
    // 下面三套是同一个喇叭身子配不同的「音量档位」符号：弧线 / 竖条 / 圆点，
    // 各三张图，跟着音量在静音 / 小声 / 大声之间换。
    IconStyle {
        key: "arc",
        icon: "volume-arc-high",
        states: Some(["volume-off", "volume-arc-low", "volume-arc-high"]),
        zh: "弧线2",
        en: "Arcs 2",
    },
    IconStyle {
        key: "bar",
        icon: "volume-bar-high",
        states: Some(["volume-off", "volume-bar-low", "volume-bar-high"]),
        zh: "竖条",
        en: "Bars",
    },
    IconStyle {
        key: "dot",
        icon: "volume-dot-high",
        states: Some(["volume-off", "volume-dot-low", "volume-dot-high"]),
        zh: "圆点",
        en: "Dots",
    },
    // 光喇叭：没有档位符号，但静音时仍然得有自己的样子（喇叭 + 叉），
    // 不然静音了图标一点没变，看着像没生效。小声 / 大声共用同一张。
    IconStyle {
        key: "speaker",
        icon: "speaker",
        states: Some(["volume-off", "speaker", "speaker"]),
        zh: "音箱",
        en: "Speaker",
    },
];

/// 没选样式时的取值。
fn default_icon_style() -> String {
    "auto".to_string()
}

impl Default for OsdConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            volume: true,
            mute: true,
            prev_track: true,
            next_track: true,
            play_pause: true,
            stop: true,
            width: 191,
            height: 50,
            corner: 10,
            bar_round: 100,
            opacity: 100,
            anchor: OsdAnchor::BottomCenter,
            custom_x: 60,
            custom_y: 60,
            monitor: 0,
            monitor_mode: OsdMonitorMode::Single,
            detailed: false,
            animate: true,
            anim_style: OsdAnimStyle::default(),
            fade_in_ms: 150,
            fade_out_ms: 240,
            hold_ms: 1400,
            hover_hold: true,
            interactive: true,
            wheel_hover: true,
            snap: true,
            takeover_media_keys: false,
            block_system_osd: false,
            title_text: String::new(),
            texts: Vec::new(),
            z_order: Vec::new(),
            icon_style: default_icon_style(),
            percent_sign: true,
            shadow: OsdShadow::default(),
            filter_mode: OsdFilterMode::Off,
            filter_apps: Vec::new(),
            colors: OsdColors::default(),
            part_colors: Vec::new(),
            layout: default_osd_layout(),
            media_layout: default_media_layout(),
        }
    }
}

impl OsdConfig {
    /// 音量条的圆角占「半高」的比例（0..=1）。100% = 全圆头，0 = 方角。
    pub fn bar_round_ratio(&self) -> f32 {
        self.bar_round.min(100) as f32 / 100.0
    }

    /// 这个功能要不要弹提示。
    pub fn shows(&self, function: OsdFunction) -> bool {
        if !self.enabled {
            return false;
        }
        match function {
            OsdFunction::Volume => self.volume,
            OsdFunction::Mute => self.mute,
            OsdFunction::PrevTrack => self.prev_track,
            OsdFunction::NextTrack => self.next_track,
            OsdFunction::PlayPause => self.play_pause,
            OsdFunction::Stop => self.stop,
        }
    }

    /// 按这次触发的操作挑模板：返回一份「已经切到对应模板」的配置副本。
    ///
    /// 音量 / 静音 → 音量模板（`layout`）；上一曲 / 下一曲 / 播放暂停 / 停止 →
    /// 媒体键模板（`media_layout`）。配置里存的 `detailed` 只是编辑器当前在改哪套，
    /// 运行时不看它 —— 否则换个模板就得改设置，媒体键触发时也画成音量样式。
    pub fn for_function(&self, function: OsdFunction) -> OsdConfig {
        let mut cfg = self.clone();
        cfg.detailed = function.is_media();
        cfg
    }

    /// 某个元素的当前位置、尺寸与显隐（缺项回落到默认排布）。
    ///
    /// 老配置没有 `size`（读进来是 0），这里统一补成该元素的默认尺寸；
    /// 越界的值也在这里夹一遍 —— 渲染和设置页读的都是这个结果。
    /// 当前模板（音量 / 媒体键）对应的那份排布。
    fn active(&self) -> &Vec<OsdPartPos> {
        if self.detailed {
            &self.media_layout
        } else {
            &self.layout
        }
    }

    fn active_mut(&mut self) -> &mut Vec<OsdPartPos> {
        if self.detailed {
            &mut self.media_layout
        } else {
            &mut self.layout
        }
    }

    /// 这个元素当前模板下的对齐方式。
    pub fn align_of(&self, part: OsdPart) -> OsdAlign {
        self.part(part)
            .align
            .unwrap_or_else(|| part.default_align())
    }

    pub fn part(&self, part: OsdPart) -> OsdPartPos {
        let fallback = default_osd_layout()
            .into_iter()
            .find(|position| position.part == part)
            .unwrap_or(OsdPartPos {
                part,
                x: 0.1,
                y: 0.1,
                size: part.default_size(),
                width: 0,
                align: None,
                visible: true,
            });
        let mut position = self
            .active()
            .iter()
            .find(|position| position.part == part)
            .copied()
            .unwrap_or(fallback);
        if position.size == 0 {
            position.size = part.default_size();
        }
        let (low, high) = part.size_range();
        position.size = position.size.clamp(low, high);
        position
    }

    /// 这个元素在不在（模块化「删掉」就是把它关掉）。
    ///
    /// 自定义文字还要求内容非空 —— 没写字就没有东西可画。
    pub fn shows_part(&self, part: OsdPart) -> bool {
        if !self.part(part).visible {
            return false;
        }
        match part {
            // 状态文字空着时有自动内容（音量 / 下一曲），照样算在。
            OsdPart::Title => true,
            OsdPart::Text(index) => !self.text_at(index as usize).trim().is_empty(),
            _ => true,
        }
    }

    /// 当前模板里的元素清单（**按图层顺序，靠后的画在上面**）。
    ///
    /// 固定的五个 + 这份排布里出现的自定义文字，然后按 `z_order` 重排；
    /// `z_order` 里没提到的（新加的文字、老配置）按内置顺序补在最后。
    pub fn parts(&self) -> Vec<OsdPart> {
        let mut existing = OsdPart::FIXED.to_vec();
        // 新建的文字**一律**列出来：万一某次重置把它从排布里挤掉了，
        // 卡片上还在画、列表里却找不到，那就没法再调它了。
        for index in 0..self.texts.len() {
            existing.push(OsdPart::Text(index as u16));
        }
        for position in self.active() {
            if matches!(position.part, OsdPart::Text(_)) && !existing.contains(&position.part) {
                existing.push(position.part);
            }
        }
        let mut out: Vec<OsdPart> = Vec::with_capacity(existing.len());
        for part in &self.z_order {
            if existing.contains(part) && !out.contains(part) {
                out.push(*part);
            }
        }
        for part in existing {
            if !out.contains(&part) {
                out.push(part);
            }
        }
        out
    }

    /// 把某个元素挪到第 `target` 层（0 = 最底层，最后 = 最上层）。
    pub fn move_part_to(&mut self, part: OsdPart, target: usize) {
        let mut order = self.parts();
        let Some(from) = order.iter().position(|slot| *slot == part) else {
            return;
        };
        order.remove(from);
        let target = target.min(order.len());
        order.insert(target, part);
        self.z_order = order;
    }

    /// 第 `index` 条自定义文字（越界给空串）。
    pub fn text_at(&self, index: usize) -> &str {
        self.texts
            .get(index)
            .map(|text| text.as_str())
            .unwrap_or("")
    }

    /// 写第 `index` 条自定义文字（越界就补齐）。
    pub fn set_text_at(&mut self, index: usize, value: String) {
        while self.texts.len() <= index {
            self.texts.push(String::new());
        }
        self.texts[index] = value;
    }

    /// 一条文字的内容（状态文字读 `title_text`，自定义文字读 `texts`）。
    pub fn text_for(&self, part: OsdPart) -> &str {
        match part {
            OsdPart::Title => &self.title_text,
            OsdPart::Text(index) => self.text_at(index as usize),
            _ => "",
        }
    }

    /// 写一条文字的内容。
    pub fn set_text_for(&mut self, part: OsdPart, value: String) {
        match part {
            OsdPart::Title => self.title_text = value,
            OsdPart::Text(index) => self.set_text_at(index as usize, value),
            _ => {}
        }
    }

    /// 新建一条自定义文字（条数不限），两个模板各补一条排布。
    ///
    /// 位置摆在「现有元素最靠下的那条」下面，压不到东西；摆不下就贴着
    /// 底边挤一点（反正拖一下就能挪走），总比正好盖在状态文字上强。
    pub fn add_text(&mut self) -> OsdPart {
        let index = self.texts.len();
        self.texts.push(String::new());
        let part = OsdPart::Text(index as u16);
        let lowest = self
            .active()
            .iter()
            .filter(|slot| slot.visible)
            .map(|slot| slot.y)
            .fold(0.0f32, f32::max);
        let y = (lowest + 0.12).min(0.88);
        let position = OsdPartPos {
            part,
            x: 0.15,
            y,
            size: 13,
            width: 0,
            align: None,
            visible: true,
        };
        for layout in [&mut self.layout, &mut self.media_layout] {
            if !layout.iter().any(|slot| slot.part == part) {
                layout.push(position);
            }
        }
        part
    }

    /// 删掉第 `index` 条自定义文字：内容、两套排布里的条目一起删，
    /// 后面的文字编号整体往前挪一格（不然会留个空洞，`texts` 也越删越长）。
    pub fn remove_text(&mut self, index: usize) {
        if index >= self.texts.len() {
            return;
        }
        self.texts.remove(index);
        self.z_order
            .retain(|part| *part != OsdPart::Text(index as u16));
        for part in self.z_order.iter_mut() {
            if let OsdPart::Text(other) = *part {
                if other as usize > index {
                    *part = OsdPart::Text(other - 1);
                }
            }
        }
        for layout in [&mut self.layout, &mut self.media_layout] {
            layout.retain(|slot| slot.part != OsdPart::Text(index as u16));
            for slot in layout.iter_mut() {
                if let OsdPart::Text(other) = slot.part {
                    if other as usize > index {
                        slot.part = OsdPart::Text(other - 1);
                    }
                }
            }
        }
    }

    /// 收掉内容为空的自定义文字（rev 38 迁移用）。
    ///
    /// 老版本固定留了三条空文字，编辑器里光占三行；现在改成按需新建，
    /// 读老配置时把空的收掉、有内容的往前挤并重新编号。
    pub fn compact_texts(&mut self) {
        let keep: Vec<usize> = (0..self.texts.len())
            .filter(|index| !self.texts[*index].trim().is_empty())
            .collect();
        let mut mapped: Vec<Option<u16>> = vec![None; self.texts.len()];
        for (new_index, old_index) in keep.iter().enumerate() {
            mapped[*old_index] = Some(new_index as u16);
        }
        self.texts = keep
            .iter()
            .map(|index| self.texts[*index].clone())
            .collect();
        for layout in [&mut self.layout, &mut self.media_layout] {
            let mut next: Vec<OsdPartPos> = Vec::with_capacity(layout.len());
            for slot in layout.iter() {
                match slot.part {
                    OsdPart::Text(old_index) => {
                        if let Some(Some(new_index)) = mapped.get(old_index as usize) {
                            let mut moved = *slot;
                            moved.part = OsdPart::Text(*new_index);
                            next.push(moved);
                        }
                    }
                    _ => next.push(*slot),
                }
            }
            *layout = next;
        }
        // 颜色表里的 Text(n) 也跟着重编号；被删掉的那几条颜色一起丢掉。
        let mut colors: Vec<PartColor> = Vec::with_capacity(self.part_colors.len());
        for entry in std::mem::take(&mut self.part_colors) {
            match entry.part {
                OsdPart::Text(old_index) => {
                    if let Some(Some(new_index)) = mapped.get(old_index as usize) {
                        colors.push(PartColor {
                            part: OsdPart::Text(*new_index),
                            color: entry.color,
                        });
                    }
                }
                _ => colors.push(entry),
            }
        }
        self.part_colors = colors;
    }

    /// 元素的条目（没有就按默认值补一条）。
    fn entry_mut(&mut self, part: OsdPart) -> &mut OsdPartPos {
        let mut position = self.part(part);
        position.visible = true;
        let layout = self.active_mut();
        if !layout.iter().any(|slot| slot.part == part) {
            layout.push(position);
        }
        layout
            .iter_mut()
            .find(|slot| slot.part == part)
            .expect("刚补过")
    }

    /// 挪动一个元素（夹在 0..1 之间）。
    pub fn set_part(&mut self, part: OsdPart, x: f32, y: f32) {
        // x 可以到 1.0：卡片的左右两侧一样，都能贴边（拖动那边已经保证不越界）。
        let x = x.clamp(0.0, 1.0);
        let y = y.clamp(0.0, 0.95);
        let slot = self.entry_mut(part);
        slot.x = x;
        slot.y = y;
    }

    /// 这个元素自己的颜色；空串 = 跟随全局文字色。
    pub fn part_color(&self, part: OsdPart) -> String {
        self.part_colors
            .iter()
            .find(|entry| entry.part == part)
            .map(|entry| entry.color.clone())
            .unwrap_or_default()
    }

    /// 设置元素颜色；传空串就是「恢复跟随文字色」。
    pub fn set_part_color(&mut self, part: OsdPart, color: String) {
        let color = color.trim().to_string();
        if let Some(pos) = self.part_colors.iter().position(|entry| entry.part == part) {
            if color.is_empty() {
                self.part_colors.remove(pos);
            } else {
                self.part_colors[pos].color = color;
            }
            return;
        }
        if !color.is_empty() {
            self.part_colors.push(PartColor { part, color });
        }
    }

    /// 改一个元素的尺寸（逻辑像素）。
    pub fn set_part_size(&mut self, part: OsdPart, size: u32) {
        let (low, high) = part.size_range();
        let size = size.clamp(low, high);
        self.entry_mut(part).size = size;
    }

    /// 改一个元素的宽度（逻辑像素）；传 0 回到「自动铺到右边距」。
    pub fn set_part_width(&mut self, part: OsdPart, width: u32) {
        self.entry_mut(part).width = width.min(4000);
    }

    /// 把当前模板恢复成默认（两个模板各恢复各的），卡片外观一起回到出厂。
    ///
    /// 元素位置是归一化的，卡片一变尺寸它们按比例跟着走；用户点「恢复默认布局」
    /// 通常就是「我把它拖乱了，回到原来那样」。
    /// 把当前模板恢复成出厂排布，**连带整张卡片的外观**一起回默认。
    ///
    /// 用户点这个就是「我把它调乱了，回到原来那样」，所以配色、元素单独色、
    /// 圆角、不透明度、阴影、图层顺序全部一起回出厂 —— 只把位置放回去、
    /// 留着上一版配色和影子，等于恢复了个寂寞（0.47 之前的毛病）。
    ///
    /// 只恢复**当前**这套模板的排布（另一套各恢复各的），文字内容一个字不动：
    /// 那是用户写的东西，不是「布局」；自定义文字连位置也一起留着（它们没有出厂位置）。
    pub fn reset_layout(&mut self) {
        // 固定的五个元素回出厂排布；**用户自己新建的文字原样留着** ——
        // 它们没有「出厂位置」，抹掉会让文字从元素列表里消失、还跑回卡片左上角。
        let mut fresh = if self.detailed {
            default_media_layout()
        } else {
            default_osd_layout()
        };
        fresh.extend(
            self.active()
                .iter()
                .filter(|slot| matches!(slot.part, OsdPart::Text(_)))
                .copied(),
        );
        if self.detailed {
            self.media_layout = fresh;
        } else {
            self.layout = fresh;
        }
        let defaults = OsdConfig::default();
        // 排布本身。
        self.width = defaults.width;
        self.height = defaults.height;
        self.z_order.clear();
        // 外观：配色（含元素单独色）、圆角、不透明度、投影。
        self.colors = defaults.colors;
        self.part_colors.clear();
        self.corner = defaults.corner;
        self.bar_round = defaults.bar_round;
        self.opacity = defaults.opacity;
        self.shadow = defaults.shadow;
    }

    /// 改一个元素内容的水平对齐。
    pub fn set_part_align(&mut self, part: OsdPart, align: OsdAlign) {
        self.entry_mut(part).align = Some(align);
    }

    /// 显示 / 隐藏（删掉）一个元素。
    pub fn set_part_visible(&mut self, part: OsdPart, visible: bool) {
        self.entry_mut(part).visible = visible;
    }

    /// 卡片实际的不透明度（0..1）。
    pub fn alpha(&self) -> f32 {
        self.opacity.clamp(10, 100) as f32 / 100.0
    }

    /// 用户固定选的喇叭图标；`None` = 跟着音量变（默认）。
    /// 选中的那一套图标样式（认不出来就是 `auto`）。
    pub fn icon_style(&self) -> &'static IconStyle {
        let style = self.icon_style.trim();
        ICON_STYLES
            .iter()
            .find(|entry| entry.key.eq_ignore_ascii_case(style))
            .unwrap_or(&ICON_STYLES[0])
    }

    /// 固定不随音量变的那张图；动态样式返回 `None`（三态由 `states` 决定）。
    pub fn fixed_icon(&self) -> Option<&'static str> {
        let entry = self.icon_style();
        if entry.key.eq_ignore_ascii_case("auto") || entry.states.is_some() {
            return None;
        }
        Some(entry.icon)
    }

    /// 选一个喇叭样式（`auto` = 跟着音量变）。
    pub fn set_icon_style(&mut self, style: &str) {
        self.icon_style = style.to_string();
    }

    /// 百分比后面带不带 `%`。
    pub fn percent_display(&self, percent: &str) -> String {
        if self.percent_sign {
            percent.to_string()
        } else {
            percent.trim_end_matches('%').to_string()
        }
    }
}

/// 一个亮度预设。
///
/// `contrast` 现在就留着：将来加上对比度调节时，老配置读进来是 `None`，
/// 不会因为少字段整份配置读不出来。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct BrightnessPreset {
    pub name: String,
    /// 亮度百分比（0..=100）。
    pub brightness: u32,
    /// 对比度百分比（0..=100），`None` = 这个预设不管对比度。
    #[serde(default)]
    pub contrast: Option<u32>,
}

/// 最多保留几个预设：再多就把面板旁边的窗口挤爆了。
pub const MAX_BRIGHTNESS_PRESETS: usize = 8;

/// 亮度滚轮默认步进：一格 2%，和音量滚轮默认的 2 个百分点一致。
pub const DEFAULT_BRIGHTNESS_SCROLL_STEP: u32 = 2;
/// 亮度滚轮步进可调范围（百分点）。一格 100% 就没法调了，上限压到 50。
pub const BRIGHTNESS_SCROLL_STEP_RANGE: (u32, u32) = (1, 50);

/// 亮度面板默认宽度：0 = 跟随音量面板宽度（两块一样宽，并排 / 叠着都齐）。
pub const DEFAULT_BRIGHTNESS_WIDTH: u32 = 0;
/// 亮度面板宽度可调范围（逻辑像素）。0 = 跟随音量面板宽度。
pub const BRIGHTNESS_WIDTH_RANGE: (u32, u32) = (0, 620);
/// 音量面板默认宽度（逻辑像素），和 `theme::FLYOUT_WIDTH` 一致。
pub const DEFAULT_FLYOUT_WIDTH: u32 = 384;
/// 音量面板宽度可调范围（逻辑像素）：再窄行里的名字就全被截没了。
pub const FLYOUT_WIDTH_RANGE: (u32, u32) = (300, 620);

/// 面板里音量条的默认粗细（逻辑像素），和滑条自己的默认值一致。
pub const DEFAULT_PANEL_BAR_THICKNESS: u32 = 6;
/// 面板音量条粗细可调范围（逻辑像素）：比 2 还细就看不见了，比 20 粗会顶到名字上。
pub const PANEL_BAR_THICKNESS_RANGE: (u32, u32) = (2, 20);
/// 面板音量条的默认圆角：存「半高的百分之几」，100 = 全圆头（胶囊形）。
pub const DEFAULT_PANEL_BAR_ROUND: u32 = 100;

/// 托盘右键菜单（自绘）默认最大宽度（逻辑像素）。
///
/// 设备名经常是 `Headphones (Meizu HiFi DAC Headphone Amplifier)` 这种，
/// 不定上限菜单能横穿半个屏幕，所以默认就压到 420，超出的用 … 收尾。
pub const DEFAULT_TRAY_MENU_MAX_WIDTH: u32 = 420;
/// 托盘菜单宽度可调范围（逻辑像素）：下限是「放得下一行设备名」。
pub const TRAY_MENU_WIDTH_RANGE: (u32, u32) = (168, 640);

/// 动态图标的默认灵敏度（百分比）。
///
/// 150% 是按本机试出来的：这个倍数下流行乐的高潮段刚好打满、安静段还剩一点动静。
pub const DEFAULT_TRAY_MOTION_GAIN: u32 = 150;
/// 灵敏度可调范围（百分比）。
pub const TRAY_MOTION_GAIN_RANGE: (u32, u32) = (50, 300);

/// 界面字体族（空 = 跟随系统）。
///
/// 只认一个名字：设置窗根节点上挂一次，整棵树都继承（gpui 的文本样式是往下继承的）。
pub const UI_FONT_DEFAULT: &str = "";
/// 界面字重的默认值（常规）。
pub const DEFAULT_UI_FONT_WEIGHT: u32 = 400;
/// 界面对比度的默认值（100% = 主题原样）。
pub const DEFAULT_UI_CONTRAST: u32 = 100;
/// 「跟随系统」时亮色档的默认底色：色块行里最后一个浅色。
pub const DEFAULT_LIGHT_SURFACE: &str = "#f4efe6";
/// 「跟随系统」时深色档的默认底色：色块行里第四个深色。
pub const DEFAULT_DARK_SURFACE: &str = "#262b33";
/// 界面对比度的可调范围（百分比）。低于 60 次级文字基本糊没了，高于 160 就刺眼。
pub const UI_CONTRAST_RANGE: (u32, u32) = (60, 160);

/// 音量面板**基准字号**（逻辑像素）。
///
/// 面板里各处字号本来是 10 / 11 / 11.5 / 12.5 这些固定值，其中正文（设备读数、
/// 空列表提示）是 11.5。字号设置就是「相对这一档放大多少」，
/// 所以 11.5 就是「100%」时的那一版面板，老配置迁移也按它换算。
pub const PANEL_BASE_FONT_SIZE: f32 = 11.5;
/// 面板字号可调范围（逻辑像素）。
///
/// 6 是还能看清的下限，72 已经比设备名那一栏还高；再往两头就是手滑。
pub const PANEL_FONT_SIZE_RANGE: (f32, f32) = (6.0, 72.0);
/// 面板字间距可调范围（逻辑像素，负数 = 紧排）。
pub const PANEL_LETTER_SPACING_RANGE: (f32, f32) = (-5.0, 20.0);
/// 面板行高的上限（逻辑像素）。0 = 跟着字号自动算。
///
/// 不设实际意义上的限制：填多少就是多少（行高本来就是给人自己排的），
/// 这一千只是挡住手改配置文件写出天文数字。
pub const PANEL_LINE_HEIGHT_MAX: u32 = 1000;

/// 按住音量 / 亮度快捷键时连续触发的间隔默认值（毫秒）。
///
/// 120ms ≈ 每秒 8 档，按住两秒从 0 拉到 100%；再快就和滑条一样滑溜，反而不好停。
pub const DEFAULT_HOTKEY_REPEAT_INTERVAL: u32 = 120;
/// 连发间隔的可调范围（毫秒）。30ms 以下基本等于一路滑到底，1s 以上按着没感觉。
pub const HOTKEY_REPEAT_RANGE: (u32, u32) = (30, 1000);

/// 按住多久才开始连发（毫秒）。0 = 不额外等待，按住就按间隔走。
///
/// 和系统「键盘字符重复延迟」一个意思：手滑按一下不会立刻连跳好几档，
/// 按住不放才进入连发。上限放到 9999（快十秒），够慢了；输入框也按四位给。
pub const HOTKEY_REPEAT_DELAY_RANGE: (u32, u32) = (0, 9999);

/// 「按住连续触发」默认在哪些动作上生效：音量四个 + 亮度六个。
///
/// 名字和 [`crate::platform::hotkey::Action::key`] 一一对应。切歌、静音、播放暂停、
/// 面板和设置默认不勾 —— 切歌按一下就是一曲，按住连发就把整张专辑跳完了。
pub const DEFAULT_REPEAT_ACTIONS: [&str; 10] = [
    "volume_up",
    "volume_down",
    "app_volume_up",
    "app_volume_down",
    "brightness_up",
    "brightness_down",
    "brightness_all_up",
    "brightness_all_down",
    "brightness_monitor_up",
    "brightness_monitor_down",
];

/// 亮度快捷键的步进可调范围（百分点）。0 = 跟随「亮度步进」，由它自己的行解释。
pub const BRIGHTNESS_HOTKEY_STEP_RANGE: (u32, u32) = (0, 50);

/// 竖向面板高度可调范围（逻辑像素）。0 = 自动 / 跟随，由各自面板自己解释。
pub const VERTICAL_HEIGHT_RANGE: (u32, u32) = (0, 900);
/// 竖向面板高度的下限（逻辑像素）：再矮推子就没地方立了。
pub const VERTICAL_HEIGHT_MIN: f32 = 140.0;

/// 把配置里写的竖向高度换算成像素：`0` = 自动 / 跟随（返回 `None`）。
fn vertical_height_value(raw: u32) -> Option<f32> {
    if raw == 0 {
        return None;
    }
    Some((raw as f32).clamp(VERTICAL_HEIGHT_MIN, VERTICAL_HEIGHT_RANGE.1 as f32))
}

/// 设备的自定义名字。
///
/// 按设备 id 存、不按名字存：设备改名（拔插 / 驱动更新）后自定义名还在，
/// 拔掉的设备也会留着这一条，插回来照样生效。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceAlias {
    pub id: String,
    pub name: String,
}

/// 显示器的自定义名字。
///
/// 按 `DisplayInfo::key`（`序号:描述`）存 —— 显示器没有 GUID 可拿，
/// 描述（EDID 里的物理名）+ 枚举序号是唯一比较稳的「同一台」的判据。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayAlias {
    pub key: String,
    pub name: String,
}

/// 设备的自定义图标。
///
/// 和改名一样按设备 id 存：设备拔掉再插回来（id 不变）图标还在。
/// `icon` 存的是图标文件名（`speaker` / `bookshelf` / `earbuds` …），
/// 只认白名单里的名字，别的值一律按「跟随设备类型」处理。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DeviceIconAlias {
    pub id: String,
    pub icon: String,
}

/// 应用的自定义名字（面板右键「重命名」）。
///
/// 按会话 key 存，和 `hidden_apps` 同一套：普通应用就是 `exe:<全路径小写>`，
/// 进程重启、换会话都不会丢；拿不到进程路径的少数应用退化成 `pid:<pid>`，
/// 那一条只在同一个进程活着时有意义。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppAlias {
    pub key: String,
    pub name: String,
}

/// 单个应用的响度均衡覆盖。
///
/// 全局开关在 `Config::loudness`；这里只记「这个应用跟全局不一样」的那部分。
/// 按会话 key 存，和 `hidden_apps` / `app_names` 同一套，重启不丢。
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AppLoudness {
    pub key: String,
    pub enabled: bool,
}

/// 响度均衡。
///
/// 算法照 TomSteady（Winamp 的 AGC 插件，Tom St Denis 写的 `dsp_tom.dll` v1.4）：
/// 每 26ms 一块（Winamp 的 1152 采样块），量出这一块**乘完当前增益之后**的峰值，
/// 跟目标峰值比 —— 超了就往下走一格，不到就往上走一格，一格 = 满量程的 `1/2^级别`。
/// 连续往上走 128 块之后步长再乘上级别（安静素材不用等半分钟才抬起来）。
///
/// 两个参数直接对应插件面板上的两个滑块（反汇编 `dsp_tom.dll` 得到的原值）：
///
/// | 这里 | 插件里的名字 | 范围 | 默认 |
/// |------|--------------|------|------|
/// | `level` | Desired Real Volume | 0–100 | 75 |
/// | `smoothness` | Volume Slide Smoothness（INI 里叫 `stepping`）| 5–10 | 8 |
///
/// 插件把 `stepping` 同时当定点小数位数用（`1<<stepping` = 100%），所以它的滑块
/// 最低只给到 5；我们内部用 f32、步长按 `1/2^级别` 算，1–10 都能用
/// （1 = 一块变 50%，10 = 一千块才走完一圈）。
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct LoudnessConfig {
    pub enabled: bool,
    /// 最大放大级别（1–100，%）：目标峰值占满量程的百分比。
    ///
    /// 也就是「最多能放大到多响」—— 100% 就是把声音推到满刻度附近
    /// （插件里的硬保护是 30000/32768 ≈ 91.6%，到那儿之前就开始压），
    /// 1% 等于几乎不放大。插件默认 75，这里默认满档 100：均衡的意义就是
    /// 「该多响就多响」，压着上限反而听不出它干活。
    pub level: u32,
    /// 音量变化级别（1–10）：一格走多远。
    ///
    /// 数值越大越稳（从静音走到满增益要 `2^级别` 块 ≈ 26ms × 2^级别），
    /// 越小越跟手。插件默认 8（约 6.7 秒走完，实测就是它）。
    pub smoothness: u32,
}

impl Default for LoudnessConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            level: 100,
            smoothness: 8,
        }
    }
}

impl LoudnessConfig {
    /// 引擎采样节拍（毫秒）。跟以前一样每 15ms 采一次会话峰值。
    pub const TICK_MS: u32 = 15;
    /// 插件的一个处理块（毫秒）：Winamp 一块 1152 采样，44.1kHz 下 26.1ms。
    /// 所有「一格」「一张票」都是按块算的，所以引擎要按块攒够时间才决策。
    pub const BLOCK_MS: u32 = 26;
    /// 插件里的「防止削顶」线：`30000/32768`，块内峰值超过它就直接压一格。
    pub const CLIP_LEVEL: f32 = 30000.0 / 32768.0;
    /// 连续抬升多少块之后步长乘上级别（`[0x100723cc] >= 0x80`）。
    pub const FAST_AFTER: u32 = 128;
    /// 预测器投票的饱和值和判决线（插件里那张 0x07 起的表，8 上 7 下）。
    pub const VOTE_MAX: i32 = 15;
    pub const VOTE_UP: i32 = 8;
    pub const VOTE_DOWN: i32 = 7;
    /// 输出（峰值 × 增益）超过目标的这个倍数，就按比例往回走，不再只投票。
    ///
    /// 1.5 倍 = +3.5dB：到这个程度已经「明显过响」，再用插件的 8 票投票去降
    /// 就太慢（视频里的瞬态只有一两块）。见 `Steady::step` 的说明。
    pub const OVER_FIT: f32 = 1.5;

    /// 增益越过 1.0 多少才值得开接管流。
    ///
    /// 和 1.0 之间留 2% 死区（0.17dB，听不出来）：正好压在 1.0 上时，
    /// 接管流会建了又拆 —— 每拆一次原声都要掉一下再回来，听着一顿一顿。
    pub const BOOST_ENGAGE: f32 = 1.02;
    /// 低于这个峰值就当「这一块没有内容」，增益一动都不动。
    ///
    /// 0.001 ≈ -60dB，人耳在这个电平上已经听不出内容了。不加这条的话，
    /// 对话间隙、曲间空白、视频缓冲的静音块全都在「不到目标」那一支里投票，
    /// 把增益一路堆到上限 —— 下一句一开口就是几倍的爆音（用户听到的
    /// 「一开始播放突然很大声、音质很差地响几百毫秒」就是它）。
    pub const MIN_PEAK: f32 = 0.001;
    /// 增益上限（4 倍 = +12dB）。
    ///
    /// 插件里 `vol` 也有上限（否则安静素材会把底噪一起抬成巨响）。插件跑在
    /// Winamp 的 DSP 链里，面对的是「音频文件音轨基本顶到满幅」——那里 8 倍
    /// （+18dB）几乎碰不到。
    ///
    /// 接在系统混音上是另一回事：播放器自己的音量旋钮在素材**之后**，用户把
    /// 播放器音量调到 20%，会话电平表读到的峰值就只有 0.05 上下（-26dB）——
    /// 8 倍会长期贴着上限跑，输出在 2 倍和 8 倍之间来回摆（实测 DOpus 播视频，
    /// 12dB 的反复抽动，听感就是「一直往响里叠、很炸」）。4 倍能覆盖到 0.23
    /// （-12.7dB）的素材，静的那一头不再硬顶；真要更响，把 `level` 调低的收益
    /// 和风险都更明确（那是「响到多响」的旋钮，不是这个上限）。
    pub const MAX_GAIN: f32 = 4.0;

    /// 目标峰值（线性幅度）。`level` 是百分比。
    ///
    /// 内部钳到防削顶线以下（`CLIP_LEVEL × 0.99 ≈ 90.6%`）：目标高过削顶线的话，
    /// 「到了目标」和「削顶了要压」两支会互相打架，增益在 0.86–0.97 之间来回跳，
    /// 听感是持续发抖。插件里同样有这个矛盾（`level` 拉到 100 时目标就是 100%，
    /// 而削顶线在 91.6%），只是它跑在 Winamp 的 DSP 链里、抖动被播放器自己的
    /// 音量盖住了；接在系统混音上必须收掉。
    pub fn target(&self) -> f32 {
        let want = self.level.clamp(1, 100) as f32 / 100.0;
        want.min(Self::CLIP_LEVEL * 0.99)
    }

    /// 一格走多远（增益单位）。`smoothness` 就是定点小数位数。
    pub fn step(&self) -> f32 {
        1.0 / (1u32 << self.smoothness.clamp(1, 10)) as f32
    }

    /// 一拍（15ms）合多少块（26ms）。引擎按块攒够才走一步。
    pub fn blocks_per_tick(&self) -> f32 {
        Self::TICK_MS as f32 / Self::BLOCK_MS as f32
    }
}

/// 开机自启的落地方式。
///
/// 三条路的时机差得不少：注册表 `Run` 要等资源管理器把启动项逐个拉起来；
/// 计划任务在**会话一建立**时就被任务计划程序拉起来 —— 早一两秒；
/// 系统启动走开机触发的 SYSTEM 任务，用户会话一出现就把主程序送进去。
///
/// 只保留一种，同时存在就是启动两次（第二次被单实例挡掉，但白跑一遍）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LaunchMethod {
    /// `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`（Windows 的默认做法）。
    #[default]
    RunKey,
    /// 计划任务：登录时触发。
    Task,
    /// 计划任务：开机时触发（SYSTEM），登录后把自己送进用户会话。
    Boot,
}

impl LaunchMethod {
    /// 优先级从早到晚：清理多余登记时按这个顺序决定留哪一条。
    pub const ALL: [Self; 3] = [Self::Boot, Self::Task, Self::RunKey];

    pub fn index(self) -> u32 {
        match self {
            Self::RunKey => 0,
            Self::Task => 1,
            Self::Boot => 2,
        }
    }

    pub fn from_index(value: u32) -> Self {
        match value {
            1 => Self::Task,
            2 => Self::Boot,
            _ => Self::RunKey,
        }
    }
}

#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    /// 界面语言：中文 / English。
    pub language: Lang,
    /// 对数音量刻度（默认关闭）。
    pub use_logarithmic_volume: bool,
    /// 对数刻度的下限 dB（默认 -40）。
    pub logarithmic_min_db: i32,
    /// 面板失去焦点时自动隐藏。
    pub hide_on_focus_lost: bool,
    /// 开机自启动。
    pub launch_at_login: bool,
    /// 开机自启用哪种方式落地（注册表 / 计划任务）。
    pub launch_method: LaunchMethod,
    /// 启动时以管理员身份运行。
    ///
    /// 计划任务那两档由任务计划程序按最高权限静默拉起（不弹 UAC）；注册表那档
    /// 只能自己 `runas` 重开一次，UAC 开着的机器上会弹确认。
    pub run_as_admin: bool,

    /// 列出「系统声音」会话。
    pub show_system_sounds: bool,
    /// 正在发声的应用排在列表最前面（面板开着时随声音实时调整）。
    /// rev 52 起默认关：列表会随声音自己跳动，想让它排最前得自己去开。
    pub sounding_apps_first: bool,

    /// 在托盘图标上滚动滚轮调节音量。
    pub use_scroll_wheel_in_tray: bool,
    /// 面板打开时，在整个面板上滚动滚轮调节当前设备音量。
    pub use_global_mouse_wheel_hook: bool,
    /// 在应用行上滚动滚轮调节该应用的音量。
    pub use_scroll_wheel_in_apps: bool,
    /// 一格滚轮的音量步进（百分比）。
    pub scroll_wheel_step: u32,
    /// 滚轮按**分贝数**步进（而不是按百分比）。
    ///
    /// 单独的开关，不跟「对数音量刻度」绑在一起：刻度只决定滑条怎么画，
    /// 一格滚轮改变多少才是用户真正在调的东西，两者要能分开选。
    pub scroll_wheel_in_db: bool,
    /// 一格滚轮的音量步进（dB，`scroll_wheel_in_db` 打开时生效）。
    ///
    /// 这个 dB 数是**真实的音量变化**：对数刻度下位置本来就按 dB 等分，
    /// 线性刻度下则要按当前位置算 —— 幅度翻倍才是 +6 dB，同样的 dB
    /// 在不同位置上对应的滑条行程并不一样。
    pub log_scroll_wheel_step: f32,

    /// 托盘图标样式（后续「更换托盘图标」功能的入口）。
    pub tray_icon_style: TrayIconStyle,

    /// 托盘图标的颜色（`#rrggbb`）。
    ///
    /// 空 = 跟随任务栏深浅：深色任务栏画浅墨、浅色任务栏画深墨（系统图标一贯的样子）。
    /// 填了就用这个色，深浅任务栏都不改 —— 单色图标才染得动，软件图标 / 自定义 ico 不受影响。
    #[serde(default)]
    pub tray_icon_color: String,
    /// 自定义托盘图标的路径（`.ico`）。样式选「自定义」时读它。
    #[serde(default)]
    pub tray_icon_custom: String,
    /// 彩色律动：图标颜色跟着响度走（安静偏冷、越响越红）。
    ///
    /// 和上面那个固定色是上下位关系：开着它时固定色不生效。
    /// 只作用于单色样式；软件图标 / 自定义 ico 是彩色原图，染色会毁掉原样。
    #[serde(default)]
    pub tray_color_by_level: bool,

    /// 动态模式（`TrayIconStyle::Dynamic`）画哪种律动。
    pub tray_motion: TrayMotion,
    /// 律动的灵敏度：把峰值电平放大多少（百分比，50..=300）。
    ///
    /// 峰值本身是个很窄的区间（流行乐多数时候在 0.1~0.4），不放大就只能看到
    /// 一点点抖动；放太大又会在安静段落一直贴着满格。
    pub tray_motion_gain: u32,

    /// 数字图标的字体族（空 = 用 `Segoe UI`）。
    ///
    /// 数字是按目标尺寸**原生光栅化**的（不是画一张大图再缩小），
    /// 所以换个字体立刻就是那个字体自己的字形，不会糊。
    pub tray_number_font: String,
    /// 托盘数字的抗锯齿方式（照 Photoshop 字符面板那七档）。
    #[serde(default)]
    pub tray_number_antialias: TrayAntialias,
    /// **老字段（≤ rev 66）**：数字墨迹占图标高度的百分比。
    ///
    /// 只是个迁移用的入口 —— 读进来换算成 `tray_digit_*` 的字号，之后不再写盘
    /// （`skip_serializing`），配置文件里会自然消失。
    #[serde(default, skip_serializing)]
    pub tray_number_size: u32,
    /// 一位数（0–9）的托盘数字外观。
    #[serde(default = "default_tray_digit_one")]
    pub tray_digit_one: TrayDigitMetrics,
    /// 两位数（10–99）的托盘数字外观。
    #[serde(default = "default_tray_digit_two")]
    pub tray_digit_two: TrayDigitMetrics,
    /// 三位数（100）的托盘数字外观。
    #[serde(default = "default_tray_digit_three")]
    pub tray_digit_three: TrayDigitMetrics,

    /// 应用图标从 ICO 里取哪一档（16 / 32 / 64），同时决定列表里的显示尺寸。
    pub app_icon_size: u32,

    /// 应用名首字母大写（`cloudmusic` → `Cloudmusic`）。
    ///
    /// 进程名原样是文件名，全小写的一堆（`chrome`、`cloudmusic`、`foobar2000`），
    /// 直接列出来像命令行而不像应用名。默认开；关掉就是文件名的原样。
    /// 只动第一个字符，其余保持原样（`foobar2000` 不会变成 `Foobar2000`）。
    #[serde(default = "default_true")]
    pub app_name_capitalize: bool,

    /// 面板弹出动画（从任务栏上沿向上展开）。
    pub popup_animation: bool,
    /// 弹出动画时长（毫秒）：越大越慢。只在 [`PopupAnimStyle::Unfold`] 下有用。
    pub popup_animation_ms: u32,
    /// 弹出方式：自己那套「从任务栏上沿展开」，还是照系统面板整块滑进来。
    #[serde(default)]
    pub popup_anim_style: PopupAnimStyle,
    /// 收起（消失）时也走一段动画：滑下去 / 淡出之后再藏。默认开。
    ///
    /// 「弹出动画」总闸关掉时这一项自然也不生效。
    #[serde(default = "default_true")]
    pub panel_close_animation: bool,

    /// 松开音量滑条时播放一声轻响。默认开：拖到满意的位置松手，有一声确认更踏实。
    pub release_sound: bool,

    /// 松手提示音也管屏幕提示（OSD）里那根滑条。默认开。
    ///
    /// OSD 常常是滚轮捎带着弹出来的，拖完松手同样需要那一声确认；但有人只想要
    /// 面板那一声（OSD 随手一拖就划走），所以挂在主开关下面单独给一个子开关。
    /// 主开关关掉时这一项没有意义，设置页里也不显示。
    #[serde(default = "default_true")]
    pub release_sound_osd: bool,

    /// 托盘右键菜单的布局（顺序 / 显隐 / 子菜单）。
    pub tray_menu: TrayMenuConfig,

    /// 托盘右键菜单的外观：跟随主题（自绘）/ Windows 原生。
    pub tray_menu_style: TrayMenuStyle,

    /// 托盘右键菜单的最大宽度（逻辑像素）。超出部分用 … 截断。
    ///
    /// 只对自绘菜单（`tray_menu_style = theme`）有效：原生菜单的宽度由系统按文字算，
    /// 改不了。
    pub tray_menu_max_width: u32,

    /// 音量条上的发声特效样式。
    pub activity_effect: ActivityEffect,

    /// 设备的自定义名字：填了之后面板 / 托盘 / 提示里只显示它，
    /// 系统原名只在设置页里露出来。
    pub device_names: Vec<DeviceAlias>,

    /// 显示器的自定义名字：填了之后亮度窗口和设置页都显示它（系统名留作副标题）。
    pub display_names: Vec<DisplayAlias>,

    /// 播放设备的自定义图标：填了之后面板 / 设置页 / 托盘菜单都用它，
    /// 没填就按端点的 FormFactor 猜（音箱 / 耳机 / 耳麦 / 显示器…）。
    pub device_icons: Vec<DeviceIconAlias>,

    /// 设备最大音量限制。
    pub volume_cap: VolumeCap,

    /// 调音量时解除静音（方向 × 操作方式，每种调法各调各的）。
    pub unmute: UnmuteOnChange,

    /// 调音量时的音效（每变化一档响一下，范围按手势勾）。
    #[serde(default)]
    pub volume_sound: VolumeSoundConfig,

    /// 托盘悬浮提示的自定义内容。
    pub tooltip: TooltipConfig,

    /// 显示器亮度调节（DDC/CI），默认关闭。
    pub brightness: BrightnessConfig,

    /// 屏幕提示条（OSD）：调音量 / 静音 / 切歌时在屏幕上闪一下。
    pub osd: OsdConfig,

    /// 黑名单：这些应用在前台时 Bolume 让路（含「全屏状态禁用」）。
    #[serde(default)]
    pub app_block: AppBlockConfig,

    /// 启动时自动套用的设备 / 音量状态。
    pub startup: StartupProfile,

    /// 应用列表排序：0 = 按名称，1 = 按最近发声。
    pub app_sort: u32,

    /// 设置窗口尺寸（内容区，逻辑像素）：拖动窗口后自动记住。
    pub settings_width: f32,
    pub settings_height: f32,

    /// OSD 布局编辑器的画布底色（`#rrggbb`）。
    ///
    /// 只在编辑器里当「桌面」用：同一张卡片摆在深底 / 浅底上好不好看是两回事，
    /// 右边那排色块只能看到颜色本身，看不出对比。它不影响提示条的任何外观。
    pub editor_bg: String,

    /// OSD 布局编辑器右侧参数栏宽度（逻辑像素）：拖着左边缘改。
    pub editor_panel_w: f32,

    /// 音量面板宽度（逻辑像素）。
    ///
    /// 面板里所有行的宽度都跟着它走，所以这个数字一改整块就宽了 / 窄了；
    /// 亮度面板默认也取同一个宽度（`brightness.width = 0` = 跟随它）。
    pub flyout_width: u32,

    /// 面板里音量条的粗细（逻辑像素）。
    ///
    /// 只管**面板**（音量 / 亮度两块），屏幕提示条（OSD）的条高归布局编辑器管。
    pub panel_bar_thickness: u32,

    /// 面板里音量条的圆角，存「半高的百分之几」：100 = 全圆头（默认），0 = 方角。
    ///
    /// 和 OSD 的 `bar_round` 一个口径：存百分比而不是像素，改粗细时圆角跟着一起缩放，
    /// 才不会出现「条变粗了圆角却还是老样子」的方块感。
    pub panel_bar_round: u32,

    /// 音量面板排版：横向（一路一行）/ 竖向（一路一条推子）。
    pub panel_layout: PanelLayout,

    /// 竖向排版时要不要在推子下面写应用名。
    ///
    /// 默认**关**：竖着一列只有四十来像素宽，写名字只能写三四个字，
    /// 还不如只留图标认脸。
    pub vertical_app_names: bool,

    /// 竖向排版的面板高度（逻辑像素）。**0 = 自动**（按内容算，默认）。
    ///
    /// 显式写数字就按它来，推子把多出来的高度吃掉 —— 调音台那种长推子。
    pub vertical_height: u32,

    /// 面板页脚那一排按钮：放哪些、靠哪一边、谁在前谁在后。
    ///
    /// 见 [`FOOTER_ITEMS`] 那张注册表 —— 以后加按钮不用改这里。
    #[serde(default)]
    pub footer: FooterConfig,

    /// 打开 / 收起音量面板。每个动作可录三组。
    pub hotkey_flyout: HotkeySlots,
    /// 打开设置窗口。
    pub hotkey_settings: HotkeySlots,
    /// 设备音量增加。
    pub hotkey_volume_up: HotkeySlots,
    /// 设备音量减少。
    pub hotkey_volume_down: HotkeySlots,
    /// 当前应用音量增加。
    pub hotkey_app_volume_up: HotkeySlots,
    /// 当前应用音量减少。
    pub hotkey_app_volume_down: HotkeySlots,

    /// 快捷键调整**设备**音量的步进（百分比；`hotkey_volume_in_db` 打开时按 dB）。
    pub hotkey_volume_step: u32,
    /// 快捷键调音量时跟随对数刻度（对数刻度下每格固定 dB，而不是固定百分比）。
    pub hotkey_volume_follow_log: bool,
    /// 快捷键步进固定按 dB 算（与刻度画法无关，即使线性刻度也按 dB 走）。
    pub hotkey_volume_in_db: bool,

    /// 快捷键调整**当前应用**音量的步进（对应关系与设备那组完全一样）。
    pub hotkey_app_volume_step: u32,
    /// 应用音量步进跟随对数刻度。
    pub hotkey_app_volume_follow_log: bool,
    /// 应用音量步进固定按 dB 算。
    pub hotkey_app_volume_in_db: bool,

    /// 媒体键动作：上一曲 / 下一曲 / 播放暂停 / 静音 / 停止。
    pub hotkey_media_prev: HotkeySlots,
    pub hotkey_media_next: HotkeySlots,
    pub hotkey_media_play_pause: HotkeySlots,
    pub hotkey_media_mute: HotkeySlots,
    pub hotkey_media_stop: HotkeySlots,

    /// 亮度调节：当前（鼠标所在屏）/ 全部 / 指定编号。
    ///
    /// 三组都只动 DDC 亮度，步进沿用 `brightness.step`，指定编号读
    /// `brightness.hotkey_monitor`。
    pub hotkey_brightness_up: HotkeySlots,
    pub hotkey_brightness_down: HotkeySlots,
    pub hotkey_brightness_all_up: HotkeySlots,
    pub hotkey_brightness_all_down: HotkeySlots,
    pub hotkey_brightness_monitor_up: HotkeySlots,
    pub hotkey_brightness_monitor_down: HotkeySlots,

    /// 按住音量 / 亮度 / 切歌快捷键时连续触发。
    ///
    /// 低级钩子吞掉按键之后系统自己的「按住重复」不会再来，必须自己做。
    /// 这是**总闸**：关掉之后一个键都不连发，具体哪个键参与由
    /// `hotkey_repeat_actions` 决定（设置页里每个快捷键前面那个勾）。
    pub hotkey_hold_repeat: bool,
    /// 连发间隔（毫秒）。
    pub hotkey_repeat_interval: u32,

    /// 按住多久才开始连发（毫秒，0–9999，默认 0）。
    ///
    /// 就是系统那套「键盘字符重复延迟」：按下之后先不动，按住超过这么久才开始
    /// 按 `hotkey_repeat_interval` 一档档往下走。0 = 不等（和以前一样，按下就进连发节奏）。
    /// 只在 `hotkey_hold_repeat` 开着、且这个动作勾了连发时才有意义。
    #[serde(default)]
    pub hotkey_repeat_delay: u32,

    /// 「按住连续触发」在哪些动作上生效：动作名，见 [`crate::platform::hotkey::Action::key`]。
    ///
    /// 默认是音量四个 + 亮度六个（[`DEFAULT_REPEAT_ACTIONS`]）。存成名字而不是位图，
    /// 是为了以后加动作时老配置不至于错位；缺这个键的老配置按默认那十个算。
    #[serde(default = "default_repeat_actions")]
    pub hotkey_repeat_actions: Vec<String>,

    /// 「按住连续触发」时不理会系统自己的长按重复（默认开）。
    ///
    /// 按住不放时 Windows 会按「键盘重复延迟 / 重复速率」（控制面板里那两条滑条）
    /// 一直补 keydown；我们自己也按 `hotkey_repeat_interval` 补。两套节奏叠在一起，
    /// 步进忽快忽慢，而且第一档之后会先等一段系统的重复延迟。打开这一项就把系统
    /// 那一路彻底关掉（`RegisterHotKey` 走 `MOD_NOREPEAT`、钩子里的重复直接吞掉），
    /// 节奏只看上面那个间隔。
    ///
    /// 关掉它则退回混着走：按住时按系统的速率重复，`hotkey_repeat_interval` 只当
    /// 一道额外的节拍。只在 `hotkey_hold_repeat` 开着时才谈得上（关掉连发时这一项不生效）。
    #[serde(default = "default_true")]
    pub hotkey_ignore_system_repeat: bool,

    /// NumLock 关着的时候也把小键盘当数字看（Num0..Num9 / Num.）。
    ///
    /// 系统这会儿发的是方向键 / Ins / Del，低级钩子按扫描码把它们认回来再匹配：
    /// 录过 `Num4` 之后，NumLock 开不开都能触发。默认**关** —— 关着才和 Windows
    /// 一致（小键盘就是方向键）；打开后没配成快捷键的键照旧放过去，
    /// 别的程序里还是方向键。
    pub hotkey_numpad_raw: bool,

    /// 配色跟着 Windows 的亮暗走（默认开）。
    ///
    /// 打开后：系统亮色用浅色模板、深色用深色模板，系统一改界面立刻跟着换；
    /// 关掉就是固定用深色模板（底色仍可以自己挑）。
    pub theme_follow_system: bool,
    /// 跟随系统时「亮色」那一档的底色（默认倒数第一个浅色）。
    ///
    /// 亮暗两档各存一套：跟随时系统一换就整套配色换掉，两档共用一份底色会
    /// 让两档长得一模一样（换了跟没换一样），自定义也就失去意义了。
    pub theme_light_surface: String,
    /// 跟随系统时「亮色」那一档的强调色（空 = 跟随 Windows 的强调色）。
    pub theme_light_accent: String,
    /// 跟随系统时「深色」那一档的底色（默认第四个深色）。
    pub theme_dark_surface: String,
    /// 跟随系统时「深色」那一档的强调色（空 = 跟随 Windows 的强调色）。
    pub theme_dark_accent: String,
    /// 固定模式下用的底色（空 = 深色模板自己的底色）。
    ///
    /// 换底色会连带换掉文字和描边的明暗（浅底配黑字），否则会出现灰底白字看不清。
    pub theme_surface: String,
    /// 自选强调色（空 = 跟随 Windows 的强调色）。选中态、滑条、角标都用它。
    pub theme_accent: String,
    /// 设置窗口的字号缩放（百分比，100 = 原始字号）。
    pub ui_font_scale: u32,
    /// 界面不透明度（百分比，100 = 完全不透明）。
    pub ui_opacity: u32,
    /// 界面字体族（空 = 跟随系统默认字体）。
    ///
    /// 和托盘数字的字体分开存：那个只影响托盘上那几个数字，这个管整个设置窗。
    pub ui_font: String,
    /// 界面字重（100..900，400 = 常规）。
    ///
    /// 只影响设置窗自己的文字（托盘数字从 rev 67 起没有字重设置，笔画固定 600）。
    pub ui_font_weight: u32,
    /// 界面对比度（百分比，100 = 主题原样）。
    ///
    /// 只动次级文字 / 描边 / 悬停层的浓度和主文字的柔和度，不改底色和强调色：
    /// 亮度对比拉开一点，深色主题在小屏幕上才看得清。
    pub ui_contrast: u32,

    /// 界面动效（默认开）。
    ///
    /// 管的是**设置页切换**：换左栏分页、换页内页签时，新内容淡入并轻微上浮。
    /// 关掉就一帧到位。面板 / 提示条那套淡入是 `popup_animation`，跟这个分开。
    pub ui_motion: bool,
    /// 音量面板的字体族（空 = 跟随系统默认字体）。
    ///
    /// 和 `ui_font` 分开存：设置窗和面板是两个窗口，想各自用不同字体也合理。
    pub panel_font: String,
    /// 音量面板的字号（逻辑像素）。
    ///
    /// 面板里跟字走的尺寸（行高、面板宽度、竖排段高）都按
    /// `panel_font_size / PANEL_BASE_FONT_SIZE` 缩放 —— 只放大字会把行挤成一团。
    pub panel_font_size: f32,
    /// 音量面板的字间距（逻辑像素，负数 = 紧排）。
    ///
    /// 挂在面板根节点的文本样式上，整棵树继承（`letter_spacing` 属于文本样式，
    /// 子节点只覆盖字号，不会把它盖掉）。
    pub panel_letter_spacing: f32,
    /// 音量面板里每行的高度（逻辑像素，0 = 跟着字号自动算）。
    ///
    /// 填了就**以它为准**、不再乘字号：字放大而想把行留得紧，就靠这一项。
    pub panel_line_height: f32,
    /// 音量面板的字重（100..900，400 = 常规）。
    pub panel_font_weight: u32,
    /// 面板里设备名称的字号（逻辑像素）。
    pub device_name_size: u32,

    /// 被右键「折叠此应用」的会话 key（`exe:<路径>` 或 `pid:<pid>`）。
    ///
    /// 存 key 而不是进程名：key 对普通应用就是 exe 全路径（小写），
    /// 进程重启后依然稳定，不会因为重新打开而重新出现。
    pub hidden_apps: Vec<String>,

    /// 应用的自定义名字：填了之后面板里只显示它，系统名只在悬停提示里兜底。
    ///
    /// 和 `hidden_apps` 一样按会话 key（`exe:<路径>`）存，重启不丢。
    pub app_names: Vec<AppAlias>,

    /// 响度均衡的全局参数。
    pub loudness: LoudnessConfig,

    /// 单个应用的响度均衡开关（覆盖全局）。
    ///
    /// 只有「跟全局不一样」的应用才会在这里留一条；没有条目 = 跟随全局。
    pub app_loudness: Vec<AppLoudness>,

    /// 配置格式版本：用来判断要不要把老配置的默认值升级到新默认。
    ///
    /// 必须单独标 `#[serde(default)]`：容器级的 default 会拿 `Config::default()`
    /// 里的版本号去补，老配置会被当成新配置，迁移就永远不触发。
    #[serde(default)]
    pub config_rev: u32,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            language: Lang::Zh,
            use_logarithmic_volume: false,
            logarithmic_min_db: -40,
            hide_on_focus_lost: true,
            launch_at_login: false,
            launch_method: LaunchMethod::default(),
            run_as_admin: false,
            show_system_sounds: true,
            sounding_apps_first: false,
            use_scroll_wheel_in_tray: true,
            use_global_mouse_wheel_hook: true,
            use_scroll_wheel_in_apps: true,
            scroll_wheel_step: 2,
            // 默认按百分比走，和「对数刻度默认关闭」是一致的一套。
            scroll_wheel_in_db: false,
            // 与改动前的行为一致：旧版把 `scroll_wheel_step` 在对数模式下当 dB 用，
            // 默认 2 就是「一格 2 dB」。老配置没有这个字段，读进来也落在这里。
            log_scroll_wheel_step: DEFAULT_LOG_SCROLL_STEP_DB,
            tray_icon_style: TrayIconStyle::Arcs,
            tray_icon_color: String::new(),
            tray_icon_custom: String::new(),
            tray_color_by_level: false,
            tray_motion: TrayMotion::default(),
            tray_motion_gain: DEFAULT_TRAY_MOTION_GAIN,
            tray_number_font: DEFAULT_NUMBER_FONT.to_string(),
            tray_number_antialias: TrayAntialias::default(),
            tray_number_size: 0,
            tray_digit_one: default_tray_digit_one(),
            tray_digit_two: default_tray_digit_two(),
            tray_digit_three: default_tray_digit_three(),
            app_icon_size: 32,
            app_name_capitalize: true,
            popup_animation: true,
            popup_animation_ms: DEFAULT_POPUP_ANIMATION_MS,
            popup_anim_style: PopupAnimStyle::default(),
            panel_close_animation: true,
            release_sound: true,
            release_sound_osd: true,
            tray_menu: TrayMenuConfig::default(),
            tray_menu_style: TrayMenuStyle::default(),
            tray_menu_max_width: DEFAULT_TRAY_MENU_MAX_WIDTH,
            activity_effect: ActivityEffect::Level,
            device_names: Vec::new(),
            display_names: Vec::new(),
            device_icons: Vec::new(),
            volume_cap: VolumeCap::default(),
            unmute: UnmuteOnChange::default(),
            volume_sound: VolumeSoundConfig::default(),
            tooltip: TooltipConfig::default(),
            brightness: BrightnessConfig::default(),
            osd: OsdConfig::default(),
            app_block: AppBlockConfig::default(),
            startup: StartupProfile::default(),
            app_sort: 0,
            settings_width: 860.0,
            settings_height: 620.0,
            // 画布默认一块纯黑的桌面（就是那排按钮里的「黑」）。
            editor_bg: "#000000".to_string(),
            editor_panel_w: 268.0,
            flyout_width: DEFAULT_FLYOUT_WIDTH,
            panel_bar_thickness: DEFAULT_PANEL_BAR_THICKNESS,
            panel_bar_round: DEFAULT_PANEL_BAR_ROUND,
            panel_layout: PanelLayout::Horizontal,
            vertical_app_names: false,
            vertical_height: 0,
            footer: FooterConfig::default(),
            // 快捷键默认全部为空，由用户自己录。
            hotkey_flyout: HotkeySlots::default(),
            hotkey_settings: HotkeySlots::default(),
            hotkey_volume_up: HotkeySlots::default(),
            hotkey_volume_down: HotkeySlots::default(),
            hotkey_app_volume_up: HotkeySlots::default(),
            hotkey_app_volume_down: HotkeySlots::default(),
            hotkey_volume_step: 2,
            hotkey_volume_follow_log: false,
            hotkey_volume_in_db: false,
            hotkey_app_volume_step: 2,
            hotkey_app_volume_follow_log: false,
            hotkey_app_volume_in_db: false,
            hotkey_media_prev: HotkeySlots::default(),
            hotkey_media_next: HotkeySlots::default(),
            hotkey_media_play_pause: HotkeySlots::default(),
            hotkey_media_mute: HotkeySlots::default(),
            hotkey_media_stop: HotkeySlots::default(),
            hotkey_brightness_up: HotkeySlots::default(),
            hotkey_brightness_down: HotkeySlots::default(),
            hotkey_brightness_all_up: HotkeySlots::default(),
            hotkey_brightness_all_down: HotkeySlots::default(),
            hotkey_brightness_monitor_up: HotkeySlots::default(),
            hotkey_brightness_monitor_down: HotkeySlots::default(),
            hotkey_hold_repeat: true,
            hotkey_repeat_interval: DEFAULT_HOTKEY_REPEAT_INTERVAL,
            hotkey_repeat_delay: 0,
            hotkey_repeat_actions: default_repeat_actions(),
            hotkey_ignore_system_repeat: true,
            hotkey_numpad_raw: false,
            theme_follow_system: true,
            theme_light_surface: DEFAULT_LIGHT_SURFACE.to_string(),
            theme_light_accent: String::new(),
            theme_dark_surface: DEFAULT_DARK_SURFACE.to_string(),
            theme_dark_accent: String::new(),
            theme_surface: String::new(),
            theme_accent: String::new(),
            ui_font_scale: 100,
            ui_opacity: 100,
            ui_font: UI_FONT_DEFAULT.to_string(),
            ui_font_weight: DEFAULT_UI_FONT_WEIGHT,
            ui_contrast: DEFAULT_UI_CONTRAST,
            ui_motion: true,
            panel_font: UI_FONT_DEFAULT.to_string(),
            panel_font_size: PANEL_BASE_FONT_SIZE,
            panel_letter_spacing: 0.0,
            panel_line_height: 0.0,
            panel_font_weight: DEFAULT_UI_FONT_WEIGHT,
            device_name_size: 13,
            hidden_apps: Vec::new(),
            app_names: Vec::new(),
            loudness: LoudnessConfig::default(),
            app_loudness: Vec::new(),
            config_rev: CONFIG_REV,
        }
    }
}

/// 快捷键步进作用在哪一路音量上。
///
/// 设备主音量与「当前应用」各有一套步进（数值 + 跟随对数刻度 + 固定 dB），
/// 互不影响：给应用调得细一点、给设备调得粗一点是常见需求。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum HotkeyStepTarget {
    /// 设备主音量。
    Device,
    /// 当前应用（正在发声的那个）。
    App,
}

impl Config {
    /// 音量面板宽度（逻辑像素）：配置里的数字钳进可调范围。
    pub fn flyout_width(&self) -> f32 {
        self.flyout_width
            .clamp(FLYOUT_WIDTH_RANGE.0, FLYOUT_WIDTH_RANGE.1) as f32
    }

    /// 面板里音量条的粗细（逻辑像素）：钳进可调范围。
    pub fn panel_bar_thickness(&self) -> f32 {
        self.panel_bar_thickness
            .clamp(PANEL_BAR_THICKNESS_RANGE.0, PANEL_BAR_THICKNESS_RANGE.1)
            as f32
    }

    /// 面板里音量条的圆角（逻辑像素）：按「半高 × 百分比」算，不会超过半高。
    pub fn panel_bar_corner(&self) -> f32 {
        self.panel_bar_thickness() * 0.5 * (self.panel_bar_round.min(100) as f32 / 100.0)
    }

    /// 亮度面板宽度（逻辑像素）：默认跟随音量面板（`brightness.width = 0`）。
    pub fn brightness_width(&self) -> f32 {
        self.brightness.panel_width(self.flyout_width())
    }

    /// 音量面板竖向排版时的高度。`None` = 自动（按内容算）。
    pub fn vertical_panel_height(&self) -> Option<f32> {
        vertical_height_value(self.vertical_height)
    }

    /// 这台设备有没有自定义名字（空字符串不算）。
    pub fn device_alias(&self, id: &str) -> Option<&str> {
        self.device_names
            .iter()
            .find(|alias| alias.id == id)
            .map(|alias| alias.name.as_str())
            .map(str::trim)
            .filter(|name| !name.is_empty())
    }

    /// 设备该显示的名字：有自定义就用自定义，否则用系统名。
    ///
    /// 面板 / 托盘 / 悬浮提示都走这里 —— 「自定义之后只显示自定义名」是全局的，
    /// 只有一个地方例外：设置页的设备行会把系统原名当副标题露出来。
    pub fn device_label(&self, id: &str, name: &str) -> String {
        self.device_alias(id).unwrap_or(name).to_string()
    }

    /// 设自定义名字；传空（或全空格）= 恢复系统原名。返回有没有改动。
    pub fn set_device_alias(&mut self, id: &str, name: &str) -> bool {
        let name = name.trim();
        let existing = self.device_names.iter().position(|alias| alias.id == id);
        if name.is_empty() {
            if let Some(index) = existing {
                self.device_names.remove(index);
                return true;
            }
            return false;
        }
        match existing {
            Some(index) => {
                if self.device_names[index].name == name {
                    return false;
                }
                self.device_names[index].name = name.to_string();
            }
            None => self.device_names.push(DeviceAlias {
                id: id.to_string(),
                name: name.to_string(),
            }),
        }
        true
    }

    /// 这个应用有没有自定义名字（空字符串不算）。
    pub fn app_alias(&self, key: &str) -> Option<&str> {
        self.app_names
            .iter()
            .find(|alias| alias.key == key)
            .map(|alias| alias.name.as_str())
            .map(str::trim)
            .filter(|name| !name.is_empty())
    }

    /// 应用在面板里该显示的名字：有自定义就用自定义，否则用进程名。
    pub fn app_label(&self, key: &str, name: &str) -> String {
        self.app_alias(key).unwrap_or(name).to_string()
    }

    /// 设应用的自定义名字；传空（或全空格）= 恢复进程名。返回有没有改动。
    pub fn set_app_alias(&mut self, key: &str, name: &str) -> bool {
        let name = name.trim();
        let existing = self.app_names.iter().position(|alias| alias.key == key);
        if name.is_empty() {
            if let Some(index) = existing {
                self.app_names.remove(index);
                return true;
            }
            return false;
        }
        match existing {
            Some(index) => {
                if self.app_names[index].name == name {
                    return false;
                }
                self.app_names[index].name = name.to_string();
            }
            None => self.app_names.push(AppAlias {
                key: key.to_string(),
                name: name.to_string(),
            }),
        }
        true
    }

    /// 这个应用要不要做响度均衡：单应用覆盖优先，没有覆盖就跟全局开关。
    pub fn loudness_for(&self, key: &str) -> bool {
        self.app_loudness
            .iter()
            .find(|entry| entry.key == key)
            .map(|entry| entry.enabled)
            .unwrap_or(self.loudness.enabled)
    }

    /// 这个应用的响度均衡**单独设置**成了什么（`None` = 跟着全局，没留条目）。
    ///
    /// 面板上的角标问的是「它是不是被单独开了」—— 全局开着时每个应用都在均衡，
    /// 那种情况不该在每一个图标上打标；只有覆盖成「开」的才值得标一下。
    pub fn app_loudness_override(&self, key: &str) -> Option<bool> {
        self.app_loudness
            .iter()
            .find(|entry| entry.key == key)
            .map(|entry| entry.enabled)
    }

    /// 设单个应用的响度均衡开关。传 `None` = 删掉覆盖、回到跟随全局。
    ///
    /// 返回有没有改动（跟 `set_app_alias` 一样的约定，方便调用方决定要不要落盘）。
    pub fn set_app_loudness(&mut self, key: &str, enabled: Option<bool>) -> bool {
        let existing = self.app_loudness.iter().position(|entry| entry.key == key);
        match enabled {
            None => match existing {
                Some(index) => {
                    self.app_loudness.remove(index);
                    true
                }
                None => false,
            },
            Some(enabled) => {
                // 和全局一致就别留条目了：留着的话以后改全局开关它不跟着走，
                // 用户会觉得「怎么这个应用不听话」。
                if enabled == self.loudness.enabled {
                    return match existing {
                        Some(index) => {
                            self.app_loudness.remove(index);
                            true
                        }
                        None => false,
                    };
                }
                match existing {
                    Some(index) => {
                        if self.app_loudness[index].enabled == enabled {
                            return false;
                        }
                        self.app_loudness[index].enabled = enabled;
                    }
                    None => self.app_loudness.push(AppLoudness {
                        key: key.to_string(),
                        enabled,
                    }),
                }
                true
            }
        }
    }

    /// 这台设备用的自定义图标名（没有就是 `None`，交给设备类型去猜）。
    pub fn device_icon(&self, id: &str) -> Option<&str> {        self.device_icons
            .iter()
            .find(|alias| alias.id == id)
            .map(|alias| alias.icon.as_str())
            .map(str::trim)
            .filter(|icon| !icon.is_empty())
    }

    /// 设自定义图标；传空 = 回到「跟随设备类型」。返回有没有改动。
    pub fn set_device_icon(&mut self, id: &str, icon: &str) -> bool {
        let icon = icon.trim();
        let existing = self.device_icons.iter().position(|alias| alias.id == id);
        if icon.is_empty() {
            if let Some(index) = existing {
                self.device_icons.remove(index);
                return true;
            }
            return false;
        }
        match existing {
            Some(index) => {
                if self.device_icons[index].icon == icon {
                    return false;
                }
                self.device_icons[index].icon = icon.to_string();
            }
            None => self.device_icons.push(DeviceIconAlias {
                id: id.to_string(),
                icon: icon.to_string(),
            }),
        }
        true
    }

    /// 显示器的自定义名字（没有就是 `None`）。
    pub fn display_alias(&self, key: &str) -> Option<&str> {
        self.display_names
            .iter()
            .find(|alias| alias.key == key)
            .map(|alias| alias.name.as_str())
            .map(str::trim)
            .filter(|name| !name.is_empty())
    }

    /// 显示器该显示的名字：有自定义就用自定义，否则用系统名。
    pub fn display_label(&self, key: &str, name: &str) -> String {
        self.display_alias(key).unwrap_or(name).to_string()
    }

    /// 设显示器的自定义名字；传空（或全空格）= 恢复系统原名。返回有没有改动。
    pub fn set_display_alias(&mut self, key: &str, name: &str) -> bool {
        let name = name.trim();
        let existing = self.display_names.iter().position(|alias| alias.key == key);
        if name.is_empty() {
            if let Some(index) = existing {
                self.display_names.remove(index);
                return true;
            }
            return false;
        }
        match existing {
            Some(index) => {
                if self.display_names[index].name == name {
                    return false;
                }
                self.display_names[index].name = name.to_string();
            }
            None => self.display_names.push(DisplayAlias {
                key: key.to_string(),
                name: name.to_string(),
            }),
        }
        true
    }

    /// 这一路快捷键步进的数值（百分比模式是百分点，dB 模式是 dB 数）。
    pub fn hotkey_step_value(&self, target: HotkeyStepTarget) -> u32 {
        match target {
            HotkeyStepTarget::Device => self.hotkey_volume_step,
            HotkeyStepTarget::App => self.hotkey_app_volume_step,
        }
        .max(1)
    }

    /// 这一路快捷键步进是否跟随对数刻度。
    pub fn hotkey_step_follow_log(&self, target: HotkeyStepTarget) -> bool {
        match target {
            HotkeyStepTarget::Device => self.hotkey_volume_follow_log,
            HotkeyStepTarget::App => self.hotkey_app_volume_follow_log,
        }
    }

    /// 这一路快捷键步进是否固定在 dB 模式。
    pub fn hotkey_step_in_db(&self, target: HotkeyStepTarget) -> bool {
        match target {
            HotkeyStepTarget::Device => self.hotkey_volume_in_db,
            HotkeyStepTarget::App => self.hotkey_app_volume_in_db,
        }
    }

    /// 这个步进数字当前代表 dB 还是百分比（设置页拿它决定数字框要不要写 `dB`）。
    pub fn hotkey_step_is_db(&self, target: HotkeyStepTarget) -> bool {
        self.hotkey_step_in_db(target)
            || (self.use_logarithmic_volume && self.hotkey_step_follow_log(target))
    }

    /// 快捷键走一格之后，滑条该落在哪（0..1）。
    ///
    /// 三种算法，优先级从高到低：
    /// - **固定 dB**：把步进数字当 dB，按真实幅度算，和刻度怎么画无关
    ///   （线性刻度上也是「大一档 X dB」，听起来响度变化一致）；
    /// - **跟随对数刻度**：只有对数刻度才有意义 —— 对数刻度里位置与 dB 是线性的，
    ///   一格就是 `step / |min_db|` 的位移（和 EarTrumpet 的 `Volume += step` 一致）；
    /// - **百分比**：位置直接加 `step%`。
    pub fn hotkey_step_display(
        &self,
        target: HotkeyStepTarget,
        display: f32,
        direction: f32,
    ) -> f32 {
        let display = display.clamp(0.0, 1.0);
        let step = self.hotkey_step_value(target) as f32;
        let min_db = self.logarithmic_min_db as f32;
        if self.hotkey_step_in_db(target) {
            let amplitude = if self.use_logarithmic_volume {
                crate::audio::to_log_volume(display, min_db)
            } else {
                display
            };
            // 滑条最底下按「刻度下限」起算：静音时幅度是 0，算出来是 -140 dB。
            let db = if amplitude <= 0.0005 {
                min_db
            } else {
                crate::audio::amplitude_to_db(amplitude)
            };
            let amplitude = crate::audio::db_to_amplitude((db + direction * step).min(0.0));
            return if self.use_logarithmic_volume {
                crate::audio::to_display_volume(amplitude, min_db)
            } else {
                amplitude.clamp(0.0, 1.0)
            };
        }
        if self.use_logarithmic_volume && self.hotkey_step_follow_log(target) {
            return (display + direction * step / min_db.abs().max(1.0)).clamp(0.0, 1.0);
        }
        (display + direction * step / 100.0).clamp(0.0, 1.0)
    }

    /// 亮度滚轮走 `notches` 格之后，亮度该落在哪（0..100 的百分点）。
    ///
    /// 照音量 `scroll_display` 的百分比分支：一格固定行程、边界截断。
    /// 亮度是 0..100 的线性量，没有「对数刻度 / dB 步进」那两种画法，
    /// 所以「一格走多少」就是用户填的那个百分比（`BrightnessConfig::scroll_step`）。
    pub fn brightness_scroll(&self, percent: f32, notches: f32) -> u32 {
        let step = self.brightness.scroll_step.clamp(1, 100) as f32;
        (percent + notches * step).round().clamp(0.0, 100.0) as u32
    }

    /// 滚轮走 `notches` 格之后，滑条该落在哪（0..1）。
    ///
    /// 两种步进方式，由 `scroll_wheel_in_db` 选：
    /// - 百分比：一格固定行程，`scroll_wheel_step` 个百分点；
    /// - 分贝数：一格固定 **dB**，跟滑条是线性还是对数刻度无关 ——
    ///   先把当前位置换算成真实幅度 / dB，加上步进，再换算回滑条位置。
    ///   线性刻度上位置与幅度成正比，同样的 dB 在不同位置对应的行程并不一样，
    ///   所以才要按当前位置算，而不是拿一个常数去乘。
    pub fn scroll_display(&self, display: f32, notches: f32) -> f32 {
        let display = display.clamp(0.0, 1.0);
        if !self.scroll_wheel_in_db {
            let step = self.scroll_wheel_step.max(1) as f32 / 100.0;
            return (display + notches * step).clamp(0.0, 1.0);
        }
        let min_db = self.logarithmic_min_db as f32;
        let amplitude = if self.use_logarithmic_volume {
            crate::audio::to_log_volume(display, min_db)
        } else {
            display
        };
        // 滑条最底下按「刻度下限」起算：静音时幅度是 0，算出来的 dB 是 -140，
        // 一格滚轮根本推不上来。
        let db = if amplitude <= 0.0005 {
            min_db
        } else {
            crate::audio::amplitude_to_db(amplitude)
        };
        let step = self.log_scroll_wheel_step.max(LOG_SCROLL_STEP_RANGE_DB.0);
        let target_db = (db + notches * step).min(0.0);
        let target = crate::audio::db_to_amplitude(target_db).clamp(0.0, 1.0);
        if self.use_logarithmic_volume {
            crate::audio::to_display_volume(target, min_db)
        } else {
            target
        }
    }
}

pub struct ConfigStore {
    path: PathBuf,
    config: Config,
}

impl ConfigStore {
    pub fn load(path: PathBuf) -> Self {
        let raw = std::fs::read_to_string(&path).ok();
        let existing = raw
            .as_deref()
            .and_then(|content| serde_json::from_str::<Config>(content).ok());
        let mut config = existing.clone().unwrap_or_default();
        let mut migrated = false;
        // rev 6 之前没有独立的对数滚轮步进：那会儿 `scroll_wheel_step` 在对数模式下
        // 就是 dB。老配置读进来沿用当时的数字，滚轮手感不变（只有新配置才用 2 dB 默认）。
        let legacy_log_step = raw.as_deref().and_then(|content| {
            let value = serde_json::from_str::<serde_json::Value>(content).ok()?;
            if value.get("log_scroll_wheel_step").is_some() {
                return None;
            }
            value
                .get("scroll_wheel_step")?
                .as_u64()
                .map(|step| step.max(1) as f32)
        });
        if let Some(step) = legacy_log_step {
            config.log_scroll_wheel_step = step;
            migrated = true;
        }
        // rev 28：亮度滚轮从「段数」改成「步进百分比」。
        // 段数越大每格越小，反直觉；老配置按 100 / 段数换算（除不尽向下取整、最低 1，
        // 和旧代码算出来的那个数完全一致，滚轮手感不变）。
        let legacy_brightness_segments = raw.as_deref().and_then(|content| {
            let value = serde_json::from_str::<serde_json::Value>(content).ok()?;
            let brightness = value.get("brightness")?;
            if brightness.get("scroll_step").is_some() {
                return None;
            }
            brightness
                .get("scroll_segments")?
                .as_u64()
                .map(|segments| segments.clamp(1, 100) as u32)
        });
        if let Some(segments) = legacy_brightness_segments {
            config.brightness.scroll_step = (100 / segments).max(1);
            migrated = true;
        }
        // 旧版的默认时长是 140ms，那会儿托盘图标一弹出就是一条深色细线。
        // 读到旧默认值说明用户没主动改过，跟着新默认一起升级到 400ms。
        if config.popup_animation_ms == LEGACY_POPUP_ANIMATION_MS {
            config.popup_animation_ms = DEFAULT_POPUP_ANIMATION_MS;
            migrated = true;
        }
        // rev 65：默认从 400ms 收到 200ms —— 400 那档在「淡入淡出」上肉眼可见地拖。
        // 同样只动「还是旧默认值」的配置，用户自己调过的原样保留。
        if config.config_rev < 65 && config.popup_animation_ms == LEGACY_POPUP_ANIMATION_MS_400 {
            config.popup_animation_ms = DEFAULT_POPUP_ANIMATION_MS;
            migrated = true;
        }
        // rev 2 改了两个默认值：对数刻度默认关闭、托盘图标默认用喇叭。
        // 老配置里写的是当时的默认值，一并升级过来，不然「默认」改了也看不出效果。
        if config.config_rev < 2 {
            config.use_logarithmic_volume = false;
            config.tray_icon_style = TrayIconStyle::Arcs;
            migrated = true;
        }
        // rev 3：设置页从双列长页换成了左栏分页，原来的宽窗已经没必要留着；
        // 比新默认更大的一律收回默认尺寸（自己调小的保持不动）。
        if config.config_rev < 3 {
            let (width, height) = (config.settings_width, config.settings_height);
            if width > 900.0 || height > 780.0 || width < 200.0 || height < 200.0 {
                config.settings_width = 860.0;
                config.settings_height = 620.0;
            }
            migrated = true;
        }
        if config.config_rev < CONFIG_REV {
            // rev 52：「重启」改成默认显示。rev 30 加它时默认是闭着眼睛的，老配置里
            // 多半还闭着 —— 只改默认值等于没改，用户升级完照样看不到。这里把已有的
            // 那几处眼睛全打开（用户之前主动开过的也是开着的，不冲突）。
            //
            // 补节点这一步也放这儿：迁移是从上往下跑的，比 rev 30 更老的配置还没走到
            // 下面那段 ensure_action，先补上（之后那段发现已存在就会跳过）。
            if config.config_rev < 52 {
                config.tray_menu.ensure_action("restart");
                config.tray_menu.set_action_visible("restart", true);
                // 「正在发声的应用排最前」改成默认关：老配置里存的是 rev 34 那个 true，
                // 只改 `Default` 的话读进来还是开着的，用户看不到任何变化。
                config.sounding_apps_first = false;
            }
            // rev 53：面板字号从「百分比」改成「字体字号（逻辑像素）」。
            //
            // 老配置存的是 80..150 的百分比，这里按 100% = 11.5px 换算，四舍五入到
            // 0.1px（数字框就是按十分之一存的），用户已经调好的大小一点不变。
            // 判断依据是「新字段在不在」而不是 rev：手改过配置的人可能 rev 已经是 53
            // 但还留着老的百分比。
            if config.config_rev < 53 {
                let legacy_scale = raw.as_deref().and_then(|content| {
                    let value = serde_json::from_str::<serde_json::Value>(content).ok()?;
                    if value.get("panel_font_size").is_some() {
                        return None;
                    }
                    value.get("panel_font_scale")?.as_u64()
                });
                if let Some(scale) = legacy_scale {
                    let size = PANEL_BASE_FONT_SIZE * scale as f32 / 100.0;
                    config.panel_font_size = (size * 10.0).round() / 10.0;
                }
            }
            // rev 54：分隔线从「渲染时自动补两条」改成菜单里的条目。
            //
            // 只改「原样没动过的老默认」那一份：默认布局换成新顺序 + 四条分隔线；
            // 用户自己排过的布局一个字都不动 —— 他们本来就没有那两条自动线，
            // 升级后一样不会有线，想加自己插。
            if config.config_rev < 54 {
                config.tray_menu.rebase_legacy_default();
            }
            // rev 55：设备图标（`device_icons`）是新字段，serde 默认空表 =
            // 「跟随设备类型」，老配置不需要搬任何东西，这里只留个记号。
            // rev 56：默认菜单里两条「声音设置」对调位置。
            //
            // 只认 rev 55 那份默认顺序：自己排过的布局不动（他们想要什么顺序，自己拖就行）。
            if config.config_rev < 56 {
                config.tray_menu.swap_sound_entries();
            }
            // rev 57：「应用名首字母大写」（`app_name_capitalize`）是新字段，
            // serde 默认就是开着 —— 老配置读进来立刻生效，不用搬数据。
            if config.config_rev < 58 {
                // rev 58：两个默认值反过来了。老配置里存的是旧默认（提示音关、解除静音开），
                // 不搬一下的话用户看到的还是老行为。
                config.release_sound = true;
                config.unmute.enabled = false;
            }
            // rev 59：外观模式默认改成「跟随系统」。
            //
            // 老配置里存的是旧默认（固定一套），不搬一下用户看不到新默认。
            // 两档槽位本来就各有一档默认主题（rev 46 起），空着也能落到那一档，
            // 所以这里只翻开关、不碰主题名。
            if config.config_rev < 59 {
                config.theme_follow_system = true;
            }
            // rev 60：松手提示音多了一个「OSD 里也响」的子开关。
            //
            // 老配置里根本没有这一项，取新默认（开）会让刚升级的人凭空多出
            // 一声 OSD 提示音；这里按主开关搬一次 —— 原来关着提示音的人不会
            // 因为升级反而响起来，开着的人则顺理成章延到 OSD。
            if config.config_rev < 60 {
                config.release_sound_osd = config.release_sound;
            }
            // rev 50：音量条自己那条「元素颜色」搬进「强调色」。
            //
            // 编辑器里每行末尾那个色块收掉了 —— 音量条那一格调的就是填充色，和「强调色」
            // 是同一个东西，留着两份只会互相盖（`part_color(Bar)` 优先级更高，用户在
            // 强调色那一行怎么点都不动）。老配置里设过的那条搬进 `colors.accent`
            // （accent 空着才搬），然后把这一条删掉。
            if config.config_rev < 50 {
                if let Some(index) = config
                    .osd
                    .part_colors
                    .iter()
                    .position(|entry| entry.part == OsdPart::Bar)
                {
                    let color = config.osd.part_colors.remove(index).color;
                    if config.osd.colors.accent.trim().is_empty() && !color.trim().is_empty() {
                        config.osd.colors.accent = color;
                    }
                }
            }
            // rev 48：跟随系统的亮 / 暗两档各有自己的自选色（rev 47 加的四个字段）。
            // 老配置只有一对全局自选色，打开「跟随系统」后那对会被整对忽略 ——
            // 界面悄悄回到主题原色。这里搬一份到档位里，观感不变；搬完档位就不是空的，
            // 用户之后主动清空也不会被搬回来（迁移只按 rev 走一次）。
            //
            // 底色按明暗分档搬：浅底只进亮档、深底只进暗档 —— 反着搬会让暗主题套上
            // 一个浅底、字看不见。强调色不挑明暗，两档都搬。
            //
            // rev 72 之后四档都有默认底色了，不能再拿「四格都空」当开关
            // （反序列化时缺字段就是新默认），rev 号本身已经能说明「那四个字段不存在」。
            if config.config_rev < 48 && config.theme_follow_system {
                let accent = config.theme_accent.trim().to_string();
                if !accent.is_empty() {
                    config.theme_light_accent = accent.clone();
                    config.theme_dark_accent = accent;
                }
                let surface = config.theme_surface.trim().to_string();
                if !surface.is_empty() {
                    let light = crate::ui::theme::parse_color(&surface)
                        .map(|rgba| {
                            let r = ((rgba >> 24) & 0xFF) as f32;
                            let g = ((rgba >> 16) & 0xFF) as f32;
                            let b = ((rgba >> 8) & 0xFF) as f32;
                            0.299 * r + 0.587 * g + 0.114 * b > 127.0
                        })
                        .unwrap_or(false);
                    if light {
                        config.theme_light_surface = surface;
                    } else {
                        config.theme_dark_surface = surface;
                    }
                }
            }
            // rev 72：主题预设删掉之后，亮暗两档的底色成了决定外观的那一项。
            //
            // 老配置里这两格多半是空的（以前空着 = 用那套主题自己的底色），
            // 现在空着就等于亮色档落到深色模板上 —— 填成新默认（浅色档倒数第一个、
            // 深色档第四个）。迁移只按 rev 走一次，之后用户自己清空不会被填回来。
            if config.config_rev < 72 {
                if config.theme_light_surface.trim().is_empty() {
                    config.theme_light_surface = DEFAULT_LIGHT_SURFACE.to_string();
                }
                if config.theme_dark_surface.trim().is_empty() {
                    config.theme_dark_surface = DEFAULT_DARK_SURFACE.to_string();
                }
            }
            // rev 29：黑名单落地。原来 OSD 有「全屏时不弹」、快捷键页有「全屏时禁用快捷键」，
            // 现在是黑名单上的一个「全屏状态禁用」—— 两处任一开着就认为用户想要全屏让路。
            if config.config_rev < 29 {
                let legacy = raw.as_deref().and_then(|content| {
                    let value = serde_json::from_str::<serde_json::Value>(content).ok()?;
                    let osd = value
                        .get("osd")
                        .and_then(|osd| osd.get("disable_fullscreen"))
                        .and_then(|flag| flag.as_bool())
                        .unwrap_or(false);
                    let hotkey = value
                        .get("hotkey_disable_fullscreen")
                        .and_then(|flag| flag.as_bool())
                        .unwrap_or(false);
                    Some(osd || hotkey)
                });
                if legacy == Some(true) {
                    config.app_block.fullscreen = true;
                }
            }
            // rev 32（及以前）的托盘数字字重在 rev 67 之后不再是设置项，那条迁移随之作废。
            // rev 31：托盘右键菜单多了一套自绘外观（`tray_menu_style` / `tray_menu_max_width`），
            // 两个字段都有 serde 默认值，老配置读进来就是「跟随主题 + 420px」，不用搬数据。
            // rev 30：托盘右键多了「重启」。老配置的菜单树里没有这一项的话补一个
            // （不补的话老配置永远没有它）。默认显隐由 rev 52 那段负责，这里只管「有没有」。
            if config.config_rev < 30 {
                config.tray_menu.ensure_action("restart");
            }
            // rev 9：滚轮步进从「跟随对数刻度」拆成独立开关。老配置里对数刻度开着，
            // 说明那时候一格是按 dB 走的，把它原样搬到开关上，手感不变。
            if config.config_rev < 9 && config.use_logarithmic_volume {
                config.scroll_wheel_in_db = true;
            }
            // rev 10：托盘数字从「像素高度」改成「占可画区的百分比」。
            // 老值按当前画布可画区（画布减 1px 边）换算，观感不变（0 是「自动」，不动）。
            if config.config_rev < 10 && config.tray_number_size > 0 {
                let box_side = (crate::tray::canvas_size() - 2.0).max(4.0);
                config.tray_number_size = ((config.tray_number_size as f32 / box_side) * 100.0)
                    .round()
                    .clamp(1.0, 100.0) as u32;
            }
            // rev 67：托盘数字重做成「字号 / 宽度 / 高度 / 字距」，按位数分三档。
            // 老配置只有一个「占图标高度的百分比」：按当时的墨迹高度反推一个字号
            // （`墨迹 ÷ 0.72` 就是字号像素，再按 DPI 换成点），三档都填同一个值 ——
            // 调过数字大小的人升级后看到的还是那个大小。0 是「自动」，保持自动。
            if config.config_rev < 67 && config.tray_number_size > 0 {
                let box_side = (crate::tray::canvas_size() - 2.0).max(4.0);
                let ink_px = box_side * config.tray_number_size.min(100) as f32 / 100.0;
                let font_px = ink_px / 0.72;
                let pt = (font_px * 72.0 / crate::tray::system_dpi() as f32)
                    .round()
                    .clamp(1.0, 200.0) as u32;
                for metrics in [
                    &mut config.tray_digit_one,
                    &mut config.tray_digit_two,
                    &mut config.tray_digit_three,
                ] {
                    metrics.size_pt = pt;
                }
            }
            // rev 11：新增「显示器亮度」段，默认关闭（老配置读进来就是关着的）。
            // rev 12：亮度段里多了「悬停滚轮」开关与段数，老配置走默认（开、50 段）。
            // rev 13：新增「调音量时解除静音」，老配置走默认（开，六种组合全开）。
            // rev 14：解除静音多了「拖动滑条」两条，老配置走默认（开）。
            // rev 15：快捷键多了「当前应用」音量键与自己那套步进，设备步进多了固定 dB 开关。
            // rev 16：设备开关名单从「一个开关 + 名单」换成三选一规则（手动 / 黑名单 / 白名单）。
            // 老配置里那个开关是开着的，语义正好等于现在的黑名单模式。
            if config.config_rev < 16 {
                let legacy_on = raw.as_deref().and_then(|content| {
                    let value = serde_json::from_str::<serde_json::Value>(content).ok()?;
                    value.get("startup")?.get("apply_device_states")?.as_bool()
                });
                if legacy_on == Some(true) {
                    config.startup.device_state_mode = DeviceStateMode::Blacklist;
                }
            }
            // rev 17：设备开关规则改成四选一，并去掉名单里的「启用 / 禁用」按钮带来的歧义：
            // 旧的「手动」（什么都不做）改名「禁用」；旧的「黑名单」（逐台显式开关）改名「手动」；
            // 黑名单 / 白名单现在只看**成员资格**（在名单里 = 黑 / 白）。
            if config.config_rev < 17 {
                config.startup.device_state_mode = match config.startup.device_state_mode {
                    DeviceStateMode::Manual => DeviceStateMode::Disabled,
                    DeviceStateMode::Blacklist => DeviceStateMode::Manual,
                    // 老白名单：只有 enabled 的条目是白的，把多余的条目丢掉。
                    DeviceStateMode::Whitelist => {
                        config.startup.device_states.retain(|state| state.enabled);
                        DeviceStateMode::Whitelist
                    }
                    DeviceStateMode::Disabled => DeviceStateMode::Disabled,
                };
            }
            // rev 20：新增「设备自定义名字」，老配置读进来就是空表（全用系统名）。
            // rev 21：新增屏幕提示条（OSD）与「全屏时禁用快捷键」。老配置读进来 OSD 是开着的
            // （新功能直接可用），全屏禁用是关着的（保持原来的手感）。
            // rev 27：OSD 卡片支持自定义配色（背景 / 文字 / 强调）。
            // rev 26：媒体键模板有了自己的一套排布（`media_layout`），元素还能各自选左/中/右对齐。
            // rev 25：OSD 新增「接管系统媒体键」（没配快捷键时也弹提示）。
            // rev 24：OSD 新增「按应用过滤」「全屏时不弹」和「拖动吸附开关」；
            // 默认外观也换成用户自己调好的那套（300×48、圆角 10）。
            // 这些字段都有 serde 默认值，老配置读进来不用搬数据。
            // rev 23：读数拆成「百分比 / 分贝」两个元素，每个元素还多了自己的尺寸、
            // 可以单独删掉。老配置的 `value` 直接读成百分比，分贝用默认位置补上，
            // 不需要搬数据 —— 这里只是把版本号推一格。
            // rev 22：OSD 默认排版收小了一档（320×96 → 300×64，四个元素重新对齐）。
            // 只有**还停在旧默认值**的配置才整体换掉；自己拖过位置的布局一个字都不动。
            if config.config_rev < 22 {
                let legacy_layout = [
                    (OsdPart::Icon, 0.05, 0.26),
                    (OsdPart::Title, 0.20, 0.14),
                    (OsdPart::Percent, 0.66, 0.14),
                    (OsdPart::Bar, 0.20, 0.67),
                ];
                let untouched = config.osd.width == 320
                    && config.osd.height == 96
                    && legacy_layout.iter().all(|(part, x, y)| {
                        let position = config.osd.part(*part);
                        (position.x - x).abs() < 0.005 && (position.y - y).abs() < 0.005
                    });
                if untouched {
                    let defaults = OsdConfig::default();
                    config.osd.width = defaults.width;
                    config.osd.height = defaults.height;
                    config.osd.layout = defaults.layout;
                }
            }
            // rev 19：托盘右键菜单改成可自定义布局；老配置按默认布局补齐
            // （设备列表在最前，其余照旧，等于行为不变）。
            // rev 18：三种模式的名单分开存。老配置只有一份，跟着当时选中的模式搬进它自己的桶，
            // 这样切到别的模式时不会看到另一份名单。
            // rev 38：文本元素改成「按需新建」。老配置里固定留的三条空文字收掉，
            // 有内容的往前挤、重新编号（两套排布一起改）。
            if config.config_rev < 38 {
                config.osd.compact_texts();
            }
            if config.config_rev < 18 {
                // 只在旧的那份非空、且目标桶还是空的时候搬：已经写过新字段的配置不能被清掉。
                let legacy = !config.startup.device_states.is_empty();
                match config.startup.device_state_mode {
                    DeviceStateMode::Blacklist
                        if legacy && config.startup.device_blacklist.is_empty() =>
                    {
                        config.startup.device_blacklist =
                            std::mem::take(&mut config.startup.device_states);
                    }
                    DeviceStateMode::Whitelist
                        if legacy && config.startup.device_whitelist.is_empty() =>
                    {
                        config.startup.device_whitelist =
                            std::mem::take(&mut config.startup.device_states);
                    }
                    // 手动 / 禁用：留在 `device_states`（手动那份）里。
                    _ => {}
                }
            }
            config.config_rev = CONFIG_REV;
            migrated = true;
        }
        // 页脚按钮表：注册表里有、配置里没有的补进去（默认显示）。
        //
        // 故意放在 rev 判断**外面**、每次都跑：以后往 `FOOTER_ITEMS` 里加按钮，
        // 老用户升级完就能在页脚上看到它，不用先自己找到设置页去打开。
        // 幂等，已有的一项都不动（显隐、位置都是用户自己排的）。
        if config.footer.ensure_all() {
            migrated = true;
        }
        let store = Self { path, config };
        // 首次运行就把默认配置落盘，便携版目录里一眼能看到 config.json。
        if existing.is_none() || !store.path.exists() || migrated {
            let _ = store.save();
        }
        store
    }

    pub fn get(&self) -> &Config {
        &self.config
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn update(&mut self, change: impl FnOnce(&mut Config)) -> io::Result<()> {
        change(&mut self.config);
        self.save()
    }

    fn save(&self) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(&self.config).map_err(io::Error::other)?;
        std::fs::write(&self.path, json)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 自启方式：默认注册表（跟以前的行为一致），老配置读进来也是注册表。
    ///
    /// 存的是 `run-key` / `task` / `boot` 这几个短名，跟 `panel_layout` 的写法统一；
    /// 拆成短横线是为了以后能再加变体而不破坏已有配置文件。
    #[test]
    fn launch_method_defaults_to_run_key() {
        assert_eq!(Config::default().launch_method, LaunchMethod::RunKey);
        let old: Config = serde_json::from_str("{\"config_rev\": 61}").unwrap();
        assert_eq!(old.launch_method, LaunchMethod::RunKey);
        // 存盘再读回来还是同一个（枚举名改过就会在这里断）。
        for method in LaunchMethod::ALL {
            let mut config = Config::default();
            config.launch_method = method;
            let json = serde_json::to_string(&config).unwrap();
            let back: Config = serde_json::from_str(&json).unwrap();
            assert_eq!(back.launch_method, method);
        }
        let json = serde_json::to_string(&Config::default()).unwrap();
        assert!(json.contains("\"launch_method\":\"run-key\""));
    }

    /// 「启动时以管理员运行」默认关着：不能一装好就向系统要管理员权限。
    #[test]
    fn run_as_admin_defaults_to_off() {
        assert!(!Config::default().run_as_admin);
        let old: Config = serde_json::from_str("{\"config_rev\": 61}").unwrap();
        assert!(!old.run_as_admin);
    }

    /// 分段按钮用序号换回来，两边必须对得上（写错了点一下会跳回注册表）。
    #[test]
    fn launch_method_index_round_trips() {
        for method in LaunchMethod::ALL {
            assert_eq!(LaunchMethod::from_index(method.index()), method);
        }
        // 认不出来的序号按注册表算 —— 那是老行为，最保守。
        assert_eq!(LaunchMethod::from_index(9), LaunchMethod::RunKey);
    }

    /// 「禁用 Windows 音量条」默认关着：不能一装好就把人家的音量条干掉。
    #[test]
    fn block_system_osd_defaults_off() {
        let config = Config::default();
        assert!(!config.osd.block_system_osd);
        // 老配置（没有这个字段）读进来也应该是关的。
        let old: Config = serde_json::from_str("{\"config_rev\": 42}").unwrap();
        assert!(!old.osd.block_system_osd);
    }

    /// 「滚轮悬浮调节」默认开着：滚轮本来就能调（以前只认音量条那一条），
    /// 老配置升级上来不该突然变成不能滚。
    #[test]
    fn wheel_hover_defaults_on() {
        assert!(Config::default().osd.wheel_hover);
        let old: Config = serde_json::from_str("{\"config_rev\": 43}").unwrap();
        assert!(old.osd.wheel_hover);
    }

    /// 元素颜色：设了能读回来，空串（或纯空白）就是恢复「跟随文字色」。
    #[test]
    fn part_color_set_and_clear() {
        let mut config = Config::default();
        assert_eq!(config.osd.part_color(OsdPart::Db), "");
        config.osd.set_part_color(OsdPart::Db, "#ff8800".into());
        assert_eq!(config.osd.part_color(OsdPart::Db), "#ff8800");
        // 各管各的：改分贝不影响百分比。
        assert_eq!(config.osd.part_color(OsdPart::Percent), "");
        config.osd.set_part_color(OsdPart::Db, "   ".into());
        assert_eq!(config.osd.part_color(OsdPart::Db), "");
    }

    /// 面板音量条的粗细 / 圆角：默认 6px + 全圆头（和以前画出来的一模一样）。
    #[test]
    fn panel_bar_look_defaults_and_clamps() {
        let config = Config::default();
        assert_eq!(config.panel_bar_thickness(), 6.0);
        // 圆角 = 半高 × 100%。
        assert!((config.panel_bar_corner() - 3.0).abs() < f32::EPSILON);

        // 粗细超范围要夹住；圆角跟着新的半高走，不会超过它。
        let mut config = Config::default();
        config.panel_bar_thickness = 99;
        assert_eq!(config.panel_bar_thickness(), 20.0);
        assert!((config.panel_bar_corner() - 10.0).abs() < f32::EPSILON);
        config.panel_bar_round = 0;
        assert_eq!(config.panel_bar_corner(), 0.0);

        // 老配置没有这两个字段：读进来就是默认值，画出来和以前一样。
        let old: Config = serde_json::from_str("{\"config_rev\": 61}").unwrap();
        assert_eq!(old.panel_bar_thickness, DEFAULT_PANEL_BAR_THICKNESS);
        assert_eq!(old.panel_bar_round, DEFAULT_PANEL_BAR_ROUND);
    }

    /// 音量条圆角：默认 100（全圆头，和以前一样），比例钳在 0..=1。
    #[test]
    fn bar_round_defaults_to_full_pill() {
        let mut osd = OsdConfig::default();
        assert_eq!(osd.bar_round, 100);
        assert!((osd.bar_round_ratio() - 1.0).abs() < f32::EPSILON);
        osd.bar_round = 0;
        assert!((osd.bar_round_ratio() - 0.0).abs() < f32::EPSILON);
        osd.bar_round = 50;
        assert!((osd.bar_round_ratio() - 0.5).abs() < f32::EPSILON);
        // 手改配置写出 300 也只当 100 用。
        osd.bar_round = 300;
        assert!((osd.bar_round_ratio() - 1.0).abs() < f32::EPSILON);
    }

    /// 音量条的轨道底色 / 静音填充：默认空（用主题那两个），设了能存能读回来。
    #[test]
    fn bar_track_and_muted_colors_round_trip() {
        let mut config = Config::default();
        assert_eq!(config.osd.colors.bar_track, "");
        assert_eq!(config.osd.colors.bar_muted, "");
        config.osd.colors.bar_track = "#ffffff".into();
        config.osd.colors.bar_muted = "#f87171".into();
        let json = serde_json::to_string(&config).expect("序列化");
        let back: Config = serde_json::from_str(&json).expect("反序列化");
        assert_eq!(back.osd.colors, config.osd.colors);
    }

    /// 「恢复默认布局」连配色 / 阴影 / 圆角 / 图层一起回出厂，文字内容不动。
    #[test]
    fn reset_layout_also_restores_appearance() {
        let mut osd = OsdConfig::default();
        let factory = OsdConfig::default();
        osd.colors.card = "#101010".into();
        osd.colors.text = "#ff0000".into();
        osd.colors.accent = "#00ff00".into();
        osd.set_part_color(OsdPart::Percent, "#123456".into());
        osd.corner = 2;
        osd.bar_round = 0;
        osd.opacity = 55;
        osd.shadow.enabled = true;
        osd.shadow.blur = 40;
        osd.shadow.opacity = 90;
        osd.shadow.offset_y = -12;
        osd.shadow.color = "#000000".into();
        let mut order = osd.parts();
        order.reverse();
        osd.z_order = order;
        let text = "+10 dB".to_string();
        osd.title_text = text.clone();

        osd.reset_layout();

        assert_eq!(osd.colors, factory.colors, "配色要回默认（空 = 跟随主题）");
        assert!(osd.part_colors.is_empty(), "元素单独色也要清掉");
        assert_eq!(osd.corner, factory.corner);
        assert_eq!(
            osd.bar_round, factory.bar_round,
            "音量条圆角也要回默认（全圆头）"
        );
        assert_eq!(osd.opacity, factory.opacity);
        assert_eq!(
            osd.shadow, factory.shadow,
            "阴影（开关 / 偏移 / 扩散 / 颜色）全回默认"
        );
        assert!(osd.z_order.is_empty(), "图层顺序回内置顺序");
        assert_eq!(osd.title_text, text, "文字是用户写的东西，不跟着重置");
    }

    /// 重置不会把用户新建的文字弄丢：位置留着，元素列表里照样看得到。
    #[test]
    fn reset_layout_keeps_custom_texts() {
        let mut osd = OsdConfig::default();
        let part = osd.add_text();
        osd.set_text_for(part, "{hh}:{mm}".into());
        osd.set_part(part, 0.42, 0.66);
        osd.set_part_size(part, 15);

        osd.reset_layout();

        let slot = osd.part(part);
        assert_eq!((slot.x, slot.y), (0.42, 0.66), "文字的位置不该被重置丢掉");
        assert_eq!(slot.size, 15);
        assert_eq!(osd.text_for(part), "{hh}:{mm}", "内容更不该丢");
        assert!(
            osd.parts().contains(&part),
            "元素列表里必须还能看到这条文字"
        );
        // 固定元素照常回出厂。
        assert_eq!(osd.part(OsdPart::Icon).x, default_osd_layout()[0].x);
    }

    /// 「恢复默认布局」必须真的把拖动过的元素放回去，两个模板各恢复各的。
    #[test]
    fn reset_layout_restores_both_templates() {
        let defaults = default_osd_layout();
        let media_defaults = default_media_layout();
        let mut osd = OsdConfig::default();

        // 拖动 + 改尺寸 + 关掉一个元素，再恢复。
        osd.set_part(OsdPart::Icon, 0.6, 0.7);
        osd.set_part_size(OsdPart::Icon, 33);
        osd.set_part_visible(OsdPart::Bar, false);
        osd.width = 320;
        osd.height = 120;
        osd.reset_layout();
        assert_eq!(
            (osd.width, osd.height),
            (OsdConfig::default().width, OsdConfig::default().height),
            "卡片尺寸也要回到出厂"
        );
        for expected in &defaults {
            let actual = osd.part(expected.part);
            assert_eq!(actual.x, expected.x, "{:?} 的 x 没回到默认", expected.part);
            assert_eq!(actual.y, expected.y, "{:?} 的 y 没回到默认", expected.part);
            assert_eq!(
                actual.size, expected.size,
                "{:?} 的 size 没回到默认",
                expected.part
            );
            assert_eq!(
                actual.visible, expected.visible,
                "{:?} 的 visible 没回到默认",
                expected.part
            );
        }

        // 媒体键模板同理，而且不能互相串味。
        osd.detailed = true;
        osd.set_part(OsdPart::Title, 0.9, 0.9);
        osd.reset_layout();
        let actual = osd.part(OsdPart::Title);
        let expected = media_defaults
            .iter()
            .find(|position| position.part == OsdPart::Title)
            .copied()
            .expect("媒体键模板必须有标题");
        assert_eq!((actual.x, actual.y), (expected.x, expected.y));
        assert!(osd.detailed, "恢复排布不该顺手切模板");
        // 音量模板那份没被媒体键模板的恢复连带改掉。
        osd.detailed = false;
        for expected in &defaults {
            assert_eq!(osd.part(expected.part).x, expected.x);
        }
    }

    /// 发声特效多了一个「浮光」（流光 + 呼吸）：索引 / 序列化名都要能往返。
    #[test]
    fn activity_effect_drift_round_trips() {
        assert_eq!(ActivityEffect::Drift.index(), 5);
        assert_eq!(ActivityEffect::from_index(5), ActivityEffect::Drift);
        // 越界的索引回落到默认的电平表，不能 panic。
        assert_eq!(ActivityEffect::from_index(99), ActivityEffect::Level);

        let json = serde_json::to_string(&ActivityEffect::Drift).expect("要能序列化");
        assert_eq!(json, "\"drift\"");
        let parsed: ActivityEffect = serde_json::from_str(&json).expect("要能反序列化");
        assert_eq!(parsed, ActivityEffect::Drift);
        // 关掉它才不用补帧。
        assert!(ActivityEffect::Drift.animates());
    }

    /// 卡片阴影：默认关，参数有默认值，序列化能往返。
    #[test]
    fn osd_shadow_defaults_to_off() {
        let config: Config = serde_json::from_str("{}").expect("空配置要能解析");
        assert!(!config.osd.shadow.enabled);
        assert_eq!(config.osd.shadow.offset_y, 4);
        assert_eq!(config.osd.shadow.blur, 12);
        assert_eq!(config.osd.shadow.opacity, 45);
        assert!(config.osd.shadow.color.is_empty());

        let mut on = config.clone();
        on.osd.shadow.enabled = true;
        on.osd.shadow.color = "#000000".to_string();
        let json = serde_json::to_string(&on).expect("要能序列化");
        let back: Config = serde_json::from_str(&json).expect("要能反序列化");
        assert_eq!(back.osd.shadow, on.osd.shadow);
    }

    /// 连发 / 亮度快捷键这些新字段：老配置读进来要有默认值。
    #[test]
    fn hold_repeat_and_brightness_hotkeys_have_defaults() {
        let config: Config = serde_json::from_str("{}").expect("空配置要能解析");
        assert!(config.hotkey_hold_repeat, "按住连发默认开");
        assert_eq!(
            config.hotkey_repeat_interval,
            DEFAULT_HOTKEY_REPEAT_INTERVAL
        );
        // 连发延迟默认 0：按下就按间隔走，和加这一项之前完全一样。
        assert_eq!(config.hotkey_repeat_delay, 0, "连发延迟默认 0");
        // rev 61：默认不理会系统的长按重复，重复节奏只看上面那个间隔。
        assert!(config.hotkey_ignore_system_repeat, "默认忽略系统重复");
        let old: Config = serde_json::from_str(r#"{"config_rev": 60}"#).expect("老配置要能解析");
        assert!(
            old.config_rev == 60 && old.hotkey_ignore_system_repeat,
            "老配置没有这一项时取新默认（开）"
        );
        assert_eq!(config.brightness.hotkey_monitor, 1);
        // 0 = 跟随面板的亮度步进，默认就是这个。
        assert_eq!(config.brightness.hotkey_step, 0);
        // 连发默认只勾音量和亮度：十个动作，名字都得能被认出来（写错一个就静默失效）。
        assert_eq!(
            config.hotkey_repeat_actions.len(),
            DEFAULT_REPEAT_ACTIONS.len(),
            "默认勾选数量"
        );
        for name in DEFAULT_REPEAT_ACTIONS {
            let action = crate::platform::hotkey::Action::from_key(name)
                .unwrap_or_else(|| panic!("默认动作名要能对应上：{name}"));
            assert!(
                crate::platform::hotkey::repeats_on_hold(&config, action),
                "{name} 默认应当参与连发"
            );
        }
        assert!(
            !crate::platform::hotkey::repeats_on_hold(
                &config,
                crate::platform::hotkey::Action::MediaNext
            ),
            "切歌默认不连发"
        );
        // 老配置里没有这一项：读进来要按默认那十个算，不能变成空表。
        let older: Config =
            serde_json::from_str(r#"{"config_rev": 71}"#).expect("老配置要能解析");
        assert_eq!(
            older.hotkey_repeat_actions.len(),
            DEFAULT_REPEAT_ACTIONS.len(),
            "缺字段时取默认集合"
        );
        // 总闸关掉之后一个都不连发。
        let mut off = config.clone();
        off.hotkey_hold_repeat = false;
        assert!(!crate::platform::hotkey::repeats_on_hold(
            &off,
            crate::platform::hotkey::Action::VolumeUp
        ));
        for slots in [
            config.hotkey_brightness_up,
            config.hotkey_brightness_down,
            config.hotkey_brightness_all_up,
            config.hotkey_brightness_all_down,
            config.hotkey_brightness_monitor_up,
            config.hotkey_brightness_monitor_down,
        ] {
            assert!(slots.all().all(|hotkey| hotkey.is_empty()), "默认不绑键");
        }
    }

    /// 老配置里没有 `monitor_mode`：读进来必须是「指定一台」，行为不能变。
    #[test]
    fn osd_monitor_mode_defaults_to_single() {
        let legacy = r#"{ "osd": { "monitor": 2 } }"#;
        let config: Config = serde_json::from_str(legacy).expect("旧配置要能解析");
        assert_eq!(config.osd.monitor_mode, OsdMonitorMode::Single);
        assert_eq!(config.osd.monitor, 2);
        // 两种新模式的字符串值也要能读回来（存盘再读一遍）。
        for (text, mode) in [
            ("single", OsdMonitorMode::Single),
            ("cursor", OsdMonitorMode::Cursor),
            ("all", OsdMonitorMode::All),
        ] {
            let json = format!(r#"{{ "osd": {{ "monitor_mode": "{text}" }} }}"#);
            let parsed: Config = serde_json::from_str(&json).expect("新模式要能解析");
            assert_eq!(parsed.osd.monitor_mode, mode);
        }
    }

    /// 旧配置里快捷键是单个对象，升级后必须还能读进来（否则整份配置回落默认）。
    #[test]
    fn legacy_single_hotkey_object_still_loads() {
        let legacy = r#"{
            "language": "en-US",
            "popup_animation_ms": 200,
            "hotkey_flyout": { "mods": 2, "key": 65 },
            "hotkey_settings": { "mods": 0, "key": 0 },
            "hotkey_volume_up": { "mods": 1, "key": 175 },
            "hotkey_volume_down": { "mods": 0, "key": 0 }
        }"#;
        let config: Config = serde_json::from_str(legacy).expect("旧配置要能解析");
        assert!(config.language.is_en());
        assert_eq!(config.popup_animation_ms, 200);
        assert_eq!(config.hotkey_flyout.get(0), Hotkey::new(2, 65));
        assert!(config.hotkey_flyout.get(1).is_empty());
        assert_eq!(config.hotkey_volume_up.get(0), Hotkey::new(1, 175));
        // 新字段走默认值。
        assert_eq!(config.hotkey_volume_step, 2);
        assert!(!config.hotkey_volume_follow_log);
        // 「跟随系统」默认开着，两档的底色各给一档默认（浅色档倒数第一个、深色档第四个）。
        assert!(config.theme_follow_system);
        assert_eq!(config.theme_light_surface, DEFAULT_LIGHT_SURFACE);
        assert_eq!(config.theme_dark_surface, DEFAULT_DARK_SURFACE);
        // 强调色默认空着 = 跟随 Windows 的强调色。
        assert!(config.theme_light_accent.is_empty());
        assert!(config.theme_dark_accent.is_empty());
    }

    /// 模板跟着「是谁触发的」走：音量 / 静音用音量模板，媒体键用媒体键模板。
    ///
    /// 配置里存的 `detailed` 只是编辑器在改哪套，不能影响运行时 —— 否则切到媒体键
    /// 模板调完布局，调音量也会画成媒体键那套。
    #[test]
    fn template_follows_the_triggering_action() {
        let mut cfg = OsdConfig::default();

        // 编辑器故意停在媒体键模板上，运行时音量操作照样用音量模板。
        cfg.detailed = true;
        let volume = cfg.for_function(OsdFunction::Volume);
        assert!(!volume.detailed);
        assert!(volume.shows_part(OsdPart::Bar), "音量模板要画音量条");
        assert!(volume.shows_part(OsdPart::Percent), "音量模板要画百分比");

        // 反过来也一样：编辑器停在音量模板，媒体键仍然是媒体键模板。
        cfg.detailed = false;
        for function in [
            OsdFunction::PrevTrack,
            OsdFunction::NextTrack,
            OsdFunction::PlayPause,
            OsdFunction::Stop,
        ] {
            let media = cfg.for_function(function);
            assert!(media.detailed, "{function:?} 该用媒体键模板");
            assert!(media.shows_part(OsdPart::Icon));
            assert!(media.shows_part(OsdPart::Title));
            assert!(!media.shows_part(OsdPart::Bar), "媒体键模板没有音量条");
            assert!(!media.shows_part(OsdPart::Percent), "媒体键模板没有读数");
        }

        // 静音算音量操作。
        assert!(!cfg.for_function(OsdFunction::Mute).detailed);
    }

    /// 默认值：托盘图标用喇叭、对数刻度关掉。
    #[test]
    fn defaults_use_speaker_icon_and_linear_scale() {
        let config = Config::default();
        assert_eq!(config.tray_icon_style, TrayIconStyle::Arcs);
        assert!(!config.use_logarithmic_volume);
        assert_eq!(config.config_rev, CONFIG_REV);
    }

    /// 老配置（没有 config_rev）读进来要跟着升级到新默认，并且立刻落盘。
    #[test]
    fn old_config_upgrades_to_new_defaults() {
        let dir = std::env::temp_dir().join(format!("bolume-config-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(
            &path,
            r#"{"use_logarithmic_volume": true, "tray_icon_style": "number"}"#,
        )
        .unwrap();

        let store = ConfigStore::load(path.clone());
        assert!(!store.get().use_logarithmic_volume);
        assert_eq!(store.get().tray_icon_style, TrayIconStyle::Arcs);
        assert_eq!(store.get().config_rev, CONFIG_REV);
        // 升级后的值已经写回文件，下次启动不会又变一次。
        let saved: Config = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(saved.tray_icon_style, TrayIconStyle::Arcs);
        assert!(!saved.use_logarithmic_volume);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// rev 48：老配置只有一对全局自选色，打开「跟随系统」后那对会被档位整对顶掉。
    /// 读进来要搬一份到档位里 —— 浅色底只进亮档、深色底只进暗档，强调色两档都进。
    #[test]
    fn follow_system_slots_inherit_legacy_custom_colors() {
        let dir = std::env::temp_dir().join(format!("bolume-slot-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();

        // 浅底 + 强调色：底色进亮档，强调色两档都进。
        let light = dir.join("light.json");
        std::fs::write(
            &light,
            r##"{"config_rev": 47, "theme_follow_system": true,
                "theme_surface": "#f4efe6", "theme_accent": "#f87171"}"##,
        )
        .unwrap();
        let store = ConfigStore::load(light);
        assert_eq!(store.get().theme_light_surface, "#f4efe6");
        assert_eq!(store.get().theme_light_accent, "#f87171");
        assert_eq!(store.get().theme_dark_accent, "#f87171");
        // 浅底不该进暗档：暗档留着的就是 rev 72 的默认底色。
        assert_eq!(store.get().theme_dark_surface, DEFAULT_DARK_SURFACE);
        // 全局那对留着不动：切回固定主题时还是原来那个。
        assert_eq!(store.get().theme_surface, "#f4efe6");

        // 深底只进暗档。
        let dark = dir.join("dark.json");
        std::fs::write(
            &dark,
            r##"{"config_rev": 47, "theme_follow_system": true, "theme_surface": "#151519"}"##,
        )
        .unwrap();
        let store = ConfigStore::load(dark);
        assert_eq!(store.get().theme_dark_surface, "#151519");
        assert_eq!(store.get().theme_light_surface, DEFAULT_LIGHT_SURFACE);

        // 已经是新版本、档位被用户清空：不能再搬回来（迁移只按 rev 走一次）。
        let cleared = dir.join("cleared.json");
        std::fs::write(
            &cleared,
            r##"{"config_rev": 48, "theme_follow_system": true,
                "theme_surface": "#f4efe6", "theme_accent": "#f87171"}"##,
        )
        .unwrap();
        let store = ConfigStore::load(cleared);
        // rev 72 之前还空着的底色会被填成新默认（那之后自己清空的不会再被填）。
        assert_eq!(store.get().theme_light_surface, DEFAULT_LIGHT_SURFACE);
        assert!(store.get().theme_light_accent.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// rev 50：音量条自己那条元素色搬进强调色。
    ///
    /// 编辑器里每行末尾那个色块撤掉之后，音量条的填充只剩「强调色」一个入口；
    /// 老配置里单独给音量条挑过色的，不搬进强调色就再也调不动那一格了
    /// （`part_color(Bar)` 优先级比强调色高）。
    #[test]
    fn rev50_moves_bar_part_color_into_accent() {
        let dir = std::env::temp_dir().join(format!("bolume-barc{}-{}", std::process::id(), 50));
        std::fs::create_dir_all(&dir).unwrap();

        // 强调色空着：搬过去。
        let empty = dir.join("empty.json");
        std::fs::write(
            &empty,
            r##"{"config_rev": 49, "osd": {"part_colors": [{"part": "bar", "color": "#4ade80"}]}}"##,
        )
        .unwrap();
        let store = ConfigStore::load(empty);
        assert_eq!(store.get().osd.colors.accent, "#4ade80");
        assert!(
            store
                .get()
                .osd
                .part_colors
                .iter()
                .all(|entry| entry.part != OsdPart::Bar),
            "音量条那一条搬完要删掉"
        );

        // 强调色已经有值：不覆盖，但那一条照样清掉（免得压着强调色不让改）。
        let kept = dir.join("kept.json");
        std::fs::write(
            &kept,
            r##"{"config_rev": 49, "osd": {"colors": {"accent": "#facc15"},
                "part_colors": [{"part": "bar", "color": "#4ade80"}]}}"##,
        )
        .unwrap();
        let store = ConfigStore::load(kept);
        assert_eq!(store.get().osd.colors.accent, "#facc15");
        assert!(
            !store
                .get()
                .osd
                .part_colors
                .iter()
                .any(|entry| entry.part == OsdPart::Bar)
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// rev 50：编辑器自己的两个外观项落在 `Config` 顶层，默认值不能是空的。
    #[test]
    fn editor_appearance_has_sane_defaults() {
        let config = Config::default();
        assert_eq!(config.editor_bg, "#000000");
        assert!(config.editor_panel_w >= 250.0 && config.editor_panel_w <= 420.0);
        // 老配置读进来就是这两个默认值（字段缺失 = 用 Default）。
        let old: Config = serde_json::from_str(r#"{"config_rev": 49}"#).unwrap();
        assert_eq!(old.editor_bg, "#000000");
        assert_eq!(old.editor_panel_w, 268.0);
    }

    /// 托盘数字的出厂外观：用户 2026-10-25 调好的那套（Arial / 锐利 / 20 点 +
    /// 宽高按位数缩放）。老配置缺这三个字段时也必须落到这一套，不能落到「自动」。
    #[test]
    fn tray_digits_default_to_the_tuned_look() {
        let config = Config::default();
        assert_eq!(config.tray_number_font, "Arial");
        assert_eq!(config.tray_number_antialias, super::TrayAntialias::Sharp);
        let one = config.tray_digit_one;
        assert_eq!((one.size_pt, one.width, one.height), (20, 60, 60));
        let two = config.tray_digit_two;
        assert_eq!((two.size_pt, two.width, two.height), (20, 45, 45));
        let three = config.tray_digit_three;
        assert_eq!((three.size_pt, three.width, three.height), (20, 33, 33));

        // 老配置文件里没有这三项：serde 的 default 必须给同一套值。
        let old: Config = serde_json::from_str(r#"{"config_rev": 66}"#).unwrap();
        assert_eq!(old.tray_digit_one.width, 60);
        assert_eq!(old.tray_digit_two.width, 45);
        assert_eq!(old.tray_digit_three.width, 33);
        assert_eq!(old.tray_number_antialias, super::TrayAntialias::Sharp);
    }

    /// rev 10：托盘数字从「像素高度」改成「占可画区的百分比」，老值要按同一块画布换算。
    #[test]
    fn legacy_tray_number_pixels_become_percent() {
        let dir = std::env::temp_dir().join(format!("bolume-numbersize-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(&path, r#"{"tray_number_size": 7, "config_rev": 9}"#).unwrap();

        let store = ConfigStore::load(path.clone());
        // 7px 在 96 DPI 的 14px 可画区里占一半 —— 50%。
        let canvas = crate::tray::canvas_size();
        let expected = ((7.0 / (canvas - 2.0)) * 100.0).round().clamp(1.0, 100.0) as u32;
        assert_eq!(store.get().tray_number_size, expected);
        assert_eq!(store.get().config_rev, CONFIG_REV);

        // 「自动」(0) 原样保留，不能被换算成一个具体百分比。
        std::fs::write(&path, r#"{"tray_number_size": 0, "config_rev": 9}"#).unwrap();
        let auto = ConfigStore::load(path);
        assert_eq!(auto.get().tray_number_size, 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 老配置里没有 `log_scroll_wheel_step`，读进来要落到「一格 2 dB」——
    /// 旧版在对数模式下就是把 `scroll_wheel_step` 当 dB 用的，行为不能变。
    #[test]
    fn missing_log_wheel_step_falls_back_to_two_db() {
        let config: Config = serde_json::from_str(r#"{"scroll_wheel_step": 7}"#).unwrap();
        assert_eq!(config.scroll_wheel_step, 7);
        assert!((config.log_scroll_wheel_step - 2.0).abs() < 1e-6);
        // 默认按百分比走：一格 7 个百分点。
        assert!((config.scroll_display(0.5, 1.0) - 0.57).abs() < 1e-6);

        // 打开「按分贝数步进」后，一格是真实的 2 dB —— 线性刻度上 0.5（= -6 dB）
        // 加完应该落在 -4 dB 附近，而不是固定 2% 的行程。
        let mut in_db = config.clone();
        in_db.scroll_wheel_in_db = true;
        let next = in_db.scroll_display(0.5, 1.0);
        let after_db = crate::audio::amplitude_to_db(next);
        assert!((after_db - -4.0).abs() < 0.05, "{after_db}");
    }

    /// rev 16/17：设备开关名单从「一个开关 + 名单」换成分档规则。
    /// rev 15 的老开关开着（逐台显式开关）→ rev 16 的黑名单 → rev 17 的**手动**（语义等价）；
    /// 关着（或没有这一项）→ 现在的**禁用**。
    #[test]
    fn legacy_device_state_switch_maps_to_manual() {
        let dir = std::env::temp_dir().join(format!("bolume-devmode-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");

        std::fs::write(
            &path,
            r#"{"config_rev": 15, "startup": {"apply_device_states": true, "device_states": []}}"#,
        )
        .unwrap();
        let store = ConfigStore::load(path.clone());
        assert_eq!(
            store.get().startup.device_state_mode,
            DeviceStateMode::Manual,
            "老开关开着 = 逐台显式开关，对应现在的手动模式"
        );

        std::fs::write(
            &path,
            r#"{"config_rev": 15, "startup": {"apply_device_states": false}}"#,
        )
        .unwrap();
        let manual = ConfigStore::load(path);
        assert_eq!(
            manual.get().startup.device_state_mode,
            DeviceStateMode::Disabled,
            "老开关关着就落到禁用（不做事）"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// rev 18：三种模式的名单分开存，老配置那份跟着当时选中的模式搬家。
    #[test]
    fn legacy_single_list_moves_into_mode_bucket() {
        let dir = std::env::temp_dir().join(format!("bolume-listbuckets-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");

        std::fs::write(
            &path,
            r#"{"config_rev": 17, "startup": {"device_state_mode": "whitelist",
                 "device_states": [{"id": "hp", "name": "耳机", "enabled": true}]}}"#,
        )
        .unwrap();
        let store = ConfigStore::load(path.clone());
        let startup = &store.get().startup;
        assert_eq!(
            startup.device_whitelist.len(),
            1,
            "白名单模式下的那份要搬进白名单"
        );
        assert!(startup.device_states.is_empty());
        assert!(startup.device_blacklist.is_empty());

        // 手动模式下则留在手动那份里。
        std::fs::write(
            &path,
            r#"{"config_rev": 17, "startup": {"device_state_mode": "manual",
                 "device_states": [{"id": "hp", "name": "耳机", "enabled": true}]}}"#,
        )
        .unwrap();
        let manual = ConfigStore::load(path);
        assert_eq!(manual.get().startup.device_states.len(), 1);
        assert!(manual.get().startup.device_whitelist.is_empty());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// rev 9：滚轮步进从「跟着对数刻度」拆成独立开关，老配置要保住原来的手感。
    #[test]
    fn legacy_log_scale_turns_on_db_wheel_step() {
        let dir = std::env::temp_dir().join(format!("bolume-wheeldb-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(
            &path,
            r#"{"use_logarithmic_volume": true, "config_rev": 8}"#,
        )
        .unwrap();

        let store = ConfigStore::load(path.clone());
        assert!(store.get().use_logarithmic_volume);
        assert!(
            store.get().scroll_wheel_in_db,
            "对数刻度开着的老配置，一格本来就是 dB，要原样搬到新开关上"
        );

        // 对数刻度关着的老配置保持按百分比走。
        std::fs::write(
            &path,
            r#"{"use_logarithmic_volume": false, "config_rev": 8}"#,
        )
        .unwrap();
        let linear = ConfigStore::load(path);
        assert!(!linear.get().scroll_wheel_in_db);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 老配置里没有 `log_scroll_wheel_step`，读进来要沿用当时的
    /// `scroll_wheel_step`（那会儿对数模式下它就是 dB），滚轮手感不能变。
    #[test]
    fn legacy_scroll_step_becomes_log_step() {
        let dir = std::env::temp_dir().join(format!("bolume-logstep-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(
            &path,
            r#"{"scroll_wheel_step": 3, "use_logarithmic_volume": true}"#,
        )
        .unwrap();

        let store = ConfigStore::load(path.clone());
        assert_eq!(store.get().scroll_wheel_step, 3);
        assert!((store.get().log_scroll_wheel_step - 3.0).abs() < 1e-6);

        // 迁移结果落盘了：下次启动不会又回到 2 dB 默认。
        let again = ConfigStore::load(path.clone());
        assert!((again.get().log_scroll_wheel_step - 3.0).abs() < 1e-6);

        // 已经有独立字段的配置不受影响。
        std::fs::write(
            &path,
            r#"{"scroll_wheel_step": 3, "log_scroll_wheel_step": 0.5}"#,
        )
        .unwrap();
        let kept = ConfigStore::load(path);
        assert!((kept.get().log_scroll_wheel_step - 0.5).abs() < 1e-6);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 快捷键步进：百分比 / 跟随对数刻度 / 固定 dB 三种算法。
    #[test]
    fn hotkey_step_modes_differ() {
        use HotkeyStepTarget::{App, Device};
        let mut config = Config::default();
        // 百分比：默认 2 个百分点。
        assert!((config.hotkey_step_display(Device, 0.5, 1.0) - 0.52).abs() < 1e-6);

        // 跟随对数刻度：每格 step dB，刻度 -40 dB 满量程 → 位移 2/40。
        config.use_logarithmic_volume = true;
        config.hotkey_volume_follow_log = true;
        assert!(config.hotkey_step_is_db(Device));
        assert!((config.hotkey_step_display(Device, 0.5, 1.0) - 0.55).abs() < 1e-6);

        // 固定 dB：线性刻度下也按真实幅度算，1.0 减 2 dB ≈ 0.7943。
        config.use_logarithmic_volume = false;
        config.hotkey_volume_in_db = true;
        assert!(config.hotkey_step_is_db(Device));
        let next = config.hotkey_step_display(Device, 1.0, -1.0);
        assert!((next - 0.7943).abs() < 1e-3, "got {next}");

        // 应用那一路是独立的一套，改设备不影响它。
        assert!((config.hotkey_step_display(App, 0.5, 1.0) - 0.52).abs() < 1e-6);
        config.hotkey_app_volume_in_db = true;
        assert!(config.hotkey_step_is_db(App));
        assert!(config.hotkey_step_is_db(Device));
    }

    /// rev 14 的老配置没有应用快捷键那几项，读进来要落到默认（步进 2%、两个开关关）。
    #[test]
    fn app_hotkey_fields_default_when_missing() {
        let legacy = r#"{"config_rev": 14, "hotkey_volume_step": 5}"#;
        let config: Config = serde_json::from_str(legacy).expect("rev 14 的配置要能读");
        assert_eq!(config.hotkey_volume_step, 5, "老字段不能被新默认值盖掉");
        assert_eq!(config.hotkey_app_volume_step, 2);
        assert!(!config.hotkey_app_volume_follow_log);
        assert!(!config.hotkey_app_volume_in_db);
        assert!(!config.hotkey_volume_in_db);
        assert!((0..3).all(|slot| config.hotkey_app_volume_up.get(slot).is_empty()));
        assert!((0..3).all(|slot| config.hotkey_app_volume_down.get(slot).is_empty()));
    }

    /// 自定义设备名：有就盖住系统名，清空就恢复。
    #[test]
    fn device_alias_overrides_and_clears() {
        let mut config = Config::default();
        let id = "speaker-1";
        assert_eq!(
            config.device_label(id, "Speakers (Realtek)"),
            "Speakers (Realtek)"
        );

        assert!(config.set_device_alias(id, "  桌面音箱  "));
        assert_eq!(config.device_label(id, "Speakers (Realtek)"), "桌面音箱");

        // 同一个 id 改第二次是就地改，不会多出一条。
        assert!(config.set_device_alias(id, "书架箱"));
        assert_eq!(config.device_names.len(), 1);
        assert_eq!(config.device_label(id, "Speakers (Realtek)"), "书架箱");

        // 值没变就不算改动（免得白写配置）。
        assert!(!config.set_device_alias(id, "书架箱"));

        // 传空 = 恢复系统原名。
        assert!(config.set_device_alias(id, "  "));
        assert!(config.device_names.is_empty());
        assert_eq!(
            config.device_label(id, "Speakers (Realtek)"),
            "Speakers (Realtek)"
        );

        // 清一个本来就没有的条目，不算改动。
        assert!(!config.set_device_alias(id, ""));
    }

    /// 应用改名：按会话 key 存、幂等、传空 = 恢复进程名。
    ///
    /// key 就是 `exe:<全路径小写>`（见 `audio::engine` 里的分组键），和
    /// `hidden_apps` 共用同一套 key —— 折叠过的应用改完名还得能认出来。
    #[test]
    fn app_alias_overrides_and_clears() {
        let mut config = Config::default();
        let key = "exe:c:\\program files\\music\\player.exe";
        assert_eq!(config.app_label(key, "Player"), "Player");

        assert!(config.set_app_alias(key, "  网易云  "));
        assert_eq!(config.app_label(key, "Player"), "网易云");

        assert!(config.set_app_alias(key, "网易云音乐"));
        assert_eq!(config.app_names.len(), 1);
        assert!(!config.set_app_alias(key, "网易云音乐"));

        // 别的应用各存各的。
        assert!(config.set_app_alias("exe:d:\\game.exe", "游戏"));
        assert_eq!(config.app_label("exe:d:\\game.exe", "Game"), "游戏");
        assert_eq!(config.app_names.len(), 2);

        // 空名字 = 恢复进程名（不是存一个空别名）。
        assert!(config.set_app_alias(key, "  "));
        assert_eq!(config.app_label(key, "Player"), "Player");
        assert!(!config.set_app_alias(key, ""));
    }

    /// 响度均衡：单应用覆盖优先于全局；和全局一致时不留条目；
    /// 传 `None` 删覆盖、回到跟随全局。
    #[test]
    fn loudness_override_follows_global_until_set() {
        let mut config = Config::default();
        let music = "exe:c:\\music\\player.exe";
        let game = "exe:d:\\game.exe";

        // 默认全局关，谁都跟随。
        assert!(!config.loudness.enabled);
        assert!(!config.loudness_for(music));

        // 打得跟全局一致 = 不留条目，否则以后改全局它就不跟着走了。
        assert!(!config.set_app_loudness(music, Some(false)));
        assert!(config.app_loudness.is_empty());

        // 单独给一个应用开。
        assert!(config.set_app_loudness(music, Some(true)));
        assert!(config.loudness_for(music));
        assert!(!config.loudness_for(game));
        assert!(!config.set_app_loudness(music, Some(true)));

        // 全局打开：game 没有条目，直接跟着全局生效；
        // music 的条目和全局一致了，再设一次就会被整理掉。
        config.loudness.enabled = true;
        assert!(config.loudness_for(game));
        assert!(config.loudness_for(music));
        assert!(config.set_app_loudness(music, Some(true)));
        assert!(config.app_loudness.is_empty());
        assert!(config.loudness_for(game));

        // 全局开着时单独关一个应用。
        assert!(config.set_app_loudness(game, Some(false)));
        assert!(!config.loudness_for(game));
        assert!(config.loudness_for(music));

        // 删覆盖 = 回到跟随全局。
        assert!(config.set_app_loudness(game, None));
        assert!(config.loudness_for(game));
        assert!(!config.set_app_loudness(game, None));
    }

    /// `smoothness` 照 TomSteady 的默认值 8（INI 里的 `stepping`）；
    /// `level` 反过来取满档 100（插件默认 75，但压着上限听不出均衡在干活）。
    #[test]
    fn loudness_defaults_match_tomsteady() {
        let loudness = LoudnessConfig::default();
        assert!(!loudness.enabled);
        assert_eq!(loudness.level, 100);
        assert_eq!(loudness.smoothness, 8);

        // 100% 目标被削顶线钳到 90.6%；一格 = 1/2^8 = 1/256（插件把 stepping
        // 当定点小数位数用）。
        assert!((loudness.target() - LoudnessConfig::CLIP_LEVEL * 0.99).abs() < 1e-6);
        assert!((loudness.step() - 1.0 / 256.0).abs() < 1e-9);

        // 一拍 15ms 合不到一块（一块 26ms），引擎得攒两拍才决策一次。
        assert!((loudness.blocks_per_tick() - 15.0 / 26.0).abs() < 1e-6);
        assert!(loudness.blocks_per_tick() < 1.0);

        // 防削顶线是插件里的 30000/32768，不是 1.0。
        assert!((LoudnessConfig::CLIP_LEVEL - 0.91552734).abs() < 1e-6);
        assert!(LoudnessConfig::CLIP_LEVEL < 1.0);
    }

    /// 两个参数都要夹到有效范围：填 0 或填 300 不能算出离谱的增益 / 步长。
    #[test]
    fn loudness_params_clamp() {
        let zero = LoudnessConfig {
            level: 0,
            smoothness: 0,
            ..LoudnessConfig::default()
        };
        // level 0 也留 1%，否则等于「不放大」；步长最粗到 50%（一块走半程）。
        assert!((zero.target() - 0.01).abs() < 1e-6);
        assert!((zero.step() - 0.5).abs() < 1e-6);

        let huge = LoudnessConfig {
            level: 300,
            smoothness: 99,
            ..LoudnessConfig::default()
        };
        // 目标高不过防削顶线：level 100 实际落在 90.6%（见 `target()` 的说明）。
        assert!((huge.target() - LoudnessConfig::CLIP_LEVEL * 0.99).abs() < 1e-6);
        assert!((huge.step() - 1.0 / 1024.0).abs() < 1e-9);

        // 步长必须是正的，不然 `gain.max(step)` 之后增益会卡在 0。
        assert!(zero.step() > 0.0);
        assert!(huge.step() > 0.0);
    }

    /// 设备图标：按 id 存、幂等、传空 = 回到跟随设备类型。
    #[test]
    fn device_icon_overrides_and_clears() {
        let mut config = Config::default();
        let id = "speaker-1";
        assert_eq!(config.device_icon(id), None);

        assert!(config.set_device_icon(id, " headset "));
        assert_eq!(config.device_icon(id), Some("headset"));

        assert!(config.set_device_icon(id, "earbuds"));
        assert_eq!(config.device_icons.len(), 1);
        assert_eq!(config.device_icon(id), Some("earbuds"));
        assert!(!config.set_device_icon(id, "earbuds"));

        // 另一台设备各存各的，互不影响。
        assert!(config.set_device_icon("speaker-2", "bookshelf"));
        assert_eq!(config.device_icon("speaker-2"), Some("bookshelf"));
        assert_eq!(config.device_icon(id), Some("earbuds"));

        assert!(config.set_device_icon(id, "  "));
        assert_eq!(config.device_icon(id), None);
        assert_eq!(config.device_icons.len(), 1);
        assert!(!config.set_device_icon(id, ""));
    }

    /// 白名单：配置里写了个不存在的图标名时不能把界面搞成空白。
    #[test]
    fn unknown_device_icon_falls_back_to_kind() {
        use crate::audio::DeviceKind;
        let mut config = Config::default();
        config.set_device_icon("dev", "not-an-icon");
        let resolved = crate::ui::device_icon_resolved(&config, "dev", DeviceKind::Headphones);
        assert_eq!(resolved.as_ref(), "headphones");

        config.set_device_icon("dev", "earbuds");
        let resolved = crate::ui::device_icon_resolved(&config, "dev", DeviceKind::Headphones);
        assert_eq!(resolved.as_ref(), "earbuds");
    }

    /// 显示器别名：和音频设备同一套语义（按 key 存、空白名恢复系统名）。
    #[test]
    fn display_alias_overrides_and_clears() {
        let mut config = Config::default();
        let key = "0:ROG XG279Q";
        assert_eq!(config.display_label(key, "ROG XG279Q"), "ROG XG279Q");

        assert!(config.set_display_alias(key, "  主屏  "));
        assert_eq!(config.display_label(key, "ROG XG279Q"), "主屏");

        // 同一台改第二次是就地改，不会多出一条。
        assert!(config.set_display_alias(key, "游戏屏"));
        assert_eq!(config.display_names.len(), 1);
        assert_eq!(config.display_alias(key), Some("游戏屏"));

        // 值没变就不算改动。
        assert!(!config.set_display_alias(key, "游戏屏"));

        // 传空 = 恢复系统原名。
        assert!(config.set_display_alias(key, ""));
        assert!(config.display_names.is_empty());
        assert_eq!(config.display_label(key, "ROG XG279Q"), "ROG XG279Q");
    }

    /// rev 19 的老配置没有 `device_names`，读进来要是空表。
    #[test]
    fn device_names_default_empty_on_old_config() {
        let legacy: Config = serde_json::from_str(r#"{"config_rev": 19}"#).unwrap();
        assert!(legacy.device_names.is_empty());
        assert!(legacy.device_icons.is_empty());
        assert_eq!(legacy.device_label("x", "原名"), "原名");
        assert_eq!(legacy.device_icon("x"), None);
    }

    /// 托盘菜单默认布局：设备列表在最前面，系统入口平铺（不再收在子菜单里）。
    #[test]
    fn tray_menu_defaults_are_flat() {
        let menu = TrayMenuConfig::default();
        let ids: Vec<&str> = menu.root.iter().map(|node| node.id.as_str()).collect();
        assert_eq!(ids.first(), Some(&"devices"));
        for id in [
            "legacy-mixer",
            "legacy-playback",
            "legacy-recording",
            "legacy-sounds",
            "sound-settings",
            "apps-volume",
        ] {
            assert!(ids.contains(&id), "{id} 应该直接摆在右键里");
        }
        assert!(menu.root.iter().all(|node| !node.is_folder()));
        assert!(menu.root.iter().any(|node| node.id == "devices" && node.visible));

        // 老配置（没有 tray_menu 段）也要拿到这套默认。
        let legacy: Config = serde_json::from_str(r#"{"config_rev": 18}"#).unwrap();
        assert_eq!(legacy.tray_menu, TrayMenuConfig::default());
    }

    /// 「重启」默认就摆在右键里（排在设置 / 退出前面）。
    #[test]
    fn tray_menu_restart_is_visible_by_default() {
        let menu = TrayMenuConfig::default();
        let index = menu
            .root
            .iter()
            .position(|node| node.id == "restart")
            .expect("默认布局里要留一个重启节点");
        assert!(menu.root[index].visible, "重启默认显示");
        assert_eq!(
            menu.root.get(index + 1).map(|node| node.id.as_str()),
            Some("settings"),
            "重启排在设置前面"
        );
        assert_eq!(
            menu.root.last().map(|node| node.id.as_str()),
            Some("quit"),
            "退出永远是最后一项"
        );
        assert!(menu.contains("restart"));
    }

    /// rev 54：默认顺序和那四条分隔线（rev 56 把两条「声音设置」对调了）。
    #[test]
    fn rev54_default_layout_has_separators() {
        let menu = TrayMenuConfig::default();
        let ids: Vec<&str> = menu.root.iter().map(|node| node.id.as_str()).collect();
        assert_eq!(
            ids,
            vec![
                "devices",
                "separator",
                "toggle",
                "separator",
                "legacy-mixer",
                "apps-volume",
                "legacy-sounds",
                "sound-settings",
                "separator",
                "legacy-playback",
                "legacy-recording",
                "separator",
                "restart",
                "settings",
                "quit",
            ]
        );
        assert_eq!(
            menu.root.iter().filter(|node| node.is_separator()).count(),
            4
        );
    }

    /// rev 54：「原样没动过的老默认」换成新顺序，被关掉的条目依然关着。
    #[test]
    fn rev54_rebases_only_untouched_legacy_default() {
        // 1) 老默认 + 把「音量面板」关了 → 顺序换新，显隐跟着走。
        let mut menu = TrayMenuConfig {
            root: TrayMenuConfig::LEGACY_DEFAULT_IDS
                .iter()
                .map(|id| TrayMenuNode::action(id))
                .collect(),
        };
        menu.root.iter_mut().for_each(|node| {
            if node.id == "toggle" {
                node.visible = false;
            }
        });
        menu.rebase_legacy_default();
        assert_eq!(menu.root.len(), TrayMenuConfig::default().root.len());
        let toggle = menu
            .root
            .iter()
            .find(|node| node.id == "toggle")
            .expect("新布局里还有音量面板");
        assert!(!toggle.visible, "老配置里关着的，换完顺序还是关着");
        assert!(menu.root.iter().filter(|node| node.is_separator()).count() == 4);

        // 2) 用户自己排过的（顺序不同）一个字都不动。
        let mut custom = TrayMenuConfig {
            root: vec![
                TrayMenuNode::action("quit"),
                TrayMenuNode::action("devices"),
            ],
        };
        let before = custom.clone();
        custom.rebase_legacy_default();
        assert_eq!(custom, before, "自己排过的布局不能被覆盖");

        // 3) 顺序已经和新默认一致、只是没有分隔线：只补线，不动顺序。
        let mut aligned = TrayMenuConfig::default();
        aligned.root.retain(|node| !node.is_separator());
        let ids_before: Vec<String> = aligned.root.iter().map(|n| n.id.clone()).collect();
        aligned.rebase_legacy_default();
        let ids_after: Vec<String> = aligned
            .root
            .iter()
            .filter(|node| !node.is_separator())
            .map(|n| n.id.clone())
            .collect();
        assert_eq!(ids_before, ids_after, "顺序不变");
        assert_eq!(
            aligned
                .root
                .iter()
                .filter(|node| node.is_separator())
                .count(),
            4,
            "缺的四条线补上"
        );
    }

    /// rev 56：默认顺序里两条「声音设置」对调；只动没改过的默认，自己排过的不碰。
    #[test]
    fn rev56_swaps_sound_entries_in_default_order() {
        // 1) rev 55 那份默认（四条线的顺序）→ 对调，线一条不丢、位置不动。
        let mut menu = TrayMenuConfig {
            root: TrayMenuConfig::REV55_ORDER
                .iter()
                .map(|id| TrayMenuNode::action(id))
                .collect(),
        };
        // 关掉「声音设置 [旧]」，对调后依然是关着的那一条（显隐跟条目走）。
        menu.root
            .iter_mut()
            .filter(|node| node.id == "legacy-sounds")
            .for_each(|node| node.visible = false);
        menu.insert_separator(None, 1);
        assert!(menu.swap_sound_entries());
        let ids: Vec<&str> = menu.root.iter().map(|node| node.id.as_str()).collect();
        assert_eq!(ids[1], "separator", "分隔线原地不动");
        let sounds = ids.iter().position(|id| *id == "sound-settings").unwrap();
        let legacy = ids.iter().position(|id| *id == "legacy-sounds").unwrap();
        assert!(legacy < sounds, "旧版入口排在前面");
        assert_eq!(sounds, legacy + 1, "两条挨着，中间不夹别的东西");
        assert!(!menu.root[legacy].visible, "被关掉的那条对调后还是关着");

        // 2) 用户自己排过的顺序：一个字不动。
        let mut custom = TrayMenuConfig {
            root: vec![
                TrayMenuNode::action("quit"),
                TrayMenuNode::action("sound-settings"),
                TrayMenuNode::action("legacy-sounds"),
            ],
        };
        let before = custom.clone();
        assert!(!custom.swap_sound_entries());
        assert_eq!(custom, before, "自己排过的布局不能被覆盖");
    }

    /// rev 29 的老配置没有重启节点：迁移时补一个（rev 52 起默认是打开的）。
    #[test]
    fn old_config_gains_visible_restart() {
        let dir = std::env::temp_dir().join(format!("bolume-tray-restart-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(
            &path,
            r#"{"config_rev": 29, "tray_menu": {"root": [
                {"id": "devices"}, {"id": "settings"}, {"id": "quit"}
            ]}}"#,
        )
        .unwrap();

        let store = ConfigStore::load(path.clone());
        let menu = &store.get().tray_menu;
        let restart = menu
            .root
            .iter()
            .find(|node| node.id == "restart")
            .expect("迁移后要有重启节点");
        assert!(restart.visible, "rev 52 起补进来的重启是开着的");
        assert_eq!(
            menu.root.last().map(|node| node.id.as_str()),
            Some("quit"),
            "补在退出前面，退出仍是最后一项"
        );
        assert_eq!(store.get().config_rev, CONFIG_REV);

        // 已经存在（用户打开了、或挪进子菜单）的一律不重排。
        let mut menu = TrayMenuConfig::default();
        menu.root.retain(|node| node.id != "restart");
        menu.root.push(TrayMenuNode::folder(
            "folder:1",
            "子菜单",
            vec![TrayMenuNode::action("restart")],
        ));
        menu.ensure_action("restart");
        assert_eq!(
            menu.root.iter().filter(|node| node.id == "restart").count(),
            0
        );
        assert_eq!(
            menu.root.last().map(|node| node.id.as_str()),
            Some("folder:1")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// rev 52：老配置里「重启」闭着的眼睛要打开、正在发声排最前要关掉。
    ///
    /// 两处都是「改了默认值」—— 只动 `Default` 的话，读进来的还是老配置里存的值，
    /// 用户升级完看到的和升级前一模一样，等于没改。
    #[test]
    fn rev52_opens_restart_and_turns_off_sounding_first() {
        let dir = std::env::temp_dir().join(format!("bolume-rev52-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(
            &path,
            r#"{"config_rev": 51, "sounding_apps_first": true, "tray_menu": {"root": [
                {"id": "devices"}, {"id": "restart", "visible": false}, {"id": "quit"}
            ]}}"#,
        )
        .unwrap();

        let store = ConfigStore::load(path.clone());
        let config = store.get();
        assert!(!config.sounding_apps_first, "排最前默认关");
        let restart = config
            .tray_menu
            .root
            .iter()
            .find(|node| node.id == "restart")
            .expect("重启还在原位");
        assert!(restart.visible, "老配置里闭着的眼睛要打开");
        assert_eq!(store.get().config_rev, CONFIG_REV);

        // 面板字体那几个字段是新增的，老配置读进来走默认：跟随系统 / 基准字号 / 常规 / 不调字距行高。
        assert_eq!(config.panel_font, "");
        assert_eq!(config.panel_font_size, PANEL_BASE_FONT_SIZE);
        assert_eq!(config.panel_letter_spacing, 0.0);
        assert_eq!(config.panel_line_height, 0.0);
        assert_eq!(config.panel_font_weight, 400);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// rev 53：面板字号从「百分比」搬成「字体字号（像素）」。
    ///
    /// 100% = 11.5px（正文基准），所以 130% 应该变成 15.0px（保留一位小数）。
    /// 已经写了解析结果（`panel_font_size`）的配置一律不搬 —— 手改过 rev 的人可能
    /// 还留着旧字段，按 rev 判断会把他的字号又盖一次。
    #[test]
    fn rev53_turns_panel_scale_into_font_size() {
        let dir = std::env::temp_dir().join(format!("bolume-rev53-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");
        std::fs::write(&path, r#"{"config_rev": 52, "panel_font_scale": 130}"#).unwrap();

        let store = ConfigStore::load(path.clone());
        assert_eq!(store.get().panel_font_size, 15.0, "130% 就是 15px");
        assert_eq!(store.get().config_rev, CONFIG_REV);

        // 两个字段都在：以新字段为准（老字段只是历史残留）。
        std::fs::write(
            &path,
            r#"{"config_rev": 52, "panel_font_scale": 80, "panel_font_size": 9.5}"#,
        )
        .unwrap();
        let store = ConfigStore::load(path.clone());
        assert_eq!(store.get().panel_font_size, 9.5);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 子菜单：收进去 / 挪回来 / 删掉子菜单都不丢条目。
    #[test]
    fn tray_menu_folder_moves_keep_entries() {
        let mut menu = TrayMenuConfig::default();
        menu.root
            .push(TrayMenuNode::folder("folder:1", "子菜单 1", Vec::new()));
        let folder_index = menu.root.len() - 1;

        // 把「设置」收进子菜单。
        let settings_index = menu
            .root
            .iter()
            .position(|node| node.id == "settings")
            .unwrap();
        menu.move_root_into_last_folder(settings_index);
        let folder = menu.root.iter().find(|node| node.is_folder()).unwrap();
        assert_eq!(folder.children.len(), 1);
        assert_eq!(folder.children[0].id, "settings");
        assert!(!menu.root.iter().any(|node| node.id == "settings"));

        // 再挪回来（子菜单的下标在搬走一个根条目后会变，按当前树重新找）。
        let folder_index = menu.root.iter().position(|node| node.is_folder()).unwrap();
        menu.move_child_to_root(folder_index, 0);
        assert!(menu.root.iter().any(|node| node.id == "settings"));

        // 删子菜单：里面的条目回到右键，一个都不能少。
        let before = menu.root.len();
        menu.move_root_into_last_folder(
            menu.root
                .iter()
                .position(|node| node.id == "settings")
                .unwrap(),
        );
        let with_child = menu.root.len();
        menu.delete_folder(folder_index);
        // 少了一个「子菜单」节点、多了一个条目 → 总数比 with_child 多不了；
        // 相对删之前（含子菜单节点）正好少 1。
        assert_eq!(menu.root.len(), with_child);
        assert_eq!(menu.root.len() + 1, before);
        assert!(menu.root.iter().all(|node| !node.is_folder()));
    }

    /// 拖拽搬条目：根 → 子菜单、子菜单 → 根、子菜单 → 另一个子菜单都不丢不重。
    #[test]
    fn tray_menu_drag_between_folders() {
        let mut menu = TrayMenuConfig::default();
        menu.root
            .push(TrayMenuNode::folder("folder:1", "子菜单 1", Vec::new()));
        menu.root
            .push(TrayMenuNode::folder("folder:2", "子菜单 2", Vec::new()));

        let settings_index = menu
            .root
            .iter()
            .position(|node| node.id == "settings")
            .unwrap();
        let folder1_index = menu.root.iter().position(|n| n.id == "folder:1").unwrap();
        // 根 → 第一个子菜单（下标在删除后要往前挪）。
        menu.move_node((None, settings_index), Some(folder1_index), usize::MAX);
        let folder1 = menu.root.iter().find(|n| n.id == "folder:1").unwrap();
        assert_eq!(folder1.children.len(), 1);
        assert_eq!(folder1.children[0].id, "settings");

        // 子菜单 1 → 子菜单 2。
        let folder1_index = menu.root.iter().position(|n| n.id == "folder:1").unwrap();
        menu.move_node((Some(folder1_index), 0), Some(folder1_index + 1), 0);
        let folder1 = menu.root.iter().find(|n| n.id == "folder:1").unwrap();
        let folder2 = menu.root.iter().find(|n| n.id == "folder:2").unwrap();
        assert!(folder1.children.is_empty());
        assert_eq!(folder2.children.len(), 1);

        // 子菜单 2 → 根菜单末尾。
        let folder2_index = menu.root.iter().position(|n| n.id == "folder:2").unwrap();
        menu.move_node((Some(folder2_index), 0), None, usize::MAX);
        assert_eq!(menu.root.last().map(|n| n.id.as_str()), Some("settings"));
        // 条目总数不变：15 个根条目（11 个动作 + 4 条分隔线）+ 2 个子菜单。
        let total = menu.root.len()
            + menu
                .root
                .iter()
                .map(|node| node.children.len())
                .sum::<usize>();
        assert_eq!(total, 17);
    }

    /// 拖动换位：同一容器里上下挪，下标按「搬之前」的那一行算。
    #[test]
    fn tray_menu_drag_reorders_in_place() {
        let mut menu = TrayMenuConfig::default();
        let ids: Vec<String> = menu.root.iter().map(|node| node.id.clone()).collect();

        // 把第 2 项（toggle）拖到第 0 项上面。
        menu.move_node((None, 1), None, 0);
        assert_eq!(menu.root[0].id, ids[1]);
        assert_eq!(menu.root[1].id, ids[0]);

        // 把它再拖到第 3 项「上面」：往下搬时下标要补掉自己空出来的那一格。
        menu.move_node((None, 0), None, 3);
        assert_eq!(
            menu.root[2].id,
            ids[1],
            "{:?}",
            menu.root.iter().map(|n| &n.id).collect::<Vec<_>>()
        );
        assert_eq!(menu.root[0].id, ids[0]);
        // 条目一个不多一个不少。
        assert_eq!(menu.root.len(), ids.len());
    }

    /// 上移下移只在自己的容器里换位置。
    #[test]
    fn tray_menu_shift_stays_in_container() {
        let mut menu = TrayMenuConfig::default();
        let first = menu.root[0].id.clone();
        let second = menu.root[1].id.clone();
        menu.shift(None, 1, true);
        assert_eq!(menu.root[0].id, second);
        assert_eq!(menu.root[1].id, first);
        // 已经在最上面的再往上挪，什么都不变。
        menu.shift(None, 0, true);
        assert_eq!(menu.root[0].id, second);
    }

    /// 解除静音的默认：总开关关着，各格子全开 —— 打开总开关就是「全场景都解除」。
    #[test]
    fn unmute_defaults_to_all_on_but_master_off() {
        let mut config = Config::default();
        assert!(!config.unmute.enabled, "总开关默认关，别自己把静音跳掉");
        // 打开总开关，各格子应该已经全开。
        config.unmute.enabled = true;
        for gesture in [
            VolumeGesture::Hotkey,
            VolumeGesture::HoverScroll,
            VolumeGesture::Drag,
            VolumeGesture::TrayScroll,
            VolumeGesture::AppHoverScroll,
            VolumeGesture::AppDrag,
            VolumeGesture::AppHotkey,
        ] {
            assert!(config.unmute.allows(gesture, true), "{gesture:?} 增大");
            assert!(config.unmute.allows(gesture, false), "{gesture:?} 减小");
        }
    }

    /// 各格互不影响：关掉一条路，另一条照旧解除静音。
    #[test]
    fn unmute_flags_are_independent() {
        let mut config = Config::default();
        config.unmute.enabled = true;
        config.unmute.increase_hover = false;
        assert!(!config.unmute.allows(VolumeGesture::HoverScroll, true));
        assert!(config.unmute.allows(VolumeGesture::HoverScroll, false));
        assert!(config.unmute.allows(VolumeGesture::TrayScroll, true));
        assert!(config.unmute.allows(VolumeGesture::Hotkey, true));
        // 拖拽和其它几条互不影响。
        assert!(config.unmute.allows(VolumeGesture::Drag, true));
        config.unmute.decrease_drag = false;
        assert!(config.unmute.allows(VolumeGesture::Drag, true));
        assert!(!config.unmute.allows(VolumeGesture::Drag, false));

        // 应用音量条的三条路（悬停滚动 / 拖 / 快捷键）也各管各的：
        // 关掉应用拖拽，主音量拖拽和应用的另外两条路都不受影响。
        config.unmute.increase_app_drag = false;
        assert!(!config.unmute.allows(VolumeGesture::AppDrag, true));
        assert!(config.unmute.allows(VolumeGesture::AppHoverScroll, true));
        assert!(config.unmute.allows(VolumeGesture::AppHotkey, true));
        assert!(config.unmute.allows(VolumeGesture::Drag, true));
        assert!(config.unmute.allows(VolumeGesture::Hotkey, true));

        // 总开关一关，所有格子全部失效（老行为）。
        config.unmute.enabled = false;
        for gesture in [
            VolumeGesture::Hotkey,
            VolumeGesture::HoverScroll,
            VolumeGesture::Drag,
            VolumeGesture::TrayScroll,
            VolumeGesture::AppHoverScroll,
            VolumeGesture::AppDrag,
            VolumeGesture::AppHotkey,
        ] {
            assert!(!config.unmute.allows(gesture, true));
            assert!(!config.unmute.allows(gesture, false));
        }
    }

    /// rev 12 的老配置没有 `unmute` 段，读进来要落到默认（总开关关、各格全开）。
    #[test]
    fn unmute_missing_in_old_config_uses_defaults() {
        let legacy = r#"{"config_rev": 12}"#;
        let config: Config = serde_json::from_str(legacy).expect("rev 12 的配置要能读");
        assert!(!config.unmute.enabled);
        assert!(config.unmute.increase_tray);
        assert_eq!(config.config_rev, 12);
    }

    /// rev 58 把两个默认值反过来了：老配置要搬过来，不然用户看到的还是老行为。
    #[test]
    fn rev58_flips_release_sound_and_unmute_defaults() {
        let dir = std::env::temp_dir().join(format!("bolume-rev58-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("config.json");
        // 老配置：提示音关着、解除静音开着（都是旧默认）。
        let legacy = r#"{"config_rev": 57, "release_sound": false,
            "unmute": {"enabled": true}}"#;
        std::fs::write(&path, legacy).expect("写临时配置");
        let config = ConfigStore::load(path.clone()).get().clone();
        assert!(config.release_sound, "松手提示音要跟着新默认打开");
        assert!(!config.unmute.enabled, "解除静音总开关要跟着新默认关掉");
        // 各格保留原样：打开总开关还能用回老行为。
        assert!(config.unmute.increase_hotkey);
        assert_eq!(config.config_rev, CONFIG_REV);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// rev 60：松手提示音多了一个「OSD 里也响」的子开关。
    ///
    /// 老配置里没有这一项：默认值和「新装」的答案要分清 —— 新装是开，
    /// 老配置则按主开关搬（主开关关着的人不该因为升级在 OSD 里听见声音）。
    #[test]
    fn rev64_close_animation_defaults_on() {
        let dir = std::env::temp_dir().join(format!("bolume-rev64-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("old.json");
        std::fs::write(&path, r#"{"config_rev": 63, "popup_animation": true}"#)
            .expect("写临时配置");
        let config = ConfigStore::load(path.clone()).get().clone();
        assert!(config.panel_close_animation, "老配置升上来默认带收起动画");
        assert_eq!(config.popup_anim_style, PopupAnimStyle::System);
        assert_eq!(config.osd.anim_style, OsdAnimStyle::Fade);
        // 现在那两档能存能读。
        let path = dir.join("new.json");
        std::fs::write(
            &path,
            r#"{"config_rev": 66, "popup_anim_style": "fade", "osd": {"anim_style": "system"}}"#,
        )
        .expect("写临时配置");
        let config = ConfigStore::load(path.clone()).get().clone();
        assert_eq!(config.popup_anim_style, PopupAnimStyle::Fade);
        assert_eq!(config.osd.anim_style, OsdAnimStyle::System);
        assert_eq!(PopupAnimStyle::from_index(1), PopupAnimStyle::Fade);
        assert_eq!(OsdAnimStyle::from_index(1), OsdAnimStyle::System);
        assert_eq!(PopupAnimStyle::Fade.index(), 1);
        assert_eq!(OsdAnimStyle::System.index(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// rev 66：面板的「从任务栏展开」和屏幕提示的「向上展开」两档删掉了。
    ///
    /// 老配置里写着 `unfold` 的靠 serde alias 落到最接近的那一档（面板 → system，
    /// 屏幕提示 → system），读进来不报错、下次保存就写成新名字。
    #[test]
    fn rev66_removed_styles_fall_back_to_system() {
        let dir = std::env::temp_dir().join(format!("bolume-rev66-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("old.json");
        std::fs::write(
            &path,
            r#"{"config_rev": 65, "popup_anim_style": "unfold", "osd": {"anim_style": "unfold"}}"#,
        )
        .expect("写临时配置");
        let config = ConfigStore::load(path.clone()).get().clone();
        assert_eq!(config.popup_anim_style, PopupAnimStyle::System);
        assert_eq!(config.osd.anim_style, OsdAnimStyle::System);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rev63_animation_styles_default_to_the_old_ones() {
        let dir = std::env::temp_dir().join(format!("bolume-rev63-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);

        // 老配置里没有这两项：读进来还是各自原来那套，行为不变。
        let old = dir.join("old.json");
        std::fs::write(&old, r#"{"config_rev": 62, "popup_animation": true}"#)
            .expect("写临时配置");
        let config = ConfigStore::load(old.clone()).get().clone();
        assert_eq!(config.popup_anim_style, PopupAnimStyle::System);
        assert_eq!(config.osd.anim_style, OsdAnimStyle::Fade);

        // 存成 follow system 之后能原样读回来。
        let sys = dir.join("sys.json");
        std::fs::write(
            &sys,
            r#"{"config_rev": 63, "popup_anim_style": "system", "osd": {"anim_style": "system"}}"#,
        )
        .expect("写临时配置");
        let config = ConfigStore::load(sys.clone()).get().clone();
        assert!(config.popup_anim_style.is_system());
        assert!(config.osd.anim_style.is_system());
        assert_eq!(config.popup_anim_style.index(), 0, "System 是现在第一档");
        assert_eq!(PopupAnimStyle::from_index(0), PopupAnimStyle::System);
        assert_eq!(OsdAnimStyle::from_index(9), OsdAnimStyle::Fade, "认不出的按默认");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn rev60_release_sound_osd_follows_the_master() {
        let dir = std::env::temp_dir().join(format!("bolume-rev60-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);

        // 主开关关着的老配置：子开关跟着关。
        let off = dir.join("off.json");
        std::fs::write(&off, r#"{"config_rev": 59, "release_sound": false}"#)
            .expect("写临时配置");
        let config = ConfigStore::load(off.clone()).get().clone();
        assert!(!config.release_sound, "主开关保持用户的关");
        assert!(!config.release_sound_osd, "子开关跟着主开关关掉");
        assert_eq!(config.config_rev, CONFIG_REV);

        // 主开关开着的老配置：子开关跟着开（升级后 OSD 也响）。
        let on = dir.join("on.json");
        std::fs::write(&on, r#"{"config_rev": 59, "release_sound": true}"#)
            .expect("写临时配置");
        let config = ConfigStore::load(on.clone()).get().clone();
        assert!(config.release_sound_osd, "主开关开着的人顺理成章延到 OSD");

        // 全新安装：两个都是开的。
        let fresh = Config::default();
        assert!(fresh.release_sound, "新装松手提示音默认开");
        assert!(fresh.release_sound_osd, "新装 OSD 也跟着响");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 外观模式新默认：rev 59 起默认「跟随系统」，老配置也跟着搬过去。
    ///
    /// 强调色空着的语义也跟着变了（= 跟随 Windows 的强调色），那条不用搬数据 ——
    /// 老配置里空着的强调色本来就表示「跟着走」。
    #[test]
    fn rev59_defaults_appearance_mode_to_follow_system() {
        assert!(Config::default().theme_follow_system, "默认跟随系统");

        let dir = std::env::temp_dir().join(format!("bolume-rev59-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("config.json");
        // 老配置：固定一套主题（旧默认），两档槽位都是空的。
        let legacy = r#"{"config_rev": 58, "theme_follow_system": false,
            "theme": "Bolume Gray", "theme_light": "", "theme_dark": ""}"#;
        std::fs::write(&path, legacy).expect("写临时配置");
        let config = ConfigStore::load(path.clone()).get().clone();
        assert!(config.theme_follow_system, "老配置要跟着新默认切到跟随系统");
        // 主题预设没了：老配置里的主题名读进来就丢掉，外观只认底色 + 强调色。
        assert_eq!(
            config.theme_light_surface, DEFAULT_LIGHT_SURFACE,
            "空着的亮色档要填成默认底色"
        );
        assert_eq!(config.config_rev, CONFIG_REV);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 亮度滚轮：一格走 100/段数，除不尽向下取整，最低 1。
    #[test]
    fn brightness_scroll_step_is_a_percentage() {
        let mut config = Config::default();
        assert_eq!(config.brightness.scroll_step, 2, "默认一格 2%");

        config.brightness.scroll_step = 10;
        assert_eq!(config.brightness_scroll(50.0, 1.0), 60);

        // 越界值被夹回来，别让滑条变成死键。
        config.brightness.scroll_step = 0;
        assert_eq!(config.brightness_scroll(50.0, 1.0), 51);
        config.brightness.scroll_step = 1000;
        assert_eq!(config.brightness_scroll(50.0, 1.0), 100);
    }

    /// rev 28 之前存的是「段数」：读进来要换算成步进百分比，滚轮手感不变。
    #[test]
    fn legacy_brightness_segments_become_a_step() {
        let dir = std::env::temp_dir().join(format!("bolume-scroll-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join("config.json");

        // 50 段 → 一格 2%。
        std::fs::write(
            &path,
            r#"{"config_rev": 27, "brightness": {"enabled": true, "scroll_segments": 50}}"#,
        )
        .unwrap();
        let store = ConfigStore::load(path.clone());
        assert_eq!(store.get().brightness.scroll_step, 2);

        // 除不尽向下取整：60 段 → 1%（和旧代码算出来的完全一致）。
        std::fs::write(
            &path,
            r#"{"config_rev": 27, "brightness": {"enabled": true, "scroll_segments": 60}}"#,
        )
        .unwrap();
        let store = ConfigStore::load(path.clone());
        assert_eq!(store.get().brightness.scroll_step, 1);

        // 已经写了新字段的配置不能被老字段盖掉。
        std::fs::write(
            &path,
            r#"{"config_rev": 28, "brightness": {"scroll_step": 7, "scroll_segments": 50}}"#,
        )
        .unwrap();
        let store = ConfigStore::load(path.clone());
        assert_eq!(store.get().brightness.scroll_step, 7);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 亮度滚轮的位置换算：一格格走、两头截断，和音量的百分比分支一个手感。
    #[test]
    fn brightness_scroll_clamps_at_both_ends() {
        let mut config = Config::default();
        assert_eq!(config.brightness_scroll(50.0, 1.0), 52);
        assert_eq!(config.brightness_scroll(50.0, -2.0), 46);
        assert_eq!(config.brightness_scroll(99.0, 3.0), 100);
        assert_eq!(config.brightness_scroll(1.0, -3.0), 0);
        // 步进改了，一格的距离跟着变。
        config.brightness.scroll_step = 5;
        assert_eq!(config.brightness_scroll(50.0, 1.0), 55);
    }

    /// 亮度段的旧配置（没有滚轮两个字段）读进来要走默认：开、一格 2%。
    #[test]
    fn brightness_config_without_scroll_fields_uses_defaults() {
        let legacy =
            r#"{"config_rev": 11, "brightness": {"enabled": true, "step": 2, "presets": []}}"#;
        let config: Config = serde_json::from_str(legacy).expect("rev 11 的配置要能读");
        assert!(config.brightness.scroll_wheel);
        assert_eq!(
            config.brightness.scroll_step,
            DEFAULT_BRIGHTNESS_SCROLL_STEP
        );
    }

    /// 新的数组写法要能读回三组。
    #[test]
    fn hotkey_array_round_trips_three_slots() {
        let mut config = Config::default();
        config.hotkey_volume_up.set(0, Hotkey::new(2, 65));
        config.hotkey_volume_up.set(2, Hotkey::new(0, 0xB3));
        let json = serde_json::to_string(&config).unwrap();
        let back: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(back.hotkey_volume_up.get(0), Hotkey::new(2, 65));
        assert!(back.hotkey_volume_up.get(1).is_empty());
        assert_eq!(back.hotkey_volume_up.get(2), Hotkey::new(0, 0xB3));
    }

    /// OSD 的元素位置要能存能读，缺项回落到默认排布。
    #[test]
    fn osd_layout_round_trips_and_falls_back() {
        let mut config = Config::default();
        config.osd.set_part(OsdPart::Bar, 0.3, 0.8);
        // 越界的位置要夹回 0..1。
        config.osd.set_part(OsdPart::Icon, -1.0, 3.0);
        let json = serde_json::to_string(&config).unwrap();
        let back: Config = serde_json::from_str(&json).unwrap();
        assert_eq!(back.osd.part(OsdPart::Bar).x, 0.3);
        assert_eq!(back.osd.part(OsdPart::Bar).y, 0.8);
        assert_eq!(back.osd.part(OsdPart::Icon).x, 0.0);
        assert_eq!(back.osd.part(OsdPart::Icon).y, 0.95);
        // 把清单清空：每个元素都要回落到默认位置，不能变成 0,0 叠在一起。
        let mut empty = back.clone();
        empty.osd.layout.clear();
        let defaults = OsdConfig::default();
        for part in OsdPart::FIXED {
            assert_eq!(empty.osd.part(part), defaults.part(part));
        }
    }

    /// 位置选项的序号要能来回换，认不出的值退回默认的下居中。
    #[test]
    fn osd_anchor_index_round_trips() {
        let anchors = [
            OsdAnchor::TopLeft,
            OsdAnchor::TopCenter,
            OsdAnchor::TopRight,
            OsdAnchor::BottomLeft,
            OsdAnchor::BottomCenter,
            OsdAnchor::BottomRight,
            OsdAnchor::Custom,
            OsdAnchor::Cursor,
        ];
        for anchor in anchors {
            assert_eq!(OsdAnchor::from_index(anchor.index()), anchor);
        }
        assert_eq!(OsdAnchor::from_index(99), OsdAnchor::BottomCenter);
        assert!(OsdAnchor::Custom.is_custom());
        assert_eq!(OsdConfig::default().anchor, OsdAnchor::BottomCenter);
    }

    /// 每种功能单独开关，总开关优先。
    #[test]
    fn osd_function_switches_respect_master() {
        let mut config = Config::default();
        assert!(config.osd.shows(OsdFunction::Volume));
        config.osd.prev_track = false;
        assert!(!config.osd.shows(OsdFunction::PrevTrack));
        assert!(config.osd.shows(OsdFunction::NextTrack));
        config.osd.enabled = false;
        for function in OsdFunction::ALL {
            assert!(
                !config.osd.shows(function),
                "{function:?} 关掉总开关后都不该弹"
            );
        }
    }

    /// 老配置（rev 20）读进来：OSD 用默认值，黑名单里的全屏禁用默认关。
    #[test]
    fn rev20_config_gets_osd_defaults() {
        let legacy = r#"{"config_rev": 20, "scroll_wheel_step": 2}"#;
        let config: Config = serde_json::from_str(legacy).expect("rev 20 的配置要能读");
        assert!(config.osd.enabled);
        assert_eq!(config.osd.width, OsdConfig::default().width);
        assert_eq!(config.osd.layout.len(), OsdPart::FIXED.len());
        assert!(!config.app_block.fullscreen);
        assert!(config.app_block.entries.is_empty());
    }

    /// rev 22：还停在旧默认排版（320×96）的配置要整体换成新的小卡片，
    /// 自己拖过位置的布局一个字都不动。
    #[test]
    fn rev21_default_osd_layout_migrates_but_custom_keeps() {
        let dir = std::env::temp_dir().join(format!("bolume-osd-rev22-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");

        // 旧默认值：尺寸 320×96 + 四个元素都在老位置。
        std::fs::write(
            &path,
            r#"{"config_rev": 21, "osd": {"width": 320, "height": 96, "layout": [
                {"part": "icon", "x": 0.05, "y": 0.26},
                {"part": "title", "x": 0.20, "y": 0.14},
                {"part": "value", "x": 0.66, "y": 0.14},
                {"part": "bar", "x": 0.20, "y": 0.67}]}}"#,
        )
        .unwrap();
        let store = ConfigStore::load(path.clone());
        let defaults = OsdConfig::default();
        assert_eq!(store.get().osd.width, defaults.width);
        assert_eq!(store.get().osd.height, defaults.height);
        assert_eq!(store.get().osd.layout, defaults.layout);

        // 用户自己拖过音量条：尺寸和位置都保持原样。
        std::fs::write(
            &path,
            r#"{"config_rev": 21, "osd": {"width": 320, "height": 96, "layout": [
                {"part": "icon", "x": 0.05, "y": 0.26},
                {"part": "title", "x": 0.20, "y": 0.14},
                {"part": "value", "x": 0.66, "y": 0.14},
                {"part": "bar", "x": 0.28, "y": 0.62}]}}"#,
        )
        .unwrap();
        let custom = ConfigStore::load(path);
        assert_eq!(custom.get().osd.width, 320);
        assert_eq!(custom.get().osd.height, 96);
        assert!((custom.get().osd.part(OsdPart::Bar).x - 0.28).abs() < 1e-6);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 黑名单：程序按完整路径比，目录按前缀比（含子目录），都忽略大小写。
    #[test]
    fn app_block_matches_exe_and_folder() {
        let mut block = AppBlockConfig::default();
        assert!(!block.blocks_exe(r"C:\Tools\Quicker\Quicker.exe"));

        assert!(block.add(AppBlockKind::App, r"C:\Tools\Quicker\Quicker.exe"));
        assert!(block.blocks_exe(r"c:\tools\quicker\quicker.EXE"));
        assert!(!block.blocks_exe(r"C:\Tools\Quicker\QuickerStarter.exe"));
        // 重复的（哪怕大小写 / 斜杠写法不同）不再加第二条。
        assert!(!block.add(AppBlockKind::App, r"c:\tools\quicker/quicker.exe"));
        assert_eq!(block.entries.len(), 1);

        assert!(block.add(AppBlockKind::Dir, r"C:\Games\"));
        assert!(block.blocks_exe(r"C:\Games\Launcher.exe"));
        assert!(block.blocks_exe(r"C:\Games\bin\Game.exe"));
        assert!(!block.blocks_exe(r"C:\Games2\Game.exe"));
        assert_eq!(block.entries[1].path, r"C:\Games", "尾部斜杠要去掉");

        block.remove(0);
        assert_eq!(block.entries.len(), 1);
        assert!(!block.blocks_exe(r"C:\Tools\Quicker\Quicker.exe"));
    }

    /// rev 29：OSD 的「全屏时不弹」与快捷键的「全屏时禁用」搬进黑名单，任一为真就打开。
    #[test]
    fn rev28_fullscreen_switches_fold_into_app_block() {
        let dir = std::env::temp_dir().join(format!("bolume-block-rev29-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.json");

        std::fs::write(
            &path,
            r#"{"config_rev": 28, "hotkey_disable_fullscreen": true,
                "osd": {"disable_fullscreen": false}}"#,
        )
        .unwrap();
        let store = ConfigStore::load(path.clone());
        assert!(store.get().app_block.fullscreen, "快捷键那边开着就该搬过来");

        std::fs::write(
            &path,
            r#"{"config_rev": 28, "hotkey_disable_fullscreen": false,
                "osd": {"disable_fullscreen": true}}"#,
        )
        .unwrap();
        let store = ConfigStore::load(path.clone());
        assert!(store.get().app_block.fullscreen, "OSD 那边开着也算");

        std::fs::write(
            &path,
            r#"{"config_rev": 28, "hotkey_disable_fullscreen": false,
                "osd": {"disable_fullscreen": false}}"#,
        )
        .unwrap();
        let store = ConfigStore::load(path);
        assert!(!store.get().app_block.fullscreen);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 手动输入的 exe 名：只写 `DNF` 也认（补 `.exe`），装在任何目录都算。
    #[test]
    fn app_block_matches_by_exe_name_anywhere() {
        let mut block = AppBlockConfig::default();
        assert!(block.add_input("DNF"));
        assert_eq!(block.entries[0].kind, AppBlockKind::Name);
        assert_eq!(block.entries[0].path, "dnf.exe");
        assert_eq!(block.entries[0].name(), "dnf.exe");

        assert!(block.blocks_exe(r"E:\Game\DNF\DNF_CHINA\DNF\DNF.exe"));
        assert!(block.blocks_exe("c:/somewhere/else/DNF.EXE"));
        assert!(!block.blocks_exe(r"E:\Game\DNF\CrossProxy.exe"));
        assert!(!block.blocks_exe(""));

        // 写全名也一样，不重复加。
        assert!(!block.add_input("DNF.EXE"));
        assert_eq!(block.entries.len(), 1);
    }

    /// 手动输入带路径分隔符的当完整路径（引号也能粘）。
    #[test]
    fn app_block_input_tells_path_from_name() {
        let mut block = AppBlockConfig::default();
        assert!(block.add_input(r#""E:\Tools\Quicker\Quicker.exe""#));
        assert_eq!(block.entries[0].kind, AppBlockKind::App);
        assert!(block.blocks_exe(r"e:\tools\quicker\Quicker.exe"));

        assert!(
            !block.add_input(r"E:\Tools\Quicker\Quicker.exe"),
            "重复的不再加"
        );
        assert!(!block.add_input("   "), "空行不算一条");
        assert_eq!(block.entries.len(), 1);
    }

    /// 亮度面板宽度默认跟随音量面板；写了具体像素才自己说了算。
    /// 融合默认是关的，且位置落在横排那一轴（下方）。
    #[test]
    fn brightness_merge_defaults_to_off() {
        let config = Config::default();
        assert!(!config.brightness.merged);
        assert_eq!(config.brightness.merge_side, BrightnessMergeSide::Below);
        // 光开融合不够：亮度总开关没开就什么都不长。
        let mut config = config;
        config.brightness.merged = true;
        assert!(!config.brightness.merged_with_audio());
        config.brightness.enabled = true;
        assert!(config.brightness.merged_with_audio());
    }

    /// 位置选项和排版对不上时退回该轴默认：横排下方、竖排右侧。
    #[test]
    fn merge_side_falls_back_to_the_axis_default() {
        let mut config = Config::default();
        config.brightness.merge_side = BrightnessMergeSide::Left;
        assert_eq!(
            config.brightness.merge_side_of(false),
            BrightnessMergeSide::Below
        );
        assert_eq!(
            config.brightness.merge_side_of(true),
            BrightnessMergeSide::Left
        );

        config.brightness.merge_side = BrightnessMergeSide::Above;
        assert_eq!(
            config.brightness.merge_side_of(false),
            BrightnessMergeSide::Above
        );
        assert_eq!(
            config.brightness.merge_side_of(true),
            BrightnessMergeSide::Right
        );
    }

    #[test]
    fn brightness_width_follows_the_panel_unless_set() {
        let mut config = Config::default();
        assert_eq!(config.brightness.width, 0, "默认是「跟随」");
        assert_eq!(config.flyout_width(), DEFAULT_FLYOUT_WIDTH as f32);
        assert_eq!(config.brightness_width(), config.flyout_width());

        // 显式写了宽度：不跟着音量面板走。
        config.brightness.width = 200;
        assert_eq!(config.brightness_width(), 200.0);
        config.flyout_width = 500;
        assert_eq!(config.brightness_width(), 200.0);

        // 回到 0 又跟着走。
        config.brightness.width = 0;
        assert_eq!(config.brightness_width(), 500.0);

        // 手改配置写歪了（太窄 / 太宽）按边界算，不至于把界面画崩。
        config.flyout_width = 10;
        assert_eq!(config.flyout_width(), FLYOUT_WIDTH_RANGE.0 as f32);
        config.flyout_width = 5000;
        assert_eq!(config.flyout_width(), FLYOUT_WIDTH_RANGE.1 as f32);
        config.brightness.width = 1;
        assert_eq!(config.brightness_width(), 120.0);
    }

    /// 音量音效：老配置里没有这一节时读成「关 + 轻点 + 三手势全勾」，
    /// 存一轮名字也不丢（枚举按 kebab-case 写盘）。
    #[test]
    fn volume_sound_defaults_and_round_trips() {
        let legacy: Config = serde_json::from_str(r#"{"config_rev": 72}"#).expect("老配置要能读");
        let sound = &legacy.volume_sound;
        assert!(!sound.enabled, "新装 / 老配置一律不开");
        assert_eq!(sound.style, VolumeSoundStyle::Click);
        assert_eq!(sound.volume, 60);
        assert!(sound.on_hotkey && sound.on_wheel && sound.on_drag);
        assert!(sound.custom.is_empty());

        // 存一轮再读回来，选择与范围都得原样。
        let mut config = Config::default();
        config.volume_sound.enabled = true;
        config.volume_sound.style = VolumeSoundStyle::Chime;
        config.volume_sound.on_wheel = false;
        config.volume_sound.custom = "C:////sounds////tick.wav".to_string();
        let text = serde_json::to_string(&config).expect("要能写");
        let back: Config = serde_json::from_str(&text).expect("要能读回来");
        assert!(back.volume_sound.enabled);
        assert_eq!(back.volume_sound.style, VolumeSoundStyle::Chime);
        assert!(!back.volume_sound.on_wheel);
        assert_eq!(back.volume_sound.custom, "C:////sounds////tick.wav");

        // 分段选择按序号来回：顺序和 `ALL` 一致，超出范围落到第一个。
        for style in VolumeSoundStyle::ALL {
            assert_eq!(VolumeSoundStyle::from_index(style.index()), style);
        }
        assert_eq!(VolumeSoundStyle::from_index(99), VolumeSoundStyle::Click);
    }

    /// 三种手势的范围映射：快捷键 / 滚轮 / 拖拽各管各的，应用那几种跟主设备同类。
    #[test]
    fn volume_sound_scope_maps_every_gesture() {
        let mut sound = VolumeSoundConfig {
            enabled: true,
            on_hotkey: true,
            on_wheel: false,
            on_drag: false,
            ..Default::default()
        };
        assert!(sound.covers(VolumeGesture::Hotkey));
        assert!(sound.covers(VolumeGesture::AppHotkey));
        assert!(!sound.covers(VolumeGesture::HoverScroll));
        assert!(!sound.covers(VolumeGesture::AppDrag));

        sound.on_wheel = true;
        assert!(sound.covers(VolumeGesture::TrayScroll));
        assert!(sound.covers(VolumeGesture::AppHoverScroll));
        assert!(!sound.covers(VolumeGesture::Drag));

        sound.on_drag = true;
        assert!(sound.covers(VolumeGesture::Drag));
        assert!(sound.covers(VolumeGesture::AppDrag));

        sound.enabled = false;
        assert!(!sound.covers(VolumeGesture::Hotkey));
    }

    #[test]
    fn vertical_heights_are_optional_and_clamped() {
        let mut config = Config::default();
        // 0 = 自动 / 跟随，两边都是 None。
        assert_eq!(config.vertical_height, 0);
        assert_eq!(config.brightness.vertical_height, 0);
        assert_eq!(config.vertical_panel_height(), None);
        assert_eq!(config.brightness.vertical_panel_height(), None);

        // 写数字就按数字来。
        config.vertical_height = 260;
        config.brightness.vertical_height = 320;
        assert_eq!(config.vertical_panel_height(), Some(260.0));
        assert_eq!(config.brightness.vertical_panel_height(), Some(320.0));

        // 手改配置写太小 / 太大：钳进范围，别把推子压没。
        config.vertical_height = 5;
        assert_eq!(config.vertical_panel_height(), Some(VERTICAL_HEIGHT_MIN));
        config.brightness.vertical_height = 9999;
        assert_eq!(
            config.brightness.vertical_panel_height(),
            Some(VERTICAL_HEIGHT_RANGE.1 as f32)
        );

        // 老配置没有这两个字段，读出来是 0（自动 / 跟随）。
        let legacy: Config = serde_json::from_str("{}").expect("空配置也要能读");
        assert_eq!(legacy.vertical_height, 0);
        assert_eq!(legacy.brightness.vertical_height, 0);
    }

    #[test]
    fn layouts_default_to_horizontal_and_survive_a_round_trip() {
        let config = Config::default();
        assert_eq!(config.panel_layout, PanelLayout::Horizontal);
        assert!(!config.vertical_app_names, "竖向默认不写应用名");
        assert_eq!(config.brightness.panel_layout, PanelLayout::Horizontal);
        assert_eq!(config.brightness.name_mode, DisplayNameMode::Number);

        // 老配置文件里没有这两个字段，读出来必须是默认值而不是报错。
        let legacy: Config = serde_json::from_str("{}").expect("空配置也要能读");
        assert_eq!(legacy.panel_layout, PanelLayout::Horizontal);
        assert_eq!(legacy.brightness.name_mode, DisplayNameMode::Number);

        // 写进文件再读回来要对得上（serde 用的是 kebab-case）。
        let mut config = Config::default();
        config.panel_layout = PanelLayout::Vertical;
        config.vertical_app_names = true;
        config.brightness.panel_layout = PanelLayout::Vertical;
        config.brightness.name_mode = DisplayNameMode::FullName;
        let text = serde_json::to_string(&config).expect("序列化");
        assert!(text.contains("\"panel_layout\":\"vertical\""), "{text}");
        assert!(text.contains("\"name_mode\":\"full-name\""), "{text}");
        let back: Config = serde_json::from_str(&text).expect("反序列化");
        assert_eq!(back.panel_layout, PanelLayout::Vertical);
        assert!(back.vertical_app_names);
        assert_eq!(back.brightness.panel_layout, PanelLayout::Vertical);
        assert_eq!(back.brightness.name_mode, DisplayNameMode::FullName);
    }

    /// 「全屏例外」只在全屏这一条上生效：例外里的程序全屏照常工作，
    /// 但它在让路名单里该让路还是让路。
    #[test]
    fn fullscreen_exempt_only_cancels_the_fullscreen_rule() {
        let mut block = AppBlockConfig {
            fullscreen: true,
            ..Default::default()
        };
        block
            .fullscreen_exempt
            .push(AppBlockEntry::from_input("game.exe").expect("能认出来"));
        let chain = vec!["C:////Games////Game.exe".to_string()];
        assert!(block.exempts_fullscreen(&chain), "全屏例外应该命中");
        assert!(
            !block.blocks_exe("C:////Games////Game.exe"),
            "例外名单不是让路名单"
        );

        // 两份名单各管各的：加到让路名单里，前台照样让路。
        block
            .entries
            .push(AppBlockEntry::from_input("dnf.exe").expect("能认出来"));
        assert!(block.blocks_exe("C:////Games////DNF.EXE"));
        assert!(!block.exempts_fullscreen(&["C:////Games////DNF.EXE".to_string()]));
    }

    /// 老配置里没有 `fullscreen_exempt`：读进来是空名单，行为等于以前（全屏一律让路）。
    #[test]
    fn rev51_fullscreen_exempt_defaults_to_empty() {
        let config: Config = serde_json::from_str(
            r#"{"config_rev": 50, "app_block": {"fullscreen": true,
                "entries": [{"kind": "name", "path": "dnf.exe"}]}}"#,
        )
        .unwrap();
        let block = &config.app_block;
        assert!(block.fullscreen);
        assert!(block.fullscreen_exempt.is_empty());
        assert_eq!(block.entries.len(), 1);
        assert_eq!(
            config.config_rev, 50,
            "反序列化不改 rev（迁移在 store 里跑）"
        );
    }

    #[test]
    fn layout_indexes_match_the_settings_segments() {
        for layout in [PanelLayout::Horizontal, PanelLayout::Vertical] {
            assert_eq!(PanelLayout::from_index(layout.index()), layout);
        }
        for mode in [DisplayNameMode::Number, DisplayNameMode::FullName] {
            assert_eq!(DisplayNameMode::from_index(mode.index()), mode);
        }
        // 越界的下标回落到第一项，不会 panic。
        assert_eq!(PanelLayout::from_index(99), PanelLayout::Horizontal);
        assert_eq!(DisplayNameMode::from_index(99), DisplayNameMode::Number);
    }

    /// 页脚默认就是老样子：左边 WiFi / 蓝牙、右边响度均衡 / 音量合成器 / 设置 / 置顶。
    #[test]
    fn footer_default_matches_the_old_row() {
        let footer = FooterConfig::default();
        let left: Vec<&str> = footer
            .items
            .iter()
            .filter(|item| item.side.is_left())
            .map(|item| item.id.as_str())
            .collect();
        let right: Vec<&str> = footer
            .items
            .iter()
            .filter(|item| !item.side.is_left())
            .map(|item| item.id.as_str())
            .collect();
        assert_eq!(left, ["wifi", "bluetooth"]);
        assert_eq!(right, ["loudness", "mixer", "settings", "pin"]);
        assert!(footer.items.iter().all(|item| item.visible));
    }

    /// 组内换序：插到谁前面就是谁前面，拖回原处不算改动。
    #[test]
    fn footer_place_reorders_within_one_group() {
        let mut footer = FooterConfig::default();
        let right = |footer: &FooterConfig| -> Vec<String> {
            footer
                .items
                .iter()
                .filter(|item| !item.side.is_left())
                .map(|item| item.id.clone())
                .collect()
        };
        // 音量合成器拖到响度均衡前面。
        assert!(footer.place("mixer", FooterSide::Right, Some("loudness"), false));
        assert_eq!(right(&footer), ["mixer", "loudness", "settings", "pin"]);
        // 同一个落点再放一次：位置没变，就不算改动（不会为了空操作落一次盘）。
        assert!(!footer.place("mixer", FooterSide::Right, Some("loudness"), false));
        // 拖到自己身上同理：插到自己前面等于没动，不能顺势滑到组尾。
        assert!(!footer.place("mixer", FooterSide::Right, Some("mixer"), false));
        assert_eq!(right(&footer), ["mixer", "loudness", "settings", "pin"]);
        // 左侧那组一个字都没动。
        let left: Vec<String> = footer
            .items
            .iter()
            .filter(|item| item.side.is_left())
            .map(|item| item.id.clone())
            .collect();
        assert_eq!(left, ["wifi", "bluetooth"]);
    }

    /// 跨组搬家：`None` = 放到那一组的末尾。
    #[test]
    fn footer_place_moves_between_groups() {
        let mut footer = FooterConfig::default();
        let left = |footer: &FooterConfig| -> Vec<String> {
            footer
                .items
                .iter()
                .filter(|item| item.side.is_left())
                .map(|item| item.id.clone())
                .collect()
        };
        let right = |footer: &FooterConfig| -> Vec<String> {
            footer
                .items
                .iter()
                .filter(|item| !item.side.is_left())
                .map(|item| item.id.clone())
                .collect()
        };
        // 设置拖到左边那组的末尾。
        assert!(footer.place("settings", FooterSide::Left, None, false));
        assert_eq!(left(&footer), ["wifi", "bluetooth", "settings"]);
        assert_eq!(right(&footer), ["loudness", "mixer", "pin"]);

        // 蓝牙拖到右边那组的**最前面**。
        assert!(footer.place("bluetooth", FooterSide::Right, Some("loudness"), false));
        assert_eq!(left(&footer), ["wifi", "settings"]);
        assert_eq!(
            right(&footer),
            ["bluetooth", "loudness", "mixer", "pin"]
        );
    }

    /// 从「未放置」放回来 = 摆到右边末尾 + 取消隐藏。
    #[test]
    fn footer_place_can_restore_a_hidden_item() {
        let mut footer = FooterConfig::default();
        footer.set_visible("pin", false);
        assert!(!footer.is_visible("pin"));
        assert!(footer.place("pin", FooterSide::Right, None, true));
        assert!(footer.is_visible("pin"));
        assert_eq!(
            footer.items.last().map(|item| item.id.as_str()),
            Some("pin")
        );
        // 认不出来的 id：认了但不动，别 panic。
        assert!(!footer.place("gone", FooterSide::Left, None, true));
    }

    /// 老配置里没有 `footer` 字段：读进来就是默认那一排，不会凭空少几个按钮。
    #[test]
    fn footer_defaults_when_missing_from_config() {
        let old: Config = serde_json::from_str("{\"config_rev\": 71}").unwrap();
        assert_eq!(old.footer.items.len(), FOOTER_ITEMS.len());
        assert!(old.footer.is_visible("settings"));

        // 只存了一个按钮的老配置：认得出的保留，其余按注册表补齐。
        let mut partial: Config = serde_json::from_str(
            "{\"footer\":{\"items\":[{\"id\":\"pin\",\"visible\":false,\"side\":\"Right\"}]}}",
        )
        .unwrap();
        assert_eq!(partial.footer.items.len(), 1, "读的时候不补，补在 store 里");
        assert!(partial.footer.ensure_all());
        assert_eq!(partial.footer.items.len(), FOOTER_ITEMS.len());
        // 已有的那一条一个字都不动。
        assert!(!partial.footer.is_visible("pin"));
        // 再跑一次没有变化（幂等）。
        assert!(!partial.footer.ensure_all());
    }

    /// 认不出来的 id（手改过配置 / 从新版降级）留着不删，渲染时跳过。
    #[test]
    fn footer_keeps_unknown_ids() {
        let mut footer: FooterConfig = serde_json::from_str(
            "{\"items\":[{\"id\":\"gone\",\"visible\":true,\"side\":\"Left\"}]}",
        )
        .unwrap();
        footer.ensure_all();
        assert!(footer.index_of("gone").is_some());
        assert!(footer_spec("gone").is_none());
        assert_eq!(footer.items.len(), FOOTER_ITEMS.len() + 1);
    }
}
