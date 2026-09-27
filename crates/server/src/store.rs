// SPDX-License-Identifier: MPL-2.0

//! 重启之后还该在的东西，存在数据目录下的一个 SQLite 文件里。
//!
//! # 存什么、不存什么
//!
//! 存：频道树（含谁建的）、频道发号游标、按公钥定下来的角色、封禁名单、
//! 还没被用掉的管理员链接。
//!
//! 不存：谁在线、会话、文字消息。前两样本来就是一断就没的；文字消息在
//! 「明确不做」的清单里（长期消息历史），这是语音软件不是聊天软件。
//!
//! # 状态机不知道有这一层
//!
//! [`crate::state`] 一行 SQL 都没有，也不该有。它每改一次状态，就把「重启之后
//! 也该在」的那部分记成 [`Change`]；连接层在同一把锁里取走（`take_changes`），
//! 交给 [`Store::record`]。启动时反过来，读出 [`Saved`] 交给
//! [`crate::state::Server::restore`]。
//!
//! 写库必须跟改状态在同一把锁里，顺序才不会乱，见 `conn::Hub::mutate`。
//!
//! # 为什么是 SQLite
//!
//! 封禁名单和角色要的是「改一半断电也不会坏」。SQLite 的事务白给这一点，
//! 而且它是编进二进制里的（`bundled`），自部署的人不需要装任何东西。
//!
//! # 版本
//!
//! 表结构带版本号（SQLite 的 `user_version`），[`MIGRATIONS`] 一条一条往上升。
//! **只加不改**：已经发出去的迁移一个字都不能动，改了的话，已经升过级的库和
//! 新建的库就不一样了。

use std::path::Path;

use protocol::control::{Channel, Role};
use protocol::PublicKey;
use rusqlite::{params, Connection, OptionalExtension};

use crate::state::{BanEntry, Change, ChannelId, Saved};

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
    // v2：角色、封禁、管理员链接。
    //
    // role 存的是协议里 Role 的数值。secrets 放不该出现在日志里的东西，
    // 眼下只有还没被用掉的管理员链接里的码。
    "CREATE TABLE roles (
         public_key BLOB    PRIMARY KEY,
         role       INTEGER NOT NULL
     );
     CREATE TABLE bans (
         public_key   BLOB    PRIMARY KEY,
         name         TEXT    NOT NULL,
         banned_at_ms INTEGER NOT NULL,
         banned_by    TEXT    NOT NULL DEFAULT '',
         reason       TEXT    NOT NULL DEFAULT ''
     );
     CREATE TABLE secrets (
         key   TEXT PRIMARY KEY,
         value TEXT NOT NULL
     );",
];

/// 这个版本的服务端认得的最高表结构版本。
pub const SCHEMA_VERSION: u32 = MIGRATIONS.len() as u32;

const NEXT_CHANNEL_KEY: &str = "next_channel_id";
const ADMIN_CLAIM_KEY: &str = "admin_claim";

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
    ///
    /// 公钥长度不对、角色认不出来的行直接跳过：存档是个文件，会被手改。
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

        let mut stmt = self
            .conn
            .prepare("SELECT public_key, role FROM roles ORDER BY public_key")?;
        let roles = stmt
            .query_map([], |row| {
                Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, i32>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter_map(|(key, role)| Some((to_key(&key)?, Role::try_from(role).ok()?)))
            .collect();

        let mut stmt = self.conn.prepare(
            "SELECT public_key, name, banned_at_ms, banned_by, reason FROM bans ORDER BY banned_at_ms",
        )?;
        let bans = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .filter_map(|(key, name, banned_at_ms, banned_by, reason)| {
                Some(BanEntry {
                    public_key: to_key(&key)?,
                    name,
                    banned_at_ms,
                    banned_by,
                    reason,
                })
            })
            .collect();

        let admin_claim: Option<String> = self
            .conn
            .query_row(
                "SELECT value FROM secrets WHERE key = ?1",
                [ADMIN_CLAIM_KEY],
                |r| r.get(0),
            )
            .optional()?;

        Ok(Saved {
            channels,
            next_channel_id: next_channel_id.unwrap_or(0),
            roles,
            bans,
            admin_claim,
        })
    }

    /// 记下一条新的管理员链接里的码。启动时发现还没有管理员才会调。
    pub fn save_admin_claim(&mut self, code: &str) -> Result<(), StoreError> {
        self.conn.execute(
            "INSERT INTO secrets (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![ADMIN_CLAIM_KEY, code],
        )?;
        Ok(())
    }

    /// 把状态机记下的变化写进去。**一批一个事务**：删频道那一下会挪子频道、
    /// 再删自己，要么全写进去，要么全没写。
    pub fn record(&mut self, changes: &[Change]) -> Result<(), StoreError> {
        if changes.is_empty() {
            return Ok(());
        }
        let tx = self.conn.transaction()?;
        for change in changes {
            match change {
                Change::ChannelSaved(channel) => {
                    let created_by =
                        (!channel.created_by.is_empty()).then_some(&channel.created_by);
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
                    // 发号游标只往前走：删掉最大的那个频道之后重启，
                    // 也不会把它的 id 再发出去。
                    tx.execute(
                        "INSERT INTO meta (key, value) VALUES (?1, ?2)
                         ON CONFLICT(key) DO UPDATE SET value = max(value, excluded.value)",
                        params![NEXT_CHANNEL_KEY, channel.id.wrapping_add(1)],
                    )?;
                }
                Change::ChannelRemoved(id) => {
                    tx.execute("DELETE FROM channels WHERE id = ?1", [id])?;
                }
                Change::RoleSet(key, role) => {
                    tx.execute(
                        "INSERT INTO roles (public_key, role) VALUES (?1, ?2)
                         ON CONFLICT(public_key) DO UPDATE SET role = excluded.role",
                        params![key.0.as_slice(), *role as i32],
                    )?;
                }
                Change::BanAdded(entry) => {
                    tx.execute(
                        "INSERT INTO bans (public_key, name, banned_at_ms, banned_by, reason)
                         VALUES (?1, ?2, ?3, ?4, ?5)
                         ON CONFLICT(public_key) DO UPDATE SET
                             name = excluded.name,
                             banned_at_ms = excluded.banned_at_ms,
                             banned_by = excluded.banned_by,
                             reason = excluded.reason",
                        params![
                            entry.public_key.0.as_slice(),
                            entry.name,
                            entry.banned_at_ms,
                            entry.banned_by,
                            entry.reason,
                        ],
                    )?;
                }
                Change::BanRemoved(key) => {
                    tx.execute("DELETE FROM bans WHERE public_key = ?1", [key.0.as_slice()])?;
                }
                Change::AdminClaimUsed => {
                    tx.execute("DELETE FROM secrets WHERE key = ?1", [ADMIN_CLAIM_KEY])?;
                }
            }
        }
        tx.commit()?;
        Ok(())
    }
}

