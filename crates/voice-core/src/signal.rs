// SPDX-License-Identifier: MPL-2.0
//! 合成信号，以及信号域的延迟测量。
//!
//! 放在 voice-core 里是因为两个探针都要用：M1 量协议链路的波形延迟，
//! M2 量信号过一遍 APM 之后被后移多少。量法是同一套。
//!
//! 为什么要在墙钟延迟之外再做一次互相关：
//! 墙钟只能量到「我们自己打的时间戳之间差多少」，量不到 Opus 自己的算法延迟
//! （编码器前瞻 lookahead 会把重建出来的波形整体后移）。互相关量的是真正的
//! 波形延迟，也就是用户耳朵听到的那个延迟。两个数字对不上的差值，
//! 就是编解码器自己吃掉的那部分 —— 这个差值必须能跟 `get_lookahead()` 对上，
//! 对不上说明测量方法有问题。

/// 指数扫频（chirp）。选它而不是噪声或正弦的理由：
/// - 全程不重复，互相关只有一个尖峰，没有周期性歧义；
/// - 宽带，Opus 会真的去编码它，不会被 DTX 当成静音吃掉；
/// - 幅度恒定，相关峰稳定。
pub fn chirp(len: usize, sample_rate: u32, f0: f64, f1: f64, amp: f64) -> Vec<i16> {
    let fs = sample_rate as f64;
    let total = len.max(1) as f64;
    let ratio = f1 / f0;
    let mut out = Vec::with_capacity(len);
    let mut phase = 0.0f64;
    for i in 0..len {
        let t = i as f64 / total;
        let f = f0 * ratio.powf(t);
        phase += std::f64::consts::TAU * f / fs;
        if phase > std::f64::consts::TAU {
            phase -= std::f64::consts::TAU;
        }
        out.push((phase.sin() * amp * i16::MAX as f64) as i16);
    }
    out
}

/// 数字静音。用来验证 DTX 是否真的生效。
pub fn silence(len: usize) -> Vec<i16> {
    vec![0i16; len]
}

#[derive(Debug, Clone, Copy)]
pub struct LagEstimate {
    /// `observed[m] ≈ reference[m - lag]`
    pub lag: usize,
    /// 归一化相关系数，1.0 是完美匹配。低于 0.5 基本说明没对上，数字别信。
    pub peak: f64,
    /// 峰值落在搜索区间边界上 —— 真实延迟可能在区间外，结果不可信。
    pub at_boundary: bool,
}

/// 归一化互相关，在 `[0, max_lag]` 里找最佳滞后。
///
/// 只拿参考信号中段的一个窗口去滑，不做全长相关：全长相关没有必要，
/// 而且中段能避开起播时的静音和收尾的残帧。
pub fn best_lag(
    reference: &[i16],
    observed: &[i16],
    ref_start: usize,
    ref_len: usize,
    max_lag: usize,
) -> Option<LagEstimate> {
    if ref_start + ref_len > reference.len() {
        return None;
    }
    if ref_start + max_lag + ref_len > observed.len() {
        return None;
    }

    let r: Vec<f32> = reference[ref_start..ref_start + ref_len]
        .iter()
        .map(|&s| s as f32)
        .collect();
    let r_energy: f32 = r.iter().map(|v| v * v).sum();
    if r_energy <= 0.0 {
        return None;
    }

    let mut best = (0usize, f64::NEG_INFINITY);
    for lag in 0..=max_lag {
        let base = ref_start + lag;
        let o = &observed[base..base + ref_len];
        let mut dot = 0.0f32;
        let mut energy = 0.0f32;
        for (a, &b) in r.iter().zip(o.iter()) {
            let b = b as f32;
            dot += a * b;
            energy += b * b;
        }
        if energy <= 0.0 {
            continue;
        }
        let score = (dot / (r_energy * energy).sqrt()) as f64;
        if score > best.1 {
            best = (lag, score);
        }
    }

    if !best.1.is_finite() {
        return None;
    }
    Some(LagEstimate {
        lag: best.0,
        peak: best.1,
        at_boundary: best.0 == 0 || best.0 == max_lag,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chirp_is_broadband_and_bounded() {
        let c = chirp(48_000, 48_000, 200.0, 6000.0, 0.5);
        assert_eq!(c.len(), 48_000);
        let peak = c.iter().map(|v| v.unsigned_abs()).max().unwrap();
        // 幅度 0.5 满量程，允许一点取整误差
        assert!(peak > 15_000 && peak <= 16_400, "peak = {peak}");
        // 前后频率不同：过零点数量应该差很多
        let zc = |s: &[i16]| s.windows(2).filter(|w| (w[0] >= 0) != (w[1] >= 0)).count();
        assert!(
            zc(&c[..4800]) * 4 < zc(&c[43200..]),
            "chirp did not sweep up"
        );
    }

    #[test]
    fn recovers_a_known_delay() {
        let reference = chirp(48_000, 48_000, 200.0, 6000.0, 0.5);
        let delay = 1234usize;
        let mut observed = vec![0i16; delay];
        observed.extend_from_slice(&reference);
        let est = best_lag(&reference, &observed, 8_000, 4_800, 4_000).unwrap();
        assert_eq!(est.lag, delay);
        assert!(est.peak > 0.99, "peak = {}", est.peak);
        assert!(!est.at_boundary);
    }

    #[test]
    fn survives_mild_distortion() {
        // 模拟编解码后的波形：加噪 + 轻微削波
        let reference = chirp(48_000, 48_000, 200.0, 6000.0, 0.5);
        let delay = 900usize;
        let mut observed = vec![0i16; delay];
        let mut noise: i32 = 12345;
        for &s in &reference {
            noise = noise.wrapping_mul(1103515245).wrapping_add(12345);
            let n = (noise >> 20) as i16 / 8;
            observed.push(s.saturating_add(n));
        }
        let est = best_lag(&reference, &observed, 8_000, 4_800, 4_000).unwrap();
        assert_eq!(est.lag, delay);
        assert!(est.peak > 0.8, "peak = {}", est.peak);
    }

    #[test]
    fn reports_boundary_peak() {
        let reference = chirp(20_000, 48_000, 200.0, 6000.0, 0.5);
        let observed = reference.clone();
        let est = best_lag(&reference, &observed, 4_000, 2_000, 1_000).unwrap();
        assert_eq!(est.lag, 0);
        assert!(est.at_boundary);
    }

    #[test]
    fn refuses_when_observed_is_too_short() {
        let reference = chirp(20_000, 48_000, 200.0, 6000.0, 0.5);
        assert!(best_lag(&reference, &reference[..5_000], 4_000, 2_000, 1_000).is_none());
    }

    #[test]
    fn refuses_on_silent_reference() {
        let reference = silence(20_000);
        assert!(best_lag(&reference, &reference, 4_000, 2_000, 1_000).is_none());
    }
}
