use anyhow::{Context, Result, bail};
use serde::Deserialize;
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
