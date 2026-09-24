//! The `AudioUnit` trait: the dynamic node interface the whole engine is built
//! on, and the one thing every processor in the graph implements.
//!
//! The trait is object-safe by construction — `Net` stores `Box<dyn AudioUnit>`
//! and is itself an `AudioUnit` — and generic over the sample format `S`, with
//! `F32` as the default so the overwhelmingly common `Box<dyn AudioUnit>` reads
//! unchanged. The generic is load-bearing rather than speculative: the plugin
//! hosts implement `AudioUnit<F64>`, so it cannot be specialized away.
//!
//! # What is here and what is not
//!
//! This module holds the trait and nothing else. The implementations that used
//! to sit beside it — `An<X>` (the `AudioNode` bridge), `Unit`,
//! `BigBlockAdapter`, `BlockRateAdapter`, `DummyUnit` — stayed in
//! `fundsp-tutti`, because each names `AudioNode` or `An`, which are that
//! crate's static-arity vocabulary and not part of the dynamic contract.
//!
//! Reading the split the other way round: what is here is exactly what a crate
//! outside the fork needs in order to *be* a node, and the 43 `impl AudioUnit`
//! sites across the engine confirm the line — every one of them implements this
//! trait and none of them defines an `AudioNode`.

use crate::buffer::{BufferMut, BufferRef};
use crate::math::AttoHash;
use crate::num::{Sample, F32};
use crate::setting::Setting;
use crate::signal::{Signal, SignalFrame};
use crate::value::Tail;
use dyn_clone::DynClone;

/// An audio processor with an object safe interface.
/// Once constructed, it has a fixed number of inputs and outputs.
///
/// Generic over sample format `S`. The default `S = F32` means existing code
/// using `AudioUnit` or `Box<dyn AudioUnit>` continues to work unchanged as f32.
/// Use `AudioUnit<F64>` for native f64 processing.
///
/// # What left the trait, and why `ping` did not
///
/// The fork's convenience methods — `get_mono`, `get_stereo`, `filter_mono`,
/// `filter_stereo`, `response`, `response_db`, `display` — were derived from
/// `tick`/`route` and had no caller outside the fork. They are
/// `fundsp_tutti::audiounit::AudioUnitExt` now, blanket-implemented there for
/// every `AudioUnit`, so the fork's tests and examples still reach them and no
/// engine node carries them (design doc 013, Phase 0).
///
/// `ping` and `set_hash` stay, although no engine node overrides either and
/// both are inert for every engine node. The fork's `Net` calls `ping` through
/// `Box<dyn AudioUnit>` on every `determine_order`, and that dynamic call is
/// the only way a fundsp generator held in a `Net` as an `An<X>` (its noise
/// sources and oscillators) receives its pseudorandom seed through `set_hash`.
/// A free function cannot recurse through a trait object into an `An<X>`, so
/// moving them out of the trait would silently unseed those fork nodes. They
/// go with `Net` in Phase 5.
///
/// The [`latency`](Self::latency) example is `ignore`d here and executed in the
/// fork: it builds its subjects with `fundsp_tutti::prelude64` constructors,
/// and the fork depends on THIS crate, so a doctest here naming one would be a
/// dev-dependency cycle. `fundsp_tutti::audiounit`'s test module runs it.
pub trait AudioUnit<S: Sample = F32>: Send + Sync + DynClone {
    /// Reset the input state of the unit to an initial state where it has not processed any data.
    /// In other words, reset time to zero.
    fn reset(&mut self) {
        // The default implementation does nothing.
    }

