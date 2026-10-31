//! 显示器亮度：DDC/CI 读写，独立线程持有物理显示器句柄。
//!
//! 协议细节照 `E:\Project\AGENT\CODE\ddc-control-main`（ddc 0.2.2 + ddc-winapi 0.2.2）：
//! 亮度就是 MCCS 的 VCP 码 **0x10**，读用 `get_vcp_feature`、写用 `set_vcp_feature`。
//! 对比度是 0x12，这里先把常量留好（设置里将来要加，只需多一条命令）。
//!
//! 为什么不放在 UI 线程：一次 DDC/CI 往返通常 10~40ms，个别显示器要几百毫秒，
//! 直接调会把面板卡住。所以句柄归工作线程所有，界面只收发纯数据 ——
//! 和音频线程（`audio::AudioEngine`）是同一套做法。

use std::{
    rc::Rc,
    sync::{
        Mutex,
        mpsc::{Receiver, RecvTimeoutError, Sender, channel},
    },
    time::{Duration, Instant},
};

use crate::platform::backlight::Backlight;

use ddc::Ddc;
use ddc_winapi::Monitor;
use windows::Win32::Foundation::{LPARAM, POINT, RECT};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory1, DXGI_OUTPUT_DESC1, IDXGIFactory1, IDXGIOutput, IDXGIOutput6,
};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, GetMonitorInfoW, HDC, HMONITOR, MONITOR_DEFAULTTONEAREST, MONITORINFO,
    MONITORINFOEXW, MonitorFromPoint,
};
use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
use windows::core::{BOOL, Interface as _};

/// 上一次扫出来的每条亮度通道属于哪块屏（`HMONITOR`），下标就是 `displays` 的序号。
///
/// 光标在哪台、提示条拖的是哪条通道都靠它对号。不能用 `EnumDisplayMonitors` 的
/// 下标：一块屏给不出物理显示器时（笔记本合盖后的内置屏）两边台数会错开。
static ENTRY_SCREENS: Mutex<Vec<isize>> = Mutex::new(Vec::new());

/// 上一次扫描时系统报的全部屏幕句柄。插拔、开合盖子后按它触发整轮重扫。
static KNOWN_SCREENS: Mutex<Vec<isize>> = Mutex::new(Vec::new());

unsafe extern "system" fn collect_handles(
    monitor: HMONITOR,
    _dc: HDC,
    _rect: *mut RECT,
    data: LPARAM,
) -> BOOL {
    let list = unsafe { &mut *(data.0 as *mut Vec<HMONITOR>) };
    list.push(monitor);
    BOOL(1)
}

/// 按 `EnumDisplayMonitors` 的顺序列出所有屏幕句柄。
fn screen_handles() -> Vec<HMONITOR> {
    let mut handles: Vec<HMONITOR> = Vec::new();
    let _ = unsafe {
        EnumDisplayMonitors(
            None,
            None,
            Some(collect_handles),
            LPARAM(&mut handles as *mut Vec<HMONITOR> as isize),
        )
    };
    handles
}

/// 鼠标所在屏幕的亮度通道序号（0 起）。
///
/// 序号以 `scan()` 那次扫出来的 `ENTRY_SCREENS` 为准（**不**是
/// `EnumDisplayMonitors` 的下标）：一块屏给不出物理显示器时（笔记本合盖后的
/// 内置屏就这样）两边台数会错开，按系统下标数就会落到隔壁屏的通道上。
pub fn monitor_index_at_cursor() -> usize {
    let mut point = POINT::default();
    let target = unsafe {
        if GetCursorPos(&mut point).is_err() {
            return 0;
        }
        MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST)
    };
    index_for_handle(target.0 as isize)
}

/// 这块屏（`HMONITOR` 句柄）的亮度通道序号（0 起）。
///
/// 句柄来自 `platform::monitors`，和 `scan()` 收的那套是同一批。认不出来就落到
/// 第一台：拖了总得调点什么，整条没反应更糟。还没扫过（面板一次都没开）时退回
/// 按枚举顺序数一遍，好歹给个号。
pub fn index_for_handle(handle: isize) -> usize {
    let known = ENTRY_SCREENS
        .lock()
        .map(|list| list.clone())
        .unwrap_or_default();
    if known.is_empty() {
        return screen_handles()
            .iter()
            .position(|candidate| candidate.0 as isize == handle)
            .unwrap_or(0);
    }
    known.iter().position(|known| *known == handle).unwrap_or(0)
}

/// MCCS 的亮度 VCP 码（参考实现用的就是它）。
pub const VCP_BRIGHTNESS: u8 = 0x10;
/// MCCS 的对比度 VCP 码：现在就留好，将来加对比度只需多一条命令。
pub const VCP_CONTRAST: u8 = 0x12;

