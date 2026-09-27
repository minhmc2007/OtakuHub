//! Handler bodies. One function per route, each doing the smallest useful thing.

use askama::Template;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Redirect, Response};


use super::views::*;
use crate::auth::{CurrentUser, MaybeUser};
use crate::db::{history, jobs, playlists, progress, sessions, settings, users};
use crate::error::{AppError, AppResult};
use crate::media::{jobs as runner, transcode, Codec};
use crate::source::{Anime, Episode, Mode, Quality};
use crate::state::AppState;
use crate::util;

type Body = std::collections::HashMap<String, String>;


/// The series row remembers the last episode opened, but the position inside that episode is
/// only accurate in `episode_progress`.
fn history_rows(state: &AppState, user_id: i64, limit: i64) -> Vec<HistoryRow> {
    history::recent(state.db(), user_id, limit)
        .unwrap_or_default()
        .iter()
        .map(|p| {
            let mut row = HistoryRow::from(p);
            if let Ok(Some(entry)) = progress::get(state.db(), user_id, &p.episode_id) {
                row.set_resume(entry.position_secs);
            }
            row
        })
        .collect()
}

pub async fn home_page(
    State(state): State<AppState>,
    user: MaybeUser,
    Query(q): Query<Body>,
) -> AppResult<Html<String>> {
    let uid = user.0.as_ref().map(|u| u.id);
    let results = ResultsFragment::search(&state, q.get("q").map(String::as_str).unwrap_or("")).await;
    let recent: Vec<HistoryRow> = uid.map(|id| history_rows(&state, id, 12)).unwrap_or_default();
    let chips: Vec<PlaylistChip> = uid
        .and_then(|id| playlists::all(state.db(), id).ok())
        .unwrap_or_default()
        .into_iter()
        .map(|p| PlaylistChip {
            label: p.name.clone(),
            action: "add",
            in_playlist: false,
            is_selected: false,
            vals: crate::source::json_obj(&[("playlist", &p.name)]),
            name: p.name,
            count: p.count,
        })
        .collect();
    let has_accounts = state.db().with(users::count).unwrap_or(0) > 0;
    let chrome = Chrome::of(&state, user.0.as_ref());
    Ok(super::html(
        HomeTemplate {
            results,
            recent,
            playlists: chips,
            has_accounts,
            signed_in: chrome.signed_in,
            theme: chrome.theme,
            user: user.0.clone(),
        }
        .render(),
    ))
}

pub async fn login_page(State(state): State<AppState>, user: MaybeUser) -> AppResult<Html<String>> {
    let chrome = Chrome::of(&state, user.0.as_ref());
    Ok(super::html(
        LoginTemplate {
            signed_in: chrome.signed_in,
            theme: chrome.theme,
        }
        .render(),
    ))
}

pub async fn signup_page(State(state): State<AppState>, user: MaybeUser) -> AppResult<Html<String>> {
    let first = state.db().with(users::count).unwrap_or(0) == 0;
    let chrome = Chrome::of(&state, user.0.as_ref());
    Ok(super::html(
        SignupTemplate {
            first,
            signed_in: chrome.signed_in,
            theme: chrome.theme,
        }
        .render(),
    ))
}

/// A full results page, so a search can arrive as a normal navigation too.
pub async fn browse_page(State(state): State<AppState>, user: MaybeUser) -> AppResult<Html<String>> {
    let term = "one piece".to_string();
    let results = state.source().search(&term).await?;
    let chrome = Chrome::of(&state, user.0.as_ref());
    Ok(super::html(
        ResultsTemplate {
            cards: results.iter().take(12).map(CardView::from_anime).collect(),
            signed_in: chrome.signed_in,
            theme: chrome.theme,
            term,
        }
        .render(),
    ))
}

/// Search results as a fragment, swapped in by htmx.
pub async fn search(
    State(state): State<AppState>,
    _user: MaybeUser,
    Query(q): Query<Body>,
) -> AppResult<Html<String>> {
    let term = q.get("q").cloned().unwrap_or_default();
    let term = term.trim().to_string();
    if term.is_empty() {
        return Ok(Html(
            r#"<section class="oh-section"><h2 class="oh-h3">Search</h2><p class="oh-muted">Type a title to begin.</p></section>"#
                .into(),
        ));
    }
    let results = state.source().search(&term).await.map_err(|e| {
        tracing::warn!(error = %e, term = %term, "search failed");
        e
    })?;
    Ok(super::html(
        ResultsGrid {
            cards: results.iter().map(CardView::from_anime).collect(),
            term,
        }
        .render(),
    ))
}

