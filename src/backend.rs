use clap::ValueEnum;

use crate::codec::Codec;

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lowercase")]
pub enum Backend {
    /// CPU encoders (libx265 / libsvtav1). Best compression, slowest.
    #[default]
    Software,
    /// NVIDIA NVENC (hevc_nvenc / av1_nvenc). Fast, larger files than software.
    Nvenc,
    /// Intel Quick Sync Video (hevc_qsv / av1_qsv). Intel iGPU only.
    Qsv,
    /// VAAPI (Linux). Works with AMD and Intel iGPUs.
    Vaapi,
}

/// Concrete ffmpeg invocation parameters for a (codec, backend) pair.
pub struct EncoderConfig {
    pub encoder: &'static str,
    /// Flag name for the quality value (e.g. "-crf", "-cq", "-qp").
    pub quality_flag: &'static str,
    /// Default value if the user didn't specify one.
    pub default_quality: u8,
    /// Default preset, or None when the backend has no meaningful preset.
    pub default_preset: Option<&'static str>,
    /// Args inserted before `-i <input>` (e.g. VAAPI device init).
    pub preamble: Vec<String>,
    /// Extra args appended after `-c:v <encoder>` (e.g. `-rc vbr` for NVENC CQ).
    pub extra_post: Vec<String>,
    /// Video filter chain to apply (e.g. `format=nv12,hwupload` for VAAPI).
    pub video_filter: Option<String>,
}

impl Backend {
    pub fn label(self) -> &'static str {
        match self {
            Backend::Software => "software",
            Backend::Nvenc => "nvenc",
            Backend::Qsv => "qsv",
            Backend::Vaapi => "vaapi",
        }
    }

    pub fn config(self, codec: Codec) -> EncoderConfig {
        match (self, codec) {
            (Backend::Software, Codec::X265) => EncoderConfig {
                encoder: "libx265",
                quality_flag: "-crf",
                default_quality: 23,
                default_preset: Some("medium"),
                preamble: vec![],
                extra_post: vec![],
                video_filter: None,
            },
            (Backend::Software, Codec::Av1) => EncoderConfig {
                encoder: "libsvtav1",
                quality_flag: "-crf",
                default_quality: 30,
                default_preset: Some("8"),
                preamble: vec![],
                extra_post: vec![],
                video_filter: None,
            },

            (Backend::Nvenc, Codec::X265) => EncoderConfig {
                encoder: "hevc_nvenc",
                quality_flag: "-cq",
                default_quality: 23,
                default_preset: Some("p4"),
                preamble: vec![],
                extra_post: vec!["-rc".into(), "vbr".into()],
                video_filter: None,
            },
            (Backend::Nvenc, Codec::Av1) => EncoderConfig {
                encoder: "av1_nvenc",
                quality_flag: "-cq",
                default_quality: 28,
                default_preset: Some("p4"),
                preamble: vec![],
                extra_post: vec!["-rc".into(), "vbr".into()],
                video_filter: None,
            },

            (Backend::Qsv, Codec::X265) => EncoderConfig {
                encoder: "hevc_qsv",
                quality_flag: "-global_quality",
                default_quality: 23,
                default_preset: Some("medium"),
                preamble: vec![],
                extra_post: vec![],
                video_filter: None,
            },
            (Backend::Qsv, Codec::Av1) => EncoderConfig {
                encoder: "av1_qsv",
                quality_flag: "-global_quality",
                default_quality: 28,
                default_preset: Some("medium"),
                preamble: vec![],
                extra_post: vec![],
                video_filter: None,
            },

            (Backend::Vaapi, Codec::X265) => EncoderConfig {
                encoder: "hevc_vaapi",
                quality_flag: "-qp",
                default_quality: 23,
                default_preset: None,
                preamble: vec!["-vaapi_device".into(), "/dev/dri/renderD128".into()],
                extra_post: vec![],
                video_filter: Some("format=nv12,hwupload".into()),
            },
            (Backend::Vaapi, Codec::Av1) => EncoderConfig {
                encoder: "av1_vaapi",
                quality_flag: "-qp",
                default_quality: 28,
                default_preset: None,
                preamble: vec!["-vaapi_device".into(), "/dev/dri/renderD128".into()],
                extra_post: vec![],
                video_filter: Some("format=nv12,hwupload".into()),
            },
        }
    }
}
