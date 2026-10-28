//! 托盘图标的动态模式：把默认播放设备的峰值电平画成一张会动的图标。
//!
//! 输入只有一个「当前有多响」（0..1 的峰值），所以所有效果都建立在两个量上：
//!
//! - **包络**：峰值经过「快起慢落」的跟随后得到的值，代表「这会儿多响」；
//! - **节拍**：电平突然抬头时触发一次、然后指数衰减的量，代表「刚刚砸了一下」。
//!
//! 每个效果自己决定把这两个量翻译成什么形状。这样即便没有 FFT，也能看出
//! 图标是跟着音乐在迈步，而不是在原地抖。
//!
//! 画法是 4 倍超采样 + 盒式降采样：形状按 0/1 硬边画，缩下来边缘自带抗锯齿，
//! 16px 下既锐利又不毛躁。

use std::time::Instant;

use crate::config::TrayMotion;

/// 超采样倍数。4 倍足够：16px 的画布是 64×64，降下来边缘有 17 级灰度。
const SS: u32 = 4;

/// 律动状态机：包络 / 节拍 / 时间轴。
pub struct Motion {
    /// 跟响度的包络（0..1）。
    level: f32,
    /// 四根律动条各自的包络（回落速度不同，看着像电平表在往下淌）。
    bars: [f32; 4],
    /// 节拍强度：触发时置 1，之后指数衰减。
    beat: f32,
    /// 距上次节拍多久（秒），用来做冷却和涟漪扩散。
    beat_age: f32,
    /// 累计运行时间（秒），驱动旋转与相位。
    time: f32,
    /// 上一帧的目标电平，用来判断「抬头」。
    prev_target: f32,
    /// 上一帧的时刻。
    last: Option<Instant>,
}

impl Default for Motion {
    fn default() -> Self {
        Self {
            level: 0.0,
            bars: [0.0; 4],
            beat: 0.0,
            beat_age: 9.0,
            time: 0.0,
            prev_target: 0.0,
            last: None,
        }
    }
}

impl Motion {
    /// 推进一帧并返回覆盖率图（长度 = `side * side`，值 0..1）。
    ///
    /// `gain` 是灵敏度倍数（1.0 = 100%）：峰值本身多数时候只有 0.1~0.4，
    /// 不放大就只能看到一点点抖动。
    pub fn step(&mut self, peak: f32, gain: f32, effect: TrayMotion, side: u32) -> Vec<f32> {
        let now = Instant::now();
        // 第一帧没有上一帧可比：按 60fps 估一个 dt，不然会有一个巨大的时间跳变。
        let dt = self
            .last
            .map(|last| now.duration_since(last).as_secs_f32())
            .unwrap_or(1.0 / 60.0)
            .clamp(0.0, 0.1);
        self.last = Some(now);
        self.time += dt;

        // 感知压缩：听感上「小声」其实已经有不小幅度，开方后小信号也看得见。
        let target = (peak.clamp(0.0, 1.0) * gain).clamp(0.0, 1.0).powf(0.72);

        // 包络：起得快（听见就起），落得慢（不然会闪成一片）。
        let attack = (dt * 22.0).min(1.0);
        let release = (dt * 6.0).min(1.0);
        let follow = if target > self.level { attack } else { release };
        self.level += (target - self.level) * follow;

        // 每根条单独跟一次，回落速度一根比一根快：同一记鼓点会在四根条上
        // 先后落下去，看起来才有「电平表在淌」的感觉。
        for index in 0..self.bars.len() {
            let weight = 1.0 - 0.07 * index as f32;
            let goal = target * weight;
            let fall = (dt * (4.0 + index as f32 * 3.5)).min(1.0);
            let k = if goal > self.bars[index] {
                attack
            } else {
                fall
            };
            self.bars[index] += (goal - self.bars[index]) * k;
        }

        // 节拍：电平比上一帧抬头超过阈值就算一下。冷却 140ms，免得一段持续的
        // 响声里每一帧都触发（那样涟漪会糊成一团白）。
        if target - self.prev_target > 0.08 && target > 0.04 && self.beat_age > 0.14 {
            self.beat = 1.0;
            self.beat_age = 0.0;
        }
        self.prev_target = target;
        self.beat_age += dt;
        self.beat *= (-dt * 3.2).exp();

        self.paint(effect, side)
    }

