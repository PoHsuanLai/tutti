//! The generic plugin node over a Processor that renders in chunks
//! (`Processor::chunk`, design 021 §10.4, step 4b PR 2): where it cuts a
//! block, the transport each piece reads, `reset`, the loss reports in
//! `ProcessOutput`, harmony, and that the audio path stays allocation-free.
//!
//! The plugin is a recording format of this file's own (`Rec`): it copies its
//! input to its output and writes down what each `process` call was handed.
//! `Rec` is `InProcess` only to be loaded through `load_client`; the thread
//! that loads it passes its `Rig` in a thread-local.

use std::cell::RefCell;
use std::num::NonZeroUsize;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use assert_no_alloc::AllocDisabler;
use tutti_fixtures::main_thread;
use tutti_fixtures::midi;
use tutti_graph::contract::Direct;
use tutti_graph::{
    Env, Event, Harmony, Node, Offset, ParamRamp, PlayClock, Transport, TransportChanges,
};
use tutti_plugin_host::{InProcClient, NodeControls, PluginNode, load_client};
use tutti_plugin_types::{
    AudioBufferMut, BusRole, ChannelLayout, ChannelTopology, ContentScale, Control, Controller,
    EditorError, EditorSize, EditorWindow, Features, InProcess, Load, Loaded, LoadedPlugin,
    MainThread, Normalized, Origin, ParamId, ParameterInfo, PluginBus, PluginDescriptor,
    PluginError, PluginIo, PluginResult, PluginTail, Prepare, Processor, Refused, RestartRules,
    Samples, Speaker, Stuck, WindowHandle,
};
use tutti_tempo::{Shape, TempoMap, TempoPoint};
use tutti_transport::TempoReader;
use tutti_types::{Beat, Bpm, Frame, SampleRate};
use tutti_ump::{MidiChannel, MidiGroup};

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

const SR: SampleRate = SampleRate(48_000.0);
/// Quarter notes per frame at 120 BPM and 48 kHz.
const BEATS_PER_FRAME: f64 = 2.0 / 48_000.0;
const PARAM: u32 = 7;

/// What one `process` call was handed.
#[derive(Debug, Clone, PartialEq)]
struct Seen {
    frames: usize,
    quarters: f64,
    tempo: f64,
    /// `(frame, tempo)` of each transport change in the call.
    changes: Vec<(u32, f64)>,
    /// `(frame, playing, quarters)` of the same changes.
    moves: Vec<(u32, bool, f64)>,
    /// The transport's `tempo_inc`.
    inc: f64,
    midi: Vec<u32>,
    /// Chord offsets, `None` when no sequencer context was attached.
    chords: Option<Vec<i32>>,
}

/// The plugin's script and its record, shared with the test.
struct Rig {
    chunk: Option<NonZeroUsize>,
    features: Features,
    record: bool,
    seen: Mutex<Vec<Seen>>,
    resets: AtomicUsize,
    /// MIDI events reported lost when a chunk completes.
    lose: AtomicUsize,
    /// Report the completed chunk's parameter changes as unsent.
    unsent: AtomicBool,
    /// Declare a second input bus (one placed channel) beside the stereo main.
    key_bus: AtomicBool,
}

impl Rig {
    fn new(chunk: Option<usize>, features: Features) -> Arc<Self> {
        Arc::new(Self {
            chunk: chunk.and_then(NonZeroUsize::new),
            features,
            record: true,
            seen: Mutex::new(Vec::with_capacity(4096)),
            resets: AtomicUsize::new(0),
            lose: AtomicUsize::new(0),
            unsent: AtomicBool::new(false),
            key_bus: AtomicBool::new(false),
        })
    }

    fn frames(&self) -> Vec<usize> {
        self.seen.lock().unwrap().iter().map(|s| s.frames).collect()
    }
}

thread_local! {
    static ROUTE: RefCell<Option<Arc<Rig>>> = const { RefCell::new(None) };
}

struct Rec {
    rig: Arc<Rig>,
    descriptor: PluginDescriptor,
    origin: Origin,
}

struct RecProcessor {
    rig: Arc<Rig>,
    /// Frames into the chunk, as the plugin itself counts them.
    pos: usize,
}

struct RecController {
    rig: Arc<Rig>,
    descriptor: PluginDescriptor,
    origin: Origin,
}

