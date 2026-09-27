//! Session tokens. The token is the only thing in the cookie, and it lives in the
//! database so a session can be revoked and so a restart does not sign everyone out.

use rusqlite::{params, Connection};

use super::users::User;
use super::Db;
use crate::error::{AppError, AppResult};
use crate::time::now_secs;

pub const COOKIE: &str = "ohub_sid";
pub const TTL_SECS: i64 = 30 * 24 * 3600;

pub fn create(conn: &Connection, user_id: i64) -> AppResult<String> {
    let token = crate::auth::new_token();
    let now = now_secs();
    conn.execute(
        "INSERT INTO sessions (token, user_id, created_at, expires_at) VALUES (?1, ?2, ?3, ?4)",
        params![token, user_id, now, now + TTL_SECS],
    )?;
    Ok(token)
}

pub fn resolve(conn: &Connection, token: &str) -> AppResult<Option<User>> {
    let row: Option<(i64, i64)> = conn
        .query_row(
            "SELECT user_id, expires_at FROM sessions WHERE token = ?1",
            params![token],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .ok();
    let Some((user_id, expires_at)) = row else {
        return Ok(None);
    };
    if expires_at < now_secs() {
        drop(conn.execute("DELETE FROM sessions WHERE token = ?1", params![token]));
        return Ok(None);
    }
    super::users::by_id(conn, user_id)
}

pub fn destroy(conn: &Connection, token: &str) -> AppResult<()> {
    conn.execute("DELETE FROM sessions WHERE token = ?1", params![token])?;
    Ok(())
}

pub fn destroy_all_for(conn: &Connection, user_id: i64) -> AppResult<()> {
    conn.execute("DELETE FROM sessions WHERE user_id = ?1", params![user_id])?;
    Ok(())
}

/// Sign out every session for an account except one. Used after a password change, which
/// is the only way a stolen cookie gets revoked otherwise.
pub fn destroy_all_except(
    conn: &Connection,
    user_id: i64,
    keep: Option<&str>,
) -> AppResult<usize> {
    let n = match keep {
        Some(token) => conn.execute(
            "DELETE FROM sessions WHERE user_id = ?1 AND token <> ?2",
            params![user_id, token],
        )?,
        None => conn.execute("DELETE FROM sessions WHERE user_id = ?1", params![user_id])?,
    };
    Ok(n)
}

pub fn purge_expired(conn: &Connection) -> AppResult<usize> {
    Ok(conn.execute("DELETE FROM sessions WHERE expires_at < ?1", params![now_secs()])?)
}

pub fn count_for_user(db: &Db, user_id: i64) -> AppResult<i64> {
    db.with(|c| {
        Ok(c.query_row(
            "SELECT COUNT(*) FROM sessions WHERE user_id = ?1",
            params![user_id],
            |r| r.get(0),
        )?)
    })
}

/// The cookie value. `Secure` is left off because the default is plain http on localhost.
pub fn cookie_header(token: &str) -> String {
    format!("{COOKIE}={token}; Path=/; HttpOnly; SameSite=Lax; Max-Age={TTL_SECS}")
}

pub fn clear_header() -> String {
    format!("{COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0")
}

pub fn token_from_cookie(header: Option<&str>) -> Option<String> {
    let raw = header?;
    for part in raw.split(';') {
        let part = part.trim();
        if let Some(v) = part.strip_prefix(&format!("{COOKIE}=")) {
            if !v.is_empty() {
                return Some(v.to_string());
            }
        }
    }
    None
}

/// Read the session cookie off an incoming request.
pub fn token_from_headers(headers: &axum::http::HeaderMap) -> Option<String> {
    token_from_cookie(headers.get(axum::http::header::COOKIE)?.to_str().ok())
}

pub fn require(token: Option<String>, db: &Db) -> AppResult<User> {
    let Some(token) = token else {
        return Err(AppError::Unauthorized);
    };
    let user = db.with(|c| resolve(c, &token))?;
    user.ok_or(AppError::Unauthorized)
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
    fn round_trips_a_user() {
        let (db, id) = setup();
        let token = db.with(|c| create(c, id)).unwrap();
        let got = require(Some(token), &db).unwrap();
        assert_eq!(got.id, id);
    }

    #[test]
    fn missing_token_is_unauthorized() {
        let (db, _) = setup();
        assert!(matches!(require(None, &db), Err(AppError::Unauthorized)));
    }

    #[test]
    fn logout_kills_the_session() {
        let (db, id) = setup();
        let token = db.with(|c| create(c, id)).unwrap();
        db.with(|c| destroy(c, &token)).unwrap();
        assert!(require(Some(token), &db).is_err());
    }

    #[test]
    fn expired_sessions_are_rejected_and_swept() {
        let (db, id) = setup();
        db.with(|c| {
            c.execute(
                "INSERT INTO sessions (token, user_id, created_at, expires_at)
                 VALUES ('old', ?1, 0, 1)",
                params![id],
            )?;
            Ok(())
        })
        .unwrap();
        assert!(require(Some("old".into()), &db).is_err());
        let left = count_for_user(&db, id).unwrap();
        assert_eq!(left, 0, "the stale row should be gone");
    }

    #[test]
    fn sign_out_everywhere_clears_all_sessions() {
        let (db, id) = setup();
        for _ in 0..3 {
            db.with(|c| create(c, id)).unwrap();
        }
        assert_eq!(count_for_user(&db, id).unwrap(), 3);
        db.with(|c| destroy_all_for(c, id)).unwrap();
        assert_eq!(count_for_user(&db, id).unwrap(), 0);
    }

    #[test]
    fn cookie_parsing_survives_junk() {
        assert_eq!(token_from_cookie(Some("a=1; ohub_sid=abc; b=2")).as_deref(), Some("abc"));
        assert_eq!(token_from_cookie(Some("ohub_sid=")), None);
        assert_eq!(token_from_cookie(Some("other=1")), None);
        assert_eq!(token_from_cookie(None), None);
    }

    #[test]
    fn clear_header_expires_immediately() {
        assert!(clear_header().contains("Max-Age=0"));
    }
}