    /// 只按当前状态出一张图（不推进时间）。切换样式 / 音量变化时用它保持画面连续。
    pub fn snapshot(&self, effect: TrayMotion, side: u32) -> Vec<f32> {
        self.paint(effect, side)
    }

    /// 当前包络（0..1，快起慢落之后的值）。
    ///
    /// 「彩色律动」拿它当色温：颜色跟着这个值走，才不会每一帧都跳色。
    pub fn level(&self) -> f32 {
        self.level
    }

    /// 静音 / 正在暂停：把状态压回去，避免切回来时先跳一下。
    pub fn reset(&mut self) {
        self.level = 0.0;
        self.bars = [0.0; 4];
        self.beat = 0.0;
        self.beat_age = 9.0;
        self.prev_target = 0.0;
        self.last = None;
    }

    fn paint(&self, effect: TrayMotion, side: u32) -> Vec<f32> {
        let mut canvas = Raster::new(side);
        match effect {
            TrayMotion::Bars => self.paint_bars(&mut canvas),
            TrayMotion::Ripple => self.paint_ripple(&mut canvas),
            TrayMotion::Orbit => self.paint_orbit(&mut canvas),
            TrayMotion::Pulse => self.paint_pulse(&mut canvas),
        }
        canvas.downsample()
    }

    /// 四根律动条：底边对齐，高度跟包络，顶端是硬的（这个尺寸下圆角只会糊）。
    fn paint_bars(&self, canvas: &mut Raster) {
        const COUNT: usize = 4;
        let bar = 0.14;
        let gap = 0.075;
        let total = bar * COUNT as f32 + gap * (COUNT as f32 - 1.0);
        let left = (1.0 - total) / 2.0;
        let bottom = 0.88;
        // 一点声音都没有时留 13% 的短桩：图标还是个「电平表」，不是空白。
        let floor = 0.13;
        for index in 0..COUNT {
            let height = (floor + (bottom - floor) * self.bars[index]).clamp(floor, bottom);
            let x0 = left + index as f32 * (bar + gap);
            canvas.rect(x0, bottom - height, x0 + bar, bottom, 1.0);
        }
    }

    /// 涟漪：中心一个实心点，节拍来了向外扩一圈，边扩边淡。
    fn paint_ripple(&self, canvas: &mut Raster) {
        let center = 0.5;
        // 核心点压到 3px 出头：再大就把扩散环整个盖住，看着只剩一团光晕。
        let core = 0.10 + 0.02 * self.level;
        canvas.disc(center, center, core, 0.55 + 0.45 * self.level);

        // 扩散环：550ms 从贴着核心扩到 0.38，幂次让它在接近边缘时淡得更快
        // （才有「散开」的错觉）。最大半径留一点余量，别贴着画布边被切掉。
        if self.beat > 0.01 {
            let progress = (self.beat_age / 0.6).clamp(0.0, 1.0);
            if progress < 1.0 {
                // 从核心外面一点起步（0.19 ≈ 3px 半径），一路扩到 0.44：
                // 16px 下 1px 的环会被抗锯齿摊成灰边，所以环给到 0.1（≈1.6px）
                // 并且一开始就够亮 —— 宁可粗一点，也别淡成一圈雾。
                let radius = 0.19 + 0.25 * progress;
                let alpha = (1.0 - progress).powf(1.1) * (0.45 + 0.55 * self.beat);
                canvas.ring(center, center, radius, 0.10, alpha);
            }
        }
    }

    /// 光环：一圈留缺口的环在转，转速和亮度跟着响度走；中间一颗点。
    fn paint_orbit(&self, canvas: &mut Raster) {
        use std::f32::consts::TAU;
        let speed = 0.55 + 2.6 * self.level;
        // 顺时针转：角度随时间是减的（屏幕 y 轴朝下）。
        let head = -(self.time * speed) % TAU;
        let sweep = TAU * 0.72;
        let alpha = 0.45 + 0.55 * self.level.max(self.beat * 0.6);
        canvas.arc(0.5, 0.5, 0.375, 0.085, head - sweep, head, alpha);
        // 缺口的对面再点一颗小点，让「转」有个参照物。
        let back = head - sweep - 0.55;
        let (x, y) = (0.5 + 0.375 * back.cos(), 0.5 + 0.375 * back.sin());
        canvas.disc(x, y, 0.055, 0.35 + 0.5 * self.level);
        canvas.disc(0.5, 0.5, 0.10 + 0.04 * self.level, 0.5 + 0.5 * self.level);
    }

