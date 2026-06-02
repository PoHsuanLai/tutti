//! Audio buffer types shared by all plugin host crates.
//!
//! The [`Sample`] trait lets generic process functions dispatch to the
//! correct pointer-array layout at compile time. Per-format extensions
//! (e.g. `tutti-vst3-host`'s `Vst3Sample`) layer format-specific constants
//! on top.

use std::ffi::c_void;

/// Marker trait for plugin-compatible sample types (f32, f64).
///
/// Generic process code such as `Vst3Instance::process` /
/// `ClapInstance::process` uses `<T: Sample>` so the compiler monomorphizes
/// one specialization per concrete type.
pub trait Sample: Copy + Default + Send + 'static {
    /// Fill the appropriate [`BufferPtrs`] from the caller's input/output
    /// slices and return the two `*mut *mut c_void` arrays that C plugin
    /// APIs (VST3, AU) require.
    fn prepare_ffi_buffers(
        ptrs_f32: &mut BufferPtrs<f32>,
        ptrs_f64: &mut BufferPtrs<f64>,
        inputs: &[&[Self]],
        outputs: &mut [&mut [Self]],
    ) -> (*mut *mut c_void, *mut *mut c_void);
}

impl Sample for f32 {
    fn prepare_ffi_buffers(
        ptrs_f32: &mut BufferPtrs<f32>,
        _ptrs_f64: &mut BufferPtrs<f64>,
        inputs: &[&[Self]],
        outputs: &mut [&mut [Self]],
    ) -> (*mut *mut c_void, *mut *mut c_void) {
        ptrs_f32.prepare(inputs, outputs)
    }
}

impl Sample for f64 {
    fn prepare_ffi_buffers(
        _ptrs_f32: &mut BufferPtrs<f32>,
        ptrs_f64: &mut BufferPtrs<f64>,
        inputs: &[&[Self]],
        outputs: &mut [&mut [Self]],
    ) -> (*mut *mut c_void, *mut *mut c_void) {
        ptrs_f64.prepare(inputs, outputs)
    }
}

/// Pair of pre-allocated pointer arrays handed to a C plugin API on each
/// process call, one per bus direction. Allocated once per plugin instance
/// and reused so the realtime path is allocation-free.
pub struct BufferPtrs<T> {
    pub input: Vec<*mut T>,
    pub output: Vec<*mut T>,
}

unsafe impl<T> Send for BufferPtrs<T> {}
unsafe impl<T> Sync for BufferPtrs<T> {}

impl<T> BufferPtrs<T> {
    pub fn new(num_inputs: usize, num_outputs: usize) -> Self {
        Self {
            input: vec![std::ptr::null_mut(); num_inputs],
            output: vec![std::ptr::null_mut(); num_outputs],
        }
    }

    pub fn resize_inputs(&mut self, count: usize) {
        self.input = vec![std::ptr::null_mut(); count];
    }

    pub fn resize_outputs(&mut self, count: usize) {
        self.output = vec![std::ptr::null_mut(); count];
    }

    /// Fill pointer arrays from buffer slices, returning raw `*mut *mut
    /// c_void` for FFI. Input slices are cast to `*mut T` to satisfy C APIs
    /// that use `*mut *mut c_void` for both directions — well-behaved
    /// plugins must not mutate inputs.
    pub fn prepare(
        &mut self,
        inputs: &[&[T]],
        outputs: &mut [&mut [T]],
    ) -> (*mut *mut c_void, *mut *mut c_void) {
        for (i, input_slice) in inputs.iter().enumerate() {
            if i < self.input.len() {
                self.input[i] = input_slice.as_ptr() as *mut T;
            }
        }
        for (i, output_slice) in outputs.iter_mut().enumerate() {
            if i < self.output.len() {
                self.output[i] = output_slice.as_mut_ptr();
            }
        }
        (
            self.input.as_mut_ptr() as *mut *mut c_void,
            self.output.as_mut_ptr() as *mut *mut c_void,
        )
    }
}

/// Block of deinterleaved audio passed to a plugin's process call.
///
/// `inputs` and `outputs` borrow channel slices owned by the host. `T`
/// picks 32-bit or 64-bit processing.
pub struct AudioBuffer<'a, T: Sample = f32> {
    pub inputs: &'a [&'a [T]],
    pub outputs: &'a mut [&'a mut [T]],
    pub num_samples: usize,
    pub sample_rate: f64,
}

impl<'a, T: Sample> AudioBuffer<'a, T> {
    /// `num_samples` is derived from the first output channel's length, or
    /// the first input channel's length if there are no outputs.
    ///
    /// # Panics
    /// Panics if both `inputs` and `outputs` are empty.
    pub fn new(inputs: &'a [&'a [T]], outputs: &'a mut [&'a mut [T]], sample_rate: f64) -> Self {
        let num_samples = outputs
            .first()
            .map(|s| s.len())
            .or_else(|| inputs.first().map(|s| s.len()))
            .expect("AudioBuffer requires at least one input or output channel");
        Self {
            inputs,
            outputs,
            num_samples,
            sample_rate,
        }
    }

    pub fn num_inputs(&self) -> usize {
        self.inputs.len()
    }

    pub fn num_outputs(&self) -> usize {
        self.outputs.len()
    }

    pub fn clear_outputs(&mut self) {
        for output in self.outputs.iter_mut() {
            output.fill(T::default());
        }
    }
}

pub type AudioBuffer32<'a> = AudioBuffer<'a, f32>;
pub type AudioBuffer64<'a> = AudioBuffer<'a, f64>;
