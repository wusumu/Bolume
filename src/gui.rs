//! 界面层启动：托盘、面板窗口、设置窗口与音频事件泵。

use std::{
    cell::{Cell, RefCell},
    collections::HashMap,
    rc::Rc,
    sync::atomic::{AtomicU64, Ordering},
    time::Instant,
};

use gpui::{
    App, AppContext as _, Bounds, Entity, TitlebarOptions, WindowBackgroundAppearance,
    WindowBackgroundAppearance as _WBA, WindowBounds, WindowDecorations, WindowHandle, WindowKind,
    WindowOptions, point, px, size,
};
use tray_icon::{
    MouseButton, MouseButtonState, TrayIconEvent,
    menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu},
};

use crate::{
    app_paths::AppPaths,
    audio::{
        AudioCommand, AudioEngine, AudioEvent, DeviceInfo, Snapshot, amplitude_to_db, readings,
        to_display_volume, to_log_volume,
    },
    config::{Config, ConfigStore, OsdFunction, TrayIconStyle},
    display::{DisplayEngine, DisplayEvent, DisplayInfo},
    i18n::Lang,
    tray::{TooltipFacts, Tray, TrayUpdate},
    ui::{Assets, theme},
    windows::{
        Brightness, Flyout, Settings,
        flyout::{MuteNote, note_muted, shown_muted},
        osd::OsdRequest,
        platform,
    },
};

/// 托盘菜单动作。
#[derive(Clone, Debug)]
enum ShellAction {
    Toggle,
    /// 菜单里点了一项：原生菜单与自绘菜单共用同一份 id 表（见 `run_menu_action`）。
    Menu(String),
    /// 右键托盘图标：弹自绘菜单（原生模式下由托盘自己弹，不会走到这里）。
    TrayMenu,
}

struct AppState {
    engine: Rc<AudioEngine>,
    config: Rc<RefCell<ConfigStore>>,
    tray: Rc<Tray>,
    config_path: String,
    snapshot: Rc<RefCell<Snapshot>>,
    flyout: RefCell<Option<WindowHandle<Flyout>>>,
    settings: RefCell<Option<WindowHandle<Settings>>>,
    /// 托盘的「跟随主题」右键菜单（自绘）。原生菜单模式下它一直藏着。
    tray_menu: RefCell<Option<WindowHandle<crate::windows::TrayMenu>>>,
    /// 应用右键菜单（自绘）：单独一扇小窗，尺寸只跟菜单内容走 ——
    /// 画在面板里会被面板窗口裁掉（竖向排版的面板只有一百多像素高）。
    app_menu: RefCell<Option<WindowHandle<crate::windows::AppMenu>>>,
    /// 显示器亮度窗口（只在启用亮度调节时显示）。
    brightness: RefCell<Option<WindowHandle<Brightness>>>,
    /// 屏幕提示条（OSD）：只在真的要用时创建，和亮度窗一样省一份合成表面。
    ///
    /// 一块屏一扇窗，键是「显示器键」（见 `osd::monitor_keys`）；
    /// 「全部显示器」时这里会有好几条。
    osd: RefCell<Vec<(u32, WindowHandle<crate::windows::Osd>)>>,
    /// OSD 布局编辑器（单独一扇可缩放的窗口）。
    osd_editor: RefCell<Option<WindowHandle<crate::windows::OsdEditor>>>,
    /// DDC/CI 工作线程：显示器列表与读写都走它。
    display_engine: Rc<DisplayEngine>,
    /// 最近一次枚举到的显示器（亮度窗口与设置页共用）。
    displays: Rc<RefCell<Vec<DisplayInfo>>>,
    /// 枚举失败的原因（没有 DDC/CI 通道时给界面一句话）。
    display_error: Rc<RefCell<Option<String>>>,
    /// 音量面板当前的窗口矩形（逻辑像素：x, y, 宽, 高）。
    ///
    /// 亮度面板靠它算自己贴哪一侧 —— 面板自己贴任务栏所在的角落，位置随时会变，
    /// 两边各算一套迟早会算岔，干脆由面板摆好后记一笔，亮度窗照着摆。
    /// 只给 `flyout_frame` / `set_flyout_frame` 用。
    flyout_frame: Rc<Cell<Option<(f32, f32, f32, f32)>>>,
    /// 静音图标的「先声夺人」表，面板与快捷键共用（键同面板的 drag key）。
    ///
    /// 快捷键的静音是 toggle，基准必须是「用户最后按下的意图」而不是快照：
    /// 快照 30Hz 才回读一次，连按两下时第二下读到的还是旧值，于是连发两条
    /// 一样的命令 —— 按两下依旧是静音。共用这张表以后，面板刚点过、
    /// 紧接着按键也接得上。
    mute_override: Rc<RefCell<HashMap<String, MuteNote>>>,
    /// 亮度「先声夺人」表（键同显示器的 `key`）。
    ///
    /// 和上一条一个道理，只是慢得多：DDC/CI 写一次要六十毫秒上下（本机两台 ROG
    /// 实测 60ms），工作线程写完把读数报回来时，界面早就接着按了下一下、算出了
    /// 更远的值。拿这次回读盖掉本地预测，下一档又从旧值重算 —— 按住不放时数值
    /// 会来回跳（74 → 76 → 74），写下去的还常常是重复值，手感上就是「回弹 + 阻力」。
    brightness_override: Rc<RefCell<HashMap<String, BrightnessNote>>>,
}

/// 亮度「先声夺人」的记录：刚写下去的目标 + 时间。
#[derive(Clone, Copy)]
struct BrightnessNote {
    want: u32,
    at: Instant,
}

/// 回读一直对不上时最多顶多久。DDC/CI 一次写六十毫秒、加上排队，留足余量。
const BRIGHTNESS_OVERRIDE_FOR: std::time::Duration = std::time::Duration::from_millis(800);

/// 记一笔「这一台刚被写成多少」：回读回来之前，下一档按这个值算。
///
/// 快捷键和亮度面板共用这张表 —— 拖完滑条紧接着按快捷键，也接得上。
pub fn note_brightness(cx: &App, key: &str, value: u32) {
    cx.global::<AppState>()
        .brightness_override
        .borrow_mut()
        .insert(
            key.to_string(),
            BrightnessNote {
                want: value.min(100),
                at: Instant::now(),
            },
        );
}

/// 回读值该不该采信：刚写过就顶住本地目标，回读对上了或等够久才交还。
///
/// 判据和 `flyout::resolve_muted` 一样是「对上就交还」，不是「顶满多久」：
/// 写成功时工作线程报回来的正是这个值，交还等于没变；写之前那一刻报上来的
/// 旧值才对不上，那一下正是要挡的。
fn resolve_brightness(map: &mut HashMap<String, BrightnessNote>, key: &str, real: u32) -> u32 {
    let Some(note) = map.get(key) else {
        return real;
    };
    let settle = note.want == real || note.at.elapsed() > BRIGHTNESS_OVERRIDE_FOR;
    if settle {
        map.remove(key);
        return real;
    }
    note.want
}

/// 音量面板当前的窗口矩形（逻辑像素：x, y, 宽, 高）。
pub fn flyout_frame(cx: &App) -> Option<(f32, f32, f32, f32)> {
    cx.global::<AppState>().flyout_frame.get()
}

/// 面板摆好之后记一笔，亮度面板下一次定位就用这个矩形。
pub fn set_flyout_frame(cx: &mut App, frame: (f32, f32, f32, f32)) {
    cx.global::<AppState>().flyout_frame.set(Some(frame));
}

/// 目前枚举到的显示器数量（亮度面板有多高靠它算，面板让位也要用）。
pub fn display_count(cx: &App) -> usize {
    cx.global::<AppState>().displays.borrow().len()
}

/// 光标所在的那台显示器 —— 面板就弹在这台上。
///
/// 「在哪台屏上点开的就出现在哪台」：托盘图标只长在任务栏所在的那台（工作区被
/// 任务栏吃掉的那台），点它时光标必然停在那台上；快捷键没有点击点，就按光标位置算
/// （人在哪台屏上干活，面板就出在哪台）。万一认不出光标在哪台（刚拔掉、远程会话），
/// 退回主屏 —— 面板不能因为认不出屏幕就不出来。
pub fn display_at_cursor(cx: &App) -> Option<Rc<dyn gpui::PlatformDisplay>> {
    use windows::Win32::Foundation::POINT;
    use windows::Win32::Graphics::Gdi::{MONITOR_DEFAULTTONEAREST, MonitorFromPoint};
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

    let mut position = POINT::default();
    let target = unsafe {
        if GetCursorPos(&mut position).is_err() {
            return cx.primary_display();
        }
        MonitorFromPoint(position, MONITOR_DEFAULTTONEAREST)
    };
    // gpui 的 `DisplayId` 存的就是 HMONITOR 的值（`display_id_for_monitor` 一行直转），
    // 所以这里能拿 Win32 句柄直接当 id 查 —— 不用比 DPI，也不用比设备名。
    cx.find_display(gpui::DisplayId::new(target.0 as u64))
        .or_else(|| cx.primary_display())
}

/// 当前枚举到的显示器（设置页要用它列出显示器、就地改名字）。
pub fn displays_snapshot(cx: &App) -> Vec<DisplayInfo> {
    cx.global::<AppState>().displays.borrow().clone()
}

/// 融合模式那块亮度块（音量面板里的子视图）要用的几样东西。
///
/// 它读的显示器列表、DDC 线程和错误信息都和独立亮度窗口是同一份 —— 放在面板里
/// 只是换了个容器，数据一点没分家。
pub fn brightness_embed_deps(
    cx: &App,
) -> (
    Rc<RefCell<ConfigStore>>,
    Rc<DisplayEngine>,
    Rc<RefCell<Vec<DisplayInfo>>>,
    Rc<RefCell<Option<String>>>,
) {
    let state = cx.global::<AppState>();
    (
        state.config.clone(),
        state.display_engine.clone(),
        state.displays.clone(),
        state.display_error.clone(),
    )
}

impl gpui::Global for AppState {}

impl AppState {
    fn clone_handles(
        &self,
    ) -> (
        Rc<AudioEngine>,
        Rc<RefCell<ConfigStore>>,
        Rc<Tray>,
        Rc<RefCell<Snapshot>>,
    ) {
        (
            self.engine.clone(),
            self.config.clone(),
            self.tray.clone(),
            self.snapshot.clone(),
        )
    }
}

