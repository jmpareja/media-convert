use anyhow::{Context, Result, bail};
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use crate::backend::Backend;
use crate::codec::Codec;
use crate::container::Container;
use crate::profile::{EncodingProfile, HLS_SEGMENT_SECONDS, hls_variants, web_encoder_args};
use crate::upscale::Upscale;

#[derive(Clone, Debug)]
pub struct SubtitleInput {
    pub path: PathBuf,
    pub language: Option<String>,
}

pub struct EncodeOptions<'a> {
    /// Top-level transcoding mode. `Standard` honors the configurable
    /// codec/backend/container/quality/preset/upscale fields below; `Web`
    /// ignores them all and emits a fixed browser-friendly H.264/AAC/MP4
    /// `+faststart` configuration.
    pub profile: EncodingProfile,
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
    /// passing through unchanged. Only meaningful for the Standard profile;
    /// Web ignores it (validated at the CLI/GUI layer).
    pub merge_only: bool,
    /// Resize the video to a fixed output resolution. Disabled by default;
    /// ignored in `merge_only` mode (which stream-copies video without
    /// touching pixels) and in the Web profile (which has its own fixed
    /// pixel pipeline).
    pub upscale: Upscale,
}

pub fn spawn_encode(input: &Path, output: &Path, opts: &EncodeOptions) -> Result<Child> {
    prepare_output_dirs(output, opts.profile)?;
    let mut cmd = build_ffmpeg_command(input, output, opts);
    cmd.stdout(Stdio::piped());
    cmd.spawn()
        .context("failed to invoke ffmpeg (is it installed and on PATH?)")
}

/// Ensure the directories ffmpeg is about to write into already exist.
///
/// Standard/Web produce a single file, so we just create the file's parent
/// directory if it isn't already there. HLS produces a directory bundle —
/// the `.hls` root and one subdirectory per variant — and ffmpeg's HLS
/// muxer does not auto-create these.
fn prepare_output_dirs(output: &Path, profile: EncodingProfile) -> Result<()> {
    match profile {
        EncodingProfile::Standard | EncodingProfile::Web => {
            if let Some(parent) = output.parent() {
                std::fs::create_dir_all(parent)
                    .with_context(|| format!("failed to create {}", parent.display()))?;
            }
        }
        EncodingProfile::Hls => {
            std::fs::create_dir_all(output)
                .with_context(|| format!("failed to create {}", output.display()))?;
            for v in hls_variants() {
                let sub = output.join(v.name);
                std::fs::create_dir_all(&sub)
                    .with_context(|| format!("failed to create {}", sub.display()))?;
            }
        }
    }
    Ok(())
}

