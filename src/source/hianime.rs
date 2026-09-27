//! The hianime provider: the chain ani-cli drives, in Rust. `search`, `episodes`, `servers`,
//! `embed`, `master`. The numeric id is the slug after the final dash: `one-piece-1` is `1`.

use std::time::Duration;

use scraper::{Element, Html, Selector};

use crate::error::{AppError, AppResult};
use crate::source::{deobf, m3u8, Anime, AnimeSource, Episode, Mode, Quality, SourceFuture, Stream, Subtitle};
use crate::util;

/// The user agent ani-cli presents. The CDN rejects requests without one.
const AGENT: &str =
    "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/124.0.0.0 Safari/537.36";

const TIMEOUT: Duration = Duration::from_secs(20);

/// The sidebar repeats the result markup, so the document is cut before it.
const SIDEBAR_MARK: &str = "id=\"main-sidebar\"";

/// Cloudflare's interstitial title.
const CHALLENGE_MARK: &str = "Just a moment";

pub struct HiAnime {
    http: reqwest::Client,
    base: String,
    embed_servers: Vec<String>,
}

impl HiAnime {
    pub fn new(base: &str, embed_servers: Vec<String>) -> AppResult<Self> {
        let http = reqwest::Client::builder()
            .user_agent(AGENT)
            .timeout(TIMEOUT)
            .connect_timeout(Duration::from_secs(10))
            .build()
            .map_err(|e| AppError::internal(format!("http client: {e}")))?;
        Ok(Self {
            http,
            base: base.trim_end_matches('/').to_string(),
            embed_servers,
        })
    }

    /// The underlying HTTP client, so a caller can make a request the way the source
    /// would. Used by the integration tests to check the CDN's referer rules.
    pub fn client(&self) -> &reqwest::Client {
        &self.http
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    /// GET a URL as text, optionally with a Referer. Rejects a Cloudflare challenge
    /// explicitly so the user sees why nothing was found.
    async fn get_text(&self, url: &str, referer: Option<&str>) -> AppResult<String> {
        let mut req = self.http.get(url);
        if let Some(r) = referer {
            req = req.header(reqwest::header::REFERER, r);
        }
        let res = req
            .send()
            .await
            .map_err(|e| AppError::upstream("hianime", format!("{url}: {e}")))?;
        let status = res.status();
        let body = res
            .text()
            .await
            .map_err(|e| AppError::upstream("hianime", format!("{url}: {e}")))?;
        if !status.is_success() {
            return Err(AppError::upstream("hianime", format!("HTTP {} from {url}", status.as_u16())));
        }
        if body.contains(CHALLENGE_MARK) {
            return Err(AppError::upstream(
                "hianime",
                "the CDN wants a browser check, try again in a moment",
            ));
        }
        Ok(body)
    }

    /// The two list endpoints wrap their markup in a JSON `html` field with the quotes
    /// and newlines escaped.
    async fn get_html_fragment(&self, url: &str, referer: Option<&str>) -> AppResult<String> {
        let body = self.get_text(url, referer).await?;
        let trimmed = body.trim_start();
        if !trimmed.starts_with('{') {
            return Ok(body);
        }
        let value: serde_json::Value = serde_json::from_str(trimmed)
            .map_err(|e| AppError::upstream("hianime", format!("bad json from {url}: {e}")))?;
        match value.get("html").and_then(|v| v.as_str()) {
            Some(fragment) => Ok(util::unescape_json_string(fragment)),
            None => Ok(body),
        }
    }

    /// hianime keys its API by the numeric tail of the slug.
    pub fn numeric_id(anime_id: &str) -> &str {
        match anime_id.rsplit_once('-') {
            Some((_, tail)) if !tail.is_empty() => tail,
            _ => anime_id,
        }
    }
}

impl AnimeSource for HiAnime {
    fn name(&self) -> &'static str {
        "hianime"
    }

    fn search<'a>(&'a self, query: &'a str) -> SourceFuture<'a, Vec<Anime>> {
        crate::source::future(self.do_search(query))
    }

    fn details<'a>(&'a self, anime_id: &'a str) -> SourceFuture<'a, Anime> {
        crate::source::future(self.do_details(anime_id))
    }

    fn episodes<'a>(&'a self, anime_id: &'a str) -> SourceFuture<'a, Vec<Episode>> {
        crate::source::future(self.do_episodes(anime_id))
    }

    fn stream<'a>(
        &'a self,
        anime: &'a Anime,
        episode: &'a Episode,
        mode: Mode,
    ) -> SourceFuture<'a, Stream> {
        crate::source::future(self.do_stream(anime, episode, mode))
    }
}

