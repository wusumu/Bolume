//! 音频引擎：独立工作线程持有全部 COM 对象，UI 线程只收发纯数据。

use std::{
    collections::{HashMap, HashSet},
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
    EDataFlow, ERole, IAudioSessionControl2, IAudioSessionManager2,
    IMMDeviceEnumerator, IMMNotificationClient, IMMNotificationClient_Impl,
    ISimpleAudioVolume, eMultimedia, eRender,
};
use windows::Win32::System::Com::CLSCTX_ALL;
use windows::Win32::System::Threading::{
    OpenProcess, PROCESS_ACCESS_RIGHTS, PROCESS_QUERY_LIMITED_INFORMATION, WaitForSingleObject,
};
use windows::core::{GUID, Interface, PCWSTR};

use super::boost::{BoostEvent, BoostTarget};
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

/// trace 里只留文件名，路径太长看不清。
fn short_key(key: &str) -> &str {
    key.rsplit(['\\', '/']).next().unwrap_or(key)
}

/// 响度均衡开着时的采样间隔。
///
/// 15ms 一拍，一拍最多走一块（插件一块 26ms）。不能跟着面板的可视状态走：
/// AGC 的步进是按块算的，拍子一变慢，音量就变成一格一格跳 ——
/// 而且面板关着的时候声音一样要均衡。
const LOUDNESS_TICK: Duration = Duration::from_millis(crate::config::LoudnessConfig::TICK_MS as u64);

/// 等接管线程「已退出接管」回报的最长时间。
///
/// 正常只要几十毫秒（`boost` 里 `PENDING_MS 24` + `TICK 8ms`）。超过这个时间
/// 就是回报丢了，硬放行 —— 卡在表里的 pid 会让那个应用再也调不动。
const RELEASE_TIMEOUT: Duration = Duration::from_millis(600);

/// 一个会话的 AGC 状态（TomSteady 那套）。
///
/// 照 `dsp_tom.dll` v1.4 的 NQ 主循环（`0x10001DD0` 起）：
///
/// 1. 把这一块乘上当前增益，量出乘完之后的峰值；
/// 2. 超过目标峰值 → 投一票「往下」，降到 7 票时减一格；
/// 3. 不到目标峰值 → 投一票「往上」，到 8 票时加一格；连续加 128 块之后
///    一次加「级别」格（安静素材不用等半分钟才抬起来）。
///
/// 插件里那张「预测器」表（`0x100052f8`，初值 0x07，每块 ±1，8 上 7 下）
/// 稳态下等价于一个 0..15 的滞回计数器，这里就用计数器表示。
///
/// 有两处**故意偏离插件**（都是因为它跑在系统混音上，不是在 Winamp 的 DSP 链里）：
///
/// - 静音块不参与投票（插件会照样加，于是间隙里把增益堆满，下一句就是爆音）；
/// - 超过防削顶线时直接按比例压到目标，而不是「减一格」——
///   一格只有 3%，从 8 倍降下来要好几秒，那几秒全是削顶失真。
#[derive(Default)]
struct Steady {
    /// 当前增益（插件里的「filter volume」）。1.0 = 原样。
    gain: f32,
    /// 投票（0..15）。插件的表初值是 7，判决线 8 上 7 下。
    votes: i32,
    /// 连续抬升的块数，满 [`LoudnessConfig::FAST_AFTER`] 之后步长乘级别。
    rising: u32,
    /// 攒着还没凑够一块的毫秒。
    accum: f32,
    /// 这一块里采到过的**最大**峰值。
    ///
    /// 插件在 DSP 里看的是块内每一个样本，它量的天然就是块峰值。我们只能
    /// 15ms 读一次 `GetPeakValue`，一块 26ms 里只看得到两个瞬时值 ——
    /// 视频里的枪声、鼓点、音效比 15ms 还短，采样点全落在缝隙里就漏掉了。
    /// 实测（DOpus 播视频，level 20 / target 0.2）：meter 报 0.15，实际输出
    /// 峰值到 0.76，隔几秒就顶一次削顶 —— 用户听到的「隔几秒炸一下」。
    /// 所以一块之内要取最大值，而不是拿最后一拍的瞬时值代表整块。
    block_peak: f32,
    /// 是否已经初始化过（`Default` 里的 0 增益不是合法的起点）。
    started: bool,
}

impl Steady {
    fn reset(&mut self) {
        *self = Self {
            gain: 1.0,
            votes: crate::config::LoudnessConfig::VOTE_DOWN,
            rising: 0,
            accum: 0.0,
            block_peak: 0.0,
            started: true,
        };
    }

