//! 脱壳：把自己从「文件管理器 / 启动器」的进程树里挪出来。
//!
//! # 为什么需要
//!
//! `process loopback` 是按**进程树**抓音频的。从 Dopus、Total Commander、
//! 各种第三方启动器里双击运行时，Bolume 就成了它们的子进程；接管这个父进程的
//! 声音时，**自己渲染出去的那条流也在同一棵树里**，抓进来乘增益再送出去就是一个
//! 闭环回授。听感是一段不断重复、辨不出源头的怪音，源停了也不消失（回授自己
//! 维持着），只有手动改一下那个应用的音量强行退出接管才会安静。
//!
//! 所以启动时看一眼父进程：不是系统壳（explorer / 任务计划 / 登录流程）就
//! 重新拉起一份自己、本进程退出。新实例的父进程是已经退场的老进程，父链断开，
//! 回授的前提就没了。这条路径对用户完全无感（窗口还没画出来）。
//!
//! 副本带 [`DETACHED_FLAG`]，不会再脱一次 —— 也防住「老进程的 pid 被系统复用
//! 给另一个同名程序」导致的无限重启。

use std::process::Command;

/// 脱壳副本专用的启动参数。
pub const DETACHED_FLAG: &str = "--detached";

/// 父进程是这些时不用脱壳：它们本身不会成为音频接管的目标。
///
/// - `explorer.exe`：桌面 / 开始菜单 / 任务栏 / 快捷方式；
/// - `svchost.exe`：任务计划（用户登录时启动那一档走这里）；
/// - `taskhostw.exe`：任务计划的宿主（Win10 上「用户登录时启动」跑的是它）；
/// - 其余三个是登录流程里的，`boot-handoff` 送进来的实例父进程一般就是它们。
const SHELLS: &[&str] = &[
    "explorer.exe",
    "svchost.exe",
    "taskhostw.exe",
    "services.exe",
    "winlogon.exe",
    "userinit.exe",
];

/// 这次启动要不要脱壳。
pub fn needs_detach(args: &[String]) -> bool {
    if args.iter().any(|arg| arg == DETACHED_FLAG) {
        return false;
    }
    // 调试 / 脚本里想保留原来的父进程时用。
    if std::env::var_os("BOLUME_NO_DETACH").is_some() {
        return false;
    }
    // 父进程已经退场（`restart::spawn` 的副本就是这样）：父链本来就在旧进程那里
    // 断掉了，不存在「接管父进程」这回事。
    let Some(parent) = parent_of(std::process::id()) else {
        return false;
    };
    match name_of(parent) {
        Some(name) => !SHELLS.iter().any(|shell| shell.eq_ignore_ascii_case(&name)),
        None => false,
    }
}

/// 拉起脱壳副本（原参数全带上，再加 [`DETACHED_FLAG`]）。
pub fn respawn(args: &[String]) -> bool {
    use crate::platform::hotkey::trace;
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let mut command = Command::new(exe);
    command.args(args).arg(DETACHED_FLAG);
    if let Ok(dir) = std::env::current_dir() {
        command.current_dir(dir);
    }
    let ok = command.spawn().is_ok();
    trace(&format!(
        "detach parent={:?} from={:?} ok={ok}",
        parent_of(std::process::id()).and_then(name_of),
        std::env::current_exe().ok()
    ));
    ok
}

/// 本进程是不是 `pid` 的后代（含它自己）。
///
/// 用来挡回授：接管前确认自己不在对方的进程树里。父链最多走 32 层，
/// 够覆盖任何正常的启动方式。
pub fn is_descendant_of(pid: u32) -> bool {
    let mut current = std::process::id();
    for _ in 0..32 {
        if current == pid {
            return true;
        }
        match parent_of(current) {
            Some(parent) if parent != 0 && parent != current => current = parent,
            _ => return false,
        }
    }
    false
}

/// 查某个进程的父进程 id。
pub fn parent_of(pid: u32) -> Option<u32> {
    entry(pid).map(|(parent, _)| parent)
}

/// 查某个进程的可执行文件名。
pub fn name_of(pid: u32) -> Option<String> {
    entry(pid).map(|(_, name)| name)
}

/// 从进程快照里取一项：(父 pid, 可执行文件名)。
fn entry(pid: u32) -> Option<(u32, String)> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
    };
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?;
        let mut raw = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut found = None;
        if Process32FirstW(snapshot, &mut raw).is_ok() {
            loop {
                if raw.th32ProcessID == pid {
                    // `szExeFile` 是定长 UTF-16，遇到第一个 0 就是结尾。
                    let end = raw
                        .szExeFile
                        .iter()
                        .position(|unit| *unit == 0)
                        .unwrap_or(raw.szExeFile.len());
                    found = Some((
                        raw.th32ParentProcessID,
                        String::from_utf16_lossy(&raw.szExeFile[..end]),
                    ));
                    break;
                }
                if Process32NextW(snapshot, &mut raw).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);
        found
    }
}