pub fn run(
    show_on_start: bool,
    settings_on_start: bool,
    editor_on_start: bool,
    restart_wait: bool,
    osd_demo: bool,
    osd_demo_media: bool,
    osd_demo_value: f32,
) {
    const INSTANCE_NAME: &str = crate::platform::single_instance::INSTANCE_NAME;
    // 重启副本：旧进程这会儿还在退出途中，锁还没松 —— 等一会儿再抢，
    // 不能像普通第二次启动那样直接「唤醒已有实例」然后自己退场。
    let acquired = if restart_wait {
        crate::platform::SingleInstance::acquire_wait(
            INSTANCE_NAME,
            crate::platform::restart::RESTART_WAIT,
        )
    } else {
        crate::platform::SingleInstance::acquire(INSTANCE_NAME)
    };
    let Some(instance) = acquired else {
        // 已经有一个 Bolume 在跑：把它的面板叫出来，本进程直接退场。
        if !crate::platform::SingleInstance::activate_existing(INSTANCE_NAME) {
            eprintln!("Bolume 已在运行。");
        }
        return;
    };

    // 第二个实例每次启动都会触发这个信号，由界面线程把面板弹到前台。
    let (activate_tx, activate_rx) = async_channel::unbounded::<()>();
    instance.on_activate(move || {
        let _ = activate_tx.try_send(());
    });

    let paths = AppPaths::discover();
    let config_path = paths.config_path().display().to_string();
    let config = Rc::new(RefCell::new(ConfigStore::load(paths.config_path())));
    let icon_pixels = config.borrow().get().app_icon_size;
    let name_capitalize = config.borrow().get().app_name_capitalize;
    let engine = Rc::new(AudioEngine::spawn(
        paths.icons_dir(),
        icon_pixels,
        name_capitalize,
    ));

    // 显示器亮度走独立的 DDC/CI 线程（一次往返几十毫秒，绝不能放在界面线程上）。
    // 默认不开亮度调节，但线程本身只是待命，不点亮界面就不发命令。
    let display_engine = Rc::new(DisplayEngine::spawn());
    let displays: Rc<RefCell<Vec<DisplayInfo>>> = Rc::new(RefCell::new(Vec::new()));
    let display_error: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    if config.borrow().get().brightness.enabled {
        display_engine.send(crate::display::DisplayCommand::Refresh);
    }

    // 自启动状态与系统对账：**系统里登记的才是事实**，配置只跟着记一笔。
    //
    // 三件事都在这里做：
    //  * 用户在任务管理器「启动」页里关掉 / 打开过、或者删掉了计划任务 → 配置跟着改，
    //    设置页才不会一直显示错；
    //  * 便携版移动了目录 → 把登记的那条路径改到当前 exe；
    //  * 两边都没有 → 配置也置为关闭。
    //
    // 反过来的方向（配置开着、系统里没有）不再自动补写：那多半是用户自己关掉的，
    // 替他重开等于跟用户对着干。
    //
    // 方式（注册表 / 计划任务）以**实际登记在哪边**为准：用户在任务计划程序里
    // 手动删了任务、又自己加了注册表启动项，那就该按注册表那套走。
    {
        let method = crate::platform::autostart::detected()
            .unwrap_or(config.borrow().get().launch_method);
        let admin = config.borrow().get().run_as_admin;
        // 三档里只留一档（老版本残留、或者用户自己加过注册表那一条）：
        // 不去重的话会启动两次 —— 第二次虽然被单实例挡掉，但会平白弹一下面板。
        for other in crate::config::LaunchMethod::ALL {
            if other != method {
                crate::platform::autostart::remove(other);
            }
        }
        let actual = crate::platform::autostart::is_enabled(method);
        if actual {
            crate::platform::autostart::ensure_path(method, admin);
        }
        let stale = {
            let store = config.borrow();
            let current = store.get();
            current.launch_at_login != actual || current.launch_method != method
        };
        if stale {
            let _ = config.borrow_mut().update(|config| {
                config.launch_at_login = actual;
                config.launch_method = method;
            });
        }
    }

    let cfg0 = config.borrow().get().clone();

    // 最大音量限制交给音频线程去做「最后一道闸」：滑条、滚轮、快捷键、
    // 硬件音量键改上去的音量都会在这里被按回上限。
    sync_volume_cap(&engine, &cfg0.volume_cap);
    // 响度均衡：参数和单应用开关都在音频线程里生效。
    sync_loudness(&engine, &cfg0);

    // 配色：打底深 / 浅两套模板，再叠用户自选的底色 / 强调色（没有主题预设了）。
    // 「跟随系统」时模板跟着 Windows 的亮暗走，否则固定用深色那套。
    apply_theme_extras(&cfg0);

    // 全局快捷键：EarTrumpet 的 HotkeyManager。注册在独立的消息线程里，
    // 事件回来后由下面的泵按配置分发（同一个组合多个动作时取配置顺序第一个）。
    let hotkey_rx = crate::platform::hotkey::spawn();
    crate::platform::hotkey::apply_config(&cfg0);
    // 老版本「注册表级系统 OSD 守卫」的残留：删自启动项、叫停可能还在跑的守卫进程。
    crate::platform::legacy_osd::sweep();

    // 播放 / 暂停要问系统媒体会话才知道这次是「开始播」还是「停下来」。
    // 那条 MTA 线程和 manager 提前备好，否则第一次按键要等 WinRT 起服务。
    std::thread::spawn(crate::platform::media::prewarm);

    let startup_number = number_style(&cfg0);
    let tray = match Tray::new(
        crate::tray::IconSpec {
            style: cfg0.tray_icon_style,
            ink: crate::tray::parse_ink(&cfg0.tray_icon_color),
            color_by_level: cfg0.tray_color_by_level,
            custom: &cfg0.tray_icon_custom,
            number: &startup_number,
        },
        cfg0.language,
    ) {
        Ok(tray) => Rc::new(tray),
        Err(error) => {
            eprintln!("{error}");
            return;
        }
    };

    // 托盘菜单：先按空设备列表建一份，拿到第一份快照后会自动重建。
    // 「跟随主题」模式下不挂原生菜单（右键走自绘窗口），否则右键会连着弹两个。
    apply_tray_menu(&tray, &[], config.borrow().get());

    let (action_tx, action_rx) = async_channel::unbounded::<ShellAction>();
    {
        let sender = action_tx.clone();
        TrayIconEvent::set_event_handler(Some(move |event| {
            match event {
                TrayIconEvent::Click {
                    button: MouseButton::Left,
                    button_state: MouseButtonState::Up,
                    ..
                } => {
                    let _ = sender.try_send(ShellAction::Toggle);
                }
                // 右键：原生模式下由托盘自己弹菜单（这条消息白跑一趟，open 时会挡掉）；
                // 主题模式下就靠它弹自绘菜单。
                TrayIconEvent::Click {
                    button: MouseButton::Right,
                    button_state: MouseButtonState::Up,
                    ..
                } => {
                    let _ = sender.try_send(ShellAction::TrayMenu);
                }
                // 光标进出托盘图标：滚轮钩子只在悬停时生效。
                TrayIconEvent::Enter { rect, .. } => {
                    crate::platform::mouse_wheel::set_hovering(true);
                    // 顺便记下图标矩形：Enter/Leave 偶尔会漏，滚轮时用位置兜底核对。
                    crate::platform::mouse_wheel::set_icon_rect(Some((
                        rect.position.x,
                        rect.position.y,
                        rect.position.x + f64::from(rect.size.width),
                        rect.position.y + f64::from(rect.size.height),
                    )));
                }
                TrayIconEvent::Leave { .. } => {
                    crate::platform::mouse_wheel::set_hovering(false);
                    crate::platform::mouse_wheel::set_icon_rect(None);
                }
                _ => {}
            }
        }));
    }
    {
        let sender = action_tx.clone();
        MenuEvent::set_event_handler(Some(move |event: MenuEvent| {
            let id = event.id.0.clone();
            // 菜单项用固定 id（见 build_tray_menu），切语言重建菜单后 id 依然对得上。
            let _ = sender.try_send(ShellAction::Menu(id));
        }));
    }

    let initial_snapshot = Snapshot::default();
    let shared_snapshot = Rc::new(RefCell::new(initial_snapshot.clone()));

    gpui_platform::application()
        .with_assets(Assets)
        .run(move |cx: &mut App| {
            // 托盘常驻程序：关掉设置窗口不等于退出，只有托盘菜单「退出」才真的退。
            cx.set_quit_mode(gpui::QuitMode::Explicit);

            let state = AppState {
                engine: engine.clone(),
                config: config.clone(),
                tray: tray.clone(),
                config_path: config_path.clone(),
                snapshot: shared_snapshot.clone(),
                flyout: RefCell::new(None),
                settings: RefCell::new(None),
                tray_menu: RefCell::new(None),
                app_menu: RefCell::new(None),
                brightness: RefCell::new(None),
                osd: RefCell::new(Vec::new()),
                osd_editor: RefCell::new(None),
                display_engine: display_engine.clone(),
                displays: displays.clone(),
                display_error: display_error.clone(),
                flyout_frame: Rc::new(Cell::new(None)),
                mute_override: Rc::new(RefCell::new(HashMap::new())),
                brightness_override: Rc::new(RefCell::new(HashMap::new())),
            };
            cx.set_global(state);

            // 空闲时把工作集还给系统（见 `platform::memory`）。
            //
            // 面板、亮度卡片、提示条、设置窗全收着就修剪一次：Windows 不会主动回收
            // 工作集，gpui 应用每个窗口一套图集，跑一天之后任务管理器上能到一百多 MB，
            // 其中绝大多数是访问过就不再读的页。30 秒看一次，节流与判据都在模块里。
            cx.spawn(async move |cx| {
                let mut trimmer = crate::platform::memory::IdleTrim::default();
                loop {
                    cx.background_executor()
                        .timer(std::time::Duration::from_secs(30))
                        .await;
                    trimmer.tick();
                }
            })
            .detach();

            // 动态托盘的驱动循环。
            //
            // 60fps 推一帧（16ms）；关掉动态模式时退到 400ms 一轮只做个判断 —— 为一个
            // 没开的特效把 CPU 每秒唤醒 60 次不值当。峰值本身由音频线程按 33ms 采，
            // 画面在中间插值，所以看起来还是顺的。
            cx.spawn(async move |cx| {
                loop {
                    let running = cx.update(|cx| {
                        let state = cx.global::<AppState>();
                        let config = state.config.borrow();
                        let cfg = config.get();
                        // 动态图案 / 彩色律动都靠这条 60fps 的循环推包络。
                        crate::tray::needs_frames(cfg.tray_icon_style, cfg.tray_color_by_level)
                    });
                    let wait = if running { 16 } else { 400 };
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(wait))
                        .await;
                    if !running {
                        continue;
                    }
                    cx.update(|cx| {
                        let state = cx.global::<AppState>();
                        let cfg = state.config.borrow().get().clone();
                        // 再确认一次：这一帧等的那 16ms 里用户可能刚好把动态关掉。
                        // 只看 sleep 前那个快照的话，这最后一帧会把静态图标又盖回动画帧，
                        // 表现就是「从动态切到别的模式不刷新，得再点一下或调音量」。
                        let dynamic = matches!(cfg.tray_icon_style, TrayIconStyle::Dynamic);
                        if !crate::tray::needs_frames(cfg.tray_icon_style, cfg.tray_color_by_level)
                        {
                            return;
                        }
                        let device = state
                            .snapshot
                            .borrow()
                            .devices
                            .iter()
                            .find(|device| device.is_default)
                            .map(|device| device.peak);
                        let Some(peak) = device else {
                            return;
                        };
                        if dynamic {
                            // 动态图案：整张图由 `animate` 一步步推。
                            let muted = state
                                .snapshot
                                .borrow()
                                .devices
                                .iter()
                                .find(|device| device.is_default)
                                .map(|device| device.muted)
                                .unwrap_or(false);
                            state
                                .tray
                                .animate(peak, cfg.tray_motion_gain, cfg.tray_motion, muted);
                            return;
                        }
                        // 彩色律动叠在静态图标上：那一帧还是走 `update`，
                        // 它自己会推一次包络再重画（`last` 已清空，不会被去重跳过）。
                        refresh_tray_icon(cx);
                    });
                }
            })
            .detach();
            // 启动时如果本来就是动态模式，音频线程要立刻提频。
            sync_tray_motion(cx);

            // 面板窗口：先隐藏创建，之后由托盘/快捷键显示。
            // 创建时显式绑主屏：不给 `display_id` 的话 gpui 会自己按鼠标位置挑一台，
            // 那一步会在开窗流程里重查显示器列表，容易撞上借用冲突。
            // 「弹在哪台屏」不靠这个 —— 每次显示都由 `Flyout::update_frame_parked`
            // 按光标所在的显示器摆位（见 `gui::display_at_cursor`），跨屏移动只是
            // 一次 `SetWindowPos`，gpui 在 WM_MOVE 里自己会把 display 换成新的那台。
            let primary_id = cx.primary_display().map(|display| display.id());
            // 初始位置就放在主屏右下角（窗口是隐藏的，用户看不到）。
            let area = cx
                .primary_display()
                .map(|display| display.visible_bounds())
                .unwrap_or_else(|| Bounds::new(point(px(0.), px(0.)), size(px(1920.), px(1080.))));
            let flyout_width = config.borrow().get().flyout_width();
            let starts_at = point(
                area.origin.x + area.size.width - px(flyout_width) - px(12.),
                area.origin.y + area.size.height - px(200.) - px(12.),
            );
            let bounds = Bounds::new(starts_at, size(px(flyout_width), px(200.)));
            let options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                kind: WindowKind::PopUp,
                display_id: primary_id,
                titlebar: Some(TitlebarOptions {
                    // 必须为 true：gpui 在 Windows 上把这个标志当作「隐藏系统标题栏」，
                    // 只有隐藏时才会走 WM_NCCALCSIZE 把客户区扩到整个窗口。
                    // 设成 false 会保留 31px 标题栏 + 8px 边框，内容区被整体下推。
                    appears_transparent: true,
                    ..Default::default()
                }),
                window_background: WindowBackgroundAppearance::Transparent,
                window_decorations: Some(WindowDecorations::Client),
                focus: false,
                // 隐藏创建：首次显示时由 Flyout::show 定位到主屏右下角。
                // （若改为显示创建再隐藏，DWM 圆角会丢失且窗口整体变半透明。）
                show: false,
                is_movable: false,
                is_resizable: false,
                is_minimizable: false,
                // 竖向排版的面板可以很窄（一路 46px），下限压小才不会把窗口顶回去。
                window_min_size: Some(size(px(72.), px(60.))),
                ..Default::default()
            };

            let flyout_entity = match cx.open_window(options, {
                let config = config.clone();
                let engine = engine.clone();
                move |window, cx| {
                    platform::configure_popup(window);
                    // 窗口创建时就置为全透明：首次弹出前没有任何 hide 调用，
                    // 这里不设的话第一次 ShowWindow 会用 alpha = 1 合成一帧。
                    platform::set_opacity(window, 0.0);
                    window.refresh();
                    let view = cx.new(|cx| {
                        let mute_override = cx.global::<AppState>().mute_override.clone();
                        let displays = cx.global::<AppState>().displays.clone();
                        let mut flyout = Flyout::new(
                            initial_snapshot.clone(),
                            config.clone(),
                            engine.clone(),
                            mute_override,
                            cx.focus_handle(),
                            displays,
                        );
                        flyout.on_open_settings =
                            Some(Rc::new(|_window: &mut gpui::Window, cx: &mut App| {
                                open_settings(cx);
                            }));
                        observe_activation(&mut flyout, window, cx);
                        flyout
                    });
                    view
                }
            }) {
                Ok(handle) => handle,
                Err(error) => {
                    eprintln!("无法创建面板窗口：{error}");
                    return;
                }
            };

            cx.global::<AppState>()
                .flyout
                .borrow_mut()
                .replace(flyout_entity.clone());

            // 预热：先在屏幕外把面板「显示」出来一次，让 DWM 的首次出现淡入
            // 在看不见的地方用完 —— 否则第一次打开面板时整块卡片是半透明的。
            {
                let warm = flyout_entity.clone();
                cx.defer(move |cx| {
                    let _ = warm.update(cx, |flyout, window, cx| flyout.warm_up(window, cx));
                });
            }

            // 融合模式下亮度块是面板里的子视图：早点挂上，第一次弹出就是完整的。
            {
                let flyout = flyout_entity.clone();
                cx.defer(move |cx| {
                    let _ = flyout.update(cx, |flyout, _window, cx| {
                        flyout.sync_brightness_embed(cx);
                    });
                });
            }

            // 托盘右键菜单（「跟随主题」那版）：同样隐藏创建，每次右键按光标位置摆好再露头。
            // 原生菜单模式下它一直藏着，右键交给系统菜单。
            let menu_options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                    point(px(0.), px(0.)),
                    size(px(240.), px(120.)),
                ))),
                kind: WindowKind::PopUp,
                display_id: primary_id,
                titlebar: Some(TitlebarOptions {
                    appears_transparent: true,
                    ..Default::default()
                }),
                window_background: WindowBackgroundAppearance::Transparent,
                window_decorations: Some(WindowDecorations::Client),
                focus: false,
                show: false,
                is_movable: false,
                is_resizable: false,
                is_minimizable: false,
                // 一行菜单只有 34px 高，下限再压小点，别让系统把它顶回去。
                window_min_size: Some(size(px(100.), px(28.))),
                ..Default::default()
            };
            match cx.open_window(menu_options, {
                let config = config.clone();
                let snapshot = shared_snapshot.clone();
                move |window, cx| {
                    platform::configure_popup(window);
                    platform::set_opacity(window, 0.0);
                    window.refresh();
                    cx.new(|_| crate::windows::TrayMenu::new(config.clone(), snapshot.clone()))
                }
            }) {
                Ok(handle) => {
                    // 失焦立刻收起：走系统的激活事件，不再等轮询。
                    let _ = handle.update(cx, |menu, window, cx| menu.watch_blur(window, cx));
                    cx.global::<AppState>()
                        .tray_menu
                        .borrow_mut()
                        .replace(handle);
                }
                Err(error) => eprintln!("无法创建托盘菜单窗口：{error}"),
            }

            // 应用右键菜单：同样隐藏创建，右键时按点在屏幕上的位置摆好再露头。
            //
            // 这扇窗用 `configure_osd` 那套（保留 `WS_EX_NOACTIVATE`）：菜单弹出时
            // 绝不能把面板挤到后台 —— 面板一失焦就自动收起了，菜单就成了孤儿。
            let app_menu_options = WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds::new(
                    point(px(0.), px(0.)),
                    size(px(216.), px(160.)),
                ))),
                kind: WindowKind::PopUp,
                display_id: primary_id,
                titlebar: Some(TitlebarOptions {
                    appears_transparent: true,
                    ..Default::default()
                }),
                window_background: WindowBackgroundAppearance::Transparent,
                window_decorations: Some(WindowDecorations::Client),
                focus: false,
                show: false,
                is_movable: false,
                is_resizable: false,
                is_minimizable: false,
                window_min_size: Some(size(px(120.), px(24.))),
                ..Default::default()
            };
            match cx.open_window(app_menu_options, {
                let config = config.clone();
                let snapshot = shared_snapshot.clone();
                let engine = engine.clone();
                move |window, cx| {
                    platform::configure_osd(window);
                    platform::set_opacity(window, 0.0);
                    window.refresh();
                    cx.new(|_| {
                        crate::windows::AppMenu::new(
                            config.clone(),
                            snapshot.clone(),
                            engine.clone(),
                        )
                    })
                }
            }) {
                Ok(handle) => {
                    cx.global::<AppState>()
                        .app_menu
                        .borrow_mut()
                        .replace(handle);
                }
                Err(error) => eprintln!("无法创建应用菜单窗口：{error}"),
            }

            // 托盘动作分发
            cx.spawn(async move |cx| {
                while let Ok(action) = action_rx.recv().await {
                    cx.update(|cx| match action {
                        ShellAction::Toggle => toggle_flyout(cx),
                        ShellAction::Menu(id) => run_menu_action(cx, &id),
                        ShellAction::TrayMenu => open_tray_menu(cx),
                    });
                }
            })
            .detach();

            // 音频事件泵：刷新面板、托盘与设置页的设备列表
            let events = engine.events();
            // 启动套用：只在这次启动的第一份快照之后跑一遍（之后用户怎么调都不再干预）。
            let mut startup = crate::startup::Startup::new(cfg0.startup.clone());
            cx.spawn(async move |cx| {
                let mut last_device_key = String::new();
                while let Ok(event) = events.recv().await {
                    let snapshot = match event {
                        AudioEvent::Snapshot(snapshot) => snapshot,
                        AudioEvent::LoudnessReleased(key) => {
                            // 用户自己拖了音量：把这个应用的响度均衡关掉并落配置。
                            // 关掉之后音频线程那边会收到新的 SetLoudness，滑条就能自由拖了。
                            let (engine, config, _, _) =
                                cx.update(|cx| cx.global::<AppState>().clone_handles());
                            let mut changed = false;
                            let _ = config.borrow_mut().update(|config| {
                                changed = config.set_app_loudness(&key, Some(false));
                            });
                            if changed {
                                let config = config.borrow();
                                sync_loudness(&engine, config.get());
                            }
                            let _ = cx.update(|cx| cx.refresh_windows());
                            continue;
                        }
                        AudioEvent::Error(_) => continue,
                    };
                    let (engine, config, tray, shared) =
                        cx.update(|cx| cx.global::<AppState>().clone_handles());

                    startup.on_snapshot(&snapshot, &engine);

                    // 菜单指纹：启用/禁用、默认设备、设备名任何一项变了都要重建，
                    // 只看 id 会漏掉「在系统里新启用一台设备」（id 顺序往往不变）。
                    let device_key = snapshot.device_menu_key();
                    *shared.borrow_mut() = snapshot.clone();
                    if device_key != last_device_key {
                        last_device_key = device_key;
                        // 设备集合或默认设备变化时，让设置页重新绘制，
                        // 托盘菜单顶部的设备列表也要跟着重新勾选。
                        let _ = cx.update(|cx| {
                            rebuild_tray_menu(cx);
                            cx.refresh_windows();
                        });
                    }

                    if let Some(device) = snapshot
                        .devices
                        .iter()
                        .find(|device| device.is_default)
                        .cloned()
                    {
                        let cfg = config.borrow().get().clone();
                        let min_db = cfg.logarithmic_min_db as f32;
                        let display = if cfg.use_logarithmic_volume {
                            to_display_volume(device.volume, min_db)
                        } else {
                            device.volume
                        };
                        let facts = tooltip_facts(&snapshot, &cfg, &device);
                        let number = number_style(&cfg);
                        tray.update(&TrayUpdate {
                            volume: device.volume,
                            display,
                            muted: device.muted,
                            peak: device.peak,
                            icon: crate::tray::IconSpec {
                                style: cfg.tray_icon_style,
                                ink: crate::tray::parse_ink(&cfg.tray_icon_color),
                                color_by_level: cfg.tray_color_by_level,
                                custom: &cfg.tray_icon_custom,
                                number: &number,
                            },
                            motion_gain: cfg.tray_motion_gain,
                            motion_effect: cfg.tray_motion,
                            logarithmic: cfg.use_logarithmic_volume,
                            min_db,
                            lang: cfg.language,
                            tooltip: &cfg.tooltip,
                            facts: &facts,
                        });
                    }

                    let _ = cx.update(|cx| {
                        let handle = cx.global::<AppState>().flyout.borrow().clone();
                        if let Some(handle) = handle {
                            let _ = handle.update(cx, |flyout, window, cx| {
                                flyout.apply_and_refresh(snapshot.clone(), window, cx);
                            });
                        }
                        // 提示条正显示着就跟着刷新读数（滚轮 / 硬件键改的音量也能跟上）。
                        // 「全部显示器」时每扇窗都要刷。
                        let osd_handles = cx.global::<AppState>().osd.borrow().clone();
                        for (_, handle) in osd_handles {
                            let _ = handle.update(cx, |osd, _, cx| {
                                if osd.is_visible() {
                                    osd.sync_snapshot(snapshot.clone(), cx);
                                }
                            });
                        }
                    });
                }
            })
            .detach();

            // 显示器事件泵：DDC/CI 线程报上来的列表 / 亮度变化，同步给亮度窗口。
            let display_events = display_engine.events();
            cx.spawn(async move |cx| {
                while let Ok(event) = display_events.recv().await {
                    cx.update(|cx| {
                        let handle = {
                            let state = cx.global::<AppState>();
                            match event {
                                DisplayEvent::Monitors(mut list) => {
                                    // 刚写下去的目标还没落地时，回读报的是上一档：
                                    // 拿它盖掉本地值，下一档就会往回算（长按时的回弹）。
                                    {
                                        let mut overrides = state.brightness_override.borrow_mut();
                                        for info in list.iter_mut() {
                                            if let Some(real) = info.brightness {
                                                info.brightness = Some(resolve_brightness(
                                                    &mut overrides,
                                                    &info.key,
                                                    real,
                                                ));
                                            }
                                        }
                                    }
                                    *state.displays.borrow_mut() = list;
                                    *state.display_error.borrow_mut() = None;
                                }
                                DisplayEvent::Error(error) => {
                                    *state.display_error.borrow_mut() = Some(error);
                                }
                            }
                            state.brightness.borrow().clone()
                        };
                        if let Some(handle) = handle {
                            let _ = handle.update(cx, |_, _, cx| cx.notify());
                        }
                    });
                }
            })
            .detach();

            // 托盘滚轮：悬停在图标上滚动即调当前设备音量。
            let wheel_rx = crate::platform::mouse_wheel::spawn();
            cx.spawn(async move |cx| {
                while let Ok(step) = wheel_rx.recv().await {
                    cx.update(|cx| adjust_device_volume(cx, step));
                }
            })
            .detach();

            // 全局快捷键：面板 / 设置 / 音量 ±2%（EarTrumpet 的 AbsoluteVolume*）。
            cx.spawn(async move |cx| {
                while let Ok(hotkey) = hotkey_rx.recv().await {
                    cx.update(|cx| dispatch_hotkey(cx, hotkey));
                }
            })
            .detach();

            // 又启动了一次 Bolume：把已存在的面板叫到前台。
            cx.spawn(async move |cx| {
                while activate_rx.recv().await.is_ok() {
                    cx.update(|cx| show_flyout(cx));
                }
            })
            .detach();

            engine.send(AudioCommand::Refresh);

            // 系统亮暗 / 强调色巡检：跟随系统模式下 Windows 一切「应用模式」，
            // 已经开着的窗口也跟着换肤；强调色那一格空着时（跟随 Windows 强调色）
            // 同理。注册表的变更通知要挂窗口过程，这里每 2 秒读一次就够 ——
            // 两次注册表读，开销可以忽略，用户也感知不到这两秒。
            cx.spawn(async move |cx| {
                let mut was_dark = crate::platform::system_theme::is_dark();
                let mut was_accent = crate::platform::system_theme::accent_hex();
                loop {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(2000))
                        .await;
                    let dark = crate::platform::system_theme::is_dark();
                    let accent = crate::platform::system_theme::accent_hex();
                    if dark == was_dark && accent == was_accent {
                        continue;
                    }
                    let dark_changed = dark != was_dark;
                    let accent_changed = accent != was_accent;
                    was_dark = dark;
                    was_accent = accent;
                    let _ =
                        cx.update(|cx| sync_system_look(cx, dark_changed, accent_changed));
                }
            })
            .detach();

            // 调试入口：--show 启动即展开面板。
            if show_on_start {
                cx.spawn(async move |cx| {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(400))
                        .await;
                    cx.update(|cx| toggle_flyout(cx));
                })
                .detach();
            }

            // 调试入口：--osd-demo 启动即按当前布局弹一次音量卡片；
            // 加 --osd-demo-media 则弹媒体键那张（播放 / 暂停）。
            // 只画卡片、不碰任何设备音量与播放状态 —— 验 OSD 布局 / 阴影 / 淡出时用。
            //
            // `BOLUME_OSD_DEMO_AFTER=1450` 会在第一张之后再弹一次（毫秒，从第一次
            // 触发算起）：用来验「出场演到一半又被呼出」要接着往回走，不能跳。
            if osd_demo || osd_demo_media {
                let replay = std::env::var("BOLUME_OSD_DEMO_AFTER")
                    .ok()
                    .and_then(|value| value.trim().parse::<u64>().ok())
                    .filter(|ms| *ms > 0);
                cx.spawn(async move |cx| {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(400))
                        .await;
                    let request = || {
                        if osd_demo_media {
                            crate::windows::osd::OsdRequest::device(
                                crate::config::OsdFunction::PlayPause,
                            )
                            .playing(false)
                        } else {
                            crate::windows::osd::OsdRequest::device(
                                crate::config::OsdFunction::Volume,
                            )
                            .predicted(osd_demo_value)
                        }
                    };
                    cx.update(|cx| show_osd(cx, request()));
                    if let Some(after) = replay {
                        cx.background_executor()
                            .timer(std::time::Duration::from_millis(after))
                            .await;
                        cx.update(|cx| show_osd(cx, request()));
                    }
                })
                .detach();
            }

            // 调试入口：--settings 启动即打开设置窗口。
            if settings_on_start {
                open_settings(cx);
            }

            // 调试入口：--osd-editor 启动即打开 OSD 布局编辑器。
            if editor_on_start {
                cx.spawn(async move |cx| {
                    cx.background_executor()
                        .timer(std::time::Duration::from_millis(500))
                        .await;
                    cx.update(|cx| open_osd_editor(cx));
                })
                .detach();
            }
        });
}