macro_rules! rec_control {
    ($ty:ty) => {
        impl Control for $ty {
            fn info(&self) -> &PluginDescriptor {
                &self.descriptor
            }
            fn wiring(&self, _: &MainThread) -> LoadedPlugin {
                let stereo = ChannelTopology::new([Speaker::FrontLeft, Speaker::FrontRight]);
                let mut wiring = LoadedPlugin {
                    inputs: vec![
                        PluginBus::main(ChannelLayout::STEREO).placed(Some(stereo.clone())),
                    ],
                    outputs: vec![PluginBus::main(ChannelLayout::STEREO).placed(Some(stereo))],
                    features: self.rig.features,
                    ..LoadedPlugin::default()
                };
                if self.rig.key_bus.load(Ordering::Relaxed) {
                    wiring.inputs.push(
                        PluginBus::new(ChannelLayout::MONO, BusRole::Aux)
                            .placed(Some(ChannelTopology::new([Speaker::FrontCenter]))),
                    );
                }
                wiring
            }
            fn parameters(&self, _: &MainThread) -> Vec<ParameterInfo> {
                vec![ParameterInfo::new(ParamId::new(PARAM), "p")]
            }
            fn parameter(&self, _: &MainThread, _: ParamId) -> Option<Normalized> {
                None
            }
            fn set_parameter(&mut self, _: &MainThread, _: ParamId, _: Normalized) {}
            fn save_state(&mut self, _: &MainThread) -> PluginResult<Vec<u8>> {
                Ok(Vec::new())
            }
            fn load_state(&mut self, _: &MainThread, _: &[u8]) -> PluginResult<()> {
                Ok(())
            }
            fn dropped_reported_edits(&self) -> u64 {
                0
            }
        }
        impl EditorWindow for $ty {
            fn has_editor(&self, _: &MainThread) -> bool {
                false
            }
            fn open_editor(
                &mut self,
                _: &MainThread,
                _: WindowHandle,
                _: ContentScale,
            ) -> PluginResult<EditorSize> {
                Err(PluginError::Editor(EditorError::PluginError("none".into())))
            }
            fn close_editor(&mut self, _: &MainThread) {}
            fn editor_idle(&mut self, _: &MainThread) {}

            /// The default capabilities: this surface reports no resize support.
            fn editor_capabilities(
                &mut self,
                _main: &tutti_plugin_types::MainThread,
            ) -> tutti_plugin_types::EditorCapabilities {
                tutti_plugin_types::EditorCapabilities::default()
            }

            /// Not supported here.
            fn set_editor_size(
                &mut self,
                _main: &tutti_plugin_types::MainThread,
                _requested: tutti_plugin_types::EditorSize,
            ) -> tutti_plugin_types::PluginResult<tutti_plugin_types::EditorSize> {
                Err(tutti_plugin_types::PluginError::Editor(
                    tutti_plugin_types::EditorError::PluginError(
                        "set_editor_size not supported".into(),
                    ),
                ))
            }

            /// Nothing asked: this surface never queues a resize request.
            fn poll_editor_resize_request(
                &mut self,
                _main: &tutti_plugin_types::MainThread,
            ) -> Option<tutti_plugin_types::EditorSize> {
                None
            }
        }
    };
}
rec_control!(Rec);
rec_control!(RecController);

impl Loaded for Rec {
    type Processor = RecProcessor;
    type Controller = RecController;
    const RESTARTS: RestartRules = RestartRules::CLAP;

    fn activate(
        self,
        _: &MainThread,
        _: Prepare,
    ) -> Result<(RecProcessor, RecController), Refused<Self>> {
        Ok((
            RecProcessor {
                rig: self.rig.clone(),
                pos: 0,
            },
            RecController {
                rig: self.rig,
                descriptor: self.descriptor,
                origin: self.origin,
            },
        ))
    }

    fn deactivate(
        _: &MainThread,
        processor: RecProcessor,
        controller: RecController,
    ) -> Result<Self, Stuck<RecProcessor, RecController>> {
        let _ = processor;
        Ok(Rec {
            rig: controller.rig,
            descriptor: controller.descriptor,
            origin: controller.origin,
        })
    }
}

impl Controller for RecController {
    type Loaded = Rec;
    type Processor = RecProcessor;
    fn latency(&self, _: &MainThread) -> Samples {
        Samples(0)
    }
    fn tail(&self, _: &MainThread) -> PluginTail {
        PluginTail::None
    }
}

