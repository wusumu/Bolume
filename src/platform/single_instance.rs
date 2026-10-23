//! 单实例互斥：第二个实例唤醒已运行实例并直接退出。

use windows::Win32::{
    Foundation::{CloseHandle, ERROR_ALREADY_EXISTS, GetLastError, HANDLE},
    System::Threading::{
        CreateEventW, CreateMutexW, EVENT_MODIFY_STATE, OpenEventW, SetEvent, WaitForSingleObject,
    },
};
use windows::core::PCWSTR;

/// 事件名：第二个实例用它叫醒第一个实例把面板弹出来。
const ACTIVATE_SUFFIX: &str = ".Activate";

pub struct SingleInstance {
    mutex: HANDLE,
    activate: HANDLE,
}

impl SingleInstance {
    /// 抢主实例；已有实例时返回 None（调用方应再调 `activate_existing` 唤醒它）。
    pub fn acquire(name: &str) -> Option<Self> {
        let mutex_name = wide(name);
        let event_name = wide(&format!("{name}{ACTIVATE_SUFFIX}"));
        unsafe {
            let mutex = CreateMutexW(None, true, PCWSTR(mutex_name.as_ptr())).ok()?;
            if GetLastError() == ERROR_ALREADY_EXISTS {
                let _ = CloseHandle(mutex);
                return None;
            }
            // 自动重置事件：主实例每收到一次信号就弹一次面板。
            let activate = CreateEventW(None, false, false, PCWSTR(event_name.as_ptr()))
                .unwrap_or(HANDLE(std::ptr::null_mut()));
            Some(Self { mutex, activate })
        }
    }

    /// 重启副本用：等上一个实例真的退场（锁释放）再接手。
    ///
    /// 副本是旧进程 spawn 的，那会儿旧进程还在退出途中，直接 `acquire` 会误判「已有实例」
    /// 而把自己叫停 —— 菜单里点「重启」就变成什么都没发生。
    pub fn acquire_wait(name: &str, timeout: std::time::Duration) -> Option<Self> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Some(instance) = Self::acquire(name) {
                return Some(instance);
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(std::time::Duration::from_millis(60));
        }
    }

    /// 第二个实例调用：唤醒已运行实例。找不到事件（老版本/权限问题）时返回 false。
    pub fn activate_existing(name: &str) -> bool {
        let event_name = wide(&format!("{name}{ACTIVATE_SUFFIX}"));
        unsafe {
            let Ok(event) = OpenEventW(EVENT_MODIFY_STATE, false, PCWSTR(event_name.as_ptr()))
            else {
                return false;
            };
            let ok = SetEvent(event).is_ok();
            let _ = CloseHandle(event);
            ok
        }
    }

    /// 在后台等信号，收到就执行回调（回调里只做发消息这种轻活）。
    pub fn on_activate(&self, callback: impl Fn() + Send + 'static) {
        if self.activate.is_invalid() {
            return;
        }
        // HANDLE 里是裸指针，不能跨线程传；地址本身是安全的，转成 usize 带过去。
        let raw = self.activate.0 as usize;
        std::thread::Builder::new()
            .name("bolume-single-instance".into())
            .spawn(move || {
                let event = HANDLE(raw as *mut std::ffi::c_void);
                loop {
                    let result = unsafe { WaitForSingleObject(event, u32::MAX) };
                    if result.0 != 0 {
                        break;
                    }
                    callback();
                }
            })
            .ok();
    }
}

impl Drop for SingleInstance {
    fn drop(&mut self) {
        unsafe {
            let _ = CloseHandle(self.mutex);
            let _ = CloseHandle(self.activate);
        }
    }
}

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}
