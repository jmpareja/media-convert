use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::thread;

use eframe::egui;
use egui_extras::{Column, TableBuilder};

use media_convert::backend::Backend;
use media_convert::codec::Codec;
use media_convert::container::Container;
use media_convert::convert::{EncodeOptions, SubtitleInput, read_progress, spawn_encode};
use media_convert::output::{default_output_dir, sum_file_sizes, validate_output};
use media_convert::{probe, scan};

const HINT_SOURCE_FOLDER: &str =
    "Pick a directory to scan for video files. The tree under it is mirrored under the output directory.";
const HINT_SOURCE_FILE: &str =
    "Pick a single video file. Output will use the same relative name with the chosen container extension.";
const HINT_OUTPUT: &str =
    "Pick where converted files are written. The source directory tree is mirrored under this path.";

const HINT_CODEC: &str = "Target video codec for re-encoding.";
const HINT_CODEC_X265: &str =
    "HEVC / x265 — efficient compression with broad hardware decode support on modern devices.";
const HINT_CODEC_AV1: &str =
    "AV1 — better compression than x265, but software encoding is slow and decode hardware is newer / less universal.";

const HINT_BACKEND: &str =
    "Encoder backend. Software gives the smallest files; hardware backends are much faster but produce larger files at equivalent quality.";
const HINT_BACKEND_SOFTWARE: &str =
    "CPU encoders (libx265 / libsvtav1). Best compression, slowest. Works everywhere.";
const HINT_BACKEND_NVENC: &str =
    "NVIDIA NVENC (hevc_nvenc / av1_nvenc). Fast hardware encoding; requires an NVIDIA GPU with the matching codec capability.";
const HINT_BACKEND_QSV: &str =
    "Intel Quick Sync Video (hevc_qsv / av1_qsv). Requires an Intel iGPU.";
const HINT_BACKEND_VAAPI: &str =
    "VAAPI (Linux). Works with AMD and Intel iGPUs via /dev/dri/renderD128.";

const HINT_CONTAINER: &str = "Output container format.";
const HINT_CONTAINER_MKV: &str =
    "Matroska — preserves all tracks (video, audio, subtitles, attachments). Best for archival and rich subtitle support.";
const HINT_CONTAINER_MP4: &str =
    "MP4 — universal playback. Subtitles are transcoded to mov_text; image-based subs (PGS/DVB) and attachments are dropped.";

const HINT_QUALITY: &str =
    "Quality value (lower = better quality, larger files). The flag and meaningful range depend on the backend: software/nvenc/qsv ~18-30, vaapi -qp ~20-30.";
const HINT_PRESET: &str =
    "Encoder preset. libx265: ultrafast..placebo. libsvtav1: 0-13 (lower = slower / better). nvenc: p1..p7. qsv: veryfast..veryslow. vaapi: ignored.";

const HINT_FORCE: &str =
    "Re-encode files even when their video stream is already in the target codec.";
const HINT_RECURSE: &str = "Walk into subdirectories of the source folder when scanning.";
const HINT_EMBED_SUBTITLES: &str =
    "Auto-discover sidecar .srt files (e.g. movie.srt, movie.en.srt next to movie.mp4) and mux them into the output as subtitle tracks.";
const HINT_SHOW_ONLY_ENCODE: &str =
    "Hide files that are already in the target codec or where the output already exists.";

fn main() -> eframe::Result<()> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1100.0, 700.0])
            .with_min_inner_size([700.0, 400.0]),
        ..Default::default()
    };
    eframe::run_native(
        "media-convert",
        options,
        Box::new(|_cc| Ok(Box::new(App::default()))),
    )
}

#[derive(Clone, Debug)]
enum Decision {
    Encode,
    SkipAlreadyTarget,
    SkipOutputExists,
}

#[derive(Clone, Debug)]
enum Status {
    Pending,
    Encoding(Option<f64>),
    Done,
    Failed(String),
    Canceled,
}

