//! 剪贴板 / 快捷发送本地存储（SQLite）。
//!
//! 表结构与 Android 版完全对齐：同名表 `clipboard_entries`、同名字段、
//! 相同默认值与索引，`PRAGMA user_version = 3` 对齐 Android 数据库 v3，
//! 因此 Android 与 macOS 之间的 db 文件可以直接互换。
//!
//! 语义对齐 Android Room DAO（`ClipboardDao`）：
//! - 普通剪贴板条目按 `text` 去重（命中 → 刷新 timestamp 置顶），上限 1000，
//!   超出裁剪最旧的未置顶条目；
//! - 快捷发送条目（`isQuickSend = 1`，默认置顶）同样按文本去重，上限 20；
//! - 置顶条目永不被自动裁剪。

use std::path::Path;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use rusqlite::{params, Connection, OptionalExtension, Transaction};

/// 普通剪贴板条目上限（Android: `MAX_ITEMS = 1000`）。
pub const MAX_CLIPBOARD_ITEMS: i64 = 1000;
/// 快捷发送条目上限（Android: `MAX_QUICK_SEND_ITEMS = 20`）。
pub const MAX_QUICK_SEND_ITEMS: i64 = 20;
/// Android 数据库当前版本（v3：已含 `consumed`、`code` 两列）。
const SCHEMA_VERSION: i64 = 3;

/// 项目全局存储单例（输入法进程内唯一）。
static STORE: OnceLock<Arc<ClipboardStore>> = OnceLock::new();

/// 单条剪贴板/快捷发送条目（对应 Android `ClipboardEntry`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClipboardEntry {
    pub id: i64,
    pub text: String,
    /// 快捷发送触发编码（如 "dh"），普通剪贴板条目为空串。
    pub code: String,
    /// 时间戳（Unix 毫秒，对齐 `System.currentTimeMillis()`）。
    pub timestamp: i64,
    /// 置顶：置顶条目永不被自动裁剪。
    pub is_pinned: bool,
    /// false = 普通剪贴板条目；true = 快捷发送条目。
    pub is_quick_send: bool,
    /// 候选栏"已消费"标记（Android 语义，macOS 面板上屏时同样置位）。
    pub consumed: bool,
}

impl ClipboardEntry {
    /// 是否为快捷发送条目（与 Android 相同命名）。
    pub fn is_quick_send(&self) -> bool {
        self.is_quick_send
    }
}

/// 剪贴板/快捷发送存储。
///
/// 参考 `xime-sync-store` 的 `HistoryRepo` 模式：`Mutex<Connection>` 保证
/// Send+Sync，手写 SQL（无 ORM）。所有查询按 timestamp 倒序，先新后旧。
pub struct ClipboardStore {
    conn: Mutex<Connection>,
}

/// 初始化全局存储（幂等，第二个调用直接返回已有实例）。
/// `db_dir` 不存在时自动创建；打开后自动执行 schema 迁移。
pub fn init(db_dir: impl AsRef<Path>) -> Arc<ClipboardStore> {
    STORE
        .get_or_init(|| {
            let dir = db_dir.as_ref();
            if let Err(e) = std::fs::create_dir_all(dir) {
                eprintln!("[xime-clipboard] create dir failed: {e}");
            }
            let db_path = dir.join("clipboard.db");
            match ClipboardStore::open(db_path) {
                Ok(store) => Arc::new(store),
                Err(e) => {
                    eprintln!("[xime-clipboard] open db failed: {e}");
                    // 打开失败也提供一个（空）实例，避免后续面板操作 panic。
                    Arc::new(ClipboardStore {
                        conn: Mutex::new(Connection::open_in_memory().expect("in-memory sqlite")),
                    })
                }
            }
        })
        .clone()
}

/// 取全局存储实例（未调用 `init` 时返回 None）。
pub fn store() -> Option<Arc<ClipboardStore>> {
    STORE.get().cloned()
}

/// 当前时间（Unix 毫秒，对齐 Android `System.currentTimeMillis()`）。
pub fn now_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

