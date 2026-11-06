//! The old read path against the new, on the same packets.
//!
//! `MidiEvent::message()` normalises (MIDI 1.0 promoted to 2.0), decodes with
//! `midi2` and builds a `MidiMessage`. `tutti_ump::decode` decodes and nothing
//! else, so it is not the same work: translation is a separate step in the
//! design. The figures are what a consumer pays to read a block today and what
//! it pays to read one after the move, translation excluded.

use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use tutti_midi_types::MidiEvent;
use tutti_ump::{CCNumber, MidiChannel, MidiGroup, Ump, decode};

fn block() -> Vec<MidiEvent> {
    let (g, c) = (MidiGroup::FIRST, MidiChannel::FIRST);
    (0..64u32)
        .map(|i| {
            let note = (36 + i % 48) as u8;
            match i % 4 {
                0 | 1 => MidiEvent::note_on(g, c, note, 0x8000 + i as u16),
                2 => MidiEvent::note_off(g, c, note, 0x4000),
                _ => MidiEvent::cc(g, c, CCNumber::new(7), 0x8000_0000 + i),
            }
        })
        .collect()
}

fn baseline(c: &mut Criterion) {
    let events = block();
    let packets: Vec<Ump> = events.iter().map(|e| Ump(e.data)).collect();
    let mut g = c.benchmark_group("read_block_of_64");
    g.throughput(Throughput::Elements(64));
    g.bench_function("MidiEvent::message", |b| {
        b.iter(|| {
            for e in black_box(&events) {
                black_box(e.message());
            }
        });
    });
    g.bench_function("tutti_ump::decode", |b| {
        b.iter(|| {
            for &p in black_box(&packets) {
                black_box(decode(p).ok());
            }
        });
    });
    g.finish();
}

criterion_group!(benches, baseline);
criterion_main!(benches);
