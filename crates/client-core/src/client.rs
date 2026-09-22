// SPDX-License-Identifier: GPL-3.0-or-later

//! 一条到服务器的连接，以及界面用来驱动它的把手。

use std::net::TcpStream;
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use protocol::control::{
    server_message, Authenticate, Hello, JoinChannel, Ping, SelfState, ServerMessage, TextMessage,
    Welcome, PROTOCOL_VERSION,
};
use protocol::Invite;
use rustls::pki_types::ServerName;
use transport::{client_config, derive_voice_key, VoiceKey, DOWNSTREAM, UPSTREAM};
use voice_core::identity::Identity;

use crate::error::ConnectError;
use crate::roster::{ChatLine, Roster};
use crate::wire::{Reader, Wire};

/// 多久发一次心跳。
///
/// 服务端 30 秒收不到东西就踢人，所以这个值要有足够的余量 ——
/// 掉一两个包不能导致掉线。5 秒给了六次机会。
pub const HEARTBEAT: Duration = Duration::from_secs(5);

/// 建连接时等多久。
///
/// 比默认的 TCP 超时短得多：用户盯着「正在连接」这四个字，
/// 等 20 秒和等 8 秒是完全不同的体验，而地址不对的时候多等那 12 秒
/// 一点用都没有。
pub const CONNECT_TIMEOUT: Duration = Duration::from_secs(8);

/// 界面要知道的事。
#[derive(Debug, Clone, PartialEq)]
pub enum Event {
    /// 名单或频道树变了 —— 重画。
    RosterChanged,
    /// 有人进来了。界面据此播提示音、念 TTS。
    ///
    /// 名字直接带在这儿，而不是让界面去 [`Roster`] 里查 ——
    /// 收到「谁走了」的时候，那个人已经从名单里删掉了。
    Joined {
        session: u32,
        name: String,
    },
    Left {
        session: u32,
        name: String,
    },
    /// 收到一条文字。
    Text(ChatLine),
    /// 连接结束了。附带一句能直接显示给用户的说明。
    Disconnected(String),
}

/// 一条活着的连接。克隆出来的把手可以在任意线程上用。
#[derive(Clone)]
pub struct Client {
    wire: Arc<Mutex<Wire>>,
    roster: Arc<Mutex<Roster>>,
    session_id: u32,
    udp_port: u16,
    /// 邀请链接里写的服务器地址。语音要往这儿发 —— **不要用控制面那条
    /// TCP 连接的对端地址**：服务器可能在 NAT 后面，两者未必一样，
    /// 而邀请链接里的那个才是用户实际能连上的。
    host: String,
    voice: Arc<VoiceKeys>,
}

pub struct VoiceKeys {
    pub upstream: VoiceKey,
    pub downstream: VoiceKey,
}

impl Client {
    /// 按一条邀请链接连上去，走完认证。
    ///
    /// 这是用户实际经历的全部过程：粘一串东西进来，然后就在频道里了。
    pub fn connect(
        link: &str,
        identity: &Identity,
        desired_name: &str,
    ) -> Result<(Self, Receiver<Event>), ConnectError> {
        let invite = Invite::parse(link).map_err(ConnectError::BadInvite)?;
        Self::connect_to(&invite, identity, desired_name)
    }

    pub fn connect_to(
        invite: &Invite,
        identity: &Identity,
        desired_name: &str,
    ) -> Result<(Self, Receiver<Event>), ConnectError> {
        let sock = connect_tcp(&invite.host, invite.port)?;
        sock.set_nodelay(true)?;
        let read_sock = sock.try_clone()?;

        let config =
            Arc::new(client_config(invite.cert).map_err(|e| ConnectError::Tls(e.to_string()))?);
        // 服务端证书是自签的，主机名不参与判断（我们固定的是证书本身），
        // 所以这里放什么都行。放一个固定的常量，免得给人「换个名字就能绕过」的错觉。
        let name = ServerName::try_from("kaimai").expect("常量，不会失败");
        let mut conn = rustls::ClientConnection::new(config, name)
            .map_err(|e| ConnectError::Tls(e.to_string()))?;

        let mut handshake_sock = sock.try_clone()?;
        if let Err(e) = conn.complete_io(&mut handshake_sock) {
            let text = e.to_string();
            return Err(if text.contains("指纹对不上") {
                ConnectError::WrongCertificate(text)
            } else {
                ConnectError::Tls(text)
            });
        }

        // 语音密钥在这里就有了 —— 不另起握手，见 transport::derive_voice_key。
        let voice = VoiceKeys {
            upstream: derive_voice_key(&conn, UPSTREAM)
                .map_err(|e| ConnectError::Tls(e.to_string()))?,
            downstream: derive_voice_key(&conn, DOWNSTREAM)
                .map_err(|e| ConnectError::Tls(e.to_string()))?,
        };

        // 握手走完就把读超时撤掉。**留着它会吃数据** ——
        // 读超时跟到达的数据撞车时，Winsock 会丢掉已经读出来的那部分，
        // 表现为 TLS 流错位（见 server::conn 的模块文档）。
        sock.set_read_timeout(None)?;
        read_sock.set_read_timeout(None)?;

        let wire = Arc::new(Mutex::new(Wire { conn, sock }));
        let mut reader = Reader::new(read_sock, Arc::clone(&wire));

        let welcome = authenticate(
            &wire,
            &mut reader,
            identity,
            invite.code.as_deref().unwrap_or(""),
            desired_name,
        )?;

        let roster = Arc::new(Mutex::new(Roster::from_welcome(&welcome)));
        let client = Client {
            wire,
            roster,
            session_id: welcome.session_id,
            udp_port: welcome.udp_port as u16,
            host: invite.host.clone(),
            voice: Arc::new(voice),
        };

        let (tx, rx) = mpsc::channel();
        client.spawn_reader(reader, tx);
        client.spawn_heartbeat();
        Ok((client, rx))
    }

