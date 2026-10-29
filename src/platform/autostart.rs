//! 开机自启动：三种落地方式，同一套对外接口。
//!
//! * **注册表**（`LaunchMethod::RunKey`，默认）：`HKCU\...\CurrentVersion\Run` 里一条
//!   启动项。不需要任何权限，但资源管理器要一路加载到「跑启动项」那一步才轮到它，
//!   托盘图标因此总慢半拍。
//! * **计划任务**（`LaunchMethod::Task`）：登录时触发的任务计划程序任务。会话一建立
//!   就被拉起来（不用等资源管理器把启动项过一遍），是**界面能出现**的最早时机。
//! * **系统启动**（`LaunchMethod::Boot`）：开机触发、以 SYSTEM 身份跑的任务。
//!   它自己不画界面 —— 开机时用户会话还不存在，托盘图标无处可挂。它在会话 0 里
//!   等到用户会话出现，再用**那个会话里 explorer 的令牌**把主程序 CreateProcessAsUser
//!   送进去，然后自己退场。所以拿到的权限和用户自己双击**完全一样**
//!   （UAC 开着的机器就是普通权限，不会凭空提权，也不会弹 UAC）。
//!   注册这条任务要管理员；没有管理员权限时设置页不提供这个选项。
//!
//! 三种方式**只保留一种**：同时存在就是启动两次（第二个会被单实例挡掉，
//! 但白跑一遍、还多一次唤醒已有实例的弹面板）。
//!
//! ## 注册表那条坑（老版本踩过）
//!
//! 两处注册表都要看：
//!
//! * `Run` 里的 `Bolume` 是「登记」——值里写死了当时那个 exe 的完整路径；
//! * `Explorer\StartupApproved\Run` 里的 `Bolume` 是**任务管理器「启动」页那个开关**。
//!   在那里关掉时 Windows 并不删 `Run` 的值，只在这个 12 字节的二进制值里
//!   把首字节改成奇数（启用是 `0x02` / `0x06`，禁用是 `0x03` / `0x07`）。
//!
//! 所以只看 `Run` 有没有值，会把「任务管理器里明确关掉的」当成开着；反过来，
//! 光往 `Run` 里写值也**不会**让一个被关过的启动项重新跑起来。判定和写回都得成对处理。
//!
//! ## 计划任务这边
//!
//! 用 `ITaskFolder::RegisterTask` 直接注册一份 XML —— 跟任务计划程序自己导出的
//! 格式一模一样，字段含义一眼能看懂，也省掉临时文件和 `schtasks.exe` 那个黑框。
//!
//! 几个务必写进 XML 的项：`ExecutionTimeLimit` 必须是 `PT0S`（不写就是 3 天，
//! 常驻程序跑到第 4 天会被任务计划程序掐掉）、`DisallowStartIfOnBatteries` /
//! `StopIfGoingOnBatteries` 关掉（不然笔记本一拔电源就退出）、
//! `MultipleInstancesPolicy` 用 `IgnoreNew`。
//!
//! **为什么不直接用「开机时」去画托盘**：托盘程序必须有桌面会话才看得见 ——
//! 登录前启动只会进会话 0（无桌面、无通知区域、也没有用户的音频设备），
//! 图标永远等不到登录，等于白跑一个进程。所以「系统启动」这一档是
//! **开机时起、登录时接管**：起的是 SYSTEM 的守护，画界面的还是用户会话里的那份。

use windows::core::{BSTR, Interface, PCWSTR};
use windows::Win32::System::Com::{CLSCTX_INPROC_SERVER, CoCreateInstance};
use windows::Win32::System::Registry::{
    HKEY, HKEY_CURRENT_USER, KEY_SET_VALUE, REG_BINARY, REG_OPTION_NON_VOLATILE, REG_SAM_FLAGS,
    REG_SZ, RRF_RT_REG_BINARY, RRF_RT_REG_SZ, RegCloseKey, RegCreateKeyExW, RegDeleteValueW,
    RegGetValueW, RegSetValueExW,
};
use windows::Win32::System::TaskScheduler::{
    IExecAction, ITaskFolder, ITaskService, TASK_CREATE_OR_UPDATE, TASK_LOGON_INTERACTIVE_TOKEN,
    TASK_LOGON_SERVICE_ACCOUNT, TaskScheduler,
};
use windows::Win32::System::Variant::VARIANT;
use windows::Win32::UI::Shell::IsUserAnAdmin;

