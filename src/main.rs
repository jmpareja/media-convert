mod cli;

use anyhow::{Context, Result, bail};
use clap::Parser;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use cli::Cli;
use media_convert::codec::Codec;
use media_convert::container::Container;
use media_convert::convert::{self, EncodeOptions};
use media_convert::{probe, scan};

fn main() -> Result<()> {
    let args = Cli::parse();
    let single_file_mode = validate(&args)?;

    let videos = scan::find_videos(&args.source, !args.no_recurse);
    if videos.is_empty() {
        println!("no video files found under {}", args.source.display());
        return Ok(());
    }
    println!(
        "found {} candidate file(s); target codec: {}",
        videos.len(),
        args.codec.label()
    );

    let rel_root: PathBuf = if single_file_mode {
        args.source
            .parent()
            .map(Path::to_path_buf)
            .unwrap_or_default()
    } else {
        args.source.clone()
    };

    let cfg = args.backend.config(args.codec);
    let preset_owned = args.preset.clone();
    let quality = args.quality.unwrap_or(cfg.default_quality);
    let preset = preset_owned.as_deref().or(cfg.default_preset);

    let mut converted = 0u64;
    let mut skipped = 0u64;
    let mut failed = 0u64;

    for (idx, input) in videos.iter().enumerate() {
        let rel = input
            .strip_prefix(&rel_root)
            .unwrap_or(input)
            .to_path_buf();
        let output = if single_file_mode && output_is_filepath(&args.output) {
            args.output.clone()
        } else {
            output_path(&args.output, &rel, args.container)
        };

        let prefix = format!("[{}/{}]", idx + 1, videos.len());

        if output.exists() {
            println!("{prefix} skip (output exists): {}", rel.display());
            skipped += 1;
            continue;
        }

        match decide(input, args.codec, args.force) {
            Ok(Decision::Skip(reason)) => {
                println!("{prefix} skip ({reason}): {}", rel.display());
                skipped += 1;
                continue;
            }
            Ok(Decision::Encode) => {}
            Err(e) => {
                eprintln!("{prefix} probe failed for {}: {e:#}", rel.display());
                failed += 1;
                continue;
            }
        }

        if args.dry_run {
            println!("{prefix} would encode: {} -> {}", rel.display(), output.display());
            continue;
        }

        let info = probe::video_info(input).ok();
        let duration = info.as_ref().and_then(|i| i.duration_secs);
        let source_subtitle_count = info.as_ref().map(|i| i.subtitle_count).unwrap_or(0);

        let subtitles: Vec<convert::SubtitleInput> = if args.embed_subtitles {
            scan::discover_subtitles(input)
                .into_iter()
                .map(|s| convert::SubtitleInput {
                    path: s.path,
                    language: s.language,
                })
                .collect()
        } else {
            Vec::new()
        };
        if !subtitles.is_empty() {
            println!(
                "{prefix} found {} sidecar subtitle file(s)",
                subtitles.len()
            );
        }

        let opts = EncodeOptions {
            codec: args.codec,
            backend: args.backend,
            container: args.container,
            quality,
            preset,
            subtitles: &subtitles,
            source_subtitle_count,
        };

        let rel_str = rel.display().to_string();
        let tty = io::stdout().is_terminal();
        let prefix_for_cb = prefix.clone();
        let rel_for_cb = rel_str.clone();
        if tty {
            print!("{prefix}   0% {rel_str}");
            let _ = io::stdout().flush();
        } else {
            println!("{prefix} encoding: {} -> {}", rel_str, output.display());
        }
        let on_progress = move |frac: Option<f64>| {
            if !tty {
                return;
            }
            let pct = frac.map(|f| (f * 100.0) as u32).unwrap_or(0);
            let mut out = io::stdout().lock();
            let _ = write!(out, "\r{prefix_for_cb} {pct:>3}% {rel_for_cb}\x1b[K");
            let _ = out.flush();
        };
        let result = convert::encode_with_progress(input, &output, &opts, duration, on_progress);
        if tty {
            println!();
        }
        if let Err(e) = result {
            eprintln!("{prefix} FAILED: {e:#}");
            let _ = std::fs::remove_file(&output);
            failed += 1;
        } else {
            converted += 1;
        }
    }

    println!("\ndone: {converted} converted, {skipped} skipped, {failed} failed");
    Ok(())
}

