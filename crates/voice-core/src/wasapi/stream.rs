// SPDX-License-Identifier: MPL-2.0
//! 打开真的采集/渲染流，量真的延迟。
//!
//! # 怎么做到不出声
//!
//! 测设备延迟的常规做法是放个信号再录回来做互相关 —— 那会真的从音箱里响。
//! 这里两边都绕开了：
//!
//! - **采集**：WASAPI 自己就给了答案。`IAudioCaptureClient::GetBuffer` 的
//!   `pu64QPCPosition` 是「这一包的第一个采样点被设备记录下来的那一刻」的
//!   性能计数器值。拿它跟我们拿到数据的时刻一减，就是设备到应用的真实延迟，
//!   不需要任何信号，也不需要麦克风前面有声音。
//!
//! - **渲染**：灌静音。渲染侧的延迟不是"测"出来的，是**选**出来的 ——
//!   我们在队列里压多少音频，新写进去的一帧就要等多久才播得到。所以真正的问题是
//!   「最浅能压到多少而不欠载」，答案靠从一个周期起往上扫得到。全程输出 0。
//!
//! 真正的声学往返（音箱 → 空气 → 麦克风）是另一回事，会出声，得单独开关。
//!
//! # 为什么要量唤醒间隔
//!
//! 申请到 3 ms 的周期，和每 3 ms 真能准时醒来，是两回事。醒不准就只能靠加深
//! 缓冲兜底，那等于把申请来的低延迟又还回去了。所以每一路都同时记唤醒间隔的
//! 分布和 glitch 计数 —— 这两个数不好看的话，周期数字本身没有意义。

use std::time::Instant;

use windows::core::Interface;
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    eCapture, eConsole, eRender, IAudioCaptureClient, IAudioClient, IAudioClient3,
    IAudioRenderClient, IMMDevice, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY, AUDCLNT_SHAREMODE_EXCLUSIVE, AUDCLNT_SHAREMODE_SHARED,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, WAVEFORMATEX,
};
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL};
use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};

use crate::metrics::Histogram;
use crate::wasapi::{Direction, Format};

/// 独占模式下 Initialize 会因为缓冲没对齐而失败，按 MSDN 的规矩重算一次再来。
const AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED: i32 = -2004287463; // 0x88890019

/// 把 AUDCLNT_* 的 HRESULT 翻成人话。裸的 0x88890016 对着查手册太费劲，
/// 而这些错误码恰恰是独占模式最容易踩的地方。
pub fn describe_hresult(code: i32) -> Option<&'static str> {
    Some(match code as u32 {
        0x8889_0002 => "已经初始化过",
        0x8889_0003 => "端点类型不对（把渲染当采集用了）",
        0x8889_0004 => "设备已失效（被拔了或被禁用）",
        0x8889_0006 => "缓冲要得太大",
        0x8889_0008 => "设备不接受这个格式",
        0x8889_000A => "设备正被别的程序独占",
        0x8889_000E => "系统设置里禁止了独占模式",
        0x8889_000F => "创建端点失败",
        0x8889_0010 => "Windows 音频服务没在跑",
        0x8889_0012 => "这个设备只能独占用",
        0x8889_0013 => "事件驱动的独占模式要求缓冲时长等于周期",
        0x8889_0015 => "缓冲大小不对",
        0x8889_0016 => "缓冲大小错误（独占模式下缓冲必须正好是一个周期）",
        0x8889_0017 => "驱动说 CPU 扛不住这个周期",
        0x8889_0019 => "缓冲没按帧对齐",
        _ => return None,
    })
}

/// 把 WASAPI 的错误翻成人话。
///
/// 放在 voice-core 里而不是探针里，是为了让 COM 的类型止步于这个 crate ——
/// 上层不该为了打印一条错误就去依赖 windows crate。
pub fn describe_error(e: &windows::core::Error) -> String {
    let code = e.code().0;
    match describe_hresult(code) {
        Some(text) => format!("{text}（{:#010x}）", code as u32),
        None => format!("{e}"),
    }
}

