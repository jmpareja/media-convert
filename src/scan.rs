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
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().is_file())
        .map(|e| e.into_path())
        .filter(|p| is_video(p))
        .collect()
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
}
