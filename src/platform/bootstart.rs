//! 开机 → 会话的两段式启动。
//!
//! 系统启动那一档注册的是**开机触发、SYSTEM 身份**的任务，动作是本 exe 带
//! `--boot-handoff`。那个进程跑在会话 0：没有桌面、没有通知区域、也没有用户的音频
//! 设备，所以它**不画界面**，只做两件事 ——
//!
//! 1. 等：等到有用户会话、而且那个会话里 explorer 已经在了（图标要挂在
//!    通知区域上，壳没起来之前挂上去等于白挂）。
//! 2. 送：拿那个会话里 **explorer 的令牌**，`CreateProcessAsUserW` 把主程序送进
//!    `winsta0\default`，然后自己退场。
//!
//! 用 explorer 的令牌而不是 `WTSQueryUserToken`：后者拿到的是完整令牌，UAC 开着的
//! 机器上等于凭空给了一份管理员权限；explorer 的令牌和用户自己双击**一模一样**，
//! 该是普通权限就是普通权限。开了「启动时以管理员运行」时才改用完整令牌。
//!
//! 另外还有 `--elevate`：注册表那条启动项没法由系统静默提权，只能自己
//! `ShellExecuteW(runas)` 重新拉一份自己。

use std::time::{Duration, Instant};

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{
    DuplicateTokenEx, SecurityImpersonation, TOKEN_ALL_ACCESS, TOKEN_ASSIGN_PRIMARY,
    TOKEN_DUPLICATE, TOKEN_QUERY, TokenPrimary,
};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Environment::{CreateEnvironmentBlock, DestroyEnvironmentBlock};
use windows::Win32::System::RemoteDesktop::{
    ProcessIdToSessionId, WTSEnumerateSessionsW, WTSFreeMemory, WTS_SESSION_INFOW, WTSActive,
};
use windows::Win32::System::Threading::{
    CREATE_UNICODE_ENVIRONMENT, CreateProcessAsUserW, OpenProcess, OpenProcessToken,
    PROCESS_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION, STARTUPINFOW,
};
use windows::core::{PCWSTR, PWSTR};

/// 在登录界面干等的上限。超过就退场 —— 任务里还挂了「登录时」触发，
/// 真有人晚点登录时任务计划程序会重新拉起一份，不必一直挂着一个 SYSTEM 进程。
const WAIT_LIMIT: Duration = Duration::from_secs(30 * 60);

/// 没等到就再问一次的间隔。
const POLL: Duration = Duration::from_millis(500);

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// `--boot-handoff` 的入口：把主程序送进用户会话，然后退场。
///
/// `admin` 来自任务动作里附带的 `--elevate`（就是设置页那个「启动时以管理员运行」），
/// 不从配置文件读 —— 会话 0 里的 SYSTEM 进程不该去碰用户的配置。
pub fn handoff(admin: bool) {
    // 已经有实例在跑（手动先开了、或者「登录时」那条抢在前面）：不叫醒、不弹面板，
    // 直接退场。开机那一刻弹个面板才是最糟的行为。
    if crate::platform::SingleInstance::is_running(crate::platform::single_instance::INSTANCE_NAME)
    {
        return;
    }
    let deadline = Instant::now() + WAIT_LIMIT;
    loop {
        for session in active_sessions() {
            if let Some(token) = session_token(session, admin) {
                let launched = launch_in_session(token);
                unsafe {
                    let _ = CloseHandle(token);
                }
                if launched {
                    return;
                }
            }
        }
        if Instant::now() >= deadline {
            return;
        }
        std::thread::sleep(POLL);
    }
}

/// 当前活动着的会话（用户登进来才有的那种）。
fn active_sessions() -> Vec<u32> {
    unsafe {
        let mut info: *mut WTS_SESSION_INFOW = std::ptr::null_mut();
        let mut count = 0u32;
        if WTSEnumerateSessionsW(None, 0, 1, &mut info, &mut count).is_err() || info.is_null() {
            return Vec::new();
        }
        let sessions = std::slice::from_raw_parts(info, count as usize)
            .iter()
            .filter(|session| session.State == WTSActive)
            .map(|session| session.SessionId)
            .collect();
        WTSFreeMemory(info as *mut _);
        sessions
    }
}

