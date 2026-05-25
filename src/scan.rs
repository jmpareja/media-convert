use std::path::{Path, PathBuf};
use walkdir::WalkDir;

const VIDEO_EXTENSIONS: &[&str] = &[
    "mkv", "mp4", "avi", "mov", "wmv", "flv", "webm", "m4v", "ts", "mpg", "mpeg", "m2ts",
];

#[derive(Clone, Debug)]
pub struct SidecarSubtitle {
    pub path: PathBuf,
    pub language: Option<String>,
}

/// Find sidecar `.srt` files next to `video`. A sidecar is a file in the same
/// directory whose stem either matches the video stem exactly (e.g.
/// `movie.srt` next to `movie.mp4`) or starts with `<stem>.` (e.g.
/// `movie.en.srt`, `movie.eng.srt`). When a 2- or 3-letter ASCII language
/// code follows the stem, it's returned as `language`.
pub fn discover_subtitles(video: &Path) -> Vec<SidecarSubtitle> {
    let Some(stem) = video.file_stem().and_then(|s| s.to_str()) else {
        return Vec::new();
    };
    let parent = match video.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    let Ok(entries) = std::fs::read_dir(parent) else {
        return Vec::new();
    };

    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let is_srt = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("srt"))
            .unwrap_or(false);
        if !is_srt {
            continue;
        }
        let Some(srt_stem) = path.file_stem().and_then(|s| s.to_str()) else {
            continue;
        };
        if srt_stem == stem {
            out.push(SidecarSubtitle {
                path,
                language: None,
            });
        } else if let Some(suffix) = srt_stem.strip_prefix(&format!("{stem}.")) {
            let first = suffix.split('.').next().unwrap_or("");
            let language = if (first.len() == 2 || first.len() == 3)
                && first.chars().all(|c| c.is_ascii_alphabetic())
            {
                Some(first.to_lowercase())
            } else {
                None
            };
            out.push(SidecarSubtitle { path, language });
        }
    }
    out.sort_by(|a, b| a.path.cmp(&b.path));
    out
}

pub fn find_videos(root: &Path, recursive: bool) -> Vec<PathBuf> {
    if root.is_file() {
        return if is_video(root) {
            vec![root.to_path_buf()]
        } else {
            Vec::new()
        };
    }
    let mut walker = WalkDir::new(root).follow_links(false);
    if !recursive {
        walker = walker.max_depth(1);
    }
    walker
        .into_iter()
        // Prune media-convert's own output bundles so re-running the tool on
        // a tree doesn't pick its previous outputs up as fresh sources.
        // `.hls/` directories are HLS bundle roots — walk would otherwise
        // descend and find each `.ts` segment as a "video" and re-encode
        // it. `filter_entry` prunes whole subtrees, so children of `.hls/`
        // are never enumerated.
        .filter_entry(|e| !is_hls_bundle_dir(e.path()))
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
        .filter(|p| is_video(p))
        // `.web.mp4` files are the Web profile's own outputs — same logic.
        // Recognising them by the trailing `.web.mp4` keeps the rule scoped
        // to our convention rather than excluding all `.mp4`.
        .filter(|p| !is_web_sibling(p))
        .collect()
}

fn is_hls_bundle_dir(p: &Path) -> bool {
    p.is_dir()
        && p.extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("hls"))
}

fn is_web_sibling(p: &Path) -> bool {
    p.file_name()
        .and_then(|n| n.to_str())
        .is_some_and(|n| n.to_ascii_lowercase().ends_with(".web.mp4"))
}