impl HiAnime {
    async fn do_search(&self, query: &str) -> AppResult<Vec<Anime>> {
        let q = query.trim();
        if q.is_empty() {
            return Ok(Vec::new());
        }
        let url = self.url(&format!("/search?keyword={}", util::url_encode(&q.replace(' ', "+"))));
        let body = self.get_text(&url, None).await?;

        // Cut before the sidebar so the recommendation rail is not parsed as results.
        let body = match body.find(SIDEBAR_MARK) {
            Some(at) => &body[..at],
            None => &body[..],
        };

        let block = Selector::parse("div.film-detail").expect("static selector");
        let link = Selector::parse("h3.film-name a").expect("static selector");
        let poster = Selector::parse("img.film-poster-img").expect("static selector");
        let doc = Html::parse_document(body);

        let mut seen = std::collections::HashSet::new();
        let mut out = Vec::new();
        for item in doc.select(&block) {
            let Some(a) = item.select(&link).next() else {
                continue;
            };
            let Some(title) = a.value().attr("title") else {
                continue;
            };
            let id = slug_of(a.value().attr("href").unwrap_or(""), &self.base);
            if id.is_empty() || !seen.insert(id.clone()) {
                continue;
            }
            let title = util::decode_entities(&util::decode_entities(title));
            if title.trim().is_empty() {
                continue;
            }
            // The poster lives in the sibling block that precedes this one, in the same
            // result item. Looking for it inside the detail block finds nothing.
            let img = item
                .prev_sibling_element()
                .and_then(|prev| prev.select(&poster).next())
                .and_then(|i| i.value().attr("src"))
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string);
            out.push(Anime::new(id, title.trim()).with_poster(img));
        }
        Ok(out)
    }

    /// Read the title and poster off the provider's page for this slug.
    async fn do_details(&self, anime_id: &str) -> AppResult<Anime> {
        let url = self.url(&format!("/{anime_id}"));
        let body = self.get_text(&url, None).await?;
        let doc = Html::parse_document(&body);

        // The title is on the heading, and the poster is the first image with the
        // poster class. Both are on the same page, so one request covers them.
        let title_sel = Selector::parse("h2.film-name").expect("static selector");
        let poster_sel = Selector::parse("img.film-poster-img").expect("static selector");

        let title = doc
            .select(&title_sel)
            .next()
            .and_then(|h| h.value().attr("data-jname").or_else(|| h.value().attr("title")))
            .map(|t| util::decode_entities(t).trim().to_string())
            .filter(|t| !t.is_empty());

        let poster = doc
            .select(&poster_sel)
            .next()
            .and_then(|i| i.value().attr("src"))
            .map(|s| absolutise_image(&self.base, s.trim()));

        match (title, poster) {
            (Some(title), poster) => Ok(Anime::new(anime_id, title).with_poster(poster)),
            (None, _) => Err(AppError::upstream(
                "hianime",
                format!("no title on the page for {anime_id}"),
            )),
        }
    }

    async fn do_episodes(&self, anime_id: &str) -> AppResult<Vec<Episode>> {
        let url = self.url(&format!("/api/theme/episode/list/{}", Self::numeric_id(anime_id)));
        let body = self.get_html_fragment(&url, None).await?;
        let item = Selector::parse("a.ep-item").expect("static selector");
        let doc = Html::parse_document(&body);
        let mut out = Vec::new();
        for el in doc.select(&item) {
            let v = el.value();
            let (Some(id), Some(number)) = (v.attr("data-id"), v.attr("data-number")) else {
                continue;
            };
            if id.is_empty() || number.is_empty() {
                continue;
            }
            out.push(Episode::new(id, number).with_title(v.attr("title").map(str::to_string)));
        }
        // A few listings repeat an entry across season blocks, so dedupe on the id.
        out.sort_by(|a, b| {
            a.sort_key()
                .partial_cmp(&b.sort_key())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        out.dedup_by(|a, b| a.id == b.id);
        Ok(out)
    }

    async fn do_stream(&self, _anime: &Anime, episode: &Episode, mode: Mode) -> AppResult<Stream> {
        let url = self.url(&format!("/api/theme/episode/servers?episodeId={}", episode.id));
        let body = self.get_html_fragment(&url, None).await?;
        let embed = self.pick_embed(&body, mode)?;

        // The stream host only serves requests that came from the embed page.
        let referer = m3u8::origin(&embed).unwrap_or_default();
        let page = self.get_text(&embed, Some(&referer)).await?;
        let blob = deobf::extract_blob(&page).ok_or_else(|| {
            AppError::upstream("embed", "the player config was not in the embed page")
        })?;
        let cfg = deobf::parse_config(&blob)?;
        if cfg.src.is_empty() {
            return Err(AppError::upstream("embed", "the player config named no playlist"));
        }

        let master = self.get_text(&cfg.src, Some(&referer)).await?;
        let qualities = m3u8::parse_master(&master, &cfg.src)?;

        let subtitles = cfg
            .subtitles
            .iter()
            .filter(|s| !s.src.is_empty())
            .map(|s| Subtitle {
                lang: if s.lang.is_empty() { "und".into() } else { s.lang.clone() },
                label: if s.label.is_empty() { s.lang.clone() } else { s.label.clone() },
                url: m3u8::absolutise(&cfg.src, &s.src),
            })
            .collect();

        Ok(Stream {
            master_url: cfg.src,
            referer,
            qualities,
            subtitles,
        })
    }
}

impl HiAnime {
    /// Walk the server list in configured order and take the first one that decodes.
    fn pick_embed(&self, servers_html: &str, mode: Mode) -> AppResult<String> {
        let item = Selector::parse("div.server-item").expect("static selector");
        let doc = Html::parse_document(servers_html);
        let wanted = mode.as_str();

        let candidates: Vec<(usize, &str)> = doc
            .select(&item)
            .filter_map(|el| {
                let v = el.value();
                let hash = v.attr("data-hash")?;
                if v.attr("data-type") != Some(wanted) {
                    return None;
                }
                let name = v.attr("data-server-name").unwrap_or("");
                let rank = self
                    .embed_servers
                    .iter()
                    .position(|s| s == name)
                    .unwrap_or(usize::MAX);
                Some((rank, hash))
            })
            .collect();

        for (_, hash) in &candidates {
            if let Ok(url) = base64_decode(hash) {
                if url.starts_with("http") {
                    return Ok(url);
                }
            }
        }
        Err(AppError::upstream(
            "hianime",
            format!("no {} source for this episode", wanted),
        ))
    }
}

fn base64_decode(s: &str) -> Result<String, base64::DecodeError> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD.decode(s.trim())?;
    String::from_utf8(bytes).map_err(|_| base64::DecodeError::InvalidByte(0, 0))
}

