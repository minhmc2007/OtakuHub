//! Web layer: routing and handlers.

pub mod pages;
pub mod views;

use axum::extract::State;
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{delete, get, post};
use axum::Router;
use tower_http::compression::CompressionLayer;
use tower_http::set_header::SetResponseHeaderLayer;
use tower_http::trace::TraceLayer;

use crate::auth::CurrentUser;
use crate::db::{history, playlists, sessions, settings, users};
use crate::error::{AppError, AppResult};
use crate::state::AppState;

pub fn router(state: AppState) -> Router {
    let api = Router::new()
        .route("/search", get(pages::search))
        .route("/browse", get(pages::browse_page))
        .route("/episodes", get(pages::episodes))
        .route("/play", get(pages::play_info))
        .route("/poster", get(pages::poster))
        .route("/settings", get(pages::settings_page))
        .route("/settings/save", post(pages::save_settings))
        .route("/user/rename", post(pages::rename))
        .route("/user/password", post(pages::change_password))
        .route("/user/signout", post(pages::signout))
        .route("/user/overview", get(pages::user_overview))
        .route("/playlist/new", post(pages::new_playlist))
        .route("/playlist/delete", post(pages::delete_playlist))
        .route("/playlist/add", post(pages::add_to_playlist))
        .route("/playlist/remove", post(pages::remove_from_playlist))
        .route("/history/clear", post(pages::clear_history))
        .route("/history/forget", post(pages::forget_history))
        .route("/history/position", post(pages::save_progress))
        .route("/progress/restart", post(pages::restart_episode))
        .route("/metrics", get(pages::metrics))
        .route("/cache/purge", post(pages::purge_cache))
        .route("/transcode", post(pages::start_transcode))
        .route("/job/{id}", get(pages::job_status))
        .route("/job/{id}/cancel", post(pages::cancel_job))
        .route("/job/{id}", delete(pages::delete_job));

    let media = Router::new()
        .route("/playlist", get(pages::playlist))
        .route("/segment", get(pages::segment))
        .route("/subtitle", get(pages::subtitle))
        .route("/job/{id}/index.m3u8", get(pages::job_playlist))
        .route("/job/{id}/{file}", get(pages::job_file));

    Router::new()
        .route("/", get(pages::home_page).post(login))
        .route("/login", get(pages::login_page).post(login))
        .route("/signup", get(pages::signup_page).post(signup))
        .route("/logout", post(logout))
        .route("/anime/{id}", get(pages::anime_page))
        .route("/watch/{anime}/{episode}", get(pages::watch_page))
        .route("/user", get(pages::user_page))
        .route("/user/settings", get(pages::settings_page))
        .route("/playlist/{name}", get(pages::playlist_page))
        .route("/static/{*path}", get(static_file))
        .nest("/api", api)
        .nest("/media", media)
        .fallback(pages::not_found)
        .layer(axum::middleware::from_fn(error_pages))
        .layer(CompressionLayer::new())
        .layer(SetResponseHeaderLayer::if_not_present(
            axum::http::header::X_CONTENT_TYPE_OPTIONS,
            axum::http::HeaderValue::from_static("nosniff"),
        ))
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}


/// `AppError` can only produce a fragment, because it never sees the request. This layer can,
/// so a navigation gets the styled error page and an htmx swap keeps the fragment.
async fn error_pages(
    request: axum::http::Request<axum::body::Body>,
    next: axum::middleware::Next,
) -> Response {
    use axum::response::IntoResponse;

    let wants_fragment = request
        .headers()
        .get("hx-request")
        .and_then(|v| v.to_str().ok())
        .map(|v| v == "true")
        .unwrap_or(false);
    // The player asks for a playlist and must get the m3u8 or a JSON error, never HTML.
    let wants_data = request.uri().path().starts_with("/media/")
        || request.uri().path() == "/api/metrics";

    let response = next.run(request).await;
    if wants_fragment || wants_data || !response.status().is_client_error() {
        return response;
    }

    let status = response.status();
    let message = match status {
        axum::http::StatusCode::NOT_FOUND => "that page does not exist".to_string(),
        axum::http::StatusCode::UNAUTHORIZED => "you need to sign in first".to_string(),
        axum::http::StatusCode::FORBIDDEN => "you cannot do that".to_string(),
        _ => "something went wrong on the server".to_string(),
    };
    let mut full = Html(pages::error_page(&message, status)).into_response();
    *full.status_mut() = status;
    full
}

/// A trimmed form field. A missing field reads as empty, which is what an unchecked
/// box sends.
pub fn field(form: &std::collections::HashMap<String, String>, key: &str) -> String {
    form.get(key).map(|v| v.trim().to_string()).unwrap_or_default()
}

