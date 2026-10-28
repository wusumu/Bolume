//! 音量音效：调音量时每变化一档响一下。
//!
//! 播放走 winmm 的 `waveOut`：开机第一次播的时候开一个常驻句柄，之后每一发都是
//! 「准备缓冲 → 丢进去」，几毫秒就能听见。这里干三件事：
//!
//! - 内置音效在代码里合成（44.1k / 16 位 / 单声道），不占体积、不挑设备；
//! - 自定义文件读进来按「音效音量」缩放样本，再拼回一段 WAV；
//! - 两份结果都缓存在内存里，`waveOutWrite` 之后那块内存要一直活着，所以缓冲
//!   由 [`Voice`] 持有，播完了才回收。
//!
//! # 为什么不用 `PlaySoundW`
//!
//! 最开始用的是 `PlaySoundW(SND_MEMORY | SND_ASYNC)`，单发没问题，可拖滑条时
//! 每 30ms 一发，实测**一声都听不到**：PlaySound 每次调用都会先掐掉上一声再开
//! 新的，而它自己"起播"要一两百毫秒 —— 每一发都死在起跑线上，端点电平表全程
//! 0.00。加 `SND_NOSTOP` 只让它变成「约 300ms 才响一下」，还是跟不上手感。
//! `waveOut` 常开句柄没有这个启动开销，缓冲直接排队，快拖就是一串点子。
//!
//! 另有一个「松手提示音」（[`tick`]），那是面板松手时走的系统提示音，和这里无关。

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    mem::size_of,
    sync::Arc,
    time::{Duration, Instant},
};

use windows::Win32::Media::Audio::{
    PlaySoundW, CALLBACK_NULL, HWAVEOUT, SND_ASYNC, SND_MEMORY, SND_NODEFAULT, SND_NOSTOP, WAVEHDR,
    WAVE_FORMAT_PCM, WAVE_MAPPER, WAVEFORMATEX, WHDR_DONE, waveOutOpen, waveOutPrepareHeader,
    waveOutUnprepareHeader, waveOutWrite,
};
use windows::core::{PCWSTR, PSTR};

use crate::config::{Config, VolumeGesture, VolumeSoundStyle};

/// 两次音效之间至少隔这么久。
///
/// 拖滑条时一帧一个值，不拦就是「嗡嗡」一片而不是一下一下的嗒。40ms ≈ 25 下 / 秒：
/// 快拖听得出一串点子（正好是反馈），慢拖每格都脆生生一下。
const MIN_GAP: Duration = Duration::from_millis(40);

/// 合成用的采样率（输出统一成这个）。
const RATE: u32 = 44_100;

/// 同时在途的缓冲上限。
///
/// 排满就不再收新的：快拖滑条时缓冲一旦堆积，手停下来还会「叮叮」好几声，
/// 听感就是「音效跟不上手」。2 层 = 一个在放、一个等着，短音效照样连成串。
/// 铃音也在这个额度里 —— 它已经从 300ms 收到 150ms（见 `synth`），
/// 就是为了别把队列占满。
const MAX_VOICES: usize = 2;

/// 自定义文件最多取这么长（秒）：音效是「一下」，不是播放器。
/// 挑了一首整曲进来也只放开头，免得拖一下滑条放半分钟。
const MAX_SECONDS: f32 = 3.0;

/// 缓存总时长上限（秒）：攒够两分钟就整个清掉重来。
///
/// 每个音源 × 每档音量都是一份，正常用不到这么多；万一有人来回拧音量旋钮，
/// 这里兜底，别让它一路涨上去。
const CACHE_LIMIT: usize = 2 * 1024 * 1024;

thread_local! {
    /// 音源 + 音效音量 → 完整 WAV 字节（自定义文件那条路要它来缩放）。
    static CACHE: RefCell<HashMap<String, Arc<Vec<u8>>>> = RefCell::new(HashMap::new());
    /// 音源 + 音量 + 采样率 → 已经转成播放器口径的 PCM。
    static CLIPS: RefCell<HashMap<String, Arc<Clip>>> = RefCell::new(HashMap::new());
    /// 上一次响的时刻（节流用）。
    static LAST: Cell<Option<Instant>> = const { Cell::new(None) };
    /// 常驻播放通道；开不出来就退回 `PlaySound`。
    static SINK: RefCell<Option<Sink>> = const { RefCell::new(None) };
}

/// 一段可以直接塞进 `waveOut` 的音频：单声道 / 16 位 / 播放器的采样率。
struct Clip {
    audio: Vec<u8>,
}

/// 在途缓冲：`header` 的地址和它指向的内存都得活到播完。
struct Voice {
    header: Box<WAVEHDR>,
    clip: Arc<Clip>,
}

/// 常开的 `waveOut` 通道。
struct Player {
    handle: HWAVEOUT,
    rate: u32,
    voices: Vec<Voice>,
}

