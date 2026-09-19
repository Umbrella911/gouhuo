//! WASAPI：Windows 的音频设备接口。
//!
//! M2 的全部问题就一个：**80 ms 里设备到底吃掉多少？**
//! M1 里那 30 ms 是拍脑袋的假设，这个模块负责把它换成实测值。
//!
//! 三件事决定了这个数字，而且量级差得很远：
//!
//! 1. **共享模式 vs 独占模式。** 共享模式要过 Windows 的混音器（audiodg.exe），
//!    引擎周期默认 10 ms；独占模式绕过混音器直接跟驱动对话，好的声卡能到 3 ms 以下。
//!    代价是独占的时候别的程序（游戏本身！）发不出声 —— 对开黑软件基本不可接受。
//!    所以共享模式是主路径，独占只是用来标定「最好能到多少」。
//!
//! 2. **IAudioClient3 的低延迟共享模式。** Win10 之后共享模式也能把引擎周期
//!    压到驱动支持的最小值（`GetSharedModeEnginePeriod`），不用独占也能拿到
//!    接近独占的延迟。这是这个项目最该吃到的红利。
//!
//! 3. **虚拟声卡。** SteelSeries Sonar、雷蛇 Synapse、Voicemeeter、直播软件的
//!    虚拟线 —— 中国玩家机器上这些极其常见，而它们是用户态的音频路由，
//!    在真硬件的延迟上再叠一层。只测「默认设备」会得到一个完全不代表硬件的数字，
//!    所以这里把所有活动端点全都列出来。

mod device;
mod stream;

pub use device::*;
pub use stream::*;

use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};

/// COM 的 per-thread 初始化。WASAPI 全套都是 COM 对象，碰之前必须先初始化，
/// 而且是**每个线程**各初始化一次。
///
/// 用 MULTITHREADED 而不是 APARTMENTTHREADED：音频线程不跑消息循环，
/// STA 在这里只会带来莫名其妙的死锁。
pub struct ComGuard {
    _not_send: std::marker::PhantomData<*const ()>,
}

impl ComGuard {
    pub fn new() -> Self {
        // 返回 S_FALSE 表示这个线程已经初始化过了，也算成功 —— 照样要配对 CoUninitialize。
        let _ = unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }.ok();
        Self {
            _not_send: std::marker::PhantomData,
        }
    }
}

impl Default for ComGuard {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for ComGuard {
    fn drop(&mut self) {
        unsafe { CoUninitialize() };
    }
}
