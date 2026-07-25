//! The per-buffer graph render, called from the audio callback.
//!
//! [`Engine`] ticks the DSP graph + transport and renders one output buffer per
//! block.

use crate::transport::Declick;
use crate::transport::MotionFsm;
use crate::{AudioThreadCell, Ordering};
use fundsp::audiounit::AudioUnit;
use fundsp::buffer::BufferArray;
use fundsp::prelude::{BufferRef, U8};
use fundsp::realnet::NetBackend;
use fundsp::MAX_BUFFER_SIZE;

/// Widest graph root [`Engine::process_segment`] renders without dropping
/// channels — and the widest output (device) width it folds to. The scratch is
/// stack-allocated, so this is a fixed ceiling: mono through 7.1. A root or
/// device wider than this clamps (its extra channels are dropped / silent).
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

    /// Process a segment of the buffer into `channels`-wide interleaved `output`.
    ///
    /// Drives the graph through fundsp's SIMD block path
    /// ([`NetBackend::process`]) in [`MAX_BUFFER_SIZE`] chunks rather than one
    /// frame at a time. The graph root has no inputs, so the input buffer is
    /// empty. The root is rendered at its **own** output width (up to
    /// [`MAX_ROOT_CHANNELS`]) into the stack scratch, then each frame is folded
    /// to `channels` — the device / target width — via the ITU/Dolby matrices
    /// ([`tutti_types::downmix`]): a surround root plays folded to a stereo
    /// device, or straight through to a matching-width surround device; a mono
    /// root duplicates into every target channel of a wider output.
    ///
    /// The scratch is a stack-allocated [`BufferArray`] sized to
    /// [`MAX_ROOT_CHANNELS`], **sliced to the root's actual output count** before
    /// each `process` call. The slicing is load-bearing: `Net::process` iterates
    /// `output.channels()` and indexes its own `output_edge` table by that
    /// channel, so handing it a wider buffer than the net's width indexes past
    /// the end and panics — in release, inside the audio callback. The whole path
    /// is alloc-free (stack scratch + a stack `[f32; MAX_ROOT_CHANNELS]` frame).
    #[inline]
    pub fn process_segment(&self, output: &mut [f32], frames: usize, channels: usize) {
        let Some(ref mut backend) = *self.net_backend.borrow_mut() else {
            return;
        };
        debug_assert!(backend.inputs() == 0);

        // Drain any pending frontend commit BEFORE reading the output arity, so a
        // just-committed width change (e.g. the master going surround via
        // `commit_output_arity_change`) is reflected in `outputs()` and the
        // scratch is sliced to the new width this same block. `process` below
        // also drains, but it reads the width from the buffer we slice here — so
        // the pump must happen first. Cheap and RT-safe: it only swaps in an
        // already-allocated net from the commit queue (no allocation).
        backend.pump();

        // The root's real width, clamped to what the scratch can hold. A wider
        // root drops its extra channels (they can't be rendered), but must never
        // index past the buffer.
        let root_channels = backend.outputs().clamp(1, MAX_ROOT_CHANNELS);
        // The output (device) width sets the interleave stride. It is NOT clamped
        // to MAX_ROOT_CHANNELS — `fold_frame` writes exactly `out_ch` channels
        // (zero-filling any past the root width), so a wider-than-8 device simply
        // gets silent extra channels. Only the render scratch is bounded.
        let out_ch = channels.max(1);

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

            // Fold each planar frame (root_channels wide) to the interleaved
            // output width. Gather into a stack frame sliced to the root width —
            // no allocation.
            for i in 0..block {
                let mut frame = [0.0f32; MAX_ROOT_CHANNELS];
                let src = &mut frame[..root_channels];
                for (c, s) in src.iter_mut().enumerate() {
                    *s = buffer_mut.channel_f32(c)[i];
                }
                let o = (done + i) * out_ch;
                tutti_types::fold_frame(src, &mut output[o..o + out_ch]);
            }

            done += block;
        }
    }

    /// Apply the declick fade-out gain ramp to the `channels`-wide interleaved
    /// output buffer. Returns true if the fade completed during this buffer.
    /// The ramp is per-channel — the same gain applies across every channel of
    /// a frame, so it works at any width.
    #[inline]
    fn apply_declick(&self, output: &mut [f32], frames: usize, channels: usize) -> bool {
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
            for c in 0..channels {
                output[i * channels + c] *= gain;
            }
        }

        // Silence any remaining samples after the fade completes
        if samples_to_process < frames {
            for s in &mut output[samples_to_process * channels..frames * channels] {
                *s = 0.0;
            }
        }

        let new_remaining = remaining.saturating_sub(frames as u32);
        self.declick
            .remaining
            .store(new_remaining, Ordering::Release);

        new_remaining == 0
    }

    /// Render `frames` samples into `channels`-wide interleaved `output`.
    ///
    /// The graph root is folded to `channels` (the device / target width) via
    /// the ITU/Dolby matrices — see [`process_segment`](Self::process_segment).
    /// Called once per block from the audio callback. RT-safe: no allocation,
    /// no locks, no I/O.
    #[inline]
    pub fn process(&self, output: &mut [f32], frames: usize, channels: usize) {
        self.motion.drain();
        self.process_segment(output, frames, channels);

        if self.apply_declick(output, frames, channels) {
            self.motion.complete_declick();
        }
    }

    /// Reset `AudioThreadCell` owners for device switching.
    pub fn reset_owners(&self) {
        self.net_backend.reset_owner();
        self.motion.reset_owner();
    }
}
