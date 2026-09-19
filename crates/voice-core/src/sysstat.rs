//! 进程自测：常驻内存和 CPU 占用。两条都是红线，所以直接内建，
//! 不靠外部工具人工看任务管理器。
//!
//! 注意 M1 阶段量的是 latency-probe 整个进程，不是最终的 voice-core。
//! 真正对红线负责的数字要等 voice-core 拆成独立进程之后再量。

use std::time::Instant;

use windows_sys::Win32::Foundation::FILETIME;
use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
use windows_sys::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};

#[derive(Debug, Clone, Copy)]
pub struct MemInfo {
    /// 当前工作集（任务管理器里的「内存」那一列）。
    pub working_set: u64,
    /// 进程生命周期内的工作集峰值 —— 红线要看这个，不是瞬时值。
    pub peak_working_set: u64,
}

pub fn mem_info() -> Option<MemInfo> {
    let mut c: PROCESS_MEMORY_COUNTERS = unsafe { std::mem::zeroed() };
    c.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
    let ok = unsafe { GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) };
    if ok == 0 {
        return None;
    }
    Some(MemInfo {
        working_set: c.WorkingSetSize as u64,
        peak_working_set: c.PeakWorkingSetSize as u64,
    })
}

fn filetime_to_nanos(ft: FILETIME) -> u64 {
    // FILETIME 是 100 ns 为单位的 64 位数，拆成高低两个 u32 存的。
    let v = ((ft.dwHighDateTime as u64) << 32) | ft.dwLowDateTime as u64;
    v * 100
}

fn process_cpu_nanos() -> Option<u64> {
    let mut creation: FILETIME = unsafe { std::mem::zeroed() };
    let mut exit: FILETIME = unsafe { std::mem::zeroed() };
    let mut kernel: FILETIME = unsafe { std::mem::zeroed() };
    let mut user: FILETIME = unsafe { std::mem::zeroed() };
    let ok = unsafe {
        GetProcessTimes(
            GetCurrentProcess(),
            &mut creation,
            &mut exit,
            &mut kernel,
            &mut user,
        )
    };
    if ok == 0 {
        return None;
    }
    Some(filetime_to_nanos(kernel) + filetime_to_nanos(user))
}

/// 两次采样之间的 CPU 占用。
pub struct CpuSampler {
    last_wall: Instant,
    last_cpu: u64,
}

impl CpuSampler {
    pub fn start() -> Self {
        Self {
            last_wall: Instant::now(),
            last_cpu: process_cpu_nanos().unwrap_or(0),
        }
    }

    /// 返回自上次采样以来的 CPU 占用，单位是**单核百分比**
    /// （8 核机器上一个核跑满 = 100.0，不是 12.5）。红线按单核算。
    pub fn sample(&mut self) -> f64 {
        let now = Instant::now();
        let cpu = process_cpu_nanos().unwrap_or(self.last_cpu);
        let wall_ns = now.duration_since(self.last_wall).as_nanos() as f64;
        let cpu_ns = cpu.saturating_sub(self.last_cpu) as f64;
        self.last_wall = now;
        self.last_cpu = cpu;
        if wall_ns <= 0.0 {
            0.0
        } else {
            cpu_ns / wall_ns * 100.0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mem_info_is_available() {
        let m = mem_info().expect("GetProcessMemoryInfo should work on any Windows");
        assert!(m.working_set > 0);
        assert!(m.peak_working_set >= m.working_set);
    }

    #[test]
    fn cpu_sampler_reports_busy_work() {
        let mut s = CpuSampler::start();
        let mut acc = 0u64;
        let spin_until = Instant::now() + std::time::Duration::from_millis(80);
        while Instant::now() < spin_until {
            acc = acc.wrapping_add(1);
        }
        std::hint::black_box(acc);
        let pct = s.sample();
        assert!(pct > 20.0, "busy loop should register CPU, got {pct}");
    }
}
