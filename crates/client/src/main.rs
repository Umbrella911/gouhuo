// SPDX-License-Identifier: GPL-3.0-or-later
#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

//! 开麦客户端。
//!
//! 这个文件只做一件事：把 `client-core` 的状态搬到界面上，把界面的点击搬回去。
//! **所有规则都不在这里** —— 能不能进、谁在哪个频道、消息怎么归属，
//! 全在 `client-core` 和服务端，那些地方都有测试。
//!
//! # 界面线程不碰网络
//!
//! `client-core` 的读线程收到消息之后往 channel 里塞事件，这里用
//! `upgrade_in_event_loop` 把更新搬回界面线程。界面线程从不阻塞在 socket 上，
//! 所以网络卡住了界面照样能动 —— 这在语音软件里是必须的，
//! 用户第一件想做的事就是点「离开」。

use std::rc::Rc;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};

use client_core::{Client, ConnectError, Event};
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
use voice_core::identity::Identity;

slint::include_modules!();

fn main() -> Result<(), slint::PlatformError> {
    let app = App::new()?;

    // 身份是本地的一对密钥，没有账号密码。第一次跑会生成一个。
    let (identity, first_run) = match load_identity() {
        Ok((identity, first_run)) => (Rc::new(identity), first_run),
        Err(e) => {
            app.set_error_headline("打不开你的身份文件".into());
            app.set_error_advice(
                format!("{e}\n检查一下 %APPDATA%\\kaimai 这个目录是不是只读的。").into(),
            );
            app.run()?;
            return Ok(());
        }
    };
    app.set_my_fingerprint(identity.public_key().fingerprint().to_grouped_hex().into());
    app.set_nick(default_nick().into());

    // 命令行上给了链接就填进去。Windows 把 `kaimai://` 的协议处理器
    // 就是这么调起来的 —— 所以这一个参数同时也是「一键加入」的落点。
    if let Some(link) = link_from_args() {
        app.set_invite_link(link.into());
    }

    // 用 Arc<Mutex<..>> 而不是 Rc<RefCell<..>>：连接结果要从后台线程
    // 搬回界面线程，那个闭包必须是 Send 的。
    let state = Arc::new(Mutex::new(State::default()));

    wire_join(&app, &identity, &state);
    wire_actions(&app, &state);

    // 点链接进来的老用户直接连，这才叫一键加入。
    //
    // **第一次跑的人不自动连**：那时昵称还是 Windows 用户名，
    // 而且身份刚生成，该让他先看一眼再进去，否则他会顶着 "admin"
    // 出现在一屋子人面前。
    if !first_run && !app.get_invite_link().is_empty() {
        app.invoke_join();
    }

    // 窗口要先创建出来才有句柄可以改。
    app.show()?;
    dark_titlebar(&app);
    slint::run_event_loop()?;
    Ok(())
}

/// 让标题栏跟着窗口一起是深色的。
///
/// 深色应用配一条浅色标题栏，是「界面停留在 2010 年」最典型的症状之一：
/// 整个窗口看着像是两个程序拼起来的。Windows 10 1809 起支持这个属性，
/// 更早的版本上这次调用会失败，忽略就行 —— 老系统上只是标题栏还是浅色。
#[cfg(windows)]
fn dark_titlebar(app: &App) {
    use raw_window_handle::{HasWindowHandle, RawWindowHandle};
    use windows_sys::Win32::Graphics::Dwm::DwmSetWindowAttribute;

    /// DWMWA_USE_IMMERSIVE_DARK_MODE
    const DARK_MODE: u32 = 20;

    let handle = app.window().window_handle();
    let Ok(handle) = handle.window_handle() else {
        return;
    };
    let RawWindowHandle::Win32(win32) = handle.as_raw() else {
        return;
    };
    let on: i32 = 1;
    // SAFETY: hwnd 来自窗口系统本身，属性值是一个我们自己的 i32，长度也是它的大小。
    unsafe {
        DwmSetWindowAttribute(
            win32.hwnd.get() as *mut core::ffi::c_void,
            DARK_MODE,
            (&on as *const i32).cast(),
            std::mem::size_of::<i32>() as u32,
        );
    }
}