    /// Sever any shared *live* I/O so this unit is safe to tick in isolation on
    /// a worker thread, concurrently with the live graph.
    ///
    /// `Net::clone` (run on every `commit()`) shares some units' live input
    /// handles by `Arc` — e.g. a synth's MIDI inbox, a clip reader's command
    /// channel. That is correct for the frontend↔backend swap, where only one
    /// instance is ever ticked. But an *offline* clone (a region render) is
    /// ticked on a worker thread **while the original keeps playing**; a shared
    /// inbox means the worker drains events/commands the live unit needs (each
    /// is delivered to exactly one consumer), garbling live playback.
    ///
    /// Implementors that hold shared live input reset it to fresh, dead, empty
    /// local state here — after `isolate()` the unit reads nothing from the
    /// live world and steals nothing from it. Pure-DSP units share no live I/O,
    /// so the default does nothing. Called by the offline-render isolation pass
    /// on every node of the cloned net before it reaches the worker.
    ///
    /// This severs *inputs only*; re-pointing a unit at offline data (transport,
    /// scheduled events) is the data-carrying step [`Self::rebind_offline`]
    /// performs immediately after.
    fn isolate(&mut self) {
        // The default implementation does nothing.
    }

    /// Re-point this unit at the offline render's data — the half `isolate()`
    /// cannot do, because it carries no data.
    ///
    /// `isolate()` leaves a severed unit still aiming at the *live* transport,
    /// which nothing advances offline: a voice bound to it reads a frozen
    /// playhead and renders silence. Implementors holding a transport (or their
    /// own clock) re-seat it here.
    ///
    /// # Why the context is `&dyn Any`
    ///
    /// The offline context names a timeline, and this crate cannot name one —
    /// `Timeline` lives in `tutti-core`, which depends on *this* crate. Passing
    /// it opaquely keeps the hook where every node already is (beside `isolate`,
    /// reached through `Net` without a type switch) while letting the transport
    /// vocabulary stay downstream. Implementors downcast it once:
    ///
    /// ```ignore
    /// fn rebind_offline(&mut self, ctx: &dyn Any) {
    ///     let Some(transport) = ctx.downcast_ref::<OfflineTransport>() else { return };
    ///     self.transport = transport.clone();
    /// }
    /// ```
    ///
    /// **A transport-aware unit that does not implement this renders against
    /// the live playhead**, and does so silently — there is no value to compare
    /// and no error to raise. That is exactly why this is a defaulted method on
    /// the node rather than a match arm in the renderer: a new node declares its
    /// own rebinding and is covered automatically, where a renderer-side ladder
    /// would skip anything it had not been taught to name. Pure-DSP units are
    /// unaffected by time-of-render and correctly do nothing.
    #[allow(unused_variables)]
    fn rebind_offline(&mut self, ctx: &dyn core::any::Any) {
        // The default implementation does nothing.
    }

    /// Set the sample rate of the unit.
    /// The default sample rate is 44100 Hz.
    /// The unit is allowed to reset itself here in response to sample rate changes.
    /// If the sample rate stays unchanged, then the goal is to maintain current state.
    #[allow(unused_variables)]
    fn set_sample_rate(&mut self, sample_rate: crate::SampleRate) {
        let sample_rate: f64 = sample_rate.get();
        // The default implementation does nothing.
    }

    /// Process one sample.
    /// The length of `input` and `output` must be equal to `inputs` and `outputs`, respectively.
    fn tick(&mut self, input: &[S::Scalar], output: &mut [S::Scalar]);

    /// Process up to 64 (MAX_BUFFER_SIZE) samples.
    /// If `size` is zero then this is a no-op, which is permitted.
    fn process(&mut self, size: usize, input: &BufferRef<S>, output: &mut BufferMut<S>);

    /// Set a parameter. What formats are recognized depends on the component.
    #[allow(unused_variables)]
    fn set(&mut self, setting: Setting) {}

    /// Number of inputs to this unit.
    /// Equals size of the input argument in `tick` and `process`.
    /// This should be fixed after construction.
    fn inputs(&self) -> usize;

    /// Number of outputs from this unit.
    /// Equals size of the output argument in `tick` and `process`.
    /// This should be fixed after construction.
    fn outputs(&self) -> usize;

    /// Route constants, latencies and frequency responses at `frequency` Hz
    /// from inputs to outputs. Return output signal.
    fn route(&mut self, input: &SignalFrame, frequency: f64) -> SignalFrame;