impl Player {
    /// 开机第一次播的时候开通道：先按合成的 44.1k 开，不行退到 48k。
    fn open() -> Option<Self> {
        for rate in [RATE, 48_000] {
            let format = WAVEFORMATEX {
                wFormatTag: WAVE_FORMAT_PCM as u16,
                nChannels: 1,
                nSamplesPerSec: rate,
                nAvgBytesPerSec: rate * 2,
                nBlockAlign: 2,
                wBitsPerSample: 16,
                cbSize: 0,
            };
            let mut handle = HWAVEOUT(std::ptr::null_mut());
            let result =
                unsafe { waveOutOpen(Some(&mut handle), WAVE_MAPPER, &format, None, None, CALLBACK_NULL) };
            if result == 0 {
                return Some(Self {
                    handle,
                    rate,
                    voices: Vec::new(),
                });
            }
        }
        None
    }

    fn write(&mut self, clip: &Arc<Clip>) {
        self.reap();
        if self.voices.len() >= MAX_VOICES {
            return;
        }
        let mut header = Box::new(WAVEHDR {
            lpData: PSTR(clip.audio.as_ptr() as *mut u8),
            dwBufferLength: clip.audio.len() as u32,
            ..Default::default()
        });
        let size = size_of::<WAVEHDR>() as u32;
        unsafe {
            if waveOutPrepareHeader(self.handle, &mut *header, size) != 0 {
                return;
            }
            if waveOutWrite(self.handle, &mut *header, size) != 0 {
                let _ = waveOutUnprepareHeader(self.handle, &mut *header, size);
                return;
            }
        }
        self.voices.push(Voice {
            header,
            clip: Arc::clone(clip),
        });
    }

    /// 回收已经播完的缓冲（否则排满就再也不出声了）。
    fn reap(&mut self) {
        let size = size_of::<WAVEHDR>() as u32;
        let mut index = 0;
        while index < self.voices.len() {
            if self.voices[index].header.dwFlags & WHDR_DONE != 0 {
                unsafe {
                    let _ = waveOutUnprepareHeader(
                        self.handle,
                        &mut *self.voices[index].header,
                        size,
                    );
                }
                self.voices.remove(index);
            } else {
                index += 1;
            }
        }
    }
}

/// 播放出口：正常是常开的 `waveOut`，开不出来退回 `PlaySound`。
enum Sink {
    Wave(Player),
    Legacy,
}

/// 松手提示音：直接播系统「默认提示音」。
///
/// 与 EarTrumpet 完全一致：它的 `SystemSoundsHelper` 里是
/// `System.Media.SystemSounds.Beep.Play()`，.NET 内部走的就是 `MessageBeep(MB_OK)`。
/// gpui 在 Windows 上的 `Window::play_system_bell()` 也正是这一句，
/// 所以这里直接转调，让系统按当前「声音」方案播放，不合成、不自带 wav。
///
/// 注意它和上面那套「音量音效」是两件事：这个是滑条**松手**时那一声，
/// 只跟 `release_sound` 走；开了拖拽音效以后会被跳过（见 `flyout` 里的调用点）。
pub fn tick(window: &gpui::Window) {
    window.play_system_bell();
}

/// 音量变了就调一次（变没变由调用方判：到顶 / 到底那几下不该响）。
pub fn step(config: &Config, gesture: VolumeGesture) {
    if !config.volume_sound.covers(gesture) {
        return;
    }
    let now = Instant::now();
    let throttled = LAST.with(|last| {
        let mut hit = false;
        if let Some(previous) = last.get() {
            hit = now.duration_since(previous) < MIN_GAP;
        }
        if !hit {
            last.set(Some(now));
        }
        hit
    });
    if throttled {
        return;
    }
    play(config);
}

/// 试听：设置页那个按钮。
///
/// 不看总开关、不看范围（按下去的意思就是「让我听一下」），但**要**走节流 ——
/// 连点两下试听不该叠成一团噪音。
pub fn preview(config: &Config) {
    let now = Instant::now();
    let throttled = LAST.with(|last| {
        let mut hit = false;
        if let Some(previous) = last.get() {
            hit = now.duration_since(previous) < MIN_GAP;
        }
        if !hit {
            last.set(Some(now));
        }
        hit
    });
    if throttled {
        return;
    }
    play(config);
}

/// 把这一段音频交给系统播（立刻返回）。
fn play(config: &Config) {
    let sound = &config.volume_sound;
    let gain = gain(sound.volume);
    let base = format!("{}@{:03}", sound.source_key(), (gain * 1000.0) as u32);

    SINK.with(|cell| {
        let mut slot = cell.borrow_mut();
        let sink = slot.get_or_insert_with(|| match Player::open() {
            Some(player) => Sink::Wave(player),
            // 通道开不出来（老声卡 / 独占模式）：退回 PlaySound，单发还是能响。
            None => Sink::Legacy,
        });
        match sink {
            Sink::Wave(player) => {
                let clip = clip_for(&base, sound, gain, player.rate);
                // 空 = 自定义文件读不出来，这一下就是不响（见 `build`）。
                if clip.audio.is_empty() {
                    return;
                }
                player.write(&clip);
            }
            Sink::Legacy => {
                let bytes = wav_for(&base, sound, gain);
                if bytes.is_empty() {
                    return;
                }
                // `SND_NOSTOP`：正在响的那一声别被掐，这一发直接算失败。
                unsafe {
                    PlaySoundW(
                        PCWSTR(bytes.as_ptr() as *const u16),
                        None,
                        SND_MEMORY | SND_ASYNC | SND_NODEFAULT | SND_NOSTOP,
                    );
                }
            }
        }
    });
}

