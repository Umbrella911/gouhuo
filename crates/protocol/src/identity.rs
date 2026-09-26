// SPDX-License-Identifier: MIT OR Apache-2.0

//! 身份与指纹。
//!
//! # 身份是一对 Ed25519 密钥，不是用户名密码
//!
//! 用户的身份就是一对本地生成的 Ed25519 密钥，公钥即身份。没有注册、没有密码、
//! 没有找回流程，也没有一个中心服务器知道你是谁 —— 这跟「开源、可自部署」是一回事。
//!
//! 代价是换机器要带着私钥走，所以**一键导出导入是必需功能不是附加功能**
//! （这正是要修的第三个痛点：身份是本地文件换机就没了）。私钥的生成和存储
//! 在 `voice-core` 里；这个 crate 只定义「线上和界面上长什么样」。
//!
//! # 指纹为什么截断到 16 字节
//!
//! 自部署的服务端不会有 CA 签的证书，只能自签名。那 TLS 就必须靠**证书固定**
//! 来防中间人 —— 邀请链接里带上服务端证书的指纹，客户端连上去比对。
//!
//! 攻击者要冒充服务端，就得造一个证书让它的指纹跟目标一样，这是**第二原像攻击**，
//! 截断到 128 位仍然是 2^128 的工作量。碰撞攻击只有 2^64，但碰撞在这里没用：
//! 指纹是服务器管理员先定下来的，攻击者没法让双方都用他造的那一对。
//!
//! 换来的是邀请链接短一截 —— 全长 SHA-256 会让链接多出 26 个字符。

use crate::base32;

/// 截断的 SHA-256 指纹。用于服务端证书，也用于给用户看的公钥摘要。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Fingerprint(pub [u8; Fingerprint::LEN]);

impl Fingerprint {
    pub const LEN: usize = 16;

    /// 对任意字节算指纹。服务端证书传 DER，公钥传 32 字节原始公钥。
    pub fn of(bytes: &[u8]) -> Self {
        use sha2::{Digest, Sha256};
        let digest = Sha256::digest(bytes);
        let mut out = [0u8; Self::LEN];
        out.copy_from_slice(&digest[..Self::LEN]);
        Self(out)
    }

    /// 给人看的形式：小写十六进制，每 4 个字符一组用连字符隔开。
    ///
    /// 分组是为了让人能在两个屏幕之间逐段核对 —— 一长串不分组的十六进制，
    /// 人会看两眼开头两眼结尾就说「一样」，中间根本不看。
    pub fn to_grouped_hex(self) -> String {
        let mut out = String::with_capacity(Self::LEN * 2 + Self::LEN / 2);
        for (i, byte) in self.0.iter().enumerate() {
            if i > 0 && i % 2 == 0 {
                out.push('-');
            }
            out.push_str(&format!("{byte:02x}"));
        }
        out
    }
}

impl core::fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(&self.to_grouped_hex())
    }
}

/// Ed25519 公钥 —— 用户身份本身。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct PublicKey(pub [u8; PublicKey::LEN]);

impl PublicKey {
    pub const LEN: usize = 32;

    pub fn fingerprint(&self) -> Fingerprint {
        Fingerprint::of(&self.0)
    }

    /// 给人看/复制粘贴的短形式（base32，小写无填充）。
    ///
    /// 用在「把我的身份发给管理员加白名单」这种场景。不是给机器解析用的 ——
    /// 线上传的一律是 32 字节原始公钥。
    pub fn to_text(self) -> String {
        base32::encode(&self.0)
    }

    pub fn from_text(text: &str) -> Option<Self> {
        let bytes = base32::decode(text).ok()?;
        Some(Self(bytes.try_into().ok()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_is_stable_and_truncated() {
        let a = Fingerprint::of(b"hello");
        let b = Fingerprint::of(b"hello");
        assert_eq!(a, b, "同样的输入必须给同样的指纹");
        assert_ne!(a, Fingerprint::of(b"hellp"), "改一个字节就该完全不同");
        assert_eq!(a.0.len(), 16);
    }

    #[test]
    fn fingerprint_matches_sha256_prefix() {
        use sha2::{Digest, Sha256};
        let data = b"gouhuo server certificate";
        let full = Sha256::digest(data);
        assert_eq!(Fingerprint::of(data).0[..], full[..16]);
    }

    #[test]
    fn grouped_hex_is_readable() {
        let fp = Fingerprint([0xab; 16]);
        let text = fp.to_grouped_hex();
        assert_eq!(text, "abab-abab-abab-abab-abab-abab-abab-abab");
        // 8 组，每组 4 个十六进制字符
        assert_eq!(text.split('-').count(), 8);
        assert!(text.split('-').all(|g| g.len() == 4));
    }

    #[test]
    fn public_key_text_roundtrips() {
        let key = PublicKey([7u8; 32]);
        let text = key.to_text();
        assert_eq!(PublicKey::from_text(&text), Some(key));
        // 用户会大写、会加连字符，都得认
        assert_eq!(PublicKey::from_text(&text.to_uppercase()), Some(key));
    }

    #[test]
    fn public_key_text_rejects_wrong_length() {
        assert_eq!(PublicKey::from_text(""), None);
        assert_eq!(PublicKey::from_text(&base32::encode(&[1u8; 31])), None);
        assert_eq!(PublicKey::from_text(&base32::encode(&[1u8; 33])), None);
        assert_eq!(PublicKey::from_text("这不是 base32"), None);
    }
}
