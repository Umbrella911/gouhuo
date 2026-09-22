// SPDX-License-Identifier: MPL-2.0

//! 服务端的自签名证书。
//!
//! # 为什么是自签名
//!
//! 自部署的服务端跑在公会成员家里的机器上、跑在没有域名的 IP 上、跑在内网里。
//! 让管理员去申请 CA 证书是不现实的，而「装完即用」也不允许多一步。
//!
//! 所以走自签名 + **证书固定**：邀请链接里带着证书指纹，客户端连上去比对。
//! 信任不是从 CA 来的，是从「发邀请链接给你的那个人」来的 —— 这跟现实一致：
//! 你信这个服务器，是因为你信拉你进来的那个人。
//!
//! # 证书必须持久化
//!
//! **重启服务端不能换证书。** 换了指纹就变了，之前发出去的所有邀请链接全部失效，
//! 而那些链接可能已经躺在几十个人的聊天记录里。
//!
//! 所以 [`ServerCert::load_or_create`] 是默认路径，随手 `generate()` 只该出现在
//! 测试里。这个约束在代码里不容易被察觉，所以写在这儿。
//!
//! # 有效期为什么给得很长
//!
//! CA 体系里有效期的作用是限制「私钥被盗但没人发现」的窗口。这里没有 CA，
//! 撤销的唯一手段就是换证书重发邀请链接 —— 有效期帮不上忙，却能制造一种
//! 新的故障：某天所有人突然连不上，管理员完全不知道为什么。
//!
//! 所以给足够长的有效期，并且客户端的校验器**不看有效期**（见 `pinning`）。

use std::io;
use std::path::Path;

use protocol::Fingerprint;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};

/// 证书里写的名字。客户端根本不看它（固定的是指纹），但 TLS 要求有。
const SUBJECT_NAME: &str = "kaimai";

/// 有效期。见模块文档：长是刻意的。
const VALID_YEARS: i32 = 50;

pub struct ServerCert {
    pub cert_der: Vec<u8>,
    pub key_der: Vec<u8>,
}

impl ServerCert {
    /// 新生成一张。**生产路径应该用 [`ServerCert::load_or_create`]** ——
    /// 每次重启换一张证书会让所有已发出的邀请链接失效。
    pub fn generate() -> io::Result<Self> {
        let mut params = rcgen::CertificateParams::new(vec![SUBJECT_NAME.to_string()])
            .map_err(|e| io::Error::other(format!("构造证书参数失败: {e}")))?;
        params.distinguished_name = rcgen::DistinguishedName::new();
        params
            .distinguished_name
            .push(rcgen::DnType::CommonName, SUBJECT_NAME);
        params.not_before = rcgen::date_time_ymd(2020, 1, 1);
        params.not_after = rcgen::date_time_ymd(2020 + VALID_YEARS, 1, 1);

        let key = rcgen::KeyPair::generate()
            .map_err(|e| io::Error::other(format!("生成密钥失败: {e}")))?;
        let cert = params
            .self_signed(&key)
            .map_err(|e| io::Error::other(format!("自签名失败: {e}")))?;

        Ok(Self {
            cert_der: cert.der().to_vec(),
            key_der: key.serialize_der(),
        })
    }

    /// 这张证书的指纹 —— 邀请链接里带的就是它。
    pub fn fingerprint(&self) -> Fingerprint {
        Fingerprint::of(&self.cert_der)
    }

    pub fn certificate_der(&self) -> CertificateDer<'static> {
        CertificateDer::from(self.cert_der.clone())
    }

    pub fn private_key_der(&self) -> PrivateKeyDer<'static> {
        PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(self.key_der.clone()))
    }

    /// 读证书；没有就生成一张存下去。
    ///
    /// 返回值里的 `bool` 表示**是不是新建的** —— 新建意味着指纹变了，
    /// 上层应该把新的邀请链接打出来提醒管理员重发。
    pub fn load_or_create(dir: &Path) -> io::Result<(Self, bool)> {
        let cert_path = dir.join("server.cert.der");
        let key_path = dir.join("server.key.der");

        match (std::fs::read(&cert_path), std::fs::read(&key_path)) {
            (Ok(cert_der), Ok(key_der)) => Ok((Self { cert_der, key_der }, false)),
            (Err(e), _) | (_, Err(e)) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => {
                let cert = Self::generate()?;
                std::fs::create_dir_all(dir)?;
                // 先写临时文件再改名：断电不会留下一张只有一半的证书，
                // 那会让服务端下次启动时既读不出来、又不知道该不该重新生成。
                write_atomic(&cert_path, &cert.cert_der)?;
                write_atomic(&key_path, &cert.key_der)?;
                Ok((cert, true))
            }
        }
    }
}

fn write_atomic(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = path.with_extension("tmp");
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

/// 私钥不该出现在任何日志里。
impl core::fmt::Debug for ServerCert {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "ServerCert({})", self.fingerprint())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(name: &str) -> std::path::PathBuf {
        let mut p = std::env::temp_dir();
        p.push(format!("kaimai-cert-{name}-{}", std::process::id()));
        p
    }

    #[test]
    fn generates_a_usable_cert() {
        let cert = ServerCert::generate().unwrap();
        assert!(!cert.cert_der.is_empty());
        assert!(!cert.key_der.is_empty());
        // 指纹必须是对 DER 算的 —— 客户端拿到的就是 DER
        assert_eq!(cert.fingerprint(), Fingerprint::of(&cert.cert_der));
    }

    #[test]
    fn each_generation_is_distinct() {
        let a = ServerCert::generate().unwrap();
        let b = ServerCert::generate().unwrap();
        assert_ne!(a.fingerprint(), b.fingerprint());
    }

    /// 最要紧的一条：重启不能换指纹，否则已发出的邀请链接全废。
    #[test]
    fn load_or_create_keeps_the_same_fingerprint() {
        let dir = temp_dir("stable");
        let _ = std::fs::remove_dir_all(&dir);

        let (first, created) = ServerCert::load_or_create(&dir).unwrap();
        assert!(created, "第一次必须是新建");
        let (second, created) = ServerCert::load_or_create(&dir).unwrap();
        assert!(!created, "第二次必须是读出来的");
        assert_eq!(
            first.fingerprint(),
            second.fingerprint(),
            "重启换了指纹 —— 所有邀请链接都会失效"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn debug_does_not_leak_the_private_key() {
        let cert = ServerCert::generate().unwrap();
        let rendered = format!("{cert:?}");
        let key_hex: String = cert.key_der.iter().map(|b| format!("{b:02x}")).collect();
        assert!(!rendered.contains(&key_hex));
        assert!(rendered.contains(&cert.fingerprint().to_grouped_hex()));
    }
}
