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
}

#[derive(Debug, Deserialize)]
struct Format {
    duration: Option<String>,
}

pub struct VideoInfo {
    pub codec: String,
    pub duration_secs: Option<f64>,
}

pub fn video_info(path: &Path) -> Result<VideoInfo> {
    let output = Command::new("ffprobe")
        .args([
            "-v",
            "error",
            "-select_streams",
            "v:0",
            "-show_entries",
            "stream=codec_name:format=duration",
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

    let codec = parsed
        .streams
        .into_iter()
        .next()
        .and_then(|s| s.codec_name)
        .with_context(|| format!("no video stream found in {}", path.display()))?;

    let duration_secs = parsed
        .format
        .and_then(|f| f.duration)
        .and_then(|s| s.parse::<f64>().ok())
        .filter(|d| d.is_finite() && *d > 0.0);

    Ok(VideoInfo {
        codec,
        duration_secs,
    })
}

pub fn video_codec(path: &Path) -> Result<String> {
    Ok(video_info(path)?.codec)
}
