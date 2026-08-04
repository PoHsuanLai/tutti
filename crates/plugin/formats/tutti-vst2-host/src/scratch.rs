//! Pre-allocated scratch buffers for VST2 process calls.
//!
//! The `vst` crate's `AudioBuffer` wants raw pointer arrays; we can't
//! point directly at the caller's `&[&[f32]]` slice-of-slices because
//! VST2 wants contiguous pointer tables. These scratch buffers are
//! allocated once at load time and reused on every block to keep the
//! audio thread allocation-free.
//!
//! Both widths are resident. VST 2.4 has two render entry points —
//! `processReplacing` and `processReplacingF64` — and the caller picks per
//! block, not per instance: `InProcessVst2Client` implements `AudioUnit`
//! at both widths on one object, and the subprocess loader switches on the
//! incoming `AudioBufferMut` variant. Neither knows at construction which
//! it will be asked for, and growing a buffer on the first f64 block would
//! allocate on the audio thread.

use tutti_plugin_types::ChannelLayout;

/// Scratch buffers for VST2's audio pointer tables, at both sample widths.
///
/// Owns contiguous per-channel `Vec<f32>` / `Vec<f64>` plus the parallel
/// pointer tables that the `vst` crate's `AudioBuffer<f32>` /
/// `AudioBuffer<f64>` consume. Sized at construction to `num_inputs` /
/// `num_outputs` channels × `block_size` samples.
///
/// Carrying both costs `block_size * (in + out) * 12` bytes over one width —
/// at a 512-sample block and stereo I/O, 24 KiB.
pub struct RenderScratch {
    pub(crate) inputs: Vec<Vec<f32>>,
    pub(crate) outputs: Vec<Vec<f32>>,
    pub(crate) input_ptrs: Vec<*const f32>,
    pub(crate) output_ptrs: Vec<*mut f32>,
    pub(crate) inputs_f64: Vec<Vec<f64>>,
    pub(crate) outputs_f64: Vec<Vec<f64>>,
    pub(crate) input_ptrs_f64: Vec<*const f64>,
    pub(crate) output_ptrs_f64: Vec<*mut f64>,
}

