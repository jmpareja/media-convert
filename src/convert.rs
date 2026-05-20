use anyhow::{Context, Result, bail};
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use crate::backend::Backend;
use crate::codec::Codec;
use crate::container::Container;
use crate::upscale::Upscale;

#[derive(Clone, Debug)]
pub struct SubtitleInput {
    pub path: PathBuf,
    pub language: Option<String>,
}

pub struct EncodeOptions<'a> {
    pub codec: Codec,
    pub backend: Backend,
    pub container: Container,
    pub quality: u8,
    /// `None` means: use the backend default (or omit `-preset` entirely if
    /// the backend has no preset concept, e.g. VAAPI).
    pub preset: Option<&'a str>,
    /// External subtitle files to mux into the output as additional tracks.
    pub subtitles: &'a [SubtitleInput],
    /// `codec_name` (from ffprobe) for each subtitle stream that's already
    /// inside the source video, in source-stream order. The length doubles as
    /// the count of source subtitle streams, used to compute output stream
    /// indices for sidecar SRTs we add. Per-stream codec choice depends on
    /// these — e.g. MKV target rejects `-c:s copy` for `mov_text`, so that
    /// stream gets transcoded to SRT instead.
    pub source_subtitle_codecs: &'a [String],
    /// Source-stream indices to drop from the output via `-map -0:N`.
    /// Set by the probe step for streams with unrecognized codecs
    /// (codec_id 0, like `mp4s` MPEG-4 systems tracks) that the muxer
    /// would otherwise reject.
    pub unmappable_stream_indices: &'a [usize],
    /// Skip re-encoding the video stream — copy it through (`-c:v copy`),
    /// suppress any backend-specific preamble / filter / quality flag, and
    /// just remux. Used by `--merge-subtitles` to add subtitle tracks to a
    /// file without burning encoder cycles on a stream we'd otherwise be
    /// passing through unchanged.
    pub merge_only: bool,
    /// Resize the video to a fixed output resolution. Disabled by default;
    /// ignored in `merge_only` mode (which stream-copies video without
    /// touching pixels).
    pub upscale: Upscale,
}

pub fn spawn_encode(input: &Path, output: &Path, opts: &EncodeOptions) -> Result<Child> {
    if let Some(parent) = output.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let mut cmd = build_ffmpeg_command(input, output, opts);
    cmd.stdout(Stdio::piped());
    cmd.spawn()
        .context("failed to invoke ffmpeg (is it installed and on PATH?)")
}

