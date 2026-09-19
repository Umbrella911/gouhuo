//! 语音包加密。**不自研密码学** —— 这里只是把 RustCrypto 的 ChaCha20-Poly1305
//! 按我们的包格式接上去。
//!
//! - 头部明文，同时作为 AAD：服务端要按 session 查频道转发，不解密负载。
//! - nonce = `session(4) || seq(4) || 0000`，全部由头部推出，不上线传。
//! - 就地加解密，每帧零分配（语音路径 50 次/秒 × N 人，分配器是能测出来的）。
//!
//! **nonce 唯一性**：同一把 key 下 `(session, seq)` 必须永不重复。
//! seq 是 u32，20 ms 帧下约 994 天回绕；真正的约束是**重连必须换 key 或换 session**。
//! 密钥协商走 Noise（snow crate）或从控制面 TLS 派生 —— 见 M3。

use chacha20poly1305::aead::{AeadInPlace, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Key, Nonce, Tag};

use crate::{ProtocolError, VoiceHeader, MAX_DATAGRAM, VOICE_HEADER_LEN};

/// Poly1305 认证标签长度。每包固定开销，算带宽时别忘了它。
pub const TAG_LEN: usize = 16;

pub struct VoiceCipher {
    aead: ChaCha20Poly1305,
}

impl VoiceCipher {
    pub fn new(key: &[u8; 32]) -> Self {
        Self {
            aead: ChaCha20Poly1305::new(Key::from_slice(key)),
        }
    }

    fn nonce(h: &VoiceHeader) -> Nonce {
        let mut n = [0u8; 12];
        n[0..4].copy_from_slice(&h.session.to_le_bytes());
        n[4..8].copy_from_slice(&h.seq.to_le_bytes());
        Nonce::from(n)
    }

    /// 打包并加密到 `out`（会先 clear）。布局：`头部(明文) || 密文 || tag`。
    pub fn seal(
        &self,
        h: VoiceHeader,
        payload: &[u8],
        out: &mut Vec<u8>,
    ) -> Result<(), ProtocolError> {
        if VOICE_HEADER_LEN + payload.len() + TAG_LEN > MAX_DATAGRAM {
            return Err(ProtocolError::TooLarge);
        }
        let hdr = h.to_bytes();
        out.clear();
        out.extend_from_slice(&hdr);
        out.extend_from_slice(payload);
        let tag = self
            .aead
            .encrypt_in_place_detached(&Self::nonce(&h), &hdr, &mut out[VOICE_HEADER_LEN..])
            .map_err(|_| ProtocolError::Crypto)?;
        out.extend_from_slice(&tag);
        Ok(())
    }

    /// 校验并解密到 `out`（会先 clear），返回头部。
    ///
    /// 校验失败一律丢包，不回错误给对端 —— 语音面不给攻击者任何 oracle。
    pub fn open(&self, packet: &[u8], out: &mut Vec<u8>) -> Result<VoiceHeader, ProtocolError> {
        let (h, rest) = VoiceHeader::parse(packet)?;
        if rest.len() < TAG_LEN {
            return Err(ProtocolError::Truncated);
        }
        let (ct, tag) = rest.split_at(rest.len() - TAG_LEN);
        out.clear();
        out.extend_from_slice(ct);
        self.aead
            .decrypt_in_place_detached(&Self::nonce(&h), &h.to_bytes(), out, Tag::from_slice(tag))
            .map_err(|_| ProtocolError::Crypto)?;
        Ok(h)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hdr(seq: u32) -> VoiceHeader {
        VoiceHeader {
            session: 42,
            seq,
            timestamp: seq * 960,
            flags: 0,
        }
    }

    #[test]
    fn seal_open_roundtrip() {
        let c = VoiceCipher::new(&[9u8; 32]);
        let payload: Vec<u8> = (0..80u8).collect();
        let mut wire = Vec::new();
        c.seal(hdr(1), &payload, &mut wire).unwrap();
        assert_eq!(wire.len(), VOICE_HEADER_LEN + payload.len() + TAG_LEN);

        let mut back = Vec::new();
        assert_eq!(c.open(&wire, &mut back).unwrap(), hdr(1));
        assert_eq!(back, payload);
    }

    #[test]
    fn tampered_header_is_rejected() {
        let c = VoiceCipher::new(&[9u8; 32]);
        let mut wire = Vec::new();
        c.seal(hdr(1), &[1, 2, 3, 4], &mut wire).unwrap();
        wire[0] ^= 0x01; // 改 session：冒名转发
        let mut back = Vec::new();
        assert_eq!(c.open(&wire, &mut back), Err(ProtocolError::Crypto));
    }

    #[test]
    fn tampered_payload_is_rejected() {
        let c = VoiceCipher::new(&[9u8; 32]);
        let mut wire = Vec::new();
        c.seal(hdr(1), &[1, 2, 3, 4], &mut wire).unwrap();
        let n = wire.len();
        wire[n - TAG_LEN - 1] ^= 0x80;
        let mut back = Vec::new();
        assert_eq!(c.open(&wire, &mut back), Err(ProtocolError::Crypto));
    }

    #[test]
    fn wrong_key_is_rejected() {
        let a = VoiceCipher::new(&[1u8; 32]);
        let b = VoiceCipher::new(&[2u8; 32]);
        let mut wire = Vec::new();
        a.seal(hdr(1), &[1, 2, 3, 4], &mut wire).unwrap();
        let mut back = Vec::new();
        assert_eq!(b.open(&wire, &mut back), Err(ProtocolError::Crypto));
    }
}
