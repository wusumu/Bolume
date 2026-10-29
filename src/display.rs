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
    sync::mpsc::{Receiver, RecvTimeoutError, Sender, channel},
    time::{Duration, Instant},
};

use ddc::Ddc;
use ddc_winapi::Monitor;
use windows::Win32::Foundation::{LPARAM, POINT, RECT};
use windows::Win32::Graphics::Gdi::{
    EnumDisplayMonitors, HDC, HMONITOR, MONITOR_DEFAULTTONEAREST, MonitorFromPoint,
};
use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;
use windows::core::BOOL;

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

/// 鼠标所在屏幕在 DDC 枚举里的序号（0 起）。
///
/// `ddc_winapi::Monitor::enumerate()` 的顺序 = `EnumDisplayMonitors` 的顺序
/// （它内部就是先枚举 HMONITOR、再取每块屏的物理显示器），所以这里照同样的
/// 顺序数一遍，就知道光标那块屏排第几 —— 不用去匹配设备名（DDC 那边只给
/// 物理显示器描述，和 `\\.\DISPLAY1` 这种对不上）。
///
/// 一台显示器同时报出多个 physical monitor 的罕见情况下序号会往后偏，
/// 那时「当前」会落到隔壁屏 —— 面板照样能调，不是死路。
pub fn monitor_index_at_cursor() -> usize {
    let mut point = POINT::default();
    let target = unsafe {
        if GetCursorPos(&mut point).is_err() {
            return 0;
        }
        MonitorFromPoint(point, MONITOR_DEFAULTTONEAREST)
    };
    let handles = screen_handles();
    handles
        .iter()
        .position(|handle| *handle == target)
        .unwrap_or(0)
}

/// 这块屏（`HMONITOR` 句柄）在 DDC 枚举里排第几（0 起）。
///
/// 顺序和 `Monitor::enumerate()` 一致（见 `monitor_index_at_cursor` 的说明），
/// 所以只要手里有句柄就能对上 `displays` 里的那一项 —— 提示条拖亮度条时要靠它
/// 找到「这条提示条所在那块屏」的亮度通道。认不出来就落到第一台：拖了总得
/// 调点什么，整条没反应更糟。
pub fn index_for_handle(handle: isize) -> usize {
    screen_handles()
        .iter()
        .position(|candidate| candidate.0 as isize == handle)
        .unwrap_or(0)
}

/// MCCS 的亮度 VCP 码（参考实现用的就是它）。
pub const VCP_BRIGHTNESS: u8 = 0x10;
/// MCCS 的对比度 VCP 码：现在就留好，将来加对比度只需多一条命令。
pub const VCP_CONTRAST: u8 = 0x12;

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

/// 工作线程里的一台显示器：句柄 + 它的亮度上限。
struct Entry {
    monitor: Monitor,
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
}

impl Entry {
    fn info(&self) -> DisplayInfo {
        DisplayInfo {
            key: self.key.clone(),
            name: self.name.clone(),
            brightness: self.readable.then(|| percent_of(self.raw, self.max)),
        }
    }

    /// 读一次，把结果落回自己身上。
    fn reread(&mut self) -> bool {
        match read_stable(&mut self.monitor) {
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

fn run_worker(commands: Receiver<DisplayCommand>, events: async_channel::Sender<DisplayEvent>) {
    let mut entries: Vec<Entry> = Vec::new();
    // 排着一次补读；`None` = 所有显示器都读得通，不用补。
    let mut retry_at: Option<Instant> = None;

    loop {
        // 有显示器读不到就先排个补读：到点了就算没有命令也要醒一次。
        let command = match retry_at {
            Some(at) => {
                let wait = at.saturating_duration_since(Instant::now());
                match commands.recv_timeout(wait) {
                    Ok(command) => Some(command),
                    Err(RecvTimeoutError::Timeout) => None,
                    Err(RecvTimeoutError::Disconnected) => return,
                }
            }
            None => match commands.recv() {
                Ok(command) => Some(command),
                Err(_) => return,
            },
        };

        match command {
            // 补读到点：读到了就把新读数推给界面，读不到就等下一档退避。
            None => {
                retry_unreadable(&mut entries);
                emit(&events, &entries);
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

/// 枚举显示器并读出各自亮度。句柄留在 `Entry` 里，下次写就不用重新枚举。
fn scan(events: &async_channel::Sender<DisplayEvent>) -> Vec<Entry> {
    let monitors = match Monitor::enumerate() {
        Ok(monitors) => monitors,
        Err(error) => {
            let _ = events.send_blocking(DisplayEvent::Error(format!("无法枚举显示器：{error}")));
            return Vec::new();
        }
    };

    let mut entries = Vec::with_capacity(monitors.len());
    for (index, monitor) in monitors.into_iter().enumerate() {
        let name = monitor.description();
        let name = if name.trim().is_empty() {
            format!("显示器 {}", index + 1)
        } else {
            name
        };
        let key = format!("{index}:{name}");
        let mut entry = Entry {
            monitor,
            key,
            name,
            max: 100,
            raw: 0,
            readable: false,
            misses: 0,
        };
        entry.reread();
        entries.push(entry);
    }

    emit(events, &entries);
    entries
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
    // 上一次扫描没读到，不代表这台不能写：写之前先补读一次。
    // （显示器刚醒、显卡刚初始化完的那一下读不到很正常，直接放弃就是「拖不动」。）
    if !entry.readable && !entry.reread() {
        return;
    }
    let raw = raw_of(percent, entry.max);
    if entry
        .monitor
        .set_vcp_feature(VCP_BRIGHTNESS, raw as u16)
        .is_ok()
    {
        entry.raw = raw;
    }
}

fn emit(events: &async_channel::Sender<DisplayEvent>, entries: &[Entry]) {
    let list: Vec<DisplayInfo> = entries.iter().map(Entry::info).collect();
    let _ = events.send_blocking(DisplayEvent::Monitors(list));
}

#[cfg(test)]
mod tests {
    use super::{RETRY_CAP, RETRY_FIRST, percent_of, raw_of, retry_delay};

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
