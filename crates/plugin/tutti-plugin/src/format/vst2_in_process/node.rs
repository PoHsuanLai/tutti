//! The in-process VST2 plugin as a graph node: MIDI
//! arrives on an event input, on its frame, and the plugin's MIDI-out leaves
//! on an event output (a plugin that declared `Features::MIDI_OUT`), each
//! event on the frame the plugin gave it.
//!
//! **Parameters** arrive on the same event input: a ramp (a
//! [`PluginAutomation`] node's) or a curve segment, addressed by VST2 index.
//! They go through the out-of-process node's delivery (`host::node`'s
//! `param_delivery` and `param_input`): a slot per parameter declared at
//! load, **changes only** against the value last delivered to the instance,
//! and the same segment points. The block is the chunk: before the plugin
//! processes it, each parameter that moved is set to its latest value in the
//! block with `setParameter`, under the instance lock, as the out-of-process
//! VST2 loader sets a chunk's. VST 2.4 has no sample-accurate parameter
//! event, so a point inside the block lands on its first frame; the two
//! hosts differ only in that the out-of-process one's chunk is the
//! pipeline's, this one's the graph's block. A block the audio thread finds
//! the instance locked for (silence, counted in
//! [`contention_count`](InProcessVst2Client::contention_count)) delivers
//! nothing, and its values are owed to the next block. Counted in
//! [`InProcessVst2Controls::param_delivery`].
//!
//! Forkable by state transfer (see its `IntoNode` below, and `fork.rs`). The
//! live node and its fork read the transport the same way: from the block's
//! `Cx`, the transport its input was rendered at (`TransportFeed`).

use std::sync::Arc;

use tutti_core::{ChannelLayout, MeterMap, RtPublish, Samples};
use tutti_graph::{
    Cx, Event, EventKind, IntoNode, Io, Node, NodeParts, Offset, Prepare, Shape, SortedEvents,
    Status, Ump,
};
use tutti_midi_types::ump::MidiEvent;
use tutti_types::Latency;

use super::client::{drive_f32, InProcessVst2Client};
use crate::host::node::{
    param_input, DeliveryCounters, ParamDeliveryStats, PluginAutomation, PluginMeter, TimedParam,
};
use crate::protocol::Features;

/// The most channels a block hands the plugin (`drive_f32`'s stack tables).
const MAX_CHANNELS: usize = 16;

impl Node for InProcessVst2Client {
    /// The plugin's channels, one event input (MIDI and parameter events),
    /// and a MIDI event output for a plugin that declared MIDI out; its
    /// latency and tail as loaded.
    fn shape(&self) -> Shape {
        let midi_out = u16::from(self.features.contains(Features::MIDI_OUT));
        let latency = Latency::new(self.metadata.latency_samples);
        Shape::audio(
            ChannelLayout::from_count(self.metadata.num_inputs.count()),
            ChannelLayout::from_count(self.metadata.num_outputs.count()),
        )
        .with_events(1, midi_out)
        .with_latency(latency)
        .with_tail(self.metadata.tail)
    }

