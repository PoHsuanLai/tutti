//! The in-process plugin as a graph node, over any [`InProcess`] format: MIDI
//! arrives on an event input, on its frame, and the plugin's MIDI-out leaves
//! on an event output (a plugin that declared `Features::MIDI_OUT`), each
//! event on the frame the plugin gave it.
//!
//! **Parameters** arrive on the same event input: a ramp or a curve segment
//! (a foreign control lane's, `ControlLaneNode::for_foreign`), addressed by
//! the plugin's own parameter id.
//! They go through the out-of-process node's delivery (`host::node`'s
//! `param_delivery` and `param_input`): a slot per parameter declared at
//! load, **changes only** against the value last delivered to the instance,
//! and the same segment points. The block is the chunk: before the plugin
//! processes it, each parameter that moved is set to its latest value in the
//! block is handed to the Processor as [`ProcessContext::params`](tutti_plugin_types::ProcessContext::params), which
//! consumes it as its format does (VST2 sets each queue's last point, as the
//! out-of-process VST2 loader sets a chunk's). The two hosts differ only in
//! that the out-of-process one's chunk is the pipeline's, this one's the
//! graph's block. A block with no Processor in hand (silence, counted in
//! [`unlent_blocks`](InProcClient::unlent_blocks)) or one the Processor
//! failed ([`failed_blocks`](InProcClient::failed_blocks)) delivers nothing,
//! and its values are owed to the next block. Counted in
//! [`NodeControls::param_delivery`].
//!
//! Forkable by state transfer (see its `IntoNode` below, and `fork.rs`). The
//! live node and its fork read the transport the same way: from the block's
//! `Cx`, the transport its input was rendered at (`TransportFeed`).

use arrayvec::ArrayVec;
use tutti_graph::{
    Cx, Event, EventKind, IntoNode, Io, Node, NodeParts, Offset, Prepare, Shape, SortedEvents,
    Status,
};
use tutti_types::Latency;
use tutti_types::{ChannelLayout, Samples};

use super::client::{InProcClient, PluginNode, Rendered, render};
use crate::host::node::{NodeControls, param_input};
use tutti_plugin_types::{Features, InProcess, MAX_BLOCK_CHANNELS, WireMidi};

