//! Upstream provider. One trait, one implementation (hianime), and the plain data types
//! the rest of the app passes around. Nothing below this module knows about HTML.

pub mod deobf;
pub mod hianime;
pub mod m3u8;

use serde::{Deserialize, Serialize};

use crate::error::AppResult;

/// A title as it appears in search results and playlists.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Anime {
    /// The provider slug, for example `one-piece-1`. Doubles as the stable key for a playlist.
    pub id: String,
    pub title: String,
    /// Upstream image URL. Proxied through our cache before it ever reaches a browser.
    pub poster: Option<String>,
}

impl Anime {
    pub fn new(id: impl Into<String>, title: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            poster: None,
        }
    }
    pub fn with_poster(mut self, poster: Option<String>) -> Self {
        self.poster = poster.filter(|p| !p.is_empty());
        self
    }
    /// The page for this title, used as the canonical link and for og tags.
    pub fn page_url(&self, base: &str) -> String {
        format!("{}/{}", base.trim_end_matches('/'), self.id)
    }
}

/// htmx parses that attribute as JSON, so a title containing a quote has to be escaped as
/// JSON. An unescaped one breaks the parse and the button does nothing, with no error.
pub fn json_obj(pairs: &[(&str, &str)]) -> String {
    let mut out = String::from("{");
    for (i, (k, v)) in pairs.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('"');
        out.push_str(k);
        out.push_str("\":");
        out.push_str(&json_string(v));
    }
    out.push('}');
    out
}

/// A JSON string literal, quotes and control characters escaped.
fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Episode {
    /// Provider episode id, sent back to the servers endpoint.
    pub id: String,
    /// Display number as the provider labels it, which is not always a plain integer.
    pub number: String,
    pub title: Option<String>,
}

impl Episode {
    pub fn new(id: impl Into<String>, number: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            number: number.into(),
            title: None,
        }
    }
    /// Sortable form of the number, for ordering and for "next episode" arithmetic.
    pub fn sort_key(&self) -> f64 {
        parse_leading_number(&self.number)
    }
}

/// "Episode 12.5" and "12" both start with a number; anything else sorts last.
fn parse_leading_number(s: &str) -> f64 {
    let digits: String = s
        .chars()
        .skip_while(|c| !c.is_ascii_digit())
        .take_while(|c| c.is_ascii_digit() || *c == '.')
        .collect();
    digits.parse().unwrap_or(f64::MAX)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Mode {
    #[default]
    Sub,
    Dub,
}

impl Mode {
    pub fn as_str(self) -> &'static str {
        match self {
            Mode::Sub => "sub",
            Mode::Dub => "dub",
        }
    }
    pub fn parse(s: &str) -> Mode {
        match s {
            "dub" => Mode::Dub,
            _ => Mode::Sub,
        }
    }
    pub fn toggled(self) -> Mode {
        match self {
            Mode::Sub => Mode::Dub,
            Mode::Dub => Mode::Sub,
        }
    }
}

/// One rendition of an episode. `height` is 0 when the provider only gives a label.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Quality {
    pub label: String,
    pub height: u32,
    pub bandwidth: u64,
    pub url: String,
}