use crate::config::LaunchMethod;

const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";
const APPROVED_KEY: &str =
    "Software\\Microsoft\\Windows\\CurrentVersion\\Explorer\\StartupApproved\\Run";
const VALUE_NAME: &str = "Bolume";

/// 任务计划程序里那个任务的名字（根目录下）。
///
/// 两种任务分开命名：一个任务只能有一套登录凭据，SYSTEM 的登录任务和普通用户的
/// 登录任务没法共用一条，而且要靠名字区分「现在装的是哪一档」。
const TASK_NAME: &str = "Bolume";
const BOOT_TASK_NAME: &str = "BolumeBoot";

/// SYSTEM 账户（本机账号，任何语言版本都一样）。
const SYSTEM_SID: &str = "S-1-5-18";

/// 系统启动那份副本被任务计划程序拉起时带上的参数：它只做「把自己送进用户会话」。
pub const BOOT_HANDOFF_FLAG: &str = "--boot-handoff";

/// 任务管理器里「启用」时写的 12 个字节（首字节偶数 = 启用）。
const APPROVED_ON: [u8; 12] = [0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0];

fn wide(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

// ───────────────────────────── 注册表方式 ─────────────────────────────

/// 打开（必要时创建）一个要写的子键。
fn open_key(path: &str, access: REG_SAM_FLAGS) -> Option<HKEY> {
    let subkey = wide(path);
    let mut handle = HKEY::default();
    let status = unsafe {
        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey.as_ptr()),
            None,
            None,
            REG_OPTION_NON_VOLATILE,
            access,
            None,
            &mut handle,
            None,
        )
    };
    status.is_ok().then_some(handle)
}

fn write_string(path: &str, name: &str, text: &str) -> bool {
    let Some(key) = open_key(path, KEY_SET_VALUE) else {
        return false;
    };
    let name = wide(name);
    let data: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let bytes = unsafe {
        std::slice::from_raw_parts(data.as_ptr() as *const u8, data.len() * std::mem::size_of::<u16>())
    };
    let ok = unsafe { RegSetValueExW(key, PCWSTR(name.as_ptr()), None, REG_SZ, Some(bytes)).is_ok() };
    unsafe {
        let _ = RegCloseKey(key);
    }
    ok
}

fn write_binary(path: &str, name: &str, data: &[u8]) -> bool {
    let Some(key) = open_key(path, KEY_SET_VALUE) else {
        return false;
    };
    let name = wide(name);
    let ok = unsafe { RegSetValueExW(key, PCWSTR(name.as_ptr()), None, REG_BINARY, Some(data)).is_ok() };
    unsafe {
        let _ = RegCloseKey(key);
    }
    ok
}

fn remove_value(path: &str, name: &str) -> bool {
    let Some(key) = open_key(path, KEY_SET_VALUE) else {
        return false;
    };
    let name = wide(name);
    let ok = unsafe { RegDeleteValueW(key, PCWSTR(name.as_ptr())).is_ok() };
    unsafe {
        let _ = RegCloseKey(key);
    }
    ok
}

/// `Run` 里登记的那条命令（原样，含引号）；没登记返回 `None`。
pub fn registered_command() -> Option<String> {
    let subkey = wide(RUN_KEY);
    let name = wide(VALUE_NAME);
    let mut data = [0u16; 1024];
    let mut size = (data.len() * std::mem::size_of::<u16>()) as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey.as_ptr()),
            PCWSTR(name.as_ptr()),
            RRF_RT_REG_SZ,
            None,
            Some(data.as_mut_ptr() as *mut _),
            Some(&mut size),
        )
    };
    if status.is_err() {
        return None;
    }
    let len = data.iter().position(|&c| c == 0).unwrap_or(data.len());
    Some(String::from_utf16_lossy(&data[..len]))
}

