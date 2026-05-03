# media-convert

Re-encode a media library to a target codec (HEVC/x265 or AV1), preserving the
source directory tree. Ships as a CLI and an egui-based GUI. Supports CPU
encoding (best compression) and three hardware backends, output to MKV or MP4,
and optional embedding of sidecar `.srt` subtitle files.

## Requirements

- `ffmpeg` and `ffprobe` on `PATH`, built with the encoders you intend to use:
  `libx265`, `libsvtav1`, and any of `hevc_nvenc`/`av1_nvenc`,
  `hevc_qsv`/`av1_qsv`, `hevc_vaapi`/`av1_vaapi`.
- Rust toolchain (stable, edition 2024).
- Vendor driver for hardware backends (see [Hardware backend prerequisites](#hardware-backend-prerequisites)).

### Install ffmpeg

Most distro packages of ffmpeg already include `libx265`, `libsvtav1`, NVENC,
QSV and VAAPI support — you just need the right vendor driver loaded.

**Debian / Ubuntu**
```bash
sudo apt update
sudo apt install ffmpeg
```

**Fedora / RHEL** (RPM Fusion provides the full-featured build)
```bash
sudo dnf install https://mirrors.rpmfusion.org/free/fedora/rpmfusion-free-release-$(rpm -E %fedora).noarch.rpm
sudo dnf install ffmpeg
```

**Arch / Manjaro**
```bash
sudo pacman -S ffmpeg
```

**macOS** (Homebrew — NVENC/QSV/VAAPI not applicable)
```bash
brew install ffmpeg
```

Verify the encoders you want are present:
```bash
ffmpeg -hide_banner -encoders | grep -E 'x265|svtav1|nvenc|qsv|vaapi'
```

### Install Rust

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
```
(or use your distro's `rustup` package, e.g. `sudo apt install rustup`).

### Hardware backend prerequisites

You only need the driver for the backend you plan to use.

- **NVENC** — install NVIDIA's proprietary driver (`nvidia-driver` on
  Debian/Ubuntu, `akmod-nvidia` on Fedora, `nvidia` on Arch). Verify with
  `nvidia-smi`. AV1 NVENC requires an RTX 40-series GPU or newer.
- **QSV** (Intel iGPU) — install the Intel media driver:
  ```bash
  sudo apt install intel-media-va-driver-non-free   # Debian/Ubuntu
  sudo dnf install intel-media-driver                # Fedora
  sudo pacman -S intel-media-driver                  # Arch
  ```
  AV1 QSV requires Arc / Xe / 11th-gen Intel or newer.
- **VAAPI** — for AMD: `mesa-va-drivers` (Debian/Ubuntu) or `mesa-vdpau-drivers`/
  `libva-mesa-driver` (Fedora/Arch). For Intel, the QSV driver above also
  provides VAAPI. Verify with `vainfo` (in `vainfo` / `libva-utils`).

## Install media-convert

```bash
./install.sh
```

Builds and installs `media-convert` and `media-convert-gui` to
`~/.local/bin` (override with `INSTALL_ROOT=/some/prefix`).

## CLI

```bash
media-convert --source ./in [--output ./out] [options]
```

`--source` accepts either a directory (scanned for video files) or a single
video file. `--output` is optional — if omitted, it defaults to a sibling
of the source directory named `<source>-converted` (for a file source, the
file's parent directory is used as the basis).

Common flags:

| Flag | Purpose |
|---|---|
| `-s, --source <path>` | Source directory or single video file |
| `-o, --output <path>` | Output directory (default: `<source>-converted` sibling) |
| `-c, --codec <x265\|av1>` | Target codec (default `x265`) |
| `-b, --backend <software\|nvenc\|qsv\|vaapi>` | Encoder backend (default `software`) |
| `--container <mkv\|mp4>` | Output container (default `mkv`) |
| `--quality <N>` | CRF/CQ/QP value (0-63); backend-aware default |
| `--preset <name>` | Encoder preset; backend-aware default |
| `--embed-subtitles` | Mux sidecar `.srt` files next to each source into the output |
| `--no-recurse` | Don't descend into subdirectories of `--source` |
| `--dry-run` | Scan and report what would be converted |
| `--force` | Re-encode even if source already matches target |

While encoding, the CLI prints a redrawn `[i/N] NN% file.<ext>` line per file.

### Examples

```bash
# Default output: ~/Videos-converted (sibling of the source)
media-convert -s ~/Videos

# Re-encode a library to HEVC/MKV, software (best compression)
media-convert -s ~/Videos -o ~/Videos-x265

# Hardware-accelerated AV1 with NVENC
media-convert -s ~/Videos -o ~/Videos-av1 -c av1 -b nvenc

# Single file, output to MP4 with mov_text subtitles
media-convert -s ./episode.mkv -o ./out --container mp4

# Embed any movie.en.srt / movie.ger.srt sidecars next to each input
media-convert -s ~/Videos -o ~/Videos-x265 --embed-subtitles

# Preview only — no encoding
media-convert -s ~/Videos -o ~/Videos-x265 --dry-run
```

## GUI

```bash
media-convert-gui
```

Pick a source (folder or single file) — the output directory is auto-filled
to `<source>-converted` and is editable. Configure codec / backend /
container / quality, **Scan**, then **Convert**. Hover any control to see a
tooltip explaining what the option does. The Status column shows live
`encoding NN%` per file. Toggle "Embed sidecar SRT subtitles" to mux any
`movie.srt` / `movie.en.srt`-style files alongside each source.

## Backends

| Backend | Vendor | Encoders | Notes |
|---|---|---|---|
| `software` | CPU | `libx265` / `libsvtav1` | Best compression, slowest. |
| `nvenc` | NVIDIA | `hevc_nvenc` / `av1_nvenc` | Fast; AV1 needs RTX 40-series or newer. |
| `qsv` | Intel iGPU | `hevc_qsv` / `av1_qsv` | AV1 needs Arc / Xe / 11th-gen+. |
| `vaapi` | Linux generic | `hevc_vaapi` / `av1_vaapi` | AMD and Intel. Uses `/dev/dri/renderD128`. |

GPU backends are much faster but produce larger files at equivalent visual
quality than software encoders.

### Quality scale (lower is better, except VAAPI)

- `software` / `nvenc`: `-crf` / `-cq`, default 23 (x265) or 28-30 (AV1).
- `qsv`: `-global_quality`, same defaults.
- `vaapi`: `-qp` (constant quantizer; sane range ~20-30).

### Preset

- `libx265`: `ultrafast`..`placebo` (default `medium`).
- `libsvtav1`: `0`-`13`, lower = slower/better (default `8`).
- `nvenc`: `p1`..`p7`, higher = slower/better (default `p4`).
- `qsv`: `veryfast`..`veryslow` (default `medium`).
- `vaapi`: ignored.

## Output

Each input is re-muxed at the mirrored path under `--output`, with the
extension determined by `--container`:

- **MKV** (default): preserves all tracks. `-map 0` plus `-c:a copy`,
  `-c:s copy`, `-c:d copy`, `-c:t copy` keeps audio, subtitles, data
  streams, and attachments (e.g. fonts) intact. Best for archival.
- **MP4**: selective mapping (`-map 0:v -map 0:a? -map 0:s?`). Subtitles
  are transcoded to `mov_text` (3GPP timed text); image-based subs (PGS,
  DVB), data streams, and attachments are dropped. Use this for broad
  player compatibility.

Files whose video stream already matches the target codec are skipped unless
`--force` is passed. Files whose output already exists are skipped
unconditionally — delete the output and re-run to redo a single file.

### Output validation

Before encoding, `media-convert` checks the output directory:

- **Errors** (abort): the path exists but isn't a directory, the directory
  can't be created, or it isn't writable. Writability is probed by creating
  and removing a unique temporary file.
- **Warnings** (continue): the source's total on-disk size exceeds the free
  space on the output filesystem. Re-encoding usually shrinks files, but
  this catches the case where the conversion might run out of room.

The CLI prints warnings to stderr and bails on errors. The GUI shows them in
a modal — the writability check runs at **Scan** time and the disk-space
check runs at **Convert** time once the file list is known.

## Subtitle handling

By default, subtitle tracks already inside the source are passed through
(MKV) or transcoded to `mov_text` (MP4). Pass `--embed-subtitles` to also
auto-discover sidecar `.srt` files next to each source and mux them in as
extra tracks. The discovery rules:

- A file in the same directory whose stem matches the video stem exactly,
  e.g. `movie.srt` next to `movie.mp4`.
- Or whose stem starts with `<video-stem>.<lang>`, e.g. `movie.en.srt`,
  `movie.ger.srt`. Two- or three-letter ASCII codes are tagged as the
  subtitle's `language` metadata.

## License

MIT — see [LICENSE](LICENSE).
