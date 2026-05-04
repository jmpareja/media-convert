use clap::Parser;
use std::path::PathBuf;

use media_convert::backend::Backend;
use media_convert::codec::Codec;
use media_convert::container::Container;

#[derive(Parser, Debug)]
#[command(version, about = "Re-encode a media library to a target codec")]
pub struct Cli {
    /// Source directory to scan for video files
    #[arg(short, long)]
    pub source: PathBuf,

    /// Output directory; converted files mirror the source tree using the
    /// container extension chosen by --container. If omitted, defaults to a
    /// sibling of the source directory named "<source>-converted".
    #[arg(short, long)]
    pub output: Option<PathBuf>,

    /// Target video codec
    #[arg(short, long, value_enum, default_value_t = Codec::X265)]
    pub codec: Codec,

    /// Output container. `mkv` (default) preserves all tracks and attachments.
    /// `mp4` is broadly compatible but transcodes subtitles to mov_text and
    /// drops image-based subs / attachments / data streams.
    #[arg(long, value_enum, default_value_t = Container::Mkv)]
    pub container: Container,

    /// Encoder backend. `software` is libx265/libsvtav1 (best compression).
    /// `nvenc` (NVIDIA), `qsv` (Intel iGPU) and `vaapi` (Linux AMD/Intel) are
    /// hardware-accelerated and much faster but produce larger files at
    /// equivalent visual quality.
    #[arg(short, long, value_enum, default_value_t = Backend::Software)]
    pub backend: Backend,

    /// Quality value; flag and scale depend on backend
    /// (software/nvenc/qsv: lower=better, ~18-30; vaapi -qp: ~20-30).
    /// If omitted, a backend-aware default is used.
    #[arg(long)]
    pub quality: Option<u8>,

    /// Encoder preset. Defaults are backend/codec-specific.
    /// libx265: ultrafast..placebo. libsvtav1: 0-13 (lower=slower/better).
    /// nvenc: p1..p7. qsv: veryfast..veryslow. vaapi: ignored.
    #[arg(long)]
    pub preset: Option<String>,

    /// Scan and report what would be converted, without running ffmpeg
    #[arg(long)]
    pub dry_run: bool,

    /// Re-encode even if the source video stream is already in the target codec
    #[arg(long)]
    pub force: bool,

    /// Do not descend into subdirectories when --source is a directory
    #[arg(long)]
    pub no_recurse: bool,

    /// Auto-discover sidecar .srt files (e.g. movie.srt, movie.en.srt next to
    /// movie.mp4) and mux them into the output as additional subtitle tracks.
    #[arg(long)]
    pub embed_subtitles: bool,

    /// Merge sidecar subtitles into each source without re-encoding the
    /// video. The video and audio streams are stream-copied and any
    /// discovered sidecar .srt files are muxed in as new subtitle tracks
    /// (implies --embed-subtitles). Files with no sidecar SRTs are skipped
    /// since there'd be nothing to merge.
    #[arg(long)]
    pub merge_subtitles: bool,
}
