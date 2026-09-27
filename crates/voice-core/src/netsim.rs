// SPDX-License-Identifier: MPL-2.0

//! 离线的网络 + 播放模拟：用虚拟时钟把一小时的包推过 [`Playback`]，几秒钟跑完。
//!
//! 为什么要它：自适应抖动缓冲的验收标准是「抖了一小时之后，延迟回到起始水平」。
//! 实时跑一小时不现实，而且每次结果都不一样；这里的时钟是虚拟的、随机数有种子，
//! 同一个场景跑多少次都是同一个结果，固定缓冲和自适应缓冲吃的是**同一串**到达时刻。
//!
//! 播放用的就是线上那份 [`Playback`]，只把 Opus 换成了一个生成周期信号的假解码器
//! （加速要找得到基音周期，行为才跟真人声一样）。
//!
//! 网络模型：固定的单程延迟 + 高斯抖动 + 随机丢包（跟 latency-probe 的 netem 一样），
//! 再加两样真网络里会有的东西：
//!
//! - **延迟尖峰**：隔一阵子，网络卡一下（路由切换、WiFi 重传风暴）。卡住期间发出去的包
//!   延迟从 `spike_ms` 线性降到 0 —— 效果是这些包一起堵住，然后成串地到
//! - **烂网络时段**：某些时间段里抖动变大

use crate::jitter::JitterConfig;
use crate::playout::{FrameDecoder, Playback};

const RATE: f64 = 48_000.0;

/// 说话方式。
#[derive(Debug, Clone, Copy)]
pub enum Talk {
    /// 一直在说（`TransmitMode::Always`，或者一直按着说话键）。
    /// 最能暴露「延迟只涨不降」的情况：没有句间停顿帮忙重置。
    Continuous,
    /// 说 `on` 秒、停 `off` 秒。
    Spurts { on_s: f64, off_s: f64 },
}

#[derive(Debug, Clone)]
pub struct Scenario {
    pub name: &'static str,
    pub seconds: f64,
    pub frame_ms: f64,
    pub owd_ms: f64,
    pub jitter_ms: f64,
    pub loss: f64,
    /// 每隔多少秒卡一次。0 = 不卡。
    pub spike_every_s: f64,
    /// 卡的时候延迟最多多加多少。
    pub spike_ms: f64,
    /// 卡多久（这段时间里发出去的包都受影响）。
    pub spike_len_ms: f64,
    /// 烂网络时段：(开始秒, 持续秒, 那段时间的抖动)。
    pub rough: Vec<(f64, f64, f64)>,
    pub talk: Talk,
    pub seed: u64,
}

impl Scenario {
    /// 同城、一直在说、一小时：每分钟卡一次 200 ms，中间夹三段一分钟的烂网络。
    pub fn city_hour() -> Self {
        Self {
            name: "city-1h-continuous",
            seconds: 3600.0,
            frame_ms: 10.0,
            owd_ms: 12.0,
            jitter_ms: 4.0,
            loss: 0.005,
            spike_every_s: 60.0,
            spike_ms: 200.0,
            spike_len_ms: 250.0,
            rough: vec![
                (600.0, 60.0, 15.0),
                (1800.0, 60.0, 15.0),
                (3000.0, 60.0, 15.0),
            ],
            talk: Talk::Continuous,
            seed: 1,
        }
    }

    /// 同上，但是正常说话：说 4 秒停 3 秒。
    pub fn city_hour_spurts() -> Self {
        Self {
            name: "city-1h-spurts",
            talk: Talk::Spurts {
                on_s: 4.0,
                off_s: 3.0,
            },
            ..Self::city_hour()
        }
    }

    /// 干净的局域网，一直在说，十分钟。看自适应缓冲在好网络上会不会白白加深。
    pub fn lan() -> Self {
        Self {
            name: "lan-10min",
            seconds: 600.0,
            frame_ms: 10.0,
            owd_ms: 1.0,
            jitter_ms: 0.3,
            loss: 0.0,
            spike_every_s: 0.0,
            spike_ms: 0.0,
            spike_len_ms: 0.0,
            rough: Vec::new(),
            talk: Talk::Continuous,
            seed: 2,
        }
    }

