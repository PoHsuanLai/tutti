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

use bevy_tutti::AudioEngineState;
use bevy_tutti::graph::{
    AudioConfig, AudioGraphRes, GraphReconcilePlugin, SessionNode, SpawnAudioNode, TransportRes,
};
use bevy_tutti::midi::{MidiSourceInstall, TuttiMidiPlugin};
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
    graph.graph().set_sample_rate(SampleRate(SAMPLE_RATE));
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

impl SessionNode for Tap {}

/// A tap entity, inserted as a graph node, and what it hears.
fn spawn_tap(app: &mut App) -> (Entity, Seen) {
    let seen = Seen::default();
    let entity = app
        .world_mut()
        .commands()
        .spawn_audio_node(Tap(Arc::clone(&seen)))
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

/// **A target re-bound to a new node keeps its clip**: the feed is keyed by
/// the node (the session's feeds), and the event reconcile moves every
/// feed of the entity's old node onto its new one, so the clip plays the new
/// node and not the one the entity left.
///
/// Mutation (run): `graph::events::reconcile` not calling `move_feeds` when
/// an entity's node changes → the clip still feeds the old node, the new tap
/// hears nothing → fails.
#[test]
fn a_re_bound_target_keeps_its_clip() {
    use bevy_tutti::graph::InsertAudioNode;

    let mut app = app();
    let (tap, old) = spawn_tap(&mut app);
    app.world_mut().spawn(MidiSourceInstall::new(
        tap,
        note(60, Beat(1.0 / 3.0), BeatDuration(1.0), MF).to_vec(),
    ));
    app.update();

    let new = Seen::default();
    app.world_mut()
        .commands()
        .entity(tap)
        .insert_audio_node(Tap(Arc::clone(&new)));
    app.world_mut().flush();
    app.update();

    roll(&mut app, 0, 9_000);
    assert_eq!(note_ons(&new), vec![(8_000, 60)], "the new node plays it");
    assert!(
        note_ons(&old).is_empty(),
        "the node the entity left does not"
    );
}

/// **A target whose node is removed, then bound to a new one, keeps its
/// clip.** Removing `AudioNode` removes the old node from the graph, and
/// every feed keyed by it goes with it (`Graph::remove`), so by the time the
/// new node is bound there is nothing for the event reconcile to move. The
/// sequencer re-aims the clip at the target's node on every rebuild
/// (`tutti_session::Midi::retarget`), so the same clip reaches the new node
/// anyway.
///
/// Mutations (run), each fails: the rebuild not re-aiming
/// its clips (`retarget` skipped: `play_clip` finds no clip fed into the
/// new node and makes a second, leaving the first unfed in the graph → "the
/// same clip"); the session's `play_clip` making a fresh clip on every call
/// (→ "the same clip"); the rebuild not reaching a rebound target (its
/// `Changed<AudioNode>` ignored) → the clip is never re-aimed, the new tap
/// hears nothing. Before the move into the session (run): the
/// rebuild setting the feed only when it created a clip → the new tap
/// heard `[]`.
#[test]
fn a_target_removed_then_re_bound_keeps_its_clip() {
    use bevy_tutti::graph::InsertAudioNode;
    use bevy_tutti::midi::SequencedClips;
    use tutti_core::AudioNode;

    let mut app = app();
    let (tap, old) = spawn_tap(&mut app);
    app.world_mut().spawn(MidiSourceInstall::new(
        tap,
        note(60, Beat(1.0 / 3.0), BeatDuration(1.0), MF).to_vec(),
    ));
    app.update();
    let clip = app.world().resource::<SequencedClips>().node(tap);
    assert!(clip.is_some(), "precondition: the target has a clip");

    app.world_mut().entity_mut(tap).remove::<AudioNode>();
    app.world_mut().flush();
    let new = Seen::default();
    app.world_mut()
        .commands()
        .entity(tap)
        .insert_audio_node(Tap(Arc::clone(&new)));
    app.world_mut().flush();
    app.update();

    roll(&mut app, 0, 9_000);
    assert_eq!(note_ons(&new), vec![(8_000, 60)], "the new node plays it");
    assert!(
        note_ons(&old).is_empty(),
        "the node the entity left does not"
    );
    assert_eq!(
        app.world().resource::<SequencedClips>().node(tap),
        clip,
        "the same clip"
    );
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
/// Mutation (run, against `tutti_session::Midi::play_clip`): a fresh clip node
/// per edit (`play_clip` never finding the one it
/// has) → the old node sends nothing more → no note-off → fails.
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
/// Mutations (run), each fails: the rebuild not clearing a clip no install
/// names (`Midi::stop_clip` skipped) → the second note plays at beat 1; the
/// rebuild not reaching a removed install's target (its prune of installs
/// it saw marking nothing) → nothing is rebuilt, same.
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
    let entity = app.world_mut().commands().spawn_audio_node(synth).id();
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
/// (`spawn_audio_node`); the declaration on the synth reaches the graph's
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
        .spawn_audio_node(MidiClipNode::new(note(
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
    assert_eq!(graph.graph_ref().node_event_inputs(node_of(&app, synth)), 1);
    assert_eq!(
        graph.graph_ref().event_sources(node_of(&app, synth), 0),
        vec![node_of(&app, clip).into()]
    );

    app.world_mut().entity_mut(synth).remove::<EventSources>();
    app.update();
    let graph = app.world().resource::<AudioGraphRes>();
    assert!(
        graph
            .graph_ref()
            .event_sources(node_of(&app, synth), 0)
            .is_empty()
    );
}

/// **An install on a synth plays through a clip node of its own, wired to
/// its event input, edited in place, and kept (emptied) when the last
/// install goes**, so the notes it left sounding end on its frames
/// (`removing_the_last_install_ends_its_notes`).
///
/// Mutations (run): a fresh clip node per edit (the
/// session's `play_clip` never finding the one it has) → the node changes
/// across the edit → fails. Removing the clip node with the last install
/// (the rebuild's `gone` taking targets no install names) → it is
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
            .graph_ref()
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
    assert!(
        app.world()
            .resource::<AudioGraphRes>()
            .graph_ref()
            .contains(clip)
    );
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
    assert!(
        !app.world()
            .resource::<AudioGraphRes>()
            .graph_ref()
            .contains(clip)
    );
}

/// **An install moved to another target leaves the old one silent**: the
/// target it named before the edit is rebuilt too, so its clip stops (its
/// held note ends on the edit's block) and the new target plays.
///
/// Mutation (run): the rebuild not reaching an install's previous target
/// (the `installs.insert` return dropped) → the old tap's clip keeps
/// playing, its second note sounds at frame 24 000 and the held one never
/// ends → fails.
#[test]
fn an_install_moved_to_another_target_silences_the_old_one() {
    let mut app = app();
    let (a, heard_a) = spawn_tap(&mut app);
    let (b, heard_b) = spawn_tap(&mut app);
    let mut events = note(60, Beat(0.0), BeatDuration(32.0), MF).to_vec();
    events.extend(note(62, Beat(1.0), BeatDuration(1.0), MF));
    let install = app
        .world_mut()
        .spawn(MidiSourceInstall::new(a, events))
        .id();
    app.update();
    roll(&mut app, 0, 100);
    app.world_mut()
        .entity_mut(install)
        .insert(MidiSourceInstall::new(
            b,
            note(64, Beat(1.0), BeatDuration(1.0), MF).to_vec(),
        ));
    app.update();
    roll(&mut app, 100, 30_000);
    assert_eq!(note_ons(&heard_a), vec![(0, 60)], "the old target stops");
    assert_eq!(note_offs(&heard_a), vec![(100, 60)], "its held note ends");
    assert_eq!(note_ons(&heard_b), vec![(24_000, 64)], "the new one plays");
}

/// **A clip goes when its target loses its node with no install edit**:
/// nothing about the install changes, so the rebuild has to find the
/// target itself (a clip whose target has no node).
///
/// Mutation (run): drop the reach of clip targets with no node → nothing
/// is reached, the clip node stays in the graph → fails.
#[test]
fn a_clip_goes_when_its_target_loses_its_node_with_no_install_edit() {
    use bevy_tutti::midi::SequencedClips;

    let mut app = app();
    let synth = spawn_graph_synth(&mut app);
    app.world_mut().spawn(MidiSourceInstall::new(
        synth,
        note(60, Beat(0.0), BeatDuration(1.0), MF).to_vec(),
    ));
    app.update();
    let clip = app
        .world()
        .resource::<SequencedClips>()
        .node(synth)
        .expect("a clip node");
    app.world_mut()
        .entity_mut(synth)
        .remove::<tutti_core::AudioNode>();
    app.update();
    assert_eq!(app.world().resource::<SequencedClips>().node(synth), None);
    assert!(
        !app.world()
            .resource::<AudioGraphRes>()
            .graph_ref()
            .contains(clip)
    );
}

/// **An install added while there is no graph plays once the graph
/// appears.** `engine_ready` can be true before an `AudioGraphRes` exists;
/// the edit is consumed by that run, and the target must still be rebuilt
/// when the graph arrives.
///
/// Mutation (run): `rebuild` returning at the missing graph without keeping
/// the reached targets (`clips.retry = reached` removed) → nothing plays →
/// fails.
#[test]
fn an_install_added_with_no_graph_plays_when_the_graph_appears() {
    let mut app = app();
    let (tap, seen) = spawn_tap(&mut app);
    app.update();
    let graph = app
        .world_mut()
        .remove_resource::<AudioGraphRes>()
        .expect("a graph");
    app.world_mut().spawn(MidiSourceInstall::new(
        tap,
        note(60, Beat(0.0), BeatDuration(1.0), MF).to_vec(),
    ));
    app.update();
    app.world_mut().insert_resource(graph);
    app.update();
    roll(&mut app, 0, 100);
    assert_eq!(note_ons(&seen), vec![(0, 60)]);
}

/// **An install removed while there is no graph ends its notes once the
/// graph appears.** The install is gone from the world and from the
/// sequencer's record by then; only the kept target tells the rebuild to
/// silence its clip.
///
/// Mutation (run): the same as above (`clips.retry = reached` removed) →
/// the clip keeps its second note and plays it at beat 1 → fails.
#[test]
fn an_install_removed_with_no_graph_ends_its_notes_when_the_graph_appears() {
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
    let graph = app
        .world_mut()
        .remove_resource::<AudioGraphRes>()
        .expect("a graph");
    app.world_mut().despawn(install);
    app.update();
    app.world_mut().insert_resource(graph);
    app.update();
    roll(&mut app, 100, 30_000);
    assert_eq!(note_ons(&seen), vec![(0, 60)]);
    assert_eq!(note_offs(&seen), vec![(100, 60)]);
}

/// **A clip node removed behind the sequencer's back is forgotten.**
/// `retarget` on it fails with `NotSessionMidi`; the stale entry goes, so the
/// target is not retried for good, and an edit that leaves installs plays a
/// fresh clip.
///
/// Mutation (run): the `NotSessionMidi` arm left out of `rebuild` (the error
/// takes the retry path) → `node` still names the dead clip → fails on the
/// first assertion.
#[test]
fn a_clip_removed_behind_its_back_is_forgotten_and_replaced() {
    use bevy_tutti::midi::SequencedClips;

    let mut app = app();
    let (tap, seen) = spawn_tap(&mut app);
    let first = app
        .world_mut()
        .spawn(MidiSourceInstall::new(
            tap,
            note(60, Beat(0.0), BeatDuration(1.0), MF).to_vec(),
        ))
        .id();
    app.update();
    let dead = app
        .world()
        .resource::<SequencedClips>()
        .node(tap)
        .expect("a clip node");
    app.world_mut().resource_mut::<AudioGraphRes>().remove(dead);
    // The last install goes: nothing is left to play, the dead clip is dropped.
    app.world_mut().despawn(first);
    app.update();
    assert_eq!(app.world().resource::<SequencedClips>().node(tap), None);

    // A new install gets a fresh clip that plays.
    app.world_mut().spawn(MidiSourceInstall::new(
        tap,
        note(64, Beat(0.0), BeatDuration(1.0), MF).to_vec(),
    ));
    app.update();
    let fresh = app
        .world()
        .resource::<SequencedClips>()
        .node(tap)
        .expect("a fresh clip");
    assert_ne!(fresh, dead);
    roll(&mut app, 0, 100);
    assert_eq!(note_ons(&seen), vec![(0, 64)]);
}

// A `play_clip` or `retarget` that fails is kept in the same retry set and
// tried again, logged once. Not covered here: `rebuild` only asks a target
// with an event input, and `play_clip` fails only for one without, so no
// session state reachable from a test makes it fail once and then succeed.