#[derive(Clone, Debug)]
struct FileEntry {
    abs: PathBuf,
    rel: PathBuf,
    output: PathBuf,
    source_codec: Option<String>,
    duration_secs: Option<f64>,
    source_subtitle_count: usize,
    decision: Decision,
    status: Status,
}

enum ScanMsg {
    Discovered(FileEntry),
    Error(String),
    Done(usize), // total scanned
}

enum EncodeMsg {
    Started(usize),
    Progress(usize, f64),
    Finished(usize, std::result::Result<(), String>),
    Canceled(usize),
    AllDone,
}

struct ScanWorker {
    rx: Receiver<ScanMsg>,
    cancel: Arc<AtomicBool>,
}

struct EncodeWorker {
    rx: Receiver<EncodeMsg>,
    cancel: Arc<AtomicBool>,
    current_child: Arc<Mutex<Option<Child>>>,
}

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
enum Severity {
    Warning,
    Error,
}

struct Notification {
    severity: Severity,
    messages: Vec<String>,
}

struct App {
    source: Option<PathBuf>,
    output: Option<PathBuf>,
    codec: Codec,
    backend: Backend,
    container: Container,
    quality: u8,
    preset: String,
    force: bool,
    recurse: bool,
    embed_subtitles: bool,

    files: Vec<FileEntry>,

    scanner: Option<ScanWorker>,
    encoder: Option<EncodeWorker>,

    log: Vec<String>,
    notification: Option<Notification>,
    selected_only_encode: bool,
    startup_checked: bool,
}

impl Default for App {
    fn default() -> Self {
        let codec = Codec::X265;
        let backend = Backend::Software;
        let cfg = backend.config(codec);
        Self {
            source: None,
            output: None,
            codec,
            backend,
            container: Container::Mkv,
            quality: cfg.default_quality,
            preset: cfg.default_preset.unwrap_or("").to_string(),
            force: false,
            recurse: true,
            embed_subtitles: false,
            files: Vec::new(),
            scanner: None,
            encoder: None,
            log: Vec::new(),
            notification: None,
            selected_only_encode: true,
            startup_checked: false,
        }
    }
}

impl App {
    fn reset_to_backend_defaults(&mut self) {
        let cfg = self.backend.config(self.codec);
        self.quality = cfg.default_quality;
        self.preset = cfg.default_preset.unwrap_or("").to_string();
    }

    /// Update `self.source`, and pre-fill `self.output` with a sensible
    /// default if the user hasn't picked one yet. Existing user choices for
    /// output are preserved so the user can override the suggestion.
    fn set_source(&mut self, source: PathBuf) {
        if self.output.is_none() {
            self.output = Some(default_output_dir(&source));
        }
        self.source = Some(source);
    }
}

impl App {
    fn log(&mut self, msg: impl Into<String>) {
        let s = msg.into();
        eprintln!("{s}");
        self.log.push(s);
        if self.log.len() > 500 {
            let drop = self.log.len() - 500;
            self.log.drain(..drop);
        }
    }

    fn notify(&mut self, severity: Severity, msg: impl Into<String>) {
        let s = msg.into();
        self.log(s.clone());
        match &mut self.notification {
            Some(n) => {
                if severity == Severity::Error {
                    n.severity = Severity::Error;
                }
                n.messages.push(s);
            }
            None => {
                self.notification = Some(Notification {
                    severity,
                    messages: vec![s],
                });
            }
        }
    }

    fn notify_error(&mut self, msg: impl Into<String>) {
        self.notify(Severity::Error, msg);
    }

    fn notify_warning(&mut self, msg: impl Into<String>) {
        self.notify(Severity::Warning, msg);
    }

