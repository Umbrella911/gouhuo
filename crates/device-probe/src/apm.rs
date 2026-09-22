// SPDX-License-Identifier: GPL-3.0-or-later
//! 给 APM 逐块计价：每一块吃掉多少 CPU、多少延迟。
//!
//! 不接设备、不出声 —— APM 是纯信号处理，喂合成信号就行。
//!
//! # 量延迟为什么用互相关
//!
//! APM 不会告诉你它把信号推后了多少。高通是个 IIR 滤波器、AEC3 内部有分析窗和
//! 延迟估计器、AGC 有前瞻 —— 每一块都可能引入群延迟。唯一可信的量法是
//! **把信号喂进去、把出来的跟原始的做互相关**，看波形被推后了多少。
//! 这跟 M1 量协议链路波形延迟是同一套方法，代码也是同一份（`voice_core::signal`）。
//!
//! # 为什么要喂远端帧
//!
//! AEC 要消的是「我播出去的声音又被我录回来」，所以它必须同时看到两路。
//! 这里远端喂静音（没有回声可消），但**调用照样要发生** —— 真实客户端每 10 ms
//! 两路都要走一遍，CPU 账必须把它算进去。

use std::time::Instant;

use voice_core::apm::{Apm, ApmConfig};
use voice_core::metrics::Histogram;
use voice_core::signal;

const RATE: u32 = 48_000;

pub struct ApmRow {
    pub label: String,
    /// 每 10 ms 帧的处理耗时（近端 + 远端）。
    pub cpu_us_p50: f64,
    pub cpu_us_p95: f64,
    /// 折算成单核占比 %：耗时 / 10 ms。
    pub cpu_pct: f64,
    /// 互相关量到的波形延迟。`None` = 相关峰太低，信号被处理得认不出来了。
    pub delay_ms: Option<f64>,
    pub delay_peak: Option<f64>,
    /// 输出相对输入的幅度比。AGC 会明显改变它；接近 0 说明信号被吃掉了。
    pub level_ratio: f64,
}

/// 跑一个配置。
///
/// `seconds` 越长越准，但 APM 的各块都有收敛过程（AEC3 的延迟估计、AGC 的增益
/// 爬升），所以前 1 秒一律丢掉不计。
pub fn measure(label: &str, cfg: ApmConfig, seconds: f64) -> Result<ApmRow, String> {
    let apm = Apm::new(RATE, cfg).map_err(|e| format!("建不起来: {e}"))?;
    let n = apm.frame_samples();
    let frames = (seconds * 100.0) as usize; // 每帧 10 ms
    let warmup = 100; // 丢掉前 1 秒

    if frames <= warmup + 50 {
        return Err("时间太短，扣掉收敛期就没样本了".into());
    }

    // 扫频信号：全程不重复，互相关只有一个尖峰。跟 M1 用的是同一个发生器。
    let reference = signal::chirp(frames * n, RATE, 200.0, 3800.0, 0.35);

    let mut out_pcm: Vec<i16> = Vec::with_capacity(frames * n);
    let mut cpu_us = Histogram::with_capacity(frames);
    let mut frame = vec![0.0f32; n];
    let mut render = vec![0.0f32; n];

    let (mut in_energy, mut out_energy) = (0.0f64, 0.0f64);

    for k in 0..frames {
        let block = &reference[k * n..(k + 1) * n];
        for (dst, &src) in frame.iter_mut().zip(block) {
            *dst = src as f32 / 32768.0;
        }
        render.fill(0.0);

        let t = Instant::now();
        // 两路都要走：真客户端每 10 ms 都是这样跑的，CPU 账要算全。
        apm.analyze_render(&mut render)
            .map_err(|e| format!("远端帧失败: {e}"))?;
        apm.process_capture(&mut frame)
            .map_err(|e| format!("近端帧失败: {e}"))?;
        let elapsed_us = t.elapsed().as_secs_f64() * 1e6;

        if k >= warmup {
            cpu_us.push(elapsed_us);
            for (&i, &o) in block.iter().zip(frame.iter()) {
                in_energy += (i as f64 / 32768.0).powi(2);
                out_energy += (o as f64).powi(2);
            }
        }
        out_pcm.extend(frame.iter().map(|&s| (s.clamp(-1.0, 1.0) * 32767.0) as i16));
    }

    // 互相关：输出和输入是逐帧对齐送进去的，所以滞后直接就是 APM 引入的延迟。
    // 搜索范围 0~100 ms，比任何合理的 APM 延迟都宽裕。
    let ref_start = frames * n / 2;
    let ref_len = (n * 20).min(9_600);
    let max_lag = (0.100 * RATE as f64) as usize;
    let est = signal::best_lag(&reference, &out_pcm, ref_start, ref_len, max_lag);

    let (delay_ms, delay_peak) = match est {
        // 相关峰太低说明信号被处理得认不出来了（比如降噪把扫频当噪声抑掉了），
        // 这时候报一个数字比不报还糟。
        //
        // 注意**只拒上边界**：lag 命中上界说明真实延迟在搜索区间之外，不可信；
        // 但 lag = 0 是个完全合法的答案（这一块根本不引入延迟），
        // 把它一起拒掉的话「全关」和「只开 AGC2」会莫名其妙报"对不上"。
        Some(e) if e.peak >= 0.5 && e.lag < max_lag => {
            (Some(e.lag as f64 * 1000.0 / RATE as f64), Some(e.peak))
        }
        Some(e) => (None, Some(e.peak)),
        None => (None, None),
    };

    let p50 = cpu_us.pct(0.50);
    let p95 = cpu_us.pct(0.95);

    Ok(ApmRow {
        label: label.to_string(),
        cpu_us_p50: p50,
        cpu_us_p95: p95,
        cpu_pct: p50 / 10_000.0 * 100.0, // 一帧 10 ms = 10000 µs
        delay_ms,
        delay_peak,
        level_ratio: if in_energy > 0.0 {
            (out_energy / in_energy).sqrt()
        } else {
            f64::NAN
        },
    })
}

/// 默认要测的一组：先各块单独计价，最后是真正要发布的全开配置。
pub fn measure_all(seconds: f64) -> Vec<Result<ApmRow, String>> {
    use voice_core::apm::ApmModule;

    let mut rows = Vec::new();
    rows.push(measure("全关（基线）", ApmConfig::none(), seconds));
    for m in ApmModule::ALL {
        rows.push(measure(&format!("只开 {m}"), ApmConfig::only(m), seconds));
    }
    // 真实配置：全开，并且把 M2 实测的设备延迟告诉 AEC3。
    let shipping = ApmConfig {
        stream_delay_ms: Some(30),
        ..ApmConfig::default()
    };
    rows.push(measure("全开（实际配置）", shipping, seconds));
    rows
}
