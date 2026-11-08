//! One callback's worth of output work, with no device and no thread.
//!
//! [`process_audio`] is the *inner* seam — the engine render, callable without
//! a device. Everything around it in the output callback lives here: the
//! [`MAX_FRAMES`] clamp, the zero-fill and the sample-format conversion. Keeping it outside
//! the closure handed to `device.build_output_stream` is what lets a test run
//! it without a sound card.
//!
//! [`OutputBlock`] is the *outer* seam, and it maps onto `tutti_io`'s pump
//! split exactly:
//!
//! | `tutti-io` | here | owns |
//! |---|---|---|
//! | `PumpLoop` | [`OutputBlock`] | one pass' work: buffers, layout, stride |
//! | `PumpDriver` | `StreamDriver` | who calls it, and on what thread |
//! | `RunningPump` | `RunningStream` | the handle whose drop is the stop |
//!
//! [`OutputBlock::render`] is **not a second implementation of the callback** —
//! the same claim `tutti_io::ManualDriver`'s doc makes about
//! `PumpLoop::pump_once`. `CpalDriver`'s closure body is
//! `move |data, _| block.render(data)`, and a manual driver calls the identical
//! method.
//!
//! The `debug_assert!` on the callback size deliberately does **not** live
//! here: an over-sized buffer is a claim about *CPAL's* contract, not about
//! this block, so it belongs at the driver boundary. Keeping it out is also
//! what lets a debug-build test observe the clamp instead of panicking before
//! it.

use std::sync::Arc;
use tutti_types::{ChannelLayout, InterleavedMut};

use crate::output::{AudioCallbackState, MAX_FRAMES, process_audio};

/// The working set one output stream's callback reads, sized once.
///
/// Every buffer here is allocated in [`OutputBlock::new`], on the control
/// thread, and never resized: a CPAL callback larger than [`MAX_FRAMES`] is
/// clamped in [`render`](OutputBlock::render) and its tail silenced rather
/// than reallocating on the audio thread.
pub struct OutputBlock {
    state: Arc<AudioCallbackState>,
    /// The device's true channel layout. The engine folds the graph root to
    /// this width (declared with [`Engine::support_output`](tutti_engine::Engine::support_output)
    /// when the block is built); a device wider than `MAX_ROOT_CHANNELS` (rare)
    /// simply gets silent extra channels — the root renders ≤ 8 and the fold
    /// zero-fills the rest.
    layout: ChannelLayout,
    /// The interleave stride for both the mix buffer and the device buffer,
    /// derived ONCE here — never inside the per-frame loops.
    channels: usize,
    buffer: Vec<f32>,
    last_rendered_frames: usize,
}

impl std::fmt::Debug for OutputBlock {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OutputBlock")
            .field("state", &self.state)
            .field("layout", &self.layout)
            .field("channels", &self.channels)
            .field("last_rendered_frames", &self.last_rendered_frames)
            .finish_non_exhaustive()
    }
}

impl OutputBlock {
    /// Allocate the callback's working set for a device of `layout` width.
    ///
    /// Control-thread only. This is the only allocation on the output path,
    /// and the one place the device's width reaches the engine: it builds the
    /// engine's fold from the graph root to the device
    /// ([`Engine::support_output`](tutti_engine::Engine::support_output)), so the
    /// callback never does.
    ///
    /// # Panics
    ///
    /// If `layout` has no channels: a device has at least one.
    pub fn new(state: Arc<AudioCallbackState>, layout: ChannelLayout) -> Self {
        state
            .engine
            .support_output(layout)
            .expect("a device has at least one channel");
        let channels = layout.count() as usize;
        Self {
            state,
            layout,
            channels,
            buffer: vec![0.0f32; MAX_FRAMES * channels],
            last_rendered_frames: 0,
        }
    }

    /// The device width this block was sized for.
    pub fn layout(&self) -> ChannelLayout {
        self.layout
    }

    /// The interleave stride — `layout().count()`, hoisted.
    pub fn channels(&self) -> usize {
        self.channels
    }

    /// Frames the last [`render`](Self::render) actually produced, after the
    /// [`MAX_FRAMES`] clamp.
    ///
    /// Exists so a test can assert the clamp happened rather than infer it
    /// from a silent tail.
    pub fn last_rendered_frames(&self) -> usize {
        self.last_rendered_frames
    }

    /// Render one block and write it to `data` in the device's sample format.
    ///
    /// This is the entire output callback. CPAL calls it with whatever buffer
    /// the backend handed over; a manual driver calls it with a buffer of the
    /// caller's choosing.
    ///
    /// # Real-time
    ///
    /// Runs on the audio thread. Must not allocate, lock, or block — see
    /// `tests/rt_no_alloc.rs`.
    #[inline]
    pub fn render<T>(&mut self, data: &mut [T])
    where
        T: cpal::SizedSample + cpal::FromSample<f32>,
    {
        let channels = self.channels;
        // Clamp so we never allocate. If CPAL hands us a larger buffer we
        // process the head and write silence to the tail. The assertion that
        // this should not happen lives at the driver boundary, not here.
        let frames = (data.len() / channels).min(MAX_FRAMES);
        self.last_rendered_frames = frames;

        let mix = &mut self.buffer[..frames * channels];
        // Zero before rendering — the previous callback's contents are not
        // meaningful input for the graph.
        mix.fill(0.0);
        let mut mix = InterleavedMut::new(mix, self.layout);
        process_audio(&self.state, &mut mix);
        // Back to a flat slice for the device write.
        let mix = mix.as_ref().samples();

        write_output(data, channels, mix, frames);
    }
}

/// Convert the rendered f32 mix into the device's sample format.
///
/// `data` stays a bare `&mut [T]` and `channels` a bare `usize`: `T` is
/// `cpal::SizedSample` (i16, u32, f64, …), so `InterleavedMut` — which is
/// f32-only by construction — cannot describe the destination. Widening the
/// newtype over `T` would buy nothing here, because the only arithmetic in this
/// function is `i / channels`, and the width it needs is the *source's*, which
/// the caller already reads off the `InterleavedMut` it built. The vocabulary
/// stops at the format boundary, as it does at the C ABI and WIT boundaries.
///
/// Private on purpose: it is reached through [`OutputBlock::render`], which is
/// how CPAL reaches it. A parallel public conversion helper would be a second
/// implementation of the matrix under test.
#[inline]
fn write_output<T: cpal::SizedSample + cpal::FromSample<f32>>(
    data: &mut [T],
    channels: usize,
    output: &[f32],
    rendered_frames: usize,
) {
    // `output` is already `channels`-wide interleaved (the engine folded the
    // graph root to the device width). Copy every channel through; frames past
    // what we rendered (an over-sized CPAL callback) get silence.
    let silence = T::from_sample(0.0);
    for (i, sample) in data.iter_mut().enumerate() {
        let frame = i / channels;
        *sample = if frame < rendered_frames {
            T::from_sample(output[i])
        } else {
            silence
        };
    }
}
