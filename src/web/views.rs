//! View models. Anything a template would have to compute is precomputed here, which keeps
//! the templates free of logic and makes each rule testable on its own.

use askama::Template;

use crate::db::jobs::{Job, State as JobState};
use crate::db::settings;
use crate::db::users::User;
use crate::error::AppResult;
use crate::source::{Anime, Episode, Mode, Stream, Subtitle};
use crate::state::{AppState, Banner};

/// `time::ago` already subtracts from now, so this used to subtract a second time and
/// rendered the raw epoch seconds as "56 years ago".
fn ago(updated_at_millis: i64) -> String {
    crate::time::ago(updated_at_millis.div_euclid(1000))
}

fn bytes(n: u64) -> String {
    crate::config::human_bytes(n)
}

/// Poster URL, always through our own proxy so no CDN host sees a referrer from a page.
pub fn poster_url(poster: &Option<String>) -> Option<String> {
    poster
        .as_ref()
        .map(|u| format!("/api/poster?u={}", crate::util::url_encode(u)))
}

/// A poster tile. `html` is the rendered card, so every page can drop tiles in a grid
/// without repeating the markup or needing a template macro.
#[derive(Debug, Clone)]
pub struct CardView {
    pub id: String,
    pub title: String,
    pub poster: Option<String>,
    pub html: String,
}

#[derive(Template)]
#[template(path = "card.html")]
struct CardTemplate {
    id: String,
    title: String,
    poster: Option<String>,
    initials: String,
}

impl CardView {
    pub fn new(id: impl Into<String>, title: impl Into<String>, poster: Option<String>) -> Self {
        let id = id.into();
        let title = title.into();
        let proxied = poster_url(&poster);
        let initials: String = title.chars().take(2).collect();
        let html = CardTemplate {
            id: id.clone(),
            title: title.clone(),
            poster: proxied.clone(),
            initials,
        }
        .render()
        .unwrap_or_default();
        Self {
            id,
            title,
            poster: proxied,
            html,
        }
    }

    pub fn from_anime(a: &Anime) -> Self {
        Self::new(a.id.clone(), a.title.clone(), a.poster.clone())
    }
}

#[derive(Debug, Clone)]
pub struct HistoryRow {
    pub id: String,
    pub title: String,
    pub episode: String,
    pub when: String,
    /// Straight to the episode that was open, at the point it was left.
    pub resume_href: String,
    /// "12:34 in", or empty at the start.
    pub position: String,
    /// The tile, rendered as a card so the home page grid needs no markup of its own.
    pub html: String,
}

impl From<&crate::db::history::Progress> for HistoryRow {
    fn from(p: &crate::db::history::Progress) -> Self {
        let card = CardView::new(p.anime_id.clone(), p.title.clone(), p.poster.clone());
        let mut row = Self {
            when: ago(p.updated_at),
            resume_href: format!("/watch/{}/{}?ep={}", p.anime_id, p.episode_id, p.episode_number),
            position: String::new(),
            id: p.anime_id.clone(),
            title: p.title.clone(),
            episode: p.episode_number.clone(),
            html: card.html,
        };
        // The per episode row is the authority on where to resume, so the row and the
        // watch page can never disagree. The series row only knows the last episode.
        row.set_resume(p.position_secs);
        row
    }
}

impl HistoryRow {
    /// Point this row at a resume position. A finished episode stores zero, so it drops the
    /// offset. An existing `t=` is replaced, since `&t=1&t=2` is a duplicate parameter.
    pub fn set_resume(&mut self, secs: i64) {
        if let Some(at) = self.resume_href.find("&t=") {
            self.resume_href.truncate(at);
        }
        if secs > 30 {
            self.resume_href.push_str(&format!("&t={secs}"));
            self.position = format!("{} in", clock(secs));
        } else {
            self.position.clear();
        }
    }
}

