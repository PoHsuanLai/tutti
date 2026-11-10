//! The messages the cases send, built through `tutti_ump`'s types.

use tutti_ump::{Address, CCNumber, Message, MidiChannel, MidiGroup, NoteNumber, U7, Ump, Voice1};

fn voice(channel: u8, voice: Voice1) -> Ump {
    Ump::from(Message::Midi1(
        Address {
            group: MidiGroup::FIRST,
            channel: MidiChannel::of(channel),
        },
        voice,
    ))
}

pub fn note_on(channel: u8, k: u8, velocity: u8) -> Ump {
    voice(
        channel,
        Voice1::NoteOn {
            note: NoteNumber::of(k),
            velocity: U7::of(velocity),
        },
    )
}

pub fn note_off(channel: u8, k: u8) -> Ump {
    voice(
        channel,
        Voice1::NoteOff {
            note: NoteNumber::of(k),
            velocity: U7::of(0),
        },
    )
}

pub fn cc(channel: u8, controller: u8, value: u8) -> Ump {
    voice(
        channel,
        Voice1::ControlChange {
            controller: CCNumber::of(controller),
            value: U7::of(value),
        },
    )
}
