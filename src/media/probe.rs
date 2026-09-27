//! Hardware encoder discovery. A candidate is proven by encoding one frame and taking a packet
//! out, so a driver that is present but unusable is rejected here.

use std::path::{Path, PathBuf};

use ffmpeg::ffi::AVHWDeviceType;
use ffmpeg::Rational;

use super::Codec;
use crate::error::AppResult;

/// A hardware encode path, in the order they are tried.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Encoder {
    Vaapi,
    Cuda,
    Qsv,
    Amf,
    Software,
}

/// What the machine can actually do, as opposed to what it appears to have.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hardware {
    pub encoder: Encoder,
    /// The device node or index that was used, for the settings page.
    pub device: String,
    /// A human description of what was found, for the settings page.
    pub detail: String,
    /// Encoders that were present but failed the live test, so the user knows why
    /// hardware was not used.
    pub rejected: Vec<(String, String)>,
}

impl Hardware {
    pub fn is_hardware(&self) -> bool {
        self.encoder.is_hardware()
    }
    pub fn summary(&self) -> String {
        if self.is_hardware() {
            format!("{} on {}", self.encoder.label(), self.device)
        } else {
            "software encoding".to_string()
        }
    }
}

impl Encoder {
    pub fn is_hardware(self) -> bool {
        !matches!(self, Encoder::Software)
    }

    pub fn label(self) -> &'static str {
        match self {
            Encoder::Vaapi => "VAAPI",
            Encoder::Cuda => "NVENC",
            Encoder::Qsv => "Quick Sync",
            Encoder::Amf => "AMD AMF",
            Encoder::Software => "software",
        }
    }

    /// The libav encoder name. The suffix is the codec.
    pub fn encoder_name(self, codec: Codec) -> &'static str {
        match (self, codec) {
            (Encoder::Vaapi, Codec::H264) => "h264_vaapi",
            (Encoder::Vaapi, Codec::H265) => "hevc_vaapi",
            (Encoder::Cuda, Codec::H264) => "h264_nvenc",
            (Encoder::Cuda, Codec::H265) => "hevc_nvenc",
            (Encoder::Qsv, Codec::H264) => "h264_qsv",
            (Encoder::Qsv, Codec::H265) => "hevc_qsv",
            (Encoder::Amf, Codec::H264) => "h264_amf",
            (Encoder::Amf, Codec::H265) => "hevc_amf",
            (Encoder::Software, c) => c.software_encoder(),
        }
    }

    pub fn hw_device_type(self) -> Option<AVHWDeviceType> {
        match self {
            Encoder::Vaapi => Some(AVHWDeviceType::AV_HWDEVICE_TYPE_VAAPI),
            Encoder::Cuda => Some(AVHWDeviceType::AV_HWDEVICE_TYPE_CUDA),
            Encoder::Qsv => Some(AVHWDeviceType::AV_HWDEVICE_TYPE_QSV),
            Encoder::Amf => Some(AVHWDeviceType::AV_HWDEVICE_TYPE_AMF),
            Encoder::Software => None,
        }
    }

    /// Taken from `ffmpeg::ffi`, not the `Pixel` enum. That enum is a hand maintained mirror of
    /// libav's and does not line up with the linked library: it reports VAAPI as 45, libav as 44.
    pub fn hw_pixel_format(self) -> ffmpeg::ffi::AVPixelFormat {
        match self {
            Encoder::Vaapi => ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_VAAPI,
            Encoder::Cuda => ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_CUDA,
            Encoder::Qsv => ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_QSV,
            Encoder::Amf => ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_AMF_SURFACE,
            Encoder::Software => ffmpeg::ffi::AVPixelFormat::AV_PIX_FMT_YUV420P,
        }
    }

    /// Argument for `av_hwdevice_ctx_create`. `None` means "pick the default device".
    pub fn device_hint(self, discovered: &Option<PathBuf>) -> String {
        match self {
            Encoder::Cuda => "0".to_string(),
            Encoder::Qsv => match discovered {
                Some(p) => p.to_string_lossy().into_owned(),
                None => String::new(),
            },
            Encoder::Vaapi | Encoder::Amf => match discovered {
                Some(p) => p.to_string_lossy().into_owned(),
                None => String::new(),
            },
            Encoder::Software => String::new(),
        }
    }
}

/// One candidate path plus how to check that the device behind it exists.
struct Candidate {
    encoder: Encoder,
    /// PCI vendor id, or `None` when the check is device file based.
    vendor: Option<u32>,
    /// A device file that has to exist.
    requires: &'static str,
}

const VENDOR_INTEL: u32 = 0x8086;
const VENDOR_AMD: u32 = 0x1002;
const VENDOR_NVIDIA: u32 = 0x10de;

