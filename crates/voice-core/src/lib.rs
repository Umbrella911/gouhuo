//! 客户端语音内核。完全独立于 UI —— 如果实测内存不达标，把它拆成独立进程
//! 加托盘常驻是纯工程重构，不需要重写任何逻辑。
//!
//! M1 阶段只含三块：抖动缓冲、实时时钟、度量。
//! 后续补：WASAPI 采集/渲染、APM(AEC3/NS/AGC)、Opus 编解码封装、传输、全局热键。

pub mod clock;
pub mod jitter;
pub mod metrics;
pub mod net;

#[cfg(windows)]
pub mod sysstat;
