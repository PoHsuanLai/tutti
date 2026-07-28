//! The whole chain, rendered: an ECS declaration produces audible samples.
//!
//! Every other MIDI test asserts on the *plumbing* — a sender on the bus, an
//! event at a port. This one renders a real `SoundFontUnit` through the graph
//! and measures the output, so a break anywhere between `MidiSourceInstall` and
//! a moving speaker cone fails it. It replaces the "run the example and listen"
//! step, which no example implemented.
//!
//! Skipped when the soundfont is absent, following the pattern in
//! `synth/soundfont.rs`'s own tests — the asset is in the repo but a consumer
//! checking out this crate alone may not have it.

#![cfg(all(feature = "midi", feature = "soundfont"))]

use std::path::PathBuf;
use std::sync::Arc;

use bevy_app::prelude::*;
use bevy_ecs::entity::Entity;

use bevy_tutti::graph::{AudioConfig, AudioGraphRes, GraphReconcilePlugin, TransportRes};
use bevy_tutti::midi::{MidiNote, MidiSourceInstall, MidiTargetRegistry, TuttiMidiPlugin};
use bevy_tutti::AudioEngineState;
use tutti_core::dsp::{AudioUnit, Net};
use tutti_core::transport::Transport;
use tutti_core::AudioNode;
use tutti_synth::{SoundFont, SoundFontUnit, SynthesizerSettings};

const SAMPLE_RATE: f64 = 48_000.0;

/// The repo's test soundfont, or `None` on a checkout without it.
fn soundfont() -> Option<Arc<SoundFont>> {
    let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()? // crates/
        .parent()? // repo root
        .join("crates/tutti/assets/soundfonts/TimGM6mb.sf2");
    let mut file = std::fs::File::open(&path).ok()?;
    SoundFont::new(&mut file).ok().map(Arc::new)
}

/// RMS of a rendered stereo block — how loud it actually is.
fn rms(samples: &[(f32, f32)]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_sq: f32 = samples.iter().map(|(l, r)| l * l + r * r).sum();
    (sum_sq / (samples.len() * 2) as f32).sqrt()
}

/// Render `frames` from the graph.
///
/// Per-sample `tick` rather than a `process` block: `BufferVec` holds one SIMD
/// block per channel, capping `process` at 64 frames, and these tests need
/// quarter-second spans. The clip source is polled by the unit either way.
fn render(app: &mut App, frames: usize) -> Vec<(f32, f32)> {
    let mut graph = app.world_mut().resource_mut::<AudioGraphRes>();
    let mut out = Vec::with_capacity(frames);
    for _ in 0..frames {
        let mut frame = [0.0f32; 2];
        graph.0.tick(&[], &mut frame);
        out.push((frame[0], frame[1]));
    }
    out
}

/// Set up an app with a real soundfont synth wired to output.
fn app_with_soundfont() -> Option<(App, Entity)> {
    let sf = soundfont()?;
    let mut settings = SynthesizerSettings::new(SAMPLE_RATE as i32);
    settings.enable_reverb_and_chorus = false;
    let unit = SoundFontUnit::new(sf, &settings).ok()?;

    let mut app = App::new();
    let mut net = Net::new(0, 2);
    let node = net.push(Box::new(unit));
    net.pipe_output(node);
    // A backend, because the Commit-phase `commit_graph` asserts one exists.
    // We render the frontend `Net` directly rather than through the backend —
    // `tick` on this side sees the same units.
    let _backend = net.backend();

    app.insert_resource(AudioGraphRes(net));
    app.insert_resource(TransportRes(Transport::new(SAMPLE_RATE)));
    app.insert_resource(AudioConfig {
        sample_rate: SAMPLE_RATE,
        channels: Default::default(),
    });
    app.insert_resource(AudioEngineState::Running);
    app.insert_resource(bevy_tutti::midi::test_support::midi_bus_for_test());
    app.insert_resource(bevy_tutti::midi::test_support::clock_master_for_test(
        SAMPLE_RATE,
    ));
    app.add_plugins((GraphReconcilePlugin, TuttiMidiPlugin));
    app.world_mut()
        .resource_mut::<MidiTargetRegistry>()
        .register::<SoundFontUnit>();

    let synth = app.world_mut().spawn(AudioNode(node)).id();
    Some((app, synth))
}

