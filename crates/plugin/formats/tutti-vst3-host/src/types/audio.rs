//! VST3-specific sample-format extension on top of `tutti_plugin_types::Sample`.

use vst3::Steinberg::Vst::{AudioBusBuffers, SymbolicSampleSizes_};

pub(crate) const K_SAMPLE_32_INT: i32 = SymbolicSampleSizes_::kSample32 as i32;
pub(crate) const K_SAMPLE_64_INT: i32 = SymbolicSampleSizes_::kSample64 as i32;

/// Adds the VST3 format-specific FFI details on top of the shared
/// [`tutti_plugin_types::Sample`] trait: the `symbolicSampleSize` tag and the
/// matching `AudioBusBuffers` union member. Generic process code uses
/// `<T: Vst3Sample>` to get both without branching on the format.
pub trait Vst3Sample: tutti_plugin_types::Sample {
    /// The `kSample32` or `kSample64` constant the plugin expects in
    /// `ProcessData::symbolicSampleSize`.
    const VST3_SYMBOLIC_SIZE: i32;

    /// Store a channel-pointer table into `bus`'s buffer union via the member
    /// that matches this format.
    ///
    /// `channelBuffers32`/`channelBuffers64` overlay the same machine pointer (a
    /// pointer's width is independent of its pointee's), so either member writes
    /// the same bytes; the plugin reads whichever [`VST3_SYMBOLIC_SIZE`] names.
    /// Each impl writes only its own member, so the f32 path never mentions an
    /// f64 cast and vice versa.
    ///
    /// [`VST3_SYMBOLIC_SIZE`]: Self::VST3_SYMBOLIC_SIZE
    fn set_channel_buffers(bus: &mut AudioBusBuffers, channel_ptrs: *mut *mut std::ffi::c_void);
}

impl Vst3Sample for f32 {
    const VST3_SYMBOLIC_SIZE: i32 = K_SAMPLE_32_INT;

    fn set_channel_buffers(bus: &mut AudioBusBuffers, channel_ptrs: *mut *mut std::ffi::c_void) {
        bus.__field0.channelBuffers32 = channel_ptrs as *mut *mut f32;
    }
}

impl Vst3Sample for f64 {
    const VST3_SYMBOLIC_SIZE: i32 = K_SAMPLE_64_INT;

    fn set_channel_buffers(bus: &mut AudioBusBuffers, channel_ptrs: *mut *mut std::ffi::c_void) {
        bus.__field0.channelBuffers64 = channel_ptrs as *mut *mut f64;
    }
}
