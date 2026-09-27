// SPDX-License-Identifier: MPL-2.0

//! 重启之后还该在的东西，存在数据目录下的一个 SQLite 文件里。
//!
//! # 存什么、不存什么
//!
//! 存：频道树（含谁建的）和频道发号游标。
//!
//! 不存：谁在线、会话、文字消息。前两样本来就是一断就没的；文字消息在
//! 「明确不做」的清单里（长期消息历史），这是语音软件不是聊天软件。
//!
//! # 状态机不知道有这一层
//!
//! [`crate::state`] 一行 SQL 都没有，也不该有。它的每次变化本来就以
//! [`Broadcast`] 的形式说出来了，而频道的每一次变化（建、挪、删）都是一条
//! 带着完整信息的 `ChannelState`。所以这里只做一件事：**把那些广播翻译成
//! 写库**。启动时反过来，读出 [`Saved`] 交给 [`crate::state::Server::restore`]。
//!
//! 写库必须跟改状态在同一把锁里，顺序才不会乱，见 `conn::Hub::mutate`。
//!
//! # 为什么是 SQLite
//!
//! 眼下只有频道，一个文本文件也够用。但接下来要存的是封禁名单和角色（#6），
//! 那些要的是「改一半断电也不会坏」。SQLite 的事务白给这一点，而且它是
//! 编进二进制里的（`bundled`），自部署的人不需要装任何东西。
//!
//! # 版本
//!
//! 表结构带版本号（SQLite 的 `user_version`），[`MIGRATIONS`] 一条一条往上升。
//! **只加不改**：已经发出去的迁移一个字都不能动，改了的话，已经升过级的库和
//! 新建的库就不一样了。

use std::path::Path;

use protocol::control::{server_message, Channel};
use rusqlite::{params, Connection, OptionalExtension};

use crate::state::{Broadcast, ChannelId, Saved};

/// 表结构的迁移，第 n 条把库从版本 n 升到 n+1。**只能往后加。**
const MIGRATIONS: &[&str] = &[
    // v1：频道树。
    //
    // created_by 是建的人的公钥（32 字节）。NULL = 服务器自带的根频道。
    // meta 是个键值表，眼下只放频道发号游标。
    "CREATE TABLE channels (
         id          INTEGER PRIMARY KEY,
         parent_id   INTEGER NOT NULL,
         name        TEXT    NOT NULL,
         description TEXT    NOT NULL DEFAULT '',
         max_users   INTEGER NOT NULL DEFAULT 0,
         min_role    INTEGER NOT NULL,
         created_by  BLOB
     );
     CREATE TABLE meta (
         key   TEXT PRIMARY KEY,
         value INTEGER NOT NULL
     );",
];

/// 这个版本的服务端认得的最高表结构版本。
pub const SCHEMA_VERSION: u32 = MIGRATIONS.len() as u32;

const NEXT_CHANNEL_KEY: &str = "next_channel_id";

pub struct Store {
    conn: Connection,
}

/// 存档出了问题。每一条都要能直接打给自部署的人看。
#[derive(Debug)]
pub enum StoreError {
    Sqlite(rusqlite::Error),
    /// 库是更新版本的服务端写的。**不能硬开**：旧代码不认得新表结构，
    /// 往里写只会把它写坏。
    TooNew {
        found: u32,
        known: u32,
    },
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StoreError::Sqlite(e) => write!(f, "存档读写失败：{e}"),
            StoreError::TooNew { found, known } => write!(
                f,
                "存档是更新版本的服务端写的（表结构 v{found}，这个版本只认得到 v{known}）。\
                 升级服务端再启动；别用旧版本硬开，会把存档写坏。"
            ),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<rusqlite::Error> for StoreError {
    fn from(e: rusqlite::Error) -> Self {
        StoreError::Sqlite(e)
    }
}

impl Store {
    /// 打开（没有就建）存档，升到最新表结构。
    pub fn open(path: &Path) -> Result<Self, StoreError> {
        Self::init(Connection::open(path)?)
    }

    /// 只在内存里的存档，测试用。
    pub fn in_memory() -> Result<Self, StoreError> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(mut conn: Connection) -> Result<Self, StoreError> {
        // 写前日志：写到一半断电，库也还是改之前或改之后的样子，不会是半截。
        // 内存库不支持 WAL，会停在 memory 模式，不影响。
        let _: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;

        let found: u32 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if found > SCHEMA_VERSION {
            return Err(StoreError::TooNew {
                found,
                known: SCHEMA_VERSION,
            });
        }
        for (version, sql) in MIGRATIONS.iter().enumerate().skip(found as usize) {
            // 每一步迁移连同版本号一起提交：升到一半断电，下次从断的那一步接着来。
            let tx = conn.transaction()?;
            tx.execute_batch(sql)?;
            tx.pragma_update(None, "user_version", version as u32 + 1)?;
            tx.commit()?;
        }
        Ok(Self { conn })
    }

