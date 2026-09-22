// SPDX-License-Identifier: GPL-3.0-or-later

//! 端到端语音：一个人说话，另一个人听见，**并且量出来花了多久**。
//!
//! 走的是完整链路 —— 真 TLS 握手派生的密钥、真 Opus、真加密、真 UDP、
//! 真服务端转发、真抖动缓冲。只有两头的声卡是合成的。
//!
//! # 这是第一次真正量端到端
//!
//! M1 量的是协议链路（46.5 ms），M2 量的是设备（30.4 ms）和 APM（15.0 ms），
//! 加起来 91.9 ms。但那三段**从没作为一条链路跑过** —— 加法能对，接线可能错：
//! 少一次拷贝、多一层缓冲、线程调度慢了一拍，加法都看不出来。
//!
//! 这里喂一个啁啾进去，从另一头把它捞出来做互相关，得到的是**实测**的延迟。
//!
//! # 这个数字不包括什么
//!
//! **不含声卡。** 合成设备一到节拍就把整帧交出去，真麦克风要先攒满 10 ms
//! 才有一帧，真扬声器那边还有驱动和硬件的缓冲 —— 这部分 M2 单独量过 30.4 ms。
//!
//! **不含 APM。** 那一段 M2 量过 15.0 ms。
//!
//! **网络是本机回环。** 真网络的往返要另外加。
//!
//! 所以这里量到的是「**我们自己写的那一段**」：分帧、编码器前瞻、加密、
//! 转发、抖动缓冲、解码、混音。它也正是唯一由我们的代码决定的部分。

use std::net::{TcpListener, UdpSocket};
use std::sync::Arc;
use std::time::{Duration, Instant};

use client_core::Client;
use protocol::Invite;
use server::conn::Hub;
use server::state::{Config, Server};
use transport::{server_config, ServerCert};
use voice_core::audio::{
    Capture, CollectingRender, NullRender, Render, SyntheticCapture, FRAME_SAMPLES, SAMPLE_RATE,
};
use voice_core::identity::Identity;
use voice_core::pipeline::{Pipeline, PipelineConfig, TransmitMode, DEFAULT_JITTER_FRAMES};

/// 啁啾前面留多少帧静音。
///
/// 给链路时间把自己撑起来：抖动缓冲要预填，Opus 编码器前几帧在爬坡，
/// 服务端还要先从保活包里学到我们的 UDP 地址。
const LEAD_FRAMES: usize = 60;

/// 啁啾本身多长。
const CHIRP_FRAMES: usize = 40;

/// 啁啾之后再跑多久，好让它整个被推出来。
const TAIL_FRAMES: usize = 60;

struct TestServer {
    invite: Invite,
    #[allow(dead_code)]
    hub: Arc<Hub>,
}

fn start_server() -> TestServer {
    let cert = ServerCert::generate().unwrap();
    let fingerprint = cert.fingerprint();
    let tls = Arc::new(server_config(&cert).unwrap());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let voice = UdpSocket::bind("127.0.0.1:0").unwrap();

    let hub = Arc::new(Hub::new(Server::new(Config::default()), voice));
    let voice_hub = Arc::clone(&hub);
    std::thread::spawn(move || voice_hub.run_voice());
    let accept_hub = Arc::clone(&hub);
    std::thread::spawn(move || server::accept_loop(listener, tls, accept_hub));

    TestServer {
        invite: Invite {
            host: addr.ip().to_string(),
            port: addr.port(),
            cert: fingerprint,
            code: None,
        },
        hub,
    }
}

fn join(server: &TestServer, name: &str) -> Client {
    let link = server.invite.to_url().unwrap();
    Client::connect(&link, &Identity::generate().unwrap(), name)
        .expect("连不上")
        .0
}

fn voice_config(client: &Client, server: &TestServer, mode: TransmitMode) -> PipelineConfig {
    let addr = format!("{}:{}", server.invite.host, client.udp_port())
        .parse()
        .expect("服务端给的语音地址解析不了");
    PipelineConfig {
        session_id: client.session_id(),
        server: addr,
        upstream_key: *client.voice_keys().upstream.as_bytes(),
        downstream_key: *client.voice_keys().downstream.as_bytes(),
        jitter_frames: DEFAULT_JITTER_FRAMES,
        mode,
    }
}

/// 啁啾，f32。
fn chirp_f32(samples: usize) -> Vec<f32> {
    voice_core::signal::chirp(samples, SAMPLE_RATE, 300.0, 3400.0, 0.5)
        .into_iter()
        .map(|s| s as f32 / i16::MAX as f32)
        .collect()
}