impl ClipboardStore {
    /// 打开（或创建）数据库并执行迁移到最新 schema。
    pub fn open(db_path: impl AsRef<Path>) -> rusqlite::Result<Self> {
        let conn = Connection::open(db_path)?;
        migrate(&conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
        })
    }

    /// 最近普通剪贴板条目（timestamp 倒序，最多 `limit` 条）。
    pub fn list_clipboard(&self, limit: i64) -> rusqlite::Result<Vec<ClipboardEntry>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, text, code, timestamp, isPinned, isQuickSend, consumed
             FROM clipboard_entries
             WHERE isQuickSend = 0
             ORDER BY timestamp DESC, id DESC
             LIMIT ?1",
        )?;
        let rows = stmt.query_map(params![limit], row_to_entry)?;
        rows.collect()
    }

    /// 全部快捷发送条目（timestamp 倒序）。
    pub fn list_quick_send(&self) -> rusqlite::Result<Vec<ClipboardEntry>> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = conn.prepare(
            "SELECT id, text, code, timestamp, isPinned, isQuickSend, consumed
             FROM clipboard_entries
             WHERE isQuickSend = 1
             ORDER BY timestamp DESC, id DESC",
        )?;
        let rows = stmt.query_map([], row_to_entry)?;
        rows.collect()
    }

    /// 按 id 查找条目。
    pub fn find_by_id(&self, id: i64) -> rusqlite::Result<Option<ClipboardEntry>> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT id, text, code, timestamp, isPinned, isQuickSend, consumed
             FROM clipboard_entries WHERE id = ?1 LIMIT 1",
            params![id],
            row_to_entry,
        )
        .optional()
    }

    /// 普通剪贴板：按 text 去重插入/刷新，超出上限裁剪最旧未置顶条目
    /// （对应 Android `upsertAndTrim`）。
    pub fn upsert_and_trim(&self, text: &str, now: i64) -> rusqlite::Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let existing = find_by_text(&tx, text, false)?;
        if let Some(id) = existing {
            update_timestamp(&tx, id, now)?;
        } else {
            tx.execute(
                "INSERT INTO clipboard_entries (text, code, timestamp, isPinned, isQuickSend, consumed)
                 VALUES (?1, '', ?2, 0, 0, 0)",
                params![text, now],
            )?;
            trim_unpinned(&tx)?;
        }
        tx.commit()
    }

    /// 把普通剪贴板条目加入快捷发送（复制为快捷发送条目，默认置顶；
    /// 对应 Android `addQuickSend`）。
    pub fn add_to_quick_send(&self, source_id: i64, now: i64) -> rusqlite::Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        let source: Option<String> = tx
            .query_row(
                "SELECT text FROM clipboard_entries WHERE id = ?1 AND isQuickSend = 0",
                params![source_id],
                |r| r.get(0),
            )
            .optional()?;
        let Some(text) = source else {
            return Ok(());
        };
        if let Some(id) = find_by_text(&tx, &text, true)? {
            update_timestamp(&tx, id, now)?;
        } else {
            tx.execute(
                "INSERT INTO clipboard_entries (text, code, timestamp, isPinned, isQuickSend, consumed)
                 VALUES (?1, '', ?2, 1, 1, 0)",
                params![&text, now],
            )?;
            trim_quick_send(&tx)?;
        }
        let _ = text;
        tx.commit()
    }

    /// 新建/更新快捷发送条目（手填文本 + 触发编码；对应 Android `insertQuickSend`）。
    pub fn insert_quick_send(&self, text: &str, code: &str, now: i64) -> rusqlite::Result<()> {
        let mut conn = self.conn.lock().unwrap();
        let tx = conn.transaction()?;
        if let Some(id) = find_by_text(&tx, text, true)? {
            tx.execute(
                "UPDATE clipboard_entries SET text = ?1, code = ?2, timestamp = ?3 WHERE id = ?4",
                params![text, code, now, id],
            )?;
        } else {
            tx.execute(
                "INSERT INTO clipboard_entries (text, code, timestamp, isPinned, isQuickSend, consumed)
                 VALUES (?1, ?2, ?3, 1, 1, 0)",
                params![text, code, now],
            )?;
            trim_quick_send(&tx)?;
        }
        tx.commit()
    }

    /// 更新快捷发送条目的文本与编码（对应 Android `updateQuickSendItem`）。
    pub fn update_quick_send_item(
        &self,
        id: i64,
        text: &str,
        code: &str,
        now: i64,
    ) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE clipboard_entries SET text = ?1, code = ?2, timestamp = ?3 WHERE id = ?4 AND isQuickSend = 1",
            params![text, code, now, id],
        )?;
        Ok(())
    }

    /// 刷新条目时间戳（置顶语义：时间靠前 = 排在列表最前）。
    pub fn update_timestamp(&self, id: i64, now: i64) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        update_timestamp(&conn, id, now)
    }

    /// 标记条目已消费（对应 Android `markConsumed`）。
    pub fn mark_consumed(&self, id: i64) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "UPDATE clipboard_entries SET consumed = 1 WHERE id = ?1",
            params![id],
        )?;
        Ok(())
    }

    /// 删除普通剪贴板条目（不影响快捷发送）。
    pub fn delete_clipboard(&self, id: i64) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM clipboard_entries WHERE isQuickSend = 0 AND id = ?1",
            params![id],
        )?;
        Ok(())
    }

    /// 删除快捷发送条目（不影响普通剪贴板）。
    pub fn delete_quick_send(&self, id: i64) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute(
            "DELETE FROM clipboard_entries WHERE isQuickSend = 1 AND id = ?1",
            params![id],
        )?;
        Ok(())
    }

    /// 清空普通剪贴板（保留快捷发送）。
    pub fn clear_clipboard(&self) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM clipboard_entries WHERE isQuickSend = 0", [])?;
        Ok(())
    }

    /// 清空快捷发送（保留普通剪贴板）。
    pub fn clear_quick_send(&self) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM clipboard_entries WHERE isQuickSend = 1", [])?;
        Ok(())
    }

    /// 清空全部条目。
    pub fn clear_all(&self) -> rusqlite::Result<()> {
        let conn = self.conn.lock().unwrap();
        conn.execute("DELETE FROM clipboard_entries", [])?;
        Ok(())
    }

    /// 普通剪贴板条目总数。
    pub fn count_clipboard(&self) -> rusqlite::Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM clipboard_entries WHERE isQuickSend = 0",
            [],
            |r| r.get(0),
        )
    }

    /// 快捷发送条目总数。
    pub fn count_quick_send(&self) -> rusqlite::Result<i64> {
        let conn = self.conn.lock().unwrap();
        conn.query_row(
            "SELECT COUNT(*) FROM clipboard_entries WHERE isQuickSend = 1",
            [],
            |r| r.get(0),
        )
    }
}

