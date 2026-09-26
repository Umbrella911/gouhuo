// SPDX-License-Identifier: MPL-2.0
//! 声学往返：用**真音箱和真麦克风**量回声抑制，两个后端各跑一遍。
//!
//! # 为什么非要有这个
//!
//! `mod echo` 里那套合成测试的回声路径是**纯线性**的：延迟 + 衰减，没别的。
//! 线性路径上自适应滤波器能收敛到近乎完美，所以那里量到的是「滤波器收敛得多好」。
//!
//! 真实的音箱-房间-麦克风回路完全不是那样：
//!
//! - **音箱是非线性的。** 音量开大了纸盆会削波，功放也会。回声里出现了远端信号里
//!   根本没有的谐波 —— 线性滤波器对这部分**无能为力**，只能靠后面的抑制器压。
//! - **两个晶振在漂。** 音箱和麦克风是两个设备、两个时钟，滤波器刚收敛好就开始
//!   慢慢对不上，得一直重新跟踪。
//! - **房间有混响。** 回声不是一个延迟，是一条几十甚至上百毫秒的拖尾。
//! - **有本底噪声。** 风扇、空调，还有麦克风自己的底噪。
//!
//! **区分 AEC 好坏的恰恰是这些东西。** 所以合成测试跑出来的 dB 数**不能**直接
//! 当成产品表现，这个模块才能。
//!
//! # 跑法
//!
//! ```bash
//! cargo test -p voice-core --release acoustic -- --ignored --nocapture --test-threads 1
//! ```
//!
//! **会真的出声，也会真的录音。** 跑之前要摆出「外放开黑」的场景：
//!
//! 1. 音箱开着，音量在平时听得清的位置（不是最大）
//! 2. **摘掉耳机** —— 戴着耳机就没有回声可消，这个测试会直接判定"没有回声"并失败
//! 3. 人别说话，安静几十秒
//!
//! 设备默认用系统默认端点（那才是用户实际的音频路径）。要指定别的：
//!
//! ```bash
//! set GOUHUO_AEC_RENDER=EDIFIER
//! set GOUHUO_AEC_CAPTURE=Nova
//! ```
//!
//! 值是设备名的一部分，大小写不敏感。
//!
//! # 为什么用两个线程
//!
//! 真客户端就是两个线程：采集线程调 `process_capture`，播放线程调 `analyze_render`，
//! 中间是一个 `Arc<dyn AudioProcessor>`。这里照搬同一个结构，顺便把 sonora 后端那把
//! `Mutex` 的争用也一起测了 —— 单线程跑的话那条路径根本不会被走到。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use super::echo::{energy_db, speech_like};
use super::{Apm, ApmConfig};
use crate::audio::{Capture, Render, FRAME_SAMPLES, SAMPLE_RATE};
use crate::wasapi::{list_endpoints, Direction, WasapiCapture, WasapiRender};

/// 远端信号的电平，RMS。
///
/// -18 dBFS：音箱开到能听清、麦克风就在旁边的量级，跟合成测试用的是同一个数，
/// 这样两边的 dB 能对照着看。再大就开始逼音箱进削波区 —— 那确实更接近
/// "开大声吵架"的场景，但先把常规音量测明白。
const FAR_RMS: f32 = 0.125;

/// 告诉 AEC3 的设备延迟。M2 实测：渲染队列 20 + 采集 10.4 ≈ 30 ms。
const STREAM_DELAY_MS: u16 = 30;

/// 回声至少要比本底噪声高这么多，这次测量才作数。
///
/// 低于这条线只有一个解释：**麦克风根本没听见音箱**（戴着耳机、音箱没开、
/// 麦克风选错了、或者 Windows 自己的回声消除已经先把它吃掉了）。
/// 这时候报一个"抑制了 60 dB"是纯粹的假数字 —— 消掉的是不存在的东西。
const MIN_ECHO_OVER_NOISE_DB: f32 = 10.0;