#[cfg(not(windows))]
fn dark_titlebar(_app: &App) {}

#[derive(Default)]
struct State {
    client: Option<Client>,
}

/// 点「加入」之后发生的事。
fn wire_join(app: &App, identity: &Rc<Identity>, state: &Arc<Mutex<State>>) {
    let weak = app.as_weak();
    let identity = Rc::clone(identity);
    let state = Arc::clone(state);

    app.on_join(move || {
        let Some(app) = weak.upgrade() else { return };
        if app.get_connecting() {
            return;
        }
        let link = app.get_invite_link().to_string();
        let nick = {
            let typed = app.get_nick().to_string();
            let trimmed = typed.trim().to_string();
            if trimmed.is_empty() {
                default_nick()
            } else {
                trimmed
            }
        };

        app.set_connecting(true);
        app.set_error_headline("".into());
        app.set_error_advice("".into());

        // 连接会阻塞（DNS、TCP、TLS 握手，最长 8 秒），**不能在界面线程上做** ——
        // 否则窗口会白到超时为止，用户以为程序死了。
        let weak = app.as_weak();
        let state = Arc::clone(&state);
        // 身份只有一份，不能移进后台线程。导出再导入拿一份副本 ——
        // 这条路径本来就要能跑（换机器就是靠它），顺手也验了一次。
        let identity_copy = match Identity::import(&identity.export()) {
            Ok(copy) => copy,
            Err(_) => {
                app.set_connecting(false);
                return;
            }
        };

        std::thread::spawn(move || {
            let outcome = Client::connect(&link, &identity_copy, &nick);
            let _ = weak.upgrade_in_event_loop(move |app| match outcome {
                Ok((client, events)) => {
                    on_connected(&app, &state, client, events, &nick);
                }
                Err(e) => {
                    show_error(&app, &e);
                    app.set_connecting(false);
                }
            });
        });
    });
}

fn show_error(app: &App, e: &ConnectError) {
    app.set_error_headline(e.headline().into());
    app.set_error_advice(e.advice().into());
}

fn on_connected(
    app: &App,
    state: &Arc<Mutex<State>>,
    client: Client,
    events: Receiver<Event>,
    nick: &str,
) {
    // 服务端可能改过昵称（重名会加后缀），以它给的为准。
    let actual = {
        let roster = client.roster();
        roster.name_of(roster.me)
    };
    app.set_nick(if actual.is_empty() {
        nick.into()
    } else {
        actual.into()
    });

    state.lock().expect("state poisoned").client = Some(client.clone());
    app.set_connecting(false);
    app.set_connected(true);
    app.set_self_muted(false);
    app.set_self_deafened(false);
    refresh(app, &client);

    pump_events(app.as_weak(), Arc::clone(state), client, events);
}

/// 把事件从读线程搬到界面线程。
fn pump_events(
    weak: slint::Weak<App>,
    state: Arc<Mutex<State>>,
    client: Client,
    events: Receiver<Event>,
) {
    std::thread::spawn(move || {
        // `recv` 在通道另一端被丢掉时返回 Err，循环自然结束 ——
        // 不需要额外的关闭信号。
        while let Ok(event) = events.recv() {
            let client = client.clone();
            let state = Arc::clone(&state);
            let posted = weak.upgrade_in_event_loop(move |app| match event {
                Event::Disconnected(reason) => {
                    state.lock().expect("state poisoned").client = None;
                    app.set_connected(false);
                    app.set_error_headline("连接断开".into());
                    app.set_error_advice(reason.into());
                }
                // 其余的都只是「画面该变了」。进出提示音和 TTS 等 M5 音频那边接上再说，
                // 事件里已经带着名字了。
                _ => refresh(&app, &client),
            });
            if posted.is_err() {
                // 窗口关了
                return;
            }
        }
    });
}

