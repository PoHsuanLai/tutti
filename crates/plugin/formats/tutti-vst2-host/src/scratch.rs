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

use tutti_plugin_types::ChannelLayout;

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
    pub fn new(num_inputs: ChannelLayout, num_outputs: ChannelLayout, block_size: usize) -> Self {
        let inputs: Vec<Vec<f32>> = (0..num_inputs.count())
            .map(|_| vec![0.0f32; block_size])
            .collect();
        let outputs: Vec<Vec<f32>> = (0..num_outputs.count())
            .map(|_| vec![0.0f32; block_size])
            .collect();
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
    ///
    /// Writes exactly `num_samples` per channel, which is the block length the
    /// plugin was asked to render — a caller whose slices are *longer* than that
    /// keeps whatever was past the block, same as the f64 path.
    ///
    /// Both sides are sliced deliberately. This was `copy_from_slice`, which
    /// requires equal lengths and so panicked whenever a caller passed a slice
    /// longer than `num_samples` — reachable through the public `process_f32`,
    /// which documents `num_samples` as a separate argument precisely so the two
    /// need not match.
    pub fn copy_out_f32(&self, caller_outputs: &mut [&mut [f32]], num_samples: usize) {
        for (i, out_channel) in caller_outputs.iter_mut().enumerate() {
            if i < self.outputs.len() {
                let n = num_samples
                    .min(out_channel.len())
                    .min(self.outputs[i].len());
                out_channel[..n].copy_from_slice(&self.outputs[i][..n]);
            }
        }
    }

    /// Cast scratch outputs back up to the caller's f64 channels.
    ///
    /// Bounded on both sides for the same reason as
    /// [`copy_out_f32`](Self::copy_out_f32): `out_channel[..num_samples]` alone
    /// panics on a caller slice *shorter* than the block, the mirror image of the
    /// `copy_from_slice` fault. The `zip` already stopped at the scratch's end;
    /// the `min` is what makes the destination safe too.
    pub fn copy_out_f64(&self, caller_outputs: &mut [&mut [f64]], num_samples: usize) {
        for (i, out_channel) in caller_outputs.iter_mut().enumerate() {
            if i < self.outputs.len() {
                let n = num_samples.min(out_channel.len());
                for (o, &s) in out_channel[..n].iter_mut().zip(&self.outputs[i]) {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Fill the scratch outputs with a recognizable ramp so a copy-out can be
    /// checked sample by sample.
    fn scratch_with_ramp(channels: u16, block: usize) -> RenderScratch {
        let layout = ChannelLayout::from_count(channels);
        let mut s = RenderScratch::new(layout, layout, block);
        for (c, ch) in s.outputs.iter_mut().enumerate() {
            for (i, v) in ch.iter_mut().enumerate() {
                *v = (c * 100 + i) as f32;
            }
        }
        s
    }

    /// A caller slice LONGER than the rendered block must not panic, and must
    /// keep whatever lay past the block.
    ///
    /// This is the `copy_from_slice` fault: equal-length-or-panic, against a
    /// public API that takes `num_samples` separately precisely so the caller's
    /// buffer may be larger.
    #[test]
    fn copy_out_f32_tolerates_a_caller_slice_longer_than_the_block() {
        let s = scratch_with_ramp(2, 4);
        let mut l = vec![-1.0f32; 8];
        let mut r = vec![-1.0f32; 8];
        let mut outs: Vec<&mut [f32]> = vec![&mut l, &mut r];

        s.copy_out_f32(&mut outs, 4);

        assert_eq!(&l[..4], &[0.0, 1.0, 2.0, 3.0], "block is copied");
        assert!(
            l[4..].iter().all(|&v| v == -1.0),
            "samples past the block are left alone, got {:?}",
            &l[4..]
        );
        assert_eq!(&r[..4], &[100.0, 101.0, 102.0, 103.0]);
    }

    /// And a caller slice SHORTER than the block must not panic either — it is
    /// filled as far as it goes.
    #[test]
    fn copy_out_f32_tolerates_a_caller_slice_shorter_than_the_block() {
        let s = scratch_with_ramp(1, 8);
        let mut short = vec![-1.0f32; 3];
        let mut outs: Vec<&mut [f32]> = vec![&mut short];

        s.copy_out_f32(&mut outs, 8);

        assert_eq!(short, vec![0.0, 1.0, 2.0], "filled to the caller's length");
    }

    /// The f64 path has the mirror-image hazard — `out[..num_samples]` panics on
    /// a short caller slice — so it is bounded on both sides too.
    #[test]
    fn copy_out_f64_is_bounded_on_both_sides() {
        let s = scratch_with_ramp(1, 8);

        let mut short = vec![-1.0f64; 3];
        let mut outs: Vec<&mut [f64]> = vec![&mut short];
        s.copy_out_f64(&mut outs, 8);
        assert_eq!(short, vec![0.0, 1.0, 2.0]);

        let mut long = vec![-1.0f64; 12];
        let mut outs: Vec<&mut [f64]> = vec![&mut long];
        s.copy_out_f64(&mut outs, 8);
        assert_eq!(&long[..8], &[0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0]);
        assert!(
            long[8..].iter().all(|&v| v == -1.0),
            "past the block stays untouched"
        );
    }

    /// More caller channels than the scratch holds: the extras are skipped, not
    /// indexed into.
    #[test]
    fn extra_caller_channels_are_ignored() {
        let s = scratch_with_ramp(1, 4);
        let (mut a, mut b) = (vec![-1.0f32; 4], vec![-1.0f32; 4]);
        let mut outs: Vec<&mut [f32]> = vec![&mut a, &mut b];

        s.copy_out_f32(&mut outs, 4);

        assert_eq!(a, vec![0.0, 1.0, 2.0, 3.0]);
        assert!(
            b.iter().all(|&v| v == -1.0),
            "channel 1 has no scratch behind it"
        );
    }
}
