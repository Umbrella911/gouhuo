// SPDX-License-Identifier: GPL-3.0-or-later

//! 单独试麦：只开采集和播放，**完全不碰网络**。
//!
//! # 为什么不复用 `pipeline`
//!
//! [`crate::pipeline::Pipeline`] 要 session id、服务器地址、两把密钥 ——
//! 那些东西只有连上服务器之后才有。而「我的麦克风好不好使」这个问题，
//! **恰恰在连不上的时候最想知道**：是我麦克风坏了，还是服务器的问题？
//!
//! 硬凑的话就得给它一个假地址和假密钥，然后往一个死地址上发包。
//! 这里两个线程六十行，比那干净。
//!
//! # 跟 `pipeline` 里那套的关系
//!
//! 电平怎么算、试听队列怎么攒，两边是同一套规矩（见 [`crate::pipeline`]
//! 的模块文档）。两边都改的时候要一起改 —— 不一致的话，用户会发现
//! 「试麦时条子动，进了频道就不动了」。

use std::io;
use std::sync::atomic::{AtomicBool, AtomicI32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;

use crate::audio::{Capture, Render, FRAME_SAMPLES};
use crate::pipeline::AudioProcessor;

/// 试听队列最深攒几帧。跟 `pipeline` 里那个同理，见那边的注释。
const MONITOR_MAX_FRAMES: usize = 3;

const SILENT_DB_CENTI: i32 = -12_000;

struct Shared {
    input_centi_db: AtomicI32,
    monitoring: AtomicBool,
    monitor: Mutex<std::collections::VecDeque<Vec<f32>>>,
    stop: AtomicBool,
    /// 采集那边有没有真的读到过一帧。界面上要分清「设备还没开起来」
    /// 和「开起来了但是静音」—— 这两个的下一步完全不一样。
    running: AtomicBool,
    /// 设备出错时的说明，直接显示给用户。
    error: Mutex<Option<String>>,
}

/// 一次试麦。丢掉它就会把两个线程收干净、把设备还回去。
pub struct MicCheck {
    shared: Arc<Shared>,
    threads: Vec<JoinHandle<()>>,
}

impl MicCheck {
    /// 开始试麦。**立刻返回**，两个线程在后台跑。
    ///
    /// 设备打不开不在这里报错 —— 设备是在音频线程上打开的（COM 的线程亲和性，
    /// 见 `wasapi::live`），所以错误要从 [`error`](Self::error) 取。
    pub fn start(
        mut capture: Box<dyn Capture>,
        mut render: Box<dyn Render>,
        processor: Option<Box<dyn AudioProcessor>>,
    ) -> io::Result<Self> {
        let shared = Arc::new(Shared {
            input_centi_db: AtomicI32::new(SILENT_DB_CENTI),
            monitoring: AtomicBool::new(false),
            monitor: Mutex::new(std::collections::VecDeque::new()),
            stop: AtomicBool::new(false),
            running: AtomicBool::new(false),
            error: Mutex::new(None),
        });
        let processor: Option<Arc<dyn AudioProcessor>> = processor.map(Arc::from);
        let mut threads = Vec::new();

        {
            let shared = Arc::clone(&shared);
            let processor = processor.clone();
            threads.push(
                std::thread::Builder::new()
                    .name("kaimai-miccheck-in".into())
                    .spawn(move || {
                        let mut frame = vec![0.0f32; FRAME_SAMPLES];
                        while !shared.stop.load(Ordering::Relaxed) {
                            match capture.read(&mut frame) {
                                Ok(true) => {}
                                Ok(false) => break,
                                Err(e) => {
                                    shared.fail(e);
                                    return;
                                }
                            }
                            shared.running.store(true, Ordering::Relaxed);
                            if let Some(processor) = &processor {
                                processor.process_capture(&mut frame);
                            }
                            shared.input_centi_db.store(
                                (crate::pipeline::frame_db(&frame) * 100.0) as i32,
                                Ordering::Relaxed,
                            );
                            if shared.monitoring.load(Ordering::Relaxed) {
                                let mut queue = shared.monitor.lock().expect("monitor poisoned");
                                if queue.len() >= MONITOR_MAX_FRAMES {
                                    queue.pop_front();
                                }
                                queue.push_back(frame.clone());
                            }
                        }
                    })?,
            );
        }

        {
            let shared = Arc::clone(&shared);
            threads.push(
                std::thread::Builder::new()
                    .name("kaimai-miccheck-out".into())
                    .spawn(move || {
                        let mut out = vec![0.0f32; FRAME_SAMPLES];
                        while !shared.stop.load(Ordering::Relaxed) {
                            out.fill(0.0);
                            if let Some(mine) =
                                shared.monitor.lock().expect("monitor poisoned").pop_front()
                            {
                                out.copy_from_slice(&mine);
                            }
                            // **没开试听的时候也要照常播静音。**
                            // 播放线程的节拍来自设备（write 会阻塞），
                            // 不转的话打开试听时要先等设备预热，听起来像卡了一下。
                            if let Err(e) = render.write(&out) {
                                shared.fail(e);
                                return;
                            }
                        }
                    })?,
            );
        }

        Ok(Self { shared, threads })
    }

    /// 麦克风当前电平，分贝（满刻度 0 dB）。
    pub fn input_db(&self) -> f32 {
        self.shared.input_centi_db.load(Ordering::Relaxed) as f32 / 100.0
    }

    /// 试听。**用扬声器开会啸叫**，界面上要写清楚戴耳机。
    pub fn set_monitoring(&self, on: bool) {
        self.shared.monitoring.store(on, Ordering::Relaxed);
        if !on {
            self.shared
                .monitor
                .lock()
                .expect("monitor poisoned")
                .clear();
        }
    }

    pub fn is_monitoring(&self) -> bool {
        self.shared.monitoring.load(Ordering::Relaxed)
    }

    /// 采集那边真的转起来了没有。
    pub fn is_running(&self) -> bool {
        self.shared.running.load(Ordering::Relaxed)
    }

    /// 设备出了什么问题，能直接显示给用户。
    pub fn error(&self) -> Option<String> {
        self.shared.error.lock().expect("error poisoned").clone()
    }
}

impl Shared {
    fn fail(&self, e: io::Error) {
        *self.error.lock().expect("error poisoned") = Some(e.to_string());
        // 一边出错另一边也没必要转下去了：它们共用一副耳机。
        self.stop.store(true, Ordering::Relaxed);
    }
}

impl Drop for MicCheck {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::Relaxed);
        // 两个线程都阻塞在设备上，最多一个设备周期（10 ms 上下）就会醒。
        // 不需要额外的叫醒机制 —— 这也是「让设备当时钟」的附带好处。
        for thread in self.threads.drain(..) {
            let _ = thread.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::audio::{CollectingRender, SyntheticCapture};
    use std::time::Duration;

    fn tone(frames: usize) -> Vec<f32> {
        (0..FRAME_SAMPLES * frames)
            .map(|i| 0.2 * (i as f32 * 0.05).sin())
            .collect()
    }

    #[test]
    fn the_level_follows_the_microphone() {
        let mut source = vec![0.0f32; FRAME_SAMPLES * 30];
        source.extend_from_slice(&tone(60));

        let check = MicCheck::start(
            Box::new(SyntheticCapture::new(source).then_silence()),
            Box::new(crate::audio::NullRender::default()),
            None,
        )
        .unwrap();

        std::thread::sleep(Duration::from_millis(200));
        let silent = check.input_db();
        std::thread::sleep(Duration::from_millis(500));
        let loud = check.input_db();

        assert!(silent < -90.0, "静音时该贴底，实际 {silent:.1} dB");
        assert!(
            loud > silent + 40.0,
            "有声音时该抬起来：{silent:.1} -> {loud:.1}"
        );
    }

    #[test]
    fn monitoring_routes_the_mic_to_the_speakers() {
        let (render, played) = CollectingRender::new();
        let check = MicCheck::start(
            Box::new(SyntheticCapture::new(tone(100)).then_silence()),
            Box::new(render),
            None,
        )
        .unwrap();

        // 没开试听：播出去的必须是纯静音
        std::thread::sleep(Duration::from_millis(300));
        let quiet: f32 = played.lock().unwrap().iter().map(|s| s * s).sum();
        assert_eq!(quiet, 0.0, "没开试听却有声音");

        check.set_monitoring(true);
        std::thread::sleep(Duration::from_millis(300));
        let loud: f32 = played.lock().unwrap().iter().map(|s| s * s).sum();
        assert!(loud > 0.0, "开了试听还是没声音");
    }

    /// 丢掉就该把线程收干净，而且不能挂住。
    #[test]
    fn dropping_stops_promptly() {
        let check = MicCheck::start(
            Box::new(SyntheticCapture::new(tone(1000)).then_silence()),
            Box::new(crate::audio::NullRender::default()),
            None,
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(100));

        let start = std::time::Instant::now();
        drop(check);
        assert!(
            start.elapsed() < Duration::from_millis(500),
            "关试麦花了 {:?}",
            start.elapsed()
        );
    }

    /// 设备中途出错要能报出来，而且两个线程都要停。
    #[test]
    fn a_device_error_is_reported() {
        struct Broken;
        impl Capture for Broken {
            fn read(&mut self, _out: &mut [f32]) -> io::Result<bool> {
                Err(io::Error::other("麦克风拔了"))
            }
        }

        let check = MicCheck::start(
            Box::new(Broken),
            Box::new(crate::audio::NullRender::default()),
            None,
        )
        .unwrap();
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(check.error().as_deref(), Some("麦克风拔了"));
        assert!(!check.is_running(), "出错了还报在跑");
    }
}