pub async fn anime_page(
    State(state): State<AppState>,
    user: MaybeUser,
    Path(id): Path<String>,
) -> AppResult<Response> {
    let viewer = current_of(&user);
    // The provider's page is the authority on the title and poster; what the viewer has
    // stored is only a fallback, and only ever as good as the last time it was saved.
    let anime = match state.source().details(&id).await {
        Ok(found) => found,
        Err(e) => {
            tracing::warn!(error = %e, id, "could not read the title, using what is stored");
            super::find_anime(&state, &id, viewer.as_ref())
                .await
                .unwrap_or_else(|_| Anime::new(id.clone(), super::title_case(&id)))
        }
    };
    let episodes = state.source().episodes(&id).await?;
    // Which of these the viewer has watched, so the list can show it.
    let watched = match viewer {
        Some(v) => progress::for_anime(state.db(), v.id(), &id)?,
        None => Vec::new(),
    };
    let buttons = save_buttons(&state, user.0.as_ref(), &anime, "")?;
    let can_save = !buttons.is_empty();
    let chrome = Chrome::of(&state, user.0.as_ref());
    Ok(super::html(
        AnimeTemplate {
            poster: poster_url(&anime.poster),
            episodes: EpisodeView::with_progress(
                episodes.iter().map(EpisodeView::new).collect(),
                &watched,
            ),
            watched_count: watched.iter().filter(|w| w.finished).count(),
            save_buttons: buttons,
            can_save,
            signed_in: chrome.signed_in,
            theme: chrome.theme,
            anime,
        }
        .render(),
    )
    .into_response())
}

pub async fn watch_page(
    State(state): State<AppState>,
    user: MaybeUser,
    Path((anime_id, episode_id)): Path<(String, String)>,
    Query(q): Query<Body>,
) -> AppResult<Response> {
    let viewer = current_of(&user);
    // The title is stored into the watch history, so it has to be the real one and not
    // a slug reconstructed from the URL.
    let anime = super::find_anime(&state, &anime_id, viewer.as_ref()).await?;
    let anime = match state.source().details(&anime_id).await {
        Ok(found) => found,
        Err(e) => {
            tracing::debug!(error = %e, id = %anime_id, "no provider title, using the stored one");
            anime
        }
    };
    let mode = Mode::parse(q.get("mode").map(String::as_str).unwrap_or("sub"));
    let number = q.get("ep").cloned().unwrap_or_default();
    let want: Option<u32> = q.get("q").and_then(|v| v.parse().ok());
    let episode = Episode::new(&episode_id, number.clone());
    let stream = state.source().stream(&anime, &episode, mode).await?;

    // Read the saved position before the watch is recorded, so a fresh visit still offers
    // to resume. `?restart` in the URL is how the viewer answers "start over".
    let saved = match viewer.as_ref() {
        Some(v) => progress::get(state.db(), v.id(), &episode_id).ok().flatten(),
        None => None,
    };
    let restart = q.get("restart").is_some();
    let resume = saved
        .filter(|_| !restart)
        .filter(|p| p.position_secs > 30 && !p.finished)
        .map(|p| p.position_secs);

    if let Some(v) = viewer.as_ref() {
        super::record_watch(&state, v, &anime, &episode);
        if restart {
            let _ = progress::restart(state.db(), v.id(), &episode_id);
            let _ = history::set_position(state.db(), v.id(), &anime_id, 0);
        }
    }

    let play = PlayView::build(&stream, want, &number, &anime_id, &anime.title, &episode_id, mode);
    let buttons = save_buttons(&state, user.0.as_ref(), &anime, "")?;
    let chrome = Chrome::of(&state, user.0.as_ref());
    Ok(super::html(
        WatchTemplate {
            mode_toggle_label: if mode == Mode::Dub { "Sub" } else { "Dub" }.to_string(),
            mode_toggle: mode_toggle(&anime_id, &episode_id, &number, mode),
            mode_label: if mode == Mode::Dub { "dubbed" } else { "subtitled" }.to_string(),
            episode: episode.clone(),
            play: play.clone(),
            resume_secs: resume,
            resume_label: resume.map(crate::web::views::clock).unwrap_or_default(),
            has_save: !buttons.is_empty(),
            save_buttons: buttons,
            signed_in: chrome.signed_in,
            theme: chrome.theme,
            anime,
            mode,
        }
        .render(),
    )
    .into_response())
}

