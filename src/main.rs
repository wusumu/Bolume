#![cfg_attr(
    all(target_os = "windows", not(debug_assertions)),
    windows_subsystem = "windows"
)]

mod app_paths;
mod audio;
mod config;
mod display;
mod i18n;
mod motion;
mod platform;
mod probe;
mod profiles;
mod startup;
mod textvars;
mod tray;
mod ui;
mod windows;

#[cfg(feature = "gui")]
mod gui;

/// 界面上显示的版本号。
///
/// Cargo 的版本必须写三段（`0.41.0`），界面上只留两段（`0.41`）。
/// 版本按 0.01 一档往上走：0.41 → 0.49 → 0.50（不是 0.4.10）。
pub fn version() -> &'static str {
    const RAW: &str = env!("CARGO_PKG_VERSION");
    match RAW.strip_suffix(".0") {
        Some(short) => short,
        None => RAW,
    }
}

/// 临时调试用的极简日志器：把 gpui 内部的 log 输出打到 stderr。
struct StderrLogger;

impl log::Log for StderrLogger {
    fn enabled(&self, _metadata: &log::Metadata) -> bool {
        true
    }

    fn log(&self, record: &log::Record) {
        eprintln!("[{} {}] {}", record.level(), record.target(), record.args());
    }

    fn flush(&self) {}
}

/// panic 落盘。
///
/// release 编的是 `panic = "abort"`：一个 panic 就是进程直接消失，没有控制台、
/// 没有对话框、事件日志里只有一句 `0xc0000409`。开机自启那条路上的表现就是
/// **「任务跑了，但程序没起来」** —— 光看任务计划完全查不出来。这里把消息、
/// 位置和调用栈追加进 `crash.log`，事后至少能指到哪一行。
fn install_panic_log() {
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        let mut text = String::from("panic: ");
        let payload = info.payload();
        if let Some(message) = payload.downcast_ref::<&str>() {
            text.push_str(message);
        } else if let Some(message) = payload.downcast_ref::<String>() {
            text.push_str(message);
        } else {
            text.push_str("(非字符串 payload)");
        }
        match info.location() {
            Some(at) => text.push_str(&format!(" @ {}:{}:{}", at.file(), at.line(), at.column())),
            None => text.push_str(" @ (未知位置)"),
        }
        if let Some(name) = std::thread::current().name() {
            text.push_str(&format!("\n  thread: {name}"));
        }
        let args: Vec<String> = std::env::args().skip(1).collect();
        text.push_str(&format!("\n  args: {}", args.join(" ")));
        // 顺手记一行「开机多久了」：`Instant` 的起点就是系统启动那一刻，开机头几秒
        // 内任何 `Instant::now() - 十几秒` 都会 panic，有这一行一眼能对上号。
        text.push_str(&format!(
            "\n  uptime: {:?}",
            std::time::Instant::now()
        ));
        // 不再写调用栈：release 是 `strip = "symbols"`，栈里全是 `<unknown>`，
        // 一行位置（`文件:行:列`）反而更直接。
        platform::logfile::append("crash.log", &text);
        // 图形设备起不来时，用户看到的是「双击没反应」——什么提示都没有。
        // 这一种给个框，理由直接写在脸上（虚拟机没装显卡增强驱动最常见）。
        if text.contains("failed to initialize Windows platform") {
            startup_failure_box(&text);
        }
        previous(info);
    }));
}

/// 启动阶段（图形设备初始化）失败时的提示框。
///
/// 这是进程启动的第一个动作，界面上没有任何东西可以显示，日志又要用户自己去
/// exe 目录翻，所以只能弹框。只在 `failed to initialize Windows platform` 这一种
/// 上触发 —— 运行中途的 panic 不弹，免得用户回来发现程序卡在一个框上。
fn startup_failure_box(detail: &str) {
    // 注意：本项目自己也有一层叫 `windows` 的模块（设置页那一堆），
    // 这里必须写 `::windows` 才是 crate。
    use ::windows::Win32::UI::WindowsAndMessaging::{
        MB_ICONERROR, MB_OK, MB_SETFOREGROUND, MessageBoxW,
    };
    use ::windows::core::PCWSTR;

    let text = format!(
        "Bolume 需要一个能用的 DirectX 11 图形设备才能画出界面，这台机器上初始化失败了。\n\n\
         常见原因：虚拟机没装显卡增强驱动（VMware Tools / VirtualBox 增强功能），\
         或者显卡驱动异常。装好驱动再启动一次通常就能解决。\n\n\
         详细信息已写进 exe 旁边的 crash.log。\n\n{detail}"
    );
    let text: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let caption: Vec<u16> = "Bolume 启动失败".encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        MessageBoxW(
            None,
            PCWSTR(text.as_ptr()),
            PCWSTR(caption.as_ptr()),
            MB_OK | MB_ICONERROR | MB_SETFOREGROUND,
        );
    }
}

