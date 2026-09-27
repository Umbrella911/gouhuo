// SPDX-License-Identifier: GPL-3.0-or-later
//! `latency-probe sim`：固定缓冲 vs 自适应缓冲，离线跑一小时。
//!
//! 实时的那套（`run.rs`）量的是「这一刻有多快」，几秒钟；这里回答的是
//! 「抖了一小时之后还剩多少延迟」—— 那个实时跑不了，而且每次结果都不一样。
//! 模拟器本身在 `voice_core::netsim`，播放逻辑就是线上那一份。

use voice_core::jitter::JitterConfig;
use voice_core::netsim::{self, Outcome, Scenario};

/// 固定缓冲的深度，跟实时测量默认的那一档一样（M1 定的 2 帧）。
const FIXED_FRAMES: usize = crate::DEFAULT_TARGET;

pub fn main(seconds: Option<f64>) {
    let mut scenarios = vec![
        Scenario::city_hour(),
        Scenario::city_hour_spurts(),
        Scenario::lan(),
    ];
    if let Some(seconds) = seconds {
        for s in &mut scenarios {
            s.seconds = seconds;
        }
    }

    println!();
    println!("  固定缓冲（{FIXED_FRAMES} 帧）vs 自适应缓冲，虚拟时钟，同一串到达时刻");
    println!();
    println!(
        "  {:<22} {:<6} {:>9} {:>9} {:>7} {:>7} {:>7} {:>7} {:>9}",
        "场景", "缓冲", "首分钟", "末分钟", "平均", "p95", "p99", "没播出", "还回去"
    );
    for s in &scenarios {
        let fixed = netsim::run(s, JitterConfig::fixed(FIXED_FRAMES));
        let adaptive = netsim::run(s, JitterConfig::adaptive(s.frame_ms));
        row(s.name, "固定", &fixed);
        row("", "自适应", &adaptive);
        if s.name == Scenario::city_hour().name {
            series(&fixed, &adaptive);
        }
    }
    println!();
    println!("  延迟 = 发送端采到这一帧 → 它的第一个采样开始播，毫秒。不含声卡、APM、编码前瞻。");
    println!("  没播出 = 网络丢的 + 迟到被丢的，占发出去的帧的比例。");
    println!("  还回去 = 加速播一共删掉的时长（自适应才有）。");
    println!();
}

fn row(name: &str, buffer: &str, o: &Outcome) {
    println!(
        "  {:<22} {:<6} {:>7.1}ms {:>7.1}ms {:>5.1}ms {:>5.1}ms {:>5.1}ms {:>6.2}% {:>7.0}ms",
        name,
        buffer,
        o.minutes.first().map_or(f64::NAN, |m| m.mean_ms),
        o.minutes.last().map_or(f64::NAN, |m| m.mean_ms),
        o.mean_ms,
        o.p95_ms,
        o.p99_ms,
        o.missing_pct(),
        o.removed_ms,
    );
}

/// 每 5 分钟一个点：一眼看出是台阶还是平的。
fn series(fixed: &Outcome, adaptive: &Outcome) {
    println!();
    println!("    每 5 分钟的平均延迟（ms）：");
    let points = |o: &Outcome| -> String {
        o.minutes
            .chunks(5)
            .map(|chunk| {
                let (sum, n) = chunk
                    .iter()
                    .filter(|m| m.frames > 0)
                    .fold((0.0, 0usize), |(s, n), m| (s + m.mean_ms, n + 1));
                format!("{:>5.0}", if n == 0 { f64::NAN } else { sum / n as f64 })
            })
            .collect::<Vec<_>>()
            .join("")
    };
    println!("      固定  {}", points(fixed));
    println!("      自适应{}", points(adaptive));
    println!();
}