    fn jitter_at(&self, t_s: f64) -> f64 {
        self.rough
            .iter()
            .find(|(start, len, _)| t_s >= *start && t_s < start + len)
            .map_or(self.jitter_ms, |(_, _, j)| *j)
    }

    fn spike_at(&self, t_ms: f64) -> f64 {
        if self.spike_every_s <= 0.0 {
            return 0.0;
        }
        let period = self.spike_every_s * 1000.0;
        // 第一次卡在一个周期之后，开头留给缓冲稳定下来。
        let into = t_ms % period;
        if t_ms < period || into >= self.spike_len_ms {
            return 0.0;
        }
        self.spike_ms * (1.0 - into / self.spike_len_ms)
    }

    fn talking(&self, t_s: f64) -> bool {
        match self.talk {
            Talk::Continuous => true,
            Talk::Spurts { on_s, off_s } => t_s % (on_s + off_s) < on_s,
        }
    }
}

/// 一分钟的统计。
#[derive(Debug, Clone, Copy, Default)]
pub struct Minute {
    pub mean_ms: f64,
    pub p95_ms: f64,
    pub frames: usize,
}

#[derive(Debug, Clone)]
pub struct Outcome {
    /// 每分钟一行。
    pub minutes: Vec<Minute>,
    pub mean_ms: f64,
    pub p50_ms: f64,
    pub p95_ms: f64,
    pub p99_ms: f64,
    /// 发出去的音频帧。
    pub sent: u64,
    /// 真正播出来的（没丢、没迟到）。
    pub played: u64,
    /// 迟到被丢的。
    pub late: u64,
    /// 用 PLC 编出来的帧（网络丢的 + 迟到的 + 停着等的）。
    pub concealed: u64,
    pub accelerations: u64,
    pub removed_ms: f64,
}

impl Outcome {
    /// 没按时播出来的比例：网络丢的、迟到丢的都算。
    pub fn missing_pct(&self) -> f64 {
        if self.sent == 0 {
            return 0.0;
        }
        100.0 * (self.sent - self.played.min(self.sent)) as f64 / self.sent as f64
    }
}

/// 生成周期信号的假解码器，见模块文档。
struct Periodic {
    frame: usize,
    position: usize,
    period: Vec<f32>,
}

impl Periodic {
    fn new(frame: usize) -> Self {
        // 150 Hz，一个周期正好 320 个采样，查表就行。
        let period = (0..320)
            .map(|i| {
                let t = i as f32 / 320.0;
                ((2.0 * std::f32::consts::PI * t).sin()
                    + 0.4 * (4.0 * std::f32::consts::PI * t).sin())
                    * 0.25
            })
            .collect();
        Self {
            frame,
            position: 0,
            period,
        }
    }

    fn render(&mut self, start: usize, out: &mut [f32]) {
        for (i, s) in out.iter_mut().enumerate() {
            *s = self.period[(start + i) % self.period.len()];
        }
        self.position = start + out.len();
    }
}

impl FrameDecoder for Periodic {
    fn decode(&mut self, payload: &[u8], out: &mut [f32]) -> bool {
        let Ok(bytes) = <[u8; 8]>::try_from(payload) else {
            return false;
        };
        let frame_index = u64::from_le_bytes(bytes) as usize;
        self.render(frame_index * self.frame, out);
        true
    }

    fn conceal(&mut self, out: &mut [f32]) -> bool {
        let at = self.position;
        self.render(at, out);
        true
    }
}

struct Packet {
    arrived: f64,
    seq: u32,
    sent: f64,
    /// `None` = terminator。
    frame_index: Option<u64>,
}