/// Seconds as `m:ss`, or `h:mm:ss` past an hour.
pub fn clock(secs: i64) -> String {
    let s = secs.max(0);
    let (h, m, sec) = (s / 3600, (s % 3600) / 60, s % 60);
    if h > 0 {
        format!("{h}:{m:02}:{sec:02}")
    } else {
        format!("{m}:{sec:02}")
    }
}

#[derive(Debug, Clone)]
pub struct PlaylistChip {
    pub name: String,
    pub count: i64,
    /// True when the anime is already saved here. Drives the save buttons.
    pub in_playlist: bool,
    /// True when this row is the page being shown. Drives the library list.
    pub is_selected: bool,
    /// The action this button performs, so the template needs no conditionals.
    pub action: &'static str,
    pub label: String,
    /// A ready made `hx-vals` payload. Built as JSON in Rust because a title containing a
    /// quote breaks htmx's parse if it is interpolated into the attribute directly.
    pub vals: String,
}

/// One save button per playlist the user owns.
pub fn save_buttons(
    state: &AppState,
    user: Option<&User>,
    anime: &Anime,
    poster: &str,
) -> AppResult<Vec<PlaylistChip>> {
    let Some(u) = user else {
        return Ok(Vec::new());
    };
    let holding = crate::db::playlists::holders(state.db(), u.id, &anime.id)?;
    Ok(crate::db::playlists::all(state.db(), u.id)?
        .into_iter()
        .map(|p| {
            let in_playlist = holding.contains(&p.name);
            PlaylistChip {
                label: if in_playlist {
                    format!("{} saved", p.name)
                } else {
                    format!("+ {}", p.name)
                },
                action: if in_playlist { "remove" } else { "add" },
                is_selected: false,
                in_playlist,
                count: p.count,
                vals: crate::source::json_obj(&[
                    ("playlist", &p.name),
                    ("anime", &anime.id),
                    ("title", &anime.title),
                    ("poster", poster),
                ]),
                name: p.name,
            }
        })
        .collect())
}

#[derive(Debug, Clone)]
pub struct EpisodeView {
    pub id: String,
    pub number: String,
    pub title: String,
    /// Watched through.
    pub watched: bool,
    /// Partly watched, and the percentage, so a half finished episode is not
    /// unwatched.
    pub percent: u8,
}

impl EpisodeView {
    pub fn new(e: &Episode) -> Self {
        Self {
            id: e.id.clone(),
            number: e.number.clone(),
            title: e
                .title
                .clone()
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| format!("Episode {}", e.number)),
            watched: false,
            percent: 0,
        }
    }

    /// Mark each episode from what the viewer has actually watched.
    pub fn with_progress(list: Vec<Self>, entries: &[crate::db::progress::Entry]) -> Vec<Self> {
        list.into_iter()
            .map(|mut e| {
                if let Some(p) = entries.iter().find(|p| p.episode_id == e.id) {
                    e.watched = p.finished;
                    if !p.finished && p.duration_secs > 0 {
                        e.percent =
                            ((p.position_secs * 100) / p.duration_secs).clamp(1, 99) as u8;
                    }
                }
                e
            })
            .collect()
    }
}

#[derive(Debug, Clone)]
pub struct QualityButton {
    pub slug: String,
    pub label: String,
    pub height: u32,
    /// True when the source already has this rendition.
    pub direct: bool,
    pub selected: bool,
    /// Where a direct rendition goes.
    pub href: String,
    /// A ready made `hx-vals` payload for the convert action, JSON escaped in Rust.
    pub vals: String,
}

#[derive(Debug, Clone)]
pub struct SubtitleTrack {
    pub label: String,
    pub url: String,
}

