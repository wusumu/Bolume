//! 每应用增益接管（boost）：把某个应用的音频抓出来、乘增益、再渲染回**它所在的那台设备**。
//!
//! # 为什么需要这一层
//!
//! 响度均衡要把「本来就轻的应用」抬起来，就得让增益超过 1.0。但 WASAPI 的会话
//! 音量有硬上限：实测 `ISimpleAudioVolume::SetMasterVolume(1.5)` 读回来是 `1.0000`
//! （`4.0` 也一样）。参考实现 Wale 的 `Volume` 同样被夹在 0–1，所以「抬上去」
//! 这件事在 API 层做不到，只能自己接管音频流。
//!
//! # 做法
//!
//! 1. `ActivateAudioInterfaceAsync` + `VAD\Process_Loopback` 按 pid 抓这个进程的
//!    音频（Win10 2004+）。process loopback 按**进程**抓，跟设备无关。
//! 2. 把原会话的音量压到 [`ATTEN`]（在它**自己那台设备**的会话表上压），否则会听到两份。
//! 3. 自己开一个渲染流到**同一个端点**，采样乘上 `增益 / ATTEN`、削顶，再写出去。
//!
//! 第 2、3 步都必须认设备：应用被面板指到耳机之后，它的会话只存在于耳机那台
//! 端点上。以前一律用 `GetDefaultAudioEndpoint`，结果放大的那一份从音箱出来，
//! 原声（耳机）又没被压住 —— 用户看到的是「指定播放设备不好使」，听到的是
//! 两份声音叠在一起。
//!
//! # 为什么要压音量、不能静音（实测，Win11 25H2 / Realtek USB）
//!
//! process loopback 抓的是会话**音量节点之后**的信号：
//!
//! | 会话音量 | 电平表（`IAudioMeterInformation`） | 抓到的峰值 |
//! |----------|-----------------------------------|------------|
//! | 1.00     | 0.193                             | 0.193      |
//! | 0.10     | 0.193                             | 0.019      |
//! | 0.02     | 0.193                             | 0.004      |
//! | 0（或静音）| 0.193                            | **0**（静音包） |
//!
//! 两条结论：**静音会让捕获直接变哑**（所以不能靠 `SetMute` 让原声让位，
//! 一静音连抓都抓不到）；**电平表读的是音量之前**（所以响度算法不受影响，
//! 不用另做补偿）。压到 2% 时原声残留在 -34dB，相对接管后的输出约 -41dB，
//! 听不出来；而 2% 的信号在 float32 通路里乘 50 倍回来不会有量化损失。
//!
//! **但上表不是所有进程都成立**：实测 Dopus 播放视频时，会话被压到 2%（回读
//! 确认过），抓到的峰值仍是 0.985 满幅 —— 它的捕获点在会话音量**之前**。
//! 那种进程按上表乘 50 倍就是「满幅原声 × 50」→ 削顶失真。不过这不是靠猜能解决的：
//! 用「压前 / 压后的能量比」去判，会被内容自己的起伏带偏（后段常常虚高），
//! 实测直接导致「声音只剩一成」。所以增益固定按 `1 / ATTEN` 走，能量比只记进
//! 诊断行（`ratio`）留着以后分析。
//!
//! # 已知代价（都是这条路绕不过去的）
//!
//! * 声音会比画面对晚 40ms 上下；
//! * 受 DRM 保护的内容抓不到（只剩静音）；
//! * 独占模式（ASIO / 独占 WASAPI）的应用抓不到，这里会报失败并退回原音量。
//!
//! 只有 `max_gain > 1.0` 且算出来的增益真的超过 1.0 时才会走到这里，默认不开。

use std::{
    collections::{HashMap, HashSet, VecDeque},
    ffi::c_void,
    sync::{
        Arc, Condvar, Mutex,
        mpsc::{Receiver, RecvTimeoutError, Sender, TryRecvError, channel},
    },
    time::{Duration, Instant},
};

use windows::Win32::Foundation::E_NOINTERFACE;
use windows::Win32::Media::Audio::{
    AUDCLNT_BUFFERFLAGS_SILENT, AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_LOOPBACK,
    AUDIOCLIENT_ACTIVATION_PARAMS, AUDIOCLIENT_ACTIVATION_PARAMS_0,
    AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK, AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS,
    ActivateAudioInterfaceAsync, IActivateAudioInterfaceAsyncOperation,
    IActivateAudioInterfaceCompletionHandler, IActivateAudioInterfaceCompletionHandler_Impl,
    IAudioCaptureClient, IAudioClient, IAudioRenderClient, IAudioSessionControl,
    IAudioSessionControl2, IAudioSessionManager2, IMMDevice, IMMDeviceEnumerator,
    ISimpleAudioVolume,
    PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE, VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
    eMultimedia, eRender,
};
use windows::Win32::System::Com::{
    CLSCTX_ALL, CoTaskMemAlloc, CoTaskMemFree, StructuredStorage::PROPVARIANT,
};
use windows::Win32::System::Variant::VT_BLOB;
use windows::core::{GUID, Interface, Ref, implement};

use super::com::{self, ComApartment};

use windows::Win32::Media::Audio::WAVEFORMATEX;

/// 轮询节拍。shared mode 的最小周期就是 10ms 上下，再密也只是空转。
const TICK: Duration = Duration::from_millis(8);
/// 渲染端提前排队的时长。太小会欠载（咔哒声），太大会拖延迟。
const PENDING_MS: u32 = 24;
/// ring 里最多留多少毫秒的捕获数据。超了丢最老的，避免延迟越滚越大。
const RING_MS: u32 = 120;
/// 接管期间核对会话音量的间隔（见 `Hub::tick` 末尾的兜底）。
///
/// 250ms 是权衡：太稀的话，应用新开一条会话（浏览器换流、播放器重开音频端点）
/// 到我们把它压下去之间，原声会和接管流叠着响一整段；太密就是每 250ms 一次
/// 会话枚举的 COM 开销。一次枚举只有几毫秒。
const GUARD_EVERY: Duration = Duration::from_millis(250);
/// 刚压下去的那一小段用更密的间隔核对。
///
/// 这两秒最危险：别的程序（或者我们自己上一轮留下的还原动作）一旦把会话音量
/// 抬回 100%，原声就和接管流（增益可能到 8 / 2% = 400 倍）叠在一起，
/// 500ms 的间隔足够炸两下。密到 60ms，最坏也就漏一拍。
const GUARD_TIGHT: Duration = Duration::from_secs(2);
const GUARD_TIGHT_EVERY: Duration = Duration::from_millis(60);
/// 压音量没压住时，最多试几次 / 两次之间隔多久（共约 380ms）。
///
/// 有的应用会自己把会话音量写回去（换集、换音轨、切全屏），一次没压住不代表
/// 压不下去；给几轮重试，比立刻关流（听感就是「开了均衡没反应」）强。
const ATTEN_TRIES: u8 = 12;
const ATTEN_RETRY_EVERY: Duration = Duration::from_millis(30);
/// 增益下降的平滑系数（每次 `pump` ≈ 8ms，0.25 约 30ms 走完）。
const GAIN_FALL: f32 = 0.25;
/// 增益上升的平滑系数（慢一些，避免「刚压下去又抬上来」的抽吸）。
const GAIN_RISE: f32 = 0.07;
/// 输出端限幅器的天花板。留 2% 余量给 `soften` 之后的取整/上混。
const LIMIT_CEIL: f32 = 0.98;
/// 限幅器的释放速度（每个样本向 1.0 逼近的比例）。
///
/// 48k 下约 120ms。太快会「一喘一喘」（每来一个音效就被按下去又抬起来，
/// 像 pumping）；太慢则整段都闷。120ms 听不出抽吸，也不会闷住下一句。
const LIMIT_RELEASE: f32 = 0.00017;
/// 限幅器压到这个比例以下算「异常」。
const LIMIT_FLOOR: f32 = 0.25;
/// 连续多少拍都异常才关流（一拍约 8ms，125 拍 ≈ 1 秒）。
const LIMIT_RUN: u32 = 125;
/// 开声之前先攒多少毫秒的数据（见 `Stream::out_started`）。
const PREFILL_MS: u32 = 40;
/// 建流失败后多久再试（应用可能只是暂时不出声 / 设备在切）。
const RETRY_AFTER: Duration = Duration::from_secs(10);
/// 接管期间把原会话压到多少（压 0 / 静音会让捕获变哑，见模块文档）。
///
/// 2% = -34dB：原声残留相对接管后的输出约 -41dB，听不出来；
/// 抓到的信号再乘 `1/ATTEN` = 50 倍补回来，float32 通路里没有量化损失。
pub(crate) const ATTEN: f32 = 0.02;
/// 写过 [`ATTEN`] 之后静音多久再放声。
///
/// 音量写下去到捕获流里反映出来有几十毫秒延迟，这段里抓到的还是压之前的满幅
/// 数据（实测：接管刚开的那一下设备电平表直接顶到 1.0，就是这一包乘了
/// `1/ATTEN` 放出去的）。固定等这么久，比「看见峰值变小才放行」可靠 ——
/// 后者会被内容里的静音间隙骗过。
const COMP_WINDOW: Duration = Duration::from_millis(150);

