//! SQLite access. One file on disk, WAL mode, a single connection behind a mutex. Every query
//! here is a point lookup or a small index scan, so a pool would buy no throughput.

use std::sync::{Arc, Mutex};

use rusqlite::{params, Connection, OpenFlags};

use crate::error::{AppError, AppResult};

pub mod history;
pub mod jobs;
pub mod playlists;
pub mod progress;
pub mod sessions;
pub mod settings;
pub mod users;

#[derive(Clone)]
pub struct Db {
    conn: Arc<Mutex<Connection>>,
}

impl Db {
    pub fn open(path: &std::path::Path) -> AppResult<Self> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        Self::from_conn(conn)
    }

    /// In memory database for tests.
    pub fn open_memory() -> AppResult<Self> {
        Self::from_conn(Connection::open_in_memory()?)
    }

    fn from_conn(conn: Connection) -> AppResult<Self> {
        // WAL keeps readers off the writer's back, NORMAL sync is safe under WAL.
        let _: String = conn.query_row("PRAGMA journal_mode=WAL", [], |r| r.get(0))?;
        conn.execute_batch(
            "PRAGMA foreign_keys = ON;
             PRAGMA synchronous = NORMAL;
             PRAGMA busy_timeout = 5000;",
        )?;
        let db = Self {
            conn: Arc::new(Mutex::new(conn)),
        };
        db.migrate()?;
        Ok(db)
    }

    /// Run a closure with the connection. The mutex is the whole concurrency story here.
    pub fn with<T>(&self, f: impl FnOnce(&Connection) -> AppResult<T>) -> AppResult<T> {
        let guard = self
            .conn
            .lock()
            .map_err(|_| AppError::internal("database mutex poisoned"))?;
        f(&guard)
    }

    fn migrate(&self) -> AppResult<()> {
        let conn = self.conn.lock().expect("fresh mutex");
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        if version >= SCHEMA_VERSION {
            return Ok(());
        }
        conn.execute_batch(SCHEMA)?;
        // A CREATE TABLE IF NOT EXISTS does not touch a table that is already there, so a
        // column added to SCHEMA has to be added to the old file by hand.
        for (from, to, sql) in UPGRADES {
            if version >= *from && version < *to {
                conn.execute_batch(sql)?;
            }
        }
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        tracing::info!(version = SCHEMA_VERSION, "schema ready");
        Ok(())
    }

    /// Rows changed by the last statement. Used to build "nothing to do" replies.
    pub fn last_changes(&self) -> AppResult<usize> {
        self.with(|c| Ok(c.changes() as usize))
    }
}

/// Anime whose stored title never resolved. Nothing else corrects such a row, so the ids are
/// collected once at startup and looked up again.
pub fn unresolved(db: &Db) -> AppResult<Vec<String>> {
    db.with(|c| {
        let mut ids: Vec<String> = Vec::new();
        for sql in [
            "SELECT anime_id FROM playlist_items WHERE poster IS NULL OR poster = ''",
            "SELECT anime_id FROM history WHERE poster IS NULL OR poster = ''",
        ] {
            let mut stmt = c.prepare(sql)?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            for id in rows {
                let id = id?;
                if !ids.contains(&id) {
                    ids.push(id);
                }
            }
        }
        Ok(ids)
    })
}

/// Rewrite the stored title and poster for an anime everywhere they are kept.
pub fn relabel(db: &Db, anime: &crate::source::Anime) -> AppResult<usize> {
    db.with(|c| {
        let mut n = c.execute(
            "UPDATE playlist_items SET title = ?2, poster = ?3 WHERE anime_id = ?1",
            params![anime.id, anime.title, anime.poster],
        )?;
        n += c.execute(
            "UPDATE history SET title = ?2, poster = ?3 WHERE anime_id = ?1",
            params![anime.id, anime.title, anime.poster],
        )?;
        Ok(n)
    })
}


pub const SCHEMA_VERSION: i64 = 3;

/// Steps applied to a database that was created at an older version. Each entry runs once,
/// on the way past that version.
const UPGRADES: &[(i64, i64, &str)] = &[
    (
        1,
        2,
        // Where in the episode the viewer stopped. Zero means the start, which is what every
        // existing row means.
        "ALTER TABLE history ADD COLUMN position_secs INTEGER NOT NULL DEFAULT 0;",
    ),
    (
        2,
        3,
        // One row per episode the viewer has touched. `history` only remembers the last
        // episode of a series, which cannot say which of the other episodes were watched.
        r#"
        CREATE TABLE IF NOT EXISTS episode_progress (
            user_id       INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
            anime_id      TEXT    NOT NULL,
            episode_id    TEXT    NOT NULL,
            episode_num   TEXT    NOT NULL,
            position_secs INTEGER NOT NULL DEFAULT 0,
            duration_secs INTEGER NOT NULL DEFAULT 0,
            finished      INTEGER NOT NULL DEFAULT 0,
            updated_at    INTEGER NOT NULL,
            PRIMARY KEY (user_id, episode_id)
        );
        CREATE INDEX IF NOT EXISTS episode_by_anime
            ON episode_progress(user_id, anime_id);
        "#,
    ),
];

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS users (
    id            INTEGER PRIMARY KEY AUTOINCREMENT,
    username      TEXT    NOT NULL UNIQUE,
    password_hash TEXT    NOT NULL,
    salt          TEXT    NOT NULL,
    created_at    INTEGER NOT NULL
);

