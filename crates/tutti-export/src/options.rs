//! Public option types — every variant the user picks from when
//! configuring an export.

/// Noise added before quantizing, to decorrelate quantization error from the
/// signal.
///
/// Scaled to one LSB at the format's integer width, so it is a no-op for a
/// format that does not quantize (`Pcm::Float32`, and Ogg Vorbis).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum Dither {
    /// No noise. Quantization error stays correlated with the signal, which is
    /// audible as distortion on quiet material.
    Off,
    /// One uniform draw per sample (RPDF).
    Rectangular,
    /// Two uniform draws summed (TPDF) — decorrelates the noise from the signal
    /// in a way a single draw does not. The default.
    #[default]
    Triangular,
}