/// 按缓存取一份「播放器口径」的 PCM。
fn clip_for(
    key: &str,
    sound: &crate::config::VolumeSoundConfig,
    gain: f32,
    rate: u32,
) -> Arc<Clip> {
    let cache_key = format!("{key}@{rate}");
    CLIPS.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(clip) = cache.get(&cache_key) {
            return Arc::clone(clip);
        }
        let clip = Arc::new(Clip {
            audio: to_player(&wav_for(key, sound, gain), rate),
        });
        if cache.len() > 64 {
            cache.clear();
        }
        cache.insert(cache_key, Arc::clone(&clip));
        clip
    })
}

/// 合成 / 读盘拿到一份完整 WAV（按音源 + 音量缓存）。
fn wav_for(key: &str, sound: &crate::config::VolumeSoundConfig, gain: f32) -> Arc<Vec<u8>> {
    CACHE.with(|cache| {
        let mut cache = cache.borrow_mut();
        if let Some(bytes) = cache.get(key) {
            return Arc::clone(bytes);
        }
        let bytes = Arc::new(build(sound, gain));
        // 攒太多就整个丢掉：反正都是能重算的，不用精打细算哪个该淘汰。
        if cache.values().map(|bytes| bytes.len()).sum::<usize>() > CACHE_LIMIT {
            cache.clear();
        }
        cache.insert(key.to_string(), Arc::clone(&bytes));
        bytes
    })
}

/// WAV 里我们关心的那几项。
struct WavInfo {
    tag: u16,
    channels: u16,
    bits: u16,
    rate: u32,
    /// `data` 载荷的起点与长度（已按文件实际长度钳过）。
    start: usize,
    len: usize,
}

/// 逐块扫一遍 WAV，拿到格式和 `data` 载荷。
fn scan(raw: &[u8]) -> Option<WavInfo> {
    if raw.len() < 12 || &raw[0..4] != b"RIFF" || &raw[8..12] != b"WAVE" {
        return None;
    }
    let mut info = WavInfo {
        tag: 0,
        channels: 0,
        bits: 0,
        rate: 0,
        start: 0,
        len: 0,
    };
    let mut found_data = false;
    let mut offset = 12usize;
    while offset + 8 <= raw.len() {
        let id = &raw[offset..offset + 4];
        let size = u32::from_le_bytes(raw[offset + 4..offset + 8].try_into().ok()?) as usize;
        let body = offset + 8;
        match id {
            b"fmt " if body + 16 <= raw.len() => {
                info.tag = u16::from_le_bytes(raw[body..body + 2].try_into().ok()?);
                info.channels = u16::from_le_bytes(raw[body + 2..body + 4].try_into().ok()?);
                info.rate = u32::from_le_bytes(raw[body + 4..body + 8].try_into().ok()?);
                info.bits = u16::from_le_bytes(raw[body + 14..body + 16].try_into().ok()?);
            }
            b"data" => {
                info.start = body;
                info.len = size.min(raw.len().saturating_sub(body));
                found_data = true;
            }
            _ => {}
        }
        offset = body + size + (size & 1);
    }
    (found_data && info.channels > 0 && info.rate > 0).then_some(info)
}

/// WAV → 单声道 16 位、播放器采样率的 PCM。
///
/// 认不出来的（浮点 / 压缩 / 8 位）原样返回，让声卡自己看着办 —— 总比没声音好。
fn to_player(wav: &[u8], rate: u32) -> Vec<u8> {
    let Some(info) = scan(wav) else {
        return Vec::new();
    };
    let data = &wav[info.start..info.start + info.len];
    if info.tag != WAVE_FORMAT_PCM as u16 || info.bits != 16 || info.channels == 0 {
        return data.to_vec();
    }
    let frame = info.channels as usize * 2;
    let frames = data.len() / frame;
    let mut mono: Vec<i16> = Vec::with_capacity(frames);
    for index in 0..frames {
        let base = index * frame;
        let mut sum = 0i32;
        for channel in 0..info.channels as usize {
            let at = base + channel * 2;
            sum += i16::from_le_bytes([data[at], data[at + 1]]) as i32;
        }
        mono.push((sum / info.channels as i32) as i16);
    }
    if info.rate == rate {
        return mono.iter().flat_map(|value| value.to_le_bytes()).collect();
    }
    resample(&mono, info.rate, rate)
        .iter()
        .flat_map(|value| value.to_le_bytes())
        .collect()
}