/// 任务管理器那个「已禁用」标记的判定：首字节奇数 = 被关掉了。
///
/// 值不存在（`StartupApproved` 没有这一条）按**启用**算 —— 那是 Windows 的默认，
/// 用户没动过任务管理器时就是这种情况。
fn approved_from_bytes(bytes: &[u8]) -> bool {
    match bytes.first() {
        Some(first) => first & 1 == 0,
        None => true,
    }
}

/// 任务管理器「启动」页里 Bolume 是不是启用状态（没被关过）。
pub fn is_approved() -> bool {
    let subkey = wide(APPROVED_KEY);
    let name = wide(VALUE_NAME);
    let mut data = [0u8; 12];
    let mut size = data.len() as u32;
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            PCWSTR(subkey.as_ptr()),
            PCWSTR(name.as_ptr()),
            RRF_RT_REG_BINARY,
            None,
            Some(data.as_mut_ptr() as *mut _),
            Some(&mut size),
        )
    };
    if status.is_err() {
        return true;
    }
    approved_from_bytes(&data[..(size as usize).min(data.len())])
}

/// 注册表这条路**真的**会随登录启动：登记了、并且没在任务管理器里被关掉。
fn run_key_enabled() -> bool {
    registered_command().is_some() && is_approved()
}

/// 「启动时以管理员运行」在注册表这条路里的落地：`Run` 值后面带这个参数，
/// 主程序先 `ShellExecuteW(runas)` 把自己提起来再接手。
///
/// 注册表启动项没法像计划任务那样由系统静默提权 —— UAC 开着的机器上会弹一次确认。
pub const ELEVATE_FLAG: &str = "--elevate";

/// `Run` 里该登记的那条命令。
fn run_key_command(admin: bool) -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let path = format!("\"{}\"", exe.display());
    Some(if admin {
        format!("{path} {ELEVATE_FLAG}")
    } else {
        path
    })
}

/// 让 `Run` 里的路径跟上本进程（便携版挪了目录之后用）。
///
/// 只动路径，不碰任务管理器那个开关 —— 用户关掉的状态要留着。
fn run_key_ensure_path(admin: bool) -> bool {
    let Some(command) = registered_command() else {
        return false;
    };
    let Some(wanted) = run_key_command(admin) else {
        return false;
    };
    if command == wanted {
        return true;
    }
    write_string(RUN_KEY, VALUE_NAME, &wanted)
}

/// 开关注册表那条启动项，返回是否成功。
///
/// 打开时**两个值一起写**：`Run` 里的路径 + 任务管理器那个「启用」标记。
/// 只写前者的话，之前被任务管理器关过的启动项依旧不会跑（这就是「开了没反应」）。
fn run_key_set(enabled: bool, admin: bool) -> bool {
    if enabled {
        let Some(command) = run_key_command(admin) else {
            return false;
        };
        if !write_string(RUN_KEY, VALUE_NAME, &command) {
            return false;
        }
        // 这一步失败也不回滚：只是「任务管理器里关过」的那种情况会继续不启动，
        // 下次打开设置页看到开关是关的，再点一次就好过整条设置被清掉。
        let _ = write_binary(APPROVED_KEY, VALUE_NAME, &APPROVED_ON);
        true
    } else {
        let _ = remove_value(RUN_KEY, VALUE_NAME);
        // 顺手清掉标记：`Run` 那条都没了，留着它只是一个孤儿项
        // （任务管理器列表里点不回来，老版本遗留过一条 `BolumeOsdGuard`）。
        let _ = remove_value(APPROVED_KEY, VALUE_NAME);
        registered_command().is_none()
    }
}

// ───────────────────────────── 计划任务方式 ─────────────────────────────

/// 在当前线程初始化 MTA 套间（任务计划程序是纯 COM，不是 WinRT）。
fn com() -> crate::audio::com::ComApartment {
    crate::audio::com::ComApartment::init_mta()
}

