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
use media_convert::inhibit::Inhibitor;
use media_convert::output::{
    default_output, hls_output_path, sum_file_sizes, validate_output, web_output_path,
};
use media_convert::profile::EncodingProfile;
use media_convert::{probe, scan};

fn main() -> Result<()> {
    let args = Cli::parse();
    let single_file_mode = validate(&args)?;
    let web = args.profile == EncodingProfile::Web;
    let hls = args.profile == EncodingProfile::Hls;
    // Bundle profiles (Web, HLS) compute outputs from the source path
    // (sibling-by-default) and ignore --codec/--backend/--container
    // entirely. Each one's default is per-source, so the validate /
    // probe / decide steps differ from a Standard run.
    let bundle = web || hls;

    // Bundle profiles default outputs to siblings next to each source;
    // --output is honoured if given. For Web a `--output` file path is
    // the exact output (single-file mode); for HLS a `--output` is
    // always treated as a directory root because the output is itself
    // a directory.
    let bundle_output_root: Option<PathBuf> = if bundle { args.output.clone() } else { None };
    let output_root: PathBuf = if bundle {
        bundle_output_root.clone().unwrap_or_default()
    } else {
        let root = args
            .output
            .clone()
            .unwrap_or_else(|| default_output(&args.source, args.container));
        if args.output.is_none() {
            println!("--output not given; defaulting to {}", root.display());
        }
        if root.exists() && !root.is_dir() && !single_file_mode {
            bail!("--output exists and is not a directory: {}", root.display());
        }
        root
    };

    let videos = scan::find_videos(&args.source, !args.no_recurse);
    if videos.is_empty() {
        println!("no video files found under {}", args.source.display());
        return Ok(());
    }
    if web {
        println!(
            "found {} candidate file(s); profile: web (sibling .web.mp4)",
            videos.len()
        );
    } else if hls {
        println!(
            "found {} candidate file(s); profile: hls (sibling .hls/ bundle)",
            videos.len()
        );
    } else {
        println!(
            "found {} candidate file(s); target codec: {}",
            videos.len(),
            args.codec.label()
        );
    }

    // Skip the output-dir validation when the user is writing a single
    // file to an explicit filepath (we'd be creating a parent dir, not the
    // filepath itself, and ffmpeg handles that on demand). In bundle
    // modes skip unless the user passed --output pointing at a real
    // directory root — per-source siblings live next to their sources,
    // so there's no shared root to validate. HLS's `--output` is always
    // a directory root (the output is itself a directory).
    let bundle_root_is_dir = (web || hls)
        && bundle_output_root
            .as_deref()
            .is_some_and(|p| hls || !output_is_filepath(p));
    let validate_output_dir =
        bundle_root_is_dir || !(bundle || single_file_mode && output_is_filepath(&output_root));
    if validate_output_dir && !args.dry_run {
        let total = sum_file_sizes(&videos);
        let v = validate_output(&output_root, Some(total))
            .with_context(|| format!("output directory check failed: {}", output_root.display()))?;
        for w in &v.warnings {
            eprintln!("warning: {w}");
        }
    }

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

    // Block the screensaver and system suspend for the duration of the encode
    // batch. Dropped at the end of `main` (or on early return / panic).
    let _inhibitor = if args.dry_run {
        None
    } else {
        let inh = Inhibitor::acquire("Encoding video files");
        if inh.is_active() {
            println!("inhibited screensaver / system sleep via systemd-inhibit");
        }
        Some(inh)
    };

    for (idx, input) in videos.iter().enumerate() {
        let rel = input.strip_prefix(&rel_root).unwrap_or(input).to_path_buf();
        let output = if web {
            match bundle_output_root.as_deref() {
                // --output was a file path; only meaningful for single-file
                // source mode. Use it verbatim.
                Some(root) if output_is_filepath(root) => root.to_path_buf(),
                // --output was a directory root: mirror `rel` underneath
                // and append `.web.mp4`.
                Some(root) => web_output_under_root(root, &rel),
                // No --output: sibling next to the source.
                None => web_output_path(input),
            }
        } else if hls {
            match bundle_output_root.as_deref() {
                // HLS output is itself a directory; a --output value
                // must be a directory root that contains mirrored
                // `<stem>.hls/` bundles.
                Some(root) => hls_output_under_root(root, &rel),
                None => hls_output_path(input),
            }
        } else if single_file_mode && output_is_filepath(&output_root) {
            output_root.clone()
        } else {
            output_path(&output_root, &rel, args.container)
        };

        let prefix = format!("[{}/{}]", idx + 1, videos.len());

        if output.exists() {
            println!("{prefix} skip (output exists): {}", rel.display());
            skipped += 1;
            continue;
        }

        // The "already in target codec" check uses ffprobe's codec_name —
        // not enough to tell whether a file is already a valid web copy
        // or HLS bundle (we'd also need to verify profile, level, audio
        // codec, faststart, segment layout etc.). Just re-encode unless
        // the sibling already exists.
        if !args.merge_subtitles && !bundle {
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
        }

        let subtitles: Vec<convert::SubtitleInput> = if args.embed_subtitles || args.merge_subtitles
        {
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

        // In merge mode, an input without sidecars produces an output
        // identical to the input — pointless work. Skip it before we
        // print "would merge" or fire ffmpeg.
        if args.merge_subtitles && subtitles.is_empty() {
            println!(
                "{prefix} skip (no sidecar subtitles to merge): {}",
                rel.display()
            );
            skipped += 1;
            continue;
        }

        if args.dry_run {
            let verb = if args.merge_subtitles {
                "would merge subs into"
            } else {
                "would encode"
            };
            println!("{prefix} {verb}: {} -> {}", rel.display(), output.display());
            continue;
        }

        let info = probe::video_info(input).ok();
        let duration = info.as_ref().and_then(|i| i.duration_secs);
        let source_subtitle_codecs: Vec<String> = info
            .as_ref()
            .map(|i| i.subtitle_codecs.clone())
            .unwrap_or_default();
        let unmappable_stream_indices: Vec<usize> = info
            .as_ref()
            .map(|i| i.unmappable_stream_indices.clone())
            .unwrap_or_default();

        if !subtitles.is_empty() {
            println!(
                "{prefix} found {} sidecar subtitle file(s)",
                subtitles.len()
            );
        }

        let opts = EncodeOptions {
            profile: args.profile,
            codec: args.codec,
            backend: args.backend,
            container: args.container,
            quality,
            preset,
            subtitles: &subtitles,
            source_subtitle_codecs: &source_subtitle_codecs,
            unmappable_stream_indices: &unmappable_stream_indices,
            merge_only: args.merge_subtitles,
            upscale: args.upscale,
        };

        let rel_str = rel.display().to_string();
        let tty = io::stdout().is_terminal();
        let prefix_for_cb = prefix.clone();
        let rel_for_cb = rel_str.clone();
        let action = if args.merge_subtitles {
            "merging"
        } else {
            "encoding"
        };
        if tty {
            print!("{prefix}   0% {rel_str}");
            let _ = io::stdout().flush();
        } else {
            println!("{prefix} {action}: {} -> {}", rel_str, output.display());
        }
        let on_progress = move |info: convert::ProgressInfo| {
            if !tty {
                return;
            }
            let pct = info.fraction.map(|f| (f * 100.0) as u32).unwrap_or(0);
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

    if args.profile == EncodingProfile::Web && args.merge_subtitles {
        bail!(
            "--profile web is incompatible with --merge-subtitles: web mode re-encodes the video, \
             while merge-subtitles stream-copies it. Pick one."
        );
    }
    if args.profile == EncodingProfile::Hls && args.merge_subtitles {
        bail!(
            "--profile hls is incompatible with --merge-subtitles: hls mode re-encodes a full \
             rendition ladder, while merge-subtitles stream-copies the video. Pick one."
        );
    }

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

/// HLS-profile equivalent of `output_path`: place the source's `rel` path
/// underneath `root` with the basename swapped to `<stem>.hls/` so callers
/// get a mirrored bundle tree under their chosen output root.
fn hls_output_under_root(root: &Path, rel: &Path) -> PathBuf {
    let stem = rel
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let parent = rel.parent().unwrap_or(Path::new(""));
    let name = if stem.is_empty() {
        "hls".to_string()
    } else {
        format!("{stem}.hls")
    };
    root.join(parent).join(name)
}

/// Web-profile equivalent of `output_path`: write the source's `rel` path
/// underneath `root` with the extension swapped to `.web.mp4`. Mirrors the
/// source tree so callers get parallel structure under their chosen
/// output root.
fn web_output_under_root(root: &Path, rel: &Path) -> PathBuf {
    let stem = rel
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    let parent = rel.parent().unwrap_or(Path::new(""));
    let name = if stem.is_empty() {
        "web.mp4".to_string()
    } else {
        format!("{stem}.web.mp4")
    };
    root.join(parent).join(name)
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

    #[test]
    fn web_output_under_root_mirrors_source_tree_and_swaps_extension() {
        let out = web_output_under_root(Path::new("/web"), Path::new("Show/S01/ep1.mkv"));
        assert_eq!(out, PathBuf::from("/web/Show/S01/ep1.web.mp4"));
    }

    #[test]
    fn web_output_under_root_preserves_dotted_basenames() {
        let out = web_output_under_root(
            Path::new("/web"),
            Path::new("Show/Game.of.Thrones.S01E01.1080p.x265.mkv"),
        );
        assert_eq!(
            out,
            PathBuf::from("/web/Show/Game.of.Thrones.S01E01.1080p.x265.web.mp4")
        );
    }

    #[test]
    fn web_output_under_root_handles_bare_filename() {
        // Source had no parent dir in `rel` (rel_root == source.parent()) —
        // output goes directly under the root.
        let out = web_output_under_root(Path::new("/web"), Path::new("ep1.mkv"));
        assert_eq!(out, PathBuf::from("/web/ep1.web.mp4"));
    }

    #[test]
    fn hls_output_under_root_mirrors_source_tree_to_dot_hls_dirs() {
        let out = hls_output_under_root(Path::new("/hls"), Path::new("Show/S01/ep1.mkv"));
        assert_eq!(out, PathBuf::from("/hls/Show/S01/ep1.hls"));
    }

    #[test]
    fn hls_output_under_root_preserves_dotted_basenames() {
        let out = hls_output_under_root(
            Path::new("/hls"),
            Path::new("Show/Game.of.Thrones.S01E01.1080p.x265.mkv"),
        );
        assert_eq!(
            out,
            PathBuf::from("/hls/Show/Game.of.Thrones.S01E01.1080p.x265.hls")
        );
    }
}