pub fn flag(form: &std::collections::HashMap<String, String>, key: &str) -> bool {
    form.get(key).map(|v| v == "1" || v == "on" || v == "true").unwrap_or(false)
}

/// Does this request come from htmx? Some actions answer differently for a fragment.
pub fn is_htmx(headers: &axum::http::HeaderMap) -> bool {
    headers
        .get("hx-request")
        .and_then(|v| v.to_str().ok())
        .map(|v| v == "true")
        .unwrap_or(false)
}

/// The alert fragment every accepted or rejected action returns.
pub fn alert(kind: &str, message: &str) -> Html<String> {
    Html(format!(
        r#"<div class="oh-alert oh-alert-{kind}" role="alert">{}</div>"#,
        crate::error::escape_html(message)
    ))
}

/// Attach a body to a response built from headers alone.
pub fn with_body(mut response: Response, body: Vec<u8>) -> Response {
    *response.body_mut() = axum::body::Body::from(body);
    response
}

/// A response with only a content type, ready for `with_body`.
pub fn content_type(kind: &str) -> Response {
    let mut response = Response::new(axum::body::Body::empty());
    response.headers_mut().insert(
        axum::http::header::CONTENT_TYPE,
        axum::http::HeaderValue::from_str(kind).unwrap_or(axum::http::HeaderValue::from_static("application/octet-stream")),
    );
    response
}

/// A redirect that also sets one cookie. The value is a whole `name=value; attrs` string.
pub fn redirect_with_cookie(path: &str, value: &str) -> Response {
    let mut response = Redirect::to(path).into_response();
    if let Ok(v) = axum::http::HeaderValue::from_str(value) {
        response.headers_mut().append(axum::http::header::SET_COOKIE, v);
    }
    response
}

