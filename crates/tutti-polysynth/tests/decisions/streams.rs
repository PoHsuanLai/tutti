//! Random message streams for `PolySynth`, and the driver that logs the voice
//! table after every message. The same file was compiled against the code
//! before the shared note logic (`tutti-voicing`), which captured
//! `old.txt`, and is compiled against the code after it, so a stream is the same
//! stream on both sides. Self-contained: tutti_ump, tutti_graph, tutti_polysynth.

use tutti_graph::contract::Direct;
use tutti_polysynth::{
    AllocationStrategy, FilterType, OscillatorType, PolySynth, SynthConfig, VoiceMode,
};
use tutti_types::{Adsr, Amplitude, SampleRate, Seconds};
use tutti_ump::{Address, CCNumber, Message, MidiChannel, MidiGroup, NoteNumber, U7, Ump, Voice1};

pub struct Rng(pub u64);
impl Rng {
    pub fn below(&mut self, n: u64) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0 % n
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Op {
    On(u8, u8),
    Off(u8, u8),
    Sustain(u8, bool),
    Sostenuto(u8, bool),
    NotesOff(u8),
    SoundOff(u8),
    Reset(u8),
}

fn pack(channel: u8, voice: Voice1) -> Ump {
    Ump::from(Message::Midi1(
        Address {
            group: MidiGroup::FIRST,
            channel: MidiChannel::new(channel).unwrap(),
        },
        voice,
    ))
}
fn cc(c: u8, n: u8, v: u8) -> Ump {
    pack(
        c,
        Voice1::ControlChange {
            controller: CCNumber::new(n).unwrap(),
            value: U7::new(v).unwrap(),
        },
    )
}
impl Op {
    pub fn ump(self) -> Ump {
        let key = |k| NoteNumber::new(k).unwrap();
        match self {
            Op::On(c, k) => pack(
                c,
                Voice1::NoteOn {
                    note: key(k),
                    velocity: U7::new(100).unwrap(),
                },
            ),
            Op::Off(c, k) => pack(
                c,
                Voice1::NoteOff {
                    note: key(k),
                    velocity: U7::new(0).unwrap(),
                },
            ),
            Op::Sustain(c, on) => cc(c, 64, if on { 127 } else { 0 }),
            Op::Sostenuto(c, on) => cc(c, 66, if on { 127 } else { 0 }),
            Op::NotesOff(c) => cc(c, 123, 0),
            Op::SoundOff(c) => cc(c, 120, 0),
            Op::Reset(c) => cc(c, 121, 0),
        }
    }
}

pub const STREAMS: usize = 300;
pub const MESSAGES: usize = 32;

/// Stream `i`: its messages and the synth it is played on. Eight voices; the
/// strategy rotates through all six, every seventh stream is mono and every
/// eleventh legato.
pub fn stream(i: usize) -> (Vec<Op>, SynthConfig) {
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ ((i as u64 + 1).wrapping_mul(0xD1B5_4A32_D192_ED03)));
    let dense = (i / 6).is_multiple_of(2);
    let ops = (0..MESSAGES)
        .map(|_| {
            let c = rng.below(2) as u8;
            let k = 60 + rng.below(13) as u8;
            // Half of the streams are dense: mostly strikes, so that eight
            // are held at once and the steal order decides something.
            let roll = rng.below(14);
            let roll = if dense && roll < 9 { 0 } else { roll };
            match roll {
                0..=4 => Op::On(c, k),
                5..=8 => Op::Off(c, k),
                9 => Op::Sustain(c, rng.below(2) == 0),
                10 => Op::Sostenuto(c, rng.below(2) == 0),
                11 => Op::NotesOff(c),
                12 => Op::SoundOff(c),
                _ => Op::Reset(c),
            }
        })
        .collect();
    let strategy = [
        AllocationStrategy::Oldest,
        AllocationStrategy::Newest,
        AllocationStrategy::Quietest,
        AllocationStrategy::HighestNote,
        AllocationStrategy::LowestNote,
        AllocationStrategy::NoSteal,
    ][i % 6];
    let voice_mode = match i {
        _ if i % 11 == 10 => VoiceMode::Legato,
        _ if i % 7 == 6 => VoiceMode::Mono,
        _ => VoiceMode::Poly,
    };
    let cfg = SynthConfig {
        sample_rate: SampleRate(48_000.0),
        max_voices: 8,
        voice_mode,
        oscillator: OscillatorType::Saw,
        filter: FilterType::None,
        envelope: Adsr {
            attack: Seconds(0.001),
            decay: Seconds(0.02),
            sustain: Amplitude(0.6),
            release: Seconds(0.05),
        },
        allocation_strategy: strategy,
        ..SynthConfig::default()
    };
    (ops, cfg)
}

pub type Table = Vec<(u32, u8, u8, u8)>;

/// FNV-1a of a table's text: what the fixture keeps of the old code's table.
pub fn fnv(text: &str) -> u32 {
    text.bytes().fold(0x811c_9dc5, |h, b| {
        (h ^ u32::from(b)).wrapping_mul(0x0100_0193)
    })
}

/// Plays stream `i` and returns, per message, `"<op> | seq:ch:key:phase ..."`.
pub fn play(i: usize, table: impl Fn(&PolySynth) -> Table) -> Vec<String> {
    let (ops, cfg) = stream(i);
    let mut d = Direct::new(PolySynth::new(cfg).unwrap(), SampleRate(48_000.0), 64);
    ops.iter()
        .map(|op| {
            d.node.apply_midi(op.ump());
            d.block();
            d.block();
            let t = table(&d.node)
                .iter()
                .map(|(s, c, k, p)| format!("{s}:{c}:{k}:{p}"))
                .collect::<Vec<_>>()
                .join(" ");
            format!("{op:?} | {t}")
        })
        .collect()
}