    fn show_notification(&mut self, ctx: &egui::Context) {
        let Some(notif) = &self.notification else {
            return;
        };
        let (title, color) = match notif.severity {
            Severity::Error => ("Error", egui::Color32::from_rgb(220, 80, 80)),
            Severity::Warning => ("Warning", egui::Color32::from_rgb(220, 160, 60)),
        };
        let messages = notif.messages.clone();

        let mut close = false;
        egui::Modal::new(egui::Id::new("notification_modal")).show(ctx, |ui| {
            ui.set_min_width(420.0);
            ui.set_max_width(640.0);
            ui.colored_label(color, egui::RichText::new(title).heading().strong());
            ui.separator();
            egui::ScrollArea::vertical()
                .max_height(300.0)
                .show(ui, |ui| {
                    for (i, m) in messages.iter().enumerate() {
                        if i > 0 {
                            ui.add_space(6.0);
                        }
                        ui.label(m);
                    }
                });
            ui.add_space(8.0);
            ui.horizontal(|ui| {
                if ui.button("OK").clicked() {
                    close = true;
                }
            });
        });
        if close {
            self.notification = None;
        }
    }

    fn pick_dir(&mut self, kind: &str) -> Option<PathBuf> {
        let start = match kind {
            "source" => self.source.clone(),
            _ => self.output.clone(),
        };
        let mut dlg = rfd::FileDialog::new();
        if let Some(p) = start {
            dlg = dlg.set_directory(p);
        }
        dlg.pick_folder()
    }

    fn pick_source_file(&mut self) -> Option<PathBuf> {
        let start = self
            .source
            .as_ref()
            .and_then(|p| if p.is_file() { p.parent() } else { Some(p.as_path()) })
            .map(Path::to_path_buf);
        let mut dlg = rfd::FileDialog::new().add_filter(
            "Video",
            &["mkv", "mp4", "avi", "mov", "wmv", "flv", "webm", "m4v", "ts", "mpg", "mpeg", "m2ts"],
        );
        if let Some(p) = start {
            dlg = dlg.set_directory(p);
        }
        dlg.pick_file()
    }

    fn start_scan(&mut self) {
        let Some(source) = self.source.clone() else {
            self.notify_error("Pick a source folder or file before scanning.");
            return;
        };
        let Some(output) = self.output.clone() else {
            self.notify_error("Pick an output directory before scanning.");
            return;
        };
        if !source.is_dir() && !source.is_file() {
            self.notify_error(format!("Source does not exist:\n{}", source.display()));
            return;
        }
        if source.is_file() && !media_convert::scan::is_video(&source) {
            self.notify_error(format!(
                "Source file is not a recognised video format:\n{}",
                source.display()
            ));
            return;
        }

        // Confirm the output directory is writable before kicking off a scan.
        // Disk-space warnings happen later (at start_encode) once we know the
        // total source size.
        match validate_output(&output, None) {
            Ok(v) => {
                for w in v.warnings {
                    self.notify_warning(w);
                }
            }
            Err(e) => {
                self.notify_error(format!("Output directory is not usable:\n{e:#}"));
                return;
            }
        }

        self.files.clear();

        let codec = self.codec;
        let container = self.container;
        let force = self.force;
        let recurse = self.recurse;
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_thread = cancel.clone();
        let (tx, rx) = channel();

        thread::spawn(move || {
            let videos = scan::find_videos(&source, recurse);
            let rel_root: PathBuf = if source.is_file() {
                source.parent().map(Path::to_path_buf).unwrap_or_default()
            } else {
                source.clone()
            };
            for abs in videos.iter() {
                if cancel_thread.load(Ordering::Relaxed) {
                    break;
                }
                let rel = abs
                    .strip_prefix(&rel_root)
                    .unwrap_or(abs.as_path())
                    .to_path_buf();
                let mut out = output.join(&rel);
                out.set_extension(container.extension());

                let (source_codec, duration_secs, source_subtitle_count, decision) =
                    if out.exists() {
                        (None, None, 0, Decision::SkipOutputExists)
                    } else {
                        match probe::video_info(abs) {
                            Ok(info) => {
                                let dec = if force || !codec.matches_source(&info.codec) {
                                    Decision::Encode
                                } else {
                                    Decision::SkipAlreadyTarget
                                };
                                (
                                    Some(info.codec),
                                    info.duration_secs,
                                    info.subtitle_count,
                                    dec,
                                )
                            }
                            Err(e) => {
                                let _ = tx.send(ScanMsg::Error(format!(
                                    "probe failed for {}: {e:#}",
                                    rel.display()
                                )));
                                continue;
                            }
                        }
                    };

                let entry = FileEntry {
                    abs: abs.clone(),
                    rel,
                    output: out,
                    source_codec,
                    duration_secs,
                    source_subtitle_count,
                    decision,
                    status: Status::Pending,
                };
                if tx.send(ScanMsg::Discovered(entry)).is_err() {
                    return;
                }
            }
            let _ = tx.send(ScanMsg::Done(videos.len()));
        });

        self.scanner = Some(ScanWorker { rx, cancel });
        self.log("scan: started");
    }

