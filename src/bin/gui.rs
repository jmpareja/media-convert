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
use media_convert::inhibit::Inhibitor;
use media_convert::output::{sum_file_sizes, validate_output};
use media_convert::scan::SidecarSubtitle;
use media_convert::{probe, scan};

const HINT_SOURCE_FOLDER: &str = "Pick a directory to scan for video files. The tree under it is mirrored under the output directory.";
const HINT_SOURCE_FILE: &str = "Pick a single video file. Output will use the same relative name with the chosen container extension.";
const HINT_OUTPUT: &str = "Pick where converted files are written. The source directory tree is mirrored under this path.";

const HINT_CODEC: &str = "Target video codec for re-encoding.";
const HINT_CODEC_X265: &str =
    "HEVC / x265 — efficient compression with broad hardware decode support on modern devices.";
const HINT_CODEC_AV1: &str = "AV1 — better compression than x265, but software encoding is slow and decode hardware is newer / less universal.";

const HINT_BACKEND: &str = "Encoder backend. Software gives the smallest files; hardware backends are much faster but produce larger files at equivalent quality.";
const HINT_BACKEND_SOFTWARE: &str =
    "CPU encoders (libx265 / libsvtav1). Best compression, slowest. Works everywhere.";
const HINT_BACKEND_NVENC: &str = "NVIDIA NVENC (hevc_nvenc / av1_nvenc). Fast hardware encoding; requires an NVIDIA GPU with the matching codec capability.";
const HINT_BACKEND_QSV: &str =
    "Intel Quick Sync Video (hevc_qsv / av1_qsv). Requires an Intel iGPU.";
const HINT_BACKEND_VAAPI: &str =
    "VAAPI (Linux). Works with AMD and Intel iGPUs via /dev/dri/renderD128.";

const HINT_CONTAINER: &str = "Output container format.";
const HINT_CONTAINER_MKV: &str = "Matroska — preserves all tracks (video, audio, subtitles, attachments). Best for archival and rich subtitle support.";
const HINT_CONTAINER_MP4: &str = "MP4 — universal playback. Subtitles are transcoded to mov_text; image-based subs (PGS/DVB) and attachments are dropped.";

const HINT_QUALITY: &str = "Quality value (lower = better quality, larger files). The flag and meaningful range depend on the backend: software/nvenc/qsv ~18-30, vaapi -qp ~20-30.";
const HINT_PRESET: &str = "Encoder preset. libx265: ultrafast..placebo. libsvtav1: 0-13 (lower = slower / better). nvenc: p1..p7. qsv: veryfast..veryslow. vaapi: ignored.";

const HINT_FORCE: &str =
    "Re-encode files even when their video stream is already in the target codec.";
const HINT_RECURSE: &str = "Walk into subdirectories of the source folder when scanning.";
const HINT_EMBED_SUBTITLES: &str = "Auto-discover sidecar .srt files (e.g. movie.srt, movie.en.srt next to movie.mp4) and mux them into the output as subtitle tracks.";
const HINT_MERGE_SUBTITLES: &str = "Skip re-encoding entirely — just stream-copy each video and mux in any sidecar .srt files. Files with no sidecar SRTs are skipped. Codec / quality / preset settings are ignored in this mode.";

// Status palette — muted, JetBrains/VSCode-leaning accents. Saturated primary
// colors compete with the rest of the UI; these read calmer at a glance.
const STATUS_ENCODE: egui::Color32 = egui::Color32::from_rgb(110, 175, 145);
const STATUS_ENCODING: egui::Color32 = egui::Color32::from_rgb(100, 150, 200);
const STATUS_DONE: egui::Color32 = egui::Color32::from_rgb(130, 175, 110);
const STATUS_FAILED: egui::Color32 = egui::Color32::from_rgb(200, 110, 110);
const STATUS_CANCELED: egui::Color32 = egui::Color32::from_rgb(200, 160, 90);

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
        Box::new(|cc| {
            // Keep egui's default font sizes (denser, more conventional look).
            // Subtle rounded corners on widgets without inflating the type scale.
            let mut style = (*cc.egui_ctx.style()).clone();
            style.visuals.window_corner_radius = egui::CornerRadius::same(6);
            style.visuals.widgets.noninteractive.corner_radius = egui::CornerRadius::same(3);
            style.visuals.widgets.inactive.corner_radius = egui::CornerRadius::same(3);
            style.visuals.widgets.hovered.corner_radius = egui::CornerRadius::same(3);
            style.visuals.widgets.active.corner_radius = egui::CornerRadius::same(3);
            style.visuals.widgets.open.corner_radius = egui::CornerRadius::same(3);
            cc.egui_ctx.set_style(style);

            Ok(Box::new(App::default()))
        }),
    )
}