fn roll(app: &App) {
    let transport = app.world().resource::<TransportRes>().clone();
    let _ = transport
        .motion
        .try_send(tutti_core::transport::MotionEvent::Play);
    transport.motion.drain();
}

/// A note declared in the ECS makes sound.
///
/// The end-to-end claim: `MidiSourceInstall` → `rebuild` → resolved port →
/// installed clip → engine beat→offset → rustysynth → samples. Silence here
/// means a break anywhere along it.
#[test]
fn a_declared_note_produces_audio() {
    let Some((mut app, synth)) = app_with_soundfont() else {
        eprintln!("skipping: TimGM6mb.sf2 not present");
        return;
    };
    roll(&app);

    app.world_mut().spawn(MidiSourceInstall::from_notes(
        synth,
        &[MidiNote::new(60.0, 0.0, 2.0).with_velocity(1.0)],
    ));
    app.update();

    // Half a second at 120 BPM covers the note-on comfortably.
    let samples = render(&mut app, 24_000);
    let level = rms(&samples);
    assert!(
        level > 1e-4,
        "an ECS-declared note must reach the speakers, got RMS {level}"
    );
}

/// Silence before the note, sound after — the scheduling is real, not a note
/// that fires the instant the clip is installed.
///
/// Without this, "it made noise" would pass equally for a source that ignored
/// the beat entirely, which is what the old path effectively did.
///
/// The beat is set by hand: it is normally advanced by a `TransportClock` node
/// the engine adds to the graph, and this test builds a minimal graph holding
/// only the synth. Setting it is what the audio thread would have done.
#[test]
fn the_note_waits_for_its_beat() {
    let Some((mut app, synth)) = app_with_soundfont() else {
        eprintln!("skipping: TimGM6mb.sf2 not present");
        return;
    };
    roll(&app);

    app.world_mut().spawn(MidiSourceInstall::from_notes(
        synth,
        &[MidiNote::new(60.0, 4.0, 4.0).with_velocity(1.0)],
    ));
    app.update();

    // Still well before beat 4.
    let before = rms(&render(&mut app, 4_000));

    // Advance onto the note and render again.
    let transport = app.world().resource::<TransportRes>().clone();
    transport.settings.set_beat(tutti_core::Beat(4.0));
    let after = rms(&render(&mut app, 12_000));

    assert!(
        before < 1e-5,
        "nothing should sound before the note's beat, got RMS {before}"
    );
    assert!(
        after > 1e-4,
        "the note should sound once its beat arrives: {before} then {after}"
    );
}

/// Live preview still reaches a synth that has a clip installed.
///
/// This is what commit `01ad5b006` bought — `MidiInPort::poll` layers the
/// installed source over the mailbox rather than replacing it. Before that, a
/// synth playing a clip went deaf to the keyboard, and the pushed events sat in
/// the mailbox and popped out stale on the next `clear()`.
#[test]
fn preview_still_sounds_under_an_installed_clip() {
    let Some((mut app, synth)) = app_with_soundfont() else {
        eprintln!("skipping: TimGM6mb.sf2 not present");
        return;
    };
    roll(&app);

    // A clip whose first note is far in the future, so anything audible in the
    // next quarter-second can only be the preview.
    app.world_mut().spawn(MidiSourceInstall::from_notes(
        synth,
        &[MidiNote::new(60.0, 100.0, 1.0)],
    ));
    app.update();

    let quiet = rms(&render(&mut app, 6_000));
    assert!(quiet < 1e-5, "the clip's note is far away; expected silence");

    // Push a live note straight at the synth's mailbox, as a keyboard would.
    {
        let node = app.world().get::<AudioNode>(synth).unwrap().0;
        let graph = app.world().resource::<AudioGraphRes>();
        let unit = graph.0.node_as::<SoundFontUnit>(node).unwrap();
        unit.midi_port().sender().queue(&[
            tutti_midi_types::ump::MidiEvent::note_on(0, 0, 67, 0xFFFF),
        ]);
    }

    let previewed = rms(&render(&mut app, 12_000));
    assert!(
        previewed > 1e-4,
        "live preview must still sound while a clip is installed, got RMS {previewed}"
    );
}