/// The library, with one playlist shown.
///
/// `/user` and `/playlist/{name}` are the same page, so they are one handler. The bare
/// route shows the first playlist; a named route shows that one and offers to delete it.
pub async fn user_page(State(state): State<AppState>, user: CurrentUser) -> AppResult<Response> {
    library_page(state, user, None).await
}

pub async fn playlist_page(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(name): Path<String>,
) -> AppResult<Response> {
    library_page(state, user, Some(name)).await
}

async fn library_page(
    state: AppState,
    user: CurrentUser,
    name: Option<String>,
) -> AppResult<Response> {
    let uid = user.id();
    // Only a playlist the viewer actually opened can be deleted, so the bare library
    // route does not offer it.
    let opened = name.clone();
    // A bare route shows the first playlist, and `render_playlist` falls back to the same
    // one, so an account with no playlist renders empty rather than 404.
    let chosen = match name {
        Some(n) => Some(n),
        None => playlists::all(state.db(), uid)?
            .first()
            .map(|p| p.name.clone()),
    };
    let entries = render_playlist(&state, uid, chosen.as_deref()).await?;
    let chrome = Chrome::of(&state, Some(&user.0));
    Ok(super::html(
        UserTemplate {
            jobs: job_rows(&state, &user),
            recent: history_rows(&state, uid, 8),
            playlists: library_chips(&state, &user, opened.as_deref()),
            show_delete: opened.is_some(),
            signed_in: chrome.signed_in,
            theme: chrome.theme,
            shown_playlist: chosen.unwrap_or_default(),
            entries,
            user: user.0.clone(),
        }
        .render(),
    )
    .into_response())
}

pub async fn settings_page(State(state): State<AppState>, user: CurrentUser) -> AppResult<Response> {
    let st = settings::get(state.db(), user.id())?;
    Ok(super::html(
        SettingsTemplate {
            banner: state.banner(),
            cache_human: crate::config::human_bytes(state.cache().actual_bytes()),
            served_human: crate::config::human_bytes(crate::cache::served_bytes()),
            upstream_fetches: state.upstream_fetches(),
            theme: st.theme.clone(),
            heights: [360, 480, 720, 1080, 1440, 2160],
            signed_in: true,
            settings: st,
            themes: ["system", "light", "dark"],
            user: user.0.clone(),
        }
        .render(),
    )
    .into_response())
}

/// A full page for a failed navigation, so an error is never a titleless fragment with no
/// way out. The fragment in `AppError` is still what an htmx swap wants.
pub fn error_page(message: &str, status: axum::http::StatusCode) -> String {
    let (heading, title) = match status {
        axum::http::StatusCode::NOT_FOUND => ("Nothing here", "Not found"),
        axum::http::StatusCode::UNAUTHORIZED => ("Sign in first", "Sign in"),
        axum::http::StatusCode::FORBIDDEN => ("Not allowed", "Not allowed"),
        s if s.is_server_error() => ("Something broke", "Error"),
        _ => ("That did not work", "Error"),
    };
    super::html(
        ErrorTemplate {
            theme: "system".to_string(),
            signed_in: false,
            heading: heading.to_string(),
            message: message.to_string(),
            title: title.to_string(),
        }
        .render(),
    )
    .0
}

pub async fn not_found(State(state): State<AppState>, user: MaybeUser) -> Response {
    let chrome = Chrome::of(&state, user.0.as_ref());
    let body = NotFoundTemplate {
        signed_in: chrome.signed_in,
        theme: chrome.theme,
    }
    .render();
    let html = match body {
        Ok(s) => s,
        Err(_) => "<h1>not found</h1>".to_string(),
    };
    (axum::http::StatusCode::NOT_FOUND, Html(html)).into_response()
}


pub async fn signout(State(state): State<AppState>, headers: axum::http::HeaderMap) -> Response {
    if let Some(token) = crate::db::sessions::token_from_headers(&headers) {
        let _ = state.db().with(|c| crate::db::sessions::destroy(c, &token));
    }
    (
        [(axum::http::header::SET_COOKIE, crate::db::sessions::clear_header())],
        Redirect::to("/"),
    )
        .into_response()
}

pub async fn rename(
    State(state): State<AppState>,
    user: CurrentUser,
    axum::extract::Form(form): axum::extract::Form<Body>,
) -> AppResult<Html<String>> {
    let new_name = super::field(&form, "username");
    state.db().with(|c| users::rename(c, user.id(), &new_name))?;
    Ok(super::alert("ok", "username updated"))
}