    pub fn session_id(&self) -> u32 {
        self.session_id
    }

    /// 语音要发到服务器的哪个 UDP 端口。
    pub fn udp_port(&self) -> u16 {
        self.udp_port
    }

    /// 服务器的地址，取自邀请链接。
    pub fn server_host(&self) -> &str {
        &self.host
    }

    pub fn voice_keys(&self) -> &VoiceKeys {
        &self.voice
    }

    /// 借出名单来画界面。**别在持有它的时候做慢事情** ——
    /// 读线程要拿同一把锁才能把新状态写进去。
    pub fn roster(&self) -> MutexGuard<'_, Roster> {
        self.roster.lock().expect("roster poisoned")
    }

    pub fn join_channel(&self, channel_id: u32) {
        self.send(&JoinChannel { channel_id }.into());
    }

    pub fn send_text(&self, body: &str) {
        let body = body.trim();
        if body.is_empty() {
            return;
        }
        // 频道、发送者、时间戳全由服务端盖章，这里填什么都会被覆盖。
        self.send(
            &TextMessage {
                channel_id: 0,
                sender_session_id: 0,
                body: body.to_string(),
                timestamp_ms: 0,
            }
            .into_client(),
        );
    }

    pub fn set_self_state(&self, self_muted: bool, self_deafened: bool) {
        self.send(
            &SelfState {
                self_muted,
                self_deafened,
            }
            .into(),
        );
    }

    /// 主动断开。读线程会因此醒过来并发出 [`Event::Disconnected`]。
    pub fn disconnect(&self) {
        if let Ok(wire) = self.wire.lock() {
            let _ = wire.sock.shutdown(std::net::Shutdown::Both);
        }
    }

    fn send(&self, message: &protocol::control::ClientMessage) {
        // 发不出去不用在这里处理：读线程马上就会发现连接断了，
        // 由它统一走善后流程。两个地方都报错只会让界面弹两次。
        if let Ok(mut wire) = self.wire.lock() {
            let _ = wire.send(message);
        }
    }

    fn spawn_reader(&self, mut reader: Reader, tx: Sender<Event>) {
        let roster = Arc::clone(&self.roster);
        std::thread::Builder::new()
            .name("kaimai-client-read".into())
            .spawn(move || {
                let reason = loop {
                    match reader.next::<ServerMessage>() {
                        Ok(Some(message)) => {
                            for event in apply(&roster, message) {
                                if tx.send(event).is_err() {
                                    // 界面没了，不用再读了
                                    return;
                                }
                            }
                        }
                        Ok(None) => break "服务器关闭了连接".to_string(),
                        Err(e) => break format!("连接断了：{e}"),
                    }
                };
                let _ = tx.send(Event::Disconnected(reason));
            })
            .expect("开不出读线程");
    }

    fn spawn_heartbeat(&self) {
        let client = self.clone();
        std::thread::Builder::new()
            .name("kaimai-client-ping".into())
            .spawn(move || loop {
                std::thread::sleep(HEARTBEAT);
                // 连接断了之后 send 是空操作，这个线程会自己空转到进程结束。
                // 为此专门加一条关闭通道不划算 —— 它每 5 秒醒一次。
                if Arc::strong_count(&client.wire) <= 1 {
                    return;
                }
                client.send(
                    &Ping {
                        timestamp: now_ms(),
                        udp_packets_received: 0,
                    }
                    .into(),
                );
            })
            .expect("开不出心跳线程");
    }
}