impl InProcess for Rec {
    fn load_in_process(_: &MainThread, path: &Path, load: &Load) -> PluginResult<Self> {
        let rig = ROUTE.with(|r| r.borrow_mut().take()).expect("a rig routed");
        Ok(Rec {
            rig,
            descriptor: PluginDescriptor {
                id: "rec".into(),
                name: "Rec".into(),
                ..PluginDescriptor::default()
            },
            origin: Origin::new(path, load.clone()),
        })
    }
    fn origin(&self) -> &Origin {
        &self.origin
    }
}

impl Processor for RecProcessor {
    fn process(&mut self, io: &mut PluginIo<'_>) -> PluginResult<()> {
        let AudioBufferMut::F32(buffer) = &mut io.buffer else {
            unreachable!("the graph is f32");
        };
        let frames = buffer.num_samples;
        for (out, input) in buffer.outputs.iter_mut().zip(buffer.inputs) {
            out[..frames].copy_from_slice(&input[..frames]);
        }
        if self.rig.record {
            let ctx = io.ctx;
            let transport = ctx.transport.expect("a transport is attached");
            self.rig.seen.lock().unwrap().push(Seen {
                frames,
                quarters: transport.position.quarters,
                tempo: transport.timing.tempo,
                changes: ctx
                    .transport_changes
                    .iter()
                    .map(|c| (c.offset, c.transport.timing.tempo))
                    .collect(),
                moves: ctx
                    .transport_changes
                    .iter()
                    .map(|c| {
                        (
                            c.offset,
                            c.transport.state.playing,
                            c.transport.position.quarters,
                        )
                    })
                    .collect(),
                inc: transport.timing.tempo_inc,
                midi: ctx.midi_events.iter().map(|(at, _)| at.get()).collect(),
                chords: ctx.expressive.as_ref().map(|e| {
                    e.chords
                        .map(|c| c.changes.iter().map(|v| v.sample_offset).collect())
                        .unwrap_or_default()
                }),
            });
        }
        if let Some(chunk) = self.rig.chunk {
            self.pos += frames;
            if self.pos >= chunk.get() {
                self.pos = 0;
                io.out
                    .count_midi_lost(self.rig.lose.load(Ordering::Relaxed));
                if self.rig.unsent.load(Ordering::Relaxed) {
                    io.out.set_params_unsent();
                }
            }
        }
        Ok(())
    }

    fn stop(&mut self) {}

    fn reset(&mut self) {
        self.pos = 0;
        self.rig.resets.fetch_add(1, Ordering::Relaxed);
    }

    fn chunk(&self) -> Option<NonZeroUsize> {
        self.rig.chunk
    }
}

/// A map at 120 BPM, with a step to 90 at `to_90` beats when given.
fn tempo(to_90: Option<f64>) -> TempoReader {
    let mut points = vec![TempoPoint {
        beat: Beat(0.0),
        tempo: Bpm::of(120.0),
        shape: Shape::Step,
    }];
    if let Some(beat) = to_90 {
        points.push(TempoPoint {
            beat: Beat(beat),
            tempo: Bpm::of(90.0),
            shape: Shape::Step,
        });
    }
    TempoReader::fixed(Arc::new(TempoMap::new(points).expect("a map")))
}

/// The node over `rig`, prepared for blocks of up to `block`.
fn node(rig: &Arc<Rig>, block: usize, map: TempoReader) -> (Direct<PluginNode<Rec>>, NodeControls) {
    ROUTE.with(|r| *r.borrow_mut() = Some(rig.clone()));
    let (client, _handle): (InProcClient<Rec>, _) =
        load_client::<Rec>(&main_thread(), Path::new("rec"), &Load::new(), SR)
            .expect("the recording plugin loads");
    let controls = client.controls();
    (Direct::new(client.bind(map), SR, block), controls)
}

/// The block of `len` frames starting at global frame `frame`, rolling at
/// 120 BPM from beat 4 at frame 0.
fn run_block(unit: &mut Direct<PluginNode<Rec>>, frame: u64, len: usize) {
    run_block_at(
        unit,
        frame,
        len,
        Bpm::of(120.0),
        Beat(4.0 + frame as f64 * BEATS_PER_FRAME),
    );
}

/// The block of `len` frames at global frame `frame`, with the transport's own
/// tempo and playhead given: a test with a tempo step needs a playhead that
/// follows the map, or the block after the step is a seek.
fn run_block_at(unit: &mut Direct<PluginNode<Rec>>, frame: u64, len: usize, bpm: Bpm, beat: Beat) {
    run_block_with(unit, frame, len, true, bpm, beat);
}

