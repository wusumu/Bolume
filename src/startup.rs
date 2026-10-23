//! 启动套用：把「上次记下的设备与音量状态」在启动那一次恢复回去。
//!
//! 只做一次：之后用户在面板里怎么拖、怎么切，都不会被这里再拉回去。
//! 设备侧（默认设备 / 开关名单 / 主音量）在第一份快照到手时就办完；
//! 应用音量要等那些应用自己开声（WASAPI 里才有会话），所以在启动后的
//! `APP_WINDOW` 之内，谁出现就给谁套一次。

use std::{
    collections::HashMap,
    time::{Duration, Instant},
};

use crate::{
    audio::{AudioCommand, AudioEngine, Snapshot},
    config::StartupProfile,
};

/// 应用音量的匹配窗口：启动后这么久之内冒出来的应用都会被套用一次。
const APP_WINDOW: Duration = Duration::from_secs(120);

pub struct Startup {
    profile: StartupProfile,
    started: Instant,
    /// 设备侧只做一次。
    devices_done: bool,
    /// 还没套用的应用音量：会话 key → 百分比。
    pending_apps: HashMap<String, u32>,
}

impl Startup {
    pub fn new(profile: StartupProfile) -> Self {
        let pending_apps = if profile.set_app_volumes {
            profile
                .app_volumes
                .iter()
                .map(|app| (app.key.clone(), app.percent.min(100)))
                .collect()
        } else {
            HashMap::new()
        };
        Self {
            profile,
            started: Instant::now(),
            devices_done: false,
            pending_apps,
        }
    }

    /// 每来一份快照就调一次。
    pub fn on_snapshot(&mut self, snapshot: &Snapshot, engine: &AudioEngine) {
        if !self.devices_done {
            self.devices_done = true;
            for command in device_commands(&self.profile, snapshot) {
                engine.send(command);
            }
        }
        self.apply_apps(snapshot, engine);
    }

    fn apply_apps(&mut self, snapshot: &Snapshot, engine: &AudioEngine) {
        for command in app_commands(&mut self.pending_apps, snapshot, self.started.elapsed()) {
            engine.send(command);
        }
    }
}

/// 应用音量要发的命令（纯函数，方便直接测）。
///
/// 匹配到就记进 `done` 并删掉，所以同一个应用只会被套用一次；
/// 差值小于 0.5% 的直接跳过，免得每次快照都白打一次 WASAPI。
fn app_commands(
    pending: &mut HashMap<String, u32>,
    snapshot: &Snapshot,
    elapsed: Duration,
) -> Vec<AudioCommand> {
    if pending.is_empty() {
        return Vec::new();
    }
    if elapsed > APP_WINDOW {
        // 窗口过了就不再等：免得半夜插进来一个从没启动过的应用被莫名其妙改了音量。
        pending.clear();
        return Vec::new();
    }
    let mut commands = Vec::new();
    let mut done = Vec::new();
    for app in &snapshot.apps {
        if app.is_system {
            continue;
        }
        let Some(percent) = pending.get(&app.key).copied() else {
            continue;
        };
        done.push(app.key.clone());
        if ((app.volume * 100.0) - percent as f32).abs() < 0.5 {
            continue;
        }
        commands.push(AudioCommand::SetAppVolume {
            key: app.key.clone(),
            volume: (percent as f32 / 100.0).clamp(0.0, 1.0),
            unmute: false,
        });
    }
    for key in done {
        pending.remove(&key);
    }
    commands
}

