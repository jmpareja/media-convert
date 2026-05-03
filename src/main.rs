mod cli;

use anyhow::{Context, Result};
use clap::Parser;
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};

use cli::Cli;
use media_convert::codec::Codec;
use media_convert::convert::{self, EncodeOptions};
use media_convert::{probe, scan};

fn main() -> Result<()> {
    let args = Cli::parse();

    if !args.source.is_dir() {
        anyhow::bail!("source is not a directory: {}", args.source.display());
    }

    let videos = scan::find_videos(&args.source);
    if videos.is_empty() {
        println!("no video files found under {}", args.source.display());
        return Ok(());
    }
    println!(
        "found {} candidate file(s); target codec: {}",
        videos.len(),
        args.codec.label()
    );

    let cfg = args.backend.config(args.codec);
    let preset_owned = args.preset.clone();
    let opts = EncodeOptions {
        codec: args.codec,
        backend: args.backend,
        quality: args.quality.unwrap_or(cfg.default_quality),
        preset: preset_owned.as_deref().or(cfg.default_preset),
    };

    let mut converted = 0u64;
    let mut skipped = 0u64;
    let mut failed = 0u64;

    for (idx, input) in videos.iter().enumerate() {
        let rel = input
            .strip_prefix(&args.source)
            .unwrap_or(input)
            .to_path_buf();
        let output = output_path(&args.output, &rel);

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

        let duration = probe::video_info(input).ok().and_then(|i| i.duration_secs);

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

fn output_path(output_root: &Path, rel: &Path) -> PathBuf {
    let mut out = output_root.join(rel);
    out.set_extension("mkv");
    out
}