/// Render a template, turning a render failure into a visible message instead of a 500
/// with an empty body.
pub fn html(body: Result<String, askama::Error>) -> Html<String> {
    match body {
        Ok(s) => Html(s),
        Err(e) => {
            tracing::error!(error = %e, "template failed to render");
            Html(r#"<div class="oh-alert oh-alert-warn">this page could not be rendered</div>"#.into())
        }
    }
}

/// Record a watch, and return the history row so a caller can show it.
pub fn record_watch(
    state: &AppState,
    user: &CurrentUser,
    anime: &crate::source::Anime,
    episode: &crate::source::Episode,
) {
    if episode.number.is_empty() {
        return;
    }
    if let Err(e) = history::record(state.db(), user.id(), anime, episode) {
        tracing::warn!(error = %e, "could not record history");
    }
}

/// Resolve a slug into a title. The title is not in the URL, so it comes from the
/// viewer's own history or playlists, and otherwise falls back to a tidied slug.
pub async fn find_anime(
    state: &AppState,
    id: &str,
    user: Option<&CurrentUser>,
) -> AppResult<crate::source::Anime> {
    let mut anime = crate::source::Anime::new(id, title_case(id));
    let Some(user) = user else {
        return Ok(anime);
    };
    let from_db = state.db().with(|c| {
        let row = c
            .query_row(
                "SELECT title, poster FROM history WHERE user_id = ?1 AND anime_id = ?2",
                rusqlite::params![user.id(), id],
                |r| Ok((r.get::<_, String>(0)?, r.get::<_, Option<String>>(1)?)),
            )
            .ok();
        if row.is_some() {
            return Ok(row);
        }
        let mut out = None;
        for p in playlists::list(c, user.id())? {
            for e in playlists::items(c, p.id)? {
                if e.anime_id == id {
                    out = Some((e.title, e.poster));
                    break;
                }
            }
            if out.is_some() {
                break;
            }
        }
        Ok(out)
    })?;
    if let Some((title, poster)) = from_db {
        anime.title = title;
        anime.poster = poster;
    }
    Ok(anime)
}

/// `one-piece-1` reads as "One Piece 1". Only a fallback, never the real title.
fn title_case(slug: &str) -> String {
    let mut out = String::with_capacity(slug.len());
    let mut start_of_word = true;
    for c in slug.chars() {
        if c == '-' || c == '_' || c == ' ' {
            // A dash becomes a space and starts the next word, so the next letter is
            // capitalised and the dash itself is not kept.
            out.push(' ');
            start_of_word = true;
            continue;
        }
        if start_of_word {
            out.extend(c.to_uppercase());
            start_of_word = false;
        } else {
            out.push(c);
        }
    }
    let joined = out.split_whitespace().collect::<Vec<_>>().join(" ");
    if joined.is_empty() {
        // Nothing survived, so the raw slug is more useful than an empty heading.
        slug.to_string()
    } else {
        joined
    }
}

/// The user's theme, or `system`.
pub fn theme_of(state: &AppState, user: Option<&CurrentUser>) -> String {
    user.and_then(|u| settings::get(state.db(), u.id()).ok())
        .map(|s| s.theme)
        .unwrap_or_else(|| "system".to_string())
}

pub fn require_theme(state: &AppState, user: &CurrentUser) -> String {
    settings::get(state.db(), user.id())
        .map(|s| s.theme)
        .unwrap_or_else(|_| "system".to_string())
}


pub async fn login(
    State(state): State<AppState>,
    axum::extract::Form(form): axum::extract::Form<std::collections::HashMap<String, String>>,
) -> AppResult<Response> {
    let username = field(&form, "username");
    let password = field(&form, "password");
    let user = users::login(state.db(), &username, &password)?;
    let token = state.db().with(|c| sessions::create(c, user.id))?;
    Ok(redirect_with_cookie("/", &sessions::cookie_header(&token)))
}

pub async fn signup(
    State(state): State<AppState>,
    axum::extract::Form(form): axum::extract::Form<std::collections::HashMap<String, String>>,
) -> AppResult<Response> {
    let username = field(&form, "username");
    let password = field(&form, "password");
    let first = state.db().with(users::count)? == 0;
    let user_id = state.db().with(|c| users::create(c, &username, &password))?;
    let token = state.db().with(|c| sessions::create(c, user_id))?;
    tracing::info!(user = %username, first_account = first, "account created");
    Ok(redirect_with_cookie("/", &sessions::cookie_header(&token)))
}

pub async fn logout(
    State(state): State<AppState>,
    headers: axum::http::HeaderMap,
) -> Response {
    if let Some(token) = sessions::token_from_headers(&headers) {
        let _ = state.db().with(|c| sessions::destroy(c, &token));
    }
    (
        [(axum::http::header::SET_COOKIE, sessions::clear_header())],
        Redirect::to("/"),
    )
        .into_response()
}


pub fn now() -> i64 {
    crate::time::now_secs()
}

pub fn missing(what: &str) -> AppError {
    AppError::not_found(what)
}

pub async fn static_file(
    State(state): State<AppState>,
    axum::extract::Path(path): axum::extract::Path<String>,
) -> AppResult<Response> {
    let name = path.rsplit('/').next().unwrap_or("");
    let file = static_dir(&state).join(name);
    if !file.is_file() {
        return Err(missing(&path));
    }
    let kind = match name.rsplit('.').next() {
        Some("css") => "text/css; charset=utf-8",
        Some("js") => "text/javascript; charset=utf-8",
        _ => "application/octet-stream",
    };
    let body = tokio::fs::read(&file).await?;
    Ok(with_body(content_type(kind).into_response(), body))
}

/// Where the bundled assets live: next to the binary in a packaged install, otherwise
/// the project directory during development.
pub fn static_dir(state: &AppState) -> std::path::PathBuf {
    if let Some(dir) = std::env::var_os("OTAKUHUB_STATIC") {
        return std::path::PathBuf::from(dir);
    }
    let local = state.config().data_dir.join("static");
    if local.is_dir() {
        return local;
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let beside = dir.join("static");
            if beside.is_dir() {
                return beside;
            }
        }
    }
    std::path::PathBuf::from("static")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slugs_read_as_titles_only_as_a_fallback() {
        // Dashes become spaces and start a new capitalised word, which is how a slug
        // reads as a title.
        assert_eq!(title_case("one-piece-1"), "One Piece 1");
        assert_eq!(title_case("naruto"), "Naruto");
        assert_eq!(title_case("spy_x_family-9876"), "Spy X Family 9876");
    }

    #[test]
    fn a_punctuation_only_slug_still_produces_something() {
        for slug in ["", "-", "---", "  "] {
            let out = title_case(slug);
            assert!(!out.is_empty() || slug.trim().is_empty(), "{slug:?} gave {out:?}");
        }
    }

    #[test]
    fn field_reads_a_trimmed_value_and_defaults_to_empty() {
        let form = [("a".to_string(), "  x  ".to_string())].into_iter().collect();
        assert_eq!(field(&form, "a"), "x");
        assert_eq!(field(&form, "b"), "");
    }

    #[test]
    fn flag_accepts_the_shapes_a_browser_sends() {
        let form = [
            ("on".to_string(), "on".to_string()),
            ("one".to_string(), "1".to_string()),
            ("yes".to_string(), "true".to_string()),
            ("off".to_string(), "0".to_string()),
        ]
        .into_iter()
        .collect();
        assert!(flag(&form, "on"));
        assert!(flag(&form, "one"));
        assert!(flag(&form, "yes"));
        assert!(!flag(&form, "off"));
        assert!(!flag(&form, "absent"));
    }
}