/// 面板组（音量面板 + 亮度卡片）压回「任务栏之后」。
///
/// 两扇窗都是 topmost。被激活（用户点一下）的那一刻，系统会把它提到 topmost 组
/// 最前面 —— 于是它比任务栏还靠前，收起时整段滑动就从任务栏**前面**划过去。
/// 所以在激活时把两扇窗都重新插回任务栏后面（topmost 组里仅次于任务栏：
/// 比别的置顶小工具都靠前，又不会压住任务栏）。
///
/// `window` / `visible` 是刚刚被激活的那扇窗：它正在自己的回调里，不能再 `update`。
fn sink_panel_group(
    window: &gpui::Window,
    visible: bool,
    self_is_brightness: bool,
    cx: &mut App,
) {
    use crate::windows::platform;
    if visible {
        platform::present_behind_taskbar(window);
    }
    // 激活会让这一组窗口一起跳到最前面，另一扇也得压回去。
    if self_is_brightness {
        let handle = cx.global::<AppState>().flyout.borrow().clone();
        if let Some(handle) = handle {
            let _ = handle.update(cx, |flyout, window, _| {
                if flyout.is_visible() {
                    platform::present_behind_taskbar(window);
                }
            });
        }
    } else {
        let handle = cx.global::<AppState>().brightness.borrow().clone();
        if let Some(handle) = handle {
            let _ = handle.update(cx, |view, window, _| {
                if view.is_visible() {
                    platform::present_behind_taskbar(window);
                }
            });
        }
    }
}

fn observe_activation(
    flyout: &mut Flyout,
    window: &mut gpui::Window,
    cx: &mut gpui::Context<Flyout>,
) {
    let _ = flyout;
    // 失去焦点即收起（可在设置里关闭）
    let mut was_active = false;
    cx.observe_window_activation(window, move |flyout, window, cx| {
        let active = window.is_window_active();
        let hide_on_blur = flyout.config.borrow().get().hide_on_focus_lost;
        if active && !was_active {
            sink_panel_group(window, flyout.is_visible(), false, cx);
        }
        // 钉住的面板不等焦点：点别的窗口也赖在那儿（这正是「置顶」的意义）。
        if was_active && !active && hide_on_blur && flyout.is_visible() && !flyout.is_pinned() {
            // 点到亮度窗口上也算「还在这一组面板里」，不能把面板收掉：
            // 那扇窗是另开的，焦点一换面板就消失的话根本没法拖亮度条。
            // 谁先收到失焦是由系统决定的，所以延后一点再判断，见 `schedule_group_blur_check`。
            schedule_group_blur_check(cx);
        }
        was_active = active;
    })
    .detach();
}

/// 亮度窗口：自己失焦且音量面板也不在前台时，两个一起收起。
fn observe_brightness_activation(
    view: &mut Brightness,
    window: &mut gpui::Window,
    cx: &mut gpui::Context<Brightness>,
) {
    let _ = view;
    let mut was_active = false;
    cx.observe_window_activation(window, move |view, window, cx| {
        let active = window.is_window_active();
        if active && !was_active {
            sink_panel_group(window, view.is_visible(), true, cx);
        }
        if was_active && !active {
            schedule_group_blur_check(cx);
        }
        was_active = active;
    })
    .detach();
}

/// 面板组失焦后的统一收尾。
///
/// 两扇窗（音量面板 + 亮度窗口）是一组：点其中一扇，另一扇会失焦，
/// 但那不代表用户要关掉它们。谁先收到消息由系统决定，所以延后一拍再看
/// 「这一组里还有没有窗口在前台」：都没有才整组收起，否则原样留着。
///
/// 调用点只在真正失焦时（`was_active && !active`），重复调用是无害的。
fn schedule_group_blur_check(cx: &mut App) {
    let enabled = cx
        .global::<AppState>()
        .config
        .borrow()
        .get()
        .hide_on_focus_lost;
    if !enabled {
        return;
    }
    cx.spawn(async move |cx| {
        cx.background_executor()
            .timer(std::time::Duration::from_millis(40))
            .await;
        cx.update(|cx| {
            if flyout_window_active(cx) || brightness_window_active(cx) {
                return;
            }
            hide_flyout(cx);
            hide_brightness(cx);
        });
    })
    .detach();
}

fn flyout_window_active(cx: &mut App) -> bool {
    let handle = cx.global::<AppState>().flyout.borrow().clone();
    handle
        .map(|handle| {
            handle
                .update(cx, |_, window, _| window.is_window_active())
                .unwrap_or(false)
        })
        .unwrap_or(false)
}

fn brightness_window_active(cx: &mut App) -> bool {
    let handle = cx.global::<AppState>().brightness.borrow().clone();
    handle
        .map(|handle| {
            handle
                .update(cx, |_, window, _| window.is_window_active())
                .unwrap_or(false)
        })
        .unwrap_or(false)
}