fn task_service() -> Option<ITaskService> {
    let service: ITaskService =
        unsafe { CoCreateInstance(&TaskScheduler, None, CLSCTX_INPROC_SERVER).ok()? };
    unsafe {
        service
            .Connect(
                &VARIANT::default(),
                &VARIANT::default(),
                &VARIANT::default(),
                &VARIANT::default(),
            )
            .ok()?;
    }
    Some(service)
}

fn root_folder(service: &ITaskService) -> Option<ITaskFolder> {
    unsafe { service.GetFolder(&BSTR::from("\\")).ok() }
}

/// 某一档方式对应的任务名（注册表那条没有任务，落到默认名上，不会用来查）。
fn task_name(method: LaunchMethod) -> &'static str {
    if method == LaunchMethod::Boot {
        BOOT_TASK_NAME
    } else {
        TASK_NAME
    }
}

fn task_path(method: LaunchMethod) -> BSTR {
    BSTR::from(format!("\\{}", task_name(method)))
}

/// 任务在不在（不管启用状态）。
fn task_exists(method: LaunchMethod) -> bool {
    let _apartment = com();
    let Some(service) = task_service() else {
        return false;
    };
    let Some(folder) = root_folder(&service) else {
        return false;
    };
    unsafe { folder.GetTask(&task_path(method)).is_ok() }
}

/// 任务注册的启动命令（exe 路径，不带引号）；没有任务返回 `None`。
fn task_command(method: LaunchMethod) -> Option<String> {
    let _apartment = com();
    let service = task_service()?;
    let folder = root_folder(&service)?;
    let task = unsafe { folder.GetTask(&task_path(method)) }.ok()?;
    let actions = unsafe { task.Definition().ok()?.Actions().ok()? };
    let action = unsafe { actions.get_Item(1).ok()? };
    let exec: IExecAction = action.cast().ok()?;
    let mut path = BSTR::new();
    unsafe { exec.Path(&mut path).ok()? };
    Some(path.to_string())
}

/// 任务存在、而且没在任务计划程序里被停用。
fn task_enabled(method: LaunchMethod) -> bool {
    let _apartment = com();
    let Some(service) = task_service() else {
        return false;
    };
    let Some(folder) = root_folder(&service) else {
        return false;
    };
    let Ok(task) = (unsafe { folder.GetTask(&task_path(method)) }) else {
        return false;
    };
    unsafe { task.Enabled() }
        .map(|flag| flag.as_bool())
        .unwrap_or(false)
}

/// 当前登录用户的 `域\用户名`（任务要绑到具体的人身上）。
///
/// 取不到就返回 `None`：XML 里省掉 `<UserId>`，任务计划程序会按注册调用者
/// 的令牌来算，也不至于整个注册失败。
fn current_user() -> Option<String> {
    let user = std::env::var("USERNAME").ok().filter(|name| !name.is_empty())?;
    let domain = std::env::var("USERDOMAIN")
        .or_else(|_| std::env::var("COMPUTERNAME"))
        .ok()
        .filter(|name| !name.is_empty())?;
    Some(format!("{domain}\\{user}"))
}

fn xml_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for ch in text.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            _ => out.push(ch),
        }
    }
    out
}

