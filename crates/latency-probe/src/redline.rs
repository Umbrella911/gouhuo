//! 红线。破线算 bug，不算优化项。
//!
//! 这几条是这个项目唯一真正的差异化，松一次就回不去了。改这个文件之前先想清楚。

/// 端到端延迟（同城），毫秒。嘴到耳，含设备。
pub const E2E_MS: f64 = 80.0;
/// voice-core 常驻内存，MB。
pub const VOICE_CORE_RSS_MB: f64 = 60.0;
/// 通话中 CPU，单核占比 %。
pub const CPU_PCT: f64 = 3.0;
/// 静音时带宽（DTX 生效），kbps。
pub const SILENT_KBPS: f64 = 5.0;
/// 说话时带宽，kbps。
pub const SPEAKING_KBPS: f64 = 40.0;
/// 安装包，MB。
pub const INSTALLER_MB: f64 = 30.0;

/// 80 ms 里预留给 WASAPI 采集 + 渲染的额度，毫秒。
///
/// **这是个假设，不是实测值。** 依据是共享模式下采集和渲染各约一个 10 ms
/// 引擎周期，加上混音器的一跳。独占模式应该能压到一半以下。
/// M2 的任务就是把这个数换成实测值 —— 在那之前，协议链路的预算按它扣。
pub const DEVICE_BUDGET_MS: f64 = 30.0;

/// 留给协议链路（打包 + 编码 + 网络 + 抖动缓冲 + 解码）的毫秒数。
pub const PROTOCOL_BUDGET_MS: f64 = E2E_MS - DEVICE_BUDGET_MS;
