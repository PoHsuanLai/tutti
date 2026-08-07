//! Beat-scheduled playback: what gets installed, and what that costs.
//!
//! The assertions here are on the *installed source*, not on audio: whether a
//! clip reaches a synth's port, whether two installs on one synth both survive,
//! and whether a rebuild leaves a note hanging. Rendering a synth to prove a
//! note sounded would test rustysynth, not this layer.

#![cfg(all(feature = "midi", feature = "synth"))]

use bevy_app::prelude::*;
use bevy_ecs::entity::Entity;

use bevy_tutti::graph::{AudioConfig, AudioGraphRes, GraphReconcilePlugin, TransportRes};
use bevy_tutti::midi::{MidiSourceInstall, MidiTargetRegistry, TuttiMidiPlugin};
use bevy_tutti::AudioEngineState;
use tutti_core::dsp::Net;
use tutti_core::transport::Transport;
use tutti_core::AudioNode;
use tutti_core::{Beat, BeatDuration, SampleRate};
use tutti_midi_runtime::tutti_midi_types::tutti_types::{MidiChannel, MidiGroup};
use tutti_midi_runtime::TimedMidiEvent;
use tutti_midi_types::ump::MidiEvent;
use tutti_polysynth::{PolySynth, SynthConfig};

const SAMPLE_RATE: f64 = 48_000.0;

/// A note-on/note-off pair as timed events, which is what an install holds.
///
/// Local to this test rather than a library constructor: a note record with a
/// duration is authoring vocabulary, and MIDI 2.0 defines none — the wire has
/// only the two events this builds. Whether the engine should own such a record
/// is an open design question, and it should not be settled by a test helper.
///
/// `note_on` takes the native 16-bit MIDI-2 velocity, so callers name the field
/// the spec defines rather than a normalized float.
fn note(number: u8, start: Beat, duration: BeatDuration, velocity: u16) -> [TimedMidiEvent; 2] {
    [
        TimedMidiEvent::new(
            start,
            MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, number, velocity),
        ),
        // `Beat + BeatDuration` is the affine operator (`unit_affine!`): a
        // position plus a span is a position. `Beat + Beat` deliberately does
        // not compile, which is what keeps the two straight here.
        TimedMidiEvent::new(
            start + duration,
            MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::FIRST, number, 0),
        ),
    ]
}

/// The mezzo-forte default — MIDI 2.0's center velocity, and what a 7-bit 64
/// widens to through the spec's Min-Center-Max scaler.
const MF: u16 = 0x8000;

fn app() -> App {
    let mut app = App::new();
    let mut net = Net::new(0, 2);
    let _backend = net.backend();
    app.insert_resource(AudioGraphRes(net));
    app.insert_resource(TransportRes(Transport::new(SAMPLE_RATE)));
    app.insert_resource(AudioConfig {
        sample_rate: SampleRate(SAMPLE_RATE),
        channels: Default::default(),
    });
    app.insert_resource(AudioEngineState::Running);
    app.insert_resource(bevy_tutti::midi::test_support::midi_bus_for_test());
    app.insert_resource(bevy_tutti::midi::test_support::clock_master_for_test(
        48_000.0,
    ));
    // `engine_ready` claims every resource the engine block inserts is
    // present, and the route rebuild takes `MidiRoutingRes` as a plain
    // `ResMut` on that promise. A test asserting readiness supplies it.
    app.insert_resource(bevy_tutti::midi::test_support::routing_table_for_test().0);
    app.add_plugins((GraphReconcilePlugin, TuttiMidiPlugin));
    app.world_mut()
        .resource_mut::<MidiTargetRegistry>()
        .register::<PolySynth>();
    app
}

/// A synth entity, plus a handle on its port for assertions.
fn spawn_synth(app: &mut App) -> Entity {
    let synth = PolySynth::new(SynthConfig::default()).expect("builds a synth");
    let node = {
        let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
        graph.0.push(Box::new(synth))
    };
    app.world_mut().spawn(AudioNode(node)).id()
}

/// Start the transport rolling.
///
/// A clip source emits nothing while paused — `BeatWindow::from_timeline`
/// returns `None` — so every playback assertion needs this first.
fn roll(app: &App) {
    let transport = app.world().resource::<TransportRes>().clone();
    let _ = transport
        .motion
        .try_send(tutti_core::transport::MotionEvent::Play);
    transport.motion.drain();
    assert!(
        tutti_core::transport::Timeline::is_rolling(&transport.0),
        "the transport must be rolling for a clip to emit"
    );
}

/// Poll the synth's port for one block, returning what it yields.
///
/// This is what the audio thread does. Polling drains, so a caller sees each
/// event once.
fn poll(app: &App, entity: Entity, block: usize) -> Vec<MidiEvent> {
    let node = app.world().get::<AudioNode>(entity).expect("has a node");
    let graph = app.world().resource::<AudioGraphRes>();
    let synth = graph
        .0
        .node_as::<PolySynth>(node.0)
        .expect("is a PolySynth");
    let mut buf = [MidiEvent::noop(); 64];
    let n = synth.midi_port().poll(block, &mut buf);
    buf[..n].to_vec()
}

