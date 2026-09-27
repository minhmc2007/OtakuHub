//! The transcode job runner. One worker per episode on a blocking thread, since libav is
//! synchronous. The database row is the state, so a crash leaves a row that startup can reap.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tokio::task::JoinHandle;

use crate::db::jobs::{self, State};
use crate::error::AppResult;
use crate::media::{probe, transcode};
use crate::state::AppState;

/// Cancel flags for jobs that are running right now, keyed by job id.
#[derive(Clone, Default)]
pub struct Running {
    flags: Arc<Mutex<HashMap<String, Arc<AtomicBool>>>>,
}

impl Running {
    pub fn is_active(&self, id: &str) -> bool {
        self.flags.lock().map(|m| m.contains_key(id)).unwrap_or(false)
    }

    pub fn active_count(&self) -> usize {
        self.flags.lock().map(|m| m.len()).unwrap_or(0)
    }

    /// Ask a job to stop. Returns false when it was not running, which is a noop, since the
    /// caller may be looking at stale state.
    pub fn cancel(&self, id: &str) -> bool {
        self.flags
            .lock()
            .ok()
            .and_then(|mut m| m.remove(id))
            .map(|f| {
                f.store(true, Ordering::Relaxed);
                true
            })
            .unwrap_or(false)
    }

    fn insert(&self, id: &str, flag: Arc<AtomicBool>) {
        if let Ok(mut m) = self.flags.lock() {
            m.insert(id.to_string(), flag);
        }
    }

    fn remove(&self, id: &str) {
        if let Ok(mut m) = self.flags.lock() {
            m.remove(id);
        }
    }
}

/// Start a job, or return the existing row for the same request, so the caller can point
/// the player at `/media/job/<id>/index.m3u8` either way.
pub fn enqueue(
    state: &AppState,
    request: transcode::Request,
    spec: crate::db::jobs::Job,
) -> AppResult<crate::db::jobs::Job> {
    // A finished job for the same inputs is the whole point of caching it.
    match state.db().with(|c| jobs::by_id(c, &spec.id))? {
        // A finished job for the same inputs is the whole point of caching it.
        Some(existing) if existing.state == State::Done && job_dir_complete(state, &existing.id) => {
            return Ok(existing)
        }
        // A job already in flight is the job the caller asked for.
        Some(existing) if !existing.state.is_terminal() => return Ok(existing),
        // Failed or cancelled: start again from a clean directory.
        Some(existing) => {
            let _ = std::fs::remove_dir_all(state.cache().job_dir(&existing.id));
        }
        None => {}
    }

    let job = crate::db::jobs::Job {
        state: State::Queued,
        ..spec
    };
    state.db().with(|c| jobs::insert(c, &job))?;
    let id = job.id.clone();
    spawn(state, job, request);
    state
        .db()
        .with(|c| jobs::by_id(c, &id))?
        .ok_or_else(|| crate::error::AppError::internal("the job row vanished after insert"))
}

/// Spawn the worker. The libav pipeline is blocking, so it gets its own thread.
fn spawn(state: &AppState, job: crate::db::jobs::Job, request: transcode::Request) {
    let cancel = Arc::new(AtomicBool::new(false));
    state.running().insert(&job.id, Arc::clone(&cancel));

    let app = state.clone();
    let workers = app.workers().clone();
    let id = job.id.clone();
    let out_dir = request.out_dir.clone();
    let handle = tokio::task::spawn_blocking(move || {
        let _ = std::fs::create_dir_all(&out_dir);
        let db = app.db().clone();
        let running = app.running().clone();
        let _ = db.with(|c| jobs::set_state(c, &id, State::Running, None));

        let started = std::time::Instant::now();
        let mut on_progress = |p: transcode::Progress| {
            if p.frames % 30 == 0 {
                tracing::debug!(job = %id, frames = p.frames, height = p.out_height, "transcode running");
            }
        };

        let result = transcode::run(&request, &cancel, &mut on_progress);
        // Software is a fallback, so it has to be reachable at run time too. A cancelled
        // run is never retried, since the viewer asked it to stop.
        let (result, fell_back) = match &result {
            Err(_) if request.backend != crate::media::Encoder::Software => {
                if cancel.load(Ordering::Relaxed) {
                    (result, false)
                } else {
                    tracing::warn!(
                        job = %id,
                        backend = %request.backend.encoder_name(request.codec),
                        "retrying in software after the hardware encoder produced nothing"
                    );
                    let mut soft = request.clone();
                    soft.backend = crate::media::Encoder::Software;
                    // A half written directory from the failed attempt would be served as
                    // if it were the finished job.
                    let _ = std::fs::remove_dir_all(&request.out_dir);
                    let _ = std::fs::create_dir_all(&request.out_dir);
                    (transcode::run(&soft, &cancel, &mut on_progress), true)
                }
            }
            _ => (result, false),
        };
        let was_cancelled = cancel.load(Ordering::Relaxed);
        let took = started.elapsed();

        if let (Ok(()), true) = (&result, fell_back) {
            let _ = db.with(|c| jobs::set_encoder(c, &id, "libx264/libx265"));
            tracing::info!(job = %id, secs = took.as_secs(), "job done in software");
        }

        match result {
            Ok(()) => {
                let _ = db.with(|c| jobs::set_state(c, &id, State::Done, None));
                tracing::info!(job = %id, secs = took.as_secs(), "job done");
            }
            Err(e) => {
                // A cancelled run leaves a partial directory that must not be served.
                if was_cancelled {
                    let _ = std::fs::remove_dir_all(&request.out_dir);
                    let _ = db.with(|c| jobs::set_state(c, &id, State::Cancelled, None));
                    tracing::info!(job = %id, "job cancelled");
                } else {
                    tracing::error!(job = %id, error = %e, "job failed");
                    let _ = db.with(|c| jobs::set_state(c, &id, State::Failed, Some(&e.to_string())));
                }
            }
        }
        running.remove(&id);
    });
    workers.track(handle);
}