/// 按「开了亮度调节 + 面板正显示」决定亮度窗口露不露面。
/// 把亮度窗口建出来（已经建过就直接返回）。
///
/// 这扇窗只在「显示器亮度」开着时才用得上，所以改成第一次真要显示时才创建：
/// 每多一扇 gpui 窗口就多一份 DirectComposition 表面、字形排版缓存和窗口上下文，
/// 常驻程序省一份是一份。
pub fn ensure_brightness_window(cx: &mut App) {
    if cx.global::<AppState>().brightness.borrow().is_some() {
        return;
    }
    let (config, display_engine, displays, display_error) = {
        let state = cx.global::<AppState>();
        (
            state.config.clone(),
            state.display_engine.clone(),
            state.displays.clone(),
            state.display_error.clone(),
        )
    };
    let primary_id = cx.primary_display().map(|display| display.id());
    let area = cx
        .primary_display()
        .map(|display| display.visible_bounds())
        .unwrap_or_else(|| Bounds::new(point(px(0.), px(0.)), size(px(1920.), px(1080.))));
    let (flyout_width, panel_width) = {
        let config = config.borrow();
        let config = config.get();
        (config.flyout_width(), config.brightness_width())
    };
    let bounds = Bounds::new(
        point(
            area.origin.x + area.size.width - px(flyout_width) - px(20.) - px(panel_width),
            area.origin.y + area.size.height - px(200.) - px(12.),
        ),
        size(px(panel_width), px(200.)),
    );
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        kind: WindowKind::PopUp,
        display_id: primary_id,
        titlebar: Some(TitlebarOptions {
            appears_transparent: true,
            ..Default::default()
        }),
        window_background: WindowBackgroundAppearance::Transparent,
        window_decorations: Some(WindowDecorations::Client),
        focus: false,
        show: false,
        is_movable: false,
        is_resizable: false,
        is_minimizable: false,
        // 最小尺寸只用来兜住「窗口被缩没」，必须比 BRIGHTNESS_WIDTH 小 ——
        // 不然 gpui 会把亮度窗按最小宽度撑回去，拖拽条就不是 100px 了。
        // 竖向的亮度面板一台显示器一列（64px），别用横排的 120 当地板。
        window_min_size: Some(size(px(64.), px(60.))),
        ..Default::default()
    };
    let handle = match cx.open_window(options, {
        let config = config.clone();
        let display_engine = display_engine.clone();
        let displays = displays.clone();
        let display_error = display_error.clone();
        move |window, cx| {
            platform::configure_popup(window);
            // 和面板一样：创建时就置全透明，第一次 ShowWindow 才不会闪空框。
            platform::set_opacity(window, 0.0);
            window.refresh();
            cx.new(|cx| {
                let mut view = Brightness::new(
                    config.clone(),
                    display_engine.clone(),
                    displays.clone(),
                    display_error.clone(),
                    cx,
                );
                observe_brightness_activation(&mut view, window, cx);
                view
            })
        }
    }) {
        Ok(handle) => handle,
        Err(error) => {
            eprintln!("无法创建亮度窗口：{error}");
            return;
        }
    };
    cx.global::<AppState>()
        .brightness
        .borrow_mut()
        .replace(handle);
}

pub fn sync_brightness(cx: &mut App) {
    let (enabled, merged, flyout) = {
        let state = cx.global::<AppState>();
        let config = state.config.borrow();
        let config = config.get();
        (
            config.brightness.enabled,
            config.brightness.merged_with_audio(),
            state.flyout.borrow().clone(),
        )
    };
    let visible = flyout
        .map(|handle| {
            handle
                .update(cx, |flyout, _, _| flyout.is_visible())
                .unwrap_or(false)
        })
        .unwrap_or(false);
    // 融合模式：亮度块长在音量面板里，独立那扇窗一直收着（建出来也浪费一份合成表面）。
    if enabled && visible && !merged {
        show_brightness(cx);
    } else {
        hide_brightness(cx);
    }
}

/// 亮度的两个开关（启用 / 融合）或面板排版变了之后调一次：
/// 让音量面板把融合块挂上 / 摘下，尺寸跟着重排。
pub fn refresh_merged_brightness(cx: &mut App) {
    let handle = cx.global::<AppState>().flyout.borrow().clone();
    if let Some(handle) = handle {
        let _ = handle.update(cx, |flyout, window, cx| {
            flyout.sync_brightness_embed(cx);
            if flyout.is_visible() {
                flyout.update_frame(window, cx);
            }
            window.refresh();
        });
    }
    sync_brightness(cx);
}

fn show_brightness(cx: &mut App) {
    ensure_brightness_window(cx);
    let handle = cx.global::<AppState>().brightness.borrow().clone();
    if let Some(handle) = handle {
        let _ = handle.update(cx, |view, window, cx| {
            if view.is_visible() {
                // 已经在屏幕上：只压回「任务栏之后」，别再往上提。
                // 用 `bring_to_front` 会把它插到任务栏**前面**，收起时就从上面划过去。
                crate::windows::platform::present_behind_taskbar(window);
            } else {
                view.show(window, cx);
            }
        });
    }
}

fn hide_brightness(cx: &mut App) {
    let handle = cx.global::<AppState>().brightness.borrow().clone();
    if let Some(handle) = handle {
        let _ = handle.update(cx, |view, window, cx| view.hide(window, cx));
    }
}

/// 收起面板（亮度窗口失焦时用）。
///
/// 钉住（页脚的置顶按钮）的面板不走：钉住的意思就是「别自己消失」，
/// 这是自动收起的最后一道闸，别的失焦路径漏了也在这儿兜住。
fn hide_flyout(cx: &mut App) {
    // 面板一收，它上面那份右键菜单也跟着收：菜单是独立窗口，不会自己跟着走。
    hide_app_menu(cx);
    let handle = cx.global::<AppState>().flyout.borrow().clone();
    if let Some(handle) = handle {
        let _ = handle.update(cx, |flyout, window, cx| {
            if flyout.is_visible() && !flyout.is_pinned() {
                flyout.hide_from_blur(window, cx);
            }
        });
    }
}

/// 预设增删之后让亮度窗口重排一次（它的高度跟着预设行数走）。
pub fn refresh_brightness_window(cx: &mut App) {
    let handle = cx.global::<AppState>().brightness.borrow().clone();
    if let Some(handle) = handle {
        let _ = handle.update(cx, |view, window, cx| {
            if view.is_visible() {
                view.update_frame(window, cx);
            }
            cx.notify();
        });
    }
}

/// 打底用哪种模板：跟着 Windows 亮暗（且开了跟随系统）时按系统走，否则一律深色。
///
/// 固定模式下选浅色底也行 —— 底色自己会决定文字 / 描边的明暗，
/// 这里的模板只负责「用户还没选底色时长什么样」。
pub fn base_is_dark(config: &Config) -> bool {
    !config.theme_follow_system || crate::platform::system_theme::is_dark()
}

/// 当前该用哪一套自选底色 / 强调色：跟随时取当前那一档自己的，否则取全局那对。
///
/// 亮 / 暗两档各存一份的原因见 `Config::theme_light_surface`：共用一份自选色时，
/// 跟随系统一换档长得一模一样，用户会以为「没切换」。
///
/// 强调色那一格空着 = **跟随 Windows 的强调色**（色块行第一个「跟随」就是它），
/// 读不到系统的（键被策略锁住 / 不是 Windows）才退回模板自带的那一抹。
/// 底色空着 = 用模板自己的底色。
pub fn theme_custom(config: &Config) -> (String, String) {
    let (surface, accent) = active_custom_slots(config);
    let accent = if accent.trim().is_empty() {
        crate::platform::system_theme::accent_hex().unwrap_or_default()
    } else {
        accent.to_string()
    };
    (surface.to_string(), accent)
}

/// 当前生效的那一对自选色槽位（强调色空串代表「跟随系统强调色」）。
fn active_custom_slots(config: &Config) -> (&String, &String) {
    if config.theme_follow_system {
        if crate::platform::system_theme::is_dark() {
            (&config.theme_dark_surface, &config.theme_dark_accent)
        } else {
            (&config.theme_light_surface, &config.theme_light_accent)
        }
    } else {
        (&config.theme_surface, &config.theme_accent)
    }
}

/// 强调色现在是不是跟着 Windows 的强调色走（当前生效的那一格是空串）。
pub fn accent_follows_system(config: &Config) -> bool {
    active_custom_slots(config).1.trim().is_empty()
}

/// 把配色与几项外观设置推进界面层：打底模板、自选底色、自选强调色、对比度、
/// 设置窗字号、界面不透明度、界面字体。
///
/// 改这几项里任何一项都要调一次，顺序不能反（先模板、后叠自选色）。
pub fn apply_theme_extras(config: &Config) {
    // 先把调色板重置成模板，再叠自选色。
    //
    // `apply_custom` 只会在**当前**调色板上盖一层：自选色被清空（点掉已选中的色块）时，
    // 直接在旧调色板上叠空串是撤不掉的 —— 窗口会一直停在最后一次自选的颜色上。
    theme::set_mode(base_is_dark(config));
    let (surface, accent) = theme_custom(config);
    theme::apply_custom(&surface, &accent, config.ui_contrast);
    theme::set_font_scale(config.ui_font_scale as f32 / 100.0);
    theme::set_ui_opacity(config.ui_opacity as f32 / 100.0);
    theme::set_ui_font(&config.ui_font);
    theme::set_ui_font_weight(config.ui_font_weight);
}

/// 系统亮暗 / 强调色变了：跟随系统时重算调色板，并把开着的窗口都重画一遍。
///
/// 两件事分开判：亮暗只在「跟随系统」时才有意义；强调色只要当前那一格是空串
/// （= 跟随 Windows 强调色）就得跟着换，和外观模式是固定还是跟随无关。
fn sync_system_look(cx: &mut App, dark_changed: bool, accent_changed: bool) {
    let config = cx.global::<AppState>().config.borrow().get().clone();
    let follow_dark = dark_changed && config.theme_follow_system;
    let follow_accent = accent_changed && accent_follows_system(&config);
    if !follow_dark && !follow_accent {
        return;
    }
    apply_theme_extras(&config);
    repaint_windows(cx);
}

/// 换肤之后让已经开着的窗口都重画一遍。
///
/// `App::refresh_windows()` 只置脏；Windows 上还得各补一条 WM_PAINT 才会真的
/// 走到绘制（和设置页里 `refresh_ui` 补的是同一个理由）。面板 / 亮度窗平时是
/// 藏着的，重画没有副作用；自绘托盘菜单是独立窗口，不刷的话它停在旧配色上。
pub fn repaint_windows(cx: &mut App) {
    cx.refresh_windows();
    let (settings, flyout, brightness, tray_menu, osds) = {
        let state = cx.global::<AppState>();
        (
            state.settings.borrow().clone(),
            state.flyout.borrow().clone(),
            state.brightness.borrow().clone(),
            state.tray_menu.borrow().clone(),
            state
                .osd
                .borrow()
                .iter()
                .map(|(_, handle)| handle.clone())
                .collect::<Vec<_>>(),
        )
    };
    if let Some(handle) = settings {
        let _ = handle.update(cx, |_, window, _| platform::post_repaint(window));
    }
    if let Some(handle) = flyout {
        let _ = handle.update(cx, |_, window, _| platform::post_repaint(window));
    }
    if let Some(handle) = brightness {
        let _ = handle.update(cx, |_, window, _| platform::post_repaint(window));
    }
    if let Some(handle) = tray_menu {
        let _ = handle.update(cx, |_, window, _| platform::post_repaint(window));
    }
    for handle in osds {
        let _ = handle.update(cx, |_, window, _| platform::post_repaint(window));
    }
}

/// 音量面板宽度改了：整块重排，跟着它走的亮度面板也重新贴一次。
///
/// 面板没开着的时候会被「预览」出来（和手动打开一模一样，只是不抢焦点）——
/// 改外观本来就得看着效果调，这一下是用户预期里的。
pub fn refresh_flyout_size(cx: &mut App) {
    let handle = cx.global::<AppState>().flyout.borrow().clone();
    if let Some(handle) = handle {
        let _ = handle.update(cx, |flyout, window, cx| {
            // 排版一换，融合块的方向和尺寸都跟着变（横排 → 竖排是长高换成占宽）。
            flyout.sync_brightness_embed(cx);
            flyout.refresh_layout(window, cx);
        });
    }
    refresh_brightness_window(cx);
}

/// 设置页改了页脚按钮（放哪些、摆哪边、什么顺序）之后调一次。
///
/// 只刷开着的面板；关着的时候不用管，下次弹出直接读新配置 ——
/// 为此把面板预览出来太打扰。
pub fn refresh_flyout_footer(cx: &mut App) {
    let handle = cx.global::<AppState>().flyout.borrow().clone();
    if let Some(handle) = handle {
        let _ = handle.update(cx, |flyout, window, cx| flyout.refresh_footer(window, cx));
    }
}

/// 设置窗关掉时把它弹出来的面板一起收走。
///
/// 钉住的留着：那是用户自己按的置顶，意思是「别自己消失」。
pub fn release_flyout(cx: &mut App) {
    let handle = cx.global::<AppState>().flyout.borrow().clone();
    if let Some(handle) = handle {
        let _ = handle.update(cx, |flyout, window, cx| {
            if flyout.is_visible() && !flyout.is_pinned() {
                flyout.hide(window, cx);
            }
        });
    }
    // 亮度面板是跟着音量面板来的（挨着摆、一起出现 / 一起收），收完得同步一次，
    // 否则音量面板走了它自己还留在屏幕上。
    sync_brightness(cx);
}

/// 重新枚举一遍显示器（设置页「显示器」那一页打开时调一次）。
///
/// 列表只在亮度面板打开 / 启动时刷 —— 设置页只读那份缓存。开机时显示器要是还在打盹，
/// 那次枚举就会存下一屏「不支持」，之后不开面板就一直这么挂着。进这页先补读一次，
/// 读不到也没关系：工作线程自己会按退避继续补。
pub fn refresh_displays(cx: &mut App) {
    cx.global::<AppState>()
        .display_engine
        .send(crate::display::DisplayCommand::Refresh);
}

/// 设置页改了「启用显示器亮度」之后调用：立刻生效，不用重启。
pub fn refresh_brightness(cx: &mut App) {
    let enabled = cx
        .global::<AppState>()
        .config
        .borrow()
        .get()
        .brightness
        .enabled;
    if enabled {
        cx.global::<AppState>()
            .display_engine
            .send(crate::display::DisplayCommand::Refresh);
    }
    refresh_merged_brightness(cx);
}

/// 把 OSD 窗口建出来（这块屏已经有了就直接返回）。
///
/// 和亮度窗一样按需创建：默认配置下它只在用户按音量键时才需要，
/// 常驻程序少一扇窗就少一份 DirectComposition 表面。
/// `monitor` 是显示器键（0 = 主显示器，n = 第 n 台），见 `osd::monitor_keys`。
fn ensure_osd_window(cx: &mut App, monitor: u32) -> Option<WindowHandle<crate::windows::Osd>> {
    let existing = cx
        .global::<AppState>()
        .osd
        .borrow()
        .iter()
        .find(|(key, _)| *key == monitor)
        .map(|(_, handle)| handle.clone());
    if let Some(handle) = existing {
        return Some(handle);
    }
    let (config, engine, snapshot) = {
        let state = cx.global::<AppState>();
        (
            state.config.clone(),
            state.engine.clone(),
            state.snapshot.clone(),
        )
    };
    let cfg = config.borrow().get().osd.clone();
    let primary_id = cx.primary_display().map(|display| display.id());
    let area = cx
        .primary_display()
        .map(|display| display.visible_bounds())
        .unwrap_or_else(|| Bounds::new(point(px(0.), px(0.)), size(px(1920.), px(1080.))));
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds::new(
            point(area.origin.x, area.origin.y),
            size(
                px(crate::windows::osd::card_size(&cfg).0),
                px(crate::windows::osd::card_size(&cfg).1),
            ),
        ))),
        kind: WindowKind::PopUp,
        display_id: primary_id,
        titlebar: Some(TitlebarOptions {
            appears_transparent: true,
            ..Default::default()
        }),
        window_background: WindowBackgroundAppearance::Transparent,
        window_decorations: Some(WindowDecorations::Client),
        // 绝不抢焦点：提示条弹出时正在放歌 / 打游戏的那个窗口必须还是前台。
        focus: false,
        show: false,
        is_movable: false,
        is_resizable: false,
        is_minimizable: false,
        ..Default::default()
    };
    let handle = match cx.open_window(options, move |window, cx| {
        platform::configure_osd(window);
        platform::set_opacity(window, 0.0);
        window.refresh();
        cx.new(|_| crate::windows::Osd::new(config.clone(), engine.clone(), snapshot.clone()))
    }) {
        Ok(handle) => handle,
        Err(error) => {
            eprintln!("无法创建 OSD 窗口：{error}");
            return None;
        }
    };
    // 这扇窗钉在哪块屏上：`place()` 用它算工作区，不再看配置里的 `monitor`。
    let _ = handle.update(cx, |osd, _, _| osd.set_monitor(monitor));
    cx.global::<AppState>()
        .osd
        .borrow_mut()
        .push((monitor, handle.clone()));
    Some(handle)
}

