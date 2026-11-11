//! Sample-format discriminator that travels on the wire.

/// Which sample width the host and plugin exchange audio in.
///
/// Negotiated at load: the host states a preference and the subprocess replies
/// with what the plugin actually accepted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum SampleFormat {
    /// 32-bit float, the default and what every format supports.
    #[default]
    Float32,
    /// 64-bit float, offered only by plugins that declare double precision.
    Float64,
}

/// The format an activation runs at: the one the host `requested`, unless it
/// is 64-bit and the plugin cannot take it, which falls back to 32-bit, the
/// width every format accepts.
///
/// One decision for every format's loader, so `Features::F64_AUDIO` (set from
/// the result) means "negotiated" and not "the plugin could".
///
/// ```
/// use tutti_plugin_types::{SampleFormat, negotiate};
/// assert_eq!(negotiate(SampleFormat::Float64, true), SampleFormat::Float64);
/// assert_eq!(negotiate(SampleFormat::Float64, false), SampleFormat::Float32);
/// assert_eq!(negotiate(SampleFormat::Float32, true), SampleFormat::Float32);
/// ```
#[must_use]
pub fn negotiate(requested: SampleFormat, supports_f64: bool) -> SampleFormat {
    match requested {
        SampleFormat::Float64 if supports_f64 => SampleFormat::Float64,
        _ => SampleFormat::Float32,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole table: 64-bit only when asked for and supported; a plugin's
    /// capability alone never raises a 32-bit request.
    #[test]
    fn negotiation_table() {
        use SampleFormat::{Float32 as F32, Float64 as F64};
        for (requested, supports, want) in [
            (F32, false, F32),
            (F32, true, F32),
            (F64, false, F32),
            (F64, true, F64),
        ] {
            assert_eq!(
                negotiate(requested, supports),
                want,
                "{requested:?} {supports}"
            );
        }
    }
}
