//! The per-buffer graph render, called from the audio callback.
//!
//! [`Engine`] ticks the DSP graph + transport and renders one output buffer per
//! block.

use crate::transport::Declick;
use crate::transport::MotionFsm;
use crate::{AudioThreadCell, Ordering};
use fundsp::audiounit::AudioUnit;
use fundsp::buffer::BufferArray;
use fundsp::prelude::{BufferRef, U2};
use fundsp::realnet::NetBackend;
use fundsp::MAX_BUFFER_SIZE;

/// The audio engine: ticks the DSP graph + transport and renders one output
/// buffer per block from the audio callback.
pub struct Engine {
    motion: MotionFsm,
    net_backend: AudioThreadCell<Option<NetBackend>>,
    /// Cached from the transport so the fade path avoids a double deref.
    declick: Declick,
}

impl Engine {
    pub fn new(motion: MotionFsm, net_backend: NetBackend) -> Self {
        let declick = motion.declick.clone();
        Self {
            motion,
            net_backend: AudioThreadCell::new(Some(net_backend)),
            declick,
        }
    }

    /// Process a segment of the buffer.
    ///
    /// Drives the graph through fundsp's SIMD block path
    /// ([`NetBackend::process`]) in [`MAX_BUFFER_SIZE`] chunks rather than one
    /// frame at a time. The graph root has no inputs, so the input buffer is
    /// empty; the planar per-channel output is interleaved into `output`.
    /// A mono net (1 output) duplicates channel 0 into both L/R, matching the
    /// old per-sample `get_stereo()` behaviour exactly. The scratch
    /// [`BufferArray`] is stack-allocated, so the hot path stays alloc-free.
    #[inline]
    pub fn process_segment(&self, output: &mut [f32], frames: usize) {
        let Some(ref mut backend) = *self.net_backend.borrow_mut() else {
            return;
        };

        // 0 inputs on the graph root; 1 output → duplicate, 2 → straight L/R.
        // Any other count is a graph misconfiguration the old path panicked on.
        let outputs = backend.outputs();
        debug_assert!(backend.inputs() == 0);
        debug_assert!(
            outputs == 1 || outputs == 2,
            "graph root must have 1 or 2 outputs"
        );
        let mono = outputs == 1;

        let empty_input = BufferRef::new(&[]);
        let mut scratch = BufferArray::<U2>::new();

        let mut done = 0;
        while done < frames {
            let block = (frames - done).min(MAX_BUFFER_SIZE);

            let mut buffer_mut = scratch.buffer_mut();
            backend.process(block, &empty_input, &mut buffer_mut);

            let left = buffer_mut.channel_f32(0);
            let right = if mono {
                left
            } else {
                buffer_mut.channel_f32(1)
            };
            for i in 0..block {
                let o = (done + i) * 2;
                output[o] = left[i];
                output[o + 1] = right[i];
            }

            done += block;
        }
    }

    /// Apply declick fade-out gain ramp to the output buffer.
    /// Returns true if the fade completed during this buffer.
    #[inline]
    fn apply_declick(&self, output: &mut [f32], frames: usize) -> bool {
        let remaining = self.declick.remaining.load(Ordering::Acquire);
        if remaining == 0 {
            return false;
        }

        let total = self.declick.total.load(Ordering::Acquire) as f32;
        if total == 0.0 {
            return false;
        }

        let samples_to_process = (remaining as usize).min(frames);

        for i in 0..samples_to_process {
            let r = remaining - i as u32 - 1;
            let gain = r as f32 / total;
            output[i * 2] *= gain;
            output[i * 2 + 1] *= gain;
        }

        // Silence any remaining samples after the fade completes
        if samples_to_process < frames {
            for i in samples_to_process..frames {
                output[i * 2] = 0.0;
                output[i * 2 + 1] = 0.0;
            }
        }

        let new_remaining = remaining.saturating_sub(frames as u32);
        self.declick
            .remaining
            .store(new_remaining, Ordering::Release);

        new_remaining == 0
    }

    /// Render `frames` stereo samples into `output` (interleaved L/R).
    ///
    /// Called once per block from the audio callback. RT-safe: no allocation,
    /// no locks, no I/O.
    #[inline]
    pub fn process(&self, output: &mut [f32], frames: usize) {
        self.motion.drain();
        self.process_segment(output, frames);

        if self.apply_declick(output, frames) {
            self.motion.complete_declick();
        }
    }

    /// Reset `AudioThreadCell` owners for device switching.
    pub fn reset_owners(&self) {
        self.net_backend.reset_owner();
        self.motion.reset_owner();
    }
}
