//! **A MIDI clip event lands on its frame** (doc 013 §6, "the frame is the
//! source of truth").
//!
//! At 90 BPM and 48 kHz frame 32 000 is exactly beat 1. A clock that
//! accumulated its beat read `1.0000000000000007` there after 500 chunks of
//! 64 frames, and the clip source placed events by comparing beats and
//! flooring an offset (`((beat - start) / bps) as u32`): the chunk before
//! frame 32 000 saw beat 1 as inside it (its `end_beat` rounded high) and
//! floored it onto its last frame, 31 999, a frame early. The clocks now
//! count frames and derive the beat in closed form, and the clip places by
//! frame (`BeatWindow::place`).
//!
//! A note at beat 1 lands on frame 32 000 exactly, and one at beat 3 on
//! frame 96 000, on every path a clip source is driven by: live through the
//! `Net` engine and the native graph engine (the source polled by a `Legacy`
//! node, chunk-major), offline `Net`-style (polled per 64 frames, then the
//! `OfflineTimeline` advanced) and through `OfflineTimeline::render_graph`.
//!
//! Each path also checks the beat the source read on those frames' chunks:
//! 1 and 3, to the bit.
//!
//! Mutations (run):
//! - accumulate in `FrameClock::advance` (restart the segment on every call
//!   at `beat + frames × beats_per_sample`) → the chunks of frames 32 000 and
//!   96 000 read `1.0000000000000007`-ish, not 1 → every path fails. (The
//!   landing frames survive that mutation alone: the placement rule's
//!   tolerance absorbs an error of 2e-11 frames. The drift grows with the
//!   session; the exact read is the property.)
//! - place by `(beat - start) / bps as u32` with beat bounds in `BeatWindow`
//!   (the old `offset_of`/`end_beat` rule) → an event an ulp before a block's
//!   first frame lands on the previous block's last frame
//!   (`an_event_an_ulp_before_its_frame_lands_on_it`).

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use tutti_core::dsp::Net;
use tutti_core::graph::{OutPort, Source};
use tutti_core::{
    AudioUnit, Beat, Bpm, BufferMut, BufferRef, BufferVec, ChannelLayout, Engine, InterleavedMut,
    MotionEvent, NodeKey, OfflineTimeline, OfflineTimelineConfig, SampleRate, Samples, Signal,
    SignalFrame, Timeline, Transport, TransportClock,
};
use tutti_graph::{Editor, Legacy, Prepare};
use tutti_midi_runtime::{MidiClipSource, TimedClipEvent};
use tutti_midi_types::ump::MidiEvent;
use tutti_midi_types::{MidiChannel, MidiGroup, MidiUnitId, MidiUnitIn};

const SR: f64 = 48_000.0;
const TEMPO: Bpm = Bpm(90.0);
const UNIT: MidiUnitId = MidiUnitId::new(1);
/// Rendered: past beat 3 (frame 96 000) by a few device blocks.
const FRAMES: usize = 98_304;

fn note(beat: f64) -> TimedClipEvent {
    TimedClipEvent {
        beat: Beat(beat),
        event: MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0xffff),
    }
}

/// A timeline that records every beat read from it: what the source saw.
struct Logged {
    inner: Arc<dyn Timeline>,
    reads: Mutex<Vec<Beat>>,
}

impl Timeline for Logged {
    fn beat(&self) -> Beat {
        let beat = self.inner.beat();
        self.reads.lock().expect("reads").push(beat);
        beat
    }
    fn tempo(&self) -> Bpm {
        self.inner.tempo()
    }
    fn is_rolling(&self) -> bool {
        self.inner.is_rolling()
    }
}

/// Polls a clip source on every call, and logs the absolute frame of each
/// event it emits: the frames this unit has been called for, plus the
/// event's offset in the call.
#[derive(Clone)]
struct ClipProbe {
    source: MidiClipSource,
    timeline: Arc<Logged>,
    frame: Arc<AtomicU64>,
    hits: Arc<Mutex<Vec<u64>>>,
}