impl<F: InProcess + 'static> Node for PluginNode<F>
where
    F::Processor: 'static,
{
    /// The plugin's channels, one event input (MIDI and parameter events),
    /// and a MIDI event output for a plugin that declared MIDI out; its
    /// latency and tail as the control side last read them. The plugin reports its tail apart from
    /// its latency, so the tail excludes latency as [`Shape::tail`] asks.
    fn shape(&self) -> Shape {
        let this = &self.client;
        let midi_out = u16::from(this.features.contains(Features::MIDI_OUT));
        // The plugin's figures as the control side last re-read them
        // (`poll_changes` keeps them current; nothing is dispatched to the
        // plugin here), falling back to this node's copy while the control
        // thread holds the instance.
        let (latency, tail) = match this.inner.try_lock() {
            Some(instance) => {
                let (latency, tail) = instance.figures();
                (Latency::new(latency), tail)
            }
            None => (Latency::new(this.declared.latency), this.declared.tail),
        };
        Shape::audio(
            ChannelLayout::from_count(this.declared.inputs.count()),
            ChannelLayout::from_count(this.declared.outputs.count()),
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
        let this = &mut self.client;
        this.set_rate(p.sample_rate());
        this.params.prepare(p.max_block().get());
        this.segments.prepare(this.params.room());
    }

    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let Self {
            client: this,
            tempo,
            cursor,
        } = self;
        let size = io.frames();
        if this.rate_refused {
            // The plugin was not told this node's rate (`set_rate`): silence,
            // not audio at another rate.
            return Status::Silent;
        }
        let n_in = (this.declared.inputs.count() as usize).min(MAX_BLOCK_CHANNELS);
        let n_out = (this.declared.outputs.count() as usize).min(MAX_BLOCK_CHANNELS);
        // No clamp and no size check: the Processor splits a block longer than
        // the one the plugin was told.
        // The event input: its MIDI, in its (sorted) order, past the cap
        // dropped and counted (the list is inline and cannot grow; a SysEx
        // that lost a packet loses the rest of it, `MidiIntake`); its
        // parameter events
        // into the block's changes (`param_input`, the out-of-process node's
        // walk, with the block as the chunk).
        this.midi.clear();
        this.params.begin();
        let events = if io.event_input_count() > 0 {
            io.events(0)
        } else {
            SortedEvents::EMPTY
        };
        let midi = &mut this.midi;
        let intake = &mut this.intake;
        intake.new_chunk();
        let mut midi_dropped = 0u64;
        param_input::take(
            events.as_slice(),
            0,
            size,
            0,
            cx,
            &mut this.segments,
            &mut this.params,
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
        this.params.count_midi_dropped(midi_dropped);
        io.count_lost(midi_dropped);
        // Built before the plugin is entered: `drive_f32` publishes it to the
        // cell `audioMasterGetTime` reads, then calls `process`.
        let transport = {
            // One lend of the project's tempo map for the block, ended with it.
            let map = tempo.lend();
            this.transport.block(cx, size, this.features, &map, cursor)
        };
        let sends = this.features.contains(Features::MIDI_OUT) && io.event_output_count() > 0;
        // The graph's buffers go to the plugin as they are: the Processor
        // reads them through the channel tables below. Scoped so the tables
        // release `io` before the event output is taken.
        let block_midi = this.midi.len();
        let rendered = {
            let (inputs, mut outputs) = io.split();
            // A plugin wider than the tables is handed their first
            // `MAX_BLOCK_CHANNELS` channels; the graph's outputs past them would
            // keep whatever their buffers last held, so they are silenced.
            for out in outputs.iter_mut().skip(n_out) {
                out[..size].fill(0.0);
            }
            let ins: ArrayVec<&[f32], MAX_BLOCK_CHANNELS> =
                (0..n_in).map(|ch| &inputs.get(ch)[..size]).collect();
            let mut outs: ArrayVec<&mut [f32], MAX_BLOCK_CHANNELS> = outputs
                .iter_mut()
                .take(n_out)
                .map(|out| &mut out[..size])
                .collect();
            let rendered = render(
                &mut this.rt,
                (&this.unlent, &this.failed),
                &this.params,
                &mut this.midi,
                &mut this.out,
                &transport,
                &ins,
                &mut outs,
                size,
                this.sample_rate,
            );
            if rendered != Rendered::Ran {
                for out in &mut outs {
                    out.fill(0.0);
                }
            }
            rendered
        };
        let processed = rendered == Rendered::Ran;
        if processed {
            // The writes the plugin refused, counted dropped.
            this.params.refused(this.out.params_refused() as u64);
        }
        // Set on the plugin with the block, or owed to the next one.
        this.params.handed_off(processed);
        if !processed {
            // The block's MIDI was dropped with it (`render`): counted,
            // and the rest of every SysEx open in it is skipped.
            this.params.count_midi_dropped(block_midi as u64);
            io.count_lost(block_midi as u64);
            this.intake.lost();
        }
        if sends && processed {
            let out_events = &mut this.out.midi_events;
            tutti_plugin_types::insertion_sort_by_key(out_events, |e| e.offset);
            let writer = io.event_out(0);
            let block = Samples(size);
            for e in out_events.iter() {
                let at = (e.offset as usize).min(size - 1);
                if let Some(at) = Offset::new(at, block) {
                    let _ = writer.push(Event::midi(at, e.ump));
                }
            }
        }
        Status::Modified
    }

    fn reset(&mut self) {
        let this = &mut self.client;
        // The segments in flight stop, as on the out-of-process node; what
        // was delivered to the plugin is kept (the plugin keeps its
        // parameters), and no block's changes are in flight between blocks.
        this.segments.clear();
        this.params.abandon();
        // A SysEx open across the reset is not completed past it: its rest
        // is skipped (the instance's reassembler keeps its open message, so
        // it is dropped there, counted, at the next `START`).
        this.intake.lost();
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

/// The plugin, inserted as a graph node. Its controls ([`NodeControls`]) are
/// its meter cell (the same cell as [`NodeControls::set_meter`]), so a host can
/// give it the project meter after it has moved into the graph, and its
/// parameter delivery's counters, as an out-of-process plugin's
/// `PluginControls` are. Its [`ForkSource`] forks it **by state transfer**: a
/// second instance of the plugin from the same library, restored from this
/// one's saved state ([`PluginNode::fork_instance`]). So `Editor::fork` (an
/// offline export) renders a graph holding it.
///
/// [`ForkSource`]: tutti_graph::ForkSource
impl<F: InProcess + 'static> IntoNode for PluginNode<F>
where
    F::Processor: 'static,
{
    type Controls = NodeControls;

    fn into_parts(self) -> NodeParts<NodeControls> {
        let fork = super::fork::Fork::of(&self);
        let controls = self.client.controls();
        NodeParts {
            node: Box::new(self),
            controls,
            fork: Some(Box::new(fork)),
        }
    }
}

impl<F: InProcess> InProcClient<F> {
    /// The node's controls: its meter cell and its parameter delivery's
    /// counters, the same [`NodeControls`] inserting it hands back. Its block
    /// is the out-of-process node's chunk: a block's points all count as sent,
    /// though a format may take only each parameter's last one, and a block
    /// the plugin did not run (no Processor in hand, or one that failed it;
    /// see `unlent_blocks` and `failed_blocks`) loses its MIDI, counted.
    pub fn controls(&self) -> NodeControls {
        NodeControls::new(self.transport.meter(), self.params.counters())
    }
}

/// The owed path needs a block with no Processor in hand, which only code
/// inside the crate can bring about on cue (a rate change that recalls the
/// Processor, through `InProcClient::inner`); the rest of the
/// node's parameter delivery is pinned against the public API in
/// `tests/vst2_in_process_params.rs`.
#[cfg(all(test, feature = "vst2"))]
mod owed_tests {
    use tutti_fixtures::main_thread;
    use tutti_plugin_types::{Load, MainThread};
    use tutti_vst2_host::Vst2Loaded;

    use std::sync::atomic::Ordering;

    use tutti_graph::contract::Direct;
    use tutti_graph::{Event, Offset, ParamRamp};
    use tutti_types::SampleRate;

    use crate::format::in_process::InProcClient;
    use crate::handles::{ParamId, PluginHandle};

    /// The reference VST2 plugin through the generic loader.
    fn load_client(
        main: &MainThread,
        path: &std::path::Path,
        rate: SampleRate,
    ) -> tutti_plugin_ipc::Result<(InProcClient<Vst2Loaded>, PluginHandle)> {
        super::super::load_client::<Vst2Loaded>(main, path, &Load::new(), rate)
    }

    /// A reader of a constant 120 BPM map, for nodes these tests drive.
    fn tempo() -> tutti_transport::TempoReader {
        tutti_transport::TempoReader::constant(tutti_types::Bpm::of(120.0)).expect("a tempo")
    }

    const BLOCK: usize = 64;

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
        let (client, _handle) =
            load_client(&main, tutti_fixtures::vst2_probe(), SampleRate(48_000.0))
                .unwrap_or_else(|e| panic!("in-process load of the probe: {e:?}"));
        let fork = client
            .bind(tempo())
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

    /// Starts a rate change the way one parked for the main thread's pump
    /// does: the Processor is recalled, so the node's next block hands it
    /// back and renders without it.
    fn recall(inner: &parking_lot::Mutex<super::super::instance::Instance<Vst2Loaded>>) {
        inner
            .lock()
            .set_sample_rate(&main_thread(), 44_100.0, None)
            .expect("the change starts");
    }

    /// Finishes it: the pump collects the Processor and lends the new one.
    fn pump(inner: &parking_lot::Mutex<super::super::instance::Instance<Vst2Loaded>>) {
        let _ = inner.lock().pump(&main_thread());
    }

    /// **A block with no Processor in hand owes its values to the next.** A
    /// rate change recalls the Processor across one block that ramps
    /// parameter 1 to 0.75: the block renders silent (one unlent block), the
    /// plugin keeps its default (`1 / 16`, the probe's), nothing is counted
    /// sent and the block is counted deferred. Once the pump has lent the new
    /// Processor, the next block carries no event, and the owed value reaches
    /// the plugin on it.
    ///
    /// Mutation (run): `self.params.handed_off(true)` in `process` (the
    /// unlent block counted as delivered) → nothing deferred, and the
    /// value is recorded as sent, so the empty block sends nothing and the
    /// plugin keeps `1 / 16` → fails.
    #[test]
    fn an_unlent_block_owes_its_values_to_the_next() {
        let (client, handle) = load_client(
            &main_thread(),
            tutti_fixtures::vst2_probe(),
            SampleRate(48_000.0),
        )
        .unwrap_or_else(|e| {
            panic!(
                "in-process load of the probe at {:?}: {e:?}",
                tutti_fixtures::vst2_probe()
            )
        });
        let inner = std::sync::Arc::clone(&client.inner);
        let unlent = std::sync::Arc::clone(&client.unlent);
        let controls = client.controls();
        let mut unit = Direct::new(client.bind(tempo()), SampleRate(48_000.0), BLOCK);
        assert_eq!(read(&handle, 1), Some(1.0 / 16.0), "the probe's default");
        // The realtime end takes the Processor it was lent.
        unit.block();

        recall(&inner);
        unit.events(
            0,
            &[Event::ramp(
                Offset::new(0, tutti_types::Samples(BLOCK)).expect("inside the block"),
                ParamRamp::foreign(1, 0.75, tutti_types::Samples(0)),
            )],
        );
        unit.block();
        pump(&inner);
        assert_eq!(
            unlent.load(Ordering::Relaxed),
            1,
            "the block had no Processor"
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
            out.extend(
                tutti_ump::Sysex7Packets::checked(MidiGroup::FIRST, &payload)
                    .expect("a 7-bit payload"),
            );
            let at = Offset::new(0, tutti_types::Samples(BLOCK)).expect("inside the block");
            out.iter().map(|e| Event::midi(at, *e)).collect::<Vec<_>>()
        };
        (packets(60), packets(12))
    }

    /// **A block with no Processor in hand loses its MIDI, counted, and the
    /// SysEx open in it is not completed by its tail.** The dump's first three
    /// packets reach the plugin; the next three go with a block whose
    /// Processor was recalled; the tail and a second dump follow on the new
    /// Processor. The node skips the cut dump's tail; the plugin's host never
    /// completes the cut dump: it drops it at the second dump's `START`.
    ///
    /// Mutation (run): the `intake.lost()` in `process`'s unlent branch
    /// removed → the tail is delivered and counted nowhere → the count is
    /// short → fails. Mutation (run): the unlent block's MIDI not counted →
    /// the count is short by three → fails.
    #[test]
    fn an_unlent_block_cuts_the_sysex_open_in_it() {
        let (client, _handle) = load_client(
            &main_thread(),
            tutti_fixtures::vst2_probe(),
            SampleRate(48_000.0),
        )
        .unwrap_or_else(|e| {
            panic!(
                "in-process load of the probe at {:?}: {e:?}",
                tutti_fixtures::vst2_probe()
            )
        });
        let inner = std::sync::Arc::clone(&client.inner);
        let controls = client.controls();
        let mut unit = Direct::new(client.bind(tempo()), SampleRate(48_000.0), BLOCK);
        let (cut, next) = dumps();

        unit.events(0, &cut[..3]);
        unit.block();
        recall(&inner);
        unit.events(0, &cut[3..6]);
        unit.block();
        pump(&inner);
        let tail: Vec<Event> = cut[6..].iter().chain(&next).copied().collect();
        unit.events(0, &tail);
        unit.block();

        // The host keeps one SysEx reassembler across activations, so the
        // cut dump is still open on it, and its second `START` drops it.
        let fresh = unit.node.rt.held().expect("the new Processor");
        assert_eq!(fresh.dropped_sysex(), 1, "the cut dump, whole");
        assert_eq!(
            controls.dropped_midi_in(),
            (3 + cut.len() - 6) as u64,
            "the unlent block's, and the cut dump's tail"
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
        let (client, _handle) = load_client(
            &main_thread(),
            tutti_fixtures::vst2_probe(),
            SampleRate(48_000.0),
        )
        .unwrap_or_else(|e| {
            panic!(
                "in-process load of the probe at {:?}: {e:?}",
                tutti_fixtures::vst2_probe()
            )
        });
        let controls = client.controls();
        let mut unit = Direct::new(client.bind(tempo()), SampleRate(48_000.0), BLOCK);
        let (cut, next) = dumps();

        unit.events(0, &cut[..3]);
        unit.block();
        unit.node.reset();
        let tail: Vec<Event> = cut[3..].iter().chain(&next).copied().collect();
        unit.events(0, &tail);
        unit.block();

        let processor = unit.node.rt.held().expect("lent");
        assert_eq!(processor.dropped_sysex(), 1, "the cut dump, whole");
        assert_eq!(controls.dropped_midi_in(), (cut.len() - 3) as u64);
    }
}