/// 收掉不在 `keys` 里的 OSD 窗口（从「全部显示器」换回「指定一台」时用）。
///
/// 每扇窗都是一份 DirectComposition 表面，留着不删会一直占着显存。
fn prune_osd_windows(cx: &mut App, keys: &[u32]) {
    let stale: Vec<(u32, WindowHandle<crate::windows::Osd>)> = {
        let list = cx.global::<AppState>().osd.borrow();
        list.iter()
            .filter(|(key, _)| !keys.contains(key))
            .cloned()
            .collect()
    };
    if stale.is_empty() {
        return;
    }
    cx.global::<AppState>()
        .osd
        .borrow_mut()
        .retain(|(key, _)| keys.contains(key));
    for (_, handle) in stale {
        let _ = handle.update(cx, |_, window, _| window.remove_window());
    }
}

/// 弹一条提示。配置里关了这个功能就什么都不做。
pub fn show_osd(cx: &mut App, request: OsdRequest) {
    let (enabled, filter, blocked) = {
        let state = cx.global::<AppState>();
        let config = state.config.borrow();
        let cfg = config.get();
        (
            cfg.osd.shows(request.function),
            cfg.osd.clone(),
            app_blocked(cfg),
        )
    };
    // 黑名单 / 全屏让路：整条提示都不弹（和 OSD 自己的按应用过滤叠加）。
    let enabled = enabled && !blocked && osd_allowed(&filter);
    crate::platform::hotkey::trace(&format!(
        "show_osd fn={:?} enabled={enabled}",
        request.function
    ));
    if !enabled {
        return;
    }
    let keys = crate::windows::osd::monitor_keys(&filter);
    prune_osd_windows(cx, &keys);
    for key in keys {
        if let Some(handle) = ensure_osd_window(cx, key) {
            let _ = handle.update(cx, |osd, window, cx| {
                osd.present(request.clone(), window, cx)
            });
        }
    }
}

/// 打开 OSD 布局编辑器（已经有了就提到前面）。
///
/// 设置页里的预览只有 500 多像素宽，卡片只能 1:1 画；这里单独开一扇
/// 1000×680、可以拖边改大小的窗口，卡片按空间放大到 6 倍。
pub fn open_osd_editor(cx: &mut App) {
    let existing = cx.global::<AppState>().osd_editor.borrow().clone();
    if let Some(handle) = existing {
        match handle.update(cx, |_, window, _| platform::raise(window)) {
            Ok(()) => return,
            Err(_) => {
                cx.global::<AppState>().osd_editor.borrow_mut().take();
            }
        }
    }
    let config = cx.global::<AppState>().config.clone();
    let lang = config.borrow().get().language;
    let Some(display) = cx.primary_display() else {
        return;
    };
    let area = display.visible_bounds();
    let window_size = size(px(1000.), px(680.));
    let bounds = Bounds::new(
        point(
            area.origin.x + (area.size.width - window_size.width) / 2.0,
            area.origin.y + (area.size.height - window_size.height) / 2.0,
        ),
        window_size,
    );
    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        kind: WindowKind::Normal,
        display_id: cx.primary_display().map(|display| display.id()),
        titlebar: Some(TitlebarOptions {
            title: Some(lang.pick("OSD 布局编辑器", "OSD layout editor").into()),
            appears_transparent: true,
            ..Default::default()
        }),
        window_background: WindowBackgroundAppearance::Opaque,
        window_decorations: Some(WindowDecorations::Client),
        focus: true,
        show: true,
        is_movable: true,
        is_resizable: true,
        is_minimizable: false,
        window_min_size: Some(size(px(560.), px(320.))),
        ..Default::default()
    };
    let opened = cx.open_window(options, move |window, cx| {
        platform::configure_settings_window(window);
        cx.new(|cx| crate::windows::OsdEditor::new(config.clone(), cx))
    });
    if let Ok(handle) = opened {
        cx.global::<AppState>()
            .osd_editor
            .borrow_mut()
            .replace(handle);
    }
}

/// 黑名单命中：前台应用在名单里，或者开着「全屏状态禁用」且此刻是全屏。
///
/// 命中时快捷键 / 滚轮 / 提示条全部让路。托盘图标手动点开面板**不拦** ——
/// 那是用户明确喊出来的操作，拦了连改设置都进不去。
///
/// 比对范围是**前台进程连同它的祖先进程**：DNF 这类游戏把窗口交给子进程
/// （`CrossProxy.exe`）画，只看窗口那个进程的话「禁用 DNF.exe」永远不生效。
pub fn app_blocked(config: &crate::config::Config) -> bool {
    let chain = crate::platform::fullscreen::foreground_process_chain();
    // 「全屏例外」：名单里的程序全屏时照常工作，所以全屏这条不成立。
    if config.app_block.fullscreen
        && crate::platform::fullscreen::is_fullscreen()
        && !config.app_block.exempts_fullscreen(&chain)
    {
        return true;
    }
    chain.iter().any(|exe| config.app_block.blocks_exe(exe))
}

/// 只看名单（不看全屏开关）。钩子线程里用这份：`SHQueryUserNotificationState`
/// 那种壳层调用不敢在低级钩子里做，慢一下就把按键拖丢了。
pub fn blocked_by_list(config: &crate::config::Config) -> bool {
    if config.app_block.entries.is_empty() {
        return false;
    }
    crate::platform::fullscreen::foreground_process_chain()
        .iter()
        .any(|exe| config.app_block.blocks_exe(exe))
}

/// 这条提示该不该弹（OSD 自己的按应用过滤）。
fn osd_allowed(cfg: &crate::config::OsdConfig) -> bool {
    use crate::config::OsdFilterMode;

    if !cfg.filter_mode.uses_list() {
        return true;
    }
    let name = crate::platform::fullscreen::foreground_process_name();
    // 取不到进程名就照常弹（宁可多弹，别因为读不到就永远不弹）；
    // 自己在设置页里点「保持显示」时前台就是 Bolume，也不能被名单挡掉。
    if name.is_empty() || name.starts_with("bolume") {
        return true;
    }
    let listed = cfg
        .filter_apps
        .iter()
        .any(|app| app.eq_ignore_ascii_case(&name));
    match cfg.filter_mode {
        OsdFilterMode::Off => true,
        OsdFilterMode::Blacklist => !listed,
        OsdFilterMode::Whitelist => listed,
    }
}

/// 提示条是不是被「保持显示」钉住了。
pub fn osd_pinned(cx: &App) -> bool {
    cx.global::<AppState>()
        .osd
        .borrow()
        .first()
        .and_then(|(_, handle)| handle.read_with(cx, |osd, _| osd.is_pinned()).ok())
        .unwrap_or(false)
}

/// 钉住 / 松开提示条（设置页的「保持显示」按钮）。
///
/// 状态由设置页自己拿着：这里只负责把命令送到窗口上。
/// 「全部显示器」时每块屏那张一起钉。
pub fn set_osd_pinned(cx: &mut App, pinned: bool) {
    let keys = {
        let config = cx.global::<AppState>().config.borrow();
        crate::windows::osd::monitor_keys(&config.get().osd)
    };
    for key in keys {
        if let Some(handle) = ensure_osd_window(cx, key) {
            let _ = handle.update(cx, |osd, window, cx| osd.set_pinned(pinned, window, cx));
        }
    }
}

/// 收掉设置页「保持显示」钉住的提示条（设置窗关掉时调用）。
///
/// 只动**已经存在**的提示条窗口：为了隐藏一张卡片去新建一扇窗没有意义，
/// 而且这时候用户已经离开设置页了，不该再弹任何东西出来。
pub fn release_osd_pins(cx: &mut App) {
    let handles: Vec<_> = cx
        .global::<AppState>()
        .osd
        .borrow()
        .iter()
        .map(|(_, handle)| handle.clone())
        .collect();
    for handle in handles {
        let _ = handle.update(cx, |osd, window, cx| osd.release_pin(window, cx));
    }
}

/// 全局快捷键分发。
///
/// 匹配规则与 EarTrumpet 的 `AppSettings.RegisterHotkeys` 一致：按配置顺序找到
/// 第一个组合相等的动作就执行，多个动作录了同一个键时只有第一个生效。
fn dispatch_hotkey(cx: &mut App, hotkey: crate::config::Hotkey) {
    dispatch_hotkey_inner(cx, hotkey, false);
}

/// `from_repeat` = 这一发是「按住连发」补出来的：动作照做，但别再起一个新的
/// 连发任务 —— 否则每补一发都会再挂一个定时器，越按越快。
fn dispatch_hotkey_inner(cx: &mut App, hotkey: crate::config::Hotkey, from_repeat: bool) {
    // 正在录快捷键：这个组合是给设置页收的，别顺手把音量改了。
    let settings_handle = cx.global::<AppState>().settings.borrow().clone();
    if let Some(handle) = settings_handle {
        let delivered = handle
            .update(cx, |settings, window, cx| {
                if settings.is_capturing() {
                    settings.accept_captured(hotkey, window, cx);
                    true
                } else {
                    false
                }
            })
            .unwrap_or(false);
        if delivered {
            return;
        }
    }

    let action = {
        let state = cx.global::<AppState>();
        let config = state.config.borrow();
        // 没给音量键配快捷键时，开了「禁用 Windows 音量条」就按默认音量动作走 ——
        // 键已经被我们吞了，再没人处理的话按音量键会变成什么都不发生。
        crate::platform::hotkey::action_for(config.get(), hotkey).or_else(|| {
            if config.get().osd.block_system_osd && hotkey.mods == 0 {
                crate::platform::hotkey::volume_action(hotkey.key)
            } else {
                None
            }
        })
    };
    use crate::platform::hotkey::Action;
    crate::platform::hotkey::trace(&format!(
        "dispatch mods={} key={} action={action:?} repeat={from_repeat}",
        hotkey.mods, hotkey.key
    ));

    // 黑名单 / 全屏在前台时不响应快捷键：游戏里误触音量键、切歌键非常烦人。
    // 「打开设置」例外 —— 那是用户主动喊出来的，不该被拦（否则黑名单写错了就再也进不去）。
    if action.is_some() && !matches!(action, Some(Action::Settings)) {
        let block = {
            let state = cx.global::<AppState>();
            let config = state.config.borrow();
            app_blocked(config.get())
        };
        // 「禁用 Windows 音量条」开着的时候，音量键要在黑名单里也照常生效：
        // 那个开关的卖点就是「任何程序里都不弹」，游戏里按音量键没反应更奇怪。
        // 这类键不转发、就地执行，既改了音量又不会让系统弹条。
        let kill_osd = {
            let state = cx.global::<AppState>();
            let config = state.config.borrow();
            config.get().osd.block_system_osd
        };
        let silent_volume = kill_osd
            && hotkey.mods == 0
            && matches!(
                action,
                Some(Action::VolumeUp) | Some(Action::VolumeDown) | Some(Action::MediaMute)
            );
        if block && !silent_volume {
            // 键已经被（注册热键或低级钩子）吃下来了，这里不能再让它凭空消失：
            // 裸的媒体键 / 音量键重新注入一遍，等于 Bolume 从没碰过它 ——
            // 游戏里按播放暂停，该由游戏 / 播放器自己收。
            // 用户自己配的组合键（`Ctrl+Alt+0` 那种）不转发：那是专门配给 Bolume 的，
            // 让路就该什么都不做，不能凭空变成一次切歌。
            // 音量键在 `kill_osd` 时走不到这里（上面被 `silent_volume` 放行），
            // 所以这里能转发的一定是媒体键 —— 它们不会弹系统 OSD。
            let raw_key = crate::platform::hotkey::is_raw_system_key(hotkey.key, hotkey.mods);
            if raw_key {
                if let Some(action) = action {
                    if crate::platform::hotkey::forward_to_system(action) {
                        crate::platform::hotkey::trace("dispatch forwarded to system: app blocked");
                        return;
                    }
                }
            }
            crate::platform::hotkey::trace("dispatch skipped: app blocked");
            return;
        }
    }

    match action {
        Some(Action::Flyout) => toggle_flyout(cx),
        Some(Action::Settings) => open_settings(cx),
        Some(Action::VolumeUp) => absolute_volume(cx, 1.0),
        Some(Action::VolumeDown) => absolute_volume(cx, -1.0),
        Some(Action::AppVolumeUp) => adjust_app_volume(cx, 1.0),
        Some(Action::AppVolumeDown) => adjust_app_volume(cx, -1.0),
        // 屏幕亮度：当前（鼠标所在屏）/ 全部 / 指定编号。
        Some(Action::BrightnessUp) => nudge_brightness(cx, BrightnessTarget::Cursor, 1.0),
        Some(Action::BrightnessDown) => nudge_brightness(cx, BrightnessTarget::Cursor, -1.0),
        Some(Action::BrightnessAllUp) => nudge_brightness(cx, BrightnessTarget::All, 1.0),
        Some(Action::BrightnessAllDown) => nudge_brightness(cx, BrightnessTarget::All, -1.0),
        Some(Action::BrightnessMonitorUp) => {
            let number = {
                let state = cx.global::<AppState>();
                let config = state.config.borrow();
                config.get().brightness.hotkey_monitor
            };
            nudge_brightness(cx, BrightnessTarget::Index(number), 1.0);
        }
        Some(Action::BrightnessMonitorDown) => {
            let number = {
                let state = cx.global::<AppState>();
                let config = state.config.borrow();
                config.get().brightness.hotkey_monitor
            };
            nudge_brightness(cx, BrightnessTarget::Index(number), -1.0);
        }
        // 静音键：OSD 开着就自己切（否则会和系统自带的音量提示条叠在一起），
        // 关着就照旧交给系统，行为和以前一样。
        Some(Action::MediaMute) => {
            let show_osd = cx
                .global::<AppState>()
                .config
                .borrow()
                .get()
                .osd
                .shows(OsdFunction::Mute);
            if show_osd {
                toggle_default_mute(cx);
            } else {
                crate::platform::hotkey::send_media_key(crate::platform::hotkey::VK_VOLUME_MUTE);
            }
        }
        // 其余媒体键：原样注入给播放器，同时弹一条提示（系统自己不弹）。
        Some(action) => {
            if let Some(key) = action.media_key() {
                // 播放 / 暂停共用一个键，先问系统现在在不在放，再发键 ——
                // 发完再问就晚了，状态已经翻过来了。
                let playing = (action == Action::MediaPlayPause).then(|| toggled_playback(cx));
                crate::platform::hotkey::send_media_key(key);
                let function = match action {
                    Action::MediaPrev => OsdFunction::PrevTrack,
                    Action::MediaNext => OsdFunction::NextTrack,
                    Action::MediaPlayPause => OsdFunction::PlayPause,
                    Action::MediaStop => OsdFunction::Stop,
                    _ => return,
                };
                let mut request = OsdRequest::device(function);
                if let Some(playing) = playing {
                    request = request.playing(playing);
                }
                show_osd(cx, request);
            }
        }
        // 没配快捷键的媒体键：钩子只在开了「接管媒体键」时才把它送过来，
        // 这里弹 OSD 再把键转发给播放器，和配了快捷键时行为一致。
        None => {
            if hotkey.mods == 0 {
                if let Some(action) = crate::platform::hotkey::media_action(hotkey.key) {
                    let playing = (action == Action::MediaPlayPause).then(|| toggled_playback(cx));
                    crate::platform::hotkey::send_media_key(hotkey.key);
                    let function = match action {
                        Action::MediaPrev => OsdFunction::PrevTrack,
                        Action::MediaNext => OsdFunction::NextTrack,
                        Action::MediaPlayPause => OsdFunction::PlayPause,
                        Action::MediaStop => OsdFunction::Stop,
                        _ => return,
                    };
                    let mut request = OsdRequest::device(function);
                    if let Some(playing) = playing {
                        request = request.playing(playing);
                    }
                    show_osd(cx, request);
                }
            }
        }
    }

    // 音量 / 亮度 / 切歌可以按住连发；静音、播放暂停、面板、设置按一下是一下。
    //
    // 具体哪些动作连发由配置决定（设置页里每个快捷键前面那个勾），这里只问一句。
    // 没配快捷键的媒体键走的是上面那条 `None` 分支（`action` 是空的），要按
    // 「这颗键本来是哪个动作」再判一次 —— 否则同一颗切歌键，配了快捷键能连发、
    // 只靠「接管媒体键」时不能，用户只会觉得按快了才灵。
    let repeat = action.or_else(|| {
        if hotkey.mods == 0 {
            crate::platform::hotkey::media_action(hotkey.key)
        } else {
            None
        }
    });
    let repeats = match repeat {
        Some(action) => {
            let state = cx.global::<AppState>();
            let config = state.config.borrow();
            crate::platform::hotkey::repeats_on_hold(config.get(), action)
        }
        None => false,
    };
    if !from_repeat && repeats {
        start_hold_repeat(cx, hotkey);
    }
}