enum Decision {
    Encode,
    Skip(String),
}

fn decide(input: &Path, target: Codec, force: bool) -> Result<Decision> {
    if force {
        return Ok(Decision::Encode);
    }
    let source_codec = probe::video_codec(input).context("ffprobe")?;
    if target.matches_source(&source_codec) {
        Ok(Decision::Skip(format!("already {}", target.label())))
    } else {
        Ok(Decision::Encode)
    }
}

fn validate(args: &Cli) -> Result<bool> {
    require_on_path("ffmpeg")?;
    require_on_path("ffprobe")?;

    if let Some(q) = args.quality
        && q > 63
    {
        bail!("--quality must be 0-63 (got {q}); see backend docs for the meaningful range");
    }

    let single_file_mode = if args.source.is_file() {
        if !scan::is_video(&args.source) {
            bail!(
                "source file is not a recognised video format: {}",
                args.source.display()
            );
        }
        true
    } else if args.source.is_dir() {
        false
    } else {
        bail!("source does not exist: {}", args.source.display());
    };

    if args.output.exists() && !args.output.is_dir() && !single_file_mode {
        bail!(
            "--output exists and is not a directory: {}",
            args.output.display()
        );
    }

    Ok(single_file_mode)
}

fn require_on_path(prog: &str) -> Result<()> {
    Command::new(prog)
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|_| ())
        .with_context(|| format!("`{prog}` not found on PATH; install ffmpeg first"))
}

fn output_path(output_root: &Path, rel: &Path, container: Container) -> PathBuf {
    let mut out = output_root.join(rel);
    out.set_extension(container.extension());
    out
}

fn output_is_filepath(p: &Path) -> bool {
    if p.is_dir() {
        return false;
    }
    if p.is_file() {
        return true;
    }
    if let Some(s) = p.to_str()
        && (s.ends_with('/') || s.ends_with(std::path::MAIN_SEPARATOR))
    {
        return false;
    }
    p.extension().is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn nonexistent_with_extension_is_filepath() {
        assert!(output_is_filepath(Path::new("/tmp/no/such/out.mkv")));
        assert!(output_is_filepath(Path::new("relative.mp4")));
    }

    #[test]
    fn nonexistent_without_extension_is_directory() {
        assert!(!output_is_filepath(Path::new("/tmp/no/such/out")));
        assert!(!output_is_filepath(Path::new("converted")));
    }

    #[test]
    fn trailing_slash_means_directory_even_with_dot() {
        assert!(!output_is_filepath(Path::new("/tmp/foo/")));
    }

    #[test]
    fn existing_file_is_filepath() {
        let dir = tempdir().unwrap();
        let f = dir.path().join("existing.mkv");
        fs::write(&f, b"").unwrap();
        assert!(output_is_filepath(&f));
    }

    #[test]
    fn existing_directory_is_not_filepath() {
        let dir = tempdir().unwrap();
        assert!(!output_is_filepath(dir.path()));
    }

    #[test]
    fn output_path_uses_mkv_extension_for_mkv_container() {
        let out = output_path(
            Path::new("/out"),
            Path::new("show/episode.mp4"),
            Container::Mkv,
        );
        assert_eq!(out, PathBuf::from("/out/show/episode.mkv"));
    }

    #[test]
    fn output_path_uses_mp4_extension_for_mp4_container() {
        let out = output_path(
            Path::new("/out"),
            Path::new("show/episode.mkv"),
            Container::Mp4,
        );
        assert_eq!(out, PathBuf::from("/out/show/episode.mp4"));
    }
}
