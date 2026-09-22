// SPDX-License-Identifier: GPL-3.0-or-later
//! 真开流、真测量。
//!
//! # 会有什么副作用
//!
//! - **不会出声。** 渲染路径全程灌静音，采集路径只读时间戳、不保留任何音频。
//! - 会短暂点亮 Windows 的「麦克风正在使用」指示灯 —— 测采集必须打开采集流。
//! - **独占模式会让别的程序在这几秒里发不出声**，所以默认不测，要加 `--exclusive`。
//!
//! # 两边量的不是同一种东西
//!
//! **采集**量的是既成事实：声音已经被设备记下来了，问的是「到我手上过了多久」。
//! WASAPI 的 QPC 时间戳直接给了答案，没什么可选的。
//!
//! **渲染**量的是一个选择：我们在队列里压多少音频。压得浅延迟低，但一有调度抖动
//! 就欠载（用户听到咔哒）。所以渲染侧不是「测出一个数」，而是**扫出能撑住的最浅深度**。
//! 只报一个深度下的 p50 是自欺欺人 —— 那只反映我们自己填了多少。

use voice_core::metrics::Summary;
use voice_core::wasapi::{self, DeviceInfo, Direction, ShareMode, StreamStats};

pub struct Measurement {
    pub device: String,
    pub direction: Direction,
    pub share_mode: ShareMode,
    /// 表格里 mode 那一列显示什么。渲染的每次试探会带上目标深度。
    pub label: String,
    /// 这一路的设备周期。判断余量够不够要拿它当尺子 ——
    /// 写死 10 ms 的话，换一台周期不是 10 ms 的机器判据就错了。
    pub period_ms: f64,
    pub outcome: Result<Report, String>,
}

impl Measurement {
    /// 这一次试探算不算「撑住了」——零欠载。
    pub fn clean(&self) -> bool {
        self.outcome
            .as_ref()
            .map(|r| r.glitches == 0)
            .unwrap_or(false)
    }

    /// 除了零欠载，还留住了**至少一个完整周期**的余量。
    ///
    /// 为什么是一个周期而不是半个：线程漏醒一次，代价就是一整个周期。
    /// 余量小于一个周期 = 一次调度抖动就咔哒。这个软件是要跟游戏抢 CPU 的，
    /// 把红线定在「永远不漏醒」上不现实。
    ///
    /// 只看「这次没欠载」更不够：共享模式引擎的节奏精确到一帧，留一帧余量也能
    /// 测着零欠载 —— 那是测试环境干净，不是设计安全。
    pub fn has_headroom(&self) -> bool {
        self.clean()
            && self
                .outcome
                .as_ref()
                .map(|r| r.measured.p50 >= self.period_ms)
                .unwrap_or(false)
    }

    /// 采集：实测延迟的 p95。渲染：实际维持的队列深度。
    /// 两边都是「这一路吃掉多少延迟」，可以直接相加。
    ///
    /// 时间戳不可信的采集路径返回 None —— 见 [`Measurement::capture_timestamp_trustworthy`]。
    pub fn latency_ms(&self) -> Option<f64> {
        if self.direction == Direction::Capture && !self.capture_timestamp_trustworthy() {
            return None;
        }
        self.outcome.as_ref().ok().map(|r| match self.direction {
            Direction::Capture => r.measured.p95,
            Direction::Render => r.queue_ms,
        })
    }

    /// 这一路的采集时间戳是不是真的在报「设备什么时候记下的」。
    ///
    /// 判据：音频**物理上**至少要在设备侧攒够一个周期才可能交给我们，所以
    /// 实测延迟远小于一个周期就只有一种解释 —— 那个 QPC 位置不是设备记录时刻，
    /// 而是「路由把数据递给我们」的时刻。两种情况会这样：
    /// - 虚拟声卡：它自己造时间戳，不含上游真声卡数字化那一段
    /// - 独占模式：这台机器上实测 p50 = 0.00 ms，驱动压根没填
    ///
    /// 这种数字比没有还糟 —— 它会让预算算出一个谁都达不到的乐观值。
    pub fn capture_timestamp_trustworthy(&self) -> bool {
        if self.direction != Direction::Capture {
            return true;
        }
        self.outcome
            .as_ref()
            .map(|r| r.measured.p50 >= self.period_ms * 0.25)
            .unwrap_or(false)
    }
}

pub struct Report {
    pub format: String,
    pub buffer_ms: f64,
    pub reported_latency_ms: f64,
    /// 采集：设备到应用的实测延迟。渲染：**欠载余量**（离咔哒声还有多远），不是延迟。
    pub measured: Summary,
    /// 渲染专用：实际维持的队列深度。**这才是渲染吃掉的延迟。**
    pub queue_ms: f64,
    pub wake: Summary,
    pub glitches: u64,
}

impl Report {
    fn from(mut s: StreamStats) -> Self {
        let measured = match s.direction {
            Direction::Capture => s.capture_latency_ms.summary(),
            Direction::Render => s.render_margin_ms.summary(),
        };
        Report {
            queue_ms: s.target_queue_ms,
            format: s.format.to_string(),
            buffer_ms: s.buffer_ms,
            reported_latency_ms: s.reported_latency_ms,
            measured,
            wake: s.wake_interval_ms.summary(),
            glitches: s.glitches,
        }
    }
}

