//! Deinterleaved channel buffers and the C-FFI pointer marshalling that hands
//! them to a plugin's process call.
//!
//! # The plugin boundary in the engine's I/O vocabulary
//!
//! In [`tutti_types::io`] terms a plugin is an [`AudioOut`](tutti_types::io::AudioOut)
//! back-to-back with an [`AudioIn`](tutti_types::io::AudioIn): its **input** side
//! is written (the host pushes a block in), its **output** side is polled (the
//! host pulls the processed block out), and the format-native `process` is the
//! private in→out step wedged between. [`AudioBuffer<T>`] is that back-to-back
//! carrier for one block — `inputs` is the AudioOut side, `outputs` the AudioIn
//! side. It does *not* implement the two traits literally: those speak flat
//! *interleaved* samples on a cold/block path, whereas the plugin ABI is
//! *deinterleaved* (one buffer per channel) on the RT audio thread, so the two
//! shapes deliberately don't unify — forcing an interleave transpose here would
//! add work to the hot path. The vocabulary names the *roles*; this module owns
//! the RT-planar realisation.
//!
//! The reason is **layout, not width**: interleaved-versus-planar is what keeps
//! the two apart, and a plugin bus's runtime width is no obstacle to either.
//!
//! Audio crosses the plugin boundary one buffer *per channel* (deinterleaved),
//! and the C plugin ABIs (VST3, AU) take those channel buffers as a `void**` —
//! a pointer to an array of per-channel pointers (`*mut *mut c_void` in Rust).
//! This module owns the three pieces that shape that hand-off:
//!
//! - [`AudioBuffer`] — the safe, borrowed view of one process block: a slice of
//!   input channel slices and a slice of output channel slices, plus the block
//!   length and sample rate. This is what host code reads and writes.
//! - [`BufferPtrs`] — the pre-allocated `void**` pointer scratch handed to the C
//!   API each block. Filled from an [`AudioBuffer`]'s slices without allocating,
//!   so the realtime path stays allocation-free.
//! - [`Sample`] — a compile-time switch (`f32` / `f64`) that routes a generic
//!   `process::<T>` call to the matching-width [`BufferPtrs`]. Per-format
//!   extensions (e.g. `tutti-vst3-host`'s `Vst3Sample`) layer format-specific
//!   constants on top.

use std::ffi::c_void;

/// Plugin-compatible sample width (`f32` or `f64`), used as a compile-time
/// switch by generic process code.
///
/// Generic process code such as `Vst3Instance::process` / `ClapActive::process`
/// is written `<T: Sample>`, so the compiler monomorphizes one specialization
/// per concrete width. This is the element type `S` of the engine's
/// [`AudioIn`](tutti_types::io::AudioIn) / [`AudioOut`](tutti_types::io::AudioOut)
/// vocabulary, narrowed to the two widths a plugin bus can negotiate: `f32`
/// (the whole edge world) and `f64` (a 64-bit plugin bus).
pub trait Sample: Copy + Default + Send + 'static {}

impl Sample for f32 {}

impl Sample for f64 {}

/// Pre-allocated channel-pointer arrays handed to a C plugin API on each
/// process call — one array per process direction (input / output).
///
/// Each `Vec` is the backing store for a `void**`: it holds one pointer per
/// channel (each pointing at that channel's deinterleaved sample buffer), and
/// [`prepare`](Self::prepare) returns the `Vec`s' bases for the C API. The
/// input table holds `*const T` and the output table `*mut T`, so the type
/// itself never makes a read-only buffer writable; the one `*const` to `*mut`
/// cast an ABI that wants `**mut` for inputs needs belongs at the FFI call,
/// where its safety argument is made.
///
/// Allocated once per plugin instance and reused, so the realtime `process`
/// path only *refills* the pointer slots (never grows the `Vec`s) and stays
/// allocation-free. An instance that can process at both widths holds one of
/// these per width.
///
/// The slots keep whatever the last [`prepare`](Self::prepare) wrote, so they
/// dangle between blocks; they are only valid for the block they were filled
/// from.
///
/// Sendable for the plugin sample widths:
///
/// ```
/// fn send_sync<T: Send + Sync>() {}
/// send_sync::<tutti_plugin_types::BufferPtrs<f32>>();
/// send_sync::<tutti_plugin_types::BufferPtrs<f64>>();
/// ```
///
/// but not for an element type that is not a [`Sample`]:
///
/// ```compile_fail
/// fn send<T: Send>() {}
/// send::<tutti_plugin_types::BufferPtrs<std::rc::Rc<u8>>>();
/// ```
///
/// and its slots are not reachable from safe code:
///
/// ```compile_fail
/// let mut p = tutti_plugin_types::BufferPtrs::<f32>::new(1, 1);
/// p.output[0] = std::ptr::null_mut();
/// ```
pub struct BufferPtrs<T> {
    input: Vec<*const T>,
    output: Vec<*mut T>,
}

