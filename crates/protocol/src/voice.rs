// SPDX-License-Identifier: MIT OR Apache-2.0

//! 语音包线上格式。
//!
//! ```text
//! 0               4               8              12  13
//! +---------------+---------------+---------------+---+--------------------+
//! |    session    |      seq      |   timestamp   |fl |   Opus payload     |
//! +---------------+---------------+---------------+---+--------------------+
//!   u32 LE          u32 LE          u32 LE          u8
//! ```
//!
//! 头部 13 字节，明文，同时作为 AEAD 的 AAD（服务端要按 session 查频道转发，
//! 不能也不该解密负载）。
//!
//! 13 字节不是随便定的，它直接吃带宽红线：50 包/秒时头部本身就是
//! `(13 + 28) * 8 * 50 = 16.4 kbps`。10 ms 帧会翻倍到 32.8 kbps，
//! 说话带宽 40 kbps 的红线当场就没了 —— 见 latency-probe 的带宽报告。

use core::fmt;

/// 头部字节数。
pub const VOICE_HEADER_LEN: usize = 13;

/// 本次说话结束（talk spurt terminator）。收到后抖动缓冲可以立刻收尾，
/// 不必等到欠载才发现对面不说了。
pub const FLAG_TERMINATOR: u8 = 1 << 0;

/// 该帧由 DTX 生成（舒适噪声），不计入「正在说话」指示。
pub const FLAG_DTX: u8 = 1 << 1;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VoiceHeader {
    /// 说话者的会话 id，由服务端在登录时分配。客户端发出时填自己的，
    /// 服务端转发时**重写**为真实来源，杜绝冒名。
    pub session: u32,
    /// 帧序号，每帧 +1。抖动缓冲靠它排序和发现丢包。
    pub seq: u32,
    /// 48 kHz 采样时钟，本帧第一个采样点。用于跨帧长变化时对齐。
    pub timestamp: u32,
    pub flags: u8,
}

impl VoiceHeader {
    pub fn to_bytes(self) -> [u8; VOICE_HEADER_LEN] {
        let mut b = [0u8; VOICE_HEADER_LEN];
        b[0..4].copy_from_slice(&self.session.to_le_bytes());
        b[4..8].copy_from_slice(&self.seq.to_le_bytes());
        b[8..12].copy_from_slice(&self.timestamp.to_le_bytes());
        b[12] = self.flags;
        b
    }

    /// 解出头部，返回剩余负载。
    pub fn parse(buf: &[u8]) -> Result<(Self, &[u8]), ProtocolError> {
        if buf.len() < VOICE_HEADER_LEN {
            return Err(ProtocolError::Truncated);
        }
        let h = VoiceHeader {
            session: u32::from_le_bytes(buf[0..4].try_into().unwrap()),
            seq: u32::from_le_bytes(buf[4..8].try_into().unwrap()),
            timestamp: u32::from_le_bytes(buf[8..12].try_into().unwrap()),
            flags: buf[12],
        };
        Ok((h, &buf[VOICE_HEADER_LEN..]))
    }

    pub fn is_terminator(self) -> bool {
        self.flags & FLAG_TERMINATOR != 0
    }
}

/// 明文打包（仅用于本地测量和 UDP 不通时的调试路径）。生产路径走 [`crate::VoiceCipher`]。
pub fn encode_voice(
    h: VoiceHeader,
    payload: &[u8],
    out: &mut Vec<u8>,
) -> Result<(), ProtocolError> {
    if VOICE_HEADER_LEN + payload.len() > MAX_DATAGRAM {
        return Err(ProtocolError::TooLarge);
    }
    out.clear();
    out.extend_from_slice(&h.to_bytes());
    out.extend_from_slice(payload);
    Ok(())
}

use crate::MAX_DATAGRAM;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProtocolError {
    /// 包比头部还短。
    Truncated,
    /// 超过 [`MAX_DATAGRAM`]。
    TooLarge,
    /// AEAD 校验失败：伪造、篡改，或者密钥不对。直接丢，不要重试。
    Crypto,
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ProtocolError::Truncated => f.write_str("voice packet truncated"),
            ProtocolError::TooLarge => f.write_str("voice packet exceeds MAX_DATAGRAM"),
            ProtocolError::Crypto => f.write_str("voice packet failed authentication"),
        }
    }
}

impl std::error::Error for ProtocolError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrip() {
        let h = VoiceHeader {
            session: 7,
            seq: 12345,
            timestamp: 960 * 12345,
            flags: FLAG_TERMINATOR,
        };
        let mut buf = Vec::new();
        encode_voice(h, &[1, 2, 3], &mut buf).unwrap();
        assert_eq!(buf.len(), VOICE_HEADER_LEN + 3);
        let (got, payload) = VoiceHeader::parse(&buf).unwrap();
        assert_eq!(got, h);
        assert_eq!(payload, &[1, 2, 3]);
        assert!(got.is_terminator());
    }

    #[test]
    fn rejects_truncated() {
        assert_eq!(
            VoiceHeader::parse(&[0u8; 12]),
            Err(ProtocolError::Truncated)
        );
    }

    #[test]
    fn rejects_oversize() {
        let mut buf = Vec::new();
        let h = VoiceHeader {
            session: 0,
            seq: 0,
            timestamp: 0,
            flags: 0,
        };
        assert_eq!(
            encode_voice(h, &vec![0u8; MAX_DATAGRAM], &mut buf),
            Err(ProtocolError::TooLarge)
        );
    }
}