pub async fn change_password(
    State(state): State<AppState>,
    user: CurrentUser,
    headers: axum::http::HeaderMap,
    axum::extract::Form(form): axum::extract::Form<Body>,
) -> AppResult<Html<String>> {
    let current = super::field(&form, "current");
    let next = super::field(&form, "password");
    // The current password is checked even though the session is already valid: a
    // hijacked cookie alone should not be enough to lock the owner out.
    users::login(state.db(), user.name(), &current)?;
    let keep = sessions::token_from_headers(&headers);
    state.db().with(|c| users::set_password(c, user.id(), &next))?;
    // Changing a password is how a stolen cookie gets revoked, so the old ones go. The
    // caller's own session stays or they would be signed out by their own fix.
    state.db().with(|c| sessions::destroy_all_except(c, user.id(), keep.as_deref()))?;
    Ok(super::alert("ok", "password changed, other sessions signed out"))
}


pub async fn new_playlist(
    State(state): State<AppState>,
    user: CurrentUser,
    axum::extract::Form(form): axum::extract::Form<Body>,
) -> AppResult<Response> {
    let name = super::field(&form, "name");
    match playlists::new(state.db(), user.id(), &name) {
        Ok(_) => Ok(Redirect::to("/user").into_response()),
        Err(e) => Ok(super::alert("warn", &e.public()).into_response()),
    }
}

pub async fn delete_playlist(
    State(state): State<AppState>,
    user: CurrentUser,
    axum::extract::Form(form): axum::extract::Form<Body>,
) -> AppResult<Response> {
    let name = super::field(&form, "name");
    match playlists::drop_it(state.db(), user.id(), &name) {
        Ok(()) => Ok(Redirect::to("/user").into_response()),
        Err(e) => Ok(super::alert("warn", &e.public()).into_response()),
    }
}

pub async fn add_to_playlist(
    State(state): State<AppState>,
    user: CurrentUser,
    axum::extract::Form(form): axum::extract::Form<Body>,
) -> AppResult<Html<String>> {
    let (playlist, mut anime) = anime_from_form(&form);
    // The title and poster belong to the provider. The lookup can fail, and whatever the form
    // carried would then be stored for good, so a playlist never keeps a slug as its title.
    match state.source().details(&anime.id).await {
        Ok(found) => anime = found,
        Err(e) => tracing::warn!(
            error = %e,
            id = %anime.id,
            "saving with the title the page sent"
        ),
    }
    let added = playlists::add(state.db(), user.id(), &playlist, &anime)?;
    let message = if added {
        format!("saved to {playlist}")
    } else {
        "already in that playlist".to_string()
    };
    Ok(super::alert("ok", &message))
}

pub async fn remove_from_playlist(
    State(state): State<AppState>,
    user: CurrentUser,
    axum::extract::Form(form): axum::extract::Form<Body>,
) -> AppResult<Html<String>> {
    let playlist = super::field(&form, "playlist");
    let id = super::field(&form, "anime");
    let removed = playlists::remove(state.db(), user.id(), &playlist, &id)?;
    Ok(super::alert(
        "ok",
        if removed { "removed" } else { "it was not in that playlist" },
    ))
}

fn anime_from_form(form: &Body) -> (String, Anime) {
    let playlist = super::field(form, "playlist");
    let poster = super::field(form, "poster");
    let anime = Anime::new(super::field(form, "anime"), super::field(form, "title"))
        .with_poster((!poster.is_empty()).then_some(poster));
    (playlist, anime)
}


pub async fn clear_history(
    State(state): State<AppState>,
    user: CurrentUser,
    headers: axum::http::HeaderMap,
) -> Response {
    let n = history::wipe(state.db(), user.id()).unwrap_or(0);
    // The per episode rows go too, or the library would keep showing watched episodes
    // after their history was cleared.
    let _ = progress::wipe(state.db(), user.id());
    if super::is_htmx(&headers) {
        super::alert("ok", &format!("cleared {n} entries")).into_response()
    } else {
        Redirect::to("/user").into_response()
    }
}

pub async fn forget_history(
    State(state): State<AppState>,
    user: CurrentUser,
    headers: axum::http::HeaderMap,
    axum::extract::Form(form): axum::extract::Form<Body>,
) -> Response {
    let id = super::field(&form, "anime");
    let _ = history::forget(state.db(), user.id(), &id);
    // Forget the series, so its episodes go back to unwatched too.
    let _ = progress::wipe_anime(state.db(), user.id(), &id);
    if super::is_htmx(&headers) {
        super::alert("ok", "removed from continue watching").into_response()
    } else {
        Redirect::to("/user").into_response()
    }
}