fn run_block_with(
    unit: &mut Direct<PluginNode<Rec>>,
    frame: u64,
    len: usize,
    playing: bool,
    bpm: Bpm,
    beat: Beat,
) {
    let env = Env {
        frame: Frame(frame),
        sample_rate: SR,
        block_len: Samples(len),
        transport: Transport::new(playing, bpm, beat, None),
        changes: TransportChanges::NONE,
        play: PlayClock::ZERO,
    };
    for (i, s) in unit.inputs_mut()[0].iter_mut().enumerate().take(len) {
        *s = (frame as usize + i) as f32;
    }
    unit.block_in(&env);
}

fn at(frame: usize, len: usize) -> Offset {
    Offset::new(frame, Samples(len)).expect("inside the block")
}

fn note_on(frame: usize, len: usize) -> Event {
    Event::midi(
        at(frame, len),
        midi::note_on(
            MidiGroup::FIRST,
            MidiChannel::of(1),
            60,
            midi::wide_velocity(100),
        ),
    )
}

/// **A block is cut at the Processor's chunk edges, counted from `prepare`**,
/// whatever the block's length: blocks of 100 over a chunk of 64 are 64+36,
/// then 28+64+8, then 56+44, then 20+64+16. The audio the pieces carry joins
/// back to the block bit for bit (the plugin copies its input), and **every
/// piece reads the transport at its chunk's first frame** (the contract of
/// `Processor::chunk`, amended in design 021 step 4b PR 3a: the transport is
/// chunk-level, like the parameters).
#[test]
fn a_block_is_cut_at_the_chunk_edges_and_each_piece_reads_its_chunks_transport() {
    let rig = Rig::new(Some(64), Features::TRANSPORT);
    let (mut unit, _) = node(&rig, 100, tempo(None));
    let mut heard = Vec::new();
    for k in 0..4 {
        run_block(&mut unit, k * 100, 100);
        heard.extend_from_slice(unit.output(0));
    }
    assert_eq!(
        rig.frames(),
        [64, 36, 28, 64, 8, 56, 44, 20, 64, 16],
        "cut at multiples of 64 from prepare"
    );
    let ramp: Vec<f32> = (0..400).map(|i| i as f32).collect();
    assert_eq!(heard, ramp, "the pieces join to the block, bit for bit");
    let mut start = 0usize;
    for seen in rig.seen.lock().unwrap().iter() {
        let chunk_start = start / 64 * 64;
        let want = 4.0 + chunk_start as f64 * BEATS_PER_FRAME;
        assert!(
            (seen.quarters - want).abs() < 1e-9,
            "a piece of {} frames from frame {start} read beat {}, not its chunk's {want}",
            seen.frames,
            seen.quarters
        );
        assert!(seen.changes.is_empty());
        start += seen.frames;
    }
}

/// **A tempo-map point is a transport change of its chunk, on the chunk's
/// frame, in the piece that holds it and the pieces after it.** The map steps
/// to 90 BPM at frame 80, in the chunk that starts at 64: the change is at
/// chunk frame 16, the piece that holds it (64..100) and the rest of that
/// chunk (100..128) are handed it (the chunk's changes so far), and the chunk's
/// own transport stays the 120 it started at; the chunks before and after have
/// no change and the tempo they stand at.
#[test]
fn a_transport_change_is_the_chunks_on_the_chunks_frame() {
    let rig = Rig::new(Some(64), Features::TRANSPORT);
    // Frame 80 is 80 / 24000 beats in; half a frame past it, so frame 80 is
    // the first at or after the point.
    let (mut unit, _) = node(&rig, 100, tempo(Some(4.0 + 79.5 * BEATS_PER_FRAME)));
    run_block(&mut unit, 0, 100);
    // The playhead follows the map: 90 BPM from the step, so the second block
    // continues the stretch (a playhead that did not is a seek, below).
    let step = 4.0 + 79.5 * BEATS_PER_FRAME;
    let beat = step + 20.5 * BEATS_PER_FRAME * 0.75;
    run_block_at(&mut unit, 100, 100, Bpm::of(90.0), Beat(beat));
    let seen = rig.seen.lock().unwrap();
    let pieces: Vec<_> = seen
        .iter()
        .map(|s| (s.frames, s.changes.clone(), s.tempo))
        .collect();
    assert_eq!(
        pieces,
        [
            (64, vec![], 120.0),
            (36, vec![(16, 90.0)], 120.0),
            (28, vec![(16, 90.0)], 120.0),
            (64, vec![], 90.0),
            (8, vec![], 90.0),
        ]
    );
}