fn candidates() -> Vec<Candidate> {
    vec![
        Candidate {
            encoder: Encoder::Cuda,
            vendor: Some(VENDOR_NVIDIA),
            requires: "/dev/nvidiactl",
        },
        Candidate {
            encoder: Encoder::Qsv,
            vendor: Some(VENDOR_INTEL),
            requires: "",
        },
        Candidate {
            encoder: Encoder::Amf,
            vendor: Some(VENDOR_AMD),
            requires: "/dev/kfd",
        },
        Candidate {
            encoder: Encoder::Vaapi,
            vendor: None,
            requires: "",
        },
    ]
}

/// With `allow_hardware` false, or when every hardware path fails its live test, the software encoder.
pub fn detect(allow_hardware: bool) -> AppResult<Hardware> {
    ffmpeg::init().map_err(|e| crate::error::AppError::internal(format!("ffmpeg: {e}")))?;
    quiet_libav();
    let render_node = find_render_node();
    let mut rejected = Vec::new();

    if allow_hardware {
        for candidate in candidates() {
            let Some(device) = device_for(&candidate, render_node.as_deref()) else {
                continue;
            };
            // Both codecs are checked, because a build can have one without the other.
            let codec = [Codec::H264, Codec::H265]
                .into_iter()
                .find(|c| ffmpeg::codec::encoder::find_by_name(candidate.encoder.encoder_name(*c)).is_some());
            let Some(codec) = codec else {
                rejected.push((
                    candidate.encoder.label().into(),
                    "this ffmpeg has no encoder for it".into(),
                ));
                continue;
            };
            match verify(
                candidate.encoder,
                codec,
                &candidate.encoder.device_hint(&Some(device.clone())),
            ) {
                Ok(()) => {
                    tracing::info!(
                        encoder = candidate.encoder.label(),
                        device = %device.display(),
                        "hardware encoder verified"
                    );
                    let encoder = candidate.encoder;
                    return Ok(Hardware {
                        detail: describe(encoder, &device),
                        device: device.display().to_string(),
                        encoder,
                        rejected,
                    });
                }
                Err(why) => {
                    tracing::warn!(encoder = candidate.encoder.label(), %why, "hardware encoder rejected");
                    rejected.push((candidate.encoder.label().to_string(), why));
                }
            }
        }
    }

    Ok(Hardware {
        encoder: Encoder::Software,
        device: "cpu".to_string(),
        detail: if allow_hardware {
            "no hardware encoder passed the live test".to_string()
        } else {
            "hardware encoding is switched off".to_string()
        },
        rejected,
    })
}

/// libav writes driver probing notes straight to stderr, which fills the log with one MFX
/// session error per rejected candidate.
fn quiet_libav() {
    ffmpeg::util::log::set_level(ffmpeg::util::log::Level::Quiet);
}

fn describe(encoder: Encoder, device: &Path) -> String {
    let name = match encoder {
        Encoder::Cuda => "NVIDIA NVENC",
        Encoder::Qsv => "Intel Quick Sync",
        Encoder::Amf => "AMD Advanced Media Framework",
        Encoder::Vaapi => "Video Acceleration API",
        Encoder::Software => "CPU",
    };
    format!("{name} on {}", device.display())
}

/// Match a candidate to a device on this machine.
fn device_for(candidate: &Candidate, render_node: Option<&Path>) -> Option<PathBuf> {
    if !candidate.requires.is_empty() && !Path::new(candidate.requires).exists() {
        return None;
    }
    match candidate.vendor {
        // VAAPI runs on Intel and AMD, so the vendor check is only for QSV and AMF.
        None => render_node.map(Path::to_path_buf),
        Some(vendor) => {
            let has_vendor = gpu_vendors().contains(&vendor);
            if !has_vendor {
                return None;
            }
            if candidate.encoder == Encoder::Cuda {
                return Some(PathBuf::from("/dev/nvidia0"));
            }
            render_node.map(Path::to_path_buf)
        }
    }
}

/// PCI vendor ids of every display class device, read from sysfs.
fn gpu_vendors() -> Vec<u32> {
    let mut out = Vec::new();
    for entry in std::fs::read_dir("/sys/class/drm").into_iter().flatten().flatten() {
        let card = entry.file_name().to_string_lossy().into_owned();
        if !card.starts_with("card") || card.contains('-') {
            continue;
        }
        let path = format!("/sys/class/drm/{card}/device/vendor");
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(id) = u32::from_str_radix(text.trim().trim_start_matches("0x"), 16) {
                out.push(id);
            }
        }
    }
    out.sort_unstable();
    out.dedup();
    out
}

/// The render node a VAAPI or QSV device should be opened on.
pub fn find_render_node() -> Option<PathBuf> {
    let mut best: Option<PathBuf> = None;
    for entry in std::fs::read_dir("/dev/dri").ok()?.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !name.starts_with("renderD") {
            continue;
        }
        let path = PathBuf::from("/dev/dri").join(&name);
        // The lowest numbered node is the primary GPU on the systems this runs on.
        let better = match &best {
            None => true,
            Some(current) => node_number(&name) < node_number(&current.to_string_lossy()),
        };
        if better {
            best = Some(path);
        }
    }
    best
}