/// 设备侧要发的命令（纯函数，方便直接测）。
///
/// 顺序是有意的：先切默认设备、再动开关名单，最后才写主音量——
/// 主音量写的是「切过去之后那台设备」，所以它必须排在默认设备之后。
pub fn device_commands(profile: &StartupProfile, snapshot: &Snapshot) -> Vec<AudioCommand> {
    let mut commands = Vec::new();

    if profile.set_default_device && !profile.default_device_id.is_empty() {
        let target = profile.default_device_id.as_str();
        let usable = snapshot
            .devices
            .iter()
            .any(|device| device.enabled && device.id == target);
        if usable && snapshot.current_device_id.as_deref() != Some(target) {
            commands.push(AudioCommand::SetDefaultDevice {
                device_id: target.to_string(),
            });
        }
    }

    match profile.device_state_mode {
        // 禁用：这套功能整个关掉，一台都不碰。
        crate::config::DeviceStateMode::Disabled => {}
        // 手动：名单里每台按自己记的启用 / 禁用下发，名单外的保持系统原样。
        crate::config::DeviceStateMode::Manual => {
            for wanted in profile.device_list(profile.device_state_mode) {
                let Some(device) = snapshot
                    .devices
                    .iter()
                    .find(|device| device.id == wanted.id)
                else {
                    // 设备没插 / 已经被删掉：跳过，不要凭空造一台出来。
                    continue;
                };
                if device.enabled != wanted.enabled {
                    commands.push(AudioCommand::SetDeviceEnabled {
                        device_id: wanted.id.clone(),
                        enabled: wanted.enabled,
                    });
                }
            }
        }
        // 黑名单：在名单里 = 禁用，其它保持系统原样（不看每条的 enabled）。
        crate::config::DeviceStateMode::Blacklist => {
            for wanted in profile.device_list(profile.device_state_mode) {
                let Some(device) = snapshot
                    .devices
                    .iter()
                    .find(|device| device.id == wanted.id)
                else {
                    continue;
                };
                if device.enabled {
                    commands.push(AudioCommand::SetDeviceEnabled {
                        device_id: device.id.clone(),
                        enabled: false,
                    });
                }
            }
        }
        // 白名单：在名单里 = 启用，其它（含以后新插的）全部禁用。
        crate::config::DeviceStateMode::Whitelist => {
            let list = profile.device_list(profile.device_state_mode);
            // 名单是空的绝不能执行：那会把所有设备一起禁掉，直接没声音。
            // 宁可不动作，等用户先把设备加进白名单。
            if list.is_empty() {
                return commands;
            }
            for device in &snapshot.devices {
                let wanted = list.iter().any(|state| state.id == device.id);
                if device.enabled != wanted {
                    commands.push(AudioCommand::SetDeviceEnabled {
                        device_id: device.id.clone(),
                        enabled: wanted,
                    });
                }
            }
        }
    }

    if profile.set_master_volume {
        // 主音量写给谁：有记录默认设备就写它，否则写当前默认设备。
        let target = if profile.default_device_id.is_empty() {
            snapshot.current_device_id.clone()
        } else {
            Some(profile.default_device_id.clone())
        };
        if let Some(device_id) = target {
            let usable = snapshot
                .devices
                .iter()
                .any(|device| device.enabled && device.id == device_id);
            if usable {
                commands.push(AudioCommand::SetDeviceVolume {
                    device_id,
                    volume: profile.master_volume.min(100) as f32 / 100.0,
                    // 定死解除静音：开机后套用音量就是为了能听见声音，
                    // 上一轮关机时是静音状态也不该继续哑着。不给开关。
                    unmute: true,
                });
            }
        }
    }

    commands
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{audio::DeviceInfo, audio::DeviceKind, config::StartupDeviceState};

    fn snapshot() -> Snapshot {
        let device = |id: &str, name: &str, is_default: bool, enabled: bool| DeviceInfo {
            id: id.to_string(),
            name: name.to_string(),
            description: String::new(),
            is_default,
            enabled,
            volume: 0.3,
            muted: false,
            peak: 0.0,
            kind: DeviceKind::Speakers,
        };
        Snapshot {
            devices: vec![
                device("spk", "扬声器", true, true),
                device("hp", "耳机", false, true),
                device("off", "旧声卡", false, false),
            ],
            current_device_id: Some("spk".to_string()),
            apps: Vec::new(),
            apps_revision: 0,
            app_routing: false,
        }
    }

    /// 默认设备与主音量：切到记录的那台，并把音量写给**它**而不是当前默认设备。
    #[test]
    fn switches_default_device_before_master_volume() {
        let profile = StartupProfile {
            set_default_device: true,
            default_device_id: "hp".to_string(),
            set_master_volume: true,
            master_volume: 35,
            ..Default::default()
        };
        let commands = device_commands(&profile, &snapshot());
        assert_eq!(commands.len(), 2, "{commands:?}");
        match &commands[0] {
            AudioCommand::SetDefaultDevice { device_id } => assert_eq!(device_id, "hp"),
            other => panic!("第一条应该是切默认设备：{other:?}"),
        }
        match &commands[1] {
            AudioCommand::SetDeviceVolume {
                device_id,
                volume,
                unmute,
            } => {
                assert_eq!(device_id, "hp");
                assert!((volume - 0.35).abs() < 1e-6);
                assert!(unmute, "启动套用音量要固定解除静音");
            }
            other => panic!("第二条应该是写主音量：{other:?}"),
        }
    }

    /// 已经是默认设备就不要再切一次（少一次 PolicyConfig 调用）。
    #[test]
    fn skips_default_device_when_already_default() {
        let profile = StartupProfile {
            set_default_device: true,
            default_device_id: "spk".to_string(),
            ..Default::default()
        };
        assert!(device_commands(&profile, &snapshot()).is_empty());
    }

    /// 黑名单：名单里的设备一律禁用，名单外的不动（不看每条记录的 enabled）。
    #[test]
    fn blacklist_disables_listed_devices_only() {
        let profile = StartupProfile {
            device_state_mode: crate::config::DeviceStateMode::Blacklist,
            device_blacklist: vec![
                StartupDeviceState {
                    id: "hp".to_string(),
                    name: "耳机".to_string(),
                    // 占位值故意写成 true，证明黑名单不看这个字段。
                    enabled: true,
                },
                StartupDeviceState {
                    id: "ghost".to_string(),
                    name: "拔掉的".to_string(),
                    enabled: false,
                },
            ],
            ..Default::default()
        };
        let commands = device_commands(&profile, &snapshot());
        assert_eq!(commands.len(), 1, "{commands:?}");
        match &commands[0] {
            AudioCommand::SetDeviceEnabled { device_id, enabled } => {
                assert_eq!(device_id, "hp");
                assert!(!enabled);
            }
            other => panic!("黑名单只该禁用名单里的设备：{other:?}"),
        }
    }

    /// 各模式的名单互不串门：同一台设备写进白名单，切到黑名单不该看得见。
    #[test]
    fn each_mode_uses_its_own_list() {
        let mut profile = StartupProfile {
            device_state_mode: crate::config::DeviceStateMode::Blacklist,
            device_blacklist: vec![StartupDeviceState {
                id: "hp".to_string(),
                name: "耳机".to_string(),
                enabled: false,
            }],
            device_whitelist: vec![StartupDeviceState {
                id: "hp".to_string(),
                name: "耳机".to_string(),
                enabled: true,
            }],
            ..Default::default()
        };
        // 黑名单：把 hp 禁用掉。
        let commands = device_commands(&profile, &snapshot());
        assert_eq!(commands.len(), 1, "{commands:?}");
        match &commands[0] {
            AudioCommand::SetDeviceEnabled { device_id, enabled } => {
                assert_eq!(device_id, "hp");
                assert!(!enabled);
            }
            other => panic!("{other:?}"),
        }

        // 切到白名单：用的应该是白名单那份（只留 hp），于是被禁的是 spk。
        profile.device_state_mode = crate::config::DeviceStateMode::Whitelist;
        let commands = device_commands(&profile, &snapshot());
        assert_eq!(commands.len(), 1, "{commands:?}");
        match &commands[0] {
            AudioCommand::SetDeviceEnabled { device_id, enabled } => {
                assert_eq!(device_id, "spk");
                assert!(!enabled, "白名单外的一律禁用");
            }
            other => panic!("{other:?}"),
        }
    }

    /// 白名单：在名单里就留，其它（含新插的设备）一律禁用。
    #[test]
    fn whitelist_disables_everything_not_listed() {
        let mut snapshot = snapshot();
        // 新接进来的一台设备，从没记过名单。
        snapshot.devices.push(DeviceInfo {
            id: "new".to_string(),
            name: "新显示器".to_string(),
            description: String::new(),
            is_default: false,
            enabled: true,
            volume: 0.5,
            muted: false,
            peak: 0.0,
            kind: DeviceKind::Speakers,
        });
        let profile = StartupProfile {
            device_state_mode: crate::config::DeviceStateMode::Whitelist,
            device_whitelist: vec![StartupDeviceState {
                id: "spk".to_string(),
                name: "扬声器".to_string(),
                enabled: true,
            }],
            ..Default::default()
        };
        let commands = device_commands(&profile, &snapshot);
        // spk 已经是开的、off 本来就被禁用 → 都不用下发；只补 hp 和 new。
        let mut disabled: Vec<String> = Vec::new();
        for command in &commands {
            match command {
                AudioCommand::SetDeviceEnabled { device_id, enabled } => {
                    assert!(!enabled, "白名单模式只会禁用名单外的设备");
                    disabled.push(device_id.clone());
                }
                other => panic!("只该有设备开关命令：{other:?}"),
            }
        }
        disabled.sort();
        assert_eq!(disabled, vec!["hp", "new"], "{commands:?}");
    }

    /// 白名单是空的时候绝不能动手：否则所有设备一起被禁，直接没声音。
    #[test]
    fn empty_whitelist_is_a_no_op() {
        let profile = StartupProfile {
            device_state_mode: crate::config::DeviceStateMode::Whitelist,
            device_whitelist: Vec::new(),
            ..Default::default()
        };
        assert!(device_commands(&profile, &snapshot()).is_empty());
    }

    /// 「禁用」模式下名单里有内容也不该发任何命令。
    #[test]
    fn disabled_mode_ignores_the_list() {
        let profile = StartupProfile {
            device_state_mode: crate::config::DeviceStateMode::Disabled,
            device_states: vec![StartupDeviceState {
                id: "off".to_string(),
                name: "旧声卡".to_string(),
                enabled: true,
            }],
            ..Default::default()
        };
        assert!(device_commands(&profile, &snapshot()).is_empty());
    }

    /// 手动模式：逐台按记录的开关下发，只补差值。
    #[test]
    fn applies_only_changed_device_states() {
        let profile = StartupProfile {
            device_state_mode: crate::config::DeviceStateMode::Manual,
            device_states: vec![
                StartupDeviceState {
                    id: "off".to_string(),
                    name: "旧声卡".to_string(),
                    enabled: true,
                },
                StartupDeviceState {
                    id: "hp".to_string(),
                    name: "耳机".to_string(),
                    enabled: true,
                },
                StartupDeviceState {
                    id: "ghost".to_string(),
                    name: "拔掉的设备".to_string(),
                    enabled: false,
                },
            ],
            ..Default::default()
        };
        let commands = device_commands(&profile, &snapshot());
        assert_eq!(commands.len(), 1, "{commands:?}");
        match &commands[0] {
            AudioCommand::SetDeviceEnabled { device_id, enabled } => {
                assert_eq!(device_id, "off");
                assert!(*enabled);
            }
            other => panic!("应该是启用旧声卡：{other:?}"),
        }
    }

    /// 记录里的默认设备不在（拔了）就不写主音量到它身上。
    #[test]
    fn unplugged_default_device_is_skipped() {
        let profile = StartupProfile {
            set_default_device: true,
            default_device_id: "ghost".to_string(),
            set_master_volume: true,
            master_volume: 40,
            ..Default::default()
        };
        let commands = device_commands(&profile, &snapshot());
        // 默认设备切不了；主音量也不该退回去写给当前设备（用户明确指定了另一台）。
        assert!(commands.is_empty(), "{commands:?}");
    }

    /// 应用音量：只认 key 对得上、且值真的不一样的会话，而且只套一次。
    #[test]
    fn app_volumes_apply_once_when_app_shows_up() {
        let mut snap = snapshot();
        snap.apps.push(crate::audio::AppInfo {
            key: "exe:c:\\music.exe".to_string(),
            pid: 1234,
            name: "Music".to_string(),
            process_path: None,
            icon_path: None,
            volume: 0.9,
            muted: false,
            peak: 0.0,
            is_system: false,
            state: 1,
            device_id: None,
            session_devices: Vec::new(),
        });
        let mut pending = HashMap::from([("exe:c:\\music.exe".to_string(), 25u32)]);
        let commands = app_commands(&mut pending, &snap, Duration::from_secs(1));
        assert_eq!(commands.len(), 1, "{commands:?}");
        match &commands[0] {
            AudioCommand::SetAppVolume { key, volume, .. } => {
                assert_eq!(key, "exe:c:\\music.exe");
                assert!((volume - 0.25).abs() < 1e-6);
            }
            other => panic!("应该是写应用音量：{other:?}"),
        }
        assert!(pending.is_empty(), "套用过的应用要从待办里删掉");

        // 下一次同一份快照：什么都不该再发。
        assert!(app_commands(&mut pending, &snap, Duration::from_secs(2)).is_empty());
    }

    /// 已经就在目标值上的应用不要重复写。
    #[test]
    fn app_volume_already_at_target_is_skipped() {
        let mut snap = snapshot();
        snap.apps.push(crate::audio::AppInfo {
            key: "exe:c:\\music.exe".to_string(),
            pid: 1,
            name: "Music".to_string(),
            process_path: None,
            icon_path: None,
            volume: 0.25,
            muted: false,
            peak: 0.0,
            is_system: false,
            state: 1,
            device_id: None,
            session_devices: Vec::new(),
        });
        let mut pending = HashMap::from([("exe:c:\\music.exe".to_string(), 25u32)]);
        assert!(app_commands(&mut pending, &snap, Duration::from_secs(1)).is_empty());
        assert!(pending.is_empty());
    }

    /// 窗口过了就不再等，免得启动很久以后才打开的应用被改音量。
    #[test]
    fn app_window_expires() {
        let mut pending = HashMap::from([("exe:c:\\music.exe".to_string(), 25u32)]);
        let commands = app_commands(
            &mut pending,
            &snapshot(),
            APP_WINDOW + Duration::from_secs(1),
        );
        assert!(commands.is_empty());
        assert!(pending.is_empty());
    }
}