/// 线性插值重采样（音效只有几十毫秒，够用且不会引入可听的失真）。
fn resample(samples: &[i16], from: u32, to: u32) -> Vec<i16> {
    if samples.is_empty() || from == 0 || to == 0 {
        return Vec::new();
    }
    let ratio = to as f64 / from as f64;
    let frames = (samples.len() as f64 * ratio) as usize;
    let mut out = Vec::with_capacity(frames);
    for index in 0..frames {
        let at = index as f64 / ratio;
        let left = at.floor() as usize;
        let frac = at - left as f64;
        let a = samples[left.min(samples.len() - 1)] as f64;
        let b = samples[(left + 1).min(samples.len() - 1)] as f64;
        out.push((a + (b - a) * frac).round() as i16);
    }
    out
}

/// 音效音量百分比 → 线性幅度。
///
/// 1.7 次方：线性映射下 50% 听起来还是接近一半响，低档几乎听不见；这条曲线在
/// 60%（默认）大约是满幅的四成，调到 100% 才真正「响」。
fn gain(percent: u32) -> f32 {
    let value = percent.min(100) as f32 / 100.0;
    // 0 就是彻底静音（想关掉又不想动总开关的人这么用）。
    value.powf(1.7) * 0.92
}

/// 生成一份 WAV：内置音效现算，自定义文件读盘 + 缩放。
///
/// 挑过自定义文件但读不出来（被删 / 被挪走 / 不是 WAV）时返回**空**，
/// 也就是这一下不响 —— 用户选的是自己那一声，拿「轻点」顶包只会让人以为
/// 「设置没生效」；设置页那边同时会标红提示重新选。
fn build(sound: &crate::config::VolumeSoundConfig, gain: f32) -> Vec<u8> {
    if sound.uses_custom_file() {
        return std::fs::read(sound.custom_path())
            .ok()
            .and_then(|raw| scale_wav(&raw, gain))
            .unwrap_or_default();
    }
    encode(&synth(sound.style), gain)
}

// ---------------------------------------------------------------------------
// 内置音效
// ---------------------------------------------------------------------------

/// 确定性噪声：同一个音效每次合成的字节完全一样（测试能锁住，缓存也稳）。
struct Noise(u32);

impl Noise {
    fn new(seed: u32) -> Self {
        Self(seed | 1)
    }

    fn next(&mut self) -> f32 {
        // xorshift32：够随机，也不用引 rand。
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 17;
        self.0 ^= self.0 << 5;
        (self.0 as f32 / u32::MAX as f32) * 2.0 - 1.0
    }
}

const TAU: f32 = std::f32::consts::TAU;