/// 重画左边的树和右边的聊天。
///
/// 每次事件都整个重建列表，不做增量。20 个人几十条消息，重建的代价
/// 完全测不出来；而增量更新是「名单和实际对不上」这类 bug 的主要来源。
fn refresh(app: &App, client: &Client) {
    let roster = client.roster();

    let mut rows: Vec<Row> = Vec::new();
    for node in roster.tree() {
        let members = roster.users_in(node.channel.id);
        rows.push(Row {
            is_channel: true,
            id: node.channel.id as i32,
            name: node.channel.name.clone().into(),
            depth: node.depth as i32,
            muted: false,
            deafened: false,
            speaking: false,
            is_me: false,
            is_current: node.channel.id == roster.my_channel(),
            count: members.len() as i32,
        });
        for user in members {
            rows.push(Row {
                is_channel: false,
                id: user.session_id as i32,
                name: user.name.clone().into(),
                depth: node.depth as i32 + 1,
                muted: user.self_muted || user.server_muted,
                deafened: user.self_deafened,
                // 说话指示要等 UDP 那半边接上
                speaking: false,
                is_me: user.session_id == roster.me,
                is_current: false,
                count: 0,
            });
        }
    }

    let chat: Vec<ChatRow> = roster
        .chat
        .iter()
        .map(|line| ChatRow {
            sender: line.sender_name.clone().into(),
            body: line.body.clone().into(),
            time: clock_time(line.timestamp_ms).into(),
            is_me: line.sender_session == roster.me,
        })
        .collect();

    // 自己的静音状态以服务端为准 —— 别的地方（将来的全局热键）也会改它。
    if let Some(me) = roster.my_user() {
        app.set_self_muted(me.self_muted);
        app.set_self_deafened(me.self_deafened);
    }

    app.set_rows(ModelRc::new(VecModel::from(rows)));
    app.set_chat(ModelRc::new(VecModel::from(chat)));
}

fn wire_actions(app: &App, state: &Arc<Mutex<State>>) {
    // 每个回调都要拿到当前连接。写成一个小闭包而不是宏 ——
    // 回调的签名各不相同（有的带参数有的不带），宏反而要绕。
    fn current(state: &Arc<Mutex<State>>) -> Option<Client> {
        state.lock().expect("state poisoned").client.clone()
    }

    {
        let state = Arc::clone(state);
        app.on_join_channel(move |id| {
            if let Some(client) = current(&state) {
                client.join_channel(id as u32);
            }
        });
    }

    {
        let state = Arc::clone(state);
        let weak = app.as_weak();
        app.on_send_text(move || {
            let (Some(app), Some(client)) = (weak.upgrade(), current(&state)) else {
                return;
            };
            client.send_text(&app.get_draft());
            app.set_draft(SharedString::new());
        });
    }

    {
        let state = Arc::clone(state);
        let weak = app.as_weak();
        app.on_toggle_mute(move || {
            let (Some(app), Some(client)) = (weak.upgrade(), current(&state)) else {
                return;
            };
            // 关着耳朵的时候单独开麦没有意义，服务端也会把它改回去。
            client.set_self_state(!app.get_self_muted(), app.get_self_deafened());
        });
    }

    {
        let state = Arc::clone(state);
        let weak = app.as_weak();
        app.on_toggle_deafen(move || {
            let (Some(app), Some(client)) = (weak.upgrade(), current(&state)) else {
                return;
            };
            let deafened = !app.get_self_deafened();
            // 关耳朵连带闭麦。服务端也会这么改，这里跟着改是为了按下去立刻有反馈。
            client.set_self_state(app.get_self_muted() || deafened, deafened);
        });
    }

    {
        let state = Arc::clone(state);
        let weak = app.as_weak();
        app.on_leave(move || {
            if let Some(client) = current(&state) {
                client.disconnect();
            }
            if let Some(app) = weak.upgrade() {
                // 主动离开不是错误，别把上一次的报错留在登录页上。
                app.set_connected(false);
                app.set_error_headline("".into());
                app.set_error_advice("".into());
            }
        });
    }
}

