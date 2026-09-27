//! Transcode job records. The row is the job: the worker, the HTTP layer and the UI all
//! read state from here, so nothing has to be kept in memory to stay in sync.

use rusqlite::{params, Connection, OptionalExtension};

use super::Db;
use crate::error::{AppError, AppResult};
use crate::time::now_millis;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum State {
    Queued,
    Running,
    Done,
    Failed,
    Cancelled,
}

impl State {
    pub fn as_str(self) -> &'static str {
        match self {
            State::Queued => "queued",
            State::Running => "running",
            State::Done => "done",
            State::Failed => "failed",
            State::Cancelled => "cancelled",
        }
    }
    pub fn parse(s: &str) -> State {
        match s {
            "running" => State::Running,
            "done" => State::Done,
            "failed" => State::Failed,
            "cancelled" => State::Cancelled,
            _ => State::Queued,
        }
    }
    pub fn is_terminal(self) -> bool {
        matches!(self, State::Done | State::Failed | State::Cancelled)
    }
}

#[derive(Debug, Clone)]
pub struct Job {
    pub id: String,
    pub user_id: i64,
    pub anime_id: String,
    pub anime_title: String,
    pub episode_id: String,
    pub episode_number: String,
    pub poster: Option<String>,
    pub source_url: String,
    pub referer: Option<String>,
    pub codec: String,
    pub max_height: i64,
    pub encoder: String,
    pub state: State,
    pub error: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Job {
    pub fn label(&self) -> String {
        format!("{} episode {}", self.anime_title, self.episode_number)
    }
}

/// `user_id` is part of the key, since without it two accounts asking for the same episode
/// collide on one row that the upsert then leaves owned by the first of them.
pub fn key_id(user_id: i64, episode_id: &str, codec: &str, height: i64, encoder: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(user_id.to_string().as_bytes());
    h.update(b"|");
    h.update(episode_id.as_bytes());
    h.update(b"|");
    h.update(codec.as_bytes());
    h.update(b"|");
    h.update(height.to_string().as_bytes());
    h.update(b"|");
    h.update(encoder.as_bytes());
    format!("{:x}", h.finalize())[..24].to_string()
}

/// Insert a job, or requeue the existing row. The id is a hash of the inputs, so a retry
/// lands on the same primary key and a plain INSERT made that a 500.
pub fn insert(conn: &Connection, job: &Job) -> AppResult<()> {
    conn.execute(
        "INSERT INTO transcode_jobs
            (id, user_id, anime_id, anime_title, episode_id, episode_number, poster,
             source_url, referer, codec, max_height, encoder, state, error, created_at, updated_at)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16)
         ON CONFLICT(id) DO UPDATE SET
             state = excluded.state,
             error = NULL,
             encoder = excluded.encoder,
             codec = excluded.codec,
             max_height = excluded.max_height,
             source_url = excluded.source_url,
             referer = excluded.referer,
             created_at = excluded.created_at,
             updated_at = excluded.updated_at",
        params![
            job.id, job.user_id, job.anime_id, job.anime_title, job.episode_id,
            job.episode_number, job.poster, job.source_url, job.referer, job.codec,
            job.max_height, job.encoder, job.state.as_str(), job.error,
            job.created_at, job.updated_at
        ],
    )?;
    Ok(())
}

pub fn by_id(conn: &Connection, id: &str) -> AppResult<Option<Job>> {
    conn.query_row(&format!("{COLS} WHERE id = ?1"), params![id], row)
        .optional()
        .map_err(AppError::Db)
}

/// Jobs may only be read or written by the account that started them.
pub fn owned_by(conn: &Connection, id: &str, user_id: i64) -> AppResult<Option<Job>> {
    conn.query_row(
        &format!("{COLS} WHERE id = ?1 AND user_id = ?2"),
        params![id, user_id],
        row,
    )
    .optional()
    .map_err(AppError::Db)
}

const COLS: &str = "SELECT id, user_id, anime_id, anime_title, episode_id, episode_number, poster, \
                    source_url, referer, codec, max_height, encoder, state, error, created_at, updated_at \
                    FROM transcode_jobs";

