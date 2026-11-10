//! The in-process VST2 plugin as a graph node: MIDI
//! arrives on an event input, on its frame, and the plugin's MIDI-out leaves
//! on an event output (a plugin that declared `Features::MIDI_OUT`), each
//! event on the frame the plugin gave it.
//!
//! **Parameters** arrive on the same event input: a ramp or a curve segment
//! (a foreign control lane's, `ControlLaneNode::for_foreign`), addressed by
//! VST2 index.
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

use arrayvec::ArrayVec;
use tutti_graph::{
    Cx, Event, EventKind, IntoNode, Io, Node, NodeParts, Offset, Prepare, Shape, SortedEvents,
    Status,
};
use tutti_types::Latency;
use tutti_types::{ChannelLayout, Samples};

use super::client::{InProcessVst2Client, drive_f32};
use crate::host::node::{DeliveryCounters, ParamDeliveryStats, PluginMeter, param_input};
use tutti_plugin_types::{Features, WireMidi};

/// The most channels a block hands the plugin (the stack tables `process`
/// builds from its `Io`). A wider plugin's channels past it read silence and
/// its graph outputs past it are silent.
const MAX_CHANNELS: usize = 16;

impl Node for InProcessVst2Client {
    /// The plugin's channels, one event input (MIDI and parameter events),
    /// and a MIDI event output for a plugin that declared MIDI out; its
    /// latency and tail as the control side last read them. The plugin reports its tail apart from
    /// its latency, so the tail excludes latency as [`Shape::tail`] asks.
    fn shape(&self) -> Shape {
        let midi_out = u16::from(self.features.contains(Features::MIDI_OUT));
        // The plugin's figures as the control side last re-read them
        // (`poll_changes` keeps them current; nothing is dispatched to the
        // plugin here), falling back to this node's copy while the control
        // thread holds the instance.
        let (latency, tail) = match self.inner.try_lock() {
            Some(instance) => {
                let (latency, tail) = instance.figures();
                (Latency::new(latency), tail)
            }
            None => (Latency::new(self.declared.latency), self.declared.tail),
        };
        Shape::audio(
            ChannelLayout::from_count(self.declared.inputs.count()),
            ChannelLayout::from_count(self.declared.outputs.count()),
        )
        .with_events(1, midi_out)
        .with_latency(latency)
        .with_tail(tail)
    }

