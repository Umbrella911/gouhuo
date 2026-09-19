//! 抖动缓冲。
//!
//! **M1 里这是故意写得很朴素的固定深度缓冲** —— 它的作用是给出延迟的物理下限
//! 和一个诚实的基线。自适应抖动缓冲 + PLC 是这个项目技术含量最高的地方，
//! 也是低延迟卖点的主战场，那个在 M4 做，做完拿这里的数字对比。
//!
//! 朴素版的规则，全部写死、可预测：
//! - 攒够 `target_frames` 帧才开始播（预缓冲）。
//! - 播放中每拍要 `next_seq`：
//!   - 在 -> 出帧；
//!   - 不在但后面的帧已经到了 -> 判丢包，走 PLC，序号照样 +1；
//!   - 缓冲空了 -> 欠载，走 PLC，并**退回预缓冲**重新攒。
//! - 播放中迟到的包（seq < next_seq）直接丢。
//!
//! 最后一条是固定缓冲的本质代价：每欠载一次就重新攒 `target_frames`，
//! 延迟阶梯式上涨且再也不下来。自适应版要解决的就是这个。

use std::collections::BTreeMap;

#[derive(Debug, Clone)]
pub enum Playout {
    /// 预缓冲中，输出静音。不计入延迟统计。
    Prebuffering,
    /// 正常出帧。
    Frame { seq: u32, payload: Vec<u8> },
    /// 序号缺失但后续帧已到 —— 真丢包，走 PLC。
    Lost { seq: u32 },
    /// 缓冲被抽空 —— 欠载，走 PLC 并重新预缓冲。
    Underrun,
}

#[derive(Debug, Clone, Copy)]
pub struct JitterConfig {
    /// 起播深度（帧）。固定缓冲的延迟就是 `target_frames * 帧长`，一分不少。
    pub target_frames: usize,
    /// 缓冲上限，防止对端猛灌把内存吃了。超了丢最旧的。
    pub max_frames: usize,
}

