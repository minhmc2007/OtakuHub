//! What the viewer has watched, one row per episode, carrying the position inside it so a
//! return visit can offer to continue.

use rusqlite::params;

use super::Db;
use crate::error::AppResult;
use crate::time::next_millis;

/// One episode's state.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub episode_id: String,
    pub episode_num: String,
    pub position_secs: i64,
    pub duration_secs: i64,
    pub finished: bool,
}

/// Below this, the viewer has barely started, so there is nothing to resume and an episode
/// is not marked as started.
const MIN_STARTED_SECS: i64 = 5;

/// Within this of the end, the episode counts as watched. Ninety seconds covers a long
/// end credits roll, which people do not watch to the last frame.
const FINISH_SLACK_SECS: i64 = 90;

/// Record where the viewer is. A position near the end marks it finished and clears the
/// stored position, so a return visit starts at the beginning.
pub fn save(
    db: &Db,
    user_id: i64,
    anime_id: &str,
    episode_id: &str,
    episode_num: &str,
    position_secs: i64,
    duration_secs: i64,
) -> AppResult<bool> {
    let position = position_secs.max(0);
    let duration = duration_secs.max(0);
    let finished = duration > 0 && position >= duration - FINISH_SLACK_SECS;
    let stored = if finished { 0 } else { position };
    // A position of a few seconds is a page load, not a start, and writing those would
    // fill the table with noise for anyone clicking through a list.
    if stored < MIN_STARTED_SECS && !finished {
        return Ok(false);
    }

    db.with(|c| {
        let n = c.execute(
            "INSERT INTO episode_progress
                (user_id, anime_id, episode_id, episode_num, position_secs, duration_secs,
                 finished, updated_at)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
             ON CONFLICT(user_id, episode_id) DO UPDATE SET
                 position_secs = excluded.position_secs,
                 duration_secs = excluded.duration_secs,
                 finished = excluded.finished,
                 updated_at = excluded.updated_at",
            params![
                user_id,
                anime_id,
                episode_id,
                episode_num,
                stored,
                duration,
                finished as i64,
                next_millis()
            ],
        )?;
        Ok(n > 0)
    })
}

/// Clear an episode's position, for the "start over" choice on a return visit. The row stays
/// so the episode still counts as watched, only the resume point is dropped.
pub fn restart(db: &Db, user_id: i64, episode_id: &str) -> AppResult<bool> {
    db.with(|c| {
        let n = c.execute(
            "UPDATE episode_progress SET position_secs = 0, updated_at = ?3
             WHERE user_id = ?1 AND episode_id = ?2",
            params![user_id, episode_id, next_millis()],
        )?;
        Ok(n > 0)
    })
}

