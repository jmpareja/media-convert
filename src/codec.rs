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