/// Poster URLs are sometimes site relative, so they are made absolute before use.
fn absolutise_image(base: &str, src: &str) -> String {
    if src.starts_with("http") {
        return src.to_string();
    }
    if let Some(rest) = src.strip_prefix("//") {
        let scheme = if base.starts_with("https") { "https" } else { "http" };
        return format!("{scheme}://{rest}");
    }
    if src.starts_with('/') {
        return format!("{}{}", base.trim_end_matches('/'), src);
    }
    format!("{}/{}", base.trim_end_matches('/'), src)
}

/// The slug is the last path segment of the href, with any query string dropped.
fn slug_of(href: &str, base: &str) -> String {
    // A link that points off site is not a title, whatever its last segment looks like.
    if href.starts_with("http") && !href.starts_with(base) {
        return String::new();
    }
    let host = base.split("://").nth(1).unwrap_or("");
    let path = href.split(['?', '#']).next().unwrap_or(href);
    let trimmed = path.trim_end_matches('/');
    // The last segment of a bare origin is the host, not a slug.
    let last = match trimmed.rsplit('/').next() {
        Some(seg) if !seg.is_empty() && seg != host => seg,
        _ => return String::new(),
    };
    last.to_string()
}

impl Episode {
    pub fn with_title(mut self, title: Option<String>) -> Self {
        self.title = title.filter(|t| !t.is_empty());
        self
    }
}

