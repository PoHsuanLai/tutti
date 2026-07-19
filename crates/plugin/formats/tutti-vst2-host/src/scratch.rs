//! Pre-allocated scratch buffers for VST2 process calls.
//!
//! The `vst` crate's `AudioBuffer` wants raw pointer arrays; we can't
//! point directly at the caller's `&[&[f32]]` slice-of-slices because
//! VST2 wants contiguous pointer tables. These scratch buffers are
//! allocated once at load time and reused on every block to keep the
//! audio thread allocation-free.
//!
//! Also handles VST2's f32-only constraint: [`prepare_f64`] casts the
//! caller's `f64` samples down to `f32` on the way in, and
//! [`copy_out_f64`] casts back on the way out.

/// Scratch buffers for VST2's f32 audio pointer tables.
///
/// Owns contiguous per-channel `Vec<f32>` plus parallel `Vec<*const f32>`
/// / `Vec<*mut f32>` pointer tables that the `vst` crate's `AudioBuffer`
/// can consume. Sized at construction to `num_inputs` / `num_outputs`
/// channels × `block_size` samples.
pub struct RenderScratch {
    pub(crate) inputs: Vec<Vec<f32>>,
    pub(crate) outputs: Vec<Vec<f32>>,
    pub(crate) input_ptrs: Vec<*const f32>,
    pub(crate) output_ptrs: Vec<*mut f32>,
}

// SAFETY: the raw pointers point into the owned `Vec<Vec<f32>>` fields
// within the same struct. They are never read concurrently — callers
// ensure exclusive access (subprocess server holds a single instance;
// in-process backend serializes via Mutex). `Sync` is needed because
// `tutti-plugin`'s in-process audio unit requires `Send + Sync`
// (fundsp's `dyn AudioUnit` bound), even though every actual touch of
// the pointers is serialized.
unsafe impl Send for RenderScratch {}
unsafe impl Sync for RenderScratch {}

impl RenderScratch {
    pub fn new(num_inputs: usize, num_outputs: usize, block_size: usize) -> Self {
        let inputs: Vec<Vec<f32>> = (0..num_inputs).map(|_| vec![0.0f32; block_size]).collect();
        let outputs: Vec<Vec<f32>> = (0..num_outputs).map(|_| vec![0.0f32; block_size]).collect();
        let input_ptrs: Vec<*const f32> = inputs.iter().map(|v| v.as_ptr()).collect();
        let output_ptrs: Vec<*mut f32> = outputs.iter().map(|v| v.as_ptr() as *mut f32).collect();
        Self {
            inputs,
            outputs,
            input_ptrs,
            output_ptrs,
        }
    }

    /// Copy caller's f32 inputs into the scratch Vecs, zero missing channels,
    /// and refresh the pointer table.
    pub fn prepare_f32(&mut self, caller_inputs: &[&[f32]], num_samples: usize) {
        for (i, src) in caller_inputs.iter().enumerate() {
            if i < self.inputs.len() {
                self.inputs[i][..num_samples].copy_from_slice(&src[..num_samples]);
            }
        }
        for i in caller_inputs.len()..self.inputs.len() {
            self.inputs[i][..num_samples].fill(0.0);
        }
        for ch in &mut self.outputs {
            ch[..num_samples].fill(0.0);
        }
        self.refresh_ptrs();
    }

    /// Cast caller's f64 inputs down to f32 scratch, zero missing channels,
    /// and refresh the pointer table.
    pub fn prepare_f64(&mut self, caller_inputs: &[&[f64]], num_samples: usize) {
        for (i, src) in caller_inputs.iter().enumerate() {
            if i < self.inputs.len() {
                for (d, &s) in self.inputs[i][..num_samples].iter_mut().zip(*src) {
                    *d = s as f32;
                }
            }
        }
        for i in caller_inputs.len()..self.inputs.len() {
            self.inputs[i][..num_samples].fill(0.0);
        }
        for ch in &mut self.outputs {
            ch[..num_samples].fill(0.0);
        }
        self.refresh_ptrs();
    }

    /// Copy scratch outputs back to the caller's f32 channels.
    pub fn copy_out_f32(&self, caller_outputs: &mut [&mut [f32]], num_samples: usize) {
        for (i, out_channel) in caller_outputs.iter_mut().enumerate() {
            if i < self.outputs.len() {
                out_channel.copy_from_slice(&self.outputs[i][..num_samples]);
            }
        }
    }

    /// Cast scratch outputs back up to the caller's f64 channels.
    pub fn copy_out_f64(&self, caller_outputs: &mut [&mut [f64]], num_samples: usize) {
        for (i, out_channel) in caller_outputs.iter_mut().enumerate() {
            if i < self.outputs.len() {
                for (o, &s) in out_channel[..num_samples].iter_mut().zip(&self.outputs[i]) {
                    *o = s as f64;
                }
            }
        }
    }

    /// Pointers into the Vecs can become stale after any mutation — refresh
    /// them before each `vst_buffer.from_raw` call.
    ///
    /// In practice the Vec never reallocates (same shape, same capacity),
    /// so this is paranoia. Cheap paranoia.
    fn refresh_ptrs(&mut self) {
        for (ptr, ch) in self.input_ptrs.iter_mut().zip(&self.inputs) {
            *ptr = ch.as_ptr();
        }
        for (ptr, ch) in self.output_ptrs.iter_mut().zip(&mut self.outputs) {
            *ptr = ch.as_mut_ptr();
        }
    }
}
