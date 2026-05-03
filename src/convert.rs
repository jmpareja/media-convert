use anyhow::{Context, Result, bail};
use std::io::{BufRead, BufReader, Read};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use crate::backend::Backend;
use crate::codec::Codec;
use crate::container::Container;

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
    /// Number of subtitle streams already inside the source video; used to
    /// compute output stream indices when tagging the language of the
    /// sidecar SRTs we add.
    pub source_subtitle_count: usize,
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
pub(crate) fn build_ffmpeg_command(
    input: &Path,
    output: &Path,
    opts: &EncodeOptions,
) -> Command {
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
    for sub in opts.subtitles {
        cmd.arg("-i").arg(&sub.path);
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
    for i in 0..opts.subtitles.len() {
        cmd.arg("-map").arg(format!("{}", i + 1));
    }

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

    cmd.args(["-c:a", "copy"]);
    match opts.container {
        Container::Mkv => {
            cmd.args(["-c:s", "copy", "-c:d", "copy", "-c:t", "copy"]);
        }
        Container::Mp4 => {
            cmd.args(["-c:s", "mov_text"]);
        }
    }

    for (i, sub) in opts.subtitles.iter().enumerate() {
        if let Some(lang) = &sub.language {
            let stream_idx = opts.source_subtitle_count + i;
            cmd.arg(format!("-metadata:s:s:{stream_idx}"))
                .arg(format!("language={lang}"));
        }
    }

    cmd.arg(output);
    cmd
}

/// Drain ffmpeg's `-progress` stream until EOF, invoking `on_progress` with a
/// fraction in [0.0, 1.0] each time `out_time` advances. If `total_secs` is
/// `None`, the fraction is `None` (caller can show indeterminate progress).
pub fn read_progress<R, F>(stdout: R, total_secs: Option<f64>, mut on_progress: F)
where
    R: Read,
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
            source_subtitle_count: 0,
        }
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
        // MKV preserves subtitle/data/attachment streams without re-encoding
        assert!(args.windows(2).any(|w| w == ["-c:s", "copy"]));
        assert!(args.windows(2).any(|w| w == ["-c:d", "copy"]));
        assert!(args.windows(2).any(|w| w == ["-c:t", "copy"]));
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
        assert!(args.windows(2).any(|w| w == ["-vf", "format=nv12,hwupload"]));
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
        let mut opts = opts_software_x265();
        opts.subtitles = &subs;
        opts.source_subtitle_count = 2;
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
        assert_eq!(inputs[0], "/in/a.mp4");
        assert_eq!(inputs[1], "/in/a.en.srt");
        assert_eq!(inputs[2], "/in/a.fr.srt");
        // Sidecar maps reference inputs 1 and 2
        assert!(args.windows(2).any(|w| w == ["-map", "1"]));
        assert!(args.windows(2).any(|w| w == ["-map", "2"]));
        // Language metadata indices start from source_subtitle_count
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
        assert_eq!(args.last().map(String::as_str), Some("/out/dir/a.mkv"));
    }

    fn collect_progress(input: &[u8], total_secs: Option<f64>) -> Vec<Option<f64>> {
        let calls = RefCell::new(Vec::new());
        read_progress(input, total_secs, |frac| calls.borrow_mut().push(frac));
        calls.into_inner()
    }

    #[test]
    fn read_progress_emits_fraction_for_out_time_us() {
        let calls = collect_progress(b"out_time_us=500000\n", Some(1.0));
        assert_eq!(calls, vec![Some(0.5)]);
    }

    #[test]
    fn read_progress_treats_out_time_ms_as_microseconds() {
        // ffmpeg's `out_time_ms` is historically microseconds, not milliseconds.
        let calls = collect_progress(b"out_time_ms=750000\n", Some(1.5));
        assert_eq!(calls, vec![Some(0.5)]);
    }

    #[test]
    fn read_progress_emits_none_when_total_unknown() {
        let calls = collect_progress(b"out_time_us=500000\n", None);
        assert_eq!(calls, vec![None]);
    }

    #[test]
    fn read_progress_clamps_overshoot() {
        let calls = collect_progress(b"out_time_us=2000000\n", Some(1.0));
        assert_eq!(calls, vec![Some(1.0)]);
    }

    #[test]
    fn read_progress_treats_negative_values_as_zero() {
        let calls = collect_progress(b"out_time_us=-100\n", Some(1.0));
        assert_eq!(calls, vec![Some(0.0)]);
    }

    #[test]
    fn read_progress_emits_one_on_end_marker() {
        let calls = collect_progress(b"progress=end\n", Some(60.0));
        assert_eq!(calls, vec![Some(1.0)]);
    }

    #[test]
    fn read_progress_ignores_unknown_keys_and_malformed_lines() {
        let input = b"frame=42\nbitrate=500kbps\nnokey\nout_time_us=invalid\nout_time_us=1000000\n";
        let calls = collect_progress(input, Some(2.0));
        assert_eq!(calls, vec![Some(0.5)]);
    }

    #[test]
    fn read_progress_emits_for_each_advance() {
        let input = b"out_time_us=250000\nout_time_us=500000\nout_time_us=750000\nprogress=end\n";
        let calls = collect_progress(input, Some(1.0));
        assert_eq!(calls, vec![Some(0.25), Some(0.5), Some(0.75), Some(1.0)]);
    }

    #[test]
    fn read_progress_strips_whitespace_around_key_and_value() {
        let calls = collect_progress(b"  out_time_us  =  500000  \n", Some(1.0));
        assert_eq!(calls, vec![Some(0.5)]);
    }
}
