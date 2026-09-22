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

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::mpsc::Receiver;
use std::sync::{Arc, Mutex};

use client_core::{Client, ConnectError, Event};
use slint::{ComponentHandle, Model, ModelRc, SharedString, VecModel};
use voice_core::identity::Identity;
use voice_core::miccheck::{scan_microphones, MicCheck};
use voice_core::pipeline::{Pipeline, PipelineConfig, TransmitMode, DEFAULT_JITTER_FRAMES};

/// 多久去问一次语音链路的状态。
///
/// 定这个数的是**电平条**：200 ms 的条子看着是一跳一跳的，用户会以为卡了，
/// 而它恰恰是用来判断「麦克风有没有在动」的，跟不上就失去了意义。
/// 说话指示也跟着受益。
///
/// 每秒 20 次读几个原子变量加两次加锁，跟音频线程各自每秒 100 次比
/// 完全不在一个量级上。
const VOICE_POLL: std::time::Duration = std::time::Duration::from_millis(50);

/// 多久看一次按住说话的键有没有被按下。
///
/// 这个数直接决定「按下去到开始发声」的延迟，所以要比语音状态那个快得多。
/// 20 ms 是两帧音频，用户感觉不出来；再快就只是在空转了。
const PTT_POLL: std::time::Duration = std::time::Duration::from_millis(20);

mod settings;

use settings::{db_to_level, level_to_db, Settings, TalkMode};
use voice_core::hotkey::{Hotkeys, Key};

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

    // 全局热键。起不来不该让客户端打不开 —— 语音激活那条路不需要它。
    let hotkeys = match Hotkeys::start() {
        Ok(hotkeys) => Some(Rc::new(hotkeys)),
        Err(e) => {
            eprintln!("全局热键起不来，按住说话用不了：{e}");
            None
        }
    };

    let stored = Settings::load();
    app.set_nick(if stored.nick.is_empty() {
        default_nick().into()
    } else {
        stored.nick.clone().into()
    });
    app.set_invite_link(stored.last_invite.clone().into());
    app.set_ptt_mode(stored.talk_mode == TalkMode::PushToTalk);
    app.set_ptt_label(
        stored
            .ptt_key
            .map(|key| key.label())
            .unwrap_or_default()
            .into(),
    );
    app.set_vad_level(db_to_level(stored.vad_threshold_db));
    if let Some(hotkeys) = &hotkeys {
        hotkeys.set_ptt(stored.ptt_key);
    }

    // 命令行上给了链接就填进去，盖过上次存的那条。Windows 把 `kaimai://`
    // 的协议处理器就是这么调起来的 —— 这一个参数同时也是「一键加入」的落点。
    if let Some(link) = link_from_args() {
        app.set_invite_link(link.into());
    }

    // 用 Arc<Mutex<..>> 而不是 Rc<RefCell<..>>：连接结果要从后台线程
    // 搬回界面线程，那个闭包必须是 Send 的。
    let state = Arc::new(Mutex::new(State::default()));
    state.lock().expect("state poisoned").settings = stored;

    wire_join(&app, &identity, &state);
    wire_actions(&app, &state);
    wire_settings(&app, &state, hotkeys.clone());
    wire_scan(&app, &state);
    load_devices(&app, &state);
    spawn_status_poll(app.as_weak(), Arc::clone(&state));
    if let Some(hotkeys) = hotkeys {
        spawn_ptt_poll(app.as_weak(), Arc::clone(&state), hotkeys);
    }

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
    /// 语音链路。丢掉它就会把音频线程收干净。
    voice: Option<Arc<Pipeline>>,
    /// 没连服务器时的独立试麦。
    ///
    /// 跟 `voice` **永远不会同时存在** —— 两个都要开同一副耳机，
    /// 虽然共享模式下不会打架，但麦克风会被采两遍，电平也会对不上。
    mic_check: Option<Arc<MicCheck>>,
    settings: Settings,
    /// 采集流的把手：实际打开的设备叫什么。
    capture: Option<voice_core::wasapi::CaptureDiagnostics>,
    /// 下拉框里第 n 项对应哪个设备 id。第 0 项是「系统默认」，所以是 None。
    capture_ids: Vec<Option<String>>,
    render_ids: Vec<Option<String>>,
}

