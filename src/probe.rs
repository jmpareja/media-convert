use anyhow::{Context, Result, bail};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::Path;
use std::process::Command;

#[derive(Debug, Deserialize)]
struct ProbeOutput {
    streams: Vec<Stream>,
    format: Option<Format>,
}

#[derive(Debug, Deserialize)]
struct Stream {
    index: Option<u32>,
    codec_name: Option<String>,
    codec_type: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    bit_rate: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Format {
    duration: Option<String>,
}

pub struct VideoInfo {
    pub codec: String,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub bit_rate: Option<u64>,
    pub duration_secs: Option<f64>,
    /// Codec name (`codec_name` from ffprobe) for each subtitle stream,
    /// in source-stream order. Used by the encoder to pick a per-stream
    /// output codec — MKV can't `-c:s copy` a `mov_text` track, for
    /// instance, so it gets transcoded to SRT.
    pub subtitle_codecs: Vec<String>,
    /// Source-stream indices the muxer can't accept — streams where
    /// ffprobe reports no `codec_name` (codec_id 0). Common case:
    /// `mp4s` MPEG-4 systems data tracks in MP4 sources, which the
    /// matroska muxer rejects with "Tag mp4s incompatible with output
    /// codec id '0'". The encoder negative-maps these.
    pub unmappable_stream_indices: Vec<usize>,
}

impl VideoInfo {
    pub fn subtitle_count(&self) -> usize {
        self.subtitle_codecs.len()
    }
}

pub fn video_info(path: &Path) -> Result<VideoInfo> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=index,codec_name,codec_type,width,height,bit_rate:format=duration",
            "-of",
            "json",
        ])
        .arg(path)
        .output()
        .context("failed to invoke ffprobe (is it installed and on PATH?)")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("ffprobe failed for {}: {}", path.display(), stderr.trim());
    }

    let parsed: ProbeOutput = serde_json::from_slice(&output.stdout)
        .with_context(|| format!("failed to parse ffprobe JSON for {}", path.display()))?;

    let mut codec: Option<String> = None;
    let mut width: Option<u32> = None;
    let mut height: Option<u32> = None;
    let mut bit_rate: Option<u64> = None;
    let mut subtitle_codecs: Vec<String> = Vec::new();
    let mut unmappable_stream_indices: Vec<usize> = Vec::new();
    for s in parsed.streams {
        let has_codec = s.codec_name.as_deref().is_some_and(|c| !c.is_empty());
        if !has_codec {
            if let Some(idx) = s.index {
                unmappable_stream_indices.push(idx as usize);
            }
            continue;
        }
        match s.codec_type.as_deref() {
            Some("video") if codec.is_none() => {
                codec = s.codec_name;
                width = s.width;
                height = s.height;
                bit_rate = s.bit_rate.and_then(|r| r.parse().ok());
            }
            Some("subtitle") => {
                subtitle_codecs.push(s.codec_name.unwrap_or_default());
            }
            _ => {}
        }
    }
    let codec = codec.with_context(|| format!("no video stream found in {}", path.display()))?;

    let duration_secs = parsed
        .format
        .and_then(|f| f.duration)
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|d| d.is_finite() && *d > 0.0);

    Ok(VideoInfo {
        codec,
        width,
        height,
        bit_rate,
        duration_secs,
        subtitle_codecs,
        unmappable_stream_indices,
    })
}

pub fn video_codec(path: &Path) -> Result<String> {
    Ok(video_info(path)?.codec)
}

#[derive(Debug, Deserialize)]
struct FullProbeOutput {
    streams: Vec<FullStream>,
    format: Option<FullFormat>,
}

#[derive(Debug, Deserialize)]
struct FullStream {
    index: Option<u32>,
    codec_name: Option<String>,
    codec_long_name: Option<String>,
    codec_type: Option<String>,
    profile: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    pix_fmt: Option<String>,
    r_frame_rate: Option<String>,
    avg_frame_rate: Option<String>,
    bit_rate: Option<String>,
    channels: Option<u32>,
    channel_layout: Option<String>,
    sample_rate: Option<String>,
    tags: Option<HashMap<String, String>>,
    disposition: Option<HashMap<String, u32>>,
}

