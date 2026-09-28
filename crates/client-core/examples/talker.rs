// SPDX-License-Identifier: MPL-2.0

//! 一个不停说话的机器人：连上服务器，一直往频道里发一段合成人声。
//!
//! 不要麦克风、不要声卡。给测量用 —— 量客户端「频道里有人在说话」时的内存和 CPU，
//! 得真有一路声音在解码、混音、过抖动缓冲，不然量到的是一个闲着的客户端。
//!
//! ```text
//! cargo run --release -p client-core --example talker -- gouhuo://j/... [--seconds N] [--name 名字]
//! ```
//!
//! 不给 `--seconds` 就一直说到 Ctrl-C。

use std::io;
use std::time::{Duration, Instant};

use client_core::Client;
use voice_core::audio::{Capture, NullRender, FRAME_MS, FRAME_SAMPLES, SAMPLE_RATE};
use voice_core::clock::Ticker;
use voice_core::identity::Identity;
use voice_core::pipeline::{default_jitter, Pipeline, PipelineConfig, TransmitMode};

/// 循环放一秒钟的合成「浊音」：150 Hz 基频加几个谐波，带一点起伏，
/// 让编码器不至于把它当成纯音压到几乎没有码率。按设备一样的节拍出帧。
struct LoopingVoice {
    clip: Vec<f32>,
    cursor: usize,
    ticker: Ticker,
}

impl LoopingVoice {
    fn new() -> Self {
        let rate = SAMPLE_RATE as f32;
        let clip = (0..SAMPLE_RATE as usize)
            .map(|i| {
                let t = i as f32 / rate;
                // 每秒起伏两次，像一个人在一句一句地说
                let envelope = 0.55 + 0.45 * (2.0 * std::f32::consts::PI * 2.0 * t).sin();
                let voiced: f32 = (1..=6)
                    .map(|h| (2.0 * std::f32::consts::PI * 150.0 * h as f32 * t).sin() / h as f32)
                    .sum();
                voiced * envelope * 0.15
            })
            .collect();
        Self {
            clip,
            cursor: 0,
            ticker: Ticker::start(Duration::from_millis(FRAME_MS as u64)).0,
        }
    }
}

impl Capture for LoopingVoice {
    fn read(&mut self, out: &mut [f32]) -> io::Result<bool> {
        self.ticker.tick();
        for sample in out.iter_mut() {
            *sample = self.clip[self.cursor];
            self.cursor = (self.cursor + 1) % self.clip.len();
        }
        Ok(true)
    }
}

fn main() {
    let mut args = std::env::args().skip(1);
    let Some(link) = args.next() else {
        eprintln!("用法：talker <邀请链接> [--seconds N] [--name 名字]");
        std::process::exit(2);
    };
    let mut seconds: Option<f64> = None;
    let mut name = "说话机器人".to_string();
    while let Some(arg) = args.next() {
        match arg.as_str() {
            "--seconds" => seconds = args.next().and_then(|s| s.parse().ok()),
            "--name" => name = args.next().unwrap_or(name),
            other => {
                eprintln!("不认识的参数：{other}");
                std::process::exit(2);
            }
        }
    }

    let identity = Identity::generate().expect("生成身份失败");
    let (client, _events) = match Client::connect(&link, &identity, &name) {
        Ok(ok) => ok,
        Err(e) => {
            eprintln!("连不上：{e}");
            std::process::exit(1);
        }
    };
    let Some(server) = format!("{}:{}", client.server_host(), client.udp_port())
        .parse()
        .ok()
    else {
        eprintln!(
            "服务器地址解析不了：{}:{}",
            client.server_host(),
            client.udp_port()
        );
        std::process::exit(1);
    };
    let keys = client.voice_keys();
    let _voice = Pipeline::start(
        PipelineConfig {
            session_id: client.session_id(),
            server,
            upstream_key: *keys.upstream.as_bytes(),
            downstream_key: *keys.downstream.as_bytes(),
            jitter: default_jitter(),
            mode: TransmitMode::Always,
        },
        Box::new(LoopingVoice::new()),
        Box::new(NullRender::default()),
        None,
    )
    .expect("语音链路起不来");

    println!(
        "「{name}」在说话（会话 {}，每帧 {FRAME_SAMPLES} 个采样）",
        client.session_id()
    );
    // 测量脚本靠这一行把它的音量调成 0：客户端照样解码、混音，只是不从音箱里出声。
    println!(
        "公钥 {}",
        protocol::base32::encode(&identity.public_key().0)
    );
    let started = Instant::now();
    loop {
        std::thread::sleep(Duration::from_millis(500));
        if seconds.is_some_and(|s| started.elapsed().as_secs_f64() >= s) {
            break;
        }
    }
    client.disconnect();
}