/// How the player should load this episode.
#[derive(Debug, Clone)]
pub struct PlayView {
    /// A proxied source playlist, or empty while a conversion is being set up.
    pub playlist_url: String,
    /// True when there is nothing to stream yet and ffmpeg has to work.
    pub live: bool,
    pub qualities: Vec<QualityButton>,
    pub subs: Vec<SubtitleTrack>,
    pub notice: Option<String>,
    /// The series this episode belongs to, so the player can report a resume point.
    pub anime_id: String,
}

impl PlayView {
    /// A source rendition is used when one exists, and ffmpeg is asked for a conversion
    /// only when one does not.
    pub fn build(
        stream: &Stream,
        want: Option<u32>,
        episode: &str,
        anime_id: &str,
        anime_title: &str,
        episode_id: &str,
        mode: Mode,
    ) -> Self {
        let want = want.unwrap_or(0);
        // The highest rendition is picked here, not from the caller's order, so
        // an unsorted list still gives the best picture.
        let best = stream.qualities.iter().max_by_key(|q| q.height);
        let direct = if want > 0 {
            stream.qualities.iter().find(|q| q.height == want)
        } else {
            best
        };

        let selected_slug = direct
            .map(|q| q.slug())
            .or_else(|| (want > 0).then(|| format!("{want}p")));

        let mut qualities: Vec<QualityButton> = stream
            .qualities
            .iter()
            .map(|q| QualityButton {
                slug: q.slug(),
                label: q.label.clone(),
                height: q.height,
                direct: true,
                selected: Some(q.slug()) == selected_slug,
                href: watch_href(anime_id, episode_id, episode, mode, q.height),
                vals: String::new(),
            })
            .collect();

        // Offer the viewer's target as a convert option when the source cannot serve it.
        let target_slug = format!("{want}p");
        if want > 0 && !stream.has_height(want) && !qualities.iter().any(|q| q.slug == target_slug) {
            qualities.push(QualityButton {
                slug: target_slug,
                label: format!("{want}p, convert"),
                height: want,
                direct: false,
                selected: true,
                href: String::new(),
                vals: crate::source::json_obj(&[
                    ("anime", anime_id),
                    ("title", anime_title),
                    ("poster", ""),
                    ("ep_id", episode_id),
                    ("ep", episode),
                    ("mode", mode.as_str()),
                    ("q", &want.to_string()),
                ]),
            });
        }

        let notice = if direct.is_none() {
            let base = "the source has no rendition at that height, so ffmpeg converts";
            if episode.is_empty() {
                base.to_string()
            } else {
                format!("{base} for episode {episode}")
            }
        } else {
            String::new()
        };

        Self {
            playlist_url: direct
                .map(|q| {
                    format!(
                        "/media/playlist?u={}&r={}",
                        crate::util::url_encode(&q.url),
                        crate::util::url_encode(&stream.referer)
                    )
                })
                .unwrap_or_default(),
            live: direct.is_none(),
            qualities,
            subs: stream.subtitles.iter().map(|s| track(s, &stream.referer)).collect(),
            notice: (!notice.is_empty()).then_some(notice),
            anime_id: anime_id.to_string(),
        }
    }
}

fn track(s: &Subtitle, referer: &str) -> SubtitleTrack {
    SubtitleTrack {
        label: if s.label.is_empty() { s.lang.clone() } else { s.label.clone() },
        url: format!(
            "/media/subtitle?u={}&r={}",
            crate::util::url_encode(&s.url),
            crate::util::url_encode(referer)
        ),
    }
}

fn watch_href(anime: &str, episode: &str, number: &str, mode: Mode, height: u32) -> String {
    format!(
        "/watch/{}/{}?ep={}&mode={}&q={}",
        crate::util::url_encode(anime),
        crate::util::url_encode(episode),
        crate::util::url_encode(number),
        mode.as_str(),
        height
    )
}

#[derive(Debug, Clone)]
pub struct JobRow {
    pub id: String,
    pub label: String,
    pub state: String,
    pub detail: String,
    pub error: Option<String>,
    pub running: bool,
    /// The stop endpoint, present only while a job runs.
    pub can_cancel: Option<String>,
    /// The local playlist, present only once a job has finished.
    pub play_href: Option<String>,
    pub html: String,
}