/// 引擎发给接管线程的一条目标。
#[derive(Clone, Debug, PartialEq)]
pub struct BoostTarget {
    pub pid: u32,
    /// 输出增益（1.0 = 原样，2.0 = +6dB）。
    pub gain: f32,
    /// 这个应用的声音现在在哪台端点上（`None` = 系统默认设备）。
    ///
    /// 引擎从会话扫描里拿到的是**会话实际挂着的设备**，也就是应用被指定到哪台
    /// 就是哪台。接管流按它渲染、按它读音量 / 压音量：认错设备的话，放大的那份
    /// 和原声会分头从两台设备出来，而且原声压不住。
    pub device: Option<String>,
}

/// 接管线程回报给引擎的事件。
#[derive(Clone, Debug)]
pub enum BoostEvent {
    /// 这个进程抓不了（独占模式 / DRM / 找不到会话），或者流中途断了。
    Failed {
        pid: u32,
        reason: String,
        /// 过一会儿还能再试吗。
        ///
        /// 「抓不了」是一次性的结论（独占模式 / DRM / 声音不在默认设备上），
        /// 本次运行里没必要再试；「流断了」（设备变化 / 持续削顶）是暂时的，
        /// 冷却 [`RETRY_AFTER`] 之后会重开 —— 引擎那边不能因此把这个应用
        /// 永久拉黑，否则一首歌里来一下大动态，后面就再也抬不起来了。
        retry: bool,
    },
    /// 这条流已经关掉、原会话音量也还回去了，引擎可以重新写这个应用的音量。
    ///
    /// 引擎在退出接管的那一拍**不能**写音量：那时流还在出声，写下去就是
    /// 「原声 + 放大后的接管流」两份 —— 听起来是回声，音量还会顶到削顶。
    Released { pid: u32 },
}

enum Command {
    Set(Vec<BoostTarget>),
    Shutdown,
}

/// 接管线程的句柄。引擎只拿着它发目标、收失败。
pub struct BoostHub {
    commands: Sender<Command>,
    events: Receiver<BoostEvent>,
    thread: Option<std::thread::JoinHandle<()>>,
}

/// 退出前把还没还回去的音量还回去。
///
/// `quit_now()` 走的是 `process::exit`，析构函数一个都不会跑，接管线程也就没机会
/// 调 `close()` —— 用户一退出，被接管的应用会一直哑在 2%。所以退出路径要先调它。
///
/// 不碰接管线程：直接照账本还原（账本就是为此存在的），这样不用等线程、也不用
/// 担心它正在 pump。
pub fn release_all() {
    std::thread::spawn(|| {
        let _com = ComApartment::init_mta();
        let Ok(enumerator) = com::create_device_enumerator() else {
            return;
        };
        restore_stale_volumes(&enumerator);
    })
    .join()
    .ok();
}

impl BoostHub {
    pub fn new() -> Self {
        let (command_tx, command_rx) = channel();
        let (event_tx, event_rx) = channel();
        let thread = std::thread::Builder::new()
            .name("bolume-boost".into())
            .spawn(move || {
                let _com = ComApartment::init_mta();
                let Ok(enumerator) = com::create_device_enumerator() else {
                    // 连设备都枚举不了，这一层等于不存在；不阻塞引擎。
                    return;
                };
                // 先把上次没来得及还的会话音量还回去，再开始干活。
                restore_stale_volumes(&enumerator);
                let mut hub = Hub {
                    commands: command_rx,
                    events: event_tx,
                    enumerator,
                    targets: Vec::new(),
                    streams: HashMap::new(),
                    known_volume: HashMap::new(),
                    failed: HashMap::new(),
                };
                hub.run();
            })
            .ok();
        Self {
            commands: command_tx,
            events: event_rx,
            thread,
        }
    }

    /// 覆盖当前的目标集合。只有变化时才调用（引擎那边自己判）。
    pub fn set_targets(&self, targets: Vec<BoostTarget>) {
        let _ = self.commands.send(Command::Set(targets));
    }

    /// 取走攒着的失败事件。
    pub fn drain_events(&self) -> Vec<BoostEvent> {
        let mut out = Vec::new();
        while let Ok(event) = self.events.try_recv() {
            out.push(event);
        }
        out
    }
}