/// 前几个包不计入统计：流刚起来时 WASAPI 必定报一次 DATA_DISCONTINUITY，
/// 队列也还没进入稳态。不跳过的话每一路都会平白带一个 glitch。
const WARMUP_PACKETS: u64 = 4;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ShareMode {
    /// 过 Windows 混音器。别的程序（游戏本身）同时能出声 —— 开黑软件的唯一选择。
    Shared,
    /// 绕过混音器直接跟驱动对话。延迟低，但独占期间别的程序发不出声。
    /// 这里只用来标定「这台机器最好能到多少」。
    Exclusive,
}

impl std::fmt::Display for ShareMode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            ShareMode::Shared => "shared",
            ShareMode::Exclusive => "exclusive",
        })
    }
}

#[derive(Debug, Clone)]
pub struct StreamStats {
    pub direction: Direction,
    pub share_mode: ShareMode,
    pub device_name: String,
    pub format: Format,
    /// `Initialize` 实际给到的缓冲大小。
    pub buffer_frames: u32,
    pub buffer_ms: f64,
    /// 驱动自报的流延迟。厂商填的，参考值，不是实测。
    pub reported_latency_ms: f64,
    /// 事件唤醒的间隔分布。理想情况应该紧紧贴着周期。
    pub wake_interval_ms: Histogram,
    /// 采集专用：设备记录下这一包到我们拿到手，中间过了多久。**这是实测的采集延迟。**
    pub capture_latency_ms: Histogram,
    /// 渲染专用：每次唤醒、补数据**之前**队列里还剩多少毫秒。
    ///
    /// 这是**欠载余量**，不是延迟 —— 很容易搞混。它衡量的是「离咔哒声还有多远」，
    /// 归零就是已经欠载了。余量小说明这个深度太激进。
    pub render_margin_ms: Histogram,
    /// 渲染专用：实际维持的队列深度（夹过之后的值）。
    ///
    /// **这才是渲染侧吃掉的延迟。** 我们刚写进去的一帧，前面压着这么多音频要先播完。
    /// 而它是我们自己选的 —— 所以渲染侧的问题不是「测出一个数」，是「最浅能选到多少」。
    pub target_queue_ms: f64,
    pub packets: u64,
    pub frames: u64,
    /// 采集侧的数据不连续（丢了一段），或渲染侧的欠载。不为 0 说明这个配置扛不住。
    pub glitches: u64,
}

impl StreamStats {
    fn new(
        direction: Direction,
        share_mode: ShareMode,
        device_name: String,
        format: Format,
    ) -> Self {
        Self {
            direction,
            share_mode,
            device_name,
            format,
            buffer_frames: 0,
            buffer_ms: f64::NAN,
            reported_latency_ms: f64::NAN,
            wake_interval_ms: Histogram::with_capacity(4096),
            capture_latency_ms: Histogram::with_capacity(4096),
            render_margin_ms: Histogram::with_capacity(4096),
            target_queue_ms: f64::NAN,
            packets: 0,
            frames: 0,
            glitches: 0,
        }
    }
}

/// 性能计数器，换算成 100 ns 单位 —— WASAPI 的 QPC 位置就是这个单位。
struct Qpc {
    freq: i64,
}

impl Qpc {
    fn new() -> Self {
        let mut freq = 0i64;
        let _ = unsafe { QueryPerformanceFrequency(&mut freq) };
        Self { freq: freq.max(1) }
    }

    fn now_100ns(&self) -> i64 {
        let mut c = 0i64;
        let _ = unsafe { QueryPerformanceCounter(&mut c) };
        // 先乘后除会溢出（QPC 计数很大），拆成整秒 + 余数。
        let secs = c / self.freq;
        let rem = c % self.freq;
        secs * 10_000_000 + rem * 10_000_000 / self.freq
    }
}

/// RAII 包一下事件句柄，免得早退路径漏掉 CloseHandle。
pub(crate) struct Event(pub(crate) HANDLE);

impl Event {
    pub(crate) fn new() -> windows::core::Result<Self> {
        Ok(Self(unsafe { CreateEventW(None, false, false, None)? }))
    }
}

impl Drop for Event {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

fn default_device(direction: Direction) -> windows::core::Result<IMMDevice> {
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
    let flow = match direction {
        Direction::Render => eRender,
        Direction::Capture => eCapture,
    };
    unsafe { enumerator.GetDefaultAudioEndpoint(flow, eConsole) }
}

fn device_by_id(id: &str) -> windows::core::Result<IMMDevice> {
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };
    let wide: Vec<u16> = id.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe { enumerator.GetDevice(windows::core::PCWSTR(wide.as_ptr())) }
}