/// 那个会话里 explorer 的进程号 —— 它在了，才说明通知区域建好了。
fn shell_pid(session: u32) -> Option<u32> {
    unsafe {
        let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0).ok()?;
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut found = None;
        let mut more = Process32FirstW(snapshot, &mut entry).is_ok();
        while more {
            let name = String::from_utf16_lossy(&entry.szExeFile);
            if name.trim_end_matches('\0').eq_ignore_ascii_case("explorer.exe") {
                let mut id = 0u32;
                if ProcessIdToSessionId(entry.th32ProcessID, &mut id).is_ok() && id == session {
                    found = Some(entry.th32ProcessID);
                    break;
                }
            }
            more = Process32NextW(snapshot, &mut entry).is_ok();
        }
        let _ = CloseHandle(snapshot);
        found
    }
}

/// 用来起进程的令牌。
///
/// * `admin == false`：explorer 的令牌复制一份（= 用户双击的那个权限）。
/// * `admin == true`：`WTSQueryUserToken` 拿会话的完整令牌（管理员账户才真提权）。
fn session_token(session: u32, admin: bool) -> Option<HANDLE> {
    let source = if admin {
        let mut token = HANDLE::default();
        unsafe { windows::Win32::System::RemoteDesktop::WTSQueryUserToken(session, &mut token) }
            .ok()?;
        token
    } else {
        let pid = shell_pid(session)?;
        unsafe {
            let process = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
            let mut token = HANDLE::default();
            let opened = OpenProcessToken(
                process,
                TOKEN_DUPLICATE | TOKEN_ASSIGN_PRIMARY | TOKEN_QUERY,
                &mut token,
            )
            .is_ok();
            let _ = CloseHandle(process);
            if !opened {
                return None;
            }
            token
        }
    };
    // 一律复制成主令牌：`CreateProcessAsUserW` 只认主令牌，
    // 而会话令牌是「能不能进程用它」另说的那种。
    let mut primary = HANDLE::default();
    let duplicated = unsafe {
        DuplicateTokenEx(
            source,
            TOKEN_ALL_ACCESS,
            None,
            SecurityImpersonation,
            TokenPrimary,
            &mut primary,
        )
    }
    .is_ok();
    unsafe {
        let _ = CloseHandle(source);
    }
    duplicated.then_some(primary)
}

/// 用那条令牌在它的会话桌面上起一份主程序。
fn launch_in_session(token: HANDLE) -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let app = wide(&exe.display().to_string());
    let mut command: Vec<u16> = format!("\"{}\"", exe.display())
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    // 桌面名决定它落在哪个窗口站：不写就是 `winsta0\default` 之外的默认值，界面看不见。
    let desktop = wide("winsta0\\default");
    let workdir = exe
        .parent()
        .map(|dir| wide(&dir.display().to_string()))
        .unwrap_or_else(|| wide(""));
    unsafe {
        let mut env: *mut core::ffi::c_void = std::ptr::null_mut();
        let has_env = CreateEnvironmentBlock(&mut env, Some(token), false).is_ok();
        let startup = STARTUPINFOW {
            cb: std::mem::size_of::<STARTUPINFOW>() as u32,
            lpDesktop: PWSTR(desktop.as_ptr() as *mut u16),
            ..Default::default()
        };
        let mut process = PROCESS_INFORMATION::default();
        let created = CreateProcessAsUserW(
            Some(token),
            PCWSTR(app.as_ptr()),
            Some(PWSTR(command.as_mut_ptr())),
            None,
            None,
            false,
            CREATE_UNICODE_ENVIRONMENT,
            has_env.then_some(env as *const core::ffi::c_void),
            PCWSTR(workdir.as_ptr()),
            &startup,
            &mut process,
        )
        .is_ok();
        if has_env {
            let _ = DestroyEnvironmentBlock(env);
        }
        if created {
            let _ = CloseHandle(process.hProcess);
            let _ = CloseHandle(process.hThread);
        }
        created
    }
}

/// `--elevate`：注册表那条启动项没法静默提权，自己 `runas` 重新拉一份。
///
/// UAC 关着的机器上不会有任何提示（直接就是管理员）；UAC 开着的机器上弹一次确认，
/// 取消就什么都不发生。
pub fn elevate_self() {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let Ok(exe) = std::env::current_exe() else {
        return;
    };
    let file = wide(&exe.display().to_string());
    let verb = wide("runas");
    let workdir = exe
        .parent()
        .map(|dir| wide(&dir.display().to_string()))
        .unwrap_or_default();
    unsafe {
        // 返回值 > 32 才算成功；用户点了「否」就是 <= 32，这里什么都不用做。
        ShellExecuteW(
            None,
            PCWSTR(verb.as_ptr()),
            PCWSTR(file.as_ptr()),
            PCWSTR::null(),
            PCWSTR(workdir.as_ptr()),
            SW_SHOWNORMAL,
        );
    }
}