    /// 呼吸：三圈同心，半径一起跟着包络胀缩，外圈最淡。
    fn paint_pulse(&self, canvas: &mut Raster) {
        let level = self.level.max(self.beat * 0.55);
        canvas.disc(0.5, 0.5, 0.085 + 0.05 * level, 0.65 + 0.35 * level);
        canvas.ring(0.5, 0.5, 0.245 + 0.085 * level, 0.07, 0.45 + 0.5 * level);
        canvas.ring(0.5, 0.5, 0.40, 0.055, 0.30 + 0.30 * level);
    }
}

/// 超采样画布：形状按归一化坐标（0..1）硬边填，最后缩成目标尺寸。
struct Raster {
    /// 超采样后的边长。
    side: u32,
    coverage: Vec<f32>,
}

impl Raster {
    fn new(side: u32) -> Self {
        let side = side.max(1);
        Self {
            side,
            coverage: vec![0.0; (side * side * SS * SS) as usize],
        }
    }

    /// 归一化坐标 → 超采样像素下标。
    fn at(&self, value: f32) -> f32 {
        value * (self.side * SS) as f32
    }

    fn plot(&mut self, x: i32, y: i32, alpha: f32) {
        let n = (self.side * SS) as i32;
        if x < 0 || y < 0 || x >= n || y >= n || alpha <= 0.0 {
            return;
        }
        let index = (y * n + x) as usize;
        // 取最大值而不是叠加：两个形状压在一起不该亮一倍。
        if self.coverage[index] < alpha {
            self.coverage[index] = alpha;
        }
    }

    fn rect(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, alpha: f32) {
        let n = (self.side * SS) as i32;
        let (lx, rx) = (self.at(x0).round() as i32, self.at(x1).round() as i32);
        let (ty, by) = (self.at(y0).round() as i32, self.at(y1).round() as i32);
        for y in ty.max(0)..by.min(n) {
            for x in lx.max(0)..rx.min(n) {
                self.plot(x, y, alpha);
            }
        }
    }

    fn disc(&mut self, cx: f32, cy: f32, radius: f32, alpha: f32) {
        self.annulus(cx, cy, radius, 0.0, alpha, None);
    }

    fn ring(&mut self, cx: f32, cy: f32, radius: f32, thickness: f32, alpha: f32) {
        self.annulus(cx, cy, radius, thickness, alpha, None);
    }

    /// 一段圆弧（角度用弧度，`from` → `to` 顺时针/逆时针都行，给谁大给谁小都认）。
    fn arc(
        &mut self,
        cx: f32,
        cy: f32,
        radius: f32,
        thickness: f32,
        from: f32,
        to: f32,
        alpha: f32,
    ) {
        self.annulus(cx, cy, radius, thickness, alpha, Some((from, to)));
    }

    /// 圆环 / 圆的公共实现：按半径 + 角度两个条件逐像素判定。
    ///
    /// `thickness = 0` 就是实心圆；`range` 给角度区间时只画那一段。
    fn annulus(
        &mut self,
        cx: f32,
        cy: f32,
        radius: f32,
        thickness: f32,
        alpha: f32,
        range: Option<(f32, f32)>,
    ) {
        use std::f32::consts::TAU;
        let n = (self.side * SS) as i32;
        let step = 1.0 / (self.side * SS) as f32;
        let half = thickness / 2.0;
        let outer = radius + half;
        // 只扫包围盒：16px 的图标整屏扫也就 4096 个点，但包围盒让逻辑更清楚。
        let x0 = ((cx - outer) * n as f32).floor().max(0.0) as i32;
        let x1 = ((cx + outer) * n as f32).ceil().min(n as f32) as i32;
        let y0 = ((cy - outer) * n as f32).floor().max(0.0) as i32;
        let y1 = ((cy + outer) * n as f32).ceil().min(n as f32) as i32;
        for y in y0..y1 {
            for x in x0..x1 {
                let px = (x as f32 + 0.5) * step;
                let py = (y as f32 + 0.5) * step;
                let dx = px - cx;
                let dy = py - cy;
                let distance = (dx * dx + dy * dy).sqrt();
                if distance > outer || (thickness > 0.0 && distance < radius - half) {
                    continue;
                }
                if let Some((from, to)) = range {
                    let angle = dy.atan2(dx);
                    // 归一化到 [from, from + TAU) 再比：调用方传的区间可能负、
                    // 也可能跨过 ±π，直接比大小会漏掉半边。
                    let (mut start, mut end) = (from, to);
                    if end < start {
                        std::mem::swap(&mut start, &mut end);
                    }
                    let span = (end - start).min(TAU);
                    let mut delta = (angle - start) % TAU;
                    if delta < 0.0 {
                        delta += TAU;
                    }
                    if delta > span {
                        continue;
                    }
                }
                self.plot(x, y, alpha);
            }
        }
    }

