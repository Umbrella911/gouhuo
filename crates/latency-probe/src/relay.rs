// SPDX-License-Identifier: GPL-3.0-or-later
//! 最小 UDP 转发器 —— 也就是服务端语音面的全部核心。
//!
//! 这就是「音频-only 的所谓 SFU」：收包、查频道、转发。不解码，不转码，
//! 不碰负载。放在这里是为了让测出来的延迟包含真实的服务端一跳，
//! 而不是客户端直连的假数字。
//!
//! 真服务端比这里多的只有：按 session 查频道成员、重写头部里的 session
//! （杜绝冒名）、限速。都是查表和拷贝，不改变延迟量级 —— M1 要验证的正是这一点。

use std::io;
use std::net::SocketAddr;
use std::thread::JoinHandle;
use std::time::Instant;

use voice_core::metrics::Histogram;

pub struct RelayStats {
    pub forwarded: u64,
    pub bytes: u64,
    /// 单包在转发器里待的时间（微秒）。这个数如果不是个位数微秒，
    /// 说明转发路径上有不该有的东西。
    pub hold_us: Histogram,
}

pub struct Relay {
    addr: SocketAddr,
    join: JoinHandle<RelayStats>,
}

impl Relay {
    /// 客户端应该把包发到这个地址。
    pub fn addr(&self) -> SocketAddr {
        self.addr
    }

    pub fn shutdown(self) -> RelayStats {
        // 发哨兵包唤醒，不用读超时 —— 读超时会吃掉真数据报，见 voice_core::net。
        let _ = voice_core::net::send_wake(self.addr);
        self.join.join().expect("relay thread panicked")
    }
}

pub fn spawn(dest: SocketAddr) -> io::Result<Relay> {
    let sock = voice_core::net::bind_voice_socket("127.0.0.1:0")?;
    let addr = sock.local_addr()?;

    let join = std::thread::Builder::new()
        .name("relay".into())
        .spawn(move || {
            voice_core::clock::boost_current_thread();
            let mut buf = [0u8; protocol::MAX_DATAGRAM];
            let mut stats = RelayStats {
                forwarded: 0,
                bytes: 0,
                hold_us: Histogram::with_capacity(4096),
            };
            while let Ok((len, _from)) = sock.recv_from(&mut buf) {
                if voice_core::net::is_wake(&buf[..len]) {
                    break;
                }
                let t = Instant::now();
                if sock.send_to(&buf[..len], dest).is_ok() {
                    stats.forwarded += 1;
                    stats.bytes += len as u64;
                }
                stats.hold_us.push(t.elapsed().as_secs_f64() * 1e6);
            }
            stats
        })?;

    Ok(Relay { addr, join })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::UdpSocket;
    use std::time::Duration;

    fn bind_probe_socket() -> UdpSocket {
        voice_core::net::bind_voice_socket("127.0.0.1:0").unwrap()
    }

    #[test]
    fn forwards_payload_unchanged() {
        let sink = bind_probe_socket();
        sink.set_read_timeout(Some(Duration::from_millis(500)))
            .unwrap();
        let relay = spawn(sink.local_addr().unwrap()).unwrap();

        let client = UdpSocket::bind("127.0.0.1:0").unwrap();
        let payload: Vec<u8> = (0..200u8).collect();
        client.send_to(&payload, relay.addr()).unwrap();

        let mut buf = [0u8; 1200];
        let (len, _) = sink.recv_from(&mut buf).expect("relay should forward");
        assert_eq!(&buf[..len], &payload[..]);

        let stats = relay.shutdown();
        assert_eq!(stats.forwarded, 1);
        assert_eq!(stats.bytes, payload.len() as u64);
    }

    #[test]
    fn does_not_forward_the_wake_sentinel() {
        let sink = bind_probe_socket();
        sink.set_read_timeout(Some(Duration::from_millis(150)))
            .unwrap();
        let relay = spawn(sink.local_addr().unwrap()).unwrap();
        let stats = relay.shutdown();
        assert_eq!(stats.forwarded, 0);
        let mut buf = [0u8; 64];
        assert!(sink.recv_from(&mut buf).is_err(), "哨兵包不该被转发出去");
    }

    #[test]
    fn shuts_down_without_traffic() {
        let sink = bind_probe_socket();
        let relay = spawn(sink.local_addr().unwrap()).unwrap();
        let stats = relay.shutdown();
        assert_eq!(stats.forwarded, 0);
    }
}
