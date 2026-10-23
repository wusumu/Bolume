//! 音频引擎：独立工作线程持有全部 COM 对象，UI 线程只收发纯数据。

use std::{
    collections::HashSet,
    path::PathBuf,
    sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel},
    time::{Duration, Instant},
};

use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::Endpoints::{
    IAudioEndpointVolume, IAudioEndpointVolumeCallback, IAudioEndpointVolumeCallback_Impl,
    IAudioMeterInformation,
};
use windows::Win32::Media::Audio::{
    AUDIO_VOLUME_NOTIFICATION_DATA, DEVICE_STATE, DEVICE_STATE_ACTIVE, DEVICE_STATE_DISABLED,
    EDataFlow, ERole, IAudioSessionControl, IAudioSessionControl2, IAudioSessionManager2,
    IMMDevice, IMMDeviceEnumerator, IMMNotificationClient, IMMNotificationClient_Impl,
    ISimpleAudioVolume, eMultimedia, eRender,
};
use windows::Win32::System::Com::CLSCTX_ALL;
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_ACCESS_RIGHTS, PROCESS_QUERY_LIMITED_INFORMATION, WaitForSingleObject,
};
use windows::core::{GUID, Interface, PCWSTR};

use super::model::{AppInfo, DeviceInfo, DeviceKind, Snapshot};
use super::{
    appdevice::AppDevicePolicy,
    appinfo::AppInfoResolver,
    com::{
        self, ComApartment, PKEY_DEVICE_DEVICE_DESC, PKEY_DEVICE_FRIENDLY_NAME,
        PKEY_DEVICE_INTERFACE_FRIENDLY_NAME,
    },
    policy::PolicyConfig,
};

/// 界面可见时的采样间隔（≈30fps）。
const TICK_VISIBLE: Duration = Duration::from_millis(33);
/// 后台时的采样间隔。
const TICK_HIDDEN: Duration = Duration::from_millis(500);
/// 会话列表重扫间隔。可见时压得比较短，新开始播放的应用能马上出现在面板里。
const SESSION_SCAN_VISIBLE: Duration = Duration::from_millis(400);
const SESSION_SCAN_HIDDEN: Duration = Duration::from_millis(2500);
/// 设备枚举刷新间隔。
const DEVICE_SCAN_VISIBLE: Duration = Duration::from_millis(4000);
const DEVICE_SCAN_HIDDEN: Duration = Duration::from_millis(8000);
/// 每应用播放设备的刷新间隔。
///
/// 它只在别的程序（系统设置 / 其他混音器）改过路由时才变，查一次要走一趟 COM，
/// 没必要跟着 30fps 的采样跑。
const APP_DEVICE_SCAN: Duration = Duration::from_millis(1500);

/// 一次静音写入「应该落地」的等待上限。
///
/// `IAudioEndpointVolume::SetMute` 在 USB / 蓝牙端点上**同步阻塞**（实测约 740ms，
/// 命令要递到设备固件里走一圈才回来），返回之后读回来的值才是新的。所以写完先
/// 按意图对外显示这么久，过了这段再拿真实值来对账 —— 太快对账会读到写之前的值，
/// 面板就先跳回旧状态再跳回来。
const MUTE_APPLY_TIMEOUT: Duration = Duration::from_millis(1100);

/// 对不上账时最多陪它折腾这么久（之后交还真实值，免得跟别的混音器死磕）。
const MUTE_GIVE_UP: Duration = Duration::from_millis(3000);

#[derive(Debug)]
pub enum AudioCommand {
    SetVisible(bool),
    /// 托盘图标正在画律动：即使面板没开也要按可见时的频率采峰值。
    SetAnimating(bool),
    /// `unmute`：写完音量顺手把静音解掉（由设置里的「调音量时解除静音」决定）。
    SetDeviceVolume {
        device_id: String,
        volume: f32,
        unmute: bool,
    },
    SetDeviceMute {
        device_id: String,
        muted: bool,
    },
    /// `unmute`：写完音量顺手把这个应用解掉静音（由设置里「调音量时解除静音」决定）。
    SetAppVolume {
        key: String,
        volume: f32,
        unmute: bool,
    },
    SetAppMute {
        key: String,
        muted: bool,
    },
    SetDefaultDevice {
        device_id: String,
    },
    /// 把**单个应用**指到某台播放设备（`None` = 恢复跟随默认设备）。
    ///
    /// 走的是未公开的 `Windows.Media.Internal.AudioPolicyConfig`，按进程 id 落盘，
    /// 应用重启后依然有效。系统声音（pid 0）改不了，会被忽略。
    SetAppDevice {
        pid: u32,
        device_id: Option<String>,
    },
    /// 启用 / 禁用一台播放设备（走 IPolicyConfig::SetEndpointVisibility，
    /// 和系统「声音」面板里右键「禁用设备」是同一件事）。
    SetDeviceEnabled {
        device_id: String,
        enabled: bool,
    },
    /// 设备音量被系统改了（硬件音量键 / 系统音量 OSD / 其他混音器）。
    /// 只用来把音频线程叫醒，实际读数由随后的 tick() 完成。
    EndpointChanged,
    /// 设备集合或默认设备变了（系统设置里启用/禁用、插拔、切换默认设备）。
    DevicesChanged,
    /// 换应用图标的提取尺寸（16 / 32 / 64，跟着设置走）。
    SetIconSize(u32),
    /// 应用名首字母大写（`cloudmusic` → `Cloudmusic`），跟着设置走。
    SetNameCapitalize(bool),
    /// 设备最大音量限制：`device_id` 为空 = 所有设备，`limit` 为 `None` = 取消限制。
    ///
    /// 放在音频线程里做，是因为它必须是「最后一道闸」：滑条、滚轮、快捷键、
    /// 硬件音量键、别的混音器改音量，最终都会经过这里。
    SetVolumeCap {
        device_id: String,
        limit: Option<f32>,
    },
    Refresh,
    Shutdown,
}

#[derive(Debug, Clone)]
pub enum AudioEvent {
    Snapshot(Snapshot),
    Error(String),
}

pub struct AudioEngine {
    commands: Sender<AudioCommand>,
    events: async_channel::Receiver<AudioEvent>,
}

impl AudioEngine {
    pub fn spawn(icons_dir: PathBuf, icon_pixels: u32, name_capitalize: bool) -> Self {
        let (command_tx, command_rx) = channel();
        let (event_tx, event_rx) = async_channel::unbounded();

        // 设备音量回调要在音频线程里注册，所以把发送端再克隆一份带过去。
        let wake = command_tx.clone();
        std::thread::Builder::new()
            .name("bolume-audio".into())
            .spawn(move || {
                run_worker(
                    command_rx,
                    event_tx,
                    icons_dir,
                    icon_pixels,
                    name_capitalize,
                    wake,
                )
            })
            .expect("无法启动音频线程");

        Self {
            commands: command_tx,
            events: event_rx,
        }
    }

