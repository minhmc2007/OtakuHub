//! Continue watching. One row per anime, holding the last episode opened.

use rusqlite::{params, Connection};

use super::Db;
use crate::error::AppResult;
use crate::time::next_millis;

#[derive(Debug, Clone)]
pub struct Progress {
    pub anime_id: String,
    pub title: String,
    pub poster: Option<String>,
    pub episode_number: String,
    pub episode_id: String,
    pub updated_at: i64,
    /// How far into the episode the viewer got, so the row can be resumed.
    pub position_secs: i64,
}

pub fn touch(
    conn: &Connection,
    user_id: i64,
    anime: &crate::source::Anime,
    episode: &crate::source::Episode,
) -> AppResult<()> {
    // Milliseconds, not seconds: two episodes opened in the same second still have a defined
    // order, and "continue watching" is ordered by this column.
    conn.execute(
        "INSERT INTO history (user_id, anime_id, title, poster, episode_number, episode_id,
                              updated_at, position_secs)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 0)
         ON CONFLICT(user_id, anime_id) DO UPDATE SET
             title = excluded.title,
             poster = COALESCE(excluded.poster, history.poster),
             episode_number = excluded.episode_number,
             episode_id = excluded.episode_id,
             updated_at = excluded.updated_at,
             position_secs = CASE WHEN history.episode_id = excluded.episode_id
                                  THEN history.position_secs ELSE 0 END",
        params![
            user_id,
            anime.id,
            anime.title,
            anime.poster,
            episode.number,
            episode.id,
            next_millis()
        ],
    )?;
    Ok(())
}

/// Record how far into the episode the viewer is. Leaves `updated_at` alone, so an idle
/// tab does not push a show back to the top of the list.
pub fn set_position(db: &Db, user_id: i64, anime_id: &str, secs: i64) -> AppResult<bool> {
    db.with(|c| {
        let n = c.execute(
            "UPDATE history SET position_secs = ?3
             WHERE user_id = ?1 AND anime_id = ?2",
            params![user_id, anime_id, secs.max(0)],
        )?;
        Ok(n > 0)
    })
}

pub fn list(conn: &Connection, user_id: i64, limit: i64) -> AppResult<Vec<Progress>> {
    let mut stmt = conn.prepare(
        "SELECT anime_id, title, poster, episode_number, episode_id, updated_at, position_secs
         FROM history WHERE user_id = ?1 ORDER BY updated_at DESC LIMIT ?2",
    )?;
    let rows = stmt.query_map(params![user_id, limit], |r| {
        Ok(Progress {
            anime_id: r.get(0)?,
            title: r.get(1)?,
            poster: r.get(2)?,
            episode_number: r.get(3)?,
            episode_id: r.get(4)?,
            updated_at: r.get(5)?,
            position_secs: r.get(6)?,
        })
    })?;
    rows.collect::<Result<Vec<_>, _>>().map_err(crate::error::AppError::Db)
}

pub fn clear(conn: &Connection, user_id: i64) -> AppResult<usize> {
    Ok(conn.execute("DELETE FROM history WHERE user_id = ?1", params![user_id])?)
}

pub fn remove(conn: &Connection, user_id: i64, anime_id: &str) -> AppResult<bool> {
    Ok(conn.execute(
        "DELETE FROM history WHERE user_id = ?1 AND anime_id = ?2",
        params![user_id, anime_id],
    )? > 0)
}

pub fn recent(db: &Db, user_id: i64, limit: i64) -> AppResult<Vec<Progress>> {
    db.with(|c| list(c, user_id, limit))
}
pub fn record(
    db: &Db,
    user_id: i64,
    anime: &crate::source::Anime,
    episode: &crate::source::Episode,
) -> AppResult<()> {
    db.with(|c| touch(c, user_id, anime, episode))
}
pub fn wipe(db: &Db, user_id: i64) -> AppResult<usize> {
    db.with(|c| clear(c, user_id))
}
pub fn forget(db: &Db, user_id: i64, anime_id: &str) -> AppResult<bool> {
    db.with(|c| remove(c, user_id, anime_id))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::users;
    use crate::source::{Anime, Episode};

    fn anime() -> Anime {
        Anime {
            id: "one-piece-1".into(),
            title: "One Piece".into(),
            poster: Some("p.jpg".into()),
        }
    }
    fn ep(n: &str) -> Episode {
        Episode {
            id: n.into(),
            number: n.into(),
            title: None,
        }
    }

    #[test]
    fn watching_updates_in_place() {
        let db = Db::open_memory().unwrap();
        let id = db.with(|c| users::create(c, "rei", "hunter2000")).unwrap();
        record(&db, id, &anime(), &ep("1")).unwrap();
        record(&db, id, &anime(), &ep("2")).unwrap();
        let rows = recent(&db, id, 10).unwrap();
        assert_eq!(rows.len(), 1, "one row per anime");
        assert_eq!(rows[0].episode_number, "2");
    }

    #[test]
    fn new_rows_land_on_top() {
        let db = Db::open_memory().unwrap();
        let id = db.with(|c| users::create(c, "rei", "hunter2000")).unwrap();
        record(&db, id, &anime(), &ep("1")).unwrap();
        let mut other = anime();
        other.id = "naruto-1".into();
        other.title = "Naruto".into();
        record(&db, id, &other, &ep("1")).unwrap();
        let rows = recent(&db, id, 10).unwrap();
        assert_eq!(rows[0].anime_id, "naruto-1");
    }

    #[test]
    fn a_missing_poster_does_not_erase_the_old_one() {
        let db = Db::open_memory().unwrap();
        let id = db.with(|c| users::create(c, "rei", "hunter2000")).unwrap();
        record(&db, id, &anime(), &ep("1")).unwrap();
        let mut bare = anime();
        bare.poster = None;
        record(&db, id, &bare, &ep("2")).unwrap();
        assert_eq!(recent(&db, id, 1).unwrap()[0].poster.as_deref(), Some("p.jpg"));
    }

    #[test]
    fn limit_and_clear() {
        let db = Db::open_memory().unwrap();
        let id = db.with(|c| users::create(c, "rei", "hunter2000")).unwrap();
        for n in 1..=3 {
            let mut a = anime();
            a.id = format!("a-{n}");
            record(&db, id, &a, &ep("1")).unwrap();
        }
        assert_eq!(recent(&db, id, 2).unwrap().len(), 2);
        assert_eq!(wipe(&db, id).unwrap(), 3);
        assert!(recent(&db, id, 10).unwrap().is_empty());
    }

    #[test]
    fn one_user_cannot_see_another_history() {
        let db = Db::open_memory().unwrap();
        let a = db.with(|c| users::create(c, "a", "hunter2000")).unwrap();
        let b = db.with(|c| users::create(c, "b", "hunter2000")).unwrap();
        record(&db, a, &anime(), &ep("1")).unwrap();
        assert!(recent(&db, b, 10).unwrap().is_empty());
    }
}