#[derive(Template)]
#[template(path = "frag/job_status.html")]
struct JobTemplate {
    job: JobRow,
}

impl JobRow {
    pub fn new(job: &Job, state: &AppState) -> Self {
        let running = !job.state.is_terminal();
        let hardware = job
            .encoder
            .contains("vaapi")
            || job.encoder.contains("nvenc")
            || job.encoder.contains("qsv")
            || job.encoder.contains("amf");
        let mut detail = format!(
            "{} at {}p via {} ({} encoding)",
            job.codec.to_uppercase(),
            job.max_height,
            if job.encoder.is_empty() { "?" } else { &job.encoder },
            if hardware { "hardware" } else { "software" }
        );
        let size = crate::media::jobs::job_size(state, &job.id);
        if size > 0 {
            detail = format!("{detail}, {}", bytes(size));
        }

        let row = JobRow {
            id: job.id.clone(),
            label: job.label(),
            state: job.state.as_str().to_string(),
            detail,
            error: job.error.clone(),
            running,
            can_cancel: running
                .then(|| format!("/api/job/{}/cancel", job.id)),
            play_href: (job.state == JobState::Done)
                .then(|| format!("/media/job/{}/index.m3u8", job.id)),
            html: String::new(),
        };
        Self { html: JobTemplate { job: row.clone() }.render().unwrap_or_default(), ..row }
    }
}

/// A name that does not exist is a 404; it used to fall back to the first playlist, so
/// `/playlist/DoesNotExist` showed someone else's titles. No name at all has to render.
pub async fn render_playlist(
    state: &AppState,
    user_id: i64,
    name: Option<&str>,
) -> AppResult<Vec<CardView>> {
    let lists = crate::db::playlists::all(state.db(), user_id)?;
    let list = match name {
        Some(want) => Some(
            lists
                .iter()
                .find(|p| p.name == want)
                .ok_or_else(|| crate::error::AppError::not_found("playlist"))?,
        ),
        None => lists.first(),
    };
    let Some(list) = list else {
        return Ok(Vec::new());
    };
    Ok(crate::db::playlists::entries(state.db(), list.id)?
        .into_iter()
        .map(|e| CardView::new(e.anime_id, e.title, e.poster))
        .collect())
}


/// A full page: the layout needs to know who is signed in and which theme to use.
#[derive(Debug, Clone)]
pub struct Chrome {
    pub signed_in: bool,
    pub theme: String,
}

impl Chrome {
    pub fn of(state: &AppState, user: Option<&User>) -> Self {
        Self {
            theme: user
                .and_then(|u| settings::get(state.db(), u.id).ok())
                .map(|s| s.theme)
                .unwrap_or_else(|| "system".to_string()),
            signed_in: user.is_some(),
        }
    }
}

#[derive(Template)]
#[template(path = "home.html")]
pub struct HomeTemplate {
    pub user: Option<User>,
    pub recent: Vec<HistoryRow>,
    pub playlists: Vec<PlaylistChip>,
    pub has_accounts: bool,
    /// Results for a `?q=` visit, so the page works without scripting. With scripting
    /// the search box swaps these in instead.
    pub results: Option<ResultsFragment>,
    pub signed_in: bool,
    pub theme: String,
}

#[derive(Template)]
#[template(path = "signup.html")]
pub struct SignupTemplate {
    pub first: bool,
    pub signed_in: bool,
    pub theme: String,
}

#[derive(Template)]
#[template(path = "login.html")]
pub struct LoginTemplate {
    pub signed_in: bool,
    pub theme: String,
}

#[derive(Template)]
#[template(path = "results.html")]
pub struct ResultsTemplate {
    pub term: String,
    pub cards: Vec<CardView>,
    pub signed_in: bool,
    pub theme: String,
}

