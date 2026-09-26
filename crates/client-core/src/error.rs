// SPDX-License-Identifier: GPL-3.0-or-later

//! 连不上的时候到底是为什么。
//!
//! 这个文件的每一条错误都会**原样显示给用户**，所以它们不是给程序员看的。
//! 痛点之一是「装完还要读文档」—— 连不上又只给一句 `Connection refused`，
//! 用户唯一能做的就是去问发链接给他的那个人，而那个人也不知道。
//!
//! 每条错误都要回答两件事：**发生了什么**，和**现在该做什么**。

use std::fmt;

#[derive(Debug)]
pub enum ConnectError {
    /// 邀请链接本身就不对。
    BadInvite(protocol::InviteError),
    /// TCP 都没连上。
    Unreachable {
        host: String,
        port: u16,
        source: std::io::Error,
    },
    /// 证书指纹对不上 —— 要么服务器换了证书，要么有人在中间。
    WrongCertificate(String),
    /// TLS 本身失败了（不是指纹的问题）。
    Tls(String),
    /// 服务端明确拒绝了。
    Rejected {
        reason: protocol::control::rejected::Reason,
        detail: String,
    },
    /// 连上了，但对面说的话我们听不懂 —— 大概率不是篝火服务端。
    NotAGouhuoServer,
    /// 连接中途断了。
    Io(std::io::Error),
}

impl ConnectError {
    /// 一句话说清楚出了什么事。
    pub fn headline(&self) -> String {
        use protocol::control::rejected::Reason;
        match self {
            ConnectError::BadInvite(_) => "这条邀请链接不对".into(),
            ConnectError::Unreachable { host, port, .. } => {
                format!("连不上 {host}:{port}")
            }
            ConnectError::WrongCertificate(_) => "服务器的身份对不上".into(),
            ConnectError::Tls(_) => "加密连接建不起来".into(),
            ConnectError::Rejected { reason, detail } => match reason {
                Reason::InviteRequired => "这个服务器要邀请码".into(),
                Reason::Full => "服务器满了".into(),
                Reason::Banned => "你被这个服务器封了".into(),
                Reason::VersionMismatch => "版本对不上".into(),
                Reason::BadSignature => "身份验证没通过".into(),
                // 服务端自己说了原因就用它的，别自作主张翻译
                _ if !detail.is_empty() => detail.clone(),
                _ => "服务器拒绝了这次连接".into(),
            },
            ConnectError::NotAGouhuoServer => "对面不是篝火服务器".into(),
            ConnectError::Io(_) => "连接断了".into(),
        }
    }

    /// 现在该做什么。界面上显示在 [`Self::headline`] 下面一行。
    pub fn advice(&self) -> String {
        use protocol::control::rejected::Reason;
        match self {
            ConnectError::BadInvite(_) => {
                "确认整条链接都复制全了 —— 中间断行或者少几个字符都会这样。\
                 如果是别人转发给你的，让他重新发一次原始链接。"
                    .into()
            }
            ConnectError::Unreachable { .. } => "服务器可能没开，或者端口没转发出来。\
                 如果你们在同一个局域网，让对方确认防火墙放行了；\
                 如果是走公网，确认路由器上 TCP 和 UDP 两个方向都转发了同一个端口。"
                .into(),
            ConnectError::WrongCertificate(detail) => {
                format!(
                    "{detail}\n\
                     最常见的原因是服务器重装过、或者数据目录丢了 —— \
                     那样它会生成新证书，所有老邀请链接都失效，得让管理员重发一条。\
                     如果管理员说没换过，就别连了。"
                )
            }
            ConnectError::Tls(detail) => {
                format!("{detail}\n对面可能不是篝火服务器，或者版本差太远。")
            }
            ConnectError::Rejected { reason, detail } => match reason {
                Reason::InviteRequired => {
                    "找管理员要一条带邀请码的链接。直接给 IP 和端口是进不来的。".into()
                }
                Reason::Full => "等会儿再试。人数上限是服务器自己设的。".into(),
                Reason::VersionMismatch => {
                    format!("{detail}\n升级客户端，或者让管理员升级服务端。")
                }
                Reason::BadSignature => "你的身份文件可能坏了。可以重新生成一个 —— \
                     但那等于换了个人，服务器上给你的权限要重新配。"
                    .into(),
                _ => detail.clone(),
            },
            ConnectError::NotAGouhuoServer => {
                "这个地址和端口上跑的是别的东西。确认一下链接有没有搞错。".into()
            }
            ConnectError::Io(_) => "网络断了或者服务器关了。过一会儿重连试试。".into(),
        }
    }
}

impl fmt::Display for ConnectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}\n{}", self.headline(), self.advice())
    }
}

impl std::error::Error for ConnectError {}

impl From<std::io::Error> for ConnectError {
    fn from(e: std::io::Error) -> Self {
        ConnectError::Io(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::control::rejected::Reason;

    /// 每一条错误都必须**既说发生了什么，又说该做什么**。
    ///
    /// 这条测试存在的意义是：将来加新的错误分支时，别只写一半。
    #[test]
    fn every_error_says_what_happened_and_what_to_do() {
        let cases = vec![
            ConnectError::BadInvite(protocol::InviteError::Truncated),
            ConnectError::Unreachable {
                host: "example.com".into(),
                port: 20800,
                source: std::io::Error::other("x"),
            },
            ConnectError::WrongCertificate("指纹对不上".into()),
            ConnectError::Tls("握手失败".into()),
            ConnectError::NotAGouhuoServer,
            ConnectError::Io(std::io::Error::other("x")),
        ];
        let rejections = [
            Reason::InviteRequired,
            Reason::Full,
            Reason::Banned,
            Reason::VersionMismatch,
            Reason::BadSignature,
            Reason::Internal,
        ];

        let all = cases
            .into_iter()
            .chain(rejections.into_iter().map(|reason| ConnectError::Rejected {
                reason,
                detail: "服务端给的说明".into(),
            }));

        for error in all {
            let headline = error.headline();
            let advice = error.advice();
            assert!(!headline.is_empty(), "{error:?} 没说发生了什么");
            assert!(!advice.is_empty(), "{error:?} 没说该怎么办");
            // 一句话就要说清楚，别把整段解释塞进标题
            assert!(
                headline.chars().count() < 40,
                "{error:?} 的标题太长了：{headline}"
            );
        }
    }

    /// 报错里不能出现只有程序员看得懂的东西。
    #[test]
    fn errors_do_not_leak_jargon() {
        let error = ConnectError::Unreachable {
            host: "127.0.0.1".into(),
            port: 20800,
            source: std::io::Error::from(std::io::ErrorKind::ConnectionRefused),
        };
        let text = error.to_string();
        for jargon in ["ConnectionRefused", "os error", "Err(", "rustls"] {
            assert!(!text.contains(jargon), "报错里漏出了 `{jargon}`：{text}");
        }
    }
}
