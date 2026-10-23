//! 系统媒体会话（SMTC）：查当前播放器到底在「播放」还是「暂停」。
//!
//! 播放 / 暂停是**同一个**媒体键，光看按键分不出这次是开始播还是停下来，
//! 只能问系统。Windows 的 `GlobalSystemMediaTransportControlsSessionManager`
//! 就是干这个的（EarTrumpet 的媒体控制也走它）。
//!
//! WinRT 的异步对象要求公寓里有消息泵，所以查询统一放在一条自己的 MTA 线程上：
//! 调用方拿到的是「请求进、布尔出」的同步接口，一次查询通常不到 1ms，
//! 拿不到（老播放器 / 系统不支持）就返回 `None`，由调用方决定怎么兜底。

use std::{
    sync::{
        OnceLock,
        mpsc::{self, Receiver, RecvTimeoutError, Sender},
    },
    time::Duration,
};

use windows::Media::Control::{
    GlobalSystemMediaTransportControlsSessionManager,
    GlobalSystemMediaTransportControlsSessionPlaybackStatus as PlaybackStatus,
};

/// 平时一次查询最多等多久。超时就当「不知道」，绝不能把界面卡在这儿。
const QUERY_TIMEOUT: Duration = Duration::from_millis(80);

/// 第一次向系统要 manager 可能要几百毫秒（要起 COM 服务）。
const OPEN_TIMEOUT: Duration = Duration::from_millis(1500);

struct Query {
    timeout: Duration,
    reply: Sender<Option<bool>>,
}

fn channel() -> &'static Sender<Query> {
    static CHANNEL: OnceLock<Sender<Query>> = OnceLock::new();
    CHANNEL.get_or_init(spawn)
}

/// 预热：启动时调一次，把 MTA 线程和 manager 先备好。
///
/// 不预热的话第一次按播放暂停键要等 WinRT 起服务，那一键的手感会明显发木。
pub fn prewarm() {
    let (reply, answer) = mpsc::channel();
    let sent = channel().send(Query {
        timeout: OPEN_TIMEOUT,
        reply,
    });
    if sent.is_ok() {
        let _ = answer.recv_timeout(OPEN_TIMEOUT);
    }
}

/// 当前是否有播放器正在播放。
///
/// `Some(true)` 正在播放，`Some(false)` 已暂停 / 已停止，`None` 问不出来
/// （没有 SMTC 会话，或者系统太老）。
pub fn is_playing() -> Option<bool> {
    let (reply, answer) = mpsc::channel();
    channel()
        .send(Query {
            timeout: QUERY_TIMEOUT,
            reply,
        })
        .ok()?;
    match answer.recv_timeout(QUERY_TIMEOUT) {
        Ok(state) => state,
        Err(RecvTimeoutError::Timeout) | Err(RecvTimeoutError::Disconnected) => None,
    }
}

/// MTA 查询线程：一次握手，之后反复用同一个 manager。
fn spawn() -> Sender<Query> {
    let (tx, rx) = mpsc::channel::<Query>();
    std::thread::Builder::new()
        .name("bolume-smtc".into())
        .spawn(move || run(rx))
        .expect("spawn smtc thread");
    tx
}

fn run(rx: Receiver<Query>) {
    // WinRT 要求当前线程先声明公寓。MTA 不需要消息泵，适合这种一问一答。
    let _ = unsafe {
        windows::Win32::System::Com::CoInitializeEx(
            None,
            windows::Win32::System::Com::COINIT_MULTITHREADED,
        )
    };
    let mut manager: Option<GlobalSystemMediaTransportControlsSessionManager> = None;
    for query in rx {
        let state = poll(&mut manager, query.timeout);
        let _ = query.reply.send(state);
    }
}

/// 问一次系统：当前会话在播放吗？
fn poll(
    manager: &mut Option<GlobalSystemMediaTransportControlsSessionManager>,
    timeout: Duration,
) -> Option<bool> {
    if manager.is_none() {
        *manager = open_manager(timeout);
    }
    let manager = manager.as_ref()?;
    let session = manager.GetCurrentSession().ok()?;
    let info = session.GetPlaybackInfo().ok()?;
    let status = info.PlaybackStatus().ok()?;
    Some(matches!(status, PlaybackStatus::Playing))
}

/// 第一次向系统要 manager（`RequestAsync` 是异步对象，用 `when` 等回调）。
fn open_manager(timeout: Duration) -> Option<GlobalSystemMediaTransportControlsSessionManager> {
    let operation = GlobalSystemMediaTransportControlsSessionManager::RequestAsync().ok()?;
    let (tx, rx) = mpsc::channel::<Option<GlobalSystemMediaTransportControlsSessionManager>>();
    operation
        .when(move |result| {
            let _ = tx.send(result.ok());
        })
        .ok()?;
    rx.recv_timeout(timeout).ok().flatten()
}