/// 任务定义（跟任务计划程序导出的格式一致）。
///
/// * `LaunchMethod::Task`：登录触发，跑在**当前用户**的交互令牌下；
///   `admin` 决定 `RunLevel` —— 要管理员就直接让任务计划程序以最高权限拉起
///   （静默，不弹 UAC）。
/// * `LaunchMethod::Boot`：开机触发 + 登录触发，以 **SYSTEM** 跑，动作带
///   `--boot-handoff`（它自己再把人送进用户会话）。
///
/// `user` 为空就省掉 `<UserId>`。
fn task_xml(exe: &std::path::Path, method: LaunchMethod, admin: bool, user: Option<&str>) -> String {
    let command = xml_escape(&exe.display().to_string());
    let workdir = exe
        .parent()
        .map(|dir| xml_escape(&dir.display().to_string()))
        .unwrap_or_default();
    let user_element = user
        .map(|user| format!("<UserId>{}</UserId>", xml_escape(user)))
        .unwrap_or_default();
    let mine = || {
        format!("    <LogonTrigger>\n      <Enabled>true</Enabled>{user_element}\n    </LogonTrigger>")
    };
    let anyone = || "    <LogonTrigger>\n      <Enabled>true</Enabled>\n    </LogonTrigger>".to_string();
    let (description, triggers, arguments) = match method {
        LaunchMethod::Boot => (
            "Bolume - launch at system startup",
            format!(
                "    <BootTrigger>\n      <Enabled>true</Enabled>\n    </BootTrigger>\n{}",
                anyone()
            ),
            format!(
                "      <Arguments>{BOOT_HANDOFF_FLAG}{}</Arguments>\n",
                if admin { format!(" {ELEVATE_FLAG}") } else { String::new() }
            ),
        ),
        _ => ("Bolume - launch at sign-in", mine(), String::new()),
    };
    // SYSTEM 身份**不能**写 `<LogonType>` —— 写了任务计划程序直接判「值超出范围」
    // （0x80041318，报的就是这一行）。只给 `<UserId>` 时它会自己认出服务账户。
    let principal = if method == LaunchMethod::Boot {
        String::new()
    } else {
        format!(
            "\n      <LogonType>InteractiveToken</LogonType>\n      <RunLevel>{}</RunLevel>",
            if admin { "HighestAvailable" } else { "LeastPrivilege" }
        )
    };
    // 系统启动那一档挂了「开机时」和「任何用户登录时」两条触发器，**必须允许并发**：
    // 开机那份可能已经把进程送进会话又退场了（送进去的进程自己退了），也可能卡在
    // 等待里，登录这一刻得能再起一份交接去补。交接内部用 `Global\` 互斥体保证同时
    // 只有一份在干活，所以不会送两个实例进会话。登录那一档只有一条触发器，照旧
    // 用 `IgnoreNew` 挡住重复点击。
    let instances = if method == LaunchMethod::Boot {
        "Parallel"
    } else {
        "IgnoreNew"
    };
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.4" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Author>Bolume</Author>
    <Description>{description}</Description>
  </RegistrationInfo>
  <Triggers>
{triggers}
  </Triggers>
  <Principals>
    <Principal id="Author">{user_element}{principal}
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>{instances}</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>false</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>7</Priority>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>{command}</Command>
{arguments}      <WorkingDirectory>{workdir}</WorkingDirectory>
    </Exec>
  </Actions>
</Task>
"#
    )
}

/// 造一个 BSTR 类型的 `VARIANT`（`RegisterTask` 的 `userid` 要它）。
///
/// 结构体字面量拼，别用字段赋值：`VARIANT` 里套了三层 union，字段赋值会走
/// `DerefMut`、顺手把旧值析构掉，编译器直接不让。
fn variant_bstr(text: &str) -> VARIANT {
    use windows::Win32::System::Variant::{VT_BSTR, VARIANT_0, VARIANT_0_0, VARIANT_0_0_0};
    VARIANT {
        Anonymous: VARIANT_0 {
            Anonymous: std::mem::ManuallyDrop::new(VARIANT_0_0 {
                vt: VT_BSTR,
                wReserved1: 0,
                wReserved2: 0,
                wReserved3: 0,
                Anonymous: VARIANT_0_0_0 {
                    bstrVal: std::mem::ManuallyDrop::new(BSTR::from(text)),
                },
            }),
        },
    }
}

