// SPDX-License-Identifier: GPL-3.0-or-later

//! 存在磁盘上的偏好设置。
//!
//! # 为什么不用 JSON / TOML
//!
//! 就这么几个值，拉一个序列化库进来不划算 —— 而且这个文件**用户会手改**：
//! 自部署这群人遇到问题的第一反应就是打开配置文件看看。一行一个
//! `键=值`，记事本打开就懂，改错了也只是那一行被忽略。
//!
//! # 坏文件不能让程序起不来
//!
//! 这个文件跟身份文件放在一起。身份文件坏了是大事（等于换了个人），
//! 偏好设置坏了不是 —— 认不出来的行直接跳过，缺的值用默认。
//! 绝不能因为有人手滑多打了个字就打不开客户端。

use std::collections::BTreeMap;
use std::io;
use std::path::{Path, PathBuf};

use voice_core::hotkey::Key;

/// 按住说话还是语音激活。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TalkMode {
    PushToTalk,
    VoiceActivity,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Settings {
    pub nick: String,
    /// 上次连的邀请链接。下次打开直接填好 —— 省掉「那条链接我放哪儿了」。
    pub last_invite: String,
    pub talk_mode: TalkMode,
    /// 按住说话绑的键。`None` 表示还没绑。
    pub ptt_key: Option<Key>,
    /// 语音激活的阈值，分贝。
    pub vad_threshold_db: f32,
    /// 用哪个麦克风。`None` 是系统默认。
    ///
    /// 存的是 WASAPI 的设备 id，重启之后还有效。设备没了会退回默认 ——
    /// 拔个耳机不该让人从此说不了话。
    pub capture_device: Option<String>,
    /// 用哪个扬声器/耳机。
    pub render_device: Option<String>,
    /// 单独给某个人调的音量，百分比。键是那个人公钥的 base32。
    ///
    /// **按公钥存，不按会话 id，也不按昵称**：会话 id 一断线就换，昵称会改、
    /// 会重名。只有公钥跨会话、跨服务器都认得出是同一个人 —— 在这个服务器上
    /// 把谁调小了，在另一个服务器上碰到他也还是小的。
    ///
    /// 100% 的不存。只存调过的，文件里就只有「这几个人我调过」。
    pub user_volumes: BTreeMap<String, u32>,
    /// 有人进出我所在的频道时响一声。
    pub cue_sounds: bool,
    /// 顺便把名字念出来（Windows 自带的语音合成）。
    ///
    /// **默认关**：一屋子人进进出出，每次都念一句会很吵，而且第一次听到
    /// 电脑突然开口说话会吓一跳。想要的人自己开。
    pub announce_names: bool,
    /// 提示音和念名字的音量，百分比。
    pub cue_volume: u32,
}

/// 提示音音量的默认值。提示音本身已经比人声轻了，再打个六折：
/// 挂几个小时，每次有人进出都响，宁可轻一点。
pub const DEFAULT_CUE_VOLUME: u32 = 60;

/// 单人音量的范围，百分比。上限跟语音链路那边的 `MAX_VOLUME` 对齐。
pub const MAX_USER_VOLUME: u32 = 400;
pub const DEFAULT_USER_VOLUME: u32 = 100;

/// 滑条上的原始值 → 存下来的百分比。
///
/// 取到 5% 一档：没人分得出 67% 和 68%，而文件里出现 67.3829 只会让手改的人困惑。
/// 100% 附近吸住 —— 拖过头想调回原样的时候，不该要人对准一个像素。
pub fn snap_volume(raw: f32) -> u32 {
    if !raw.is_finite() {
        return DEFAULT_USER_VOLUME;
    }
    let clamped = raw.clamp(0.0, MAX_USER_VOLUME as f32);
    if (clamped - DEFAULT_USER_VOLUME as f32).abs() <= 7.0 {
        return DEFAULT_USER_VOLUME;
    }
    ((clamped / 5.0).round() * 5.0) as u32
}

/// 设置文件里单人音量那几行的前缀：`volume.<公钥>=<百分比>`。
const VOLUME_PREFIX: &str = "volume.";

/// 电平条和滑块用的分贝范围。
///
/// 下限 -60 dB：再往下是本底噪声，画出来也只是一条贴着左边的线。
/// 上限 0 dB 是满刻度，超过就是削波了。
pub const MIN_DB: f32 = -60.0;
pub const MAX_DB: f32 = 0.0;

/// 分贝 → 0–1。界面上电平条和阈值刻线共用这把尺子。
pub fn db_to_level(db: f32) -> f32 {
    ((db - MIN_DB) / (MAX_DB - MIN_DB)).clamp(0.0, 1.0)
}