/// 把一条服务端消息应用到名单上，产出界面要知道的事。
fn apply(roster: &Arc<Mutex<Roster>>, message: ServerMessage) -> Vec<Event> {
    let mut roster = roster.lock().expect("roster poisoned");
    match message.payload {
        Some(server_message::Payload::UserState(state)) => {
            let Some(user) = state.user else {
                return Vec::new();
            };
            let session = user.session_id;
            let name = user.name.clone();
            let previous = roster.users.insert(session, user);
            match previous {
                // 第一次见到这个人 —— 但别把自己的登录当成「有人进来了」
                None if session != roster.me => {
                    vec![Event::Joined { session, name }, Event::RosterChanged]
                }
                _ => vec![Event::RosterChanged],
            }
        }
        Some(server_message::Payload::UserLeft(left)) => {
            let Some(user) = roster.users.remove(&left.session_id) else {
                return Vec::new();
            };
            vec![
                Event::Left {
                    session: left.session_id,
                    name: user.name,
                },
                Event::RosterChanged,
            ]
        }
        Some(server_message::Payload::ChannelState(state)) => {
            let Some(channel) = state.channel else {
                return Vec::new();
            };
            if state.removed {
                roster.channels.remove(&channel.id);
            } else {
                roster.channels.insert(channel.id, channel);
            }
            vec![Event::RosterChanged]
        }
        Some(server_message::Payload::TextMessage(text)) => {
            let line = ChatLine {
                sender_session: text.sender_session_id,
                sender_name: roster.name_of(text.sender_session_id),
                body: text.body,
                timestamp_ms: text.timestamp_ms,
            };
            roster.push_chat(line.clone());
            vec![Event::Text(line)]
        }
        // Pong 暂时不产生界面事件。接上 UDP 之后它会变成「语音通不通」的指示。
        Some(server_message::Payload::Pong(_)) => Vec::new(),
        // 认不出来的分支：新服务端发了我们不懂的东西。**忽略，不要断开。**
        _ => Vec::new(),
    }
}

fn connect_tcp(host: &str, port: u16) -> Result<TcpStream, ConnectError> {
    use std::net::ToSocketAddrs;

    let unreachable = |source: std::io::Error| ConnectError::Unreachable {
        host: host.to_string(),
        port,
        source,
    };

    let addrs: Vec<_> = (host, port)
        .to_socket_addrs()
        .map_err(unreachable)?
        .collect();
    if addrs.is_empty() {
        return Err(unreachable(std::io::Error::other("这个地址查不到")));
    }

    // 一个域名可能解析出好几个地址（IPv6 + IPv4）。挨个试，
    // 全都不通才算失败 —— 只试第一个的话，IPv6 没配好的机器会莫名其妙连不上。
    let mut last = None;
    for addr in addrs {
        match TcpStream::connect_timeout(&addr, CONNECT_TIMEOUT) {
            Ok(sock) => return Ok(sock),
            Err(e) => last = Some(e),
        }
    }
    Err(unreachable(last.expect("上面判过非空")))
}

fn authenticate(
    wire: &Arc<Mutex<Wire>>,
    reader: &mut Reader,
    identity: &Identity,
    invite_code: &str,
    desired_name: &str,
) -> Result<Welcome, ConnectError> {
    {
        let mut w = wire.lock().expect("wire poisoned");
        w.send(&protocol::control::ClientMessage::from(Hello {
            protocol_version: PROTOCOL_VERSION,
            client_version: crate::client_version(),
            public_key: identity.public_key().0.to_vec(),
        }))?;
    }

    let nonce = match reader.next::<ServerMessage>()?.and_then(|m| m.payload) {
        Some(server_message::Payload::Challenge(c)) => c.nonce,
        Some(server_message::Payload::Rejected(r)) => return Err(rejected(r)),
        // 对面回了个我们看不懂的东西 —— 多半根本不是开麦服务端
        _ => return Err(ConnectError::NotAKaimaiServer),
    };

    {
        let mut w = wire.lock().expect("wire poisoned");
        w.send(&protocol::control::ClientMessage::from(Authenticate {
            signature: identity.sign(&nonce).to_vec(),
            invite_code: invite_code.to_string(),
            desired_name: desired_name.to_string(),
        }))?;
    }

    match reader.next::<ServerMessage>()?.and_then(|m| m.payload) {
        Some(server_message::Payload::Welcome(w)) => Ok(w),
        Some(server_message::Payload::Rejected(r)) => Err(rejected(r)),
        _ => Err(ConnectError::NotAKaimaiServer),
    }
}

fn rejected(r: protocol::control::Rejected) -> ConnectError {
    ConnectError::Rejected {
        reason: protocol::control::rejected::Reason::try_from(r.reason)
            .unwrap_or(protocol::control::rejected::Reason::Unspecified),
        detail: r.detail,
    }
}

fn now_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn heartbeat_leaves_room_for_lost_packets() {
        // 服务端 30 秒不收东西就踢。心跳要密到掉几个包也不会掉线。
        let server_timeout = Duration::from_secs(30);
        assert!(
            HEARTBEAT * 5 < server_timeout,
            "心跳太稀了：掉几个包就会被踢下线"
        );
    }

    #[test]
    fn connect_timeout_is_short_enough_to_wait_for() {
        assert!(CONNECT_TIMEOUT.as_secs() <= 10, "用户会盯着这个时间干等");
        assert!(CONNECT_TIMEOUT.as_secs() >= 5, "太短会把慢网络误判成连不上");
    }
}
