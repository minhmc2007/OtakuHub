//! The on disk cache. Content addressed files mean a request for the same segment twice hits
//! the disk, and `media_cache` records size and access time for the evictor.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use rusqlite::params;

use crate::db::Db;
use crate::error::{AppError, AppResult};
use crate::time::now_secs;

/// Bytes handed out, used by the settings page to show throughput.
static SERVED: AtomicU64 = AtomicU64::new(0);

pub fn served_bytes() -> u64 {
    SERVED.load(Ordering::Relaxed)
}

pub fn note_served(bytes: usize) {
    SERVED.fetch_add(bytes as u64, Ordering::Relaxed);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Segment,
    Thumb,
    Key,
    Subtitle,
}

impl Kind {
    fn dir(self) -> &'static str {
        match self {
            Kind::Segment => "blobs",
            Kind::Key => "blobs",
            Kind::Subtitle => "blobs",
            Kind::Thumb => "thumbs",
        }
    }
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Segment => "segment",
            Kind::Thumb => "thumb",
            Kind::Key => "key",
            Kind::Subtitle => "subtitle",
        }
    }
    fn parse(s: &str) -> Kind {
        match s {
            "thumb" => Kind::Thumb,
            "key" => Kind::Key,
            "subtitle" => Kind::Subtitle,
            _ => Kind::Segment,
        }
    }
    fn extension(self) -> &'static str {
        match self {
            Kind::Thumb => "img",
            Kind::Subtitle => "vtt",
            _ => "bin",
        }
    }
}

#[derive(Clone)]
pub struct Cache {
    root: PathBuf,
    jobs_dir: PathBuf,
}

