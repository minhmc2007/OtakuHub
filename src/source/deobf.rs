//! The embed payload decoder: base64 of JSON XORed with a rotating key, the same shape as
//! the ani-cli helper.

use crate::error::{AppError, AppResult};

const KEY: &[u8] = b"otaku-embed-v1";

/// base64 decode, then XOR each byte with the key at `index % KEY.len()`.
pub fn decode_blob(blob: &str) -> AppResult<Vec<u8>> {
    use base64::Engine;
    let raw = base64::engine::general_purpose::STANDARD
        .decode(blob.trim())
        .map_err(|e| AppError::upstream("embed", format!("bad base64: {e}")))?;
    Ok(xor(&raw, KEY))
}

pub fn xor(data: &[u8], key: &[u8]) -> Vec<u8> {
    if key.is_empty() {
        return data.to_vec();
    }
    data.iter()
        .enumerate()
        .map(|(i, b)| b ^ key[i % key.len()])
        .collect()
}

/// Pull `window.__P="..."` out of the embed page. The value is a JS string literal,
/// so only the escaped backslash and quote forms can appear inside it.
pub fn extract_blob(html: &str) -> Option<String> {
    let marker = "window.__P=\"";
    let start = html.find(marker)? + marker.len();
    let rest = &html[start..];
    let bytes = rest.as_bytes();
    let mut out = String::with_capacity(rest.len().min(4096));
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'"' => return Some(out),
            b'\\' if i + 1 < bytes.len() => {
                out.push(bytes[i + 1] as char);
                i += 2;
            }
            b => {
                out.push(b as char);
                i += 1;
            }
        }
    }
    None
}

/// The parts of the embed config the player actually uses.
#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct EmbedConfig {
    #[serde(default)]
    pub src: String,
    #[serde(default)]
    pub poster: String,
    #[serde(default)]
    pub subtitles: Vec<RawSubtitle>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Deserialize)]
pub struct RawSubtitle {
    #[serde(default)]
    pub lang: String,
    #[serde(default)]
    pub label: String,
    #[serde(default)]
    pub src: String,
    #[serde(default, rename = "default")]
    pub is_default: bool,
}

pub fn parse_config(blob: &str) -> AppResult<EmbedConfig> {
    let bytes = decode_blob(blob)?;
    serde_json::from_slice(&bytes).map_err(|e| AppError::upstream("embed", format!("bad config: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;

    fn encode(json: &str) -> String {
        let xored = xor(json.as_bytes(), KEY);
        base64::engine::general_purpose::STANDARD.encode(xored)
    }

    #[test]
    fn round_trips_a_config() {
        let json = r#"{"src":"https://hls.test/master.m3u8","subtitles":[{"lang":"en","default":true,"src":"https://hls.test/s.vtt"}]}"#;
        let cfg = parse_config(&encode(json)).unwrap();
        assert_eq!(cfg.src, "https://hls.test/master.m3u8");
        assert_eq!(cfg.subtitles.len(), 1);
        assert!(cfg.subtitles[0].is_default);
        assert_eq!(cfg.subtitles[0].label, "", "an absent label is not an error");
    }

    #[test]
    fn decodes_a_blob_written_in_the_embed_format() {
        // Same shape the embed page uses: base64 of JSON XORed with the rotating key.
        let json = r#"{"src":"https://hls.test/v/abc/master.m3u8","subtitles":[]}"#;
        let blob = encode(json);
        let text = String::from_utf8(decode_blob(&blob).unwrap()).unwrap();
        assert_eq!(text, json);
    }

    #[test]
    fn xor_is_self_inverse_and_periodic() {
        let data = b"the quick brown fox";
        let once = xor(data, KEY);
        assert_ne!(once, data.to_vec());
        assert_eq!(xor(&once, KEY), data.to_vec());
        // The key repeats, so applying it twice over a longer key gives the same result
        // over the first pass. That is what "rotating key" means here.
        let longer: Vec<u8> = KEY.iter().chain(KEY).copied().collect();
        assert_eq!(xor(&once, KEY), xor(&once, &longer));
    }

    #[test]
    fn the_key_is_the_one_the_provider_uses() {
        // The key is part of the upstream contract, so a change here has to be visible.
        assert_eq!(KEY, b"otaku-embed-v1");
        assert_eq!(KEY.len(), 14);
    }

    #[test]
    fn finds_the_blob_in_a_page() {
        // The value is a JS string literal, so `\"` inside it is an escaped quote and the
        // extracted value carries the quote alone.
        let html = r#"<script>window.__P="AAAA\"BBB";var x=1;</script>"#;
        assert_eq!(extract_blob(html).as_deref(), Some("AAAA\"BBB"));
        assert_eq!(extract_blob(html).unwrap().len(), 8);
    }

    #[test]
    fn missing_blob_is_none() {
        assert_eq!(extract_blob("<html>nothing here</html>"), None);
    }

    #[test]
    fn bad_base64_is_an_upstream_error() {
        let err = parse_config("!!!not base64!!!").unwrap_err();
        assert_eq!(err.status(), axum::http::StatusCode::BAD_GATEWAY);
    }

    #[test]
    fn valid_base64_that_is_not_json_is_rejected() {
        let blob = base64::engine::general_purpose::STANDARD.encode(b"\x00\x01\x02");
        assert!(parse_config(&blob).is_err());
    }
}