/// The player reports where it is, per episode. This is what marks episodes watched and
/// what a return visit offers to resume from.
pub async fn save_progress(
    State(state): State<AppState>,
    user: CurrentUser,
    axum::extract::Form(form): axum::extract::Form<Body>,
) -> AppResult<Response> {
    let anime = super::field(&form, "anime");
    let episode = super::field(&form, "ep_id");
    let number = super::field(&form, "ep");
    if anime.is_empty() || episode.is_empty() {
        return Err(AppError::bad("the progress report is incomplete"));
    }
    let position: i64 = super::field(&form, "t").parse().unwrap_or(0);
    let duration: i64 = super::field(&form, "d").parse().unwrap_or(0);
    progress::save(
        state.db(),
        user.id(),
        &anime,
        &episode,
        &number,
        position,
        duration,
    )?;
    // The per series row still moves, so "continue watching" keeps working.
    let _ = history::set_position(state.db(), user.id(), &anime, position);
    Ok(StatusCode::NO_CONTENT.into_response())
}

/// "Start over" on a return visit: drop the resume point, keep the episode marked watched.
pub async fn restart_episode(
    State(state): State<AppState>,
    user: CurrentUser,
    axum::extract::Form(form): axum::extract::Form<Body>,
) -> AppResult<Html<String>> {
    let episode = super::field(&form, "ep_id");
    if episode.is_empty() {
        return Err(AppError::bad("no episode"));
    }
    progress::restart(state.db(), user.id(), &episode)?;
    let _ = history::set_position(state.db(), user.id(), &super::field(&form, "anime"), 0);
    Ok(super::alert("ok", "starting from the beginning"))
}


pub async fn episodes(
    State(state): State<AppState>,
    _user: MaybeUser,
    Query(q): Query<Body>,
) -> AppResult<Html<String>> {
    let id = q.get("id").cloned().unwrap_or_default();
    if id.is_empty() {
        return Err(AppError::bad("no anime given"));
    }
    let list = state.source().episodes(&id).await?;
    Ok(super::html(
        EpisodeListTemplate {
            episodes: list.iter().map(EpisodeView::new).collect(),
            anime_id: id,
        }
        .render(),
    ))
}

/// The player fragment, used when the viewer changes quality without a page load.
pub async fn play_info(
    State(state): State<AppState>,
    _user: MaybeUser,
    Query(q): Query<Body>,
) -> AppResult<Html<String>> {
    let anime_id = q.get("anime").cloned().unwrap_or_default();
    let episode_id = q.get("ep_id").cloned().unwrap_or_default();
    if anime_id.is_empty() || episode_id.is_empty() {
        return Err(AppError::bad("no episode given"));
    }
    let mode = Mode::parse(q.get("mode").map(String::as_str).unwrap_or("sub"));
    let number = q.get("ep").cloned().unwrap_or_default();
    let want: Option<u32> = q.get("q").and_then(|v| v.parse().ok());
    let anime = Anime::new(anime_id.clone(), q.get("title").cloned().unwrap_or_default());
    let episode = Episode::new(episode_id.clone(), number.clone());
    let stream = state.source().stream(&anime, &episode, mode).await?;
    let play = PlayView::build(&stream, want, &number, &anime_id, &anime.title, &episode_id, mode);
    Ok(super::html(
        PlayerTemplate {
            mode_toggle_label: if mode == Mode::Dub { "Sub" } else { "Dub" }.to_string(),
            mode_toggle: mode_toggle(&anime_id, &episode_id, &number, mode),
            mode_label: if mode == Mode::Dub { "dubbed" } else { "subtitled" }.to_string(),
            episode,
            has_save: false,
            save_buttons: Vec::new(),
            play,
            resume_secs: None,
            resume_label: String::new(),
            anime,
            mode,
        }
        .render(),
    ))
}

fn mode_toggle(anime: &str, episode: &str, number: &str, mode: Mode) -> String {
    format!(
        "/watch/{}/{}?ep={}&mode={}",
        util::url_encode(anime),
        util::url_encode(episode),
        util::url_encode(number),
        mode.toggled().as_str()
    )
}


pub async fn poster(
    State(state): State<AppState>,
    _user: CurrentUser,
    Query(q): Query<Body>,
) -> AppResult<Response> {
    let Some(url) = q.get("u").filter(|u| u.starts_with("http")) else {
        return Err(AppError::bad("no image url"));
    };
    let body = crate::proxy::fetch_image(&state, url).await?;
    // The type comes from the bytes, since the same host serves jpeg, webp and avif and the
    // response is sent with `nosniff`.
    let kind = crate::proxy::image_type(&body).unwrap_or("application/octet-stream");
    // Long lived, because a poster for a given title never changes.
    Ok(with_headers(body, kind, "public, max-age=604800, immutable"))
}