    pub fn send(&self, command: AudioCommand) {
        let _ = self.commands.send(command);
    }

    pub fn events(&self) -> async_channel::Receiver<AudioEvent> {
        self.events.clone()
    }
}

impl Drop for AudioEngine {
    fn drop(&mut self) {
        let _ = self.commands.send(AudioCommand::Shutdown);
    }
}

struct DeviceEntry {
    id: String,
    name: String,
    description: String,
    kind: DeviceKind,
    volume: IAudioEndpointVolume,
    /// 音量变化通知的注册句柄：留着它才能在刷新设备时注销（否则 WASAPI 自己持有引用）。
    notify: Option<IAudioEndpointVolumeCallback>,
    meter: Option<IAudioMeterInformation>,
    level: f32,
    muted: bool,
    peak: f32,
}

/// 被系统禁用的设备：没有可用的音量接口，只留登记信息，
/// 设置页据此显示「已禁用 / 启用」。
struct DisabledEntry {
    id: String,
    name: String,
    description: String,
    kind: DeviceKind,
}

/// 设备音量变化回调。
///
/// EarTrumpet 用的是同一套机制（`IAudioEndpointVolumeCallback` +
/// `RegisterControlChangeNotify`）：硬件音量键、系统音量 OSD、其他程序改音量都会
/// 立刻回调到这里。此前只靠后台 500ms 轮询，媒体键按下去托盘数字要等半秒才动。
#[windows::core::implement(IAudioEndpointVolumeCallback)]
struct EndpointNotify {
    /// mpsc 的 Sender 不是 Sync，回调可能从任意线程进来，所以套一层 Mutex。
    wake: std::sync::Mutex<Sender<AudioCommand>>,
}

impl IAudioEndpointVolumeCallback_Impl for EndpointNotify_Impl {
    fn OnNotify(&self, _data: *mut AUDIO_VOLUME_NOTIFICATION_DATA) -> windows::core::Result<()> {
        if let Ok(wake) = self.wake.lock() {
            // mpsc 通道是无界的，send 不会阻塞。
            let _ = wake.send(AudioCommand::EndpointChanged);
        }
        Ok(())
    }
}

/// 设备集合变化回调（EarTrumpet 的 `DeviceNotificationClient` 同款）。
///
/// 之前设备列表纯靠每 4~8 秒轮询一次 `EnumAudioEndpoints`，所以在系统「声音」
/// 设置里新启用一台设备后，托盘右键菜单最多要等 8 秒才出现那一行。
/// 注册这个回调后，系统一改设备状态就立刻叫醒音频线程重扫。
#[windows::core::implement(IMMNotificationClient)]
struct DeviceNotify {
    wake: std::sync::Mutex<Sender<AudioCommand>>,
}

impl DeviceNotify {
    fn wake(&self) {
        if let Ok(wake) = self.wake.lock() {
            let _ = wake.send(AudioCommand::DevicesChanged);
        }
    }
}

impl IMMNotificationClient_Impl for DeviceNotify_Impl {
    fn OnDeviceStateChanged(
        &self,
        _device_id: &PCWSTR,
        _new_state: DEVICE_STATE,
    ) -> windows::core::Result<()> {
        self.wake();
        Ok(())
    }

    fn OnDeviceAdded(&self, _device_id: &PCWSTR) -> windows::core::Result<()> {
        self.wake();
        Ok(())
    }

    fn OnDeviceRemoved(&self, _device_id: &PCWSTR) -> windows::core::Result<()> {
        self.wake();
        Ok(())
    }

    fn OnDefaultDeviceChanged(
        &self,
        _flow: EDataFlow,
        _role: ERole,
        _device_id: &PCWSTR,
    ) -> windows::core::Result<()> {
        self.wake();
        Ok(())
    }

    /// 设备属性变化（改名、换图标）跟 EarTrumpet 一样忽略：
    /// 这个回调触发得很碎，跟着重扫设备会让音频线程一直在忙。
    /// 名字变化 8 秒内的轮询会补上。
    fn OnPropertyValueChanged(
        &self,
        _device_id: &PCWSTR,
        _key: &windows::Win32::Foundation::PROPERTYKEY,
    ) -> windows::core::Result<()> {
        Ok(())
    }
}

struct AppEntry {
    key: String,
    pid: u32,
    name: String,
    process_path: Option<PathBuf>,
    icon_path: Option<PathBuf>,
    is_system: bool,
    /// 同一个 exe 可能开多条会话（Discord 的主进程与语音进程、浏览器的多个标签页），
    /// EarTrumpet 把它们合并成一行，音量写入时对所有会话一起生效。
    volumes: Vec<ISimpleAudioVolume>,
    meters: Vec<IAudioMeterInformation>,
    /// 这些会话分别挂在哪些播放设备上（去重）。
    ///
    /// 会话是挂在设备上的：应用被指到别的设备后，它的会话只存在于那台设备。
    /// 面板不用这个，但排查「调到不了某个应用」时要靠它看清会话到底在哪。
    session_devices: Vec<String>,
    level: f32,
    muted: bool,
    peak: f32,
    state: u32,
}

struct Worker {
    enumerator: IMMDeviceEnumerator,
    policy: Option<PolicyConfig>,
    /// 每应用的播放设备（未公开的 AudioPolicyConfig）。拿不到就整组功能降级。
    app_device: Option<AppDevicePolicy>,
    /// pid → 该应用被指定的设备。查一次 COM 要几微秒，别每帧都问。
    app_devices: std::collections::HashMap<u32, Option<String>>,
    last_app_device_scan: Instant,
    resolver: AppInfoResolver,
    devices: Vec<DeviceEntry>,
    /// 被禁用的播放设备（音量接口拿不到，只记录信息）。
    disabled_devices: Vec<DisabledEntry>,
    apps: Vec<AppEntry>,
    current_id: Option<String>,
    apps_revision: u64,
    visible: bool,
    /// 动态托盘图标在看峰值：面板没开也得按 33ms 采一次。
    animating: bool,
    last_session_scan: Instant,
    last_device_scan: Instant,
    /// 设备音量回调的落地端：回调一来就往这里塞一条 EndpointChanged。
    wake: Sender<AudioCommand>,
    /// 快照出口。
    ///
    /// 正常是每拍由主循环推一次，但静音写入会同步阻塞几百毫秒（USB/蓝牙端点上
    /// 实测 ~740ms），这期间主循环发不出快照 —— 于是写完之前就得先把「意图」推
    /// 出去，否则面板会拿着上一次的旧状态画到写入返回为止。
    events: async_channel::Sender<AudioEvent>,
    /// 最大音量限制：(绑定的设备 id（空 = 所有设备）, 上限 0..1)。
    cap: Option<(String, f32)>,
    /// 刚写下去的静音意图：设备 id → 守门记录。
    ///
    /// 不挂在 `DeviceEntry` 上：设备列表重建（插拔、启用禁用、切默认）时条目会
    /// 重新造，挂在条目上的守门会跟着丢 —— 而那次重建恰好也可能是因为静音变了
    /// 触发的，丢了就没人把它按回去。
    mute_guard: std::collections::HashMap<String, MuteGuard>,
}