/// 独占模式要指定声道数，挑设备声称支持的那个。都不支持就别试了。
fn exclusive_channels(d: &DeviceInfo) -> Option<u16> {
    if d.exclusive_48k_mono_i16 {
        Some(1)
    } else if d.exclusive_48k_stereo_i16 {
        Some(2)
    } else {
        None
    }
}

fn channels_for(d: &DeviceInfo, share_mode: ShareMode) -> Result<u16, String> {
    match share_mode {
        // 共享模式用引擎的混音格式，这个参数不生效
        ShareMode::Shared => Ok(2),
        ShareMode::Exclusive => {
            exclusive_channels(d).ok_or_else(|| "设备不接受 48 kHz 16 位独占".to_string())
        }
    }
}

/// 这一路按哪个周期走：共享模式是引擎周期，独占是我们申请的设备最小周期。
fn period_of(d: &DeviceInfo, share_mode: ShareMode) -> f64 {
    let p = match share_mode {
        ShareMode::Shared => d.shared_period_ms(),
        ShareMode::Exclusive => d.min_period_ms,
    };
    if p.is_finite() && p > 0.0 {
        p
    } else {
        10.0
    }
}

fn fail(d: &DeviceInfo, share_mode: ShareMode, label: String, msg: String) -> Measurement {
    Measurement {
        device: d.name.clone(),
        direction: d.direction,
        share_mode,
        label,
        period_ms: period_of(d, share_mode),
        outcome: Err(msg),
    }
}

pub fn run_capture(d: &DeviceInfo, share_mode: ShareMode, seconds: f64) -> Measurement {
    let label = share_mode.to_string();
    let channels = match channels_for(d, share_mode) {
        Ok(c) => c,
        Err(e) => return fail(d, share_mode, label, e),
    };

    let outcome = wasapi::open_device(Some(&d.id), d.direction)
        .map_err(|e| format!("打不开设备: {e}"))
        .and_then(|dev| {
            wasapi::measure_capture(&dev, &d.name, share_mode, channels, seconds)
                .map_err(|e| wasapi::describe_error(&e))
        })
        .and_then(check_got_data);

    Measurement {
        device: d.name.clone(),
        direction: d.direction,
        share_mode,
        label,
        period_ms: period_of(d, share_mode),
        outcome,
    }
}

fn check_got_data(s: StreamStats) -> Result<Report, String> {
    if s.packets == 0 {
        Err("流开起来了但一个包都没收到 —— 设备可能被别的程序独占".into())
    } else {
        Ok(Report::from(s))
    }
}

/// 渲染侧：从最浅往深扫一遍队列深度。
///
/// 起点是一个周期。比一个周期还浅在事件驱动模型下必然欠载：事件是「已经播完一个
/// 周期」时才触发的，醒来时队列里剩的就是 深度 − 一个周期，浅于此就是负数。
///
/// **不在第一个不欠载的深度上停。** 共享模式下引擎每次精确消费一个周期，抖动几乎
/// 为零，所以哪怕只留一帧余量也能跑几秒不欠载 —— 但那个工作点只要漏醒一次就咔哒，
/// 而真机上我们是跟游戏抢 CPU 的。所以要把整条曲线扫出来，让调用方能分开看
/// 「理论下限」和「留得住余量的可用工作点」。
pub fn sweep_render(d: &DeviceInfo, share_mode: ShareMode, seconds: f64) -> Vec<Measurement> {
    let label0 = share_mode.to_string();
    let channels = match channels_for(d, share_mode) {
        Ok(c) => c,
        Err(e) => return vec![fail(d, share_mode, label0, e)],
    };

    let period_ms = period_of(d, share_mode);

    // 独占模式下缓冲必须正好等于一个周期，队列深度根本没得选，扫多个点是白跑。
    // 共享模式才有得挑。
    let factors: &[f64] = match share_mode {
        ShareMode::Shared => &[1.0, 1.25, 1.5, 2.0, 3.0],
        ShareMode::Exclusive => &[1.0],
    };
    let mut out = Vec::new();
    for &f in factors {
        let target = period_ms * f;
        let outcome = wasapi::open_device(Some(&d.id), d.direction)
            .map_err(|e| format!("打不开设备: {e}"))
            .and_then(|dev| {
                wasapi::measure_render(&dev, &d.name, share_mode, channels, seconds, target)
                    .map_err(|e| wasapi::describe_error(&e))
            })
            .and_then(check_got_data);

        // 标签用**实际**生效的深度：请求 10 ms 但被夹到 11 ms 的话，
        // 标签写 10 就是在骗人，而预算正是按这个数算的。
        let actual = outcome.as_ref().map(|r| r.queue_ms).unwrap_or(target);
        let m = Measurement {
            device: d.name.clone(),
            direction: d.direction,
            share_mode,
            label: format!("{share_mode}@{actual:.1}"),
            period_ms,
            outcome,
        };
        out.push(m);
    }
    out
}
