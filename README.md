# media-convert

Re-encode a media library to a target codec (HEVC/x265 or AV1), preserving the
source directory tree. Ships as a CLI and an egui-based GUI. Supports CPU
encoding (best compression) and three hardware backends.

## Requirements

- `ffmpeg` and `ffprobe` on `PATH` (build with the encoders you intend to use:
  `libx265`, `libsvtav1`, and any of `hevc_nvenc`/`av1_nvenc`,
  `hevc_qsv`/`av1_qsv`, `hevc_vaapi`/`av1_vaapi`).
- Rust toolchain (stable, edition 2024).

## Install

```bash
./install.sh
```

Builds and installs `media-convert` and `media-convert-gui` to
`~/.local/bin` (override with `INSTALL_ROOT=/some/prefix`).

## CLI

```bash
media-convert --source ./in --output ./out [options]
```

Common flags:

| Flag | Purpose |
|---|---|
| `-c, --codec <x265\|av1>` | Target codec (default `x265`) |
| `-b, --backend <software\|nvenc\|qsv\|vaapi>` | Encoder backend (default `software`) |
| `--quality <N>` | CRF/CQ/QP value; backend-aware default |
| `--preset <name>` | Encoder preset; backend-aware default |
| `--dry-run` | Scan and report what would be converted |
| `--force` | Re-encode even if source already matches target |

While encoding, the CLI prints a redrawn `[i/N] NN% file.mkv` line per file.

## GUI

```bash
media-convert-gui
```

Pick source and output directories, configure codec/backend/quality, **Scan**,
then **Convert**. The Status column shows live `encoding NN%` per file.

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

Each input is re-muxed into `.mkv` at the mirrored path under `--output`. All
audio, subtitle, data and attachment streams are stream-copied (`-c:a copy`,
`-c:s copy`, `-c:d copy`, `-c:t copy`). Files whose video stream already
matches the target codec are skipped unless `--force` is passed.

## License

MIT — see [LICENSE](LICENSE).