/// 一次静音写入的守门记录。
#[derive(Clone, Copy)]
struct MuteGuard {
    /// 用户最后要的状态。
    want: bool,
    /// 最近一次真正写下去的时刻（用来判断「还没到点」还是「该重写了」）。
    sent_at: Instant,
    /// 这一轮守门是什么时候开始的（用来兜底放弃，别无限重写）。
    since: Instant,
}

fn run_worker(
    commands: Receiver<AudioCommand>,
    events: async_channel::Sender<AudioEvent>,
    icons_dir: PathBuf,
    icon_pixels: u32,
    name_capitalize: bool,
    wake: Sender<AudioCommand>,
) {
    let _apartment = ComApartment::init_mta();
    let enumerator = match com::create_device_enumerator() {
        Ok(enumerator) => enumerator,
        Err(error) => {
            let _ = events.send_blocking(AudioEvent::Error(format!(
                "无法初始化音频设备枚举器：{error}"
            )));
            return;
        }
    };

    let mut worker = Worker {
        enumerator,
        policy: PolicyConfig::new(),
        app_device: AppDevicePolicy::new(),
        app_devices: std::collections::HashMap::new(),
        last_app_device_scan: Instant::now() - Duration::from_secs(10),
        resolver: AppInfoResolver::new(icons_dir, icon_pixels, name_capitalize),
        devices: Vec::new(),
        disabled_devices: Vec::new(),
        apps: Vec::new(),
        current_id: None,
        apps_revision: 0,
        visible: false,
        animating: false,
        last_session_scan: Instant::now() - Duration::from_secs(10),
        last_device_scan: Instant::now() - Duration::from_secs(10),
        wake,
        events: events.clone(),
        cap: None,
        mute_guard: std::collections::HashMap::new(),
    };

    // 设备集合变化（启用/禁用、插拔、切默认）由系统主动通知，不再等下一次轮询。
    let device_notify: IMMNotificationClient = DeviceNotify {
        wake: std::sync::Mutex::new(worker.wake.clone()),
    }
    .into();
    let notify_registered = unsafe {
        worker
            .enumerator
            .RegisterEndpointNotificationCallback(&device_notify)
    }
    .is_ok();
    if !notify_registered {
        // 注册失败不影响功能，只是退回到 4~8 秒的设备轮询。
        eprintln!("设备变化通知注册失败，设备列表将按轮询刷新");
    }

    worker.refresh_devices();
    worker.rescan_sessions();
    worker.refresh_levels();
    let _ = events.send_blocking(AudioEvent::Snapshot(worker.snapshot()));

    loop {
        let timeout = if worker.visible || worker.animating {
            TICK_VISIBLE
        } else {
            TICK_HIDDEN
        };
        match commands.recv_timeout(timeout) {
            Ok(AudioCommand::Shutdown) => break,
            Ok(command) => {
                worker.handle_burst(&commands, command);
            }
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }

        worker.tick();
        if events
            .send_blocking(AudioEvent::Snapshot(worker.snapshot()))
            .is_err()
        {
            break;
        }
    }

    if notify_registered {
        let _ = unsafe {
            worker
                .enumerator
                .UnregisterEndpointNotificationCallback(&device_notify)
        };
    }
}

impl Worker {
    /// 处理一条命令，并把队列里紧随其后的同类音量命令合并。
    ///
    /// 拖动滑条时每帧都会发一条 SetVolume，USB DAC / 蓝牙设备每次写入都有
    /// 几十毫秒开销，逐条执行会明显排队、越拖越滞后。这里只保留同一个目标
    /// 的最后一次设定值，其余命令照原顺序处理。
    fn handle_burst(&mut self, commands: &Receiver<AudioCommand>, first: AudioCommand) {
        let mut pending = first;
        let mut deferred: Vec<AudioCommand> = Vec::new();

        while let Ok(next) = commands.try_recv() {
            // 设备音量回调只是「叫醒」用：紧接着就会 tick + 发快照，
            // 直接丢掉，免得多条 EndpointChanged 打断音量命令的合并。
            if matches!(next, AudioCommand::EndpointChanged) {
                continue;
            }
            if deferred.is_empty() {
                let same_target = match (&pending, &next) {
                    (
                        AudioCommand::SetDeviceVolume { device_id: a, .. },
                        AudioCommand::SetDeviceVolume { device_id: b, .. },
                    ) => a == b,
                    (
                        AudioCommand::SetAppVolume { key: a, .. },
                        AudioCommand::SetAppVolume { key: b, .. },
                    ) => a == b,
                    // 静音同理，而且这边更值得合并：写一次要同步阻塞约 740ms，
                    // 连按五下逐条写会让设备在「静音 / 不静音」之间来回趟三秒多，
                    // 面板就算一直按意图画，最后也得跟着这串中间态抖。
                    // 只留最后那一个意图，等于「用户连按完 = 按最后一下」。
                    (
                        AudioCommand::SetDeviceMute { device_id: a, .. },
                        AudioCommand::SetDeviceMute { device_id: b, .. },
                    ) => a == b,
                    _ => false,
                };
                if same_target {
                    // 同一目标的两条音量命令合并成一条（只写最后那个值），
                    // 但「要不要解除静音」不能跟着丢：前面那条要解除，就还得解除。
                    let unmute = match (&pending, &next) {
                        (
                            AudioCommand::SetDeviceVolume { unmute: a, .. },
                            AudioCommand::SetDeviceVolume { unmute: b, .. },
                        ) => *a || *b,
                        _ => false,
                    };
                    pending = match next {
                        AudioCommand::SetDeviceVolume {
                            device_id, volume, ..
                        } => AudioCommand::SetDeviceVolume {
                            device_id,
                            volume,
                            unmute,
                        },
                        other => other,
                    };
                    continue;
                }
            }
            deferred.push(next);
            if deferred.len() >= 64 {
                break;
            }
        }

        self.handle(pending);
        for command in deferred {
            self.handle(command);
        }
    }