thread_local! {
    /// 定时器丢掉就停了，所以要让它活到窗口关掉为止。
    static VOICE_TIMER: RefCell<Option<slint::Timer>> = const { RefCell::new(None) };
    static PTT_TIMER: RefCell<Option<slint::Timer>> = const { RefCell::new(None) };
    /// 等用户按键设置热键时，接收那个键的通道。
    static REBIND: RefCell<Option<std::sync::mpsc::Receiver<Key>>> =
        const { RefCell::new(None) };
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

    {
        // 连上了才存 —— 存一条连不上的链接只会让下次打开就看到一个错误。
        let mut locked = state.lock().expect("state poisoned");
        locked.client = Some(client.clone());
        locked.settings.nick = app.get_nick().to_string();
        locked.settings.last_invite = app.get_invite_link().to_string();
        let _ = locked.settings.save();
    }
    app.set_connecting(false);
    app.set_connected(true);
    app.set_self_muted(false);
    app.set_self_deafened(false);
    app.set_voice_error("".into());
    app.set_udp_ok(false);
    refresh(app, &client);

    start_voice(app, state, &client);
    pump_events(app.as_weak(), Arc::clone(state), client, events);
}

/// 起语音链路，并开一个定时器把它的状态搬到界面上。
///
/// 设备打不开不该让人掉线 —— 文字和名单照样能用，只是没声音。所以这里
/// 失败只是把原因显示出来，不动连接。
fn start_voice(app: &App, state: &Arc<Mutex<State>>, client: &Client) {
    // 先把独立试麦停掉：两个都开的话麦克风会被采两遍。
    stop_mic_check(state);

    let Some(addr) = resolve_voice_addr(client) else {
        app.set_voice_error("服务器没给出语音端口".into());
        return;
    };

    let mode = transmit_mode(&state.lock().expect("state poisoned").settings);
    let cfg = PipelineConfig {
        session_id: client.session_id(),
        server: addr,
        upstream_key: *client.voice_keys().upstream.as_bytes(),
        downstream_key: *client.voice_keys().downstream.as_bytes(),
        jitter_frames: DEFAULT_JITTER_FRAMES,
        mode,
    };

    let (capture_id, render_id) = {
        let locked = state.lock().expect("state poisoned");
        (
            locked.settings.capture_device.clone(),
            locked.settings.render_device.clone(),
        )
    };
    let capture = voice_core::wasapi::WasapiCapture::new(capture_id);
    let diagnostics = capture.diagnostics();

    match Pipeline::start(
        cfg,
        Box::new(capture),
        Box::new(voice_core::wasapi::WasapiRender::new(render_id)),
        audio_processor(),
    ) {
        Ok(pipeline) => {
            let mut locked = state.lock().expect("state poisoned");
            locked.voice = Some(Arc::new(pipeline));
            locked.capture = Some(diagnostics);
        }
        Err(e) => app.set_voice_error(format!("{e}").into()),
    }
}

/// 把设备列表灌进下拉框，并记下「第 n 项是哪个 id」。
///
/// 第 0 项永远是「系统默认」—— 绝大多数人不该需要管这个，
/// 而且它是唯一在换了耳机之后还能跟着走的选项。
fn load_devices(app: &App, state: &Arc<Mutex<State>>) {
    use voice_core::wasapi::{list_endpoints, Direction};

    for (direction, is_capture) in [(Direction::Capture, true), (Direction::Render, false)] {
        let endpoints = list_endpoints(direction).unwrap_or_default();
        let mut labels: Vec<SharedString> = vec!["系统默认".into()];
        let mut ids: Vec<Option<String>> = vec![None];
        for endpoint in endpoints {
            // 标出虚拟声卡。玩家机器上这类设备极多，而「没声音」十有八九
            // 就是选中了一个没在路由的虚拟麦。
            let suffix = if endpoint.is_hardware {
                ""
            } else {
                "（虚拟）"
            };
            labels.push(format!("{}{suffix}", endpoint.name).into());
            ids.push(Some(endpoint.id));
        }

        let saved = {
            let locked = state.lock().expect("state poisoned");
            if is_capture {
                locked.settings.capture_device.clone()
            } else {
                locked.settings.render_device.clone()
            }
        };
        let index = ids
            .iter()
            .position(|id| *id == saved)
            // 存下来的设备没了（拔了耳机、换了机器）就退回「系统默认」，
            // 跟 open_or_default 的行为一致。
            .unwrap_or(0) as i32;

        let model = ModelRc::new(VecModel::from(labels));
        let mut locked = state.lock().expect("state poisoned");
        if is_capture {
            locked.capture_ids = ids;
            drop(locked);
            app.set_capture_devices(model);
            app.set_capture_index(index);
        } else {
            locked.render_ids = ids;
            drop(locked);
            app.set_render_devices(model);
            app.set_render_index(index);
        }
    }
}