/// 这块屏（`HMONITOR`）的 GDI 设备名，形如 `\\.\DISPLAY1`。
fn gdi_device_name(handle: HMONITOR) -> Option<String> {
    let mut info = MONITORINFOEXW::default();
    info.monitorInfo.cbSize = std::mem::size_of::<MONITORINFOEXW>() as u32;
    // `MONITORINFOEXW` 的头一段就是 `MONITORINFO`，所以这里能直接转过去。
    let ok = unsafe { GetMonitorInfoW(handle, &mut info as *mut MONITORINFOEXW as *mut MONITORINFO) };
    if !ok.as_bool() {
        return None;
    }
    let len = info
        .szDevice
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(info.szDevice.len());
    Some(String::from_utf16_lossy(&info.szDevice[..len]))
}

/// 正在输出 HDR 信号的屏，返回它们的 GDI 设备名（形如 `\\\\.\\DISPLAY1`）。
///
/// HDR 打开之后系统的色调映射会把背光那点变化吃掉，DDC/CI 的亮度照写不误，
/// 画面上却看不出变化。与其让人拖着条怀疑坏没坏，不如照实标一句「开不了」。
///
/// 判定看 DXGI 报的**当前色彩空间**：HDR 那几档都是 PQ（`G2084`）/ HLG / 线性
/// （`G10`）传输曲线。这里不用 `DISPLAYCONFIG_GET_ADVANCED_COLOR_INFO` ——
/// 实测那一位在 Win11 上常年是「开」：系统给普通 SDR 屏也开着高级色彩（自动色彩
/// 管理），开 / 关 HDR 它都不变，拿它判断会一直标成「开着 HDR」。
fn hdr_screens() -> Vec<String> {
    let mut names = Vec::new();
    let Ok(factory) = (unsafe { CreateDXGIFactory1::<IDXGIFactory1>() }) else {
        return names;
    };
    let mut adapter_index = 0;
    loop {
        let Ok(adapter) = (unsafe { factory.EnumAdapters1(adapter_index) }) else {
            break;
        };
        adapter_index += 1;
        let mut output_index = 0;
        loop {
            let Ok(output) = (unsafe { adapter.EnumOutputs(output_index) }) else {
                // 枚举到头（`DXGI_ERROR_NOT_FOUND`）或者这块适配器不肯给，都往下一位。
                break;
            };
            output_index += 1;
            if let Some(name) = hdr_output_name(&output) {
                names.push(name);
            }
        }
    }
    names
}

/// 这台输出开着 HDR 就返回它的 GDI 设备名。
fn hdr_output_name(output: &IDXGIOutput) -> Option<String> {
    let six: IDXGIOutput6 = output.cast().ok()?;
    let desc: DXGI_OUTPUT_DESC1 = unsafe { six.GetDesc1() }.ok()?;
    if !desc.AttachedToDesktop.as_bool() || !is_hdr_color_space(desc.ColorSpace) {
        return None;
    }
    let len = desc
        .DeviceName
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(desc.DeviceName.len());
    Some(String::from_utf16_lossy(&desc.DeviceName[..len]))
}

/// 这个色彩空间是不是在走 HDR 那条曲线（PQ / HLG / 线性 scRGB）。
fn is_hdr_color_space(space: windows::Win32::Graphics::Dxgi::Common::DXGI_COLOR_SPACE_TYPE) -> bool {
    use windows::Win32::Graphics::Dxgi::Common::DXGI_COLOR_SPACE_TYPE;
    // HDR 只有这几档：PQ（2084）的四种封装、HLG 的两种、以及 scRGB 线性。
    // 广色域的 SDR（P2020 + gamma 2.2 / 2.4）不算 —— 那种画面亮度照旧跟着背光走。
    matches!(
        space,
        DXGI_COLOR_SPACE_TYPE(1)
            | DXGI_COLOR_SPACE_TYPE(12)
            | DXGI_COLOR_SPACE_TYPE(13)
            | DXGI_COLOR_SPACE_TYPE(14)
            | DXGI_COLOR_SPACE_TYPE(16)
            | DXGI_COLOR_SPACE_TYPE(18)
            | DXGI_COLOR_SPACE_TYPE(19)
    )
}

/// 这块屏开没开 HDR（`screens` 是 `hdr_screens()` 那批 GDI 设备名）。
///
/// 按**设备名**对号，不按下标：内置屏合盖后仍可能留在枚举里但拿不到物理显示器，
/// 按下标对会张冠李戴。
fn screen_is_hdr(handle: isize, screens: &[String]) -> bool {
    gdi_device_name(hmonitor(handle))
        .is_some_and(|name| screens.iter().any(|screen| screen.eq_ignore_ascii_case(&name)))
}

/// `isize` 形式的句柄转回 `HMONITOR`（两边存的是同一个值）。
fn hmonitor(handle: isize) -> HMONITOR {
    HMONITOR(handle as *mut std::ffi::c_void)
}

