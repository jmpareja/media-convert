use clap::ValueEnum;

/// Top-level transcoding profile. `Standard` uses the configurable
/// codec/backend/container/quality/preset/upscale knobs (the historical
/// behaviour); `Web` is a fixed browser-friendly configuration (libx264
/// high@4.0, AAC stereo 192k, MP4 with `+faststart`) that writes a sibling
/// `<stem>.web.mp4` next to the source; `Hls` produces an adaptive
/// HTTP Live Streaming bundle (`<stem>.hls/` with `master.m3u8` plus
/// `360p`, `720p` and `1080p` variant playlists + TS segments). In `Web`
/// and `Hls` modes the codec/backend/container/quality/preset/upscale
/// fields on `EncodeOptions` are ignored.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lowercase")]
pub enum EncodingProfile {
    #[default]
    Standard,
    Web,
    Hls,
}

impl EncodingProfile {
    pub fn label(self) -> &'static str {
        match self {
            EncodingProfile::Standard => "standard",
            EncodingProfile::Web => "web",
            EncodingProfile::Hls => "hls",
        }
    }
}

/// One rendition in the HLS ladder. The fixed list is exposed via
/// [`hls_variants`].
pub struct HlsVariant {
    /// Folder name in the output bundle (also used as the variant id
    /// in ffmpeg's `var_stream_map`).
    pub name: &'static str,
    /// Target output height in pixels; ffmpeg scales width to preserve
    /// aspect ratio (`-2` keeps the width an even number).
    pub height: u32,
    /// Hint width at 16:9 — used only when building the master playlist's
    /// `RESOLUTION` attribute (ffmpeg writes the master; we don't
    /// post-process it).
    pub width_hint: u32,
    /// Target video bitrate (e.g. `"2800k"`).
    pub video_bitrate: &'static str,
    /// Maximum video bitrate cap (typically 10-20% above the target).
    pub video_maxrate: &'static str,
    /// VBV buffer size for the rate controller.
    pub video_bufsize: &'static str,
    /// x264 profile (`baseline`/`main`/`high`).
    pub video_profile: &'static str,
    /// H.264 level (encoded as a decimal string, e.g. `"3.1"`).
    pub video_level: &'static str,
    /// Audio bitrate (e.g. `"128k"`).
    pub audio_bitrate: &'static str,
}

/// HLS rendition ladder. Three steps wide enough to span phone-tier
/// connections through TV-grade Wi-Fi. Settings follow Apple's HLS
/// authoring guidelines for VOD; the master playlist's `RESOLUTION`
/// attribute uses 16:9 widths because the player only needs the
/// declaration to pick a rendition (the actual encoded width tracks
/// the source's aspect ratio).
pub fn hls_variants() -> &'static [HlsVariant] {
    &[
        HlsVariant {
            name: "360p",
            height: 360,
            width_hint: 640,
            video_bitrate: "800k",
            video_maxrate: "900k",
            video_bufsize: "1600k",
            video_profile: "main",
            video_level: "3.0",
            audio_bitrate: "96k",
        },
        HlsVariant {
            name: "720p",
            height: 720,
            width_hint: 1280,
            video_bitrate: "2800k",
            video_maxrate: "3200k",
            video_bufsize: "5600k",
            video_profile: "main",
            video_level: "3.1",
            audio_bitrate: "128k",
        },
        HlsVariant {
            name: "1080p",
            height: 1080,
            width_hint: 1920,
            video_bitrate: "5000k",
            video_maxrate: "5500k",
            video_bufsize: "10000k",
            video_profile: "high",
            video_level: "4.0",
            audio_bitrate: "128k",
        },
    ]
}

/// HLS segment duration target in seconds. 6s matches Apple's authoring
/// recommendation for VOD — short enough for snappy starts and adaptive
/// bitrate switching, long enough that segment overhead is small.
pub const HLS_SEGMENT_SECONDS: u32 = 6;

