//! Regression gate: the unit's `Node::process` must not allocate, at any
//! block length.
//!
//! A block walks a variable number of render segments (one per distinct
//! event offset), so the likely regressions are shaped like allocation — collecting the segment boundaries into a `Vec`,
//! growing the scratch buffers to `size` on the audio thread, or collecting
//! the block's events when it carries more than usual.
//!
//! What the unit owns, and where each is sized:
//! - `left_buffer` / `right_buffer` — sized in `prepare` to the prepared
//!   `MaxBlock` and never resized on the audio thread (no block is longer).
//! - no event scratch: the block's events are read in place, through
//!   `Io::sub_blocks`.
//!
//! Driven by hand through `support::Hand` (`tutti_graph::contract::Direct`
//! underneath), prepared outside every gate as a graph prepares a node.
//! - the player's voices (a pool sized to the polyphony), the core's tables
//!   (`Voicing`, sized to the polyphony) and the chorus and reverb lines are
//!   all sized at construction.
//!
//! Note the fixture dependency: these run against the committed `TimGM6mb.sf2`,
//! and construction (which does allocate, heavily) is deliberately outside
//! every gate.

use std::num::NonZeroUsize;
use std::sync::Arc;
use tutti_graph::UmpAt;

use assert_no_alloc::AllocDisabler;
mod support;

use support::Hand;
use tutti_fixtures::midi;
use tutti_graph::Offset;
use tutti_sf2::SoundFont;
use tutti_soundfont::{Settings, SoundFontUnit};
use tutti_types::{SampleRate, Samples};
use tutti_ump::{
    Address, Attribute, Bank, CCNumber, Data128, Flex, Message, MidiChannel, MidiGroup, NoteNumber,
    RegisteredPerNoteController, Stream, Sysex7Packet, SysexStatus, System, U7, U14, Ump, Utility,
    Voice1, Voice2,
};

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

/// Load the committed test SoundFont, failing loudly if the checkout lacks it.
fn load_test_soundfont() -> Arc<SoundFont> {
    let path = tutti_fixtures::asset("soundfonts/TimGM6mb.sf2");
    let mut file = std::fs::File::open(&path)
        .unwrap_or_else(|e| panic!("test soundfont missing at {}: {e}", path.display()));
    Arc::new(
        SoundFont::new(&mut file)
            .unwrap_or_else(|e| panic!("test soundfont at {} is malformed: {e}", path.display())),
    )
}

/// A unit at 44.1 kHz, prepared (outside any gate) and driven by hand.
fn unit() -> Hand {
    let settings = Settings::new(SampleRate::SR_44K1);
    Hand::new(SoundFontUnit::new(load_test_soundfont(), &settings).expect("create SoundFontUnit"))
}

/// `offset` frames into a block of [`Hand`]'s length.
fn at(offset: u32) -> Offset {
    Offset::new(offset as usize, Samples(64)).expect("inside the block")
}

fn note_on(key: u8, offset: u32) -> UmpAt {
    let velocity = tutti_fixtures::midi::wide_velocity(100); // 7-bit 100, widened (M2-115)
    (
        at(offset),
        midi::note_on(MidiGroup::FIRST, MidiChannel::FIRST, key, velocity),
    )
}

fn note_off(key: u8, offset: u32) -> UmpAt {
    (
        at(offset),
        midi::note_off(MidiGroup::FIRST, MidiChannel::FIRST, key, 0),
    )
}

#[test]
fn soundfont_process_idle_is_allocation_free() {
    let mut unit = unit();

    for _ in 0..16 {
        unit.block(64);
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..2_000 {
            unit.block(64);
        }
    });
}

#[test]
fn soundfont_process_with_active_voices_is_allocation_free() {
    let mut unit = unit();
    unit.queue_midi_at(&[note_on(60, 0), note_on(64, 0), note_on(67, 0)]);

    for _ in 0..32 {
        unit.block(64);
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..2_000 {
            unit.block(64);
        }
    });
}

