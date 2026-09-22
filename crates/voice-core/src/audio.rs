// SPDX-License-Identifier: MPL-2.0

//! 音频设备的抽象，以及不需要真声卡的实现。
//!
//! # 为什么要抽象
//!
//! 整条语音链路（采集 → APM → Opus → 加密 → UDP → 抖动缓冲 → 解码 → 混音 → 播放）
//! 里，只有两头跟硬件打交道。把这两头抽成 trait 之后：
//!
//! - **整条链路能在没有声卡的机器上测**。CI 跑的就是这个，而且能喂一个已知的
//!   信号进去、从另一头取出来做互相关 —— 这是唯一能**真正量到端到端延迟**的办法。
//!   M1 和 M2 量的是分段，加起来的 91.9 ms 从没作为一条链路验证过。
//! - 换设备、加「测试音」功能、将来把 voice-core 拆成独立进程，都不动中间那段。
//!
//! # 为什么帧长写死 10 ms
//!
//! APM 只接受 10 ms 的帧（libwebrtc 的硬性约定），Opus 也支持 10 ms。
//! 让整条链路自始至终用同一个帧长，直接消掉一整类「这里攒一点那里补一点」
//! 的缓冲代码 —— 那种代码是延迟悄悄变大的主要来源，而且极难查。
//!
//! 代价是不能用 20 ms 帧省带宽。M1 量过：10 ms 帧的协议延迟 46.5 ms，
//! 20 ms 帧要多一档，而带宽产品线放开之后省那点已经不值得了。
//!
//! # 为什么是 f32 单声道
//!
//! APM 吃 f32，Opus 也有 f32 接口，WASAPI 共享模式的混音格式基本都是 f32。
//! 全程 f32 就不用在中间转来转去。单声道是因为人说话不需要立体声，
//! 而双声道直接让带宽和 CPU 翻倍。

use std::io;

/// 采样率。Opus 的原生速率之一，也是 WASAPI 共享模式最常见的引擎速率。
pub const SAMPLE_RATE: u32 = protocol::SAMPLE_RATE;

/// 一帧多少毫秒。见模块文档，**不要改**。
pub const FRAME_MS: u32 = 10;

/// 一帧多少个采样点。
pub const FRAME_SAMPLES: usize = (SAMPLE_RATE as usize * FRAME_MS as usize) / 1000;

/// 采集端。
///
/// [`read`](Capture::read) **应该阻塞**到有一整帧为止 —— 它同时也是发送线程的时钟。
/// 自己拿 sleep 去凑节拍会跟设备的真实节奏漂开，然后表现为周期性的爆音。
pub trait Capture: Send {
    /// 读满一帧。返回 `false` 表示流结束了（设备拔了、测试信号放完了）。
    fn read(&mut self, out: &mut [f32]) -> io::Result<bool>;
}

/// 播放端。
///
/// [`write`](Render::write) **应该阻塞**到设备收得下为止 —— 它是播放线程的时钟。
/// 这样播放节拍直接由设备决定，不存在我们的时钟和设备时钟对不齐的问题。
pub trait Render: Send {
    fn write(&mut self, frame: &[f32]) -> io::Result<()>;
}

/// 把一段固定的采样点当麦克风用，放完就结束。
///
/// 按真实速度出帧（10 ms 一帧），所以拿它测出来的延迟是有意义的 ——
/// 全速灌进去的话，量到的只是 CPU 有多快。
pub struct SyntheticCapture {
    samples: Vec<f32>,
    cursor: usize,
    ticker: crate::clock::Ticker,
    /// 放完之后继续出静音而不是结束。量延迟时要用：信号已经进了链路，
    /// 但链路还得继续转才能把它推出来。
    pub silence_after_end: bool,
    first_frame_at: TimeMark,
}

/// 「第一帧是什么时候的事」。量端到端延迟时，两头各记一个，差值才有意义。
pub type TimeMark = std::sync::Arc<std::sync::Mutex<Option<std::time::Instant>>>;

fn mark(slot: &TimeMark) {
    let mut slot = slot.lock().expect("time mark poisoned");
    if slot.is_none() {
        *slot = Some(std::time::Instant::now());
    }
}

impl SyntheticCapture {
    pub fn new(samples: Vec<f32>) -> Self {
        Self {
            samples,
            cursor: 0,
            ticker: crate::clock::Ticker::start(std::time::Duration::from_millis(FRAME_MS as u64))
                .0,
            silence_after_end: false,
            first_frame_at: TimeMark::default(),
        }
    }

    /// 第一帧的第一个采样点是什么时候送出去的。
    ///
    /// 这是量延迟的**时间原点**。记在设备里面而不是外面，因为外面记的是
    /// 「线程起来了」，中间还隔着一次线程调度 —— 那个抖动比要量的东西还大。
    pub fn first_frame_at(&self) -> TimeMark {
        std::sync::Arc::clone(&self.first_frame_at)
    }

    /// 放完信号之后继续出静音，直到调用方自己停。
    pub fn then_silence(mut self) -> Self {
        self.silence_after_end = true;
        self
    }

    /// 已经送出去多少个采样点。用来对齐互相关的起点。
    pub fn position(&self) -> usize {
        self.cursor
    }
}