fn row_to_entry(row: &rusqlite::Row) -> rusqlite::Result<ClipboardEntry> {
    Ok(ClipboardEntry {
        id: row.get("id")?,
        text: row.get("text")?,
        code: row.get("code")?,
        timestamp: row.get("timestamp")?,
        is_pinned: row.get("isPinned")?,
        is_quick_send: row.get("isQuickSend")?,
        consumed: row.get("consumed")?,
    })
}

fn find_by_text(
    conn: &Connection,
    text: &str,
    is_quick_send: bool,
) -> rusqlite::Result<Option<i64>> {
    conn.query_row(
        "SELECT id FROM clipboard_entries
         WHERE text = ?1 AND isQuickSend = ?2 LIMIT 1",
        params![text, is_quick_send as i64],
        |r| r.get(0),
    )
    .optional()
}

fn update_timestamp(conn: &Connection, id: i64, now: i64) -> rusqlite::Result<()> {
    conn.execute(
        "UPDATE clipboard_entries SET timestamp = ?1 WHERE id = ?2",
        params![now, id],
    )?;
    Ok(())
}

/// 裁剪最旧的未置顶条目（置顶条目永不被自动裁剪；对应 Android `trimUnpinned`）。
fn trim_unpinned(tx: &Transaction) -> rusqlite::Result<()> {
    let unpinned: i64 = tx.query_row(
        "SELECT COUNT(*) FROM clipboard_entries WHERE isPinned = 0",
        [],
        |r| r.get(0),
    )?;
    if unpinned > MAX_CLIPBOARD_ITEMS {
        tx.execute(
            "DELETE FROM clipboard_entries
             WHERE isPinned = 0
             AND id IN (
                 SELECT id FROM clipboard_entries
                 WHERE isPinned = 0 ORDER BY timestamp ASC LIMIT ?1
             )",
            params![unpinned - MAX_CLIPBOARD_ITEMS],
        )?;
    }
    Ok(())
}

