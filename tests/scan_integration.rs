use std::fs;

use media_convert::scan;
use tempfile::tempdir;

#[test]
fn finds_videos_across_a_realistic_tree() {
    let dir = tempdir().unwrap();
    let root = dir.path();

    fs::write(root.join("top.mkv"), b"").unwrap();
    fs::write(root.join("README.txt"), b"").unwrap();

    let season1 = root.join("season1");
    fs::create_dir(&season1).unwrap();
    fs::write(season1.join("ep01.mp4"), b"").unwrap();
    fs::write(season1.join("ep02.MOV"), b"").unwrap();
    fs::write(season1.join("subs.srt"), b"").unwrap();

    let nested = season1.join("extras");
    fs::create_dir(&nested).unwrap();
    fs::write(nested.join("trailer.webm"), b"").unwrap();

    let recursive = scan::find_videos(root, true);
    assert_eq!(recursive.len(), 4);

    let shallow = scan::find_videos(root, false);
    assert_eq!(shallow.len(), 1);
    assert!(shallow[0].ends_with("top.mkv"));
}

#[test]
fn single_file_source_returns_just_that_file() {
    let dir = tempdir().unwrap();
    let f = dir.path().join("movie.m4v");
    fs::write(&f, b"").unwrap();

    let videos = scan::find_videos(&f, true);
    assert_eq!(videos, vec![f]);
}

#[test]
fn single_non_video_file_returns_empty() {
    let dir = tempdir().unwrap();
    let f = dir.path().join("notes.txt");
    fs::write(&f, b"").unwrap();

    assert!(scan::find_videos(&f, true).is_empty());
}