/// Several events at distinct offsets in one block, so `process` splits the
/// render into multiple segments.
///
/// A regression that collected segment
/// boundaries into a `Vec`, or that allocated a scratch slice per segment,
/// lands here and nowhere else — the two tests above drive one segment per
/// block and would stay green.
#[test]
fn soundfont_process_with_events_inside_block_is_allocation_free() {
    let mut unit = unit();

    // Warm up every lazily-sized buffer at full occupancy first.
    unit.queue_midi_at(&[note_on(48, 0), note_on(60, 16), note_on(72, 48)]);
    for _ in 0..64 {
        unit.block(64);
    }
    unit.queue_midi_at(&[note_off(48, 0), note_off(60, 0), note_off(72, 0)]);
    for _ in 0..256 {
        unit.block(64);
    }

    // Steady churn with events spread across the block, including frame 0 and
    // the last frame — the two boundary segments of the split loop.
    assert_no_alloc::assert_no_alloc(|| {
        for i in 0..200 {
            if i % 2 == 0 {
                unit.queue_midi_at(&[note_on(60, 0), note_on(64, 24), note_on(67, 63)]);
            } else {
                unit.queue_midi_at(&[note_off(60, 0), note_off(64, 24), note_off(67, 63)]);
            }
            unit.block(64);
        }
    });
}

/// Many events in one block — every frame carries one, so the split loop runs
/// its maximum number of segments for a 64-frame block.
///
/// The worst case for the segment walk: 64 chunks in a block, each with its
/// event, read in place.
#[test]
fn soundfont_process_with_an_event_every_frame_is_allocation_free() {
    let mut unit = unit();

    // One event per frame of the block, alternating on and off across keys so
    // the voice collection churns rather than retriggering one slot.
    let on: Vec<UmpAt> = (0..64u32)
        .map(|f| note_on(36 + (f % 24) as u8, f))
        .collect();
    let off: Vec<UmpAt> = (0..64u32)
        .map(|f| note_off(36 + (f % 24) as u8, f))
        .collect();

    for _ in 0..8 {
        unit.queue_midi_at(&on);
        for _ in 0..16 {
            unit.block(64);
        }
        unit.queue_midi_at(&off);
        for _ in 0..64 {
            unit.block(64);
        }
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..64 {
            unit.queue_midi_at(&on);
            for _ in 0..8 {
                unit.block(64);
            }
            unit.queue_midi_at(&off);
            for _ in 0..32 {
                unit.block(64);
            }
        }
    });
}

/// One-frame blocks: a graph may
/// hand the node a block of one frame.
#[test]
fn soundfont_one_frame_blocks_are_allocation_free() {
    let mut unit = unit();
    unit.queue_midi_at(&[note_on(60, 0), note_on(64, 0), note_on(67, 0)]);

    for _ in 0..256 {
        unit.tick();
    }

    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..100_000 {
            unit.tick();
        }
    });
}