impl ClipProbe {
    fn new(timeline: Arc<dyn Timeline>, events: &[TimedClipEvent]) -> Self {
        let timeline = Arc::new(Logged {
            inner: timeline,
            reads: Mutex::default(),
        });
        let source = MidiClipSource::new(
            UNIT,
            events.iter().copied(),
            Arc::clone(&timeline) as Arc<dyn Timeline>,
            SampleRate(SR),
        );
        Self {
            source,
            timeline,
            frame: Arc::default(),
            hits: Arc::default(),
        }
    }
    fn poll(&self, size: usize) {
        let mut buf = [MidiEvent::noop(); 8];
        let n = self.source.poll_unit(UNIT, size, &mut buf);
        let at = self.frame.fetch_add(size as u64, Ordering::Relaxed);
        let mut hits = self.hits.lock().expect("hits");
        for ev in &buf[..n] {
            hits.push(at + u64::from(ev.frame_offset));
        }
    }
}

impl AudioUnit for ClipProbe {
    fn inputs(&self) -> usize {
        0
    }
    fn outputs(&self) -> usize {
        1
    }
    fn tick(&mut self, _: &[f32], output: &mut [f32]) {
        self.poll(1);
        output[0] = 0.0;
    }
    fn process(&mut self, size: usize, _: &BufferRef, output: &mut BufferMut) {
        self.poll(size);
        for i in 0..size {
            output.set_f32(0, i, 0.0);
        }
    }
    fn route(&mut self, _: &SignalFrame, _: f64) -> SignalFrame {
        let mut out = SignalFrame::new(1);
        out.set(0, Signal::Latency(0.0));
        out
    }
    fn get_id(&self) -> u64 {
        tutti_core::mnemonic(b"TCLIPPRB")
    }
    fn as_any(&self) -> &dyn std::any::Any {
        self
    }
    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }
    fn footprint(&self) -> usize {
        std::mem::size_of::<Self>()
    }
}

fn probe(timeline: Arc<dyn Timeline>, events: &[TimedClipEvent]) -> ClipProbe {
    ClipProbe::new(timeline, events)
}

fn transport() -> Transport {
    let t = Transport::new(SR);
    t.settings.set_tempo(TEMPO);
    t
}

fn play(engine: &Engine, t: &Transport) {
    t.motion.try_send(MotionEvent::Play).expect("room");
    let mut buf = vec![0.0f32; 512];
    for _ in 0..FRAMES / 512 {
        engine.process(&mut InterleavedMut::new(&mut buf, ChannelLayout::MONO));
    }
}

/// What a path produced: each event's frame, and every beat the source read.
struct Run {
    hits: Vec<u64>,
    reads: Vec<Beat>,
}

fn hits(p: &ClipProbe) -> Run {
    Run {
        hits: p.hits.lock().expect("hits").clone(),
        reads: p.timeline.reads.lock().expect("reads").clone(),
    }
}

fn live_net(events: &[TimedClipEvent]) -> Run {
    let t = transport();
    let p = probe(Arc::new(t.clone()), events);
    let mut net = Net::new(0, 1);
    // The clock first, as bevy-tutti's build pushes it: the source reads the
    // beat of its chunk's first frame.
    net.push(Box::new(TransportClock::new(t.clock_links(), SR)));
    let id = net.push(Box::new(p.clone()));
    net.connect_output(id, 0, 0);
    net.set_sample_rate(SampleRate(SR));
    let backend = net.backend();
    // The backend is fed through the net; keep the frontend alive.
    Box::leak(Box::new(net));
    play(&Engine::new(t.motion.clone(), backend), &t);
    hits(&p)
}

fn graph_with(p: &ClipProbe) -> (Editor, tutti_graph::Executor) {
    let (mut ed, exec) = Editor::new(Prepare::new(SampleRate(SR), Samples(1024)));
    ed.insert(NodeKey(1), "clip", Legacy::new(p.clone()));
    ed.spec_mut().topology.outputs = vec![Source::Node(OutPort {
        node: NodeKey(1),
        port: 0,
    })];
    ed.commit().expect("commits");
    (ed, exec)
}