/// 注册（或覆盖）任务，返回是否成功。
///
/// 系统启动那一档要管理员权限：非提权时 `RegisterTask` 直接被拒（`E_ACCESSDENIED`），
/// 调用方据此提示「需要管理员权限」。
fn task_register(method: LaunchMethod, admin: bool) -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    let _apartment = com();
    let Some(service) = task_service() else {
        return false;
    };
    let Some(folder) = root_folder(&service) else {
        return false;
    };
    let (xml, logon, user) = match method {
        LaunchMethod::Boot => (
            task_xml(&exe, method, admin, Some(SYSTEM_SID)),
            TASK_LOGON_SERVICE_ACCOUNT,
            variant_bstr(SYSTEM_SID),
        ),
        _ => (
            task_xml(&exe, method, admin, current_user().as_deref()),
            TASK_LOGON_INTERACTIVE_TOKEN,
            VARIANT::default(),
        ),
    };
    let result = unsafe {
        folder.RegisterTask(
            &task_path(method),
            &BSTR::from(xml.as_str()),
            TASK_CREATE_OR_UPDATE.0,
            &user,
            &VARIANT::default(),
            logon,
            &VARIANT::default(),
        )
    };
    if let Err(error) = &result {
        // 注册失败基本只有两种：权限不够（非提权时注册 SYSTEM 任务）、XML 被拒。
        // 设置页只显示「需要管理员权限」那一句，这里把系统原话留给调试。
        eprintln!("注册计划任务失败（{method:?}）：{error}");
    }
    result.is_ok()
}

/// 删除任务，返回删干净的确认。
fn task_delete(method: LaunchMethod) -> bool {
    let _apartment = com();
    let Some(service) = task_service() else {
        return false;
    };
    let Some(folder) = root_folder(&service) else {
        return false;
    };
    let _ = unsafe { folder.DeleteTask(&task_path(method), 0) };
    !task_exists(method)
}

fn task_ensure_path(method: LaunchMethod, admin: bool) -> bool {
    let Some(command) = task_command(method) else {
        return false;
    };
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    if command.eq_ignore_ascii_case(&exe.display().to_string()) {
        return true;
    }
    task_register(method, admin)
}

fn task_set(method: LaunchMethod, enabled: bool, admin: bool) -> bool {
    if enabled {
        task_register(method, admin) && task_enabled(method)
    } else {
        task_delete(method)
    }
}

// ───────────────────────────── 对外接口 ─────────────────────────────

/// 上一次「需要管理员权限」的操作是不是被拒了（设置页据此提示一句）。
///
/// 每次 `set_enabled` 开头都会清掉，只有真的失败了才重新置位 —— 用户换回不需要
/// 管理员的方式之后提示自动消失。
static ADMIN_DENIED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// 当前进程是不是提过权的。
///
/// UAC 关掉 / 全局管理员放行的机器上永远为真；UAC 开着的机器上普通双击运行
/// 就是假 —— 系统启动那一档和「启动时以管理员运行」都要在这个前提下判。
pub fn is_elevated() -> bool {
    unsafe { IsUserAnAdmin().as_bool() }
}

/// 上一次「需要管理员权限」的操作是不是被拒了（设置页据此提示一句）。
///
/// 每次 `set_enabled` 开头都会清掉，只有真的失败了才重新置位 —— 用户换回不需要
/// 管理员的方式之后提示自动消失。
///
/// 调试：`BOLUME_ADMIN_DENIED=1` 强行让它为真，不用真去关 UAC 也能看那句提示的排版。
pub fn admin_denied() -> bool {
    ADMIN_DENIED.load(std::sync::atomic::Ordering::Relaxed)
        || std::env::var_os("BOLUME_ADMIN_DENIED").is_some()
}

/// 这一档在系统里有没有登记（不管启用状态）。
fn exists(method: LaunchMethod) -> bool {
    match method {
        LaunchMethod::RunKey => registered_command().is_some(),
        other => task_exists(other),
    }
}

/// 现在**实际**登记在哪一档（都没有就 `None`）。
///
/// 设置页拿它当「当前方式」显示：用户如果在任务计划程序里手动删了任务，
/// 页面就该如实显示注册表那一套，而不是固执地按配置里记的那个画。
///
/// 判定顺序是「**真的会跑**的优先」，同档优先里再取最早的触发时机：
/// 任务被停用、注册表那条还在的时候，实际生效的是注册表 —— 这时候要报注册表，
/// 不能让页面显示一个停用的任务。
pub fn detected() -> Option<LaunchMethod> {
    LaunchMethod::ALL
        .into_iter()
        .find(|method| is_enabled(*method))
        .or_else(|| LaunchMethod::ALL.into_iter().find(|method| exists(*method)))
}