#[derive(Clone, Debug)]
enum Decision {
    Encode,
    SkipAlreadyTarget,
    SkipOutputExists,
    SkipNoSubtitles,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum FilterState {
    All,
    ToEncode,
    Done,
    Failed,
}

/// Where converted files go.
///
/// `Single` — one output directory (or, for a single-file source, one explicit
/// output filepath) that mirrors the source tree underneath it.
///
/// `PerSource` — each source produces its output next to itself: a file source
/// becomes `<stem>-converted.<ext>` in the same dir, a directory source
/// becomes a sibling `<dirname>-converted/` mirror. Used when the user picks
/// multiple sources where a single shared root would force collisions.
#[derive(Clone, Debug, PartialEq, Eq)]
enum OutputDest {
    Single(PathBuf),
    PerSource,
}

impl OutputDest {
    fn display_label(&self) -> String {
        match self {
            OutputDest::Single(p) => p.display().to_string(),
            OutputDest::PerSource => "(next to each source)".into(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SortColumn {
    Name,
    Resolution,
    Bitrate,
    Codec,
    Status,
}

#[derive(Clone, Debug)]
enum Status {
    Pending,
    Encoding(media_convert::convert::ProgressInfo),
    Done,
    Failed(String),
    Canceled,
}

#[derive(Clone, Debug)]
enum MetaState {
    NotLoaded,
    Loading,
    Loaded(probe::FileMetadata),
    Failed(String),
}

#[derive(Clone, Debug)]
struct FileEntry {
    abs: PathBuf,
    rel: PathBuf,
    output: PathBuf,
    source_codec: Option<String>,
    width: Option<u32>,
    height: Option<u32>,
    bit_rate: Option<u64>,
    duration_secs: Option<f64>,
    source_subtitle_codecs: Vec<String>,
    unmappable_stream_indices: Vec<usize>,
    /// Sidecar SRTs discovered at scan time. Only populated when merge mode
    /// is on (so we can drive the SkipNoSubtitles decision); in normal
    /// encode mode this stays empty and discovery happens at encode start.
    discovered_subtitles: Vec<SidecarSubtitle>,
    decision: Decision,
    status: Status,
    metadata: MetaState,
}

enum ScanMsg {
    Discovered(FileEntry),
    Error(String),
    Done(usize), // total scanned
}

enum EncodeMsg {
    Started(usize),
    Progress(usize, media_convert::convert::ProgressInfo),
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
    sources: Vec<PathBuf>,
    output: Option<OutputDest>,
    codec: Codec,
    backend: Backend,
    container: Container,
    quality: u8,
    preset: String,
    force: bool,
    recurse: bool,
    embed_subtitles: bool,
    merge_subtitles: bool,
    show_advanced: bool,

    files: Vec<FileEntry>,

    scanner: Option<ScanWorker>,
    encoder: Option<EncodeWorker>,

    log: Vec<String>,
    notification: Option<Notification>,
    startup_checked: bool,

    search_query: String,
    filter_state: FilterState,
    sort_column: Option<SortColumn>,
    sort_ascending: bool,
    selected_file_index: Option<usize>,
    metadata_tx: Sender<(PathBuf, std::result::Result<probe::FileMetadata, String>)>,
    metadata_rx: Receiver<(PathBuf, std::result::Result<probe::FileMetadata, String>)>,
}

impl Default for App {
    fn default() -> Self {
        let codec = Codec::X265;
        let backend = Backend::Software;
        let cfg = backend.config(codec);
        let (metadata_tx, metadata_rx) = channel();
        Self {
            sources: Vec::new(),
            output: None,
            codec,
            backend,
            container: Container::Mkv,
            quality: cfg.default_quality,
            preset: cfg.default_preset.unwrap_or("").to_string(),
            force: false,
            recurse: true,
            embed_subtitles: false,
            merge_subtitles: false,
            show_advanced: false,
            files: Vec::new(),
            scanner: None,
            encoder: None,
            log: Vec::new(),
            notification: None,
            startup_checked: false,
            search_query: String::new(),
            filter_state: FilterState::All,
            sort_column: None,
            sort_ascending: true,
            selected_file_index: None,
            metadata_tx,
            metadata_rx,
        }
    }
}

impl App {
    fn reset_to_backend_defaults(&mut self) {
        let cfg = self.backend.config(self.codec);
        self.quality = cfg.default_quality;
        self.preset = cfg.default_preset.unwrap_or("").to_string();
    }

    /// Update `self.sources`, and pre-fill `self.output` with a sensible
    /// default if the user hasn't picked one yet. Existing user choices for
    /// output are preserved so the user can override the suggestion.
    fn set_sources(&mut self, sources: Vec<PathBuf>) {
        if self.output.is_none() && !sources.is_empty() {
            // Multiple sources of any kind would collide if forced under a single
            // shared output root, so each gets a per-source default.
            self.output = Some(if sources.len() > 1 {
                OutputDest::PerSource
            } else {
                OutputDest::Single(media_convert::output::default_output(
                    &sources[0],
                    self.container,
                ))
            });
        }
        self.sources = sources;
    }

    fn filtered_indices(&self) -> Vec<usize> {
        let mut indices: Vec<_> = (0..self.files.len())
            .filter(|&i| {
                let f = &self.files[i];
                let matches_search = self.search_query.is_empty()
                    || f.rel
                        .to_string_lossy()
                        .to_lowercase()
                        .contains(&self.search_query.to_lowercase());
                let matches_filter = match self.filter_state {
                    FilterState::All => true,
                    FilterState::ToEncode => {
                        matches!(f.decision, Decision::Encode) && !matches!(f.status, Status::Done)
                    }
                    FilterState::Done => matches!(f.status, Status::Done),
                    FilterState::Failed => matches!(f.status, Status::Failed(_)),
                };
                matches_search && matches_filter
            })
            .collect();

        if let Some(sort_col) = self.sort_column {
            indices.sort_by(|&a_idx, &b_idx| {
                let a = &self.files[a_idx];
                let b = &self.files[b_idx];
                let cmp = match sort_col {
                    SortColumn::Name => a.rel.cmp(&b.rel),
                    SortColumn::Resolution => {
                        let a_res = a.width.unwrap_or(0) * a.height.unwrap_or(0);
                        let b_res = b.width.unwrap_or(0) * b.height.unwrap_or(0);
                        a_res.cmp(&b_res)
                    }
                    SortColumn::Bitrate => a.bit_rate.cmp(&b.bit_rate),
                    SortColumn::Codec => a.source_codec.cmp(&b.source_codec),
                    SortColumn::Status => {
                        let status_val = |s: &Status| match s {
                            Status::Pending => 0,
                            Status::Encoding(_) => 1,
                            Status::Done => 2,
                            Status::Failed(_) => 3,
                            Status::Canceled => 4,
                        };
                        status_val(&a.status).cmp(&status_val(&b.status))
                    }
                };
                if self.sort_ascending {
                    cmp
                } else {
                    cmp.reverse()
                }
            });
        }
        indices
    }
}

fn open_path(path: &Path) {
    let folder = if path.is_dir() {
        path
    } else {
        path.parent().unwrap_or(path)
    };
    #[cfg(target_os = "linux")]
    let _ = Command::new("xdg-open").arg(folder).spawn();
    #[cfg(target_os = "macos")]
    let _ = Command::new("open").arg(folder).spawn();
    #[cfg(target_os = "windows")]
    let _ = Command::new("explorer").arg(folder).spawn();
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
        let start: Option<PathBuf> = match kind {
            "source" => self.sources.first().cloned(),
            _ => match &self.output {
                Some(OutputDest::Single(p)) => Some(p.clone()),
                _ => None,
            },
        };
        let mut dlg = rfd::FileDialog::new();
        if let Some(p) = start {
            dlg = dlg.set_directory(p);
        }
        dlg.pick_folder()
    }

    fn pick_source_file(&mut self) -> Option<Vec<PathBuf>> {
        let start = self
            .sources
            .first()
            .and_then(|p| {
                if p.is_file() {
                    p.parent()
                } else {
                    Some(p.as_path())
                }
            })
            .map(Path::to_path_buf);
        let mut dlg = rfd::FileDialog::new().add_filter(
            "Video",
            &[
                "mkv", "mp4", "avi", "mov", "wmv", "flv", "webm", "m4v", "ts", "mpg", "mpeg",
                "m2ts",
            ],
        );
        if let Some(p) = start {
            dlg = dlg.set_directory(p);
        }
        dlg.pick_files()
    }

    fn start_scan(&mut self) {
        if self.sources.is_empty() {
            self.notify_error("Pick a source folder or file before scanning.");
            return;
        }
        let Some(output) = self.output.clone() else {
            self.notify_error("Pick an output directory before scanning.");
            return;
        };

        let mut bad_sources: Vec<String> = Vec::new();
        for src in &self.sources {
            if src.is_file() {
                if !media_convert::scan::is_video(src) {
                    bad_sources.push(format!("not a recognised video: {}", src.display()));
                }
            } else if !src.is_dir() {
                bad_sources.push(format!("does not exist: {}", src.display()));
            }
        }
        if !bad_sources.is_empty() {
            self.notify_error(format!("Source problems:\n{}", bad_sources.join("\n")));
            return;
        }

        // Confirm the output directory is writable before kicking off a scan.
        // Disk-space warnings happen later (at start_encode) once we know the
        // total source size.
        if let OutputDest::Single(p) = &output {
            match validate_output(p, None) {
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

        self.files.clear();
        self.selected_file_index = None;

        let sources = self.sources.clone();
        let codec = self.codec;
        let container = self.container;
        let force = self.force;
        let recurse = self.recurse;
        let merge_subtitles = self.merge_subtitles;
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_thread = cancel.clone();
        let (tx, rx) = channel();

        thread::spawn(move || {
            let mut total_discovered = 0;
            for source in sources {
                if cancel_thread.load(Ordering::Relaxed) {
                    break;
                }
                let videos = scan::find_videos(&source, recurse);
                let rel_root: PathBuf = if source.is_file() {
                    source.parent().map(Path::to_path_buf).unwrap_or_default()
                } else {
                    source.clone()
                };
                total_discovered += videos.len();
                for abs in videos.iter() {
                    if cancel_thread.load(Ordering::Relaxed) {
                        break;
                    }
                    let rel = abs
                        .strip_prefix(&rel_root)
                        .unwrap_or(abs.as_path())
                        .to_path_buf();

                    let out = match &output {
                        OutputDest::PerSource => {
                            media_convert::output::default_output(abs, container)
                        }
                        OutputDest::Single(root) => {
                            let mut o = root.join(&rel);
                            o.set_extension(container.extension());
                            o
                        }
                    };

                    let (
                        source_codec,
                        width,
                        height,
                        bit_rate,
                        duration_secs,
                        source_subtitle_codecs,
                        unmappable_stream_indices,
                        discovered_subtitles,
                        decision,
                    ) = if out.exists() {
                        (
                            None,
                            None,
                            None,
                            None,
                            None,
                            Vec::new(),
                            Vec::new(),
                            Vec::new(),
                            Decision::SkipOutputExists,
                        )
                    } else {
                        match probe::video_info(abs) {
                            Ok(info) => {
                                // Discover sidecars at scan time only when merge
                                // mode is on — that's what drives the
                                // SkipNoSubtitles decision and what the encode
                                // step will reuse. Plain encode mode does
                                // discovery later, gated on the embed checkbox.
                                let subs = if merge_subtitles {
                                    scan::discover_subtitles(abs)
                                } else {
                                    Vec::new()
                                };
                                let dec = if merge_subtitles {
                                    if subs.is_empty() {
                                        Decision::SkipNoSubtitles
                                    } else {
                                        Decision::Encode
                                    }
                                } else if force || !codec.matches_source(&info.codec) {
                                    Decision::Encode
                                } else {
                                    Decision::SkipAlreadyTarget
                                };
                                (
                                    Some(info.codec),
                                    info.width,
                                    info.height,
                                    info.bit_rate,
                                    info.duration_secs,
                                    info.subtitle_codecs,
                                    info.unmappable_stream_indices,
                                    subs,
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
                        width,
                        height,
                        bit_rate,
                        duration_secs,
                        source_subtitle_codecs,
                        unmappable_stream_indices,
                        discovered_subtitles,
                        decision,
                        status: Status::Pending,
                        metadata: MetaState::NotLoaded,
                    };
                    if tx.send(ScanMsg::Discovered(entry)).is_err() {
                        return;
                    }
                }
            }
            let _ = tx.send(ScanMsg::Done(total_discovered));
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
        // burning encoder time. PerSource skips this; each output is on
        // whatever filesystem its source lives on.
        if let Some(OutputDest::Single(output)) = self.output.clone() {
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
        let merge_subtitles = self.merge_subtitles;
        let jobs: Vec<(usize, FileEntry, Vec<SubtitleInput>)> = self
            .files
            .iter()
            .enumerate()
            .filter(|(_, f)| matches!(f.decision, Decision::Encode))
            .filter(|(_, f)| !matches!(f.status, Status::Done))
            .map(|(i, f)| {
                // Merge mode already discovered sidecars at scan time; reuse
                // them so the table view and the encode share one source of
                // truth. Plain encode mode does discovery here only when the
                // user has opted in via the embed-subtitles checkbox.
                let subs = if merge_subtitles {
                    f.discovered_subtitles
                        .iter()
                        .cloned()
                        .map(|s| SubtitleInput {
                            path: s.path,
                            language: s.language,
                        })
                        .collect()
                } else if embed_subtitles {
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
            self.notify_warning(
                "Nothing to encode. Run Scan first, or there are no encodable files.",
            );
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

            // Block screensaver / system sleep for the batch. Dropped when this
            // closure returns (normal completion, cancel, or break).
            let _inhibitor = Inhibitor::acquire("Encoding video files");

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
                    source_subtitle_codecs: &entry.source_subtitle_codecs,
                    unmappable_stream_indices: &entry.unmappable_stream_indices,
                    merge_only: merge_subtitles,
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

    fn select_file(&mut self, idx: usize) {
        self.selected_file_index = Some(idx);
        let Some(entry) = self.files.get_mut(idx) else {
            return;
        };
        if !matches!(entry.metadata, MetaState::NotLoaded) {
            return;
        }
        entry.metadata = MetaState::Loading;
        let path = entry.abs.clone();
        let tx = self.metadata_tx.clone();
        thread::spawn(move || {
            let result = probe::file_metadata(&path).map_err(|e| format!("{e:#}"));
            let _ = tx.send((path, result));
        });
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
                            f.status = Status::Encoding(media_convert::convert::ProgressInfo {
                                fraction: None,
                                fps: None,
                                speed: None,
                            });
                        }
                        needs_repaint = true;
                    }
                    Ok(EncodeMsg::Progress(idx, info)) => {
                        if let Some(f) = self.files.get_mut(idx)
                            && matches!(f.status, Status::Encoding(_))
                        {
                            f.status = Status::Encoding(info);
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

        loop {
            match self.metadata_rx.try_recv() {
                Ok((path, result)) => {
                    if let Some(f) = self.files.iter_mut().find(|f| f.abs == path) {
                        f.metadata = match result {
                            Ok(m) => MetaState::Loaded(m),
                            Err(e) => MetaState::Failed(e),
                        };
                    }
                    needs_repaint = true;
                }
                Err(std::sync::mpsc::TryRecvError::Empty) => break,
                Err(std::sync::mpsc::TryRecvError::Disconnected) => break,
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
            read_progress(stdout, duration_secs, |info| {
                let _ = tx.send(EncodeMsg::Progress(idx, info));
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

        let mut to_remove = None;

        // Handle drag-and-drop
        ctx.input(|i| {
            let paths: Vec<_> = i
                .raw
                .dropped_files
                .iter()
                .filter_map(|d| d.path.clone())
                .collect();
            if !paths.is_empty() {
                self.set_sources(paths);
                self.start_scan();
            }
        });

        // No fill — panels share the ctx background so the UI reads as one
        // surface instead of stacked translucent overlays.
        let panel_frame = egui::Frame::NONE.inner_margin(8.0);
        // Wider horizontal padding for the table panel so the columns don't
        // crowd the window edges.
        let central_frame = panel_frame.inner_margin(egui::Margin::symmetric(20, 8));

        egui::TopBottomPanel::top("top")
            .frame(panel_frame)
            .show(ctx, |ui| {
                ui.add_space(4.0);

                ui.horizontal_top(|ui| {
                    ui.vertical(|ui| {
                        ui.group(|ui| {
                            ui.set_width(ui.available_width() * 0.45);
                            ui.heading("Input / Output");
                            egui::Grid::new("io_grid")
                                .num_columns(2)
                                .spacing([8.0, 8.0])
                                .show(ui, |ui| {
                                    ui.label("Source:");
                                    ui.vertical(|ui| {
                                        ui.horizontal(|ui| {
                                            if ui
                                                .button("Folder…")
                                                .on_hover_text(HINT_SOURCE_FOLDER)
                                                .clicked()
                                                && let Some(p) = self.pick_dir("source")
                                            {
                                                self.set_sources(vec![p]);
                                            }
                                            if ui
                                                .button("File…")
                                                .on_hover_text(HINT_SOURCE_FILE)
                                                .clicked()
                                                && let Some(p) = self.pick_source_file()
                                            {
                                                self.set_sources(p);
                                            }
                                        });
                                        if self.sources.is_empty() {
                                            ui.label("(none)");
                                        } else if self.sources.len() == 1 {
                                            let p = &self.sources[0];
                                            let kind = if p.is_file() { "file" } else { "folder" };
                                            ui.label(format!("{} [{kind}]", p.display()));
                                        } else {
                                            ui.label(format!(
                                                "{} items selected",
                                                self.sources.len()
                                            ))
                                            .on_hover_ui(|ui| {
                                                for p in &self.sources {
                                                    ui.label(p.display().to_string());
                                                }
                                            });
                                        }
                                    });
                                    ui.end_row();

                                    ui.label("Output:");
                                    ui.vertical(|ui| {
                                        if ui.button("Pick…").on_hover_text(HINT_OUTPUT).clicked()
                                            && let Some(p) = self.pick_dir("output")
                                        {
                                            self.output = Some(OutputDest::Single(p));
                                        }
                                        ui.label(
                                            self.output
                                                .as_ref()
                                                .map(OutputDest::display_label)
                                                .unwrap_or_else(|| "(none)".into()),
                                        );
                                    });
                                    ui.end_row();
                                });
                        });
                    });

                    ui.vertical(|ui| {
                        ui.group(|ui| {
                            ui.set_width(ui.available_width());
                            ui.heading("Encoder Settings");
                            let prev_codec = self.codec;
                            let prev_backend = self.backend;

                            egui::Grid::new("encoder_grid")
                                .num_columns(2)
                                .spacing([8.0, 8.0])
                                .show(ui, |ui| {
                                    ui.label("Codec:").on_hover_text(HINT_CODEC);
                                    egui::ComboBox::from_id_salt("codec_combo")
                                        .selected_text(self.codec.label())
                                        .show_ui(ui, |ui| {
                                            ui.selectable_value(
                                                &mut self.codec,
                                                Codec::X265,
                                                "x265",
                                            )
                                            .on_hover_text(HINT_CODEC_X265);
                                            ui.selectable_value(&mut self.codec, Codec::Av1, "av1")
                                                .on_hover_text(HINT_CODEC_AV1);
                                        });
                                    ui.end_row();

                                    ui.label("Backend:").on_hover_text(HINT_BACKEND);
                                    egui::ComboBox::from_id_salt("backend_combo")
                                        .selected_text(self.backend.label())
                                        .show_ui(ui, |ui| {
                                            ui.selectable_value(
                                                &mut self.backend,
                                                Backend::Software,
                                                "software",
                                            )
                                            .on_hover_text(HINT_BACKEND_SOFTWARE);
                                            ui.selectable_value(
                                                &mut self.backend,
                                                Backend::Nvenc,
                                                "nvenc",
                                            )
                                            .on_hover_text(HINT_BACKEND_NVENC);
                                            ui.selectable_value(
                                                &mut self.backend,
                                                Backend::Qsv,
                                                "qsv",
                                            )
                                            .on_hover_text(HINT_BACKEND_QSV);
                                            ui.selectable_value(
                                                &mut self.backend,
                                                Backend::Vaapi,
                                                "vaapi",
                                            )
                                            .on_hover_text(HINT_BACKEND_VAAPI);
                                        });
                                    ui.end_row();

                                    ui.label("Container:").on_hover_text(HINT_CONTAINER);
                                    egui::ComboBox::from_id_salt("container_combo")
                                        .selected_text(self.container.label())
                                        .show_ui(ui, |ui| {
                                            ui.selectable_value(
                                                &mut self.container,
                                                Container::Mkv,
                                                "mkv",
                                            )
                                            .on_hover_text(HINT_CONTAINER_MKV);
                                            ui.selectable_value(
                                                &mut self.container,
                                                Container::Mp4,
                                                "mp4",
                                            )
                                            .on_hover_text(HINT_CONTAINER_MP4);
                                        });
                                    ui.end_row();

                                    let cfg = self.backend.config(self.codec);
                                    ui.label(format!(
                                        "{}:",
                                        cfg.quality_flag.trim_start_matches('-')
                                    ))
                                    .on_hover_text(HINT_QUALITY);
                                    ui.add(egui::Slider::new(&mut self.quality, 0..=63))
                                        .on_hover_text(HINT_QUALITY);
                                    ui.end_row();

                                    ui.label("Preset:").on_hover_text(HINT_PRESET);
                                    ui.add_enabled(
                                        cfg.default_preset.is_some(),
                                        egui::TextEdit::singleline(&mut self.preset)
                                            .desired_width(80.0),
                                    )
                                    .on_hover_text(HINT_PRESET);
                                    ui.end_row();
                                });

                            if self.codec != prev_codec || self.backend != prev_backend {
                                self.reset_to_backend_defaults();
                            }
                        });
                    });
                });

                ui.add_space(4.0);

                ui.horizontal(|ui| {
                    let response = egui::CollapsingHeader::new("Advanced Options")
                        .open(Some(self.show_advanced))
                        .show(ui, |ui| {
                            ui.horizontal(|ui| {
                                ui.checkbox(&mut self.force, "Force re-encode")
                                    .on_hover_text(HINT_FORCE);
                                if self.sources.iter().any(|p| p.is_dir()) {
                                    ui.separator();
                                    ui.checkbox(&mut self.recurse, "Recurse subdirectories")
                                        .on_hover_text(HINT_RECURSE);
                                }
                                ui.separator();
                                ui.checkbox(
                                    &mut self.embed_subtitles,
                                    "Embed sidecar SRT subtitles",
                                )
                                .on_hover_text(HINT_EMBED_SUBTITLES);
                                ui.separator();
                                ui.checkbox(
                                    &mut self.merge_subtitles,
                                    "Merge subtitles only (no re-encode)",
                                )
                                .on_hover_text(HINT_MERGE_SUBTITLES);
                            });
                        });
                    if response.header_response.clicked() {
                        self.show_advanced = !self.show_advanced;
                    }
                });

                ui.add_space(4.0);

                ui.horizontal(|ui| {
                    let scanning = self.scanner.is_some();
                    let encoding = self.encoder.is_some();
                    ui.add_enabled_ui(!scanning && !encoding, |ui| {
                        if ui.button("Scan").clicked() {
                            self.start_scan();
                        }
                    });
                    if scanning {
                        ui.add(egui::Spinner::new());
                    }
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

                    ui.label("Search:");
                    ui.add(egui::TextEdit::singleline(&mut self.search_query).desired_width(120.0));
                    ui.separator();
                    ui.label("Filter:");
                    egui::ComboBox::from_id_salt("filter_combo")
                        .selected_text(match self.filter_state {
                            FilterState::All => "All",
                            FilterState::ToEncode => "To Encode",
                            FilterState::Done => "Done",
                            FilterState::Failed => "Failed",
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(&mut self.filter_state, FilterState::All, "All");
                            ui.selectable_value(
                                &mut self.filter_state,
                                FilterState::ToEncode,
                                "To Encode",
                            );
                            ui.selectable_value(&mut self.filter_state, FilterState::Done, "Done");
                            ui.selectable_value(
                                &mut self.filter_state,
                                FilterState::Failed,
                                "Failed",
                            );
                        });
                });
                ui.add_space(4.0);
            });

        // Metadata panel: lazy-loaded ffprobe details for the selected file,
        // sits above the log panel.
        egui::TopBottomPanel::bottom("metadata")
            .resizable(true)
            .default_height(200.0)
            .frame(panel_frame)
            .show(ctx, |ui| {
                ui.add_space(2.0);
                ui.label(egui::RichText::new("Metadata").strong());
                ui.separator();
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        match self.selected_file_index.and_then(|i| self.files.get(i)) {
                            None => {
                                ui.label(
                                    "Select a file in the table above to view its metadata.",
                                );
                            }
                            Some(entry) => match &entry.metadata {
                                MetaState::NotLoaded | MetaState::Loading => {
                                    ui.label(format!(
                                        "Loading metadata for {}…",
                                        entry.rel.display()
                                    ));
                                }
                                MetaState::Failed(err) => {
                                    ui.colored_label(
                                        egui::Color32::from_rgb(220, 80, 80),
                                        format!(
                                            "Failed to read metadata for {}:\n{err}",
                                            entry.rel.display()
                                        ),
                                    );
                                }
                                MetaState::Loaded(meta) => {
                                    render_metadata(ui, entry, meta);
                                }
                            },
                        }
                    });
            });

        egui::TopBottomPanel::bottom("bottom")
            .resizable(true)
            .default_height(140.0)
            .frame(panel_frame)
            .show(ctx, |ui| {
                let summary = summarize(&self.files);
                let total_jobs = summary.to_encode + summary.done + summary.failed;

                ui.horizontal(|ui| {
                    ui.label("Log:");
                    if total_jobs > 0 {
                        ui.separator();
                        let progress = summary.done as f32 / total_jobs as f32;
                        ui.add(
                            egui::ProgressBar::new(progress)
                                .text(format!("Batch Progress: {} / {}", summary.done, total_jobs))
                                .desired_width(300.0),
                        );

                        if let Some(f) = self
                            .files
                            .iter()
                            .find(|f| matches!(f.status, Status::Encoding(_)))
                            && let Status::Encoding(info) = &f.status
                        {
                            if let Some(fps) = info.fps {
                                ui.label(format!("| {fps:.1} fps"));
                            }
                            if let (Some(speed), Some(duration), Some(frac)) =
                                (info.speed, f.duration_secs, info.fraction)
                                && speed > 0.0
                            {
                                let remaining_secs = (1.0 - frac) * duration / speed as f64;
                                if remaining_secs > 0.0 {
                                    ui.label(format!("| ETA: {}s", remaining_secs as u64));
                                }
                            }
                        }
                    }
                    if total_jobs > 0 && summary.done == total_jobs {
                        ui.separator();
                        ui.label(
                            egui::RichText::new("✔ Batch Complete!")
                                .color(STATUS_DONE)
                                .strong(),
                        );
                    }
                });

                egui::ScrollArea::vertical()
                    .stick_to_bottom(true)
                    .show(ui, |ui| {
                        for line in &self.log {
                            ui.monospace(line);
                        }
                    });
            });

        // File-details panel: lives between the log panel (very bottom) and the
        // central table. Resizable so the user can pick the split they want;
        // the table fills whatever space is left.
        egui::TopBottomPanel::bottom("file_details")
            .frame(panel_frame)
            .resizable(true)
            .default_height(180.0)
            .min_height(60.0)
            .show(ctx, |ui| {
                ui.heading("File Details");
                ui.add_space(4.0);
                if let Some(idx) = self.selected_file_index
                    && let Some(f) = self.files.get(idx)
                {
                    egui::ScrollArea::vertical()
                        .id_salt("metadata_scroll")
                        .show(ui, |ui| {
                            egui::Grid::new("metadata_grid")
                                .num_columns(2)
                                .spacing([16.0, 4.0])
                                .show(ui, |ui| {
                                    ui.label("Path:");
                                    ui.monospace(f.abs.display().to_string());
                                    ui.end_row();

                                    ui.label("Output:");
                                    ui.monospace(f.output.display().to_string());
                                    ui.end_row();

                                    ui.label("Format:");
                                    let res = if let (Some(w), Some(h)) = (f.width, f.height) {
                                        format!("{w}x{h}")
                                    } else {
                                        "unknown".into()
                                    };
                                    let codec = f.source_codec.as_deref().unwrap_or("unknown");
                                    let br = f
                                        .bit_rate
                                        .map(format_bitrate)
                                        .unwrap_or_else(|| "unknown".into());
                                    let dur = f
                                        .duration_secs
                                        .map(format_duration)
                                        .unwrap_or_else(|| "unknown".into());
                                    ui.label(format!("{codec}, {res}, {br}, {dur}"));
                                    ui.end_row();

                                    ui.label("Subtitles (internal):");
                                    if f.source_subtitle_codecs.is_empty() {
                                        ui.label("none");
                                    } else {
                                        ui.label(format!(
                                            "{} tracks: {}",
                                            f.source_subtitle_codecs.len(),
                                            f.source_subtitle_codecs.join(", ")
                                        ));
                                    }
                                    ui.end_row();

                                    ui.label("Subtitles (sidecar):");
                                    let sidecars = media_convert::scan::discover_subtitles(&f.abs);
                                    if sidecars.is_empty() {
                                        ui.label("none discovered");
                                    } else {
                                        ui.vertical(|ui| {
                                            for s in sidecars {
                                                let lang =
                                                    s.language.as_deref().unwrap_or("unknown lang");
                                                ui.label(format!(
                                                    "{} ({})",
                                                    s.path.file_name().unwrap().to_string_lossy(),
                                                    lang
                                                ));
                                            }
                                        });
                                    }
                                    ui.end_row();
                                });
                        });
                } else {
                    ui.label("Select a file in the table to see details.");
                }
            });

        egui::CentralPanel::default().frame(central_frame).show(ctx, |ui| {
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

            let indices = self.filtered_indices();
            let mut newly_selected = None;

            // The table fills the central panel directly, wrapped in a
            // rounded-corner Frame so it has a visible left/right/top/bottom
            // border that connects at the corners. `auto_shrink([false; 2])`
            // keeps it from collapsing to content size; the data columns get
            // fixed initial widths so they don't stretch past the window, and
            // the file column takes the remainder and clips long names.
            //
            // Note: `.resizable(true)` on the table makes egui_extras pin
            // every column's width to its stored value — including the
            // Remainder column, which then stops tracking window resizes.
            // We mark the file column non-resizable so it keeps absorbing
            // the remainder; the data columns stay user-resizable.
            let table_frame = egui::Frame::NONE
                .stroke(ui.visuals().widgets.noninteractive.bg_stroke)
                .corner_radius(egui::CornerRadius::same(6))
                .inner_margin(1.0);
            // Captured during header rendering, painted after the table
            // renders. `divider_x` is where File ends and Resolution begins —
            // egui_extras doesn't auto-draw a divider there because File is
            // non-resizable. `header_bottom_y` is the header row's bottom edge;
            // we paint a single full-width hline so the underline is solid
            // (per-cell hlines leave `item_spacing.x` gaps between columns).
            let divider_x = std::cell::Cell::new(0.0_f32);
            let header_bottom_y = std::cell::Cell::new(0.0_f32);
            let frame_resp = table_frame.show(ui, |ui| {
            TableBuilder::new(ui)
                    .striped(true)
                    .resizable(true)
                    .vscroll(true)
                    .auto_shrink([false; 2])
                    .column(Column::remainder().at_least(150.0).clip(true).resizable(false)) // File
                    .column(Column::initial(90.0).at_least(60.0))           // Resolution
                    .column(Column::initial(90.0).at_least(60.0))           // Bitrate
                    .column(Column::initial(90.0).at_least(60.0))           // Source codec
                    .column(Column::initial(110.0).at_least(80.0))          // Decision
                    .column(Column::initial(160.0).at_least(120.0).resizable(false)) // Status
                    .header(30.0, |mut header| {
                            let mut header_cell = |ui: &mut egui::Ui, col: Option<SortColumn>, label: &str| {
                                // No per-cell hline here — a single full-width
                                // underline is painted after the table to
                                // avoid gaps at the column-spacing locations.
                                ui.with_layout(egui::Layout::left_to_right(egui::Align::Center), |ui| {
                                    ui.add_space(ui.spacing().item_spacing.x);
                                    let mut text = egui::RichText::new(label).strong();
                                    if let Some(col) = col {
                                        if self.sort_column == Some(col) {
                                            text = text.underline();
                                            let arrow = if self.sort_ascending { " ▴" } else { " ▾" };
                                            ui.horizontal(|ui| {
                                                if ui.selectable_label(true, text).clicked() {
                                                    self.sort_ascending = !self.sort_ascending;
                                                }
                                                ui.label(arrow);
                                            });
                                        } else if ui.selectable_label(false, text).clicked() {
                                            self.sort_column = Some(col);
                                            self.sort_ascending = true;
                                        }
                                    } else {
                                        ui.label(text);
                                    }
                                });
                            };

                            header.col(|ui| {
                                header_bottom_y.set(ui.max_rect().bottom());
                                header_cell(ui, Some(SortColumn::Name), "File");
                            });
                            header.col(|ui| {
                                divider_x.set(ui.max_rect().left());
                                header_cell(ui, Some(SortColumn::Resolution), "Resolution");
                            });
                            header.col(|ui| {
                                header_cell(ui, Some(SortColumn::Bitrate), "Bitrate");
                            });
                            header.col(|ui| {
                                header_cell(ui, Some(SortColumn::Codec), "Source codec");
                            });
                            header.col(|ui| {
                                header_cell(ui, None, "Decision");
                            });
                            header.col(|ui| {
                                header_cell(ui, Some(SortColumn::Status), "Status");
                            });
                        })
                        .body(|mut body| {
                            for idx in indices {
                                let f = &self.files[idx];
                                let is_selected = self.selected_file_index == Some(idx);

                                body.row(30.0, |mut row| {
                                    row.set_selected(is_selected);

                                    row.col(|ui| {
                                        ui.monospace(f.rel.display().to_string());
                                    });
                                    row.col(|ui| {
                                        if let (Some(w), Some(h)) = (f.width, f.height) {
                                            ui.label(format!("{w}x{h}"));
                                        } else {
                                            ui.label("-");
                                        }
                                    });
                                    row.col(|ui| {
                                        if let Some(br) = f.bit_rate {
                                            ui.label(format_bitrate(br));
                                        } else {
                                            ui.label("-");
                                        }
                                    });
                                    row.col(|ui| {
                                        ui.label(f.source_codec.as_deref().unwrap_or("-"));
                                    });
                                    row.col(|ui| {
                                        decision_ui(ui, &f.decision);
                                    });
                                    row.col(|ui| {
                                        status_ui(ui, &f.status, f.duration_secs);
                                    });

                                    let row_response = row.response();
                                    if row_response.clicked() {
                                        newly_selected = Some(idx);
                                    }
                                    row_response.context_menu(|ui| {
                                        if ui.button("Open source folder").clicked() {
                                            open_path(&f.abs);
                                            ui.close_menu();
                                        }
                                        if ui.button("Open output folder").clicked() {
                                            open_path(&f.output);
                                            ui.close_menu();
                                        }
                                        ui.separator();
                                        if ui.button("Remove from list").clicked() {
                                            to_remove = Some(idx);
                                            ui.close_menu();
                                        }
                                    });
                                });
                            }
                        });
            });

            // The frame's response.rect spans the whole table area;
            // insetting by inner_margin keeps lines off the rounded corners.
            let stroke = ui.visuals().widgets.noninteractive.bg_stroke;
            let table_rect = frame_resp.response.rect;
            // Full-height File/Resolution divider — File is non-resizable so
            // egui_extras skips it.
            ui.painter().vline(
                divider_x.get(),
                egui::Rangef::new(table_rect.top() + 1.0, table_rect.bottom() - 1.0),
                stroke,
            );
            // Solid underline below the header row.
            ui.painter().hline(
                egui::Rangef::new(table_rect.left() + 1.0, table_rect.right() - 1.0),
                header_bottom_y.get(),
                stroke,
            );

            // `select_file` both updates the selection index and kicks off a
            // background metadata fetch (no-op if already loaded/loading).
            if let Some(idx) = newly_selected {
                self.select_file(idx);
            }
        });

        if let Some(idx) = to_remove {
            self.files.remove(idx);
            if self.selected_file_index == Some(idx) {
                self.selected_file_index = None;
            } else if let Some(sel) = self.selected_file_index
                && sel > idx
            {
                self.selected_file_index = Some(sel - 1);
            }
        }
    }
}

fn decision_ui(ui: &mut egui::Ui, d: &Decision) {
    let (text, color) = match d {
        Decision::Encode => ("encode", STATUS_ENCODE),
        Decision::SkipAlreadyTarget => ("skip (target)", egui::Color32::GRAY),
        Decision::SkipOutputExists => ("skip (exists)", egui::Color32::GRAY),
        Decision::SkipNoSubtitles => ("skip (no sidecar subs)", egui::Color32::GRAY),
    };
    ui.colored_label(color, text);
}

fn status_ui(ui: &mut egui::Ui, s: &Status, duration_secs: Option<f64>) {
    match s {
        Status::Pending => {
            ui.label("pending");
        }
        Status::Encoding(info) => {
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 4.0;
                let mut status_text = String::from("enc");
                if let Some(p) = info.fraction {
                    status_text.push_str(&format!(" {:>2}%", (p * 100.0) as u32));
                }
                ui.label(
                    egui::RichText::new(status_text)
                        .color(STATUS_ENCODING)
                        .strong(),
                );
                if info.fraction.is_none() {
                    ui.add(egui::Spinner::new().size(12.0));
                    return;
                }
                if let Some(fps) = info.fps {
                    ui.label(egui::RichText::new(format!("{fps:.0}fps")).small());
                }
                if let (Some(speed), Some(duration), Some(frac)) =
                    (info.speed, duration_secs, info.fraction)
                    && speed > 0.0
                {
                    let remaining_secs = (1.0 - frac) * duration / speed as f64;
                    if remaining_secs > 0.0 {
                        ui.label(
                            egui::RichText::new(format!("{}s", remaining_secs as u64)).small(),
                        );
                    }
                }
            });
        }
        Status::Done => {
            ui.colored_label(STATUS_DONE, "done");
        }
        Status::Failed(msg) => {
            ui.colored_label(STATUS_FAILED, "failed").on_hover_text(msg);
        }
        Status::Canceled => {
            ui.colored_label(STATUS_CANCELED, "canceled");
        }
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
            (
                Decision::SkipAlreadyTarget
                | Decision::SkipOutputExists
                | Decision::SkipNoSubtitles,
                _,
            ) => s.skipped += 1,
            (Decision::Encode, _) => s.to_encode += 1,
        }
    }
    s
}

fn format_duration(seconds: f64) -> String {
    let secs = seconds.round() as u64;
    let hours = secs / 3600;
    let mins = (secs % 3600) / 60;
    let secs = secs % 60;
    if hours > 0 {
        format!("{hours}:{mins:02}:{secs:02}")
    } else {
        format!("{mins}:{secs:02}")
    }
}

fn format_bitrate(bps: u64) -> String {
    if bps >= 1_000_000 {
        format!("{:.1} Mbps", bps as f64 / 1_000_000.0)
    } else {
        format!("{:.0} kbps", bps as f64 / 1_000.0)
    }
}

fn render_metadata(ui: &mut egui::Ui, entry: &FileEntry, meta: &probe::FileMetadata) {
    egui::Grid::new("metadata_format_grid")
        .num_columns(2)
        .spacing([16.0, 4.0])
        .striped(true)
        .show(ui, |ui| {
            ui.strong("Path");
            ui.monospace(entry.abs.display().to_string());
            ui.end_row();

            if let Some(name) = &meta.format_name {
                ui.strong("Container");
                let label = match &meta.format_long_name {
                    Some(long) if long != name => format!("{name}  ({long})"),
                    _ => name.clone(),
                };
                ui.label(label);
                ui.end_row();
            }

            if let Some(size) = meta.size_bytes {
                ui.strong("Size");
                ui.label(format!("{} ({} bytes)", format_bytes(size), size));
                ui.end_row();
            }

            if let Some(d) = meta.duration_secs {
                ui.strong("Duration");
                ui.label(format_duration(d));
                ui.end_row();
            }

            if let Some(br) = meta.bit_rate {
                ui.strong("Overall bitrate");
                ui.label(format!("{} kb/s", br / 1000));
                ui.end_row();
            }

            if let Some(t) = &meta.title {
                ui.strong("Title");
                ui.label(t);
                ui.end_row();
            }
        });

    ui.add_space(8.0);
    ui.label(egui::RichText::new(format!("Streams ({})", meta.streams.len())).strong());
    ui.add_space(2.0);

    for s in &meta.streams {
        let codec = s.codec_name.as_deref().unwrap_or("(unknown)");
        let header = format!("#{} {}: {}", s.index, s.codec_type, codec);
        ui.monospace(header);
        let detail = stream_detail(s);
        if !detail.is_empty() {
            ui.monospace(format!("    {detail}"));
        }
    }
}

fn stream_detail(s: &probe::StreamInfo) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(profile) = &s.profile {
        parts.push(format!("profile={profile}"));
    }
    if let (Some(w), Some(h)) = (s.width, s.height) {
        parts.push(format!("{w}×{h}"));
    }
    if let Some(p) = &s.pix_fmt {
        parts.push(p.clone());
    }
    if let Some(r) = &s.frame_rate {
        parts.push(format_frame_rate(r));
    }
    if let Some(c) = s.channels {
        match s.channel_layout.as_deref() {
            Some(layout) if !layout.is_empty() => parts.push(format!("{c}ch ({layout})")),
            _ => parts.push(format!("{c}ch")),
        }
    }
    if let Some(sr) = s.sample_rate {
        parts.push(format!("{sr} Hz"));
    }
    if let Some(b) = s.bit_rate {
        parts.push(format!("{} kb/s", b / 1000));
    }
    if let Some(lang) = &s.language {
        parts.push(format!("lang={lang}"));
    }
    if let Some(t) = &s.title {
        parts.push(format!("title=\"{t}\""));
    }
    let mut flags: Vec<&str> = Vec::new();
    if s.default {
        flags.push("default");
    }
    if s.forced {
        flags.push("forced");
    }
    if !flags.is_empty() {
        parts.push(format!("[{}]", flags.join(",")));
    }
    parts.join(" • ")
}

fn format_bytes(n: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    if n >= GIB {
        format!("{:.2} GiB", n as f64 / GIB as f64)
    } else if n >= MIB {
        format!("{:.2} MiB", n as f64 / MIB as f64)
    } else if n >= KIB {
        format!("{:.2} KiB", n as f64 / KIB as f64)
    } else {
        format!("{n} B")
    }
}

fn format_frame_rate(s: &str) -> String {
    if let Some((n, d)) = s.split_once('/')
        && let (Ok(n), Ok(d)) = (n.parse::<f64>(), d.parse::<f64>())
        && d > 0.0
    {
        return format!("{:.3} fps", n / d);
    }
    s.to_string()
}
