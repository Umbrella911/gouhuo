//! 端点枚举与能力探测。
//!
//! 这里只读不写：不打开流、不出声、不碰别人正在用的设备。
//! 唯一的副作用是 `Activate` 出一个 `IAudioClient`，那是纯查询，不 Initialize。

use std::fmt;

use windows::core::Interface;
use windows::Win32::Devices::FunctionDiscovery::{
    PKEY_Device_EnumeratorName, PKEY_Device_FriendlyName,
};
use windows::Win32::Media::Audio::{
    eCapture, eConsole, eRender, IAudioClient, IAudioClient3, IMMDevice, IMMDeviceEnumerator,
    MMDeviceEnumerator, AUDCLNT_SHAREMODE_EXCLUSIVE, DEVICE_STATE_ACTIVE, WAVEFORMATEX,
};
use windows::Win32::System::Com::{CoCreateInstance, CoTaskMemFree, CLSCTX_ALL, STGM_READ};
use windows::Win32::System::Variant::VT_LPWSTR;

/// mmreg.h 里的 wFormatTag 取值。windows crate 把它们散在不同 feature 下，
/// 为两个常量多开一个 feature 不值。
const WAVE_FORMAT_PCM: u16 = 0x0001;
const WAVE_FORMAT_IEEE_FLOAT: u16 = 0x0003;
const WAVE_FORMAT_EXTENSIBLE: u16 = 0xFFFE;

/// 采集还是渲染。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Render,
    Capture,
}

impl fmt::Display for Direction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Direction::Render => "render",
            Direction::Capture => "capture",
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Format {
    pub sample_rate: u32,
    pub channels: u16,
    pub bits: u16,
    /// true = 浮点样本（共享模式的引擎格式几乎一定是 32 位浮点）。
    pub float: bool,
}

impl fmt::Display for Format {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} Hz {}ch {}{}",
            self.sample_rate,
            self.channels,
            self.bits,
            if self.float { "f" } else { "i" }
        )
    }
}

/// `IAudioClient3` 报出来的引擎周期，单位是帧。
///
/// `min` 才是重点：Win10 之后共享模式也能申请到这个周期，不用走独占。
/// 但驱动不配合的话 min == default，那就没红利可吃。
#[derive(Debug, Clone, Copy)]
pub struct EnginePeriods {
    pub default_frames: u32,
    pub fundamental_frames: u32,
    pub min_frames: u32,
    pub max_frames: u32,
    pub current_frames: u32,
    pub sample_rate: u32,
}

impl EnginePeriods {
    pub fn default_ms(&self) -> f64 {
        frames_to_ms(self.default_frames, self.sample_rate)
    }
    pub fn min_ms(&self) -> f64 {
        frames_to_ms(self.min_frames, self.sample_rate)
    }
    pub fn current_ms(&self) -> f64 {
        frames_to_ms(self.current_frames, self.sample_rate)
    }
    /// 把引擎周期压到最小能省多少毫秒。0 就是这个驱动没有低延迟红利。
    pub fn headroom_ms(&self) -> f64 {
        self.default_ms() - self.min_ms()
    }
}

fn frames_to_ms(frames: u32, rate: u32) -> f64 {
    if rate == 0 {
        return f64::NAN;
    }
    frames as f64 * 1000.0 / rate as f64
}

#[derive(Debug, Clone)]
pub struct DeviceInfo {
    pub id: String,
    pub name: String,
    pub direction: Direction,
    pub is_default: bool,
    /// 总线枚举器名。这是判断真硬件还是虚拟声卡的**可靠**依据，
    /// 比猜名字强得多：HDAUDIO / USB 是真硬件，SWD 一类是软件设备。
    ///
    /// 为什么要分：SteelSeries Sonar、雷蛇 Synapse、Voicemeeter、直播软件的虚拟线
    /// 在中国玩家机器上极其常见，它们是用户态音频路由，在真硬件的延迟上再叠一层。
    /// 拿虚拟声卡的数字去填 DEVICE_BUDGET_MS 会得出一个谁都达不到的乐观值。
    pub bus: String,
    /// 共享模式下引擎用的格式。我们送进去的音频会被重采样到它 ——
    /// 不是 48 kHz 的话，每一帧都要过一次重采样器。
    pub mix_format: Option<Format>,
    /// `GetDevicePeriod` 的 default，共享模式的引擎周期。
    pub default_period_ms: f64,
    /// `GetDevicePeriod` 的 minimum，独占模式能申请到的最小周期。
    pub min_period_ms: f64,
    pub engine: Option<EnginePeriods>,
    /// 独占模式下 48 kHz 单声道 16 位是否被接受。
    /// 不接受的话独占路径就得自己做格式转换，延迟优势要打折。
    pub exclusive_48k_mono_i16: bool,
    pub exclusive_48k_stereo_i16: bool,
}