fn row(r: &rusqlite::Row<'_>) -> rusqlite::Result<Job> {
    Ok(Job {
        id: r.get(0)?,
        user_id: r.get(1)?,
        anime_id: r.get(2)?,
        anime_title: r.get(3)?,
        episode_id: r.get(4)?,
        episode_number: r.get(5)?,
        poster: r.get(6)?,
        source_url: r.get(7)?,
        referer: r.get(8)?,
        codec: r.get(9)?,
        max_height: r.get(10)?,
        encoder: r.get(11)?,
        state: State::parse(&r.get::<_, String>(12)?),
        error: r.get(13)?,
        created_at: r.get(14)?,
        updated_at: r.get(15)?,
    })
}

pub fn set_state(conn: &Connection, id: &str, state: State, error: Option<&str>) -> AppResult<()> {
    conn.execute(
        "UPDATE transcode_jobs SET state = ?1, error = ?2, updated_at = ?3 WHERE id = ?4",
        params![state.as_str(), error, now_millis(), id],
    )?;
    Ok(())
}

/// Record which encoder actually ran. A job that fell back to software has to say so,
/// because the row is what the conversions list shows.
pub fn set_encoder(conn: &Connection, id: &str, encoder: &str) -> AppResult<()> {
    conn.execute(
        "UPDATE transcode_jobs SET encoder = ?2, updated_at = ?3 WHERE id = ?1",
        params![id, encoder, now_millis()],
    )?;
    Ok(())
}

pub fn list_for_user(conn: &Connection, user_id: i64, limit: i64) -> AppResult<Vec<Job>> {
    let mut stmt = conn.prepare(&format!("{COLS} WHERE user_id = ?1 ORDER BY updated_at DESC LIMIT ?2"))?;
    let rows = stmt.query_map(params![user_id, limit], row)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(AppError::Db)
}

/// Jobs left behind by a crash. They are reaped at startup because no worker survives a restart.
pub fn unfinished(conn: &Connection) -> AppResult<Vec<Job>> {
    let mut stmt = conn.prepare(&format!("{COLS} WHERE state IN ('queued','running')"))?;
    let rows = stmt.query_map([], row)?;
    rows.collect::<Result<Vec<_>, _>>().map_err(AppError::Db)
}

pub fn delete(conn: &Connection, id: &str, user_id: i64) -> AppResult<bool> {
    Ok(conn.execute(
        "DELETE FROM transcode_jobs WHERE id = ?1 AND user_id = ?2",
        params![id, user_id],
    )? > 0)
}

pub fn get(db: &Db, id: &str) -> AppResult<Option<Job>> {
    db.with(|c| by_id(c, id))
}
pub fn get_owned(db: &Db, id: &str, user_id: i64) -> AppResult<Option<Job>> {
    db.with(|c| owned_by(c, id, user_id))
}
pub fn save_new(db: &Db, job: &Job) -> AppResult<()> {
    db.with(|c| insert(c, job))
}
pub fn update(db: &Db, id: &str, state: State, error: Option<&str>) -> AppResult<()> {
    db.with(|c| set_state(c, id, state, error))
}
pub fn mine(db: &Db, user_id: i64, limit: i64) -> AppResult<Vec<Job>> {
    db.with(|c| list_for_user(c, user_id, limit))
}
pub fn stalled(db: &Db) -> AppResult<Vec<Job>> {
    db.with(unfinished)
}
pub fn drop_it(db: &Db, id: &str, user_id: i64) -> AppResult<bool> {
    db.with(|c| delete(c, id, user_id))
}