/// 本底噪声低于这个数，就认定麦克风自己带噪声门。
///
/// -90 dB 已经贴着 16 位的最低位了。**真实的房间不可能这么安静** ——
/// 再静的房间加上麦克风自身的底噪，也就 -70 dB 上下。量到 -96 只有一个解释：
/// 麦克风（或者它的驱动）在没声音的时候直接输出了全零。
///
/// # 为什么这条会让整次测量作废
///
/// 噪声门是**非线性、时变**的处理，而且它在我们的 APM **之前**。
/// AEC 的线性滤波器建模的是"播出去的信号怎么变成录回来的信号"，
/// 中间插一个会突然把信号整个掐掉的门，这个映射就不存在了 ——
/// 滤波器刚学会就被推翻一次。
///
/// 实测（Arctis Nova Pro Wireless）：webrtc 7.4 dB、sonora 12.9 dB，
/// 而同样两个后端在合成的线性回路上是 26 dB 和 58 dB。差的那几十 dB
/// 不是 AEC 不行，是这条链路上根本没有可学的东西。
///
/// 游戏耳麦基本都有这个（SteelSeries ClearCast、雷蛇、HyperX 都是），
/// 所以**量 AEC 要用不带 DSP 的麦克风**：普通 USB 麦、3.5mm 麦、摄像头麦。
const DIGITAL_SILENCE_DB: f32 = -90.0;

/// 一次声学往返量到的东西。
struct Acoustic {
    render_name: String,
    capture_name: String,
    /// 什么都不播的时候麦克风听到的。房间 + 麦克风的本底。
    noise_floor_db: f32,
    /// 播着远端信号、**进 APM 之前**麦克风听到的。这就是真实回声。
    echo_in_db: f32,
    /// 出 APM 之后还剩下的。
    residual_db: f32,
}

impl Acoustic {
    /// 消掉了多少。这是"会不会啸叫"的依据（回路增益要小于 1）。
    fn erle_db(&self) -> f32 {
        self.echo_in_db - self.residual_db
    }

    /// 回声比本底高多少。低于 [`MIN_ECHO_OVER_NOISE_DB`] 这次测量就不作数。
    fn echo_over_noise_db(&self) -> f32 {
        self.echo_in_db - self.noise_floor_db
    }

    /// 残留比本底高多少。
    ///
    /// **这条比 ERLE 更接近"听不听得见"。** 残留一旦压到本底噪声以下，
    /// 再多的抑制量都没有意义了 —— 你听到的是房间噪声，不是回声。
    /// 而且这时候 ERLE 的数字还会继续涨（回声越响分子越大），很容易被它骗到。
    fn residual_over_noise_db(&self) -> f32 {
        self.residual_db - self.noise_floor_db
    }
}

/// 按名字挑设备，挑不到就用系统默认。
///
/// 返回 `(device_id, 显示名)`。`None` 的 id 表示"系统默认"。
fn pick(direction: Direction, env: &str) -> Result<(Option<String>, String), String> {
    let endpoints = list_endpoints(direction).map_err(|e| format!("列设备失败: {e}"))?;
    if endpoints.is_empty() {
        return Err("一个设备都没有".into());
    }

    if let Ok(want) = std::env::var(env) {
        let want = want.to_lowercase();
        let hit = endpoints
            .iter()
            .find(|e| e.name.to_lowercase().contains(&want))
            .ok_or_else(|| {
                format!(
                    "{env}={want} 没匹配到设备。有这些：\n  {}",
                    endpoints
                        .iter()
                        .map(|e| e.name.as_str())
                        .collect::<Vec<_>>()
                        .join("\n  ")
                )
            })?;
        return Ok((Some(hit.id.clone()), hit.name.clone()));
    }

    let default = endpoints
        .iter()
        .find(|e| e.is_default)
        .unwrap_or(&endpoints[0]);
    Ok((None, default.name.clone()))
}

/// 流刚开起来的头半秒一律丢掉。
///
/// **不丢的话本底噪声会量成数字静音。** 无线耳麦（Arctis 这类）在
/// `Start()` 之后要过一会儿才真的出数据，之前交付的是全零的帧 ——
/// 量出来 -96 dB，正好是 16 位的最低位，看着像个正经数字，其实是假的。
///
/// 第一次踩到这个坑的表现是：先跑的那个后端本底 -96.6 dB、后跑的 -75.0 dB，
/// 同一个房间同一个麦，差 21 dB。
const WARMUP_FRAMES: usize = 50;

/// 录一段麦克风，不做任何处理，返回 RMS 的 dB。用来量本底噪声。
fn listen(capture_id: Option<String>, frames: usize) -> Result<f32, String> {
    let mut capture = WasapiCapture::new(capture_id);
    let mut frame = vec![0.0f32; FRAME_SAMPLES];
    let mut all = Vec::with_capacity(frames * FRAME_SAMPLES);
    for k in 0..(frames + WARMUP_FRAMES) {
        match capture.read(&mut frame) {
            Ok(true) => {
                if k >= WARMUP_FRAMES {
                    all.extend_from_slice(&frame);
                }
            }
            Ok(false) => break,
            Err(e) => return Err(format!("录音失败: {e}")),
        }
    }
    if all.is_empty() {
        return Err("一帧都没录到".into());
    }
    Ok(energy_db(&all))
}