/// 0–1 → 分贝。
pub fn level_to_db(level: f32) -> f32 {
    MIN_DB + level.clamp(0.0, 1.0) * (MAX_DB - MIN_DB)
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            nick: String::new(),
            last_invite: String::new(),
            // 默认语音激活。默认按住说话的话，没绑键的新用户会发现
            // 怎么说都没人听见，而且完全不知道为什么。
            talk_mode: TalkMode::VoiceActivity,
            ptt_key: None,
            vad_threshold_db: DEFAULT_VAD_THRESHOLD_DB,
            capture_device: None,
            render_device: None,
            user_volumes: BTreeMap::new(),
            cue_sounds: true,
            announce_names: false,
            cue_volume: DEFAULT_CUE_VOLUME,
        }
    }
}

/// 语音激活的默认阈值。安静房间里够用；机械键盘和风扇会把它顶起来，
/// 那正是 APM 的降噪要解决的事。
pub const DEFAULT_VAD_THRESHOLD_DB: f32 = -45.0;

impl Settings {
    /// 这个人（按公钥的 base32）的音量，百分比。没调过就是 100。
    pub fn user_volume(&self, key: &str) -> u32 {
        self.user_volumes
            .get(key)
            .copied()
            .unwrap_or(DEFAULT_USER_VOLUME)
    }

    pub fn set_user_volume(&mut self, key: &str, percent: u32) {
        let percent = percent.min(MAX_USER_VOLUME);
        if percent == DEFAULT_USER_VOLUME {
            self.user_volumes.remove(key);
        } else {
            self.user_volumes.insert(key.to_string(), percent);
        }
    }

    pub fn path() -> io::Result<PathBuf> {
        // 跟身份文件放在一起，搬机器的时候一起走。
        let identity = voice_core::identity::Identity::default_path()?;
        Ok(identity
            .parent()
            .unwrap_or(Path::new("."))
            .join("settings.txt"))
    }

    /// 读。读不出来就用默认 —— 第一次跑本来就没有这个文件。
    pub fn load() -> Self {
        Self::path()
            .and_then(std::fs::read_to_string)
            .map(|text| Self::parse(&text))
            .unwrap_or_default()
    }

    pub fn parse(text: &str) -> Self {
        let mut settings = Self::default();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else {
                continue;
            };
            let value = value.trim();
            let key = key.trim();
            if let Some(who) = key.strip_prefix(VOLUME_PREFIX) {
                // 不认识的值跳过，不夹到范围里：手改成 4000 多半是手滑，
                // 当成 400% 放出来会吓人一跳。
                if !who.is_empty() && !who.contains(char::is_whitespace) {
                    if let Ok(percent) = value.parse::<u32>() {
                        if percent <= MAX_USER_VOLUME {
                            settings.set_user_volume(who, percent);
                        }
                    }
                }
                continue;
            }
            match key {
                "nick" => settings.nick = value.to_string(),
                "last_invite" => settings.last_invite = value.to_string(),
                "talk_mode" => {
                    settings.talk_mode = match value {
                        "ptt" => TalkMode::PushToTalk,
                        _ => TalkMode::VoiceActivity,
                    }
                }
                "ptt_key" => settings.ptt_key = value.parse().ok().and_then(Key::decode),
                "cue_sounds" => settings.cue_sounds = value != "off",
                "announce_names" => settings.announce_names = value == "on",
                "cue_volume" => {
                    if let Ok(percent) = value.parse::<u32>() {
                        if percent <= 100 {
                            settings.cue_volume = percent;
                        }
                    }
                }
                "capture_device" => settings.capture_device = non_empty(value),
                "render_device" => settings.render_device = non_empty(value),
                "vad_threshold_db" => {
                    // 解析不出来或者离谱的值一律退回默认 —— 一个手滑打成
                    // 正数的阈值会让用户一个字都发不出去。
                    if let Ok(db) = value.parse::<f32>() {
                        if db.is_finite() && (MIN_DB..=MAX_DB).contains(&db) {
                            settings.vad_threshold_db = db;
                        }
                    }
                }
                // 认不出来的键跳过。将来加了新设置，老版本读到也不会炸。
                _ => {}
            }
        }
        settings
    }

    pub fn save(&self) -> io::Result<()> {
        let path = Self::path()?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(&path, self.serialize())
    }

    fn serialize(&self) -> String {
        let mode = match self.talk_mode {
            TalkMode::PushToTalk => "ptt",
            TalkMode::VoiceActivity => "vad",
        };
        format!(
            "# 篝火的偏好设置。可以手改，改坏了那一行会被忽略。\n\
             nick={}\n\
             last_invite={}\n\
             talk_mode={}\n\
             ptt_key={}\n\
             vad_threshold_db={:.1}\n\
             capture_device={}\n\
             render_device={}\n\
             cue_sounds={}\n\
             announce_names={}\n\
             cue_volume={}\n",
            // 值里有换行的话会把文件切坏，所以过滤掉。
            // 昵称里的换行是粘贴时最容易带进来的东西。
            one_line(&self.nick),
            one_line(&self.last_invite),
            mode,
            self.ptt_key.map(Key::encode).unwrap_or(0),
            self.vad_threshold_db,
            one_line(self.capture_device.as_deref().unwrap_or("")),
            one_line(self.render_device.as_deref().unwrap_or("")),
            on_off(self.cue_sounds),
            on_off(self.announce_names),
            self.cue_volume,
        ) + &self.serialize_volumes()
    }

    fn serialize_volumes(&self) -> String {
        if self.user_volumes.is_empty() {
            return String::new();
        }
        let mut out = String::from(
            "# 单独调过音量的人：volume.<公钥>=<百分比>，删掉那行就回到 100%。
",
        );
        for (who, percent) in &self.user_volumes {
            out.push_str(&format!(
                "{VOLUME_PREFIX}{}={percent}
",
                one_line(who)
            ));
        }
        out
    }
}