/// 指定方式下，是不是**真的**会随登录启动。
pub fn is_enabled(method: LaunchMethod) -> bool {
    match method {
        LaunchMethod::RunKey => run_key_enabled(),
        other => task_enabled(other),
    }
}

/// 只撤这一档，别的不动（清理多余登记用）。
pub fn remove(method: LaunchMethod) {
    match method {
        LaunchMethod::RunKey => {
            let _ = run_key_set(false, false);
        }
        other => {
            let _ = task_delete(other);
        }
    }
}

/// 开关指定方式的启动项，返回是否成功。
///
/// **三档互斥**，这里是最权威的一处：
///
/// * 开 → 先把另外两档撤掉再建这一种。顺序反了会在中间那一下出现两条同时在，
///   登录时启动两次（第二次虽然被单实例挡掉，但会平白弹一下面板）。
/// * 关 → 三档一起撤。开关叫「开机自动启动」，关掉就该一条都不剩；
///   就算之前手工弄出过两条，也不能留一条偷偷跑。
///
/// `admin` = 用户开了「启动时以管理员运行」，注册进任务的是不是最高权限。
pub fn set_enabled(method: LaunchMethod, enabled: bool, admin: bool) -> bool {
    ADMIN_DENIED.store(false, std::sync::atomic::Ordering::Relaxed);
    let ok = if enabled {
        for other in LaunchMethod::ALL {
            if other != method {
                remove(other);
            }
        }
        match method {
            LaunchMethod::RunKey => run_key_set(true, admin),
            other => task_set(other, true, admin),
        }
    } else {
        for other in LaunchMethod::ALL {
            remove(other);
        }
        !LaunchMethod::ALL.into_iter().any(exists)
    };
    // 失败 + 当前没提权 = 多半就是权限不够（任务计划程序直接 E_ACCESSDENIED）。
    if !ok && !is_elevated() {
        ADMIN_DENIED.store(true, std::sync::atomic::Ordering::Relaxed);
    }
    ok
}