/// HDR 开关或者屏幕台数变了没有：变了就得整轮重扫。
///
/// 空列表（还没扫过、或者亮度面板压根没开）直接算没变 —— 不能因为「没扫过」
/// 就替人扫一轮，那会白白摸一遍 DDC/CI。
fn hdr_changed(entries: &[Entry]) -> bool {
    if entries.is_empty() {
        return false;
    }
    let current: Vec<isize> = screen_handles()
        .iter()
        .map(|handle| handle.0 as isize)
        .collect();
    let known = KNOWN_SCREENS
        .lock()
        .map(|list| list.clone())
        .unwrap_or_default();
    // 插了一台、拔了一台、合上盖子：都算变了。比的是**集合**，
    // 合盖后拿不到物理显示器的那块屏一直在两边，不会每两秒白扫一轮。
    if current.len() != known.len() || current.iter().any(|handle| !known.contains(handle)) {
        return true;
    }
    let screens = hdr_screens();
    entries
        .iter()
        .any(|entry| entry.hdr != screen_is_hdr(entry.handle, &screens))
}

/// 界面发往工作线程的命令。
#[derive(Clone, Debug)]
pub enum DisplayCommand {
    /// 重新枚举显示器并读一遍亮度（面板弹出时调一次：显示器自己的按钮也可能改过）。
    Refresh,
    /// 把某台显示器的亮度设成百分比（0..=100）。
    SetBrightness {
        key: String,
        value: u32,
    },
    Shutdown,
}

/// 一台显示器此刻的状态。
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DisplayInfo {
    /// 稳定标识：`序号:描述`。序号来自枚举顺序，描述是 dxva2 给的物理显示器名。
    pub key: String,
    pub name: String,
    /// 亮度百分比（0..=100）。`None` = 这台显示器的 DDC/CI 不通（笔记本内屏、
    /// 便宜转接线、或者显示器关着 DDC/CI）。
    pub brightness: Option<u32>,
    /// 这台开着 HDR。开着的时候 DDC/CI 的亮度照写不误，可画面被系统的色调映射
    /// 吃掉了 —— 界面上一律标出来，并且不给调。
    pub hdr: bool,
}

impl DisplayInfo {
    /// 能不能调亮度：DDC/CI 读得通、并且没开 HDR。
    ///
    /// 界面上凡是「拖得动 / 点得动 / 套得到」的地方都按它判，别再单独看
    /// `brightness`，否则 HDR 那台会照旧给出一个拖了没反应的条。
    pub fn adjustable(&self) -> bool {
        self.brightness.is_some() && !self.hdr
    }
}

/// 工作线程发回界面的事件。
#[derive(Clone, Debug)]
pub enum DisplayEvent {
    Monitors(Vec<DisplayInfo>),
    /// 枚举都失败了（没有物理显示器句柄），把原因带给界面显示。
    Error(String),
}

pub struct DisplayEngine {
    commands: Sender<DisplayCommand>,
    events: async_channel::Receiver<DisplayEvent>,
}

impl DisplayEngine {
    pub fn spawn() -> Self {
        let (command_tx, command_rx) = channel();
        let (event_tx, event_rx) = async_channel::unbounded();
        std::thread::Builder::new()
            .name("bolume-display".into())
            .spawn(move || run_worker(command_rx, event_tx))
            .expect("无法启动显示器线程");

        Self {
            commands: command_tx,
            events: event_rx,
        }
    }

    pub fn send(&self, command: DisplayCommand) {
        let _ = self.commands.send(command);
    }

    pub fn events(&self) -> async_channel::Receiver<DisplayEvent> {
        self.events.clone()
    }
}

impl Drop for DisplayEngine {
    fn drop(&mut self) {
        let _ = self.commands.send(DisplayCommand::Shutdown);
    }
}

/// 亮度通道：外接屏走 DDC/CI，笔记本内置屏走 `\\.\LCD`。
enum Channel {
    /// 外接屏：DDC/CI（MCCS 的 VCP 0x10）。
    Ddc(Monitor),
    /// 笔记本内置屏：显卡的背光接口，和系统亮度滑块是同一条路。
    Backlight(Rc<Backlight>),
}

/// 工作线程里的一台显示器：通道 + 它的亮度上限。
struct Entry {
    channel: Channel,
    key: String,
    name: String,
    /// 这台显示器自己报的上限（dxva2 的 `maximum`）。取不到时按 100。
    max: u32,
    /// 最近一次读到的原始值（不是百分比）。
    raw: u32,
    /// 这一轮到底读没读到。**只代表「刚才那几次读」，不是终身判决**：
    /// DDC/CI 是会闪断的（I2C 上一条命令撞车就报 0xC0262582/83/89，
    /// 显示器打着盹、或者刚开机显卡还没准备好也一样），读不到要接着重试，
    /// 不能把「这一次没读到」当成「这台显示器不支持」。
    readable: bool,
    /// 连续读空了几轮：决定下一次补读隔多久（指数退避）。
    misses: u32,
    /// 这块屏开着 HDR（枚举时问一次；窗口重新枚举会再问）。
    hdr: bool,
    /// 这条通道属于哪块屏（`HMONITOR`）。名字、HDR、光标在哪台屏上都按它认，
    /// 不按序号 —— 序号会因为某块屏给不出物理显示器而错位。
    handle: isize,
}