/// 拿设备：给了 id 就用 id，没给就用默认端点。
pub fn open_device(id: Option<&str>, direction: Direction) -> windows::core::Result<IMMDevice> {
    match id {
        Some(id) => device_by_id(id),
        None => default_device(direction),
    }
}

/// 独占模式用的格式：48 kHz 16 位。声道数由调用方给 —— 设备清单里已经探过支持哪个。
fn exclusive_format(channels: u16) -> WAVEFORMATEX {
    let block_align = channels * 2;
    WAVEFORMATEX {
        wFormatTag: 1, // WAVE_FORMAT_PCM
        nChannels: channels,
        nSamplesPerSec: 48_000,
        nAvgBytesPerSec: 48_000 * block_align as u32,
        nBlockAlign: block_align,
        wBitsPerSample: 16,
        cbSize: 0,
    }
}

unsafe fn format_of(wfx: *const WAVEFORMATEX) -> Format {
    let f = unsafe { &*wfx };
    Format {
        sample_rate: f.nSamplesPerSec,
        channels: f.nChannels,
        bits: f.wBitsPerSample,
        float: f.wFormatTag == 0x0003 || (f.wFormatTag == 0xFFFE && f.wBitsPerSample == 32),
    }
}

/// 一个初始化好的流：客户端 + 事件 + 实际格式。
pub(crate) struct Opened {
    pub(crate) client: IAudioClient,
    pub(crate) event: Event,
    pub(crate) format: Format,
    #[allow(dead_code)]
    pub(crate) frame_bytes: usize,
    pub(crate) buffer_frames: u32,
    /// 设备周期换算成帧。队列深度的物理下限就是它 —— 见 measure_render。
    #[allow(dead_code)]
    pub(crate) period_frames: u32,
}

pub(crate) fn initialize(
    device: &IMMDevice,
    direction: Direction,
    share_mode: ShareMode,
    channels: u16,
) -> windows::core::Result<Opened> {
    let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None)? };
    let (mut default_period, mut min_period) = (0i64, 0i64);
    unsafe { client.GetDevicePeriod(Some(&mut default_period), Some(&mut min_period))? };

    // 共享模式的节奏由引擎周期决定，独占模式由我们申请的最小周期决定。
    let period_for_clamp = match share_mode {
        ShareMode::Shared => default_period,
        ShareMode::Exclusive => min_period,
    };

    let event = Event::new()?;
    let (share, period, owned_mix, wfx_ptr): (
        _,
        i64,
        Option<*mut WAVEFORMATEX>,
        *const WAVEFORMATEX,
    ) = match share_mode {
        ShareMode::Shared => {
            // 共享模式必须用引擎的混音格式，别的格式会被拒或者被偷偷重采样。
            let mix = unsafe { client.GetMixFormat()? };
            (AUDCLNT_SHAREMODE_SHARED, 0, Some(mix), mix as *const _)
        }
        ShareMode::Exclusive => {
            let fmt = Box::into_raw(Box::new(exclusive_format(channels)));
            (
                AUDCLNT_SHAREMODE_EXCLUSIVE,
                min_period,
                None,
                fmt as *const _,
            )
        }
    };

    // 共享模式传 0 让引擎自己定；独占模式缓冲时长必须等于周期。
    let mut buffer_duration = period;
    let mut result = unsafe {
        client.Initialize(
            share,
            AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
            buffer_duration,
            period,
            wfx_ptr,
            None,
        )
    };

    // 独占模式的经典坑：驱动要求缓冲按帧对齐，第一次必定可能失败，
    // 要拿它算出来的对齐后大小重新建一个 client 再来。少这一步，
    // 独占模式在很多声卡上就是打不开。
    if let Err(e) = &result {
        if e.code().0 == AUDCLNT_E_BUFFER_SIZE_NOT_ALIGNED {
            let aligned_frames = unsafe { client.GetBufferSize()? };
            let rate = unsafe { (*wfx_ptr).nSamplesPerSec } as f64;
            buffer_duration = (10_000_000.0 * aligned_frames as f64 / rate).round() as i64;
            let client2: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None)? };
            result = unsafe {
                client2.Initialize(
                    share,
                    AUDCLNT_STREAMFLAGS_EVENTCALLBACK,
                    buffer_duration,
                    buffer_duration,
                    wfx_ptr,
                    None,
                )
            };
            if result.is_ok() {
                let format = unsafe { format_of(wfx_ptr) };
                let frame_bytes = (format.channels as usize) * (format.bits as usize / 8);
                unsafe { client2.SetEventHandle(event.0)? };
                let buffer_frames = unsafe { client2.GetBufferSize()? };
                cleanup_format(owned_mix, wfx_ptr, share_mode);
                let period_frames = period_to_frames(period_for_clamp, format.sample_rate);
                return Ok(Opened {
                    client: client2,
                    event,
                    format,
                    frame_bytes,
                    buffer_frames,
                    period_frames,
                });
            }
        }
    }

    let outcome = result.map(|_| ());
    let format = unsafe { format_of(wfx_ptr) };
    let frame_bytes = (format.channels as usize) * (format.bits as usize / 8);
    cleanup_format(owned_mix, wfx_ptr, share_mode);
    outcome?;

    unsafe { client.SetEventHandle(event.0)? };
    let buffer_frames = unsafe { client.GetBufferSize()? };
    let _ = direction; // 方向只影响后面取哪个服务接口
    let period_frames = period_to_frames(period_for_clamp, format.sample_rate);
    Ok(Opened {
        client,
        event,
        format,
        frame_bytes,
        buffer_frames,
        period_frames,
    })
}

