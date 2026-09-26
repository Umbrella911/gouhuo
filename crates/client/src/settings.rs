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
}

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
        }
    }
}

/// 语音激活的默认阈值。安静房间里够用；机械键盘和风扇会把它顶起来，
/// 那正是 APM 的降噪要解决的事。
pub const DEFAULT_VAD_THRESHOLD_DB: f32 = -45.0;

impl Settings {
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
            match key.trim() {
                "nick" => settings.nick = value.to_string(),
                "last_invite" => settings.last_invite = value.to_string(),
                "talk_mode" => {
                    settings.talk_mode = match value {
                        "ptt" => TalkMode::PushToTalk,
                        _ => TalkMode::VoiceActivity,
                    }
                }
                "ptt_key" => settings.ptt_key = value.parse().ok().and_then(Key::decode),
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
             render_device={}\n",
            // 值里有换行的话会把文件切坏，所以过滤掉。
            // 昵称里的换行是粘贴时最容易带进来的东西。
            one_line(&self.nick),
            one_line(&self.last_invite),
            mode,
            self.ptt_key.map(Key::encode).unwrap_or(0),
            self.vad_threshold_db,
            one_line(self.capture_device.as_deref().unwrap_or("")),
            one_line(self.render_device.as_deref().unwrap_or("")),
        )
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