/// 按场景生成到达序列。固定缓冲和自适应缓冲吃同一份。
fn packets(s: &Scenario) -> (Vec<Packet>, u64) {
    let mut rng = Rng::new(s.seed);
    let frames = (s.seconds * 1000.0 / s.frame_ms) as u64;
    let mut out = Vec::with_capacity(frames as usize + 1000);
    let mut seq: u32 = 0;
    let mut was_talking = false;
    let mut sent_frames = 0;

    for k in 0..frames {
        let sent = k as f64 * s.frame_ms;
        let t_s = sent / 1000.0;
        let talking = s.talking(t_s);
        if !talking && !was_talking {
            continue;
        }
        let jitter = rng.gaussian() * s.jitter_at(t_s);
        // 抖动不能让包比光速还快：最多把单程延迟抵掉一半。
        let delay = s.owd_ms + jitter.max(-s.owd_ms * 0.5) + s.spike_at(sent);
        let lost = rng.next_f64() < s.loss;
        let frame_index = if talking {
            sent_frames += 1;
            Some(k)
        } else {
            None
        };
        if !lost {
            out.push(Packet {
                arrived: sent + delay,
                seq,
                sent,
                frame_index,
            });
        }
        seq = seq.wrapping_add(1);
        was_talking = talking;
    }
    out.sort_by(|a, b| a.arrived.total_cmp(&b.arrived));
    (out, sent_frames)
}

/// 跑一个场景。
pub fn run(s: &Scenario, cfg: JitterConfig) -> Outcome {
    let frame = (RATE * s.frame_ms / 1000.0) as usize;
    let (packets, sent) = packets(s);
    let mut playback = Playback::new(cfg, Periodic::new(frame), frame);
    playback.record_delays();

    let minute_count = (s.seconds / 60.0).ceil() as usize;
    let mut per_minute: Vec<Vec<f64>> = vec![Vec::new(); minute_count.max(1)];
    let mut all = Vec::new();
    let mut out = vec![0.0f32; frame];

    // 播放时钟跟发送时钟有一个随便的相位差，跟真机上两个设备一样。
    let mut now = 0.37;
    let end = s.seconds * 1000.0 + 2000.0;
    let mut next = 0;
    while now < end {
        while next < packets.len() && packets[next].arrived <= now {
            let p = &packets[next];
            match p.frame_index {
                Some(index) => {
                    playback.push(p.seq, p.sent, index.to_le_bytes().to_vec(), p.arrived)
                }
                None => playback.end_at(p.seq),
            }
            next += 1;
        }
        playback.pull(now, &mut out);
        for delay in playback.take_delays() {
            let minute = ((now / 60_000.0) as usize).min(per_minute.len() - 1);
            per_minute[minute].push(delay);
            all.push(delay);
        }
        now += s.frame_ms;
    }

    let minutes = per_minute
        .iter()
        .map(|d| {
            let mut d = d.clone();
            Minute {
                mean_ms: mean(&d),
                p95_ms: percentile(&mut d, 0.95),
                frames: d.len(),
            }
        })
        .collect();
    let played = all.len() as u64;
    let jitter = playback.jitter().stats;
    Outcome {
        minutes,
        mean_ms: mean(&all),
        p50_ms: percentile(&mut all, 0.50),
        p95_ms: percentile(&mut all, 0.95),
        p99_ms: percentile(&mut all, 0.99),
        sent,
        played,
        late: jitter.late,
        concealed: playback.stats.concealed,
        accelerations: playback.stats.accelerations,
        removed_ms: playback.stats.removed_ms,
    }
}

fn mean(v: &[f64]) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    v.iter().sum::<f64>() / v.len() as f64
}

fn percentile(v: &mut [f64], q: f64) -> f64 {
    if v.is_empty() {
        return f64::NAN;
    }
    v.sort_by(|a, b| a.total_cmp(b));
    v[((v.len() - 1) as f64 * q).round() as usize]
}

/// 有种子的随机数。跟 latency-probe 的是同一个算法（SplitMix64 + Box–Muller），
/// 但那个 crate 是 GPL 的，这里不能依赖它。
struct Rng(u64);

