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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extension_matches_format_name() {
        assert_eq!(Container::Mkv.extension(), "mkv");
        assert_eq!(Container::Mp4.extension(), "mp4");
    }

    #[test]
    fn label_matches_format_name() {
        assert_eq!(Container::Mkv.label(), "mkv");
        assert_eq!(Container::Mp4.label(), "mp4");
    }

    #[test]
    fn default_is_mkv() {
        assert_eq!(Container::default(), Container::Mkv);
    }
}
