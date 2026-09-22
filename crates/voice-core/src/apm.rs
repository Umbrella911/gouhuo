// SPDX-License-Identifier: MPL-2.0
//! APM：回声消除、降噪、自动增益。
//!
//! 用 [`sonora`] —— libwebrtc AudioProcessing 模块（M145）的**纯 Rust 移植**，
//! AEC3 + 降噪 + AGC2 + 高通。**绝对不要自己写 AEC** —— AEC3 是十几年的积累，
//! 自研的结果一定是外放开黑就啸叫。
//!
//! 这里只是一层薄封装，做三件事：
//! 1. 把「我们这个产品要什么」固化成一份配置，而不是让每个调用方自己拼
//! 2. 把单声道 f32 的调用方式收口（上游 API 是「声道的迭代器」，每次手写很啰嗦）
//! 3. 把 10 ms 帧长这个硬约束变成类型上看得见的东西
//!
//! # 三个必须记住的约束
//!
//! **一、帧长固定 10 ms，没得商量。** `api/audio/audio_processing.h` 原文：
//! "APM accepts only linear PCM audio data in chunks of ~10 ms"。
//! 喂别的长度直接 panic。
//!
//! 注意这只约束**处理**粒度，不约束**打包**粒度 —— 20 ms 一个包就跑两次 APM。
//! 所以它不改变 M1 算出来的帧长结论。
//!
//! **二、AEC 需要"远端"参考信号。** 回声是「对方的声音从我的音箱放出来、又被我的
//! 麦克风收进去」。要消掉它，APM 必须同时看到两路：我们**播出去**的（远端/render）
//! 和我们**录进来**的（近端/capture）。只喂 capture 的话 AEC 完全不工作。
//!
//! 这就是设计里说「用 WASAPI loopback capture 拿 AEC 参考信号」的原因：
//! 外放场景下我们播出去的音频要原样喂给 [`Apm::process_render`]。
//!
//! **三、两路的时间关系要对得上。** AEC3 自己会估延迟，但估得准不准取决于喂进来的
//! 两路是不是大致同步。真接上 WASAPI 之后要通过
//! [`ApmConfig::stream_delay_ms`] 把设备那段延迟告诉它（M2 已经量出来了：
//! 采集 10.4 ms + 渲染 20 ms）。
//!
//! # 为什么是纯 Rust 那份，不是 C++ 的
//!
//! 一直到 2026-09 用的都是 `webrtc-audio-processing`（C++ 的 libwebrtc，M131）。
//! 它在 Windows/MSVC 上开箱编不过，我们打了七个补丁、vendor 了 5.3 MB 源码、
//! 写了一个环境准备脚本才跑起来 —— 而最贵的代价是 **APM 因此默认关着**，
//! `git clone` 完 `cargo run` 跑出来的客户端没有回声消除也没有降噪。
//!
//! sonora 不需要任何 C++ 工具链：没有 meson、ninja、abseil、MAX_PATH、
//! 伪造的 `nm.exe`、符号前缀。18 秒 `cargo build` 编完。
//!
//! 换之前做了 A/B（两个后端跑同一份测量代码）：CPU 0.66% → 0.79%
//! （+0.13 个百分点），延迟 15.00 → 14.92 ms，合成回声抑制 26.0 → 57.9 dB。
//! 完整数字和它的**局限**见 `docs/apm-backend.md` —— 特别是那句
//! 「两边不是同一个版本」（M131 vs M145）。
//!
//! # 三处实现差异
//!
//! 这三处都是 API 形状的差异，不是行为差异，但都影响性能账：
//!
//! **一、`&mut self` vs `&self`。** C++ 的 APM 内部自己带锁（而且 capture 和
//! render 是**两把独立的锁**），所以那个 crate 的方法是 `&self`。sonora 是普通的
//! Rust 结构体，方法是 `&mut self`。而 [`crate::pipeline::AudioProcessor`] 要求
//! `&self` + `Send + Sync`（它被 `Arc` 着跨线程用），所以这里包了一把 `Mutex`。
//!
//! 代价是**采集和渲染被串起来了**：采集线程可能被渲染线程挡一下。一帧十几微秒、
//! 10 ms 的预算，量级上是 0.1%，但这是实时线程，写在这里免得以后有人忘了。
//!
//! **二、不是原地处理。** sonora 的签名是「源和目标两个 slice」，我们的接口是
//! 原地改。所以这里挂了一块 scratch，每帧多一次 480 点的拷贝。
//!
//! **三、没有「只分析不修改」的远端接口。** C++ 那份有 `analyze_render_frame`，
//! 外放场景下我们不打算改远端音频，用它能省一次拷贝。sonora 只有
//! `process_render_f32`，所以 [`Apm::analyze_render`] 是往 scratch 里丢、
//! 把结果扔掉 —— 参考信号照样喂给了 AEC，行为一致，只是多一次拷贝。