fn with_headers(body: Vec<u8>, kind: &str, cache: &'static str) -> Response {
    let mut response = super::content_type(kind).into_response();
    response.headers_mut().insert(
        axum::http::header::CACHE_CONTROL,
        axum::http::HeaderValue::from_static(cache),
    );
    super::with_body(response, body)
}


pub async fn segment(
    State(state): State<AppState>,
    _user: CurrentUser,
    Query(q): Query<Body>,
) -> AppResult<Response> {
    let (url, referer) = media_target(&q)?;
    let body = crate::proxy::segment(&state, &url, &referer).await?;
    Ok(super::with_body(
        super::content_type(crate::cache::content_type_for(crate::proxy::classify(&url), &url)),
        body,
    ))
}

pub async fn playlist(
    State(state): State<AppState>,
    _user: CurrentUser,
    Query(q): Query<Body>,
) -> AppResult<Response> {
    let (url, referer) = media_target(&q)?;
    let body = crate::proxy::rendition_playlist(&state, &url, &referer).await?;
    let rewritten = crate::proxy::rewrite_for_proxy(&body, &url, &referer);
    Ok(super::with_body(
        super::content_type("application/vnd.apple.mpegurl"),
        rewritten.into_bytes(),
    ))
}

pub async fn subtitle(
    State(state): State<AppState>,
    _user: CurrentUser,
    Query(q): Query<Body>,
) -> AppResult<Response> {
    let (url, referer) = media_target(&q)?;
    let body =
        crate::proxy::fetch_cached(&state, crate::cache::Kind::Subtitle, &url, Some(&referer)).await?;
    Ok(super::with_body(
        super::content_type("text/vtt; charset=utf-8"),
        body,
    ))
}

/// Both are required: a segment fetched without the referer is refused by the CDN, and
/// accepting a missing one would mean fetching arbitrary URLs from the server.
fn media_target(q: &Body) -> AppResult<(String, String)> {
    let url = q
        .get("u")
        .filter(|u| u.starts_with("http"))
        .cloned()
        .ok_or_else(|| AppError::bad("no media url"))?;
    let referer = q
        .get("r")
        .filter(|r| r.starts_with("http"))
        .cloned()
        .ok_or_else(|| AppError::bad("the media request is missing its referer"))?;
    Ok((url, referer))
}

pub async fn job_playlist(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(id): Path<String>,
) -> AppResult<Response> {
    let job = owned_job(&state, &id, &user)?;
    if job.state == jobs::State::Failed {
        return Err(AppError::internal(
            job.error.unwrap_or_else(|| "the conversion failed".into()),
        ));
    }
    let body = runner::job_playlist(&state, &job.id)?;
    Ok(super::with_body(
        super::content_type("application/vnd.apple.mpegurl"),
        body.into_bytes(),
    ))
}

/// A job id is a hex digest, so requiring one plus an owned row is what keeps `../..`
/// out of the path: only the database can name a directory.
fn owned_job(state: &AppState, id: &str, user: &CurrentUser) -> AppResult<jobs::Job> {
    if id.len() != 24 || !id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)) {
        return Err(AppError::not_found("job"));
    }
    jobs::get_owned(state.db(), id, user.id())?.ok_or_else(|| AppError::not_found("job"))
}

/// A segment from a local job directory. The name is sanitised, so the path cannot
/// escape the job directory.
pub async fn job_file(
    State(state): State<AppState>,
    user: CurrentUser,
    Path((id, file)): Path<(String, String)>,
) -> AppResult<Response> {
    let safe: String = file
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '_' || *c == '-')
        .collect();
    if safe.is_empty() || safe.contains("..") {
        return Err(AppError::bad("bad file name"));
    }
    let job = owned_job(&state, &id, &user)?;
    let path = state.cache().job_dir(&job.id).join(&safe);
    let body = tokio::fs::read(&path).await.map_err(|_| AppError::not_found("segment"))?;
    let kind = if safe.ends_with(".vtt") { "text/vtt" } else { "video/mp2t" };
    Ok(super::with_body(super::content_type(kind), body))
}


