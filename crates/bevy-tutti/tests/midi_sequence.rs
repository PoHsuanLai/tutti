//! Beat-scheduled playback: what a `MidiSourceInstall` wires to its target.
//!
//! The assertions here are on the *events the target's event input
//! receives*, through a tap node standing in for a synth: whether a note lands
//! on its frame, whether two installs on one target both play, and whether an
//! edit or a removal leaves a note hanging. Rendering a synth to prove a note
//! sounded would test the synth, not this layer.

#![cfg(all(feature = "midi", feature = "synth"))]

#[macro_use]
mod common;

use std::sync::{Arc, Mutex};

use bevy_app::prelude::*;
use bevy_ecs::entity::Entity;

use bevy_tutti::graph::{
    AudioConfig, AudioGraphRes, GraphNode, GraphReconcilePlugin, SpawnGraphNode, TransportRes,
};
use bevy_tutti::midi::{MidiSourceInstall, TuttiMidiPlugin};
use bevy_tutti::AudioEngineState;
use tutti_core::transport::Transport;
use tutti_core::{Beat, BeatDuration, Bpm, SampleRate};
use tutti_graph::{
    Cx, EventKind, IntoNode, Io, Node, NodeParts, Prepare, Shape, Status, Ump, Unforkable,
};
use tutti_midi_runtime::TimedMidiEvent;
use tutti_midi_types::ump::MidiEvent;
use tutti_midi_types::{MidiChannel, MidiGroup};
use tutti_polysynth::{PolySynth, SynthConfig};

const SAMPLE_RATE: f64 = 48_000.0;
/// At 120 BPM and 48 kHz.
const FRAMES_PER_BEAT: f64 = 24_000.0;

/// A note-on/note-off pair as timed events, which is what an install holds.
///
/// Local to this test rather than a library constructor: a note record with a
/// duration is authoring vocabulary, and MIDI 2.0 defines none — the wire has
/// only the two events this builds.
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
        // position plus a span is a position.
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
    // At the rate `roll` advances the transport for: a transport that moves
    // at another rate's pace reads as a seek every block.
    let mut graph = AudioGraphRes::headless(0, 2);
    graph.set_sample_rate(SampleRate(SAMPLE_RATE));
    app.insert_resource(graph);
    app.insert_resource(TransportRes(Transport::new(SAMPLE_RATE)));
    app.insert_resource(AudioConfig {
        sample_rate: SampleRate(SAMPLE_RATE),
        channels: tutti_core::ChannelLayout::STEREO,
    });
    app.insert_resource(AudioEngineState::Running);
    app.insert_resource(bevy_tutti::midi::test_support::clock_master_for_test());
    // `TuttiMidiPlugin` registers the `MidiFileAsset` loader at build time,
    // which panics without an `AssetServer` — a headless app supplies it.
    app.add_plugins((
        bevy_app::TaskPoolPlugin::default(),
        bevy_asset::AssetPlugin::default(),
    ));
    app.add_plugins((GraphReconcilePlugin, TuttiMidiPlugin));
    app
}

/// `(absolute frame, event)` of every MIDI event a [`Tap`] received.
type Seen = Arc<Mutex<Vec<(u64, MidiEvent)>>>;

/// A stand-in synth: one MIDI event input, logged with each event's
/// absolute frame.
struct Tap(Seen);