/// APM 各模块的开关。
///
/// 拆开是为了能单独量每一块的代价 —— M2 要回答的正是「AEC3 到底吃掉多少」。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ApmConfig {
    /// AEC3。外放开黑不啸叫全靠它，也是这条链路上最贵的一块。
    pub echo_cancel: bool,
    /// 降噪。机械键盘、风扇、空调。
    pub noise_suppression: bool,
    /// 自动增益。麦克风离嘴远近不同的人音量能差一个量级。
    pub gain_control: bool,
    /// 高通滤波，切掉低频隆隆声。开 AEC 或降噪时会被强制打开。
    pub high_pass: bool,
    /// 从"播出去"到"录回来"隔了多久，毫秒。
    ///
    /// AEC3 自己会估，`None` 就是让它自己估。但给一个靠谱的初值能让它收敛得快很多。
    /// 接上 WASAPI 之后这个值 = 渲染队列深度 + 采集延迟
    /// （M2 实测：20 + 10.4 ≈ 30 ms，见 docs/m2-baseline.txt）。
    ///
    /// 上游把它放在 `EchoCanceller::Full` 里，所以只能在建 APM 的时候定，
    /// 不能中途改 —— 要改就得重建。
    pub stream_delay_ms: Option<u16>,
}

impl ApmConfig {
    /// 全关。用来量基线 —— 一个什么都不做的 APM 本身要多少钱。
    pub fn none() -> Self {
        Self {
            echo_cancel: false,
            noise_suppression: false,
            gain_control: false,
            high_pass: false,
            stream_delay_ms: None,
        }
    }

    /// 只开一块，其余全关。给逐项计价用。
    pub fn only(which: ApmModule) -> Self {
        let mut c = Self::none();
        match which {
            ApmModule::EchoCancel => c.echo_cancel = true,
            ApmModule::NoiseSuppression => c.noise_suppression = true,
            ApmModule::GainControl => c.gain_control = true,
            ApmModule::HighPass => c.high_pass = true,
        }
        c
    }
}

impl Default for ApmConfig {
    /// 这个产品实际要用的配置：全开。
    ///
    /// 场景是「常驻几小时不打扰人」，四块每一块都直接对着这个目标：
    /// 不啸叫、不把键盘声播给别人、不用手动调音量、不传低频隆隆声。
    fn default() -> Self {
        Self {
            echo_cancel: true,
            noise_suppression: true,
            gain_control: true,
            high_pass: true,
            stream_delay_ms: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApmModule {
    EchoCancel,
    NoiseSuppression,
    GainControl,
    HighPass,
}

impl ApmModule {
    pub const ALL: [ApmModule; 4] = [
        ApmModule::EchoCancel,
        ApmModule::NoiseSuppression,
        ApmModule::GainControl,
        ApmModule::HighPass,
    ];
}

impl std::fmt::Display for ApmModule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ApmModule::EchoCancel => "AEC3",
            ApmModule::NoiseSuppression => "降噪",
            ApmModule::GainControl => "AGC2",
            ApmModule::HighPass => "高通",
        })
    }
}

use std::sync::Mutex;

use sonora::config::{
    EchoCanceller, GainController2, HighPassFilter, NoiseSuppression, NoiseSuppressionLevel,
};
use sonora::{AudioProcessing, Config, StreamConfig};

pub use sonora::Error;

/// 一路 APM，纯 Rust 实现。单声道。
///
/// 接口跟 [`crate::apm::Apm`] 一一对应，可以直接换着用。
pub struct Apm {
    inner: Mutex<Inner>,
    frame_samples: usize,
    sample_rate: u32,
}

