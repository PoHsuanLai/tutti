//! Deinterleaved channel buffers for a plugin's process call.
//!
//! # A plugin is a node, not an edge
//!
//! In the graph a plugin is a `tutti_graph::Node`: the graph's clock calls it one
//! block at a time, planar. The engine's edge vocabulary (`tutti_types`'
//! `Source` and `Sink`, interleaved frames on a clock the graph does not keep)
//! does not describe it, and [`AudioBuffer`] is not an edge: it is the borrowed,
//! deinterleaved view of one process block on the RT audio thread, the shape the
//! plugin ABIs take.
//!
//! Audio crosses the plugin boundary one buffer *per channel* (deinterleaved),
//! and the C plugin ABIs (VST3, AU) take those channel buffers as a `void**` —
//! a pointer to an array of per-channel pointers (`*mut *mut c_void` in Rust).
//! Each format's `-ffi` crate builds that table; this module owns the safe side:
//!
//! - [`AudioBuffer`] — the safe, borrowed view of one process block: a slice of
//!   input channel slices and a slice of output channel slices, plus the block
//!   length and sample rate. This is what host code reads and writes.
//! - [`Sample`] — a compile-time switch (`f32` / `f64`) that routes a generic
//!   `process::<T>` call to the matching width. Per-format
//!   extensions (e.g. `tutti-vst3-host`'s `Vst3Sample`) layer format-specific
//!   constants on top.

/// The most channels, per direction, that a block's stack-built channel tables
/// hold (the `&[&[T]]` and `&mut [&mut [T]]` slice tables of the plugin server's
/// audio pipeline and of the in-process node's `process`).
///
/// 16 covers every realistic plugin layout: most are mono or stereo and
/// surround tops out at 7.1 (8 channels), so 16 leaves room for 7.1.4 (12) and
/// a pair of buses. A wider plugin's channels past it read silence and its
/// outputs past it are silent. The tables are fixed arrays so a block builds
/// them without allocating on the audio thread.
pub const MAX_BLOCK_CHANNELS: usize = 16;

/// Plugin-compatible sample width (`f32` or `f64`), used as a compile-time
/// switch by generic process code.
///
/// Generic process code such as `Vst3Instance::process` / `ClapProcessor::process`
/// is written `<T: Sample>`, so the compiler monomorphizes one specialization
/// per concrete width: the two a plugin bus can negotiate, `f32` and `f64` (a
/// 64-bit plugin bus).
pub trait Sample: Copy + Default + Send + 'static {}

impl Sample for f32 {}

impl Sample for f64 {}

/// A block of deinterleaved audio passed to a plugin's process call.
///
/// `inputs` and `outputs` borrow channel slices owned by the host. `T`
/// picks 32-bit or 64-bit processing.
///
/// Two lifetimes, not one: `'t` is how long the *channel tables* (the outer
/// `&[…]` / `&mut […]`) are borrowed, `'d` how long the per-channel sample
/// *data* lives, with `'d: 't` (data outlives the table). Splitting them lets
/// a caller build the output table with a plain `for … zip` loop — the borrow
/// checker sees the short-lived outer array's `Drop` as ending at `'t`, so it
/// does not collide with the `'d` data borrows. Made coincident, the two force a
/// caller into a hand-unrolled `split_first_mut` recursion to sidestep a
/// conflict that was never real.
pub struct AudioBuffer<'t, 'd: 't, T: Sample = f32> {
    /// One deinterleaved slice per input channel, each `num_samples` long.
    pub inputs: &'t [&'d [T]],
    /// One deinterleaved slice per output channel, each `num_samples` long.
    /// Contents on entry are undefined — a plugin that does not write every
    /// channel leaves whatever the previous block did; see
    /// [`clear_outputs`](Self::clear_outputs).
    pub outputs: &'t mut [&'d mut [T]],
    /// Block length in **frames**, not samples: each channel slice holds this
    /// many elements, so the total sample count is `num_samples * channels`.
    pub num_samples: usize,
    /// Render rate in Hz. A raw `f64` because it is handed straight to a C ABI,
    /// which is where the unit types stop.
    pub sample_rate: f64,
}

impl<T: Sample> std::fmt::Debug for AudioBuffer<'_, '_, T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioBuffer")
            .field("inputs", &self.inputs.len())
            .field("outputs", &self.outputs.len())
            .field("num_samples", &self.num_samples)
            .field("sample_rate", &self.sample_rate)
            .finish_non_exhaustive()
    }
}

impl<'t, 'd: 't, T: Sample> AudioBuffer<'t, 'd, T> {
    /// Creates a block over the given channel tables.
    ///
    /// `num_samples` is taken from the first output channel's length, or the
    /// first input channel's length if there are no outputs.
    ///
    /// # Panics
    /// Panics if both `inputs` and `outputs` are empty.
    pub fn new(inputs: &'t [&'d [T]], outputs: &'t mut [&'d mut [T]], sample_rate: f64) -> Self {
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

    /// Number of input channels.
    pub fn num_inputs(&self) -> usize {
        self.inputs.len()
    }

    /// Number of output channels, which need not equal
    /// [`num_inputs`](Self::num_inputs).
    pub fn num_outputs(&self) -> usize {
        self.outputs.len()
    }

    /// Zeroes every output channel.
    ///
    /// The way to make a silent block actually silent: an output slice's
    /// contents are otherwise whatever the last block left, so a plugin that
    /// declines to render repeats stale audio rather than going quiet.
    /// Allocation-free and RT-safe.
    pub fn clear_outputs(&mut self) {
        for output in self.outputs.iter_mut() {
            output.fill(T::default());
        }
    }
}

/// An [`AudioBuffer`] of 32-bit samples — the width every format negotiates.
pub type AudioBuffer32<'t, 'd> = AudioBuffer<'t, 'd, f32>;
/// An [`AudioBuffer`] of 64-bit samples, for a plugin that negotiated a 64-bit
/// bus.
pub type AudioBuffer64<'t, 'd> = AudioBuffer<'t, 'd, f64>;

/// An [`AudioBuffer`] tagged with its sample width, as handed to
/// [`Processor::process`](crate::Processor::process).
///
/// The enum keeps the trait dyn-compatible while letting each format's
/// implementation match once and delegate into a single generic inner body.
///
/// Carries [`AudioBuffer`]'s two lifetimes verbatim (`'t` = channel tables,
/// `'d` = sample data, `'d: 't`) so the split survives the enum boundary — a
/// caller can still build the output table with a short-lived borrow.
#[derive(Debug)]
pub enum AudioBufferMut<'t, 'd: 't> {
    /// A 32-bit block.
    F32(AudioBuffer<'t, 'd, f32>),
    /// A 64-bit block, from a plugin that negotiated a 64-bit bus.
    F64(AudioBuffer<'t, 'd, f64>),
}

impl<'t, 'd: 't> AudioBufferMut<'t, 'd> {
    /// Block length in **frames**, whichever width this carries.
    pub fn num_samples(&self) -> usize {
        match self {
            Self::F32(b) => b.num_samples,
            Self::F64(b) => b.num_samples,
        }
    }

    /// Render rate in Hz, whichever width this carries.
    pub fn sample_rate(&self) -> f64 {
        match self {
            Self::F32(b) => b.sample_rate,
            Self::F64(b) => b.sample_rate,
        }
    }
}
