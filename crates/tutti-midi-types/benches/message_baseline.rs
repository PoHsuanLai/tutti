//! The old read path against the new, on the same packets.
//!
//! `MidiMessage::from_ump` (the old event `message()`) normalises (MIDI 1.0
//! promoted to 2.0), decodes with `midi2` and builds a `MidiMessage`. `tutti_ump::decode` decodes and nothing
//! else, so it is not the same work: translation is a separate step in the
//! design. The figures are what a consumer pays to read a block today and what
//! it pays to read one after the move, translation excluded.

use std::hint::black_box;
use tutti_fixtures::midi;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use tutti_midi_types::MidiMessage;
use tutti_ump::{CCNumber, MidiChannel, MidiGroup, Ump, decode};

fn block() -> Vec<Ump> {
    let (g, c) = (MidiGroup::FIRST, MidiChannel::FIRST);
    (0..64u32)
        .map(|i| {
            let note = (36 + i % 48) as u8;
            match i % 4 {
                0 | 1 => midi::note_on(g, c, note, 0x8000 + i as u16),
                2 => midi::note_off(g, c, note, 0x4000),
                _ => midi::cc(g, c, CCNumber::of(7), 0x8000_0000 + i),
            }
        })
        .collect()
}

/// 64 events across the channel voice constructors with seeded random group,
/// channel, note and value, shuffled: no repeating pattern to learn.
fn block_shuffled() -> Vec<Ump> {
    let mut state = 0x2545_F491_4F6C_DD1Du64;
    let mut next = move || {
        state ^= state << 13;
        state ^= state >> 7;
        state ^= state << 17;
        state
    };
    let mut v = Vec::new();
    for _ in 0..64 {
        let r = next();
        let (g, c) = (
            MidiGroup::of((r & 3) as u8),
            MidiChannel::of(((r >> 2) & 15) as u8),
        );
        let note = ((r >> 6) & 127) as u8;
        let (a, b) = (next() as u32, next() as u16);
        v.push(match (r >> 16) % 8 {
            0..=2 => midi::note_on(g, c, note, b),
            3 | 4 => midi::note_off(g, c, note, b),
            5 => midi::cc(g, c, CCNumber::masked(a as u8), a),
            6 => midi::pitch_bend(g, c, a),
            _ => midi::poly_pressure(g, c, note, a),
        });
    }
    for i in (1..v.len()).rev() {
        v.swap(i, (next() % (i as u64 + 1)) as usize);
    }
    v
}

fn baseline(c: &mut Criterion) {
    let packets = block();
    let mut g = c.benchmark_group("read_block_of_64");
    g.throughput(Throughput::Elements(64));
    g.bench_function("MidiMessage::from_ump", |b| {
        b.iter(|| {
            for &e in black_box(&packets) {
                black_box(MidiMessage::from_ump(e));
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
    let shuffled_packets = block_shuffled();
    g.bench_function("MidiMessage::from_ump_shuffled", |b| {
        b.iter(|| {
            for &e in black_box(&shuffled_packets) {
                black_box(MidiMessage::from_ump(e));
            }
        });
    });
    g.bench_function("tutti_ump::decode_shuffled", |b| {
        b.iter(|| {
            for &p in black_box(&shuffled_packets) {
                black_box(decode(p).ok());
            }
        });
    });
    g.finish();
}

criterion_group!(benches, baseline);
criterion_main!(benches);