/// **A Processor with no chunk is handed the whole block, as before**: one
/// call a block, no transport changes.
#[test]
fn a_processor_without_a_chunk_is_handed_whole_blocks() {
    let rig = Rig::new(None, Features::TRANSPORT);
    let (mut unit, _) = node(&rig, 100, tempo(Some(4.0 + 79.5 * BEATS_PER_FRAME)));
    run_block(&mut unit, 0, 100);
    run_block(&mut unit, 100, 100);
    assert_eq!(rig.frames(), [100, 100]);
    assert!(
        rig.seen
            .lock()
            .unwrap()
            .iter()
            .all(|s| s.changes.is_empty())
    );
}

/// **`reset` reaches the Processor and starts the chunk count over**: the
/// block after it is cut from its first frame again.
#[test]
fn reset_reaches_the_processor_and_restarts_the_chunks() {
    let rig = Rig::new(Some(64), Features::TRANSPORT);
    let (mut unit, _) = node(&rig, 100, tempo(None));
    run_block(&mut unit, 0, 100);
    assert_eq!(rig.frames(), [64, 36]);
    unit.node.reset();
    assert_eq!(
        rig.resets.load(Ordering::Relaxed),
        1,
        "the Processor was reset"
    );
    run_block(&mut unit, 100, 100);
    assert_eq!(rig.frames(), [64, 36, 64, 36], "counted from the reset");
}

/// **What a chunked Processor reports lost reaches the node's counters.** The
/// chunk completes in the first piece, which reports three MIDI events lost
/// and its parameter changes unsent: the node counts the three as dropped
/// MIDI and owes the ramp's value to the next chunk (a deferred chunk, nothing
/// sent). The next chunk to complete reports nothing: the owed value is sent.
#[test]
fn the_loss_a_chunked_processor_reports_is_counted() {
    let rig = Rig::new(Some(64), Features::TRANSPORT);
    let (mut unit, controls) = node(&rig, 100, tempo(None));
    rig.lose.store(3, Ordering::Relaxed);
    rig.unsent.store(true, Ordering::Relaxed);
    unit.events(
        0,
        &[Event::ramp(
            at(0, 100),
            ParamRamp::foreign(PARAM, 0.5, Samples(0)),
        )],
    );
    run_block(&mut unit, 0, 100);
    assert_eq!(controls.dropped_midi_in(), 3);
    let stats = controls.param_delivery();
    assert_eq!(stats.chunks_deferred, 1, "the chunk's values are owed");
    assert_eq!(stats.points_sent, 0);

    rig.lose.store(0, Ordering::Relaxed);
    rig.unsent.store(false, Ordering::Relaxed);
    run_block(&mut unit, 100, 100);
    assert_eq!(controls.dropped_midi_in(), 3, "nothing more was lost");
    let stats = controls.param_delivery();
    assert_eq!(
        stats.points_sent, 1,
        "the owed value went with the next chunk"
    );
    assert_eq!(stats.chunks_deferred, 1);
}

