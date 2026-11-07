//! What does `decode` and `From<Message> for Ump` cost per packet?
//!
//! The corpus is a block's worth of the traffic a synth sees: MIDI 2.0 notes,
//! controllers and bends, MIDI 1.0 notes and a few System Exclusive, Flex and
//! Stream packets. One iteration is the whole block, so the per-packet figure
//! is the printed time over the block length. The comparison with the old
//! `MidiEvent::message()` is in `tutti-midi-types`' `message_baseline` bench.

use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use tutti_ump::{Message, Ump, decode};

/// A deterministic block of 64 packets.
pub fn corpus() -> Vec<Ump> {
    let mut v = Vec::new();
    for i in 0..64u32 {
        let note = 36 + (i % 48);
        let w = match i % 8 {
            0..=2 => [0x4090_0000 | (note << 8), (0x8000 + i) << 16, 0, 0], // MIDI 2.0 note on
            3 => [0x4080_0000 | (note << 8), 0x4000_0000, 0, 0],            // MIDI 2.0 note off
            4 => [0x40B0_0700 | ((i % 4) << 16), 0x8000_0000 + i, 0, 0],    // MIDI 2.0 CC
            5 => [0x40E0_0000, 0x8000_0000 + (i << 12), 0, 0],              // MIDI 2.0 pitch bend
            6 => [0x2090_0000 | (note << 8) | 100, 0, 0, 0],                // MIDI 1.0 note on
            _ => match (i / 8) % 3 {
                0 => [0x3016_0102, 0x0304_0506, 0, 0], // SysEx7 start
                1 => [0xD610_0000, 0x02FA_F080, 0, 0], // Flex set tempo
                _ => [0xF003_5475, 0x7474_6900, 0, 0], // Stream endpoint name
            },
        };
        v.push(Ump(w));
    }
    v
}

/// 64 packets drawn from a seeded generator with varied notes, velocities,
/// channels and controllers across every family a stream carries, then
/// shuffled: no repeating pattern for the branch predictor to learn.
pub fn corpus_shuffled() -> Vec<Ump> {
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
        let (g, ch, note) = (
            (r & 3) as u32,
            ((r >> 2) & 15) as u32,
            ((r >> 6) & 127) as u32,
        );
        let (a, b) = (next() as u32, next() as u32);
        let w = match (r >> 16) % 20 {
            0..=5 => [0x4090_0000 | g << 24 | ch << 16 | note << 8, a, 0, 0],
            6..=8 => [0x4080_0000 | g << 24 | ch << 16 | note << 8, a, 0, 0],
            9..=10 => [0x40B0_0000 | g << 24 | ch << 16 | (a & 0x7F) << 8, b, 0, 0],
            11 => [0x40E0_0000 | g << 24 | ch << 16, b, 0, 0],
            12 => [0x40A0_0000 | g << 24 | ch << 16 | note << 8, b, 0, 0],
            13..=14 => [
                0x2090_0000 | g << 24 | ch << 16 | note << 8 | (a & 0x7F),
                0,
                0,
                0,
            ],
            15 => [
                0x3020_0000 | g << 24 | 6 << 16 | (a & 0x7F7F),
                b & 0x7F7F_7F7F,
                0,
                0,
            ],
            16 => [0xD610_0000, a, 0, 0],
            17 => [0xF003_0000 | (a & 0x7F7F), b, a, b],
            18 => [0x0010_0000 | (a & 0xFFFF), 0, 0, 0],
            _ => [0x1000_0000 | g << 24 | 0xF8 << 16, 0, 0, 0],
        };
        v.push(Ump(w));
    }
    // A seeded Fisher-Yates.
    for i in (1..v.len()).rev() {
        v.swap(i, (next() % (i as u64 + 1)) as usize);
    }
    v
}

fn codec(c: &mut Criterion) {
    let packets = corpus();
    let messages: Vec<Message> = packets
        .iter()
        .map(|&p| decode(p).expect("corpus decodes"))
        .collect();
    let mut g = c.benchmark_group("ump");
    g.throughput(Throughput::Elements(packets.len() as u64));
    g.bench_function("decode_block_of_64", |b| {
        b.iter(|| {
            for &p in black_box(&packets) {
                black_box(decode(p).ok());
            }
        });
    });
    g.bench_function("encode_block_of_64", |b| {
        b.iter(|| {
            for &m in black_box(&messages) {
                black_box(Ump::from(m));
            }
        });
    });
    let shuffled = corpus_shuffled();
    let shuffled_messages: Vec<Message> = shuffled.iter().filter_map(|&p| decode(p).ok()).collect();
    g.bench_function("decode_shuffled_block_of_64", |b| {
        b.iter(|| {
            for &p in black_box(&shuffled) {
                black_box(decode(p).ok());
            }
        });
    });
    g.bench_function("encode_shuffled_messages", |b| {
        b.iter(|| {
            for &m in black_box(&shuffled_messages) {
                black_box(Ump::from(m));
            }
        });
    });
    g.bench_function("is_note_off_block_of_64", |b| {
        b.iter(|| {
            for p in black_box(&packets) {
                black_box(p.is_note_off());
            }
        });
    });
    g.finish();
}

criterion_group!(benches, codec);
criterion_main!(benches);