impl Drop for BoostHub {
    fn drop(&mut self) {
        let _ = self.commands.send(Command::Shutdown);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

struct Hub {
    commands: Receiver<Command>,
    events: Sender<BoostEvent>,
    enumerator: IMMDeviceEnumerator,
    targets: Vec<BoostTarget>,
    streams: HashMap<u32, Stream>,
    /// pid → 我们压音量之前用户自己的音量。
    ///
    /// 应用**换设备**之后新会话会继承我们上次写的 [`ATTEN`]（实测：重开时
    /// `saved=0.020`）。那个 2% 是我们的手笔、不是用户的设置 —— 照它记账的话，
    /// 接管一结束就把应用还原成 2%，听着就是「一关均衡这个应用就哑了」。
    known_volume: HashMap<u32, f32>,
    /// pid → 上次失败的时刻，用来做重试冷却。
    failed: HashMap<u32, Instant>,
}

impl Hub {
    fn run(&mut self) {
        loop {
            match self.commands.recv_timeout(TICK) {
                Ok(Command::Set(targets)) => self.targets = targets,
                Ok(Command::Shutdown) | Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {}
            }
            // 命令可能是连着来的，把攒着的一次取完，别为每条都跑一遍 tick。
            loop {
                match self.commands.try_recv() {
                    Ok(Command::Set(targets)) => self.targets = targets,
                    Ok(Command::Shutdown) => {
                        self.shutdown();
                        return;
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => {
                        self.shutdown();
                        return;
                    }
                }
            }
            self.tick();
        }
        self.shutdown();
    }

    fn tick(&mut self) {
        let wanted: HashSet<u32> = self.targets.iter().map(|t| t.pid).collect();

        // 目标换了设备（用户在面板里把这个应用指到别的输出）：已经开着的流还接在
        // 旧端点上，放大的那份会继续从旧设备出来。先关掉（它会顺手把旧设备上的
        // 音量还原），等下一段按新设备重开。
        let moved: Vec<u32> = self
            .streams
            .iter()
            .filter(|(pid, stream)| {
                self.targets
                    .iter()
                    .find(|target| target.pid == **pid)
                    .is_some_and(|target| target.device != stream.device_id)
            })
            .map(|(pid, _)| *pid)
            .collect();
        for pid in moved {
            crate::platform::hotkey::trace(&format!("boost reopen pid {pid} (device changed)"));
            if let Some(stream) = self.streams.remove(&pid) {
                stream.close(&self.enumerator);
            }
            let _ = self.events.send(BoostEvent::Released { pid });
        }

        // 目标没了：`close()` 会先停流、再把原会话音量还回去。
        let gone: Vec<u32> = self
            .streams
            .keys()
            .copied()
            .filter(|pid| !wanted.contains(pid))
            .collect();
        for pid in gone {
            crate::platform::hotkey::trace(&format!("boost close pid {pid} (target gone)"));
            if let Some(stream) = self.streams.remove(&pid) {
                stream.close(&self.enumerator);
            }
            // 这个应用不再是接管目标了：记着的旧音量也一起丢掉（pid 会被复用）。
            self.known_volume.remove(&pid);
            // 不管有没有开过流都要回报：引擎那边可能在等这句才敢写音量。
            let _ = self.events.send(BoostEvent::Released { pid });
        }

        // 新目标：建流。
        for target in self.targets.clone() {
            if self.streams.contains_key(&target.pid) {
                continue;
            }
            if let Some(at) = self.failed.get(&target.pid) {
                if at.elapsed() < RETRY_AFTER {
                    continue;
                }
            }
            // 本程序是这个进程的后代时不能接管。
            //
            // process loopback 按**进程树**抓音频：从文件管理器（Dopus、Explorer
            // 的第三方替代品）里双击启动时，Bolume 就成了它的子进程，自己渲染出去
            // 那条流也在同一棵树里 —— 抓进来乘增益再送出去，是一个闭环回授。
            // 听感是一段不断重复、辨不出源头的怪音；源停了也不消失（回授自己维持），
            // 只有手动改一下那个应用的音量强行退出接管才会安静。
            if is_descendant_of(target.pid) {
                crate::platform::hotkey::trace(&format!(
                    "boost open REFUSED pid {} (本程序是它的子进程，会回授)",
                    target.pid
                ));
                self.failed.insert(target.pid, Instant::now());
                let _ = self.events.send(BoostEvent::Failed {
                    pid: target.pid,
                    reason: "Bolume 由该应用启动，接管会形成回授。请从桌面或开始菜单启动".into(),
                    retry: false,
                });
                continue;
            }
            match Stream::open(
                &self.enumerator,
                &target,
                self.known_volume.get(&target.pid).copied(),
            ) {
                Ok(stream) => {
                    self.failed.remove(&target.pid);
                    // 记下用户原本的音量（换设备后新会话会继承我们的 2%，见字段说明）。
                    if let Some(saved) = stream.saved_volume.filter(|v| *v > ATTEN * 1.2) {
                        self.known_volume.insert(target.pid, saved);
                    }
                    crate::platform::hotkey::trace(&format!(
                        "boost open pid {} dev={:?} saved={:?}",
                        target.pid, target.device, stream.saved_volume
                    ));
                    self.streams.insert(target.pid, stream);
                }
                Err(reason) => {
                    crate::platform::hotkey::trace(&format!(
                        "boost open FAILED pid {}: {reason}",
                        target.pid
                    ));
                    self.failed.insert(target.pid, Instant::now());
                    let _ = self.events.send(BoostEvent::Failed {
                        pid: target.pid,
                        reason,
                        retry: false,
                    });
                }
            }
        }

        // 增益随时在变（算法每 15ms 调一次）。
        //
        // 抓到的信号已经被会话音量压过 `ATTEN`，所以这里要除掉它，
        // 才能得到「乘完正好是 content × 增益」。
        for target in &self.targets {
            if let Some(stream) = self.streams.get_mut(&target.pid) {
                stream.gain_target = target.gain / ATTEN;
            }
        }

        // 「先听到、再压下去」：开流前就把会话压成 2% 的话，第一包起就全是
        // 静音（实测 in 恒 0）。所以先让捕获流跑起来、收到真的非静音数据，
        // 再把原会话压到 [`ATTEN`]。压完**不立即**放行 —— `zero_pending` 由
        // `pump()` 在捕获里真的看到信号变小之后才清（见那里的说明）。
        let mut not_attened: Vec<u32> = Vec::new();
        for stream in self.streams.values_mut() {
            if stream.zero_pending && stream.heard && stream.attened_at.is_none() {
                if stream.atten_retry_at.is_some_and(|at| at > Instant::now()) {
                    continue;
                }
                if let Some(saved) = stream.saved_volume {
                    let device = stream.device_id.as_deref();
                    if set_session_volume(&self.enumerator, device, stream.pid, ATTEN).is_some() {
                        // 写下去不等于真的生效（受保护的内容 / 独占模式 / 别的程序
                        // 正在改这一格）。没压住就绝不能放声：原声会和接管流
                        // （增益最高 8 / 2% = 400 倍）叠在一起，那就是「炸」。
                        let landed = session_volume(&self.enumerator, device, stream.pid)
                            .map(|value| value <= ATTEN * 2.0)
                            .unwrap_or(false);
                        if landed {
                            remember_zeroed(stream.pid, saved, device);
                            stream.attened_at = Some(Instant::now());
                            crate::platform::hotkey::trace(&format!(
                                "boost atten pid {} saved {saved:.3} -> {ATTEN}",
                                stream.pid
                            ));
                        } else if stream.atten_tries + 1 < ATTEN_TRIES {
                            // 隔一会儿再试。这期间 `zero_pending` 还挂着，一个样本都不放。
                            stream.atten_tries += 1;
                            stream.atten_retry_at = Some(Instant::now() + ATTEN_RETRY_EVERY);
                        } else {
                            let readback = session_volume(&self.enumerator, device, stream.pid)
                                .unwrap_or(-1.0);
                            crate::platform::hotkey::trace(&format!(
                                "boost atten FAILED pid {} saved {saved:.3} readback {readback:.3} tries {}",
                                stream.pid, stream.atten_tries + 1
                            ));
                            // 可能压了一半：先按记下的音量还原，再关流。
                            let _ = set_session_volume(&self.enumerator, device, stream.pid, saved);
                            not_attened.push(stream.pid);
                        }
                    }
                }
            }
        }
        for pid in not_attened {
            if let Some(stream) = self.streams.remove(&pid) {
                stream.close(&self.enumerator);
            }
            self.failed.insert(pid, Instant::now());
            let _ = self.events.send(BoostEvent::Failed {
                pid,
                reason: "原声压不下去（会话音量被别的程序占着）".into(),
                retry: true,
            });
        }

        let mut dead = Vec::new();
        for (pid, stream) in self.streams.iter_mut() {
            if !stream.pump() {
                dead.push(*pid);
            }
        }
        // 每秒一行诊断：`cap` 是这一秒捕获到的峰值，`heard` 是压之前捕获到的峰值，
        // `vol` 是回读到的会话音量，`ratio` 是压完之后量到的能量比。
        //
        // 三种组合各说明一件事：
        // * `cap ≈ heard × ATTEN`、`vol = 0.02`、`ratio ≈ 0.02` → 捕获在音量之后；
        // * `cap ≈ heard`、`vol = 0.02`、`ratio ≈ 1` → 捕获在音量之前（乘 50 会削顶）；
        // * `cap` 满幅、`vol = 1.0` → 会话被抬回去了（或者音频跑到了别的 pid）。
        for stream in self.streams.values_mut() {
            if stream.diag_at.elapsed() < Duration::from_secs(1) {
                continue;
            }
            let vol = session_volume(&self.enumerator, stream.device_id.as_deref(), stream.pid)
                .unwrap_or(-1.0);
            crate::platform::hotkey::trace(&format!(
                "boost level pid {} cap={:.4} heard={:.4} vol={vol:.3} ratio={:.3} gain={:.3} lim={:.3} zero={}",
                stream.pid,
                stream.cap_peak,
                stream.heard_peak,
                stream.seen_ratio,
                stream.gain,
                stream.lim,
                stream.zero_pending
            ));
            stream.diag_at = Instant::now();
            stream.cap_peak = 0.0;
        }

        // 兜底：接管期间会话音量被**外部**改大（在播放器界面里拖音量、系统
        // ducking 结束把别人的音量恢复回来），原声就回来了 —— 它会跟我们这条
        // 放大后的流叠在一起，听感是回声 + 爆音。
        //
        // 每 `GUARD_EVERY` 核对一次，偏离 [`ATTEN`] 就写回去。用户拖的是
        // Bolume 自己的滑条时走的是引擎那条路（会先退出接管、关流），
        // 这里最多抢跑半秒，随后 `close()` 会把音量还原。
        for stream in self.streams.values_mut() {
            let Some(attened_at) = stream.attened_at else {
                continue;
            };
            let every = if attened_at.elapsed() < GUARD_TIGHT {
                GUARD_TIGHT_EVERY
            } else {
                GUARD_EVERY
            };
            if stream.guard_at.elapsed() < every {
                continue;
            }
            stream.guard_at = Instant::now();
            let device = stream.device_id.as_deref();
            if let Some(volume) = session_volume(&self.enumerator, device, stream.pid) {
                if volume > ATTEN * 1.5 {
                    crate::platform::hotkey::trace(&format!(
                        "boost guard pid {} volume {volume:.3} -> {ATTEN}",
                        stream.pid
                    ));
                    let _ = set_session_volume(&self.enumerator, device, stream.pid, ATTEN);
                }
            }
        }

        for pid in dead {
            crate::platform::hotkey::trace(&format!("boost stream DEAD pid {pid}"));
            if let Some(stream) = self.streams.remove(&pid) {
                stream.close(&self.enumerator);
            }
            // 设备被拔了 / 一直削顶：算失败，等冷却后再来。
            self.failed.insert(pid, Instant::now());
            let _ = self.events.send(BoostEvent::Failed {
                pid,
                reason: "接管流中断（设备变化或输出持续削顶）".into(),
                retry: true,
            });
        }
    }

    fn shutdown(&mut self) {
        let streams: Vec<Stream> = self.streams.drain().map(|(_, s)| s).collect();
        for stream in streams {
            stream.close(&self.enumerator);
            let _ = self.events.send(BoostEvent::Released { pid: stream.pid });
        }
        self.targets.clear();
    }
}

/// 一条被接管的音频流。
struct Stream {
    pid: u32,
    /// 这条流接在哪台端点上（`None` = 系统默认设备）。
    ///
    /// 渲染、读音量、压音量、还原音量全都要用同一台：应用被指到别的设备之后
    /// 它的会话只存在于那台端点的会话表里。
    device_id: Option<String>,
    capture: IAudioClient,
    capture_client: IAudioCaptureClient,
    render: IAudioClient,
    render_client: IAudioRenderClient,
    channels: usize,
    max_frames: u32,
    /// 渲染端要压着的水位（帧）。
    pending_target: u32,
    ring: VecDeque<f32>,
    ring_cap: usize,
    /// 这一块实际乘的增益（向 `gain_target` 平滑靠拢）。
    gain: f32,
    /// AGC 要求的目标增益。
    ///
    /// 不能直接赋值给 `gain`：AGC 一次回退可能是 -3dB 以上（`OVER_FIT` 那条
    /// 按比例回退），8ms 内掉 30% 的幅度听感就是「被按了一下」。这里降得快
    /// （约 30ms 走完）、升得慢（约 150ms），两个方向都听不出台阶。
    gain_target: f32,
    /// 接管时把原会话压下去之前的音量（退出时按这个还原）。
    saved_volume: Option<f32>,
    /// 已经抓到过非静音数据（此时才敢把原会话压到 [`ATTEN`]）。
    heard: bool,
    /// 压下去之前抓到的最大峰值（诊断用）。
    heard_peak: f32,
    /// 压之前抓到的能量（平方和 + 样本数），用来和压之后的比。
    heard_sq: f64,
    heard_n: u64,
    /// 压之后 [`COMP_WINDOW`] 内抓到的能量（同上）。
    post_sq: f64,
    post_n: u64,
    /// 压完之后量到的「后段能量 / 前段能量」（**只做诊断**，不参与增益）。
    ///
    /// 按设计应该是 1/[`ATTEN`]（≈0.02，说明捕获点在会话音量之后）；要是接近 1，
    /// 说明捕获点在音量之前 —— 那种进程乘 50 倍会削顶。但这个比值只能当线索：
    /// 内容自己在变（压下去那几十毫秒抓到的还是压前的数据，后段就虚高），
    /// 拿它直接决定增益会算错，实测出现过的后果是「声音只剩一成」。
    seen_ratio: f32,
    /// 已经往会话写过 [`ATTEN`] 的时刻（`None` = 还没写）。
    attened_at: Option<Instant>,
    /// 压音量已经试了几次。
    ///
    /// 写下去之后可能被应用自己立刻抬回来（不少播放器每换一集就重写一次自己的
    /// 会话音量）。一次没压住就判失败太急：隔几十毫秒再试，`ATTEN_TRIES` 次都
    /// 不行才认定压不住、关流 —— 这一小段 `zero_pending` 还挂着，不会放声。
    atten_tries: u8,
    /// 下一次重试的时刻（`None` = 立刻可以试）。
    atten_retry_at: Option<Instant>,
    /// 还没把原会话压下去。这期间**只输出静音**。
    ///
    /// 压下去之前抓到的数据是**没被压过**的（捕获点在会话音量之前，见
    /// [`COMP_WINDOW`]），这一段要是被渲染出去就是「原声 × 补偿倍数」——
    /// 一开口就是一声炸。所以压之前一个样本都不往外送（也不进 ring）。
    zero_pending: bool,
    /// 连着多少拍输出顶到削顶。正常不该出现（AGC 的目标在 91.6% 以下），
    /// 出现了说明要么原声没压住、要么在自激，立刻关流重来。
    clip_run: u32,
    /// ring 攒够这么多样本才开始出声（见 `out_started`）。
    prefill: usize,
    /// 已经真的出过声了。
    ///
    /// 放行时 ring 是空的（`zero_pending` 期间一个样本都不收），立刻往渲染
    /// 缓冲里填就会欠载：输出变成一串「有声 / 静音」交替，听感是噼里啪啦、
    /// 音质很差 —— 用户说的「一开始播放音质很差地响几百毫秒」就是它。
    /// 先攒够 `prefill`（约 40ms）再开口；这一段原声还被按在 2%，
    /// 用户听不出来。
    out_started: bool,
    /// `zero_pending` 变 false 的时刻（攒不满时的兜底）。
    released_at: Option<Instant>,
    /// 上一次核对会话音量的时刻（见 `Hub::tick` 末尾那段兜底）。
    guard_at: Instant,
    /// 最近一秒里捕获到的峰值（诊断用，见 `Hub::tick` 里那行 `boost level`）。
    ///
    /// 要和 `heard_peak`（压之前的峰值）与回读的 `vol` 一起看，才能分清
    /// 「捕获在会话音量之后」「捕获在之前」「会话被抬回去了」这三种情况。
    cap_peak: f32,
    /// 上一次记诊断行的时刻。
    diag_at: Instant,
    /// 输出端限幅器当前的增益（1.0 = 没在压）。
    ///
    /// AGC 是「外部」的：它只能 15ms 读一次会话电平表，再用这个估计去决定
    /// 增益；视频里比 15ms 还短的瞬态（枪声、鼓点、音效）它根本看不见，等
    /// 看见时那一块已经放出去了。插件没有这个问题 —— 它在 DSP 里逐样本看。
    ///
    /// 所以这里补一道**逐样本**的限幅：按真实样本算，需要的增益比现在小就
    /// 立刻压下去（攻击沿第一个样本就生效，永远不会削顶），之后慢慢放回。
    /// 乘增益不会让波形变平，跟 `soften` 的波形压缩是两回事 —— 后者在
    /// 输入 1.6 时把波形按成 0.99，谐波失真一大就正是「炸」的听感。
    lim: f32,
}

/// 输出端的软拐点限幅。
///
/// AGC 一块（26ms）才走一步，接管流这一侧偶尔会比它更早涨上去（切换素材、
/// 起音、ring 里还没换成新增益的样本）。硬 clamp 会在 1.0 上削出方波，
/// 听感是刺耳的「嘶」；这里在拐点以下**一个样本都不动**，以上按双曲线压进
/// 1.0 —— 曲线在拐点处一阶连续，没有台阶，过冲只剩 26ms 也不会炸耳朵。
fn soften(value: f32) -> f32 {
    /// 拐点取 AGC 的防削顶线：这以下的信号完全不动。
    const KNEE: f32 = 0.916;
    let magnitude = value.abs();
    if magnitude <= KNEE {
        return value;
    }
    let room = 1.0 - KNEE;
    let over = magnitude - KNEE;
    let shaped = KNEE + room * (over / (room + over));
    shaped.copysign(value)
}

impl Stream {
    fn open(
        enumerator: &IMMDeviceEnumerator,
        target: &BoostTarget,
        known_volume: Option<f32>,
    ) -> Result<Self, String> {
        unsafe {
            let device = endpoint(enumerator, target.device.as_deref())
                .ok_or_else(|| "设备拿不到（指定设备不存在，默认设备也没有）".to_string())?;

            // 混音格式从渲染端拿：capture / render 两边必须用同一份，否则
            // process loopback 的 Initialize 会直接拒绝。
            let render: IAudioClient = device
                .Activate(CLSCTX_ALL, None)
                .map_err(|e| format!("渲染流拿不到：{e}"))?;
            let mix = render
                .GetMixFormat()
                .map_err(|e| format!("混音格式拿不到：{e}"))?;
            // GetMixFormat 返回的是 CoTaskMem 分配的变长结构：按 cbSize 整块拷出来，
            // 再把原块放掉。只拷 18 字节的 WAVEFORMATEX 会丢掉 EXTENSIBLE 的 SubFormat。
            let base = *mix;
            // cbSize 是驱动给的，异常值会让下面这句按天文数字拷内存。
            // （WAVEFORMATEX 是 packed，字段得先读进局部变量再比较。）
            let cb_size = base.cbSize;
            if cb_size > 64 {
                CoTaskMemFree(Some(mix as *const c_void));
                return Err(format!("混音格式的 cbSize 异常：{cb_size}"));
            }
            let size = std::mem::size_of::<WAVEFORMATEX>() + cb_size as usize;
            let mut owned = vec![0u8; size];
            std::ptr::copy_nonoverlapping(mix as *const u8, owned.as_mut_ptr(), size);
            CoTaskMemFree(Some(mix as *const c_void));

            let raw = &*(owned.as_ptr() as *const WAVEFORMATEX);
            let format = owned.as_ptr() as *const WAVEFORMATEX;
            let channels = raw.nChannels as usize;
            let rate = raw.nSamplesPerSec;
            if channels == 0 || rate == 0 {
                return Err("混音格式不合法".into());
            }
            // process loopback 只吃 32 位浮点（共享模式的混音格式本来就是）。
            let bits = raw.wBitsPerSample;
            let tag = raw.wFormatTag;
            let is_float = tag == 3 /* WAVE_FORMAT_IEEE_FLOAT */ || (tag == 0xFFFE && bits == 32);
            if !is_float {
                return Err(format!("混音格式不是 32 位浮点（{bits} 位，tag {tag}）"));
            }

            // ---- 捕获：按 pid 抓这个进程（含子进程树） ----
            let capture = open_process_loopback(target.pid)?;
            capture
                .Initialize(
                    AUDCLNT_SHAREMODE_SHARED,
                    AUDCLNT_STREAMFLAGS_LOOPBACK,
                    0,
                    0,
                    format,
                    None,
                )
                .map_err(|e| format!("捕获流初始化失败：{e}"))?;
            let capture_client: IAudioCaptureClient = capture
                .GetService()
                .map_err(|e| format!("捕获接口拿不到：{e}"))?;

            // ---- 渲染：回到同一台端点（不是「默认设备」） ----
            // buffer 按 4 倍水位请求：水位靠 tick 维持，缓冲留够就不会欠载。
            let duration = (PENDING_MS as i64 * 4) * 10_000; // 100ns 单位
            render
                .Initialize(AUDCLNT_SHAREMODE_SHARED, 0, duration, 0, format, None)
                .map_err(|e| format!("渲染流初始化失败：{e}"))?;
            let render_client: IAudioRenderClient = render
                .GetService()
                .map_err(|e| format!("渲染接口拿不到：{e}"))?;
            let max_frames = render.GetBufferSize().unwrap_or(0);
            if max_frames == 0 {
                return Err("渲染缓冲区是 0".into());
            }

            // 先把整块缓冲填成静音再 Start，否则开头会读到未初始化的内存。
            if let Ok(buffer) = render_client.GetBuffer(max_frames) {
                std::ptr::write_bytes(buffer, 0, (max_frames as usize) * channels * 4);
                let _ = render_client.ReleaseBuffer(max_frames, 0);
            }

            if let Err(e) = capture.Start() {
                return Err(format!("捕获流启动失败：{e}"));
            }
            if let Err(e) = render.Start() {
                let _ = capture.Stop();
                return Err(format!("渲染流启动失败：{e}"));
            }

            let frames_per_ms = (rate as u32 / 1000).max(1);
            let mut stream = Self {
                pid: target.pid,
                device_id: target.device.clone(),
                capture,
                capture_client,
                render,
                render_client,
                channels,
                max_frames,
                pending_target: (PENDING_MS * frames_per_ms).min(max_frames / 2).max(1),
                ring: VecDeque::new(),
                ring_cap: (RING_MS * frames_per_ms) as usize * channels,
                // 抓到的信号已经被会话音量压过 `ATTEN`，所以乘的是「目标 / ATTEN」。
                // 开流就按正确值起，别让平滑从 1.0 慢慢爬（那样头一秒会很轻）。
                gain: target.gain / ATTEN,
                gain_target: target.gain / ATTEN,
                saved_volume: None,
                heard: false,
                heard_peak: 0.0,
                heard_sq: 0.0,
                heard_n: 0,
                post_sq: 0.0,
                post_n: 0,
                seen_ratio: 0.0,
                attened_at: None,
                atten_tries: 0,
                atten_retry_at: None,
                zero_pending: true,
                clip_run: 0,
                prefill: (PREFILL_MS * frames_per_ms) as usize * channels,
                out_started: false,
                released_at: None,
                guard_at: Instant::now(),
                cap_peak: 0.0,
                diag_at: Instant::now(),
                lim: 1.0,
            };
            // 让原声让位：把会话音量压到 [`ATTEN`]（**不是静音**，静音会让捕获变哑，
            // 见模块文档）。这里还不压：先等抓到了真的非静音数据（见 `tick`），
            // 免得接管还没出声就把应用弄哑（那一小段听得出来）。
            let Some(saved) = session_volume(enumerator, target.device.as_deref(), target.pid) else {
                // 找不到会话 = 这个 pid 的声音不在我们以为的那台设备上，接管没有意义。
                let _ = stream.render.Stop();
                let _ = stream.capture.Stop();
                return Err("这台设备上找不到这个进程的会话".into());
            };
            // 读到的音量如果已经是被我们压过的（换设备后新会话会继承那个 2%），
            // 就换成这一轮之前记下的用户音量 —— 不然退出接管时会把应用还原成 2%。
            //
            // 账上也没有（进程重启、账本丢了）时按 1.0 算：2% 只可能是我们上一轮
            // 压下去又没来得及还的，当成用户的音量会让这个应用一直哑着。
            let saved = if saved <= ATTEN * 1.2 {
                known_volume
                    .filter(|value| *value > ATTEN * 1.2)
                    .unwrap_or(1.0)
            } else {
                saved
            };
            stream.saved_volume = Some(saved);
            Ok(stream)
        }
    }

    /// 搬一次数据。返回 false 表示这条流该关了。
    fn pump(&mut self) -> bool {
        // 增益平滑（见 `gain_target`）：一次掉太多会「被按一下」。
        let step = if self.gain_target < self.gain { GAIN_FALL } else { GAIN_RISE };
        self.gain += (self.gain_target - self.gain) * step;
        unsafe {
            // ---- 捕获 → ring ----
            loop {
                let size = match self.capture_client.GetNextPacketSize() {
                    Ok(size) => size,
                    Err(_) => return false,
                };
                if size == 0 {
                    break;
                }
                let mut data: *mut u8 = std::ptr::null_mut();
                let mut frames = 0u32;
                let mut flags = 0u32;
                if self
                    .capture_client
                    .GetBuffer(&mut data, &mut frames, &mut flags, None, None)
                    .is_err()
                {
                    return false;
                }
                if frames == 0 {
                    let _ = self.capture_client.ReleaseBuffer(0);
                    break;
                }
                let silent = flags & (AUDCLNT_BUFFERFLAGS_SILENT.0 as u32) != 0;
                let samples = (frames as usize) * self.channels;
                if silent || data.is_null() {
                    if !self.zero_pending {
                        for _ in 0..samples {
                            self.ring.push_back(0.0);
                        }
                    }
                } else {
                    let slice = std::slice::from_raw_parts(data as *const f32, samples);
                    let peak = slice.iter().fold(0.0f32, |max, value| max.max(value.abs()));
                    self.cap_peak = self.cap_peak.max(peak);
                    if self.zero_pending {
                        let energy: f64 = slice.iter().map(|v| (*v as f64) * (*v as f64)).sum();
                        if self.attened_at.is_none() {
                            // 压之前：记下「内容本身的能量」，用来和压之后的比。
                            self.heard_peak = self.heard_peak.max(peak);
                            self.heard_sq += energy;
                            self.heard_n += samples as u64;
                            if !self.heard {
                                self.heard = peak > 0.0001;
                            }
                        } else {
                            // 压之后：再攒满 [`COMP_WINDOW`]，然后量捕获点在哪一侧。
                            self.post_sq += energy;
                            self.post_n += samples as u64;
                            if self.settle() {
                                self.zero_pending = false;
                                self.released_at = Some(Instant::now());
                            }
                        }
                    }
                    // 还没放行：这一段没经过（或者没确认经过）判定，进 ring 就有
                    // 乘错倍数的风险，所以一个样本都不收。
                    if !self.zero_pending {
                        self.ring.extend(slice.iter().copied());
                    }
                }
                let _ = self.capture_client.ReleaseBuffer(frames);
                // 攒太多就丢最老的（延迟不能越滚越大）。
                while self.ring.len() > self.ring_cap {
                    self.ring.pop_front();
                }
            }

            // 每秒一行的诊断在 `Hub::tick` 里（那边才有设备枚举器，能回读会话音量）。

            // ---- ring → 渲染，压着水位填 ----
            let padding = match self.render.GetCurrentPadding() {
                Ok(padding) => padding,
                Err(_) => return false,
            };
            let space = self.max_frames.saturating_sub(padding);
            let fill = self.pending_target.saturating_sub(padding).min(space);
            if fill == 0 {
                return true;
            }
            let Ok(buffer) = self.render_client.GetBuffer(fill) else {
                return false;
            };
            let out = std::slice::from_raw_parts_mut(buffer as *mut f32, (fill as usize) * self.channels);
            let starving = !self.out_started
                && self.ring.len() < self.prefill
                && self
                    .released_at
                    .map(|at| at.elapsed() < Duration::from_millis(1500))
                    .unwrap_or(true);
            if self.zero_pending || starving {
                // 原声还没让位、或者 ring 还没攒够：这一小段一个字都不出
                // （见 `zero_pending` / `out_started` 的说明）。
                out.fill(0.0);
            } else {
                self.out_started = true;
                let gain = self.gain;
                // 先看一眼**这一整块**要输出的样本，需要压就现在压。
                //
                // 逐样本压也来得及（第一个超线的样本就把增益拉下来），但那是
                // 在块中间突然掉一截，听感是「咔」。这里提前一块把增益压到位，
                // 块内就是平的 —— 相当于给限幅器做了 lookahead。
                let look = (fill as usize * self.channels).min(self.ring.len());
                let ahead = self
                    .ring
                    .iter()
                    .take(look)
                    .fold(0.0f32, |max, value| max.max(value.abs()))
                    * gain;
                if ahead > LIMIT_CEIL {
                    let need = LIMIT_CEIL / ahead;
                    if need < self.lim {
                        self.lim = need;
                    }
                }
                let mut peak = 0.0f32;
                let mut lim_min = self.lim;
                for sample in out.iter_mut() {
                    let raw = match self.ring.pop_front() {
                        Some(value) => value * gain,
                        // 欠载就补静音，别把上一轮的样本留着重复播。
                        None => 0.0,
                    };
                    // 逐样本限幅（见 `lim` 的说明）：要压就立刻压，之后慢慢放。
                    let magnitude = raw.abs();
                    let need = if magnitude > LIMIT_CEIL {
                        LIMIT_CEIL / magnitude
                    } else {
                        1.0
                    };
                    if need < self.lim {
                        self.lim = need;
                    } else {
                        self.lim += (1.0 - self.lim) * LIMIT_RELEASE;
                        if self.lim > 1.0 {
                            self.lim = 1.0;
                        }
                    }
                    lim_min = lim_min.min(self.lim);
                    // `soften` 只当最后一道兜底（限幅之后几乎不会触发）。
                    let value = soften(raw * self.lim);
                    peak = peak.max(value.abs());
                    *sample = value;
                }
                // 限幅器长期贴地 = AGC 给的增益远超实际内容（或者真在自激）。
                // 输出一直被压十几 dB，继续放着只会越来越闷，关流重来。
                if lim_min < LIMIT_FLOOR {
                    self.clip_run += 1;
                } else {
                    self.clip_run = 0;
                }
                if self.clip_run > LIMIT_RUN {
                    return false;
                }
                // 自激保护：AGC 的目标在 91.6% 以下，长期顶满说明有东西在回授
                // （原声没压住 / 抓到了自己），继续放就是越放越响。
                if peak > 0.999 {
                    self.clip_run += 1;
                } else {
                    self.clip_run = 0;
                }
                if self.clip_run > 24 {
                    return false;
                }
            }
            if self.render_client.ReleaseBuffer(fill, 0).is_err() {
                return false;
            }
            true
        }
    }

    /// 压完之后静音一段，够 [`COMP_WINDOW`] 就放行。
    ///
    /// 为什么要等：音量写下去到捕获流里反映出来有几十毫秒延迟，这段里抓到的还是
    /// 压之前的满幅数据 —— 放出去就是「原声 × 50」，一开口一声炸。
    ///
    /// 顺手把「后段能量 / 前段能量」记进 `seen_ratio`，**只做诊断**：正常应该是
    /// 0.02 上下（捕获在会话音量之后）。拿它决定增益是不行的 —— 内容自己在变，
    /// 后段常常虚高，实测出现过「判成 1 倍 → 声音只剩一成」。
    fn settle(&mut self) -> bool {
        let Some(written) = self.attened_at else {
            return false;
        };
        if written.elapsed() < COMP_WINDOW {
            return false;
        }
        let heard = if self.heard_n > 0 {
            (self.heard_sq / self.heard_n as f64).sqrt() as f32
        } else {
            0.0
        };
        let post = if self.post_n > 0 {
            (self.post_sq / self.post_n as f64).sqrt() as f32
        } else {
            0.0
        };
        self.seen_ratio = if heard < 0.0005 || post <= 0.0 {
            0.0
        } else {
            post / heard
        };
        crate::platform::hotkey::trace(&format!(
            "boost release pid {} heard={heard:.4} post={post:.4} ratio={:.3} waited={}ms",
            self.pid,
            self.seen_ratio,
            written.elapsed().as_millis()
        ));
        true
    }

    /// 停流并把原会话的音量还回去。
    fn close(&self, enumerator: &IMMDeviceEnumerator) {
        unsafe {
            let _ = self.render.Stop();
            let _ = self.capture.Stop();
        }
        if let Some(saved) = self.saved_volume.filter(|_| self.attened_at.is_some()) {
            restore_session_volume(enumerator, self.device_id.as_deref(), self.pid, saved);
            forget_zeroed(self.pid);
        }
    }
}
/// 「我们把哪些应用按到了 0 音量、原来是多响、在哪台设备上」的账本。
///
/// 正常退出时 `close()` 会把音量还回去，但用户用任务管理器强杀、或者进程崩了，
/// 那段代码根本没机会跑 —— 被接管的那个应用就会一直哑着，直到用户自己去调音量。
/// 所以这里落一份账（pid + 原音量 + 端点 id），下次启动先照着它还原。
///
/// 第三项（端点 id）是后来加的：应用被指到非默认设备时，音量的读写都在那台端点
/// 的会话表上，还账也得回到那台。老账本只有两项，按默认设备还原。
fn zero_ledger() -> std::path::PathBuf {
    std::env::temp_dir().join("bolume-boost-zeroed.txt")
}

/// 账本里「没有特定设备」的占位符（端点 id 里不会出现 `-` 单独一项）。
const LEDGER_ANY_DEVICE: &str = "-";

fn remember_zeroed(pid: u32, volume: f32, device: Option<&str>) {
    use std::io::Write;
    if let Ok(mut file) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(zero_ledger())
    {
        let _ = writeln!(
            file,
            "{pid} {volume:.4} {}",
            device.unwrap_or(LEDGER_ANY_DEVICE)
        );
    }
}

fn forget_zeroed(pid: u32) {
    let path = zero_ledger();
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    let needle = pid.to_string();
    let kept: Vec<&str> = text
        .lines()
        .filter(|line| {
            line.split_whitespace().next().map(str::trim) != Some(needle.as_str())
        })
        .collect();
    if kept.is_empty() {
        let _ = std::fs::remove_file(&path);
    } else {
        let _ = std::fs::write(&path, kept.join("\n") + "\n");
    }
}

/// 启动时把上次没来得及还的音量还回去。
fn restore_stale_volumes(enumerator: &IMMDeviceEnumerator) {
    let path = zero_ledger();
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    for line in text.lines() {
        let mut parts = line.split_whitespace();
        let (Some(pid), Some(volume)) = (parts.next(), parts.next()) else {
            continue;
        };
        let (Ok(pid), Ok(volume)) = (pid.parse::<u32>(), volume.parse::<f32>()) else {
            continue;
        };
        // pid 0 是系统声音，我们从来不接管它。
        if pid == 0 {
            continue;
        }
        let device = parts.next().filter(|id| *id != LEDGER_ANY_DEVICE);
        restore_session_volume(enumerator, device, pid, volume);
    }
    let _ = std::fs::remove_file(&path);
}

/// 取一台端点：给了 id 就按 id 取，取不到再退回系统默认设备。
///
/// 「按 id 取不到」不是错误：设备被拔了 / 应用被指到一台已经消失的设备时，
/// 退回默认设备至少能把声音接住，比整条接管直接失败强。
fn endpoint(enumerator: &IMMDeviceEnumerator, device: Option<&str>) -> Option<IMMDevice> {
    unsafe {
        if let Some(id) = device {
            if let Ok(found) = enumerator.GetDevice(com::pcwstr(&com::wide(id))) {
                return Some(found);
            }
        }
        enumerator.GetDefaultAudioEndpoint(eRender, eMultimedia).ok()
    }
}

/// 本进程是不是 `pid` 的后代（含它自己）。见 [`crate::platform::detach`]。
fn is_descendant_of(pid: u32) -> bool {
    crate::platform::detach::is_descendant_of(pid)
}

/// 这条会话是不是已经结束了（进程退出后系统还没收走）。
///
/// 过期会话不能算：写进去也不生效、读出来还是旧值 —— 拿它当「还有会话在满幅放」
/// 的证据，压完回读就永远过不了，接管流会被判失败直接关掉，听感是「开了均衡没反应」。
fn expired(control: &IAudioSessionControl) -> bool {
    // AudioSessionState：0 未激活、1 激活、2 已过期。
    unsafe { control.GetState() }.map(|value| value.0 == 2).unwrap_or(false)
}

/// 读某个 pid 在指定端点上的会话音量，取**所有**会话里最大的那个。
///
/// 一个进程可以有多条会话（浏览器换流、播放器重开音频端点时会新建一条，新会话
/// 一律从 100% 起）。只读第一条会被这种情况骗过去：第一条还被我们压着 2%，
/// 第二条已经在满幅放原声 —— 原声叠在接管流上，听感是回声 + 爆音。取最大值，
/// 「有没有会话还在满幅放」这个问题才回答得对。
///
/// 找不到会话返回 `None` —— 说明这个进程的声音根本不在这台端点上。
fn session_volume(enumerator: &IMMDeviceEnumerator, device: Option<&str>, pid: u32) -> Option<f32> {
    unsafe {
        let device = endpoint(enumerator, device)?;
        let manager = device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None).ok()?;
        let list = manager.GetSessionEnumerator().ok()?;
        let count = list.GetCount().ok()?;
        let mut loudest: Option<f32> = None;
        for index in 0..count {
            let Ok(control) = list.GetSession(index) else {
                continue;
            };
            let Ok(control2) = control.cast::<IAudioSessionControl2>() else {
                continue;
            };
            if control2.GetProcessId().unwrap_or(0) != pid {
                continue;
            }
            if expired(&control) {
                continue;
            }
            let Ok(volume) = control.cast::<ISimpleAudioVolume>() else {
                continue;
            };
            let level = volume.GetMasterVolume().unwrap_or(1.0);
            loudest = Some(loudest.map_or(level, |max: f32| max.max(level)));
        }
        loudest
    }
}

/// 把某个 pid 在指定端点上的所有会话压到 `volume_level`。
///
/// 返回它原来的音量（所有会话里最大的那个）；找不到会话返回 `None` ——
/// 那就说明这个进程的声音根本不在这台端点上。
fn set_session_volume(
    enumerator: &IMMDeviceEnumerator,
    device: Option<&str>,
    pid: u32,
    volume_level: f32,
) -> Option<f32> {
    unsafe {
        let device = endpoint(enumerator, device)?;
        let manager = device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None).ok()?;
        let list = manager.GetSessionEnumerator().ok()?;
        let count = list.GetCount().ok()?;
        let context = GUID::zeroed();
        let mut previous: Option<f32> = None;
        for index in 0..count {
            let Ok(control) = list.GetSession(index) else {
                continue;
            };
            let Ok(control2) = control.cast::<IAudioSessionControl2>() else {
                continue;
            };
            if control2.GetProcessId().unwrap_or(0) != pid {
                continue;
            }
            if expired(&control) {
                continue;
            }
            let Ok(volume) = control.cast::<ISimpleAudioVolume>() else {
                continue;
            };
            let level = volume.GetMasterVolume().unwrap_or(1.0);
            previous = Some(previous.map_or(level, |max: f32| max.max(level)));
            let _ = volume.SetMasterVolume(volume_level, &context);
        }
        previous
    }
}

/// 还原会话音量。
///
/// 只在它还停在 [`ATTEN`] 的时候还原：中途要是有人自己调过，说明我们记的那个
/// 旧值已经过时了，别把人家刚调好的值盖回去。
fn restore_session_volume(
    enumerator: &IMMDeviceEnumerator,
    device: Option<&str>,
    pid: u32,
    saved: f32,
) {
    unsafe {
        let Some(device) = endpoint(enumerator, device) else {
            return;
        };
        let Ok(manager) = device.Activate::<IAudioSessionManager2>(CLSCTX_ALL, None) else {
            return;
        };
        let Ok(list) = manager.GetSessionEnumerator() else {
            return;
        };
        let Ok(count) = list.GetCount() else {
            return;
        };
        let context = GUID::zeroed();
        for index in 0..count {
            let Ok(control) = list.GetSession(index) else {
                continue;
            };
            let Ok(control2) = control.cast::<IAudioSessionControl2>() else {
                continue;
            };
            if control2.GetProcessId().unwrap_or(0) != pid {
                continue;
            }
            if expired(&control) {
                continue;
            }
            let Ok(volume) = control.cast::<ISimpleAudioVolume>() else {
                continue;
            };
            if (volume.GetMasterVolume().unwrap_or(1.0) - ATTEN).abs() > 0.002 {
                continue;
            }
            let _ = volume.SetMasterVolume(saved, &context);
        }
    }
}


/// 用 process loopback 打开某个进程的捕获流。
fn open_process_loopback(pid: u32) -> Result<IAudioClient, String> {
    let params = AUDIOCLIENT_ACTIVATION_PARAMS {
        ActivationType: AUDIOCLIENT_ACTIVATION_TYPE_PROCESS_LOOPBACK,
        Anonymous: AUDIOCLIENT_ACTIVATION_PARAMS_0 {
            ProcessLoopbackParams: AUDIOCLIENT_PROCESS_LOOPBACK_PARAMS {
                TargetProcessId: pid,
                ProcessLoopbackMode: PROCESS_LOOPBACK_MODE_INCLUDE_TARGET_PROCESS_TREE,
            },
        },
    };
    unsafe {
        // 这里必须自己拼一个 VT_BLOB：`InitPropVariantFromBuffer` 生成的是
        // VT_VECTOR|VT_UI1（实测 vt = 0x1011），而 `ActivateAudioInterfaceAsync`
        // 只认 VT_BLOB，传前者直接返回 E_INVALIDARG。
        //
        // blob 的数据**必须**用 COM 的任务分配器：`PROPVARIANT` 析构时会调用
        // `PropVariantClear`，它对 VT_BLOB 就是一句 `CoTaskMemFree(pBlobData)`。
        // 指向栈上结构体的话，函数返回时等于去 free 一个栈地址 —— 整个进程
        // 直接以 0xC0000409（堆损坏）崩掉，而且崩得很晚，看不出凶手是谁。
        let size = std::mem::size_of::<AUDIOCLIENT_ACTIVATION_PARAMS>();
        let blob = CoTaskMemAlloc(size).cast::<u8>();
        if blob.is_null() {
            return Err("blob 内存分配失败".into());
        }
        std::ptr::copy_nonoverlapping(&params as *const _ as *const u8, blob, size);

        let mut prop = PROPVARIANT::default();
        {
            let inner = &mut *prop.Anonymous.Anonymous;
            inner.vt = VT_BLOB;
            inner.Anonymous.blob.cbSize = size as u32;
            inner.Anonymous.blob.pBlobData = blob;
        }

        let handler = Activation {
            ready: Arc::new((Mutex::new(false), Condvar::new())),
            result: Arc::new(Mutex::new(None)),
        };
        let completion: IActivateAudioInterfaceCompletionHandler = handler.clone().into();
        let operation = ActivateAudioInterfaceAsync(
            VIRTUAL_AUDIO_DEVICE_PROCESS_LOOPBACK,
            &IAudioClient::IID,
            Some(&prop),
            &completion,
        )
        .map_err(|e| format!("ActivateAudioInterfaceAsync 失败：{e}"))?;

        // 回调在别的线程上跑，这里等它把结果放进 result。
        let (lock, condvar) = &*handler.ready;
        let mut done = lock.lock().unwrap_or_else(|e| e.into_inner());
        let deadline = Instant::now() + Duration::from_secs(2);
        while !*done {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                break;
            }
            let (guard, _) = condvar
                .wait_timeout(done, left)
                .unwrap_or_else(|e| e.into_inner());
            done = guard;
        }
        drop(done);
        drop(operation);

        match handler.result.lock().unwrap_or_else(|e| e.into_inner()).take() {
            Some(Ok(client)) => Ok(client),
            Some(Err(e)) => Err(format!("进程回环激活失败：{e}")),
            None => Err("进程回环激活超时（可能是独占模式或系统太旧）".into()),
        }
    }
}

