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

use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0, WAIT_TIMEOUT};
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
    CREATE_UNICODE_ENVIRONMENT, CreateMutexW, CreateProcessAsUserW, OpenProcess, OpenProcessToken,
    PROCESS_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION, ReleaseMutex, STARTUPINFOW,
    PROCESS_SYNCHRONIZE, WaitForSingleObject,
};
use windows::core::{PCWSTR, PWSTR};

/// 在登录界面干等的上限。超过就退场 —— 任务里还挂了「登录时」触发，
/// 真有人晚点登录时任务计划程序会重新拉起一份，不必一直挂着一个 SYSTEM 进程。
const WAIT_LIMIT: Duration = Duration::from_secs(30 * 60);

/// 没等到就再问一次的间隔。
const POLL: Duration = Duration::from_millis(500);

/// explorer 出现之后再多等这么久才送人。
///
/// 登录那一刻系统还在铺桌面：显卡上下文、音频端点、通知区域都要时间才就位。
/// 早一步送进去，程序自己会在一两秒内退出（事件日志里是 `0xc0000409`），
/// 从外面看就是「开机自启没生效」—— 2026-10-28 / 10-29 两次早上开机都撞在这上面。
const SETTLE: Duration = Duration::from_millis(3000);

/// 送进去之后盯这么久：还活着就算真起来了。
const ALIVE_WINDOW: Duration = Duration::from_secs(4);

/// 脱壳副本接力之后，再确认一次它还在。
const CONFIRM_WINDOW: Duration = Duration::from_secs(3);

/// 同一个会话最多试几次。
const ATTEMPTS: u32 = 3;

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// 交接过程落盘（`boot.log`）。开机那条路没人盯着，只能事后看文件。
fn log(text: &str) {
    crate::platform::logfile::append("boot.log", text);
}

/// 交接用的全局互斥体。
///
/// 任务里同时挂了「开机时」和「任何用户登录时」两条触发器，两份进程可能撞在
/// 一起。用 `Global\` 前缀：这个锁要**跨会话**，会话 0 里创建的东西在用户会话
/// 里看不见，普通命名空间等于没加锁。
struct BootGuard(HANDLE);

impl BootGuard {
    fn acquire() -> Option<Self> {
        let name = wide(r"Global\Bolume.BootHandoff");
        let handle = unsafe { CreateMutexW(None, false, PCWSTR(name.as_ptr())) }.ok()?;
        if unsafe { WaitForSingleObject(handle, 0) } == WAIT_OBJECT_0 {
            Some(Self(handle))
        } else {
            unsafe {
                let _ = CloseHandle(handle);
            }
            None
        }
    }
}

impl Drop for BootGuard {
    fn drop(&mut self) {
        unsafe {
            let _ = ReleaseMutex(self.0);
            let _ = CloseHandle(self.0);
        }
    }
}

