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
