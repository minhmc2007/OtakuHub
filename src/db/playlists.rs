//! Playlists. Every account owns any number of them; the first one created at signup
//! is called Favourite, but nothing depends on the name.

use rusqlite::{params, Connection, OptionalExtension};

use super::Db;
use crate::error::{AppError, AppResult};
use crate::time::now_secs;

#[derive(Debug, Clone)]
pub struct Playlist {
    pub id: i64,
    pub name: String,
    pub count: i64,
    pub created_at: i64,
}

#[derive(Debug, Clone)]
pub struct Entry {
    pub anime_id: String,
    pub title: String,
    pub poster: Option<String>,
    pub added_at: i64,
}

pub fn create(conn: &Connection, user_id: i64, name: &str) -> AppResult<i64> {
    let name = name.trim();
    if name.is_empty() || name.len() > 60 {
        return Err(AppError::bad("playlist name must be 1 to 60 characters"));
    }
    conn.execute(
        "INSERT INTO playlists (user_id, name, created_at) VALUES (?1, ?2, ?3)",
        params![user_id, name, now_secs()],
    )
    .map_err(|e| match e {
        rusqlite::Error::SqliteFailure(f, _) if f.code == rusqlite::ErrorCode::ConstraintViolation => {
            AppError::bad("you already have a playlist with that name")
        }
        other => AppError::Db(other),
    })?;
    Ok(conn.last_insert_rowid())
}

/// A playlist is addressed by name in URLs because the user typed the name.
pub fn by_name(conn: &Connection, user_id: i64, name: &str) -> AppResult<Option<Playlist>> {
    conn.query_row(
        "SELECT p.id, p.name, p.created_at,
                (SELECT COUNT(*) FROM playlist_items i WHERE i.playlist_id = p.id)
         FROM playlists p WHERE p.user_id = ?1 AND p.name = ?2",
        params![user_id, name],
        |r| {
            Ok(Playlist {
                id: r.get(0)?,
                name: r.get(1)?,
                count: r.get(3)?,
                created_at: r.get(2)?,
            })
        },
    )
    .optional()
    .map_err(AppError::Db)
}

