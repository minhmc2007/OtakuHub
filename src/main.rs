//! Binary entry point: configuration, startup housekeeping, and the server.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use otakuhub::error::{AppError, AppResult};
use otakuhub::{config, media, proxy, state, web};

/// How often the cache is trimmed.
const HOUSEKEEPING_SECS: u64 = 60;

/// How long a running conversion gets to notice a shutdown.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("otakuhub: {e}");
        std::process::exit(1);
    }
}

async fn run() -> AppResult<()> {
    init_tracing();

    let config = config::Config::load().map_err(AppError::internal)?;
    let addr: SocketAddr = format!("{}:{}", config.bind, config.port)
        .parse()
        .map_err(|e| AppError::internal(format!("bad listen address: {e}")))?;

    let state = state::AppState::build(config).await?;

    // A conversion cannot outlive the process, so anything still marked running when we
    // start is not runnable and its partial output is not worth keeping.
    match media::jobs::reap_stalled(&state) {
        Ok(n) if n > 0 => tracing::warn!(jobs = n, "cleared conversions left by a restart"),
        Err(e) => tracing::warn!(error = %e, "could not clear stalled conversions"),
        _ => {}
    }
    if let Err(e) = sweep_orphan_jobs(&state) {
        tracing::warn!(error = %e, "could not clear orphan job directories");
    }

    // Rows saved while the provider was down kept a slug as their title.
    tokio::spawn(relabel_stale_titles(state.clone()));

    log_banner(&state, addr);

    let upkeep = {
        let state = state.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(Duration::from_secs(HOUSEKEEPING_SECS));
            loop {
                ticker.tick().await;
                proxy::maintain(&state).await;
            }
        })
    };

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| AppError::internal(format!("cannot bind {addr}: {e}")))?;
    let server = axum::serve(listener, web::router(state.clone()))
        .with_graceful_shutdown(shutdown_signal());

    if let Err(e) = server.await {
        tracing::error!(error = %e, "the server stopped with an error");
    }
    upkeep.abort();

    // Give a running conversion a moment to notice the shutdown and tidy up.
    let deadline = Instant::now() + SHUTDOWN_GRACE;
    while state.workers().outstanding() > 0 && Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    if state.workers().outstanding() > 0 {
        tracing::warn!("a conversion is still running and will be marked interrupted");
    }
    tracing::info!("stopped");
    Ok(())
}

/// Remove job directories whose database row is gone.
fn sweep_orphan_jobs(state: &state::AppState) -> AppResult<usize> {
    let live: std::collections::HashSet<String> = state
        .db()
        .with(|c| {
            let mut stmt = c.prepare("SELECT id FROM transcode_jobs")?;
            let rows = stmt.query_map([], |r| r.get::<_, String>(0))?;
            Ok(rows.collect::<Result<Vec<String>, _>>()?)
        })
        .unwrap_or_default()
        .into_iter()
        .collect();
    state.cache().sweep_orphan_jobs(&live)
}

/// Look up every stored anime that never got a real title and write back what the
/// provider says now. Bounded so a large library cannot turn startup into a crawl.
async fn relabel_stale_titles(state: state::AppState) {
    const LIMIT: usize = 200;

    let ids = match otakuhub::db::unresolved(state.db()) {
        Ok(ids) => ids,
        Err(e) => {
            tracing::warn!(error = %e, "could not list unresolved titles");
            return;
        }
    };
    if ids.is_empty() {
        return;
    }
    tracing::info!(count = ids.len(), "resolving stored titles");

    let mut fixed = 0;
    for id in ids.into_iter().take(LIMIT) {
        match state.source().details(&id).await {
            Ok(anime) => match otakuhub::db::relabel(state.db(), &anime) {
                Ok(n) => fixed += n,
                Err(e) => tracing::warn!(error = %e, id = %id, "could not rewrite the title"),
            },
            Err(e) => tracing::warn!(error = %e, id = %id, "still unresolved"),
        }
    }
    if fixed > 0 {
        tracing::info!(rows = fixed, "stored titles resolved");
    }
}

fn log_banner(state: &state::AppState, addr: SocketAddr) {
    let banner = state.banner();
    tracing::info!("otakuhub {}", env!("CARGO_PKG_VERSION"));
    for (label, value) in state.config().explain() {
        tracing::info!("{label}: {value}");
    }
    tracing::info!(encoder = %banner.hardware, device = %banner.device, "encode path");
    for rejected in &banner.rejected {
        tracing::warn!("encoder unavailable, {rejected}");
    }
    tracing::info!("listening on http://{addr}");
}

fn init_tracing() {
    use tracing_subscriber::{fmt, prelude::*, EnvFilter};
    let filter = EnvFilter::try_from_env("OTAKUHUB_LOG")
        .or_else(|_| EnvFilter::try_from_default_env())
        .unwrap_or_else(|_| EnvFilter::new("otakuhub=info,warn"));
    let _ = tracing_subscriber::registry()
        .with(fmt::layer().with_target(false))
        .with(filter)
        .try_init();
}

async fn shutdown_signal() {
    let interrupt = async {
        tokio::signal::ctrl_c().await.ok();
    };
    #[cfg(unix)]
    let terminate = async {
        if let Ok(mut sig) =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        {
            sig.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = interrupt => tracing::info!("interrupted, shutting down"),
        _ = terminate => tracing::info!("terminated, shutting down"),
    }
}
