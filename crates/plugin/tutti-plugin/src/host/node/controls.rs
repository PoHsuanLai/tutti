//! [`PluginControls`] — the part of a [`PluginClient`](super::PluginClient) a
//! host drives from outside the graph.
//!
//! # Why a separate type
//!
//! Everything a host does to a *running* plugin node — install a transport or
//! automation source, read the latency it reports — already went through state
//! shared across the node's clones: the input slots are `Arc<ArcSwapOption<…>>`
//! (see [`input_slot`](super::input_slot)), the latency and tail are shared
//! cells. That sharing is what made reaching the node through a graph downcast
//! sound, because an install on the frontend clone is seen by the clone the
//! audio thread runs.
//!
//! This type is those shared cells and nothing else, so a host can take it
//! **once, before the node goes into the graph**, and never need the node again.
//! A graph that owns its nodes outright (rather than cloning them on commit) has
//! no frontend clone to downcast to; this handle does not care which graph the
//! node is in.
//!
//! # The sample rate had to become shared to get here
//!
//! The installers stamp each source with the node's sample rate. That rate was a
//! plain per-clone field, which was fine while every caller held the clone a
//! graph had called `set_sample_rate` on. A handle taken before insertion is not
//! that clone: its copy would read the rate from load time forever, and a source
//! installed through it after a device change would be stamped with the old one.
//! So the rate lives in a shared cell like the others, and `set_sample_rate` on
//! any clone updates it for every holder.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use arc_swap::ArcSwap;
use atomic_float::AtomicF64;
use tutti_core::{SampleRate, Samples};
use tutti_plugin_types::PluginTail;

use super::input_slot::InputSlot;
use super::transport_source::TransportSource;
use super::{
    HarmonySource, NoteExpressionSource, ParamAutomationSource, PluginParamTarget, TimedChord,
    TimedParam, TimedScale,
};
use crate::protocol::Features;

/// The per-block inputs a plugin consumes, each an [`InputSlot`] sharing its
/// producer across clones. MIDI is deliberately not here — it has a
/// live-receiver fallback the uniform slot doesn't model (see
/// [`Midi`](super::Midi)).
#[derive(Clone)]
pub(super) struct PluginInputs {
    pub(super) harmony: InputSlot<HarmonySource>,
    pub(super) params: InputSlot<ParamAutomationSource>,
    pub(super) transport: InputSlot<TransportSource>,
    pub(super) note_expression: InputSlot<NoteExpressionSource>,
}

impl PluginInputs {
    /// Slots with the gates that decide which plugins receive each input:
    /// harmony → `SEQUENCER_CONTEXT`, transport → `TRANSPORT`, note-expression →
    /// `NOTE_EXPRESSION`, params → universal (empty gate = always send).
    fn new() -> Self {
        Self {
            harmony: InputSlot::new(Features::SEQUENCER_CONTEXT),
            params: InputSlot::new(Features::empty()),
            transport: InputSlot::new(Features::TRANSPORT),
            note_expression: InputSlot::new(Features::NOTE_EXPRESSION),
        }
    }
}

/// A plugin node's host-side controls: its input slots, its latency and tail
/// cells, and its sample rate — every one shared with the node it came from.
///
/// Take it with [`PluginClient::controls`](super::PluginClient::controls). A
/// clone is another handle on the same cells, never a copy of them, so an
/// install through any handle is seen by the node the audio thread runs,
/// lock-free and with no graph commit.
///
/// Holds no audio state and no subprocess lifetime: dropping every handle
/// leaves the plugin running, and a handle outliving its plugin installs into
/// slots nothing drains.
#[derive(Clone)]
pub struct PluginControls {
    pub(super) inputs: PluginInputs,
    /// The plugin's own reported latency, written by the bridge thread when the
    /// plugin signals a change. A `usize` because an atomic needs a primitive;
    /// [`latency`](Self::latency) puts the unit back.
    latency: Arc<AtomicUsize>,
    /// Runtime tail. An `ArcSwap` rather than an atomic because [`PluginTail`]
    /// is a four-arm sum whose payload is a `usize` — "unbounded" and "never
    /// asked" are not numbers, so there is no integer encoding to
    /// compare-and-swap that does not reintroduce the sentinel the type exists
    /// to avoid. Read once per block, never per sample.
    tail: Arc<ArcSwap<PluginTail>>,
    /// The rate stamped onto a freshly-installed source. Shared — see the module
    /// docs for why it could not stay a per-clone field. `f64` at the atomic.
    sample_rate: Arc<AtomicF64>,
}