    /// 读出存档，交给 `Server::restore`。
    pub fn load(&self) -> Result<Saved, StoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, parent_id, name, description, max_users, min_role, created_by
             FROM channels ORDER BY id",
        )?;
        let channels = stmt
            .query_map([], |row| {
                Ok(Channel {
                    id: row.get(0)?,
                    parent_id: row.get(1)?,
                    name: row.get(2)?,
                    description: row.get(3)?,
                    max_users: row.get(4)?,
                    min_role: row.get(5)?,
                    created_by: row.get::<_, Option<Vec<u8>>>(6)?.unwrap_or_default(),
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        let next_channel_id: Option<ChannelId> = self
            .conn
            .query_row(
                "SELECT value FROM meta WHERE key = ?1",
                [NEXT_CHANNEL_KEY],
                |r| r.get(0),
            )
            .optional()?;

        Ok(Saved {
            channels,
            next_channel_id: next_channel_id.unwrap_or(0),
        })
    }

    /// 把一批广播里要落盘的部分写进去。**一批一个事务**：删频道那一下会挪人、
    /// 挪子频道、再删自己，要么全写进去，要么全没写。
    ///
    /// 眼下落盘的只有 `ChannelState`：频道的建、挪、删全都是它。
    pub fn record(&mut self, events: &[Broadcast]) -> Result<(), StoreError> {
        let changes: Vec<(&Channel, bool)> = events
            .iter()
            .filter_map(|event| {
                let message = match event {
                    Broadcast::Everyone(m) | Broadcast::Channel(_, m) | Broadcast::One(_, m) => m,
                };
                match &message.payload {
                    Some(server_message::Payload::ChannelState(state)) => {
                        state.channel.as_ref().map(|c| (c, state.removed))
                    }
                    _ => None,
                }
            })
            .collect();
        if changes.is_empty() {
            return Ok(());
        }

        let tx = self.conn.transaction()?;
        for (channel, removed) in changes {
            if removed {
                tx.execute("DELETE FROM channels WHERE id = ?1", [channel.id])?;
                continue;
            }
            let created_by = (!channel.created_by.is_empty()).then_some(&channel.created_by);
            tx.execute(
                "INSERT INTO channels (id, parent_id, name, description, max_users, min_role, created_by)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
                 ON CONFLICT(id) DO UPDATE SET
                     parent_id = excluded.parent_id,
                     name = excluded.name,
                     description = excluded.description,
                     max_users = excluded.max_users,
                     min_role = excluded.min_role,
                     created_by = excluded.created_by",
                params![
                    channel.id,
                    channel.parent_id,
                    channel.name,
                    channel.description,
                    channel.max_users,
                    channel.min_role,
                    created_by,
                ],
            )?;
            // 发号游标只往前走：删掉最大的那个频道之后重启，也不会把它的 id 再发出去。
            tx.execute(
                "INSERT INTO meta (key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = max(value, excluded.value)",
                params![NEXT_CHANNEL_KEY, channel.id.wrapping_add(1)],
            )?;
        }
        tx.commit()?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Config, Server, SessionId};
    use protocol::control::CreateChannel;
    use protocol::PublicKey;

    fn temp_db(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("gouhuo-store-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("gouhuo.db")
    }

    fn create(server: &mut Server, session: SessionId, name: &str) -> Vec<Broadcast> {
        server.create_channel(
            session,
            CreateChannel {
                name: name.to_string(),
                ..Default::default()
            },
        )
    }

    fn id_of(saved: &Saved, name: &str) -> Option<ChannelId> {
        saved.channels.iter().find(|c| c.name == name).map(|c| c.id)
    }

    /// 整条路走一遍：建 → 落盘 → 关掉 → 重新打开 → 恢复 → 建的人还能删。
    #[test]
    fn channels_survive_a_restart() {
        let path = temp_db("restart");
        let owner = PublicKey([7; 32]);

        {
            let mut store = Store::open(&path).unwrap();
            let mut server = Server::restore(Config::default(), store.load().unwrap());
            let session = server.admit(owner, "", "阿狸").unwrap().session_id;
            store.record(&create(&mut server, session, "开黑")).unwrap();
        }

        let mut store = Store::open(&path).unwrap();
        let saved = store.load().unwrap();
        let id = id_of(&saved, "开黑").expect("重启之后频道没了");
        let row = saved.channels.iter().find(|c| c.id == id).unwrap();
        assert_eq!(row.created_by, owner.0.to_vec(), "谁建的没记住");

        let mut server = Server::restore(Config::default(), saved);
        let session = server.admit(owner, "", "阿狸").unwrap().session_id;
        let events = server.delete_channel(session, id);
        assert!(!events.is_empty(), "重启之后建的人删不掉了");
        store.record(&events).unwrap();
        assert_eq!(id_of(&store.load().unwrap(), "开黑"), None, "删了没落盘");
    }

    /// 删频道会把子频道挪到根上 —— 挪动也得落盘，不然重启之后子频道
    /// 指着一个不存在的父频道。
    #[test]
    fn deleting_a_parent_persists_the_moved_children() {
        let mut store = Store::in_memory().unwrap();
        let mut server = Server::new(Config::default());
        let admin = server
            .admit(PublicKey([7; 32]), "", "阿狸")
            .unwrap()
            .session_id;

        store.record(&create(&mut server, admin, "父")).unwrap();
        let parent = id_of(&store.load().unwrap(), "父").unwrap();
        store
            .record(&server.create_channel(
                admin,
                CreateChannel {
                    name: "子".into(),
                    parent_id: parent,
                    ..Default::default()
                },
            ))
            .unwrap();

        store.record(&server.delete_channel(admin, parent)).unwrap();
        let saved = store.load().unwrap();
        let child = saved.channels.iter().find(|c| c.name == "子").unwrap();
        assert_eq!(child.parent_id, server.root_channel());
        assert_eq!(id_of(&saved, "父"), None);
    }

    /// 删掉最大的那个再重启，它的 id 不能被再发出去。
    #[test]
    fn the_id_cursor_never_goes_back() {
        let mut store = Store::in_memory().unwrap();
        let mut server = Server::new(Config::default());
        let admin = server
            .admit(PublicKey([7; 32]), "", "阿狸")
            .unwrap()
            .session_id;
        store.record(&create(&mut server, admin, "一次性")).unwrap();
        let id = id_of(&store.load().unwrap(), "一次性").unwrap();
        store.record(&server.delete_channel(admin, id)).unwrap();

        let saved = store.load().unwrap();
        assert!(saved.channels.iter().all(|c| c.id != id));
        assert_eq!(saved.next_channel_id, id + 1);
    }

    /// 在线状态、文字消息这些不落盘。
    #[test]
    fn only_channel_changes_are_written() {
        let mut store = Store::in_memory().unwrap();
        let mut server = Server::new(Config::default());
        let admission = server.admit(PublicKey([7; 32]), "", "阿狸").unwrap();
        store.record(&admission.broadcasts).unwrap();
        store
            .record(&server.set_self_state(admission.session_id, true, false))
            .unwrap();
        assert_eq!(store.load().unwrap(), Saved::default());
    }

    #[test]
    fn a_fresh_store_is_at_the_latest_schema_and_reopening_is_harmless() {
        let path = temp_db("schema");
        drop(Store::open(&path).unwrap());
        let store = Store::open(&path).unwrap();
        let version: u32 = store
            .conn
            .query_row("PRAGMA user_version", [], |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);
    }

    /// 新版本服务端写的库，旧版本不能硬开。
    #[test]
    fn a_store_from_the_future_is_refused() {
        let path = temp_db("future");
        {
            let store = Store::open(&path).unwrap();
            store
                .conn
                .pragma_update(None, "user_version", SCHEMA_VERSION + 1)
                .unwrap();
        }
        match Store::open(&path) {
            Err(StoreError::TooNew { found, known }) => {
                assert_eq!((found, known), (SCHEMA_VERSION + 1, SCHEMA_VERSION));
            }
            other => panic!("该拒掉，结果是 {:?}", other.map(|_| ())),
        }
        let text = StoreError::TooNew { found: 9, known: 1 }.to_string();
        assert!(text.contains("升级服务端"), "{text}");
    }

    /// 数据目录里放着一个不是 SQLite 的同名文件：要报错，不能当成空库覆盖掉。
    #[test]
    fn a_garbage_file_is_an_error_not_a_fresh_start() {
        let path = temp_db("garbage");
        std::fs::write(
            &path,
            b"this is definitely not a sqlite database, just some text",
        )
        .unwrap();
        assert!(Store::open(&path).is_err());
        let still_there = std::fs::read(&path).unwrap();
        assert!(
            still_there.starts_with(b"this is definitely"),
            "原文件被改了"
        );
    }
}