impl Entry {
    fn info(&self) -> DisplayInfo {
        DisplayInfo {
            key: self.key.clone(),
            name: self.name.clone(),
            brightness: self.readable.then(|| percent_of(self.raw, self.max)),
            hdr: self.hdr,
        }
    }

    /// 读一次，把结果落回自己身上。
    fn reread(&mut self) -> bool {
        let read = match &mut self.channel {
            Channel::Ddc(monitor) => read_stable(monitor),
            Channel::Backlight(backlight) => backlight
                .read()
                .map(|index| (index, backlight.max_index())),
        };
        match read {
            Some((value, maximum)) => {
                // `maximum` 没报（0）或者报成 1 时按 0..100 处理。
                self.max = scale_max(u32::from(maximum));
                self.raw = u32::from(value);
                self.readable = true;
                self.misses = 0;
                true
            }
            None => {
                self.readable = false;
                self.misses = self.misses.saturating_add(1);
                false
            }
        }
    }
}

/// 原始值换百分比。
///
/// 参考实现直接把亮度当 0..100 用（`num.min(100)`）；绝大多数显示器报的
/// `maximum` 也正好是 100，所以两种算法在这些机器上完全一致。
/// 少数显示器上限不是 100（有的按 0..255 报），按比例换算才不会写越界。
fn percent_of(raw: u32, max: u32) -> u32 {
    let max = scale_max(max);
    ((raw.min(max) as f32 / max as f32) * 100.0).round() as u32
}

/// 百分比换原始值，写回显示器用。
fn raw_of(percent: u32, max: u32) -> u32 {
    let max = scale_max(max);
    ((percent.min(100) as f32 / 100.0) * max as f32).round() as u32
}

/// 显示器报的上限：`0` / `1` 这种明显不是量程的值按 100 处理 ——
/// 拿 1 当上限的话任何读数都会算成 100%，等于把亮度钉死在最大。
fn scale_max(max: u32) -> u32 {
    if max <= 1 { 100 } else { max }
}

/// 读空之后隔多久补读：0.7s 起、每次翻倍、最多 20s 一次。
const RETRY_FIRST: Duration = Duration::from_millis(700);
const RETRY_CAP: Duration = Duration::from_millis(20_000);

/// 两次 DDC/CI 往返之间要喘口气。
///
/// 实测（ROG XG279Q，DisplayPort）：连着发的读里有四分之一左右会报
/// `0xC0262582 / 0xC0262583 / 0xC0262589`（I2C 收发出错），隔几十毫秒再发就基本不再出错。
/// 上一条刚失败时立刻再发，几乎必然再失败 —— 所以重试之间必须留间隔，
/// 光靠「多试几次」是不够的。
const DDC_GAP: Duration = Duration::from_millis(40);

/// 第 `misses` 次读空之后，下一次补读等多久。
fn retry_delay(misses: u32) -> Duration {
    let mut delay = RETRY_FIRST;
    for _ in 1..misses {
        delay = (delay * 2).min(RETRY_CAP);
    }
    delay.min(RETRY_CAP)
}

/// 下一次补读的时间点。全都读得到就不用补，返回 `None`。
fn next_retry(entries: &[Entry]) -> Option<Instant> {
    entries
        .iter()
        .filter(|entry| !entry.readable)
        .map(|entry| entry.misses)
        .max()
        .map(|misses| Instant::now() + retry_delay(misses))
}

/// 补读那几台读不到的。
fn retry_unreadable(entries: &mut [Entry]) {
    let mut first = true;
    for entry in entries.iter_mut().filter(|entry| !entry.readable) {
        if !first {
            std::thread::sleep(DDC_GAP);
        }
        first = false;
        entry.reread();
    }
}

/// HDR 开关多久对一次。
///
/// 系统设置里那个「使用 HDR」不会给任何窗口发消息，只能自己定时看一眼；
/// 只查色彩空间不碰 DDC/CI，一次一毫秒以内，两秒一轮不算负担。
const HDR_POLL: Duration = Duration::from_millis(2_000);