    /// The rate (parked for the plugin, dispatched from the main thread; a
    /// fork's dispatched here). Control thread.
    ///
    /// The parameter delivery's point room follows the largest block, and
    /// the segments in flight are dropped (as a re-prepare starts the node
    /// over); what was delivered to the plugin is kept.
    fn prepare(&mut self, p: &Prepare) {
        self.set_rate(p.sample_rate());
        self.params.prepare(p.max_block().get());
        self.segments.prepare(self.params.room());
    }

    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let size = io.frames();
        if self.rate_refused {
            // The plugin was not told this node's rate (`set_rate`): silence,
            // not audio at another rate.
            return Status::Silent;
        }
        let n_in = (self.declared.inputs.count() as usize).min(MAX_CHANNELS);
        let n_out = (self.declared.outputs.count() as usize).min(MAX_CHANNELS);
        // No clamp and no size check: the Processor splits a block longer than
        // the one the plugin was told.
        // The event input: its MIDI, in its (sorted) order, past the cap
        // dropped and counted (the list is inline and cannot grow; a SysEx
        // that lost a packet loses the rest of it, `MidiIntake`); its
        // parameter events
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
        let intake = &mut self.intake;
        intake.new_chunk();
        let mut midi_dropped = 0u64;
        param_input::take(
            events.as_slice(),
            0,
            size,
            0,
            cx,
            &mut self.segments,
            &mut self.params,
            |e, _| match e.kind {
                EventKind::Midi(ump) => {
                    let event = WireMidi::at(e.offset, ump);
                    midi_dropped += intake.push_displacing(midi, event);
                }
                // VST 2.4 has no chord or scale event; the walk keeps the
                // parameter events for itself; a plugin re-sends nothing.
                EventKind::Harmony(_)
                | EventKind::Ramp(_)
                | EventKind::Segment(_)
                | EventKind::Resync => {}
            },
        );
        self.params.count_midi_dropped(midi_dropped);
        io.count_lost(midi_dropped);
        // Built before the plugin is entered: `drive_f32` publishes it to the
        // cell `audioMasterGetTime` reads, then calls `process`.
        let transport = self.transport.block(cx, size, self.features);
        let sends = self.features.contains(Features::MIDI_OUT) && io.event_output_count() > 0;
        let mut out_events = tutti_plugin_types::MidiEventVec::new();
        // The graph's buffers go to the plugin as they are: `vst2-host`
        // copies them into its render scratch and the plugin's output back
        // out, the one copy each way its pointer tables need. Scoped so the
        // tables release `io` before the event output is taken.
        let block_midi = self.midi.len();
        let processed = {
            let (inputs, mut outputs) = io.split();
            // A plugin wider than the tables is handed their first
            // `MAX_CHANNELS` channels; the graph's outputs past them would
            // keep whatever their buffers last held, so they are silenced.
            for out in outputs.iter_mut().skip(n_out) {
                out[..size].fill(0.0);
            }
            let ins: ArrayVec<&[f32], MAX_CHANNELS> =
                (0..n_in).map(|ch| &inputs.get(ch)[..size]).collect();
            let mut outs: ArrayVec<&mut [f32], MAX_CHANNELS> = outputs
                .iter_mut()
                .take(n_out)
                .map(|out| &mut out[..size])
                .collect();
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
                &ins,
                &mut outs,
                size,
                self.sample_rate,
            );
            if !processed {
                for out in &mut outs {
                    out.fill(0.0);
                }
            }
            processed
        };
        // Set on the plugin with the block, or owed to the next one.
        self.params.handed_off(processed);
        if !processed {
            // The block's MIDI was dropped with it (`drive_f32`): counted,
            // and the rest of every SysEx open in it is skipped.
            self.params.count_midi_dropped(block_midi as u64);
            io.count_lost(block_midi as u64);
            self.intake.lost();
        }
        if sends {
            tutti_plugin_types::insertion_sort_by_key(&mut out_events, |e| e.offset);
            let writer = io.event_out(0);
            let block = Samples(size);
            for e in &out_events {
                let at = (e.offset as usize).min(size - 1);
                if let Some(at) = Offset::new(at, block) {
                    let _ = writer.push(Event::midi(at, e.ump.0));
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
        // A SysEx open across the reset is not completed past it: its rest
        // is skipped (the instance's reassembler keeps its open message, so
        // it is dropped there, counted, at the next `START`).
        self.intake.lost();
        // Nothing reaches the plugin from here, and nothing can. VST 2.4 has no
        // opcode that clears DSP state on its own: the only two that touch it
        // are `effMainsChanged`, where plugins allocate and free their
        // rate-dependent buffers, and the `effStartProcess`/`effStopProcess`
        // pair, which announces an interruption rather than a clear and is only
        // legal while resumed. Neither may race the audio thread's `process`.
        // A rejoin and an activation is that cycle, on the main thread, for a
        // host that wants it on a locate or a loop wrap.
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
    pub fn set_meter(&self, meter: tutti_transport::ProjectMeter) {
        self.meter.set_meter(meter);
    }

    /// Drops the meter; the plugin is told 4/4 from bar 0.
    pub fn clear_meter(&self) {
        self.meter.clear_meter();
    }

    /// Whether a meter is installed.
    #[must_use]
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

    /// MIDI events on the node's event input that never reached the plugin
    /// since it was loaded. Counted: an event past the
    /// [`MIDI_STACK_CAPACITY`](tutti_plugin_types::MIDI_STACK_CAPACITY) a block
    /// carries; every event of a block the plugin did not run (the instance
    /// was locked; see `contention_count`); and the rest of every SysEx
    /// message that lost a packet either way or was open at a reset, so a
    /// message never reaches the plugin with a gap. The in-process
    /// counterpart of
    /// [`PluginControls::dropped_midi_in`](crate::handles::PluginControls::dropped_midi_in).
    /// RT-safe: two relaxed loads.
    pub fn dropped_midi_in(&self) -> u64 {
        self.delivery.dropped_midi_in()
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
}

/// The owed path needs the instance lock held across a block, which only
/// code inside the crate can reach (`InProcessVst2Client::inner`); the rest
/// of the node's parameter delivery is pinned against the public API in
/// `tests/vst2_in_process_params.rs`.
#[cfg(test)]
mod owed_tests {
    /// This process's main thread. Each test is its own process under nextest, so
    /// the claim cannot be refused.
    fn main_thread() -> tutti_plugin_types::MainThread {
        tutti_plugin_types::MainThread::claim().expect("one test, one process")
    }

    use std::path::PathBuf;
    use std::sync::OnceLock;
    use std::sync::atomic::Ordering;

    use tutti_graph::contract::Direct;
    use tutti_graph::{Event, Offset, ParamRamp};
    use tutti_types::SampleRate;

    use crate::handles::{ParamId, PluginHandle};

    const BLOCK: usize = 64;

    /// The reference probe (`tutti-vst2-test-plugin`), as the integration
    /// tests resolve it (`tests/support/probe_path.rs`).
    fn probe() -> &'static PathBuf {
        static RESOLVED: OnceLock<PathBuf> = OnceLock::new();
        RESOLVED.get_or_init(|| {
            tutti_fixtures::cdylib("tutti_vst2_test_plugin", "tutti-vst2-test-plugin")
        })
    }

    fn read(handle: &PluginHandle, i: i32) -> Option<f32> {
        handle
            .params()
            .parameter_value(ParamId::new(i as u32))
            .map(|v| v.get() as f32)
    }

    /// **A fork told its rate off the main thread is refused, not left at the
    /// wrong rate.** A fork's `prepare` dispatches the rate under the main
    /// thread's proof; off that thread it cannot, so the node records the
    /// refusal, renders silence, and a `prepare` on the main thread clears it.
    ///
    /// Mutation (run): `set_rate` not recording the refusal off the main
    /// thread -> `refused` is false -> fails. The silence `process` renders
    /// while refused is not covered here.
    #[test]
    fn a_fork_prepared_off_the_main_thread_is_refused() {
        let main = main_thread();
        let (client, _handle) = super::super::load_client(&main, probe(), SampleRate(48_000.0))
            .unwrap_or_else(|e| panic!("in-process load of the probe: {e:?}"));
        let fork = client
            .fork_instance(&main, tutti_graph::ForkMode::Live)
            .expect("the fork loads on the main thread");
        let (fork, refused) = std::thread::spawn(move || {
            let mut fork = fork;
            fork.set_rate(SampleRate(96_000.0));
            let refused = fork.rate_refused;
            (fork, refused)
        })
        .join()
        .expect("the thread returns");
        assert!(refused, "off the main thread the rate cannot be told");
        let mut fork = fork;
        fork.set_rate(SampleRate(96_000.0));
        assert!(!fork.rate_refused, "a prepare on the main thread clears it");
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
        let (client, handle) =
            super::super::load_client(&main_thread(), probe(), SampleRate(48_000.0))
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
                    Offset::new(0, tutti_types::Samples(BLOCK)).expect("inside the block"),
                    ParamRamp::foreign(1, 0.75, tutti_types::Samples(0)),
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

    /// A 60-byte SysEx's ten packets, then a 12-byte one's two, all on
    /// group 1, as graph events at frame 0.
    fn dumps() -> (Vec<Event>, Vec<Event>) {
        use tutti_ump::MidiGroup;
        let packets = |len: usize| {
            let payload: Vec<u8> = (0..len).map(|i| (i % 0x7F) as u8).collect();
            let mut out = Vec::new();
            tutti_midi_types::sysex7_fragments(MidiGroup::FIRST, &payload, &mut out);
            let at = Offset::new(0, tutti_types::Samples(BLOCK)).expect("inside the block");
            out.iter().map(|e| Event::midi(at, e.0)).collect::<Vec<_>>()
        };
        (packets(60), packets(12))
    }

    /// **A block the instance is locked for loses its MIDI, counted, and
    /// the SysEx open in it is not completed by its tail.** The dump's first
    /// three packets reach the plugin; the next three go with a contended
    /// block; the tail and a second dump follow. The plugin's host never
    /// completes the cut dump: it drops it at the second dump's `START`.
    ///
    /// Mutation (run): the `intake.lost()` in `process`'s contended branch
    /// removed → the tail completes the dump with three packets missing, so
    /// nothing is open at the next `START` and `dropped_sysex` stays 0 →
    /// fails. Mutation (run): the contended block's MIDI not counted → the
    /// count is short by three → fails.
    #[test]
    fn a_contended_block_cuts_the_sysex_open_in_it() {
        let (client, _handle) =
            super::super::load_client(&main_thread(), probe(), SampleRate(48_000.0))
                .unwrap_or_else(|e| panic!("in-process load of the probe at {:?}: {e:?}", probe()));
        let inner = std::sync::Arc::clone(&client.inner);
        let controls = client.controls();
        let mut unit = Direct::new(client, SampleRate(48_000.0), BLOCK);
        let (cut, next) = dumps();

        unit.events(0, &cut[..3]);
        unit.block();
        {
            let _held = inner.lock();
            unit.events(0, &cut[3..6]);
            unit.block();
        }
        let tail: Vec<Event> = cut[6..].iter().chain(&next).copied().collect();
        unit.events(0, &tail);
        unit.block();

        assert_eq!(inner.lock().dropped_sysex(), 1, "the cut dump, whole");
        assert_eq!(
            controls.dropped_midi_in(),
            (3 + cut.len() - 6) as u64,
            "the contended block's, and the cut dump's tail"
        );
    }

    /// **A SysEx open across the node's reset is not completed past it**:
    /// its tail is skipped, counted, and the plugin's host drops it at the
    /// next `START`, as the CLAP and VST3 hosts do on a plugin reset.
    ///
    /// Mutation (run): the `intake.lost()` in `reset` removed → the tail
    /// completes the dump across the reset → `dropped_sysex` stays 0 →
    /// fails.
    #[test]
    fn a_sysex_open_across_a_reset_is_not_completed() {
        use tutti_graph::Node as _;
        let (client, _handle) =
            super::super::load_client(&main_thread(), probe(), SampleRate(48_000.0))
                .unwrap_or_else(|e| panic!("in-process load of the probe at {:?}: {e:?}", probe()));
        let inner = std::sync::Arc::clone(&client.inner);
        let controls = client.controls();
        let mut unit = Direct::new(client, SampleRate(48_000.0), BLOCK);
        let (cut, next) = dumps();

        unit.events(0, &cut[..3]);
        unit.block();
        unit.node.reset();
        let tail: Vec<Event> = cut[3..].iter().chain(&next).copied().collect();
        unit.events(0, &tail);
        unit.block();

        assert_eq!(inner.lock().dropped_sysex(), 1, "the cut dump, whole");
        assert_eq!(controls.dropped_midi_in(), (cut.len() - 3) as u64);
    }
}