fn main() {
    install_panic_log();

    // 调试用：确认崩溃日志这条链路真的在写（`BOLUME_PANIC_TEST=1`）。
    if std::env::var_os("BOLUME_PANIC_TEST").is_some() {
        panic!("panic log probe");
    }

    if std::env::var_os("BOLUME_LOG").is_some() {
        let _ = log::set_boxed_logger(Box::new(StderrLogger));
        log::set_max_level(log::LevelFilter::Debug);
    }

    let args: Vec<String> = std::env::args().skip(1).collect();

    // 显示器诊断：给用户一个「双击就出报告」的包 —— 把 exe 改名成 `BolumeDiag.exe`
    // 就是诊断工具，不用教他敲命令行参数（`--diag-displays` 也一样）。
    let diag_by_name = std::env::current_exe()
        .ok()
        .and_then(|exe| {
            exe.file_stem()
                .map(|stem| stem.to_string_lossy().to_lowercase())
        })
        .is_some_and(|stem| stem.starts_with("bolumediag"));
    if diag_by_name || args.iter().any(|arg| arg == "--diag-displays") {
        probe::run_diag();
        return;
    }

    // 系统启动那一档：任务是开机触发、SYSTEM 身份跑的。那个进程不画界面，
    // 只等用户会话出现、再把自己送进去（详见 `platform::bootstart`）。
    if args
        .iter()
        .any(|arg| arg == platform::autostart::BOOT_HANDOFF_FLAG)
    {
        platform::bootstart::handoff(
            args.iter()
                .any(|arg| arg == platform::autostart::ELEVATE_FLAG),
        );
        return;
    }

    // 「启动时以管理员运行」在注册表那条路上的落地：Run 值里带 `--elevate`，
    // 这里先 `runas` 拉起提权后的自己，再退场。
    if args
        .iter()
        .any(|arg| arg == platform::autostart::ELEVATE_FLAG)
        && !platform::autostart::is_elevated()
    {
        platform::bootstart::elevate_self();
        return;
    }

    if let Some(index) = args.iter().position(|arg| arg == "--icon") {
        if let Some(path) = args.get(index + 1) {
            let pixels = args
                .get(index + 2)
                .and_then(|value| value.parse::<u32>().ok())
                .unwrap_or(64);
            probe::run_icon(path, pixels);
            return;
        }
    }

    if let Some(index) = args.iter().position(|arg| arg == "--tray-motion") {
        // 调试用：把律动的帧序列画成图（不接管托盘、不碰音量）。
        let dir = args.get(index + 1).map(|v| v.as_str()).unwrap_or(".");
        probe::run_motion(dir);
        return;
    }

    if args.iter().any(|arg| arg == "--probe-media") {
        probe::run_media();
        return;
    }

    if args.iter().any(|arg| arg == "--probe-bt") {
        probe::run_bt();
        return;
    }

    if let Some(index) = args.iter().position(|arg| arg == "--probe-block") {
        // 可选一个 pid：不给就看当前前台窗口。
        let pid = args
            .get(index + 1)
            .and_then(|value| value.parse::<u32>().ok());
        probe::run_block(pid);
        return;
    }

    if let Some(index) = args.iter().position(|arg| arg == "--probe-appdev") {
        // 用法：--probe-appdev            列出每个应用被指到了哪台设备
        //       --probe-appdev <pid> <设备id|default>
        let pid = args
            .get(index + 1)
            .and_then(|value| value.parse::<u32>().ok());
        let device = args.get(index + 2).map(|value| value.as_str());
        probe::run_appdev(pid, device);
        return;
    }

    if let Some(index) = args.iter().position(|arg| arg == "--probe-sound-watch") {
        let seconds = args
            .get(index + 1)
            .and_then(|value| value.parse::<f32>().ok())
            .unwrap_or(5.0);
        probe::run_sound_watch(seconds);
        return;
    }

    if let Some(index) = args.iter().position(|arg| arg == "--probe-sound") {
        // 用法：--probe-sound [音效名] [音量%]，例如 --probe-sound chime 90
        let style = args.get(index + 1).map(|value| value.as_str());
        let volume = args
            .get(index + 2)
            .and_then(|value| value.parse::<u32>().ok());
        probe::run_sound(style, volume);
        return;
    }

    if let Some(index) = args.iter().position(|arg| arg == "--probe-appvol") {
        // 用法：--probe-appvol <pid> <0~100>
        // 走的就是面板滑块那条命令，用来查「单独调应用音量没反应」。
        let pid = args
            .get(index + 1)
            .and_then(|value| value.parse::<u32>().ok())
            .unwrap_or(0);
        let percent = args
            .get(index + 2)
            .and_then(|value| value.parse::<f32>().ok())
            .unwrap_or(50.0);
        probe::run_appvol(pid, percent);
        return;
    }

    if args.iter().any(|arg| arg == "--probe-osd") {
        // 诊断：系统那扇 OSD 窗口找不找得到、区域设没设上（会静音再取消静音一次）。
        println!("{}", crate::platform::system_osd::diag_report());
        return;
    }

    if let Some(index) = args.iter().position(|arg| arg == "--probe-display") {
        // 用法：--probe-display [轮数] [名字:百分比]
        // 例：--probe-display 3           只读三遍
        //     --probe-display 1 XG279Q:40 再写一次亮度
        let rounds = args
            .get(index + 1)
            .and_then(|value| value.parse::<u32>().ok())
            .unwrap_or(1);
        let write = args.get(index + 2).and_then(|value| {
            let (name, percent) = value.rsplit_once(':')?;
            Some((name, percent.parse::<u32>().ok()?))
        });
        probe::run_display(write, rounds);
        return;
    }

    // 探针：解析后的主题 / 自选色（跟随系统、Windows 强调色）。只读配置，不起窗口。
    #[cfg(feature = "gui")]
    if args.iter().any(|arg| arg == "--probe-theme") {
        probe::run_theme();
        return;
    }

    if let Some(index) = args.iter().position(|arg| arg == "--probe") {
        let watch = args
            .get(index + 1)
            .and_then(|value| value.parse::<u64>().ok());
        probe::run(watch);
        return;
    }

    // 从文件管理器 / 第三方启动器里双击运行时，本进程是它们的子进程。process
    // loopback 按**进程树**抓音频 —— 接管那个父进程会把我们自己的输出也一起抓
    // 进去，形成闭环回授（一段不断重复、辨不出源头的怪音，源停了也不消失）。
    // 先换成独立的父进程再继续，见 `platform::detach`。
    #[cfg(feature = "gui")]
    if platform::detach::needs_detach(&args) {
        if platform::detach::respawn(&args) {
            return;
        }
    }

    #[cfg(feature = "gui")]
    gui::run(
        args.iter().any(|arg| arg == "--show"),
        args.iter().any(|arg| arg == "--settings"),
        args.iter().any(|arg| arg == "--osd-editor"),
        // 托盘菜单「重启」拉起来的副本（内部参数）：先等旧实例退场再接手。
        args.iter()
            .any(|arg| arg == platform::restart::RESTART_WAIT_FLAG),
        // 调试入口：启动即弹一次 OSD（只画卡片，不动设备音量 / 播放状态）。
        args.iter().any(|arg| arg == "--osd-demo"),
        // 同上，但弹媒体键那张（播放 / 暂停）而不是音量那张。
        args.iter().any(|arg| arg == "--osd-demo-media"),
        // 调试：--osd-demo-value 0.03 可以指定音量卡片显示多少（0..1）。
        args.iter()
            .position(|arg| arg == "--osd-demo-value")
            .and_then(|index| args.get(index + 1))
            .and_then(|value| value.parse::<f32>().ok())
            .unwrap_or(0.5),
    );

    #[cfg(not(feature = "gui"))]
    {
        println!("Bolume 音频层已构建（未启用 gui 特性）。");
        println!("用法：Bolume --probe [秒数]");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 界面上要显示两段版本号（`0.41`），不是 Cargo 的三段（`0.41.0`）。
    #[test]
    fn version_is_shown_with_two_segments() {
        let shown = version();
        assert_eq!(shown.matches('.').count(), 1, "显示成 {shown} 了");
        assert!(!shown.ends_with(".0"), "显示成 {shown} 了");
        // 0.49 之后是 0.50：三位数字也得完整保留。
        assert!(shown.starts_with("0."), "显示成 {shown} 了");
    }
}
