//! A held key and a declared rewire (`EventSources`, the event twin of
//! `PortSources`): moving the declaration from one sink to another while a key
//! is held ends the note on the sink that loses the feed. The new sink hears
//! the later off of a note it never started, which ends nothing.

#![cfg(all(feature = "midi", feature = "synth"))]

use std::sync::{Arc, Mutex};

use bevy_app::prelude::*;
use bevy_ecs::entity::Entity;

use bevy_tutti::AudioEngineState;
use bevy_tutti::graph::{
    AudioConfig, AudioGraphRes, EventSources, GraphReconcilePlugin, SessionNode, SpawnAudioNode,
    TransportRes,
};
use tutti_core::transport::Transport;
use tutti_graph::{
    Cx, Event, EventKind, IntoNode, Io, Node, NodeParts, Offset, Prepare, Shape, Status, Unforkable,
};
use tutti_types::{Beat, Bpm, ChannelLayout, SampleRate};
use tutti_ump::Ump;

const SAMPLE_RATE: f64 = 48_000.0;
type Seen = Arc<Mutex<Vec<[u32; 4]>>>;
type Mail = Arc<Mutex<Vec<[u32; 4]>>>;

struct Tap(Seen);
impl Node for Tap {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::EMPTY).with_events(1, 0)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, io: Io<'_>) -> Status {
        for e in io.events(0) {
            if let EventKind::Midi(Ump(w)) = e.kind {
                self.0.lock().unwrap().push(w);
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

struct Src(Mail);
impl Node for Src {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::EMPTY).with_events(0, 1)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        for w in self.0.lock().unwrap().drain(..) {
            let _ = io.event_out(0).push(Event::midi(Offset::ZERO, w));
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}
impl IntoNode for Src {
    type Controls = ();
    fn into_parts(self) -> NodeParts<()> {
        IntoNode::into_parts(Unforkable(self))
    }
}
impl SessionNode for Src {}

fn app() -> App {
    let mut app = App::new();
    let mut graph = AudioGraphRes::headless(0, 2);
    graph.graph().set_sample_rate(SampleRate(SAMPLE_RATE));
    app.insert_resource(graph);
    app.insert_resource(TransportRes(Transport::new(SAMPLE_RATE)));
    app.insert_resource(AudioConfig {
        sample_rate: SampleRate(SAMPLE_RATE),
        channels: ChannelLayout::STEREO,
    });
    app.insert_resource(AudioEngineState::Running);
    app.add_plugins(GraphReconcilePlugin);
    app
}

fn spawn_src(app: &mut App, mail: Mail) -> Entity {
    let e = app.world_mut().commands().spawn_audio_node(Src(mail)).id();
    app.world_mut().flush();
    e
}

fn spawn_tap(app: &mut App, seen: Seen) -> Entity {
    let e = app.world_mut().commands().spawn_audio_node(Tap(seen)).id();
    app.world_mut().flush();
    e
}

fn roll(app: &mut App, frames: u64) {
    let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
    for f in 0..frames {
        let t = tutti_graph::Transport::new(true, Bpm(120.0), Beat(f as f64 / 24_000.0), None);
        let mut frame = [0.0f32; 2];
        graph.render_frame_at(&t, &mut frame);
    }
}

fn count(seen: &Seen, off: bool) -> usize {
    seen.lock()
        .unwrap()
        .iter()
        .filter(|w| Ump(**w).is_note_off() == off)
        .count()
}

#[test]
fn moving_an_event_declaration_with_a_held_key() {
    let mut app = app();
    let mail: Mail = Arc::default();
    let src = spawn_src(&mut app, Arc::clone(&mail));
    let (a_seen, b_seen) = (Seen::default(), Seen::default());
    let a = spawn_tap(&mut app, Arc::clone(&a_seen));
    let b = spawn_tap(&mut app, Arc::clone(&b_seen));

    app.world_mut()
        .entity_mut(a)
        .insert(EventSources::from(src));
    app.update();
    mail.lock().unwrap().push([0x2090_3c64, 0, 0, 0]);
    roll(&mut app, 4);
    assert_eq!(count(&a_seen, false), 1);

    app.world_mut().entity_mut(a).remove::<EventSources>();
    app.world_mut()
        .entity_mut(b)
        .insert(EventSources::from(src));
    app.update();
    roll(&mut app, 4);
    assert_eq!(count(&a_seen, true), 1, "A is told the note ended");

    mail.lock().unwrap().push([0x2080_3c00, 0, 0, 0]);
    roll(&mut app, 4);
    assert_eq!(count(&b_seen, true), 1, "B hears the orphan off");
    assert_eq!(count(&a_seen, true), 1, "A is told once");

    // A key held across the declaration's removal.
    mail.lock().unwrap().push([0x2090_3d64, 0, 0, 0]);
    roll(&mut app, 4);
    assert_eq!(count(&b_seen, false), 1);
    app.world_mut().entity_mut(b).remove::<EventSources>();
    app.update();
    roll(&mut app, 4);
    assert_eq!(
        count(&b_seen, true),
        2,
        "B is told when the declaration goes"
    );
}
