//! The per-buffer graph render, called from the audio callback.
//!
//! [`Engine`] ticks the DSP graph + transport and renders one output buffer per
//! block.

use crate::transport::Declick;
use crate::transport::MotionFsm;
use crate::{AudioThreadCell, ChannelLayout, Ordering};
use fundsp::audiounit::AudioUnit;
use fundsp::buffer::BufferArray;
use fundsp::prelude::{BufferRef, U8};
use fundsp::realnet::NetBackend;
use fundsp::MAX_BUFFER_SIZE;

/// Widest graph root [`Engine::process_segment`] renders without dropping
/// channels.
///
/// The scratch is stack-allocated, so this is a fixed ceiling rather than the
/// root's actual width. Eight covers mono through 7.1; a wider root still
/// renders, but only its first two channels reach the interleaved stereo
/// output — the device stream this feeds is stereo regardless.
pub const MAX_ROOT_CHANNELS: usize = 8;

/// Type-level [`MAX_ROOT_CHANNELS`], for sizing the scratch [`BufferArray`].
type MaxRootChannels = U8;

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
    /// empty; the planar per-channel output is interleaved into `output` as
    /// stereo — a mono root duplicates channel 0 into both sides, a wider root
    /// takes its first two channels.
    ///
    /// The scratch is a stack-allocated [`BufferArray`] sized to
    /// [`MAX_ROOT_CHANNELS`], sliced down to the root's actual output count
    /// before each `process` call. **The slicing is load-bearing**:
    /// `Net::process` iterates `output.channels()` and indexes its own
    /// `output_edge` table by that channel, so handing it a two-channel buffer
    /// for a one-output net indexes past the end and panics — in release, inside
    /// the audio callback. Sizing the buffer to the net is what makes a mono
    /// root reach the duplication below at all.
    #[inline]
    pub fn process_segment(&self, output: &mut [f32], frames: usize) {
        let Some(ref mut backend) = *self.net_backend.borrow_mut() else {
            return;
        };

        debug_assert!(backend.inputs() == 0);

        // Channels the root actually produces, clamped to what the scratch can
        // hold. A root wider than the scratch has its extra channels dropped —
        // the interleave below is stereo either way — but it must not be allowed
        // to index past the buffer.
        let root_channels = backend.outputs().clamp(1, MAX_ROOT_CHANNELS);
        let mono = ChannelLayout::from(root_channels) == ChannelLayout::Mono;

        let empty_input = BufferRef::new(&[]);
        let mut scratch = BufferArray::<MaxRootChannels>::new();

        let mut done = 0;
        while done < frames {
            let block = (frames - done).min(MAX_BUFFER_SIZE);

            // Slice to the root's width so `Net::process` iterates exactly the
            // channels it has edges for.
            let mut full = scratch.buffer_mut();
            let mut buffer_mut = full.subset(0, root_channels);
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