/// 换设备。链路要重起 —— WASAPI 的流是绑在设备上的，换不了。
///
/// 重起会让声音断一下（几十毫秒）。这是换设备本来就该有的代价，
/// 比为了热切换在音频线程里加一套状态机划算得多。
fn restart_voice(app: &App, state: &Arc<Mutex<State>>) {
    let client = state.lock().expect("state poisoned").client.clone();
    let Some(client) = client else {
        // 没连服务器：重起的是独立试麦。
        let was_monitoring = {
            let locked = state.lock().expect("state poisoned");
            locked
                .mic_check
                .as_ref()
                .map(|m| m.is_monitoring())
                .unwrap_or(false)
        };
        stop_mic_check(state);
        app.set_capture_in_use("".into());
        start_mic_check(app, state);
        if was_monitoring {
            if let Some(mic) = state.lock().expect("state poisoned").mic_check.clone() {
                mic.set_monitoring(true);
            }
        }
        return;
    };

    let was_monitoring = current_voice(state)
        .map(|v| v.is_monitoring())
        .unwrap_or(false);
    {
        let mut locked = state.lock().expect("state poisoned");
        // 先丢掉旧的再起新的：同一个设备不能开两路，而且旧链路的线程
        // 还占着那个设备。
        locked.voice = None;
        locked.capture = None;
    }
    app.set_voice_error("".into());
    app.set_udp_ok(false);
    app.set_capture_in_use("".into());
    start_voice(app, state, &client);
    if was_monitoring {
        if let Some(voice) = current_voice(state) {
            voice.set_monitoring(true);
        }
    }
}

/// 每个麦克风试多久。
///
/// 太短的话用户还没来得及说出一个字就轮到下一个了；太长的话五个设备要等
/// 半分钟。1.2 秒够说一句「喂喂」，五个设备一共 6 秒。
const SCAN_PER_DEVICE: std::time::Duration = std::time::Duration::from_millis(1200);

/// 挨个试每个麦克风，把结果显示出来。
///
/// 在后台线程上跑：每个设备都要真的打开一次，那几秒里界面不能是卡死的。
fn wire_scan(app: &App, state: &Arc<Mutex<State>>) {
    let state = Arc::clone(state);
    let weak = app.as_weak();
    app.on_scan_microphones(move || {
        let Some(app) = weak.upgrade() else { return };
        if app.get_scanning() {
            return;
        }
        app.set_scanning(true);
        app.set_scan_results(ModelRc::new(VecModel::from(Vec::<ScanRow>::new())));

        // 检测要挨个打开设备，跟正在跑的试麦抢同一个麦克风。先停掉。
        let was_checking = state.lock().expect("state poisoned").mic_check.is_some();
        stop_mic_check(&state);

        let weak = app.as_weak();
        let state = Arc::clone(&state);
        std::thread::spawn(move || {
            let results = scan_microphones(SCAN_PER_DEVICE);
            let _ = weak.upgrade_in_event_loop(move |app| {
                let rows: Vec<ScanRow> = results
                    .iter()
                    .map(|result| ScanRow {
                        name: format!(
                            "{}{}",
                            result.name,
                            if result.is_hardware {
                                ""
                            } else {
                                "（虚拟）"
                            }
                        )
                        .into(),
                        verdict: result.verdict().into(),
                        ok: result.hears_something(),
                        id: result.id.clone().into(),
                    })
                    .collect();
                app.set_scan_results(ModelRc::new(VecModel::from(rows)));
                app.set_scanning(false);
                if was_checking {
                    start_mic_check(&app, &state);
                }
            });
        });
    });
}

