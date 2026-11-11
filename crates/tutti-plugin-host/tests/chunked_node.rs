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
    AudioBufferMut, ChannelLayout, ContentScale, Control, Controller, EditorError, EditorSize,
    EditorWindow, Features, InProcess, Load, Loaded, LoadedPlugin, MainThread, Normalized, Origin,
    ParamId, ParameterInfo, PluginDescriptor, PluginError, PluginIo, PluginResult, PluginTail,
    Prepare, Processor, Refused, RestartRules, Samples, Stuck, WindowHandle,
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
                LoadedPlugin {
                    inputs: vec![ChannelLayout::STEREO],
                    outputs: vec![ChannelLayout::STEREO],
                    features: self.rig.features,
                    ..LoadedPlugin::default()
                }
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
    let env = Env {
        frame: Frame(frame),
        sample_rate: SR,
        block_len: Samples(len),
        transport: Transport::new(
            true,
            Bpm::of(120.0),
            Beat(4.0 + frame as f64 * BEATS_PER_FRAME),
            None,
        ),
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
/// back to the block bit for bit (the plugin copies its input), and **each
/// piece reads the transport at its own first frame**.
///
/// Mutation (run): cutting at the block's start (`phase` always 0) → the second
/// block is 64+36 → fails. Mutation (run): the transport read at the block's
/// first frame (`Offset::new(0, ..)` for every piece, steady `at(0)`) → the
/// second piece's position is the first's → fails.
#[test]
fn a_block_is_cut_at_the_chunk_edges_and_each_piece_reads_its_transport() {
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
    let mut start = 0.0;
    for seen in rig.seen.lock().unwrap().iter() {
        let want = 4.0 + start * BEATS_PER_FRAME;
        assert!(
            (seen.quarters - want).abs() < 1e-9,
            "a piece of {} frames from frame {start} read beat {}, not {want}",
            seen.frames,
            seen.quarters
        );
        assert!(seen.changes.is_empty());
        start += seen.frames as f64;
    }
}

/// **A tempo-map point inside a piece is a transport change of that piece and
/// of no other.** The map steps to 90 BPM at frame 80 of the first block: the
/// second piece (frames 64..100) has it at its frame 16, with the tempo it
/// steps to, and its own first transport is still at 120; every piece before
/// and after has no change and the tempo it stands at.
///
/// Mutation (run): each piece's `scan` run to the block's end (`size - at`, not
/// `n`) → the first piece has the change too → fails.
#[test]
fn a_transport_change_lands_in_the_piece_that_holds_it() {
    let rig = Rig::new(Some(64), Features::TRANSPORT);
    // Frame 80 is 80 / 24000 beats in; half a frame past it, so frame 80 is
    // the first at or after the point.
    let (mut unit, _) = node(&rig, 100, tempo(Some(4.0 + 79.5 * BEATS_PER_FRAME)));
    run_block(&mut unit, 0, 100);
    run_block(&mut unit, 100, 100);
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
            (28, vec![], 90.0),
            (64, vec![], 90.0),
            (8, vec![], 90.0),
        ]
    );
}

/// **A Processor with no chunk is handed the whole block, as before**: one
/// call a block, no transport changes.
///
/// Mutation (run): `chunk` read as `Some(64)` for every Processor → the call
/// is cut → fails.
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
///
/// Mutation (run): the node's `reset` not calling `Processor::reset` → no
/// reset is counted → fails. Mutation (run): `phase` not zeroed in `reset` →
/// the block after it is 28+64+8 → fails.
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
///
/// Mutation (run): `params_unsent` not read (`handed_off(true)` at the
/// chunk's end) → no deferred chunk → fails. Mutation (run): `midi_lost` not
/// folded → no dropped MIDI → fails.
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

/// **MIDI and chords reach the Processor on the frames of the piece they fall
/// in**, and a chord only for a plugin that declared sequencer context. A note
/// at frame 10 of the second block lands in its first piece (28 frames, the
/// end of a chunk begun in the block before) at 10; a note and a chord at
/// frame 70 of the first block land in its second piece at 6.
///
/// Mutation (run): the chord not pushed in `push_harmony` → no chord arrives
/// → fails. Mutation (run): the MIDI offset left on the chunk's frames (not
/// `frame - phase`) → the note in the continued chunk arrives at 46 → fails.
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
///
/// Mutation (run): a `Vec` collected per piece in the node's loop → the gate
/// aborts → fails.
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