    /// 接管流中途断了：把增益夹回 1.0 以内再接着算。
    ///
    /// 不能直接 [`Self::reset`] 到 1.0 —— 那等于把刚压下去的增益又抬满
    /// （大动态那一下会二次爆）。夹到 1.0 之后 AGC 立刻按「超目标」往下压，
    /// 走的是写会话音量那条路，几十毫秒就能回到目标。
    fn hold(&mut self, config: &crate::config::LoudnessConfig) {
        if !self.started {
            self.reset();
        }
        self.gain = self.gain.clamp(config.step(), 1.0);
        self.votes = crate::config::LoudnessConfig::VOTE_DOWN;
        self.rising = 0;
    }

    /// 喂一拍（15ms）的峰值，返回这一拍的增益。
    ///
    /// `peak` 是会话电平表读到的**音量之前**的峰值（实测见 `boost` 模块），
    /// 所以「乘完增益之后」就是 `peak * gain` —— 跟插件量的是同一个位置。
    fn push(&mut self, peak: f32, config: &crate::config::LoudnessConfig, elapsed_ms: f32) -> f32 {
        if !self.started {
            self.reset();
        }
        // 一块之内的峰值取最大（见 `block_peak` 的说明）。
        self.block_peak = self.block_peak.max(peak);
        self.accum += elapsed_ms;
        let block = crate::config::LoudnessConfig::BLOCK_MS as f32;
        // 一拍最多走一块（TICK_MS < BLOCK_MS）；卡顿之后补算，但别一次补几百块。
        let mut budget = 8;
        while self.accum >= block && budget > 0 {
            self.accum -= block;
            let block_peak = std::mem::take(&mut self.block_peak);
            self.step(block_peak, config);
            budget -= 1;
        }
        if budget == 0 {
            self.accum = 0.0;
        }
        self.gain
    }

    fn step(&mut self, peak: f32, config: &crate::config::LoudnessConfig) {
        use crate::config::LoudnessConfig as C;
        let target = config.target();
        let level = config.smoothness.clamp(1, 10) as f32;
        let step = config.step();

        // 静音块：既不涨也不降，直接跳过这一块。
        //
        // 插件里没有这条（它假设素材是连续的），但用在系统混音上必须有：
        // 视频暂停、对话间隙、曲间空白都是「峰值 0 的块」，全都落进「不到目标」
        // 那一支的话，增益会一路爬到上限；等真有声音时就是几倍的爆音，
        // 而且要好几秒才压回来 —— 这正是「一开始播放突然很大声、音质很差」。
        if peak < C::MIN_PEAK {
            self.rising = 0;
            return;
        }

        let out = peak * self.gain;
        if out > C::CLIP_LEVEL {
            // 防削顶。
            //
            // 插件这里是 `vol -= stepping`（一步顶「级别」格 ≈ 3%），慢得离谱：
            // 从安静段堆起来的 8 倍降到目标要上百块（好几秒），那几秒全是
            // 削顶失真 —— 用户说的「一开始播放突然很大声、音质很差响几百毫秒」
            // 就是这一段。这里直接按比例压到「这一块的峰值正好等于目标」，
            // 一块（26ms）之内收敛。尖峰过去后抬回来仍然走下面的慢路径
            // （release 慢、attack 快，这是 AGC 该有的手感）。
            let want = target / peak;
            self.gain = want.min(self.gain - step * level).max(step);
            self.votes = 0;
            self.rising = 0;
        } else if out > target * C::OVER_FIT {
            // 明显超目标（`OVER_FIT` = 1.5 倍，+3.5dB）。
            //
            // 插件这里只投一票，8 票才降一格 —— 那是给连续的音乐写的，超目标
            // 会连着出现几十块，投票攒得起来。视频不一样：一段对白后面接一个
            // 26ms 的音效，超目标只出现一两块，投票还没攒够音效就过去了。
            // 实测（DOpus 播视频、level 20）：AGC 一路把增益抬到 8 倍，音效来的
            // 时候输出是目标的 4 倍，全靠输出端限幅器硬压 12dB —— 听感就是
            // 「隔几秒被按一下」。所以这里补一条按比例回退：一块之内往目标走
            // 一半（`powf(0.5)`），既不猛抽，也不会让限幅器独自扛。
            let ratio = (target / out).clamp(0.01, 1.0);
            self.gain = (self.gain * ratio.sqrt()).max(step);
            self.votes = C::VOTE_DOWN;
            self.rising = 0;
        } else if out > target {
            self.rising = 0;
            self.votes = (self.votes - 1).max(0);
            if self.votes <= C::VOTE_DOWN {
                self.gain -= step;
            }
        } else {
            self.votes = (self.votes + 1).min(C::VOTE_MAX);
            if self.votes >= C::VOTE_UP {
                self.rising = self.rising.saturating_add(1);
                if self.rising >= C::FAST_AFTER {
                    self.gain += step * level;
                } else {
                    self.gain += step;
                }
            }
        }
        // 插件里最小是一格，不能让增益归零（归零就再也起不来了，
        // 而且会话音量写 0 会让 process loopback 的捕获变哑）；上限见 MAX_GAIN。
        self.gain = self.gain.clamp(step, C::MAX_GAIN);
    }
}

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
    /// 响度均衡（全局参数 + 单应用的开关覆盖）。
    ///
    /// `overrides` 里只放「跟全局不一样」的应用，和配置里 `app_loudness` 一致。
    SetLoudness {
        config: crate::config::LoudnessConfig,
        overrides: Vec<(String, bool)>,
    },
    Refresh,
    Shutdown,
}