/// 播放 / 暂停键按下去之后是新状态（`true` = 开始播，`false` = 停下来）。
///
/// 先问系统媒体会话（SMTC）：这是唯一能分辨「播放中 / 已暂停」的正经来源。
/// 问不出来（播放器不汇报、系统太老）就看有没有会话在出声 —— 有声音按下去
/// 这一键就是暂停；没声音就是开始播。完全没播放器时也按「开始播」算。
fn toggled_playback(cx: &mut App) -> bool {
    if let Some(playing) = crate::platform::media::is_playing() {
        return !playing;
    }
    let (_, _, _, shared) = cx.global::<AppState>().clone_handles();
    let snapshot = shared.borrow().clone();
    let audible = snapshot
        .apps
        .iter()
        .any(|app| !app.is_system && app.is_playing());
    !audible
}

/// 切换默认播放设备的静音状态（静音键走这里，顺带弹 OSD）。
fn toggle_default_mute(cx: &mut App) {
    let (engine, _, _, shared) = cx.global::<AppState>().clone_handles();
    let overrides = cx.global::<AppState>().mute_override.clone();
    let snapshot = shared.borrow().clone();
    let Some(device) = snapshot.hotkey_device() else {
        return;
    };
    let key = format!("device:{}", device.id);
    // toggle 的基准取「界面上现在显示的那个喇叭」：里面已经把用户刚按下的意图
    // 算进去了。直接取快照的话，快照还没回读，连按两下会连发两条一样的命令。
    let shown = shown_muted(&overrides, &key, device.muted);
    let muted = !shown;
    note_muted(&overrides, &key, muted);
    engine.send(AudioCommand::SetDeviceMute {
        device_id: device.id.clone(),
        muted,
    });
    show_osd(cx, OsdRequest::device(OsdFunction::Mute).muted(muted));
}

/// 调「当前默认设备」的音量（键盘音量键 / 全局滚轮）。
///
/// 只动默认设备那一台：Windows 给每台设备各存一份音量，把所有启用设备一起刷成
/// 同一个值，会把用户另一台设备的音量也一起改掉 —— 切过去就发现「怎么也被提升了」。
/// 步进三种算法（百分比 / 跟随对数刻度 / 固定 dB）都在 `Config::hotkey_step_display` 里，
/// 这里只负责挑刻度、夹上限。
fn absolute_volume(cx: &mut App, direction: f32) {
    let (engine, config, _, shared) = cx.global::<AppState>().clone_handles();
    let cfg = config.borrow().get().clone();
    let snapshot = shared.borrow().clone();
    let min_db = cfg.logarithmic_min_db as f32;

    let Some(device) = snapshot.hotkey_device() else {
        return;
    };

    // 不再跳过静音的设备：静音时按音量键应该「解除静音 + 调音量」，
    // 至于解不解除由设置里的「调音量时解除静音」决定（逐个方向）。
    let unmute = cfg
        .unmute
        .allows(crate::config::VolumeGesture::Hotkey, direction > 0.0);
    let display = if cfg.use_logarithmic_volume {
        to_display_volume(device.volume, min_db)
    } else {
        device.volume
    };
    let next = cfg.hotkey_step_display(crate::config::HotkeyStepTarget::Device, display, direction);
    let next = if cfg.use_logarithmic_volume {
        to_log_volume(next, min_db)
    } else {
        next
    };
    // 上限在这里也夹一下：音频线程最后还会再夹一次，但滑条读数不该先冲上去再被拉回。
    let next = match cfg.volume_cap.limit_for(&device.id) {
        Some(limit) => next.min(limit),
        None => next,
    };
    engine.send(AudioCommand::SetDeviceVolume {
        device_id: device.id.clone(),
        volume: next,
        unmute,
    });
    let predicted = if cfg.use_logarithmic_volume {
        to_display_volume(next, min_db)
    } else {
        next
    };
    let mut request = OsdRequest::device(OsdFunction::Volume).predicted(predicted);
    // 这一键会顺手解除静音：图标和灰条也要当场跟着变。只报音量的话，耳朵先听到、
    // 画面上的静音图标和灰条要等快照回读才更新，就是那股「慢半拍」的迟钝感。
    if unmute && device.muted {
        request = request.muted(false);
    }
    show_osd(cx, request);
    // 音效在「真的变了一档」时才响：到顶 / 到底那几下按了也是同一个值。
    // 声音本身在 `bolume-sound` 线程上播（见 `platform::sound`），这里只排队。
    if (predicted - display).abs() > 0.0001 {
        crate::platform::sound::step(&cfg, crate::config::VolumeGesture::Hotkey);
    }
}

/// 亮度快捷键管哪几台屏。
#[derive(Clone, Copy, PartialEq, Eq)]
enum BrightnessTarget {
    /// 鼠标所在的那块。
    Cursor,
    /// 所有能读到亮度的。
    All,
    /// 配置里指定的第 N 台（1 起）。
    Index(u32),
}

/// 亮度键：按 `brightness.step` 推一档。
///
/// 只改能读到亮度的屏（`brightness: None` = DDC/CI 不通，笔记本内屏、转接线、
/// 关掉 DDC 的显示器都在这一类），碰到就跳过，不硬写。写完顺手把亮度面板
/// 亮一下当反馈 —— 面板本来就是「正在改哪个」最直观的地方。
fn nudge_brightness(cx: &mut App, target: BrightnessTarget, direction: f32) {
    let (config, engine, displays) = {
        let state = cx.global::<AppState>();
        (
            state.config.clone(),
            state.display_engine.clone(),
            state.displays.clone(),
        )
    };
    // 快捷键可以单独设步进（0 = 跟随面板那个「亮度步进」）。
    let step = {
        let cfg = config.borrow();
        let brightness = &cfg.get().brightness;
        let raw = if brightness.hotkey_step == 0 {
            brightness.step
        } else {
            brightness.hotkey_step
        };
        raw.max(1) as f32
    };
    let list = displays.borrow().clone();
    if list.is_empty() {
        return;
    }
    let last = list.len() - 1;
    let targets: Vec<usize> = match target {
        BrightnessTarget::All => (0..list.len()).collect(),
        BrightnessTarget::Cursor => vec![crate::display::monitor_index_at_cursor().min(last)],
        // 编号写大了（换了台少一台的机器）就落到最后一台，别按了没反应。
        BrightnessTarget::Index(number) => vec![(number.saturating_sub(1) as usize).min(last)],
    };

    let mut changed = false;
    let mut shown: Option<f32> = None;
    for index in targets {
        let info = &list[index];
        let Some(current) = info.brightness else {
            continue;
        };
        let next = (current as f32 + step * direction)
            .round()
            .clamp(0.0, 100.0) as u32;
        if next == current {
            continue;
        }
        engine.send(crate::display::DisplayCommand::SetBrightness {
            key: info.key.clone(),
            value: next,
        });
        // 记一笔目标：工作线程写完会把读数报回来，那是**这一刻之前**的值，
        // 直接采信等于把预测退回一档（见 `brightness_override`）。
        note_brightness(cx, &info.key, next);
        // 本地先摆上：DDC 回读要几十毫秒，连发时下一步不该拿旧读数再算一遍。
        if let Some(entry) = displays.borrow_mut().get_mut(index) {
            entry.brightness = Some(next);
        }
        shown = Some(next as f32 / 100.0);
        changed = true;
    }

    if !changed {
        return;
    }
    // 面板要是正开着（用户自己拉开的），让它按新读数重画一次，滑条不会停在旧位置上。
    refresh_brightness_window(cx);
    // 亮度的提示条：以前是「屏幕本身就是反馈」，现在跟音量一样弹一条，
    // 外观在布局编辑器里单独调（亮度是第三套模板）。
    // 多台一起调（All）时报最后一台 —— 步进一样，读数也一样。
    if let Some(level) = shown {
        show_osd(cx, OsdRequest::device(OsdFunction::Brightness).predicted(level));
    }
}

/// 「按住连发」的令牌：每按一次 +1，跑着的重复任务发现令牌变了就自己退场。
///
/// 只用一个原子量：连发任务跑在 gpui 的执行器上，按键处理在事件回调里，
/// 两边要同步的就这一个数字。
static HOLD_REPEAT_TOKEN: AtomicU64 = AtomicU64::new(0);

/// 按住音量 / 亮度 / 切歌键时，按设置的间隔一档档往下走；键一松就停。
///
/// 钩子把按键吞掉之后，系统自己的「按住重复」不会再来（那套逻辑在被吞掉的
/// keydown 之后就断了），所以重复必须自己做。判断松没松走 `is_key_held`：
/// 被吞过的键（媒体 / 音量键）在系统那边当没按过，只能问钩子自己的按下表。
fn start_hold_repeat(cx: &mut App, hotkey: crate::config::Hotkey) {
    let (enabled, interval, delay) = {
        let state = cx.global::<AppState>();
        let config = state.config.borrow();
        (
            config.get().hotkey_hold_repeat,
            config.get().hotkey_repeat_interval,
            config.get().hotkey_repeat_delay,
        )
    };
    // 鼠标键（伪键码 0x100 起）没有「按住」这回事：一格滚轮就是一下。
    if !enabled || hotkey.key == 0 || crate::platform::hotkey::is_mouse_key(hotkey.key) {
        return;
    }
    let (low, high) = crate::config::HOTKEY_REPEAT_RANGE;
    let interval = interval.clamp(low, high) as u64;
    // 延迟可以设成 0：这时第一档照旧等一个间隔，和加这一项之前一模一样。
    let (dlow, dhigh) = crate::config::HOTKEY_REPEAT_DELAY_RANGE;
    let delay = delay.clamp(dlow, dhigh) as u64;
    let token = HOLD_REPEAT_TOKEN.fetch_add(1, Ordering::SeqCst) + 1;
    cx.spawn(async move |cx| {
        // 第一档之前先等这么久 —— 系统那套「键盘字符重复延迟」的等价物。
        // 0 就不额外等（下面循环第一次等的还是间隔）。
        let mut wait = if delay > 0 { delay } else { interval };
        loop {
            cx.background_executor()
                .timer(std::time::Duration::from_millis(wait))
                .await;
            wait = interval;
            if HOLD_REPEAT_TOKEN.load(Ordering::SeqCst) != token {
                return;
            }
            if !crate::platform::hotkey::is_key_held(hotkey.key) {
                return;
            }
            let alive = cx.update(|cx| {
                if HOLD_REPEAT_TOKEN.load(Ordering::SeqCst) != token {
                    return false;
                }
                dispatch_hotkey_inner(cx, hotkey, true);
                true
            });
            if !alive {
                return;
            }
        }
    })
    .detach();
}

/// 快捷键调「当前应用」的音量。
///
/// 「当前应用」= 此刻正在发声的那个（`AppInfo::is_playing`，即 Active 且电平不为零）；
/// 同时有几个在响就挑最响的那个 —— 用户按音量键时心里想的多半是耳朵里最吵的那个。
/// 一个都没在响时退一步取列表里的第一个非系统应用，免得快捷键完全没反应。
fn adjust_app_volume(cx: &mut App, direction: f32) {
    let (engine, config, _, shared) = cx.global::<AppState>().clone_handles();
    let cfg = config.borrow().get().clone();
    let snapshot = shared.borrow().clone();

    let Some(app) = snapshot.current_app() else {
        return;
    };

    let min_db = cfg.logarithmic_min_db as f32;
    let display = if cfg.use_logarithmic_volume {
        to_display_volume(app.volume, min_db)
    } else {
        app.volume
    };
    let next = cfg.hotkey_step_display(crate::config::HotkeyStepTarget::App, display, direction);
    let volume = if cfg.use_logarithmic_volume {
        to_log_volume(next, min_db)
    } else {
        next
    };
    let unmute = cfg
        .unmute
        .allows(crate::config::VolumeGesture::AppHotkey, direction > 0.0);
    engine.send(AudioCommand::SetAppVolume {
        key: app.key.clone(),
        volume,
        unmute,
    });
    show_osd(cx, {
        let mut request =
            OsdRequest::app(OsdFunction::Volume, app.key.clone(), app.name.clone()).predicted(next);
        // 同上：顺手解除静音时，图标与灰条要当场变。
        if unmute && app.muted {
            request = request.muted(false);
        }
        request
    });
    if (next - display).abs() > 0.0001 {
        crate::platform::sound::step(&cfg, crate::config::VolumeGesture::AppHotkey);
    }
}

/// 托盘菜单里点了某台设备：切换默认播放设备。
fn set_default_device(cx: &mut App, device_id: String) {
    let engine = cx.global::<AppState>().engine.clone();
    engine.send(AudioCommand::SetDefaultDevice { device_id });
}

