//! Beat-scheduled playback: what gets installed, and what that costs.
//!
//! The assertions here are on the *installed source*, not on audio: whether a
//! clip reaches a synth's port, whether two installs on one synth both survive,
//! and whether a rebuild leaves a note hanging. Rendering a synth to prove a
//! note sounded would test rustysynth, not this layer.

#![cfg(all(feature = "midi", feature = "synth"))]

#[macro_use]
mod common;

use bevy_app::prelude::*;
use bevy_ecs::entity::Entity;

use bevy_tutti::graph::{
    AudioConfig, AudioGraphRes, CapturedControls, GraphReconcilePlugin, TransportRes,
};
use bevy_tutti::midi::{MidiSourceInstall, MidiTarget, MidiTargetRegistry, TuttiMidiPlugin};
use bevy_tutti::AudioEngineState;
use tutti_core::transport::Transport;
use tutti_core::{Beat, BeatDuration, SampleRate};
use tutti_midi_runtime::TimedMidiEvent;
use tutti_midi_types::ump::MidiEvent;
use tutti_midi_types::{MidiChannel, MidiGroup};
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
    app.insert_resource(AudioGraphRes::headless(0, 2));
    app.insert_resource(TransportRes(Transport::new(SAMPLE_RATE)));
    app.insert_resource(AudioConfig {
        sample_rate: SampleRate(SAMPLE_RATE),
        channels: tutti_core::ChannelLayout::STEREO,
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
    // `TuttiMidiPlugin` registers the `MidiFileAsset` loader at build time,
    // which panics without an `AssetServer` — a headless app supplies it.
    app.add_plugins((
        bevy_app::TaskPoolPlugin::default(),
        bevy_asset::AssetPlugin::default(),
    ));
    app.add_plugins((GraphReconcilePlugin, TuttiMidiPlugin));
    app.world_mut()
        .resource_mut::<MidiTargetRegistry>()
        .register::<PolySynth>();
    app
}

/// A synth entity, plus a handle on its port for assertions.
///
/// Bound the way every insertion path binds one: its controls (the MIDI port)
/// are captured from the unit before it moves into the graph.
fn spawn_synth(app: &mut App) -> Entity {
    let synth = PolySynth::new(SynthConfig::default()).expect("builds a synth");
    let controls = CapturedControls::capture(app.world(), &synth);
    let node = {
        let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
        graph.insert(synth)
    };
    let mut entity = app.world_mut().spawn_empty();
    controls.bind(&mut entity, node);
    entity.id()
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
    // The captured port shares its mailbox and source slot with the synth's.
    let target = app
        .world()
        .get::<MidiTarget>(entity)
        .expect("has a MIDI port");
    let mut buf = [MidiEvent::noop(); 64];
    // The rate the synth polls at: its own, which is the device's
    // (`AudioConfig`), re-rated with it by `restart_device`.
    let rate = app.world().resource::<AudioConfig>().sample_rate;
    let n = target.port().poll(block, rate, &mut buf);
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

/// **A device restart at a new rate places every installed clip at it.** A
/// clip that kept the rate it was built with would, built at 48 kHz on a
/// 96 kHz device, put a note a third of a beat in (120 BPM) at frame 8 000,
/// where the device has reached only a sixth of a beat. `restart_device`
/// moves the transport's rate and `AudioConfig` (and re-rates the synth), as
/// done by hand here; the clip holds no rate — its synth hands it the rate
/// it runs at on every poll (`MidiInPort::poll`, doc 013 PR 12) — so the
/// clip installed at 48 kHz, not rebuilt, places the note at 16 000.
///
/// From #39, where the clip was rebuilt on a rate change; since PR 12 there
/// is no rate in the clip to rebuild, and the rebuild was dropped.
///
/// Mutation (run): the clip advancing its beat window at a fixed 48 kHz
/// instead of the rate it is polled at (`MidiClipSource::sync_to_transport`)
/// → the note lands at 8 000 → fails.
#[test]
fn a_rate_change_places_the_clip_at_the_new_rate() {
    let mut app = app();
    let synth = spawn_synth(&mut app);
    roll(&app);
    app.world_mut().spawn(MidiSourceInstall::new(
        synth,
        note(60, Beat(1.0 / 3.0), BeatDuration(1.0), MF).to_vec(),
    ));
    app.update();

    app.world()
        .resource::<TransportRes>()
        .set_sample_rate(SampleRate(96_000.0));
    app.world_mut().resource_mut::<AudioConfig>().sample_rate = SampleRate(96_000.0);
    app.update();

    let events = poll(&app, synth, 16_384);
    let note_on = events
        .iter()
        .find(|e| e.is_note_on())
        .expect("the clip should have emitted a note-on");
    assert!(
        note_on.frame_offset.abs_diff(16_000) <= 1,
        "a third of a beat at 96 kHz is frame 16 000, got {}",
        note_on.frame_offset
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
    transport
        .clock_links()
        .expect("the only playhead writer")
        .set_playhead(tutti_core::Beat(4.0));
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

/// A synth inserted as a graph node, with a MIDI event input.
fn spawn_graph_synth(app: &mut App) -> Entity {
    use bevy_tutti::graph::SpawnGraphNode;
    let synth = PolySynth::new(SynthConfig::default()).expect("builds a synth");
    let entity = app.world_mut().commands().spawn_graph_node(synth).id();
    app.world_mut().flush();
    entity
}

fn node_of(app: &App, entity: Entity) -> tutti_core::AudioNode {
    *app.world()
        .get::<tutti_core::AudioNode>(entity)
        .expect("bound to a node")
}

/// **`EventSources` wires a clip node's events to a synth's event input, and
/// removing it unwires them.** Both are inserted as graph nodes
/// (`spawn_graph_node`); the declaration on the synth reaches the graph's
/// event edges on the next update, and its removal empties them.
///
/// Mutation: `reconcile` never writing (`set_event_sources` skipped) → the
/// first assertion fails. Mutation: dropping a sink with no declaration from
/// the wanted set → the edge outlives its declaration → the second fails.
#[test]
fn event_sources_wire_a_clip_node_to_a_graph_synth() {
    use bevy_tutti::graph::{EventSources, NodeControls, SpawnGraphNode};
    use tutti_midi_runtime::{MidiClipControls, MidiClipNode};

    let mut app = app();
    let synth = spawn_graph_synth(&mut app);
    let clip = app
        .world_mut()
        .commands()
        .spawn_graph_node(MidiClipNode::new(note(
            60,
            Beat(0.0),
            BeatDuration(1.0),
            MF,
        )))
        .id();
    app.world_mut().flush();
    assert!(
        app.world()
            .get::<NodeControls<MidiClipControls>>(clip)
            .is_some(),
        "a graph node's controls are kept on its entity"
    );
    assert!(
        app.world().get::<MidiTarget>(synth).is_some(),
        "a graph synth's port is still captured, for keyboards and routing"
    );
    app.world_mut()
        .entity_mut(synth)
        .insert(EventSources::from(clip));
    app.update();
    let graph = app.world().resource::<AudioGraphRes>();
    assert_eq!(graph.node_event_inputs(node_of(&app, synth)), 1);
    assert_eq!(
        graph.event_sources(node_of(&app, synth), 0),
        vec![node_of(&app, clip)]
    );

    app.world_mut().entity_mut(synth).remove::<EventSources>();
    app.update();
    let graph = app.world().resource::<AudioGraphRes>();
    assert!(graph.event_sources(node_of(&app, synth), 0).is_empty());
}

/// **An install on a synth with an event input plays through a clip node of
/// its own, edited in place, removed with the last install**, and nothing is
/// installed on the synth's port (which would play the clip twice).
///
/// Mutation: always taking the port path (ignoring `node_event_inputs`) →
/// no clip node → fails. Mutation: a fresh clip node per edit → the node
/// changes across the edit → fails. Mutation: keeping the clip node when the
/// install goes → it is still in the graph → fails.
#[test]
fn an_install_on_a_graph_synth_plays_through_a_clip_node() {
    use bevy_tutti::midi::SequencedClips;

    let mut app = app();
    let synth = spawn_graph_synth(&mut app);
    let install = app
        .world_mut()
        .spawn(MidiSourceInstall::new(
            synth,
            note(60, Beat(0.0), BeatDuration(1.0), MF).to_vec(),
        ))
        .id();
    app.update();
    let clip = app
        .world()
        .resource::<SequencedClips>()
        .node(synth)
        .expect("the synth plays through a clip node");
    assert_eq!(
        app.world()
            .resource::<AudioGraphRes>()
            .event_sources(node_of(&app, synth), 0),
        vec![clip]
    );
    roll(&app);
    assert!(
        poll(&app, synth, 64).is_empty(),
        "the clip is not also installed on the synth's port"
    );

    app.world_mut()
        .get_mut::<MidiSourceInstall>(install)
        .unwrap()
        .events = note(62, Beat(0.0), BeatDuration(1.0), MF).to_vec();
    app.update();
    assert_eq!(
        app.world().resource::<SequencedClips>().node(synth),
        Some(clip),
        "an edit replaces the clip's events in place"
    );

    app.world_mut().despawn(install);
    app.update();
    assert_eq!(app.world().resource::<SequencedClips>().node(synth), None);
    let graph = app.world().resource::<AudioGraphRes>();
    assert!(
        !graph.contains(clip),
        "the clip node goes with the last install"
    );
    assert!(graph.event_sources(node_of(&app, synth), 0).is_empty());
}