/// Every episode the viewer has touched for one series, keyed by episode id.
pub fn for_anime(db: &Db, user_id: i64, anime_id: &str) -> AppResult<Vec<Entry>> {
    db.with(|c| {
        let mut stmt = c.prepare(
            "SELECT episode_id, episode_num, position_secs, duration_secs, finished
             FROM episode_progress WHERE user_id = ?1 AND anime_id = ?2",
        )?;
        let rows = stmt.query_map(params![user_id, anime_id], |r| {
            Ok(Entry {
                episode_id: r.get(0)?,
                episode_num: r.get(1)?,
                position_secs: r.get(2)?,
                duration_secs: r.get(3)?,
                finished: r.get::<_, i64>(4)? != 0,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(crate::error::AppError::Db)
    })
}

/// One episode, for the return visit prompt.
pub fn get(db: &Db, user_id: i64, episode_id: &str) -> AppResult<Option<Entry>> {
    db.with(|c| {
        Ok(c
            .query_row(
                "SELECT episode_id, episode_num, position_secs, duration_secs, finished
                 FROM episode_progress WHERE user_id = ?1 AND episode_id = ?2",
                params![user_id, episode_id],
                |r| {
                    Ok(Entry {
                        episode_id: r.get(0)?,
                        episode_num: r.get(1)?,
                        position_secs: r.get(2)?,
                        duration_secs: r.get(3)?,
                        finished: r.get::<_, i64>(4)? != 0,
                    })
                },
            )
            .ok())
    })
}

/// How many episodes of a series are finished, for the header count.
pub fn finished_count(db: &Db, user_id: i64, anime_id: &str) -> AppResult<i64> {
    db.with(|c| {
        Ok(c.query_row(
            "SELECT COUNT(*) FROM episode_progress
             WHERE user_id = ?1 AND anime_id = ?2 AND finished = 1",
            params![user_id, anime_id],
            |r| r.get(0),
        )?)
    })
}

/// Drop a series' episode rows, used when the viewer clears their history.
pub fn wipe_anime(db: &Db, user_id: i64, anime_id: &str) -> AppResult<usize> {
    db.with(|c| {
        Ok(c.execute(
            "DELETE FROM episode_progress WHERE user_id = ?1 AND anime_id = ?2",
            params![user_id, anime_id],
        )?)
    })
}

/// Drop everything, for a full history wipe.
pub fn wipe(db: &Db, user_id: i64) -> AppResult<usize> {
    db.with(|c| {
        Ok(c.execute(
            "DELETE FROM episode_progress WHERE user_id = ?1",
            params![user_id],
        )?)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::users;

    fn setup() -> (Db, i64) {
        let db = Db::open_memory().unwrap();
        let id = db
            .with(|c| users::create(c, "rei", "hunter2000"))
            .unwrap();
        (db, id)
    }

    #[test]
    fn a_position_is_kept() {
        let (db, u) = setup();
        assert!(save(&db, u, "one-piece-1", "ep1", "1", 754, 1440).unwrap());
        let got = get(&db, u, "ep1").unwrap().unwrap();
        assert_eq!(got.position_secs, 754);
        assert!(!got.finished);
    }

    #[test]
    fn the_end_of_an_episode_marks_it_finished_and_drops_the_position() {
        // Otherwise a return visit drops the viewer into the credits.
        let (db, u) = setup();
        save(&db, u, "cb", "ep1", "1", 1400, 1440).unwrap();
        let got = get(&db, u, "ep1").unwrap().unwrap();
        assert!(got.finished);
        assert_eq!(got.position_secs, 0);
    }

    #[test]
    fn a_page_load_is_not_a_start() {
        let (db, u) = setup();
        assert!(!save(&db, u, "cb", "ep1", "1", 2, 1440).unwrap());
        assert!(get(&db, u, "ep1").unwrap().is_none());
    }

    #[test]
    fn every_episode_of_a_series_is_tracked_separately() {
        // The reason this table exists: `history` holds one row per series, so it cannot
        // answer which of the other episodes were watched.
        let (db, u) = setup();
        for ep in ["e1", "e2", "e3"] {
            save(&db, u, "one-piece-1", ep, ep, 1400, 1440).unwrap();
        }
        assert_eq!(for_anime(&db, u, "one-piece-1").unwrap().len(), 3);
        assert_eq!(finished_count(&db, u, "one-piece-1").unwrap(), 3);
    }

    #[test]
    fn saving_twice_keeps_one_row() {
        let (db, u) = setup();
        save(&db, u, "cb", "ep1", "1", 100, 1440).unwrap();
        save(&db, u, "cb", "ep1", "1", 800, 1440).unwrap();
        let all = for_anime(&db, u, "cb").unwrap();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].position_secs, 800);
    }

    #[test]
    fn restarting_clears_the_position_but_keeps_it_watched() {
        let (db, u) = setup();
        save(&db, u, "cb", "ep1", "1", 1400, 1440).unwrap();
        assert!(restart(&db, u, "ep1").unwrap());
        let got = get(&db, u, "ep1").unwrap().unwrap();
        assert_eq!(got.position_secs, 0);
        assert!(got.finished, "a rewatch is still a watch");
    }

    #[test]
    fn one_account_cannot_see_another_accounts_progress() {
        let db = Db::open_memory().unwrap();
        let a = db.with(|c| users::create(c, "rei", "hunter2000")).unwrap();
        let b = db.with(|c| users::create(c, "minh", "hunter2000")).unwrap();
        save(&db, a, "cb", "ep1", "1", 500, 1440).unwrap();
        assert!(get(&db, b, "ep1").unwrap().is_none());
        assert_eq!(finished_count(&db, b, "cb").unwrap(), 0);
    }

    #[test]
    fn an_unknown_duration_cannot_be_mistaken_for_the_end() {
        // A live stream reports no duration, and treating position as
        // finished there would mark everything watched.
        let (db, u) = setup();
        save(&db, u, "cb", "ep1", "1", 100, 0).unwrap();
        assert!(!get(&db, u, "ep1").unwrap().unwrap().finished);
    }
}
