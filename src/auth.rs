//! Password hashing, random ids and the axum extractor that turns a cookie into a user.

use axum::extract::FromRequestParts;
use axum::http::request::Parts;

use crate::db::sessions;
use crate::db::users::User;
use crate::error::{AppError, AppResult};
use crate::state::AppState;

use argon2::Argon2;
use base64::Engine;
use password_hash::{PasswordHasher, PasswordVerifier, SaltString};

/// The legacy digest, kept only so an existing account can still sign in once and be
/// rehashed. SHA512 is a fast digest, not a password hash, and the database can leak.
fn legacy_hash(password: &str, salt_hex: &str) -> String {
    use sha2::{Digest, Sha512};
    let mut h = Sha512::new();
    h.update(salt_hex.as_bytes());
    h.update([0u8]);
    h.update(password.as_bytes());
    hex(&h.finalize())
}

/// Argon2id, with the salt carried in the encoded hash. A memory hard KDF, so a stolen
/// database cannot be ground through at the rate a GPU does SHA512.
pub fn hash_password(password: &str) -> AppResult<String> {
    let salt = SaltString::encode_b64(&random_bytes(16))
        .map_err(|e| AppError::internal(e.to_string()))?;
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| AppError::internal(format!("cannot hash a password: {e}")))
}

/// Check a password against either format, and say whether it needs rehashing.
pub fn verify_password(stored: &str, password: &str, salt_hex: &str) -> (bool, bool) {
    if stored.starts_with("$argon2") {
        let parsed = password_hash::PasswordHash::new(stored);
        let Ok(parsed) = parsed else {
            return (false, false);
        };
        return match Argon2::default().verify_password(password.as_bytes(), &parsed) {
            Ok(()) => (true, false),
            Err(_) => (false, false),
        };
    }
    // An account from before the move. Correct comparison, then an upgrade on next sign in.
    let ok = constant_time_eq(stored.as_bytes(), legacy_hash(password, salt_hex).as_bytes());
    (ok, ok)
}

pub fn hex(bytes: &[u8]) -> String {
    const D: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(D[(b >> 4) as usize] as char);
        out.push(D[(b & 0x0f) as usize] as char);
    }
    out
}

fn random_bytes(n: usize) -> Vec<u8> {
    let mut buf = vec![0u8; n];
    getrandom::fill(&mut buf).expect("the OS random source is unavailable");
    buf
}

pub fn new_salt() -> String {
    hex(&random_bytes(16))
}

pub fn new_token() -> String {
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(random_bytes(32))
}

/// Length independent comparison, so a wrong password cannot be found byte by byte.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// The signed in user, or a clear error. Handlers take this instead of a session token.
#[derive(Debug, Clone)]
pub struct CurrentUser(pub User);

impl CurrentUser {
    pub fn id(&self) -> i64 {
        self.0.id
    }
    pub fn name(&self) -> &str {
        &self.0.username
    }
}

impl FromRequestParts<AppState> for CurrentUser {
    type Rejection = AppError;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        let token = sessions::token_from_headers(&parts.headers);
        sessions::require(token, state.db()).map(CurrentUser)
    }
}

/// The signed in user if there is one, without requiring a sign in.
#[derive(Debug, Clone)]
pub struct MaybeUser(pub Option<User>);

impl FromRequestParts<AppState> for MaybeUser {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, state: &AppState) -> Result<Self, Self::Rejection> {
        let token = sessions::token_from_headers(&parts.headers);
        let user = token.and_then(|t| state.db().with(|c| sessions::resolve(c, &t)).ok().flatten());
        Ok(MaybeUser(user))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::Db;

    #[test]
    fn argon2_hashes_verify_and_are_salted() {
        let a = hash_password("hunter2000").unwrap();
        let b = hash_password("hunter2000").unwrap();
        assert_ne!(a, b, "two hashes of one password must differ, so the salt is real");
        assert!(a.starts_with("$argon2"), "not an argon2 hash: {a}");
        assert!(verify_password(&a, "hunter2000", "").0);
        assert!(!verify_password(&a, "wrong", "").0);
    }

    #[test]
    fn a_legacy_account_verifies_and_asks_to_be_upgraded() {
        let stored = legacy_hash("hunter2000", "aabb");
        let (ok, upgrade) = verify_password(&stored, "hunter2000", "aabb");
        assert!(ok);
        assert!(upgrade, "a sha-512 account must be rehashed on next sign in");
        assert!(!verify_password(&stored, "nope", "aabb").0);
    }

    #[test]
    fn the_legacy_digest_is_stable_for_the_same_inputs() {
        assert_eq!(legacy_hash("x", "s"), legacy_hash("x", "s"));
        assert_ne!(legacy_hash("x", "s"), legacy_hash("x", "t"));
    }

    #[test]
    fn constant_time_eq_behaves_like_eq() {
        assert!(constant_time_eq(b"abc", b"abc"));
        assert!(!constant_time_eq(b"abc", b"abd"));
        assert!(!constant_time_eq(b"abc", b"ab"));
        assert!(constant_time_eq(b"", b""));
    }

    #[test]
    fn salts_and_tokens_are_unique() {
        let mut salts = std::collections::HashSet::new();
        for _ in 0..200 {
            assert!(salts.insert(new_salt()));
        }
        let mut toks = std::collections::HashSet::new();
        for _ in 0..200 {
            assert!(toks.insert(new_token()));
        }
        assert!(!new_token().contains('='), "no padding, so it drops straight into a cookie");
    }

    #[test]
    fn a_real_account_verifies() {
        let db = Db::open_memory().unwrap();
        db.with(|c| crate::db::users::create(c, "rei", "hunter2000")).unwrap();
        assert!(crate::db::users::login(&db, "rei", "hunter2000").is_ok());
        assert!(crate::db::users::login(&db, "rei", "hunter2001").is_err());
    }
}
