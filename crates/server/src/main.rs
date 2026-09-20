// SPDX-License-Identifier: GPL-3.0-or-later

//! 开麦服务端。
//!
//! # 开箱即用
//!
//! 痛点之一是「装完还要读文档」。所以这个二进制**不带配置文件也能跑**：
//! 第一次启动自己生成证书和邀请码，然后把一条邀请链接打在屏幕上 ——
//! 把那一行发给朋友，就完事了。
//!
//! 要调的东西全走环境变量，没有配置文件格式要学：
//!
//! | 变量 | 作用 | 默认 |
//! |---|---|---|
//! | `KAIMAI_PORT` | 监听端口 | `49737` |
//! | `KAIMAI_DATA` | 数据目录（证书、邀请码） | `./kaimai-data` |
//! | `KAIMAI_HOST` | 写进邀请链接的地址 | 自动探测的局域网地址 |
//! | `KAIMAI_INVITE` | 邀请码；设成空串表示不要邀请码 | 首次启动随机生成 |
//! | `KAIMAI_MAX_USERS` | 人数上限 | `20` |

use std::fs;
use std::io;
use std::net::{IpAddr, Ipv4Addr, SocketAddr, TcpListener, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use protocol::Invite;
use server::conn::Hub;
use server::state::{Config, Server};
use transport::{server_config, ServerCert};

/// 默认端口。TCP 和 UDP 用同一个号 —— 用户只需要记住一个数，
/// 转发端口时也只转一次。
///
/// # 为什么不在 49152 以上
///
/// 「动态/私有段」听起来正是给自定义服务用的，其实**恰恰相反**：那是各系统
/// 的临时端口段，谁都可以在里面抢。Windows 上更糟，Hyper-V / WSL / Docker
/// 会在那段里成片地做保留 —— 本机实测 UDP 的 49409–49908 一整片都被占了，
/// 绑上去直接 WSAEACCES。而 Mumble 的 64738 正落在这一片里。
///
/// 所以要挑一个在**所有**系统的临时端口段之下的号：
/// - Linux 临时段 32768–60999
/// - Windows / macOS 临时段 49152–65535
///
/// 取交集就是「小于 32768」。再避开 1024 以下（要 root）和常见游戏/服务端口，
/// 落在 20800。
const DEFAULT_PORT: u16 = 20800;

/// 看门狗多久扫一次。比 `IDLE_TIMEOUT` 小一个数量级就够了。
const SWEEP_INTERVAL: Duration = Duration::from_secs(3);

fn main() {
    if let Err(e) = run() {
        eprintln!("启动失败：{e}");
        std::process::exit(1);
    }
}

fn run() -> io::Result<()> {
    let port = env_parse("KAIMAI_PORT", DEFAULT_PORT)?;
    let max_users = env_parse("KAIMAI_MAX_USERS", 20usize)?;
    let data_dir =
        PathBuf::from(std::env::var("KAIMAI_DATA").unwrap_or_else(|_| "./kaimai-data".to_string()));
    fs::create_dir_all(&data_dir)?;

    let (cert, cert_is_new) = ServerCert::load_or_create(&data_dir)?;
    let (invite_code, code_is_new) = load_or_create_invite_code(&data_dir)?;

    let config = Config {
        max_users,
        require_invite: invite_code.is_some(),
        invite_code: invite_code.clone(),
        admin_keys: Vec::new(),
    };
    let server = Server::new(config);

    // 先绑好两个 socket 再往下走：端口被占的话要在打印邀请链接**之前**失败，
    // 不然用户会拿着一条根本连不上的链接去找人。
    let listener =
        TcpListener::bind((Ipv4Addr::UNSPECIFIED, port)).map_err(|e| bind_error("TCP", port, e))?;
    let voice =
        UdpSocket::bind((Ipv4Addr::UNSPECIFIED, port)).map_err(|e| bind_error("UDP", port, e))?;
    // 默认 8 KB 的接收缓冲在 20 人同时说话时会溢出（M1 量过）。
    voice_core::net::set_recv_buffer(&voice, voice_core::net::DEFAULT_RECV_BUFFER)?;

    let tls_config = Arc::new(server_config(&cert).map_err(io::Error::other)?);
    let hub = Arc::new(Hub::new(server, voice));

    let host = std::env::var("KAIMAI_HOST").unwrap_or_else(|_| local_address());
    let invite = Invite {
        host: host.clone(),
        port,
        cert: cert.fingerprint(),
        code: invite_code,
    };
    print_banner(&invite, cert_is_new, code_is_new, &data_dir)?;

    server::spawn_watchdog(Arc::clone(&hub), SWEEP_INTERVAL)?;

    {
        let hub = Arc::clone(&hub);
        std::thread::Builder::new()
            .name("kaimai-voice".into())
            .spawn(move || hub.run_voice())?;
    }

    server::accept_loop(listener, tls_config, hub);
    Ok(())
}

/// 读邀请码，没有就生成一个。返回 `(码, 是不是新生成的)`。
///
/// `KAIMAI_INVITE` 设成空串表示**不要邀请码**，谁都能进 —— 局域网开黑时有用。
fn load_or_create_invite_code(dir: &Path) -> io::Result<(Option<String>, bool)> {
    if let Ok(from_env) = std::env::var("KAIMAI_INVITE") {
        let trimmed = from_env.trim().to_string();
        return Ok(if trimmed.is_empty() {
            (None, false)
        } else {
            (Some(trimmed), false)
        });
    }

    let path = dir.join("invite-code.txt");
    if let Ok(existing) = fs::read_to_string(&path) {
        let trimmed = existing.trim().to_string();
        if !trimmed.is_empty() {
            return Ok((Some(trimmed), false));
        }
    }

    // 10 字节 = 80 位。邀请码是公网上能被猜的东西，长度得够。
    let mut raw = [0u8; 10];
    getrandom::fill(&mut raw).map_err(|e| io::Error::other(format!("拿不到随机数: {e}")))?;
    let code = protocol::base32::encode(&raw);
    fs::write(&path, format!("{code}\n"))?;
    Ok((Some(code), true))
}

fn print_banner(
    invite: &Invite,
    cert_is_new: bool,
    code_is_new: bool,
    data_dir: &Path,
) -> io::Result<()> {
    let link = invite.to_url().map_err(io::Error::other)?;

    println!();
    println!("  开麦服务端已启动");
    println!("  监听 {}:{}", Ipv4Addr::UNSPECIFIED, invite.port);
    println!("  数据目录 {}", data_dir.display());
    println!("  证书指纹 {}", invite.cert.to_grouped_hex());
    if cert_is_new {
        println!("           （新生成的。换机器时把数据目录一起搬走，");
        println!("             否则指纹会变，老邀请链接全部失效）");
    }
    println!();
    println!("  把这一行发给朋友，他们粘进开麦就能进来：");
    println!();
    println!("      {link}");
    println!();
    if invite.code.is_none() {
        println!("  ⚠ 没设邀请码：任何知道地址和指纹的人都能进。");
    } else if code_is_new {
        println!("  邀请码是首次启动随机生成的，存在 invite-code.txt 里。");
    }
    if invite.host == "127.0.0.1" {
        println!("  ⚠ 没探测到局域网地址，链接里写的是本机回环地址。");
        println!("    让外面的人连进来，要设 KAIMAI_HOST 成你的公网 IP 或域名。");
    } else {
        println!(
            "  链接里写的是局域网地址 {}。要给外网的朋友用的话，",
            invite.host
        );
        println!("  设 KAIMAI_HOST 成你的公网 IP 或域名，并把端口转发过来。");
    }
    println!();
    Ok(())
}

/// 猜一个能写进邀请链接的本机地址。
///
/// 用「连一个外部地址然后看内核选了哪张网卡」这个办法 —— UDP 的 connect
/// 不发任何包，纯粹是让路由表做一次选择。比枚举网卡靠谱：有虚拟机、
/// VPN、Docker 的机器上网卡能有七八张，挑错了链接就是废的。
fn local_address() -> String {
    let probe = || -> io::Result<IpAddr> {
        let sock = UdpSocket::bind((Ipv4Addr::UNSPECIFIED, 0))?;
        sock.connect(SocketAddr::from(([223, 5, 5, 5], 53)))?;
        Ok(sock.local_addr()?.ip())
    };
    match probe() {
        Ok(ip) if !ip.is_loopback() && !ip.is_unspecified() => ip.to_string(),
        // 没网也要能起来 —— 局域网开黑经常就是没有外网的。
        _ => "127.0.0.1".to_string(),
    }
}

/// 把绑端口的失败翻译成人话。
///
/// 默认情况下这类报错只有一句「以一种访问权限不允许的方式……」，而真正的原因
/// （端口被系统保留了、或者被别的程序占了）一个字都没提。用户装完就卡在这里，
/// 又正好是「装完还要读文档」那条痛点。
fn bind_error(which: &str, port: u16, e: io::Error) -> io::Error {
    let mut lines = vec![format!("{which} 绑不上 {port} 端口：{e}")];
    match e.kind() {
        io::ErrorKind::AddrInUse => {
            lines.push(format!("{port} 端口已经被别的程序占了。"));
        }
        io::ErrorKind::PermissionDenied if cfg!(windows) => {
            lines.push(format!(
                "{port} 端口被系统保留了 —— Hyper-V / WSL / Docker 会成片地占用端口。"
            ));
            lines.push("看看被占了哪些：".into());
            lines.push(format!(
                "    netsh int ipv4 show excludedportrange protocol={}",
                which.to_lowercase()
            ));
        }
        io::ErrorKind::PermissionDenied => {
            lines.push(format!("没权限绑 {port} 端口。1024 以下要 root。"));
        }
        _ => {}
    }
    lines.push("换一个端口：KAIMAI_PORT=别的数字".into());
    io::Error::other(lines.join(
        "
  ",
    ))
}

fn env_parse<T: std::str::FromStr>(name: &str, default: T) -> io::Result<T> {
    match std::env::var(name) {
        Ok(raw) => raw
            .trim()
            .parse()
            .map_err(|_| io::Error::other(format!("{name} 的值 `{raw}` 看不懂"))),
        Err(_) => Ok(default),
    }
}
