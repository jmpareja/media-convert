use clap::ValueEnum;

use crate::backend::Backend;

#[derive(Copy, Clone, Debug, Default, PartialEq, Eq, ValueEnum)]
#[value(rename_all = "lowercase")]
pub enum Upscale {
    /// Encode at the source's native resolution (no scaling).
    #[default]
    None,
    /// Upscale to 1920x1080 with Lanczos resampling, preserving the source's
    /// display aspect ratio (letter/pillarboxed with black where needed).
    #[value(name = "1080p")]
    To1080p,
}

impl Upscale {
    pub fn label(self) -> &'static str {
        match self {
            Upscale::None => "none",
            Upscale::To1080p => "1080p",
        }
    }

    /// Build the ffmpeg filter-chain fragment that performs this upscale on
    /// the given backend, or `None` when no scaling is requested.
    ///
    /// The chain scales the source to fit the target box, pads the remainder
    /// with black, and stamps SAR=1 so non-square-pixel sources (DVD, SDTV)
    /// render correctly on square-pixel displays.
    ///
    /// NVENC stays on the GPU for the resize and round-trips through system
    /// RAM for `pad` (stock ffmpeg has no `pad_cuda`); other backends scale
    /// on the CPU. For VAAPI the caller composes this fragment with the
    /// backend's own `format=nv12,hwupload` so the padded frames land on the
    /// VAAPI device.
    pub fn filter(self, backend: Backend) -> Option<String> {
        let (w, h) = match self {
            Upscale::None => return None,
            Upscale::To1080p => (1920u32, 1080u32),
        };
        Some(match backend {
            Backend::Nvenc => format!(
                "scale_cuda={w}:{h}:force_original_aspect_ratio=decrease:interp_algo=lanczos,\
                 hwdownload,format=nv12,\
                 pad={w}:{h}:(ow-iw)/2:(oh-ih)/2,setsar=1,hwupload_cuda"
            ),
            _ => format!(
                "scale={w}:{h}:flags=lanczos:force_original_aspect_ratio=decrease,\
                 pad={w}:{h}:(ow-iw)/2:(oh-ih)/2,setsar=1"
            ),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn label_matches_value_name() {
        assert_eq!(Upscale::None.label(), "none");
        assert_eq!(Upscale::To1080p.label(), "1080p");
    }

    #[test]
    fn default_is_none() {
        assert_eq!(Upscale::default(), Upscale::None);
    }

    #[test]
    fn none_produces_no_filter_for_any_backend() {
        for b in [
            Backend::Software,
            Backend::Nvenc,
            Backend::Qsv,
            Backend::Vaapi,
        ] {
            assert!(Upscale::None.filter(b).is_none());
        }
    }

    #[test]
    fn software_uses_cpu_scale_and_pad_at_1080p() {
        let f = Upscale::To1080p.filter(Backend::Software).unwrap();
        assert!(f.contains("scale=1920:1080:flags=lanczos"));
        assert!(f.contains("force_original_aspect_ratio=decrease"));
        assert!(f.contains("pad=1920:1080:(ow-iw)/2:(oh-ih)/2"));
        assert!(f.contains("setsar=1"));
        assert!(!f.contains("scale_cuda"));
    }

    #[test]
    fn qsv_and_vaapi_use_cpu_scale_path() {
        // QSV has no `scale_qsv` in the stock config (no preamble hwaccel),
        // so the CPU scale path applies. VAAPI also scales on CPU here; its
        // own `format=nv12,hwupload` filter is appended by the caller.
        for b in [Backend::Qsv, Backend::Vaapi] {
            let f = Upscale::To1080p.filter(b).unwrap();
            assert!(f.contains("scale=1920:1080:flags=lanczos"));
            assert!(!f.contains("scale_cuda"));
        }
    }

    #[test]
    fn nvenc_uses_cuda_scale_with_hwdownload_pad_hwupload() {
        let f = Upscale::To1080p.filter(Backend::Nvenc).unwrap();
        // GPU-side resize first
        assert!(f.contains("scale_cuda=1920:1080:force_original_aspect_ratio=decrease"));
        assert!(f.contains("interp_algo=lanczos"));
        // Round-trip through system RAM for the pad
        assert!(f.contains("hwdownload,format=nv12"));
        assert!(f.contains("pad=1920:1080:(ow-iw)/2:(oh-ih)/2"));
        assert!(f.contains("setsar=1"));
        // Re-upload to GPU so NVENC consumes CUDA frames
        assert!(f.ends_with("hwupload_cuda"));
    }
}