/// 把已登记的路径（和权限）改成本进程的（便携版挪了目录、改了提权选项之后用）。
pub fn ensure_path(method: LaunchMethod, admin: bool) -> bool {
    match method {
        LaunchMethod::RunKey => run_key_ensure_path(admin),
        other => task_ensure_path(other, admin),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        BOOT_HANDOFF_FLAG, ELEVATE_FLAG, LaunchMethod, approved_from_bytes, task_xml, xml_escape,
    };
    use std::path::Path;

    /// 任务管理器写的第一字节：偶数启用（02 / 06）、奇数禁用（03 / 07）。
    ///
    /// 只看偶奇、不看具体值 —— 用户重开过一次之后 Windows 会把 02 写成 06，
    /// 按等值判会漏掉这种「重新启用」。
    #[test]
    fn approved_byte_decides_run_at_login() {
        assert!(approved_from_bytes(&[0x02, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]));
        assert!(approved_from_bytes(&[0x06, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]));
        assert!(!approved_from_bytes(&[0x03, 0, 0, 0, 0, 0x85, 0x89, 0x60, 0x1B, 0xB0, 0x45, 0xDD]));
        assert!(!approved_from_bytes(&[0x07, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]));
    }

    /// 没有这一条（用户没动过任务管理器）= 启用，不是禁用。
    #[test]
    fn missing_marker_means_enabled() {
        assert!(approved_from_bytes(&[]));
    }

    /// 任务定义得是「登录时触发 + 只跑一个 exe」，而且不能带那三个默认的坑：
    /// 3 天执行上限、电池上不启动、拔电源就退出。
    #[test]
    fn task_xml_is_a_logon_launch() {
        let xml = task_xml(
            Path::new(r"C:\Apps\Bolume\Bolume.exe"),
            LaunchMethod::Task,
            false,
            Some(r"PC\tick"),
        );
        assert!(xml.starts_with(r#"<?xml version="1.0" encoding="UTF-16"?>"#));
        assert!(xml.contains("<LogonTrigger>"));
        assert!(xml.contains("<LogonType>InteractiveToken</LogonType>"));
        assert!(xml.contains("<RunLevel>LeastPrivilege</RunLevel>"));
        assert!(!xml.contains(BOOT_HANDOFF_FLAG));
        assert!(xml.contains("<Command>C:\\Apps\\Bolume\\Bolume.exe</Command>"));
        assert!(xml.contains("<WorkingDirectory>C:\\Apps\\Bolume</WorkingDirectory>"));
        // 常驻程序：不设 `PT0S` 就会被任务计划程序按默认的 3 天掐掉。
        assert!(xml.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"));
        assert!(xml.contains("<DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>"));
        assert!(xml.contains("<StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>"));
        assert!(xml.contains("<MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>"));
        // 绑到具体用户，避免多用户机器上给别人也起一份。
        assert_eq!(xml.matches(r"<UserId>PC\tick</UserId>").count(), 2);
    }

    /// 认不出用户时省掉 `<UserId>`（任务计划程序按调用者令牌算），而不是留个空标签。
    #[test]
    fn task_xml_skips_user_when_unknown() {
        let xml = task_xml(Path::new(r"C:\Bolume.exe"), LaunchMethod::Task, false, None);
        assert!(!xml.contains("<UserId>"));
        assert!(xml.contains("<Command>C:\\Bolume.exe</Command>"));
    }

    /// 路径里带 `&` / `<` 的话必须转义，否则整份 XML 解析失败、注册直接没了。
    #[test]
    fn task_xml_escapes_paths() {
        assert_eq!(xml_escape("a&b<c"), "a&amp;b&lt;c");
        let xml = task_xml(Path::new(r"C:\A&B\Bolume.exe"), LaunchMethod::Task, false, None);
        assert!(xml.contains("<Command>C:\\A&amp;B\\Bolume.exe</Command>"));
        assert!(!xml.contains("A&B"));
    }

    /// 开了「启动时以管理员运行」，登录任务要写成最高权限 ——
    /// 由任务计划程序静默拉起，不弹 UAC。
    #[test]
    fn task_xml_raises_run_level_for_admin() {
        let xml = task_xml(Path::new(r"C:\Bolume.exe"), LaunchMethod::Task, true, None);
        assert!(xml.contains("<RunLevel>HighestAvailable</RunLevel>"));
        assert!(!xml.contains("<RunLevel>LeastPrivilege</RunLevel>"));
    }

    /// 系统启动那一档：开机触发 + SYSTEM + 动作带交接参数；
    /// 登录触发器不能限定用户（谁登录都得由它送进去）。
    #[test]
    fn task_xml_is_a_boot_handoff() {
        let xml = task_xml(
            Path::new(r"C:\Bolume.exe"),
            LaunchMethod::Boot,
            false,
            Some("S-1-5-18"),
        );
        assert!(xml.contains("<BootTrigger>"));
        assert!(xml.contains("<UserId>S-1-5-18</UserId>"));
        // SYSTEM 身份不能写 `<LogonType>`：写了任务计划程序判「值超出范围」直接拒收。
        assert!(!xml.contains("<LogonType>"));
        assert!(xml.contains(&format!("<Arguments>{BOOT_HANDOFF_FLAG}</Arguments>")));
        assert!(!xml.contains(ELEVATE_FLAG));
        assert_eq!(xml.matches("<LogonTrigger>").count(), 1);
        assert_eq!(xml.matches("<UserId>").count(), 1);
    }

    /// 系统启动 + 管理员运行：交接参数后面再带上 `--elevate`，由交接那边改用完整令牌。
    #[test]
    fn task_xml_boot_passes_elevate_through() {
        let xml = task_xml(
            Path::new(r"C:\Bolume.exe"),
            LaunchMethod::Boot,
            true,
            Some("S-1-5-18"),
        );
        assert!(xml.contains(&format!("<Arguments>{BOOT_HANDOFF_FLAG} {ELEVATE_FLAG}</Arguments>")));
    }
}