/// 跑一次完整的声学往返。
fn run(seconds: f64) -> Result<Acoustic, String> {
    let (render_id, render_name) = pick(Direction::Render, "GOUHUO_AEC_RENDER")?;
    let (capture_id, capture_name) = pick(Direction::Capture, "GOUHUO_AEC_CAPTURE")?;

    let total_frames = (seconds * 100.0) as usize;
    if total_frames < 200 {
        return Err("时间太短，AEC3 还没收敛就结束了".into());
    }

    // 第一步：什么都不播，先量本底。
    //
    // **必须在放音之前量**，而且要单独开一次采集流 —— 放完再量的话，
    // 房间里还有混响拖尾，量到的本底会偏高，后面的判定就松了。
    println!("  量本底噪声（1 秒，请保持安静）…");
    let noise_floor_db = listen(capture_id.clone(), 100)?;

    // 在放音**之前**就判掉，别白响 12 秒。
    if noise_floor_db < DIGITAL_SILENCE_DB {
        return Err(format!(
            concat!(
                "麦克风「{name}」自己带噪声门：安静时本底 {floor:.1} dB，
",
                "那是数字静音，真实房间不可能这么安静。
",
                "
",
                "这次测量不作数 —— 噪声门是非线性处理，而且在我们的 APM 之前，
",
                "AEC 的线性滤波器学不了它。量出来的低抑制量是这条链路的性质，
",
                "不是 AEC 的性质（详见 DIGITAL_SILENCE_DB 的注释）。
",
                "
",
                "换一个不带 DSP 的麦克风：普通 USB 麦、3.5mm 麦、摄像头麦都行，
",
                "游戏耳麦基本都带。用 GOUHUO_AEC_CAPTURE 指定。",
            ),
            name = capture_name,
            floor = noise_floor_db,
        ));
    }

    // 第二步：一边播一边录，中间夹着 APM。
    let cfg = ApmConfig {
        echo_cancel: true,
        // 只留 AEC 和高通。降噪会把回声残留一起压掉，AGC 会把残留重新拉起来 ——
        // 两个都会让量出来的数字不再是"AEC 消掉了多少"。
        noise_suppression: false,
        gain_control: false,
        high_pass: true,
        stream_delay_ms: Some(STREAM_DELAY_MS),
    };
    let apm = Arc::new(Apm::new(SAMPLE_RATE, cfg).map_err(|e| e.to_string())?);

    // 远端素材：5 秒，循环播。够长，听不出是在循环。
    let far = speech_like(500, 0x2545_F491_4F6C_DD1D, FAR_RMS);
    let far_frames = far.len() / FRAME_SAMPLES;

    let stop = Arc::new(AtomicBool::new(false));

    // 播放线程。**先喂给 AEC，再真的播出去** —— 顺序反了 AEC 就永远慢一拍。
    let render_thread = {
        let apm = Arc::clone(&apm);
        let stop = Arc::clone(&stop);
        let far = far.clone();
        std::thread::spawn(move || -> Result<(), String> {
            let mut render = WasapiRender::new(render_id);
            let mut frame = vec![0.0f32; FRAME_SAMPLES];
            let mut k = 0usize;
            while !stop.load(Ordering::Relaxed) {
                let at = (k % far_frames) * FRAME_SAMPLES;
                frame.copy_from_slice(&far[at..at + FRAME_SAMPLES]);
                apm.analyze_render(&mut frame).map_err(|e| e.to_string())?;
                render.write(&frame).map_err(|e| format!("播放失败: {e}"))?;
                k += 1;
            }
            Ok(())
        })
    };

    println!("  一边播一边录（{seconds:.0} 秒）…");
    let mut capture = WasapiCapture::new(capture_id);
    let mut frame = vec![0.0f32; FRAME_SAMPLES];
    let mut echo_in: Vec<f32> = Vec::with_capacity(total_frames * FRAME_SAMPLES);
    let mut mic_out: Vec<f32> = Vec::with_capacity(total_frames * FRAME_SAMPLES);

    let outcome = (|| -> Result<(), String> {
        for _ in 0..total_frames {
            match capture.read(&mut frame) {
                Ok(true) => {}
                Ok(false) => break,
                Err(e) => return Err(format!("录音失败: {e}")),
            }
            echo_in.extend_from_slice(&frame);
            apm.process_capture(&mut frame).map_err(|e| e.to_string())?;
            mic_out.extend_from_slice(&frame);
        }
        Ok(())
    })();

    // 无论成败都要把播放线程停掉，否则音箱会一直响。
    stop.store(true, Ordering::Relaxed);
    let render_result = render_thread
        .join()
        .map_err(|_| "播放线程炸了".to_string())?;
    outcome?;
    render_result?;

    if mic_out.is_empty() {
        return Err("一帧都没录到".into());
    }

    // 逐秒打一遍，再下结论。
    //
    // 只报一个平均值的话，分不清「AEC3 就这个水平」和「12 秒还没收敛完」——
    // 这两件事的应对完全不一样：前者是换实现，后者是把测试跑久点。
    println!("  逐秒的回声抑制：");
    let per_sec = SAMPLE_RATE as usize;
    for sec in 0..(mic_out.len() / per_sec) {
        let a = sec * per_sec;
        let b = a + per_sec;
        println!(
            "    {:>4}s  {:>5.1} dB",
            sec,
            energy_db(&echo_in[a..b]) - energy_db(&mic_out[a..b])
        );
    }

    // 只看后半段：前半段 AEC3 还在估回声路径。
    let tail = mic_out.len() / 2;
    Ok(Acoustic {
        render_name,
        capture_name,
        noise_floor_db,
        echo_in_db: energy_db(&echo_in[tail..]),
        residual_db: energy_db(&mic_out[tail..]),
    })
}