/// Every message kind `decode` yields is decoded and dispatched without
/// allocating: each MIDI 1.0 and MIDI 2.0 channel voice message (applied or
/// dropped), every non-channel-voice message, and a packet `decode` refuses.
/// The sound is on, so a note and a bend, a controller and a program change
/// all reach the synthesizer inside the gate.
#[test]
fn soundfont_every_decoded_message_is_allocation_free() {
    let mut unit = unit();
    let address = Address {
        group: MidiGroup::FIRST,
        channel: MidiChannel::FIRST,
    };
    let note = NoteNumber::of(60);
    let ev = |m: Message| Ump::from(m);
    let m1 = |v| midi::m1(address.channel, v);
    let m2 = |v| midi::m2(address.channel, v);
    let events = [
        m1(Voice1::NoteOn {
            note,
            velocity: U7::of(100),
        }),
        m1(Voice1::PolyPressure {
            note,
            pressure: U7::of(50),
        }),
        m1(Voice1::ControlChange {
            controller: CCNumber::VOLUME,
            value: U7::of(90),
        }),
        m1(Voice1::PitchBend(U14::of(9000))),
        m1(Voice1::ChannelPressure(U7::of(5))),
        m1(Voice1::ProgramChange(U7::of(5))),
        m1(Voice1::NoteOff {
            note,
            velocity: U7::of(0),
        }),
        m1(Voice1::NoteOn {
            note,
            velocity: U7::of(0),
        }),
        m2(Voice2::NoteOn {
            note,
            velocity: 0x8000,
            attribute: Attribute::Pitch(60 << 9),
        }),
        // Nonzero, and narrows to 0: floored at 1, not a Note Off.
        m2(Voice2::NoteOn {
            note,
            velocity: 1,
            attribute: Attribute::None,
        }),
        m2(Voice2::NoteOn {
            note,
            velocity: 0,
            attribute: Attribute::None,
        }),
        m2(Voice2::PerNotePitchBend {
            note,
            value: 0xC000_0000,
        }),
        m2(Voice2::PolyPressure {
            note,
            value: 0x4000_0000,
        }),
        m2(Voice2::RegisteredPerNote {
            note,
            controller: RegisteredPerNoteController::Pitch,
            value: 69 << 25,
        }),
        m2(Voice2::AssignablePerNote {
            note,
            controller: 74,
            value: 1 << 30,
        }),
        m2(Voice2::PerNoteManagement {
            note,
            detach: false,
            reset: true,
        }),
        m2(Voice2::Registered {
            bank: U7::of(0),
            index: U7::of(7),
            value: 12 << 25,
        }),
        m2(Voice2::Assignable {
            bank: U7::of(1),
            index: U7::of(2),
            value: 3,
        }),
        m2(Voice2::RelativeRegistered {
            bank: U7::of(0),
            index: U7::of(0),
            delta: -1,
        }),
        m2(Voice2::RelativeAssignable {
            bank: U7::of(0),
            index: U7::of(0),
            delta: 1,
        }),
        m2(Voice2::ControlChange {
            controller: CCNumber::VOLUME,
            value: 1 << 31,
        }),
        m2(Voice2::PitchBend(0x9000_0000)),
        m2(Voice2::ChannelPressure(1 << 30)),
        m2(Voice2::ProgramChange {
            program: U7::of(3),
            bank: Some(Bank {
                msb: U7::of(1),
                lsb: U7::of(2),
            }),
        }),
        m2(Voice2::NoteOff {
            note,
            velocity: 0,
            attribute: Attribute::Reserved { kind: 9, data: 2 },
        }),
        ev(Message::Utility(Utility::JrClock(1))),
        ev(Message::System(MidiGroup::FIRST, System::TimingClock)),
        ev(Message::Sysex7(
            MidiGroup::FIRST,
            Sysex7Packet::new(SysexStatus::Complete, &[1, 2, 3]).unwrap(),
        )),
        ev(Message::Data128(
            MidiGroup::FIRST,
            Data128::Sysex8Abort { stream_id: 1 },
        )),
        ev(Message::Flex(MidiGroup::FIRST, Flex::SetTempo(500_000))),
        ev(Message::Stream(Stream::StartOfClip)),
        // A reserved message type: `decode` refuses it.
        Ump([0x6000_0000, 0, 0, 0]),
    ];
    for _ in 0..4 {
        unit.queue_midi(&events);
        unit.block(64);
    }
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..100 {
            unit.queue_midi(&events);
            unit.block(64);
        }
    });
}

