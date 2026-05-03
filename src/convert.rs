use anyhow::{Context, Result, bail};
use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, ChildStdout, Command, Stdio};

use crate::backend::Backend;
use crate::codec::Codec;

pub struct EncodeOptions<'a> {
    pub codec: Codec,
    pub backend: Backend,
    pub quality: u8,
    /// `None` means: use the backend default (or omit `-preset` entirely if
    /// the backend has no preset concept, e.g. VAAPI).
    pub preset: Option<&'a str>,
}

pub fn spawn_encode(input: &Path, output: &Path, opts: &EncodeOptions) -> Result<Child> {
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }

    let cfg = opts.backend.config(opts.codec);
    let quality = opts.quality.to_string();
    let effective_preset: Option<&str> = opts.preset.or(cfg.default_preset);

    let mut cmd = Command::new("ffmpeg");
    cmd.args([
        "-hide_banner",
        "-loglevel",
        "warning",
        "-nostats",
        "-progress",
        "pipe:1",
        "-n",
    ]);

    for a in &cfg.preamble {
        cmd.arg(a);
    }

    cmd.arg("-i").arg(input);

    cmd.args(["-map", "0"]);

    if let Some(filter) = &cfg.video_filter {
        cmd.args(["-vf", filter]);
    }

    cmd.args(["-c:v", cfg.encoder]);
    cmd.args([cfg.quality_flag, &quality]);

    if let Some(preset) = effective_preset {
        cmd.args(["-preset", preset]);
    }

    for a in &cfg.extra_post {
        cmd.arg(a);
    }

    cmd.args([
        "-c:a", "copy", "-c:s", "copy", "-c:d", "copy", "-c:t", "copy",
    ]);

    cmd.arg(output);

    cmd.stdout(Stdio::piped());

    cmd.spawn()
        .context("failed to invoke ffmpeg (is it installed and on PATH?)")
}

/// Drain ffmpeg's `-progress` stream until EOF, invoking `on_progress` with a
/// fraction in [0.0, 1.0] each time `out_time` advances. If `total_secs` is
/// `None`, the fraction is `None` (caller can show indeterminate progress).
pub fn read_progress<F>(stdout: ChildStdout, total_secs: Option<f64>, mut on_progress: F)
where
    F: FnMut(Option<f64>),
{
    let total_us = total_secs.map(|s| (s * 1_000_000.0).max(1.0));
    let reader = BufReader::new(stdout);
    for line in reader.lines().map_while(|r| r.ok()) {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        match key {
            // ffmpeg historically emitted `out_time_ms` whose value was actually
            // microseconds; newer builds emit `out_time_us`. Treat both the same.
            "out_time_us" | "out_time_ms" => {
                if let Ok(us) = value.parse::<i64>() {
                    let us = us.max(0) as f64;
                    let frac = total_us.map(|t| (us / t).clamp(0.0, 1.0));
                    on_progress(frac);
                }
            }
            "progress" if value == "end" => {
                on_progress(Some(1.0));
            }
            _ => {}
        }
    }
}

pub fn encode(input: &Path, output: &Path, opts: &EncodeOptions) -> Result<()> {
    encode_with_progress(input, output, opts, None, |_| {})
}

pub fn encode_with_progress<F>(
    input: &Path,
    output: &Path,
    opts: &EncodeOptions,
    total_secs: Option<f64>,
    on_progress: F,
) -> Result<()>
where
    F: FnMut(Option<f64>) + Send + 'static,
{
    let mut child = spawn_encode(input, output, opts)?;
    let stdout = child
        .stdout
        .take()
        .context("ffmpeg child has no stdout pipe")?;
    let progress_thread = std::thread::spawn(move || read_progress(stdout, total_secs, on_progress));
    let status = child.wait().context("waiting on ffmpeg")?;
    let _ = progress_thread.join();
    if !status.success() {
        bail!(
            "ffmpeg exited with {} while encoding {}",
            status,
            input.display()
        );
    }
    Ok(())
}