fn run_worker(commands: Receiver<DisplayCommand>, events: async_channel::Sender<DisplayEvent>) {
    let mut entries: Vec<Entry> = Vec::new();
    // 排着一次补读；`None` = 所有显示器都读得通，不用补。
    let mut retry_at: Option<Instant> = None;
    let mut hdr_at = Instant::now() + HDR_POLL;

    loop {
        // 有显示器读不到就先排个补读、HDR 状态要定时对：到点了就算没有命令也醒一次。
        let wait = {
            let now = Instant::now();
            let mut next = hdr_at;
            if let Some(at) = retry_at {
                if at < next {
                    next = at;
                }
            }
            next.saturating_duration_since(now)
        };
        let command = match commands.recv_timeout(wait) {
            Ok(command) => Some(command),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => return,
        };

        match command {
            // 定时醒：补读到点就补读，HDR 状态变了就整轮重扫。
            None => {
                let now = Instant::now();
                if retry_at.is_some_and(|at| at <= now) {
                    retry_unreadable(&mut entries);
                    emit(&events, &entries);
                }
                if now >= hdr_at {
                    hdr_at = now + HDR_POLL;
                    if hdr_changed(&entries) {
                        entries = scan(&events);
                    }
                }
            }
            Some(DisplayCommand::Shutdown) => return,
            Some(DisplayCommand::Refresh) => {
                entries = scan(&events);
            }
            Some(DisplayCommand::SetBrightness { key, value }) => {
                let mut target = (key, value);
                let mut refresh_after = false;
                // 写之前先并一次：连按快捷键时队列里会压着好几档，一次 DDC 写要
                // 六十毫秒，中间那些值落下去就已经过时了 —— 白写，而且亮度会
                // 一格一格地追、越落越远（长按时那种「拖不动」的手感就是这么来的）。
                while let Ok(command) = commands.try_recv() {
                    match command {
                        DisplayCommand::SetBrightness { key, value } => target = (key, value),
                        DisplayCommand::Refresh => refresh_after = true,
                        DisplayCommand::Shutdown => return,
                    }
                }
                loop {
                    apply(&mut entries, &target.0, target.1);
                    // 先把新值回给界面：DDC 写入有延迟，等下一次读回再刷新会明显滞后。
                    emit(&events, &entries);

                    // 拖动滑条时队列里会压着一串值，只写最后一个 —— 一次 DDC 写
                    // 就要几十毫秒，逐条写等于让亮度追着鼠标跑却越落越远。
                    let mut next = None;
                    while let Ok(command) = commands.try_recv() {
                        match command {
                            DisplayCommand::SetBrightness { key, value } => {
                                next = Some((key, value));
                            }
                            DisplayCommand::Refresh => refresh_after = true,
                            DisplayCommand::Shutdown => return,
                        }
                    }
                    match next {
                        Some(next) => target = next,
                        None => break,
                    }
                }
                if refresh_after {
                    entries = scan(&events);
                }
            }
        }

        retry_at = next_retry(&entries);
    }
}

/// 界面上这台屏叫什么：驱动报的能用就用，只报通用名时换成 EDID 里的型号。
///
/// `model` 是**这块屏自己**的真型号（`platform::monitors::screens()` 按句柄给的），
/// 不是「按序号猜的隔壁屏」的。
fn display_name(index: usize, reported: &str, model: Option<&str>) -> String {
    if reported.trim().is_empty() {
        return model
            .filter(|model| !model.trim().is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| format!("显示器 {}", index + 1));
    }
    match model {
        Some(model)
            if crate::platform::monitors::is_generic_name(reported.trim())
                && !model.trim().is_empty() =>
        {
            model.to_string()
        }
        _ => reported.to_string(),
    }
}

/// 枚举显示器并读出各自亮度。句柄留在 `Entry` 里，下次写就不用重新枚举。
///
/// 一条通道对应一块屏的一台物理显示器，**按 `HMONITOR` 对号**：
/// `EnumDisplayMonitors` 里的屏和 `GetPhysicalMonitorsFromHMONITOR` 给的物理
/// 显示器不保证一一对应（笔记本合盖后内置屏还在枚举里，却拿不到物理显示器），
/// 按下标配对会把外接屏的名字和 HDR 标成内置屏的。
fn scan(events: &async_channel::Sender<DisplayEvent>) -> Vec<Entry> {
    let screens = crate::platform::monitors::screens();
    // DDC 那边的句柄类型是 `winapi` 的，和 `windows` 的不是一个东西 —— 拿它的
    // 枚举结果按值（`isize`）和上面那份对号，不必在两种句柄之间转换。
    let hmons: Vec<(isize, _)> = match ddc_winapi::enumerate_monitors() {
        Ok(list) => list.into_iter().map(|hmon| (hmon as isize, hmon)).collect(),
        Err(error) => {
            let _ = events.send_blocking(DisplayEvent::Error(format!("无法枚举显示器：{error}")));
            return Vec::new();
        }
    };
    let hdr_screens = hdr_screens();
    // 内置屏（笔记本面板）走背光接口，别的屏走 DDC/CI。
    let internal = crate::platform::backlight::internal_device();
    let mut backlight: Option<Rc<Backlight>> = None;
    let mut entries = Vec::with_capacity(screens.len());
    let mut handles = Vec::with_capacity(screens.len());
    for (index, screen) in screens.iter().enumerate() {
        let handle = screen.handle;
        let hdr = screen_is_hdr(handle, &hdr_screens);
        // 内置屏：DDC/CI 基本不通（笔记本面板没接 MCCS 那条线），走 `\\.\LCD` ——
        // 系统那个亮度滑块走的就是它，两边调的是同一个东西。
        let is_internal = internal
            .as_deref()
            .is_some_and(|device| device.eq_ignore_ascii_case(&screen.device));
        if is_internal {
            if backlight.is_none() {
                backlight = Backlight::open().map(Rc::new);
            }
            if let Some(shared) = backlight.clone() {
                let name = display_name(index, &screen.name, screen.model.as_deref());
                let mut entry = Entry {
                    channel: Channel::Backlight(shared),
                    key: format!("{index}:{name}"),
                    name,
                    max: 100,
                    raw: 0,
                    readable: false,
                    misses: 0,
                    hdr,
                    handle,
                };
                // 背光读得到就算数；读不到（驱动只给写不给读）再退回 DDC 试试。
                if entry.reread() {
                    entries.push(entry);
                    handles.push(handle);
                    continue;
                }
            }
        }
        let Some(hmon) = hmons
            .iter()
            .find(|(known, _)| *known == handle)
            .map(|(_, hmon)| *hmon)
        else {
            continue;
        };
        let physical = match ddc_winapi::get_physical_monitors_from_hmonitor(hmon) {
            Ok(list) => list,
            Err(error) => {
                if log::log_enabled!(log::Level::Debug) {
                    log::debug!(
                        "拿不到这块屏的物理显示器（{}）：{error}",
                        gdi_device_name(hmonitor(handle)).unwrap_or_default()
                    );
                }
                continue;
            }
        };
        for physical in physical {
            let monitor = unsafe { Monitor::new(physical) };
            let reported = monitor.description();
            let name = display_name(index, &reported, screen.model.as_deref());
            // key 一直用驱动报的原文：名字换成型号之后，以前存的别名还认得出来。
            let key = if reported.trim().is_empty() {
                format!("{index}:{name}")
            } else {
                format!("{index}:{reported}")
            };
            let mut entry = Entry {
                channel: Channel::Ddc(monitor),
                key,
                name,
                max: 100,
                raw: 0,
                readable: false,
                misses: 0,
                hdr,
                handle,
            };
            entry.reread();
            entries.push(entry);
            handles.push(handle);
        }
    }
    remember_screens(&handles, &screens);
    emit(events, &entries);
    entries
}