impl JitterConfig {
    pub fn fixed(target_frames: usize) -> Self {
        let target = target_frames.max(1);
        Self {
            target_frames: target,
            max_frames: target * 8 + 8,
        }
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct JitterStats {
    pub pushed: u64,
    /// 播放中到达但序号已经过去了 —— 网络抖动超出了缓冲深度。
    pub late: u64,
    pub duplicate: u64,
    /// 缓冲满被迫丢弃的最旧帧。
    pub overflow: u64,
    pub played: u64,
    pub lost: u64,
    pub underruns: u64,
    pub prebuffer_ticks: u64,
    pub max_depth: usize,
}

pub struct JitterBuffer {
    cfg: JitterConfig,
    map: BTreeMap<u32, Vec<u8>>,
    next_seq: u32,
    playing: bool,
    pub stats: JitterStats,
}

impl JitterBuffer {
    pub fn new(cfg: JitterConfig) -> Self {
        Self {
            cfg,
            map: BTreeMap::new(),
            next_seq: 0,
            playing: false,
            stats: JitterStats::default(),
        }
    }

    pub fn depth(&self) -> usize {
        self.map.len()
    }

    pub fn is_playing(&self) -> bool {
        self.playing
    }

    /// 收到一帧。`payload` 是**已解密**的 Opus 负载。
    ///
    /// 注意：seq 的 u32 回绕这里没处理。20 ms 帧下要跑 994 天才绕一圈，
    /// 一次通话不可能跨这个跨度 —— 真正的处理点在会话重建时重置，见 M3。
    pub fn push(&mut self, seq: u32, payload: Vec<u8>) {
        self.stats.pushed += 1;

        if self.playing && seq < self.next_seq {
            self.stats.late += 1;
            return;
        }
        if self.map.contains_key(&seq) {
            self.stats.duplicate += 1;
            return;
        }
        if self.map.len() >= self.cfg.max_frames {
            self.stats.overflow += 1;
            if let Some(&oldest) = self.map.keys().next() {
                self.map.remove(&oldest);
            }
        }
        self.map.insert(seq, payload);
        self.stats.max_depth = self.stats.max_depth.max(self.map.len());
    }

    /// 播放线程每个帧周期调一次。
    pub fn pop(&mut self) -> Playout {
        if !self.playing {
            if self.map.len() < self.cfg.target_frames {
                self.stats.prebuffer_ticks += 1;
                return Playout::Prebuffering;
            }
            // 起播：对表到缓冲里最早的一帧。欠载重启后同样走这里重新对表。
            self.next_seq = *self.map.keys().next().expect("just checked non-empty");
            self.playing = true;
        }

        if let Some(payload) = self.map.remove(&self.next_seq) {
            let seq = self.next_seq;
            self.next_seq = self.next_seq.wrapping_add(1);
            self.stats.played += 1;
            return Playout::Frame { seq, payload };
        }

        if !self.map.is_empty() {
            let seq = self.next_seq;
            self.next_seq = self.next_seq.wrapping_add(1);
            self.stats.lost += 1;
            return Playout::Lost { seq };
        }

        self.stats.underruns += 1;
        self.playing = false;
        Playout::Underrun
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buf(target: usize) -> JitterBuffer {
        JitterBuffer::new(JitterConfig::fixed(target))
    }

    fn played(p: &Playout) -> Option<u32> {
        match p {
            Playout::Frame { seq, .. } => Some(*seq),
            _ => None,
        }
    }

    #[test]
    fn prebuffers_before_playing() {
        let mut b = buf(3);
        b.push(0, vec![0]);
        assert!(matches!(b.pop(), Playout::Prebuffering));
        b.push(1, vec![1]);
        assert!(matches!(b.pop(), Playout::Prebuffering));
        b.push(2, vec![2]);
        assert_eq!(played(&b.pop()), Some(0));
        assert_eq!(played(&b.pop()), Some(1));
        assert_eq!(played(&b.pop()), Some(2));
        assert_eq!(b.stats.prebuffer_ticks, 2);
    }

    #[test]
    fn reorders_within_depth() {
        let mut b = buf(3);
        for seq in [2u32, 0, 1] {
            b.push(seq, vec![seq as u8]);
        }
        assert_eq!(played(&b.pop()), Some(0));
        assert_eq!(played(&b.pop()), Some(1));
        assert_eq!(played(&b.pop()), Some(2));
        assert_eq!(b.stats.late, 0);
    }

    #[test]
    fn gap_becomes_plc_not_stall() {
        let mut b = buf(2);
        b.push(0, vec![0]);
        b.push(2, vec![2]); // 1 号丢了
        assert_eq!(played(&b.pop()), Some(0));
        assert!(matches!(b.pop(), Playout::Lost { seq: 1 }));
        assert_eq!(played(&b.pop()), Some(2));
        assert_eq!(b.stats.lost, 1);
    }

    #[test]
    fn underrun_reenters_prebuffer() {
        let mut b = buf(2);
        b.push(0, vec![0]);
        b.push(1, vec![1]);
        assert_eq!(played(&b.pop()), Some(0));
        assert_eq!(played(&b.pop()), Some(1));
        assert!(matches!(b.pop(), Playout::Underrun));
        assert!(!b.is_playing());
        // 重新攒够才播 —— 这一次重新预缓冲就是固定缓冲的延迟阶梯
        b.push(2, vec![2]);
        assert!(matches!(b.pop(), Playout::Prebuffering));
        b.push(3, vec![3]);
        assert_eq!(played(&b.pop()), Some(2));
        assert_eq!(b.stats.underruns, 1);
    }

    #[test]
    fn late_packet_dropped_while_playing() {
        let mut b = buf(2);
        b.push(5, vec![5]);
        b.push(6, vec![6]);
        assert_eq!(played(&b.pop()), Some(5));
        b.push(4, vec![4]); // 抖动超出深度
        assert_eq!(b.stats.late, 1);
        assert_eq!(played(&b.pop()), Some(6));
    }

    #[test]
    fn duplicate_is_counted_not_played_twice() {
        let mut b = buf(2);
        b.push(0, vec![0]);
        b.push(0, vec![0]);
        b.push(1, vec![1]);
        assert_eq!(b.stats.duplicate, 1);
        assert_eq!(played(&b.pop()), Some(0));
        assert_eq!(played(&b.pop()), Some(1));
    }

    #[test]
    fn overflow_drops_oldest() {
        let cfg = JitterConfig {
            target_frames: 2,
            max_frames: 4,
        };
        let mut b = JitterBuffer::new(cfg);
        for seq in 0..6u32 {
            b.push(seq, vec![seq as u8]);
        }
        assert_eq!(b.stats.overflow, 2);
        assert_eq!(b.depth(), 4);
        assert_eq!(played(&b.pop()), Some(2));
    }
}
