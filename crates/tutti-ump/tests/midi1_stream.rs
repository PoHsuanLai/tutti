//! `Midi1Parser` on streams: hostile bytes under `assert_no_alloc`, a long
//! SysEx in flight, and generated streams (running status, real-time bytes
//! anywhere, SysEx of every length) read back to the messages that made them.
//!
//! The expected messages of the generated streams come from how the stream was
//! built, not from a second parser.

use assert_no_alloc::{AllocDisabler, assert_no_alloc};
use tutti_ump::{
    Address, CCNumber, Message, Midi1Parser, MidiChannel, MidiGroup, NoteNumber, Sysex7Packet,
    SysexStatus, System, U7, U14, Voice1, parse_exact,
};

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

/// A small, fast generator: the same stream for the same seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn byte(&mut self, below: u64) -> u8 {
        u8::try_from(self.below(below)).unwrap()
    }
}

#[test]
fn the_state_is_small() {
    assert!(
        size_of::<Midi1Parser>() <= 32,
        "{}",
        size_of::<Midi1Parser>()
    );
}

/// Every stream of three bytes (16.7 million), each from a fresh parser: no
/// panic and no allocation.
#[test]
fn every_three_byte_stream_is_survived() {
    let mut messages = 0u64;
    assert_no_alloc(|| {
        for a in 0..=255u8 {
            for b in 0..=255u8 {
                for c in 0..=255u8 {
                    let mut parser = Midi1Parser::new(MidiGroup::FIRST);
                    messages += parser.feed_all(&[a, b, c]).count() as u64;
                }
            }
        }
    });
    assert!(messages > 0);
}

/// 16 MiB of bytes shaped like MIDI (statuses and data in useful proportions)
/// through one parser, in blocks the way a transport delivers them, then the
/// same through `message` on every three-byte window.
#[test]
fn seeded_random_bytes_are_survived() {
    let mut rng = Rng(0x1234_5678_9ABC_DEF1);
    let mut parser = Midi1Parser::new(MidiGroup::FIRST);
    let mut block = vec![0u8; 1 << 20];
    let mut messages = 0u64;
    for _ in 0..16 {
        for slot in &mut block {
            *slot = shaped_byte(&mut rng);
        }
        assert_no_alloc(|| {
            messages += parser.feed_all(&block).count() as u64;
            parser.end_of_message();
            messages += block
                .windows(3)
                .filter(|w| parse_exact(MidiGroup::FIRST, w).is_some())
                .count() as u64;
        });
    }
    assert!(messages > 0);
}

/// A byte with MIDI's shape: statuses, `0xF0`, `0xF7`, real-time and data.
fn shaped_byte(rng: &mut Rng) -> u8 {
    match rng.below(10) {
        0 => 0xF0,
        1 => 0xF7,
        2 => 0xF8 + rng.byte(8),
        3 => 0xF1 + rng.byte(6),
        4 | 5 => 0x80 + rng.byte(0x70),
        _ => rng.byte(128),
    }
}

/// The stray status and the truncated message that aborted an earlier
/// parser under `assert_no_alloc`.
#[test]
fn a_stray_status_and_a_truncated_message_allocate_nothing() {
    assert_no_alloc(|| {
        let mut parser = Midi1Parser::new(MidiGroup::FIRST);
        assert_eq!(parser.feed_all(&[0xF7, 1, 2]).count(), 0);
        assert_eq!(parser.feed_all(&[0x90, 60]).count(), 0);
        assert!(parse_exact(MidiGroup::FIRST, &[0xF7, 1, 2]).is_none());
        assert!(parse_exact(MidiGroup::FIRST, &[0x90, 60]).is_none());
    });
}