pub fn is_video(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| {
            let lower = e.to_ascii_lowercase();
            VIDEO_EXTENSIONS.iter().any(|v| *v == lower)
        })
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn is_video_recognises_known_extensions() {
        assert!(is_video(Path::new("clip.mkv")));
        assert!(is_video(Path::new("clip.MP4")));
        assert!(is_video(Path::new("/abs/path/movie.webm")));
    }

    #[test]
    fn is_video_rejects_others() {
        assert!(!is_video(Path::new("notes.txt")));
        assert!(!is_video(Path::new("README")));
        assert!(!is_video(Path::new("archive.tar.gz")));
    }

    #[test]
    fn find_videos_returns_single_video_file() {
        let dir = tempdir().unwrap();
        let f = dir.path().join("only.mp4");
        fs::write(&f, b"").unwrap();

        let videos = find_videos(&f, true);
        assert_eq!(videos, vec![f]);
    }

    #[test]
    fn find_videos_returns_empty_for_non_video_file() {
        let dir = tempdir().unwrap();
        let f = dir.path().join("notes.txt");
        fs::write(&f, b"").unwrap();

        assert!(find_videos(&f, true).is_empty());
    }

    #[test]
    fn find_videos_walks_recursively_by_default() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.mp4"), b"").unwrap();
        fs::write(dir.path().join("readme.txt"), b"").unwrap();
        let sub = dir.path().join("nested");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("b.mkv"), b"").unwrap();
        fs::write(sub.join("c.MOV"), b"").unwrap();

        let mut videos = find_videos(dir.path(), true);
        videos.sort();
        assert_eq!(videos.len(), 3);
        assert!(videos.iter().any(|p| p.ends_with("a.mp4")));
        assert!(videos.iter().any(|p| p.ends_with("nested/b.mkv")));
        assert!(videos.iter().any(|p| p.ends_with("nested/c.MOV")));
    }

    #[test]
    fn discover_subtitles_finds_basename_match_and_lang_codes() {
        let dir = tempdir().unwrap();
        let video = dir.path().join("show.S01E01.mp4");
        fs::write(&video, b"").unwrap();
        fs::write(dir.path().join("show.S01E01.srt"), b"").unwrap();
        fs::write(dir.path().join("show.S01E01.en.srt"), b"").unwrap();
        fs::write(dir.path().join("show.S01E01.ger.srt"), b"").unwrap();
        // Should be ignored: not a sidecar of this video
        fs::write(dir.path().join("other.srt"), b"").unwrap();
        // Should be ignored: not an SRT
        fs::write(dir.path().join("show.S01E01.txt"), b"").unwrap();

        let subs = discover_subtitles(&video);
        assert_eq!(subs.len(), 3);

        let by_path: Vec<_> = subs
            .iter()
            .map(|s| {
                (
                    s.path.file_name().unwrap().to_string_lossy().into_owned(),
                    s.language.clone(),
                )
            })
            .collect();
        assert!(by_path.contains(&("show.S01E01.srt".into(), None)));
        assert!(by_path.contains(&("show.S01E01.en.srt".into(), Some("en".into()))));
        assert!(by_path.contains(&("show.S01E01.ger.srt".into(), Some("ger".into()))));
    }

    #[test]
    fn discover_subtitles_skips_non_lang_suffix() {
        let dir = tempdir().unwrap();
        let video = dir.path().join("movie.mkv");
        fs::write(&video, b"").unwrap();
        // "forced" is 6 chars — not a language code; language stays None
        fs::write(dir.path().join("movie.forced.srt"), b"").unwrap();

        let subs = discover_subtitles(&video);
        assert_eq!(subs.len(), 1);
        assert_eq!(subs[0].language, None);
    }

    #[test]
    fn find_videos_skips_subdirectories_when_recursive_is_false() {
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("a.mp4"), b"").unwrap();
        let sub = dir.path().join("nested");
        fs::create_dir(&sub).unwrap();
        fs::write(sub.join("b.mkv"), b"").unwrap();

        let videos = find_videos(dir.path(), false);
        assert_eq!(videos.len(), 1);
        assert!(videos[0].ends_with("a.mp4"));
    }

    #[test]
    fn find_videos_for_nonexistent_path_returns_empty() {
        let dir = tempdir().unwrap();
        let missing = dir.path().join("does-not-exist");
        assert!(find_videos(&missing, true).is_empty());
    }

    #[test]
    fn discover_subtitles_returns_results_sorted_by_path() {
        let dir = tempdir().unwrap();
        let video = dir.path().join("show.mp4");
        fs::write(&video, b"").unwrap();
        // Write in non-alphabetical order; result must be alphabetical
        fs::write(dir.path().join("show.fr.srt"), b"").unwrap();
        fs::write(dir.path().join("show.en.srt"), b"").unwrap();
        fs::write(dir.path().join("show.de.srt"), b"").unwrap();

        let subs = discover_subtitles(&video);
        let names: Vec<_> = subs
            .iter()
            .map(|s| s.path.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["show.de.srt", "show.en.srt", "show.fr.srt"]);
    }

    #[test]
    fn discover_subtitles_accepts_uppercase_extension() {
        let dir = tempdir().unwrap();
        let video = dir.path().join("clip.mkv");
        fs::write(&video, b"").unwrap();
        fs::write(dir.path().join("clip.SRT"), b"").unwrap();

        let subs = discover_subtitles(&video);
        assert_eq!(subs.len(), 1);
    }

    #[test]
    fn discover_subtitles_ignores_unrelated_videos() {
        let dir = tempdir().unwrap();
        let video = dir.path().join("movie.mp4");
        fs::write(&video, b"").unwrap();
        // Sidecar of a different video should not be picked up
        fs::write(dir.path().join("other.en.srt"), b"").unwrap();

        let subs = discover_subtitles(&video);
        assert!(subs.is_empty());
    }

    #[test]
    fn discover_subtitles_returns_empty_for_video_without_stem() {
        // Edge case: a path like "/" or with no file_stem returns empty rather
        // than panicking.
        let subs = discover_subtitles(Path::new("/"));
        assert!(subs.is_empty());
    }

    #[test]
    fn is_video_returns_false_for_path_without_extension() {
        assert!(!is_video(Path::new("README")));
        assert!(!is_video(Path::new("/some/path/file")));
    }

    #[test]
    fn find_videos_prunes_dot_hls_bundle_directories() {
        // Reproduce the recursive-bundle bug: a previous --profile hls run
        // left `.hls/` directories full of `.ts` segments. A fresh scan
        // must not descend into them, otherwise each segment gets treated
        // as a new source and the tree grows on every run.
        let dir = tempdir().unwrap();
        let mkv = dir.path().join("Show/S01/ep1.mkv");
        fs::create_dir_all(mkv.parent().unwrap()).unwrap();
        fs::write(&mkv, b"").unwrap();
        let bundle = dir.path().join("Show/S01/ep1.hls/720p");
        fs::create_dir_all(&bundle).unwrap();
        fs::write(bundle.join("segment_000.ts"), b"").unwrap();
        fs::write(bundle.join("segment_001.ts"), b"").unwrap();

        let videos = find_videos(dir.path(), true);
        assert_eq!(videos.len(), 1, "expected only ep1.mkv, got {videos:?}");
        assert!(videos[0].ends_with("ep1.mkv"));
    }

    #[test]
    fn find_videos_skips_dot_web_mp4_siblings() {
        // Web profile's own outputs (`<stem>.web.mp4`) live next to the
        // sources they were generated from. A re-scan must skip them so
        // we don't try to web-encode them into `<stem>.web.web.mp4`.
        let dir = tempdir().unwrap();
        fs::write(dir.path().join("ep1.mkv"), b"").unwrap();
        fs::write(dir.path().join("ep1.web.mp4"), b"").unwrap();
        // A regular `.mp4` that isn't ours should still be picked up.
        fs::write(dir.path().join("ep2.mp4"), b"").unwrap();

        let mut videos = find_videos(dir.path(), true);
        videos.sort();
        let names: Vec<_> = videos
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, vec!["ep1.mkv", "ep2.mp4"]);
    }
}
