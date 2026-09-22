// SPDX-License-Identifier: GPL-3.0-or-later
//! 带宽红线的测量：说话 < 40 kbps，静音（DTX 生效）< 5 kbps。
//!
//! 这里**不跑实时**，只把编码器喂满然后数字节 —— 带宽跟调度无关，
//! 实时化只会让数字变脏。
//!
//! 两件必须算进去的事，漏掉任何一件这个数字就是自欺欺人：
//! 1. **IP/UDP 头 28 字节**。50 包/秒时它本身就是 11.2 kbps。
//! 2. **我们自己的 13 字节包头 + 16 字节 AEAD tag**。又是 11.6 kbps。
//!
//! 也就是说 20 ms 帧下，还没编码一个比特，固定开销已经 22.8 kbps。
//! 这直接决定了 Opus 码率只能给到多少，也直接决定了 10 ms 帧是不是可行 ——
//! 10 ms 帧固定开销翻倍到 45.6 kbps，说话带宽红线当场就破了。

use opus::{Application, Bitrate, Channels, Encoder, Signal};
use protocol::{IPV4_UDP_OVERHEAD, SAMPLE_RATE, VOICE_HEADER_LEN};

use voice_core::signal;

/// AEAD tag 长度。没开 crypto feature 时也要按开了算 —— 生产路径一定加密。
const TAG_LEN: usize = 16;

#[derive(Debug, Clone, Copy)]
pub struct BandwidthRow {
    pub frame_ms: u32,
    pub bitrate: i32,
    pub crypto: bool,
    /// 每秒包数。
    pub pps: f64,
    /// 每包固定开销（IP/UDP + 我们的头 + tag），字节。
    pub fixed_overhead_bytes: usize,
    /// 固定开销折算的 kbps —— 跟 Opus 码率无关的那部分。
    pub overhead_kbps: f64,
    /// 说话时的线上带宽 kbps。
    pub speaking_kbps: f64,
    /// 静音时**照发不误**的带宽 kbps（DTX 只缩小负载，不停发）。
    pub silent_keep_sending_kbps: f64,
    /// 静音时**DTX 帧直接不发**的带宽 kbps。这才是能进 5 kbps 的做法。
    pub silent_drop_dtx_kbps: f64,
    /// 静音段里被 Opus 判定为 DTX 的帧占比。
    pub dtx_hit_rate: f64,
    pub median_speaking_payload: usize,
}

pub fn measure(
    frame_ms: u32,
    bitrate: i32,
    complexity: i32,
    fec: bool,
    crypto: bool,
) -> BandwidthRow {
    let n = SAMPLE_RATE as usize * frame_ms as usize / 1000;
    let pps = 1000.0 / frame_ms as f64;
    let fixed = IPV4_UDP_OVERHEAD + VOICE_HEADER_LEN + if crypto { TAG_LEN } else { 0 };

    // 各 6 秒，丢掉前 2 秒让编码器状态稳定（DTX 判决尤其需要预热）。
    let total_frames = (6000 / frame_ms) as usize;
    let warmup = (2000 / frame_ms) as usize;
    let measured = total_frames - warmup;

    let speech = signal::chirp(total_frames * n, SAMPLE_RATE, 180.0, 3400.0, 0.4);
    let quiet = signal::silence(total_frames * n);

    let mut enc = new_encoder(bitrate, complexity, fec);
    let mut payload = vec![0u8; protocol::MAX_DATAGRAM];

    let mut speaking_bytes = 0usize;
    let mut speaking_sizes: Vec<usize> = Vec::with_capacity(measured);
    for k in 0..total_frames {
        let len = enc
            .encode(&speech[k * n..(k + 1) * n], &mut payload)
            .expect("opus encode");
        if k >= warmup {
            speaking_bytes += len + fixed;
            speaking_sizes.push(len);
        }
    }

    // 不重建编码器：真实场景就是「说着说着停了」，DTX 判决依赖这个连续性。
    let mut silent_all_bytes = 0usize;
    let mut silent_drop_bytes = 0usize;
    let mut dtx_frames = 0usize;
    for k in 0..total_frames {
        let len = enc
            .encode(&quiet[k * n..(k + 1) * n], &mut payload)
            .expect("opus encode");
        // libopus 在 DTX 生效时返回 1 到 2 字节的「什么都没有」帧。
        // 真发送端看到这个就该**根本不发**，只在话尾补一个 terminator 包。
        let is_dtx = len <= 2;
        if k >= warmup {
            silent_all_bytes += len + fixed;
            if is_dtx {
                dtx_frames += 1;
            } else {
                silent_drop_bytes += len + fixed;
            }
        }
    }

    speaking_sizes.sort_unstable();
    let window_s = measured as f64 * frame_ms as f64 / 1000.0;
    let kbps = |bytes: usize| bytes as f64 * 8.0 / window_s / 1000.0;

    BandwidthRow {
        frame_ms,
        bitrate,
        crypto,
        pps,
        fixed_overhead_bytes: fixed,
        overhead_kbps: fixed as f64 * 8.0 * pps / 1000.0,
        speaking_kbps: kbps(speaking_bytes),
        silent_keep_sending_kbps: kbps(silent_all_bytes),
        silent_drop_dtx_kbps: kbps(silent_drop_bytes),
        dtx_hit_rate: dtx_frames as f64 / measured as f64,
        median_speaking_payload: speaking_sizes
            .get(speaking_sizes.len() / 2)
            .copied()
            .unwrap_or(0),
    }
}

fn new_encoder(bitrate: i32, complexity: i32, fec: bool) -> Encoder {
    let mut enc =
        Encoder::new(SAMPLE_RATE, Channels::Mono, Application::Voip).expect("opus encoder");
    enc.set_bitrate(Bitrate::Bits(bitrate)).expect("bitrate");
    enc.set_complexity(complexity).expect("complexity");
    enc.set_signal(Signal::Voice).expect("signal");
    enc.set_inband_fec(fec).expect("fec");
    enc.set_packet_loss_perc(if fec { 10 } else { 0 })
        .expect("loss perc");
    enc.set_dtx(true).expect("dtx");
    enc
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dtx_actually_engages_on_silence() {
        let row = measure(20, 24_000, 5, true, true);
        assert!(
            row.dtx_hit_rate > 0.8,
            "DTX barely engaged: {}",
            row.dtx_hit_rate
        );
        // 不发 DTX 帧的话，静音带宽应该趋近于 0。
        assert!(
            row.silent_drop_dtx_kbps < 5.0,
            "{} kbps",
            row.silent_drop_dtx_kbps
        );
        // 照发不误的话，光是包头就把 5 kbps 顶破了 —— 这正是要证明的点。
        assert!(
            row.silent_keep_sending_kbps > 5.0,
            "{} kbps",
            row.silent_keep_sending_kbps
        );
    }

    #[test]
    fn overhead_math_matches_the_readme_claim() {
        let row = measure(20, 24_000, 5, true, true);
        assert_eq!(row.fixed_overhead_bytes, 28 + 13 + 16);
        assert!(
            (row.overhead_kbps - 22.8).abs() < 0.1,
            "{} kbps",
            row.overhead_kbps
        );
    }

    #[test]
    fn ten_ms_frames_double_the_overhead() {
        let a = measure(20, 24_000, 5, true, true);
        let b = measure(10, 24_000, 5, true, true);
        assert!((b.overhead_kbps / a.overhead_kbps - 2.0).abs() < 0.01);
    }
}