impl Node for Tap {
    fn shape(&self) -> Shape {
        Shape::audio(
            tutti_core::ChannelLayout::EMPTY,
            tutti_core::ChannelLayout::EMPTY,
        )
        .with_events(1, 0)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, io: Io<'_>) -> Status {
        let mut seen = self.0.lock().unwrap();
        for e in io.events(0) {
            if let EventKind::Midi(Ump(data)) = e.kind {
                let at = cx.env.frame.get() + u64::from(e.offset.get());
                seen.push((
                    at,
                    MidiEvent {
                        frame_offset: e.offset.get(),
                        data,
                    },
                ));
            }
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

impl IntoNode for Tap {
    type Controls = ();
    fn into_parts(self) -> NodeParts<()> {
        IntoNode::into_parts(Unforkable(self))
    }
}

impl GraphNode for Tap {}

/// A tap entity, inserted as a graph node, and what it hears.
fn spawn_tap(app: &mut App) -> (Entity, Seen) {
    let seen = Seen::default();
    let entity = app
        .world_mut()
        .commands()
        .spawn_graph_node(Tap(Arc::clone(&seen)))
        .id();
    app.world_mut().flush();
    (entity, seen)
}

/// Render frames `from..to` one at a time, rolling at 120 BPM from beat 0 at
/// frame 0 (the local render's executor counts the frames it rendered, so
/// `from` must continue where the last render stopped).
fn roll(app: &mut App, from: u64, to: u64) {
    let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
    for f in from..to {
        let t =
            tutti_graph::Transport::new(true, Bpm(120.0), Beat(f as f64 / FRAMES_PER_BEAT), None);
        let mut frame = [0.0f32; 2];
        graph.render_frame_at(&t, &mut frame);
    }
}

fn note_ons(seen: &Seen) -> Vec<(u64, u8)> {
    seen.lock()
        .unwrap()
        .iter()
        .filter(|(_, e)| e.is_note_on())
        .map(|(f, e)| (*f, e.note().unwrap()))
        .collect()
}

fn note_offs(seen: &Seen) -> Vec<(u64, u8)> {
    seen.lock()
        .unwrap()
        .iter()
        .filter(|(_, e)| e.is_note_off())
        .map(|(f, e)| (*f, e.note().unwrap()))
        .collect()
}

/// **A note lands on its frame.** A third of a beat in at 120 BPM / 48 kHz
/// is frame 8 000, inside a block rather than on its boundary.
///
/// Mutation (run): the event wiring never writing (`graph::events::reconcile`
/// skipping `set_event_sources`) → nothing arrives → fails.
#[test]
fn a_scheduled_note_lands_on_its_frame() {
    let mut app = app();
    let (tap, seen) = spawn_tap(&mut app);
    app.world_mut().spawn(MidiSourceInstall::new(
        tap,
        note(60, Beat(1.0 / 3.0), BeatDuration(1.0), MF).to_vec(),
    ));
    app.update();
    roll(&mut app, 0, 9_000);
    assert_eq!(note_ons(&seen), vec![(8_000, 60)]);
}

/// **Two installs naming one target both play**: they are merged into its
/// one clip.
///
/// Mutation (run): the rebuild keeping each install's events apart and
/// setting them in turn (the last wins) → one note → fails.
#[test]
fn two_installs_on_one_target_both_play() {
    let mut app = app();
    let (tap, seen) = spawn_tap(&mut app);
    app.world_mut().spawn(MidiSourceInstall::new(
        tap,
        note(60, Beat(0.0), BeatDuration(1.0), MF).to_vec(),
    ));
    app.world_mut().spawn(MidiSourceInstall::new(
        tap,
        note(67, Beat(0.0), BeatDuration(1.0), MF).to_vec(),
    ));
    app.update();
    roll(&mut app, 0, 64);
    let mut notes: Vec<u8> = note_ons(&seen).into_iter().map(|(_, n)| n).collect();
    notes.sort_unstable();
    assert_eq!(notes, vec![60, 67]);
}

/// **Editing an install mid-note ends the note it left sounding**: the clip
/// node's new events replace the old, and the note the old ones held is
/// released on the edit's first block.
///
/// Mutation (run): the rebuild inserting a fresh clip node per edit and
/// removing the old one → the old node sends nothing more → no note-off →
/// fails.
#[test]
fn an_edit_does_not_hang_the_previous_note() {
    let mut app = app();
    let (tap, seen) = spawn_tap(&mut app);
    let install = app
        .world_mut()
        .spawn(MidiSourceInstall::new(
            tap,
            // A long note, so the edit lands between its on and off.
            note(60, Beat(0.0), BeatDuration(32.0), MF).to_vec(),
        ))
        .id();
    app.update();
    roll(&mut app, 0, 100);
    app.world_mut()
        .entity_mut(install)
        .insert(MidiSourceInstall::new(
            tap,
            note(64, Beat(8.0), BeatDuration(1.0), MF).to_vec(),
        ));
    app.update();
    roll(&mut app, 100, 200);
    assert_eq!(
        note_offs(&seen),
        vec![(100, 60)],
        "{:?}",
        seen.lock().unwrap()
    );
}

/// **Removing the last install naming a target ends its notes and plays
/// nothing more**, rather than looping the old clip forever.
///
/// Mutation (run): the rebuild not clearing a clip no install names → the
/// second note plays at beat 1 → fails.
#[test]
fn removing_the_last_install_ends_its_notes() {
    let mut app = app();
    let (tap, seen) = spawn_tap(&mut app);
    let mut events = note(60, Beat(0.0), BeatDuration(32.0), MF).to_vec();
    events.extend(note(62, Beat(1.0), BeatDuration(1.0), MF));
    let install = app
        .world_mut()
        .spawn(MidiSourceInstall::new(tap, events))
        .id();
    app.update();
    roll(&mut app, 0, 100);
    app.world_mut().despawn(install);
    app.update();
    roll(&mut app, 100, 30_000);
    assert_eq!(note_ons(&seen), vec![(0, 60)]);
    assert_eq!(note_offs(&seen), vec![(100, 60)]);
}

/// **Velocity keeps its full 16-bit range end to end.** Two velocities one
/// LSB apart at 16 bits are the same number at 7, so this fails the moment
/// anything on the install → clip node → edge path narrows the field (it
/// caught exactly that once, on the path before this one: a `note_on_7bit`
/// that crushed the value and widened it back).
///
/// Mutation (run): the clip node sending `MidiEvent::note_on_7bit` of the
/// event's note and velocity >> 9 → fails.
#[test]
fn velocity_keeps_its_full_width() {
    let mut app = app();
    let (tap, seen) = spawn_tap(&mut app);
    let mut events = note(60, Beat(0.0), BeatDuration(1.0), MF).to_vec();
    events.extend(note(64, Beat(0.0), BeatDuration(1.0), MF + 1));
    app.world_mut().spawn(MidiSourceInstall::new(tap, events));
    app.update();
    roll(&mut app, 0, 64);
    let mut velocities: Vec<u16> = seen
        .lock()
        .unwrap()
        .iter()
        .map(|(_, e)| e.message())
        .filter_map(|m| match m {
            tutti_midi_types::MidiMessage::NoteOn { velocity, .. } => Some(velocity),
            _ => None,
        })
        .collect();
    velocities.sort_unstable();
    assert_eq!(velocities, vec![MF, MF + 1]);
}

/// A synth inserted as a graph node, with a MIDI event input.
fn spawn_graph_synth(app: &mut App) -> Entity {
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
    use bevy_tutti::graph::{EventSources, NodeControls};
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
    app.world_mut()
        .entity_mut(synth)
        .insert(EventSources::from(clip));
    app.update();
    let graph = app.world().resource::<AudioGraphRes>();
    assert_eq!(graph.node_event_inputs(node_of(&app, synth)), 1);
    assert_eq!(
        graph.event_sources(node_of(&app, synth), 0),
        vec![node_of(&app, clip).into()]
    );

    app.world_mut().entity_mut(synth).remove::<EventSources>();
    app.update();
    let graph = app.world().resource::<AudioGraphRes>();
    assert!(graph.event_sources(node_of(&app, synth), 0).is_empty());
}

/// **An install on a synth plays through a clip node of its own, wired to
/// its event input, edited in place, and kept (emptied) when the last
/// install goes**, so the notes it left sounding end on its frames
/// (`removing_the_last_install_ends_its_notes`).
///
/// Mutation: a fresh clip node per edit → the node changes across the edit →
/// fails. Mutation: removing the clip node with the last install → it is
/// gone from the graph → fails.
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
        vec![clip.into()]
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
    assert_eq!(
        app.world().resource::<SequencedClips>().node(synth),
        Some(clip),
        "the emptied clip stays, to end its notes"
    );
    assert!(app.world().resource::<AudioGraphRes>().contains(clip));
}

/// **A clip node goes when its target leaves the event path**: here the
/// target loses its node, and the install stays. The clip node is removed
/// rather than left playing into nothing.
///
/// Mutation: drop only targets no install names (not those off the event
/// path) → the clip node stays in the graph → fails.
#[test]
fn a_clip_node_goes_when_its_target_loses_its_node() {
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
        .expect("a clip node");
    app.world_mut()
        .entity_mut(synth)
        .remove::<tutti_core::AudioNode>();
    // A rebuild runs when an install changes: write it back unchanged.
    let mut changed = app
        .world_mut()
        .get_mut::<MidiSourceInstall>(install)
        .unwrap();
    changed.events = changed.events.clone();
    app.update();
    assert_eq!(app.world().resource::<SequencedClips>().node(synth), None);
    assert!(!app.world().resource::<AudioGraphRes>().contains(clip));
}