    fn handle(&mut self, command: AudioCommand) {
        match command {
            AudioCommand::SetAnimating(animating) => {
                self.animating = animating;
            }
            AudioCommand::SetVisible(visible) => {
                self.visible = visible;
            }
            // 音量已经被系统改了，tick() 会重新读一遍，这里不需要额外动作。
            AudioCommand::EndpointChanged => {}
            // 设备集合变了必须立刻重扫：托盘菜单和面板里的设备列表都靠它。
            AudioCommand::DevicesChanged => {
                self.refresh_devices();
                self.last_device_scan = Instant::now();
                self.last_session_scan = Instant::now() - Duration::from_secs(10);
            }
            AudioCommand::SetDeviceVolume {
                device_id,
                volume,
                unmute,
            } => {
                let Some(index) = self.devices.iter().position(|d| d.id == device_id) else {
                    return;
                };
                let limit = self
                    .cap
                    .as_ref()
                    .filter(|(bound, _)| bound.is_empty() || *bound == device_id)
                    .map(|(_, limit)| *limit)
                    .unwrap_or(1.0);
                let level = volume.clamp(0.0, limit);
                let context = GUID::zeroed();
                let written = unsafe {
                    self.devices[index]
                        .volume
                        .SetMasterVolumeLevelScalar(level, &context)
                }
                .is_ok();
                if written {
                    self.devices[index].level = level;
                    // 不再顺手把 `muted` 按音量改掉：静音是静音，音量是音量。
                    // 早先这里写 `muted = level <= 0.0`，静音状态下拖滑条会把静音标记
                    // 先抹掉、下一圈轮询又读回 true，滑条就看着「回弹」了一下。
                    // 解除静音放在写完音量之后：先解再写的话，Windows 会有一小段时间
                    // 按旧音量出声（音量是 0 时那一下就是「闪一下声音」）。
                    if unmute && level > 0.0 && self.devices[index].muted {
                        // 和静音按钮同款：这条写入同样会阻塞几百毫秒，先把意图
                        // 置上并推一次快照，别让喇叭图标拖到写入返回才变。
                        self.devices[index].muted = false;
                        let snapshot = self.snapshot();
                        let _ = self.events.send_blocking(AudioEvent::Snapshot(snapshot));
                        let ok =
                            unsafe { self.devices[index].volume.SetMute(false, &context) }.is_ok();
                        if ok {
                            self.note_mute_guard(&device_id, false, Instant::now());
                        }
                    }
                }
            }
            AudioCommand::SetDeviceMute { device_id, muted } => {
                let Some(index) = self.devices.iter().position(|d| d.id == device_id) else {
                    return;
                };
                let previous = self.devices[index].muted;
                // 先把意图落到快照上并推出去，再去做那次写入。
                //
                // `SetMute` 在这个端点上同步阻塞约 740ms（USB/蓝牙设备把命令往固件
                // 里递是同步的），这期间音频线程既不推快照也不处理队列里的下一条
                // 命令。写完再改状态的话，面板会拿着上一次的旧值一直画到写入返回，
                // 用户看到的「跳一圈」就是这段空白里露出来的旧状态。
                self.devices[index].muted = muted;
                let snapshot = self.snapshot();
                let _ = self.events.send_blocking(AudioEvent::Snapshot(snapshot));

                let context = GUID::zeroed();
                let ok = unsafe { self.devices[index].volume.SetMute(muted, &context) }.is_ok();
                if ok {
                    // 写入不一定立刻落地：先按意图对外显示，等它对账
                    // （见 `Worker::mute_guard`）。
                    self.note_mute_guard(&device_id, muted, Instant::now());
                } else {
                    self.devices[index].muted = previous;
                }
            }
            AudioCommand::SetAppVolume {
                key,
                volume,
                unmute,
            } => {
                if let Some(app) = self.apps.iter_mut().find(|a| a.key == key) {
                    let level = volume.clamp(0.0, 1.0);
                    let context = GUID::zeroed();
                    let mut applied = false;
                    for volume in &app.volumes {
                        if unsafe { volume.SetMasterVolume(level, &context) }.is_ok() {
                            applied = true;
                        }
                    }
                    if applied {
                        app.level = level;
                        // 和主音量一样：别在这里动 `muted`（会跟轮询打架、看着像回弹）。
                        // 真要解静音就显式解一次。
                        if unmute && level > 0.0 {
                            for volume in &app.volumes {
                                let _ = unsafe { volume.SetMute(false, &context) };
                            }
                            app.muted = false;
                        }
                    }
                }
            }
            AudioCommand::SetAppMute { key, muted } => {
                if let Some(app) = self.apps.iter_mut().find(|a| a.key == key) {
                    let context = GUID::zeroed();
                    let mut applied = false;
                    for volume in &app.volumes {
                        if unsafe { volume.SetMute(muted, &context) }.is_ok() {
                            applied = true;
                        }
                    }
                    if applied {
                        app.muted = muted;
                    }
                }
            }
            AudioCommand::SetDefaultDevice { device_id } => {
                if let Some(policy) = &self.policy {
                    let _ = policy.set_default_endpoint(&device_id, eMultimedia);
                } else if let Some(policy) = PolicyConfig::new() {
                    let _ = policy.set_default_endpoint(&device_id, eMultimedia);
                    self.policy = Some(policy);
                }
                self.current_id = Some(device_id);
                self.refresh_devices();
                self.last_session_scan = Instant::now() - Duration::from_secs(10);
            }
            AudioCommand::SetAppDevice { pid, device_id } => {
                let policy = match self.app_device.take() {
                    Some(policy) => policy,
                    None => match AppDevicePolicy::new() {
                        Some(policy) => policy,
                        None => {
                            eprintln!("这台系统没有 AudioPolicyConfig，应用级设备切换不可用");
                            return;
                        }
                    },
                };
                if policy.set_device(pid, device_id.as_deref()) {
                    // 立刻读一次：正常情况下系统已经记住了新值。
                    // 拿不到就等下一轮扫描（系统重建会话需要一点时间）。
                    let current = policy.device_of(pid);
                    self.app_devices.insert(pid, current);
                } else {
                    eprintln!("把 pid {pid} 的输出设备改到 {device_id:?} 失败");
                }
                self.app_device = Some(policy);
                self.apps_revision = self.apps_revision.wrapping_add(1);
            }
            AudioCommand::SetDeviceEnabled { device_id, enabled } => {
                let policy = match self.policy.take() {
                    Some(policy) => policy,
                    None => match PolicyConfig::new() {
                        Some(policy) => policy,
                        None => return,
                    },
                };
                match policy.set_endpoint_visibility(&device_id, enabled) {
                    Ok(()) => {
                        // 启用/禁用会让设备在 ACTIVE / DISABLED 两个集合之间搬家，
                        // 必须整体重扫一遍，否则列表会同时缺一条、多一条。
                        self.refresh_devices();
                        self.last_device_scan = Instant::now();
                        self.last_session_scan = Instant::now() - Duration::from_secs(10);
                    }
                    Err(error) => eprintln!("切换设备启用状态失败（{device_id}）：{error}"),
                }
                self.policy = Some(policy);
            }
            AudioCommand::SetVolumeCap { device_id, limit } => {
                self.cap = limit.map(|limit| (device_id, limit.clamp(0.0, 1.0)));
                // 立刻压一次：不用等下一帧，界面上的读数和滑条马上就是上限值。
                self.enforce_cap();
            }
            AudioCommand::Shutdown => {}
            AudioCommand::Refresh => {
                self.refresh_devices();
                self.rescan_sessions();
            }
            AudioCommand::SetIconSize(pixels) => {
                if self.resolver.set_icon_pixels(pixels) {
                    // 图标路径变了（缓存名带尺寸），旧的会话记录全部作废重扫。
                    self.apps.clear();
                    self.apps_revision = self.apps_revision.wrapping_add(1);
                    self.last_session_scan = Instant::now() - Duration::from_secs(10);
                }
            }
            AudioCommand::SetNameCapitalize(on) => {
                if self.resolver.set_name_capitalize(on) {
                    // 名字是随解析结果一起缓存的，清掉才有新名字。
                    self.apps.clear();
                    self.apps_revision = self.apps_revision.wrapping_add(1);
                    self.last_session_scan = Instant::now() - Duration::from_secs(10);
                }
            }
        }
    }

