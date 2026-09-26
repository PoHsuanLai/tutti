//! [`PluginControls`] — the part of a [`PluginClient`](super::PluginClient) a
//! host drives from outside the graph.
//!
//! # Why a separate type
//!
//! A plugin node is owned by the graph once inserted (doc 013: units exist
//! exactly once, and nothing downcasts to find one). Everything a host does to
//! it afterwards — install an automation or harmony source, give it the
//! project meter, read the latency it reports — goes through state shared with
//! the node: the input slots are `Arc<ArcSwapOption<…>>` (see
//! [`input_slot`](super::input_slot)), the meter, latency and tail are shared
//! cells. This type is those shared cells and nothing else, and it is what
//! inserting a bound plugin hands back
//! ([`IntoNode::Controls`](tutti_graph::IntoNode::Controls)), so the type
//! system gives the host its control surface at insert.
//!
//! # The transport is not here
//!
//! The node reads the transport from each block's `Env`
//! (`transport_source`), so there is nothing to install: only the meter,
//! which is a layer over the timeline and not transport state, is a control.
//!
//! # The sample rate is shared
//!
//! The installers stamp each source with the node's sample rate. A handle
//! taken before insertion is not the node: its copy of a per-node field would
//! read the rate from load time forever, and a source installed through it
//! after a device change would be stamped with the old one. So the rate lives
//! in a shared cell like the others, and the node's `prepare` updates it for
//! every holder.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use arc_swap::{ArcSwap, ArcSwapOption};
use atomic_float::AtomicF64;
use tutti_core::meter::MeterMap;
use tutti_core::{RtPublish, SampleRate, Samples};
use tutti_plugin_types::PluginTail;
use tutti_types::Latency;

use super::batcher::MAX_CHUNK;
use super::input_slot::InputSlot;
use super::{
    HarmonySource, NoteExpressionSource, ParamAutomationSource, PluginParamTarget, TimedChord,
    TimedParam, TimedScale,
};
use crate::protocol::Features;

/// The per-block inputs a plugin consumes, each an [`InputSlot`] sharing its
/// producer with the node. MIDI is deliberately not here — it has a
/// live-receiver fallback the uniform slot doesn't model (see
/// [`Midi`](super::Midi)).
///
/// **The seam for event ports.** Each of these is an out-of-band input: a
/// producer that polls its own timeline. Doc 013 turns them into event ports
/// on the node (parameter automation, harmony and note expression as graph
/// events, delay-compensated by the same pass as the audio), at which point
/// this struct and the slots go. Until then they are why the node declares
/// [`Shape::legacy`](tutti_graph::Shape::legacy).
#[derive(Clone)]
pub(super) struct PluginInputs {
    pub(super) harmony: InputSlot<HarmonySource>,
    pub(super) params: InputSlot<ParamAutomationSource>,
    pub(super) note_expression: InputSlot<NoteExpressionSource>,
}

impl PluginInputs {
    /// Slots with the gates that decide which plugins receive each input:
    /// harmony → `SEQUENCER_CONTEXT`, note-expression → `NOTE_EXPRESSION`,
    /// params → universal (empty gate = always send).
    fn new() -> Self {
        Self {
            harmony: InputSlot::new(Features::SEQUENCER_CONTEXT),
            params: InputSlot::new(Features::empty()),
            note_expression: InputSlot::new(Features::NOTE_EXPRESSION),
        }
    }
}