/// `ActivateAudioInterfaceAsync` 的完成回调。
///
/// 回调在别的线程上跑，结果靠 `ready` 那个 Condvar 交回等待方。
#[implement(IActivateAudioInterfaceCompletionHandler)]
struct Activation {
    ready: Arc<(Mutex<bool>, Condvar)>,
    result: Arc<Mutex<Option<windows::core::Result<IAudioClient>>>>,
}

impl Clone for Activation {
    fn clone(&self) -> Self {
        Self {
            ready: self.ready.clone(),
            result: self.result.clone(),
        }
    }
}

impl IActivateAudioInterfaceCompletionHandler_Impl for Activation_Impl {
    fn ActivateCompleted(
        &self,
        operation: Ref<'_, IActivateAudioInterfaceAsyncOperation>,
    ) -> windows::core::Result<()> {
        let mut hr = windows::core::HRESULT(0);
        let mut interface: Option<windows::core::IUnknown> = None;
        let client = unsafe {
            match operation.ok() {
                Ok(op) => op
                    .GetActivateResult(&mut hr, &mut interface)
                    .and_then(|_| hr.ok())
                    .and_then(|_| {
                        interface.ok_or_else(|| windows::core::Error::from(E_NOINTERFACE))
                    })
                    .and_then(|unknown| unknown.cast::<IAudioClient>()),
                // 操作对象拿不到就等于激活失败。
                Err(_) => Err(windows::core::Error::from(E_NOINTERFACE)),
            }
        };
        if let Ok(mut slot) = self.result.lock() {
            *slot = Some(client);
        }
        let (lock, condvar) = &*self.ready;
        if let Ok(mut done) = lock.lock() {
            *done = true;
            condvar.notify_all();
        }
        Ok(())
    }
}