impl DeviceInfo {
    /// 是不是真的接在硬件总线上。软件设备（虚拟声卡、远程桌面音频、
    /// 各种"streaming"端点）走的是 SWD 或厂商自己的枚举器。
    pub fn is_hardware(&self) -> bool {
        matches!(
            self.bus.as_str(),
            "HDAUDIO" | "USB" | "PCI" | "BTHENUM" | "BTHHFENUM"
        )
    }

    /// 共享模式下这个设备的**单向**缓冲延迟下界，毫秒。
    ///
    /// 用当前生效的引擎周期。真实延迟还要加上驱动和硬件那一段，
    /// 那部分只能靠回环实测，`GetDevicePeriod` 是看不见的。
    pub fn shared_period_ms(&self) -> f64 {
        self.engine
            .map(|e| e.current_ms())
            .unwrap_or(self.default_period_ms)
    }
}

/// 列出所有**活动**端点。非活动的（拔掉的、禁用的）不列 —— 它们测不了。
pub fn enumerate() -> windows::core::Result<Vec<DeviceInfo>> {
    let enumerator: IMMDeviceEnumerator =
        unsafe { CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)? };

    let mut out = Vec::new();
    for direction in [Direction::Render, Direction::Capture] {
        let flow = match direction {
            Direction::Render => eRender,
            Direction::Capture => eCapture,
        };

        // 默认设备可能不存在（比如完全没有采集设备），不是错误。
        let default_id = unsafe { enumerator.GetDefaultAudioEndpoint(flow, eConsole) }
            .ok()
            .and_then(|d| device_id(&d).ok());

        let collection = unsafe { enumerator.EnumAudioEndpoints(flow, DEVICE_STATE_ACTIVE)? };
        let count = unsafe { collection.GetCount()? };
        for i in 0..count {
            let device = unsafe { collection.Item(i)? };
            let id = device_id(&device)?;
            let is_default = default_id.as_deref() == Some(id.as_str());
            out.push(inspect(&device, id, direction, is_default));
        }
    }
    Ok(out)
}

fn inspect(device: &IMMDevice, id: String, direction: Direction, is_default: bool) -> DeviceInfo {
    let name = friendly_name(device).unwrap_or_else(|_| "(读不到名字)".to_string());

    let mut info = DeviceInfo {
        id,
        name,
        direction,
        is_default,
        bus: string_prop(device, &PKEY_Device_EnumeratorName).unwrap_or_default(),
        mix_format: None,
        default_period_ms: f64::NAN,
        min_period_ms: f64::NAN,
        engine: None,
        exclusive_48k_mono_i16: false,
        exclusive_48k_stereo_i16: false,
    };

    // Activate 失败是常事：设备被独占占用了，或者驱动在闹脾气。
    // 那就只报名字，不要让整个枚举失败。
    let Ok(client) = (unsafe { device.Activate::<IAudioClient>(CLSCTX_ALL, None) }) else {
        return info;
    };

    let (mut default_period, mut min_period) = (0i64, 0i64);
    if unsafe { client.GetDevicePeriod(Some(&mut default_period), Some(&mut min_period)) }.is_ok() {
        info.default_period_ms = reftime_to_ms(default_period);
        info.min_period_ms = reftime_to_ms(min_period);
    }

    if let Ok(mix) = unsafe { client.GetMixFormat() } {
        info.mix_format = unsafe { read_format(mix) };
        unsafe { CoTaskMemFree(Some(mix as *const _)) };
    }

    info.engine = engine_periods(&client);
    info.exclusive_48k_mono_i16 = exclusive_supported(&client, 1);
    info.exclusive_48k_stereo_i16 = exclusive_supported(&client, 2);

    info
}