/// 诊断用：把亮度这条链路上的三层信息摊开写出来。
///
/// 用户报「笔记本外接屏顶着内置屏的名字」「内置屏调不了亮度」时，我们看不到他的
/// 机器，只能靠这份报告判断是哪一层对不上：系统枚举到几块屏、DDC 给了几台物理
/// 显示器、最后界面上拿到的是哪条。只在 `--diag-displays` 里用。
pub fn diag_report() -> String {
    use std::fmt::Write as _;

    let mut out = String::new();
    let _ = writeln!(
        out,
        "Bolume {} / {} build {} / {}",
        crate::version(),
        if crate::platform::winver::is_windows_11() {
            "Windows 11"
        } else {
            "Windows 10"
        },
        crate::platform::winver::build_number(),
        std::env::consts::ARCH
    );
    if let Ok(exe) = std::env::current_exe() {
        let _ = writeln!(out, "exe: {}", exe.display());
        // 用户手里是哪一版：版本号一样时靠修改时间和大小分辨（诊断包里最常问的一句）。
        if let Ok(meta) = std::fs::metadata(&exe) {
            let when = meta
                .modified()
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|since| {
                    // 不加时间库：按 UTC 秒数报，能对上文件时间就够。
                    format!("{} 秒（Unix 时间，UTC）", since.as_secs())
                })
                .unwrap_or_else(|| "?".into());
            let _ = writeln!(out, "exe 大小={} 字节，修改时间={when}", meta.len());
        }
    }
    let (width, height) = unsafe {
        (
            windows::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
                windows::Win32::UI::WindowsAndMessaging::SM_CXVIRTUALSCREEN,
            ),
            windows::Win32::UI::WindowsAndMessaging::GetSystemMetrics(
                windows::Win32::UI::WindowsAndMessaging::SM_CYVIRTUALSCREEN,
            ),
        )
    };
    let _ = writeln!(out, "虚拟桌面: {width}×{height}");

    let _ = writeln!(out, "\n{}", crate::platform::monitors::diag_report());

    let _ = writeln!(out, "── DXGI 输出（HDR 判定用）");
    match unsafe { CreateDXGIFactory1::<IDXGIFactory1>() } {
        Ok(factory) => {
            let mut adapter_index = 0;
            loop {
                let Ok(adapter) = (unsafe { factory.EnumAdapters1(adapter_index) }) else {
                    break;
                };
                adapter_index += 1;
                let description = unsafe { adapter.GetDesc1() }
                    .map(|desc| {
                        let len = desc
                            .Description
                            .iter()
                            .position(|unit| *unit == 0)
                            .unwrap_or(desc.Description.len());
                        String::from_utf16_lossy(&desc.Description[..len])
                    })
                    .unwrap_or_default();
                let _ = writeln!(out, "  适配器[{adapter_index}] {description:?}");
                let mut output_index = 0;
                loop {
                    let Ok(output) = (unsafe { adapter.EnumOutputs(output_index) }) else {
                        break;
                    };
                    output_index += 1;
                    let Ok(six) = output.cast::<IDXGIOutput6>() else {
                        let _ = writeln!(out, "    输出[{output_index}] 拿不到 IDXGIOutput6");
                        continue;
                    };
                    let Ok(desc) = (unsafe { six.GetDesc1() }) else {
                        let _ = writeln!(out, "    输出[{output_index}] GetDesc1 失败");
                        continue;
                    };
                    let len = desc
                        .DeviceName
                        .iter()
                        .position(|unit| *unit == 0)
                        .unwrap_or(desc.DeviceName.len());
                    let name = String::from_utf16_lossy(&desc.DeviceName[..len]);
                    let _ = writeln!(
                        out,
                        "    输出[{output_index}] {name:?} 桌面={} 色彩空间={} HDR={}",
                        desc.AttachedToDesktop.as_bool(),
                        desc.ColorSpace.0,
                        is_hdr_color_space(desc.ColorSpace)
                    );
                }
            }
        }
        Err(error) => {
            let _ = writeln!(out, "  建工厂失败：{error}");
        }
    }

    let _ = writeln!(out, "\n── DDC/CI 物理显示器（按 HMONITOR 对号）");
    match ddc_winapi::enumerate_monitors() {
        Ok(list) => {
            let _ = writeln!(out, "   ddc 枚举到 {} 个 HMONITOR", list.len());
            for hmon in list {
                let handle = hmon as isize;
                let device = gdi_device_name(hmonitor(handle)).unwrap_or_default();
                let _ = writeln!(out, "   [0x{handle:X}] {device}");
                match ddc_winapi::get_physical_monitors_from_hmonitor(hmon) {
                    Ok(physical) => {
                        let _ = writeln!(out, "      物理显示器 {} 台", physical.len());
                        for physical in physical {
                            let mut monitor = unsafe { Monitor::new(physical) };
                            let description = monitor.description();
                            let read = monitor.get_vcp_feature(VCP_BRIGHTNESS);
                            let _ = writeln!(
                                out,
                                "      描述={description:?} 亮度={:?}",
                                read.map(|value| (value.value(), value.maximum()))
                                    .map_err(|error| error.to_string())
                            );
                        }
                    }
                    Err(error) => {
                        let _ = writeln!(out, "      GetPhysicalMonitors 失败：{error}");
                    }
                }
            }
        }
        Err(error) => {
            let _ = writeln!(out, "   ddc 枚举失败：{error}");
        }
    }

    let _ = writeln!(out, "\n── 背光与显示路径\n{}", crate::platform::backlight::diag_report());

    // 真的跑一遍生产用的 `scan`：界面拿到的就是这一份。
    let (events, receiver) = async_channel::bounded(8);
    let entries = scan(&events);
    drop(events);
    while let Ok(event) = receiver.try_recv() {
        if let DisplayEvent::Error(message) = event {
            let _ = writeln!(out, "\n[scan 报错] {message}");
        }
    }
    let _ = writeln!(out, "── 最终条目（亮度面板与设置页看到的就是这些）");
    for entry in &entries {
        let _ = writeln!(
            out,
            "  name={:?} key={:?} 通道={} 可读={} 值={}/{} HDR={} handle=0x{:X}",
            entry.name,
            entry.key,
            match entry.channel {
                Channel::Ddc(_) => "DDC/CI",
                Channel::Backlight(_) => "背光(\\\\.\\LCD)",
            },
            entry.readable,
            entry.raw,
            entry.max,
            entry.hdr,
            entry.handle
        );
    }
    if entries.is_empty() {
        let _ = writeln!(out, "  （一条都没有）");
    }
    out
}

