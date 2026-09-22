// SPDX-License-Identifier: MPL-2.0
//! UDP socket 调优。
//!
//! 两件事，都是 M1 在**纯回环、零网络损伤**的链路上真被咬到之后才加的。
//!
//! # 一、不要用 `SO_RCVTIMEO` 来轮询退出标志
//!
//! Winsock 的读超时和正在到达的数据报撞车时，那个数据报会被整个丢掉。
//! 于是只要读超时的周期跟语音帧的间隔接近，两者就周期性地撞上：
//! 20 ms 读超时 + 20 ms 帧 = 稳定丢约 0.5%；同一份代码换成 10 ms 帧
//! （超时几乎不触发）= 一个不丢。这个丢包在任何抓包工具里都看不见，
//! 只会表现为「偶尔咔哒一下」，是最难查的那种 bug。
//!
//! 正确做法：收包线程**一直阻塞**在 `recv_from` 上，要它退出就往它自己的
//! 地址发一个哨兵包（[`WAKE_MAGIC`]）。
//!
//! # 二、默认接收缓冲太小
//!
//! Windows 上 UDP socket 的默认接收缓冲只有 8 KB。语音包一百来字节，
//! 看着绰绰有余 —— 直到接收线程被调度器晾一下，或者频道里七八个人同时说话
//! 包一起涌进来，缓冲就满了，内核**默默**丢包，不报任何错。
//!
//! 代价是每个 socket 多占几百 KB —— 相对 60 MB 的红线可以忽略，
//! 相对「用户听到咔哒声」更是可以忽略。

use std::io;
use std::net::{SocketAddr, UdpSocket};

/// 接收缓冲默认值。1 MB 够 20 个人各自积压几秒的语音包，
/// 比任何合理的调度延迟都宽裕。
pub const DEFAULT_RECV_BUFFER: usize = 1 << 20;

/// 唤醒哨兵包。真语音包最短也有 29 字节（13 字节头 + 16 字节 AEAD tag），
/// 跟这 8 个字节不可能混淆 —— 有测试盯着这条不变量。
pub const WAKE_MAGIC: [u8; 8] = [0x00, b'W', b'A', b'K', b'E', b'U', b'P', 0x00];

pub fn is_wake(buf: &[u8]) -> bool {
    buf == WAKE_MAGIC
}

/// 往 `addr` 发一个哨兵包，把阻塞在那个 socket 上的收包线程叫醒。
pub fn send_wake(addr: SocketAddr) -> io::Result<()> {
    let s = UdpSocket::bind("127.0.0.1:0")?;
    s.send_to(&WAKE_MAGIC, addr)?;
    Ok(())
}

#[cfg(windows)]
pub fn set_recv_buffer(sock: &UdpSocket, bytes: usize) -> io::Result<()> {
    use std::os::windows::io::AsRawSocket;
    use windows_sys::Win32::Networking::WinSock::{setsockopt, SOL_SOCKET, SO_RCVBUF};

    let value = bytes as i32;
    let rc = unsafe {
        setsockopt(
            sock.as_raw_socket() as usize,
            SOL_SOCKET,
            SO_RCVBUF,
            &value as *const i32 as *const u8,
            std::mem::size_of::<i32>() as i32,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(not(windows))]
pub fn set_recv_buffer(_sock: &UdpSocket, _bytes: usize) -> io::Result<()> {
    Ok(())
}

/// 绑定一个调好参数的语音 UDP socket。**不设读超时** —— 见模块文档第一条。
pub fn bind_voice_socket(addr: &str) -> io::Result<UdpSocket> {
    let sock = UdpSocket::bind(addr)?;
    set_recv_buffer(&sock, DEFAULT_RECV_BUFFER)?;
    Ok(sock)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn bind_voice_socket_works() {
        let s = bind_voice_socket("127.0.0.1:0").unwrap();
        assert!(s.local_addr().unwrap().port() > 0);
    }

    #[test]
    fn wake_magic_cannot_be_confused_with_a_voice_packet() {
        // 最短的真语音包 = 13 字节头 + 16 字节 AEAD tag。
        let shortest_voice_packet = protocol::VOICE_HEADER_LEN + 16;
        assert!(WAKE_MAGIC.len() < shortest_voice_packet);
        assert!(!is_wake(&vec![0u8; shortest_voice_packet]));
        assert!(is_wake(&WAKE_MAGIC));
    }

    #[test]
    fn send_wake_unblocks_a_blocking_receiver() {
        let sink = bind_voice_socket("127.0.0.1:0").unwrap();
        let addr = sink.local_addr().unwrap();
        let h = std::thread::spawn(move || {
            let mut buf = [0u8; 64];
            let (n, _) = sink.recv_from(&mut buf).unwrap();
            is_wake(&buf[..n])
        });
        std::thread::sleep(Duration::from_millis(30));
        send_wake(addr).unwrap();
        assert!(h.join().unwrap());
    }

    /// 缓冲太小的那一条：2000 个包一次灌进去，默认的 8 KB 连 100 个都存不下。
    #[test]
    fn large_recv_buffer_survives_a_burst() {
        let sink = bind_voice_socket("127.0.0.1:0").unwrap();
        sink.set_read_timeout(Some(Duration::from_millis(200)))
            .unwrap();
        let addr = sink.local_addr().unwrap();

        let tx = UdpSocket::bind("127.0.0.1:0").unwrap();
        let packet = [0u8; 120];
        let burst = 2_000;
        for _ in 0..burst {
            tx.send_to(&packet, addr).unwrap();
        }

        let mut buf = [0u8; 256];
        let mut got = 0;
        while got < burst && sink.recv_from(&mut buf).is_ok() {
            got += 1;
        }
        assert!(got as f64 > burst as f64 * 0.95, "只收到 {got}/{burst}");
    }

    /// 回归测试：读超时周期跟到包间隔同频时，Winsock 会吃掉数据报。
    ///
    /// 这个测试**故意重现 bug**，断言的是「用哨兵包的收法一个不丢」。
    /// 如果哪天有人为了图省事把 `set_read_timeout` 加回收包路径，这里会亮。
    #[test]
    fn blocking_recv_with_wake_loses_nothing_at_frame_cadence() {
        let sink = bind_voice_socket("127.0.0.1:0").unwrap();
        let addr = sink.local_addr().unwrap();

        let h = std::thread::spawn(move || {
            let mut buf = [0u8; 256];
            let mut got = 0u32;
            loop {
                let (n, _) = sink
                    .recv_from(&mut buf)
                    .expect("blocking recv never fails here");
                if is_wake(&buf[..n]) {
                    break;
                }
                got += 1;
            }
            got
        });

        let tx = UdpSocket::bind("127.0.0.1:0").unwrap();
        let sent = 300u32;
        let _timer = crate::clock::TimerResolutionGuard::acquire();
        let (mut ticker, _) = crate::clock::Ticker::start(Duration::from_millis(20));
        for _ in 0..sent {
            ticker.tick();
            tx.send_to(&[7u8; 120], addr).unwrap();
        }
        send_wake(addr).unwrap();

        assert_eq!(h.join().unwrap(), sent);
    }
}