/// A plugin node's host-side controls: its input slots, its meter, latency
/// and tail cells, and its sample rate — every one shared with the node it
/// came from.
///
/// Handed back by inserting a bound plugin into a graph
/// ([`IntoNode`](tutti_graph::IntoNode)), or taken earlier with
/// [`PluginClient::controls`](super::PluginClient::controls). A clone is
/// another handle on the same cells, never a copy of them, so an install
/// through any handle is seen by the node the audio thread runs, lock-free
/// and with no graph commit.
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
    /// The frames the node's IPC pipeline holds: one chunk, settled by the
    /// node's `prepare` (`Batcher::prepare`). Shared so
    /// [`declared_latency`](Self::declared_latency) is right on any handle.
    pipeline: Arc<AtomicUsize>,
    /// The project meter, for the time-signature and bar fields of the
    /// transport the node sends. Empty until a host installs one; the node
    /// then reads 4/4 from bar 0. A nullable hot-swap slot, like the input
    /// slots: the node reads it once per block.
    pub(super) meter: Arc<ArcSwapOption<RtPublish<MeterMap>>>,
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
            pipeline: Arc::new(AtomicUsize::new(MAX_CHUNK)),
            meter: Arc::new(ArcSwapOption::empty()),
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
    /// the chunk the IPC pipeline adds (see
    /// [`declared_latency`](Self::declared_latency)).
    ///
    /// RT-safe: one atomic load, no allocation.
    pub fn latency(&self) -> Samples {
        Samples(self.latency.load(Ordering::Acquire))
    }

    /// The node's whole processing latency, as its `Shape` declares it to
    /// PDC: the plugin's own figure **plus** the chunk its out-of-process
    /// pipeline holds (audio is submitted now and collected a chunk later).
    ///
    /// What a host hands `Editor::set_latency` when the plugin's figure
    /// moves, so the next commit re-plans PDC around it: one number from one
    /// place, whether the editor asks the node at insert or the host asks
    /// this handle later.
    pub fn declared_latency(&self) -> Latency {
        Latency::new(self.latency() + self.pipeline())
    }

    /// The chunk the node's pipeline holds.
    pub(super) fn pipeline(&self) -> Samples {
        Samples(self.pipeline.load(Ordering::Acquire))
    }

    /// Record the chunk the node's `prepare` settled on.
    pub(super) fn set_pipeline(&self, frames: Samples) {
        self.pipeline.store(frames.get(), Ordering::Release);
    }

    /// Give the node the project meter, for the signature and bar it tells
    /// the plugin. Pass the cell the host publishes meter edits into, and an
    /// edit reaches the running plugin with nothing re-installed. Replaces any
    /// meter installed before.
    pub fn set_meter(&self, meter: Arc<RtPublish<MeterMap>>) {
        self.meter.store(Some(meter));
    }

    /// Drop the meter; the node tells the plugin 4/4 from bar 0.
    pub fn clear_meter(&self) {
        self.meter.store(None);
    }

    /// Whether a meter is installed — what a host's binding checks.
    pub fn has_meter(&self) -> bool {
        self.meter.load().is_some()
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
    /// the node's `prepare`; a no-op on a slot with nothing installed (a
    /// later install reads the new rate).
    pub(super) fn restamp(&self, sample_rate: SampleRate) {
        self.sample_rate.store(sample_rate.get(), Ordering::Release);
        if let Some(src) = self.inputs.harmony.source_ref().load().as_ref() {
            src.set_sample_rate(sample_rate);
        }
        if let Some(src) = self.inputs.params.source_ref().load().as_ref() {
            src.set_sample_rate(sample_rate);
        }
    }

    /// Give `fork`'s slots a copy of every per-block source installed here,
    /// each reading the transport `bind` names and stamped at `fork`'s rate,
    /// and the same meter.
    ///
    /// Control thread. A copy shares only what a source reads and never
    /// writes (curves, chord lists, the meter) with the live one: its cursors
    /// and rate cell are its own, so rendering the fork moves nothing the live
    /// node reads. A source `bind` has no transport for is left out, and the
    /// fork drains that slot empty — never a copy still reading the live
    /// playhead offline.
    ///
    /// The transport itself needs no rebinding: the fork reads it from its
    /// own graph's `Env`, which for an offline fork is the render's.
    pub(super) fn rebind_sources_into(&self, fork: &PluginControls, bind: &super::fork::Rebind) {
        let rate = fork.sample_rate();
        if let Some(meter) = self.meter.load_full() {
            fork.set_meter(meter);
        }
        if let Some(src) = self.inputs.params.source_ref().load_full() {
            let transport = bind.state(src.transport());
            fork.inputs
                .params
                .install(Arc::new(src.rebound(transport, rate)));
        }
        if let Some(src) = self.inputs.harmony.source_ref().load_full() {
            let timeline = bind.timeline(src.timeline());
            fork.inputs
                .harmony
                .install(Arc::new(src.rebound(timeline, rate)));
        }
        if let Some(src) = self.inputs.note_expression.source_ref().load_full() {
            let timeline = bind.timeline(src.timeline());
            fork.inputs
                .note_expression
                .install(Arc::new(NoteExpressionSource::new(timeline, rate)));
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

    /// Whether a parameter-automation source is installed.
    pub fn has_param_automation_source(&self) -> bool {
        self.inputs.params.source_ref().load().is_some()
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
            .field("pipeline", &self.pipeline())
            .field("meter", &self.has_meter())
            .field("tail", &self.tail())
            .field("sample_rate", &self.sample_rate())
            .finish_non_exhaustive()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_core::meter::MeterMap;
    use tutti_core::transport::Transport;

    fn controls() -> PluginControls {
        PluginControls::new(Samples(0), PluginTail::default(), SampleRate(44_100.0))
    }

    /// A handle taken *before* a rate change stamps its installs with the new
    /// rate — the property that lets a host take the handle at load and keep it.
    ///
    /// `node` stands for the node's own handle, which its `prepare` restamps;
    /// `held` for the host's copy, taken first.
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

        held.set_param_automation_source(Vec::<super::super::TimedParam>::new(), {
            Transport::new(48_000.0)
        });

        // The node's slot holds what the held handle installed, stamped at the
        // rate the node was last given.
        let installed = node
            .inputs
            .params
            .source_ref()
            .load_full()
            .expect("the install through the held handle reaches the node's slot");
        assert_eq!(installed.rate(), SampleRate(48_000.0));
    }

    /// A fork's per-block sources read **the transport its mode names**: the
    /// offline timeline for an offline fork (never the live playhead), and
    /// the live transport for a live one (`ForkMode::Offline` is typed, so
    /// there is no offline context without a timeline). Their rate is the
    /// fork's own, a later rate change on the live node does not reach them —
    /// and the fork has the live node's meter, the one control its transport
    /// snapshot reads besides its `Env`.
    ///
    /// Mutation: make `Rebind::state` return the live reader for `Offline` →
    /// the offline fork reads the live beat 2.0 → fails. Mutation: share the
    /// live source's rate cell in `ParamAutomationSource::rebound` → the live
    /// restamp reaches the fork → fails. Mutation: drop the meter copy from
    /// `rebind_sources_into` → fails.
    #[test]
    fn a_fork_reads_the_transport_its_mode_names() {
        use super::super::fork::Rebind;
        use tutti_core::transport::{OfflineTimeline, OfflineTimelineConfig, OfflineTransport};
        use tutti_core::Beat;

        let live = controls();
        let transport = Transport::new(44_100.0);
        transport
            .clock_links()
            .expect("the only playhead writer")
            .set_playhead(2.0);
        live.set_param_automation_source(Vec::<super::super::TimedParam>::new(), transport);
        let meter = Arc::new(RtPublish::new(MeterMap::default()));
        live.set_meter(Arc::clone(&meter));

        let offline: OfflineTransport =
            OfflineTransport::new(Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
                start_beat: Beat(8.0),
                ..Default::default()
            })));
        let beat_of = |bind: Rebind| {
            let fork = PluginControls::new(Samples(0), PluginTail::default(), SampleRate(96_000.0));
            live.rebind_sources_into(&fork, &bind);
            live.restamp(SampleRate(22_050.0));
            let shared_meter = fork
                .meter
                .load_full()
                .is_some_and(|m| Arc::ptr_eq(&m, &meter));
            assert!(shared_meter, "the fork reads the live node's meter");
            fork.inputs.params.source_ref().load_full().map(|src| {
                assert_eq!(src.rate(), SampleRate(96_000.0), "the fork's own rate");
                src.transport().beat()
            })
        };
        assert_eq!(beat_of(Rebind::Offline(offline.clone())), Some(Beat(8.0)));
        assert_eq!(beat_of(Rebind::Live), Some(Beat(2.0)));
    }

    /// Latency and tail written through one handle are read through another.
    ///
    /// Mutation: minting a fresh `latency` cell in `Clone` makes a host's
    /// handle read the load-time figure forever — a latency change that never
    /// re-plans PDC.
    #[test]
    fn latency_and_tail_are_shared_between_handles() {
        let node = controls();
        let held = node.clone();
        node.set_latency(Samples(512));
        node.set_tail(PluginTail::Unbounded);
        assert_eq!(held.latency(), Samples(512));
        assert_eq!(held.tail(), PluginTail::Unbounded);
    }

    /// The declared latency is the plugin's figure plus the pipeline's chunk,
    /// both read live off shared cells: a host holding a handle taken at load
    /// sees a latency change and a re-prepared chunk alike, which is what it
    /// hands `Editor::set_latency`.
    ///
    /// Mutation: `declared_latency` returning `Latency::new(self.latency())`
    /// (dropping the pipeline) → 512 ≠ 576 → fails. Mutation: a per-handle
    /// `pipeline` cell → the held handle keeps 64 after the node's re-prepare
    /// to 32 → fails.
    #[test]
    fn the_declared_latency_is_the_plugins_plus_the_pipelines() {
        let node = controls();
        let held = node.clone();
        assert_eq!(held.declared_latency(), Latency::new(Samples(MAX_CHUNK)));
        node.set_pipeline(Samples(64));
        node.set_latency(Samples(512));
        assert_eq!(held.declared_latency(), Latency::new(Samples(576)));
        node.set_pipeline(Samples(32));
        assert_eq!(held.declared_latency(), Latency::new(Samples(544)));
    }
}
