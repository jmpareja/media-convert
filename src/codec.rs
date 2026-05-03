use clap::ValueEnum;

#[derive(Copy, Clone, Debug, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lowercase")]
pub enum Codec {
    X265,
    Av1,
}

impl Codec {
    pub fn matches_source(self, codec_name: &str) -> bool {
        let n = codec_name.to_ascii_lowercase();
        match self {
            Codec::X265 => matches!(n.as_str(), "hevc" | "h265"),
            Codec::Av1 => n == "av1",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Codec::X265 => "x265",
            Codec::Av1 => "av1",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_returns_lowercase_short_name() {
        assert_eq!(Codec::X265.label(), "x265");
        assert_eq!(Codec::Av1.label(), "av1");
    }

    #[test]
    fn x265_matches_hevc_and_h265_case_insensitively() {
        assert!(Codec::X265.matches_source("hevc"));
        assert!(Codec::X265.matches_source("HEVC"));
        assert!(Codec::X265.matches_source("h265"));
        assert!(Codec::X265.matches_source("H265"));
    }

    #[test]
    fn x265_does_not_match_av1_or_h264() {
        assert!(!Codec::X265.matches_source("av1"));
        assert!(!Codec::X265.matches_source("h264"));
        assert!(!Codec::X265.matches_source("vp9"));
    }

    #[test]
    fn av1_matches_only_av1_case_insensitively() {
        assert!(Codec::Av1.matches_source("av1"));
        assert!(Codec::Av1.matches_source("AV1"));
        assert!(!Codec::Av1.matches_source("hevc"));
        assert!(!Codec::Av1.matches_source("h264"));
    }
}