/// 跑一遍并把结果打出来。判定留给调用方。
fn report(seconds: f64) -> Result<Acoustic, String> {
    let a = run(seconds)?;
    println!("  音箱：{}", a.render_name);
    println!("  麦克风：{}", a.capture_name);
    println!("  本底噪声        {:>7.1} dB", a.noise_floor_db);
    println!(
        "  回声（APM 之前）{:>7.1} dB   比本底高 {:.1} dB",
        a.echo_in_db,
        a.echo_over_noise_db()
    );
    println!(
        "  残留（APM 之后）{:>7.1} dB   比本底高 {:.1} dB",
        a.residual_db,
        a.residual_over_noise_db()
    );
    println!("  >>> 消掉了 {:.1} dB", a.erle_db());
    Ok(a)
}

/// 这次测量到底作不作数。
///
/// 分开写是因为**"没测出来"和"测出来很差"必须报不同的话**：前者是物理场景
/// 没摆对（戴着耳机、音箱没开），后者才是 AEC 不行。混在一起报的话，
/// 第一次跑的人一定会以为是代码坏了。
fn assert_measurable(a: &Acoustic) {
    assert!(
        a.echo_over_noise_db() >= MIN_ECHO_OVER_NOISE_DB,
        "麦克风基本没听见音箱（回声只比本底高 {:.1} dB，要 {:.0} dB 才算数）。\n\
         这次测量不作数 —— 不是 AEC 不行，是场景没摆对：\n\
           - 戴着耳机？摘掉\n\
           - 音箱开着吗？音量够吗？\n\
           - 麦克风选对了吗？（现在用的是「{}」，可以用 GOUHUO_AEC_CAPTURE 换）\n\
           - Windows 或者声卡驱动自己的回声消除可能已经先把它吃掉了",
        a.echo_over_noise_db(),
        MIN_ECHO_OVER_NOISE_DB,
        a.capture_name
    );
}

/// **真音箱真麦克风下，AEC 能消掉多少。**
///
/// 这是「外放开黑不啸叫」唯一站得住的依据 —— `mod echo` 那套合成测试
/// 量的是线性回路，区分不出实现好坏 —— 那里两份实现差了 32 dB，
/// 在真声学回路下只差 6 dB。
#[test]
#[ignore = "要真音箱和真麦克风，会出声也会录音，跑十几秒"]
fn aec_cancels_real_acoustic_echo() {
    let seconds = 12.0;

    println!();
    println!("声学往返 —— 会真的出声、真的录音。");
    println!("请确认：音箱开着、耳机摘了、人别说话。");

    // 不用 expect：它按 Debug 打，多行消息会被转义成字面的换行符。
    let a = report(seconds).unwrap_or_else(|e| panic!("{e}"));
    assert_measurable(&a);

    println!();
    println!("残留一旦压到本底噪声附近，ERLE 就到头了 —— 再高的数字只说明");
    println!("回声更响，不说明 AEC 更好。看「残留比本底高多少」那一行。");

    // 阈值比合成测试松：真回路有非线性、有时钟漂移，
    // 25 dB 在合成测试里是轻松的，在真声学里是及格线。
    assert!(
        a.erle_db() > 20.0,
        "真声学回路下只消掉 {:.1} dB —— 外放开黑会啸叫",
        a.erle_db()
    );
}