    fn tick(&mut self) {
        if self.last_app_device_scan.elapsed() >= APP_DEVICE_SCAN {
            self.refresh_app_devices();
        }

        if self.last_device_scan.elapsed()
            >= if self.visible {
                DEVICE_SCAN_VISIBLE
            } else {
                DEVICE_SCAN_HIDDEN
            }
        {
            self.last_device_scan = Instant::now();
            self.refresh_devices();
            self.last_session_scan = Instant::now() - Duration::from_secs(10);
        }

        if self.last_session_scan.elapsed()
            >= if self.visible {
                SESSION_SCAN_VISIBLE
            } else {
                SESSION_SCAN_HIDDEN
            }
        {
            self.last_session_scan = Instant::now();
            self.rescan_sessions();
        }

        self.refresh_levels();
    }

    fn current_flow(&self) -> EDataFlow {
        eRender
    }

    fn refresh_devices(&mut self) {
        let flow = self.current_flow();
        let default_id = unsafe {
            self.enumerator
                .GetDefaultAudioEndpoint(flow, ERole(1))
                .ok()
                .and_then(|device| com::device_id(&device))
        };

        let collection = unsafe {
            match self
                .enumerator
                .EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE)
            {
                Ok(collection) => collection,
                Err(_) => return,
            }
        };
        let count = unsafe { collection.GetCount() }.unwrap_or(0);

        // 旧条目先摘下来：同一个设备的下次刷新直接复用它的 COM 对象与音量回调注册，
        // 不然每 4~8 秒重新 Activate 一次会让硬件音量键的通知短暂断链。
        let mut previous = std::mem::take(&mut self.devices);
        let mut devices = Vec::with_capacity(count as usize);
        for index in 0..count {
            let Ok(device) = (unsafe { collection.Item(index) }) else {
                continue;
            };
            let Some(id) = (unsafe { com::device_id(&device) }) else {
                continue;
            };

            let name = unsafe { com::read_string_property(&device, &PKEY_DEVICE_FRIENDLY_NAME) }
                .unwrap_or_else(|| "未知设备".to_string());
            let description =
                unsafe { com::read_string_property(&device, &PKEY_DEVICE_INTERFACE_FRIENDLY_NAME) }
                    .or_else(|| unsafe {
                        com::read_string_property(&device, &PKEY_DEVICE_DEVICE_DESC)
                    })
                    .unwrap_or_default();
            let form_factor =
                unsafe { com::read_u32_property(&device, &com::PKEY_AUDIO_ENDPOINT_FORM_FACTOR) }
                    .map(|value| value as i32)
                    .unwrap_or(-1);
            let kind = DeviceKind::from_form_factor(form_factor);

            if let Some(position) = previous.iter().position(|entry| entry.id == id) {
                let mut entry = previous.remove(position);
                entry.name = name;
                entry.description = description;
                entry.kind = kind;
                devices.push(entry);
                continue;
            }

            let Ok(volume) = (unsafe { device.Activate::<IAudioEndpointVolume>(CLSCTX_ALL, None) })
            else {
                continue;
            };
            let meter = unsafe { device.Activate::<IAudioMeterInformation>(CLSCTX_ALL, None) }.ok();

            // EarTrumpet 同款：注册系统音量变化通知，硬件音量键一按就能立刻收到。
            let notify: IAudioEndpointVolumeCallback = EndpointNotify {
                wake: std::sync::Mutex::new(self.wake.clone()),
            }
            .into();
            if unsafe { volume.RegisterControlChangeNotify(&notify) }.is_err() {
                // 注册失败不影响音量读写，只是退回到轮询。
            }

            let level = unsafe { volume.GetMasterVolumeLevelScalar() }.unwrap_or(0.0);
            let mut muted = unsafe { volume.GetMute() }
                .map(|value| value.as_bool())
                .unwrap_or(false);
            // 守门表里还挂着这台设备的意图：新条目直接按意图起来，别让一次
            // 重建把刚写下去的静音状态"读回"成旧值、闪一下。
            if let Some(guard) = self.mute_guard.get(&id).copied() {
                muted = guard.want;
            }

            devices.push(DeviceEntry {
                id,
                name,
                description,
                kind,
                volume,
                notify: Some(notify),
                meter,
                level,
                muted,
                peak: 0.0,
            });
        }

        // 已经拔掉的设备：先注销回调，否则 WASAPI 会一直持有这个对象。
        for entry in &previous {
            if let Some(notify) = &entry.notify {
                let _ = unsafe { entry.volume.UnregisterControlChangeNotify(notify) };
            }
        }