/// The same results without the page shell, for the htmx swap. Returning the full page
/// from a fragment request would nest one document inside another.
#[derive(Template)]
#[template(path = "frag/results.html")]
pub struct ResultsGrid {
    pub term: String,
    pub cards: Vec<CardView>,
}

#[derive(Debug, Clone)]
pub struct ResultsFragment {
    pub term: String,
    pub cards: Vec<CardView>,
    /// The rendered fragment, so a page can drop it in without a template of its own.
    pub html: String,
}

impl ResultsFragment {
    /// Search and render. An upstream failure is not shown as an error page here: the
    /// rest of the home page is still worth looking at.
    pub async fn search(
        state: &AppState,
        term: &str,
    ) -> Option<Self> {
        let term = term.trim();
        if term.is_empty() {
            return None;
        }
        match state.source().search(term).await {
            Ok(results) => {
                let cards: Vec<CardView> = results.iter().map(CardView::from_anime).collect();
                let html = ResultsGrid {
                    cards: cards.clone(),
                    term: term.to_string(),
                }
                .render()
                .unwrap_or_default();
                Some(Self {
                    term: term.to_string(),
                    cards,
                    html,
                })
            }
            Err(e) => {
                tracing::warn!(error = %e, term, "search on the home page failed");
                None
            }
        }
    }
}

#[derive(Template)]
#[template(path = "anime.html")]
pub struct AnimeTemplate {
    pub anime: Anime,
    pub poster: Option<String>,
    pub episodes: Vec<EpisodeView>,
    /// How many are watched, shown beside the episode count.
    pub watched_count: usize,
    pub save_buttons: Vec<PlaylistChip>,
    pub can_save: bool,
    pub signed_in: bool,
    pub theme: String,
}

#[derive(Template)]
#[template(path = "watch.html")]
pub struct WatchTemplate {
    pub anime: Anime,
    pub episode: Episode,
    pub mode: Mode,
    pub mode_label: String,
    pub mode_toggle: String,
    pub mode_toggle_label: String,
    pub play: PlayView,
    pub save_buttons: Vec<PlaylistChip>,
    pub has_save: bool,
    /// Where to continue from, and the label for it. None means the viewer never got far
    /// enough for a resume to be worth offering.
    pub resume_secs: Option<i64>,
    pub resume_label: String,
    pub signed_in: bool,
    pub theme: String,
}

#[derive(Template)]
#[template(path = "user.html")]
pub struct UserTemplate {
    pub user: User,
    pub playlists: Vec<PlaylistChip>,
    pub entries: Vec<CardView>,
    pub shown_playlist: String,
    pub show_delete: bool,
    pub recent: Vec<HistoryRow>,
    pub jobs: Vec<JobRow>,
    pub signed_in: bool,
    pub theme: String,
}

#[derive(Template)]
#[template(path = "settings.html")]
pub struct SettingsTemplate {
    pub user: User,
    pub settings: settings::Settings,
    pub heights: [u32; 6],
    pub themes: [&'static str; 3],
    pub banner: Banner,
    pub cache_human: String,
    pub served_human: String,
    pub upstream_fetches: u64,
    pub signed_in: bool,
    pub theme: String,
}

#[derive(Template)]
#[template(path = "notfound.html")]
pub struct NotFoundTemplate {
    pub theme: String,
    pub signed_in: bool,
}

/// A full page for a failed navigation, so an error is never a titleless fragment.
#[derive(Template)]
#[template(path = "error.html")]
pub struct ErrorTemplate {
    pub theme: String,
    pub signed_in: bool,
    pub heading: String,
    pub message: String,
    pub title: String,
}

#[derive(Template)]
#[template(path = "frag/episode_list.html")]
pub struct EpisodeListTemplate {
    pub anime_id: String,
    pub episodes: Vec<EpisodeView>,
}

#[derive(Template)]
#[template(path = "frag/player.html")]
pub struct PlayerTemplate {
    pub anime: Anime,
    pub episode: Episode,
    pub mode: Mode,
    pub mode_label: String,
    pub mode_toggle: String,
    pub mode_toggle_label: String,
    pub play: PlayView,
    pub save_buttons: Vec<PlaylistChip>,
    pub has_save: bool,
    pub resume_secs: Option<i64>,
    pub resume_label: String,
}


#[derive(Template)]
#[template(path = "frag/job_status.html")]
pub struct JobStatusTemplate {
    pub job: JobRow,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::source::{Quality, Subtitle};