impl Capture for SyntheticCapture {
    fn read(&mut self, out: &mut [f32]) -> io::Result<bool> {
        if self.cursor >= self.samples.len() && !self.silence_after_end {
            return Ok(false);
        }
        self.ticker.tick();
        mark(&self.first_frame_at);

        for (i, slot) in out.iter_mut().enumerate() {
            *slot = self.samples.get(self.cursor + i).copied().unwrap_or(0.0);
        }
        self.cursor += out.len();
        Ok(true)
    }
}

/// 把播放出来的采样点攒起来，并按真实速度消费。
///
/// 按真实速度是必须的：如果 `write` 立刻返回，播放线程会全速空转，
/// 抖动缓冲永远来不及填，量出来的东西没有任何意义。
pub struct CollectingRender {
    played: std::sync::Arc<std::sync::Mutex<Vec<f32>>>,
    ticker: crate::clock::Ticker,
    first_frame_at: TimeMark,
}

impl CollectingRender {
    pub fn new() -> (Self, std::sync::Arc<std::sync::Mutex<Vec<f32>>>) {
        let played = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let sink = Self {
            played: std::sync::Arc::clone(&played),
            ticker: crate::clock::Ticker::start(std::time::Duration::from_millis(FRAME_MS as u64))
                .0,
            first_frame_at: TimeMark::default(),
        };
        (sink, played)
    }

    /// 第一帧的第一个采样点是什么时候播出去的。见 [`SyntheticCapture::first_frame_at`]。
    pub fn first_frame_at(&self) -> TimeMark {
        std::sync::Arc::clone(&self.first_frame_at)
    }
}

impl Render for CollectingRender {
    fn write(&mut self, frame: &[f32]) -> io::Result<()> {
        self.ticker.tick();
        mark(&self.first_frame_at);
        self.played
            .lock()
            .expect("played poisoned")
            .extend_from_slice(frame);
        Ok(())
    }
}

/// 什么都不做的播放端。只测发送那半边时用。
pub struct NullRender {
    ticker: crate::clock::Ticker,
}

impl Default for NullRender {
    fn default() -> Self {
        Self {
            ticker: crate::clock::Ticker::start(std::time::Duration::from_millis(FRAME_MS as u64))
                .0,
        }
    }
}

impl Render for NullRender {
    fn write(&mut self, _frame: &[f32]) -> io::Result<()> {
        self.ticker.tick();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn frame_size_matches_ten_milliseconds() {
        assert_eq!(FRAME_SAMPLES, 480);
        assert_eq!(
            FRAME_SAMPLES * 1000 / SAMPLE_RATE as usize,
            FRAME_MS as usize
        );
    }

    #[test]
    fn synthetic_capture_hands_out_the_signal_then_stops() {
        let signal: Vec<f32> = (0..FRAME_SAMPLES * 3).map(|i| i as f32).collect();
        let mut capture = SyntheticCapture::new(signal);
        let mut frame = vec![0.0; FRAME_SAMPLES];

        for n in 0..3 {
            assert!(capture.read(&mut frame).unwrap());
            assert_eq!(frame[0], (n * FRAME_SAMPLES) as f32);
        }
        assert!(!capture.read(&mut frame).unwrap(), "放完了就该结束");
    }

    /// 不足一帧的尾巴要补零，不能把上一帧的内容留在里面 ——
    /// 那会在录音末尾拼出一段谁也没说过的声音。
    #[test]
    fn a_short_tail_is_zero_padded() {
        let mut capture = SyntheticCapture::new(vec![1.0; FRAME_SAMPLES + 5]);
        let mut frame = vec![9.0; FRAME_SAMPLES];
        assert!(capture.read(&mut frame).unwrap());
        assert!(capture.read(&mut frame).unwrap());
        assert_eq!(frame[0], 1.0);
        assert_eq!(frame[5], 0.0, "尾巴之后必须是零");
        assert_eq!(frame[FRAME_SAMPLES - 1], 0.0);
    }

    #[test]
    fn silence_after_end_keeps_the_pipeline_turning() {
        let mut capture = SyntheticCapture::new(vec![1.0; FRAME_SAMPLES]).then_silence();
        let mut frame = vec![0.0; FRAME_SAMPLES];
        assert!(capture.read(&mut frame).unwrap());
        assert!(capture.read(&mut frame).unwrap(), "该继续出静音");
        assert!(frame.iter().all(|&s| s == 0.0));
    }

    /// 合成设备必须按真实速度跑，否则量出来的延迟毫无意义。
    #[test]
    fn synthetic_devices_run_at_real_time() {
        let frames = 20;
        let mut capture = SyntheticCapture::new(vec![0.0; FRAME_SAMPLES * frames]);
        let mut frame = vec![0.0; FRAME_SAMPLES];

        let start = Instant::now();
        while capture.read(&mut frame).unwrap() {}
        let elapsed = start.elapsed().as_secs_f64() * 1000.0;

        let expected = (frames * FRAME_MS as usize) as f64;
        assert!(
            elapsed > expected * 0.8,
            "跑得比真实时间快得多（{elapsed:.1} ms vs {expected:.1} ms）—— 量出来的数字不作数"
        );
    }
}