/// 设置面板：切换说话方式、绑按住说话的键、试听麦克风、选设备。
fn wire_settings(app: &App, state: &Arc<Mutex<State>>, hotkeys: Option<Rc<Hotkeys>>) {
    {
        let state = Arc::clone(state);
        let weak = app.as_weak();
        app.on_pick_capture(move |index| {
            let Some(app) = weak.upgrade() else { return };
            {
                let mut locked = state.lock().expect("state poisoned");
                let id = locked.capture_ids.get(index as usize).cloned().flatten();
                locked.settings.capture_device = id;
                let _ = locked.settings.save();
            }
            restart_voice(&app, &state);
        });
    }

    {
        // 从检测结果里点一行：直接按设备 id 选，不经过下拉框的序号。
        let state = Arc::clone(state);
        let weak = app.as_weak();
        app.on_pick_capture_id(move |id| {
            let Some(app) = weak.upgrade() else { return };
            let id = id.to_string();
            {
                let mut locked = state.lock().expect("state poisoned");
                locked.settings.capture_device = Some(id.clone());
                let _ = locked.settings.save();
            }
            // 下拉框跟着走。对不上就保持原样 —— 真正生效的是上面存的 id，
            // 下拉框显示得对不对只是好看不好看的事。
            let index = state
                .lock()
                .expect("state poisoned")
                .capture_ids
                .iter()
                .position(|known| known.as_deref() == Some(id.as_str()));
            if let Some(index) = index {
                app.set_capture_index(index as i32);
            }
            restart_voice(&app, &state);
        });
    }

    {
        let state = Arc::clone(state);
        let weak = app.as_weak();
        app.on_pick_render(move |index| {
            let Some(app) = weak.upgrade() else { return };
            {
                let mut locked = state.lock().expect("state poisoned");
                let id = locked.render_ids.get(index as usize).cloned().flatten();
                locked.settings.render_device = id;
                let _ = locked.settings.save();
            }
            restart_voice(&app, &state);
        });
    }

    {
        let state = Arc::clone(state);
        let weak = app.as_weak();
        app.on_toggle_monitor(move || {
            let Some(app) = weak.upgrade() else { return };
            let (voice, mic) = {
                let locked = state.lock().expect("state poisoned");
                (locked.voice.clone(), locked.mic_check.clone())
            };
            let on = match (&voice, &mic) {
                (Some(voice), _) => {
                    let on = !voice.is_monitoring();
                    voice.set_monitoring(on);
                    on
                }
                (None, Some(mic)) => {
                    let on = !mic.is_monitoring();
                    mic.set_monitoring(on);
                    on
                }
                (None, None) => return,
            };
            app.set_monitoring(on);
        });
    }

    {
        let state = Arc::clone(state);
        app.on_set_vad(move |level| {
            let mode = {
                let mut locked = state.lock().expect("state poisoned");
                locked.settings.vad_threshold_db = level_to_db(level);
                let _ = locked.settings.save();
                transmit_mode(&locked.settings)
            };
            if let Some(voice) = current_voice(&state) {
                voice.set_mode(mode);
            }
        });
    }

    {
        let weak = app.as_weak();
        let state = Arc::clone(state);
        app.on_toggle_settings(move || {
            let Some(app) = weak.upgrade() else { return };
            let opening = !app.get_show_settings();
            app.set_show_settings(opening);
            if opening {
                // 没连服务器也要能看电平、能试听 —— 这正是连不上时最想知道的事。
                start_mic_check(&app, &state);
            } else {
                // 关掉设置就把设备还回去。常驻几小时的软件不该一直占着麦克风，
                // 而且麦克风灯一直亮着会让人不安。
                stop_mic_check(&state);
            }
        });
    }

    {
        let state = Arc::clone(state);
        let weak = app.as_weak();
        app.on_set_ptt_mode(move |ptt| {
            let Some(app) = weak.upgrade() else { return };
            app.set_ptt_mode(ptt);
            let mode = {
                let mut locked = state.lock().expect("state poisoned");
                locked.settings.talk_mode = if ptt {
                    TalkMode::PushToTalk
                } else {
                    TalkMode::VoiceActivity
                };
                let _ = locked.settings.save();
                transmit_mode(&locked.settings)
            };
            // 链路不用重起，下一帧就按新方式走。
            if let Some(voice) = current_voice(&state) {
                voice.set_mode(mode);
            }
        });
    }

    {
        let hotkeys = hotkeys.clone();
        let weak = app.as_weak();
        app.on_rebind_ptt(move || {
            let (Some(app), Some(hotkeys)) = (weak.upgrade(), hotkeys.as_ref()) else {
                return;
            };
            REBIND.with(|slot| *slot.borrow_mut() = Some(hotkeys.capture_next()));
            app.set_rebinding(true);
        });
    }

    {
        let weak = app.as_weak();
        app.on_cancel_rebind(move || {
            let Some(app) = weak.upgrade() else { return };
            if let Some(hotkeys) = &hotkeys {
                hotkeys.cancel_capture();
            }
            REBIND.with(|slot| *slot.borrow_mut() = None);
            app.set_rebinding(false);
        });
    }
}

