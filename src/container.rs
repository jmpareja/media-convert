use clap::ValueEnum;

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lowercase")]
pub enum Container {
    /// Matroska. Holds anything; best for archival and rich subtitle/track support.
    #[default]
    Mkv,
    /// MP4 (ISO BMFF). Universal playback; subtitles transcoded to mov_text,
    /// and image-based subs / attachments are dropped.
    Mp4,
}

impl Container {
    pub fn extension(self) -> &'static str {
        match self {
            Container::Mkv => "mkv",
            Container::Mp4 => "mp4",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Container::Mkv => "mkv",
            Container::Mp4 => "mp4",
        }
    }
}
