//! VST3-specific sample-format extension on top of `tutti_plugin_types::Sample`.

use vst3::Steinberg::Vst::SymbolicSampleSizes_;

pub(crate) const K_SAMPLE_32_INT: i32 = SymbolicSampleSizes_::kSample32 as i32;
pub(crate) const K_SAMPLE_64_INT: i32 = SymbolicSampleSizes_::kSample64 as i32;

/// Adds the VST3 `symbolicSampleSize` constant on top of the shared
/// [`tutti_plugin_types::Sample`] trait. Generic process code uses
/// `<T: Vst3Sample>` to get both the FFI buffer prep and the format tag.
pub trait Vst3Sample: tutti_plugin_types::Sample {
    /// The `kSample32` or `kSample64` constant the plugin expects in
    /// `ProcessData::symbolicSampleSize`.
    const VST3_SYMBOLIC_SIZE: i32;
}

impl Vst3Sample for f32 {
    const VST3_SYMBOLIC_SIZE: i32 = K_SAMPLE_32_INT;
}

impl Vst3Sample for f64 {
    const VST3_SYMBOLIC_SIZE: i32 = K_SAMPLE_64_INT;
}
