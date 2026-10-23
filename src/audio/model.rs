//! 数据模型：跨线程传递的纯数据快照，不含任何 COM 指针。

use std::path::PathBuf;

/// 对数刻度的下限（dB），对应 EarTrumpet 设置项 `LogarithmicVolumeMinDb` 的默认值。
/// 引用自商店版 EarTrumpet 2.3（`AppSettings.get_LogarithmicVolumeMinDb` 默认 -40.0）。
pub const LOG_MIN_DB: f32 = -40.0;

/// 线性幅度 → dB。对应 EarTrumpet `FloatExtensions.LinearToLog`：`20 * log10(v)`。
pub fn amplitude_to_db(amplitude: f32) -> f32 {
    20.0 * amplitude.max(1e-7).log10()
}

/// dB → 线性幅度。对应 EarTrumpet `FloatExtensions.LogToLinear`：`10^(dB / 20)`。
pub fn db_to_amplitude(db: f32) -> f32 {
    10f32.powf(db / 20.0)
}

/// 滑条位置（0..1）→ dB。
///
/// EarTrumpet 对数模式下滑条本身就是一条 dB 刻度：`Minimum = LogarithmicVolumeMinDb`、
/// `Maximum = 0`（`VolumeSlider.UpdateVolumeRange`），所以位置和 dB 是线性关系。
pub fn display_to_db(display: f32, min_db: f32) -> f32 {
    min_db * (1.0 - display.clamp(0.0, 1.0))
}

/// 线性幅度（0..1）→ 滑条位置（0..1）。
/// 对应 EarTrumpet `FloatExtensions.LinearToLogNormalized`：`1 + dB / -minDb`。
pub fn to_display_volume(volume: f32, min_db: f32) -> f32 {
    if volume <= 0.0 {
        return 0.0;
    }
    let min_db = min_db.min(-1.0);
    (1.0 + amplitude_to_db(volume) / -min_db).clamp(0.0, 1.0)
}

/// 滑条位置（0..1）→ 线性幅度（0..1）。滑条拉到最底为静音。
pub fn to_log_volume(display: f32, min_db: f32) -> f32 {
    let display = display.clamp(0.0, 1.0);
    if display <= 0.0005 {
        return 0.0;
    }
    db_to_amplitude(display_to_db(display, min_db)).clamp(0.0, 1.0)
}

pub fn volume_to_percent(display: f32) -> u32 {
    (display * 100.0).round().clamp(0.0, 100.0) as u32
}

/// 滚轮/快捷键一格对应的滑条位移。
///
/// EarTrumpet 的步进是「显示单位」：对数模式下 `Volume += step`（单位 dB），
/// 线性模式下 `Volume += step`（单位百分比），所以位移要按刻度换算。
///
/// 两种刻度用**各自**的步进：同一格在线性模式下是固定的百分比行程，
/// 在对数模式下是固定的 dB 数，二者不能共用一个数字。
pub fn scroll_delta(percent_step: u32, log_step_db: f32, logarithmic: bool, min_db: f32) -> f32 {
    if logarithmic {
        log_step_db.max(crate::config::LOG_SCROLL_STEP_RANGE_DB.0) / min_db.abs().max(1.0)
    } else {
        percent_step.max(1) as f32 / 100.0
    }
}

/// 与滑条位置对应的 Windows 标量音量百分比。
///
/// 对数模式下位置不等于幅度，百分比必须按真实幅度算（EarTrumpet 的百分比读的就是标量音量）。
pub fn percent_from_display(display: f32, logarithmic: bool, min_db: f32) -> u32 {
    let amplitude = if logarithmic {
        to_log_volume(display, min_db)
    } else {
        display
    };
    (amplitude * 100.0).round().clamp(0.0, 100.0) as u32
}

/// 连 dB 一起给出的读数（托盘提示用）。
pub fn readings(display: f32, logarithmic: bool, min_db: f32) -> (String, u32) {
    let percent = percent_from_display(display, logarithmic, min_db);
    if !logarithmic {
        return (format!("{percent}%"), percent);
    }
    let amplitude = to_log_volume(display, min_db);
    if amplitude <= 0.0005 {
        return ("-∞ dB".to_string(), percent);
    }
    let mut db = amplitude_to_db(amplitude);
    if db > -0.05 {
        db = 0.0;
    }
    (format!("{db:.1} dB"), percent)
}