/// Construct the `ffmpeg` invocation for this encode without spawning.
/// Extracted from `spawn_encode` so its argument layout can be unit-tested.
pub(crate) fn build_ffmpeg_command(input: &Path, output: &Path, opts: &EncodeOptions) -> Command {
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
        // Some MP4s carry streams ffmpeg can't identify (codec_id 0, e.g.
        // the `mp4s` MPEG-4 systems tag). With MKV's `-map 0` policy those
        // unknown streams reach the matroska muxer, which rejects them
        // ("Tag mp4s incompatible with output codec id '0'"). Skip them
        // silently instead of aborting the encode.
        "-ignore_unknown",
    ]);

    // Skip backend init (hwaccel, vaapi_device) in merge-only mode — we're
    // not touching the video stream, so bringing up the GPU just to remux
    // would be pure overhead.
    if !opts.merge_only {
        for a in &cfg.preamble {
            cmd.arg(a);
        }
    }

    cmd.arg("-i").arg(file_protocol_arg(input));
    for sub in opts.subtitles {
        cmd.arg("-i").arg(file_protocol_arg(&sub.path));
    }

    match opts.container {
        Container::Mkv => {
            cmd.args(["-map", "0"]);
        }
        Container::Mp4 => {
            // MP4 can't hold attachments, data streams, or image-based subs.
            // Map only video, audio, and (text) subtitles, all optional so
            // sources missing a track don't fail.
            cmd.args(["-map", "0:v", "-map", "0:a?", "-map", "0:s?"]);
        }
    }
    // Drop streams the muxer can't accept (codec_id 0, e.g. `mp4s` data
    // tracks in some MP4 sources). Negative maps must come after the
    // positive map so they trim the previously-selected set.
    for idx in opts.unmappable_stream_indices {
        cmd.arg("-map").arg(format!("-0:{idx}"));
    }
    for i in 0..opts.subtitles.len() {
        cmd.arg("-map").arg(format!("{}", i + 1));
    }

    if opts.merge_only {
        // Stream-copy the video; no filter / quality / preset apply.
        cmd.args(["-c:v", "copy"]);
    } else {
        // Compose the upscale filter (if any) ahead of the backend's own
        // filter so the resized frames land in whatever pixel/hwframe layout
        // the encoder expects. Both halves are optional and either may be
        // absent for a given (backend, upscale) pair.
        let combined_filter = match (
            opts.upscale.filter(opts.backend),
            cfg.video_filter.as_deref(),
        ) {
            (None, None) => None,
            (Some(u), None) => Some(u),
            (None, Some(b)) => Some(b.to_string()),
            (Some(u), Some(b)) => Some(format!("{u},{b}")),
        };
        if let Some(filter) = combined_filter {
            cmd.args(["-vf", &filter]);
        }

        cmd.args(["-c:v", cfg.encoder]);
        cmd.args([cfg.quality_flag, &quality]);

        if let Some(preset) = effective_preset {
            cmd.args(["-preset", preset]);
        }

        for a in &cfg.extra_post {
            cmd.arg(a);
        }
    }

    cmd.args(["-c:a", "copy"]);
    match opts.container {
        Container::Mkv => {
            // Pick a per-stream subtitle codec for each known source sub:
            // `mov_text` (MP4 timed text) isn't accepted by the matroska
            // muxer's stream copy path, so it's transcoded to SRT. Anything
            // else copies. If we have no codec info (probe failed), fall back
            // to the historical `-c:s copy` which works for native-MKV subs
            // and only fails on the mov_text-from-MP4 case.
            if opts.source_subtitle_codecs.is_empty() {
                cmd.args(["-c:s", "copy"]);
            } else {
                for (i, codec_name) in opts.source_subtitle_codecs.iter().enumerate() {
                    let target = subtitle_codec_for_mkv(codec_name);
                    cmd.arg(format!("-c:s:{i}")).arg(target);
                }
                // Sidecar SRTs (added as separate inputs) land at
                // stream indices N..N+M; SRT copies cleanly into MKV.
                let n = opts.source_subtitle_codecs.len();
                for i in 0..opts.subtitles.len() {
                    cmd.arg(format!("-c:s:{}", n + i)).arg("copy");
                }
            }
            cmd.args(["-c:d", "copy", "-c:t", "copy"]);
        }
        Container::Mp4 => {
            cmd.args(["-c:s", "mov_text"]);
        }
    }

    for (i, sub) in opts.subtitles.iter().enumerate() {
        if let Some(lang) = &sub.language {
            let stream_idx = opts.source_subtitle_codecs.len() + i;
            cmd.arg(format!("-metadata:s:s:{stream_idx}"))
                .arg(format!("language={lang}"));
        }
    }

    cmd.arg(file_protocol_arg(output));
    cmd
}

/// Pick the output subtitle codec to use for a given source subtitle codec
/// when targeting MKV. The matroska muxer accepts most subtitle codecs as a
/// stream copy, but rejects `mov_text` (MP4 3GPP timed text) — we transcode
/// those to SRT, which all MKV players handle.
fn subtitle_codec_for_mkv(source_codec: &str) -> &'static str {
    match source_codec {
        "mov_text" => "srt",
        _ => "copy",
    }
}

/// Wrap a filesystem path with the explicit `file:` protocol prefix so ffmpeg
/// won't mistake colons embedded in the path (e.g. gvfs SMB mounts under
/// `/run/user/.../gvfs/smb-share:server=…/…`) for a `protocol:options`
/// pair. This is the documented escape hatch in the ffmpeg manual.
fn file_protocol_arg(p: &Path) -> OsString {
    let mut s = OsString::from("file:");
    s.push(p.as_os_str());
    s
}

#[derive(Clone, Debug, PartialEq)]
pub struct ProgressInfo {
    pub fraction: Option<f64>,
    pub fps: Option<f32>,
    pub speed: Option<f32>,
}

