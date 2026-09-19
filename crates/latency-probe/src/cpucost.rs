//! 编解码的 CPU 成本 —— 直接对着 3% 单核的红线。
//!
//! 跟带宽一样不跑实时：编解码耗时跟调度无关，实时化只会让数字更脏。
//!
//! 一条必须记住的账：**编码只做一次，解码要按说话的人数做 N 次。**
//! 3 到 20 人的频道里同时说话的通常是 1 到 3 个，所以真实开销大致是
//! `encode + N × decode`。这张表给的是 N=1 的单位成本，自己乘。
//!
//! 还有一笔更大的账没在这里：APM（AEC3 + 降噪 + AGC）。AEC3 是这条链路上
//! 最贵的一块，M2 接上 webrtc-audio-processing 之后才知道还剩多少余量。

use opus::{Application, Bitrate, Channels, Decoder, Encoder, Signal};
use protocol::SAMPLE_RATE;
use voice_core::metrics::Histogram;

use crate::signal;

#[derive(Debug, Clone, Copy)]
pub struct CpuCostRow {
    pub frame_ms: u32,
    pub complexity: i32,
    pub encode_us_p50: f64,
    pub encode_us_p95: f64,
    pub decode_us_p50: f64,
    /// 一路编码 + 一路解码占单核的比例 %。
    pub cpu_pct_1_stream: f64,
    /// 一路编码 + 三路解码（频道里三个人同时说）占单核的比例 %。
    pub cpu_pct_3_streams: f64,
}

pub fn measure(frame_ms: u32, bitrate: i32, complexity: i32, fec: bool) -> CpuCostRow {
    let n = SAMPLE_RATE as usize * frame_ms as usize / 1000;
    let total_frames = (4000 / frame_ms) as usize;
    let warmup = (500 / frame_ms) as usize;

    let pcm = signal::chirp(total_frames * n, SAMPLE_RATE, 180.0, 3400.0, 0.4);

    let mut enc = Encoder::new(SAMPLE_RATE, Channels::Mono, Application::Voip).expect("encoder");
    enc.set_bitrate(Bitrate::Bits(bitrate)).expect("bitrate");
    enc.set_complexity(complexity).expect("complexity");
    enc.set_signal(Signal::Voice).expect("signal");
    enc.set_inband_fec(fec).expect("fec");
    enc.set_packet_loss_perc(if fec { 10 } else { 0 })
        .expect("loss perc");
    // 这里关掉 DTX：要量的是编码器真干活时有多贵，
    // 开着 DTX 会混进一堆几乎免费的静音帧，把均值拉低成一个骗人的数字。
    enc.set_dtx(false).expect("dtx");

    let mut dec = Decoder::new(SAMPLE_RATE, Channels::Mono).expect("decoder");
    let mut scratch = vec![0i16; n];
    let mut payload = vec![0u8; protocol::MAX_DATAGRAM];

    let mut enc_us = Histogram::with_capacity(total_frames);
    let mut dec_us = Histogram::with_capacity(total_frames);

    for k in 0..total_frames {
        let t = std::time::Instant::now();
        let len = enc
            .encode(&pcm[k * n..(k + 1) * n], &mut payload)
            .expect("encode");
        let e = t.elapsed().as_secs_f64() * 1e6;

        let t = std::time::Instant::now();
        dec.decode(&payload[..len], &mut scratch, false)
            .expect("decode");
        let d = t.elapsed().as_secs_f64() * 1e6;

        if k >= warmup {
            enc_us.push(e);
            dec_us.push(d);
        }
    }

    let e50 = enc_us.pct(0.50);
    let e95 = enc_us.pct(0.95);
    let d50 = dec_us.pct(0.50);
    let budget_us = frame_ms as f64 * 1000.0;

    CpuCostRow {
        frame_ms,
        complexity,
        encode_us_p50: e50,
        encode_us_p95: e95,
        decode_us_p50: d50,
        cpu_pct_1_stream: (e50 + d50) / budget_us * 100.0,
        cpu_pct_3_streams: (e50 + 3.0 * d50) / budget_us * 100.0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complexity_costs_cpu_monotonically() {
        let lo = measure(20, 24_000, 0, true);
        let hi = measure(20, 24_000, 10, true);
        assert!(
            hi.encode_us_p50 > lo.encode_us_p50 * 1.5,
            "complexity 10 ({:.0} µs) 应该显著贵于 0 ({:.0} µs)",
            hi.encode_us_p50,
            lo.encode_us_p50
        );
    }

    #[test]
    fn decode_is_much_cheaper_than_encode() {
        let r = measure(20, 24_000, 5, true);
        assert!(r.decode_us_p50 < r.encode_us_p50, "解码不该比编码贵");
    }

    /// 反直觉的那条：**10 ms 帧的 CPU 比 20 ms 更省**。
    ///
    /// 直觉会说「10 ms 帧要跑两倍次数的编码器，肯定更贵」，实测反过来 ——
    /// Opus 单帧的编码成本随帧长是超线性的（20 ms 帧 502 µs vs 10 ms 帧 190 µs，
    /// 2.6 倍，不是 2 倍），所以按秒摊下来短帧反而便宜。
    ///
    /// 这条很重要：帧长的取舍是「延迟 + CPU」对「带宽」，不是三者对一者。
    #[test]
    fn shorter_frames_are_not_more_expensive_per_second() {
        let long = measure(20, 24_000, 5, true);
        let short = measure(10, 24_000, 5, true);
        assert!(
            short.cpu_pct_1_stream <= long.cpu_pct_1_stream * 1.10,
            "10 ms {:.2}% 不该明显贵于 20 ms {:.2}%",
            short.cpu_pct_1_stream,
            long.cpu_pct_1_stream
        );
        // 单帧成本超线性：20 ms 一帧比 10 ms 一帧贵，但不止贵一倍。
        assert!(long.encode_us_p50 > short.encode_us_p50 * 2.0);
    }
}