fn period_to_frames(reftime: i64, rate: u32) -> u32 {
    ((reftime as f64 / 10_000_000.0) * rate as f64)
        .round()
        .max(1.0) as u32
}

fn cleanup_format(
    owned_mix: Option<*mut WAVEFORMATEX>,
    wfx_ptr: *const WAVEFORMATEX,
    share_mode: ShareMode,
) {
    match owned_mix {
        Some(mix) => unsafe { CoTaskMemFree(Some(mix as *const _)) },
        None => {
            if share_mode == ShareMode::Exclusive && !wfx_ptr.is_null() {
                drop(unsafe { Box::from_raw(wfx_ptr as *mut WAVEFORMATEX) });
            }
        }
    }
}

/// 尝试把共享模式的引擎周期压到最小（Win10+ 的低延迟共享模式）。
///
/// 成功与否完全看驱动：`engmin == engdefault` 的设备上这条路没有红利。
pub fn min_shared_period_frames(device: &IMMDevice) -> Option<(u32, u32)> {
    let client: IAudioClient = unsafe { device.Activate(CLSCTX_ALL, None) }.ok()?;
    let client3: IAudioClient3 = client.cast().ok()?;
    let mut format: *mut WAVEFORMATEX = std::ptr::null_mut();
    let mut current = 0u32;
    unsafe { client3.GetCurrentSharedModeEnginePeriod(&mut format, &mut current) }.ok()?;
    let (mut def, mut fun, mut min, mut max) = (0, 0, 0, 0);
    let ok = unsafe {
        client3.GetSharedModeEnginePeriod(format, &mut def, &mut fun, &mut min, &mut max)
    }
    .is_ok();
    unsafe { CoTaskMemFree(Some(format as *const _)) };
    ok.then_some((current, min))
}