/// 返回 `(身份, 是不是这次新建的)`。
fn load_identity() -> std::io::Result<(Identity, bool)> {
    let path = Identity::default_path()?;
    Identity::load_or_create(&path)
}

/// 从命令行里挑出邀请链接。
///
/// 只认 `kaimai://` 开头的那个参数，别的一概不管 —— 协议处理器被调起来时，
/// 参数里可能还夹着别的东西，而把任意一个参数当链接用是个很好的注入入口。
fn link_from_args() -> Option<String> {
    std::env::args()
        .skip(1)
        .find(|arg| arg.starts_with(protocol::URL_PREFIX))
}

/// 默认昵称用 Windows 的用户名 —— 比让用户对着空框发呆强。
fn default_nick() -> String {
    std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .map(|name| name.trim().to_string())
        .ok()
        .filter(|name| !name.is_empty())
        .unwrap_or_else(|| "玩家".to_string())
}

/// Unix 毫秒 → 本地的 `HH:MM`。
///
/// 自己算而不是拉一个日期库进来：这里只要「今天的几点几分」，
/// 而日期库会带着时区数据库一起进安装包。
fn clock_time(timestamp_ms: i64) -> String {
    let Ok(now) = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH) else {
        return String::new();
    };
    // 用「本机现在的本地时间」和「本机现在的 UTC 时间」之差当时区偏移。
    // 对聊天时间戳足够了 —— 误差只会出现在跨夏令时切换的那一小时，
    // 而中国不用夏令时。
    let offset = local_offset_seconds();
    let local = timestamp_ms / 1000 + offset;
    let _ = now;
    let seconds_today = local.rem_euclid(86_400);
    format!(
        "{:02}:{:02}",
        seconds_today / 3600,
        (seconds_today % 3600) / 60
    )
}

#[cfg(windows)]
fn local_offset_seconds() -> i64 {
    // SAFETY: GetTimeZoneInformation 只写它自己的输出结构，没有别的副作用。
    unsafe {
        let mut info =
            std::mem::zeroed::<windows_sys::Win32::System::Time::TIME_ZONE_INFORMATION>();
        let kind = windows_sys::Win32::System::Time::GetTimeZoneInformation(&mut info);
        // Bias 是「本地时间 + Bias = UTC」，单位是分钟，所以要取负
        let bias = match kind {
            2 => info.Bias + info.DaylightBias, // TIME_ZONE_ID_DAYLIGHT
            _ => info.Bias + info.StandardBias,
        };
        -(bias as i64) * 60
    }
}

#[cfg(not(windows))]
fn local_offset_seconds() -> i64 {
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clock_time_is_hh_mm() {
        let text = clock_time(1_700_000_000_000);
        assert_eq!(text.len(), 5, "{text}");
        assert_eq!(&text[2..3], ":");
        let hour: u32 = text[..2].parse().unwrap();
        let minute: u32 = text[3..].parse().unwrap();
        assert!(hour < 24);
        assert!(minute < 60);
    }

    /// 时间戳是服务端给的，不可信。0 和负数都不能让界面 panic。
    #[test]
    fn nonsense_timestamps_do_not_panic() {
        for stamp in [0, -1, i64::MIN, i64::MAX] {
            let text = clock_time(stamp);
            assert!(text.is_empty() || text.len() == 5, "{stamp} -> {text}");
        }
    }

    #[test]
    fn default_nick_is_never_empty() {
        assert!(!default_nick().is_empty());
    }
}