impl PluginControls {
    pub(super) fn new(latency: Samples, tail: PluginTail, sample_rate: SampleRate) -> Self {
        Self {
            inputs: PluginInputs::new(),
            latency: Arc::new(AtomicUsize::new(latency.get())),
            tail: Arc::new(ArcSwap::from_pointee(tail)),
            sample_rate: Arc::new(AtomicF64::new(sample_rate.get())),
        }
    }

    /// The latency cell, for the bridge-thread listener that writes it.
    pub(super) fn latency_cell(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.latency)
    }

    /// The tail cell, for the bridge-thread listener that writes it.
    pub(super) fn tail_cell(&self) -> Arc<ArcSwap<PluginTail>> {
        Arc::clone(&self.tail)
    }

    /// The plugin's reported latency, in **frames** — its own figure, without
    /// the block the IPC pipeline adds (the node's `route` adds that).
    ///
    /// RT-safe: one atomic load, no allocation.
    pub fn latency(&self) -> Samples {
        Samples(self.latency.load(Ordering::Acquire))
    }

    /// Overwrite the reported latency. See
    /// [`PluginClient::set_latency`](super::PluginClient::set_latency).
    pub fn set_latency(&self, samples: impl Into<Samples>) {
        self.latency.store(samples.into().get(), Ordering::Release);
    }

    /// What the plugin currently reports for its tail.
    pub fn tail(&self) -> PluginTail {
        **self.tail.load()
    }

    /// Overwrite the reported tail. See
    /// [`PluginClient::set_tail`](super::PluginClient::set_tail).
    pub fn set_tail(&self, tail: PluginTail) {
        self.tail.store(Arc::new(tail));
    }

    /// The sample rate a source installed now would be stamped with.
    pub fn sample_rate(&self) -> SampleRate {
        SampleRate(self.sample_rate.load(Ordering::Acquire))
    }

    /// Update the shared rate and re-stamp every installed source. Called from
    /// the node's `AudioUnit::set_sample_rate`; a no-op on a slot with nothing
    /// installed (a later install reads the new rate).
    pub(super) fn restamp(&self, sample_rate: SampleRate) {
        self.sample_rate.store(sample_rate.get(), Ordering::Release);
        if let Some(src) = self.inputs.transport.source_ref().load().as_ref() {
            src.set_sample_rate(sample_rate);
        }
        if let Some(src) = self.inputs.harmony.source_ref().load().as_ref() {
            src.set_sample_rate(sample_rate);
        }
        if let Some(src) = self.inputs.params.source_ref().load().as_ref() {
            src.set_sample_rate(sample_rate);
        }
    }

    /// Install per-block chord/scale context. See
    /// [`PluginClient::set_harmony_source`](super::PluginClient::set_harmony_source).
    pub fn set_harmony_source(
        &self,
        chords: impl IntoIterator<Item = TimedChord>,
        scales: impl IntoIterator<Item = TimedScale>,
        transport: impl tutti_core::transport::Timeline + 'static,
    ) {
        self.inputs.harmony.install(Arc::new(HarmonySource::new(
            chords,
            scales,
            // Erased here, not by the caller: `Transport` implements `Timeline`
            // and is `Clone`, so an `Arc<dyn …>` at the boundary only asks a
            // host to spell out a wrapping this can do itself.
            Arc::new(transport),
            self.sample_rate(),
        )));
    }

    /// Drop the harmony source; subsequent blocks feed empty chord/scale context.
    pub fn clear_harmony_source(&self) {
        self.inputs.harmony.clear();
    }

    /// Install a note-expression source. See
    /// [`PluginClient::set_note_expression_source`](super::PluginClient::set_note_expression_source).
    pub fn set_note_expression_source(&self, source: Arc<NoteExpressionSource>) {
        self.inputs.note_expression.install(source);
    }

    /// Drop the note-expression source.
    pub fn clear_note_expression_source(&self) {
        self.inputs.note_expression.clear();
    }

    /// Install a transport reader, stamped with the current shared sample rate.
    /// See
    /// [`PluginClient::set_transport_source`](super::PluginClient::set_transport_source).
    pub fn set_transport_source(
        &self,
        reader: tutti_core::transport::Transport,
        meter: Arc<tutti_core::RtPublish<tutti_core::meter::MeterMap>>,
    ) {
        self.inputs.transport.install(Arc::new(TransportSource::new(
            Arc::new(reader),
            meter,
            self.sample_rate(),
        )));
    }

    /// Whether a transport reader is installed — what a host's binding checks
    /// after swapping the node under it.
    pub fn has_transport_source(&self) -> bool {
        self.inputs.transport.source_ref().load().is_some()
    }

    /// Whether a parameter-automation source is installed.
    pub fn has_param_automation_source(&self) -> bool {
        self.inputs.params.source_ref().load().is_some()
    }

    /// Drop the transport reader; subsequent blocks feed a stopped default.
    pub fn clear_transport_source(&self) {
        self.inputs.transport.clear();
    }

    /// Install sample-accurate per-block automation, one curve per param. See
    /// [`PluginClient::set_param_automation_source`](super::PluginClient::set_param_automation_source).
    pub fn set_param_automation_source(
        &self,
        params: impl IntoIterator<Item = TimedParam>,
        transport: impl tutti_core::transport::TransportState + 'static,
    ) {
        self.inputs
            .params
            .install(Arc::new(ParamAutomationSource::new(
                params,
                Arc::new(transport),
                self.sample_rate(),
            )));
    }

    /// Drop the automation source; the plugin keeps its current param values.
    pub fn clear_param_automation_source(&self) {
        self.inputs.params.clear();
    }

    /// Build an accumulator for one of the plugin's params. See
    /// [`PluginClient::param_target`](super::PluginClient::param_target) — it
    /// stores nothing, so the caller keeps the returned `Arc` for both roles.
    pub fn param_target(
        &self,
        _param_id: u32,
        base: f32,
        min: f32,
        max: f32,
    ) -> Arc<PluginParamTarget> {
        Arc::new(PluginParamTarget::new(base, min, max))
    }
}