    /// The rate (parked for the plugin, dispatched from the main thread; a
    /// fork's dispatched here), and scratch for the largest block. Control
    /// thread.
    ///
    /// The parameter delivery's point room follows the largest block, and
    /// the segments in flight are dropped (as a re-prepare starts the node
    /// over); what was delivered to the plugin is kept.
    fn prepare(&mut self, p: &Prepare) {
        self.set_rate(p.sample_rate());
        self.ensure_scratch_size(p.max_block().get());
        self.params.prepare(p.max_block().get());
        self.segments.prepare(self.params.room());
    }

    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let size = io.frames();
        let n_in = (self.metadata.num_inputs.count() as usize).min(MAX_CHANNELS);
        let n_out = (self.metadata.num_outputs.count() as usize).min(MAX_CHANNELS);
        // No clamp and no size check: `prepare` sized the scratch to the
        // prepared `MaxBlock`, and a block is never longer.
        // The event input: its MIDI, in its (sorted) order, past the cap
        // dropped (the list is inline and cannot grow); its parameter events
        // into the block's changes (`param_input`, the out-of-process node's
        // walk, with the block as the chunk).
        self.midi.clear();
        self.params.begin();
        let events = if io.event_input_count() > 0 {
            io.events(0)
        } else {
            SortedEvents::EMPTY
        };
        let midi = &mut self.midi;
        param_input::take(
            events.as_slice(),
            0,
            size,
            0,
            cx,
            true,
            &mut self.segments,
            &mut self.params,
            |e, _| match e.kind {
                EventKind::Midi(Ump(data)) => {
                    // Past the cap the event is dropped.
                    let _ = midi.try_push(MidiEvent {
                        frame_offset: e.offset.get(),
                        data,
                    });
                }
                // VST 2.4 has no chord or scale event; the walk keeps the
                // parameter events for itself.
                EventKind::Harmony(_) | EventKind::Ramp(_) | EventKind::Segment(_) => {}
            },
        );
        {
            let (inputs, _) = io.split();
            for ch in 0..n_in {
                self.process_scratch.f32_in[ch][..size].copy_from_slice(&inputs.get(ch)[..size]);
            }
        }
        // Built before the plugin is entered: `drive_f32` publishes it to the
        // cell `audioMasterGetTime` reads, then calls `process`.
        let transport = self.transport.block(cx, size, self.features);
        let sends = self.features.contains(Features::MIDI_OUT) && io.event_output_count() > 0;
        let mut out_events = crate::protocol::MidiEventVec::new();
        let processed = drive_f32(
            &self.inner,
            &self.contention_count,
            &self.params,
            &mut self.midi,
            &mut |events| {
                if sends {
                    // Past the cap the rest are dropped.
                    for e in events {
                        if out_events.try_push(*e).is_err() {
                            break;
                        }
                    }
                }
            },
            &transport,
            &mut self.scratch,
            &mut self.process_scratch,
            n_in,
            n_out,
            size,
            self.sample_rate,
        );
        // Set on the plugin with the block, or owed to the next one.
        self.params.handed_off(processed);
        let (_, mut outputs) = io.split();
        for (ch, out) in outputs.iter_mut().enumerate().take(n_out) {
            if processed {
                out[..size].copy_from_slice(&self.process_scratch.f32_out[ch][..size]);
            } else {
                out[..size].fill(0.0);
            }
        }
        if sends {
            sort_by_offset(&mut out_events);
            let writer = io.event_out(0);
            let block = Samples(size);
            for e in &out_events {
                let at = (e.frame_offset as usize).min(size - 1);
                if let Some(at) = Offset::new(at, block) {
                    let _ = writer.push(Event::midi(at, e.data));
                }
            }
        }
        Status::Modified
    }

    fn reset(&mut self) {
        // The segments in flight stop, as on the out-of-process node; what
        // was delivered to the plugin is kept (the plugin keeps its
        // parameters), and no block's changes are in flight between blocks.
        self.segments.clear();
        self.params.abandon();
        // Nothing reaches the plugin from here, and nothing can. VST 2.4 has no
        // opcode that clears DSP state on its own: the only two that touch it
        // are `effMainsChanged`, where plugins allocate and free their
        // rate-dependent buffers, and the `effStartProcess`/`effStopProcess`
        // pair, which announces an interruption rather than a clear and is only
        // legal while resumed. Neither may race the audio thread's `process`.
        // `Vst2Instance::reset_processing_state` is that cycle, on the main
        // thread, for a host that wants it on a locate or a loop wrap.
    }
}

/// Stable insertion sort by frame offset; allocation-free.
fn sort_by_offset(events: &mut crate::protocol::MidiEventVec) {
    for i in 1..events.len() {
        let mut j = i;
        while j > 0 && events[j - 1].frame_offset > events[j].frame_offset {
            events.swap(j - 1, j);
            j -= 1;
        }
    }
}

/// The plugin, inserted as a graph node. Its controls
/// ([`InProcessVst2Controls`]) are its meter cell (the same cell as
/// [`InProcessVst2Client::set_meter`]), so a host can give it the project
/// meter after it has moved into the graph, and its parameter delivery's
/// counters, as an out-of-process plugin's `PluginControls` are. Its
/// [`ForkSource`] forks it **by state transfer**: a second instance of the
/// plugin from the same library, restored from this one's saved state
/// ([`InProcessVst2Client::fork_instance`]). So `Editor::fork` (an offline
/// export) renders a graph holding it.
///
/// [`ForkSource`]: tutti_graph::ForkSource
impl IntoNode for InProcessVst2Client {
    type Controls = InProcessVst2Controls;

    fn into_parts(self) -> NodeParts<InProcessVst2Controls> {
        let fork = super::fork::Vst2Fork::of(&self);
        let controls = self.controls();
        NodeParts {
            node: Box::new(self),
            controls,
            fork: Some(Box::new(fork)),
        }
    }
}

/// An in-process VST2 node's controls: what inserting it hands back, and what
/// [`InProcessVst2Client::controls`] returns before then. Cheap to clone;
/// every clone is a handle on the same node (the node itself is not
/// `Clone`).
#[derive(Clone, Debug)]
pub struct InProcessVst2Controls {
    meter: PluginMeter,
    delivery: Arc<DeliveryCounters>,
}

impl InProcessVst2Controls {
    /// Gives the plugin the project meter (see
    /// [`InProcessVst2Client::set_meter`]).
    pub fn set_meter(&self, meter: Arc<RtPublish<MeterMap>>) {
        self.meter.set_meter(meter);
    }

    /// Drops the meter; the plugin is told 4/4 from bar 0.
    pub fn clear_meter(&self) {
        self.meter.clear_meter();
    }

    /// Whether a meter is installed.
    pub fn has_meter(&self) -> bool {
        self.meter.has_meter()
    }

    /// The node's meter cell, as a handle of its own.
    pub fn meter(&self) -> PluginMeter {
        self.meter.clone()
    }