    /// 4×4 盒式降采样：目标尺寸 = 超采样边长 ÷ 4。
    fn downsample(&self) -> Vec<f32> {
        let n = self.side as usize;
        let mut out = vec![0.0f32; n * n];
        let factor = SS as usize;
        let area = (factor * factor) as f32;
        for y in 0..n {
            for x in 0..n {
                let mut sum = 0.0;
                for dy in 0..factor {
                    let row = (y * factor + dy) * n * factor;
                    for dx in 0..factor {
                        sum += self.coverage[row + x * factor + dx];
                    }
                }
                out[y * n + x] = (sum / area).clamp(0.0, 1.0);
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ink(coverage: &[f32]) -> usize {
        coverage.iter().filter(|value| **value > 0.05).count()
    }

    /// 每种效果都得画出东西，而且「安静」和「很响」不是同一张图。
    #[test]
    fn every_effect_changes_with_the_level() {
        for effect in [
            TrayMotion::Bars,
            TrayMotion::Ripple,
            TrayMotion::Orbit,
            TrayMotion::Pulse,
        ] {
            let mut quiet = Motion::default();
            let mut loud = Motion::default();
            // 先跑几帧把包络推上去，再取图（一帧来不及跟上）。
            let mut quiet_frame = Vec::new();
            let mut loud_frame = Vec::new();
            for _ in 0..8 {
                quiet_frame = quiet.step(0.0, 1.5, effect, 16);
                loud_frame = loud.step(0.9, 1.5, effect, 16);
            }
            assert_eq!(quiet_frame.len(), 256);
            assert!(ink(&quiet_frame) > 2, "{effect:?} 安静时也得有东西");
            assert!(
                ink(&loud_frame) > ink(&quiet_frame),
                "{effect:?} 响的时候要更饱满"
            );
        }
    }

    /// 有声音在播时，隔一帧的画面必须还在变（不然就成了静态图标）。
    #[test]
    fn frames_keep_moving() {
        let mut motion = Motion::default();
        for effect in [
            TrayMotion::Bars,
            TrayMotion::Ripple,
            TrayMotion::Orbit,
            TrayMotion::Pulse,
        ] {
            motion.reset();
            let first = motion.step(0.5, 1.5, effect, 16);
            // 时间轴在推进，哪怕音量不变形状也该动（转 / 扩散 / 呼吸）。
            std::thread::sleep(std::time::Duration::from_millis(30));
            let second = motion.step(0.5, 1.5, effect, 16);
            assert_ne!(first, second, "{effect:?} 两帧一模一样");
        }
    }

    /// 灵敏度是线性的：增益越大，同一路信号画得越满。
    #[test]
    fn gain_scales_the_picture() {
        let mut low = Motion::default();
        let mut high = Motion::default();
        let mut low_ink = 0;
        let mut high_ink = 0;
        for _ in 0..8 {
            low_ink = ink(&low.step(0.2, 1.0, TrayMotion::Bars, 16));
            high_ink = ink(&high.step(0.2, 3.0, TrayMotion::Bars, 16));
        }
        assert!(high_ink > low_ink, "把灵敏度拉高，条子该更高");
    }

    /// 降采样之后不能有「少一格」的错位（长度 = side²，最边上也该有像素）。
    #[test]
    fn rasterizes_to_the_full_canvas() {
        let mut canvas = Raster::new(16);
        canvas.rect(0.0, 0.0, 1.0, 1.0, 1.0);
        let out = canvas.downsample();
        assert_eq!(out.len(), 256);
        assert!(
            out.iter().all(|value| *value > 0.9),
            "整块填满后每一格都该是满的"
        );
    }
}