/// 合成一段音效，峰值归一化到 1.0（真正的响度由 [`gain`] 决定）。
fn synth(style: VolumeSoundStyle) -> Vec<f32> {
    let rate = RATE as f32;
    let seconds = match style {
        VolumeSoundStyle::Click => 0.034,
        VolumeSoundStyle::Tick => 0.045,
        VolumeSoundStyle::Pop => 0.070,
        VolumeSoundStyle::Wood => 0.080,
        VolumeSoundStyle::Ratchet => 0.032,
        VolumeSoundStyle::Blip => 0.060,
        // 「铃音」原来有 300ms，快拖时缓冲排到两声之后，手停了还在「叮」。
        // 收到 150ms：还是那声「叮」，但连着响不会堆起来。
        VolumeSoundStyle::Chime => 0.15,
        // 自定义没文件时按「轻点」算（和 `source_key` 里那条兜底一致）。
        VolumeSoundStyle::Custom => 0.034,
    };
    let mut noise = Noise::new(0x2545_F491);
    let mut phase = 0.0f32;
    // 预加重要上一拍的噪声值（只有棘轮用）。
    let mut previous_noise = 0.0f32;
    // 配平系数见 `normalize`。
    let trim = trim_of(style);
    let mut samples = Vec::with_capacity((seconds * rate) as usize);

    for index in 0..(seconds * rate) as usize {
        let t = index as f32 / rate;
        let value = match style {
            // 轻点：一股极短的噪声 + 一个 2.4kHz 的芯，像机械按键。
            VolumeSoundStyle::Click | VolumeSoundStyle::Custom => {
                let body = (TAU * 2_400.0 * t).sin() * (-t / 0.0045).exp() * 0.35;
                let tick = noise.next() * (-t / 0.0025).exp() * 0.55;
                attack(t, 0.0002) * (body + tick)
            }
            // 滴答：清亮的一声「滴」，比轻点长一倍、带点二次谐波。
            VolumeSoundStyle::Tick => {
                let base = (TAU * 1_500.0 * t).sin();
                let second = (TAU * 3_000.0 * t).sin() * 0.18;
                attack(t, 0.0005) * (base + second) * (-t / 0.008).exp() * 0.5
            }
            // 气泡：350 → 900Hz 上滑，「啵」的那一下。
            VolumeSoundStyle::Pop => {
                let freq = 350.0 + 550.0 * (t / 0.040).min(1.0);
                phase += TAU * freq / rate;
                attack(t, 0.002) * phase.sin() * (-t / 0.018).exp() * 0.6
            }
            // 木鱼：320Hz 的基音 + 一点 470Hz 的木味 + 起始那记噪声。
            VolumeSoundStyle::Wood => {
                let body = (TAU * 320.0 * t).sin() * (-t / 0.020).exp() * 0.5;
                let ring = (TAU * 470.0 * t).sin() * (-t / 0.013).exp() * 0.22;
                let knock = noise.next() * (-t / 0.002).exp() * 0.35;
                attack(t, 0.0003) * (body + ring + knock)
            }
            // 棘轮：挨着 B 站那两段棘轮扳手的录音调的 —— 把录下来的音频拆开量过，
            // 最脆的那几下频谱质心 ≈ 2.5kHz，能量铺在 500Hz~5kHz，8k/12k 还留一点毛刺。
            // 所以这一版是**宽带噪声打底 + 三个靠得近的金属模（2k / 3.4k / 5.2k）**，
            // 低频只留一丝实体：早先那版 820Hz 一个峰就占了近一半能量，
            // 听感是「咚」而不是「咔」。噪声还做了一阶预加重，白噪声直接上是「嘶」。
            VolumeSoundStyle::Ratchet => {
                let raw = noise.next();
                let previous = raw - 0.70 * previous_noise;
                previous_noise = raw;
                let hit = previous * (-t / 0.0016).exp() * 0.45;
                let tail = raw * (-t / 0.0045).exp() * 0.32;
                let ring = (TAU * 2_450.0 * t).sin() * (-t / 0.0030).exp() * 0.220
                    + (TAU * 3_400.0 * t).sin() * (-t / 0.0024).exp() * 0.360
                    + (TAU * 5_200.0 * t).sin() * (-t / 0.0015).exp() * 0.240;
                let body = (TAU * 650.0 * t).sin() * (-t / 0.0020).exp() * 0.12;
                // 7.5ms 后棘爪回弹一下更轻、更高的「嗒」。
                let snap = if t > 0.0075 {
                    let back = t - 0.0075;
                    (TAU * 4_600.0 * back).sin() * (-back / 0.0011).exp() * 0.22
                        + noise.next() * (-back / 0.0008).exp() * 0.26
                } else {
                    0.0
                };
                attack(t, 0.0002) * (hit + tail + ring + body + snap)
            }
            // 电子：880Hz 过 tanh 近似方波，尾巴收得快。
            VolumeSoundStyle::Blip => {
                let square = ((TAU * 880.0 * t).sin() * 3.0).tanh();
                attack(t, 0.001) * square * (-t / 0.020).exp() * 0.4
            }
            // 铃音：三个泛音叠一起，收得比早先快一倍（连着响才不拖）。
            VolumeSoundStyle::Chime => {
                let root = (TAU * 1_046.5 * t).sin() * (-t / 0.048).exp() * 0.35;
                let third = (TAU * 1_568.0 * t).sin() * (-t / 0.034).exp() * 0.18;
                let fifth = (TAU * 2_093.0 * t).sin() * (-t / 0.022).exp() * 0.10;
                attack(t, 0.003) * (root + third + fifth)
            }
        };
        samples.push(value);
    }

    normalize(&samples, trim)
}

/// 每一档的峰值（也是最终写进样本的幅度）。
///
/// 这组数不是拍出来的，是拿端点电平表**逐档量**出来的：七档都调到 60%，
/// 谁读数高就把谁的峰值压下去，直到七个都落在 0.50 上下。
/// 所以它更像是「响度配平」而不是「音量」——峰值高的都是波形稀疏、听感偏轻的，
/// 峰值低的（电子那种方波味、木鱼那种低频）反而更压耳朵。
/// 「自定义」跟「轻点」共用一档：还没挑文件时本来就是它兜底。
fn trim_of(style: VolumeSoundStyle) -> f32 {
    match style {
        VolumeSoundStyle::Click | VolumeSoundStyle::Custom => 0.50,
        VolumeSoundStyle::Tick => 0.48,
        VolumeSoundStyle::Pop => 0.50,
        VolumeSoundStyle::Wood => 0.93,
        VolumeSoundStyle::Ratchet => 0.56,
        VolumeSoundStyle::Blip => 0.33,
        VolumeSoundStyle::Chime => 0.50,
    }
}

/// 起始淡入：不留淡入会有直流跳变，一声「啪」比音效本身还响。
fn attack(t: f32, seconds: f32) -> f32 {
    (t / seconds).min(1.0)
}