impl Rng {
    fn new(seed: u64) -> Self {
        Self(seed ^ 0x9E37_79B9_7F4A_7C15)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }

    fn next_f64(&mut self) -> f64 {
        (self.next_u64() >> 11) as f64 / (1u64 << 53) as f64
    }

    fn gaussian(&mut self) -> f64 {
        let u1 = self.next_f64().max(f64::MIN_POSITIVE);
        let u2 = self.next_f64();
        (-2.0 * u1.ln()).sqrt() * (2.0 * std::f64::consts::PI * u2).cos()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn short(mut s: Scenario, seconds: f64) -> Scenario {
        s.seconds = seconds;
        s.rough = s
            .rough
            .into_iter()
            .map(|(start, len, j)| (start * seconds / 3600.0, len * seconds / 3600.0, j))
            .collect();
        s
    }

    /// 验收标准的缩短版（十分钟，测试里跑得动）：抖了这么久，
    /// 自适应的延迟最后一分钟跟第一分钟（还没卡过）差不多；固定缓冲则涨上去不下来。
    /// 一小时的完整版在 latency-probe 里：`cargo run --release -p latency-probe -- sim`
    #[test]
    fn after_ten_rough_minutes_the_delay_is_back_where_it_started() {
        let s = short(Scenario::city_hour(), 600.0);
        let adaptive = run(&s, JitterConfig::adaptive(s.frame_ms));
        let fixed = run(&s, JitterConfig::fixed(2));

        // 第一次卡在 60 秒，所以第一分钟是干净的基线。
        let a_start = adaptive.minutes[0].mean_ms;
        let a_end = adaptive.minutes.last().unwrap().mean_ms;
        assert!(
            a_end < a_start + 15.0,
            "自适应：第 1 分钟 {a_start:.1} ms，最后一分钟 {a_end:.1} ms"
        );

        let f_start = fixed.minutes[0].mean_ms;
        let f_end = fixed.minutes.last().unwrap().mean_ms;
        assert!(
            f_end > f_start + 100.0,
            "固定缓冲竟然没涨：{f_start:.1} → {f_end:.1} ms（这条是对照，它该涨）"
        );
    }

    /// 好网络上不能白白加深。
    #[test]
    fn a_clean_network_stays_shallow() {
        let s = short(Scenario::lan(), 60.0);
        let adaptive = run(&s, JitterConfig::adaptive(s.frame_ms));
        let fixed = run(&s, JitterConfig::fixed(2));
        assert!(
            adaptive.mean_ms <= fixed.mean_ms + 1.0,
            "局域网上自适应 {:.1} ms，固定 {:.1} ms",
            adaptive.mean_ms,
            fixed.mean_ms
        );
        assert!(adaptive.missing_pct() < 0.1);
    }

    /// 同一个场景跑两次是同一个结果 —— 能拿它做回归对比，全靠这一条。
    #[test]
    fn runs_are_reproducible() {
        let s = short(Scenario::city_hour(), 60.0);
        let a = run(&s, JitterConfig::adaptive(s.frame_ms));
        let b = run(&s, JitterConfig::adaptive(s.frame_ms));
        assert_eq!(a.mean_ms, b.mean_ms);
        assert_eq!(a.played, b.played);
    }

    /// 说说停停：句间停顿本身就能重置延迟，两种缓冲都不该涨上去 ——
    /// 但自适应在句子**里面**也能还回去，所以平均更低。
    #[test]
    fn talkspurts_help_both_but_adaptive_still_wins() {
        let s = short(Scenario::city_hour_spurts(), 600.0);
        let adaptive = run(&s, JitterConfig::adaptive(s.frame_ms));
        let fixed = run(&s, JitterConfig::fixed(2));
        assert!(adaptive.minutes.last().unwrap().mean_ms < adaptive.minutes[0].mean_ms + 15.0);
        assert!(
            adaptive.p95_ms <= fixed.p95_ms,
            "p95 自适应 {:.1} vs 固定 {:.1}",
            adaptive.p95_ms,
            fixed.p95_ms
        );
    }
}