/// 盯着按住说话的键，也顺便接住「正在设置热键」按下的那个键。
///
/// 轮询而不是回调：热键是在另一个线程上收到的，而改界面必须在界面线程上。
/// 每 20 ms 读一个原子变量，比每次按键都投递一次跨线程消息便宜得多 ——
/// 而按键在游戏里是每秒几十次的事。
fn spawn_ptt_poll(weak: slint::Weak<App>, state: Arc<Mutex<State>>, hotkeys: Rc<Hotkeys>) {
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, PTT_POLL, move || {
        let Some(app) = weak.upgrade() else { return };

        // 用户正在设置热键？看看按下来没有。
        let captured = REBIND.with(|slot| slot.borrow().as_ref().and_then(|rx| rx.try_recv().ok()));
        if let Some(key) = captured {
            REBIND.with(|slot| *slot.borrow_mut() = None);
            hotkeys.set_ptt(Some(key));
            app.set_rebinding(false);
            app.set_ptt_label(key.label().into());
            let mut locked = state.lock().expect("state poisoned");
            locked.settings.ptt_key = Some(key);
            let _ = locked.settings.save();
            return;
        }

        // 按住说话：把键的状态推给链路。
        let Some(voice) = current_voice(&state) else {
            return;
        };
        if app.get_ptt_mode() {
            let down = hotkeys.is_down();
            voice.set_transmitting(down);
            app.set_transmitting(down);
        }
    });
    PTT_TIMER.with(|slot| *slot.borrow_mut() = Some(timer));
}

/// 设置里的说话方式翻译成链路那边的发送方式。
fn transmit_mode(settings: &Settings) -> TransmitMode {
    match settings.talk_mode {
        TalkMode::PushToTalk => TransmitMode::PushToTalk,
        TalkMode::VoiceActivity => TransmitMode::VoiceActivity {
            threshold_db: settings.vad_threshold_db,
        },
    }
}

/// 回声消除/降噪。要 `--features apm`，见 Cargo.toml。
#[cfg(feature = "apm")]
fn audio_processor() -> Option<Box<dyn voice_core::pipeline::AudioProcessor>> {
    use voice_core::apm::{Apm, ApmConfig};
    // APM 起不来不该让语音也用不了。戴耳机的人根本不需要它。
    match Apm::new(voice_core::audio::SAMPLE_RATE, ApmConfig::default()) {
        Ok(apm) => Some(Box::new(apm)),
        Err(_) => None,
    }
}

#[cfg(not(feature = "apm"))]
fn audio_processor() -> Option<Box<dyn voice_core::pipeline::AudioProcessor>> {
    None
}

fn resolve_voice_addr(client: &Client) -> Option<std::net::SocketAddr> {
    use std::net::ToSocketAddrs;
    if client.udp_port() == 0 {
        return None;
    }
    (client.server_host(), client.udp_port())
        .to_socket_addrs()
        .ok()?
        .next()
}

/// 定时把音频状态搬到界面上。**整个程序只有一个**，连着和没连着都靠它。
///
/// 用 Slint 自己的定时器而不是线程：它就在界面线程上跑，省掉一次跨线程投递，
/// 而这件事每秒要做二十次。
fn spawn_status_poll(weak: slint::Weak<App>, state: Arc<Mutex<State>>) {
    let timer = slint::Timer::default();
    timer.start(slint::TimerMode::Repeated, VOICE_POLL, move || {
        let Some(app) = weak.upgrade() else { return };
        let (voice, mic_check, client, capture) = {
            let locked = state.lock().expect("state poisoned");
            (
                locked.voice.clone(),
                locked.mic_check.clone(),
                locked.client.clone(),
                locked.capture.clone(),
            )
        };

        // 实际用的是哪个设备、是不是虚拟声卡。两条路共用同一个把手。
        if let Some(capture) = capture {
            if capture.has_opened() {
                app.set_capture_in_use(capture.device_name().into());
                app.set_capture_is_virtual(capture.is_virtual());
            }
        }

        if let Some(voice) = voice {
            let stats = voice.stats();
            app.set_udp_ok(stats.udp_ok);
            app.set_input_level(db_to_level(stats.input_db));
            app.set_monitoring(voice.is_monitoring());
            app.set_transmitting(transmitting_now(&app, &voice, stats.input_db));
            if let Some(client) = client {
                update_speaking(&app, &client, &stats.speaking);
            }
        } else if let Some(mic) = mic_check {
            app.set_input_level(db_to_level(mic.input_db()));
            app.set_monitoring(mic.is_monitoring());
            // 没连服务器时「在不在发」没有意义，但电平条要靠它变色 ——
            // 用跟语音激活一样的判据，这样调灵敏度时看到的效果是真的。
            let threshold = state
                .lock()
                .expect("state poisoned")
                .settings
                .vad_threshold_db;
            app.set_transmitting(!app.get_ptt_mode() && mic.input_db() > threshold);
            if let Some(error) = mic.error() {
                app.set_voice_error(error.into());
            }
        }
    });
    VOICE_TIMER.with(|slot| *slot.borrow_mut() = Some(timer));
}