/// 峰值归一化到 1.0，再乘这一档自己的「配平」系数。
///
/// 七个音效的波形差别很大：方波味的 `Blip` 能量集中、听感最响，`Click` 只有
/// 二十来毫秒、峰值又只有一个尖，照 1.0 铺平的话切到「轻点」会像没响。
/// 这组系数是拿端点电平表逐档量出来配的（同样 60% 音量下读数落在 0.45 上下），
/// 换音效时响度不会突然跳一大截。超过 1 的在最后一步钳住 —— 敲击类音效削顶
/// 一点反而更像真的。
fn normalize(samples: &[f32], trim: f32) -> Vec<f32> {
    let peak = samples
        .iter()
        .fold(0.0f32, |peak, value| peak.max(value.abs()));
    if peak <= f32::EPSILON {
        return samples.to_vec();
    }
    let mut out: Vec<f32> = samples
        .iter()
        .map(|value| (value / peak * trim).clamp(-1.0, 1.0))
        .collect();
    // 收尾 5ms 淡出：波形在末尾还剩一点幅度时被硬切会「咔」一下，
    // 铃音这种截短过的尤其明显。这条淡出对所有音效都一样，听不出来。
    let fade = (RATE as f32 * 0.005) as usize;
    let len = out.len();
    if len > fade * 2 {
        for (index, value) in out[len - fade..].iter_mut().enumerate() {
            *value *= 1.0 - (index as f32 + 1.0) / fade as f32;
        }
    }
    out
}

/// 采样 → 16 位单声道 WAV 字节。
fn encode(samples: &[f32], gain: f32) -> Vec<u8> {
    let data_len = samples.len() * 2;
    let mut out = Vec::with_capacity(44 + data_len);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&((36 + data_len) as u32).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&1u16.to_le_bytes()); // PCM
    out.extend_from_slice(&1u16.to_le_bytes()); // 单声道
    out.extend_from_slice(&RATE.to_le_bytes());
    out.extend_from_slice(&(RATE * 2).to_le_bytes()); // 每秒字节数
    out.extend_from_slice(&2u16.to_le_bytes()); // 块对齐
    out.extend_from_slice(&16u16.to_le_bytes()); // 位深
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data_len as u32).to_le_bytes());
    for sample in samples {
        let value = (sample * gain * i16::MAX as f32).clamp(-32_768.0, 32_767.0);
        out.extend_from_slice(&(value as i16).to_le_bytes());
    }
    out
}

// ---------------------------------------------------------------------------
// 自定义文件
// ---------------------------------------------------------------------------

