// SPDX-License-Identifier: GPL-3.0-or-later

//! 客户端的证书固定校验器。
//!
//! # 最容易写错的地方
//!
//! 网上大量的「跳过证书校验」示例长这样：实现 `ServerCertVerifier`，
//! **所有方法都返回 Ok**。把它改成「比对一下指纹」看起来就安全了 —— 并不是。
//!
//! 证书是**公开**的：任何连过这台服务器的人都拿得到它的 DER。如果只比指纹、
//! 不验签名，中间人只要把真证书原样转发过来，就能冒充服务端 ——
//! 他根本不需要私钥。
//!
//! TLS 的认证真正靠的是 `verify_tls12_signature` / `verify_tls13_signature`：
//! 服务端要用私钥签这次握手的内容，签不出来就没戏。所以这两个方法**必须**
//! 老老实实交给底层的密码库去验，绝不能返回 Ok 了事。
//!
//! 这个文件里两件事的分工是：
//! - 指纹比对回答「**是不是这一台**」（替代 CA 的信任链）
//! - 签名校验回答「**对面是不是真的持有那把私钥**」（替代不了，也省不掉）
//!
//! # 为什么不看有效期、不看主机名
//!
//! - **有效期**：这里没有 CA，撤销的唯一手段是换证书重发邀请链接。
//!   有效期帮不上忙，却能制造「某天所有人突然连不上」的故障。
//! - **主机名**：我们固定的是证书本身，比主机名强得多。而且服务器可能是
//!   裸 IP、动态域名、内网地址 —— 要求证书里的名字对得上只会徒增失败。

use std::sync::Arc;

use protocol::Fingerprint;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature, CryptoProvider};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, Error as TlsError, SignatureScheme};

/// 只认一张证书的校验器。
#[derive(Debug)]
pub struct PinnedServerCert {
    expected: Fingerprint,
    provider: Arc<CryptoProvider>,
}

impl PinnedServerCert {
    pub fn new(expected: Fingerprint, provider: Arc<CryptoProvider>) -> Self {
        Self { expected, provider }
    }

    pub fn expected(&self) -> Fingerprint {
        self.expected
    }
}

impl ServerCertVerifier for PinnedServerCert {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, TlsError> {
        // 中间证书一概不看：自签名没有链，链上多出来的东西只可能是干扰。
        let actual = Fingerprint::of(end_entity.as_ref());

        // 定长比较，不早退。指纹不是秘密，时序侧信道在这里价值很低，
        // 但这类比较写成短路的迟早会被抄到别的地方去。
        if constant_time_eq(&actual.0, &self.expected.0) {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(TlsError::General(format!(
                "服务器证书指纹对不上。邀请链接里写的是 {}，实际拿到的是 {}。\
                 要么邀请链接过期了（服务器换过证书），要么有人在中间。",
                self.expected, actual
            )))
        }
    }

    /// **必须真验。** 见模块文档：只比指纹不验签名等于没有认证。
    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    /// 同上。
    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, TlsError> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= x ^ y;
    }
    diff == 0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constant_time_eq_behaves_like_eq() {
        assert!(constant_time_eq(b"", b""));
        assert!(constant_time_eq(b"abcd", b"abcd"));
        assert!(!constant_time_eq(b"abcd", b"abce"));
        assert!(!constant_time_eq(b"abcd", b"abc"));
        // 第一个字节就不同，也不能提前返回（行为上看不出来，但保证了写法）
        assert!(!constant_time_eq(b"zbcd", b"abcd"));
    }

    #[test]
    fn verifier_accepts_the_pinned_cert_and_rejects_others() {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let cert = crate::ServerCert::generate().unwrap();
        let other = crate::ServerCert::generate().unwrap();

        let verifier = PinnedServerCert::new(cert.fingerprint(), provider);
        let name = ServerName::try_from("kaimai").unwrap();
        let now = UnixTime::since_unix_epoch(std::time::Duration::from_secs(1_800_000_000));

        assert!(verifier
            .verify_server_cert(&cert.certificate_der(), &[], &name, &[], now)
            .is_ok());

        let err = verifier
            .verify_server_cert(&other.certificate_der(), &[], &name, &[], now)
            .unwrap_err();
        let text = err.to_string();
        // 报错要说人话：用户看到这条得知道该干什么
        assert!(text.contains("指纹对不上"), "{text}");
        assert!(text.contains("有人在中间"), "{text}");
    }

    /// 过期的证书照样接受 —— 这是刻意的，见模块文档。
    #[test]
    fn expiry_is_deliberately_ignored() {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let cert = crate::ServerCert::generate().unwrap();
        let verifier = PinnedServerCert::new(cert.fingerprint(), provider);
        let name = ServerName::try_from("kaimai").unwrap();

        // 一个远在证书有效期之后的时间点
        let far_future = UnixTime::since_unix_epoch(std::time::Duration::from_secs(4_000_000_000));
        assert!(
            verifier
                .verify_server_cert(&cert.certificate_der(), &[], &name, &[], far_future)
                .is_ok(),
            "有效期不该参与判断：固定的是证书本身"
        );
    }

    /// 主机名对不上也照样接受 —— 服务器可能是裸 IP、内网地址、动态域名。
    #[test]
    fn server_name_is_deliberately_ignored() {
        let provider = Arc::new(rustls::crypto::ring::default_provider());
        let cert = crate::ServerCert::generate().unwrap();
        let verifier = PinnedServerCert::new(cert.fingerprint(), provider);
        let now = UnixTime::since_unix_epoch(std::time::Duration::from_secs(1_800_000_000));

        for host in ["192.168.1.7", "totally-different.example"] {
            let name = ServerName::try_from(host).unwrap();
            assert!(verifier
                .verify_server_cert(&cert.certificate_der(), &[], &name, &[], now)
                .is_ok());
        }
    }
}