/// 记下这次扫出来的通道次序和系统报的屏，供「光标在哪台」和插拔检测用。
fn remember_screens(handles: &[isize], screens: &[crate::platform::monitors::MonitorInfo]) {    if let Ok(mut entry_screens) = ENTRY_SCREENS.lock() {
        *entry_screens = handles.to_vec();
    }
    if let Ok(mut known) = KNOWN_SCREENS.lock() {
        *known = screens.iter().map(|screen| screen.handle).collect();
    }
}

/// 读一次亮度，读到「两次一致」才算数。
///
/// 真机上直接读并不可靠：ASUS ROG XG279Q 上限明明是 100，偶尔会读回 250，
/// 换算出来就是 40% 这种假值；ROG STRIX 那边也偶尔报 I2C 错误。
/// 所以连读两次互相对上才采信，最坏多花几十毫秒 —— 只在 `Refresh` 时发生，不拖界面。
fn read_stable(monitor: &mut Monitor) -> Option<(u16, u16)> {
    let mut last: Option<(u16, u16)> = None;
    for attempt in 0..4 {
        if attempt > 0 {
            std::thread::sleep(DDC_GAP);
        }
        match monitor.get_vcp_feature(VCP_BRIGHTNESS) {
            Ok(value) => {
                let now = (value.value(), value.maximum());
                if last == Some(now) {
                    return Some(now);
                }
                last = Some(now);
            }
            Err(error) => {
                if log::log_enabled!(log::Level::Debug) {
                    log::debug!("DDC 读亮度失败（第 {} 次）：{error}", attempt + 1);
                }
            }
        }
    }
    // 四次都没对上一次：退一步用最后一次读到的，总比显示「不支持」强。
    if last.is_none() && log::log_enabled!(log::Level::Debug) {
        log::debug!("DDC 读亮度：这台显示器 4 次都没应答");
    }
    last
}