/// Fixed ffmpeg fragment for the Web profile. Kept in one place so the
/// settings can be audited against the spec without hunting through
/// `build_ffmpeg_command`.
///
/// Settings:
/// * Video: libx264, preset slow, CRF 20, profile high, level 4.0, yuv420p
/// * Audio: AAC, stereo downmix (`-ac 2`), 192 kbps
/// * Container flag: `-movflags +faststart` (moov atom at file start, so
///   the player can begin playback before the file is fully buffered)
pub fn web_encoder_args() -> &'static [&'static str] {
    &[
        "-c:v",
        "libx264",
        "-preset",
        "slow",
        "-crf",
        "20",
        "-profile:v",
        "high",
        "-level",
        "4.0",
        "-pix_fmt",
        "yuv420p",
        "-c:a",
        "aac",
        "-ac",
        "2",
        "-b:a",
        "192k",
        "-movflags",
        "+faststart",
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pair(args: &[&str], k: &str) -> Option<String> {
        args.iter()
            .position(|a| *a == k)
            .map(|i| args[i + 1].into())
    }

    #[test]
    fn web_args_pin_libx264_high_at_level_40() {
        let a = web_encoder_args();
        assert_eq!(pair(a, "-c:v").as_deref(), Some("libx264"));
        assert_eq!(pair(a, "-profile:v").as_deref(), Some("high"));
        assert_eq!(pair(a, "-level").as_deref(), Some("4.0"));
        assert_eq!(pair(a, "-pix_fmt").as_deref(), Some("yuv420p"));
    }

    #[test]
    fn web_args_pin_crf_20_preset_slow() {
        let a = web_encoder_args();
        assert_eq!(pair(a, "-crf").as_deref(), Some("20"));
        assert_eq!(pair(a, "-preset").as_deref(), Some("slow"));
    }

    #[test]
    fn web_args_pin_aac_stereo_192k() {
        let a = web_encoder_args();
        assert_eq!(pair(a, "-c:a").as_deref(), Some("aac"));
        assert_eq!(pair(a, "-ac").as_deref(), Some("2"));
        assert_eq!(pair(a, "-b:a").as_deref(), Some("192k"));
    }

    #[test]
    fn web_args_emit_movflags_faststart() {
        // Without +faststart the moov atom lands at the end and browsers
        // have to download the whole file before starting playback.
        let a = web_encoder_args();
        assert_eq!(pair(a, "-movflags").as_deref(), Some("+faststart"));
    }

    #[test]
    fn default_is_standard() {
        assert_eq!(EncodingProfile::default(), EncodingProfile::Standard);
    }

    #[test]
    fn label_matches_value_name() {
        assert_eq!(EncodingProfile::Standard.label(), "standard");
        assert_eq!(EncodingProfile::Web.label(), "web");
        assert_eq!(EncodingProfile::Hls.label(), "hls");
    }

    #[test]
    fn hls_variants_ladder_is_360_720_1080_in_order() {
        let v: Vec<&str> = hls_variants().iter().map(|h| h.name).collect();
        assert_eq!(v, ["360p", "720p", "1080p"]);
        let heights: Vec<u32> = hls_variants().iter().map(|h| h.height).collect();
        assert_eq!(heights, [360, 720, 1080]);
    }

    #[test]
    fn hls_variant_bitrates_monotonically_increase() {
        // A misordered ladder would confuse adaptive players. Compare
        // the leading-digit run of each bitrate string (they're all `k`
        // suffixed kbps values).
        fn kbps(s: &str) -> u32 {
            s.strip_suffix('k').unwrap().parse().unwrap()
        }
        let mut last = 0;
        for v in hls_variants() {
            let b = kbps(v.video_bitrate);
            assert!(
                b > last,
                "variant {} bitrate {b} not larger than previous {last}",
                v.name
            );
            last = b;
        }
    }
}