/// **A SysEx cut by the loss a chunked Processor reports stays cut.** The
/// chunk completes in the first block, which reports the three packets of a
/// dump (and no more) lost: the node counts them, and the dump's tail, taken
/// in the next block, is refused (counted) while a note after it is taken, so
/// a message never reaches the Processor with a gap. A loss with no SysEx open
/// cuts nothing.
#[test]
fn a_sysex_cut_by_a_reported_loss_stays_cut() {
    let payload: Vec<u8> = (0..30).collect();
    let dump: Vec<_> = tutti_ump::Sysex7Packets::checked(MidiGroup::FIRST, &payload)
        .expect("a 7-bit payload")
        .collect();
    assert_eq!(dump.len(), 5);
    let packet = |i: usize, frame: usize| Event::midi(at(frame, 64), dump[i]);
    for open in [true, false] {
        let rig = Rig::new(Some(64), Features::TRANSPORT);
        let (mut unit, controls) = node(&rig, 64, tempo(None));
        // The first chunk reports three lost; the dump is open in it only when
        // its first packets were taken there.
        rig.lose.store(3, Ordering::Relaxed);
        let first: Vec<Event> = if open {
            vec![packet(0, 0), packet(1, 1), packet(2, 2)]
        } else {
            vec![note_on(0, 64), note_on(1, 64), note_on(2, 64)]
        };
        unit.events(0, &first);
        run_block(&mut unit, 0, 64);
        assert_eq!(controls.dropped_midi_in(), 3);
        rig.lose.store(0, Ordering::Relaxed);
        unit.events(0, &[packet(3, 0), packet(4, 1), note_on(2, 64)]);
        run_block(&mut unit, 64, 64);
        let seen = rig.seen.lock().unwrap();
        let midi: Vec<_> = seen.iter().map(|s| s.midi.clone()).collect();
        if open {
            assert_eq!(midi, [vec![0, 1, 2], vec![2]], "the tail is cut");
            assert_eq!(controls.dropped_midi_in(), 3 + 2);
        } else {
            assert_eq!(midi, [vec![0, 1, 2], vec![0, 1, 2]], "nothing was open");
            assert_eq!(controls.dropped_midi_in(), 3);
        }
    }
}

/// **MIDI and chords reach the Processor on the frames of the piece they fall
/// in**, and a chord only for a plugin that declared sequencer context. A note
/// at frame 10 of the second block lands in its first piece (28 frames, the
/// end of a chunk begun in the block before) at 10; a note and a chord at
/// frame 70 of the first block land in its second piece at 6.
#[test]
fn midi_and_harmony_arrive_on_the_frames_of_their_piece() {
    let chord = Event::harmony(at(70, 100), Harmony::chord(0, 0, 0b1001_0001));
    for (declared, want) in [(true, Some(vec![6])), (false, None)] {
        let features = if declared {
            Features::TRANSPORT | Features::SEQUENCER_CONTEXT
        } else {
            Features::TRANSPORT
        };
        let rig = Rig::new(Some(64), features);
        let (mut unit, _) = node(&rig, 100, tempo(None));
        unit.events(0, &[note_on(70, 100), chord]);
        run_block(&mut unit, 0, 100);
        unit.events(0, &[note_on(10, 100)]);
        run_block(&mut unit, 100, 100);
        let seen = rig.seen.lock().unwrap();
        let midi: Vec<_> = seen.iter().map(|s| s.midi.clone()).collect();
        assert_eq!(midi, [vec![], vec![6], vec![10], vec![], vec![]]);
        let chords: Vec<_> = seen.iter().map(|s| s.chords.clone()).collect();
        let empty = declared.then(Vec::new);
        assert_eq!(
            chords,
            [empty.clone(), want, empty.clone(), empty.clone(), empty],
            "declared: {declared}"
        );
    }
}

/// **The chunked audio path allocates nothing** once warm: blocks of 100 over
/// a chunk of 64, with MIDI, a ramp and the transport walk, inside
/// `assert_no_alloc`.
#[test]
fn the_chunked_audio_path_does_not_allocate() {
    let mut rig = Rig::new(Some(64), Features::TRANSPORT);
    Arc::get_mut(&mut rig).expect("unshared").record = false;
    let (mut unit, _) = node(&rig, 100, tempo(Some(4.0 + 79.5 * BEATS_PER_FRAME)));
    let mut frame = 0;
    let mut one = |unit: &mut Direct<PluginNode<Rec>>| {
        unit.events(
            0,
            &[
                note_on(5, 100),
                Event::ramp(at(50, 100), ParamRamp::foreign(PARAM, 0.25, Samples(10))),
            ],
        );
        run_block(unit, frame, 100);
        frame += 100;
    };
    for _ in 0..32 {
        one(&mut unit);
    }
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..256 {
            one(&mut unit);
        }
    });
}

