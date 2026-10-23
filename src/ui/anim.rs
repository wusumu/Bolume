//! 弹出 / 收起动画：音量面板和显示器窗口共用这一套曲线。
//!
//! 两扇窗是同时出现的，动画只要有一点不一样就会显得「一个先落、一个后落」，
//! 所以缓动、时长、透明度压慢的指数全部集中在这里，谁也别自己另写一份。
//!
//! 曲线本身照 QcClip（`menu/animation.rs` 的 `attached_window_easing`）：三次缓出，
//! 去掉后段回弹 —— 详见 `entrance_easing` 的注释。

use std::time::{Duration, Instant};

use gpui::Window;

/// 没配置时的默认入场时长。
pub const DEFAULT_ENTRANCE_MS: u32 = 400;

/// 透明度相对进度再压慢一档的指数。
///
/// 高度曲线是「快起」（400ms 的曲线在 11% 时间就长到 42%），透明度若同步，
/// 那一帧就是一大块淡灰；取 1.3 次幂后同一时刻只有 6% 不透明度，几乎看不见。
const ALPHA_EXPONENT: f32 = 1.3;

/// 动画结束后的兜底宽限：正常由逐帧回调推进，被遮挡时靠它强制收尾。
const FALLBACK_GRACE: Duration = Duration::from_millis(400);

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

/// 动画中的可见高度：底边贴着屏幕（或任务栏）那一条，顶边随进度上移。
///
/// 进度为 0 时高度必须真的是 0：留 1px 的话，那一行内容在窗口刚露头时
/// 会先显出一条深色细线，看着就像「先冒出一道黑边框」。
pub fn anim_height(progress: f32, full_height: f32) -> f32 {
    if progress >= 1.0 {
        full_height
    } else {
        (full_height * progress).max(0.0)
    }
}

/// 一套入场动画的状态机：进度、透明度、开始时刻、兜底期限。
///
/// 用法和飞行面板一致：
/// `begin()` 摆到起点 → 窗口显示出来那一刻 `start()` → 每帧 `tick()` → 到点自己收尾。
pub struct Entrance {
    progress: f32,
    alpha: f32,
    started: Option<Instant>,
    duration: Duration,
    last_alpha: f32,
}

impl Default for Entrance {
    fn default() -> Self {
        Self {
            progress: 1.0,
            alpha: 1.0,
            started: None,
            duration: entrance_duration(DEFAULT_ENTRANCE_MS),
            last_alpha: 1.0,
        }
    }
}

impl Entrance {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn progress(&self) -> f32 {
        self.progress
    }

    /// 摆到起点：要动画就从头（0）开始，不要动画直接铺满。
    pub fn begin(&mut self, animate: bool, config_ms: u32) {
        self.duration = entrance_duration(config_ms);
        self.progress = if animate { 0.0 } else { 1.0 };
        self.alpha = if animate { 0.0 } else { 1.0 };
        self.started = None;
    }

    /// 计时起点：窗口「真的显示出来」的那一刻。
    pub fn start(&mut self) {
        self.started = Some(Instant::now());
    }

    pub fn session(&self) -> Option<Instant> {
        self.started
    }

    /// 兜底期限：超过它还没走完就强制收尾。
    pub fn fallback_after(&self) -> Duration {
        self.duration + FALLBACK_GRACE
    }

    pub fn finish(&mut self) {
        self.started = None;
        self.progress = 1.0;
        self.alpha = 1.0;
    }

    /// 兜底用：只有还在同一场动画里才收尾（避免把下一场动画提前掐断）。
    pub fn settle(&mut self, session: Option<Instant>) -> bool {
        if self.started.is_some() && self.started == session {
            self.finish();
            true
        } else {
            false
        }
    }

    /// 推进入场动画一帧并写好窗口透明度，返回当前进度。
    pub fn tick(&mut self, window: &mut Window) -> f32 {
        let Some(started) = self.started else {
            self.sync_opacity(window);
            return self.progress;
        };
        let duration = self.duration.as_secs_f32().max(0.001);
        let raw = (started.elapsed().as_secs_f32() / duration).clamp(0.0, 1.0);
        if raw >= 1.0 {
            self.finish();
        } else {
            self.progress = entrance_easing(raw);
            self.alpha = raw.powf(ALPHA_EXPONENT);
            // 下一帧继续（跟随显示器刷新，不受定时器粒度限制）。
            window.request_animation_frame();
        }
        self.sync_opacity(window);
        self.progress
    }

    /// 窗口整体透明度跟着入场进度走：低透明度时连 DWM 的圆角框和阴影都看不见，
    /// 窗口就是「凭空长出来」的，不会先闪一个空框。
    ///
    /// 入场进度之上再乘一层用户设的「界面不透明度」：不动画时窗口也该是那个透明度。
    pub fn sync_opacity(&mut self, window: &Window) {
        let alpha = (self.alpha * crate::ui::theme::ui_opacity()).clamp(0.0, 1.0);
        if (alpha - self.last_alpha).abs() > 0.004 {
            self.last_alpha = alpha;
            crate::windows::platform::set_opacity(window, alpha);
        }
    }

    /// 强制把窗口透明度设成某个值（显示前先归零、关掉动画时直接给满）。
    ///
    /// 传进来的值会乘「界面不透明度」：0 还是 0（隐藏），1 就是用户设的那个上限。
    pub fn set_alpha(&mut self, window: &Window, alpha: f32) {
        let alpha = (alpha * crate::ui::theme::ui_opacity()).clamp(0.0, 1.0);
        self.last_alpha = alpha;
        crate::windows::platform::set_opacity(window, alpha);
    }
}

#[cfg(test)]
mod tests {
    use super::{anim_height, entrance_easing};

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

    #[test]
    fn zero_progress_has_zero_height() {
        assert_eq!(anim_height(0.0, 200.0), 0.0);
        assert_eq!(anim_height(0.5, 200.0), 100.0);
        assert_eq!(anim_height(1.0, 200.0), 200.0);
    }
}