fn on_off(value: bool) -> &'static str {
    if value {
        "on"
    } else {
        "off"
    }
}

fn non_empty(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.to_string())
}

fn one_line(value: &str) -> String {
    value.replace(['\n', '\r'], " ").trim().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Settings {
        Settings {
            nick: "阿狸".into(),
            last_invite: "gouhuo://j/abc".into(),
            talk_mode: TalkMode::PushToTalk,
            ptt_key: Some(Key::Keyboard(0x20)),
            vad_threshold_db: -38.5,
            capture_device: Some("{0.0.1.00000000}.{abc}".into()),
            render_device: None,
            user_volumes: BTreeMap::from([("AAAA".to_string(), 40), ("BBBB".to_string(), 250)]),
            cue_sounds: false,
            announce_names: true,
            cue_volume: 35,
        }
    }

    #[test]
    fn round_trips() {
        let original = sample();
        assert_eq!(Settings::parse(&original.serialize()), original);
    }

    #[test]
    fn defaults_are_usable_without_a_file() {
        let settings = Settings::parse("");
        assert_eq!(settings.talk_mode, TalkMode::VoiceActivity);
        assert_eq!(settings.ptt_key, None);
    }

    /// 默认必须是语音激活。默认按住说话的话，没绑键的新用户会发现
    /// 怎么说都没人听见，而且完全不知道为什么。
    #[test]
    fn the_default_mode_works_without_any_binding() {
        assert_eq!(Settings::default().talk_mode, TalkMode::VoiceActivity);
    }

    /// 文件坏了不能让程序起不来。
    #[test]
    fn garbage_is_skipped_not_fatal() {
        let settings = Settings::parse(
            "这是一行乱写的\n\
             nick=阿狸\n\
             = 没有键\n\
             ptt_key=不是数字\n\
             未来的设置=某个值\n\
             talk_mode=谁知道呢\n",
        );
        assert_eq!(settings.nick, "阿狸");
        assert_eq!(settings.ptt_key, None);
        // 认不出来的模式退回默认，而不是让用户发不出声
        assert_eq!(settings.talk_mode, TalkMode::VoiceActivity);
    }

    #[test]
    fn comments_and_blank_lines_are_ignored() {
        let settings = Settings::parse("# 注释\n\n  \nnick=波波\n");
        assert_eq!(settings.nick, "波波");
    }

    /// 昵称里粘进换行的话不能把文件切坏。
    #[test]
    fn newlines_in_values_do_not_corrupt_the_file() {
        let settings = Settings {
            nick: "阿狸\nptt_key=999".into(),
            ..sample()
        };
        let parsed = Settings::parse(&settings.serialize());
        assert!(!parsed.nick.contains('\n'));
        assert_eq!(
            parsed.ptt_key,
            Some(Key::Keyboard(0x20)),
            "换行把别的设置覆盖掉了"
        );
    }

    #[test]
    fn mouse_buttons_survive_the_round_trip() {
        let settings = Settings {
            ptt_key: Some(Key::Mouse(5)),
            ..sample()
        };
        assert_eq!(
            Settings::parse(&settings.serialize()).ptt_key,
            Some(Key::Mouse(5))
        );
    }

    /// 没设设备的时候要是 None，不能是空字符串 —— 空字符串会被当成
    /// 一个不存在的设备 id 去打开。
    #[test]
    fn an_unset_device_is_none_not_empty() {
        let settings = Settings::parse("capture_device=\nrender_device=\n");
        assert_eq!(settings.capture_device, None);
        assert_eq!(settings.render_device, None);
    }

    /// 离谱的阈值不能让用户一个字都发不出去。
    #[test]
    fn an_absurd_threshold_falls_back_to_the_default() {
        for value in ["999", "-999", "abc", "NaN", "inf"] {
            let settings = Settings::parse(&format!("vad_threshold_db={value}\n"));
            assert_eq!(
                settings.vad_threshold_db, DEFAULT_VAD_THRESHOLD_DB,
                "阈值 `{value}` 该退回默认"
            );
        }
    }

    /// 电平条和滑块共用一把尺子，两边换算必须对得上。
    #[test]
    fn the_decibel_scale_round_trips() {
        for db in [MIN_DB, -45.0, -20.0, MAX_DB] {
            assert!((level_to_db(db_to_level(db)) - db).abs() < 0.01, "{db}");
        }
        // 超出范围的要夹住，不能跑出条子外面
        assert_eq!(db_to_level(-200.0), 0.0);
        assert_eq!(db_to_level(50.0), 1.0);
    }

    #[test]
    fn user_volumes_default_to_full_and_forget_resets() {
        let mut settings = Settings::default();
        assert_eq!(settings.user_volume("AAAA"), 100);
        settings.set_user_volume("AAAA", 30);
        assert_eq!(settings.user_volume("AAAA"), 30);
        // 调回 100 就不再记着这个人
        settings.set_user_volume("AAAA", 100);
        assert!(settings.user_volumes.is_empty());
        // 超上限的夹住
        settings.set_user_volume("BBBB", 9999);
        assert_eq!(settings.user_volume("BBBB"), MAX_USER_VOLUME);
    }

    #[test]
    fn the_slider_snaps_to_steps_and_to_full_volume() {
        assert_eq!(snap_volume(0.0), 0);
        assert_eq!(snap_volume(2.4), 0);
        assert_eq!(snap_volume(67.3829), 65);
        assert_eq!(snap_volume(94.0), 100);
        assert_eq!(snap_volume(106.9), 100);
        assert_eq!(snap_volume(250.0), 250);
        assert_eq!(snap_volume(999.0), MAX_USER_VOLUME);
        assert_eq!(snap_volume(-3.0), 0);
        assert_eq!(snap_volume(f32::NAN), DEFAULT_USER_VOLUME);
    }

    /// 静音某个人（0%）也要存下来，下次进来还是静音的。
    #[test]
    fn a_muted_person_stays_muted() {
        let mut settings = Settings::default();
        settings.set_user_volume("AAAA", 0);
        assert_eq!(
            Settings::parse(&settings.serialize()).user_volume("AAAA"),
            0
        );
    }

    #[test]
    fn absurd_volumes_are_skipped() {
        let settings = Settings::parse(
            "volume.AAAA=4000
             volume.BBBB=-5
             volume.CCCC=很大
             volume.=50
             volume.DD DD=50
             volume.EEEE=60
",
        );
        assert_eq!(
            settings.user_volumes,
            BTreeMap::from([("EEEE".to_string(), 60)])
        );
    }

    /// 提示音默认开、念名字默认关；写坏了也退回这两个默认。
    #[test]
    fn cues_default_to_a_chime_without_speech() {
        let settings = Settings::parse("cue_volume=999\ncue_volume=-1\n");
        assert!(settings.cue_sounds);
        assert!(!settings.announce_names);
        assert_eq!(settings.cue_volume, DEFAULT_CUE_VOLUME);

        let garbled = Settings::parse("cue_sounds=也许\nannounce_names=也许\n");
        assert!(garbled.cue_sounds, "认不出来的值不该把提示音关掉");
        assert!(!garbled.announce_names, "认不出来的值不该让电脑突然开口");
    }

    /// 设置文件跟身份文件放在一起，搬机器的时候一起走。
    #[test]
    fn lives_next_to_the_identity_file() {
        let (Ok(settings), Ok(identity)) = (
            Settings::path(),
            voice_core::identity::Identity::default_path(),
        ) else {
            return; // 没有 %APPDATA% 的环境，跳过
        };
        assert_eq!(settings.parent(), identity.parent());
    }
}