struct Inner {
    apm: AudioProcessing,
    /// 处理结果先落在这里，再拷回调用方的 buffer。见模块文档「三处实现差异」的第二条。
    scratch: Vec<f32>,
    /// `Some` 时每帧都要重设一次 —— 镜像 C++ 那份的行为，见模块文档。
    ///
    /// C++ 那份把 `stream_delay_ms` 放在 `EchoCanceller::Full` 里，但底层
    /// 每次 `process_capture` 之前都会调一遍 `set_stream_delay_ms`。
    /// sonora 把它暴露成普通的运行时 setter，所以这里显式照着做，
    /// 保证两个后端喂给 AEC3 的东西完全一样 —— 否则 A/B 比的就不是同一件事。
    stream_delay_ms: Option<i32>,
}

impl Apm {
    pub fn new(sample_rate: u32, cfg: ApmConfig) -> Result<Self, Error> {
        let stream = StreamConfig::new(sample_rate, 1);
        let frame_samples = stream.num_frames();

        let mut apm = AudioProcessing::builder()
            .config(build_config(cfg))
            .capture_config(stream)
            .render_config(stream)
            .build();

        let stream_delay_ms = cfg.stream_delay_ms.map(i32::from);
        // 建好就先设一次，让第一帧就有值可用。
        if let Some(d) = stream_delay_ms {
            apm.set_stream_delay_ms(d)?;
        }

        Ok(Self {
            inner: Mutex::new(Inner {
                apm,
                scratch: vec![0.0; frame_samples],
                stream_delay_ms,
            }),
            frame_samples,
            sample_rate,
        })
    }

