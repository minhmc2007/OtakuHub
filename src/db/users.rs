//! User accounts. Passwords are Argon2id; the salt column is only read for accounts that
//! still hold a legacy SHA512 digest.

use rusqlite::{params, Connection, OptionalExtension};

use super::Db;
use crate::error::{AppError, AppResult};
use crate::time::now_secs;

pub const DEFAULT_PLAYLIST: &str = "Favourite";

#[derive(Debug, Clone)]
pub struct User {
    pub id: i64,
    pub username: String,
    pub created_at: i64,
}

pub fn create(conn: &Connection, username: &str, password: &str) -> AppResult<i64> {
    let name = username.trim();
    if name.is_empty() || name.len() > 64 {
        return Err(AppError::bad("username must be 1 to 64 characters"));
    }
    if password.len() < 6 {
        return Err(AppError::bad("password needs at least 6 characters"));
    }
    if !name.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.') {
        return Err(AppError::bad("username may only hold letters, digits, _ - and ."));
    }
    let salt = crate::auth::new_salt();
    let hash = crate::auth::hash_password(password)?;
    let now = now_secs();
    conn.execute(
        "INSERT INTO users (username, password_hash, salt, created_at) VALUES (?1, ?2, ?3, ?4)",
        params![name, hash, salt, now],
    )
    .map_err(|e| match e {
        rusqlite::Error::SqliteFailure(f, _) if f.code == rusqlite::ErrorCode::ConstraintViolation => {
            AppError::bad("that username is taken")
        }
        other => AppError::Db(other),
    })?;
    let id = conn.last_insert_rowid();

    // Every account starts with the default playlist so the add button always has a target.
    conn.execute(
        "INSERT INTO playlists (user_id, name, created_at) VALUES (?1, ?2, ?3)",
        params![id, DEFAULT_PLAYLIST, now],
    )?;
    conn.execute(
        "INSERT INTO settings (user_id) VALUES (?1)",
        params![id],
    )?;
    Ok(id)
}

pub fn find_by_name(conn: &Connection, username: &str) -> AppResult<Option<(User, String, String)>> {
    conn.query_row(
        "SELECT id, username, created_at, password_hash, salt FROM users WHERE username = ?1",
        params![username.trim()],
        |r| {
            Ok((
                User {
                    id: r.get(0)?,
                    username: r.get(1)?,
                    created_at: r.get(2)?,
                },
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
            ))
        },
    )
    .optional()
    .map_err(AppError::Db)
}

pub fn by_id(conn: &Connection, id: i64) -> AppResult<Option<User>> {
    conn.query_row(
        "SELECT id, username, created_at FROM users WHERE id = ?1",
        params![id],
        |r| {
            Ok(User {
                id: r.get(0)?,
                username: r.get(1)?,
                created_at: r.get(2)?,
            })
        },
    )
    .optional()
    .map_err(AppError::Db)
}

/// Verify a username and password. Same wording for "no such user" and "wrong password"
/// so the form cannot be used to enumerate accounts.
pub fn login(db: &Db, username: &str, password: &str) -> AppResult<User> {
    let found = db.with(|c| find_by_name(c, username))?;
    let (user, stored, salt) = found.ok_or_else(|| AppError::bad("wrong username or password"))?;
    let (ok, upgrade) = crate::auth::verify_password(&stored, password, &salt);
    if !ok {
        return Err(AppError::bad("wrong username or password"));
    }
    // An account still on the old digest is rewritten on its owner's next sign in.
    if upgrade {
        if let Ok(fresh) = crate::auth::hash_password(password) {
            let _ = db.with(|c| {
                c.execute(
                    "UPDATE users SET password_hash = ?1 WHERE id = ?2",
                    params![fresh, user.id],
                )
                .map_err(AppError::Db)
            });
        }
    }
    Ok(user)
}

pub fn rename(conn: &Connection, id: i64, new_name: &str) -> AppResult<()> {
    let name = new_name.trim();
    if name.is_empty() || name.len() > 64 {
        return Err(AppError::bad("username must be 1 to 64 characters"));
    }
    if !name.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '-' || c == '.') {
        return Err(AppError::bad("username may only hold letters, digits, _ - and ."));
    }
    conn.execute(
        "UPDATE users SET username = ?1 WHERE id = ?2",
        params![name, id],
    )?;
    Ok(())
}

pub fn set_password(conn: &Connection, id: i64, password: &str) -> AppResult<()> {
    if password.len() < 6 {
        return Err(AppError::bad("password needs at least 6 characters"));
    }
    let hash = crate::auth::hash_password(password)?;
    conn.execute(
        "UPDATE users SET password_hash = ?1 WHERE id = ?2",
        params![hash, id],
    )?;
    Ok(())
}

pub fn count(conn: &Connection) -> AppResult<i64> {
    Ok(conn.query_row("SELECT COUNT(*) FROM users", [], |r| r.get(0))?)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setup() -> Db {
        let db = Db::open_memory().unwrap();
        db.with(|c| create(c, "rei", "hunter2000")).unwrap();
        db
    }

    #[test]
    fn creates_account_with_default_playlist() {
        let db = Db::open_memory().unwrap();
        let id = db.with(|c| create(c, "rei", "hunter2000")).unwrap();
        let n: i64 = db
            .with(|c| {
                Ok(c.query_row(
                    "SELECT COUNT(*) FROM playlists WHERE user_id = ?1 AND name = ?2",
                    params![id, DEFAULT_PLAYLIST],
                    |r| r.get(0),
                )?)
            })
            .unwrap();
        assert_eq!(n, 1);
    }

    #[test]
    fn duplicate_usernames_are_refused() {
        let db = setup();
        let err = db.with(|c| create(c, "rei", "another")).unwrap_err();
        assert!(err.to_string().contains("taken"));
    }

    #[test]
    fn login_accepts_the_right_password_only() {
        let db = setup();
        let ok = db.with(|c| find_by_name(c, "rei")).unwrap().unwrap();
        assert_eq!(ok.0.username, "rei");
        assert!(login(&db, "rei", "hunter2000").is_ok());
        assert!(login(&db, "rei", "hunter2001").is_err());
        assert!(login(&db, "nobody", "hunter2000").is_err());
    }

    #[test]
    fn wrong_credentials_share_one_message() {
        let db = setup();
        let a = login(&db, "rei", "nope").unwrap_err().to_string();
        let b = login(&db, "ghost", "nope").unwrap_err().to_string();
        assert_eq!(a, b);
    }

    #[test]
    fn short_passwords_and_bad_names_are_rejected() {
        let db = Db::open_memory().unwrap();
        assert!(db.with(|c| create(c, "rei", "12345")).is_err());
        assert!(db.with(|c| create(c, "a b", "hunter2000")).is_err());
        assert!(db.with(|c| create(c, "  ", "hunter2000")).is_err());
    }

    #[test]
    fn each_user_gets_their_own_salt() {
        let db = Db::open_memory().unwrap();
        db.with(|c| create(c, "a", "hunter2000")).unwrap();
        db.with(|c| create(c, "b", "hunter2000")).unwrap();
        let sa: String = db.with(|c| Ok(c.query_row("SELECT salt FROM users WHERE username='a'", [], |r| r.get(0))?)).unwrap();
        let sb: String = db.with(|c| Ok(c.query_row("SELECT salt FROM users WHERE username='b'", [], |r| r.get(0))?)).unwrap();
        assert_ne!(sa, sb);
    }
}