pub async fn start_transcode(
    State(state): State<AppState>,
    user: CurrentUser,
    axum::extract::Form(form): axum::extract::Form<Body>,
) -> AppResult<Html<String>> {
    let anime_id = super::field(&form, "anime");
    let title = super::field(&form, "title");
    let poster = super::field(&form, "poster");
    let episode_id = super::field(&form, "ep_id");
    let number = super::field(&form, "ep");
    let mode = Mode::parse(&super::field(&form, "mode"));
    if anime_id.is_empty() || episode_id.is_empty() {
        return Err(AppError::bad("the conversion request is incomplete"));
    }

    let st = settings::get(state.db(), user.id())?;
    let want: u32 = super::field(&form, "q").parse().unwrap_or(st.max_height);
    let max_height = want.max(2);
    let codec = st.codec;
    let encoder = runner::choose_encoder(&state, st.hardware);

    // The source is resolved here, not taken from the form, so a crafted request cannot
    // point the transcoder at some other host.
    let anime = Anime::new(anime_id, title).with_poster((!poster.is_empty()).then_some(poster));
    let episode = Episode::new(&episode_id, number);
    let stream = state.source().stream(&anime, &episode, mode).await?;

    if stream.has_height(max_height) {
        return Ok(super::alert(
            "ok",
            &format!("the source already has {max_height}p, so it streams directly"),
        ));
    }
    // Convert from the smallest source rendition that is still at or above the target,
    // which keeps the encode cheap and the result sharp.
    let source: Quality = stream
        .qualities
        .iter()
        .filter(|q| q.height >= max_height)
        .min_by_key(|q| q.height)
        .or_else(|| stream.best())
        .cloned()
        .ok_or_else(|| AppError::upstream("stream", "this episode has no playable source"))?;

    // Verbatim, as the probe stored it. Stripping /dev/ made it relative to the working
    // directory and every hardware encode failed.
    let device = match state.hardware().device.trim() {
        "" => None,
        dev => Some(std::path::PathBuf::from(dev)),
    };
    let spec = jobs::new_job(
        user.id(),
        &anime,
        &episode,
        &source.url,
        Some(&stream.referer),
        codec,
        max_height,
        encoder.encoder_name(codec),
    );
    let request = transcode::Request {
        out_dir: state.cache().job_dir(&spec.id),
        referer: Some(stream.referer.clone()),
        input_url: source.url.clone(),
        backend: encoder,
        device,
        max_height,
        limit_seconds: state.config().transcode_max_seconds,
        codec,
    };
    let job = runner::enqueue(&state, request, spec)?;
    Ok(super::html(
        JobStatusTemplate {
            job: JobRow::new(&job, &state),
        }
        .render(),
    ))
}

pub async fn job_status(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(id): Path<String>,
) -> AppResult<Html<String>> {
    let job = jobs::get_owned(state.db(), &id, user.id())?
        .ok_or_else(|| AppError::not_found("job"))?;
    Ok(super::html(
        JobStatusTemplate {
            job: JobRow::new(&job, &state),
        }
        .render(),
    ))
}

pub async fn cancel_job(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(id): Path<String>,
) -> AppResult<Html<String>> {
    jobs::get_owned(state.db(), &id, user.id())?
        .ok_or_else(|| AppError::not_found("job"))?;
    runner::drop_job(&state, &id, user.id())?;
    Ok(super::alert("ok", "stopped"))
}

pub async fn delete_job(
    State(state): State<AppState>,
    user: CurrentUser,
    Path(id): Path<String>,
) -> AppResult<Html<String>> {
    runner::drop_job(&state, &id, user.id())?;
    Ok(super::alert("ok", "deleted"))
}


pub async fn save_settings(
    State(state): State<AppState>,
    user: CurrentUser,
    axum::extract::Form(form): axum::extract::Form<Body>,
) -> AppResult<Response> {
    let theme = super::field(&form, "theme");
    let height: u32 = super::field(&form, "max_height").parse().unwrap_or(720);
    let st = settings::Settings {
        theme: if settings::THEME_CHOICES.contains(&theme.as_str()) {
            theme
        } else {
            "system".into()
        },
        max_height: height,
        hardware: super::flag(&form, "hardware"),
        autoplay: super::flag(&form, "autoplay"),
        dub: super::flag(&form, "dub"),
        codec: Codec::parse(&super::field(&form, "codec")).unwrap_or(Codec::H264),
    };
    settings::put(state.db(), user.id(), &st)?;
    Ok(Redirect::to("/user/settings").into_response())
}