pub fn list(conn: &Connection, user_id: i64) -> AppResult<Vec<Playlist>> {
    let mut stmt = conn.prepare(
        "SELECT p.id, p.name, p.created_at,
                (SELECT COUNT(*) FROM playlist_items i WHERE i.playlist_id = p.id)
         FROM playlists p WHERE p.user_id = ?1 ORDER BY p.created_at, p.id",
    )?;
    let rows = stmt.query_map(params![user_id], |r| {
        // The subquery is the last column, so the count is column 3 and not column 2.
        Ok(Playlist {
            count: r.get(3)?,
            created_at: r.get(2)?,
            name: r.get(1)?,
            id: r.get(0)?,
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(AppError::Db)
}

pub fn delete(conn: &Connection, user_id: i64, name: &str) -> AppResult<usize> {
    let n = conn.execute(
        "DELETE FROM playlists WHERE user_id = ?1 AND name = ?2",
        params![user_id, name],
    )?;
    if n == 0 {
        return Err(AppError::not_found("playlist"));
    }
    Ok(n)
}

/// Add an anime. Returns false when it was already there, so the UI can say "already saved".
pub fn add_item(
    conn: &Connection,
    user_id: i64,
    playlist: &str,
    anime: &crate::source::Anime,
) -> AppResult<bool> {
    let pl = by_name(conn, user_id, playlist)?.ok_or_else(|| AppError::not_found("playlist"))?;
    let n = conn.execute(
        "INSERT OR IGNORE INTO playlist_items (playlist_id, anime_id, title, poster, added_at)
         VALUES (?1, ?2, ?3, ?4, ?5)",
        params![pl.id, anime.id, anime.title, anime.poster, now_secs()],
    )?;
    Ok(n > 0)
}

pub fn remove_item(conn: &Connection, user_id: i64, playlist: &str, anime_id: &str) -> AppResult<bool> {
    let Some(pl) = by_name(conn, user_id, playlist)? else {
        return Ok(false);
    };
    let n = conn.execute(
        "DELETE FROM playlist_items WHERE playlist_id = ?1 AND anime_id = ?2",
        params![pl.id, anime_id],
    )?;
    Ok(n > 0)
}

pub fn contains(conn: &Connection, user_id: i64, playlist: &str, anime_id: &str) -> AppResult<bool> {
    let Some(pl) = by_name(conn, user_id, playlist)? else {
        return Ok(false);
    };
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM playlist_items WHERE playlist_id = ?1 AND anime_id = ?2",
        params![pl.id, anime_id],
        |r| r.get(0),
    )?;
    Ok(n > 0)
}

pub fn items(conn: &Connection, playlist_id: i64) -> AppResult<Vec<Entry>> {
    let mut stmt = conn.prepare(
        "SELECT anime_id, title, poster, added_at FROM playlist_items
         WHERE playlist_id = ?1 ORDER BY added_at DESC, anime_id",
    )?;
    let rows = stmt.query_map(params![playlist_id], |r| {
        Ok(Entry {
            anime_id: r.get(0)?,
            title: r.get(1)?,
            poster: r.get(2)?,
            added_at: r.get(3)?,
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(AppError::Db)
}

/// Every playlist that already holds this anime, used to render the toggle buttons.
pub fn playlists_holding(
    conn: &Connection,
    user_id: i64,
    anime_id: &str,
) -> AppResult<Vec<String>> {
    let mut stmt = conn.prepare(
        "SELECT p.name FROM playlists p
         JOIN playlist_items i ON i.playlist_id = p.id
         WHERE p.user_id = ?1 AND i.anime_id = ?2
         ORDER BY p.name",
    )?;
    let rows = stmt.query_map(params![user_id, anime_id], |r| r.get::<_, String>(0))?;
    rows.collect::<Result<Vec<_>, _>>().map_err(AppError::Db)
}

pub fn all(db: &Db, user_id: i64) -> AppResult<Vec<Playlist>> {
    db.with(|c| list(c, user_id))
}
pub fn get(db: &Db, user_id: i64, name: &str) -> AppResult<Option<Playlist>> {
    db.with(|c| by_name(c, user_id, name))
}
pub fn new(db: &Db, user_id: i64, name: &str) -> AppResult<Playlist> {
    let name = name.trim().to_string();
    db.with(|c| {
        let id = create(c, user_id, &name)?;
        Ok(Playlist {
            id,
            name: name.clone(),
            count: 0,
            created_at: now_secs(),
        })
    })
}
pub fn drop_it(db: &Db, user_id: i64, name: &str) -> AppResult<()> {
    db.with(|c| delete(c, user_id, name).map(|_| ()))
}
pub fn add(db: &Db, user_id: i64, playlist: &str, anime: &crate::source::Anime) -> AppResult<bool> {
    db.with(|c| add_item(c, user_id, playlist, anime))
}
pub fn remove(db: &Db, user_id: i64, playlist: &str, anime_id: &str) -> AppResult<bool> {
    db.with(|c| remove_item(c, user_id, playlist, anime_id))
}
pub fn has(db: &Db, user_id: i64, playlist: &str, anime_id: &str) -> AppResult<bool> {
    db.with(|c| contains(c, user_id, playlist, anime_id))
}
pub fn entries(db: &Db, playlist_id: i64) -> AppResult<Vec<Entry>> {
    db.with(|c| items(c, playlist_id))
}
pub fn holders(db: &Db, user_id: i64, anime_id: &str) -> AppResult<Vec<String>> {
    db.with(|c| playlists_holding(c, user_id, anime_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::users;
    use crate::source::Anime;

    fn anime(id: &str, title: &str) -> Anime {
        Anime {
            id: id.to_string(),
            title: title.to_string(),
            poster: Some("https://example.test/p.jpg".into()),
        }
    }

    fn setup() -> (Db, i64) {
        let db = Db::open_memory().unwrap();
        let id = db.with(|c| users::create(c, "rei", "hunter2000")).unwrap();
        (db, id)
    }

    #[test]
    fn default_playlist_exists_after_signup() {
        let (db, id) = setup();
        let pls = all(&db, id).unwrap();
        assert_eq!(pls.len(), 1);
        assert_eq!(pls[0].name, crate::db::users::DEFAULT_PLAYLIST);
    }

    #[test]
    fn adding_twice_reports_the_second_as_noop() {
        let (db, id) = setup();
        assert!(add(&db, id, "Favourite", &anime("one-piece-1", "One Piece")).unwrap());
        assert!(!add(&db, id, "Favourite", &anime("one-piece-1", "One Piece")).unwrap());
        let pl = get(&db, id, "Favourite").unwrap().unwrap();
        assert_eq!(pl.count, 1);
    }

    #[test]
    fn removal_toggles_back() {
        let (db, id) = setup();
        add(&db, id, "Favourite", &anime("a-1", "A")).unwrap();
        assert!(remove(&db, id, "Favourite", "a-1").unwrap());
        assert!(!remove(&db, id, "Favourite", "a-1").unwrap());
        assert!(!has(&db, id, "Favourite", "a-1").unwrap());
    }

    #[test]
    fn playlists_are_scoped_to_their_owner() {
        let (db, rei) = setup();
        let other = db.with(|c| users::create(c, "ana", "hunter2000")).unwrap();
        new(&db, rei, "Weekend").unwrap();
        assert!(get(&db, other, "Weekend").unwrap().is_none());
        assert!(drop_it(&db, other, "Weekend").is_err(), "cannot delete a stranger's playlist");
        assert!(get(&db, rei, "Weekend").unwrap().is_some());
    }

    #[test]
    fn duplicate_names_are_refused() {
        let (db, id) = setup();
        new(&db, id, "Weekend").unwrap();
        assert!(new(&db, id, "Weekend").is_err());
    }

    #[test]
    fn deleting_a_playlist_takes_its_items() {
        let (db, id) = setup();
        add(&db, id, "Favourite", &anime("a-1", "A")).unwrap();
        new(&db, id, "Later").unwrap();
        let later = get(&db, id, "Later").unwrap().unwrap();
        add(&db, id, "Later", &anime("b-1", "B")).unwrap();
        drop_it(&db, id, "Later").unwrap();
        assert!(entries(&db, later.id).unwrap().is_empty());
    }

    #[test]
    fn holders_lists_every_playlist_with_the_anime() {
        let (db, id) = setup();
        new(&db, id, "Later").unwrap();
        add(&db, id, "Favourite", &anime("a-1", "A")).unwrap();
        add(&db, id, "Later", &anime("a-1", "A")).unwrap();
        let mut h = holders(&db, id, "a-1").unwrap();
        h.sort();
        assert_eq!(h, vec!["Favourite", "Later"]);
        assert!(holders(&db, id, "zz-1").unwrap().is_empty());
    }

    #[test]
    fn empty_and_long_names_are_refused() {
        let (db, id) = setup();
        assert!(new(&db, id, "   ").is_err());
        assert!(new(&db, id, &"x".repeat(61)).is_err());
    }
}