    /// Return an ID code for this type of unit.
    fn get_id(&self) -> u64;

    /// Downcast this AudioUnit to `&dyn Any` for type inspection.
    /// This enables safe runtime type checking and downcasting.
    fn as_any(&self) -> &dyn core::any::Any;

    /// Downcast this AudioUnit to `&mut dyn Any` for type inspection and mutation.
    /// This enables safe runtime type checking and downcasting.
    fn as_any_mut(&mut self) -> &mut dyn core::any::Any;

    /// Set unit pseudorandom phase hash. Override this to use the hash.
    /// This is called from `ping` (only). It should not be called by users.
    #[allow(unused_variables)]
    fn set_hash(&mut self, hash: u64) {
        // The default implementation does nothing.
    }

    /// Ping contained `AudioUnit`s and `AudioNode`s to obtain
    /// a deterministic pseudorandom hash. The local hash includes children, too.
    /// Leaf nodes should not need to override this.
    /// If `probe` is true, then this is a probe for computing the network hash
    /// and `set_hash` should not be called yet.
    /// To set a custom hash for a graph, call this method with `ping`
    /// set to false and `hash` initialized with the custom hash.
    fn ping(&mut self, probe: bool, hash: AttoHash) -> AttoHash {
        if !probe {
            self.set_hash(hash.state());
        }
        hash.hash(self.get_id())
    }

    /// Memory footprint of this unit in bytes, without counting buffers and other allocations.
    ///
    /// Defaulted to the shallow size of the value: nothing in the engine reads
    /// this but a debug printout (`AudioUnitExt::display` in the fork) and a
    /// few tests, so a new node should not have to write it. The existing
    /// overrides stay until the Phase 4 port drops the method.
    fn footprint(&self) -> usize {
        core::mem::size_of_val(self)
    }

    /// Preallocate all needed memory, including buffers for block processing.
    fn allocate(&mut self) {
        // The default implementation does nothing.
    }

    /// Causal latency in (fractional) samples.
    /// After a reset, we can discard this many samples from the output to avoid incurring a pre-delay.
    /// The latency may depend on the sample rate.
    ///
    /// ### Example
    /// ```ignore
    /// use fundsp_tutti::prelude64::*;
    /// assert_eq!(pass().latency(), Some(0.0));
    /// assert_eq!(tick().latency(), Some(0.0));
    /// assert_eq!(sink().latency(), None);
    /// assert_eq!(limiter(0.01, 0.01).latency(), Some(441.0));
    /// ```
    fn latency(&mut self) -> Option<f64> {
        if self.outputs() == 0 {
            return None;
        }
        let mut input = SignalFrame::new(self.inputs());
        for i in 0..self.inputs() {
            input.set(i, Signal::Latency(0.0));
        }
        // The frequency argument can be anything as there are no responses to propagate,
        // only latencies. Latencies are never promoted to responses during signal routing.
        let response = self.route(&input, 1.0);
        // Return the minimum latency.
        let mut result: Option<f64> = None;
        for output in 0..self.outputs() {
            match (result, response.at(output)) {
                (None, Signal::Latency(x)) => result = Some(x),
                (Some(r), Signal::Latency(x)) => result = Some(r.min(x)),
                _ => (),
            }
        }
        result
    }

    /// How long this unit keeps producing after its input stops.
    ///
    /// Unlike [`latency`](Self::latency), this cannot be derived from
    /// [`route`](Self::route): a [`Signal`] carries a latency through a chain,
    /// and there is no equivalent carrier for a decay. A unit that has one
    /// therefore reports it here, and [`tutti_types::tail`] composes the graph's
    /// figure from what each node says.
    ///
    /// The default is [`Tail::Unknown`] — a unit that has not been taught to
    /// answer has said nothing, which is not the same as saying it has no tail.
    /// Overriding with [`Tail::None`] is how a unit states that it stops with
    /// its input.
    fn tail(&mut self) -> Tail {
        Tail::Unknown
    }
}

dyn_clone::clone_trait_object!(<S> AudioUnit<S> where S: Sample);
