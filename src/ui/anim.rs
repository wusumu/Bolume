//! 弹出 / 收起动画：音量面板和显示器窗口共用这一套曲线。
//!
//! 两扇窗是同时出现的，动画只要有一点不一样就会显得「一个先落、一个后落」，
//! 所以缓动、时长、透明度压慢的指数全部集中在这里，谁也别自己另写一份。
//!
//! 曲线本身照 QcClip（`menu/animation.rs` 的 `attached_window_easing`）：三次缓出，
//! 去掉后段回弹 —— 详见 `entrance_easing` 的注释。

use std::time::Duration;

/// 没配置时的默认入场时长。
pub const DEFAULT_ENTRANCE_MS: u32 = 400;

/// 调试用：`BOLUME_ANIM_MS=800` 可以把入场动画放慢，方便逐帧截图核对。
pub fn entrance_duration(config_ms: u32) -> Duration {
    let ms = std::env::var("BOLUME_ANIM_MS")
        .ok()
        .and_then(|value| value.trim().parse::<u32>().ok())
        .unwrap_or(config_ms);
    Duration::from_millis(ms.max(1) as u64)
}

/// 照搬 QcClip 的 `ease_with_settle_bounce` 前段（三次缓出），**去掉后段的回弹**。
///
/// QcClip 的后 30% 会让进度冲到 1.2% 再落回 1.0 —— 它是直接插值窗口的物理尺寸，
/// 多出来的那点高度由 DWM 平滑掉。这里的动画是「窗口尺寸固定、裁内容」看的，
/// 进度一旦越过 1.0，面板就要整体往下让出那 0.8px 再弹回来，表现成顶边抖一下、
/// 底边被切掉一行，也就是用户看到的「最后闪一下」。
/// 所以 70% 之后直接停在 1.0，收尾交给透明度慢慢补上。
pub fn entrance_easing(progress: f32) -> f32 {
    const SETTLE_START: f32 = 0.7;
    let progress = progress.clamp(0.0, 1.0);
    if progress < SETTLE_START {
        let normalized = progress / SETTLE_START;
        return 1.0 - (1.0 - normalized).powi(3);
    }
    1.0
}

/// 这一轮弹出 / 收起走哪套动画、从多远的地方起步。
///
/// **逐帧推进不在这里**：三档一律交给 `platform::run_slide`（独立线程 + `DwmFlush`
/// 跟合成帧，内容一帧都不重绘）。这里只记「这一轮走的是哪套、起点多远」，
/// 渲染和下一次弹出来读它。
pub struct Entrance {
    motion: EntranceMotion,
    /// 这一轮从多远的地方起步（逻辑像素）；关掉动画时恒为 0。
    slide: f32,
}

/// 面板走哪套入场动画。
///
/// 两档都是「整扇窗平移 / 原地淡」：内容一帧都不裁不缩，
/// 逐帧推进交给 `platform::run_slide`。
#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub enum EntranceMotion {
    /// 照系统面板：整块从任务栏背后滑进来（`SystemMotion::panel`）。
    #[default]
    System,
    /// 就地淡入淡出：窗口位置不动，只拉整窗不透明度。
    Fade,
}

/// 系统那一套弹出 / 收起曲线的参数。
///
/// **Windows 11（本机 25H2 实测，2560×1440 @100%）**：
/// - 快速设置面板（就是系统的「音量面板」）：整块从下方滑上来，
///   起始位移约等于「面板高度 + 80」，位移按指数衰减，时间常数 ≈48ms，
///   230ms 左右就完全到位；整段几乎没有淡入（首帧就已接近不透明）。
/// - 音量 OSD：起始位移 = 卡片高度 + 10，位移时间常数 ≈70ms，
///   同时淡入（延迟 20ms 起、250ms 线性到 1）；消失是「滑回去 + 约 140ms 淡出」。
///
/// **Windows 10**：那一代 shell 的弹出面板用的是 XAML 的 `PopupThemeTransition`
/// （默认 `FromVerticalOffset = 0`），也就是**纯淡入、不位移**，时长取 XAML 的
/// 167ms 标准档。本机是 Win11，这一档没有逐帧实测，只在注释里留依据。
pub struct SystemMotion {
    /// 起始位移（逻辑像素）。0 = 不位移，只有淡入。
    pub slide: f32,
    /// 位移的指数时间常数（毫秒）：越大越慢悠悠。
    pub tau_ms: f32,
    /// 淡入延迟（毫秒）与时长（毫秒）；`fade_ms = 0` 表示不淡入。
    pub fade_delay_ms: f32,
    pub fade_ms: f32,
    /// 收起要多久（毫秒）。
    pub exit_ms: f32,
}

