//! The router driven as a service, with no socket. Each defect guarded here was a missing
//! check between a handler and the browser, invisible by reading one handler.

use axum::body::Body;
use axum::http::{Request, StatusCode};
use tower::ServiceExt;

use otakuhub::config::Config;
use otakuhub::state::AppState;

/// A temp state with one account, so a session can be minted.
async fn state() -> (AppState, String) {
    let dir = std::env::temp_dir().join(format!(
        "otakuhub-routes-{}-{:?}",
        std::process::id(),
        std::thread::current().id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    let mut config = Config::default();
    config.cache_dir = dir.join("cache");
    config.transcode_dir = dir.join("cache/transcode");
    config.data_dir = dir.clone();
    let state = AppState::build(config).await.expect("a temp state must build");

    let (user, token) = state
        .db()
        .with(|c| {
            let id = otakuhub::db::users::create(c, "rei", "hunter2000")?;
            Ok((id, otakuhub::db::sessions::create(c, id)?))
        })
        .expect("an account must be creatable");
    assert!(user > 0);
    (state, token)
}

async fn get(state: &AppState, uri: &str, token: Option<&str>) -> (StatusCode, String) {
    let mut b = Request::builder().uri(uri);
    if let Some(t) = token {
        b = b.header(
            axum::http::header::COOKIE,
            format!("{}={t}", otakuhub::db::sessions::COOKIE),
        );
    }
    let req = b.body(Body::empty()).unwrap();
    let res = otakuhub::web::router(state.clone())
        .oneshot(req)
        .await
        .expect("the router must answer");
    let status = res.status();
    let body = axum::body::to_bytes(res.into_body(), 4 * 1024 * 1024)
        .await
        .unwrap_or_default();
    (status, String::from_utf8_lossy(&body).into_owned())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_job_path_cannot_walk_out_of_the_jobs_directory() {
    let (state, token) = state().await;
    // A file the process can definitely read, named so it passes a character filter.
    let victim = std::env::current_dir()
        .unwrap()
        .join("Cargo.toml")
        .to_string_lossy()
        .into_owned();

    for attempt in [
        // Percent encoded, so the router decodes it back into real separators.
        format!("/media/job/{victim}"),
        format!("/media/job/..%2F..%2F..%2F..%2F..%2F..%2Ftmp%2Fetc%2Fpasswd"),
        format!("/media/job/../../../../../../etc/passwd"),
        // A traversal hidden behind a legal looking id.
        format!("/media/job/0000000000000000000000aa/..%2F..%2FCargo.toml"),
    ] {
        let (status, body) = get(&state, &attempt, Some(&token)).await;
        assert!(
            !status.is_success(),
            "{attempt} answered {status} and leaked: {}",
            &body[..body.len().min(120)]
        );
        assert!(
            !body.contains("root:") && !body.contains("[package]"),
            "{attempt} returned the contents of a file outside the cache"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_job_routes_need_a_session() {
    let (state, _) = state().await;
    for uri in [
        "/media/job/0000000000000000000000aa/index.m3u8",
        "/media/job/0000000000000000000000aa/seg00000.ts",
    ] {
        let (status, _) = get(&state, uri, None).await;
        assert_eq!(
            status,
            StatusCode::UNAUTHORIZED,
            "{uri} is readable without signing in"
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_proxy_routes_need_a_session() {
    let (state, _) = state().await;
    // Without a session these would be an open request forwarder from any web page.
    for uri in [
        "/media/segment?u=https://example.test/a.ts&r=https://example.test/",
        "/media/playlist?u=https://example.test/a.m3u8&r=https://example.test/",
        "/media/subtitle?u=https://example.test/a.vtt&r=https://example.test/",
        "/api/poster?u=https://example.test/a.jpg",
    ] {
        let (status, _) = get(&state, uri, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri} was open");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn internal_addresses_are_refused_even_with_a_session() {
    let (state, token) = state().await;
    // A session is required, which is what stops a random web page. These are the cases a
    // signed in but hostile request could still ask for, so the URL check has to hold alone.
    for host in [
        "http://127.0.0.1:8099/api/metrics",
        "http://localhost:8099/",
        "http://169.254.169.254/latest/meta-data/",
        "http://10.0.0.1/",
        "http://192.168.1.1/",
        "http://[::1]:8099/",
        "http://[fd00::1]/",
        "http://0.0.0.0/",
        "http://100.64.0.1/",
        "http://metadata.google.internal/",
        "file:///etc/passwd",
    ] {
        let uri = format!(
            "/media/segment?u={}&r=https://example.test/",
            urlencode(host)
        );
        let (status, body) = get(&state, &uri, Some(&token)).await;
        assert!(
            !status.is_success(),
            "{host} was fetched: {status} {}",
            &body[..body.len().min(120)]
        );
    }
}

#[test]
fn the_url_check_covers_the_shapes_it_has_to() {
    use otakuhub::proxy::assert_public_url;
    for ok in [
        "https://hianime.at/storage/media/a.webp",
        "https://hls2.aniwatchtv.uk/v/a/1080/seg_1.ts",
        "http://example.test:8080/a.m3u8",
    ] {
        assert!(assert_public_url(ok).is_ok(), "{ok} should be allowed");
    }
    for bad in [
        "http://127.0.0.1/",
        "https://127.0.0.1:9999/",
        "http://169.254.169.254/",
        "http://[::1]/",
        "http://[::ffff:127.0.0.1]/",
        "http://localhost/",
        "http://LOCALHOST:80/",
        "http://foo.localhost/",
        "http://db.internal/",
        "http://0.0.0.0/",
        "file:///etc/passwd",
        "gopher://example.test/",
        "//example.test/a",
        "https://user:pass@127.0.0.1/a",
        "",
    ] {
        assert!(
            assert_public_url(bad).is_err(),
            "{bad} should have been refused"
        );
    }
}

/// Percent encode, so a `:` and `//` survive the query string.
fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_missing_playlist_is_a_404_not_a_different_playlist() {
    let (state, token) = state().await;
    let (status, body) = get(&state, "/playlist/DoesNotExist", Some(&token)).await;
    assert_eq!(status, StatusCode::NOT_FOUND, "body was: {}", &body[..body.len().min(200)]);
    assert!(
        !body.contains("Delete playlist"),
        "a 404 page offered to delete a playlist it does not have"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_signed_out_visitor_cannot_reach_the_user_page() {
    let (state, _) = state().await;
    for uri in ["/user", "/user/settings", "/playlist/Favourite"] {
        let (status, _) = get(&state, uri, None).await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{uri} was open");
    }
}