/// 把某个 key 的亮度写下去；写成功才更新本地缓存。
fn apply(entries: &mut [Entry], key: &str, percent: u32) {
    let Some(entry) = entries.iter_mut().find(|entry| entry.key == key) else {
        return;
    };
    // 开着 HDR 的屏不让写：写下去画面不变，界面却显示「已调到 40%」，等于骗人。
    // （界面那边本来就按 `adjustable()` 禁掉了，这里是第二道闸。）
    if entry.hdr {
        return;
    }
    // 上一次扫描没读到，不代表这台不能写：写之前先补读一次。
    // （显示器刚醒、显卡刚初始化完的那一下读不到很正常，直接放弃就是「拖不动」。）
    if !entry.readable && !entry.reread() {
        return;
    }
    let raw = raw_of(percent, entry.max);
    let written = match &mut entry.channel {
        Channel::Ddc(monitor) => monitor
            .set_vcp_feature(VCP_BRIGHTNESS, raw as u16)
            .is_ok(),
        Channel::Backlight(backlight) => backlight.write(raw as u16),
    };
    if written {
        entry.raw = raw;
    }
}

fn emit(events: &async_channel::Sender<DisplayEvent>, entries: &[Entry]) {
    let list: Vec<DisplayInfo> = entries.iter().map(Entry::info).collect();
    let _ = events.send_blocking(DisplayEvent::Monitors(list));
}

#[cfg(test)]
mod tests {
    use super::{RETRY_CAP, RETRY_FIRST, display_name, percent_of, raw_of, retry_delay};

    #[test]
    fn generic_driver_names_take_the_edid_model() {
        assert_eq!(display_name(0, "Generic Monitor", Some("XG279Q")), "XG279Q");
        assert_eq!(
            display_name(1, "Generic PnP Monitor", Some("DELL U2412M")),
            "DELL U2412M"
        );
        // EDID 也读不出来就照驱动给的写，别显示成空白。
        assert_eq!(
            display_name(0, "Generic Monitor", Some("")),
            "Generic Monitor"
        );
        assert_eq!(display_name(0, "Generic Monitor", None), "Generic Monitor");
    }

    #[test]
    fn real_model_names_are_left_alone() {
        assert_eq!(
            display_name(0, "ASUS ROG XG279Q(DisplayPort)", Some("ROG XG279Q")),
            "ASUS ROG XG279Q(DisplayPort)"
        );
        // 驱动连名字都没报：有真型号就用真型号，比「显示器 1」有用。
        assert_eq!(display_name(0, "  ", Some("XG279Q")), "XG279Q");
        assert_eq!(display_name(2, "", None), "显示器 3");
    }

    #[test]
    fn percent_and_raw_round_trip_on_hundred() {
        for percent in [0u32, 1, 17, 50, 99, 100] {
            assert_eq!(percent_of(raw_of(percent, 100), 100), percent);
        }
        assert_eq!(raw_of(0, 100), 0);
        assert_eq!(raw_of(100, 100), 100);
    }

    #[test]
    fn non_hundred_maximum_is_scaled() {
        // 按 0..255 报上限的显示器：写 50% 就是 128，读回来还是 50%。
        assert_eq!(raw_of(50, 255), 128);
        assert_eq!(percent_of(128, 255), 50);
        assert_eq!(percent_of(255, 255), 100);
        // 上限是 0（显示器没报）时按 0..100 处理，不会除零。
        assert_eq!(percent_of(37, 0), 37);
        assert_eq!(raw_of(37, 0), 37);
        // 上限报成 1 也不能当量程，否则任何读数都会被算成 100%。
        assert_eq!(percent_of(37, 1), 37);
        // 超出上限的百分比按上限截断，绝不写越界。
        assert_eq!(raw_of(140, 80), 80);
        assert_eq!(percent_of(140, 80), 100);
    }

    /// 读不到时的补读间隔：第一档要快（用户正盯着「不支持」），之后指数退避，封顶。
    #[test]
    fn retry_waits_grow_then_cap() {
        assert_eq!(retry_delay(0), RETRY_FIRST);
        assert_eq!(retry_delay(1), RETRY_FIRST);
        assert_eq!(retry_delay(2), RETRY_FIRST * 2);
        assert_eq!(retry_delay(3), RETRY_FIRST * 4);
        // 一直读不到也不会越等越离谱。
        for misses in 4..64 {
            let delay = retry_delay(misses);
            assert!(delay >= RETRY_FIRST * 4, "第 {misses} 次退避太小了：{delay:?}");
            assert!(delay <= RETRY_CAP, "第 {misses} 次退避超上限了：{delay:?}");
        }
        assert_eq!(retry_delay(u32::MAX), RETRY_CAP);
    }
}
