// SPDX-License-Identifier: MIT OR Apache-2.0
//! 打印几条邀请链接，看看实际长什么样。`cargo run -p protocol --example invite_demo`
use protocol::{Fingerprint, Invite};

fn main() {
    let cert = Fingerprint::of("某个服务端的自签名证书 DER".as_bytes());
    for (label, invite) in [
        (
            "公会服务器（域名）",
            Invite {
                host: "voice.gonghui.cn".into(),
                port: 64738,
                cert,
                code: None,
            },
        ),
        (
            "家里的机器（IP）",
            Invite {
                host: "203.0.113.42".into(),
                port: 64738,
                cert,
                code: None,
            },
        ),
        (
            "带邀请码",
            Invite {
                host: "voice.gonghui.cn".into(),
                port: 64738,
                cert,
                code: Some("winter2026".into()),
            },
        ),
    ] {
        let url = invite.to_url().unwrap();
        println!("{label}（{} 字符）", url.chars().count());
        println!("  {url}");
        println!("  纯文本形式：{}", invite.to_code().unwrap());
        println!();
    }
    println!("证书指纹给人核对的样子：");
    println!("  {cert}");
}