/// The first two keys of the standard drum kit (bank 128, patch 0) that share a
/// non-zero exclusive class, found in the font itself (SF2 2.04 section 8.1.2,
/// generator 57).
fn two_drums_of_a_class(font: &SoundFont) -> (u8, u8) {
    let kit = font
        .presets()
        .iter()
        .find(|p| p.bank() == Some(tutti_sf2::Bank::Percussion) && p.patch() == U7::new(0))
        .expect("a standard drum kit");
    let velocity = U7::of(100);
    let class_of = |k: u8| -> u8 {
        let key = NoteNumber::of(k);
        let mut class = 0;
        for pr in kit.regions() {
            if pr.contains(key, velocity) {
                for ir in font.instruments()[pr.instrument_id()].regions() {
                    if ir.contains(key, velocity) {
                        class = class.max(ir.exclusive_class().map_or(0, |c| c.get()));
                    }
                }
            }
        }
        class
    };
    (0..128u8)
        .flat_map(|a| (a + 1..128).map(move |b| (a, b)))
        .find(|&(a, b)| class_of(a) != 0 && class_of(a) == class_of(b))
        .expect("two drums share a class in this font")
}

fn cc(channel: u8, controller: CCNumber, value: u8) -> Ump {
    midi::m1_cc(MidiChannel::of(channel), controller, U7::of(value))
}

fn drum(key: u8, offset: u32) -> UmpAt {
    let velocity = tutti_fixtures::midi::wide_velocity(100); // 7-bit 100, widened (M2-115)
    (
        at(offset),
        midi::note_on(MidiGroup::FIRST, MidiChannel::of(9), key, velocity),
    )
}

/// The note logic's paths allocate nothing: a pool that is full and gives up a
/// voice on every strike (8 voices, 24 keys, several zones each), a key struck
/// again, the sustain and sostenuto pedals going down and up with notes under
/// them, All Notes Off, All Sound Off and Reset All Controllers, an exclusive
/// class ending the others of its class (a quick release), and a reset of the
/// node itself.
#[test]
fn soundfont_note_logic_is_allocation_free() {
    let font = load_test_soundfont();
    let (open, closed) = two_drums_of_a_class(&font);
    let settings = Settings::new(SampleRate::SR_44K1).with_polyphony(NonZeroUsize::new(8).unwrap());
    let mut unit = Hand::new(SoundFontUnit::new(font, &settings).expect("create SoundFontUnit"));

    let ons: Vec<UmpAt> = (0..24u32).map(|f| note_on(36 + f as u8, f * 2)).collect();
    let offs: Vec<UmpAt> = (0..24u32).map(|f| note_off(36 + f as u8, f * 2)).collect();
    let round = |unit: &mut Hand| {
        // Every key struck over a full pool, and one of them again.
        unit.queue_midi_at(&ons);
        unit.queue_midi_at(&[note_on(40, 60)]);
        unit.block(64);
        // The pedals down with notes under them, then the notes off.
        unit.queue_midi(&[
            cc(0, CCNumber::SUSTAIN, 127),
            cc(0, CCNumber::SOSTENUTO, 127),
        ]);
        unit.block(64);
        unit.queue_midi_at(&offs);
        unit.block(64);
        unit.queue_midi(&[cc(0, CCNumber::SUSTAIN, 0), cc(0, CCNumber::SOSTENUTO, 0)]);
        unit.block(64);
        // An exclusive class: the second drum ends the first.
        unit.queue_midi_at(&[drum(open, 0), drum(closed, 8)]);
        unit.block(64);
        // The panics and the reset.
        unit.queue_midi_at(&ons);
        unit.queue_midi(&[cc(0, CCNumber::ALL_NOTES_OFF, 0)]);
        unit.block(64);
        unit.queue_midi_at(&ons);
        unit.queue_midi(&[
            cc(0, CCNumber::ALL_SOUND_OFF, 0),
            cc(0, CCNumber::RESET_ALL, 0),
        ]);
        unit.block(64);
        for _ in 0..8 {
            unit.block(64);
        }
    };
    for _ in 0..4 {
        round(&mut unit);
    }
    assert_no_alloc::assert_no_alloc(|| {
        for _ in 0..50 {
            round(&mut unit);
        }
        for _ in 0..20 {
            unit.queue_midi_at(&ons);
            unit.block(64);
            tutti_graph::Node::reset(&mut *unit);
        }
    });
}