fn engine_periods(client: &IAudioClient) -> Option<EnginePeriods> {
    // IAudioClient3 是 Win10 才有的。拿不到就是这台机器/驱动没有低延迟共享模式。
    let client3: IAudioClient3 = client.cast().ok()?;

    let mut format: *mut WAVEFORMATEX = std::ptr::null_mut();
    let mut current_frames = 0u32;
    unsafe { client3.GetCurrentSharedModeEnginePeriod(&mut format, &mut current_frames) }.ok()?;
    if format.is_null() {
        return None;
    }

    let sample_rate = unsafe { (*format).nSamplesPerSec };
    let (mut default_frames, mut fundamental_frames, mut min_frames, mut max_frames) = (0, 0, 0, 0);
    let ok = unsafe {
        client3.GetSharedModeEnginePeriod(
            format,
            &mut default_frames,
            &mut fundamental_frames,
            &mut min_frames,
            &mut max_frames,
        )
    }
    .is_ok();
    unsafe { CoTaskMemFree(Some(format as *const _)) };

    ok.then_some(EnginePeriods {
        default_frames,
        fundamental_frames,
        min_frames,
        max_frames,
        current_frames,
        sample_rate,
    })
}

fn exclusive_supported(client: &IAudioClient, channels: u16) -> bool {
    let block_align = channels * 2; // 16 位
    let wfx = WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_PCM,
        nChannels: channels,
        nSamplesPerSec: 48_000,
        nAvgBytesPerSec: 48_000 * block_align as u32,
        nBlockAlign: block_align,
        wBitsPerSample: 16,
        cbSize: 0,
    };
    // 独占模式下 closest-match 必须传 None：要么支持，要么不支持，没有折中。
    unsafe { client.IsFormatSupported(AUDCLNT_SHAREMODE_EXCLUSIVE, &wfx, None) }.is_ok()
}

/// REFERENCE_TIME 是 100 ns 为单位的。
fn reftime_to_ms(rt: i64) -> f64 {
    rt as f64 / 10_000.0
}

unsafe fn read_format(wfx: *const WAVEFORMATEX) -> Option<Format> {
    if wfx.is_null() {
        return None;
    }
    let f = unsafe { &*wfx };
    // 共享模式的混音格式通常是 WAVE_FORMAT_EXTENSIBLE(0xFFFE)，子格式才说明是不是浮点。
    // 这里只要知道位深和是否浮点，按 32 位就是浮点来判断足够了 ——
    // 真要建流的时候会用 GetMixFormat 原样回传，不需要我们重建结构体。
    let float = f.wFormatTag == WAVE_FORMAT_IEEE_FLOAT
        || (f.wFormatTag == WAVE_FORMAT_EXTENSIBLE && f.wBitsPerSample == 32);
    Some(Format {
        sample_rate: f.nSamplesPerSec,
        channels: f.nChannels,
        bits: f.wBitsPerSample,
        float,
    })
}

fn device_id(device: &IMMDevice) -> windows::core::Result<String> {
    let id = unsafe { device.GetId()? };
    let s = unsafe { id.to_string() }.unwrap_or_default();
    unsafe { CoTaskMemFree(Some(id.0 as *const _)) };
    Ok(s)
}

fn friendly_name(device: &IMMDevice) -> windows::core::Result<String> {
    string_prop(device, &PKEY_Device_FriendlyName)
}

fn string_prop(
    device: &IMMDevice,
    key: &windows::Win32::Foundation::PROPERTYKEY,
) -> windows::core::Result<String> {
    let store = unsafe { device.OpenPropertyStore(STGM_READ)? };
    let value = unsafe { store.GetValue(key)? };
    let vt = unsafe { value.Anonymous.Anonymous.vt };
    if vt != VT_LPWSTR {
        return Ok(String::new());
    }
    let pwsz = unsafe { value.Anonymous.Anonymous.Anonymous.pwszVal };
    Ok(unsafe { pwsz.to_string() }.unwrap_or_default())
}