/// 托盘菜单里的「重启」：拉起一个等锁的副本，然后自己退场。
///
/// 必须先起副本再退：反过来，自己退出之后就没东西去启动了。
/// 副本带 `--restart-wait`，会等这边的单实例锁释放再接手（见 `platform::restart`）。
fn restart_app(cx: &mut App) {
    if !crate::platform::restart::spawn() {
        eprintln!("重启失败：新进程没起来，继续用当前实例。");
        return;
    }
    quit_now(cx)
}

/// 退场：先把托盘图标摘掉，再直接结束进程。
///
/// 不走「`cx.quit()` → 等 gpui 收尾 → 各项 Drop 慢慢清理」那条路：
/// 实测在本机（Windows 11 / RTX 5070 Ti）事件循环结束后、`run()` 还没返回时就会
/// 访问违例（日志最后一行是 `foreground task timeout reached`，退出码 0xC0000005），
/// 于是清理走到一半就断了 —— 露出来的现象是「重启 / 退出之后托盘里留一个点不动的
/// 幽灵图标」，重启还得多等一会儿副本才抢到锁。
///
/// 该由 Drop 干的那几件事，这里显式做掉：摘托盘图标。其余（配置、钩子、窗口、
/// 单实例锁）本来就是立刻生效或者随进程结束由系统回收的。
fn quit_now(cx: &mut App) -> ! {
    if let Some(state) = cx.try_global::<AppState>() {
        state.tray.remove();
    }
    // 接管期间被我们压到 2% 的应用，要在这里还回去：`process::exit` 不跑析构，
    // 接管线程的 `close()` 根本没机会执行。
    crate::audio::boost::release_all();
    // 被最小化的系统音量条也要放回来，否则用户下次开机会以为系统坏了。
    crate::platform::system_osd::restore();
    std::process::exit(0)
}

/// 托盘滚轮：按配置的步进调整当前默认设备音量。
fn adjust_device_volume(cx: &mut App, step: i32) {
    let (engine, config, _, shared) = cx.global::<AppState>().clone_handles();
    let cfg = config.borrow().get().clone();
    if !cfg.use_scroll_wheel_in_tray || step == 0 {
        return;
    }
    // 黑名单 / 全屏让路时滚轮也不接管（托盘图标本身还是能点）。
    if app_blocked(&cfg) {
        return;
    }
    let snapshot = shared.borrow().clone();
    let Some(device_id) = snapshot.current_device_id.clone() else {
        return;
    };
    let Some(device) = snapshot.devices.iter().find(|d| d.id == device_id) else {
        return;
    };
    let min_db = cfg.logarithmic_min_db as f32;
    let display = if cfg.use_logarithmic_volume {
        to_display_volume(device.volume, min_db)
    } else {
        device.volume
    };
    let ceiling = cap_display(cfg.volume_cap.limit_for(&device.id), &cfg, min_db);
    let next = cfg.scroll_display(display, step as f32).min(ceiling);
    let raw = if cfg.use_logarithmic_volume {
        to_log_volume(next, min_db)
    } else {
        next
    };
    let unmute = cfg
        .unmute
        .allows(crate::config::VolumeGesture::TrayScroll, step > 0);
    engine.send(AudioCommand::SetDeviceVolume {
        device_id,
        volume: raw,
        unmute,
    });
    show_osd(cx, {
        let mut request = OsdRequest::device(OsdFunction::Volume).predicted(next);
        // 同上：顺手解除静音时，图标与灰条要当场变。
        if unmute && device.muted {
            request = request.muted(false);
        }
        request
    });
    if (next - display).abs() > 0.0001 {
        crate::platform::sound::step(&cfg, crate::config::VolumeGesture::TrayScroll);
    }
}

/// 托盘右键菜单。
///
/// 顶部直接列出全部**已启用**的播放设备（勾选当前默认那台），点一下就切过去；
/// 下面才是面板 / 设置 / EarTrumpet 的「Windows Legacy」系统入口 / 退出。
///
/// 菜单项用固定 id，切语言或设备变化时整块重建也不会丢失事件对应关系。
/// 菜单里一个动作的本地化文案；返回 `None` 表示这个 id 不认识（老配置里的废弃项）。
pub(crate) fn tray_action_label(lang: Lang, id: &str) -> Option<&'static str> {
    Some(match id {
        // 文案统一成「新入口不加后缀 / 老面板加 [旧]」：不带标记的就是 Windows 设置那一套
        // （`ms-settings:`），带 [旧] 的是 mmsys.cpl 那一套（Win32 控制面板 / sndvol）。
        "toggle" => lang.pick("音量面板", "Volume panel"),
        "settings" => lang.pick("设置", "Settings"),
        "quit" => lang.pick("退出", "Exit"),
        // 这几项原来都收在「Windows 旧版声音设置」子菜单里，现在可以平铺也能自己收起来。
        "legacy-mixer" => lang.pick("音量合成器 [旧]", "Volume mixer [legacy]"),
        "legacy-playback" => lang.pick("播放设备", "Playback devices"),
        "legacy-recording" => lang.pick("录制设备", "Recording devices"),
        "legacy-sounds" => lang.pick("声音设置 [旧]", "Sound settings [legacy]"),
        "sound-settings" => lang.pick("声音设置", "Sound settings"),
        "apps-volume" => lang.pick("音量合成器", "Volume mixer"),
        "restart" => lang.pick("重启", "Restart"),
        _ => return None,
    })
}

/// 按配置里的布局生成托盘右键菜单。
///
/// 顺序、显隐、子菜单全部来自 `config.tray_menu`：
/// - `devices` 是动态块（当前启用的设备，勾选默认那台），其余是固定动作；
/// - 文件夹渲染成 `Submenu`，里面只放动作（不支持嵌套子菜单）；
/// - 分隔线就是配置里的 `separator` 条目：插了几条就是几条，渲染时不再自动补。
fn build_tray_menu(lang: Lang, devices: &[crate::audio::DeviceInfo], config: &Config) -> Menu {
    let menu = Menu::new();

    for node in &config.tray_menu.root {
        if !node.visible {
            continue;
        }
        if node.id == "devices" {
            for device in devices.iter().filter(|device| device.enabled) {
                // 自定义过名字就只显示自定义名（托盘菜单也不是设置页）。
                let label = if device.name.trim().is_empty() {
                    config.device_label(&device.id, &device.id)
                } else {
                    config.device_label(&device.id, &device.name)
                };
                let item = CheckMenuItem::with_id(
                    format!("device:{}", device.id),
                    label,
                    true,
                    device.is_default,
                    None,
                );
                let _ = menu.append(&item);
            }
            continue;
        }

        if node.is_separator() {
            let _ = menu.append(&PredefinedMenuItem::separator());
            continue;
        }

        if node.is_folder() {
            let submenu = Submenu::new(node.title.clone(), true);
            let mut any = false;
            for child in node.children.iter().filter(|child| child.visible) {
                let Some(label) = tray_action_label(lang, &child.id) else {
                    continue;
                };
                let item = MenuItem::with_id(child.id.clone(), label, true, None);
                let _ = submenu.append(&item);
                any = true;
            }
            // 空文件夹不进菜单：右键里挂一个点开啥都没有的子菜单很怪。
            if any {
                let _ = menu.append(&submenu);
            }
            continue;
        }

        let Some(label) = tray_action_label(lang, &node.id) else {
            continue;
        };
        let item = MenuItem::with_id(node.id.clone(), label, true, None);
        let _ = menu.append(&item);
    }

    menu
}

/// 按当前快照重建托盘菜单（设备集合 / 默认设备 / 语言变了都要调）。
fn rebuild_tray_menu(cx: &mut App) {
    let (devices, cfg, tray) = {
        let state = cx.global::<AppState>();
        let config = state.config.borrow();
        (
            state.snapshot.borrow().devices.clone(),
            config.get().clone(),
            state.tray.clone(),
        )
    };
    apply_tray_menu(&tray, &devices, &cfg);
}

/// 把菜单挂到托盘上：主题模式不挂原生菜单（右键走自绘窗口）。
fn apply_tray_menu(tray: &Rc<Tray>, devices: &[crate::audio::DeviceInfo], cfg: &Config) {
    if cfg.tray_menu_style == crate::config::TrayMenuStyle::Theme {
        tray.clear_menu();
        return;
    }
    tray.set_menu(build_tray_menu(cfg.language, devices, cfg));
}

/// 弹出应用右键菜单（面板里右键某个应用时调）。
///
/// `at` 是右键点在**屏幕**上的逻辑坐标。菜单是独立小窗，尺寸只跟内容走，
/// 不再受面板窗口限制 —— 竖向排版的面板只有一百多像素高，画在里面根本放不下。
pub(crate) fn open_app_menu(
    cx: &mut App,
    target: crate::windows::app_menu::MenuTarget,
    at: (f32, f32),
) {
    let handle = cx.global::<AppState>().app_menu.borrow().clone();
    let Some(handle) = handle else {
        return;
    };
    // 托盘菜单开着就先收掉：两层菜单叠一起没法点。
    hide_tray_menu(cx);
    let _ = handle.update(cx, |menu, window, cx| {
        menu.show_for(target, at, window, cx);
    });
}

/// 应用右键菜单开着没有（面板那边要据此决定挂不挂悬停提示）。
pub(crate) fn app_menu_visible(cx: &mut App) -> bool {
    let handle = cx.global::<AppState>().app_menu.borrow().clone();
    handle
        .map(|handle| {
            handle
                .update(cx, |menu, _, _| menu.is_visible())
                .unwrap_or(false)
        })
        .unwrap_or(false)
}

/// 收起应用右键菜单（面板收起、点了面板别处、打开设置时调）。
pub(crate) fn hide_app_menu(cx: &mut App) {
    let handle = cx.global::<AppState>().app_menu.borrow().clone();
    if let Some(handle) = handle {
        let _ = handle.update(cx, |menu, window, cx| {
            if menu.is_visible() {
                menu.hide(window, cx);
            }
        });
    }
}

/// 应用右键菜单里的一项被点了。
pub(crate) fn run_app_menu_action(
    cx: &mut App,
    target: &crate::windows::app_menu::MenuTarget,
    action: crate::windows::app_menu::Action,
) {
    use crate::windows::app_menu::Action;
    let (engine, config, flyout) = {
        let state = cx.global::<AppState>();
        (
            state.engine.clone(),
            state.config.clone(),
            state.flyout.borrow().clone(),
        )
    };
    match action {
        Action::Rename => {
            // 改名还是留在面板里（要用面板窗口的输入法焦点）。
            let Some(flyout) = flyout else {
                return;
            };
            let (x, y) = target.anchor;
            let _ = flyout.update(cx, |flyout, window, cx| {
                flyout.begin_rename(&target.key, &target.name, x, y, window, cx);
            });
        }
        Action::ToggleHidden => {
            let key = target.key.clone();
            let hidden = !target.hidden;
            let _ = config.borrow_mut().update(|config| {
                config.hidden_apps.retain(|existing| existing != &key);
                if hidden {
                    config.hidden_apps.push(key.clone());
                }
            });
            if let Some(flyout) = flyout {
                let _ = flyout.update(cx, |flyout, window, cx| {
                    // 折叠会改列表行数 / 列数，窗口尺寸得跟着重算。
                    flyout.update_frame(window, cx);
                    cx.notify();
                });
            }
        }
        Action::ToggleLoudness => {
            let on = !target.loudness;
            let mut changed = false;
            let _ = config.borrow_mut().update(|config| {
                changed = config.set_app_loudness(&target.key, Some(on));
            });
            if changed {
                sync_loudness(&engine, config.borrow().get());
            }
            cx.refresh_windows();
        }
        Action::Route(device_id) => {
            engine.send(AudioCommand::SetAppDevice {
                pid: target.pid,
                device_id,
            });
            cx.refresh_windows();
        }
    }
}

/// 右键托盘图标：弹自绘菜单（已经在显示就收起，第二次右键等于关掉）。
pub(crate) fn open_tray_menu(cx: &mut App) {
    // 应用菜单开着就先收掉：两层菜单叠一起没法点。
    hide_app_menu(cx);
    let (style, handle) = {
        let state = cx.global::<AppState>();
        let style = state.config.borrow().get().tray_menu_style;
        (style, state.tray_menu.borrow().clone())
    };
    if style != crate::config::TrayMenuStyle::Theme {
        return;
    }
    let Some(handle) = handle else {
        return;
    };
    let _ = handle.update(cx, |menu, window, cx| {
        if menu.is_visible() {
            menu.hide(window, cx);
        } else {
            menu.show(window, cx);
        }
    });
    watch_tray_menu(cx);
}

/// 收起自绘菜单（点面板 / 打开设置时用）。
pub(crate) fn hide_tray_menu(cx: &mut App) {
    let handle = cx.global::<AppState>().tray_menu.borrow().clone();
    if let Some(handle) = handle {
        let _ = handle.update(cx, |menu, window, cx| {
            if menu.is_visible() {
                menu.hide(window, cx);
            }
        });
    }
}

/// 菜单开着的时候盯着前台：点到别处 / 切走程序就收起。
///
/// 菜单窗口是**激活**状态（不激活就收不到点击别处的消息），所以判断依据是
/// 「它还是不是前台窗口」。刚弹出那一两百毫秒不判 —— `SetForegroundWindow`
/// 生效有延迟，太急会把菜单当成已经失焦给收了。
fn watch_tray_menu(cx: &mut App) {
    let Some(handle) = cx.global::<AppState>().tray_menu.borrow().clone() else {
        return;
    };
    cx.spawn(async move |cx| {
        cx.background_executor()
            .timer(std::time::Duration::from_millis(60))
            .await;
        loop {
            let open = handle
                .update(cx, |menu, _window, _cx| menu.is_visible())
                .unwrap_or(false);
            if !open {
                break;
            }
            // 光标划出菜单（但没点别处）时把悬浮高亮清掉：窗口还挂着一行亮条很怪。
            let _ = handle.update(cx, |menu, window, cx| {
                if menu.is_visible() && !platform::cursor_in_window(window) {
                    menu.clear_hover(cx);
                }
            });
            let active = handle
                .update(cx, |_menu, window, _cx| window.is_window_active())
                .unwrap_or(false);
            if !active {
                // 主路径是窗口激活事件（`TrayMenu::watch_blur`，收到就收）；
                // 这里只是兜底 —— 万一哪次 `activate` 没成功、事件压根不来，
                // 40ms 一轮也照样能发现。
                let _ = handle.update(cx, |menu, window, cx| menu.hide(window, cx));
                break;
            }
            cx.background_executor()
                .timer(std::time::Duration::from_millis(40))
                .await;
        }
    })
    .detach();
}

/// 菜单里点了一项：原生菜单和自绘菜单都走这一份分发。
pub(crate) fn run_menu_action(cx: &mut App, id: &str) {
    if let Some(device_id) = id.strip_prefix("device:") {
        set_default_device(cx, device_id.to_string());
        return;
    }
    match id {
        "toggle" => toggle_flyout(cx),
        "settings" => open_settings(cx),
        "quit" => quit_now(cx),
        "restart" => restart_app(cx),
        // EarTrumpet「Windows Legacy」子菜单里的系统入口。
        "legacy-mixer" => crate::platform::shell::legacy_mixer(),
        "legacy-playback" => crate::platform::shell::legacy_sound_panel("playback"),
        "legacy-recording" => crate::platform::shell::legacy_sound_panel("recording"),
        "legacy-sounds" => crate::platform::shell::legacy_sound_panel("sounds"),
        "sound-settings" => crate::platform::shell::settings_page("sound"),
        "apps-volume" => crate::platform::shell::settings_page("apps-volume"),
        _ => {}
    }
}