// SAFETY: the tables are plain pointer slots. Nothing here dereferences them,
// and no `&self` method exposes them (`prepare` takes `&mut self`), so
// sharing a `&BufferPtrs` between threads reveals nothing. Moving the table
// moves pointers to `T` samples, which only a plugin call dereferences, and
// that call's caller answers for the pointee being valid and `T: Send`
// (the `Sample` bound) for the thread it runs on.
unsafe impl<T: Sample> Send for BufferPtrs<T> {}
// SAFETY: see `Send`; there is no shared-reference API at all.
unsafe impl<T: Sample> Sync for BufferPtrs<T> {}

impl<T> BufferPtrs<T> {
    /// Allocates null-filled pointer arrays for the given channel counts.
    ///
    /// Allocating: call at instance setup, never on the audio thread.
    pub fn new(num_inputs: usize, num_outputs: usize) -> Self {
        Self {
            input: vec![std::ptr::null(); num_inputs],
            output: vec![std::ptr::null_mut(); num_outputs],
        }
    }

    /// Reallocates the input array for a new channel count, discarding any
    /// pointers it held.
    ///
    /// Allocating: call when a bus is renegotiated, never on the audio thread.
    pub fn resize_inputs(&mut self, count: usize) {
        self.input = vec![std::ptr::null(); count];
    }

    /// Reallocates the output array for a new channel count, discarding any
    /// pointers it held.
    ///
    /// Allocating: call when a bus is renegotiated, never on the audio thread.
    pub fn resize_outputs(&mut self, count: usize) {
        self.output = vec![std::ptr::null_mut(); count];
    }

    /// Number of input channel slots.
    pub fn input_len(&self) -> usize {
        self.input.len()
    }

    /// Number of output channel slots.
    pub fn output_len(&self) -> usize {
        self.output.len()
    }

    /// Fills the pointer arrays from buffer slices, returning the
    /// `(inputs, outputs)` raw pair for FFI.
    ///
    /// Allocation-free, so it is the one method here that belongs on the audio
    /// thread. Channels beyond the arrays' capacity are **silently dropped** —
    /// this refills, it never grows, which is what keeps it RT-safe; size the
    /// arrays with [`new`](Self::new) or the `resize_*` methods first.
    ///
    /// The input pointers are `*const`: they point into shared borrows, and
    /// writing through them is undefined behaviour. A caller whose C API types
    /// inputs `*mut` must cast at the call and state in its `// SAFETY:` that
    /// the plugin's ABI contract is read-only inputs. The returned pointers
    /// borrow both `self` and the passed slices and are valid only until
    /// either moves or the slices' borrows end.
    pub fn prepare(
        &mut self,
        inputs: &[&[T]],
        outputs: &mut [&mut [T]],
    ) -> (*const *const c_void, *mut *mut c_void) {
        for (slot, input_slice) in self.input.iter_mut().zip(inputs) {
            *slot = input_slice.as_ptr();
        }
        for (slot, output_slice) in self.output.iter_mut().zip(outputs.iter_mut()) {
            *slot = output_slice.as_mut_ptr();
        }
        (
            self.input.as_ptr() as *const *const c_void,
            self.output.as_mut_ptr() as *mut *mut c_void,
        )
    }
}

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
/// [`PluginAudio::process`](crate::PluginAudio::process).
///
/// The enum keeps the trait dyn-compatible while letting each format's
/// implementation match once and delegate into a single generic inner body.
///
/// Carries [`AudioBuffer`]'s two lifetimes verbatim (`'t` = channel tables,
/// `'d` = sample data, `'d: 't`) so the split survives the enum boundary — a
/// caller can still build the output table with a short-lived borrow.
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

#[cfg(test)]
mod tests {
    use super::*;

    // The sendable widths stay sendable (a regression here breaks every
    // plugin host that moves its scratch to the audio thread).
    const _: () = {
        const fn send_sync<T: Send + Sync>() {}
        send_sync::<BufferPtrs<f32>>();
        send_sync::<BufferPtrs<f64>>();
    };

    /// `prepare` points each slot at its channel, leaves surplus channels out,
    /// and the input table is a `*const` table.
    ///
    /// Mutation: write `output_slice.as_mut_ptr()` into the input slot (or
    /// drop the `zip`, indexing past `input_len`) → the pointer checks fail
    /// or the surplus-channel call panics.
    #[test]
    fn prepare_fills_slots_and_drops_surplus_channels() {
        let mut p = BufferPtrs::<f32>::new(1, 1);
        assert_eq!((p.input_len(), p.output_len()), (1, 1));
        let (a, b) = ([1.0f32; 4], [2.0f32; 4]);
        let (mut o0, mut o1) = ([0.0f32; 4], [0.0f32; 4]);
        let (o0p, o1p) = (o0.as_mut_ptr(), o1.as_mut_ptr());
        let (ins, outs) = p.prepare(&[&a, &b], &mut [&mut o0, &mut o1]);
        // SAFETY: one slot per table, filled just above; the buffers live on.
        unsafe {
            assert_eq!(*(ins as *const *const f32), a.as_ptr());
            assert_eq!(*(outs as *const *mut f32), o0p);
        }
        assert_ne!(o0p, o1p);
    }
}