impl std::fmt::Debug for PluginControls {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PluginControls")
            .field("latency", &self.latency())
            .field("tail", &self.tail())
            .field("sample_rate", &self.sample_rate())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::host::node::input_slot::BlockCtx;
    use crate::protocol::TransportInfo;
    use tutti_core::meter::MeterMap;
    use tutti_core::transport::Transport;

    const CTX: BlockCtx = BlockCtx { block_size: 64 };

    fn controls() -> PluginControls {
        PluginControls::new(Samples(0), PluginTail::default(), SampleRate(44_100.0))
    }

    /// A handle taken *before* a rate change stamps its installs with the new
    /// rate — the property that lets a host take the handle at load and keep it.
    ///
    /// `node` stands for the clone a graph calls `set_sample_rate` on; `held` for
    /// the host's copy, taken first.
    ///
    /// Mutation: storing the rate per clone (give `PluginControls` a hand-written
    /// `Clone` that copies `sample_rate` into a fresh `Arc`) leaves `held` at
    /// 44.1 kHz and fails the first assertion — the stale stamp the shared cell
    /// exists to prevent.
    #[test]
    fn a_handle_taken_before_a_rate_change_stamps_the_new_rate() {
        let node = controls();
        let held = node.clone();

        node.restamp(SampleRate(48_000.0));
        assert_eq!(held.sample_rate(), SampleRate(48_000.0));

        let meter = Arc::new(tutti_core::RtPublish::new(MeterMap::default()));
        held.set_transport_source(Transport::new(48_000.0), meter);

        // The node's slot drains what the held handle installed, stamped at the
        // rate the node was last given.
        let mut node = node;
        let out: TransportInfo = *node.inputs.transport.drain(CTX, Features::TRANSPORT);
        assert!(
            (out.sample_rate - 48_000.0).abs() < 1e-9,
            "the install through the held handle must reach the node's slot at the \
             current rate; got {}",
            out.sample_rate
        );
    }

    /// Latency and tail written through one handle are read through another.
    ///
    /// Mutation: minting a fresh `latency` cell in `Clone` makes the poll read
    /// the load-time figure forever — a latency change that never re-plans PDC.
    #[test]
    fn latency_and_tail_are_shared_between_handles() {
        let node = controls();
        let held = node.clone();
        node.set_latency(Samples(512));
        node.set_tail(PluginTail::Unbounded);
        assert_eq!(held.latency(), Samples(512));
        assert_eq!(held.tail(), PluginTail::Unbounded);
    }
}