/// Construct the `ffmpeg` invocation for this encode without spawning.
/// Extracted from `spawn_encode` so its argument layout can be unit-tested.
pub(crate) fn build_ffmpeg_command(input: &Path, output: &Path, opts: &EncodeOptions) -> Command {
    if opts.profile == EncodingProfile::Hls {
        return build_hls_command(input, output);
    }
    let cfg = opts.backend.config(opts.codec);
    let quality = opts.quality.to_string();
    let effective_preset: Option<&str> = opts.preset.or(cfg.default_preset);
    let web = opts.profile == EncodingProfile::Web;
    // Web is its own fixed pixel pipeline; the configurable upscale/preset/
    // quality/backend knobs don't apply.
    let video_passthrough = opts.merge_only && !web;

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
    // would be pure overhead. Also skip in Web mode, which is CPU-only
    // libx264 and doesn't use the configured backend.
    if !video_passthrough && !web {
        for a in &cfg.preamble {
            cmd.arg(a);
        }
    }

    cmd.arg("-i").arg(file_protocol_arg(input));
    for sub in opts.subtitles {
        cmd.arg("-i").arg(file_protocol_arg(&sub.path));
    }

    // Web is always MP4-on-output; otherwise honor the configured container.
    let effective_container = if web { Container::Mp4 } else { opts.container };
    match effective_container {
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

    if video_passthrough {
        // Stream-copy the video; no filter / quality / preset apply.
        cmd.args(["-c:v", "copy"]);
    } else if web {
        // Fixed encoder fragment lives in profile::web_encoder_args(). It
        // covers video (libx264 high@4.0 CRF 20 yuv420p), audio (aac stereo
        // 192k) and `-movflags +faststart` in one place. Audio is included
        // here, so the post-block `-c:a copy` is suppressed below.
        for a in web_encoder_args() {
            cmd.arg(a);
        }
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

    // Web sets its own audio codec inside web_encoder_args(); for all other
    // modes the audio stream copies through.
    if !web {
        cmd.args(["-c:a", "copy"]);
    }
    match effective_container {
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

/// Construct the `ffmpeg` invocation for an HLS bundle: one ABR ladder of
/// video renditions plus matching AAC stereo audio, packaged into a
/// `master.m3u8` + per-variant `playlist.m3u8` + TS segments tree under
/// `output`. The output directory and per-variant subdirectories are
/// created by [`prepare_output_dirs`] before this command runs.
///
/// Sub-arguments (per rendition `i`):
/// * `-map "[vN]"`              the scaled-then-tagged video stream
/// * `-c:v:i libx264`           CPU x264 (HLS portability before HEVC)
/// * `-b:v:i / -maxrate:v:i / -bufsize:v:i` from `hls_variants()`
/// * `-profile:v:i / -level:v:i` likewise
/// * `-preset slow -pix_fmt yuv420p` shared
/// * `-force_key_frames "expr:gte(t,n_forced*6)"` aligns keyframes
///   with the HLS segment boundary so each `.ts` starts on an I-frame
///
/// `var_stream_map` ties each `v:i` to its `a:i` and the variant folder
/// name; the positional output template `…/%v/playlist.m3u8` plus
/// `-hls_segment_filename …/%v/segment_%03d.ts` lays out the tree.
fn build_hls_command(input: &Path, output: &Path) -> Command {
    let mut cmd = Command::new("ffmpeg");
    cmd.args([
        "-hide_banner",
        "-loglevel",
        "warning",
        "-nostats",
        "-progress",
        "pipe:1",
        "-n",
        "-ignore_unknown",
    ]);
    cmd.arg("-i").arg(file_protocol_arg(input));

    let variants = hls_variants();
    // filter_complex: split the video N ways and scale each branch to the
    // variant's height (width auto-tracks source aspect ratio at multiples
    // of 2). Tag every output so `-map "[v360]"` style refs resolve.
    let mut filter = format!("[0:v]split={}", variants.len());
    for v in variants {
        filter.push_str(&format!("[v{}in]", v.name));
    }
    filter.push(';');
    let last = variants.len() - 1;
    for (i, v) in variants.iter().enumerate() {
        filter.push_str(&format!(
            "[v{name}in]scale=-2:{h}:flags=lanczos,setsar=1[v{name}]",
            name = v.name,
            h = v.height,
        ));
        if i != last {
            filter.push(';');
        }
    }
    cmd.args(["-filter_complex", &filter]);

    let gop_expr = format!("expr:gte(t,n_forced*{HLS_SEGMENT_SECONDS})");

    // Per-rendition video + audio output blocks. Each maps the scaled
    // video and the source's first audio stream, then sets codec /
    // bitrate / profile / level options scoped to that output stream
    // index (`:v:i` / `:a:i`).
    for (i, v) in variants.iter().enumerate() {
        cmd.arg("-map").arg(format!("[v{}]", v.name));
        cmd.arg("-map").arg("0:a:0?");

        let vi = format!(":v:{i}");
        let ai = format!(":a:{i}");

        cmd.args([&format!("-c{vi}"), "libx264"]);
        cmd.args([&format!("-b{vi}"), v.video_bitrate]);
        cmd.args([&format!("-maxrate{vi}"), v.video_maxrate]);
        cmd.args([&format!("-bufsize{vi}"), v.video_bufsize]);
        cmd.args([&format!("-profile{vi}"), v.video_profile]);
        cmd.args([&format!("-level{vi}"), v.video_level]);
        cmd.args([&format!("-force_key_frames{vi}"), &gop_expr]);

        cmd.args([&format!("-c{ai}"), "aac"]);
        cmd.args([&format!("-ac{ai}"), "2"]);
        cmd.args([&format!("-b{ai}"), v.audio_bitrate]);
    }

    // Shared across all video outputs. `-preset slow` is libx264's quality/
    // speed tradeoff; `-pix_fmt yuv420p` keeps the chroma layout that every
    // HLS player accepts; `-sc_threshold 0` disables scene-cut keyframes so
    // the only I-frames are the ones force_key_frames already places.
    cmd.args([
        "-preset",
        "slow",
        "-pix_fmt",
        "yuv420p",
        "-sc_threshold",
        "0",
    ]);

    // `var_stream_map` ties each (v:i, a:i) pair to a folder name. The
    // string format is a comma list of stream specs per variant, with
    // variants separated by spaces. ffmpeg substitutes `%v` in the
    // output filenames with the corresponding `name:`.
    let mut var_map = String::new();
    for (i, v) in variants.iter().enumerate() {
        if i != 0 {
            var_map.push(' ');
        }
        var_map.push_str(&format!("v:{i},a:{i},name:{}", v.name));
    }

    let seg_template = output.join("%v").join("segment_%03d.ts");
    let playlist_template = output.join("%v").join("playlist.m3u8");

    cmd.args([
        "-f",
        "hls",
        "-hls_time",
        &HLS_SEGMENT_SECONDS.to_string(),
        "-hls_playlist_type",
        "vod",
        "-hls_flags",
        "independent_segments",
        "-hls_segment_type",
        "mpegts",
        "-hls_list_size",
        "0",
    ]);
    cmd.arg("-hls_segment_filename").arg(&seg_template);
    cmd.args(["-master_pl_name", "master.m3u8"]);
    cmd.args(["-var_stream_map", &var_map]);
    cmd.arg(&playlist_template);
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
            profile: EncodingProfile::Standard,
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

    fn opts_web() -> EncodeOptions<'static> {
        let mut opts = opts_software_x265();
        opts.profile = EncodingProfile::Web;
        opts
    }

    #[test]
    fn web_profile_emits_libx264_high_level40_yuv420p() {
        let cmd = build_ffmpeg_command(
            Path::new("/in/a.mkv"),
            Path::new("/out/a.web.mp4"),
            &opts_web(),
        );
        let args = args_of(&cmd);
        assert!(args.windows(2).any(|w| w == ["-c:v", "libx264"]));
        assert!(args.windows(2).any(|w| w == ["-profile:v", "high"]));
        assert!(args.windows(2).any(|w| w == ["-level", "4.0"]));
        assert!(args.windows(2).any(|w| w == ["-pix_fmt", "yuv420p"]));
        assert!(args.windows(2).any(|w| w == ["-crf", "20"]));
        assert!(args.windows(2).any(|w| w == ["-preset", "slow"]));
    }

    #[test]
    fn web_profile_emits_aac_stereo_192k_overriding_audio_copy() {
        // Web bakes audio settings into the encoder fragment; the default
        // `-c:a copy` for other modes must NOT also be emitted, or the
        // copy would override the per-codec choice.
        let cmd = build_ffmpeg_command(
            Path::new("/in/a.mkv"),
            Path::new("/out/a.web.mp4"),
            &opts_web(),
        );
        let args = args_of(&cmd);
        assert!(args.windows(2).any(|w| w == ["-c:a", "aac"]));
        assert!(args.windows(2).any(|w| w == ["-ac", "2"]));
        assert!(args.windows(2).any(|w| w == ["-b:a", "192k"]));
        assert!(!args.windows(2).any(|w| w == ["-c:a", "copy"]));
    }

    #[test]
    fn web_profile_emits_movflags_faststart() {
        let cmd = build_ffmpeg_command(
            Path::new("/in/a.mkv"),
            Path::new("/out/a.web.mp4"),
            &opts_web(),
        );
        let args = args_of(&cmd);
        assert!(args.windows(2).any(|w| w == ["-movflags", "+faststart"]));
    }

    #[test]
    fn web_profile_uses_mp4_mapping_and_mov_text_subs() {
        // Web is always MP4-on-output regardless of `opts.container`. That
        // means selective stream mapping plus mov_text for subtitles. Even
        // if a test leaves `container: Mkv`, Web should override.
        let mut opts = opts_web();
        opts.container = Container::Mkv;
        let cmd = build_ffmpeg_command(Path::new("/in/a.mkv"), Path::new("/out/a.web.mp4"), &opts);
        let args = args_of(&cmd);
        assert!(args.windows(2).any(|w| w == ["-map", "0:v"]));
        assert!(args.windows(2).any(|w| w == ["-map", "0:a?"]));
        assert!(args.windows(2).any(|w| w == ["-map", "0:s?"]));
        assert!(!args.windows(2).any(|w| w == ["-map", "0"]));
        assert!(args.windows(2).any(|w| w == ["-c:s", "mov_text"]));
    }

    #[test]
    fn web_profile_suppresses_backend_preamble() {
        // Web is CPU libx264 only; any backend preamble from the user's
        // previously-selected hardware backend must not leak into the
        // ffmpeg command line.
        let mut opts = opts_web();
        opts.backend = Backend::Vaapi;
        let cmd = build_ffmpeg_command(Path::new("/in/a.mkv"), Path::new("/out/a.web.mp4"), &opts);
        let args = args_of(&cmd);
        assert!(!args.iter().any(|a| a == "-vaapi_device"));
        assert!(!args.iter().any(|a| a == "-hwaccel"));
        // And no VAAPI hwupload either.
        assert!(!args.iter().any(|a| a.contains("hwupload")));
    }

    #[test]
    fn web_profile_suppresses_upscale_filter() {
        // Web has its own fixed pixel pipeline; the configured `upscale`
        // field must not produce a `-vf` argument.
        let mut opts = opts_web();
        opts.upscale = Upscale::To1080p;
        let cmd = build_ffmpeg_command(Path::new("/in/a.mkv"), Path::new("/out/a.web.mp4"), &opts);
        let args = args_of(&cmd);
        assert!(!args.iter().any(|a| a == "-vf"));
    }

    #[test]
    fn web_profile_keeps_sidecar_subtitle_inputs() {
        // Sidecar SRTs still get muxed in (as mov_text) when --embed-subtitles
        // or --merge-subtitles was passed for a Standard run that the user
        // then re-ran as Web. The web profile uses MP4 mapping, so subs land
        // as mov_text. Language metadata still gets emitted.
        let subs = [SubtitleInput {
            path: PathBuf::from("/in/a.en.srt"),
            language: Some("en".into()),
        }];
        let mut opts = opts_web();
        opts.subtitles = &subs;
        let cmd = build_ffmpeg_command(Path::new("/in/a.mkv"), Path::new("/out/a.web.mp4"), &opts);
        let args = args_of(&cmd);
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
    }

    fn opts_hls() -> EncodeOptions<'static> {
        let mut opts = opts_software_x265();
        opts.profile = EncodingProfile::Hls;
        opts
    }

    #[test]
    fn hls_profile_uses_filter_complex_splitting_to_three_renditions() {
        let cmd =
            build_ffmpeg_command(Path::new("/in/a.mkv"), Path::new("/out/a.hls"), &opts_hls());
        let args = args_of(&cmd);
        let idx = args
            .iter()
            .position(|a| a == "-filter_complex")
            .expect("has -filter_complex");
        let filter = &args[idx + 1];
        // One split that fans out to three branches, each tagged for later
        // `-map "[vNNNp]"` lookups.
        assert!(filter.contains("split=3"));
        assert!(filter.contains("[v360p]"));
        assert!(filter.contains("[v720p]"));
        assert!(filter.contains("[v1080p]"));
        assert!(filter.contains("scale=-2:360"));
        assert!(filter.contains("scale=-2:720"));
        assert!(filter.contains("scale=-2:1080"));
    }

    #[test]
    fn hls_profile_emits_per_variant_libx264_blocks_in_ladder_order() {
        let cmd =
            build_ffmpeg_command(Path::new("/in/a.mkv"), Path::new("/out/a.hls"), &opts_hls());
        let args = args_of(&cmd);
        // Three video output streams in order.
        assert!(args.windows(2).any(|w| w == ["-c:v:0", "libx264"]));
        assert!(args.windows(2).any(|w| w == ["-c:v:1", "libx264"]));
        assert!(args.windows(2).any(|w| w == ["-c:v:2", "libx264"]));
        // Bitrate ladder steps from the profile module.
        assert!(args.windows(2).any(|w| w == ["-b:v:0", "800k"]));
        assert!(args.windows(2).any(|w| w == ["-b:v:1", "2800k"]));
        assert!(args.windows(2).any(|w| w == ["-b:v:2", "5000k"]));
        // Profile / level escalates with bitrate so the master playlist
        // can advertise the right CODECS hints.
        assert!(args.windows(2).any(|w| w == ["-profile:v:0", "main"]));
        assert!(args.windows(2).any(|w| w == ["-profile:v:2", "high"]));
        assert!(args.windows(2).any(|w| w == ["-level:v:0", "3.0"]));
        assert!(args.windows(2).any(|w| w == ["-level:v:2", "4.0"]));
    }

    #[test]
    fn hls_profile_emits_aac_stereo_per_variant() {
        let cmd =
            build_ffmpeg_command(Path::new("/in/a.mkv"), Path::new("/out/a.hls"), &opts_hls());
        let args = args_of(&cmd);
        assert!(args.windows(2).any(|w| w == ["-c:a:0", "aac"]));
        assert!(args.windows(2).any(|w| w == ["-c:a:1", "aac"]));
        assert!(args.windows(2).any(|w| w == ["-c:a:2", "aac"]));
        assert!(args.windows(2).any(|w| w == ["-ac:a:0", "2"]));
        assert!(args.windows(2).any(|w| w == ["-b:a:0", "96k"]));
        assert!(args.windows(2).any(|w| w == ["-b:a:1", "128k"]));
    }

    #[test]
    fn hls_profile_forces_keyframes_on_segment_boundary_per_variant() {
        // Without forced keyframes ffmpeg picks its own GOP boundaries
        // and segments end up varying length / not starting on I-frames.
        let cmd =
            build_ffmpeg_command(Path::new("/in/a.mkv"), Path::new("/out/a.hls"), &opts_hls());
        let args = args_of(&cmd);
        let want = "expr:gte(t,n_forced*6)";
        for i in 0..3 {
            let key = format!("-force_key_frames:v:{i}");
            assert!(
                args.windows(2).any(|w| w[0] == key && w[1] == want),
                "missing {key} {want}"
            );
        }
    }

    #[test]
    fn hls_profile_emits_var_stream_map_naming_360p_720p_1080p() {
        let cmd =
            build_ffmpeg_command(Path::new("/in/a.mkv"), Path::new("/out/a.hls"), &opts_hls());
        let args = args_of(&cmd);
        let idx = args
            .iter()
            .position(|a| a == "-var_stream_map")
            .expect("has -var_stream_map");
        assert_eq!(
            args[idx + 1],
            "v:0,a:0,name:360p v:1,a:1,name:720p v:2,a:2,name:1080p"
        );
    }

    #[test]
    fn hls_profile_segment_and_playlist_templates_live_under_output_dir() {
        let cmd = build_ffmpeg_command(
            Path::new("/in/a.mkv"),
            Path::new("/out/Show.S01E01.hls"),
            &opts_hls(),
        );
        let args = args_of(&cmd);
        let seg_idx = args
            .iter()
            .position(|a| a == "-hls_segment_filename")
            .expect("has -hls_segment_filename");
        assert_eq!(args[seg_idx + 1], "/out/Show.S01E01.hls/%v/segment_%03d.ts");
        // Final positional arg is the variant playlist template.
        assert_eq!(
            args.last().map(String::as_str),
            Some("/out/Show.S01E01.hls/%v/playlist.m3u8")
        );
    }

    #[test]
    fn hls_profile_requests_vod_master_playlist_and_mpegts_segments() {
        let cmd =
            build_ffmpeg_command(Path::new("/in/a.mkv"), Path::new("/out/a.hls"), &opts_hls());
        let args = args_of(&cmd);
        assert!(args.windows(2).any(|w| w == ["-f", "hls"]));
        assert!(args.windows(2).any(|w| w == ["-hls_playlist_type", "vod"]));
        assert!(
            args.windows(2)
                .any(|w| w == ["-hls_segment_type", "mpegts"])
        );
        assert!(
            args.windows(2)
                .any(|w| w == ["-master_pl_name", "master.m3u8"])
        );
        // independent_segments is what lets the player switch renditions
        // without buffering across a GOP.
        assert!(
            args.windows(2)
                .any(|w| w == ["-hls_flags", "independent_segments"])
        );
    }

    #[test]
    fn hls_profile_ignores_backend_preamble_and_upscale_filter() {
        // Like Web, HLS is its own CPU x264 pipeline and must suppress any
        // backend hwaccel preamble or `Upscale` `-vf` from leaking through.
        let mut opts = opts_hls();
        opts.backend = Backend::Vaapi;
        opts.upscale = Upscale::To1080p;
        let cmd = build_ffmpeg_command(Path::new("/in/a.mkv"), Path::new("/out/a.hls"), &opts);
        let args = args_of(&cmd);
        assert!(!args.iter().any(|a| a == "-vaapi_device"));
        assert!(!args.iter().any(|a| a == "-hwaccel"));
        assert!(!args.iter().any(|a| a == "-vf"));
    }
}
