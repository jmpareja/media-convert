use anyhow::{Context, Result, bail};
use std::fs;
use std::path::{Path, PathBuf};

use crate::container::Container;

/// Default output path when the user doesn't provide one.
///
/// For a directory source, it returns a sibling directory with `-converted` appended.
/// For a single file source, it returns a file in the same directory with
/// `-converted` appended to the stem and the target container extension.
///
/// Examples:
/// * `/movies/library` -> `/movies/library-converted`
/// * `/movies/library/movie.mkv` -> `/movies/library/movie-converted.mkv` (if container is Mkv)
/// * `library` -> `library-converted`
///
/// Falls back to `<source>/converted` when the source has no usable file name
/// (e.g. `/`, `.`).
pub fn default_output(source: &Path, container: Container) -> PathBuf {
    if source.is_file() {
        let mut p = source.to_path_buf();
        if let Some(stem) = source.file_stem() {
            let new_name = format!(
                "{}-converted.{}",
                stem.to_string_lossy(),
                container.extension()
            );
            p.set_file_name(new_name);
        }
        return p;
    }

    let basis: &Path = source;
    let Some(name) = basis.file_name() else {
        return basis.join("converted");
    };
    let new_name = format!("{}-converted", name.to_string_lossy());
    match basis.parent() {
        Some(p) if !p.as_os_str().is_empty() => p.join(new_name),
        _ => PathBuf::from(new_name),
    }
}

/// Result of validating an output directory: any non-fatal warnings the user
/// should see. Fatal problems are returned via `Err` from `validate_output`.
#[derive(Debug, Default)]
pub struct OutputValidation {
    pub warnings: Vec<String>,
}

/// Validate that `output` is usable as an output directory:
/// 1. Create it if it doesn't exist.
/// 2. Reject paths that exist but aren't a directory.
/// 3. Probe writability by creating and removing a temporary file.
/// 4. If `expected_bytes` is known, warn when free disk space is below it.
///
/// Errors are returned for fatal problems (path is a file, can't create the
/// directory, can't write to it). Soft issues like "low disk space" are
/// returned as warning strings in `OutputValidation.warnings`.
pub fn validate_output(output: &Path, expected_bytes: Option<u64>) -> Result<OutputValidation> {
    let mut warnings = Vec::new();

    if output.exists() {
        if !output.is_dir() {
            bail!(
                "output path exists and is not a directory: {}",
                output.display()
            );
        }
    } else {
        fs::create_dir_all(output)
            .with_context(|| format!("could not create output directory {}", output.display()))?;
    }

    let probe = output.join(format!(".media-convert-write-probe-{}", std::process::id()));
    fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&probe)
        .with_context(|| format!("output directory is not writable: {}", output.display()))?;
    let _ = fs::remove_file(&probe);

    match fs4::available_space(output) {
        Ok(avail) => {
            if let Some(expected) = expected_bytes
                && avail < expected
            {
                warnings.push(format!(
                    "Output filesystem has only {} free, but source files total {}. \
                     Re-encoding usually shrinks files, but this could be tight.",
                    format_bytes(avail),
                    format_bytes(expected)
                ));
            }
        }
        Err(e) => {
            warnings.push(format!(
                "Could not query disk space on {}: {e}",
                output.display()
            ));
        }
    }

    Ok(OutputValidation { warnings })
}

/// Sum the on-disk sizes of `paths`. Files that can't be stat'd are silently
/// skipped — this is a best-effort estimate for warning the user about disk
/// space, not a precondition.
pub fn sum_file_sizes(paths: &[PathBuf]) -> u64 {
    paths
        .iter()
        .filter_map(|p| fs::metadata(p).ok().map(|m| m.len()))
        .sum()
}