/// 按音效音量缩放一份自定义 WAV。
///
/// 只动 16 位 PCM —— 这是「音效文件」的绝大多数；别的格式（8 / 24 / 32 位、
/// 浮点、压缩）原样返回：`PlaySound` 自己认得，放得出声，只是音量不可调。
fn scale_wav(raw: &[u8], gain: f32) -> Option<Vec<u8>> {
    if raw.len() < 12 || &raw[0..4] != b"RIFF" || &raw[8..12] != b"WAVE" {
        return None;
    }
    // 逐块走：`fmt ` 拿格式，`data` 拿样本，其余块原样跳过。
    let mut format: Option<(u16, u16, u16, u32)> = None; // (tag, channels, bits, rate)
    let mut data: Option<(usize, usize)> = None; // (载荷起点, 载荷长度)
    let mut offset = 12usize;
    while offset + 8 <= raw.len() {
        let id = &raw[offset..offset + 4];
        let size = u32::from_le_bytes(raw[offset + 4..offset + 8].try_into().ok()?) as usize;
        let body = offset + 8;
        match id {
            b"fmt " if body + 16 <= raw.len() => {
                let tag = u16::from_le_bytes(raw[body..body + 2].try_into().ok()?);
                let channels = u16::from_le_bytes(raw[body + 2..body + 4].try_into().ok()?);
                let rate = u32::from_le_bytes(raw[body + 4..body + 8].try_into().ok()?);
                let bits = u16::from_le_bytes(raw[body + 14..body + 16].try_into().ok()?);
                format = Some((tag, channels, bits, rate));
            }
            b"data" => {
                let len = size.min(raw.len().saturating_sub(body));
                data = Some((body, len));
            }
            _ => {}
        }
        offset = body + size + (size & 1);
    }

    let (data_start, data_len) = data?;
    let (tag, channels, bits, rate) = format?;
    if tag != 1 || bits != 16 || channels == 0 || rate == 0 {
        // 缩放不了，原样交出去（还能听到声音，音量就交给设备）。
        return Some(raw.to_vec());
    }

    // 截到最长时长（按帧算，别把某一帧劈成两半）。
    let frame = channels as usize * 2;
    let cap = (MAX_SECONDS * rate as f32) as usize * frame;
    let usable = (data_len / frame * frame).min(cap).min(raw.len() - data_start);

    let mut out = raw[..data_start].to_vec();
    for chunk in raw[data_start..data_start + usable].chunks_exact(2) {
        let sample = i16::from_le_bytes([chunk[0], chunk[1]]);
        let value = (sample as f32 * gain).clamp(-32_768.0, 32_767.0) as i16;
        out.extend_from_slice(&value.to_le_bytes());
    }
    // 两个长度字段跟着改：`data` 块的长度在载荷前面 4 字节，RIFF 总长在文件头。
    let data_size_at = data_start - 4;
    out[data_size_at..data_size_at + 4].copy_from_slice(&(usable as u32).to_le_bytes());
    let riff_size = (out.len() - 8) as u32;
    out[4..8].copy_from_slice(&riff_size.to_le_bytes());
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::VolumeSoundConfig;

    fn peaks(bytes: &[u8]) -> i16 {
        bytes[44..]
            .chunks_exact(2)
            .map(|chunk| i16::from_le_bytes([chunk[0], chunk[1]]))
            .fold(0i16, |peak, value| peak.max(value.abs()))
    }

    /// 七个内置音效都得是一段像样的 WAV：头对、有声音、峰值按增益缩放。
    #[test]
    fn builtin_sounds_are_valid_wav() {
        for style in VolumeSoundStyle::ALL {
            if style == VolumeSoundStyle::Custom {
                continue;
            }
            let mut sound = VolumeSoundConfig {
                enabled: true,
                style,
                ..Default::default()
            };
            sound.volume = 100;
            let bytes = build(&sound, 1.0);
            assert_eq!(&bytes[0..4], b"RIFF", "{style:?}");
            assert_eq!(&bytes[8..12], b"WAVE", "{style:?}");
            // 44 字节头 + 单声道 16 位。
            let data_len = u32::from_le_bytes(bytes[40..44].try_into().unwrap()) as usize;
            assert_eq!(bytes.len(), 44 + data_len, "{style:?}");
            assert!(data_len > 1_000, "{style:?} 太短了（{data_len} 字节）");
            // 峰值就是配平系数（满音量、1.0 增益时）：七个音效靠这组系数
            // 在端点电平表上读到同一个数，换档不会突然响一大截或像没响。
            let peak = peaks(&bytes) as f32 / i16::MAX as f32;
            let want = trim_of(style);
            assert!(
                (peak - want).abs() < 0.02,
                "{style:?} 峰值 {peak:.3}，应约等于配平系数 {want:.2}"
            );
        }
    }

    /// 交给 `waveOut` 的那份 PCM：44 字节头剥掉、采样率对得上设备，
    /// 设备是 48k 就重采样到 48k（否则会放成快放 / 慢放）。
    #[test]
    fn player_clip_is_mono_at_the_device_rate() {
        let wav = encode(&synth(VolumeSoundStyle::Tick), 1.0);
        let same = to_player(&wav, RATE);
        assert_eq!(same.len(), wav.len() - 44);
        let up = to_player(&wav, 48_000);
        let frames = (same.len() / 2) as f64 * 48_000.0 / RATE as f64;
        assert!(
            (up.len() as f64 - frames * 2.0).abs() <= 4.0,
            "重采样后长度 {} 与预期 {} 差太多",
            up.len(),
            frames * 2.0
        );
        // 认不出来的东西别 panic，交出去让声卡自己看。
        assert!(to_player(b"nope", RATE).is_empty());
    }

    /// 音量越低样本越小：0 静音、100 满幅、60 在中间。
    #[test]
    fn volume_scales_the_samples() {
        let sound = VolumeSoundConfig::default();
        let silent = peaks(&encode(&synth(sound.style), gain(0)));
        let half = peaks(&encode(&synth(sound.style), gain(60)));
        let full = peaks(&encode(&synth(sound.style), gain(100)));
        assert_eq!(silent, 0);
        assert!(half > 0 && half < full, "{half} 应落在 0 和 {full} 之间");
    }

    /// 自定义文件按增益缩放，两个长度字段也跟着改。
    #[test]
    fn custom_wav_is_scaled_and_resized() {
        let raw = encode(&synth(VolumeSoundStyle::Click), 1.0);
        let scaled = scale_wav(&raw, 0.5).expect("自己编的 WAV 应该认得出");
        assert_eq!(scaled.len(), raw.len());
        let before = peaks(&raw);
        let after = peaks(&scaled);
        assert!(after < before / 2 + 200 && after > before / 2 - 200, "{before} → {after}");
        // RIFF 总长 = 文件长度 - 8，data 长度 = 后面剩下的字节。
        let riff = u32::from_le_bytes(scaled[4..8].try_into().unwrap()) as usize;
        assert_eq!(riff, scaled.len() - 8);
        let data = u32::from_le_bytes(scaled[40..44].try_into().unwrap()) as usize;
        assert_eq!(data, scaled.len() - 44);
    }

    /// 不是 WAV 的东西要老实说「认不出」，好让调用方退回内置音效。
    #[test]
    fn junk_is_rejected() {
        assert!(scale_wav(b"not a wave file at all", 1.0).is_none());
        assert!(scale_wav(&[], 1.0).is_none());
    }

    /// 自定义文件丢了：标成「找不到」，而且**不出声**（不能拿「轻点」顶包）。
    #[test]
    fn missing_custom_file_falls_silent() {
        let sound = VolumeSoundConfig {
            enabled: true,
            style: VolumeSoundStyle::Custom,
            custom: r"Z:\definitely\not\here\gone.wav".to_string(),
            ..Default::default()
        };
        assert!(sound.uses_custom_file(), "填了路径，就该算「用自定义」");
        assert!(sound.custom_missing(), "文件不存在要能判出来");
        assert!(
            build(&sound, 1.0).is_empty(),
            "文件读不出来时这一下应该是静音，而不是换成别的音效"
        );
        // 没填路径不算「丢了」：那是还没选，界面走「先用轻点」那套。
        let empty = VolumeSoundConfig {
            style: VolumeSoundStyle::Custom,
            ..Default::default()
        };
        assert!(!empty.custom_missing());
        assert!(!build(&empty, 1.0).is_empty());
    }

    /// 铃音收短了：连着响时排队不会越积越长（早先 300ms，快拖会拖后两声）。
    #[test]
    fn chime_is_short_enough_to_keep_up() {
        let ms = synth(VolumeSoundStyle::Chime).len() as f32 / RATE as f32 * 1000.0;
        assert!(ms <= 200.0, "铃音 {ms:.0}ms 还是太长，连发会拖后");
        // 棘轮是「一下」，必须比铃音更短促。
        let ratchet = synth(VolumeSoundStyle::Ratchet).len() as f32 / RATE as f32 * 1000.0;
        assert!(ratchet <= 60.0, "棘轮 {ratchet:.0}ms 太长了");
    }

    /// 「脆」是能量的：把前 10ms 拆成几个频点算频谱质心。
    ///
    /// 参考是 B 站那两段棘轮扳手（把音轨扒下来逐下量过：能量铺在 500Hz~6kHz，
    /// 3k 一带最集中，低频只占一成）。这一版是宽带噪声 + 2.45k/3.4k/5.2k 三个
    /// 金属模，质心必须留在 2.8kHz 以上 —— 掉下去就是又变回「咚」了。
    #[test]
    fn ratchet_stays_crisp() {
        let samples = synth(VolumeSoundStyle::Ratchet);
        // 想看波形就设 BOLUME_DUMP_SOUND=1，会写一份 wav 到 target/ 下。
        if std::env::var_os("BOLUME_DUMP_SOUND").is_some() {
            let _ = std::fs::write("target/ratchet_new.wav", encode(&samples, 1.0));
        }
        let centroid = centroid(&samples);
        println!("棘轮质心 {centroid:.0}Hz");
        assert!(centroid > 2_800.0, "棘轮质心只有 {centroid:.0}Hz，听着会发闷");
    }

    /// 1/3 倍频程能量加权出来的频谱质心（前 23ms）。
    ///
    /// 单点 DFT 在噪声上抖得厉害（同样是「脆」，读数能差 1000Hz），
    /// 按倍频程把一片格子加起来才稳 —— 参考录音也用同一把尺子量，
    /// 量出来是 3300Hz 上下。
    fn centroid(samples: &[f32]) -> f32 {
        const BANDS: [f32; 21] = [
            125.0, 160.0, 200.0, 250.0, 315.0, 400.0, 500.0, 630.0, 800.0, 1_000.0, 1_250.0,
            1_600.0, 2_000.0, 2_500.0, 3_150.0, 4_000.0, 5_000.0, 6_300.0, 8_000.0, 10_000.0,
            12_500.0,
        ];
        let take = ((RATE as f32 * 0.023) as usize).min(samples.len());
        let window = &samples[..take];
        let mut total = 0.0;
        let mut weighted = 0.0;
        for center in BANDS {
            let (low, high) = (center / 1.122, center * 1.122);
            // 每一格 43Hz（1024 点窗），落在这一档里的都加起来。
            let mut magnitude = 0.0;
            let mut bin = ((low * take as f32 / RATE as f32).floor() as usize).max(1);
            let stop = ((high * take as f32 / RATE as f32).ceil() as usize).min(take / 2);
            let mut count = 0;
            while bin <= stop {
                let omega = TAU * bin as f32 / take as f32;
                let (mut re, mut im) = (0.0f32, 0.0f32);
                for (index, value) in window.iter().enumerate() {
                    let angle = omega * index as f32;
                    re += value * angle.cos();
                    im -= value * angle.sin();
                }
                magnitude += (re * re + im * im).sqrt() / take as f32;
                count += 1;
                bin += 1;
            }
            // 取每格平均：高端一格里挤的 bin 多，直接求和会把白噪声读成「高频很亮」。
            magnitude /= count.max(1) as f32;
            total += magnitude;
            weighted += center * magnitude;
        }
        if total > 0.0 {
            weighted / total
        } else {
            0.0
        }
    }

    /// 三种手势各自看自己的勾，总开关关掉一律不响。
    #[test]
    fn scope_follows_the_ticks() {
        let mut sound = VolumeSoundConfig {
            enabled: true,
            on_hotkey: true,
            on_wheel: false,
            on_drag: false,
            ..Default::default()
        };
        assert!(sound.covers(VolumeGesture::Hotkey));
        assert!(sound.covers(VolumeGesture::AppHotkey));
        assert!(!sound.covers(VolumeGesture::TrayScroll));
        assert!(!sound.covers(VolumeGesture::AppHoverScroll));
        assert!(!sound.covers(VolumeGesture::Drag));
        sound.on_drag = true;
        assert!(sound.covers(VolumeGesture::AppDrag));
        sound.enabled = false;
        assert!(!sound.covers(VolumeGesture::Hotkey));
    }
}