impl Cache {
    pub fn new(root: PathBuf) -> AppResult<Self> {
        let jobs_dir = root.join("jobs");
        std::fs::create_dir_all(root.join("blobs"))?;
        std::fs::create_dir_all(root.join("thumbs"))?;
        std::fs::create_dir_all(&jobs_dir)?;
        Ok(Self { root, jobs_dir })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Path of the cached copy of an upstream URL.
    pub fn path_of(&self, kind: Kind, url: &str) -> PathBuf {
        let key = hash(url);
        self.root
            .join(kind.dir())
            .join(&key[..2])
            .join(format!("{}.{}", &key[2..], kind.extension()))
    }

    /// A segment we fetched ourselves, so its extension is known from the URL.
    pub fn segment_path(&self, playlist_url: &str, name: &str) -> PathBuf {
        let key = hash(&format!("{playlist_url}{name}"));
        let ext = name.rsplit('.').next().filter(|e| e.len() <= 4).unwrap_or("ts");
        let ext: String = ext.chars().filter(char::is_ascii_alphanumeric).collect();
        self.root
            .join("blobs")
            .join(&key[..2])
            .join(format!("{}.{}", &key[2..], if ext.is_empty() { "ts" } else { &ext }))
    }

    pub fn read(&self, path: &Path) -> Option<Vec<u8>> {
        std::fs::read(path).ok()
    }

    /// Write a fetched body into the cache. A partial write is removed, so a cache hit
    /// never returns half a file.
    pub fn store(&self, path: &Path, body: &[u8]) -> AppResult<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("part");
        std::fs::write(&tmp, body)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// Directory for a transcode job.
    pub fn job_dir(&self, id: &str) -> PathBuf {
        self.jobs_dir.join(id)
    }

    /// Record a file so the evictor can find it later. The path is not needed here: the
    /// evictor recomputes it from the key, which is the point of content addressing.
    pub fn track(&self, db: &Db, kind: Kind, url: &str, _path: &Path, bytes: u64) -> AppResult<()> {
        let now = now_secs();
        db.with(|c| {
            c.execute(
                "INSERT INTO media_cache (key, bytes, created_at, touched_at) VALUES (?1, ?2, ?3, ?3)
                 ON CONFLICT(key) DO UPDATE SET bytes = excluded.bytes, touched_at = excluded.touched_at",
                params![cache_key(kind, url), bytes as i64, now],
            )?;
            Ok(())
        })
    }

    pub fn touch(&self, db: &Db, kind: Kind, url: &str) -> AppResult<()> {
        let n = db.with(|c| {
            Ok(c.execute(
                "UPDATE media_cache SET touched_at = ?1 WHERE key = ?2",
                params![now_secs(), cache_key(kind, url)],
            )?)
        })?;
        // A miss means the row was swept while the file survived, or the cache was
        // rebuilt. Tracking again on read keeps the accounting honest.
        if n == 0 {
            if let Some(bytes) = self.size_of_file(&self.path_of(kind, url)) {
                self.track(db, kind, url, &self.path_of(kind, url), bytes)?;
            }
        }
        Ok(())
    }

    fn size_of_file(&self, path: &Path) -> Option<u64> {
        std::fs::metadata(path).ok().map(|m| m.len())
    }

    pub fn total_bytes(&self, db: &Db) -> AppResult<u64> {
        db.with(|c| {
            Ok(c.query_row(
                "SELECT COALESCE(SUM(bytes), 0) FROM media_cache",
                [],
                |r| r.get::<_, i64>(0),
            )? as u64)
        })
    }

    /// Files on disk, which is the truth when the table has drifted.
    pub fn actual_bytes(&self) -> u64 {
        crate::metrics::dir_size(&self.root)
    }

    /// Delete the least recently used files until the total is back under `max_bytes`.
    /// Returns how many files went away.
    pub fn evict_to(&self, db: &Db, max_bytes: u64) -> AppResult<usize> {
        let mut current = self.total_bytes(db)?;
        if current <= max_bytes {
            return Ok(0);
        }
        let victims: Vec<(String, Kind, String)> = db.with(|c| {
            let mut stmt = c.prepare(
                "SELECT key, bytes FROM media_cache ORDER BY touched_at ASC LIMIT 500",
            )?;
            let rows = stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, i64>(1)?)))?;
            let mut out = Vec::new();
            for row in rows {
                let (key, bytes): (String, i64) = row?;
                let kind = key.split_once('|').map(|(k, _)| k.to_string());
                let url = key.split_once('|').map(|(_, u)| u.to_string());
                let (Some(kind), Some(url)) = (kind, url) else {
                    continue;
                };
                out.push((key.clone(), Kind::parse(&kind), url));
                current = current.saturating_sub(bytes as u64);
                if current <= max_bytes {
                    break;
                }
            }
            Ok(out)
        })?;

        let mut removed = 0;
        for (key, kind, url) in victims {
            let path = self.path_of(kind, &url);
            if std::fs::remove_file(&path).is_ok() {
                removed += 1;
            }
            db.with(|c| {
                c.execute("DELETE FROM media_cache WHERE key = ?1", params![key])?;
                Ok(())
            })?;
        }
        Ok(removed)
    }

    /// Forget every tracked file. The files themselves are left to `purge_dirs`.
    pub fn forget_all(&self, db: &Db) -> AppResult<usize> {
        db.with(|c| Ok(c.execute("DELETE FROM media_cache", [])?))
    }

    pub fn purge(&self, db: &Db) -> AppResult<usize> {
        let removed = self.forget_all(db)?;
        for dir in ["blobs", "thumbs"] {
            let _ = std::fs::remove_dir_all(self.root.join(dir));
        }
        std::fs::create_dir_all(self.root.join("blobs"))?;
        std::fs::create_dir_all(self.root.join("thumbs"))?;
        Ok(removed)
    }

    /// Files older than the TTL, which are almost always dead CDN links.
    pub fn sweep_expired(&self, db: &Db, ttl_secs: i64) -> AppResult<usize> {
        let cutoff = now_secs() - ttl_secs;
        let stale: Vec<(String, Kind, String)> = db.with(|c| {
            let mut stmt = c.prepare("SELECT key FROM media_cache WHERE touched_at < ?1")?;
            let rows = stmt.query_map(params![cutoff], |r| r.get::<_, String>(0))?;
            let mut out = Vec::new();
            for row in rows {
                let key = row?;
                if let Some((kind, url)) = key.split_once('|') {
                    out.push((key.clone(), Kind::parse(kind), url.to_string()));
                }
            }
            Ok(out)
        })?;
        let mut n = 0;
        for (key, kind, url) in stale {
            let _ = std::fs::remove_file(self.path_of(kind, &url));
            db.with(|c| {
                c.execute("DELETE FROM media_cache WHERE key = ?1", params![key])?;
                Ok(())
            })?;
            n += 1;
        }
        Ok(n)
    }

    /// Remove job directories whose row is gone.
    pub fn sweep_orphan_jobs(&self, keep: &std::collections::HashSet<String>) -> AppResult<usize> {
        let Ok(entries) = std::fs::read_dir(&self.jobs_dir) else {
            return Ok(0);
        };
        let mut n = 0;
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            if !keep.contains(&name) && std::fs::remove_dir_all(entry.path()).is_ok() {
                n += 1;
            }
        }
        Ok(n)
    }
}