fn live_graph(events: &[TimedClipEvent]) -> Run {
    let t = transport();
    let p = probe(Arc::new(t.clone()), events);
    let (mut ed, exec) = graph_with(&p);
    let engine = Engine::with_graph(&t, &mut ed, exec).expect("within the limits");
    play(&engine, &t);
    hits(&p)
}

fn offline_timeline() -> Arc<OfflineTimeline> {
    Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
        start_beat: Beat(0.0),
        tempo: TEMPO,
        sample_rate: SampleRate(SR),
        loop_range: None,
    }))
}

fn offline_net(events: &[TimedClipEvent]) -> Run {
    let timeline = offline_timeline();
    let mut p = probe(Arc::clone(&timeline) as Arc<dyn Timeline>, events);
    let input = BufferVec::new(0);
    let mut output = BufferVec::new(1);
    for _ in 0..FRAMES / 64 {
        // A `Net` renders 64 frames a call; the export advances after each.
        p.process(64, &input.buffer_ref(), &mut output.buffer_mut());
        timeline.advance(64);
    }
    hits(&p)
}

fn offline_graph(events: &[TimedClipEvent]) -> Run {
    let timeline = offline_timeline();
    let p = probe(Arc::clone(&timeline) as Arc<dyn Timeline>, events);
    let (_ed, mut exec) = graph_with(&p);
    let mut out = vec![0.0f32; 1024];
    for _ in 0..FRAMES / 1024 {
        timeline.render_graph(&mut exec, 1024, &[], &mut [&mut out[..]]);
    }
    hits(&p)
}

/// Beat 1 on frame 32 000 and beat 3 on frame 96 000: exactly, once each;
/// and the source read those beats, to the bit, on their chunks.
fn assert_on_their_frames(what: &str, run: &Run) {
    assert_eq!(run.hits, [32_000, 96_000], "{what}");
    for beat in [1.0f64, 3.0] {
        let read = run
            .reads
            .iter()
            .find(|b| b.get() > beat - 1e-6)
            .expect("the playhead reached it");
        assert_eq!(
            read.get().to_bits(),
            beat.to_bits(),
            "{what}: the chunk on beat {beat}'s frame read {read:?}"
        );
    }
}

fn notes() -> [TimedClipEvent; 2] {
    [note(1.0), note(3.0)]
}

#[test]
fn a_clip_note_lands_on_its_frame_live_through_the_net() {
    assert_on_their_frames("live, Net", &live_net(&notes()));
}

#[test]
fn a_clip_note_lands_on_its_frame_live_through_the_graph() {
    assert_on_their_frames("live, graph", &live_graph(&notes()));
}

#[test]
fn a_clip_note_lands_on_its_frame_offline_through_the_net() {
    assert_on_their_frames("offline, Net", &offline_net(&notes()));
}

#[test]
fn a_clip_note_lands_on_its_frame_offline_through_the_graph() {
    assert_on_their_frames("offline, graph", &offline_graph(&notes()));
}

/// The placement rule, apart from the clock: an event an ulp before the
/// beat the clock reads on a block's first frame lands on that frame, not
/// on the previous block's last one; an event an ulp after it lands there
/// too; a frame later is the next frame.
///
/// Mutation: the old `BeatWindow` rule (membership `beat < end_beat`, offset
/// `((beat - start) / bps) as u32`) → the ulp-early note lands on 31 999 →
/// fails.
#[test]
fn an_event_an_ulp_before_its_frame_lands_on_it() {
    let ulp_before = Beat(f64::from_bits(1.0f64.to_bits() - 1));
    let ulp_after = Beat(f64::from_bits(1.0f64.to_bits() + 1));
    let events = [
        TimedClipEvent {
            beat: ulp_before,
            ..note(0.0)
        },
        TimedClipEvent {
            beat: ulp_after,
            ..note(0.0)
        },
        note(1.0 + 1.0 / 32_000.0),
    ];
    assert_eq!(offline_net(&events).hits, [32_000, 32_000, 32_001]);
}