/// Sorted renditions, exposed for the UI without going through a `Stream`.
pub fn quality_labels(q: &[Quality]) -> Vec<String> {
    q.iter().map(|x| x.label.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numeric_id_takes_the_tail() {
        assert_eq!(HiAnime::numeric_id("one-piece-1"), "1");
        assert_eq!(HiAnime::numeric_id("one-piece-film-red-2825"), "2825");
        assert_eq!(HiAnime::numeric_id("naruto"), "naruto");
        assert_eq!(HiAnime::numeric_id("a-"), "a-");
    }

    #[test]
    fn slug_comes_from_the_href_tail() {
        assert_eq!(slug_of("https://hianime.at/one-piece-1?x=1", "https://hianime.at"), "one-piece-1");
        assert_eq!(slug_of("/spy-x-family-1234", "https://hianime.at"), "spy-x-family-1234");
        assert_eq!(slug_of("https://hianime.at/abc/", "https://hianime.at"), "abc");
        assert_eq!(slug_of("https://other.test/abc-1", "https://hianime.at"), "");
        assert_eq!(slug_of("https://hianime.at/", "https://hianime.at"), "");
    }

    #[test]
    fn base64_round_trip() {
        let encoded = "aHR0cHM6Ly96b2tvYW5pbWUudmlkZW8vc3RyZWFtL21hbC8yMS8xL3N1Yg==";
        assert_eq!(
            base64_decode(encoded).unwrap(),
            "https://zokoanime.video/stream/mal/21/1/sub"
        );
        assert!(base64_decode("!!!").is_err());
    }

    #[test]
    fn server_picking_prefers_configured_names_and_the_right_mode() {
        let src = HiAnime::new("https://hianime.at", vec!["ZokoAnime".into()]).unwrap();
        let b64 = "aHR0cHM6Ly96b2tvYW5pbWUudmlkZW8vc3RyZWFtL21hbC8yMS8xL3N1Yg==";
        let html = format!(
            r#"<div class="item server-item" data-type="sub" data-server-name="Other" data-hash="Zm9v"></div>
               <div class="item server-item" data-type="sub" data-server-name="ZokoAnime" data-hash="{b64}"></div>
               <div class="item server-item" data-type="dub" data-server-name="ZokoAnime" data-hash="{b64}"></div>"#
        );
        let got = src.pick_embed(&html, Mode::Sub).unwrap();
        assert!(got.starts_with("https://zokoanime.video/"));
    }

    #[test]
    fn dub_is_a_different_pool() {
        let src = HiAnime::new("https://hianime.at", vec!["ZokoAnime".into()]).unwrap();
        let sub = encode_b64("https://zokoanime.video/stream/mal/21/1/sub");
        let dub = encode_b64("https://zokoanime.video/stream/mal/21/2/dub");
        let html = format!(
            r#"<div class="item server-item" data-type="sub" data-server-name="ZokoAnime" data-hash="{sub}"></div>
               <div class="item server-item" data-type="dub" data-server-name="ZokoAnime" data-hash="{dub}"></div>"#
        );
        assert!(src.pick_embed(&html, Mode::Sub).unwrap().ends_with("/1/sub"));
        assert!(src.pick_embed(&html, Mode::Dub).unwrap().ends_with("/2/dub"));
        let err = src.pick_embed("", Mode::Dub).unwrap_err();
        assert!(err.to_string().contains("no dub source"));
    }

    fn encode_b64(text: &str) -> String {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(text)
    }

    #[test]
    fn an_empty_server_list_is_reported_clearly() {
        let src = HiAnime::new("https://hianime.at", vec![]).unwrap();
        let err = src.pick_embed("<html>no servers</html>", Mode::Sub).unwrap_err();
        assert!(err.to_string().contains("no sub source"));
    }

    #[test]
    fn parses_a_search_page_fragment() {
        // Shape copied from a real result page: the href carries the slug and the
        // title attribute carries the decoded display name.
        let html = r#"<div class="film-detail">
            <h3 class="film-name"><a href="https://hianime.at/one-piece-1" title="One Piece" class="dynamic-name">One Piece</a></h3>
            <div class="fd-poster"><img src="https://cdn.test/p.jpg" class="film-poster-img" alt="One Piece"></div>
        </div>"#;
        let block = Selector::parse("div.film-detail").unwrap();
        let link = Selector::parse("h3.film-name a").unwrap();
        let poster = Selector::parse("img.film-poster-img").unwrap();
        let doc = Html::parse_document(html);
        let a = doc.select(&block).next().unwrap().select(&link).next().unwrap();
        let id = slug_of(a.value().attr("href").unwrap(), "https://hianime.at");
        assert_eq!(id, "one-piece-1");
        let img = doc.select(&poster).next().unwrap();
        assert_eq!(img.value().attr("src").unwrap(), "https://cdn.test/p.jpg");
    }

    #[test]
    fn parses_an_episode_fragment() {
        let html = r#"<a title="Episode 1" class="ssl-item ep-item" data-number="1" data-id="1" href="https://hianime.at/watch/one-piece-1?ep=1"></a>
                      <a title="Episode 2" class="ssl-item ep-item" data-number="2" data-id="2" href="https://hianime.at/watch/one-piece-1?ep=2"></a>"#;
        let item = Selector::parse("a.ep-item").unwrap();
        let doc = Html::parse_document(html);
        let eps: Vec<(String, String)> = doc
            .select(&item)
            .map(|e| (e.value().attr("data-id").unwrap().into(), e.value().attr("data-number").unwrap().into()))
            .collect();
        assert_eq!(eps, vec![("1".into(), "1".into()), ("2".into(), "2".into())]);
    }

    #[test]
    fn quality_labels_helper() {
        let q = vec![Quality::new("1080p", 1080, "a"), Quality::new("720p", 720, "b")];
        assert_eq!(quality_labels(&q), vec!["1080p", "720p"]);
    }
}