    fn stream(heights: &[u32]) -> Stream {
        Stream {
            master_url: "https://h.test/v/master.m3u8".into(),
            referer: "https://h.test/".into(),
            qualities: heights
                .iter()
                .map(|h| {
                    Quality::new(format!("{h}p"), *h, format!("https://h.test/v/{h}/index.m3u8"))
                })
                .collect(),
            subtitles: vec![Subtitle {
                lang: "en".into(),
                label: "English".into(),
                url: "https://h.test/v/subs/en.vtt".into(),
            }],
        }
    }

    fn play(s: &Stream, want: Option<u32>) -> PlayView {
        PlayView::build(s, want, "5", "one-piece-1", "One Piece", "42", Mode::Sub)
    }

    #[test]
    fn a_matching_quality_streams_directly() {
        let p = play(&stream(&[1080, 720, 360]), Some(720));
        assert!(!p.live);
        assert!(p.playlist_url.starts_with("/media/playlist?u="));
        assert!(p.notice.is_none());
        let chosen = p.qualities.iter().find(|q| q.selected).unwrap();
        assert_eq!(chosen.height, 720);
        assert!(chosen.direct);
    }

    #[test]
    fn a_missing_quality_offers_a_conversion() {
        let p = play(&stream(&[1080]), Some(720));
        assert!(p.live, "nothing to stream, so the player waits for the conversion");
        assert!(p.playlist_url.is_empty());
        let convert = p.qualities.iter().find(|q| !q.direct).expect("a convert option");
        assert_eq!(convert.height, 720);
        assert!(convert.label.contains("convert"));
        assert!(convert.selected);
        assert!(p.notice.unwrap().contains("episode 5"));
    }

    #[test]
    fn no_preference_picks_the_best_source_rendition() {
        let p = play(&stream(&[480, 1080]), None);
        assert!(!p.live);
        assert_eq!(p.qualities.iter().find(|q| q.selected).unwrap().height, 1080);
    }

    #[test]
    fn subtitles_are_proxied() {
        let p = play(&stream(&[720]), Some(720));
        assert_eq!(p.subs.len(), 1);
        // The browser asks us for the track, with the upstream URL and the referer
        // carried as parameters, so the CDN never sees this page.
        assert!(p.subs[0].url.starts_with("/media/subtitle?u="));
        assert!(p.subs[0].url.contains("h.test%2Fv%2Fsubs%2Fen.vtt"));
        assert!(p.subs[0].url.contains("r=https%3A%2F%2Fh.test"));
    }

    #[test]
    fn every_quality_gets_a_working_link() {
        let p = play(&stream(&[1080, 480]), Some(1080));
        for q in p.qualities.iter().filter(|q| q.direct) {
            assert!(q.href.starts_with("/watch/one-piece-1/42?"), "bad href {}", q.href);
            assert!(q.href.contains("mode=sub"));
            assert!(q.href.contains(&format!("q={}", q.height)));
        }
    }

    #[test]
    fn a_card_renders_its_own_markup() {
        let c = CardView::new("one-piece-1", "One Piece", Some("https://cdn.test/p.jpg".into()));
        assert!(c.html.contains("/api/poster?u="));
        assert!(c.html.contains("/anime/one-piece-1"));
        assert!(!c.html.contains("https://cdn.test"), "the CDN host must not leak");
    }