    fn cancel_scan(&mut self) {
        if let Some(s) = &self.scanner {
            s.cancel.store(true, Ordering::Relaxed);
            self.log("scan: cancel requested");
        }
    }

    fn start_encode(&mut self) {
        if self.encoder.is_some() {
            self.notify_warning("Encode is already running.");
            return;
        }

        // Re-check the output directory now that we know the total source
        // size — this surfaces low-disk-space warnings before we start
        // burning encoder time.
        if let Some(output) = self.output.clone() {
            let to_encode_paths: Vec<PathBuf> = self
                .files
                .iter()
                .filter(|f| matches!(f.decision, Decision::Encode))
                .filter(|f| !matches!(f.status, Status::Done))
                .map(|f| f.abs.clone())
                .collect();
            let total = sum_file_sizes(&to_encode_paths);
            match validate_output(&output, Some(total)) {
                Ok(v) => {
                    for w in v.warnings {
                        self.notify_warning(w);
                    }
                }
                Err(e) => {
                    self.notify_error(format!("Output directory is not usable:\n{e:#}"));
                    return;
                }
            }
        }

        let embed_subtitles = self.embed_subtitles;
        let jobs: Vec<(usize, FileEntry, Vec<SubtitleInput>)> = self
            .files
            .iter()
            .enumerate()
            .filter(|(_, f)| matches!(f.decision, Decision::Encode))
            .filter(|(_, f)| !matches!(f.status, Status::Done))
            .map(|(i, f)| {
                let subs = if embed_subtitles {
                    media_convert::scan::discover_subtitles(&f.abs)
                        .into_iter()
                        .map(|s| SubtitleInput {
                            path: s.path,
                            language: s.language,
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                (i, f.clone(), subs)
            })
            .collect();

        if jobs.is_empty() {
            self.notify_warning("Nothing to encode. Run Scan first, or there are no encodable files.");
            return;
        }

        let codec = self.codec;
        let backend = self.backend;
        let container = self.container;
        let quality = self.quality;
        let preset = self.preset.clone();
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_thread = cancel.clone();
        let current_child = Arc::new(Mutex::new(None::<Child>));
        let current_child_thread = current_child.clone();
        let (tx, rx) = channel::<EncodeMsg>();

        thread::spawn(move || {
            let preset_opt: Option<&str> = if preset.is_empty() {
                None
            } else {
                Some(preset.as_str())
            };

            for (idx, entry, subs) in jobs {
                if cancel_thread.load(Ordering::Relaxed) {
                    break;
                }
                if tx.send(EncodeMsg::Started(idx)).is_err() {
                    break;
                }
                let opts = EncodeOptions {
                    codec,
                    backend,
                    container,
                    quality,
                    preset: preset_opt,
                    subtitles: &subs,
                    source_subtitle_count: entry.source_subtitle_count,
                };
                let result = run_one(
                    &entry.abs,
                    &entry.output,
                    &opts,
                    &current_child_thread,
                    idx,
                    entry.duration_secs,
                    &tx,
                );
                if cancel_thread.load(Ordering::Relaxed) {
                    let _ = std::fs::remove_file(&entry.output);
                    let _ = tx.send(EncodeMsg::Canceled(idx));
                    break;
                }
                let _ = tx.send(EncodeMsg::Finished(
                    idx,
                    result.map_err(|e| format!("{e:#}")),
                ));
            }
            let _ = tx.send(EncodeMsg::AllDone);
        });

        self.encoder = Some(EncodeWorker {
            rx,
            cancel,
            current_child,
        });
        self.log("encode: started");
    }

    fn cancel_encode(&mut self) {
        if let Some(e) = &self.encoder {
            e.cancel.store(true, Ordering::Relaxed);
            if let Ok(mut guard) = e.current_child.lock()
                && let Some(child) = guard.as_mut()
            {
                let _ = child.kill();
            }
            self.log("encode: cancel requested");
        }
    }

    fn drain_workers(&mut self, ctx: &egui::Context) {
        let mut needs_repaint = false;
        let mut deferred_log: Vec<String> = Vec::new();
        let mut deferred_warnings: Vec<String> = Vec::new();
        let mut clear_scanner = false;
        let mut clear_encoder = false;

        if let Some(s) = &self.scanner {
            loop {
                match s.rx.try_recv() {
                    Ok(ScanMsg::Discovered(entry)) => {
                        self.files.push(entry);
                        needs_repaint = true;
                    }
                    Ok(ScanMsg::Error(msg)) => {
                        deferred_warnings.push(msg);
                        needs_repaint = true;
                    }
                    Ok(ScanMsg::Done(n)) => {
                        deferred_log.push(format!("scan: complete, {n} file(s)"));
                        clear_scanner = true;
                        needs_repaint = true;
                        break;
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        clear_scanner = true;
                        break;
                    }
                }
            }
        }

        if let Some(e) = &self.encoder {
            loop {
                match e.rx.try_recv() {
                    Ok(EncodeMsg::Started(idx)) => {
                        if let Some(f) = self.files.get_mut(idx) {
                            f.status = Status::Encoding(None);
                        }
                        needs_repaint = true;
                    }
                    Ok(EncodeMsg::Progress(idx, frac)) => {
                        if let Some(f) = self.files.get_mut(idx)
                            && matches!(f.status, Status::Encoding(_))
                        {
                            f.status = Status::Encoding(Some(frac));
                        }
                        needs_repaint = true;
                    }
                    Ok(EncodeMsg::Finished(idx, result)) => {
                        if let Some(f) = self.files.get_mut(idx) {
                            let rel_display = f.rel.display().to_string();
                            match result {
                                Ok(()) => {
                                    f.status = Status::Done;
                                    deferred_log.push(format!("ok: {rel_display}"));
                                }
                                Err(msg) => {
                                    f.status = Status::Failed(msg.clone());
                                    deferred_warnings
                                        .push(format!("Encode failed: {rel_display}\n{msg}"));
                                }
                            }
                        }
                        needs_repaint = true;
                    }
                    Ok(EncodeMsg::Canceled(idx)) => {
                        if let Some(f) = self.files.get_mut(idx) {
                            f.status = Status::Canceled;
                            deferred_log.push(format!("canceled: {}", f.rel.display()));
                        }
                        needs_repaint = true;
                    }
                    Ok(EncodeMsg::AllDone) => {
                        deferred_log.push("encode: complete".into());
                        clear_encoder = true;
                        needs_repaint = true;
                        break;
                    }
                    Err(std::sync::mpsc::TryRecvError::Empty) => break,
                    Err(std::sync::mpsc::TryRecvError::Disconnected) => {
                        clear_encoder = true;
                        break;
                    }
                }
            }
        }

        if clear_scanner {
            self.scanner = None;
        }
        if clear_encoder {
            self.encoder = None;
        }
        for line in deferred_log {
            self.log(line);
        }
        for line in deferred_warnings {
            self.notify_warning(line);
        }

        if self.scanner.is_some() || self.encoder.is_some() {
            ctx.request_repaint_after(std::time::Duration::from_millis(150));
        } else if needs_repaint {
            ctx.request_repaint();
        }
    }
}

fn is_on_path(prog: &str) -> bool {
    Command::new(prog)
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .is_ok()
}

fn run_one(
    input: &Path,
    output: &Path,
    opts: &EncodeOptions,
    slot: &Arc<Mutex<Option<Child>>>,
    idx: usize,
    duration_secs: Option<f64>,
    tx: &Sender<EncodeMsg>,
) -> anyhow::Result<()> {
    use anyhow::{anyhow, bail};

    let mut child = spawn_encode(input, output, opts)?;
    let stdout = child.stdout.take();
    {
        let mut guard = slot.lock().expect("child slot poisoned");
        *guard = Some(child);
    }

    let progress_thread = stdout.map(|stdout| {
        let tx = tx.clone();
        thread::spawn(move || {
            read_progress(stdout, duration_secs, |frac| {
                if let Some(f) = frac {
                    let _ = tx.send(EncodeMsg::Progress(idx, f));
                }
            });
        })
    });

    // Poll for completion with brief locks so cancel_encode can grab the
    // lock and call Child::kill() (SIGKILL) without waiting hours for the
    // current encode to finish.
    let status = loop {
        {
            let mut guard = slot.lock().expect("child slot poisoned");
            match guard.as_mut() {
                Some(child) => match child.try_wait() {
                    Ok(Some(status)) => break status,
                    Ok(None) => { /* still running */ }
                    Err(e) => return Err(anyhow!("waiting on ffmpeg: {e}")),
                },
                None => bail!("ffmpeg child slot cleared unexpectedly"),
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    };

    {
        let mut guard = slot.lock().expect("child slot poisoned");
        *guard = None;
    }

    if let Some(h) = progress_thread {
        let _ = h.join();
    }

    if !status.success() {
        let _ = std::fs::remove_file(output);
        bail!("ffmpeg exited with {}", status);
    }
    Ok(())
}

impl eframe::App for App {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        if !self.startup_checked {
            self.startup_checked = true;
            for prog in ["ffmpeg", "ffprobe"] {
                if !is_on_path(prog) {
                    self.notify_error(format!(
                        "`{prog}` not found on PATH. Install ffmpeg before running encodes."
                    ));
                }
            }
        }

        self.drain_workers(ctx);
        self.show_notification(ctx);

        egui::TopBottomPanel::top("top").show(ctx, |ui| {
            ui.add_space(4.0);
            ui.horizontal(|ui| {
                if ui
                    .button("Source folder…")
                    .on_hover_text(HINT_SOURCE_FOLDER)
                    .clicked()
                    && let Some(p) = self.pick_dir("source")
                {
                    self.set_source(p);
                }
                if ui
                    .button("Source file…")
                    .on_hover_text(HINT_SOURCE_FILE)
                    .clicked()
                    && let Some(p) = self.pick_source_file()
                {
                    self.set_source(p);
                }
                ui.label(
                    self.source
                        .as_ref()
                        .map(|p| {
                            let kind = if p.is_file() { "file" } else { "folder" };
                            format!("{} [{kind}]", p.display())
                        })
                        .unwrap_or_else(|| "(none)".into()),
                );
            });
            ui.horizontal(|ui| {
                if ui
                    .button("Output…")
                    .on_hover_text(HINT_OUTPUT)
                    .clicked()
                    && let Some(p) = self.pick_dir("output")
                {
                    self.output = Some(p);
                }
                ui.label(
                    self.output
                        .as_ref()
                        .map(|p| p.display().to_string())
                        .unwrap_or_else(|| "(none)".into()),
                );
            });
            ui.horizontal(|ui| {
                ui.label("Codec:").on_hover_text(HINT_CODEC);
                let prev_codec = self.codec;
                egui::ComboBox::from_id_salt("codec_combo")
                    .selected_text(self.codec.label())
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.codec, Codec::X265, "x265")
                            .on_hover_text(HINT_CODEC_X265);
                        ui.selectable_value(&mut self.codec, Codec::Av1, "av1")
                            .on_hover_text(HINT_CODEC_AV1);
                    })
                    .response
                    .on_hover_text(HINT_CODEC);
                ui.separator();
                ui.label("Backend:").on_hover_text(HINT_BACKEND);
                let prev_backend = self.backend;
                egui::ComboBox::from_id_salt("backend_combo")
                    .selected_text(self.backend.label())
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.backend, Backend::Software, "software")
                            .on_hover_text(HINT_BACKEND_SOFTWARE);
                        ui.selectable_value(&mut self.backend, Backend::Nvenc, "nvenc")
                            .on_hover_text(HINT_BACKEND_NVENC);
                        ui.selectable_value(&mut self.backend, Backend::Qsv, "qsv")
                            .on_hover_text(HINT_BACKEND_QSV);
                        ui.selectable_value(&mut self.backend, Backend::Vaapi, "vaapi")
                            .on_hover_text(HINT_BACKEND_VAAPI);
                    })
                    .response
                    .on_hover_text(HINT_BACKEND);
                if self.codec != prev_codec || self.backend != prev_backend {
                    self.reset_to_backend_defaults();
                }
                ui.separator();
                ui.label("Container:").on_hover_text(HINT_CONTAINER);
                egui::ComboBox::from_id_salt("container_combo")
                    .selected_text(self.container.label())
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut self.container, Container::Mkv, "mkv")
                            .on_hover_text(HINT_CONTAINER_MKV);
                        ui.selectable_value(&mut self.container, Container::Mp4, "mp4")
                            .on_hover_text(HINT_CONTAINER_MP4);
                    })
                    .response
                    .on_hover_text(HINT_CONTAINER);
                let cfg = self.backend.config(self.codec);
                ui.separator();
                ui.label(format!("{}:", cfg.quality_flag.trim_start_matches('-')))
                    .on_hover_text(HINT_QUALITY);
                ui.add(egui::Slider::new(&mut self.quality, 0..=63))
                    .on_hover_text(HINT_QUALITY);
                ui.separator();
                ui.label("Preset:").on_hover_text(HINT_PRESET);
                ui.add_enabled(
                    cfg.default_preset.is_some(),
                    egui::TextEdit::singleline(&mut self.preset).desired_width(80.0),
                )
                .on_hover_text(HINT_PRESET);
                ui.separator();
                ui.checkbox(&mut self.force, "Force re-encode")
                    .on_hover_text(HINT_FORCE);
                if self.source.as_deref().is_some_and(Path::is_dir) {
                    ui.separator();
                    ui.checkbox(&mut self.recurse, "Recurse subdirectories")
                        .on_hover_text(HINT_RECURSE);
                }
                ui.separator();
                ui.checkbox(&mut self.embed_subtitles, "Embed sidecar SRT subtitles")
                    .on_hover_text(HINT_EMBED_SUBTITLES);
            });
            ui.horizontal(|ui| {
                let scanning = self.scanner.is_some();
                let encoding = self.encoder.is_some();
                ui.add_enabled_ui(!scanning && !encoding, |ui| {
                    if ui.button("Scan").clicked() {
                        self.start_scan();
                    }
                });
                ui.add_enabled_ui(scanning, |ui| {
                    if ui.button("Cancel scan").clicked() {
                        self.cancel_scan();
                    }
                });
                ui.separator();
                ui.add_enabled_ui(!encoding && !self.files.is_empty(), |ui| {
                    if ui.button("Convert").clicked() {
                        self.start_encode();
                    }
                });
                ui.add_enabled_ui(encoding, |ui| {
                    if ui.button("Stop").clicked() {
                        self.cancel_encode();
                    }
                });
                ui.separator();
                ui.checkbox(&mut self.selected_only_encode, "Show only to-encode")
                    .on_hover_text(HINT_SHOW_ONLY_ENCODE);
            });
            ui.add_space(4.0);
        });

        egui::TopBottomPanel::bottom("bottom")
            .resizable(true)
            .default_height(140.0)
            .show(ctx, |ui| {
                ui.label("Log:");
                egui::ScrollArea::vertical()
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        for line in &self.log {
                            ui.monospace(line);
                        }
                    });
            });

        egui::CentralPanel::default().show(ctx, |ui| {
            let summary = summarize(&self.files);
            ui.label(format!(
                "Files: {total}  |  to encode: {to_encode}  |  done: {done}  |  failed: {failed}  |  skipped: {skipped}",
                total = summary.total,
                to_encode = summary.to_encode,
                done = summary.done,
                failed = summary.failed,
                skipped = summary.skipped,
            ));
            ui.separator();

            let only_encode = self.selected_only_encode;
            TableBuilder::new(ui)
                .striped(true)
                .resizable(true)
                .column(Column::remainder().at_least(200.0))
                .column(Column::auto().at_least(80.0))
                .column(Column::auto().at_least(120.0))
                .column(Column::auto().at_least(200.0))
                .header(20.0, |mut header| {
                    header.col(|ui| { ui.strong("File"); });
                    header.col(|ui| { ui.strong("Source codec"); });
                    header.col(|ui| { ui.strong("Decision"); });
                    header.col(|ui| { ui.strong("Status"); });
                })
                .body(|mut body| {
                    for f in self.files.iter().filter(|f| {
                        !only_encode || matches!(f.decision, Decision::Encode)
                    }) {
                        body.row(18.0, |mut row| {
                            row.col(|ui| {
                                ui.monospace(f.rel.display().to_string());
                            });
                            row.col(|ui| {
                                ui.label(f.source_codec.as_deref().unwrap_or("-"));
                            });
                            row.col(|ui| {
                                ui.label(decision_label(&f.decision));
                            });
                            row.col(|ui| {
                                ui.label(status_label(&f.status));
                            });
                        });
                    }
                });
        });
    }
}

