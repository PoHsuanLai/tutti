//! A SoundFont voice, driven by MIDI through the session: the whole chain
//! rendered, so a break anywhere between the declaration and the speaker
//! fails it.
//!
//! The session half of `bevy-tutti`'s `midi_soundfont` suite (its ECS half,
//! the `PlaySoundFont` spawner and the asset, stayed there). Every other MIDI
//! test asserts on the plumbing (`midi.rs`' stand-in synth logs the events
//! at its port, `general_midi.rs` reads the topology); these render a real
//! `SoundFontUnit` through the graph and measure the output.
//!
//! The `.sf2` is committed at `assets/soundfonts/TimGM6mb.sf2`, so a missing
//! one fails with the resolved path rather than skipping.
//!
//! The clip tests roll the transport from beat 0 at 120 BPM (24 000 frames a
//! beat), where the `bevy-tutti`
//! ones moved a hand-kept playhead; the property is the same (a note sounds
//! on its beat and not before).

use std::sync::Arc;
use tutti_fixtures::signal::rms;
use tutti_types::MaxBlock;

use tutti_fixtures::midi::{note_off, note_on};
use tutti_graph::NodeKey;
use tutti_midi_runtime::{MidiIngestNode, TimedMidiEvent};
use tutti_session::{EventSource, MidiRouteRule, Session};
use tutti_sf2::SoundFont;
use tutti_soundfont::{Settings, SoundFontUnit};
use tutti_transport::MotionEvent;
use tutti_types::{At, Beat, BeatDuration, ChannelLayout, InterleavedMut, SampleRate, Samples};
use tutti_ump::{MidiChannel, MidiGroup};

const RATE: SampleRate = SampleRate(48_000.0);

/// A note-on/note-off pair at full velocity, which is what a clip holds.
///
/// These tests assert *audible output*, so velocity is pinned at the 16-bit
/// maximum rather than left to a default.
fn note(number: u8, start: Beat, duration: BeatDuration) -> Vec<TimedMidiEvent> {
    vec![
        TimedMidiEvent::new(
            start,
            note_on(MidiGroup::FIRST, MidiChannel::FIRST, number, u16::MAX),
        ),
        TimedMidiEvent::new(
            start + duration,
            note_off(MidiGroup::FIRST, MidiChannel::FIRST, number, 0),
        ),
    ]
}

/// The repo's test soundfont, panicking with the resolved path when it is
/// missing (it is committed, so that is a broken checkout).
fn soundfont() -> Arc<SoundFont> {
    let path = tutti_fixtures::asset("soundfonts/TimGM6mb.sf2");
    let mut file = std::fs::File::open(&path).unwrap_or_else(|e| {
        panic!(
            "committed test soundfont missing at {}: {e}",
            path.display()
        )
    });
    Arc::new(
        SoundFont::new(&mut file)
            .unwrap_or_else(|e| panic!("test soundfont at {} is malformed: {e}", path.display())),
    )
}

/// An offline stereo session with a SoundFont synth on the outputs.
fn rig() -> (Session, NodeKey) {
    let unit = SoundFontUnit::new(soundfont(), &Settings::new(RATE)).expect("builds the unit");
    let mut s = Session::builder()
        .build_offline(RATE, ChannelLayout::STEREO, MaxBlock::of(Samples(128)))
        .expect("builds");
    let (synth, _) = s.graph().add(unit);
    s.graph().set_outputs_from(synth);
    s.update();
    (s, synth)
}

/// `frames` frames from the session, in 128-frame blocks.
fn render(s: &Session, frames: usize) -> Vec<f32> {
    let mut out = Vec::with_capacity(frames * 2);
    let mut buf = vec![0.0f32; 128 * 2];
    for _ in 0..frames.div_ceil(128) {
        s.render(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO));
        out.extend_from_slice(&buf);
    }
    out.truncate(frames * 2);
    out
}

fn play(s: &Session) {
    s.transport()
        .motion
        .try_send(MotionEvent::Play)
        .expect("room");
}