/// A SysEx with no `0xF7` for a megabyte holds no more state, allocates
/// nothing, streams its packets out, and the next message is clean.
#[test]
fn a_sysex_that_never_ends_costs_no_memory_and_the_next_message_is_clean() {
    let mut parser = Midi1Parser::new(MidiGroup::FIRST);
    let mut packets = 0u32;
    assert_no_alloc(|| {
        assert_eq!(parser.feed(0xF0), None);
        for i in 0..(1 << 20) {
            if let Some(Message::Sysex7(_, packet)) = parser.feed((i % 128) as u8) {
                assert_ne!(packet.status(), SysexStatus::End);
                assert_eq!(packet.bytes().len(), 6);
                packets += 1;
            }
        }
        let note = parser.feed_all(&[0x90, 60, 100]).next();
        assert!(matches!(note, Some(Message::Midi1(..))));
        let mut after = None;
        for message in parser.feed_all(&[0xF0, 1, 0xF7]) {
            assert!(after.replace(message).is_none());
        }
        assert!(matches!(
            after,
            Some(Message::Sysex7(_, p)) if p.status() == SysexStatus::Complete
        ));
    });
    assert_eq!(packets, ((1 << 20) - 1) / 6);
}

// ---- generated streams -------------------------------------------------------------

/// A byte of a stream, and the message that completes on it.
type Tagged = (u8, Option<Message>);

fn u7(rng: &mut Rng) -> U7 {
    U7::new(rng.byte(128)).unwrap()
}

/// One channel or system common message with its bytes tagged; the status
/// byte is left out when `running` says the previous message's status holds.
fn short_message(rng: &mut Rng, previous: &mut Option<u8>, out: &mut Vec<Tagged>) {
    let channel = MidiChannel::of(rng.byte(16));
    let address = Address {
        group: MidiGroup::FIRST,
        channel,
    };
    let note = NoteNumber::new(rng.byte(128)).unwrap();
    let voice = match rng.below(7) {
        0 => Voice1::NoteOff {
            note,
            velocity: u7(rng),
        },
        1 => Voice1::NoteOn {
            note,
            velocity: u7(rng),
        },
        2 => Voice1::PolyPressure {
            note,
            pressure: u7(rng),
        },
        3 => Voice1::ControlChange {
            controller: CCNumber::new(rng.byte(128)).unwrap(),
            value: u7(rng),
        },
        4 => Voice1::ProgramChange(u7(rng)),
        5 => Voice1::ChannelPressure(u7(rng)),
        _ => Voice1::PitchBend(U14::new(u16::try_from(rng.below(1 << 14)).unwrap()).unwrap()),
    };
    let wire = voice.bytes(channel);
    let wire = wire.as_slice();
    // The status stays in effect for the next message when a coin says so.
    let repeat = *previous == Some(wire[0]) && rng.below(2) == 0;
    let data = if repeat { &wire[1..] } else { wire };
    *previous = Some(wire[0]);
    push_tagged(out, data, Message::Midi1(address, voice));
}

fn common_message(rng: &mut Rng, out: &mut Vec<Tagged>) {
    let system = match rng.below(3) {
        0 => System::TimeCode(u7(rng)),
        1 => System::SongPosition(U14::new(u16::try_from(rng.below(1 << 14)).unwrap()).unwrap()),
        _ => System::SongSelect(u7(rng)),
    };
    push_tagged(
        out,
        system.bytes().as_slice(),
        Message::System(MidiGroup::FIRST, system),
    );
}

fn push_tagged(out: &mut Vec<Tagged>, bytes: &[u8], message: Message) {
    let (last, rest) = bytes.split_last().unwrap();
    out.extend(rest.iter().map(|&b| (b, None)));
    out.push((*last, Some(message)));
}

/// `0xF0`, `len` payload bytes, `0xF7`; each packet is tagged on the byte that
/// pushes it out: a seventh byte pushes the `Start`, then one more every six.
fn sysex_message(rng: &mut Rng, len: usize, out: &mut Vec<Tagged>) {
    let packet = |status, bytes: &[u8]| {
        Message::Sysex7(MidiGroup::FIRST, Sysex7Packet::new(status, bytes).unwrap())
    };
    let payload: Vec<u8> = (0..len).map(|_| rng.byte(128)).collect();
    out.push((0xF0, None));
    let mut sent = 0;
    for (i, &byte) in payload.iter().enumerate() {
        let tag = (i > 0 && i % 6 == 0).then(|| {
            let status = if sent == 0 {
                SysexStatus::Start
            } else {
                SysexStatus::Continue
            };
            sent += 1;
            packet(status, &payload[i - 6..i])
        });
        out.push((byte, tag));
    }
    let (status, tail) = match sent {
        0 => (SysexStatus::Complete, &payload[..]),
        _ => (SysexStatus::End, &payload[6 * sent..]),
    };
    out.push((0xF7, Some(packet(status, tail))));
}

