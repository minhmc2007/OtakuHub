//! Shared application state. Everything a handler needs is behind this one cloneable value.

use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use crate::cache::Cache;
use crate::config::Config;
use crate::db::Db;
use crate::error::AppResult;
use crate::media::jobs::{Running, Workers};
use crate::media::probe::{self, Hardware};
use crate::metrics::Meter;
use crate::source::hianime::HiAnime;
use crate::source::AnimeSource;

#[derive(Clone)]
pub struct AppState(Arc<Inner>);

struct Inner {
    pub config: Config,
    pub db: Db,
    pub cache: Cache,
    pub http: reqwest::Client,
    pub source: Arc<dyn AnimeSource>,
    pub hardware: Hardware,
    pub running: Running,
    pub workers: Workers,
    pub metrics: Mutex<Meter>,
    pub started: Instant,
    /// Never incremented: nothing calls `note_upstream_fetch`, so the settings row reads 0 forever.
    pub upstream_fetches: AtomicU64,
}

impl AppState {
    pub async fn build(config: Config) -> AppResult<Self> {
        std::fs::create_dir_all(&config.cache_dir)?;
        std::fs::create_dir_all(&config.transcode_dir)?;
        let db = Db::open(&config.db_path())?;
        let cache = Cache::new(config.cache_dir.clone())?;
        let source: Arc<dyn AnimeSource> = Arc::new(HiAnime::new(
            &config.origins.base,
            config.origins.embed_servers.clone(),
        )?);
        let hardware = probe::detect(config.hw_encode).unwrap_or_else(|e| {
            tracing::warn!(error = %e, "hardware probe failed, using software encoding");
            Hardware {
                encoder: probe::Encoder::Software,
                device: "cpu".into(),
                detail: format!("probe failed: {e}"),
                rejected: Vec::new(),
            }
        });
        Ok(Self(Arc::new(Inner {
            http: crate::proxy::client(),
            source,
            hardware,
            running: Running::default(),
            workers: Workers::default(),
            metrics: Mutex::new(Meter::new()),
            started: Instant::now(),
            upstream_fetches: AtomicU64::new(0),
            db,
            cache,
            config,
        })))
    }

    pub fn config(&self) -> &Config {
        &self.0.config
    }
    pub fn db(&self) -> &Db {
        &self.0.db
    }
    pub fn cache(&self) -> &Cache {
        &self.0.cache
    }
    pub fn http(&self) -> &reqwest::Client {
        &self.0.http
    }
    pub fn source(&self) -> &Arc<dyn AnimeSource> {
        &self.0.source
    }
    pub fn hardware(&self) -> &Hardware {
        &self.0.hardware
    }
    pub fn running(&self) -> &Running {
        &self.0.running
    }
    pub fn workers(&self) -> &Workers {
        &self.0.workers
    }
    pub fn uptime(&self) -> std::time::Duration {
        self.0.started.elapsed()
    }
    pub fn note_upstream_fetch(&self) {
        self.0.upstream_fetches.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }
    pub fn upstream_fetches(&self) -> u64 {
        self.0.upstream_fetches.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// A metrics sample. The meter is behind a mutex because sysinfo is not reentrant.
    pub fn sample_metrics(&self) -> crate::metrics::Usage {
        let mut guard = self
            .0
            .metrics
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        guard.sample(&self.0.config.cache_dir, self.uptime())
    }

    /// The state a template needs for the header and the settings page.
    pub fn banner(&self) -> Banner {
        Banner {
            hardware: self.0.hardware.summary(),
            hardware_encoder: self.0.hardware.encoder.label().to_string(),
            hardware_is_gpu: self.0.hardware.is_hardware(),
            device: self.0.hardware.device.clone(),
            rejected: self.0
                .hardware
                .rejected
                .iter()
                .map(|(n, w)| format!("{n}: {w}"))
                .collect(),
            running_jobs: self.0.running.active_count(),
            uptime: crate::time::duration(self.uptime().as_secs() as i64),
            version: env!("CARGO_PKG_VERSION").to_string(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Banner {
    pub hardware: String,
    pub hardware_encoder: String,
    pub hardware_is_gpu: bool,
    pub device: String,
    pub rejected: Vec<String>,
    pub running_jobs: usize,
    pub uptime: String,
    pub version: String,
}
