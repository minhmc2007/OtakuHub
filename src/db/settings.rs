//! Per user playback and appearance settings.

use rusqlite::{params, Connection};

use super::Db;
use crate::error::{AppError, AppResult};
use crate::media::Codec;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Settings {
    pub codec: Codec,
    pub max_height: u32,
    pub hardware: bool,
    /// `light`, `dark` or `system`.
    pub theme: String,
    pub autoplay: bool,
    pub dub: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            codec: Codec::H264,
            max_height: 720,
            hardware: true,
            theme: "system".to_string(),
            autoplay: true,
            dub: false,
        }
    }
}

/// Heights the UI offers. Anything not in this list is rejected on write.
pub const HEIGHT_CHOICES: [u32; 7] = [360, 480, 720, 1080, 1440, 2160, 0];
pub const THEME_CHOICES: [&str; 3] = ["system", "light", "dark"];

pub fn load(conn: &Connection, user_id: i64) -> AppResult<Settings> {
    let found: Option<(String, i64, i64, String, i64, i64)> = conn
        .query_row(
            "SELECT codec, max_height, hardware, theme, autoplay, dub
             FROM settings WHERE user_id = ?1",
            params![user_id],
            |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
        )
        .ok();
    let Some((codec, h, hw, theme, ap, dub)) = found else {
        return Ok(Settings::default());
    };
    Ok(Settings {
        codec: Codec::parse(&codec).unwrap_or(Codec::H264),
        max_height: h as u32,
        hardware: hw != 0,
        theme,
        autoplay: ap != 0,
        dub: dub != 0,
    })
}

pub fn save(conn: &Connection, user_id: i64, s: &Settings) -> AppResult<()> {
    if !HEIGHT_CHOICES.contains(&s.max_height) {
        return Err(AppError::bad("unsupported height"));
    }
    if !THEME_CHOICES.contains(&s.theme.as_str()) {
        return Err(AppError::bad("unsupported theme"));
    }
    conn.execute(
        "INSERT INTO settings (user_id, codec, max_height, hardware, theme, autoplay, dub)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT(user_id) DO UPDATE SET
             codec = excluded.codec,
             max_height = excluded.max_height,
             hardware = excluded.hardware,
             theme = excluded.theme,
             autoplay = excluded.autoplay,
             dub = excluded.dub",
        params![
            user_id,
            s.codec.as_str(),
            s.max_height as i64,
            s.hardware as i64,
            s.theme,
            s.autoplay as i64,
            s.dub as i64
        ],
    )?;
    Ok(())
}

pub fn get(db: &Db, user_id: i64) -> AppResult<Settings> {
    db.with(|c| load(c, user_id))
}

pub fn put(db: &Db, user_id: i64, s: &Settings) -> AppResult<()> {
    db.with(|c| save(c, user_id, s))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::users;

    fn setup() -> (Db, i64) {
        let db = Db::open_memory().unwrap();
        let id = db.with(|c| users::create(c, "rei", "hunter2000")).unwrap();
        (db, id)
    }

    #[test]
    fn defaults_when_nothing_saved() {
        let (db, id) = setup();
        assert_eq!(get(&db, id).unwrap(), Settings::default());
    }

    #[test]
    fn round_trips_every_field() {
        let (db, id) = setup();
        let want = Settings {
            codec: Codec::H265,
            max_height: 1080,
            hardware: false,
            theme: "dark".into(),
            autoplay: false,
            dub: true,
        };
        put(&db, id, &want).unwrap();
        assert_eq!(get(&db, id).unwrap(), want);
    }

    #[test]
    fn saving_twice_updates_in_place() {
        let (db, id) = setup();
        let mut s = Settings::default();
        s.max_height = 480;
        put(&db, id, &s).unwrap();
        s.max_height = 2160;
        put(&db, id, &s).unwrap();
        assert_eq!(get(&db, id).unwrap().max_height, 2160);
    }

    #[test]
    fn out_of_range_values_are_refused() {
        let (db, id) = setup();
        let mut s = Settings::default();
        s.max_height = 999;
        assert!(put(&db, id, &s).is_err());
        s.max_height = 720;
        s.theme = "neon".into();
        assert!(put(&db, id, &s).is_err());
    }

    #[test]
    fn settings_are_per_user() {
        let (db, a) = setup();
        let b = db.with(|c| users::create(c, "ana", "hunter2000")).unwrap();
        let mut s = Settings::default();
        s.codec = Codec::H265;
        put(&db, a, &s).unwrap();
        assert_eq!(get(&db, b).unwrap().codec, Codec::H264);
    }

    #[test]
    fn an_unknown_codec_in_the_db_falls_back() {
        let (db, id) = setup();
        db.with(|c| {
            c.execute("UPDATE settings SET codec = 'av1' WHERE user_id = ?1", params![id])?;
            Ok(())
        })
        .unwrap();
        assert_eq!(get(&db, id).unwrap().codec, Codec::H264);
    }
}