fn format_bytes(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = KIB * 1024;
    const GIB: u64 = MIB * 1024;
    const TIB: u64 = GIB * 1024;
    if bytes >= TIB {
        format!("{:.2} TiB", bytes as f64 / TIB as f64)
    } else if bytes >= GIB {
        format!("{:.2} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.2} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.2} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn absolute_directory_becomes_sibling_with_suffix() {
        let dir = tempdir().unwrap();
        let src = dir.path().join("library");
        fs::create_dir(&src).unwrap();

        let out = default_output(&src, Container::Mkv);
        assert_eq!(out, dir.path().join("library-converted"));
    }

    #[test]
    fn file_source_stays_in_same_directory_with_suffix() {
        let dir = tempdir().unwrap();
        let lib = dir.path().join("library");
        fs::create_dir(&lib).unwrap();
        let file = lib.join("movie.mp4");
        fs::write(&file, b"").unwrap();

        let out = default_output(&file, Container::Mkv);
        assert_eq!(out, lib.join("movie-converted.mkv"));
    }

    #[test]
    fn relative_directory_stays_relative() {
        let out = default_output(Path::new("movies"), Container::Mkv);
        assert_eq!(out, PathBuf::from("movies-converted"));
    }

    #[test]
    fn nested_relative_directory_keeps_parent() {
        let out = default_output(Path::new("a/b/library"), Container::Mkv);
        assert_eq!(out, PathBuf::from("a/b/library-converted"));
    }

    #[test]
    fn root_path_falls_back_to_subdirectory() {
        let out = default_output(Path::new("/"), Container::Mkv);
        assert_eq!(out, PathBuf::from("/converted"));
    }

    #[test]
    fn single_segment_absolute_path_stays_under_root() {
        let out = default_output(Path::new("/library"), Container::Mkv);
        assert_eq!(out, PathBuf::from("/library-converted"));
    }

    #[test]
    fn validate_output_creates_missing_directory() {
        let dir = tempdir().unwrap();
        let target = dir.path().join("new-output");
        assert!(!target.exists());

        let v = validate_output(&target, None).expect("should succeed");
        assert!(target.is_dir());
        assert!(v.warnings.is_empty());
    }

    #[test]
    fn validate_output_accepts_existing_directory() {
        let dir = tempdir().unwrap();
        let v = validate_output(dir.path(), None).expect("should succeed");
        assert!(v.warnings.is_empty());
    }

    #[test]
    fn validate_output_rejects_path_that_is_a_file() {
        let dir = tempdir().unwrap();
        let file = dir.path().join("not-a-dir");
        fs::write(&file, b"hello").unwrap();

        let err = validate_output(&file, None).unwrap_err();
        assert!(
            err.to_string().contains("not a directory"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn validate_output_warns_when_source_exceeds_free_space() {
        let dir = tempdir().unwrap();
        // Pass an absurdly large expected size so the warning fires regardless
        // of the filesystem the test runs on.
        let huge = u64::MAX / 2;
        let v = validate_output(dir.path(), Some(huge)).expect("should succeed");
        assert!(
            v.warnings.iter().any(|w| w.contains("free")),
            "expected a free-space warning, got {:?}",
            v.warnings
        );
    }

    #[test]
    fn validate_output_no_warnings_when_expected_size_fits() {
        let dir = tempdir().unwrap();
        // 1 KiB easily fits anywhere a tempdir lives.
        let v = validate_output(dir.path(), Some(1024)).expect("should succeed");
        assert!(
            v.warnings.is_empty(),
            "unexpected warnings: {:?}",
            v.warnings
        );
    }

    #[test]
    fn validate_output_leaves_no_probe_file_behind() {
        let dir = tempdir().unwrap();
        validate_output(dir.path(), None).unwrap();
        let leftover: Vec<_> = fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(".media-convert-write-probe")
            })
            .collect();
        assert!(leftover.is_empty(), "probe file was not cleaned up");
    }

    #[test]
    fn sum_file_sizes_totals_existing_files() {
        let dir = tempdir().unwrap();
        let a = dir.path().join("a");
        let b = dir.path().join("b");
        fs::write(&a, vec![0u8; 100]).unwrap();
        fs::write(&b, vec![0u8; 50]).unwrap();
        let total = sum_file_sizes(&[a, b]);
        assert_eq!(total, 150);
    }

    #[test]
    fn sum_file_sizes_skips_missing_files() {
        let dir = tempdir().unwrap();
        let a = dir.path().join("a");
        fs::write(&a, vec![0u8; 42]).unwrap();
        let missing = dir.path().join("nope");
        let total = sum_file_sizes(&[a, missing]);
        assert_eq!(total, 42);
    }
}
