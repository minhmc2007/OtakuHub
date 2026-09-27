//! Media: hardware probing, transcoding, and the caching HLS proxy.

pub mod jobs;
pub mod probe;
pub mod range;
pub mod transcode;

pub use probe::{Encoder, Hardware};

use serde::{Deserialize, Serialize};

/// The two codecs the UI offers. Anything else is not worth exposing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum Codec {
    #[default]
    H264,
    H265,
}

impl Codec {
    pub fn as_str(self) -> &'static str {
        match self {
            Codec::H264 => "h264",
            Codec::H265 => "h265",
        }
    }
    pub fn parse(s: &str) -> Option<Codec> {
        match s {
            "h264" | "avc" | "x264" => Some(Codec::H264),
            "h265" | "hevc" | "hev1" | "x265" => Some(Codec::H265),
            _ => None,
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Codec::H264 => "H.264 / AVC",
            Codec::H265 => "H.265 / HEVC",
        }
    }
    /// Four character tag, which is what MP4 muxing wants.
    pub fn fcc_tag(self) -> &'static str {
        match self {
            Codec::H264 => "avc1",
            Codec::H265 => "hvc1",
        }
    }
    /// The software encoder used only when no hardware path works.
    pub fn software_encoder(self) -> &'static str {
        match self {
            Codec::H264 => "libx264",
            Codec::H265 => "libx265",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codec_names_round_trip() {
        for c in [Codec::H264, Codec::H265] {
            assert_eq!(Codec::parse(c.as_str()), Some(c));
        }
        assert_eq!(Codec::parse("AV1"), None);
        assert_eq!(Codec::parse("hevc"), Some(Codec::H265));
        assert_eq!(Codec::parse("x265"), Some(Codec::H265));
    }

    #[test]
    fn labels_and_tags() {
        assert_eq!(Codec::H265.fcc_tag(), "hvc1");
        assert_eq!(Codec::H265.software_encoder(), "libx265");
        assert!(Codec::H264.label().contains("AVC"));
    }
}
