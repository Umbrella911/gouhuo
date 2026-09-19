//! 红线。破线算 bug，不算优化项。
//!
//! 放在 voice-core 里是因为它是**全项目**的策略，不属于某一个探针：
//! latency-probe（M1，协议链路）和 device-probe（M2，音频设备）都对着同一组数字。
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
/// **M2 实测值**（见 docs/m2-baseline.txt），不再是假设。拆开是：
/// - 采集 10.3 ms —— 一个引擎周期，WASAPI 的 QPC 时间戳直接量到的，没得选
/// - 渲染 20.0 ms —— 我们维持的队列深度，**这是个设计选择，不是物理限制**
///
/// 渲染那 20 ms 值得说清楚：物理下限是 10 ms（一个周期多一帧），实测零欠载。
/// 但那个工作点的余量只有一帧，线程漏醒一次就是一声咔哒 —— 而这软件是要跟
/// 游戏抢 CPU 的。取 20 ms 是为了留住整整一个周期的余量，扛得住漏醒一次。
/// 想换回 10 ms 也行，那是拿「偶尔咔哒」换 10 ms 延迟，属于产品决定不是技术决定。
///
/// 这个数字来自一台机器（Ryzen 7 5700X3D + USB 声卡，共享模式引擎周期 10 ms）。
/// 虚拟声卡、蓝牙耳机都不在里面，它们只会更高。
pub const DEVICE_BUDGET_MS: f64 = 30.5;

/// 留给协议链路（打包 + 编码 + 网络 + 抖动缓冲 + 解码）的毫秒数。
pub const PROTOCOL_BUDGET_MS: f64 = E2E_MS - DEVICE_BUDGET_MS;
