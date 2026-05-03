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
    codec_name: Option<String>,
    codec_type: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Format {
    duration: Option<String>,
}

pub struct VideoInfo {
    pub codec: String,
    pub duration_secs: Option<f64>,
    pub subtitle_count: usize,
}

pub fn video_info(path: &Path) -> Result<VideoInfo> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-show_entries",
            "stream=codec_name,codec_type:format=duration",
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
    let mut subtitle_count = 0usize;
    for s in parsed.streams {
        match s.codec_type.as_deref() {
            Some("video") if codec.is_none() => codec = s.codec_name,
            Some("subtitle") => subtitle_count += 1,
            _ => {}
        }
    }
    let codec = codec
        .with_context(|| format!("no video stream found in {}", path.display()))?;

    let duration_secs = parsed
        .format
        .and_then(|f| f.duration)
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|d| d.is_finite() && *d > 0.0);

    Ok(VideoInfo {
        codec,
        duration_secs,
        subtitle_count,
    })
}

pub fn video_codec(path: &Path) -> Result<String> {
    Ok(video_info(path)?.codec)
}