fn cache_key(kind: Kind, url: &str) -> String {
    format!("{}|{}", kind.as_str(), url)
}

/// Lowercase hex SHA256. Content addressing is the whole point of the layout, so this
/// is the one hash in the app that must never change.
pub fn hash(input: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(input.as_bytes());
    crate::auth::hex(&h.finalize())
}

/// Guess a content type from a URL, for the `Content-Type` on a cached response.
pub fn content_type_for(kind: Kind, url: &str) -> &'static str {
    match kind {
        Kind::Thumb => "image/jpeg",
        Kind::Subtitle => "text/vtt",
        _ => {
            let path = url.split(['?', '#']).next().unwrap_or(url).to_ascii_lowercase();
            if path.ends_with(".m3u8") {
                "application/vnd.apple.mpegurl"
            } else if path.ends_with(".m4s") || path.ends_with(".mp4") {
                "video/iso.segment"
            } else if path.ends_with(".aac") {
                "audio/aac"
            } else if path.ends_with(".vtt") {
                "text/vtt"
            } else {
                // A segment we do not recognise is still a segment, and HLS is the only
                // thing this route carries.
                "video/mp2t"
            }
        }
    }
}

pub fn not_found(what: &str) -> AppError {
    AppError::not_found(what)
}

/// The table key for a URL. Public because the proxy reads the access time directly.
pub fn key_for(kind: Kind, url: &str) -> String {
    cache_key(kind, url)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache() -> (tempfile::TempDir, Cache, Db) {
        let dir = tempfile::tempdir().unwrap();
        let cache = Cache::new(dir.path().to_path_buf()).unwrap();
        let db = Db::open_memory().unwrap();
        (dir, cache, db)
    }

    #[test]
    fn hash_is_stable_and_url_specific() {
        assert_eq!(hash("https://a.test/x.ts"), hash("https://a.test/x.ts"));
        assert_ne!(hash("https://a.test/x.ts"), hash("https://a.test/y.ts"));
        assert_eq!(hash("x").len(), 64);
    }

    #[test]
    fn same_url_maps_to_the_same_path() {
        let (_d, cache, _db) = cache();
        assert_eq!(cache.path_of(Kind::Segment, "u"), cache.path_of(Kind::Segment, "u"));
        assert_ne!(cache.path_of(Kind::Segment, "u"), cache.path_of(Kind::Thumb, "u"));
    }

    #[test]
    fn store_and_read_round_trip() {
        let (_d, cache, _db) = cache();
        let path = cache.path_of(Kind::Segment, "https://a.test/s.ts");
        cache.store(&path, b"payload").unwrap();
        assert_eq!(cache.read(&path).unwrap(), b"payload");
    }

    #[test]
    fn a_missing_file_reads_as_none() {
        let (_d, cache, _db) = cache();
        assert!(cache.read(&cache.path_of(Kind::Segment, "nope")).is_none());
    }

    #[test]
    fn no_part_files_survive_a_store() {
        let (_d, cache, _db) = cache();
        let path = cache.path_of(Kind::Segment, "u");
        cache.store(&path, b"x").unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .flatten()
            .filter(|e| e.file_name().to_string_lossy().ends_with(".part"))
            .collect();
        assert!(leftovers.is_empty());
    }

    #[test]
    fn segment_paths_use_the_url_extension() {
        let (_d, cache, _db) = cache();
        let p = cache.segment_path("https://h.test/v/1080/index.m3u8", "seg_00001.ts");
        assert!(p.to_string_lossy().ends_with(".ts"));
        let p = cache.segment_path("https://h.test/v/x/index.m3u8", "seg_1.m4s");
        assert!(p.to_string_lossy().ends_with(".m4s"));
    }

    #[test]
    fn tracking_accumulates_and_touches_stay_unique() {
        let (_d, cache, db) = cache();
        cache.track(&db, Kind::Segment, "u1", Path::new("a"), 100).unwrap();
        cache.track(&db, Kind::Segment, "u2", Path::new("b"), 250).unwrap();
        assert_eq!(cache.total_bytes(&db).unwrap(), 350);
        cache.touch(&db, Kind::Segment, "u1").unwrap();
        assert_eq!(cache.total_bytes(&db).unwrap(), 350, "a touch must not change the size");
    }

    #[test]
    fn tracking_the_same_url_twice_replaces_the_row() {
        let (_d, cache, db) = cache();
        cache.track(&db, Kind::Segment, "u", Path::new("a"), 100).unwrap();
        cache.track(&db, Kind::Segment, "u", Path::new("a"), 180).unwrap();
        assert_eq!(cache.total_bytes(&db).unwrap(), 180);
    }

    #[test]
    fn eviction_trims_to_the_ceiling_and_removes_the_files() {
        let (_d, cache, db) = cache();
        for i in 0..5 {
            let url = format!("u{i}");
            let p = cache.path_of(Kind::Segment, &url);
            cache.store(&p, &vec![0u8; 1000]).unwrap();
            cache.track(&db, Kind::Segment, &url, &p, 1000).unwrap();
        }
        for i in 0..5 {
            cache.touch(&db, Kind::Segment, &format!("u{i}")).unwrap();
            std::thread::sleep(std::time::Duration::from_millis(2));
        }
        let removed = cache.evict_to(&db, 2500).unwrap();
        assert!(removed >= 2, "expected at least two files evicted, got {removed}");
        assert!(cache.total_bytes(&db).unwrap() <= 2500);
        assert!(!cache.path_of(Kind::Segment, "u0").exists(), "the oldest file is gone");
    }

    #[test]
    fn eviction_below_the_total_is_a_noop() {
        let (_d, cache, db) = cache();
        cache.track(&db, Kind::Segment, "u", Path::new("a"), 10).unwrap();
        assert_eq!(cache.evict_to(&db, 1_000_000).unwrap(), 0);
    }

    #[test]
    fn expiry_drops_old_rows_and_files() {
        let (_d, cache, db) = cache();
        let p = cache.path_of(Kind::Segment, "old");
        cache.store(&p, b"x").unwrap();
        cache.track(&db, Kind::Segment, "old", &p, 1).unwrap();
        assert_eq!(cache.sweep_expired(&db, 1_000_000).unwrap(), 0, "nothing is old yet");
        assert_eq!(cache.sweep_expired(&db, -1).unwrap(), 1);
        assert!(!p.exists());
        assert_eq!(cache.total_bytes(&db).unwrap(), 0);
    }

    #[test]
    fn purge_clears_everything_and_recreates_the_dirs() {
        let (_d, cache, db) = cache();
        let p = cache.path_of(Kind::Thumb, "u");
        cache.store(&p, b"img").unwrap();
        cache.track(&db, Kind::Thumb, "u", &p, 3).unwrap();
        cache.purge(&db).unwrap();
        assert_eq!(cache.total_bytes(&db).unwrap(), 0);
        assert!(!p.exists());
        assert!(cache.root().join("thumbs").is_dir(), "the layout is restored");
        cache.store(&cache.path_of(Kind::Thumb, "v"), b"x").unwrap();
    }

    #[test]
    fn orphan_job_dirs_are_removed() {
        let (_d, cache, _db) = cache();
        let keep = cache.job_dir("keep");
        let drop = cache.job_dir("drop");
        std::fs::create_dir_all(&keep).unwrap();
        std::fs::create_dir_all(&drop).unwrap();
        let keep_set: std::collections::HashSet<String> = ["keep".to_string()].into_iter().collect();
        assert_eq!(cache.sweep_orphan_jobs(&keep_set).unwrap(), 1);
        assert!(keep.exists());
        assert!(!drop.exists());
    }

    #[test]
    fn content_types_by_extension() {
        assert_eq!(content_type_for(Kind::Thumb, "x"), "image/jpeg");
        assert_eq!(content_type_for(Kind::Subtitle, "x"), "text/vtt");
        assert_eq!(content_type_for(Kind::Segment, "a/index.M3U8"), "application/vnd.apple.mpegurl");
        assert_eq!(content_type_for(Kind::Segment, "a/s.m4s"), "video/iso.segment");
        assert_eq!(content_type_for(Kind::Segment, "a/s.ts"), "video/mp2t");
    }

    #[test]
    fn served_counter_moves_forward() {
        let before = served_bytes();
        note_served(1024);
        assert_eq!(served_bytes(), before + 1024);
    }
}
