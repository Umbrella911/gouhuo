// SPDX-License-Identifier: MPL-2.0

//! 服务端重启之后频道还在，对着**真服务端**、真存档文件跑。
//!
//! 「重启」的做法：在同一个存档文件上再起一个服务端，用同一个身份连上去。
//! 旧的那个不用关 —— 它只是不再有人连。

use std::net::{TcpListener, UdpSocket};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use client_core::Client;
use protocol::Invite;
use server::conn::Hub;
use server::state::{Config, Server};
use server::store::Store;
use transport::{server_config, ServerCert};
use voice_core::identity::Identity;

const WAIT: Duration = Duration::from_secs(5);

/// 在 `db` 这份存档上起一个服务端，返回邀请链接。
fn start(db: &Path) -> String {
    let cert = ServerCert::generate().unwrap();
    let fingerprint = cert.fingerprint();
    let tls = Arc::new(server_config(&cert).unwrap());
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    let voice = UdpSocket::bind("127.0.0.1:0").unwrap();

    let store = Store::open(db).unwrap();
    let server = Server::restore(Config::default(), store.load().unwrap());
    let hub = Arc::new(Hub::with_store(server, voice, store));
    let accept_hub = Arc::clone(&hub);
    std::thread::spawn(move || server::accept_loop(listener, tls, accept_hub));

    Invite {
        host: addr.ip().to_string(),
        port: addr.port(),
        cert: fingerprint,
        code: None,
    }
    .to_url()
    .unwrap()
}

fn temp_db() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("gouhuo-persist-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir.join("gouhuo.db")
}

fn channel_named(client: &Client, name: &str) -> Option<u32> {
    client
        .roster()
        .channels
        .values()
        .find(|c| c.name == name)
        .map(|c| c.id)
}

fn eventually(what: &str, mut ok: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while Instant::now() < deadline {
        if ok() {
            return;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    panic!("一直没等到：{what}");
}

/// 验收标准：建频道 → 重启服务端 → 频道还在，建的人还能删。
#[test]
fn a_channel_survives_a_server_restart_and_its_creator_can_still_delete_it() {
    let db = temp_db();
    let identity = Identity::generate().unwrap();

    let before = start(&db);
    let (alice, _events) = Client::connect(&before, &identity, "阿狸").unwrap();
    alice.create_channel("开黑", 0);
    eventually("频道建好", || channel_named(&alice, "开黑").is_some());
    alice.disconnect();

    // 重启：同一份存档，新的服务端
    let after = start(&db);
    let (alice, _events) = Client::connect(&after, &identity, "阿狸").unwrap();
    let id = channel_named(&alice, "开黑").expect("重启之后频道没了");
    assert!(
        alice.roster().can_delete_channel(id),
        "重启之后界面上认不出这是我建的了"
    );

    alice.delete_channel(id);
    eventually("频道删掉", || channel_named(&alice, "开黑").is_none());

    // 删也要落盘：再重启一次，它不能回来
    let again = start(&db);
    let (bob, _events) = Client::connect(&again, &Identity::generate().unwrap(), "波波").unwrap();
    assert_eq!(
        channel_named(&bob, "开黑"),
        None,
        "删掉的频道重启之后又回来了"
    );
}
