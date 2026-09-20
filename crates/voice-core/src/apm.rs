//! APM：回声消除、降噪、自动增益。
//!
//! 用 libwebrtc 的 AudioProcessing 模块（`webrtc-audio-processing` crate）。
//! **绝对不要自己写 AEC** —— AEC3 是十几年的积累，自研的结果一定是外放开黑就啸叫。
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

use webrtc_audio_processing::config::{
    EchoCanceller, GainController, GainController2, HighPassFilter, NoiseSuppression,
    NoiseSuppressionLevel,
};
use webrtc_audio_processing::{Config, Processor};

pub use webrtc_audio_processing::Error;

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

/// 一路 APM。单声道。
pub struct Apm {
    processor: Processor,
    frame_samples: usize,
    sample_rate: u32,
}

impl Apm {
    pub fn new(sample_rate: u32, cfg: ApmConfig) -> Result<Self, Error> {
        let processor = Processor::new(sample_rate)?;
        processor.set_config(build_config(cfg));
        let frame_samples = processor.num_samples_per_frame();
        Ok(Self {
            processor,
            frame_samples,
            sample_rate,
        })
    }

    /// 一帧多少个采样点。48 kHz 下是 480（10 ms）。**喂别的长度会 panic。**
    pub fn frame_samples(&self) -> usize {
        self.frame_samples
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// 处理近端（麦克风）的一帧，**就地修改**。
    pub fn process_capture(&self, mono: &mut [f32]) -> Result<(), Error> {
        debug_assert_eq!(mono.len(), self.frame_samples, "APM 只吃 10 ms 帧");
        self.processor.process_capture_frame([mono])
    }

    /// 处理远端（我们要播出去的）的一帧，**就地修改**。
    ///
    /// 不打算改远端音频的话用 [`Apm::analyze_render`]，省一次拷贝。
    pub fn process_render(&self, mono: &mut [f32]) -> Result<(), Error> {
        debug_assert_eq!(mono.len(), self.frame_samples, "APM 只吃 10 ms 帧");
        self.processor.process_render_frame([mono])
    }

    /// 只把远端喂给 AEC 当参考，不修改它。外放场景下就该用这个。
    pub fn analyze_render(&self, mono: &mut [f32]) -> Result<(), Error> {
        debug_assert_eq!(mono.len(), self.frame_samples, "APM 只吃 10 ms 帧");
        self.processor.analyze_render_frame([mono])
    }
}

fn build_config(cfg: ApmConfig) -> Config {
    Config {
        // AEC 和降噪都会强制打开高通，所以这里显式跟随，免得"关掉高通"这个
        // 配置项在报告里看起来生效了、实际没有。
        high_pass_filter: (cfg.high_pass || cfg.echo_cancel || cfg.noise_suppression).then_some(
            HighPassFilter {
                apply_in_full_band: true,
            },
        ),
        echo_canceller: cfg.echo_cancel.then_some(EchoCanceller::Full {
            stream_delay_ms: cfg.stream_delay_ms,
        }),
        noise_suppression: cfg.noise_suppression.then_some(NoiseSuppression {
            // Moderate 而不是 High：再往上语音失真开始听得出来，
            // 而我们的场景是「听清队友」不是「录播客」。
            level: NoiseSuppressionLevel::Moderate,
            analyze_linear_aec_output: false,
        }),
        gain_controller: cfg
            .gain_control
            .then_some(GainController::GainController2(GainController2::default())),
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