/// A note reaches the synth's port with a real frame offset — the thing the old
/// per-frame path could never do (it left every event at offset 0).
#[test]
fn a_scheduled_note_lands_at_a_frame_offset() {
    let mut app = app();
    let synth = spawn_synth(&mut app);
    roll(&app);

    // One note a third of a beat in, so its offset falls inside a block rather
    // than on its boundary.
    app.world_mut().spawn(MidiSourceInstall::new(
        synth,
        note(60, Beat(1.0 / 3.0), BeatDuration(1.0), MF).to_vec(),
    ));
    app.update();

    // At 120 BPM / 48 kHz a beat is 24 000 samples, so a third of one is 8 000 —
    // far inside a 16 384-frame block.
    let events = poll(&app, synth, 16_384);
    let note_on = events
        .iter()
        .find(|e| e.is_note_on())
        .expect("the clip should have emitted a note-on");
    assert!(
        note_on.frame_offset > 0,
        "a beat-scheduled note must carry a sample offset, got {}",
        note_on.frame_offset
    );
}

/// Two installs naming one synth both play.
///
/// `MidiInPort::install` *replaces* — it holds one source, not a stack — so a
/// rebuild that installed per-component would silently drop all but the last.
/// They have to be merged into one clip.
#[test]
fn two_installs_on_one_synth_both_sound() {
    let mut app = app();
    let synth = spawn_synth(&mut app);
    roll(&app);

    app.world_mut().spawn(MidiSourceInstall::new(
        synth,
        note(60, Beat(0.0), BeatDuration(1.0), MF).to_vec(),
    ));
    app.world_mut().spawn(MidiSourceInstall::new(
        synth,
        note(67, Beat(0.0), BeatDuration(1.0), MF).to_vec(),
    ));
    app.update();

    let notes: Vec<u8> = poll(&app, synth, 16_384)
        .iter()
        .filter(|e| e.is_note_on())
        .filter_map(|e| e.note())
        .collect();

    assert!(
        notes.contains(&60) && notes.contains(&67),
        "both installs must survive the merge, saw {notes:?}"
    );
}

/// Editing an install mid-playback must not leave the previous note sounding.
///
/// A rebuild mints a fresh `MidiClipSource` with a fresh cursor, starting at the
/// current beat — so the outgoing clip's note-off is simply never delivered. The
/// rebuild has to silence the target itself.
#[test]
fn a_rebuild_does_not_hang_the_previous_note() {
    let mut app = app();
    let synth = spawn_synth(&mut app);

    let install = app
        .world_mut()
        .spawn(MidiSourceInstall::new(
            synth,
            // A long note, so a mid-playback edit lands between its on and off.
            note(60, Beat(0.0), BeatDuration(32.0), MF).to_vec(),
        ))
        .id();
    app.update();
    let _ = poll(&app, synth, 512); // consume the note-on

    // Edit it — the note-off at beat 32 belongs to a clip about to be discarded.
    app.world_mut()
        .entity_mut(install)
        .insert(MidiSourceInstall::new(
            synth,
            note(64, Beat(0.0), BeatDuration(32.0), MF).to_vec(),
        ));
    app.update();

    let events = poll(&app, synth, 512);
    let all_notes_off = events.iter().map(|e| e.message()).any(|m| {
        matches!(
            m,
            tutti_midi_types::MidiMessage::ControlChange { index: 123, .. }
        )
    });
    assert!(
        all_notes_off,
        "a rebuild must silence the target, else the outgoing note hangs: {events:?}"
    );
}

/// Removing the last install naming a target clears its source, so the synth
/// stops playing rather than looping the old clip forever.
#[test]
fn removing_the_last_install_clears_the_source() {
    let mut app = app();
    let synth = spawn_synth(&mut app);
    roll(&app);

    let install = app
        .world_mut()
        .spawn(MidiSourceInstall::new(
            synth,
            note(60, Beat(4.0), BeatDuration(1.0), MF).to_vec(),
        ))
        .id();
    app.update();

    app.world_mut().despawn(install);
    app.update();
    let _ = poll(&app, synth, 512); // drain the all-notes-off

    // Move the transport onto the note and poll: a cleared port yields nothing.
    let transport = app.world().resource::<TransportRes>().clone();
    transport.settings.set_beat(tutti_core::Beat(4.0));
    let events = poll(&app, synth, 512);
    assert!(
        !events.iter().any(|e| e.is_note_on()),
        "a cleared target must not keep playing: {events:?}"
    );
}

/// Velocity keeps its full 16-bit range end to end.
///
/// Two velocities one LSB apart at 16 bits are the same number at 7, so this
/// fails the moment anything on the install → port path narrows the field. It
/// caught exactly that once: a `note_on_7bit` call that crushed the value and
/// widened it back.
#[test]
fn velocity_keeps_its_full_width() {
    let mut app = app();
    let synth = spawn_synth(&mut app);
    roll(&app);

    let mut events = note(60, Beat(0.0), BeatDuration(1.0), MF).to_vec();
    events.extend(note(64, Beat(0.0), BeatDuration(1.0), MF + 1));
    app.world_mut().spawn(MidiSourceInstall::new(synth, events));
    app.update();

    let velocities: Vec<u16> = poll(&app, synth, 16_384)
        .iter()
        .map(|e| e.message())
        .filter_map(|m| match m {
            tutti_midi_types::MidiMessage::NoteOn { velocity, .. } => Some(velocity),
            _ => None,
        })
        .collect();

    assert_eq!(velocities.len(), 2, "both notes should sound");
    assert_ne!(
        velocities[0], velocities[1],
        "one 16-bit LSB apart — indistinguishable at 7 bits: {velocities:?}"
    );
    assert!(
        velocities.contains(&MF) && velocities.contains(&(MF + 1)),
        "the exact values must arrive, not merely differ: {velocities:?}"
    );
}