/// 面板上显示的刻度文字。
///
/// 对数模式下 EarTrumpet 显示 dB（`VolumeToStringConverter`：`{0:0.0}` + " dB"）；
/// 线性模式下照旧显示百分比。
pub fn volume_label(display: f32, logarithmic: bool, min_db: f32) -> String {
    if logarithmic {
        readings(display, true, min_db).0
    } else {
        format!("{}%", volume_to_percent(display))
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DeviceKind {
    Speakers,
    Headphones,
    Headset,
    Microphone,
    Digital,
    Unknown,
}

impl DeviceKind {
    /// 依据端点 FormFactor 猜测图标类型。
    pub fn from_form_factor(form_factor: i32) -> Self {
        // EndpointFormFactor: 0 RemoteNetworkDevice, 1 Speakers, 2 LineLevel,
        // 3 Headphones, 4 Microphone, 5 Headset, 6 Handset, 7 DigitalPassthrough,
        // 8 SPDIF, 9 DigitalAudioDisplayDevice, ...
        match form_factor {
            1 | 2 => Self::Speakers,
            3 => Self::Headphones,
            4 => Self::Microphone,
            5 | 6 => Self::Headset,
            7 | 8 | 9 => Self::Digital,
            _ => Self::Unknown,
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    pub description: String,
    /// 是否为当前默认设备。
    pub is_default: bool,
    /// 设备是否启用（被系统「禁用」的设备这里为 false，只能启用不能调音量）。
    pub enabled: bool,
    pub volume: f32,
    pub muted: bool,
    pub peak: f32,
    pub kind: DeviceKind,
}

#[derive(Clone, Debug, PartialEq)]
pub struct AppInfo {
    /// 会话实例 id，进程重启后会变化。
    pub key: String,
    pub pid: u32,
    pub name: String,
    pub process_path: Option<PathBuf>,
    pub icon_path: Option<PathBuf>,
    pub volume: f32,
    pub muted: bool,
    pub peak: f32,
    pub is_system: bool,
    /// 0 = Inactive, 1 = Active, 2 = Expired
    pub state: u32,
    /// 这个应用被单独指到了哪台播放设备；`None` = 跟随系统默认设备。
    ///
    /// 对应 Windows「设置 → 应用音量 → 输出设备」里改的那一项，按**进程**记录。
    pub device_id: Option<String>,
    /// 这些会话实际挂在哪些播放设备上（去重）。
    ///
    /// 和上面的 `device_id`（配置）不是一回事：改完设备后应用要重建音频流才会真迁过去，
    /// 中间这段时间两者会不一致。面板不用，探针用它排查「调到不了某个应用」。
    pub session_devices: Vec<String>,
}

impl AppInfo {
    pub fn is_playing(&self) -> bool {
        self.state == 1 && self.peak > 0.0005
    }

    /// 是否被单独指定了播放设备（面板上要给个标记）。
    ///
    /// 系统声音改不了（pid 0），就算读到值也不显示标记。
    pub fn is_routed(&self) -> bool {
        !self.is_system && self.device_id.is_some()
    }
}

#[derive(Clone, Debug, Default, PartialEq)]
pub struct Snapshot {
    pub devices: Vec<DeviceInfo>,
    pub current_device_id: Option<String>,
    pub apps: Vec<AppInfo>,
    /// 应用列表的世代号，用于区分「只是音量变了」与「列表结构变了」。
    pub apps_revision: u64,
    /// 这台系统能不能给单个应用指定播放设备（未公开接口拿不到时为 false）。
    pub app_routing: bool,
}

impl Snapshot {
    /// 「当前应用」：快捷键调应用音量时的目标。
    ///
    /// 正在发声（`is_playing`）的里挑最响的那个 —— 用户按音量键时，
    /// 心里想的多数是耳朵里最吵的那个；几个一起响时才需要这个优先级。
    /// **系统声音不算**：提示音经常恰好最响，抢走快捷键只会让人莫名其妙。
    /// 一个都没在响就退一步取第一个非系统应用，免得快捷键完全没反应。
    pub fn current_app(&self) -> Option<&AppInfo> {
        self.apps
            .iter()
            .filter(|app| !app.is_system && app.is_playing())
            .max_by(|a, b| a.peak.total_cmp(&b.peak))
            .or_else(|| self.apps.iter().find(|app| !app.is_system))
    }

    /// 键盘快捷键 / 全局滚轮调音量时的目标设备。
    ///
    /// **只挑一台**：Windows 给每台设备各存一份音量，把所有启用设备一起刷成同一个值，
    /// 用户切到另一台时会发现「怎么也被调了」。没有默认设备（理论上不会）才退到第一台启用的。
    pub fn hotkey_device(&self) -> Option<&DeviceInfo> {
        self.devices
            .iter()
            .find(|device| device.is_default)
            .or_else(|| self.devices.iter().find(|device| device.enabled))
    }

    /// 托盘右键菜单里那一段设备列表的「指纹」。
    ///
    /// 菜单是 Win32 原生弹出菜单，只能在内容变化时整块重建，所以需要判断
    /// 「这次快照和上次相比，菜单看到的东西变了吗」。
    ///
    /// 关键是要带上 `enabled`：被禁用的设备在快照里**排在启用设备后面**，
    /// 一台设备从禁用变启用，只是从后半段搬到前半段，**id 顺序常常完全不变**。
    /// 早先的指纹只拼 id，于是「在设置里新启用一台设备」被判定成没变化，
    /// 托盘右键里就一直少那一行，直到重启。
    pub fn device_menu_key(&self) -> String {
        let mut key = String::new();
        for device in &self.devices {
            key.push_str(&device.id);
            key.push(if device.is_default { '=' } else { ':' });
            key.push(if device.enabled { '+' } else { '-' });
            key.push_str(&device.name);
            key.push('\u{1}');
        }
        key
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 公式对齐商店版 EarTrumpet 2.3：
    /// 对数模式滑条 Minimum = LogarithmicVolumeMinDb(-40)、Maximum = 0，
    /// dB = 20log10(v)，位置 = 1 + dB/40。
    #[test]
    fn matches_eartrumpet_log_scale() {
        assert!((display_to_db(0.5, -40.0) + 20.0).abs() < 1e-4);
        assert!((to_log_volume(0.5, -40.0) - 0.1).abs() < 1e-5);
        assert_eq!(percent_from_display(0.5, true, -40.0), 10);
        assert_eq!(readings(0.5, true, -40.0).0, "-20.0 dB");

        assert!((amplitude_to_db(0.5) + 6.0206).abs() < 1e-3);
        assert!((to_display_volume(0.5, -40.0) - 0.84948).abs() < 1e-3);
        assert_eq!(readings(1.0, true, -40.0).0, "0.0 dB");
        assert_eq!(readings(0.0, true, -40.0).0, "-∞ dB");
        assert_eq!(readings(0.84, false, -40.0).0, "84%");

        // 滚轮步进是显示单位：对数模式 2 = 2 dB，线性模式 2 = 2% 行程。
        assert!((scroll_delta(2, 2.0, true, -40.0) - 0.05).abs() < 1e-6);
        assert!((scroll_delta(2, 2.0, false, -40.0) - 0.02).abs() < 1e-6);
        // 两种刻度的步进互相独立：改了百分比不该影响对数模式的 dB。
        assert!((scroll_delta(9, 1.0, true, -40.0) - 0.025).abs() < 1e-6);
        assert!((scroll_delta(9, 1.0, false, -40.0) - 0.09).abs() < 1e-6);
        // 下限 0.1 dB：再小也不让滚轮变成一格扫不动。
        assert!((scroll_delta(2, 0.0, true, -40.0) - 0.0025).abs() < 1e-6);
    }

    fn device(id: &str, name: &str, enabled: bool, is_default: bool) -> DeviceInfo {
        DeviceInfo {
            id: id.to_string(),
            name: name.to_string(),
            description: String::new(),
            is_default,
            enabled,
            volume: 0.0,
            muted: false,
            peak: 0.0,
            kind: DeviceKind::Speakers,
        }
    }

    /// 键盘音量键 / 全局滚轮只该动**一台**设备：另一台设备的音量是系统里各自记住的，
    /// 一起刷会让用户切过去时发现「怎么也被调了」。
    #[test]
    fn hotkey_volume_targets_only_the_default_device() {
        let snap = Snapshot {
            devices: vec![
                device("A", "显示器", true, false),
                device("B", "耳机", true, true),
            ],
            current_device_id: Some("B".into()),
            apps: Vec::new(),
            apps_revision: 0,
            app_routing: false,
        };
        assert_eq!(snap.hotkey_device().map(|d| d.id.as_str()), Some("B"));

        // 没有默认设备时退到第一台启用的，不能返回 None 让快捷键彻底失灵。
        let no_default = Snapshot {
            devices: vec![
                device("A", "显示器", true, false),
                device("B", "耳机", true, false),
            ],
            app_routing: false,
            ..Snapshot::default()
        };
        assert_eq!(no_default.hotkey_device().map(|d| d.id.as_str()), Some("A"));

        // 一台启用设备都没有：没有可调的目标。
        let none = Snapshot {
            devices: vec![device("A", "显示器", false, false)],
            ..Snapshot::default()
        };
        assert!(none.hotkey_device().is_none());
    }

    /// 在系统里新启用一台设备时，快照里它的位置只是从「禁用段」挪到「启用段」，
    /// id 顺序常常一模一样 —— 指纹必须能看出这个变化，否则托盘菜单不会重建。
    #[test]
    fn menu_key_notices_newly_enabled_device() {
        let default = device("A", "扬声器", true, true);
        let before = Snapshot {
            devices: vec![default.clone(), device("B", "Realtek", false, false)],
            current_device_id: Some("A".into()),
            apps: Vec::new(),
            apps_revision: 0,
            app_routing: false,
        };
        let after = Snapshot {
            devices: vec![default, device("B", "Realtek", true, false)],
            current_device_id: Some("A".into()),
            apps: Vec::new(),
            apps_revision: 0,
            app_routing: false,
        };
        assert_ne!(
            before.device_menu_key(),
            after.device_menu_key(),
            "启用了一台设备，托盘菜单应该重建"
        );
        // 内容一样时指纹必须稳定，否则每次快照都会白重建一遍菜单。
        assert_eq!(before.device_menu_key(), before.device_menu_key());
    }

    fn app(key: &str, is_system: bool, state: u32, peak: f32) -> AppInfo {
        AppInfo {
            key: key.to_string(),
            pid: 1,
            name: key.to_string(),
            process_path: None,
            icon_path: None,
            volume: 0.5,
            muted: false,
            peak,
            is_system,
            state,
            device_id: None,
            session_devices: Vec::new(),
        }
    }

    /// 被单独指定了设备的应用要能被认出来（面板打标记），系统声音不算。
    #[test]
    fn routed_apps_are_marked_but_system_sounds_never_are() {
        let mut music = app("music", false, 1, 0.2);
        assert!(!music.is_routed(), "跟随默认设备时不该有标记");
        music.device_id = Some("{0.0.0.00000000}.{X}".into());
        assert!(music.is_routed());

        let mut system = app("system", true, 1, 0.0);
        system.device_id = Some("{0.0.0.00000000}.{X}".into());
        assert!(!system.is_routed(), "系统声音改不了设备");
    }

    /// 「当前应用」：正在发声的里挑最响的；都不响就退到第一个非系统应用。
    #[test]
    fn current_app_prefers_loudest_playing() {
        let snapshot = Snapshot {
            devices: Vec::new(),
            current_device_id: None,
            apps: vec![
                // 系统声音总是排在最前面，但它不该抢走快捷键。
                app("system", true, 1, 0.9),
                app("idle", false, 1, 0.0),
                app("music", false, 1, 0.2),
                app("video", false, 1, 0.6),
            ],
            apps_revision: 0,
            app_routing: false,
        };
        assert_eq!(
            snapshot.current_app().map(|a| a.key.as_str()),
            Some("video")
        );

        // 一个都没在响：退到第一个非系统应用，而不是系统声音。
        let quiet = Snapshot {
            apps: vec![app("system", true, 1, 0.0), app("idle", false, 1, 0.0)],
            ..snapshot
        };
        assert_eq!(quiet.current_app().map(|a| a.key.as_str()), Some("idle"));

        // 空列表不能慌。
        let empty = Snapshot::default();
        assert!(empty.current_app().is_none());
    }

    /// 默认设备变了、设备改名了，菜单上显示的勾选和名字也得跟着变。
    #[test]
    fn menu_key_notices_default_and_name_changes() {
        let base = Snapshot {
            devices: vec![
                device("A", "扬声器", true, true),
                device("B", "Realtek", true, false),
            ],
            current_device_id: Some("A".into()),
            apps: Vec::new(),
            apps_revision: 0,
            app_routing: false,
        };
        let switched = Snapshot {
            devices: vec![
                device("A", "扬声器", true, false),
                device("B", "Realtek", true, true),
            ],
            current_device_id: Some("B".into()),
            ..base.clone()
        };
        let renamed = Snapshot {
            devices: vec![
                device("A", "ROG XG279Q", true, true),
                device("B", "Realtek", true, false),
            ],
            ..base.clone()
        };
        assert_ne!(base.device_menu_key(), switched.device_menu_key());
        assert_ne!(base.device_menu_key(), renamed.device_menu_key());
    }
}
