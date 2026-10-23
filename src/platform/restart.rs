//! 重启自己：拉起一个「等本次实例退场后接手」的副本。
//!
//! 单实例锁握在旧进程手里，直接 spawn 一个新进程会被它当成「已有实例」——
//! 唤醒旧实例然后自己退场，看起来就是「点了重启没反应」。
//! 所以副本带 `--restart-wait`，启动后先等锁空出来再继续（见 `SingleInstance::acquire_wait`）。

use std::process::Command;
use std::time::Duration;

/// 重启副本专用的启动参数：启动时先等上一个实例真的退出。
pub const RESTART_WAIT_FLAG: &str = "--restart-wait";

/// 等旧实例退场的最长时间（正常几百毫秒；卡死时也别干等）。
pub const RESTART_WAIT: Duration = Duration::from_secs(20);

/// 拉起重启副本；起不来（拿不到自身路径 / 系统拒绝）返回 false。
pub fn spawn() -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    Command::new(exe).arg(RESTART_WAIT_FLAG).spawn().is_ok()
}