#[derive(Debug, Clone)]
pub enum AudioEvent {
    Snapshot(Snapshot),
    Error(String),
    /// 用户手动改了这个应用的音量，它已经从响度均衡里退出来。
    ///
    /// 均衡每 15ms 就会写一次音量，手动拖滑条如果不退出，下一个 15ms 就被顶回去 ——
    /// 滑条根本拖不动。参考实现对应的是 `AutoExcludeOnManualSet`，只是它默认关、
    /// 而且只影响本次运行；这里直接落配置，重启后依然记得「这个应用我不想均衡」。
    LoudnessReleased(String),
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
    /// 接管流该接哪台端点。
    ///
    /// 记的是「最后看到它**在出声**的那台设备」：应用被指到别的设备之后，旧设备上
    /// 往往还留着一条 `Expired` 会话，`session_devices.first()` 会认错 —— 那样放大的
    /// 那份会从旧设备出来，而原声在新设备上一点没被压住。
    ///
    /// 只在 `state == 1`（真的在播放）时更新，没在播放时保持不变：否则应用一暂停，
    /// 这个值会退回某台可能的旧设备，接管流跟着重开一次（声音会跳台）。
    boost_device: Option<String>,
    level: f32,
    muted: bool,
    peak: f32,
    /// 未钳位的峰值，只给 AGC 用。
    ///
    /// `peak` 要钳到 0..1 给面板画律动；但 AGC 算「乘完增益之后有多响」必须用
    /// 真实幅度 —— 素材本身超过满量程（播放器上混、某些 DSD/OGG 解码路径）时，
    /// 钳过的值会让防削顶永远以为 `out == gain`，压不下来。
    raw_peak: f32,
    state: u32,
    /// 响度均衡的 AGC 状态。会话重建时跟着 `AppEntry` 一起搬过去。
    steady: Steady,
    /// 这条应用的声音现在由接管流出（流建好之后一直是 true，直到均衡关掉 /
    /// 静音 / 用户手动拖了这个应用的滑条 / 流失败）。
    ///
    /// 和「增益有没有超过 1」分开记：接管期间增益可能是小于 1 的衰减，
    /// 那也需要由流来做（会话已经被压到 2%），不能靠这个数去判断要不要流。
    boost_on: bool,
    /// 接管通路要输出的增益（`boost_on` 为 true 时有效）。会话被压到
    /// [`boost::ATTEN`]，露出部分只能靠这条流补。
    boost_gain: f32,
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
    /// 响度均衡的全局参数。
    loudness: crate::config::LoudnessConfig,
    /// 单应用的开关覆盖（key → 开/关）。没有条目 = 跟随全局。
    loudness_overrides: HashMap<String, bool>,
    /// 本次运行里被用户手动改过音量、从而退出均衡的应用。
    ///
    /// 只是「界面把配置同步回来」之前的短路；配置一更新（`SetLoudness`）就清空。
    loudness_manual: HashSet<String>,
    /// 正在被均衡接管的某个应用，接管那一刻的音量。
    ///
    /// 用户取消勾选「响度均衡」时要还原成这个样子。不还原的话，均衡把 100% 压到
    /// 25%，取消后音量就永远停在 25% —— 看着像「勾一下掉一截」，勾几次就听不见了。
    loudness_saved: HashMap<String, f32>,
    /// 接管通路：每应用超过 100% 的那部分增益由它输出。
    ///
    /// 只有算出来的增益真的越过 1.02 时才有流。
    boost: crate::audio::boost::BoostHub,
    /// 这些应用抓不了（独占模式 / DRM / 声音不在默认设备上），本次运行里不再试。
    boost_blocked: HashSet<String>,
    /// 调试用：上一拍打进 trace 的状态快照（key → 一行摘要）。
    ///
    /// 只在变化时写，否则 15ms 一拍会把文件刷爆。
    trace_state: HashMap<String, String>,
    /// 正在退出接管的 pid → 开始的时刻。
    ///
    /// 从「接管中」退回「会话自己出声」时，接管线程还要把会话音量还回去
    /// （几十毫秒）。这期间引擎不能写音量，否则原声和接管流会同时出声。
    /// 等接管线程回报 [`BoostEvent::Released`] 再放开。
    ///
    /// 带时刻是为了兜底：只要有一条 `Released` 丢了（`sync_boost` 那次恰好
    /// 判定「目标没变」没往接管线程推），这个 pid 就会永久留在表里，那个应用
    /// 从此再也调不动 —— 用户看到的就是「调着调着就不行了」。超过
    /// [`RELEASE_TIMEOUT`] 一律放行。
    boost_releasing: HashMap<u32, Instant>,
    /// 上一次推给接管线程的目标集合（没变就不重复推）。
    boost_targets: Vec<BoostTarget>,
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
        loudness: crate::config::LoudnessConfig::default(),
        loudness_overrides: HashMap::new(),
        loudness_manual: HashSet::new(),
        loudness_saved: HashMap::new(),
        boost: crate::audio::boost::BoostHub::new(),
        boost_blocked: HashSet::new(),
        boost_releasing: HashMap::new(),
        boost_targets: Vec::new(),
        trace_state: HashMap::new(),
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
        // 响度均衡要按 15ms 采、按 15ms 写，节奏不能跟着面板的可视状态走
        // （面板关着的时候声音一样要均衡）。
        let timeout = if worker.loudness_wanted() {
            LOUDNESS_TICK
        } else if worker.visible || worker.animating {
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
                // 用户自己动手了：这个应用退出响度均衡（否则下面这笔写入
                // 会在 15ms 后被均衡覆盖回去，滑条像被黏住一样拖不动）。
                let mut releasing: Vec<u32> = Vec::new();
                if !self.loudness_manual.contains(&key) && self.loudness_for(&key) {
                    // 先短路，别等界面把配置同步回来。
                    self.loudness_manual.insert(key.clone());
                    // 用户自己拖到的位置就是最终值：把接管前那份账销掉，
                    // 否则界面随后发来的 `SetLoudness`（关掉均衡）会拿旧值盖回去。
                    self.loudness_saved.remove(&key);
                    if let Some(app) = self.apps.iter_mut().find(|a| a.key == key) {
                        app.steady.reset();
                        // 用户自己定的音量说了算：接管流收掉，声音回到会话那条路。
                        if app.boost_on {
                            app.boost_on = false;
                            app.boost_gain = 1.0;
                            releasing.push(app.pid);
                        }
                    }
                    // 通道是无界的，`try_send` 不会失败；这里不能用 `send` ——
                    // 它是 async 方法，丢掉返回的 Future 等于什么都没发。
                    let _ = self.events.try_send(AudioEvent::LoudnessReleased(key.clone()));
                }
                for pid in releasing {
                    self.boost_releasing.insert(pid, Instant::now());
                }
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
            AudioCommand::SetLoudness { config, overrides } => {
                // 换配置之前先把旧状态记下来：下面要算「哪些应用从均衡中变成不均衡」。
                let was_global = self.loudness.enabled;
                let was_overrides = self.loudness_overrides.clone();
                self.loudness = config;
                self.loudness_overrides.clear();
                for (key, enabled) in overrides {
                    self.loudness_overrides.insert(key, enabled);
                }
                // 配置变了就意味着「这是用户刚在界面上按的」，手动退出的短路全部作废
                // （界面上那个勾就是最终意思）。
                self.loudness_manual.clear();
                // 只有**开关**真的拨动了才给抓不了的应用再一次机会。
                //
                // 这里以前是「每次配置变化都清空」，而设置页每动一下数值都会
                // 重推一份配置 —— 拖一次滑条就把所有被拉黑的应用重新开一遍流，
                // 每条流都要压音量、settle、失败、回滚，听感是一连串的闪和抖。
                let toggled =
                    was_global != self.loudness.enabled || was_overrides != self.loudness_overrides;
                if toggled {
                    self.boost_blocked.clear();
                }

                // 关掉均衡的应用要把音量还原成接管前的值 —— 用户按的是「取消勾选」，
                // 期望听到他自己设的那个音量，而不是停在均衡压下去的位置。
                let released: Vec<String> = self
                    .apps
                    .iter()
                    .filter(|app| !app.is_system)
                    .filter(|app| {
                        let was = was_overrides
                            .get(&app.key)
                            .copied()
                            .unwrap_or(was_global);
                        let now = self
                            .loudness_overrides
                            .get(&app.key)
                            .copied()
                            .unwrap_or(self.loudness.enabled);
                        was && !now
                    })
                    .map(|app| app.key.clone())
                    .collect();
                for key in released {
                    // 手动拖过滑条的应用这里没有账（那条路已经销账了）。
                    self.restore_loudness_volume(&key);
                }

                // 参数变化（最大放大级别 / 音量变化级别）**不重置增益**。
                //
                // 以前这里对所有应用 `steady.reset()`：拖一下滑条，正在接管的
                // 应用就掉回 1.0 → 触发退出接管（关流、还音量）→ 重新从 1.0 爬。
                // 连拖几下就一路 reset 一路重开，听感就是「调着调着就不行了」。
                // 而且新参数每块都是从 config 现算的，增益本身没有陈旧值。
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
        // 放在读数之后：本拍刚采到的峰值就是这一拍要处理的输入。
        self.apply_loudness();
        // 接管通路：先收失败，再把新的目标压过去。
        self.drain_boost_events();
        self.sync_boost();
    }

    /// 收接管线程回报的失败。
    ///
    /// 抓不到（独占模式 / DRM / 会话不在默认设备上）就把增益归位：不归位的话
    /// 会话音量已经被按到 1.0、接管流又没有，用户会觉得「开了均衡反而乱了」。
    fn drain_boost_events(&mut self) {
        for event in self.boost.drain_events() {
            match event {
                BoostEvent::Failed {
                    pid,
                    reason,
                    retry,
                } => {
                    log::warn!(
                        "接管失败（pid {pid}{}）：{reason}",
                        if retry { "，冷却后再试" } else { "，本次运行不再试" }
                    );
                    self.boost_releasing.remove(&pid);
                    let config = self.loudness;
                    let mut blocked = None;
                    for app in &mut self.apps {
                        if app.pid == pid {
                            // 重要：`boost_on` 必须一起清掉。留着 true 的话
                            // `apply_loudness` 会一直走「接管中」那一支、一个字都不写，
                            // 这个应用从此再也调不动（用户说的「调着调着就不行了」）。
                            app.boost_on = false;
                            app.boost_gain = 1.0;
                            if retry {
                                app.steady.hold(&config);
                            } else {
                                app.steady.reset();
                            }
                            blocked = Some(app.key.clone());
                        }
                    }
                    if let Some(key) = blocked {
                        // 只有「抓不了」才拉黑；流断了留给接管线程冷却后重开。
                        if !retry {
                            self.boost_blocked.insert(key);
                        }
                    }
                }
                BoostEvent::Released { pid } => {
                    // 接管线程已经把会话音量还回去了，从这里开始引擎可以重新写音量。
                    // `app.level` 不用在这里改：下一拍 `refresh_levels` 会把真实值读回来。
                    self.boost_releasing.remove(&pid);
                }
            }
        }
    }

    /// 把「增益越过 1.0 的应用」推给接管线程。
    ///
    /// 一个 pid 只挑一条：同进程的多条会话本来就一起出声，抓一次就够了
    /// （process loopback 按进程抓，抓两次会得到两份一样的音频）。
    fn sync_boost(&mut self) {
        let mut targets: Vec<BoostTarget> = Vec::new();
        for app in &self.apps {
            // 只在真的越过 1.02 时才开流：1.0 附近那点增益不值得为它
            // 「压原声 + 多 40ms 延迟」，听感上是白亏的。
            if app.is_system || !app.boost_on {
                continue;
            }
            if self.boost_blocked.contains(&app.key) || self.loudness_manual.contains(&app.key) {
                continue;
            }
            if targets.iter().any(|target| target.pid == app.pid) {
                continue;
            }
            targets.push(BoostTarget {
                pid: app.pid,
                gain: app.boost_gain,
                // 会话实际挂在哪台端点，接管流就接哪台。应用被面板指到别的设备
                // 之后必须跟着走，否则放大后的那份会留在默认设备上。
                // 认「最后在出声的那台」（`boost_device`），别被旧设备上的 Expired 会话骗了。
                device: app
                    .boost_device
                    .clone()
                    .or_else(|| app.session_devices.first().cloned()),
            });
        }
        targets.sort_by_key(|target| target.pid);
        if targets == self.boost_targets {
            return;
        }
        if std::env::var_os("BOLUME_TRACE").is_some() {
            let list: Vec<String> = targets
                .iter()
                .map(|t| format!("pid {} @ {:.3} dev={:?}", t.pid, t.gain, t.device))
                .collect();
            crate::platform::hotkey::trace(&format!("targets [{}]", list.join(", ")));
        }
        self.boost_targets = targets.clone();
        self.boost.set_targets(targets);
    }

    /// 有没有任何一个应用需要响度均衡。
    ///
    /// 全局开着，或全局关着但有人单开 —— 两种都要按 15ms 跑。
    fn loudness_wanted(&self) -> bool {
        self.loudness.enabled || self.loudness_overrides.values().any(|enabled| *enabled)
    }

    /// 这个应用要不要均衡（单应用覆盖优先）。
    fn loudness_for(&self, key: &str) -> bool {
        self.loudness_overrides
            .get(key)
            .copied()
            .unwrap_or(self.loudness.enabled)
    }

    /// 把某个应用从均衡里放出来，音量还原成接管前的值。
    ///
    /// 没有账（从没接管过，或者用户手动拖过滑条）就什么都不做 —— 后者是故意的：
    /// 用户拖到哪儿就是哪儿，别再拿旧值盖回去。
    fn restore_loudness_volume(&mut self, key: &str) {
        if let Some(app) = self.apps.iter_mut().find(|app| app.key == key) {
            app.steady.reset();
        }
        let Some(saved) = self.loudness_saved.remove(key) else {
            return;
        };
        let Some(app) = self.apps.iter_mut().find(|app| app.key == key) else {
            return;
        };
        let saved = saved.clamp(0.0, 1.0);
        let context = GUID::zeroed();
        let mut applied = false;
        for volume in &app.volumes {
            if unsafe { volume.SetMasterVolume(saved, &context) }.is_ok() {
                applied = true;
            }
        }
        if applied {
            app.level = saved;
        }
    }

    /// 响度均衡：把每个应用的音量拉向「听起来一样响」的位置。
    ///
    /// 算法是 TomSteady 的 AGC（见 [`Steady`]）。
    ///
    /// 通路有两条：增益 ≤ 1.0 时直接写会话音量；增益越过 1.02 就交给接管流
    /// （WASAPI 的会话音量到不了 1.0 以上）。**接管一旦建立就一直由流承担**
    /// ——包括增益后来掉回 1 以下的衰减段——直到均衡被关、静音、用户手动拖了
    /// 这个应用的滑条、或者流失败。中途「掉到 1.02 以下就退出」会让流建了拆、
    /// 拆了建，每次都要压音量/还音量/settle，听感是一连串的跳和空洞。
    ///
    /// **接管期间会话音量归接管线程管，这里一个字都不许写。** 写下去就是
    /// 「原声 + 放大后的接管流」两份：听起来是回声，音量还会顶到削顶（炸）。
    fn apply_loudness(&mut self) {
        if !self.loudness_wanted() {
            // 全关了：接管流也要收掉，否则它会一直替这个应用出声。
            let mut drops: Vec<u32> = Vec::new();
            for app in &mut self.apps {
                if app.boost_on {
                    app.boost_on = false;
                    app.boost_gain = 1.0;
                    drops.push(app.pid);
                }
            }
            for pid in drops {
                self.boost_releasing.insert(pid, Instant::now());
            }
            return;
        }
        let global = self.loudness;
        // 覆盖表很小（只有「跟全局不一样」的应用），拷一份免得到处借 self。
        let overrides = self.loudness_overrides.clone();
        let manual = self.loudness_manual.clone();
        let context = GUID::zeroed();
        let tick = crate::config::LoudnessConfig::TICK_MS as f32;
        // 循环里要收掉接管流的 pid：`self.apps` 已经可变借出去了，没法直接写
        // `boost_releasing`，先攒着，循环之后统一插。
        let mut drops: Vec<u32> = Vec::new();

        // 没开调试就不再为 trace 构造字符串：这段在 15ms 的主循环里，
        // 每拍给每个应用 `format!` 一次是白花的。
        let tracing = std::env::var_os("BOLUME_TRACE").is_some();
        let mut trace_now: HashMap<String, String> = HashMap::new();
        for app in &mut self.apps {
            // 系统声音不参与：那是提示音，不是用户在听的内容。
            if app.is_system {
                continue;
            }
            let on = overrides.get(&app.key).copied().unwrap_or(global.enabled);
            // 用户刚手动拖过这个应用：这一轮别碰它（等界面把配置同步回来）。
            if manual.contains(&app.key) {
                if tracing {
                    trace_now.insert(app.key.clone(), "manual-skip".into());
                }
                continue;
            }
            if !on {
                // 关掉的那一路把 AGC 清干净：下次再开是从头开始，
                // 而不是拿几分钟前的增益一上来就猛调。
                app.steady.reset();
                if app.boost_on {
                    app.boost_on = false;
                    app.boost_gain = 1.0;
                    drops.push(app.pid);
                }
                continue;
            }

            // 第一次接管这个应用：记下它此刻的音量，取消勾选时还原到这里。
            if !self.loudness_saved.contains_key(&app.key) {
                self.loudness_saved.insert(app.key.clone(), app.level);
            }

            // 用户自己按的静音：把接管收掉（不然接管流会把静音绕过去），
            // 音量也不动 —— 静音着还去改音量，用户会以为均衡把静音弄没了。
            if app.muted {
                if app.boost_on {
                    app.boost_on = false;
                    app.boost_gain = 1.0;
                    drops.push(app.pid);
                }
                continue;
            }

            let gain = app.steady.push(app.raw_peak, &global, tick);
            if tracing {
                trace_now.insert(
                app.key.clone(),
                format!(
                    "pk={:.4} gain={:.3} on={} level={:.3} out={:.3}",
                    app.raw_peak,
                    gain,
                    app.boost_on,
                    app.level,
                    app.raw_peak * gain
                    ),
                );
            }
            if let Some(at) = self.boost_releasing.get(&app.pid).copied() {
                if at.elapsed() < RELEASE_TIMEOUT {
                    continue;
                }
                // 接管线程的回报丢了。放行比永久卡住强：再晚也就多一次的
                // 音量写，比「这个应用永远调不动」好。
                log::warn!("接管退出没收尾（pid {}），超时强制放行", app.pid);
                self.boost_releasing.remove(&app.pid);
            }

            // 接管一旦建立就一直跑到「均衡被关 / 静音 / 手动拖滑条 / 流失败」为止。
            //
            // 以前是「增益掉回 1.02 以下就退出」，结果素材刚好在目标附近时，
            // 增益在 1.0~1.04 之间来回摆，流就建了拆、拆了建：每一轮都要压音量、
            // 还音量、settle，听感是一连串的音量跳和几十毫秒的空洞 ——
            // 用户说的「调着调着就不行了」有一半是这个。
            // 而且衰减（增益 < 1）也能由流做，会话保持 2% 就行，没必要退出来。
            if app.boost_on {
                app.level = 1.0;
                app.boost_gain = gain.max(0.02);
                continue;
            }

            // 越过 1.02：会话满格，多出来的交给接管流。
            // 界面上的音量照旧按 1.0 显示：用户设的是「满」，多出来的是增益。
            if gain >= crate::config::LoudnessConfig::BOOST_ENGAGE {
                app.level = 1.0;
                app.boost_on = true;
                app.boost_gain = gain;
                continue;
            }

            // 1.0 上下这点（1.0–1.02，0.17dB）不值得开接管流，直接按 1.0 走。
            app.boost_gain = 1.0;
            let target = gain.min(1.0);
            // 差得太小就别写了：每 15ms 一次 COM 写入，几百个会话会白白烧 CPU，
            // USB / 蓝牙端点上还会让设备一直有事做。
            if (target - app.level).abs() < 0.0005 {
                continue;
            }
            let mut applied = false;
            for volume in &app.volumes {
                if unsafe { volume.SetMasterVolume(target, &context) }.is_ok() {
                    applied = true;
                }
            }
            if applied {
                app.level = target;
            }
        }
        for pid in drops {
            self.boost_releasing.insert(pid, Instant::now());
        }
        // 只在状态变化时落盘（15ms 一拍全程写会把文件刷爆）。
        if tracing {
            for (key, line) in &trace_now {
                if self.trace_state.get(key) != Some(line) {
                    crate::platform::hotkey::trace(&format!("agc {} {}", short_key(key), line));
                    self.trace_state.insert(key.clone(), line.clone());
                }
            }
        }
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
        // 响应均衡的累积器要跨扫描活着。
        //
        // 这个函数每次都会把 `AppEntry` 全部重造（面板开着时 400ms 一次），
        // 而平均窗口是 3 秒 —— 跟着条目一起重造的话永远攒不满一块，均衡等于不生效。
        // 所以先按 key 把上一轮的累积器搬出来，造新条目时再塞回去；
        // 应用真的退出了，那一份没人认领，随 `carried` 一起丢掉。
        let mut carried: HashMap<String, Steady> = self
            .apps
            .iter_mut()
            .map(|app| (app.key.clone(), std::mem::take(&mut app.steady)))
            .collect();
        // 接管增益同样要跨扫描搬过去：否则 400ms 重造一次会把增益打回 1.0，
        // 接管流跟着停掉再建一次，面板开着时听感会一顿一顿的。
        let mut carried_boost: HashMap<String, (bool, f32)> = self
            .apps
            .iter()
            .map(|app| (app.key.clone(), (app.boost_on, app.boost_gain)))
            .collect();
        // 接管端点也是：没在播放的那几轮扫描读不到「活会话」，不搬过去的话
        // 会退回一台瞎猜的设备，接管流白重开一次（声音跳台）。
        let mut carried_device: HashMap<String, Option<String>> = self
            .apps
            .iter()
            .map(|app| (app.key.clone(), app.boost_device.clone()))
            .collect();
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
                    if state == 1 {
                        entry.boost_device = Some(device_id.to_string());
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

                // 上一轮的 AGC 状态（应用没变就接着用，别从零开始）。
                let steady = carried.remove(&group_key).unwrap_or_default();
                // 取在 `key` 被移走之前：同一条会话的接管增益也要接着用。
                let (boost_on, boost_gain) = carried_boost.remove(&group_key).unwrap_or((false, 1.0));
                // 接管流接哪台端点：正在这条设备上出声就认它，静着的应用沿用上一轮记下的。
                let boost_device = if state == 1 {
                    Some(device_id.to_string())
                } else {
                    carried_device.remove(&group_key).flatten()
                };
                // 接管期间这条会话被我们按在 2%：那个 2% 不是用户的音量，
                // 读回来会把 `app.level` 带成 0.02（接管一结束应用就哑了）。
                let level = if boost_on { 1.0 } else { level };

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
                    boost_device,
                    level,
                    muted,
                    peak: 0.0,
                    raw_peak: 0.0,
                    state,
                    steady,
                    boost_on,
                    boost_gain,
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

        if std::env::var_os("BOLUME_TRACE").is_some() {
            let held: Vec<String> = self
                .apps
                .iter()
                .filter(|app| app.boost_on)
                .map(|app| format!("{}(pid {})", short_key(&app.key), app.pid))
                .collect();
            crate::platform::hotkey::trace(&format!(
                "rescan n={} boost_on=[{}]",
                self.apps.len(),
                held.join(", ")
            ));
        }
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
            // 接管期间会话被接管线程按在 [`boost::ATTEN`]：那不是用户的音量，
            // 读回来会让面板显示 2%，接管结束时还会把用户音量带跑。
            if !app.boost_on {
                if let Some(volume) = app.volumes.first() {
                    if let Ok(level) = unsafe { volume.GetMasterVolume() } {
                        app.level = level;
                    }
                }
            }
            // 组内任一静音就算静音；音量取第一条会话的值（写入时是整组一起写）。
            app.muted = app.volumes.iter().any(|volume| {
                unsafe { volume.GetMute() }
                    .map(|v| v.as_bool())
                    .unwrap_or(false)
            });
            let peak = app
                .meters
                .iter()
                .filter_map(|meter| unsafe { meter.GetPeakValue() }.ok())
                .fold(0.0f32, f32::max);
            app.raw_peak = peak;
            app.peak = peak.clamp(0.0, 1.0);
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
                // 接管期间会话音量被按在 1.0、而且是被**我们**静音的：
                // 对用户来说它就是满音量在响，别显示成静音。
                volume: if app.boost_on { 1.0 } else { app.level },
                muted: !app.boost_on && app.muted,
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

    /// 喂一块（26ms）的峰值。
    fn feed(steady: &mut Steady, peak: f32, config: &crate::config::LoudnessConfig, blocks: u32) {
        let block = crate::config::LoudnessConfig::BLOCK_MS as f32;
        for _ in 0..blocks {
            steady.push(peak, config, block);
        }
    }

    /// 一段很轻的素材把增益抬高，然后进入完全静音：增益必须停住。
    ///
    /// 以前静音块也走「不到目标 → 投票加一」，增益会一路爬到上限；
    /// 下一句一开口就是几倍爆音 —— 用户报的「一开始播放突然很大声、
    /// 音质很差地响几百毫秒」。
    #[test]
    fn steady_holds_gain_through_silence() {
        let config = crate::config::LoudnessConfig::default();
        let mut steady = Steady::default();
        feed(&mut steady, 0.02, &config, 1000);
        let lifted = steady.gain;
        assert!(lifted > 1.5, "很轻的素材应该把增益抬起来，实际 {lifted}");

        feed(&mut steady, 0.0, &config, 1000);
        assert!(
            (steady.gain - lifted).abs() < 1e-6,
            "静音块不该动增益：{lifted} -> {}",
            steady.gain
        );
    }

    /// 增益很高时突然来了强信号：**一块之内**就要压到防削顶线以下。
    ///
    /// 只按「减级别格」走的话要好几秒（smoothness 8 时从 4 倍降到 1 倍
    /// 要 96 块 ≈ 2.5 秒），这 2.5 秒全是削顶失真。
    #[test]
    fn steady_kills_clipping_within_one_block() {
        use crate::config::LoudnessConfig as C;
        let config = crate::config::LoudnessConfig::default();
        let mut steady = Steady::default();
        feed(&mut steady, 0.02, &config, 1000);
        assert!(steady.gain > 2.0, "先要把增益抬到 2 倍以上");

        let gain = steady.push(0.9, &config, C::BLOCK_MS as f32);
        assert!(
            gain * 0.9 <= C::CLIP_LEVEL + 1e-4,
            "一块之后仍超削顶线：gain {gain} × 0.9 = {}",
            gain * 0.9
        );
    }

    /// 削顶之后要落到**目标**上，而不是停在削顶线下面那一档。
    ///
    /// 只压到「不削顶」的话，输出会一直贴着 0.91（比目标 0.75 响 1.7dB），
    /// 而靠投票一格一格降要一分多钟 —— 听感就是「突然变大声之后一直很大声」。
    #[test]
    fn steady_settles_on_target_after_clipping() {
        use crate::config::LoudnessConfig as C;
        let config = crate::config::LoudnessConfig::default();
        let mut steady = Steady::default();
        feed(&mut steady, 0.02, &config, 1000);
        assert!(steady.gain > 2.0);

        let gain = steady.push(0.9, &config, C::BLOCK_MS as f32);
        assert!(
            (gain * 0.9 - config.target()).abs() < 0.01,
            "一块之后应该落在目标 {}，实际 {}",
            config.target(),
            gain * 0.9
        );
    }

    /// 一直很轻的素材也不能把增益堆到上限之外（否则底噪会被一起抬成巨响）。
    #[test]
    fn steady_respects_max_gain() {
        use crate::config::LoudnessConfig as C;
        let config = crate::config::LoudnessConfig::default();
        let mut steady = Steady::default();
        feed(&mut steady, 0.001, &config, 4000);
        assert!(
            steady.gain <= C::MAX_GAIN + 1e-6,
            "增益超过上限：{}",
            steady.gain
        );
    }
}