impl Quality {
    pub fn new(label: impl Into<String>, height: u32, url: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            height,
            bandwidth: 0,
            url: url.into(),
        }
    }
    /// A stable value for the quality picker, also used as the job key fragment.
    pub fn slug(&self) -> String {
        if self.height > 0 {
            format!("{}p", self.height)
        } else {
            self.label.clone()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Subtitle {
    pub lang: String,
    pub label: String,
    pub url: String,
}

/// Everything needed to play an episode: the renditions, the headers the CDN wants,
/// and the subtitle track the player should load by default.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stream {
    /// The multivariant playlist, kept for the "all qualities" case.
    pub master_url: String,
    /// Origin the CDN checks against. Passed back on every segment request.
    pub referer: String,
    /// Sorted best first.
    pub qualities: Vec<Quality>,
    pub subtitles: Vec<Subtitle>,
}

impl Stream {
    pub fn best(&self) -> Option<&Quality> {
        self.qualities.first()
    }
    /// Closest match to a target height. Exact wins, otherwise the next one down,
    /// otherwise the smallest available.
    pub fn pick(&self, height: u32) -> Option<&Quality> {
        if let Some(q) = self.qualities.iter().find(|q| q.height == height && q.height > 0) {
            return Some(q);
        }
        if let Some(q) = self
            .qualities
            .iter()
            .filter(|q| q.height > 0 && q.height < height)
            .max_by_key(|q| q.height)
        {
            return Some(q);
        }
        self.qualities.last()
    }
    pub fn has_height(&self, height: u32) -> bool {
        height > 0 && self.qualities.iter().any(|q| q.height == height)
    }
}

/// The futures are boxed so the trait stays usable as `dyn AnimeSource`, which is what lets a
/// handler take the source without knowing which one it is.
pub trait AnimeSource: Send + Sync {
    fn name(&self) -> &'static str;
    fn search<'a>(&'a self, query: &'a str) -> SourceFuture<'a, Vec<Anime>>;
    /// The provider's own page for a slug, which is where the real title and poster
    /// live. A slug alone carries neither, so anything that shows a title has to ask.
    fn details<'a>(&'a self, anime_id: &'a str) -> SourceFuture<'a, Anime>;
    fn episodes<'a>(&'a self, anime_id: &'a str) -> SourceFuture<'a, Vec<Episode>>;
    fn stream<'a>(
        &'a self,
        anime: &'a Anime,
        episode: &'a Episode,
        mode: Mode,
    ) -> SourceFuture<'a, Stream>;
}

/// A boxed, sendable future from a provider call. Boxing keeps the trait object safe,
/// and borrowing keeps it dyn compatible.
pub type SourceFuture<'a, T> =
    std::pin::Pin<Box<dyn std::future::Future<Output = AppResult<T>> + Send + 'a>>;

/// Box a future so it satisfies `SourceFuture`.
pub fn future<'a, T, F>(f: F) -> SourceFuture<'a, T>
where
    F: std::future::Future<Output = AppResult<T>> + Send + 'a,
    T: Send + 'a,
{
    Box::pin(f)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qualities_are_ordered_by_the_caller() {
        let s = Stream {
            master_url: "m".into(),
            referer: "r".into(),
            qualities: vec![
                Quality::new("1080", 1080, "a"),
                Quality::new("720", 720, "b"),
                Quality::new("360", 360, "c"),
            ],
            subtitles: vec![],
        };
        assert_eq!(s.best().unwrap().height, 1080);
    }

    #[test]
    fn exact_height_wins() {
        let s = Stream {
            master_url: "m".into(),
            referer: "r".into(),
            qualities: vec![Quality::new("1080", 1080, "a"), Quality::new("720", 720, "b")],
            subtitles: vec![],
        };
        assert_eq!(s.pick(720).unwrap().label, "720");
        assert!(s.has_height(720));
        assert!(!s.has_height(480));
    }

    #[test]
    fn a_missing_height_falls_back_to_the_next_one_down() {
        let s = Stream {
            master_url: "m".into(),
            referer: "r".into(),
            qualities: vec![Quality::new("1080", 1080, "a"), Quality::new("720", 720, "b")],
            subtitles: vec![],
        };
        assert_eq!(s.pick(900).unwrap().height, 720);
        assert_eq!(s.pick(480).unwrap().height, 720, "nothing below 480, so take the smallest");
    }

    #[test]
    fn quality_slug_falls_back_to_the_label() {
        assert_eq!(Quality::new("1080", 1080, "a").slug(), "1080p");
        assert_eq!(Quality::new("auto", 0, "a").slug(), "auto");
    }

    #[test]
    fn episode_numbers_sort_numerically() {
        assert_eq!(Episode::new("1", "2").sort_key(), 2.0);
        assert!(Episode::new("1", "10").sort_key() > Episode::new("2", "9").sort_key());
        assert_eq!(Episode::new("1", "12.5").sort_key(), 12.5);
        assert!(Episode::new("1", "special").sort_key() > 1e9);
    }

    #[test]
    fn mode_toggles_and_parses() {
        assert_eq!(Mode::parse("dub"), Mode::Dub);
        assert_eq!(Mode::parse("nonsense"), Mode::Sub);
        assert_eq!(Mode::Sub.toggled(), Mode::Dub);
        assert_eq!(Mode::Dub.toggled(), Mode::Sub);
    }

    #[test]
    fn anime_page_url_joins_cleanly() {
        assert_eq!(Anime::new("one-piece-1", "One Piece").page_url("https://x.at/"), "https://x.at/one-piece-1");
    }
}
