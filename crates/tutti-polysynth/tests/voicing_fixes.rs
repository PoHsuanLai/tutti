//! The bugs the old allocator had that its decisions are now free of, each
//! with a stream that showed it (`decisions.rs` finds them in random streams;
//! these are the small cases). Each is a change against the code at #324.

use tutti_graph::contract::Direct;
use tutti_polysynth::{
    AllocationStrategy, FilterType, OscillatorType, PolySynth, SynthConfig, VoiceMode,
};
use tutti_types::{Adsr, Amplitude, SampleRate, Seconds};
use tutti_ump::{Address, Message, MidiChannel, MidiGroup, NoteNumber, U7, Ump, Voice1};
use tutti_voicing::Phase::{self, Key, Releasing};

fn synth(voices: usize, mode: VoiceMode) -> Direct<PolySynth> {
    let config = SynthConfig {
        sample_rate: SampleRate(48_000.0),
        max_voices: voices,
        voice_mode: mode,
        oscillator: OscillatorType::Saw,
        filter: FilterType::None,
        // A long release, so a released voice is still listed when asked.
        envelope: Adsr {
            attack: Seconds(0.001),
            decay: Seconds(0.0),
            sustain: Amplitude(1.0),
            release: Seconds(2.0),
        },
        allocation_strategy: AllocationStrategy::Oldest,
        ..SynthConfig::default()
    };
    Direct::new(PolySynth::new(config).unwrap(), SampleRate(48_000.0), 64)
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

fn on(d: &mut Direct<PolySynth>, channel: u8, key: u8) {
    d.node.apply_midi(pack(
        channel,
        Voice1::NoteOn {
            note: NoteNumber::new(key).unwrap(),
            velocity: U7::new(100).unwrap(),
        },
    ));
    d.block();
}

fn off(d: &mut Direct<PolySynth>, channel: u8, key: u8) {
    d.node.apply_midi(pack(
        channel,
        Voice1::NoteOff {
            note: NoteNumber::new(key).unwrap(),
            velocity: U7::new(0).unwrap(),
        },
    ));
    d.block();
}

/// `(channel, key, phase)` of each voice, by start order.
fn voices(d: &Direct<PolySynth>) -> Vec<(u8, u8, Phase)> {
    let mut v = d.node.sounding();
    v.sort_by_key(|g| g.seq);
    v.iter()
        .map(|g| (g.channel.get(), g.key.get(), g.phase))
        .collect()
}

/// **A stolen releasing voice does not take the live voice of its key with
/// it.** A key struck while it sounds leaves two voices of it, one releasing and
/// one live. The old allocator, stealing the releasing one, removed the id the
/// live one shared, so the live one's note-off found nothing and the key stuck.
///
/// Mutation (run): `Voicing::start` removing the stolen note by its key and
/// channel (the old allocator's identity) instead of its voice makes the
/// note-off find no voice and this fail with the key still held.
#[test]
fn a_stolen_releasing_voice_leaves_its_keys_live_voice_alone() {
    let mut d = synth(4, VoiceMode::Poly);
    on(&mut d, 0, 60);
    on(&mut d, 0, 60); // the first releases, the second lives
    on(&mut d, 0, 62);
    on(&mut d, 0, 64); // every slot taken
    assert_eq!(
        voices(&d),
        [(0, 60, Releasing), (0, 60, Key), (0, 62, Key), (0, 64, Key)]
    );
    on(&mut d, 0, 66); // the releasing voice gives way
    assert_eq!(
        voices(&d),
        [(0, 60, Key), (0, 62, Key), (0, 64, Key), (0, 66, Key)]
    );
    off(&mut d, 0, 60);
    assert_eq!(
        voices(&d)[0],
        (0, 60, Releasing),
        "the live voice is released"
    );
}

/// **Mono is one voice for each channel** (a behaviour change, decided). The
/// old allocator had one slot for all channels: a second channel's note took
/// it from the first.
///
/// Mutation (run): `start_mono` choosing the voice without regard to channel
/// leaves one voice and this fails.
#[test]
fn mono_channels_keep_a_voice_each() {
    let mut d = synth(8, VoiceMode::Mono);
    on(&mut d, 0, 60);
    on(&mut d, 1, 64);
    assert_eq!(voices(&d), [(0, 60, Key), (1, 64, Key)]);
    off(&mut d, 0, 60);
    assert_eq!(voices(&d), [(0, 60, Releasing), (1, 64, Key)]);
    // A new note on channel 0 takes channel 0's voice, not channel 1's.
    on(&mut d, 0, 62);
    assert_eq!(voices(&d), [(1, 64, Key), (0, 62, Key)]);
}

/// **Legato is remembered for each channel.** A note-off on one channel used to
/// clear the memory of the other, so its next note started anew.
///
/// Mutation (run): one legato memory for every channel makes the last
/// assertion fail (a new voice, not a moved one).
#[test]
fn legato_memory_is_a_channels_own() {
    let mut d = synth(8, VoiceMode::Legato);
    on(&mut d, 0, 60);
    let first = d.node.sounding()[0].seq;
    on(&mut d, 1, 64);
    off(&mut d, 1, 64); // clears nothing of channel 0's
    on(&mut d, 0, 62);
    let after: Vec<_> = d
        .node
        .sounding()
        .into_iter()
        .filter(|g| g.channel.get() == 0)
        .collect();
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].key.get(), 62, "channel 0's voice moved to 62");
    assert_eq!(
        after[0].seq, first,
        "and is the voice it was: no new attack"
    );
}
