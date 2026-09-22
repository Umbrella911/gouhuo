// SPDX-License-Identifier: MPL-2.0
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

    fn energy_db(samples: &[f32]) -> f32 {
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
    fn speech_like(frames: usize, seed: u64, target_rms: f32) -> Vec<f32> {
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