/// 裁剪最旧的快捷发送条目（同称 Android `trimQuickSend`）。
fn trim_quick_send(tx: &Transaction) -> rusqlite::Result<()> {
    let count: i64 = tx.query_row(
        "SELECT COUNT(*) FROM clipboard_entries WHERE isQuickSend = 1",
        [],
        |r| r.get(0),
    )?;
    if count > MAX_QUICK_SEND_ITEMS {
        tx.execute(
            "DELETE FROM clipboard_entries
             WHERE isQuickSend = 1
             AND id IN (
                 SELECT id FROM clipboard_entries
                 WHERE isQuickSend = 1 ORDER BY timestamp ASC LIMIT ?1
             )",
            params![count - MAX_QUICK_SEND_ITEMS],
        )?;
    }
    Ok(())
}

/// Schema 迁移：对齐 Android Room 的 v1→v2→v3 演进。
///
/// - v0（新库）：按 v1 结构建表 + text 索引，随后走 v2/v3 迁移补列；
/// - v1→v2：`consumed` 列（Android MIGRATION_1_2）；
/// - v2→v3：`code` 列（Android MIGRATION_2_3）。
fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    let version: i64 = conn.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if version == 0 {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS clipboard_entries (
                id          INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
                text        TEXT NOT NULL,
                timestamp   INTEGER NOT NULL DEFAULT 0,
                isPinned    INTEGER NOT NULL DEFAULT 0,
                isQuickSend INTEGER NOT NULL DEFAULT 0
            );
            CREATE INDEX IF NOT EXISTS index_clipboard_entries_text
                ON clipboard_entries(text);",
        )?;
    }
    if version < 2 {
        conn.execute_batch(
            "ALTER TABLE clipboard_entries
             ADD COLUMN consumed INTEGER NOT NULL DEFAULT 0",
        )?;
    }
    if version < 3 {
        conn.execute_batch(
            "ALTER TABLE clipboard_entries
             ADD COLUMN code TEXT NOT NULL DEFAULT ''",
        )?;
    }
    if version != SCHEMA_VERSION {
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::path::Path;

    fn open_in(dir: &Path) -> ClipboardStore {
        ClipboardStore::open(dir.join("clipboard.db")).unwrap()
    }

    fn column_names(conn: &Connection) -> HashSet<String> {
        let mut stmt = conn
            .prepare("PRAGMA table_info(clipboard_entries)")
            .unwrap();
        let names: Vec<String> = stmt
            .query_map([], |r| r.get::<_, String>(1))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        names.into_iter().collect()
    }

    #[test]
    fn open_creates_v3_schema() {
        let dir = tempfile::tempdir().unwrap();
        let store = open_in(dir.path());
        let conn = store.conn.lock().unwrap();

        let version: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, SCHEMA_VERSION);

        // 列名与 Android Room 生成的 v3 表完全一致（camelCase 列名）。
        let cols = column_names(&conn);
        let expected: HashSet<String> = [
            "id",
            "text",
            "code",
            "timestamp",
            "isPinned",
            "isQuickSend",
            "consumed",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        assert_eq!(cols, expected);

        // text 列上有索引（对齐 Android `Index(value = ["text"])`）。
        let index_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'index' AND name = 'index_clipboard_entries_text'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(index_count, 1);
    }

    #[test]
    fn migrate_v1_and_v2_dbs() {
        // 手工构造 Android v1 库
        let dir = tempfile::tempdir().unwrap();
        let v1_path = dir.path().join("clipboard_v1.db");
        {
            let conn = Connection::open(&v1_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE clipboard_entries (
                    id          INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
                    text        TEXT NOT NULL,
                    timestamp   INTEGER NOT NULL DEFAULT 0,
                    isPinned    INTEGER NOT NULL DEFAULT 0,
                    isQuickSend INTEGER NOT NULL DEFAULT 0
                );
                CREATE INDEX index_clipboard_entries_text ON clipboard_entries(text);
                INSERT INTO clipboard_entries (text, timestamp, isPinned, isQuickSend)
                    VALUES ('老数据', 100, 0, 0);",
            )
            .unwrap();
            conn.pragma_update(None, "user_version", 1).unwrap();
        }
        let store = ClipboardStore::open(&v1_path).unwrap();
        let conn = store.conn.lock().unwrap();
        let cols = column_names(&conn);
        for col in ["consumed", "code"] {
            assert!(cols.contains(col), "v1 -> v3 缺少列 {col}");
        }
        let version: i64 = conn
            .pragma_query_value(None, "user_version", |r| r.get(0))
            .unwrap();
        assert_eq!(version, 3);
        // 老数据还在且新默认列生效
        let item: ClipboardEntry = conn
            .query_row(
                "SELECT * FROM clipboard_entries WHERE text = '老数据'",
                [],
                row_to_entry,
            )
            .unwrap();
        assert!(!item.consumed);
        assert_eq!(item.code, "");
        drop(conn);

        // v2 → v3
        let v2_path = dir.path().join("clipboard_v2.db");
        {
            let conn = Connection::open(&v2_path).unwrap();
            conn.execute_batch(
                "CREATE TABLE clipboard_entries (
                    id          INTEGER PRIMARY KEY AUTOINCREMENT NOT NULL,
                    text        TEXT NOT NULL,
                    timestamp   INTEGER NOT NULL DEFAULT 0,
                    isPinned    INTEGER NOT NULL DEFAULT 0,
                    isQuickSend INTEGER NOT NULL DEFAULT 0,
                    consumed    INTEGER NOT NULL DEFAULT 0
                );",
            )
            .unwrap();
            conn.pragma_update(None, "user_version", 2).unwrap();
        }
        let store = ClipboardStore::open(&v2_path).unwrap();
        let conn = store.conn.lock().unwrap();
        assert!(column_names(&conn).contains("code"));
        drop(conn);
    }

    #[test]
    fn upsert_dedup_and_refresh_timestamp() {
        let dir = tempfile::tempdir().unwrap();
        let store = open_in(dir.path());
        store.upsert_and_trim("hello", 100).unwrap();
        store.upsert_and_trim("hello", 200).unwrap();
        let items = store.list_clipboard(10).unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0].text, "hello");
        assert_eq!(items[0].timestamp, 200, "相同文本应刷新时间戳而非新增");
    }

    #[test]
    fn upsert_trims_oldest_unpinned() {
        let dir = tempfile::tempdir().unwrap();
        let store = open_in(dir.path());
        // 上限 1000；插入 1001 条 → 最旧 1 条被裁剪
        for i in 0..1001 {
            store.upsert_and_trim(&format!("text{i:04}"), i).unwrap();
        }
        let items = store.list_clipboard(2000).unwrap();
        assert_eq!(items.len() as i64, MAX_CLIPBOARD_ITEMS);
        // 最旧的一条被删掉：最新一条保留，text0000 被裁剪
        assert_eq!(items[0].text, "text1000");
        assert!(!items.iter().any(|e| e.text == "text0000"));
    }

    #[test]
    fn trim_keeps_pinned_entries() {
        let dir = tempfile::tempdir().unwrap();
        let store = open_in(dir.path());
        // 置顶条目（isPinned = 1）永不被裁剪
        store.upsert_and_trim("pinned", 0).unwrap();
        store
            .conn
            .lock()
            .unwrap()
            .execute("UPDATE clipboard_entries SET isPinned = 1", [])
            .unwrap();
        for i in 0..1000 {
            store
                .upsert_and_trim(&format!("t{i:04}"), 1000 + i)
                .unwrap();
        }
        let pinned = store
            .list_clipboard(2000)
            .unwrap()
            .iter()
            .filter(|e| e.is_pinned)
            .count();
        assert!(pinned >= 1, "置顶条目必须保留");
    }

    #[test]
    fn add_to_quick_send_copies_and_dedups() {
        let dir = tempfile::tempdir().unwrap();
        let store = open_in(dir.path());
        store.upsert_and_trim("共享文本", 10).unwrap();
        let id = store.list_clipboard(10).unwrap()[0].id;

        store.add_to_quick_send(id, 100).unwrap();
        store.add_to_quick_send(id, 200).unwrap();
        let qs = store.list_quick_send().unwrap();
        assert_eq!(qs.len(), 1, "快捷发送按文本去重");
        assert_eq!(qs[0].timestamp, 200);
        assert!(qs[0].is_pinned, "快捷发送默认置顶");
        // 原普通条目不受影响
        assert_eq!(store.count_clipboard().unwrap(), 1);
    }

    #[test]
    fn insert_quick_send_updates_existing() {
        let dir = tempfile::tempdir().unwrap();
        let store = open_in(dir.path());
        store.insert_quick_send("地址", "dz", 100).unwrap();
        store.insert_quick_send("地址", "dz2", 200).unwrap();
        let qs = store.list_quick_send().unwrap();
        assert_eq!(qs.len(), 1);
        assert_eq!(qs[0].code, "dz2", "已存在条目应更新文本/编码/时间戳");
    }

    #[test]
    fn quick_send_trims_over_limit() {
        let dir = tempfile::tempdir().unwrap();
        let store = open_in(dir.path());
        for i in 0..(MAX_QUICK_SEND_ITEMS + 5) {
            store
                .insert_quick_send(&format!("qs{i}"), &format!("c{i}"), i)
                .unwrap();
        }
        assert_eq!(store.count_quick_send().unwrap(), MAX_QUICK_SEND_ITEMS);
        let qs = store.list_quick_send().unwrap();
        assert!(!qs.iter().any(|e| e.text == "qs0"), "最旧条目应被裁剪");
    }

    #[test]
    fn deletes_are_type_isolated() {
        let dir = tempfile::tempdir().unwrap();
        let store = open_in(dir.path());
        store.upsert_and_trim("普通", 1).unwrap();
        let clip_id = store.list_clipboard(10).unwrap()[0].id;
        store.insert_quick_send("快捷", "q", 2).unwrap();
        let qs_id = store.list_quick_send().unwrap()[0].id;

        store.delete_clipboard(qs_id).unwrap();
        assert_eq!(
            store.count_quick_send().unwrap(),
            1,
            "删除普通条目接口不应误删快捷发送"
        );
        store.delete_quick_send(clip_id).unwrap();
        assert_eq!(
            store.count_clipboard().unwrap(),
            1,
            "删除快捷发送接口不应误删普通条目"
        );

        store.clear_clipboard().unwrap();
        assert_eq!(store.count_clipboard().unwrap(), 0);
        assert_eq!(
            store.count_quick_send().unwrap(),
            1,
            "清空普通剪贴板保留快捷发送"
        );
        store.clear_quick_send().unwrap();
        assert_eq!(store.count_quick_send().unwrap(), 0);
        store.insert_quick_send("再一个", "z", 3).unwrap();
        store.clear_all().unwrap();
        assert_eq!(store.count_clipboard().unwrap(), 0);
        assert_eq!(store.count_quick_send().unwrap(), 0);
    }

    #[test]
    fn mark_consumed_and_find_by_id() {
        let dir = tempfile::tempdir().unwrap();
        let store = open_in(dir.path());
        store.upsert_and_trim("上屏内容", 7).unwrap();
        let id = store.list_clipboard(10).unwrap()[0].id;
        assert!(!store.find_by_id(id).unwrap().unwrap().consumed);
        store.mark_consumed(id).unwrap();
        assert!(store.find_by_id(id).unwrap().unwrap().consumed);
        assert!(store.find_by_id(999_999).unwrap().is_none());
    }

    #[test]
    fn init_is_idempotent_and_creates_dir() {
        let dir = tempfile::tempdir().unwrap();
        let sub = dir.path().join("a/b/c");
        let a = init(&sub);
        let b = init(&sub);
        assert!(Arc::ptr_eq(&a, &b));
        assert!(a.find_by_id(1).unwrap().is_none(), "空库查询正常");
    }
}
