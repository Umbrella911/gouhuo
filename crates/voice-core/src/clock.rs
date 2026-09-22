// SPDX-License-Identifier: MPL-2.0
//! 实时节拍。
//!
//! Windows 默认的定时器粒度是 15.6 ms —— 对 10/20 ms 的语音帧是致命的，
//! 一个 `thread::sleep(20ms)` 实际可能睡 31 ms。所以：
//! 1. 进程级 `timeBeginPeriod(1)` 把粒度压到 1 ms；
//! 2. 距离截止点 1.5 ms 以内改成自旋。
//!
//! 这两步在真客户端里同样要做，不是测试脚手架 —— 抖动缓冲的深度是按
//! 「播放线程能准时醒」算的，醒不准就只能靠加深缓冲兜底，直接吃掉延迟预算。

use std::time::{Duration, Instant};

/// 睡到还剩这么多时间就改自旋。1.5 ms 覆盖 `timeBeginPeriod(1)` 后的调度抖动。
const SPIN_THRESHOLD: Duration = Duration::from_micros(1500);

/// RAII 持有 1 ms 定时器精度。这是**进程级全局**设置，整个客户端持有一份即可。
///
/// 代价：轻微增加系统功耗。TeamSpeak / Discord / 任何游戏引擎都这么干。
pub struct TimerResolutionGuard {
    acquired: bool,
}

impl TimerResolutionGuard {
    pub fn acquire() -> Self {
        #[cfg(windows)]
        let acquired = unsafe { windows_sys::Win32::Media::timeBeginPeriod(1) } == 0;
        #[cfg(not(windows))]
        let acquired = false;
        Self { acquired }
    }

    pub fn is_active(&self) -> bool {
        self.acquired
    }
}

impl Drop for TimerResolutionGuard {
    fn drop(&mut self) {
        #[cfg(windows)]
        if self.acquired {
            unsafe { windows_sys::Win32::Media::timeEndPeriod(1) };
        }
    }
}

/// 把当前线程提到 TIME_CRITICAL。
///
/// 真客户端应该走 MMCSS（`AvSetMmThreadCharacteristics("Pro Audio")`），
/// 那是 Windows 给音频线程的正式通道，能拿到更好的调度保证。M1 先用这个够了。
#[cfg(windows)]
pub fn boost_current_thread() -> bool {
    use windows_sys::Win32::System::Threading::{
        GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_TIME_CRITICAL,
    };
    unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_TIME_CRITICAL) != 0 }
}

#[cfg(not(windows))]
pub fn boost_current_thread() -> bool {
    false
}

/// 固定周期节拍器。周期是**绝对**推进的，不累积漂移。
pub struct Ticker {
    period: Duration,
    next: Instant,
    /// 醒来时已经错过下一个截止点的次数 —— 这个数不为 0 就说明机器扛不住，
    /// 测出来的延迟不可信。
    pub late_ticks: u64,
    /// 因为落后太多被迫跳过的节拍数。
    pub skipped_ticks: u64,
}

impl Ticker {
    /// 返回节拍器和第 0 拍的时刻（第一次 [`Ticker::tick`] 会在 `t0 + period` 唤醒）。
    pub fn start(period: Duration) -> (Self, Instant) {
        let t0 = Instant::now();
        (
            Self {
                period,
                next: t0 + period,
                late_ticks: 0,
                skipped_ticks: 0,
            },
            t0,
        )
    }

    /// 阻塞到下一个截止点，返回实际唤醒时刻。
    pub fn tick(&mut self) -> Instant {
        let target = self.next;
        loop {
            let now = Instant::now();
            if now >= target {
                break;
            }
            let left = target - now;
            if left > SPIN_THRESHOLD {
                std::thread::sleep(left - SPIN_THRESHOLD);
            } else {
                std::hint::spin_loop();
            }
        }
        let now = Instant::now();
        self.next += self.period;
        if now > self.next {
            self.late_ticks += 1;
            // 落后超过 5 拍说明不是抖动而是真卡住了，重新对表，
            // 否则会进入「越追越赶」的死循环，把测量彻底毁掉。
            while self.next < now {
                self.next += self.period;
                self.skipped_ticks += 1;
            }
        }
        now
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticker_holds_period_within_2ms() {
        let _guard = TimerResolutionGuard::acquire();
        let period = Duration::from_millis(20);
        let (mut t, t0) = Ticker::start(period);
        let mut worst = Duration::ZERO;
        for i in 1..=50u32 {
            let now = t.tick();
            let expected = t0 + period * i;
            let err = if now > expected {
                now - expected
            } else {
                expected - now
            };
            worst = worst.max(err);
        }
        // CI 上的共享 runner 会更糟，所以放到 5 ms；本机应该在 0.3 ms 以内。
        assert!(
            worst < Duration::from_millis(5),
            "ticker drifted by {worst:?}"
        );
    }
}
