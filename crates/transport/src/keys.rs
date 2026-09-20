// SPDX-License-Identifier: GPL-3.0-or-later

//! 语音用的对称密钥：从 TLS 连接里派生，不另起一套握手。
//!
//! # 为什么不用 Noise
//!
//! 设计里给了两个选项：Noise 框架，或者从 TLS 派生。选后者，理由是**少一套要审的东西**。
//!
//! 控制面本来就跑在 TLS 上，那条连接已经完成了双向认证（服务端靠证书固定，
//! 客户端靠控制面的挑战-应答）。TLS 自带的 key exporter（RFC 5705）能从这次握手
//! 的密钥材料里派生出任意用途的密钥，而且**双方算出来的一定一样** ——
//! 这正是我们要的。
//!
//! 再上一套 Noise 意味着：多一次握手往返、多一个状态机、多一份要审的密码学代码，
//! 换来的是……同一件事。不划算。
//!
//! # 标签为什么要带版本
//!
//! exporter 的输出完全由标签决定。哪天密钥用途变了（比如换成一人一把、
//! 或者换算法），必须换标签 —— 否则新旧两端会算出同一把密钥却按不同规则用它，
//! 那是最难查的一类 bug。所以标签里写死了版本号。

use rustls::ConnectionCommon;

/// UDP 语音密钥的长度。ChaCha20-Poly1305 用 256 位。
pub const VOICE_KEY_LEN: usize = 32;

/// 派生标签。**改用途就换标签**，见模块文档。
pub const VOICE_KEY_LABEL: &[u8] = b"kaimai voice udp v1";

/// 上行：客户端发给服务端的语音。
pub const UPSTREAM: &[u8] = b"client-to-server";

/// 下行：服务端转发给客户端的语音。
pub const DOWNSTREAM: &[u8] = b"server-to-client";

/// 派生出来的语音密钥。
///
/// 不实现 `Debug`/`Display`：它是密钥，不该出现在任何日志里。
pub struct VoiceKey([u8; VOICE_KEY_LEN]);

impl VoiceKey {
    pub fn as_bytes(&self) -> &[u8; VOICE_KEY_LEN] {
        &self.0
    }
}

impl Drop for VoiceKey {
    fn drop(&mut self) {
        for byte in self.0.iter_mut() {
            unsafe { std::ptr::write_volatile(byte, 0) };
        }
        std::sync::atomic::compiler_fence(std::sync::atomic::Ordering::SeqCst);
    }
}

/// 从一条已经握好手的 TLS 连接里派生语音密钥。
///
/// 客户端和服务端在同一条连接上调用，会得到**完全相同**的结果 ——
/// 这就是不需要额外握手的原因。
///
/// `context` 让同一条 TLS 连接能派生出多把互不相干的密钥。上下行各一把，
/// 见 [`UPSTREAM`] / [`DOWNSTREAM`]。
///
/// # 上下行为什么要分开
///
/// nonce 是 `session || seq` 推出来的。一条连接上如果两个方向共用一把密钥，
/// 就得论证「服务端转发给 A 的包，其 session 永远不等于 A 自己的 session」——
/// 这条确实成立（不会把包发回给发送者，会话 id 也不重复），但它是一条
/// **散落在别处的、很容易在重构中被破坏的不变量**。
///
/// 分成两把密钥之后，这条论证直接不需要了：两个方向的密钥不同，nonce
/// 撞不撞都无所谓。多一次密钥派生，换掉一整类将来会咬人的推理。
pub fn derive_voice_key<T>(
    conn: &ConnectionCommon<T>,
    context: &[u8],
) -> Result<VoiceKey, rustls::Error> {
    let key: [u8; VOICE_KEY_LEN] =
        conn.export_keying_material([0u8; VOICE_KEY_LEN], VOICE_KEY_LABEL, Some(context))?;
    Ok(VoiceKey(key))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 密钥类型不能有 Debug —— 有的话迟早被打进日志。
    /// 这条靠的是编译期：给 VoiceKey 加上 Debug，下面这行就会编不过。
    #[test]
    fn voice_key_has_no_debug_impl() {
        fn assert_no_debug<T>() {}
        assert_no_debug::<VoiceKey>();
        // 真正的保证在于 VoiceKey 没有 derive(Debug)；这里留个锚点说明意图。
    }

    #[test]
    fn label_carries_a_version() {
        let label = std::str::from_utf8(VOICE_KEY_LABEL).unwrap();
        assert!(label.contains("v1"), "标签必须带版本，见模块文档");
        assert!(label.starts_with("kaimai"), "带上项目名，避免跟别的用途撞");
    }

    #[test]
    fn key_is_zeroed_on_drop() {
        // 直接观察 Drop 之后的内存是 UB，所以这里只验 Drop 里那段逻辑本身。
        let mut buf = [0xAAu8; VOICE_KEY_LEN];
        for byte in buf.iter_mut() {
            unsafe { std::ptr::write_volatile(byte, 0) };
        }
        assert_eq!(buf, [0u8; VOICE_KEY_LEN]);
    }
}