/// Windows 10 的 XAML 标准淡入时长。
const WIN10_FADE_MS: f32 = 167.0;

/// 系统面板一次滑入的基准总时长（实测约 250ms）：用户调速度时按它等比缩放。
const PANEL_BASE_MS: f32 = 250.0;

impl SystemMotion {
    /// 面板（音量面板 / 显示器窗口）那一套。
    pub fn panel(slide: f32) -> Self {
        if crate::platform::winver::is_windows_11() {
            Self {
                slide,
                tau_ms: 48.0,
                fade_delay_ms: 0.0,
                fade_ms: 0.0,
                exit_ms: 145.0,
            }
        } else {
            Self {
                slide: 0.0,
                tau_ms: 48.0,
                fade_delay_ms: 0.0,
                fade_ms: WIN10_FADE_MS,
                exit_ms: WIN10_FADE_MS,
            }
        }
    }

    /// 同上，但按用户设的速度整体缩放（曲线形状不变）。
    ///
    /// 「跟随系统」原本是固定节奏；现在也允许调快调慢 —— 缩放的是
    /// 时间常数和收起时长，指数曲线的形状一点没动。
    pub fn panel_scaled(slide: f32, total_ms: f32) -> Self {
        let scale = if total_ms > 0.0 {
            (total_ms / PANEL_BASE_MS).clamp(0.25, 4.0)
        } else {
            1.0
        };
        let base = Self::panel(slide);
        Self {
            slide: base.slide,
            tau_ms: base.tau_ms * scale,
            fade_delay_ms: base.fade_delay_ms,
            fade_ms: base.fade_ms * scale,
            exit_ms: base.exit_ms * scale,
        }
    }

    /// 屏幕提示（OSD）那一套。
    pub fn osd(slide: f32) -> Self {
        if crate::platform::winver::is_windows_11() {
            Self {
                slide,
                tau_ms: 70.0,
                fade_delay_ms: 20.0,
                fade_ms: 250.0,
                exit_ms: 140.0,
            }
        } else {
            Self {
                slide: 0.0,
                tau_ms: 70.0,
                fade_delay_ms: 0.0,
                fade_ms: WIN10_FADE_MS,
                exit_ms: WIN10_FADE_MS,
            }
        }
    }

    /// 入场一共要走多久（毫秒）。
    ///
    /// 指数位移永远到不了 0，按「还剩 1px 就算到位」收 —— 再往下拖只是让
    /// 卡片多挂一百多毫秒、肉眼看不出的那点位移，反而拖慢了整个收尾。
    pub fn enter_total_ms(&self) -> f32 {
        let slide_part = if self.slide > 0.5 && self.tau_ms > 0.0 {
            self.tau_ms * (self.slide / 1.0).ln()
        } else {
            0.0
        };
        slide_part.max(self.fade_delay_ms + self.fade_ms)
    }

    /// 入场第 `ms` 毫秒：(还要挪多远, 不透明度)。
    pub fn enter(&self, ms: f32) -> (f32, f32) {
        let offset = if self.slide > 0.0 {
            self.slide * (-ms / self.tau_ms.max(1.0)).exp()
        } else {
            0.0
        };
        let alpha = if self.fade_ms <= 0.0 {
            1.0
        } else {
            ((ms - self.fade_delay_ms) / self.fade_ms).clamp(0.0, 1.0)
        };
        (offset, alpha)
    }

    /// 收起第 `ms` 毫秒：(该往回收多远, 不透明度)。位移是加速的（平方），
    /// 和系统面板「先慢慢沉、然后一下滑走」的手感对上。
    pub fn exit(&self, ms: f32) -> (f32, f32) {
        let progress = (ms / self.exit_ms.max(1.0)).clamp(0.0, 1.0);
        (self.slide * progress * progress, 1.0 - progress)
    }
}

impl Default for Entrance {
    fn default() -> Self {
        Self {
            motion: EntranceMotion::System,
            slide: 0.0,
        }
    }
}

impl Entrance {
    pub fn new() -> Self {
        Self::default()
    }

    /// 摆好这一轮的参数。`animate = false`（或系统要求减少动态效果）时起点归零，
    /// 调用方据此一帧到位。
    pub fn begin_with(&mut self, animate: bool, motion: EntranceMotion, slide: f32) {
        self.motion = motion;
        self.slide = if animate { slide.max(0.0) } else { 0.0 };
    }

    pub fn motion(&self) -> EntranceMotion {
        self.motion
    }

    /// 这一轮的起始位移（系统风格 / 展开用）。
    pub fn slide(&self) -> f32 {
        self.slide
    }