fn to_key(bytes: &[u8]) -> Option<PublicKey> {
    <[u8; PublicKey::LEN]>::try_from(bytes).ok().map(PublicKey)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::{Config, Server, SessionId};
    use protocol::control::CreateChannel;

    fn temp_db(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("gouhuo-store-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("gouhuo.db")
    }

    /// 改一次状态、把变化落盘，跟 `Hub::mutate` 做的一样。
    fn apply<T>(store: &mut Store, server: &mut Server, f: impl FnOnce(&mut Server) -> T) -> T {
        let out = f(server);
        store.record(&server.take_changes()).unwrap();
        out
    }

    fn create(server: &mut Server, session: SessionId, name: &str) {
        server.create_channel(
            session,
            CreateChannel {
                name: name.to_string(),
                ..Default::default()
            },
        );
    }

    fn id_of(saved: &Saved, name: &str) -> Option<ChannelId> {
        saved.channels.iter().find(|c| c.name == name).map(|c| c.id)
    }

    fn admin_server(store: &mut Store) -> (Server, SessionId) {
        let mut server = Server::restore(
            Config::default(),
            Saved {
                admin_claim: Some("claim".into()),
                ..Saved::default()
            },
        );
        let admin = apply(store, &mut server, |s| {
            s.admit(PublicKey([1; 32]), "claim", "管理员")
        })
        .unwrap()
        .session_id;
        (server, admin)
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
            apply(&mut store, &mut server, |s| create(s, session, "开黑"));
        }

        let mut store = Store::open(&path).unwrap();
        let saved = store.load().unwrap();
        let id = id_of(&saved, "开黑").expect("重启之后频道没了");
        let row = saved.channels.iter().find(|c| c.id == id).unwrap();
        assert_eq!(row.created_by, owner.0.to_vec(), "谁建的没记住");

        let mut server = Server::restore(Config::default(), saved);
        let session = server.admit(owner, "", "阿狸").unwrap().session_id;
        let events = apply(&mut store, &mut server, |s| s.delete_channel(session, id));
        assert!(!events.is_empty(), "重启之后建的人删不掉了");
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

        apply(&mut store, &mut server, |s| create(s, admin, "父"));
        let parent = id_of(&store.load().unwrap(), "父").unwrap();
        apply(&mut store, &mut server, |s| {
            s.create_channel(
                admin,
                CreateChannel {
                    name: "子".into(),
                    parent_id: parent,
                    ..Default::default()
                },
            )
        });

        apply(&mut store, &mut server, |s| s.delete_channel(admin, parent));
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
        apply(&mut store, &mut server, |s| create(s, admin, "一次性"));
        let id = id_of(&store.load().unwrap(), "一次性").unwrap();
        apply(&mut store, &mut server, |s| s.delete_channel(admin, id));

        let saved = store.load().unwrap();
        assert!(saved.channels.iter().all(|c| c.id != id));
        assert_eq!(saved.next_channel_id, id + 1);
    }

    /// 在线状态、闭麦这些不落盘。
    #[test]
    fn transient_state_is_not_written() {
        let mut store = Store::in_memory().unwrap();
        let mut server = Server::new(Config::default());
        let session = apply(&mut store, &mut server, |s| {
            s.admit(PublicKey([7; 32]), "", "阿狸")
        })
        .unwrap()
        .session_id;
        apply(&mut store, &mut server, |s| {
            s.set_self_state(session, true, false)
        });
        assert_eq!(store.load().unwrap(), Saved::default());
    }

    /// 管理员链接：存下来、被用掉之后删掉，用它的人的管理员身份存下来。
    #[test]
    fn the_admin_claim_is_saved_then_consumed() {
        let mut store = Store::in_memory().unwrap();
        store.save_admin_claim("claim").unwrap();
        assert_eq!(store.load().unwrap().admin_claim.as_deref(), Some("claim"));

        let mut server = Server::restore(Config::default(), store.load().unwrap());
        apply(&mut store, &mut server, |s| {
            s.admit(PublicKey([1; 32]), "claim", "我")
        })
        .unwrap();

        let saved = store.load().unwrap();
        assert_eq!(saved.admin_claim, None, "用过的管理员链接还在");
        assert_eq!(saved.roles, vec![(PublicKey([1; 32]), Role::Admin)]);
    }

    /// 角色、封禁都要跟着重启走。
    #[test]
    fn roles_and_bans_survive_a_restart() {
        let path = temp_db("roles-bans");
        let member = PublicKey([2; 32]);
        let troll = PublicKey([3; 32]);
        {
            let mut store = Store::open(&path).unwrap();
            let (mut server, admin) = admin_server(&mut store);
            let m = server.admit(member, "", "阿狸").unwrap().session_id;
            let t = server.admit(troll, "", "捣乱的").unwrap().session_id;
            apply(&mut store, &mut server, |s| {
                s.set_role(admin, m, Role::ChannelAdmin)
            });
            apply(&mut store, &mut server, |s| s.ban(admin, t, "刷屏", 1234));
        }

        let store = Store::open(&path).unwrap();
        let saved = store.load().unwrap();
        assert!(saved.roles.contains(&(member, Role::ChannelAdmin)));
        assert_eq!(saved.bans.len(), 1);
        assert_eq!(saved.bans[0].public_key, troll);
        assert_eq!(saved.bans[0].name, "捣乱的");
        assert_eq!(saved.bans[0].reason, "刷屏");
        assert_eq!(saved.bans[0].banned_at_ms, 1234);

        let mut server = Server::restore(Config::default(), saved);
        assert!(
            server.admit(troll, "", "换个名字").is_err(),
            "重启之后被封的人又进来了"
        );
        let welcome = server.admit(member, "", "阿狸").unwrap().welcome;
        assert_eq!(welcome.role, Role::ChannelAdmin as i32, "重启之后角色没了");
    }

    #[test]
    fn unbanning_is_persisted() {
        let mut store = Store::in_memory().unwrap();
        let (mut server, admin) = admin_server(&mut store);
        let troll = PublicKey([3; 32]);
        let t = server.admit(troll, "", "捣乱的").unwrap().session_id;
        apply(&mut store, &mut server, |s| s.ban(admin, t, "", 1));
        apply(&mut store, &mut server, |s| s.unban(admin, &troll.0));
        assert!(store.load().unwrap().bans.is_empty());
    }

    /// 存档里坏掉的行跳过，不让整个服务器起不来。
    #[test]
    fn broken_rows_are_skipped() {
        let mut store = Store::in_memory().unwrap();
        store
            .conn
            .execute_batch(
                "INSERT INTO roles VALUES (x'0102', 4);
                 INSERT INTO roles VALUES (zeroblob(32), 99);
                 INSERT INTO bans VALUES (x'01', '短公钥', 1, '', '');",
            )
            .unwrap();
        let saved = store.load().unwrap();
        assert!(saved.roles.is_empty(), "{:?}", saved.roles);
        assert!(saved.bans.is_empty());
        // 好的行照样读得出来
        store
            .record(&[Change::RoleSet(PublicKey([5; 32]), Role::Member)])
            .unwrap();
        assert_eq!(store.load().unwrap().roles.len(), 1);
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

    /// 上一版（v1）写的库要能升上来，里面的频道还在。
    #[test]
    fn a_v1_store_is_migrated_and_keeps_its_channels() {
        let path = temp_db("migrate");
        {
            let conn = Connection::open(&path).unwrap();
            conn.execute_batch(MIGRATIONS[0]).unwrap();
            conn.pragma_update(None, "user_version", 1).unwrap();
            conn.execute(
                "INSERT INTO channels (id, parent_id, name, min_role) VALUES (5, 1, '老频道', 1)",
                [],
            )
            .unwrap();
        }
        let store = Store::open(&path).unwrap();
        let saved = store.load().unwrap();
        assert_eq!(id_of(&saved, "老频道"), Some(5));
        assert!(saved.roles.is_empty() && saved.bans.is_empty());
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