/// 开一次独立试麦（没连服务器的时候用）。
fn start_mic_check(app: &App, state: &Arc<Mutex<State>>) {
    if state.lock().expect("state poisoned").voice.is_some() {
        // 已经连上了，语音链路就在跑，用它的电平。
        return;
    }
    let (capture_id, render_id) = {
        let locked = state.lock().expect("state poisoned");
        (
            locked.settings.capture_device.clone(),
            locked.settings.render_device.clone(),
        )
    };
    let capture = voice_core::wasapi::WasapiCapture::new(capture_id);
    let diagnostics = capture.diagnostics();
    app.set_voice_error("".into());

    match MicCheck::start(
        Box::new(capture),
        Box::new(voice_core::wasapi::WasapiRender::new(render_id)),
        audio_processor(),
    ) {
        Ok(check) => {
            let mut locked = state.lock().expect("state poisoned");
            locked.mic_check = Some(Arc::new(check));
            locked.capture = Some(diagnostics);
        }
        Err(e) => app.set_voice_error(format!("{e}").into()),
    }
}

/// 停掉独立试麦。连服务器之前必须停 —— 不然麦克风会被采两遍。
fn stop_mic_check(state: &Arc<Mutex<State>>) {
    let mut locked = state.lock().expect("state poisoned");
    locked.mic_check = None;
}

/// 现在这一刻在不在往外发。
///
/// 自己说话服务端不会转回来，所以这个只能在本地算。它跟链路里那段判断
/// 是同一套规则 —— 两边写法不一样的话，界面显示「正在发送」而实际没发，
/// 是最让人摸不着头脑的那种 bug。
fn transmitting_now(app: &App, voice: &Pipeline, input_db: f32) -> bool {
    match voice.mode() {
        TransmitMode::Always => true,
        TransmitMode::PushToTalk => app.get_transmitting(),
        TransmitMode::VoiceActivity { threshold_db } => input_db > threshold_db,
    }
}

/// 只改「谁在说话」那一列，不整个重建列表。
///
/// 这件事每秒发生五次，而整个重建会让列表的滚动位置跳回顶上。
fn update_speaking(app: &App, client: &Client, speaking: &[u32]) {
    let rows = app.get_rows();
    let me = client.roster().me;
    for i in 0..rows.row_count() {
        let Some(mut row) = rows.row_data(i) else {
            continue;
        };
        if row.is_channel {
            continue;
        }
        // 自己说没说话服务端不会转回来，所以自己那一行永远不亮。
        // 要亮的话得看本地有没有在发 —— 等按键说话做出来再说。
        let now = speaking.contains(&(row.id as u32)) && row.id as u32 != me;
        if row.speaking != now {
            row.speaking = now;
            rows.set_row_data(i, row);
        }
    }
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
                    let mut locked = state.lock().expect("state poisoned");
                    locked.client = None;
                    // 丢掉链路会 join 掉所有音频线程。**必须做** ——
                    // 留着的话麦克风还开着，而用户已经不在频道里了。
                    locked.voice = None;
                    drop(locked);
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
            let muted = !app.get_self_muted();
            client.set_self_state(muted, app.get_self_deafened());
            if let Some(voice) = current_voice(&state) {
                voice.set_muted(muted);
            }
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
            if let Some(voice) = current_voice(&state) {
                voice.set_deafened(deafened);
                voice.set_muted(app.get_self_muted() || deafened);
            }
        });
    }

    {
        let state = Arc::clone(state);
        let weak = app.as_weak();
        app.on_leave(move || {
            if let Some(client) = current(&state) {
                client.disconnect();
            }
            state.lock().expect("state poisoned").voice = None;
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
fn current_voice(state: &Arc<Mutex<State>>) -> Option<Arc<Pipeline>> {
    state.lock().expect("state poisoned").voice.clone()
}

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