/// `--boot-handoff` 的入口：把主程序送进用户会话，然后退场。
///
/// `admin` 来自任务动作里附带的 `--elevate`（就是设置页那个「启动时以管理员运行」），
/// 不从配置文件读 —— 会话 0 里的 SYSTEM 进程不该去碰用户的配置。
pub fn handoff(admin: bool) {
    // 另一份交接正在跑：直接退场，别送第二个实例进去（第二个会被单实例挡掉，
    // 顺手把面板弹出来 —— 开机弹面板是最糟的行为）。
    let Some(_guard) = BootGuard::acquire() else {
        log("另一份交接正在跑，退场");
        return;
    };
    log(&format!("开始交接（admin={admin}）"));

    let deadline = Instant::now() + WAIT_LIMIT;
    let mut tries: std::collections::HashMap<u32, u32> = std::collections::HashMap::new();
    // explorer 第一次出现的时刻 + 稳定期。
    let mut ready_at: Option<Instant> = None;

    loop {
        let sessions = active_sessions();
        let mut pending = false;
        for session in &sessions {
            let session = *session;
            // 会话里已经有界面层在跑：什么都不用做。
            if session_has_app(session) {
                log(&format!("会话 {session} 已有实例，退场"));
                return;
            }
            let count = tries.get(&session).copied().unwrap_or(0);
            if count >= ATTEMPTS {
                continue;
            }
            // explorer 在了才说明通知区域建好了；没起来就继续等。
            if shell_pid(session).is_none() {
                continue;
            }
            pending = true;
            let ready = *ready_at.get_or_insert_with(|| Instant::now() + SETTLE);
            if Instant::now() < ready {
                continue;
            }
            let Some(token) = session_token(session, admin) else {
                log(&format!("会话 {session} 拿不到令牌"));
                continue;
            };
            let pid = launch_in_session(token);
            unsafe {
                let _ = CloseHandle(token);
            }
            let Some(pid) = pid else {
                log(&format!(
                    "会话 {session} 第 {} 次送入失败（CreateProcessAsUser）",
                    count + 1
                ));
                tries.insert(session, count + 1);
                continue;
            };
            if launch_succeeded(pid, session, ALIVE_WINDOW) {
                log(&format!("会话 {session} 起好了（pid {pid}）"));
                return;
            }
            log(&format!(
                "会话 {session} 第 {} 次送进去的进程（pid {pid}）没撑住，准备重试",
                count + 1
            ));
            tries.insert(session, count + 1);
        }
        // 有会话但都试满了：再等下去也没意义。
        if !pending && !sessions.is_empty() {
            log("所有会话都试满了，退场");
            return;
        }
        if Instant::now() >= deadline {
            log("等到上限也没成功，退场");
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

/// 那个会话里是不是已经有界面层在跑。
///
/// 不能用 `SingleInstance::is_running`：那个互斥体没有 `Global\` 前缀，在会话 0
/// 里查的是会话 0 的命名空间，永远查不到用户会话里的实例（这就是「登录时那条
/// 触发器又送了一份进去」的由来）。这里直接按进程名列，顺便带上会话号比对。
fn session_has_app(session: u32) -> bool {
    unsafe {
        let Ok(snapshot) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else {
            return false;
        };
        let mut entry = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        let mut found = false;
        let mut more = Process32FirstW(snapshot, &mut entry).is_ok();
        while more {
            let name = String::from_utf16_lossy(&entry.szExeFile);
            if name.trim_end_matches('\0').eq_ignore_ascii_case("Bolume.exe") {
                let mut id = 0u32;
                if ProcessIdToSessionId(entry.th32ProcessID, &mut id).is_ok() && id == session {
                    found = true;
                    break;
                }
            }
            more = Process32NextW(snapshot, &mut entry).is_ok();
        }
        let _ = CloseHandle(snapshot);
        found
    }
}

/// 盯一会儿：还在跑就是真起来了。
fn wait_alive(pid: u32, window: Duration) -> bool {
    unsafe {
        let Ok(process) = OpenProcess(PROCESS_SYNCHRONIZE, false, pid) else {
            return false;
        };
        let waited = WaitForSingleObject(process, window.as_millis() as u32);
        let _ = CloseHandle(process);
        waited == WAIT_TIMEOUT
    }
}

/// 等会话里出现界面层。
fn wait_app(session: u32, window: Duration) -> bool {
    let deadline = Instant::now() + window;
    loop {
        if session_has_app(session) {
            // 刚看到还不够：开机那阵它可能刚起来就退，稳一下再看一眼。
            std::thread::sleep(Duration::from_millis(600));
            return session_has_app(session);
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(60));
    }
}

/// 送进去的那一份到底起来了没。
///
/// 主程序启动时会先脱一层壳（`platform::detach`：父进程不是系统壳就重新拉起一份
/// 自己、原进程退出）。所以「刚送进去的 pid 马上退出」**不一定是失败**，要看它
/// 是不是把接力棒交给了 `--detached` 副本 —— 早先直接把退出当失败，日志里那句
/// 「很快退出，准备重试」其实每次开机都会误报一遍。
fn launch_succeeded(pid: u32, session: u32, window: Duration) -> bool {
    wait_alive(pid, window) || wait_app(session, CONFIRM_WINDOW)
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

/// 用那条令牌在它的会话桌面上起一份主程序，返回新进程的 PID。
fn launch_in_session(token: HANDLE) -> Option<u32> {
    let Ok(exe) = std::env::current_exe() else {
        return None;
    };
    let app = wide(&exe.display().to_string());
    let mut command: Vec<u16> = format!("\"{}\" {}", exe.display(), crate::platform::detach::DETACHED_FLAG)
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
            let pid = process.dwProcessId;
            let _ = CloseHandle(process.hProcess);
            let _ = CloseHandle(process.hThread);
            return Some(pid);
        }
        None
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