    /// 一帧多少个采样点。48 kHz 下是 480（10 ms）。
    pub fn frame_samples(&self) -> usize {
        self.frame_samples
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// 处理近端（麦克风）的一帧，**就地修改**。
    pub fn process_capture(&self, mono: &mut [f32]) -> Result<(), Error> {
        debug_assert_eq!(mono.len(), self.frame_samples, "APM 只吃 10 ms 帧");
        let mut guard = self.lock();
        let Inner {
            apm,
            scratch,
            stream_delay_ms,
        } = &mut *guard;
        // 每帧重设，镜像 C++ 那份的行为。
        if let Some(d) = *stream_delay_ms {
            apm.set_stream_delay_ms(d)?;
        }
        apm.process_capture_f32(&[&*mono], &mut [scratch.as_mut_slice()])?;
        mono.copy_from_slice(scratch);
        Ok(())
    }

    /// 处理远端（我们要播出去的）的一帧，**就地修改**。
    pub fn process_render(&self, mono: &mut [f32]) -> Result<(), Error> {
        debug_assert_eq!(mono.len(), self.frame_samples, "APM 只吃 10 ms 帧");
        let mut guard = self.lock();
        let Inner { apm, scratch, .. } = &mut *guard;
        apm.process_render_f32(&[&*mono], &mut [scratch.as_mut_slice()])?;
        mono.copy_from_slice(scratch);
        Ok(())
    }

    /// 只把远端喂给 AEC 当参考，不修改它。
    ///
    /// sonora 没有「只分析」的接口，所以结果往 scratch 里丢了 —— 喂给 AEC 的
    /// 东西跟 C++ 那份一样，只是多一次拷贝。见模块文档「三处实现差异」的第三条。
    pub fn analyze_render(&self, mono: &mut [f32]) -> Result<(), Error> {
        debug_assert_eq!(mono.len(), self.frame_samples, "APM 只吃 10 ms 帧");
        let mut guard = self.lock();
        let Inner { apm, scratch, .. } = &mut *guard;
        apm.process_render_f32(&[&*mono], &mut [scratch.as_mut_slice()])
    }

    /// 锁中毒了就把它捡回来。
    ///
    /// 中毒意味着别的线程在持锁时 panic 了。APM 里存的是滤波器状态，不是
    /// 什么要维持不变量的东西 —— 最坏的结果是 AEC 要重新收敛一次。
    /// 为这个把整条语音链路弄断，才是更糟的选择。
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// 把 [`ApmConfig`] 翻译成 sonora 的配置。
///
/// **必须跟 `apm.rs` 的 `build_config` 逐项对齐** —— 两边配出来的东西不一样的话，
/// A/B 比的就不是同一件事了。
fn build_config(cfg: ApmConfig) -> Config {
    Config {
        // 跟 C++ 那份一样：AEC 和降噪会强制打开高通，这里显式跟随，
        // 免得"关掉高通"这个配置项在报告里看起来生效了、实际没有。
        high_pass_filter: (cfg.high_pass || cfg.echo_cancel || cfg.noise_suppression).then_some(
            HighPassFilter {
                apply_in_full_band: true,
            },
        ),
        // sonora 把 stream_delay_ms 挪到了运行时 setter，所以这里只有开关。
        echo_canceller: cfg.echo_cancel.then_some(EchoCanceller {
            // **必须显式写 false，不能用 default()。**
            //
            // sonora 这个字段默认是 true，而上游那个 crate 在 FFI 那层
            // 把它**硬编码成了 false**（注释原话：高通已经有独立的配置项了，
            // 这里再开一次是重复）。用 default() 的话两个后端配出来的东西不一样，
            // A/B 就成了在比两份不同的配置。
            enforce_high_pass_filtering: false,
            ..EchoCanceller::default()
        }),
        noise_suppression: cfg.noise_suppression.then_some(NoiseSuppression {
            level: NoiseSuppressionLevel::Moderate,
            // 上游叫 analyze_linear_aec_output，这边叫 ..._when_available，同一个东西。
            analyze_linear_aec_output_when_available: false,
        }),
        // 两边的 GainController2 都是 derive 的 Default（adaptive_digital: None），
        // 所以 default() 在两个后端里是同一个意思。
        gain_controller2: cfg.gain_control.then_some(GainController2::default()),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const RATE: u32 = 48_000;

    #[test]
    fn frame_is_ten_milliseconds() {
        let apm = Apm::new(RATE, ApmConfig::default()).unwrap();
        assert_eq!(apm.frame_samples(), 480, "48 kHz 下 10 ms 就是 480 点");
        assert_eq!(apm.frame_samples(), RATE as usize / 100);
    }

    #[test]
    fn every_config_combination_constructs() {
        // 全关 / 全开 / 每一块单独开，都必须能建起来。
        for cfg in [ApmConfig::none(), ApmConfig::default()]
            .into_iter()
            .chain(ApmModule::ALL.into_iter().map(ApmConfig::only))
        {
            Apm::new(RATE, cfg).unwrap_or_else(|e| panic!("{cfg:?} 建不起来: {e}"));
        }
    }

    #[test]
    fn processes_a_frame_without_destroying_it() {
        let apm = Apm::new(RATE, ApmConfig::default()).unwrap();
        let n = apm.frame_samples();
        let reference = crate::signal::chirp(n, RATE, 300.0, 3000.0, 0.3);
        let mut frame: Vec<f32> = reference.iter().map(|&s| s as f32 / 32768.0).collect();

        let mut render = vec![0.0f32; n];
        apm.analyze_render(&mut render).unwrap();
        apm.process_capture(&mut frame).unwrap();

        assert_eq!(frame.len(), n, "帧长不能变");
        assert!(frame.iter().all(|s| s.is_finite()), "输出里不能有 NaN/Inf");
        // 第一帧 AEC/AGC 还没收敛，不断言幅度，只断言没把信号整个吃掉或炸掉。
        let peak = frame.iter().fold(0.0f32, |a, s| a.max(s.abs()));
        assert!(peak <= 1.5, "输出爆掉了: peak={peak}");
    }

    #[test]
    fn high_pass_is_forced_on_by_aec_and_ns() {
        // 关掉高通但开 AEC，上游会强制把高通打开。我们的配置构造要跟着说实话，
        // 否则报告里"高通 关"这一行是假的。
        let forced = build_config(ApmConfig {
            high_pass: false,
            echo_cancel: true,
            ..ApmConfig::none()
        });
        assert!(forced.high_pass_filter.is_some());

        let genuinely_off = build_config(ApmConfig::none());
        assert!(genuinely_off.high_pass_filter.is_none());
    }

    /// 拿冲激独立验一遍 APM 的群延迟。
    ///
    /// device-probe 用互相关量这个数，而整条 80 ms 红线过不过就压在它身上，
    /// 所以这里用**完全不同的方法**再验一次：送一个冲激进去，看它从输出的哪个
    /// 位置冒出来。两种方法量出来的量级对不上，说明有一边错了。
    #[test]
    fn full_config_group_delay_is_sane() {
        let apm = Apm::new(RATE, ApmConfig::default()).unwrap();
        let n = apm.frame_samples();

        // 先空跑一秒让各块收敛，再送冲激。
        for _ in 0..100 {
            let mut render = vec![0.0f32; n];
            let mut silence = vec![0.0f32; n];
            apm.analyze_render(&mut render).unwrap();
            apm.process_capture(&mut silence).unwrap();
        }

        // 冲激放在一帧的开头，然后收集后面若干帧的输出。
        let mut out: Vec<f32> = Vec::new();
        for k in 0..12 {
            let mut render = vec![0.0f32; n];
            apm.analyze_render(&mut render).unwrap();
            let mut frame = vec![0.0f32; n];
            if k == 0 {
                frame[0] = 0.5;
            }
            apm.process_capture(&mut frame).unwrap();
            out.extend_from_slice(&frame);
        }

        let (peak_idx, peak) = out
            .iter()
            .enumerate()
            .map(|(i, s)| (i, s.abs()))
            .fold((0usize, 0.0f32), |a, b| if b.1 > a.1 { b } else { a });

        assert!(peak > 1e-4, "冲激整个被吃掉了，peak={peak}");
        let delay_ms = peak_idx as f64 * 1000.0 / RATE as f64;
        // 互相关量到全开配置是 15 ms 左右。冲激法不会给一模一样的数
        // （降噪和 AEC 会把冲激抹开），但必须是同一个量级。
        assert!(
            (1.0..40.0).contains(&delay_ms),
            "群延迟 {delay_ms:.1} ms 不在合理范围 —— 跟互相关量到的 ~15 ms 对不上"
        );
    }

    #[test]
    fn stream_delay_is_accepted() {
        // M2 实测的设备延迟量级
        let cfg = ApmConfig {
            stream_delay_ms: Some(30),
            ..ApmConfig::default()
        };
        let apm = Apm::new(RATE, cfg).unwrap();
        let n = apm.frame_samples();
        let mut render = vec![0.0f32; n];
        let mut capture = vec![0.0f32; n];
        apm.analyze_render(&mut render).unwrap();
        apm.process_capture(&mut capture).unwrap();
    }
}

/// 真音箱真麦克风下的回声抑制。要 `codec` 是因为帧长常量和采集/播放 trait 在 `audio` 里。
#[cfg(all(test, windows, feature = "codec"))]
#[path = "apm_acoustic.rs"]
mod acoustic;

#[cfg(test)]
mod echo {
    use super::*;

    const FRAME: usize = 480;
    /// 模拟的声学回路延迟：扬声器出声到麦克风收回来。
    /// M2 实测这台机器上是 30 ms 上下（渲染队列 20 + 采集 10.4）。
    const ECHO_DELAY_FRAMES: usize = 3;
    /// 回声比原声小多少。
    ///
    /// -6 dB：音箱开到能听清、麦克风就在旁边。比这更小的话回声本身就
    /// 快贴到 AEC 的残留底噪了，量出来的抑制量反映的是底噪而不是 AEC 的本事 ——
    /// 第一版用 -12 dB，量出来只有 17 dB，差点让人以为 AEC 不行。
    const ECHO_GAIN: f32 = 0.5;

    /// `pub(super)`：[`super::acoustic`] 要用同一个算法，否则两边的 dB 对不上。
    pub(super) fn energy_db(samples: &[f32]) -> f32 {
        if samples.is_empty() {
            return -120.0;
        }
        let rms = (samples.iter().map(|s| s * s).sum::<f32>() / samples.len() as f32).sqrt();
        if rms <= 1e-9 {
            -120.0
        } else {
            20.0 * rms.log10()
        }
    }

    /// 造一段像人说话的信号：带限到人声频段的噪声 × 音节包络。
    ///
    /// # 为什么不用正弦
    ///
    /// 第一版用的是两个正弦加一点噪声，量出来 AEC 只消掉 2.9 dB，
    /// 差点让人以为 AEC3 不行。**纯音对自适应滤波器是退化激励** ——
    /// 它只激发两个频点，滤波器在别的方向上根本没信息可学，
    /// 而真实语音是宽带的。同一套代码换成白噪声立刻就有 24.8 dB。
    ///
    /// 所以测 AEC 一定要用宽带信号。这里在白噪声上做两件事让它更像人声：
    /// 带限到 300–3400 Hz，再乘一个 4 Hz 上下的音节包络。
    pub(super) fn speech_like(frames: usize, seed: u64, target_rms: f32) -> Vec<f32> {
        let mut state = seed;
        let mut hp_prev_in = 0.0f32;
        let mut hp_prev_out = 0.0f32;
        let mut lp_prev = 0.0f32;
        // 一阶高通/低通的系数，300 Hz 和 3400 Hz
        let hp_a = (-std::f32::consts::TAU * 300.0 / 48_000.0).exp();
        let lp_b = 1.0 - (-std::f32::consts::TAU * 3400.0 / 48_000.0).exp();

        let mut out: Vec<f32> = (0..frames * FRAME)
            .map(|i| {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                // (state >> 40) 是 24 位，除以 2^23 再减 1 落在 ±1。
                let white = (state >> 40) as f32 / 8_388_608.0 - 1.0;

                hp_prev_out = hp_a * (hp_prev_out + white - hp_prev_in);
                hp_prev_in = white;
                lp_prev += lp_b * (hp_prev_out - lp_prev);

                // 音节包络：3.5 Hz，不到零（人说话时也有气声）
                let t = i as f32 / 48_000.0;
                let envelope = 0.25 + 0.75 * (0.5 + 0.5 * (t * 3.5 * std::f32::consts::TAU).sin());
                lp_prev * envelope
            })
            .collect();

        // 归一到指定的 RMS，这样测试里的分贝数是我们说了算的
        let rms = (out.iter().map(|s| s * s).sum::<f32>() / out.len() as f32).sqrt();
        if rms > 1e-9 {
            let gain = target_rms / rms;
            for s in out.iter_mut() {
                *s = (*s * gain).clamp(-1.0, 1.0);
            }
        }
        out
    }

    /// 远端：RMS -18 dB，音箱开到能听清的量级。
    fn farend(frames: usize) -> Vec<f32> {
        speech_like(frames, 0x2545_F491_4F6C_DD1D, 0.125)
    }

    /// **AEC3 真的能把回声消掉。** 这是「外放开黑不啸叫」的全部依据。
    ///
    /// 场景：对面在说话（远端），从我的音箱放出来，又被我的麦克风收回去。
    /// 我自己一声不吭。那么麦克风里的东西**全都是回声**，AEC 之后应该几乎什么都不剩。
    /// **AEC3 真的能把回声消掉。** 这是「外放开黑不啸叫」的全部依据。
    ///
    /// 场景：对面在说话（远端），从我的音箱放出来，又被我的麦克风收回去。
    /// 我自己一声不吭。那么麦克风里的东西**全都是回声**，AEC 之后应该几乎什么都不剩。
    ///
    /// 注意这是**线性**回路，量到的是"滤波器收敛得多好"。真声学回路的非线性
    /// 失真、时钟漂移、混响都不在这里 —— 那个在 `mod acoustic`。
    #[test]
    fn aec_actually_cancels_the_echo() {
        let apm = Apm::new(
            48_000,
            ApmConfig {
                echo_cancel: true,
                noise_suppression: false,
                // 关掉 AGC：它会在回声被消掉之后把残留拉起来，
                // 让这个测试量的不再是「消掉了多少」。
                gain_control: false,
                high_pass: true,
                stream_delay_ms: Some(ECHO_DELAY_FRAMES as u16 * 10),
            },
        )
        .expect("建不了 APM");

        let frames = 400;
        let far = farend(frames);
        let mut echo_in = Vec::new();
        let mut mic_out = Vec::new();

        for i in 0..frames {
            let far_frame = &far[i * FRAME..(i + 1) * FRAME];

            // 这一帧要播出去 —— 先告诉 AEC
            let mut render = far_frame.to_vec();
            apm.analyze_render(&mut render).unwrap();

            // 麦克风收到的：几帧之前播出去的东西，衰减一些。我自己没说话。
            let mut mic = if i >= ECHO_DELAY_FRAMES {
                let src =
                    &far[(i - ECHO_DELAY_FRAMES) * FRAME..(i - ECHO_DELAY_FRAMES + 1) * FRAME];
                src.iter().map(|s| s * ECHO_GAIN).collect::<Vec<f32>>()
            } else {
                vec![0.0; FRAME]
            };
            echo_in.extend_from_slice(&mic);

            apm.process_capture(&mut mic).unwrap();
            mic_out.extend_from_slice(&mic);
        }

        // 先看收敛过程，再下结论。一上来就取个平均，看不出「是还没收敛」
        // 还是「就这个水平」—— 而这两件事的应对完全不一样。
        println!("每 50 帧（0.5 秒）一段的回声抑制：");
        for block in 0..frames / 50 {
            let a = block * 50 * FRAME;
            let b = (block + 1) * 50 * FRAME;
            println!(
                "  {:>4.1}s  {:>5.1} dB",
                (block * 50) as f32 * 0.01,
                energy_db(&echo_in[a..b]) - energy_db(&mic_out[a..b])
            );
        }

        // 只看后半段：AEC3 要时间估出回声路径，前面那段是在收敛。
        let tail = frames * FRAME / 2;
        let before = energy_db(&echo_in[tail..]);
        let after = energy_db(&mic_out[tail..]);
        let cancelled = before - after;

        println!("回声进去 {before:.1} dB，出来 {after:.1} dB，消掉了 {cancelled:.1} dB");
        assert!(
            before < 0.0,
            "测试信号超了满刻度，量出来的分贝不作数：{before:.1} dB"
        );

        // 两条都要满足，因为它们说的是两件事：
        //
        // - **抑制量**决定会不会啸叫（回路增益要小于 1）
        // - **残留的绝对电平**决定听不听得见。AEC3 压到一个固定的底噪就到头了，
        //   所以回声越响，抑制量的数字越好看 —— 只看比值会被这一点骗到。
        assert!(
            cancelled > 25.0,
            "AEC 只消掉了 {cancelled:.1} dB —— 外放开黑会啸叫"
        );
        assert!(after < -40.0, "残留回声 {after:.1} dB，还听得见");
    }

    /// **没有回声的时候，AEC 不该动我的声音。**
    ///
    /// 这是真正会砸掉产品的那条：戴耳机的人根本没有回声，如果开了 AEC
    /// 之后他的声音被削掉一截，那这个功能就是负收益 —— 而绝大多数人戴耳机。
    /// **没有回声的时候，AEC 不该动我的声音。**
    ///
    /// 这是真正会砸掉产品的那条：戴耳机的人根本没有回声，如果开了 AEC
    /// 之后他的声音被削掉一截，那这个功能就是负收益 —— 而绝大多数人戴耳机。
    #[test]
    fn aec_leaves_speech_alone_when_there_is_no_echo() {
        let apm = Apm::new(
            48_000,
            ApmConfig {
                echo_cancel: true,
                noise_suppression: false,
                gain_control: false,
                high_pass: true,
                stream_delay_ms: Some(ECHO_DELAY_FRAMES as u16 * 10),
            },
        )
        .expect("建不了 APM");

        let frames = 400;
        let near = speech_like(frames, 0x9E37_79B9_7F4A_7C15, 0.125);
        let silence = vec![0.0f32; FRAME];

        let mut out = Vec::new();
        for i in 0..frames {
            // 远端全程静音：耳机用户的实际情况
            let mut render = silence.clone();
            apm.analyze_render(&mut render).unwrap();

            let mut mic = near[i * FRAME..(i + 1) * FRAME].to_vec();
            apm.process_capture(&mut mic).unwrap();
            out.extend_from_slice(&mic);
        }

        let tail = frames * FRAME / 2;
        let before = energy_db(&near[tail..]);
        let after = energy_db(&out[tail..]);
        let lost = before - after;

        println!("没有回声时：进去 {before:.1} dB，出来 {after:.1} dB，掉了 {lost:.1} dB");
        assert!(
            lost < 3.0,
            "没有回声却把声音削掉了 {lost:.1} dB —— 戴耳机的人开 AEC 反而变差"
        );
    }

    // 双讲（两个人同时说）的表现没有在这里量。
    //
    // 试过了，量不出有意义的数字：合成的近端和远端在 AEC3 眼里是同一种东西
    // （同样的带限噪声、同样的音节包络），抑制器分不开，于是把两个一起压到
    // 残留底噪 —— 得到的是测试信号的性质，不是 AEC3 的性质。
    //
    // 真实双讲里两个人的声音在频谱和时序上都是可区分的。要认真量这件事
    // 得用 ITU-T G.168 那样的标准测试集，或者干脆戴上耳机找个人说话。
    // 在那之前，与其报一个站不住的数字，不如明说没量。
}