/// **What a chunked Processor reports lost reaches the graph's event losses**,
/// the ones the session turns into `SessionEvent::EventsDropped`
/// (`Editor::take_event_losses`, which `Session::report_event_losses` drains).
/// The Processor reports three MIDI events lost when its chunk completes; the
/// graph names three lost, charged to the plugin's node.
#[test]
fn a_chunked_processors_lost_midi_reaches_the_graphs_event_losses() {
    use tutti_graph::{Editor, NodeKey, Prepare};

    let rig = Rig::new(Some(64), Features::TRANSPORT);
    rig.lose.store(3, Ordering::Relaxed);
    ROUTE.with(|r| *r.borrow_mut() = Some(rig.clone()));
    let (client, _handle): (InProcClient<Rec>, _) =
        load_client::<Rec>(&main_thread(), Path::new("rec"), &Load::new(), SR)
            .expect("the recording plugin loads");
    let (mut editor, mut exec) = Editor::new(Prepare::new(SR, Samples(100)));
    let key = NodeKey(1);
    editor.insert(key, "rec", client.bind(tempo(None)));
    editor.commit().expect("commits");
    exec.apply_pending();
    editor.collect();

    exec.process(100, &Transport::default(), &[], &mut []);
    // One chunk completed in the block (64 of its 100 frames).
    assert_eq!(rig.frames(), [64, 36]);
    let lost: u32 = editor
        .take_event_losses()
        .filter(|l| l.node == key)
        .map(|l| l.count)
        .sum();
    assert_eq!(lost, 3);
}

/// **A stop and a seek that land exactly on a block boundary inside a chunk are
/// changes at that chunk frame**, as the served node's walk reported them: a
/// block of 100 and a chunk of 64, so the second block starts at chunk frame 36
/// of the chunk begun at 64. Stopped there, the piece that starts it is handed
/// a change at 36 (not playing) and so is every later piece of the chunk; moved
/// to beat 10 there, the change is at 36 and carries the new playhead. The
/// chunk's own transport is still the one it began with.
#[test]
fn a_stop_and_a_seek_on_a_block_boundary_are_changes_at_their_chunk_frame() {
    for (playing, beat, want) in [
        (false, 4.0 + 100.0 * BEATS_PER_FRAME, (36, false)),
        (true, 10.0, (36, true)),
    ] {
        let rig = Rig::new(Some(64), Features::TRANSPORT);
        let (mut unit, _) = node(&rig, 100, tempo(None));
        run_block(&mut unit, 0, 100);
        run_block_with(&mut unit, 100, 100, playing, Bpm::of(120.0), Beat(beat));
        let seen = rig.seen.lock().unwrap();
        let moves: Vec<_> = seen.iter().map(|s| s.moves.clone()).collect();
        let held = (want.0, want.1, beat);
        assert_eq!(moves[0], []);
        assert_eq!(moves[1], []);
        assert_eq!(moves[2].len(), 1, "playing {playing}: {:?}", moves[2]);
        assert_eq!((moves[2][0].0, moves[2][0].1), want);
        assert!((moves[2][0].2 - held.2).abs() < 1e-9, "{:?}", moves[2]);
        assert!(
            (seen[2].quarters - (4.0 + 64.0 * BEATS_PER_FRAME)).abs() < 1e-9,
            "the chunk's own transport is its first frame's"
        );
    }
}

/// **A tempo ramp across a block boundary inside a chunk gives the chunk's
/// `tempo_inc`, the ramp's own slope.** The map ramps linearly in time from
/// 120 BPM at beat 0 to 90 at beat 100: `D = 120 * 100 / (120 + 90)` seconds
/// long, so the slope is `-30 / D` BPM a second, `-30 / D / 48000` a frame. The
/// chunk 64..128 spans the boundary of the 100-frame blocks, and its transport
/// (final in the piece that completes it) carries that figure.
#[test]
fn a_tempo_ramp_across_a_block_boundary_gives_the_chunks_slope() {
    let rig = Rig::new(Some(64), Features::TRANSPORT);
    let ramp = TempoReader::fixed(Arc::new(
        TempoMap::new(vec![
            TempoPoint {
                beat: Beat(0.0),
                tempo: Bpm::of(120.0),
                shape: Shape::RampTime,
            },
            TempoPoint {
                beat: Beat(100.0),
                tempo: Bpm::of(90.0),
                shape: Shape::Step,
            },
        ])
        .expect("a map"),
    ));
    let (mut unit, _) = node(&rig, 100, ramp);
    // The playhead follows the ramp: beat(t) = (120 t - 0.525 t^2 / 2) / 60.
    let (duration, rate) = (120.0 * 100.0 / 210.0, 48_000.0);
    let k = -30.0 / duration;
    let at = |frame: u64| {
        let t = frame as f64 / rate;
        (
            Beat((120.0 * t + k * t * t / 2.0) / 60.0),
            Bpm::of(120.0 + k * t),
        )
    };
    for frame in [0u64, 100] {
        let (beat, bpm) = at(frame);
        run_block_at(&mut unit, frame, 100, bpm, beat);
    }
    let want = k / rate;
    let seen = rig.seen.lock().unwrap();
    // Pieces 64 | 36 28 | 64 8 ...: the chunk 64..128 is pieces 1 and 2, and
    // the piece that completes it holds the final figure.
    assert!((seen[2].inc - want).abs() < 1e-12, "{} {want}", seen[2].inc);
}