/// A job directory is playable once `index.m3u8` exists.
pub fn job_dir_complete(state: &AppState, id: &str) -> bool {
    state.cache().job_dir(id).join("index.m3u8").is_file()
}

/// The playlist of a job, read from disk. A running job's playlist has no `ENDLIST`,
/// which is how the player knows to keep polling.
pub fn job_playlist(state: &AppState, id: &str) -> AppResult<String> {
    let path = state.cache().job_dir(id).join("index.m3u8");
    let body = std::fs::read_to_string(&path)
        .map_err(|_| crate::error::AppError::not_found("playlist"))?;
    Ok(crate::proxy::rewrite_for_local(&body, id))
}

/// Bytes on disk for one job.
pub fn job_size(state: &AppState, id: &str) -> u64 {
    crate::metrics::dir_size(&state.cache().job_dir(id))
}

/// Reap jobs that were running when the process stopped, and drop their partial output.
pub fn reap_stalled(state: &AppState) -> AppResult<usize> {
    let stalled = state.db().with(jobs::unfinished)?;
    let mut n = 0;
    for job in stalled {
        let _ = std::fs::remove_dir_all(state.cache().job_dir(&job.id));
        let _ = state
            .db()
            .with(|c| jobs::set_state(c, &job.id, State::Failed, Some("interrupted by a restart")));
        n += 1;
    }
    Ok(n)
}

/// Delete a job's row and its directory.
pub fn drop_job(state: &AppState, id: &str, user_id: i64) -> AppResult<()> {
    state.running().cancel(id);
    let _ = std::fs::remove_dir_all(state.cache().job_dir(id));
    if !state.db().with(|c| jobs::delete(c, id, user_id))? {
        return Err(crate::error::AppError::not_found("job"));
    }
    Ok(())
}

/// Honour the user's hardware switch. Turning it off gets software on a box that could
/// do better, which is what they asked for.
pub fn choose_encoder(state: &AppState, want_hardware: bool) -> probe::Encoder {
    if !want_hardware {
        return probe::Encoder::Software;
    }
    if !state.config().hw_encode {
        return probe::Encoder::Software;
    }
    state.hardware().encoder
}

/// Keep the join handles so a shutdown can wait for workers to notice a cancel.
#[derive(Clone, Default)]
pub struct Workers {
    handles: Arc<Mutex<Vec<JoinHandle<()>>>>,
}

impl Workers {
    pub fn track(&self, handle: JoinHandle<()>) {
        if let Ok(mut v) = self.handles.lock() {
            v.retain(|h| !h.is_finished());
            v.push(handle);
        }
    }
    pub fn outstanding(&self) -> usize {
        self.handles.lock().map(|v| v.iter().filter(|h| !h.is_finished()).count()).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancel_only_reports_a_running_job() {
        let running = Running::default();
        assert!(!running.cancel("nothing"));
        let flag = Arc::new(AtomicBool::new(false));
        running.insert("job", Arc::clone(&flag));
        assert!(running.is_active("job"));
        assert_eq!(running.active_count(), 1);
        assert!(running.cancel("job"));
        assert!(flag.load(Ordering::Relaxed));
        assert!(!running.is_active("job"));
        assert!(!running.cancel("job"), "a second cancel is a noop");
    }

    #[test]
    fn two_jobs_track_separately() {
        let running = Running::default();
        let a = Arc::new(AtomicBool::new(false));
        let b = Arc::new(AtomicBool::new(false));
        running.insert("a", Arc::clone(&a));
        running.insert("b", Arc::clone(&b));
        assert!(running.cancel("a"));
        assert!(a.load(Ordering::Relaxed), "a was asked to stop");
        assert!(!b.load(Ordering::Relaxed), "b was left alone");
        assert!(running.is_active("b"));
        assert_eq!(running.active_count(), 1);
    }

    #[tokio::test]
    async fn worker_handles_are_counted_while_running() {
        let workers = Workers::default();
        assert_eq!(workers.outstanding(), 0);
        workers.track(tokio::task::spawn_blocking(|| {
            std::thread::sleep(std::time::Duration::from_millis(50))
        }));
        assert_eq!(workers.outstanding(), 1);
        // Once it finishes, the next count drops it.
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        assert_eq!(workers.outstanding(), 0);
    }
}
