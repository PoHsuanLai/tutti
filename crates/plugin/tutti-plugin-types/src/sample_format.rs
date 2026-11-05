//! Sample-format discriminator that travels on the wire.

/// Which sample width the host and plugin exchange audio in.
///
/// Negotiated at load: the host states a preference and the subprocess replies
/// with what the plugin actually accepted.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum SampleFormat {
    /// 32-bit float, the default and what every format supports.
    #[default]
    Float32,
    /// 64-bit float, offered only by plugins that declare double precision.
    Float64,
}