pub fn new_job(
    user_id: i64,
    anime: &crate::source::Anime,
    episode: &crate::source::Episode,
    source_url: &str,
    referer: Option<&str>,
    codec: crate::media::Codec,
    max_height: u32,
    encoder: &str,
) -> Job {
    let height = max_height as i64;
    let id = key_id(user_id, &episode.id, codec.as_str(), height, encoder);
    Job {
        id: id.clone(),
        user_id,
        anime_id: anime.id.clone(),
        anime_title: anime.title.clone(),
        episode_id: episode.id.clone(),
        episode_number: episode.number.clone(),
        poster: anime.poster.clone(),
        source_url: source_url.to_string(),
        referer: referer.map(str::to_string),
        codec: codec.as_str().to_string(),
        max_height: height,
        encoder: encoder.to_string(),
        state: State::Queued,
        error: None,
        created_at: now_millis(),
        updated_at: now_millis(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::users;
    use crate::media::Codec;
    use crate::source::{Anime, Episode};

    fn setup() -> (Db, i64) {
        let db = Db::open_memory().unwrap();
        let id = db.with(|c| users::create(c, "rei", "hunter2000")).unwrap();
        (db, id)
    }
    fn job_for(user: i64) -> Job {
        new_job(
            user,
            &Anime {
                id: "one-piece-1".into(),
                title: "One Piece".into(),
                poster: None,
            },
            &Episode {
                id: "42".into(),
                number: "3".into(),
                title: None,
            },
            "https://cdn.test/v/1080/index.m3u8",
            Some("https://embed.test/"),
            Codec::H264,
            720,
            "h264_vaapi",
        )
    }

    #[test]
    fn same_request_maps_to_same_id() {
        let a = key_id(1, "42", "h264", 720, "h264_vaapi");
        let b = key_id(1, "42", "h264", 720, "h264_vaapi");
        assert_eq!(a, b);
    }

    #[test]
    fn every_input_changes_the_id() {
        let base = key_id(1, "42", "h264", 720, "h264_vaapi");
        assert_ne!(base, key_id(1, "43", "h264", 720, "h264_vaapi"));
        assert_ne!(base, key_id(1, "42", "h265", 720, "h264_vaapi"));
        assert_ne!(base, key_id(1, "42", "h264", 1080, "h264_vaapi"));
        assert_ne!(base, key_id(1, "42", "h264", 720, "h264_nvenc"));
        // Two accounts asking for the same episode must not share a job row.
        assert_ne!(base, key_id(2, "42", "h264", 720, "h264_vaapi"));
    }

    #[test]
    fn state_survives_a_round_trip() {
        let (db, id) = setup();
        let job = job_for(id);
        save_new(&db, &job).unwrap();
        update(&db, &job.id, State::Done, None).unwrap();
        let got = get(&db, &job.id).unwrap().unwrap();
        assert_eq!(got.state, State::Done);
        assert!(got.state.is_terminal());
    }

    #[test]
    fn failures_keep_their_reason() {
        let (db, id) = setup();
        let job = job_for(id);
        save_new(&db, &job).unwrap();
        update(&db, &job.id, State::Failed, Some("no vaapi device")).unwrap();
        let got = get(&db, &job.id).unwrap().unwrap();
        assert_eq!(got.error.as_deref(), Some("no vaapi device"));
    }

    #[test]
    fn another_user_cannot_read_a_job() {
        let (db, rei) = setup();
        let ana = db.with(|c| users::create(c, "ana", "hunter2000")).unwrap();
        let job = job_for(rei);
        save_new(&db, &job).unwrap();
        assert!(get(&db, &job.id).unwrap().is_some());
        assert!(get_owned(&db, &job.id, ana).unwrap().is_none());
    }

    #[test]
    fn stalled_jobs_are_visible_for_reaping() {
        let (db, id) = setup();
        let job = job_for(id);
        save_new(&db, &job).unwrap();
        update(&db, &job.id, State::Running, None).unwrap();
        assert_eq!(stalled(&db).unwrap().len(), 1);
        update(&db, &job.id, State::Done, None).unwrap();
        assert!(stalled(&db).unwrap().is_empty());
    }

    #[test]
    fn state_strings_round_trip() {
        for s in [State::Queued, State::Running, State::Done, State::Failed, State::Cancelled] {
            assert_eq!(State::parse(s.as_str()), s);
        }
    }
}