/// Real-time bytes dropped between any two bytes, each coming out where it
/// stands.
fn with_real_time(rng: &mut Rng, stream: Vec<Tagged>) -> Vec<Tagged> {
    const REAL_TIME: [(u8, System); 6] = [
        (0xF8, System::TimingClock),
        (0xFA, System::Start),
        (0xFB, System::Continue),
        (0xFC, System::Stop),
        (0xFE, System::ActiveSensing),
        (0xFF, System::Reset),
    ];
    let mut out = Vec::new();
    for tagged in stream {
        while rng.below(6) == 0 {
            let (byte, system) = REAL_TIME[rng.below(6) as usize];
            out.push((byte, Some(Message::System(MidiGroup::FIRST, system))));
        }
        out.push(tagged);
    }
    out
}

/// Streams of channel messages with and without running status, system common
/// messages, SysEx of every length from 0 to 40, and real-time bytes anywhere
/// (inside a message's data and inside a SysEx) read back as exactly the
/// messages that made them, in order. A SysEx of 6 bytes or fewer is one
/// `Complete`; longer, a `Start`, `Continue`s of six and an `End`.
#[test]
fn generated_streams_read_back_to_their_messages() {
    let mut rng = Rng(0x2545_F491_4F6C_DD1D);
    let (mut streams, mut sysex_packets) = (0u32, 0u32);
    for _ in 0..20_000 {
        let mut stream = Vec::new();
        let mut running = None;
        for _ in 0..1 + rng.below(8) {
            match rng.below(5) {
                0 => {
                    running = None;
                    common_message(&mut rng, &mut stream);
                }
                1 => {
                    running = None;
                    let len = rng.below(41) as usize;
                    sysex_message(&mut rng, len, &mut stream);
                }
                _ => short_message(&mut rng, &mut running, &mut stream),
            }
        }
        let stream = with_real_time(&mut rng, stream);
        let bytes: Vec<u8> = stream.iter().map(|&(b, _)| b).collect();
        let want: Vec<Message> = stream.iter().filter_map(|&(_, m)| m).collect();
        sysex_packets += want
            .iter()
            .filter(|m| matches!(m, Message::Sysex7(..)))
            .count() as u32;

        let got: Vec<Message> = Midi1Parser::new(MidiGroup::FIRST)
            .feed_all(&bytes)
            .collect();
        assert_eq!(got, want, "{bytes:02X?}");

        // The same stream cut into pieces reads the same.
        let mut parser = Midi1Parser::new(MidiGroup::FIRST);
        let mut pieces = Vec::new();
        let mut rest = &bytes[..];
        while !rest.is_empty() {
            let (head, tail) = rest.split_at(1 + rng.below(rest.len() as u64) as usize);
            pieces.extend(parser.feed_all(head));
            rest = tail;
        }
        assert_eq!(pieces, want, "{bytes:02X?}");
        streams += 1;
    }
    assert_eq!(streams, 20_000);
    assert!(sysex_packets > 10_000, "{sysex_packets}");
}

/// `Voice1::data_len` is the MIDI 1.0 table: Program Change and Channel
/// Pressure take one data byte, the other channel voice statuses two, and
/// nothing else has a count.
#[test]
fn a_channel_status_says_how_many_data_bytes_follow() {
    use tutti_ump::Voice1;
    for status in 0..=255u8 {
        let expected = match status {
            0x80..=0xBF | 0xE0..=0xEF => Some(2),
            0xC0..=0xDF => Some(1),
            _ => None,
        };
        assert_eq!(Voice1::data_len(status), expected, "status {status:#04x}");
    }
}