/// Drain ffmpeg's `-progress` stream until EOF, invoking `on_progress` with
/// updated stats.
pub fn read_progress<R, F>(stdout: R, total_secs: Option<f64>, mut on_progress: F)
where
    R: Read,
    F: FnMut(ProgressInfo),
{
    let total_us = total_secs.map(|s| (s * 1_000_000.0).max(1.0));
    let reader = BufReader::new(stdout);
    let mut current_us = 0.0;
    let mut current_fps = None;
    let mut current_speed = None;

    for line in reader.lines().map_while(|r| r.ok()) {
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        let key = key.trim();
        let value = value.trim();
        match key {
            "out_time_us" | "out_time_ms" => {
                if let Ok(us) = value.parse::<i64>() {
                    current_us = us.max(0) as f64;
                }
            }
            "fps" => {
                current_fps = value.parse::<f32>().ok();
            }
            "speed" => {
                // value is e.g. "1.23x"
                current_speed = value.strip_suffix('x').and_then(|s| s.parse::<f32>().ok());
            }
            "progress" => {
                let frac = if value == "end" {
                    Some(1.0)
                } else {
                    total_us.map(|t| (current_us / t).clamp(0.0, 1.0))
                };
                on_progress(ProgressInfo {
                    fraction: frac,
                    fps: current_fps,
                    speed: current_speed,
                });
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
    F: FnMut(ProgressInfo) + Send + 'static,
{
    let mut child = spawn_encode(input, output, opts)?;
    let stdout = child
        .stdout
        .take()
        .context("ffmpeg child has no stdout pipe")?;
    let progress_thread =
        std::thread::spawn(move || read_progress(stdout, total_secs, on_progress));
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::ffi::OsStr;

    fn args_of(cmd: &Command) -> Vec<String> {
        cmd.get_args()
            .map(|s| s.to_string_lossy().into_owned())
            .collect()
    }

    fn opts_software_x265() -> EncodeOptions<'static> {
        EncodeOptions {
            codec: Codec::X265,
            backend: Backend::Software,
            container: Container::Mkv,
            quality: 23,
            preset: None,
            subtitles: &[],
            source_subtitle_codecs: &[],
            unmappable_stream_indices: &[],
            merge_only: false,
            upscale: Upscale::None,
        }
    }

    #[test]
    fn unmappable_indices_negative_mapped_after_positive_map() {
        // Reproduces the Baruto.mp4 case: source has data streams 2 and 3
        // tagged `mp4s` with no recognized codec. With MKV's `-map 0`, the
        // matroska muxer rejects them ("Tag mp4s incompatible with output
        // codec id '0'"). Probe flags the indices, encoder negative-maps
        // them. Order matters — `-map -0:N` must follow `-map 0` so it
        // trims the previously-selected set.
        let unmappable = [2usize, 3usize];
        let mut opts = opts_software_x265();
        opts.unmappable_stream_indices = &unmappable;
        let cmd = build_ffmpeg_command(Path::new("/in/a.mp4"), Path::new("/out/a.mkv"), &opts);
        let args = args_of(&cmd);
        let positive = args
            .windows(2)
            .position(|w| w == ["-map", "0"])
            .expect("has -map 0");
        let neg2 = args
            .windows(2)
            .position(|w| w == ["-map", "-0:2"])
            .expect("has -map -0:2");
        let neg3 = args
            .windows(2)
            .position(|w| w == ["-map", "-0:3"])
            .expect("has -map -0:3");
        assert!(positive < neg2 && positive < neg3);
    }

    #[test]
    fn ffmpeg_command_passes_ignore_unknown_globally() {
        // Some MP4 sources contain streams ffmpeg can't identify (e.g.
        // `mp4s` MPEG-4 systems streams). With MKV's `-map 0`, those would
        // hit the matroska muxer and fail. `-ignore_unknown` must appear
        // before -i so the muxer drops them instead.
        let cmd = build_ffmpeg_command(
            Path::new("/in/a.mp4"),
            Path::new("/out/a.mkv"),
            &opts_software_x265(),
        );
        let args = args_of(&cmd);
        let flag_idx = args
            .iter()
            .position(|a| a == "-ignore_unknown")
            .expect("has -ignore_unknown");
        let input_idx = args.iter().position(|a| a == "-i").expect("has -i");
        assert!(flag_idx < input_idx, "-ignore_unknown must precede -i");
    }

    #[test]
    fn ffmpeg_command_uses_ffmpeg_program() {
        let cmd = build_ffmpeg_command(
            Path::new("/in/movie.mp4"),
            Path::new("/out/movie.mkv"),
            &opts_software_x265(),
        );
        assert_eq!(cmd.get_program(), OsStr::new("ffmpeg"));
    }

    #[test]
    fn mkv_container_uses_map_zero_and_copies_subs_data_attachments() {
        let cmd = build_ffmpeg_command(
            Path::new("/in/a.mp4"),
            Path::new("/out/a.mkv"),
            &opts_software_x265(),
        );
        let args = args_of(&cmd);
        // Bare `-map 0` (whole-stream copy)
        let map_idx = args.iter().position(|a| a == "-map").expect("has -map");
        assert_eq!(args[map_idx + 1], "0");
        // No probe info for source subs → fall back to global `-c:s copy`,
        // plus the always-present data and attachment copy flags
        assert!(args.windows(2).any(|w| w == ["-c:s", "copy"]));
        assert!(args.windows(2).any(|w| w == ["-c:d", "copy"]));
        assert!(args.windows(2).any(|w| w == ["-c:t", "copy"]));
    }

    #[test]
    fn mkv_transcodes_mov_text_to_srt_and_copies_other_subs() {
        // MP4 source with a mov_text track plus a subrip track. Matroska
        // muxer rejects `-c:s copy` for mov_text, so we emit per-stream:
        // stream 0 (mov_text) → srt; stream 1 (subrip) → copy.
        let source_subs = ["mov_text".to_string(), "subrip".to_string()];
        let mut opts = opts_software_x265();
        opts.source_subtitle_codecs = &source_subs;
        let cmd = build_ffmpeg_command(Path::new("/in/a.mp4"), Path::new("/out/a.mkv"), &opts);
        let args = args_of(&cmd);
        assert!(args.windows(2).any(|w| w == ["-c:s:0", "srt"]));
        assert!(args.windows(2).any(|w| w == ["-c:s:1", "copy"]));
        // The fallback global form must NOT be emitted when per-stream args
        // are present, otherwise it would override the per-stream choice.
        assert!(!args.windows(2).any(|w| w == ["-c:s", "copy"]));
    }

    #[test]
    fn mkv_passes_image_subs_through_with_copy() {
        // hdmv_pgs_subtitle (Blu-ray PGS) is image-based and CAN'T be turned
        // into text. It must be copied through as-is — never transcoded.
        let source_subs = ["hdmv_pgs_subtitle".to_string()];
        let mut opts = opts_software_x265();
        opts.source_subtitle_codecs = &source_subs;
        let cmd = build_ffmpeg_command(Path::new("/in/a.mkv"), Path::new("/out/a.mkv"), &opts);
        let args = args_of(&cmd);
        assert!(args.windows(2).any(|w| w == ["-c:s:0", "copy"]));
        assert!(!args.iter().any(|a| a == "srt"));
    }

    #[test]
    fn mkv_per_stream_includes_sidecar_indices() {
        // 1 source mov_text + 2 sidecar SRTs → output streams 0,1,2.
        // Per-stream args should cover all three.
        let subs = [
            SubtitleInput {
                path: PathBuf::from("/in/a.en.srt"),
                language: None,
            },
            SubtitleInput {
                path: PathBuf::from("/in/a.fr.srt"),
                language: None,
            },
        ];
        let source_subs = ["mov_text".to_string()];
        let mut opts = opts_software_x265();
        opts.subtitles = &subs;
        opts.source_subtitle_codecs = &source_subs;
        let cmd = build_ffmpeg_command(Path::new("/in/a.mp4"), Path::new("/out/a.mkv"), &opts);
        let args = args_of(&cmd);
        assert!(args.windows(2).any(|w| w == ["-c:s:0", "srt"]));
        assert!(args.windows(2).any(|w| w == ["-c:s:1", "copy"]));
        assert!(args.windows(2).any(|w| w == ["-c:s:2", "copy"]));
    }

    #[test]
    fn mp4_container_uses_selective_mapping_and_mov_text() {
        let mut opts = opts_software_x265();
        opts.container = Container::Mp4;
        let cmd = build_ffmpeg_command(Path::new("/in/a.mkv"), Path::new("/out/a.mp4"), &opts);
        let args = args_of(&cmd);
        // Selective mapping: video required, audio + subs optional
        assert!(args.windows(2).any(|w| w == ["-map", "0:v"]));
        assert!(args.windows(2).any(|w| w == ["-map", "0:a?"]));
        assert!(args.windows(2).any(|w| w == ["-map", "0:s?"]));
        // No bare `-map 0`
        assert!(!args.windows(2).any(|w| w == ["-map", "0"]));
        // Subtitles get transcoded to mov_text; no data/attachment copy
        assert!(args.windows(2).any(|w| w == ["-c:s", "mov_text"]));
        assert!(!args.windows(2).any(|w| w == ["-c:s", "copy"]));
        assert!(!args.windows(2).any(|w| w == ["-c:d", "copy"]));
        assert!(!args.windows(2).any(|w| w == ["-c:t", "copy"]));
    }

    #[test]
    fn vaapi_backend_emits_preamble_and_video_filter() {
        let mut opts = opts_software_x265();
        opts.backend = Backend::Vaapi;
        let cmd = build_ffmpeg_command(Path::new("/in/a.mp4"), Path::new("/out/a.mkv"), &opts);
        let args = args_of(&cmd);
        // VAAPI device init must come before -i
        let dev_idx = args
            .iter()
            .position(|a| a == "-vaapi_device")
            .expect("has -vaapi_device");
        let input_idx = args.iter().position(|a| a == "-i").expect("has -i");
        assert!(dev_idx < input_idx, "preamble must precede -i");
        assert_eq!(args[dev_idx + 1], "/dev/dri/renderD128");
        // VAAPI requires the upload filter
        assert!(
            args.windows(2)
                .any(|w| w == ["-vf", "format=nv12,hwupload"])
        );
        // VAAPI uses hevc_vaapi for x265 with -qp
        assert!(args.windows(2).any(|w| w == ["-c:v", "hevc_vaapi"]));
        assert!(args.iter().any(|a| a == "-qp"));
        // VAAPI has no preset concept — `-preset` should be absent
        assert!(!args.iter().any(|a| a == "-preset"));
    }

    #[test]
    fn nvenc_backend_emits_extra_post_rc_vbr() {
        let mut opts = opts_software_x265();
        opts.backend = Backend::Nvenc;
        let cmd = build_ffmpeg_command(Path::new("/in/a.mp4"), Path::new("/out/a.mkv"), &opts);
        let args = args_of(&cmd);
        assert!(args.windows(2).any(|w| w == ["-c:v", "hevc_nvenc"]));
        assert!(args.windows(2).any(|w| w == ["-rc", "vbr"]));
        assert!(args.iter().any(|a| a == "-cq"));
    }

    #[test]
    fn nvenc_routes_decode_through_cuda_before_input() {
        // -hwaccel applies to the next -i, so it must appear before the main
        // input. Verify both flags are present and ordered correctly.
        let mut opts = opts_software_x265();
        opts.backend = Backend::Nvenc;
        let cmd = build_ffmpeg_command(Path::new("/in/a.mp4"), Path::new("/out/a.mkv"), &opts);
        let args = args_of(&cmd);
        let hw_idx = args
            .iter()
            .position(|a| a == "-hwaccel")
            .expect("has -hwaccel");
        let input_idx = args.iter().position(|a| a == "-i").expect("has -i");
        assert!(hw_idx < input_idx, "-hwaccel must precede -i");
        assert_eq!(args[hw_idx + 1], "cuda");
        assert!(
            args.windows(2)
                .any(|w| w == ["-hwaccel_output_format", "cuda"])
        );
    }

    #[test]
    fn explicit_preset_overrides_backend_default() {
        let mut opts = opts_software_x265();
        opts.preset = Some("slow");
        let cmd = build_ffmpeg_command(Path::new("/in/a.mp4"), Path::new("/out/a.mkv"), &opts);
        let args = args_of(&cmd);
        assert!(args.windows(2).any(|w| w == ["-preset", "slow"]));
        assert!(!args.windows(2).any(|w| w == ["-preset", "medium"]));
    }

    #[test]
    fn sidecar_subtitle_inputs_added_after_main_input_and_mapped() {
        let subs = [
            SubtitleInput {
                path: PathBuf::from("/in/a.en.srt"),
                language: Some("en".into()),
            },
            SubtitleInput {
                path: PathBuf::from("/in/a.fr.srt"),
                language: Some("fr".into()),
            },
        ];
        let source_subs = ["subrip".to_string(), "subrip".to_string()];
        let mut opts = opts_software_x265();
        opts.subtitles = &subs;
        opts.source_subtitle_codecs = &source_subs;
        let cmd = build_ffmpeg_command(Path::new("/in/a.mp4"), Path::new("/out/a.mkv"), &opts);
        let args = args_of(&cmd);
        // Three -i in order: main input, sub1, sub2
        let inputs: Vec<&String> = args
            .iter()
            .enumerate()
            .filter(|(i, a)| a.as_str() == "-i" && *i + 1 < args.len())
            .map(|(i, _)| &args[i + 1])
            .collect();
        assert_eq!(inputs.len(), 3);
        assert_eq!(inputs[0], "file:/in/a.mp4");
        assert_eq!(inputs[1], "file:/in/a.en.srt");
        assert_eq!(inputs[2], "file:/in/a.fr.srt");
        // Sidecar maps reference inputs 1 and 2
        assert!(args.windows(2).any(|w| w == ["-map", "1"]));
        assert!(args.windows(2).any(|w| w == ["-map", "2"]));
        // Language metadata indices start from the source-subtitle count
        assert!(
            args.windows(2)
                .any(|w| w == ["-metadata:s:s:2", "language=en"])
        );
        assert!(
            args.windows(2)
                .any(|w| w == ["-metadata:s:s:3", "language=fr"])
        );
    }

    #[test]
    fn subtitle_without_language_skips_metadata() {
        let subs = [SubtitleInput {
            path: PathBuf::from("/in/a.srt"),
            language: None,
        }];
        let mut opts = opts_software_x265();
        opts.subtitles = &subs;
        let cmd = build_ffmpeg_command(Path::new("/in/a.mp4"), Path::new("/out/a.mkv"), &opts);
        let args = args_of(&cmd);
        assert!(!args.iter().any(|a| a.starts_with("-metadata:s:s:")));
    }

    #[test]
    fn output_path_is_last_argument() {
        let cmd = build_ffmpeg_command(
            Path::new("/in/a.mp4"),
            Path::new("/out/dir/a.mkv"),
            &opts_software_x265(),
        );
        let args = args_of(&cmd);
        assert_eq!(args.last().map(String::as_str), Some("file:/out/dir/a.mkv"));
    }

    #[test]
    fn paths_with_colons_get_file_protocol_prefix() {
        // Reproduces the gvfs SMB-mount case: a path like
        // `…/smb-share:server=…` would otherwise look like a `protocol:options`
        // pair to ffmpeg and fail to open. The `file:` prefix forces the file
        // protocol regardless of what's in the path.
        let input = Path::new(
            "/run/user/1000/gvfs/smb-share:server=192.168.1.134,share=tv/Firefly (2002)/ep1.mp4",
        );
        let output = Path::new(
            "/run/user/1000/gvfs/smb-share:server=192.168.1.134,share=tv/Firefly (2002)-converted/ep1.mkv",
        );
        let cmd = build_ffmpeg_command(input, output, &opts_software_x265());
        let args = args_of(&cmd);

        let i_idx = args.iter().position(|a| a == "-i").expect("has -i");
        assert_eq!(
            args[i_idx + 1],
            "file:/run/user/1000/gvfs/smb-share:server=192.168.1.134,share=tv/Firefly (2002)/ep1.mp4"
        );
        assert_eq!(
            args.last().map(String::as_str),
            Some(
                "file:/run/user/1000/gvfs/smb-share:server=192.168.1.134,share=tv/Firefly (2002)-converted/ep1.mkv"
            )
        );
    }

    fn collect_progress(input: &[u8], total_secs: Option<f64>) -> Vec<ProgressInfo> {
        let calls = RefCell::new(Vec::new());
        read_progress(input, total_secs, |info| calls.borrow_mut().push(info));
        calls.into_inner()
    }

    #[test]
    fn read_progress_emits_fraction_for_out_time_us() {
        let calls = collect_progress(b"out_time_us=500000\nprogress=continue\n", Some(1.0));
        assert_eq!(
            calls,
            vec![ProgressInfo {
                fraction: Some(0.5),
                fps: None,
                speed: None
            }]
        );
    }

    #[test]
    fn read_progress_treats_out_time_ms_as_microseconds() {
        // ffmpeg's `out_time_ms` is historically microseconds, not milliseconds.
        let calls = collect_progress(b"out_time_ms=750000\nprogress=continue\n", Some(1.5));
        assert_eq!(
            calls,
            vec![ProgressInfo {
                fraction: Some(0.5),
                fps: None,
                speed: None
            }]
        );
    }

    #[test]
    fn read_progress_emits_none_when_total_unknown() {
        let calls = collect_progress(b"out_time_us=500000\nprogress=continue\n", None);
        assert_eq!(
            calls,
            vec![ProgressInfo {
                fraction: None,
                fps: None,
                speed: None
            }]
        );
    }

    #[test]
    fn read_progress_clamps_overshoot() {
        let calls = collect_progress(b"out_time_us=2000000\nprogress=continue\n", Some(1.0));
        assert_eq!(
            calls,
            vec![ProgressInfo {
                fraction: Some(1.0),
                fps: None,
                speed: None
            }]
        );
    }

    #[test]
    fn read_progress_treats_negative_values_as_zero() {
        let calls = collect_progress(b"out_time_us=-100\nprogress=continue\n", Some(1.0));
        assert_eq!(
            calls,
            vec![ProgressInfo {
                fraction: Some(0.0),
                fps: None,
                speed: None
            }]
        );
    }

    #[test]
    fn read_progress_emits_one_on_end_marker() {
        let calls = collect_progress(b"progress=end\n", Some(60.0));
        assert_eq!(
            calls,
            vec![ProgressInfo {
                fraction: Some(1.0),
                fps: None,
                speed: None
            }]
        );
    }

    #[test]
    fn read_progress_ignores_unknown_keys_and_malformed_lines() {
        let input = b"frame=42\nbitrate=500kbps\nnokey\nout_time_us=invalid\nout_time_us=1000000\nprogress=continue\n";
        let calls = collect_progress(input, Some(2.0));
        assert_eq!(
            calls,
            vec![ProgressInfo {
                fraction: Some(0.5),
                fps: None,
                speed: None
            }]
        );
    }

    #[test]
    fn read_progress_emits_for_each_advance() {
        let input = b"out_time_us=250000\nprogress=continue\nout_time_us=500000\nprogress=continue\nout_time_us=750000\nprogress=continue\nprogress=end\n";
        let calls = collect_progress(input, Some(1.0));
        assert_eq!(
            calls,
            vec![
                ProgressInfo {
                    fraction: Some(0.25),
                    fps: None,
                    speed: None
                },
                ProgressInfo {
                    fraction: Some(0.5),
                    fps: None,
                    speed: None
                },
                ProgressInfo {
                    fraction: Some(0.75),
                    fps: None,
                    speed: None
                },
                ProgressInfo {
                    fraction: Some(1.0),
                    fps: None,
                    speed: None
                }
            ]
        );
    }

    #[test]
    fn read_progress_strips_whitespace_around_key_and_value() {
        let calls = collect_progress(
            b"  out_time_us  =  500000  \n  progress  =  continue  \n",
            Some(1.0),
        );
        assert_eq!(
            calls,
            vec![ProgressInfo {
                fraction: Some(0.5),
                fps: None,
                speed: None
            }]
        );
    }

    fn vf_arg(args: &[String]) -> Option<&str> {
        let i = args.iter().position(|a| a == "-vf")?;
        args.get(i + 1).map(String::as_str)
    }

    #[test]
    fn upscale_none_emits_no_video_filter_on_software() {
        // Sanity: default (Upscale::None) on a backend with no native
        // video_filter (software) means no -vf at all.
        let cmd = build_ffmpeg_command(
            Path::new("/in/a.mp4"),
            Path::new("/out/a.mkv"),
            &opts_software_x265(),
        );
        let args = args_of(&cmd);
        assert!(!args.iter().any(|a| a == "-vf"));
    }

    #[test]
    fn upscale_1080p_software_emits_cpu_scale_pad_filter() {
        let mut opts = opts_software_x265();
        opts.upscale = Upscale::To1080p;
        let cmd = build_ffmpeg_command(Path::new("/in/a.mp4"), Path::new("/out/a.mkv"), &opts);
        let args = args_of(&cmd);
        let vf = vf_arg(&args).expect("has -vf");
        assert!(vf.contains("scale=1920:1080:flags=lanczos"));
        assert!(vf.contains("pad=1920:1080:(ow-iw)/2:(oh-ih)/2"));
        assert!(!vf.contains("scale_cuda"));
    }

    #[test]
    fn upscale_1080p_nvenc_emits_cuda_scale_with_hwdownload_pad_hwupload() {
        let mut opts = opts_software_x265();
        opts.backend = Backend::Nvenc;
        opts.upscale = Upscale::To1080p;
        let cmd = build_ffmpeg_command(Path::new("/in/a.mp4"), Path::new("/out/a.mkv"), &opts);
        let args = args_of(&cmd);
        let vf = vf_arg(&args).expect("has -vf");
        assert!(vf.contains("scale_cuda=1920:1080:force_original_aspect_ratio=decrease"));
        assert!(vf.contains("hwdownload,format=nv12"));
        assert!(vf.ends_with("hwupload_cuda"));
    }

    #[test]
    fn upscale_1080p_vaapi_chains_cpu_scale_before_backend_hwupload() {
        // VAAPI's backend filter is `format=nv12,hwupload`. With upscaling on,
        // the CPU scale+pad must come first so the padded frames are what get
        // moved onto the VAAPI device.
        let mut opts = opts_software_x265();
        opts.backend = Backend::Vaapi;
        opts.upscale = Upscale::To1080p;
        let cmd = build_ffmpeg_command(Path::new("/in/a.mp4"), Path::new("/out/a.mkv"), &opts);
        let args = args_of(&cmd);
        let vf = vf_arg(&args).expect("has -vf");
        let scale_idx = vf.find("scale=1920:1080").expect("has cpu scale");
        let upload_idx = vf.find("hwupload").expect("has hwupload");
        assert!(
            scale_idx < upload_idx,
            "scale must precede hwupload, got: {vf}"
        );
        assert!(vf.contains("format=nv12,hwupload"));
    }

    #[test]
    fn merge_only_suppresses_upscale_filter() {
        // merge_only stream-copies the video — pixels aren't touched, so any
        // requested upscale must be silently dropped (not just no-op'd).
        let mut opts = opts_software_x265();
        opts.merge_only = true;
        opts.upscale = Upscale::To1080p;
        let cmd = build_ffmpeg_command(Path::new("/in/a.mkv"), Path::new("/out/a.mkv"), &opts);
        let args = args_of(&cmd);
        assert!(!args.iter().any(|a| a == "-vf"));
        assert!(args.windows(2).any(|w| w == ["-c:v", "copy"]));
    }

    #[test]
    fn merge_only_emits_video_copy_and_omits_encoder_args() {
        let mut opts = opts_software_x265();
        opts.merge_only = true;
        let cmd = build_ffmpeg_command(Path::new("/in/a.mkv"), Path::new("/out/a.mkv"), &opts);
        let args = args_of(&cmd);
        // Stream copy the video — no libx265, no -crf, no -preset, no filter.
        assert!(args.windows(2).any(|w| w == ["-c:v", "copy"]));
        assert!(!args.iter().any(|a| a == "libx265"));
        assert!(!args.iter().any(|a| a == "-crf"));
        assert!(!args.iter().any(|a| a == "-preset"));
        assert!(!args.iter().any(|a| a == "-vf"));
    }

    #[test]
    fn merge_only_skips_backend_preamble() {
        // Backend init (e.g. VAAPI device, NVENC -hwaccel) is pointless when
        // we're not touching the video stream — it should be suppressed so a
        // user without a working GPU can still merge subs with backend=vaapi.
        let mut opts = opts_software_x265();
        opts.backend = Backend::Vaapi;
        opts.merge_only = true;
        let cmd = build_ffmpeg_command(Path::new("/in/a.mkv"), Path::new("/out/a.mkv"), &opts);
        let args = args_of(&cmd);
        assert!(!args.iter().any(|a| a == "-vaapi_device"));
        assert!(!args.iter().any(|a| a == "-hwaccel"));
        assert!(args.windows(2).any(|w| w == ["-c:v", "copy"]));
    }

    #[test]
    fn merge_only_still_muxes_sidecar_subtitles() {
        // The whole point of merge mode: stream-copy video while pulling in
        // sidecar SRTs as new subtitle tracks.
        let subs = [SubtitleInput {
            path: PathBuf::from("/in/a.en.srt"),
            language: Some("en".into()),
        }];
        let mut opts = opts_software_x265();
        opts.merge_only = true;
        opts.subtitles = &subs;
        let cmd = build_ffmpeg_command(Path::new("/in/a.mkv"), Path::new("/out/a.mkv"), &opts);
        let args = args_of(&cmd);
        // Sidecar input + map + language metadata still emitted.
        let inputs: Vec<&String> = args
            .iter()
            .enumerate()
            .filter(|(i, a)| a.as_str() == "-i" && *i + 1 < args.len())
            .map(|(i, _)| &args[i + 1])
            .collect();
        assert_eq!(inputs.len(), 2);
        assert_eq!(inputs[1], "file:/in/a.en.srt");
        assert!(args.windows(2).any(|w| w == ["-map", "1"]));
        assert!(
            args.windows(2)
                .any(|w| w == ["-metadata:s:s:0", "language=en"])
        );
        // Audio still copies; video is the only thing that switched to copy
        // mode (audio was already copy in normal mode, so this reaffirms it).
        assert!(args.windows(2).any(|w| w == ["-c:a", "copy"]));
    }
}