pub async fn metrics(
    State(state): State<AppState>,
    _user: CurrentUser,
) -> AppResult<Response> {
    let usage = state.sample_metrics();
    let banner = state.banner();
    let payload = serde_json::json!({
        "cpu_percent": usage.cpu_percent,
        "cpu_cores": usage.cpu_cores,
        "mem_used": usage.mem_used,
        "mem_total": usage.mem_total,
        "disks": usage.disks,
        "gpu": usage.gpu,
        "uptime_secs": usage.uptime_secs,
        "cache_dir": state.config().cache_dir.display().to_string(),
        "cache_bytes": state.cache().actual_bytes(),
        "cache_tracked": state.cache().total_bytes(state.db()).unwrap_or(0),
        "served_bytes": crate::cache::served_bytes(),
        "upstream_fetches": state.upstream_fetches(),
        "running_jobs": state.running().active_count(),
        "hardware": banner.hardware,
        "hardware_is_gpu": banner.hardware_is_gpu,
    });
    Ok(super::with_body(
        super::content_type("application/json; charset=utf-8"),
        serde_json::to_vec(&payload).unwrap_or_default(),
    ))
}

pub async fn purge_cache(State(state): State<AppState>, _user: CurrentUser) -> AppResult<Html<String>> {
    let removed = state.cache().purge(state.db())?;
    Ok(super::alert("ok", &format!("removed {removed} cached files")))
}


/// Turn an optional user into the extractor type the shared helpers expect.
fn current_of(user: &MaybeUser) -> Option<CurrentUser> {
    user.0.clone().map(CurrentUser)
}

fn library_chips(
    state: &AppState,
    user: &CurrentUser,
    selected: Option<&str>,
) -> Vec<PlaylistChip> {
    playlists::all(state.db(), user.id())
        .unwrap_or_default()
        .into_iter()
        .map(|p| PlaylistChip {
            is_selected: Some(p.name.as_str()) == selected,
            in_playlist: false,
            label: p.name.clone(),
            action: "add",
            count: p.count,
            vals: crate::source::json_obj(&[("playlist", &p.name)]),
            name: p.name,
        })
        .collect()
}

fn job_rows(state: &AppState, user: &CurrentUser) -> Vec<JobRow> {
    jobs::mine(state.db(), user.id(), 10)
        .unwrap_or_default()
        .iter()
        .map(|j| JobRow::new(j, state))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_media_request_needs_both_a_url_and_a_referer() {
        let good: Body = [
            ("u".to_string(), "https://h.test/s.ts".to_string()),
            ("r".to_string(), "https://h.test/".to_string()),
        ]
        .into_iter()
        .collect();
        let (url, referer) = media_target(&good).unwrap();
        assert_eq!(url, "https://h.test/s.ts");
        assert_eq!(referer, "https://h.test/");

        let no_ref: Body = [("u".to_string(), "https://h.test/s.ts".to_string())]
            .into_iter()
            .collect();
        assert!(media_target(&no_ref).is_err());

        let local: Body = [("u".to_string(), "file:///etc/passwd".to_string())]
            .into_iter()
            .collect();
        assert!(media_target(&local).is_err(), "only http urls are proxied");
    }

    #[test]
    fn job_file_names_cannot_escape_the_directory() {
        let sanitise = |file: &str| -> String {
            file.chars()
                .filter(|c| c.is_ascii_alphanumeric() || *c == '.' || *c == '_' || *c == '-')
                .collect()
        };
        assert_eq!(sanitise("seg_00001.ts"), "seg_00001.ts");
        assert_eq!(sanitise("../../etc/passwd"), "....etcpasswd");
        assert!(sanitise("").is_empty());
    }

    #[test]
    fn a_form_gives_a_title_and_optional_poster() {
        let form: Body = [
            ("playlist".to_string(), "Favourite".to_string()),
            ("anime".to_string(), "one-piece-1".to_string()),
            ("title".to_string(), "One Piece".to_string()),
            ("poster".to_string(), "  ".to_string()),
        ]
        .into_iter()
        .collect();
        let (playlist, anime) = anime_from_form(&form);
        assert_eq!(playlist, "Favourite");
        assert_eq!(anime.title, "One Piece");
        assert!(anime.poster.is_none(), "a blank poster is no poster");
    }

    #[test]
    fn the_mode_toggle_flips_and_keeps_the_episode() {
        let sub = mode_toggle("one-piece-1", "42", "5", Mode::Sub);
        assert!(sub.contains("mode=dub"));
        assert!(sub.contains("ep=5"));
        assert!(sub.contains("/one-piece-1/42"));
        let dub = mode_toggle("one-piece-1", "42", "5", Mode::Dub);
        assert!(dub.contains("mode=sub"));
    }
}