    /// What the node has done with the parameter events on its event input
    /// since the plugin was loaded: points sent (changes only), blocks whose
    /// changes went a block late (the instance was locked), points dropped
    /// for an address it has no slot for, and points a full queue replaced.
    /// The same counters as an out-of-process node's
    /// [`PluginControls::param_delivery`](crate::handles::PluginControls::param_delivery),
    /// with the block as the chunk. A block's points all count as sent,
    /// though VST2 is told only each parameter's last one. RT-safe: four
    /// relaxed loads.
    pub fn param_delivery(&self) -> ParamDeliveryStats {
        self.delivery.snapshot()
    }

    /// A [`PluginAutomation`] node for this plugin: wire its event output to
    /// the node's event input. Addresses are VST2 indices
    /// ([`ParamAddress::Index`](crate::handles::ParamAddress::Index)); an
    /// opaque one is refused.
    pub fn automation(&self, params: impl IntoIterator<Item = TimedParam>) -> PluginAutomation {
        PluginAutomation::new(params, true)
    }
}

impl InProcessVst2Client {
    /// The node's controls: its meter cell and its parameter delivery's
    /// counters, the same [`InProcessVst2Controls`] inserting it hands back.
    pub fn controls(&self) -> InProcessVst2Controls {
        InProcessVst2Controls {
            meter: self.meter(),
            delivery: self.params.counters(),
        }
    }

    /// What the node has done with the parameter events on its event input
    /// (see [`InProcessVst2Controls::param_delivery`]).
    pub fn param_delivery(&self) -> ParamDeliveryStats {
        self.params.counters().snapshot()
    }

    /// A [`PluginAutomation`] node for this plugin (see
    /// [`InProcessVst2Controls::automation`]).
    pub fn automation(&self, params: impl IntoIterator<Item = TimedParam>) -> PluginAutomation {
        PluginAutomation::new(params, true)
    }
}

/// The owed path needs the instance lock held across a block, which only
/// code inside the crate can reach (`InProcessVst2Client::inner`); the rest
/// of the node's parameter delivery is pinned against the public API in
/// `tests/vst2_in_process_params.rs`.
#[cfg(test)]
mod owed_tests {
    use std::path::PathBuf;
    use std::sync::atomic::Ordering;
    use std::sync::OnceLock;

    use tutti_core::SampleRate;
    use tutti_graph::contract::Direct;
    use tutti_graph::{Event, Offset, ParamRamp};

    use crate::handles::{ParamAddress, PluginHandle};

    const BLOCK: usize = 64;

    /// The reference probe (`tutti-vst2-test-plugin`), as the integration
    /// tests resolve it (`tests/support/probe_path.rs`).
    fn probe() -> &'static PathBuf {
        static RESOLVED: OnceLock<PathBuf> = OnceLock::new();
        RESOLVED.get_or_init(|| {
            PathBuf::from(tutti_fixture_resolve::resolve_or_panic(
                env!("TUTTI_VST2_PROBE_CANDIDATES"),
                "tutti-vst2-test-plugin",
            ))
        })
    }

    fn read(handle: &PluginHandle, i: i32) -> Option<f32> {
        handle.params().parameter_value(ParamAddress::Index(i))
    }

    /// **A block the instance is locked for owes its values to the next.**
    /// The control thread holds the instance lock for one block that ramps
    /// parameter 1 to 0.75: the block renders silent (one contention), the
    /// plugin keeps its default (`1 / 16`, the probe's), nothing is counted
    /// sent and the block is counted deferred. The next block carries no
    /// event, and the owed value reaches the plugin on it.
    ///
    /// Mutation (run): `self.params.handed_off(true)` in `process` (the
    /// contended block counted as delivered) → nothing deferred, and the
    /// value is recorded as sent, so the empty block sends nothing and the
    /// plugin keeps `1 / 16` → fails.
    #[test]
    fn a_contended_block_owes_its_values_to_the_next() {
        let (client, handle) = super::super::load_client(probe(), SampleRate(48_000.0))
            .unwrap_or_else(|e| panic!("in-process load of the probe at {:?}: {e:?}", probe()));
        let inner = std::sync::Arc::clone(&client.inner);
        let contention = std::sync::Arc::clone(&client.contention_count);
        let controls = client.controls();
        let mut unit = Direct::new(client, SampleRate(48_000.0), BLOCK);
        assert_eq!(read(&handle, 1), Some(1.0 / 16.0), "the probe's default");

        {
            let _held = inner.lock();
            unit.events(
                0,
                &[Event::ramp(
                    Offset::new(0, tutti_core::Samples(BLOCK)).expect("inside the block"),
                    ParamRamp::foreign(1, 0.75, tutti_core::Samples(0)),
                )],
            );
            unit.block();
        }
        assert_eq!(
            contention.load(Ordering::Relaxed),
            1,
            "the block was refused"
        );
        assert_eq!(read(&handle, 1), Some(1.0 / 16.0), "not yet delivered");
        let stats = controls.param_delivery();
        assert_eq!(stats.chunks_deferred, 1);
        assert_eq!(stats.points_sent, 0);

        unit.block();
        assert_eq!(read(&handle, 1), Some(0.75), "owed to the next block");
        let stats = controls.param_delivery();
        assert_eq!(stats.points_sent, 1);
        assert_eq!(stats.chunks_deferred, 1);
        assert_eq!(stats.points_dropped, 0);
    }
}