    #[test]
    fn a_card_without_a_poster_shows_initials() {
        let c = CardView::new("x-1", "Cowboy Bebop", None);
        assert!(c.poster.is_none());
        assert!(c.html.contains("Co"));
        assert!(!c.html.contains("<img"));
    }

    #[test]
    fn a_title_with_markup_is_escaped_in_the_card() {
        let c = CardView::new("x-1", "<script>alert(1)</script>", None);
        // The angle brackets have to be entities; the text between them stays readable.
        assert!(!c.html.contains("<script>"), "the tag must not survive as markup");
        assert!(!c.html.contains("</script>"));
        assert!(c.html.contains("&#60;script&#62;"), "askama escapes the brackets");
    }

    #[test]
    fn setting_a_resume_twice_does_not_duplicate_the_parameter() {
        // The series row and the per episode row can both carry an offset, and
        // `&t=900&t=900` is a duplicate query parameter.
        let mut row = HistoryRow {
            id: "cb".into(),
            title: "Cowboy Bebop".into(),
            episode: "2".into(),
            when: "just now".into(),
            resume_href: "/watch/cb/21419?ep=2".into(),
            position: String::new(),
            html: String::new(),
        };
        row.set_resume(900);
        row.set_resume(900);
        assert_eq!(row.resume_href, "/watch/cb/21419?ep=2&t=900");
        assert_eq!(row.resume_href.matches("&t=").count(), 1);
        assert_eq!(row.position, "15:00 in");
    }

    #[test]
    fn a_finished_episode_offers_no_resume() {
        let mut row = HistoryRow {
            id: "cb".into(),
            title: "Cowboy Bebop".into(),
            episode: "1".into(),
            when: "just now".into(),
            resume_href: "/watch/cb/21418?ep=1".into(),
            position: String::new(),
            html: String::new(),
        };
        row.set_resume(700);
        assert!(row.resume_href.contains("&t=700"));
        // Zero is what a finished episode stores, and it must clear what was there.
        row.set_resume(0);
        assert_eq!(row.resume_href, "/watch/cb/21418?ep=1");
        assert!(row.position.is_empty());
    }

    #[test]
    fn hx_vals_survives_a_title_with_a_quote_in_it() {
        // The bug this guards: an anime title holding a double quote broke htmx's JSON
        // parse, and the save button did nothing at all with no error shown anywhere.
        let nasty = r#"Re:Zero "Memory Snow" \ OVA"#;
        let json = crate::source::json_obj(&[
            ("anime", "one-piece-1"),
            ("title", nasty),
        ]);
        // The round trip is the assertion that matters: htmx parses this as JSON, so a
        // title that survives a parse and comes back identical is one htmx can read.
        let back: serde_json::Value = serde_json::from_str(&json).expect("must be valid JSON");
        assert_eq!(back["title"], nasty);
    }

    #[test]
    fn hx_vals_escapes_control_characters() {
        let json = crate::source::json_obj(&[("title", "a\nb\tc")]);
        let back: serde_json::Value = serde_json::from_str(&json).expect("must be valid JSON");
        assert_eq!(back["title"], "a\nb\tc");
    }

    #[test]
    fn history_rows_describe_themselves() {
        let p = crate::db::history::Progress {
            anime_id: "a-1".into(),
            title: "A".into(),
            poster: None,
            episode_number: "3".into(),
            episode_id: "3".into(),
            // Milliseconds, which is what the column holds. This test once passed a seconds
            // value and accepted either wording, so the rendered time was off by 56 years.
            updated_at: (crate::time::now_secs() - 120) * 1000,
            position_secs: 0,
        };
        let row = HistoryRow::from(&p);
        assert_eq!(row.id, "a-1");
        assert_eq!(row.episode, "3");
        assert_eq!(row.when, "2 minutes ago");
    }
}
