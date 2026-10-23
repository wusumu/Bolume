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

fn main() {
    if std::env::var_os("BOLUME_LOG").is_some() {
        let _ = log::set_boxed_logger(Box::new(StderrLogger));
        log::set_max_level(log::LevelFilter::Debug);
    }

    let args: Vec<String> = std::env::args().skip(1).collect();

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
