//! The per-buffer graph render, called from the audio callback.
//!
//! [`Engine`] ticks the DSP graph + transport and renders one output buffer per
//! block.

use crate::transport::Declick;
use crate::transport::MotionFsm;
use crate::{AudioThreadCell, InterleavedMut, Ordering};
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
    /// Build an engine over a transport's motion FSM and a committed graph
    /// backend.
    ///
    /// `net_backend` is the audio-thread half of fundsp's `Net`; the control
    /// thread keeps the frontend and hands changes over by committing.
    pub fn new(motion: MotionFsm, net_backend: NetBackend) -> Self {
        let declick = motion.declick.clone();
        Self {
            motion,
            net_backend: AudioThreadCell::new(Some(net_backend)),
            declick,
        }
    }

    /// Render the whole of `output` — an interleaved device buffer that carries
    /// its own width.
    ///
    /// Drives the graph through fundsp's SIMD block path
    /// ([`NetBackend::process`]) in [`MAX_BUFFER_SIZE`] chunks rather than one
    /// frame at a time. The graph root has no inputs, so the input buffer is
    /// empty. The root is rendered at its **own** output width (up to
    /// [`MAX_ROOT_CHANNELS`]) into the stack scratch, then each frame is folded
    /// to the *output's* width — the device / target width — via the ITU/Dolby
    /// matrices ([`tutti_types::downmix`]): a surround root plays folded to a
    /// stereo device, or straight through to a matching-width surround device; a
    /// mono root duplicates into every target channel of a wider output.
    ///
    /// # Three widths are live here; only one is the buffer's
    ///
    /// The output width (`out_ch`) comes from `output`'s own layout; the root's
    /// (`root_channels`) from `backend.outputs()`, clamped to the scratch. They are
    /// different numbers from different sources, and confusing them writes past the
    /// end of one buffer or reads garbage from the other. The output width arrives
    /// welded to the buffer it strides, which is what makes the third confusion —
    /// a width disagreeing with its slice — unrepresentable rather than merely
    /// unlikely.
    ///
    /// The scratch is a stack-allocated [`BufferArray`] sized to
    /// [`MAX_ROOT_CHANNELS`], **sliced to the root's actual output count** before
    /// each `process` call. The slicing is load-bearing: `Net::process` iterates
    /// `output.channels()` and indexes its own `output_edge` table by that
    /// channel, so handing it a wider buffer than the net's width indexes past
    /// the end and panics — in release, inside the audio callback. The whole path
    /// is alloc-free (stack scratch + a stack `[f32; MAX_ROOT_CHANNELS]` frame).
    #[inline]
    pub fn process_segment(&self, output: &mut InterleavedMut<'_>) {
        let Some(ref mut backend) = *self.net_backend.borrow_mut() else {
            return;
        };
        debug_assert!(backend.inputs() == 0);

        // Destructure at the top of the body, per the `Interleaved` rule: the
        // stride and the frame count are read ONCE here and the loops below
        // index raw. Nothing inside a loop calls back into the type.
        let out_ch = output.stride();
        let frames = output.len();
        let output = output.samples_mut();

        // Drain any pending frontend commit BEFORE reading the output arity, so a
        // just-committed width change (e.g. the master going surround via
        // `commit_output_arity_change`) is reflected in `outputs()` and the
        // scratch is sliced to the new width this same block. `process` below
        // also drains, but it reads the width from the buffer sliced here — so
        // the pump must happen first. Cheap and RT-safe: it only swaps in an
        // already-allocated net from the commit queue (no allocation).
        backend.pump();

        // The root's real width, clamped to what the scratch can hold. A wider
        // root drops its extra channels (they can't be rendered), but must never
        // index past the buffer.
        // NOTE this is the ROOT's width, not the output buffer's. `out_ch`
        // above is the output's, and it is NOT clamped to MAX_ROOT_CHANNELS —
        // `fold_frame` writes exactly `out_ch` channels (zero-filling any past
        // the root width), so a wider-than-8 device simply gets silent extra
        // channels. Only the render scratch is bounded.
        let root_channels = backend.outputs().clamp(1, MAX_ROOT_CHANNELS);

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

    /// Apply the declick fade-out gain ramp to the interleaved output buffer.
    /// Returns true if the fade completed during this buffer. The ramp is
    /// per-frame — the same gain applies across every channel of a frame, so it
    /// works at any width.
    #[inline]
    fn apply_declick(&self, output: &mut InterleavedMut<'_>) -> bool {
        let remaining = self.declick.remaining().get();
        if remaining == 0 {
            return false;
        }

        let total = self.declick.total().get() as f32;
        if total == 0.0 {
            return false;
        }

        // Stride and frame count derived ONCE, above both loops below — the
        // per-sample loop must not touch the type.
        let channels = output.stride();
        let frames = output.len();
        let output = output.samples_mut();
        // Both operands are frame counts in one type, so the fade length and
        // the block length cannot be compared as bare integers by accident.
        let frames_to_process = remaining.min(frames);

        for i in 0..frames_to_process {
            let r = remaining - i - 1;
            let gain = r as f32 / total;
            for c in 0..channels {
                output[i * channels + c] *= gain;
            }
        }

        // Silence any remaining frames after the fade completes
        if frames_to_process < frames {
            for s in &mut output[frames_to_process * channels..frames * channels] {
                *s = 0.0;
            }
        }

        let new_remaining = remaining.saturating_sub(frames);
        self.declick
            .remaining
            .store(new_remaining as u32, Ordering::Release);

        new_remaining == 0
    }

    /// Render one block into `output`, an interleaved device buffer.
    ///
    /// How many frames is `output`'s own business — it is `output.len()`,
    /// which cannot disagree with the slice the way a separate `frames`
    /// argument could. The graph root is folded to `output`'s width (the device
    /// / target width) via the ITU/Dolby matrices — see
    /// [`process_segment`](Self::process_segment). Called once per block from
    /// the audio callback. RT-safe: no allocation, no locks, no I/O.
    #[inline]
    pub fn process(&self, output: &mut InterleavedMut<'_>) {
        self.motion.drain();
        self.process_segment(output);

        if self.apply_declick(output) {
            self.motion.complete_declick();
        }
    }

    /// Reset the audio-thread ownership assertions on both cells.
    ///
    /// Call when the device switches and a different thread takes over the
    /// callback: `AudioThreadCell` pins the first thread that borrows it and
    /// panics in debug builds on any other, so a new callback thread must be
    /// announced rather than discovered.
    pub fn reset_owners(&self) {
        self.net_backend.reset_owner();
        self.motion.reset_owner();
    }
}