// SAFETY: the raw pointers point into the owned `Vec<Vec<_>>` fields
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

        let inputs_f64: Vec<Vec<f64>> = (0..num_inputs.count())
            .map(|_| vec![0.0f64; block_size])
            .collect();
        let outputs_f64: Vec<Vec<f64>> = (0..num_outputs.count())
            .map(|_| vec![0.0f64; block_size])
            .collect();
        let input_ptrs_f64: Vec<*const f64> = inputs_f64.iter().map(|v| v.as_ptr()).collect();
        let output_ptrs_f64: Vec<*mut f64> = outputs_f64
            .iter()
            .map(|v| v.as_ptr() as *mut f64)
            .collect();

        Self {
            inputs,
            outputs,
            input_ptrs,
            output_ptrs,
            inputs_f64,
            outputs_f64,
            input_ptrs_f64,
            output_ptrs_f64,
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

    /// Copy caller's f64 inputs into the f64 scratch Vecs, zero missing
    /// channels, and refresh the f64 pointer table.
    ///
    /// The `zip` stops at the shorter of scratch channel and caller slice, so
    /// a caller slice shorter than `num_samples` leaves the tail at whatever
    /// the previous block wrote. That tail is never read: `process_block_f64`
    /// hands the plugin `num_samples`, and a caller that declared more samples
    /// than it supplied has already mis-stated its own block.
    pub fn prepare_f64(&mut self, caller_inputs: &[&[f64]], num_samples: usize) {
        for (i, src) in caller_inputs.iter().enumerate() {
            if i < self.inputs_f64.len() {
                for (d, &s) in self.inputs_f64[i][..num_samples].iter_mut().zip(*src) {
                    *d = s;
                }
            }
        }
        for i in caller_inputs.len()..self.inputs_f64.len() {
            self.inputs_f64[i][..num_samples].fill(0.0);
        }
        for ch in &mut self.outputs_f64 {
            ch[..num_samples].fill(0.0);
        }
        self.refresh_ptrs_f64();
    }

    /// Cast caller's f64 inputs down into the *f32* scratch — the fallback for
    /// a plugin that never installed `processReplacingF64`.
    ///
    /// Paired with [`copy_out_f64_from_f32`](Self::copy_out_f64_from_f32);
    /// [`Vst2Instance::process_f64`](crate::Vst2Instance::process_f64) picks
    /// this pair only when the plugin's `effFlagsCanDoubleReplacing` is clear.
    pub fn prepare_f64_as_f32(&mut self, caller_inputs: &[&[f64]], num_samples: usize) {
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

    /// Copy the f64 scratch outputs back to the caller's f64 channels.
    ///
    /// Bounded on both sides for the same reason as
    /// [`copy_out_f32`](Self::copy_out_f32): `out_channel[..num_samples]` alone
    /// panics on a caller slice *shorter* than the block, the mirror image of the
    /// `copy_from_slice` fault. The `zip` already stopped at the scratch's end;
    /// the `min` is what makes the destination safe too.
    pub fn copy_out_f64(&self, caller_outputs: &mut [&mut [f64]], num_samples: usize) {
        for (i, out_channel) in caller_outputs.iter_mut().enumerate() {
            if i < self.outputs_f64.len() {
                let n = num_samples.min(out_channel.len());
                for (o, &s) in out_channel[..n].iter_mut().zip(&self.outputs_f64[i]) {
                    *o = s;
                }
            }
        }
    }

    /// Widen the *f32* scratch outputs into the caller's f64 channels — the
    /// read half of the fallback pair described on
    /// [`prepare_f64_as_f32`](Self::prepare_f64_as_f32). Bounded like
    /// [`copy_out_f64`](Self::copy_out_f64).
    pub fn copy_out_f64_from_f32(&self, caller_outputs: &mut [&mut [f64]], num_samples: usize) {
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

    /// [`refresh_ptrs`](Self::refresh_ptrs) for the f64 tables.
    fn refresh_ptrs_f64(&mut self) {
        for (ptr, ch) in self.input_ptrs_f64.iter_mut().zip(&self.inputs_f64) {
            *ptr = ch.as_ptr();
        }
        for (ptr, ch) in self.output_ptrs_f64.iter_mut().zip(&mut self.outputs_f64) {
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
        let layout = ChannelLayout::from(channels);
        let mut s = RenderScratch::new(layout, layout, block);
        for (c, ch) in s.outputs.iter_mut().enumerate() {
            for (i, v) in ch.iter_mut().enumerate() {
                *v = (c * 100 + i) as f32;
            }
        }
        for (c, ch) in s.outputs_f64.iter_mut().enumerate() {
            for (i, v) in ch.iter_mut().enumerate() {
                *v = (c * 100 + i) as f64;
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

    /// The downcasting fallback is bounded the same way, so choosing it for a
    /// plugin without `processReplacingF64` cannot panic where the native path
    /// would not.
    #[test]
    fn copy_out_f64_from_f32_is_bounded_on_both_sides() {
        let s = scratch_with_ramp(1, 8);

        let mut short = vec![-1.0f64; 3];
        let mut outs: Vec<&mut [f64]> = vec![&mut short];
        s.copy_out_f64_from_f32(&mut outs, 8);
        assert_eq!(short, vec![0.0, 1.0, 2.0]);

        let mut long = vec![-1.0f64; 12];
        let mut outs: Vec<&mut [f64]> = vec![&mut long];
        s.copy_out_f64_from_f32(&mut outs, 8);
        assert_eq!(&long[..8], &[0.0, 1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0]);
        assert!(long[8..].iter().all(|&v| v == -1.0));
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

    /// `prepare_f64` must carry a value that has no f32 representation through
    /// unchanged. The staging step is where the old path lost it, before the
    /// plugin was ever entered.
    #[test]
    fn prepare_f64_stages_without_narrowing() {
        // Needs 53 bits of mantissa: exactly representable in f64, rounds in f32.
        const EXACT: f64 = 1.0 + f64::EPSILON;
        let layout = ChannelLayout::from(1u16);
        let mut s = RenderScratch::new(layout, layout, 4);

        let src = [EXACT; 4];
        s.prepare_f64(&[&src[..]], 4);

        assert_eq!(
            &s.inputs_f64[0][..4],
            &[EXACT; 4][..],
            "the f64 staging buffer must hold the input bit-for-bit"
        );
        assert_ne!(
            EXACT as f32 as f64,
            EXACT,
            "the fixture is vacuous unless this value actually rounds in f32"
        );
    }
}