#[derive(Default)]
struct Summary {
    total: usize,
    to_encode: usize,
    done: usize,
    failed: usize,
    skipped: usize,
}

fn summarize(files: &[FileEntry]) -> Summary {
    let mut s = Summary {
        total: files.len(),
        ..Default::default()
    };
    for f in files {
        match (&f.decision, &f.status) {
            (_, Status::Done) => s.done += 1,
            (_, Status::Failed(_)) => s.failed += 1,
            (Decision::SkipAlreadyTarget | Decision::SkipOutputExists, _) => s.skipped += 1,
            (Decision::Encode, _) => s.to_encode += 1,
        }
    }
    s
}

fn decision_label(d: &Decision) -> &'static str {
    match d {
        Decision::Encode => "encode",
        Decision::SkipAlreadyTarget => "skip (already target)",
        Decision::SkipOutputExists => "skip (output exists)",
    }
}

fn status_label(s: &Status) -> String {
    match s {
        Status::Pending => "pending".into(),
        Status::Encoding(None) => "encoding…".into(),
        Status::Encoding(Some(p)) => format!("encoding {:>3}%", (p * 100.0) as u32),
        Status::Done => "done".into(),
        Status::Failed(msg) => format!("failed: {msg}"),
        Status::Canceled => "canceled".into(),
    }
}
