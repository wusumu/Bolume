//! 无界面探针：只读打印设备与应用音量，用于验证音频层。
//! 绝不修改系统音量。

use std::time::{Duration, Instant};

use crate::{
    app_paths::AppPaths,
    audio::{AudioCommand, AudioEngine, AudioEvent, Snapshot, volume_to_percent},
};

pub fn run_icon(path: &str, pixels: u32) {
    let _apartment = crate::audio::com::ComApartment::init_mta();
    let paths = AppPaths::discover();
    let target = std::path::Path::new(path);
    match crate::audio::icons::ensure_icon(target, &paths.icons_dir(), pixels) {
        Some(cached) => {
            let size = std::fs::metadata(&cached)
                .map(|meta| meta.len())
                .unwrap_or(0);
            let dims = image::image_dimensions(&cached)
                .map(|(w, h)| format!("{w}x{h}"))
                .unwrap_or_else(|_| "?".into());
            println!("icon={} size={dims} bytes={size}", cached.display());
        }
        None => println!("未能提取图标：{path}"),
    }
}

/// 探针：把四种律动的帧序列渲染成一张长图，用来肉眼验收。
///
/// 托盘图标只有 16px，真机上根本看不清形状对不对；这里按 8 倍放大拼成一条胶片，
/// 顺便模拟一段「每 150ms 一记鼓点」的信号（不碰系统音量，纯计算）。
pub fn run_motion(dir: &str) {
    use image::{ImageFormat, RgbaImage};

    const SIDE: u32 = 16;
    const ZOOM: u32 = 8;
    const FRAMES: u32 = 12;

    let _ = std::fs::create_dir_all(dir);
    for effect in [
        crate::config::TrayMotion::Bars,
        crate::config::TrayMotion::Ripple,
        crate::config::TrayMotion::Orbit,
        crate::config::TrayMotion::Pulse,
    ] {
        let mut motion = crate::motion::Motion::default();
        let mut strip = RgbaImage::new(SIDE * ZOOM * FRAMES, SIDE * ZOOM);
        for frame in 0..FRAMES {
            std::thread::sleep(Duration::from_millis(40));
            // 鼓点：每三帧一记，中间是安静段。
            let peak = if frame % 3 == 0 { 0.65 } else { 0.08 };
            let coverage = motion.step(peak, 1.5, effect, SIDE);
            for y in 0..SIDE * ZOOM {
                for x in 0..SIDE * ZOOM {
                    let index = ((y / ZOOM) * SIDE + (x / ZOOM)) as usize;
                    let alpha = coverage.get(index).copied().unwrap_or(0.0);
                    // 深色任务栏底色 + 白色墨迹。
                    let value = (32.0 + (248.0 - 32.0) * alpha).round() as u8;
                    strip.put_pixel(
                        frame * SIDE * ZOOM + x,
                        y,
                        image::Rgba([value, value, value, 255]),
                    );
                }
            }
        }
        let name = match effect {
            crate::config::TrayMotion::Bars => "bars",
            crate::config::TrayMotion::Ripple => "ripple",
            crate::config::TrayMotion::Orbit => "orbit",
            crate::config::TrayMotion::Pulse => "pulse",
        };
        let path = std::path::Path::new(dir).join(format!("{name}.png"));
        match strip.save_with_format(&path, ImageFormat::Png) {
            Ok(()) => println!("{}", path.display()),
            Err(error) => println!("保存失败 {name}: {error}"),
        }
    }
}

/// 探针：打印系统媒体会话（SMTC）报的播放状态。
///
/// 播放 / 暂停共用一个键，提示条全靠它分辨这次是「开始播」还是「停下来」。
/// 屏幕上显示得不对时先跑 `--probe-media`，看系统到底报了什么。
pub fn run_media() {
    println!("playing={:?}", crate::platform::media::is_playing());
}

/// 探针：黑名单判定。没传 pid 就看当前前台窗口。
///
/// 「拖到窗口上却拾到了别的进程」「名单加了还是不生效」先跑这个：
/// 它把名单、前台进程链（自己 + 祖先）、以及最终判定一次全打出来。
pub fn run_block(pid: Option<u32>) {
    let paths = AppPaths::discover();
    let store = crate::config::ConfigStore::load(paths.config_path());
    let config = store.get().clone();
    let block = &config.app_block;
    println!("config={}", paths.config_path().display());
    println!("fullscreen_switch={}", block.fullscreen);
    for (index, entry) in block.entries.iter().enumerate() {
        println!("  entry[{index}] {:?} {}", entry.kind, entry.path);
    }
    if block.entries.is_empty() {
        println!("（名单是空的）");
    }
    for (index, entry) in block.fullscreen_exempt.iter().enumerate() {
        println!("  exempt[{index}] {:?} {}", entry.kind, entry.path);
    }

    let chain = match pid {
        Some(pid) => crate::platform::fullscreen::process_chain(pid),
        None => crate::platform::fullscreen::foreground_process_chain(),
    };
    for (index, path) in chain.iter().enumerate() {
        let hit = block.blocks_exe(path);
        println!(
            "  chain[{index}] {path}{}",
            if hit { "  <= 命中" } else { "" }
        );
    }
    let blocked = chain.iter().any(|exe| block.blocks_exe(exe));
    let exempt = block.exempts_fullscreen(&chain);
    println!("exempt={exempt}（全屏例外名单命中时，全屏那条不算让路）");
    println!("blocked={blocked}");
    if block.fullscreen {
        println!(
            "fullscreen_now={}",
            crate::platform::fullscreen::is_fullscreen()
        );
    }
}