/// 跑一路采集，量真实的设备到应用延迟。**不出声，也不需要麦克风前面有声音。**
pub fn measure_capture(
    device: &IMMDevice,
    device_name: &str,
    share_mode: ShareMode,
    channels: u16,
    seconds: f64,
) -> windows::core::Result<StreamStats> {
    let opened = initialize(device, Direction::Capture, share_mode, channels)?;
    let capture: IAudioCaptureClient = unsafe { opened.client.GetService()? };

    let mut stats = StreamStats::new(
        Direction::Capture,
        share_mode,
        device_name.to_string(),
        opened.format,
    );
    stats.buffer_frames = opened.buffer_frames;
    stats.buffer_ms = frames_ms(opened.buffer_frames, opened.format.sample_rate);
    stats.reported_latency_ms = unsafe { opened.client.GetStreamLatency() }
        .map(reftime_ms)
        .unwrap_or(f64::NAN);

    let qpc = Qpc::new();
    crate::clock::boost_current_thread();
    unsafe { opened.client.Start()? };

    let deadline = Instant::now() + std::time::Duration::from_secs_f64(seconds);
    let mut last_wake: Option<Instant> = None;

    while Instant::now() < deadline {
        // 超时给 200 ms：正常情况下每个周期都会醒，超时说明流已经死了。
        if unsafe { WaitForSingleObject(opened.event.0, 200) } != WAIT_OBJECT_0 {
            break;
        }
        let now = Instant::now();
        if let Some(prev) = last_wake {
            stats
                .wake_interval_ms
                .push((now - prev).as_secs_f64() * 1000.0);
        }
        last_wake = Some(now);

        loop {
            let avail = unsafe { capture.GetNextPacketSize()? };
            if avail == 0 {
                break;
            }
            let mut data = std::ptr::null_mut();
            let mut frames = 0u32;
            let mut flags = 0u32;
            let mut device_pos = 0u64;
            let mut qpc_pos = 0u64;
            unsafe {
                capture.GetBuffer(
                    &mut data,
                    &mut frames,
                    &mut flags,
                    Some(&mut device_pos),
                    Some(&mut qpc_pos),
                )?
            };

            if frames > 0 {
                stats.packets += 1;
                stats.frames += frames as u64;
                // 头几个包不算数：流刚起来的时候 WASAPI 必定报一次
                // DATA_DISCONTINUITY（"从这里开始是新的"），首包的延迟也含起播开销。
                // 把它算成 glitch 的话每一路都会莫名其妙带一个 1。
                if stats.packets > WARMUP_PACKETS {
                    // 设备记录下这一包第一个采样点的时刻 -> 现在。这就是采集延迟。
                    let age_100ns = qpc.now_100ns() - qpc_pos as i64;
                    if age_100ns >= 0 {
                        stats.capture_latency_ms.push(age_100ns as f64 / 10_000.0);
                    }
                    if flags & AUDCLNT_BUFFERFLAGS_DATA_DISCONTINUITY.0 as u32 != 0 {
                        stats.glitches += 1;
                    }
                }
            }
            unsafe { capture.ReleaseBuffer(frames)? };
        }
    }

    let _ = unsafe { opened.client.Stop() };
    Ok(stats)
}

/// 跑一路渲染，量队列深度。**全程输出静音，音箱里什么都听不到。**
pub fn measure_render(
    device: &IMMDevice,
    device_name: &str,
    share_mode: ShareMode,
    channels: u16,
    seconds: f64,
    target_queue_ms: f64,
) -> windows::core::Result<StreamStats> {
    let opened = initialize(device, Direction::Render, share_mode, channels)?;
    let render: IAudioRenderClient = unsafe { opened.client.GetService()? };

    let mut stats = StreamStats::new(
        Direction::Render,
        share_mode,
        device_name.to_string(),
        opened.format,
    );
    stats.buffer_frames = opened.buffer_frames;
    stats.buffer_ms = frames_ms(opened.buffer_frames, opened.format.sample_rate);
    stats.reported_latency_ms = unsafe { opened.client.GetStreamLatency() }
        .map(reftime_ms)
        .unwrap_or(f64::NAN);

    // 目标队列深度换算成帧，并夹在 [一个周期, 整个缓冲] 之间 ——
    // 比一个周期还浅必然欠载，比缓冲还深也放不下。
    let rate = opened.format.sample_rate;
    // 队列比一个周期还浅是物理上不可能的：事件是「已经播完一个周期」才触发的，
    // 醒来时队列里剩的就是 深度 − 一个周期。深度 <= 周期 就等于每次都欠载。
    // 所以下限是 周期 + 1 帧，不是 0。
    let floor_frames = (opened.period_frames + 1).min(opened.buffer_frames);
    let target_frames = ((target_queue_ms * rate as f64 / 1000.0).round() as u32)
        .clamp(floor_frames, opened.buffer_frames);
    stats.target_queue_ms = frames_ms(target_frames, rate);

    // 起播前先把队列垫到目标深度，否则第一拍必然欠载。
    fill_silence(&render, target_frames, opened.frame_bytes)?;

    let mut warmed_up = false;

    crate::clock::boost_current_thread();
    unsafe { opened.client.Start()? };

    let deadline = Instant::now() + std::time::Duration::from_secs_f64(seconds);
    let mut last_wake: Option<Instant> = None;

    while Instant::now() < deadline {
        if unsafe { WaitForSingleObject(opened.event.0, 200) } != WAIT_OBJECT_0 {
            break;
        }
        let now = Instant::now();
        if let Some(prev) = last_wake {
            stats
                .wake_interval_ms
                .push((now - prev).as_secs_f64() * 1000.0);
        }
        last_wake = Some(now);

        // padding = 已经排队但还没播出去的帧数。
        //
        // 补数据**之前**的 padding = 欠载余量。注意它不是延迟：
        // 延迟是我们维持的深度 target_queue_ms（新写进去的一帧前面压着那么多要先播），
        // 余量是「离咔哒声还有多远」（= 深度 − 一个周期）。两个数都要，别混。
        let padding = unsafe { opened.client.GetCurrentPadding()? };
        if warmed_up {
            stats
                .render_margin_ms
                .push(frames_ms(padding, opened.format.sample_rate));
            if padding == 0 {
                stats.glitches += 1;
            }
        }

        let want = target_frames.saturating_sub(padding);
        let available = want.min(opened.buffer_frames.saturating_sub(padding));
        if available == 0 {
            continue;
        }
        if fill_silence(&render, available, opened.frame_bytes).is_err() {
            stats.glitches += 1;
            continue;
        }
        stats.packets += 1;
        stats.frames += available as u64;
        if stats.packets >= WARMUP_PACKETS {
            warmed_up = true;
        }
    }

    let _ = unsafe { opened.client.Stop() };
    Ok(stats)
}