/// **The MIDI a chunk holds in flight when the node resets is lost, counted**:
/// a note in the second piece of a chunk that never completes. The reset drops
/// the chunk (the Processor clears its own), so the delivery's dropped-MIDI
/// count has it at once and the block after reports it to the executor
/// (`Io::count_lost`, what an export fails on).
#[test]
fn the_midi_in_flight_at_a_reset_is_lost_and_counted() {
    let rig = Rig::new(Some(64), Features::TRANSPORT);
    let (mut unit, controls) = node(&rig, 100, tempo(None));
    unit.events(0, &[note_on(70, 100)]);
    run_block(&mut unit, 0, 100);
    assert_eq!(unit.lost(), 0);
    assert_eq!(controls.dropped_midi_in(), 0, "held, not lost");
    unit.node.reset();
    assert_eq!(controls.dropped_midi_in(), 1, "dropped with the chunk");
    run_block(&mut unit, 100, 100);
    assert_eq!(
        unit.lost(),
        1,
        "reported to the executor with the next block"
    );
    run_block(&mut unit, 200, 100);
    assert_eq!(unit.lost(), 1, "once");
}

/// **A chunk's transport changes past the inline bound are dropped and
/// counted**, in the same counter the served node used
/// (`NodeControls::dropped_transport_changes`): twelve tempo steps inside one
/// chunk, eight kept (`MAX_TRANSPORT_CHANGES`), four counted.
#[test]
fn transport_changes_past_the_bound_are_dropped_and_counted() {
    let rig = Rig::new(Some(64), Features::TRANSPORT);
    let mut points = vec![TempoPoint {
        beat: Beat(0.0),
        tempo: Bpm::of(120.0),
        shape: Shape::Step,
    }];
    for i in 0..12u32 {
        points.push(TempoPoint {
            beat: Beat(4.0 + (4.5 + 4.0 * f64::from(i)) * BEATS_PER_FRAME),
            tempo: Bpm::of(100.0 - f64::from(i)),
            shape: Shape::Step,
        });
    }
    let map = TempoReader::fixed(Arc::new(TempoMap::new(points).expect("a map")));
    let (mut unit, controls) = node(&rig, 100, map);
    run_block(&mut unit, 0, 64);
    assert_eq!(controls.dropped_transport_changes(), 4);
    let seen = rig.seen.lock().unwrap();
    assert_eq!(seen[0].changes.len(), 8, "the bound");
}

/// The node's shape carries the plugin's buses with their placement: a
/// stereo main on each side and, when the plugin has one, a second input bus
/// after it, whose ports begin after the main's two.
#[test]
fn the_shape_declares_the_plugins_buses_with_their_placement() {
    let shape = |key_bus: bool| {
        let rig = Rig::new(None, Features::empty());
        rig.key_bus.store(key_bus, Ordering::Relaxed);
        ROUTE.with(|r| *r.borrow_mut() = Some(rig));
        let (client, _handle): (InProcClient<Rec>, _) =
            load_client::<Rec>(&main_thread(), Path::new("rec"), &Load::new(), SR)
                .expect("the recording plugin loads");
        client.bind(tempo(None)).shape()
    };
    let stereo = [Speaker::FrontLeft, Speaker::FrontRight];

    let plain = shape(false);
    assert_eq!(plain.buses_in().len(), 1);
    assert_eq!(
        plain.buses_in().get(0).unwrap().topology.positions(),
        &stereo
    );
    assert_eq!(
        plain.buses_out().get(0).unwrap().topology.positions(),
        &stereo
    );

    let keyed = shape(true);
    assert_eq!(keyed.audio_in(), ChannelLayout::from(3u16));
    assert_eq!(keyed.buses_in().len(), 2);
    assert_eq!(keyed.buses_in().first_port(1), Some(2));
    assert_eq!(
        keyed.buses_in().get(1).unwrap().topology.positions(),
        &[Speaker::FrontCenter]
    );
}