CREATE TABLE IF NOT EXISTS sessions (
    token      TEXT    PRIMARY KEY,
    user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    created_at INTEGER NOT NULL,
    expires_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS sessions_user ON sessions(user_id);

CREATE TABLE IF NOT EXISTS playlists (
    id         INTEGER PRIMARY KEY AUTOINCREMENT,
    user_id    INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    name       TEXT    NOT NULL,
    created_at INTEGER NOT NULL,
    UNIQUE(user_id, name)
);

CREATE TABLE IF NOT EXISTS playlist_items (
    playlist_id INTEGER NOT NULL REFERENCES playlists(id) ON DELETE CASCADE,
    anime_id    TEXT    NOT NULL,
    title       TEXT    NOT NULL,
    poster      TEXT,
    added_at    INTEGER NOT NULL,
    PRIMARY KEY (playlist_id, anime_id)
);
CREATE INDEX IF NOT EXISTS items_by_playlist ON playlist_items(playlist_id, added_at DESC);

CREATE TABLE IF NOT EXISTS history (
    user_id        INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    anime_id       TEXT    NOT NULL,
    title          TEXT    NOT NULL,
    poster         TEXT,
    episode_number TEXT    NOT NULL,
    episode_id     TEXT    NOT NULL,
    updated_at     INTEGER NOT NULL,
    position_secs  INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (user_id, anime_id)
);
CREATE INDEX IF NOT EXISTS history_recent ON history(user_id, updated_at DESC);

CREATE TABLE IF NOT EXISTS episode_progress (
    user_id       INTEGER NOT NULL REFERENCES users(id) ON DELETE CASCADE,
    anime_id      TEXT    NOT NULL,
    episode_id    TEXT    NOT NULL,
    episode_num   TEXT    NOT NULL,
    position_secs INTEGER NOT NULL DEFAULT 0,
    duration_secs INTEGER NOT NULL DEFAULT 0,
    finished      INTEGER NOT NULL DEFAULT 0,
    updated_at    INTEGER NOT NULL,
    PRIMARY KEY (user_id, episode_id)
);
CREATE INDEX IF NOT EXISTS episode_by_anime ON episode_progress(user_id, anime_id);

CREATE TABLE IF NOT EXISTS settings (
    user_id      INTEGER PRIMARY KEY REFERENCES users(id) ON DELETE CASCADE,
    codec        TEXT    NOT NULL DEFAULT 'h264',
    max_height   INTEGER NOT NULL DEFAULT 720,
    hardware     INTEGER NOT NULL DEFAULT 1,
    theme        TEXT    NOT NULL DEFAULT 'system',
    autoplay     INTEGER NOT NULL DEFAULT 1,
    dub          INTEGER NOT NULL DEFAULT 0
);

CREATE TABLE IF NOT EXISTS transcode_jobs (
    id             TEXT    PRIMARY KEY,
    user_id        INTEGER NOT NULL,
    anime_id       TEXT    NOT NULL,
    anime_title    TEXT    NOT NULL,
    episode_id     TEXT    NOT NULL,
    episode_number TEXT    NOT NULL,
    poster         TEXT,
    source_url     TEXT    NOT NULL,
    referer        TEXT,
    codec          TEXT    NOT NULL,
    max_height     INTEGER NOT NULL,
    encoder        TEXT    NOT NULL,
    state          TEXT    NOT NULL,
    error          TEXT,
    created_at     INTEGER NOT NULL,
    updated_at     INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS jobs_user ON transcode_jobs(user_id, updated_at DESC);

CREATE TABLE IF NOT EXISTS media_cache (
    key        TEXT    PRIMARY KEY,
    bytes      INTEGER NOT NULL,
    created_at INTEGER NOT NULL,
    touched_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS media_by_age ON media_cache(touched_at);
"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migrations_are_idempotent() {
        let db = Db::open_memory().unwrap();
        db.migrate().unwrap();
        let v: i64 = db.with(|c| Ok(c.query_row("PRAGMA user_version", [], |r| r.get(0))?)).unwrap();
        assert_eq!(v, SCHEMA_VERSION);
    }

    #[test]
    fn foreign_keys_are_enforced() {
        let db = Db::open_memory().unwrap();
        let r: AppResult<()> = db.with(|c| {
            c.execute(
                "INSERT INTO playlist_items (playlist_id, anime_id, title, added_at)
                 VALUES (999, 'x', 'X', 0)",
                [],
            )?;
            Ok(())
        });
        assert!(r.is_err(), "orphan playlist item should be rejected");
    }

    #[test]
    fn rows_without_a_poster_are_reported_once() {
        let db = Db::open_memory().unwrap();
        let user = db
            .with(|c| crate::db::users::create(c, "rei", "hunter2000"))
            .unwrap();
        let anime = crate::source::Anime::new("cowboy-bebop-1281", "Cowboy Bebop 1281");
        crate::db::playlists::add(&db, user, "Favourite", &anime).unwrap();
        crate::db::history::record(
            &db,
            user,
            &anime,
            &crate::source::Episode::new("21418", "1"),
        )
        .unwrap();

        // The same id sits in both tables, and it comes back once.
        assert_eq!(unresolved(&db).unwrap(), vec!["cowboy-bebop-1281".to_string()]);

        let found = crate::source::Anime::new("cowboy-bebop-1281", "Cowboy Bebop")
            .with_poster(Some("https://example.test/cb.webp".into()));
        assert_eq!(relabel(&db, &found).unwrap(), 2);

        assert!(unresolved(&db).unwrap().is_empty(), "nothing left to repair");
    }

    #[test]
    fn a_resolved_row_is_not_reported() {
        let db = Db::open_memory().unwrap();
        let user = db
            .with(|c| crate::db::users::create(c, "rei", "hunter2000"))
            .unwrap();
        let anime = crate::source::Anime::new("one-piece-1", "One Piece")
            .with_poster(Some("https://example.test/op.jpg".into()));
        crate::db::playlists::add(&db, user, "Favourite", &anime).unwrap();
        assert!(unresolved(&db).unwrap().is_empty());
    }
}