fn fill_silence(
    render: &IAudioRenderClient,
    frames: u32,
    frame_bytes: usize,
) -> windows::core::Result<()> {
    if frames == 0 {
        return Ok(());
    }
    let data = unsafe { render.GetBuffer(frames)? };
    unsafe { std::ptr::write_bytes(data, 0, frames as usize * frame_bytes) };
    unsafe { render.ReleaseBuffer(frames, 0)? };
    Ok(())
}

fn frames_ms(frames: u32, rate: u32) -> f64 {
    if rate == 0 {
        f64::NAN
    } else {
        frames as f64 * 1000.0 / rate as f64
    }
}

fn reftime_ms(rt: i64) -> f64 {
    rt as f64 / 10_000.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frames_and_reftime_conversions() {
        assert_eq!(frames_ms(480, 48_000), 10.0);
        assert_eq!(frames_ms(0, 48_000), 0.0);
        assert!(frames_ms(480, 0).is_nan());
        assert_eq!(reftime_ms(100_000), 10.0);
        assert_eq!(period_to_frames(100_000, 48_000), 480);
        // 再小的周期也至少是一帧，不能算出 0 来（0 会让队列夹到 0 深度）
        assert_eq!(period_to_frames(1, 48_000), 1);
    }

    #[test]
    fn qpc_is_monotonic_and_in_100ns_units() {
        let q = Qpc::new();
        let a = q.now_100ns();
        std::thread::sleep(std::time::Duration::from_millis(20));
        let b = q.now_100ns();
        let elapsed_ms = (b - a) as f64 / 10_000.0;
        assert!(
            (15.0..60.0).contains(&elapsed_ms),
            "20 ms 的睡眠量出来是 {elapsed_ms} ms，单位换算错了"
        );
    }

    #[test]
    fn known_audclnt_errors_are_translated() {
        // 独占模式最容易踩的两个
        assert!(describe_hresult(0x8889_0016u32 as i32).is_some());
        assert!(describe_hresult(0x8889_0008u32 as i32).is_some());
        // 不认识的就老实说不认识，别编
        assert!(describe_hresult(0x1234_5678).is_none());
    }

    #[test]
    fn exclusive_format_is_well_formed() {
        for channels in [1u16, 2] {
            let f = exclusive_format(channels);
            // WAVEFORMATEX 是 packed 的，字段不能直接借引用，先拷出来。
            let (block_align, avg_bytes, rate, bits) = (
                f.nBlockAlign,
                f.nAvgBytesPerSec,
                f.nSamplesPerSec,
                f.wBitsPerSample,
            );
            assert_eq!(block_align, channels * 2, "16 位下每帧就是 2 字节一声道");
            assert_eq!(avg_bytes, rate * block_align as u32);
            assert_eq!(bits, 16);
        }
    }
}