        self.devices = devices;
        self.current_id = match &default_id {
            Some(id) if self.devices.iter().any(|d| &d.id == id) => Some(id.clone()),
            _ => self.devices.first().map(|d| d.id.clone()),
        };
        self.disabled_devices = self.enumerate_disabled(flow);
    }

    /// 单独列出被系统禁用的设备（`DEVICE_STATE_DISABLED`）。
    ///
    /// 这些设备 Activate 拿不到音量接口，放进主列表只会让"没有可用设备"的
    /// 逻辑到处要判断，所以单独存一份，只在设置页里露出来供用户重新启用。
    fn enumerate_disabled(&self, flow: EDataFlow) -> Vec<DisabledEntry> {
        let Ok(collection) = (unsafe {
            self.enumerator
                .EnumAudioEndpoints(flow, DEVICE_STATE_DISABLED)
        }) else {
            return Vec::new();
        };
        let count = unsafe { collection.GetCount() }.unwrap_or(0);
        let mut list = Vec::new();
        for index in 0..count {
            let Ok(device) = (unsafe { collection.Item(index) }) else {
                continue;
            };
            let Some(id) = (unsafe { com::device_id(&device) }) else {
                continue;
            };
            let name = unsafe { com::read_string_property(&device, &PKEY_DEVICE_FRIENDLY_NAME) }
                .unwrap_or_else(|| "未知设备".to_string());
            let description =
                unsafe { com::read_string_property(&device, &PKEY_DEVICE_INTERFACE_FRIENDLY_NAME) }
                    .or_else(|| unsafe {
                        com::read_string_property(&device, &PKEY_DEVICE_DEVICE_DESC)
                    })
                    .unwrap_or_default();
            let form_factor =
                unsafe { com::read_u32_property(&device, &com::PKEY_AUDIO_ENDPOINT_FORM_FACTOR) }
                    .map(|value| value as i32)
                    .unwrap_or(-1);
            list.push(DisabledEntry {
                id,
                name,
                description,
                kind: DeviceKind::from_form_factor(form_factor),
            });
        }
        list.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        list
    }

    /// 扫应用会话。
    ///
    /// **必须遍历所有启用的播放设备**：会话是挂在设备上的，应用被指到别的设备之后
    /// 它的会话就只存在于那台设备上。只看默认设备会漏掉它 —— 面板里应用消失、
    /// 拖音量也没反应（写的是旧设备上那条已失效的会话）。
    fn rescan_sessions(&mut self) {
        // 默认设备排最前：同一应用在两台设备上都还留着会话时，行上的读数取默认设备那条。
        let mut ids: Vec<String> = self
            .devices
            .iter()
            .map(|device| device.id.clone())
            .collect();
        if let Some(current) = self.current_id.as_ref() {
            if let Some(slot) = ids.iter().position(|id| id == current) {
                let id = ids.remove(slot);
                ids.insert(0, id);
            }
        }

        // 一台启用的设备都没有：没有会话可扫。
        let mut apps: Vec<AppEntry> = Vec::with_capacity(ids.len() * 4);
        // 分组键 → 在 apps 里的下标。同一 exe 的多条会话合并成一行。
        let mut groups: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
        // pid → 进程还在不在。同一个进程常常有好几条会话（Chromium 系），
        // 每会话都去 `OpenProcess` 一次纯属浪费，扫描期间查一次就够。
        let mut alive: std::collections::HashMap<u32, bool> = std::collections::HashMap::new();

        for device_id in &ids {
            let device = unsafe {
                self.enumerator
                    .GetDevice(com::pcwstr(&com::wide(device_id)))
            };
            let Ok(device) = device else {
                continue;
            };
            let Ok(manager) =
                (unsafe { device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) })
            else {
                continue;
            };
            let Ok(enumerator) = (unsafe { manager.GetSessionEnumerator() }) else {
                continue;
            };
            let count = unsafe { enumerator.GetCount() }.unwrap_or(0);

            for index in 0..count {
                let Ok(control) = (unsafe { enumerator.GetSession(index) }) else {
                    continue;
                };
                let Ok(control2) = control.cast::<IAudioSessionControl2>() else {
                    continue;
                };
                let Ok(volume) = control.cast::<ISimpleAudioVolume>() else {
                    continue;
                };
                let meter = control.cast::<IAudioMeterInformation>().ok();

                let pid = unsafe { control2.GetProcessId() }.unwrap_or(0);
                // 自己（松手提示音）也会开一条会话，不能在应用列表里露出来。
                if pid == std::process::id() {
                    continue;
                }
                // IsSystemSoundsSession 返回的是原始 HRESULT：S_OK(0) 是系统声音，
                // S_FALSE(1) 是普通应用。用 is_ok() 判断会把 S_FALSE 也当成功，
                // 结果所有会话都被误判成系统声音、再按 pid=0 去重，最终只剩一条。
                let is_system_hr = unsafe { control2.IsSystemSoundsSession() }.0;
                let is_system = is_system_hr == 0 || pid == 0;
                if pid == 0 && !is_system {
                    continue;
                }

                let state = unsafe { control.GetState() }
                    .map(|value| value.0)
                    .unwrap_or(1) as u32;

                // 进程已经退出、系统还没把会话收走：面板打开时这一行要立刻消失，
                // 不能等系统自己回收（实测有些应用退出后会话会以 Active 状态留一阵）。
                // 判定是一次 `OpenProcess` + `WaitForSingleObject(0)`，成本几微秒，
                // 且只跟着既有的 400ms / 2.5s 会话扫描跑，不额外开任何定时器。
                if !is_system {
                    let running = *alive.entry(pid).or_insert_with(|| process_running(pid));
                    if !running {
                        continue;
                    }
                }

                let (name, process_path, icon_path) = if is_system {
                    ("系统声音".to_string(), None, None)
                } else {
                    let meta = self.resolver.resolve(pid, "");
                    (meta.name, meta.path, meta.icon_path)
                };

                let group_key = if is_system {
                    // 每台设备上都有自己的「系统声音」会话；它们要合成一行，
                    // 不然遍历多台设备之后面板里会出现好几行「系统声音」。
                    "system".to_string()
                } else {
                    process_path
                        .as_deref()
                        .map(|path| format!("exe:{}", path.to_string_lossy().to_lowercase()))
                        .unwrap_or_else(|| format!("pid:{pid}"))
                };

                if let Some(&slot) = groups.get(&group_key) {
                    let entry = &mut apps[slot];
                    entry.volumes.push(volume);
                    if let Some(meter) = meter {
                        entry.meters.push(meter);
                    }
                    if !entry.session_devices.iter().any(|id| id == device_id) {
                        entry.session_devices.push(device_id.to_string());
                    }
                    // 组内只要有一条会话在发声，整行就算作播放中。
                    if state == 1 {
                        entry.state = 1;
                    }
                    continue;
                }

                let level = unsafe { volume.GetMasterVolume() }.unwrap_or(1.0);
                let muted = unsafe { volume.GetMute() }
                    .map(|value| value.as_bool())
                    .unwrap_or(false);

                groups.insert(group_key.clone(), apps.len());
                apps.push(AppEntry {
                    key: group_key,
                    pid,
                    name,
                    process_path,
                    icon_path,
                    is_system,
                    volumes: vec![volume],
                    meters: meter.into_iter().collect(),
                    session_devices: vec![device_id.to_string()],
                    level,
                    muted,
                    peak: 0.0,
                    state,
                });
            }
        }

        // 去掉已过期的会话（应用退出后系统会保留一小段时间）。
        apps.retain(|app| app.state != 2);

        let previous: HashSet<String> = self.apps.iter().map(|a| a.key.clone()).collect();
        let current: HashSet<String> = apps.iter().map(|a| a.key.clone()).collect();
        if previous != current {
            self.apps_revision = self.apps_revision.wrapping_add(1);
        }

        apps.sort_by(|a, b| {
            a.is_system
                .cmp(&b.is_system)
                .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
        });

        self.apps = apps;
        // 已经不在场的 pid 从解析缓存里删掉：一是缓存会一直长，二是 pid 会被系统
        // 复用给新进程 —— 留着旧的，新进程一上来就顶着上一个应用的名字和图标。
        let live: HashSet<u32> = self.apps.iter().map(|app| app.pid).collect();
        self.resolver.retain_pids(&live);
        // 名单换了，被指定的设备也要重新对一遍（新出现的应用才可能有值）。
        self.refresh_app_devices();
    }

    /// 重新问一遍每个应用被指定到哪台设备。
    ///
    /// 只在名单变化后、或每隔 `APP_DEVICE_SCAN` 跑一次：走的是 COM，
    /// 没必要跟着 30fps 的采样跑；在系统设置里改路由最多迟一两拍反映到标记上。
    fn refresh_app_devices(&mut self) {
        self.last_app_device_scan = Instant::now();
        let Some(policy) = self.app_device.as_ref() else {
            self.app_devices.clear();
            return;
        };
        let mut fresh = std::collections::HashMap::with_capacity(self.apps.len());
        for app in &self.apps {
            fresh.insert(app.pid, policy.device_of(app.pid));
        }
        self.app_devices = fresh;
    }

    /// 记一笔「这个静音意图已经写下去了」（用于对外显示对账）。
    fn note_mute_guard(&mut self, device_id: &str, want: bool, at: Instant) {
        let since = self
            .mute_guard
            .get(device_id)
            .map(|guard| guard.since)
            .unwrap_or(at);
        self.mute_guard.insert(
            device_id.to_string(),
            MuteGuard {
                want,
                sent_at: at,
                since,
            },
        );
    }

    fn refresh_levels(&mut self) {
        // 先把上限取出来：下面要可变借用 devices，同一时刻不能再借 self。
        let cap = self.cap.clone();
        let limit_for = |device_id: &str| -> Option<f32> {
            cap.as_ref()
                .filter(|(bound, _)| bound.is_empty() || bound == device_id)
                .map(|(_, limit)| *limit)
        };

        // 守门表先取出来：下面要可变借用 `devices`。
        let mut guards = std::mem::take(&mut self.mute_guard);

        for device in &mut self.devices {
            if let Ok(level) = unsafe { device.volume.GetMasterVolumeLevelScalar() } {
                device.level = level;
            }
            if let Ok(muted) = unsafe { device.volume.GetMute() } {
                let real = muted.as_bool();
                device.muted = real;
                // 守门：写完的这一小段里对外一律按用户意图显示，过了等待期再
                // 拿真实值对账。
                //
                // 为什么要等满 `MUTE_APPLY_TIMEOUT` 而不是「读到一致就收工」：
                // 连按两下的时候，第二下写下去时端点上报的还是第一下之前的值，
                // 看起来恰好等于新意图 —— 这时候放行，紧接着迟到的旧值翻上来，
                // 面板就跳一下。等满之后两种情形都能分清：真一致就交还，不一致
                // 说明端点确实没吃下这条命令，重写一次。
                let now = Instant::now();
                let mut settled = None;
                if let Some(guard) = guards.get_mut(&device.id) {
                    device.muted = guard.want;
                    if now.duration_since(guard.sent_at) >= MUTE_APPLY_TIMEOUT {
                        if real == guard.want {
                            settled = Some(false);
                        } else if now.duration_since(guard.since) >= MUTE_GIVE_UP {
                            settled = Some(true);
                        } else {
                            let context = GUID::zeroed();
                            if unsafe { device.volume.SetMute(guard.want, &context) }.is_ok() {
                                guard.sent_at = now;
                            }
                        }
                    }
                }
                if let Some(give_up) = settled {
                    guards.remove(&device.id);
                    if give_up {
                        device.muted = real;
                    }
                }
            }
            // 上限比「写的时候钳一下」更硬：硬件音量键、系统 OSD、别的混音器
            // 把音量拧过线，也会在这里被按回来（这也是「限制」两个字的含义）。
            if let Some(limit) = limit_for(&device.id) {
                if device.level > limit + 0.002 {
                    let context = GUID::zeroed();
                    if unsafe { device.volume.SetMasterVolumeLevelScalar(limit, &context) }.is_ok()
                    {
                        device.level = limit;
                    }
                }
            }
            device.peak = device
                .meter
                .as_ref()
                .and_then(|meter| unsafe { meter.GetPeakValue() }.ok())
                .unwrap_or(0.0)
                .clamp(0.0, 1.0);
        }
        self.mute_guard = guards;

        for app in &mut self.apps {
            if let Some(volume) = app.volumes.first() {
                if let Ok(level) = unsafe { volume.GetMasterVolume() } {
                    app.level = level;
                }
            }
            // 组内任一静音就算静音；音量取第一条会话的值（写入时是整组一起写）。
            app.muted = app.volumes.iter().any(|volume| {
                unsafe { volume.GetMute() }
                    .map(|v| v.as_bool())
                    .unwrap_or(false)
            });
            app.peak = app
                .meters
                .iter()
                .filter_map(|meter| unsafe { meter.GetPeakValue() }.ok())
                .fold(0.0f32, f32::max)
                .clamp(0.0, 1.0);
        }
    }

    /// 把所有超线的设备音量按回上限（刚改完设置时调用一次）。
    fn enforce_cap(&mut self) {
        let Some((bound, limit)) = self.cap.clone() else {
            return;
        };
        for device in &mut self.devices {
            if !bound.is_empty() && bound != device.id {
                continue;
            }
            if device.level <= limit + 0.002 {
                continue;
            }
            let context = GUID::zeroed();
            if unsafe { device.volume.SetMasterVolumeLevelScalar(limit, &context) }.is_ok() {
                device.level = limit;
            }
        }
    }

    fn snapshot(&self) -> Snapshot {
        let mut devices: Vec<DeviceInfo> = self
            .devices
            .iter()
            .map(|device| DeviceInfo {
                id: device.id.clone(),
                name: device.name.clone(),
                description: device.description.clone(),
                is_default: self.current_id.as_deref() == Some(device.id.as_str()),
                enabled: true,
                volume: device.level,
                muted: device.muted,
                peak: device.peak,
                kind: device.kind,
            })
            .collect();
        // 已禁用的设备接在后面：界面（托盘菜单 / 面板）按 `enabled` 过滤，
        // 只有设置页会把它们显示出来，好让用户重新启用。
        devices.extend(self.disabled_devices.iter().map(|device| DeviceInfo {
            id: device.id.clone(),
            name: device.name.clone(),
            description: device.description.clone(),
            is_default: false,
            enabled: false,
            volume: 0.0,
            muted: false,
            peak: 0.0,
            kind: device.kind,
        }));

        let apps = self
            .apps
            .iter()
            .map(|app| AppInfo {
                key: app.key.clone(),
                pid: app.pid,
                name: app.name.clone(),
                process_path: app.process_path.clone(),
                icon_path: app.icon_path.clone(),
                volume: app.level,
                muted: app.muted,
                peak: app.peak,
                is_system: app.is_system,
                state: app.state,
                device_id: self.app_devices.get(&app.pid).cloned().flatten(),
                session_devices: app.session_devices.clone(),
            })
            .collect();

        Snapshot {
            devices,
            current_device_id: self.current_id.clone(),
            apps,
            apps_revision: self.apps_revision,
            app_routing: self.app_device.is_some(),
        }
    }
}