fn node_number(name: &str) -> u32 {
    name.trim_start_matches("renderD").parse().unwrap_or(u32::MAX)
}

/// Prove an encoder really works: open it, feed one frame, take a packet out.
fn verify(encoder: Encoder, codec: Codec, hint: &str) -> Result<(), String> {
    let (w, h) = (64u32, 64u32);
    let mut enc = super::transcode::VideoEncoder::open(encoder, codec, w, h, Rational::new(1, 24), hint)
        .map_err(|e| e.to_string())?;
    let frame = super::transcode::synthetic_frame(w, h, enc.pixel_format());
    enc.send(&frame).map_err(|e| format!("rejecting a frame: {e}"))?;
    enc.flush_eof().map_err(|e| format!("cannot flush: {e}"))?;
    let packets = enc.take_packets(4);
    if packets.is_empty() {
        return Err("encoded a frame but produced no packets".to_string());
    }
    // Only dts is checked, so a packet with a good dts and no pts still passes here. Quick Sync
    // on Alder Lake hands back packets stamped around 1e15, uninitialised memory the muxer refuses.
    for p in &packets {
        let Some(dts) = p.dts() else {
            return Err("produced a packet with no timestamp".to_string());
        };
        if dts.abs() > 1_000_000_000_000 {
            return Err(format!("produced a packet with an unusable timestamp ({dts})"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoder_names_follow_the_codec() {
        assert_eq!(Encoder::Vaapi.encoder_name(Codec::H264), "h264_vaapi");
        assert_eq!(Encoder::Vaapi.encoder_name(Codec::H265), "hevc_vaapi");
        assert_eq!(Encoder::Cuda.encoder_name(Codec::H265), "hevc_nvenc");
        assert_eq!(Encoder::Qsv.encoder_name(Codec::H264), "h264_qsv");
        assert_eq!(Encoder::Amf.encoder_name(Codec::H265), "hevc_amf");
        assert_eq!(Encoder::Software.encoder_name(Codec::H264), "libx264");
        assert_eq!(Encoder::Software.encoder_name(Codec::H265), "libx265");
    }

    #[test]
    fn only_software_is_not_hardware() {
        for e in [Encoder::Vaapi, Encoder::Cuda, Encoder::Qsv, Encoder::Amf] {
            assert!(e.is_hardware(), "{e:?} should count as hardware");
        }
        assert!(!Encoder::Software.is_hardware());
    }

    #[test]
    fn every_hardware_encoder_has_a_device_type_and_format() {
        for e in [Encoder::Vaapi, Encoder::Cuda, Encoder::Qsv, Encoder::Amf] {
            assert!(e.hw_device_type().is_some(), "{e:?} needs a device type");
            assert!((e.hw_pixel_format() as i32) >= 0, "{e:?} has no surface format");
        }
        assert!(Encoder::Software.hw_device_type().is_none());
    }

    #[test]
    fn surface_formats_are_the_raw_libav_values() {
        // A libav bump that renumbers these fails here, in the test, not silently later.
        assert_eq!(Encoder::Vaapi.hw_pixel_format() as i32, 44);
        assert_eq!(Encoder::Cuda.hw_pixel_format() as i32, 117);
        assert_eq!(Encoder::Qsv.hw_pixel_format() as i32, 114);
        assert_eq!(Encoder::Amf.hw_pixel_format() as i32, 249);
    }

    #[test]
    fn cuda_uses_a_device_index_not_a_path() {
        assert_eq!(Encoder::Cuda.device_hint(&None), "0");
        assert_eq!(Encoder::Cuda.device_hint(&Some(PathBuf::from("/dev/dri/renderD128"))), "0");
    }

    #[test]
    fn node_numbers_parse() {
        assert_eq!(node_number("renderD128"), 128);
        assert_eq!(node_number("renderD12"), 12);
        assert_eq!(node_number("junk"), u32::MAX);
    }

    #[test]
    fn software_fallback_is_always_reachable() {
        let h = detect(false).unwrap();
        assert_eq!(h.encoder, Encoder::Software);
        assert!(!h.is_hardware());
        assert!(h.summary().contains("software"));
    }

    #[test]
    fn detection_reports_a_working_encoder() {
        // Software on a machine with no GPU, VAAPI on one with a GPU. Either must be usable.
        let h = detect(true).unwrap();
        let name = h.encoder.encoder_name(Codec::H264);
        assert!(
            ffmpeg::codec::encoder::find_by_name(name).is_some(),
            "detected encoder {name} does not exist"
        );
        assert!(!h.detail.is_empty());
    }
}