    pub fn finish(&mut self) {
        self.slide = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::{Entrance, EntranceMotion, SystemMotion, entrance_easing};

    /// 和实测对得上的那一档参数（Win11 快速设置面板：290 高滑 370）。
    fn panel_motion() -> SystemMotion {
        SystemMotion {
            slide: 370.0,
            tau_ms: 48.0,
            fade_delay_ms: 0.0,
            fade_ms: 0.0,
            exit_ms: 145.0,
        }
    }

    #[test]
    fn system_enter_starts_at_full_slide_and_settles() {
        let motion = panel_motion();
        let (offset, alpha) = motion.enter(0.0);
        assert_eq!(offset, 370.0, "第一帧必须还在起点，整块卡片在窗口外面");
        assert_eq!(alpha, 1.0, "Win11 的面板不淡入");
        // 16.5ms 实测 264px：指数缓出要对得上。
        let (offset, _) = motion.enter(16.5);
        assert!((offset - 264.0).abs() < 8.0, "16.5ms 时应在 264px 附近，实际 {offset}");
        // 结束点上不许留尾巴。
        let total = motion.enter_total_ms();
        assert!(total >= 200.0 && total <= 320.0, "总时长 {total} 不在实测区间");
        let (offset, _) = motion.enter(total);
        assert!(offset <= 1.05, "到点还剩 {offset}px");
    }

    #[test]
    fn system_exit_slides_back_and_fades() {
        let motion = panel_motion();
        let (offset, alpha) = motion.exit(0.0);
        assert_eq!(offset, 0.0);
        assert_eq!(alpha, 1.0);
        let (offset, alpha) = motion.exit(motion.exit_ms);
        assert!((offset - 370.0).abs() < 0.001, "收完要退回起点");
        assert!(alpha <= 0.001, "收完要不透明");
        // 位移是加速的：半程走的距离不到一半。
        let (half, _) = motion.exit(motion.exit_ms / 2.0);
        assert!(half < 370.0 * 0.4, "收起要加速，半程只该走三成多（实际 {half}）");
    }

    #[test]
    fn osd_motion_fades_in_while_sliding() {
        let motion = SystemMotion {
            slide: 62.0,
            tau_ms: 70.0,
            fade_delay_ms: 20.0,
            fade_ms: 250.0,
            exit_ms: 140.0,
        };
        assert_eq!(motion.enter(0.0), (62.0, 0.0));
        let (offset, alpha) = motion.enter(20.0);
        assert!(offset < 62.0 && alpha < 0.05, "还没到淡入起点");
        let (offset, alpha) = motion.enter(270.0);
        assert!(offset < 2.0, "位移该基本归位（{offset}）");
        assert!(alpha >= 0.999, "淡入该满（{alpha}）");
        assert!(motion.enter_total_ms() >= 270.0);
    }

    #[test]
    fn entrance_keeps_the_round_it_was_told_to_run() {
        let mut entrance = Entrance::new();
        entrance.begin_with(true, EntranceMotion::System, 300.0);
        assert_eq!(entrance.motion(), EntranceMotion::System);
        assert_eq!(entrance.slide(), 300.0, "要动画就从起点开跑");
        // 关掉动画（或系统要求减少动态效果）：起点归零，调用方一帧到位。
        entrance.begin_with(false, EntranceMotion::System, 300.0);
        assert_eq!(entrance.slide(), 0.0, "关掉动画就一帧到位");
        entrance.finish();
        assert_eq!(entrance.slide(), 0.0);
    }

    #[test]
    fn panel_speed_scales_the_curve_without_changing_its_shape() {
        let base = SystemMotion::panel_scaled(370.0, 250.0);
        assert!((base.tau_ms - 48.0).abs() < 0.01, "250ms 就是系统原始节奏");
        let fast = SystemMotion::panel_scaled(370.0, 125.0);
        assert!(fast.tau_ms < base.tau_ms, "调快要把时间常数压小");
        assert!(fast.enter_total_ms() < base.enter_total_ms());
        // 形状不变：任何时刻走掉的比例一样。
        let a = 1.0 - base.enter(10.0).0 / 370.0;
        let b = 1.0 - fast.enter(5.0).0 / 370.0;
        assert!((a - b).abs() < 0.02, "曲线形状不该被速度改变（{a} vs {b}）");
    }

    #[test]
    fn easing_is_clamped_and_monotonic() {
        assert_eq!(entrance_easing(0.0), 0.0);
        assert_eq!(entrance_easing(1.0), 1.0);
        // 70% 之后直接停在 1.0，不再回弹。
        assert_eq!(entrance_easing(0.7), 1.0);
        assert_eq!(entrance_easing(0.95), 1.0);
        let mut previous = 0.0;
        for step in 0..=70 {
            let value = entrance_easing(step as f32 / 100.0);
            assert!(value >= previous - 1e-6, "进度必须单调不减");
            previous = value;
        }
    }

}