/// 便于其他模块直接引用。
pub type VolumeInterface = IAudioEndpointVolume;

/// 这个 pid 对应的进程还在不在跑。
///
/// 只问 `WaitForSingleObject(handle, 0)`：`WAIT_OBJECT_0` = 已经退出。
/// 拿不到句柄（受保护进程、权限不足）时**保守地当成「还在」** —— 宁可多留一行，
/// 也不能把一个正常播放的应用误删掉。
fn process_running(pid: u32) -> bool {
    if pid == 0 {
        return true;
    }
    unsafe {
        // `SYNCHRONIZE` 在 windows 0.62 里没有跨类型的常量（只有 FILE_ACCESS_RIGHTS
        // 那一份），这里按原始值拼上：等句柄需要这个权限。
        const SYNCHRONIZE: PROCESS_ACCESS_RIGHTS = PROCESS_ACCESS_RIGHTS(0x0010_0000);
        let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION | SYNCHRONIZE, false, pid)
        else {
            return true;
        };
        let state = WaitForSingleObject(handle, 0);
        let _ = CloseHandle(handle);
        state != WAIT_OBJECT_0
    }
}

/// 诊断用：打印各设备的音量范围与硬件步进（dB）。
///
/// 用来回答「对数滚轮步进能细到什么程度」——`increment` 是设备自己的
/// 最小步进（这台机器上是 1 / 1.5 dB），比它更细的步进面板上看得见，
/// 但硬件那一层可能落在同一档。只读，不改任何设置。
#[test]
#[ignore]
fn dump_volume_range() {
    let _apartment = ComApartment::init_mta();
    let enumerator = com::create_device_enumerator().expect("枚举器");
    let collection =
        unsafe { enumerator.EnumAudioEndpoints(eRender, DEVICE_STATE_ACTIVE) }.unwrap();
    let count = unsafe { collection.GetCount() }.unwrap_or(0);
    for index in 0..count {
        let device = unsafe { collection.Item(index) }.unwrap();
        let id = unsafe { com::device_id(&device) }.unwrap_or_default();
        let name = unsafe { com::read_string_property(&device, &PKEY_DEVICE_FRIENDLY_NAME) }
            .unwrap_or_default();
        let volume: IAudioEndpointVolume = unsafe { device.Activate(CLSCTX_ALL, None) }.unwrap();
        let (mut lo, mut hi, mut inc) = (0f32, 0f32, 0f32);
        let range = unsafe { volume.GetVolumeRange(&mut lo, &mut hi, &mut inc) };
        let (mut step, mut steps) = (0u32, 0u32);
        let info = unsafe { volume.GetVolumeStepInfo(&mut step, &mut steps) };
        println!(
            "{name}  id={id}\n  range={range:?} [{lo} .. {hi}] inc={inc} dB\n  stepinfo={info:?} step={step}/{steps}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 验证设备变更通知真的接到了我们自己的实现上，并且能叫醒音频线程。
    ///
    /// 手工从 vtable 调一遍各个回调：`#[implement]` 生成的 vtable 若把
    /// `OnDeviceAdded` 错接到别的槽位，这里立刻能看出来。
    /// 全程不注册任何真实设备、不改系统设置。
    #[test]
    #[ignore = "需要真实的音频设备枚举器，手动跑"]
    fn device_notify_wakes_worker() {
        let _apartment = ComApartment::init_mta();
        let enumerator = com::create_device_enumerator().expect("设备枚举器");
        let (tx, rx) = channel();
        let notify: IMMNotificationClient = DeviceNotify {
            wake: std::sync::Mutex::new(tx),
        }
        .into();
        unsafe { enumerator.RegisterEndpointNotificationCallback(&notify) }.expect("注册通知");

        let expect_wake = |rx: &Receiver<AudioCommand>| {
            assert!(
                matches!(
                    rx.recv_timeout(Duration::from_secs(1)),
                    Ok(AudioCommand::DevicesChanged)
                ),
                "回调没有叫醒音频线程"
            );
        };

        unsafe { notify.OnDeviceAdded(PCWSTR::null()) }.unwrap();
        expect_wake(&rx);
        unsafe { notify.OnDeviceStateChanged(PCWSTR::null(), DEVICE_STATE_ACTIVE) }.unwrap();
        expect_wake(&rx);
        unsafe { notify.OnDeviceRemoved(PCWSTR::null()) }.unwrap();
        expect_wake(&rx);
        unsafe { notify.OnDefaultDeviceChanged(eRender, eMultimedia, PCWSTR::null()) }.unwrap();
        expect_wake(&rx);

        // 属性变化（改名 / 换图标）要忽略，不能跟着重扫设备。
        let key: windows::Win32::Foundation::PROPERTYKEY = unsafe { std::mem::zeroed() };
        unsafe { notify.OnPropertyValueChanged(PCWSTR::null(), key) }.unwrap();
        assert!(
            rx.recv_timeout(Duration::from_millis(200)).is_err(),
            "属性变化不该触发设备重扫"
        );

        let _ = unsafe { enumerator.UnregisterEndpointNotificationCallback(&notify) };
    }
}