/// 语言变更后重建托盘菜单、同步设置窗口的任务栏标题（设置页调用）。
pub fn refresh_tray_menu(cx: &mut App) {
    let (lang, settings) = {
        let state = cx.global::<AppState>();
        (
            state.config.borrow().get().language,
            state.settings.borrow().clone(),
        )
    };
    rebuild_tray_menu(cx);

    if let Some(handle) = settings {
        let title = lang.pick("Bolume 设置", "Bolume Settings").to_string();
        let _ = handle.update(cx, |_, window, _| window.set_window_title(&title));
    }
}

/// 动态模式开关 / 效果 / 灵敏度变了之后调一次：通知音频线程要不要提频，
/// 顺便按当前状态先画一帧（不然要等最多 16ms 才见到变化）。
pub fn sync_tray_motion(cx: &mut App) {
    let (on, effect) = {
        let state = cx.global::<AppState>();
        let cfg = state.config.borrow().get().clone();
        (
            crate::tray::needs_frames(cfg.tray_icon_style, cfg.tray_color_by_level),
            cfg.tray_motion,
        )
    };
    let state = cx.global::<AppState>();
    state.engine.send(AudioCommand::SetAnimating(on));
    if on {
        let muted = state
            .snapshot
            .borrow()
            .devices
            .iter()
            .find(|device| device.is_default)
            .map(|device| device.muted)
            .unwrap_or(false);
        state.tray.repaint_motion(effect, muted);
    }
}

/// 换托盘图标样式后立刻重绘：不等下一次音量事件。
pub fn refresh_tray_icon(cx: &mut App) {
    let state = cx.global::<AppState>();
    let device = state
        .snapshot
        .borrow()
        .devices
        .iter()
        .find(|device| device.is_default)
        .cloned();
    let Some(device) = device else {
        return;
    };
    let cfg = state.config.borrow().get().clone();
    let min_db = cfg.logarithmic_min_db as f32;
    let display = if cfg.use_logarithmic_volume {
        to_display_volume(device.volume, min_db)
    } else {
        device.volume
    };
    let facts = {
        let snapshot = state.snapshot.borrow();
        tooltip_facts(&snapshot, &cfg, &device)
    };
    // 动态模式：画面归 60fps 的驱动循环管，这里只把状态推一帧过去。
    if matches!(cfg.tray_icon_style, TrayIconStyle::Dynamic) {
        // 进动态先把「上一格静态画面」的记忆抹掉：动态期间没人写 `last`，
        // 留着它，切回静态时同样的音量 + 样式码会被当成没变、直接跳过，
        // 用户看到的就是「从动态切到其他模式图标不刷新」。
        state.tray.invalidate();
        state.tray.repaint_motion(cfg.tray_motion, device.muted);
        return;
    }
    // 离开动态：动态期间 `last` 是空的，这里本来就会重画；再清一次是保险 ——
    // 万一音量和样式码刚好和进动态前那一格对得上，别让「换样式没反应」再回来。
    state.tray.invalidate();
    let number = number_style(&cfg);
    state.tray.update(&TrayUpdate {
        volume: device.volume,
        display,
        muted: device.muted,
        peak: device.peak,
        icon: crate::tray::IconSpec {
            style: cfg.tray_icon_style,
            ink: crate::tray::parse_ink(&cfg.tray_icon_color),
            color_by_level: cfg.tray_color_by_level,
            custom: &cfg.tray_icon_custom,
            number: &number,
        },
        motion_gain: cfg.tray_motion_gain,
        motion_effect: cfg.tray_motion,
        logarithmic: cfg.use_logarithmic_volume,
        min_db,
        lang: cfg.language,
        tooltip: &cfg.tooltip,
        facts: &facts,
    });
}

/// 托盘提示里那些变量的此刻取值。
///
/// 只挑面板上也看得到的东西：当前设备、读数、正在发声的应用、上限。
/// 「正在发声」和面板的口径一致（`AppInfo::is_playing`），系统声音不算，
/// 同一个名字只留一次（Chromium 系经常一个名字挂好几个会话）。
pub(crate) fn tooltip_facts(
    snapshot: &Snapshot,
    cfg: &crate::config::Config,
    device: &DeviceInfo,
) -> TooltipFacts {
    let min_db = cfg.logarithmic_min_db as f32;
    let logarithmic = cfg.use_logarithmic_volume;
    let display = if logarithmic {
        to_display_volume(device.volume, min_db)
    } else {
        device.volume
    };
    let (reading, percent) = readings(display, logarithmic, min_db);

    let mut playing: Vec<&crate::audio::AppInfo> = snapshot
        .apps
        .iter()
        .filter(|app| app.is_playing() && !app.is_system)
        .collect();
    playing.sort_by(|left, right| {
        right
            .peak
            .partial_cmp(&left.peak)
            .unwrap_or(std::cmp::Ordering::Equal)
    });
    let mut names: Vec<String> = Vec::new();
    for app in playing {
        if !names.iter().any(|name| name == &app.name) {
            names.push(app.name.clone());
        }
    }
    let app_count = names.len();
    names.truncate(3);

    let db = if device.muted {
        "-∞ dB".to_string()
    } else if logarithmic {
        reading.clone()
    } else {
        format!("{:.1} dB", amplitude_to_db(device.volume))
    };

    TooltipFacts {
        reading,
        percent,
        db,
        // 自定义名字之后，「设备」和「完整设备名」两个变量都只给自定义名：
        // 模板里写了 {device_desc} 也不该把系统原名漏到托盘提示上。
        device: cfg.device_label(&device.id, &device.name),
        device_desc: match cfg.device_alias(&device.id) {
            Some(alias) => alias.to_string(),
            None if device.description.is_empty() => device.name.clone(),
            None => device.description.clone(),
        },
        muted: device.muted,
        apps: names,
        app_count,
        mode: if logarithmic {
            cfg.language.pick("对数 dB", "Log dB")
        } else {
            cfg.language.pick("百分比", "Percent")
        }
        .to_string(),
        cap: cfg
            .volume_cap
            .limit_for(&device.id)
            .map(|_| cfg.volume_cap.percent),
        logarithmic,
    }
}

/// 从配置读托盘数字图标的外观（字体 / 抗锯齿 / 三档字号宽高字距）。
fn number_style(cfg: &crate::config::Config) -> crate::tray::NumberStyle {
    crate::tray::NumberStyle {
        font: cfg.tray_number_font.clone(),
        antialias: cfg.tray_number_antialias,
        metrics: [
            cfg.tray_digit_one,
            cfg.tray_digit_two,
            cfg.tray_digit_three,
        ],
    }
}

/// 把配置里的最大音量限制同步给音频线程（启动时与设置页改动后都要调）。
fn sync_volume_cap(engine: &AudioEngine, cap: &crate::config::VolumeCap) {
    engine.send(AudioCommand::SetVolumeCap {
        device_id: cap.device_id.clone(),
        limit: cap
            .enabled
            .then(|| cap.percent.clamp(1, 100) as f32 / 100.0),
    });
}

/// 设置页改了最大音量限制后调用：立刻生效，不用重启。
pub fn refresh_volume_cap(cx: &mut App) {
    let state = cx.global::<AppState>();
    let cap = state.config.borrow().get().volume_cap.clone();
    sync_volume_cap(&state.engine, &cap);
}

/// 把响度均衡的参数和单应用开关推给音频线程。
pub fn sync_loudness(engine: &AudioEngine, config: &crate::config::Config) {
    engine.send(AudioCommand::SetLoudness {
        config: config.loudness,
        overrides: config
            .app_loudness
            .iter()
            .map(|entry| (entry.key.clone(), entry.enabled))
            .collect(),
    });
}

/// 响度均衡改了之后调用（设置页的参数 / 面板右键的单应用开关都走这里）。
pub fn refresh_loudness(cx: &mut App) {
    let state = cx.global::<AppState>();
    let config = state.config.borrow();
    sync_loudness(&state.engine, config.get());
}

/// 某台设备的音量上限在滑条刻度上的位置（0..1，对数模式下是显示位置）。
///
/// 面板里要把滑条拖到头也停在上限，就必须换算成同一套刻度。
pub fn cap_display(limit: Option<f32>, cfg: &crate::config::Config, min_db: f32) -> f32 {
    let Some(limit) = limit else {
        return 1.0;
    };
    if cfg.use_logarithmic_volume {
        to_display_volume(limit, min_db)
    } else {
        limit
    }
}

fn toggle_flyout(cx: &mut App) {
    let handle = cx.global::<AppState>().flyout.borrow().clone();
    if let Some(handle) = handle {
        match handle.update(cx, |flyout, window, cx| flyout.toggle(window, cx)) {
            // 面板展开了就把亮度窗口摆到旁边；收起了就跟着一起收。
            Ok(()) => sync_brightness(cx),
            Err(error) => eprintln!("toggle_flyout 失败：{error:?}"),
        }
    } else {
        eprintln!("toggle_flyout: 句柄为空");
    }
}

/// 确保面板处于显示状态（重复启动实例时用）。
fn show_flyout(cx: &mut App) {
    let handle = cx.global::<AppState>().flyout.borrow().clone();
    if let Some(handle) = handle {
        let _ = handle.update(cx, |flyout, window, cx| {
            if !flyout.is_visible() {
                flyout.show(window, cx);
            } else {
                crate::windows::platform::bring_to_front(window);
                crate::windows::platform::activate(window);
            }
        });
        sync_brightness(cx);
    }
}

fn open_settings(cx: &mut App) {
    // 设置窗口只留一个：已经开着就把它提到前台，别叠第二份。
    let existing = cx.global::<AppState>().settings.borrow().clone();
    if let Some(handle) = existing {
        match handle.update(cx, |_, window, _| {
            // 设置窗口是普通窗口：只提 Z 序 + 给焦点，绝不能走 TOPMOST。
            platform::raise(window);
        }) {
            Ok(()) => return,
            Err(_) => {
                cx.global::<AppState>().settings.borrow_mut().take();
            }
        }
    }
    let (engine, config, _, shared) = cx.global::<AppState>().clone_handles();
    let config_path = cx.global::<AppState>().config_path.clone();

    let lang = cx.global::<AppState>().config.borrow().get().language;
    let Some(display) = cx.primary_display() else {
        return;
    };
    let area = display.visible_bounds();
    let saved = cx.global::<AppState>().config.borrow().get().clone();
    let window_size = size(
        px(saved.settings_width.clamp(480.0, 1600.0)),
        px(saved.settings_height.clamp(420.0, 1200.0)),
    );
    let bounds = Bounds::new(
        point(
            area.origin.x + (area.size.width - window_size.width) / 2.0,
            area.origin.y + (area.size.height - window_size.height) / 2.0,
        ),
        window_size,
    );

    let options = WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(bounds)),
        kind: WindowKind::Normal,
        display_id: cx.primary_display().map(|display| display.id()),
        titlebar: Some(TitlebarOptions {
            // 自绘标题栏，但保留窗口标题文字：任务栏按钮显示的正是它。
            title: Some(lang.pick("Bolume 设置", "Bolume Settings").into()),
            appears_transparent: true,
            ..Default::default()
        }),
        window_background: WindowBackgroundAppearance::Transparent,
        window_decorations: Some(WindowDecorations::Client),
        focus: true,
        show: true,
        is_movable: true,
        is_resizable: true,
        // 自绘标题栏上有最小化按钮，这里必须放行：gpui 靠这个标志决定
        // 要不要加 WS_MINIMIZEBOX（没有任何系统按钮会被画出来，只是让
        // 「最小化」这条路走得通）。
        is_minimizable: true,
        window_min_size: Some(size(px(480.), px(420.))),
        ..Default::default()
    };

    let opened = cx.open_window(options, move |window, cx| {
        platform::configure_settings_window(window);
        cx.new(|cx| {
            Settings::new(
                config.clone(),
                config_path.clone(),
                shared.clone(),
                engine.clone(),
                cx,
            )
        })
    });
    if let Ok(handle) = &opened {
        // 改名失焦落盘：窗口切走（Alt+Tab / 点别的程序）也要存上。
        let _ = handle.update(cx, |this, window, cx| {
            this.watch_rename_blur(window, cx);
            // 「保持显示」/ 预览面板都只是给设置页看的：窗口关掉就一起收走。
            this.watch_preview_release(window, cx);
        });
        // 窗口刚显示时 gpui 的首帧可能赶在 DirectX 设备就绪之前，画了也不会被合成，
        // 于是窗口看上去全透明（只剩 DWM 的阴影）。这里补一次重画：
        // 先把内部标记置脏，再投一条 WM_PAINT，让平台真的走一遍绘制。
        let _ = handle.update(cx, |_, window, _| {
            // 窗口不透明度：设置页里调的这一项要作用在设置窗自己身上。
            platform::set_opacity(window, crate::ui::theme::ui_opacity());
            platform::post_repaint(window);
        });
    }
    if let Ok(handle) = opened {
        cx.global::<AppState>()
            .settings
            .borrow_mut()
            .replace(handle);
    }
}

#[cfg(test)]
mod tests {
    use super::{BRIGHTNESS_OVERRIDE_FOR, BrightnessNote, resolve_brightness};
    use std::{collections::HashMap, time::{Duration, Instant}};

    fn note(want: u32, age: Duration) -> BrightnessNote {
        BrightnessNote {
            want,
            at: Instant::now()
                .checked_sub(age)
                .expect("测试里的时间差不会超过开机时长"),
        }
    }

    /// 长按时的正题：回读报的是**上一档**，不能拿它把目标盖回去。
    ///
    /// 盖回去的后果不是「显示差一格」，而是下一档从旧值重算 —— 数值来回跳，
    /// 写下去的还常常是重复值（实测 30ms 间隔连发时是 74 → 76 → 74）。
    #[test]
    fn late_readback_keeps_the_new_target() {
        let mut map = HashMap::new();
        map.insert("0:XG279Q".to_string(), note(42, Duration::from_millis(5)));

        assert_eq!(resolve_brightness(&mut map, "0:XG279Q", 40), 42);
        // 顶住期间表要留着，不然下一次回读又会被采信。
        assert_eq!(map.len(), 1);
    }

    /// 回读追上了就交还（顺便清表）：这时候两边一样，顶着也没意义。
    #[test]
    fn matching_readback_settles() {
        let mut map = HashMap::new();
        map.insert("0:XG279Q".to_string(), note(42, Duration::from_millis(5)));

        assert_eq!(resolve_brightness(&mut map, "0:XG279Q", 42), 42);
        assert!(map.is_empty());
    }

    /// 写一直没成功（DDC 抖动）时不能永久乐观：等够久就采信真值。
    #[test]
    fn stale_note_gives_up() {
        let mut map = HashMap::new();
        let old = BRIGHTNESS_OVERRIDE_FOR + Duration::from_secs(1);
        map.insert("0:XG279Q".to_string(), note(42, old));

        assert_eq!(resolve_brightness(&mut map, "0:XG279Q", 40), 40);
        assert!(map.is_empty());
    }

    /// 没记过的显示器（面板里手动改、显示器自己的按钮）一律照实报。
    #[test]
    fn unknown_display_passes_through() {
        let mut map = HashMap::new();
        assert_eq!(resolve_brightness(&mut map, "1:STRIX", 55), 55);
    }
}
