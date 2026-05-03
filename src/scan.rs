use std::path::{Path, PathBuf};
use walkdir::WalkDir;

const VIDEO_EXTENSIONS: &[&str] = &[
    "mkv", "mp4", "avi", "mov", "wmv", "flv", "webm", "m4v", "ts", "mpg", "mpeg", "m2ts",
];

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