/// **A declared note makes sound.** The end-to-end claim: `Midi::play_clip`
/// → clip node → event edge → engine beat→offset → SoundFont player →
/// samples. Silence here means a break anywhere along it.
#[test]
fn a_declared_note_produces_audio() {
    let (mut s, synth) = rig();
    s.midi()
        .play_clip(synth, note(60, Beat(0.0), BeatDuration(2.0)))
        .expect("a synth takes a clip");
    s.update();
    play(&s);

    // Half a second at 120 BPM covers the note-on comfortably.
    let level = rms(&render(&s, 24_000));
    assert!(
        level > 1e-4,
        "a declared note must reach the speakers, got RMS {level}"
    );
}

/// **Silence before the note, sound after**: the scheduling is real, not a
/// note that fires the instant the clip is installed. Without this, "it made
/// noise" would pass equally for a source that ignored the beat entirely.
///
/// The clip's note is on beat 4 (frame 96 000 at 120 BPM / 48 kHz): the
/// first 4 000 frames are silent, and the 12 000 after the beat sound.
#[test]
fn the_note_waits_for_its_beat() {
    let (mut s, synth) = rig();
    s.midi()
        .play_clip(synth, note(60, Beat(4.0), BeatDuration(4.0)))
        .expect("a synth takes a clip");
    s.update();
    play(&s);

    // Still well before beat 4.
    let before = rms(&render(&s, 4_000));
    // Roll up to the note, and render across it.
    render(&s, 96_000 - 4_000);
    let after = rms(&render(&s, 12_000));

    assert!(
        before < 1e-5,
        "nothing should sound before the note's beat, got RMS {before}"
    );
    assert!(
        after > 1e-4,
        "the note should sound once its beat arrives: {before} then {after}"
    );
}

/// **Live preview still reaches a synth that has a clip installed**: the
/// live input's queue node and the clip node both feed its event input,
/// merged by frame.
///
/// The clip is silent in this window by construction; `the_note_waits_for_its_beat`
/// covers it.
#[test]
fn preview_still_sounds_under_an_installed_clip() {
    let (mut s, synth) = rig();
    // A clip whose first note is far in the future, so anything audible in
    // the next quarter-second can only be the preview.
    s.midi()
        .play_clip(synth, note(60, Beat(100.0), BeatDuration(1.0)))
        .expect("a synth takes a clip");
    s.update();
    play(&s);

    let quiet = rms(&render(&s, 6_000));
    assert!(
        quiet < 1e-5,
        "the clip's note is far away; expected silence"
    );

    // A live input on the synth, then a live note, as a keyboard sends it.
    let live = s
        .midi()
        .live_input(synth)
        .expect("a synth takes a live input");
    s.update();
    live.send(
        At::NextBlock,
        note_on(MidiGroup::FIRST, MidiChannel::FIRST, 67, 0xFFFF),
    )
    .unwrap();

    let previewed = rms(&render(&s, 12_000));
    assert!(
        previewed > 1e-4,
        "live preview must still sound while a clip is installed, got RMS {previewed}"
    );
}

/// How loud a note on channel 2 through a hardware device's inlet comes out,
/// with a rule routing `channel` to the synth: the ingest node's channel port
/// is wired to the synth's event input (`Midi::set_routes` → the session's feeds
/// → an event edge), rendered through the graph.
fn routed_level(channel: MidiChannel) -> f32 {
    let (mut s, synth) = rig();
    // The device's inlet, wired to the ingest node's one event input.
    let (inlet, mut device) = s.graph().open_event_inlet(64);
    let (input, _) = s.graph().insert(MidiIngestNode::new(1));
    s.graph()
        .set_event_sources(input, 0, &[EventSource::new(inlet, 0)]);
    let rule = MidiRouteRule::for_channel(channel).to(synth);
    s.midi()
        .set_routes(input, [&rule], None, Some)
        .expect("one declarer");
    s.update();

    device
        .push(
            At::NextBlock,
            note_on(MidiGroup::FIRST, MidiChannel::of(1), 60, u16::MAX),
        )
        .expect("room");
    rms(&render(&s, 12_000))
}

/// **A note from the hardware MIDI input, routed by a rule, sounds**, and one
/// on a channel no rule routes does not.
#[test]
fn a_routed_hardware_note_sounds() {
    let level = routed_level(MidiChannel::of(1));
    assert!(
        level > 1e-4,
        "a note routed to the synth must sound, got RMS {level}"
    );
    let other = routed_level(MidiChannel::of(5));
    assert!(
        other < 1e-6,
        "a note on an unrouted channel must not, got RMS {other}"
    );
}
