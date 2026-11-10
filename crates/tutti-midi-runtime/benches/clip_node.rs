//! The clip node's cost per roll of three clips through a real executor, in
//! 512-frame blocks at 120 BPM and 48 kHz.
//!
//! - `dense`: 10,000 note pairs, an event every 1.5 frames (59 blocks).
//! - `chords`: a chord of 8 every 2,000 frames over 128 notes held throughout
//!   (782 blocks).
//! - `pad`: 1,500 notes held, 4 of them changing every block (400 blocks).
//!
//! A figure is the time of one whole roll; divide by the block count for a
//! block. The executor is built outside the timed region.

use criterion::{BatchSize, Criterion, criterion_group, criterion_main};
use tutti_fixtures::midi::{note_off, note_on};
use tutti_graph::{Editor, Executor, Prepare, Transport};
use tutti_midi_runtime::{MidiClipNode, TimedMidiEvent};
use tutti_types::{Beat, Bpm, NodeKey, SampleRate, Samples};
use tutti_ump::{MidiChannel, MidiGroup, Ump};

const FRAMES_PER_BEAT: f64 = 24_000.0;
const BLOCK: usize = 512;

fn at(frame: u64, e: Ump) -> TimedMidiEvent {
    TimedMidiEvent::new(Beat(frame as f64 / FRAMES_PER_BEAT), e)
}

fn key(i: u32) -> (MidiChannel, u8) {
    (MidiChannel::of(((i / 128) % 16) as u8), (i % 128) as u8)
}

fn on(i: u32) -> Ump {
    let (c, n) = key(i);
    note_on(MidiGroup::FIRST, c, n, 0xFFFF)
}

fn off(i: u32) -> Ump {
    let (c, n) = key(i);
    note_off(MidiGroup::FIRST, c, n, 0)
}

fn dense() -> Vec<TimedMidiEvent> {
    (0..10_000u32)
        .flat_map(|i| {
            let f = 3 * u64::from(i);
            [at(f, on(i % 2_048)), at(f + 2, off(i % 2_048))]
        })
        .collect()
}

fn chords() -> Vec<TimedMidiEvent> {
    (0..128u32)
        .map(|i| at(0, on(i)))
        .chain((0..200u64).flat_map(|k| {
            let f = 2_000 * k + 100;
            (0..8u32).flat_map(move |j| [at(f, on(200 + j)), at(f + 1_500, off(200 + j))])
        }))
        .collect()
}

fn pad() -> Vec<TimedMidiEvent> {
    (0..1_500u32)
        .map(|i| at(0, on(i)))
        .chain((0..400u32).flat_map(|k| {
            let f = 512 * u64::from(k) + 7;
            (0..4u32).flat_map(move |j| {
                let n = (k * 4 + j) % 1_500;
                [at(f, off(n)), at(f + 1, on(n))]
            })
        }))
        .collect()
}

fn rig(events: Vec<TimedMidiEvent>) -> Executor {
    let (mut editor, exec) = Editor::new(Prepare::new(SampleRate(48_000.0), Samples(BLOCK)));
    editor.insert(NodeKey(1), "clip", MidiClipNode::new(events));
    editor.commit().expect("commits");
    exec
}

fn roll(mut exec: Executor, frames: u64) -> Executor {
    for f in (0..frames).step_by(BLOCK) {
        let t = Transport::new(true, Bpm::of(120.0), Beat(f as f64 / FRAMES_PER_BEAT), None);
        exec.process(BLOCK, &t, &[], &mut []);
    }
    exec
}

fn clip_node(c: &mut Criterion) {
    let mut group = c.benchmark_group("clip_node");
    for (name, events, frames) in [
        ("dense", dense(), 30_100),
        ("chords", chords(), 400_000),
        ("pad", pad(), 205_000),
    ] {
        group.bench_function(name, |b| {
            b.iter_batched(
                || rig(events.clone()),
                |exec| roll(exec, frames),
                BatchSize::LargeInput,
            );
        });
    }
    group.finish();
}

criterion_group!(benches, clip_node);
criterion_main!(benches);