#[derive(Debug, Deserialize)]
struct FullFormat {
    format_name: Option<String>,
    format_long_name: Option<String>,
    duration: Option<String>,
    size: Option<String>,
    bit_rate: Option<String>,
    tags: Option<HashMap<String, String>>,
}

#[derive(Debug, Clone)]
pub struct StreamInfo {
    pub index: u32,
    pub codec_type: String,
    pub codec_name: Option<String>,
    pub codec_long_name: Option<String>,
    pub profile: Option<String>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub pix_fmt: Option<String>,
    pub frame_rate: Option<String>,
    pub bit_rate: Option<u64>,
    pub channels: Option<u32>,
    pub channel_layout: Option<String>,
    pub sample_rate: Option<u32>,
    pub language: Option<String>,
    pub title: Option<String>,
    pub default: bool,
    pub forced: bool,
}

#[derive(Debug, Clone)]
pub struct FileMetadata {
    pub format_name: Option<String>,
    pub format_long_name: Option<String>,
    pub duration_secs: Option<f64>,
    pub size_bytes: Option<u64>,
    pub bit_rate: Option<u64>,
    pub title: Option<String>,
    pub streams: Vec<StreamInfo>,
}

pub fn file_metadata(path: &Path) -> Result<FileMetadata> {
    let output = Command::new("ffprobe")
        .args(["-v", "error", "-show_streams", "-show_format", "-of", "json"])
        .arg(path)
        .output()
        .context("failed to invoke ffprobe (is it installed and on PATH?)")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("ffprobe failed for {}: {}", path.display(), stderr.trim());
    }

    let parsed: FullProbeOutput = serde_json::from_slice(&output.stdout)
        .with_context(|| format!("failed to parse ffprobe JSON for {}", path.display()))?;

    let streams = parsed
        .streams
        .into_iter()
        .map(|s| {
            let tags = s.tags.unwrap_or_default();
            let disp = s.disposition.unwrap_or_default();
            let frame_rate = pick_frame_rate(s.avg_frame_rate, s.r_frame_rate);
            StreamInfo {
                index: s.index.unwrap_or(0),
                codec_type: s.codec_type.unwrap_or_else(|| "unknown".into()),
                codec_name: s.codec_name,
                codec_long_name: s.codec_long_name,
                profile: s.profile,
                width: s.width,
                height: s.height,
                pix_fmt: s.pix_fmt,
                frame_rate,
                bit_rate: s.bit_rate.and_then(|s| s.parse().ok()),
                channels: s.channels,
                channel_layout: s.channel_layout,
                sample_rate: s.sample_rate.and_then(|s| s.parse().ok()),
                language: tags.get("language").cloned(),
                title: tags.get("title").cloned(),
                default: disp.get("default").copied().unwrap_or(0) != 0,
                forced: disp.get("forced").copied().unwrap_or(0) != 0,
            }
        })
        .collect();

    let (format_name, format_long_name, duration_secs, size_bytes, bit_rate, title) =
        match parsed.format {
            Some(f) => {
                let title = f.tags.as_ref().and_then(|t| t.get("title").cloned());
                let duration_secs = f
                    .duration
                    .and_then(|s| s.parse::<f64>().ok())
                    .filter(|d| d.is_finite() && *d > 0.0);
                let size_bytes = f.size.and_then(|s| s.parse().ok());
                let bit_rate = f.bit_rate.and_then(|s| s.parse().ok());
                (
                    f.format_name,
                    f.format_long_name,
                    duration_secs,
                    size_bytes,
                    bit_rate,
                    title,
                )
            }
            None => (None, None, None, None, None, None),
        };

    Ok(FileMetadata {
        format_name,
        format_long_name,
        duration_secs,
        size_bytes,
        bit_rate,
        title,
        streams,
    })
}

fn pick_frame_rate(avg: Option<String>, r: Option<String>) -> Option<String> {
    let usable = |s: &str| !s.is_empty() && s != "0/0";
    avg.filter(|s| usable(s)).or_else(|| r.filter(|s| usable(s)))
}