fn to_i16(samples: &[f32]) -> Vec<i16> {
    samples
        .iter()
        .map(|&s| (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16)
        .collect()
}

/// 甲说话，乙听见，而且能量对得上。
///
/// 这是「跑通了」的定义。延迟是多少是下一条测试的事。
#[test]
fn a_chirp_makes_it_all_the_way_through() {
    let server = start_server();
    let alice = join(&server, "阿狸");
    let bob = join(&server, "波波");

    let mut source = vec![0.0f32; FRAME_SAMPLES * LEAD_FRAMES];
    source.extend_from_slice(&chirp_f32(FRAME_SAMPLES * CHIRP_FRAMES));
    source.extend(std::iter::repeat(0.0).take(FRAME_SAMPLES * TAIL_FRAMES));

    let capture = SyntheticCapture::new(source.clone()).then_silence();
    let alice_voice = Pipeline::start(
        voice_config(&alice, &server, TransmitMode::Always),
        Box::new(capture),
        Box::new(NullRender::default()),
        None,
    )
    .unwrap();

    let (render, played) = CollectingRender::new();
    // 波波只听不说。他必须照样能听见 —— 服务端是从保活包里学到他的地址的。
    let silent = SyntheticCapture::new(Vec::new()).then_silence();
    let bob_voice = Pipeline::start(
        voice_config(&bob, &server, TransmitMode::PushToTalk),
        Box::new(silent),
        Box::new(render),
        None,
    )
    .unwrap();

    let total_frames = LEAD_FRAMES + CHIRP_FRAMES + TAIL_FRAMES;
    std::thread::sleep(Duration::from_millis((total_frames * 10 + 500) as u64));

    let stats = bob_voice.stats();
    assert!(stats.udp_ok, "波波那边 UDP 没通 —— 保活没回来");
    assert!(stats.packets_received > 100, "波波几乎没收到包：{stats:?}");
    assert!(
        alice_voice.stats().packets_sent > 100,
        "阿狸几乎没发出去包：{:?}",
        alice_voice.stats()
    );

    let played = played.lock().unwrap().clone();
    let energy: f32 = played.iter().map(|s| s * s).sum();
    assert!(energy > 0.0, "波波那边一点声音都没有");

    // 啁啾的能量应该集中在中段，而不是均匀铺满 —— 后者说明我们捞到的是噪声
    let third = played.len() / 3;
    let mid: f32 = played[third..2 * third].iter().map(|s| s * s).sum();
    assert!(
        mid > energy * 0.5,
        "能量没集中在中段，捞到的可能不是那个啁啾"
    );
}

/// **量端到端延迟。** 见模块文档：这个数字不含声卡和 APM。
#[test]
fn end_to_end_latency_is_measured_not_added_up() {
    let server = start_server();
    let alice = join(&server, "阿狸");
    let bob = join(&server, "波波");

    let mut source = vec![0.0f32; FRAME_SAMPLES * LEAD_FRAMES];
    source.extend_from_slice(&chirp_f32(FRAME_SAMPLES * CHIRP_FRAMES));
    source.extend(std::iter::repeat(0.0).take(FRAME_SAMPLES * TAIL_FRAMES));

    let capture = SyntheticCapture::new(source.clone()).then_silence();
    let capture_t0 = capture.first_frame_at();
    let _alice_voice = Pipeline::start(
        voice_config(&alice, &server, TransmitMode::Always),
        Box::new(capture),
        Box::new(NullRender::default()),
        None,
    )
    .unwrap();

    let (render, played) = CollectingRender::new();
    let render_t0 = render.first_frame_at();
    let silent = SyntheticCapture::new(Vec::new()).then_silence();
    let _bob_voice = Pipeline::start(
        voice_config(&bob, &server, TransmitMode::PushToTalk),
        Box::new(silent),
        Box::new(render),
        None,
    )
    .unwrap();

    let total_frames = LEAD_FRAMES + CHIRP_FRAMES + TAIL_FRAMES;
    std::thread::sleep(Duration::from_millis((total_frames * 10 + 500) as u64));

    let played = played.lock().unwrap().clone();
    let cap_t0 = capture_t0.lock().unwrap().expect("采集没起来");
    let play_t0 = render_t0.lock().unwrap().expect("播放没起来");

    // ---- 方法一：互相关 ----
    //
    // 拿啁啾中段当参考（避开起播和收尾），在播出来的采样点里找它。
    let reference = to_i16(&source);
    let observed = to_i16(&played);
    let ref_start = FRAME_SAMPLES * (LEAD_FRAMES + CHIRP_FRAMES / 4);
    let ref_len = FRAME_SAMPLES * (CHIRP_FRAMES / 2);
    // 最多找 300 ms —— 比任何合理的延迟都宽，找不到就是真的没到
    let max_lag = (SAMPLE_RATE as usize * 300) / 1000;

    let estimate = voice_core::signal::best_lag(&reference, &observed, ref_start, ref_len, max_lag)
        .expect("互相关没跑起来：播出来的采样点不够长");
    assert!(
        estimate.peak > 0.3,
        "相关峰值只有 {:.2}，捞到的多半不是那个啁啾",
        estimate.peak
    );
    assert!(!estimate.at_boundary, "峰值落在搜索范围边界上，结果不可信");

    // 参考信号第 ref_start 个采样点是 cap_t0 + ref_start/fs 那一刻进链路的。
    // 它在播出来的序列里落在第 ref_start + lag 个采样点，也就是
    // play_t0 + (ref_start + lag)/fs 那一刻出耳朵。
    let fs = SAMPLE_RATE as f64;
    let in_at = cap_t0 + Duration::from_secs_f64(ref_start as f64 / fs);
    let out_at = play_t0 + Duration::from_secs_f64((ref_start + estimate.lag) as f64 / fs);
    let measured_ms = out_at.saturating_duration_since(in_at).as_secs_f64() * 1000.0;

    // ---- 方法二：理论下限 ----
    //
    // 分帧 10 ms + 编码器前瞻 + 抖动缓冲。实测必须**大于等于**它 ——
    // 小于就说明有一段根本没生效（比如抖动缓冲被绕过去了）。
    let floor = voice_core::pipeline::intrinsic_latency_ms(DEFAULT_JITTER_FRAMES);

    println!("端到端（不含声卡和 APM）：{measured_ms:.1} ms");
    println!("理论下限：{floor:.1} ms");
    println!("相关峰值：{:.3}", estimate.peak);

    assert!(
        measured_ms >= floor - 2.0,
        "实测 {measured_ms:.1} ms 比理论下限 {floor:.1} ms 还小 —— \
         多半是某一段没真的生效"
    );
    // 上限放得很宽：这是「跑通」的第一版，固定抖动缓冲，而且测试机器上
    // 还跑着服务端和两条链路。收紧要等自适应缓冲那一步。
    assert!(
        measured_ms < 150.0,
        "实测 {measured_ms:.1} ms，比理论下限 {floor:.1} ms 大太多了"
    );
}

/// 松开说话键之后就不该再发包了。
///
/// 「常驻几小时不打扰人」里，静默带宽是实打实的一条 —— 一屋子人里
/// 绝大多数时间没人说话。
#[test]
fn silence_costs_almost_nothing() {
    let server = start_server();
    let alice = join(&server, "阿狸");

    // 有信号，但按键没按下 —— PTT 模式下一个语音包都不该发
    let source = chirp_f32(FRAME_SAMPLES * 100);
    let capture = SyntheticCapture::new(source).then_silence();
    let voice = Pipeline::start(
        voice_config(&alice, &server, TransmitMode::PushToTalk),
        Box::new(capture),
        Box::new(NullRender::default()),
        None,
    )
    .unwrap();

    std::thread::sleep(Duration::from_millis(1200));
    let quiet = voice.stats();
    assert_eq!(quiet.packets_sent, 0, "没按键却在发语音：{quiet:?}");
    assert!(quiet.udp_ok, "保活没通 —— 那服务端根本不知道往哪儿发给我们");

    // 按下去就该有了
    voice.set_transmitting(true);
    std::thread::sleep(Duration::from_millis(500));
    let talking = voice.stats();
    assert!(talking.packets_sent > 20, "按了键却没发包：{talking:?}");
}

/// VAD：有人声就发，静下来就停。
#[test]
fn voice_activity_follows_the_signal() {
    let server = start_server();
    let alice = join(&server, "阿狸");

    // 前 50 帧静音，中间 50 帧有声音，后面又静音
    let mut source = vec![0.0f32; FRAME_SAMPLES * 50];
    source.extend_from_slice(&chirp_f32(FRAME_SAMPLES * 50));
    source.extend(std::iter::repeat(0.0).take(FRAME_SAMPLES * 50));

    let capture = SyntheticCapture::new(source).then_silence();
    let voice = Pipeline::start(
        voice_config(
            &alice,
            &server,
            TransmitMode::VoiceActivity {
                threshold_db: -45.0,
            },
        ),
        Box::new(capture),
        Box::new(NullRender::default()),
        None,
    )
    .unwrap();

    // 静音段
    std::thread::sleep(Duration::from_millis(450));
    let during_silence = voice.stats().packets_sent;

    // 有声段
    std::thread::sleep(Duration::from_millis(550));
    let during_speech = voice.stats().packets_sent;

    // 又静下来
    std::thread::sleep(Duration::from_millis(600));
    let after = voice.stats().packets_sent;

    assert!(
        during_silence < 5,
        "静音时不该发包，却发了 {during_silence} 个"
    );
    assert!(
        during_speech > during_silence + 20,
        "有声音时该开始发包：{during_silence} -> {during_speech}"
    );
    assert!(
        after < during_speech + 15,
        "静下来之后该停：{during_speech} -> {after}"
    );
}

/// 丢掉 Pipeline 就该把线程收干净，而且**不能挂住**。
///
/// 接收线程阻塞在 recv 上，光置个停止标志叫不醒它 —— 这条测试盯的就是
/// 那个叫醒机制真的有效。
#[test]
fn dropping_the_pipeline_stops_cleanly() {
    let server = start_server();
    let alice = join(&server, "阿狸");

    let capture = SyntheticCapture::new(vec![0.0; FRAME_SAMPLES * 10]).then_silence();
    let voice = Pipeline::start(
        voice_config(&alice, &server, TransmitMode::Always),
        Box::new(capture),
        Box::new(NullRender::default()),
        None,
    )
    .unwrap();
    std::thread::sleep(Duration::from_millis(300));

    let start = Instant::now();
    drop(voice);
    let took = start.elapsed();
    assert!(
        took < Duration::from_secs(3),
        "关链路花了 {took:?} —— 有线程没被叫醒"
    );
}

/// 合成设备要满足 trait 的约定：阻塞到下一帧。
/// 不满足的话上面所有延迟数字都不作数。
#[test]
fn synthetic_devices_pace_themselves() {
    let mut capture = SyntheticCapture::new(vec![0.0; FRAME_SAMPLES * 10]);
    let mut frame = vec![0.0; FRAME_SAMPLES];
    let start = Instant::now();
    while capture.read(&mut frame).unwrap() {}
    assert!(start.elapsed() > Duration::from_millis(80));

    let (mut render, _) = CollectingRender::new();
    let start = Instant::now();
    for _ in 0..10 {
        render.write(&frame).unwrap();
    }
    assert!(start.elapsed() > Duration::from_millis(80));
}

/// 拿**真声卡**跑一遍。默认跳过 —— CI 上没有音频设备。
///
/// ```bash
/// cargo test -p client-core --test voice_pipeline -- --ignored --nocapture
/// ```
///
/// 它只验「设备打得开、UDP 通得了」，不验听感：频道里只有一个人，
/// 播出去的全是静音，所以不会有啸叫。
#[test]
#[ignore = "要真声卡"]
#[cfg(windows)]
fn real_devices_open_and_udp_comes_up() {
    let server = start_server();
    let alice = join(&server, "阿狸");

    let capture = voice_core::wasapi::WasapiCapture::new(None);
    let diagnostics = capture.diagnostics();
    let voice = Pipeline::start(
        voice_config(&alice, &server, TransmitMode::Always),
        Box::new(capture),
        Box::new(voice_core::wasapi::WasapiRender::new(None)),
        None,
    )
    .expect("起不了语音链路");

    // 保活每 2 秒一次，给它两轮。顺便报一下麦克风电平 ——
    // 「电平条不动」到底是麦克风没收到音还是我们算错了，就看这个。
    voice.set_monitoring(false);
    let mut peak = f32::NEG_INFINITY;
    for _ in 0..50 {
        std::thread::sleep(Duration::from_millis(100));
        peak = peak.max(voice.stats().input_db);
    }
    let stats = voice.stats();
    println!("真设备：{stats:?}");
    println!("这 5 秒里麦克风的峰值电平：{peak:.1} dB（默认语音激活阈值是 -45 dB）");
    println!("采集设备：{}", diagnostics.device_name());
    println!(
        "设备标成静音的采样点占比：{:.1}%（接近 100% 就是设备那边真的没收到音）",
        diagnostics.silent_ratio() * 100.0
    );

    assert!(stats.udp_ok, "UDP 没通 —— 保活没回来：{stats:?}");
    assert!(
        stats.packets_sent > 100,
        "麦克风没出数据（5 秒该有约 500 帧）：{stats:?}"
    );
    assert_eq!(
        stats.underruns, 0,
        "播放欠载了 —— 设备节拍跟不上：{stats:?}"
    );
}

/// 试听：把自己的麦克风混进播放，**但不发给任何人**。
///
/// 这是「找人试」之前唯一能自己回答的问题：麦克风到底有没有在收音。
#[test]
fn monitoring_plays_your_own_mic_without_sending_it() {
    let server = start_server();
    let alice = join(&server, "阿狸");

    let mut source = vec![0.0f32; FRAME_SAMPLES * 20];
    source.extend_from_slice(&chirp_f32(FRAME_SAMPLES * 60));
    source.extend(std::iter::repeat(0.0).take(FRAME_SAMPLES * 20));

    let capture = SyntheticCapture::new(source.clone()).then_silence();
    let (render, played) = CollectingRender::new();
    // 按住说话，但**不按** —— 所以一个语音包都不该发出去
    let voice = Pipeline::start(
        voice_config(&alice, &server, TransmitMode::PushToTalk),
        Box::new(capture),
        Box::new(render),
        None,
    )
    .unwrap();
    voice.set_monitoring(true);

    std::thread::sleep(Duration::from_millis(1200));

    let stats = voice.stats();
    assert_eq!(
        stats.packets_sent, 0,
        "试听把声音发出去了 —— 别人会听到你在试麦：{stats:?}"
    );

    let played = played.lock().unwrap().clone();
    let energy: f32 = played.iter().map(|s| s * s).sum();
    assert!(energy > 0.0, "开了试听却一点声音都没有");

    // 真的是麦克风那个信号，不是别的什么
    let estimate = voice_core::signal::best_lag(
        &to_i16(&source),
        &to_i16(&played),
        FRAME_SAMPLES * 30,
        FRAME_SAMPLES * 20,
        (SAMPLE_RATE as usize * 200) / 1000,
    )
    .expect("互相关跑不起来");
    assert!(
        estimate.peak > 0.5,
        "播出来的跟麦克风进去的对不上，相关峰值只有 {:.2}",
        estimate.peak
    );
}

/// 关掉试听就该彻底安静 —— 而且再打开时不能先播一段旧的。
#[test]
fn monitoring_can_be_turned_off_cleanly() {
    let server = start_server();
    let alice = join(&server, "阿狸");

    let capture = SyntheticCapture::new(chirp_f32(FRAME_SAMPLES * 200)).then_silence();
    let (render, played) = CollectingRender::new();
    let voice = Pipeline::start(
        voice_config(&alice, &server, TransmitMode::PushToTalk),
        Box::new(capture),
        Box::new(render),
        None,
    )
    .unwrap();

    // 一开始没开，应该是静音
    std::thread::sleep(Duration::from_millis(400));
    let quiet: f32 = played.lock().unwrap().iter().map(|s| s * s).sum();
    assert_eq!(quiet, 0.0, "没开试听却有声音");

    voice.set_monitoring(true);
    std::thread::sleep(Duration::from_millis(400));
    let loud: f32 = played.lock().unwrap().iter().map(|s| s * s).sum();
    assert!(loud > 0.0, "开了试听还是没声音");

    voice.set_monitoring(false);
    std::thread::sleep(Duration::from_millis(100));
    let before = played.lock().unwrap().len();
    std::thread::sleep(Duration::from_millis(400));
    let after: f32 = played.lock().unwrap()[before..].iter().map(|s| s * s).sum();
    assert_eq!(after, 0.0, "关了试听还在播");
}

/// 电平表要跟着麦克风动。
///
/// 它回答的是用户最常问的那个问题：「我说话了，为什么语音激活没触发？」——
/// 看一眼电平条就知道是麦克风没收到音，还是收到了但没过阈值。
#[test]
fn the_input_level_follows_the_microphone() {
    let server = start_server();
    let alice = join(&server, "阿狸");

    // 前面静音，后面有声音
    let mut source = vec![0.0f32; FRAME_SAMPLES * 60];
    source.extend_from_slice(&chirp_f32(FRAME_SAMPLES * 100));

    let capture = SyntheticCapture::new(source).then_silence();
    let voice = Pipeline::start(
        voice_config(&alice, &server, TransmitMode::PushToTalk),
        Box::new(capture),
        Box::new(NullRender::default()),
        None,
    )
    .unwrap();

    std::thread::sleep(Duration::from_millis(400));
    let silent = voice.stats().input_db;
    std::thread::sleep(Duration::from_millis(600));
    let speaking = voice.stats().input_db;

    assert!(silent < -90.0, "静音时电平该贴底，实际 {silent:.1} dB");
    assert!(
        speaking > silent + 40.0,
        "有声音时电平该明显抬起来：{silent:.1} -> {speaking:.1} dB"
    );
    // 默认 VAD 阈值是 -45 dB，正常说话必须能过
    assert!(
        speaking > -45.0,
        "这个信号连默认阈值都过不去，电平算错了：{speaking:.1} dB"
    );
}