pub fn run(watch_seconds: Option<u64>) {
    let paths = AppPaths::discover();
    let engine = AudioEngine::spawn(paths.icons_dir(), 64, app_name_capitalize());
    let events = engine.events();

    if let Some(seconds) = watch_seconds {
        let deadline = Instant::now() + Duration::from_secs(seconds);
        let mut last_revision = u64::MAX;
        while Instant::now() < deadline {
            if let Ok(AudioEvent::Snapshot(snapshot)) = events.recv_blocking() {
                if snapshot.apps_revision != last_revision {
                    last_revision = snapshot.apps_revision;
                    println!("{}", render(&snapshot));
                } else {
                    print!("\r{}", compact(&snapshot));
                    use std::io::Write;
                    let _ = std::io::stdout().flush();
                }
            }
        }
        engine.send(AudioCommand::Shutdown);
        println!();
        return;
    }

    if let Ok(AudioEvent::Snapshot(snapshot)) = events.recv_blocking() {
        println!("{}", render(&snapshot));
    } else {
        println!("未取到音频快照");
    }
    engine.send(AudioCommand::Shutdown);
}

/// 探针：每应用的播放设备（未公开的 AudioPolicyConfig）。
///
/// 不带参数只读打印；给 pid + 设备 id 就写一次（`default` = 恢复跟随默认）。
/// 用来回答「右键菜单里选了设备却没生效」「标记不出现」这类问题。
pub fn run_appdev(pid: Option<u32>, device: Option<&str>) {
    let _apartment = crate::audio::com::ComApartment::init_mta();
    let Some(policy) = crate::audio::appdevice::AppDevicePolicy::new() else {
        println!("这台系统拿不到 AudioPolicyConfig（应用级设备切换不可用）");
        return;
    };

    if let (Some(pid), Some(device)) = (pid, device) {
        let target = (device != "default").then_some(device);
        let ok = policy.set_device(pid, target);
        match target {
            Some(id) => println!("set pid={pid} -> {id:?} applied={ok}"),
            None => println!("set pid={pid} -> 跟随默认设备 applied={ok}"),
        }
    }

    if let Some(pid) = pid {
        println!("pid={pid} 当前设备: {:?}", policy.device_of(pid));
        return;
    }

    let paths = AppPaths::discover();
    let engine = AudioEngine::spawn(paths.icons_dir(), 64, app_name_capitalize());
    let events = engine.events();
    if let Ok(AudioEvent::Snapshot(snapshot)) = events.recv_blocking() {
        println!("音频层报 app_routing={}", snapshot.app_routing);
        for app in &snapshot.apps {
            println!(
                "  {:<28} pid={:<6} device={:?}",
                app.name, app.pid, app.device_id
            );
        }
        for device in snapshot.devices.iter().filter(|d| d.enabled) {
            println!("可用设备 {} = {:?}", device.name, device.id);
        }
    }
    engine.send(AudioCommand::Shutdown);
}

/// 探针：当前解析出来的主题 / 自选色（跟随系统 + Windows 强调色）。
///
/// 用来确认「外观模式」「强调色第一个跟随」到底落在哪个颜色上 —— 界面里那些
/// 强调色的东西（选中的页签、滑条填充）都是从这套解析结果推出来的。
pub fn run_theme() {
    let paths = AppPaths::discover();
    let config = crate::config::ConfigStore::load(paths.config_path())
        .get()
        .clone();
    println!("跟随系统（外观模式） = {}", config.theme_follow_system);
    println!(
        "系统深色 = {} / 系统强调色 = {:?}",
        crate::platform::system_theme::is_dark(),
        crate::platform::system_theme::accent_hex()
    );
    println!(
        "主题 = {}（亮色槽 {:?} / 深色槽 {:?} / 固定槽 {:?}）",
        crate::gui::theme_name(&config),
        config.theme_light,
        config.theme_dark,
        config.theme
    );
    let (surface, accent) = crate::gui::theme_custom(&config);
    println!("自选底色 = {surface:?} / 自选强调色 = {accent:?}");
    println!(
        "强调色跟系统走 = {}",
        crate::gui::accent_follows_system(&config)
    );
    println!("config_rev = {}", config.config_rev);
}

/// 探针：显示器亮度（走的和亮度窗口同一条 DDC/CI 线程）。
///
/// 不带参数：连读几轮，打出每台的亮度与「不支持」；
/// 给 `名字:百分比`（例如 `XG279Q:40`）：再写一次，确认能不能写进去。
/// 用来回答「显示器显示不支持 / 拖了滑条没反应」—— 是 DDC/CI 根本读不通，
/// 还是读通过一次之后被锁在「不支持」上。
pub fn run_display(write: Option<(&str, u32)>, rounds: u32) {
    use crate::display::{DisplayCommand, DisplayEngine, DisplayEvent};
    use std::time::Duration;

    /// 工作线程偶尔会卡在一次 DDC 往返里，探针不能陪着一起挂住。
    ///
    /// 用轮询而不是「另开一条线程 recv_blocking」：那种写法超时之后线程还赖在
    /// 通道上，会把后面的事件吞掉（第一版就是这么错的，看到的全是空列表）。
    fn next(
        events: &async_channel::Receiver<DisplayEvent>,
        timeout: Duration,
    ) -> Option<DisplayEvent> {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if let Ok(event) = events.try_recv() {
                return Some(event);
            }
            if std::time::Instant::now() >= deadline {
                return None;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    let engine = DisplayEngine::spawn();
    let events = engine.events();

    for round in 0..rounds.max(1) {
        if round > 0 {
            std::thread::sleep(Duration::from_millis(600));
        }
        engine.send(DisplayCommand::Refresh);
        match next(&events, Duration::from_secs(8)) {
            Some(DisplayEvent::Monitors(list)) => {
                println!("第 {} 轮枚举：{} 台", round + 1, list.len());
                for info in &list {
                    match info.brightness {
                        Some(percent) => {
                            println!("  {:<34} {percent:>3}%  key={}", info.name, info.key)
                        }
                        None => println!(
                            "  {:<34} 不支持（DDC/CI 读不通）  key={}",
                            info.name, info.key
                        ),
                    }
                }
            }
            Some(DisplayEvent::Error(error)) => println!("第 {} 轮枚举失败：{error}", round + 1),
            None => println!("第 {} 轮超时（DDC/CI 卡住了）", round + 1),
        }
    }

    if let Some((needle, percent)) = write {
        engine.send(DisplayCommand::Refresh);
        let Some(DisplayEvent::Monitors(list)) = next(&events, Duration::from_secs(8)) else {
            println!("写入前枚举失败，放弃");
            engine.send(DisplayCommand::Shutdown);
            return;
        };
        let Some(target) = list
            .iter()
            .find(|info| info.name.contains(needle) || info.key.contains(needle))
        else {
            println!("没有名字含 {needle:?} 的显示器：");
            for info in &list {
                println!("  {}", info.name);
            }
            engine.send(DisplayCommand::Shutdown);
            return;
        };
        println!("写入 {} -> {percent}%", target.name);
        engine.send(DisplayCommand::SetBrightness {
            key: target.key.clone(),
            value: percent,
        });
        if let Some(DisplayEvent::Monitors(after)) = next(&events, Duration::from_secs(8)) {
            for info in after.iter().filter(|info| info.key == target.key) {
                println!("  写入后界面拿到的：{:?}", info.brightness);
            }
        }
        // 再真读一遍：确认不是只改了本地缓存。
        engine.send(DisplayCommand::Refresh);
        if let Some(DisplayEvent::Monitors(after)) = next(&events, Duration::from_secs(8)) {
            for info in after.iter().filter(|info| info.key == target.key) {
                println!("  重新枚举回读：{:?}", info.brightness);
            }
        }
    }

    engine.send(DisplayCommand::Shutdown);
}

/// 探针：单独调一个应用的音量（走的就是面板滑块那条命令）。
///
/// 用来回答「把应用指到别的设备之后拖它的音量没反应」这类问题：
/// 会先确认这个 pid 在不在会话名单里 —— 会话是挂在设备上的，应用跑去别台设备之后
/// 如果只枚举默认设备就找不到它，命令会被直接丢掉。
pub fn run_appvol(pid: u32, percent: f32) {
    let paths = AppPaths::discover();
    let engine = AudioEngine::spawn(paths.icons_dir(), 64, app_name_capitalize());
    let events = engine.events();
    let snapshot = match events.recv_blocking() {
        Ok(AudioEvent::Snapshot(snapshot)) => snapshot,
        _ => {
            println!("未取到音频快照");
            return;
        }
    };

    let Some(app) = snapshot.apps.iter().find(|app| app.pid == pid) else {
        println!("pid={pid} 不在会话名单里 —— 应用音量命令会被丢掉");
        for app in &snapshot.apps {
            println!("  {:<28} pid={}", app.name, app.pid);
        }
        engine.send(AudioCommand::Shutdown);
        return;
    };
    let key = app.key.clone();
    println!(
        "找到 {:<28} pid={} 当前 {:.0}% peak={:.3}",
        app.name,
        app.pid,
        app.volume * 100.0,
        app.peak
    );

    engine.send(AudioCommand::SetAppVolume {
        key: key.clone(),
        volume: percent / 100.0,
        unmute: false,
    });

    // 写入是异步的，等几张快照再回报：peak 是真实采样，掉下来才算真生效。
    let mut last = None;
    for _ in 0..24 {
        if let Ok(AudioEvent::Snapshot(snapshot)) = events.recv_blocking() {
            if let Some(app) = snapshot.apps.iter().find(|app| app.key == key) {
                last = Some((app.volume, app.peak, app.muted));
            }
        }
    }
    match last {
        Some((volume, peak, muted)) => println!(
            "  → {:.0}% peak={:.3}{}",
            volume * 100.0,
            peak,
            if muted { " (静音)" } else { "" }
        ),
        None => println!("  → 写入后应用从名单里消失了"),
    }
    engine.send(AudioCommand::Shutdown);
}

fn render(snapshot: &Snapshot) -> String {
    let mut out = String::new();
    out.push_str("=== 设备 ===\n");
    for device in &snapshot.devices {
        out.push_str(&format!(
            "{} {} [{}] {}{} peak={:.3}{}  {}\n",
            if device.is_default { "*" } else { " " },
            device.name,
            format!("{:?}", device.kind),
            format!(
                "{:.1} dB / {}%",
                super::audio::amplitude_to_db(device.volume),
                volume_to_percent(device.volume)
            ),
            if device.muted { " (静音)" } else { "" },
            device.peak,
            if device.enabled { "" } else { " (已禁用)" },
            device.description,
        ));
    }
    out.push_str("=== 应用会话 ===\n");
    if snapshot.apps.is_empty() {
        out.push_str("(当前设备没有活动会话)\n");
    }
    for app in &snapshot.apps {
        // 会话挂在哪台设备上：应用被指到别的设备后，这条会和「跟随默认」不一样。
        let on = app
            .session_devices
            .iter()
            .map(|id| {
                snapshot
                    .devices
                    .iter()
                    .find(|device| &device.id == id)
                    .map(|device| truncate(&device.name, 18))
                    .unwrap_or_else(|| "?".into())
            })
            .collect::<Vec<_>>()
            .join(" + ");
        out.push_str(&format!(
            "  {:<28} pid={:<6} {}{} peak={:.3} state={} on={} key={} icon={}\n",
            truncate(&app.name, 28),
            app.pid,
            format!(
                "{:.1} dB / {}%",
                super::audio::amplitude_to_db(app.volume),
                volume_to_percent(app.volume)
            ),
            if app.muted { " (静音)" } else { "" },
            app.peak,
            app.state,
            if on.is_empty() { "-" } else { &on },
            app.key,
            app.icon_path
                .as_ref()
                .map(|path| path.display().to_string())
                .unwrap_or_else(|| "-".into()),
        ));
    }
    out
}

fn compact(snapshot: &Snapshot) -> String {
    let device = snapshot
        .devices
        .iter()
        .find(|device| device.is_default)
        .map(|device| {
            format!(
                "{} {}% peak {:.2}",
                device.name,
                format!(
                    "{:.1} dB / {}%",
                    super::audio::amplitude_to_db(device.volume),
                    volume_to_percent(device.volume)
                ),
                device.peak
            )
        })
        .unwrap_or_else(|| "无默认设备".into());
    let apps = snapshot
        .apps
        .iter()
        .map(|app| {
            format!(
                "{}={}%/{:.2}",
                truncate(&app.name, 12),
                format!(
                    "{:.1} dB / {}%",
                    super::audio::amplitude_to_db(app.volume),
                    volume_to_percent(app.volume)
                ),
                app.peak
            )
        })
        .collect::<Vec<_>>()
        .join(" ");
    format!("{device} | {apps}")
}

/// 设置里的「应用名首字母大写」：探针要显示成界面上的样子。
fn app_name_capitalize() -> bool {
    let paths = AppPaths::discover();
    crate::config::ConfigStore::load(paths.config_path())
        .get()
        .app_name_capitalize
}

fn truncate(value: &str, limit: usize) -> String {
    let chars: Vec<char> = value.chars().collect();
    if chars.len() <= limit {
        return value.to_string();
    }
    let mut text: String = chars[..limit.saturating_sub(1)].iter().collect();
    text.push('…');
    text
}
