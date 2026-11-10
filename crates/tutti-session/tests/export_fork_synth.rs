//! Built-in synths (`PolySynth` and `SoundFontUnit`) inserted as graph nodes
//! and exported through a fork, playing the clip `Session::midi().play_clip`
//! wires to them: a clip node, which the export forks with the synth and
//! which reads the render's `Env`. The session halves of what `bevy-tutti`'s
//! export suite pinned before that crate was parked.
//!
//! Every tempo here is 90 BPM: a beat is 32 000 frames at 48 kHz (64 000 at
//! 96 kHz), placed to the frame since clips place on integer frames.

use std::sync::Arc;

use tutti_export::{ChannelLayout, EncodeConfig, ExportConfig, Format, Pcm, RenderConfig};
use tutti_fixtures::midi::{note_off, note_on};
use tutti_graph::NodeKey;
use tutti_graph::contract::Direct;
use tutti_graph::{Event, Node, Offset};
use tutti_midi_runtime::TimedMidiEvent;
use tutti_nodes::testing::assert_golden;
use tutti_polysynth::{Adsr, OscillatorType, PolySynth, SynthConfig};
use tutti_session::{
    ExportClock, ExportOutput, ExportRequest, ExportSource, ExportTarget, Session,
};
use tutti_sf2::SoundFont;
use tutti_soundfont::{Settings, SoundFontUnit};
use tutti_transport::{OfflineTimeline, OfflineTimelineConfig};
use tutti_types::{Beat, Bpm, SampleRate, Samples, Seconds, UnitParam};
use tutti_ump::{MidiChannel, MidiGroup, U7, Ump};

const RATE: f64 = 48_000.0;
/// A beat at 90 BPM and 48 kHz, in frames.
const BEAT_48K: usize = 32_000;
/// Frames of the note compared against a reference render.
const NOTE: usize = 4_096;
/// A preset of the test soundfont that is not the default piano, so a
/// fork that lost the live unit's preset is heard.
const PRESET: U7 = U7::of(24);

fn config(seconds: f64) -> ExportConfig {
    ExportConfig {
        render: RenderConfig {
            sample_rate: SampleRate(RATE),
            duration_seconds: tutti_types::Seconds(seconds),
            ..Default::default()
        },
        encode: EncodeConfig {
            format: Format::Wav(Pcm::Float32),
            channels: ChannelLayout::STEREO,
            topology: None,
        },
        ..Default::default()
    }
}

/// FNV-1a over the planes' little-endian `f32` bits, plane after plane.
fn digest(planes: &[Vec<f32>]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in planes.iter().flatten().flat_map(|s| s.to_le_bytes()) {
        h ^= u64::from(byte);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

fn render(session: &mut Session, request: ExportRequest) -> Vec<Vec<f32>> {
    match session
        .export_blocking(request)
        .expect("the export renders")
        .finished()
    {
        Some(ExportOutput::Buffers(rendered)) => rendered.planes,
        other => panic!("expected rendered buffers, got {other:?}"),
    }
}

/// A saw with an instant attack, built at the live graph's rate.
fn poly() -> PolySynth {
    PolySynth::new(SynthConfig {
        sample_rate: SampleRate(RATE),
        oscillator: OscillatorType::Saw,
        envelope: Adsr {
            attack: Seconds(0.0),
            ..Default::default()
        },
        ..Default::default()
    })
    .expect("the synth builds")
}

/// The repo's committed test soundfont.
fn font() -> Arc<SoundFont> {
    let path = tutti_fixtures::asset("soundfonts/TimGM6mb.sf2");
    let mut file = std::fs::File::open(&path).unwrap_or_else(|e| {
        panic!(
            "committed test soundfont missing at {}: {e}",
            path.display()
        )
    });
    Arc::new(SoundFont::new(&mut file).expect("the test soundfont parses"))
}

/// A SoundFont unit at `rate`, on [`PRESET`].
fn sf2(font: &Arc<SoundFont>, rate: f64) -> SoundFontUnit {
    let settings = Settings::new(SampleRate(rate));
    let mut unit = SoundFontUnit::new(Arc::clone(font), &settings).expect("the unit builds");
    unit.program_change(MidiChannel::FIRST, PRESET);
    unit
}

/// A detached stereo session at the export's rate with `unit` inserted as a
/// graph node (the path a host takes), routed to the outputs. `insert`
/// takes a unit with no params of its own (`SoundFontUnit`); `add` takes
/// one a param write reaches (`PolySynth`).
fn session_with<N: tutti_graph::IntoNode>(unit: N) -> (Session, NodeKey) {
    let mut session = Session::builder()
        .inputs(0)
        .outputs_floor(2)
        .build_detached(SampleRate::DEFAULT)
        .expect("a detached session builds");
    session.graph().set_sample_rate(SampleRate(RATE));
    let (synth, _) = session.graph().insert(unit);
    session.graph().set_outputs_from(synth);
    session.update();
    (session, synth)
}

/// Middle C from beat 1 to beat 2.
fn clip() -> Vec<TimedMidiEvent> {
    vec![
        TimedMidiEvent::new(Beat(1.0), clip_note_on()),
        TimedMidiEvent::new(
            Beat(2.0),
            note_off(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0),
        ),
    ]
}

fn clip_note_on() -> Ump {
    note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0xFFFF)
}

fn install_clip(session: &mut Session, synth: NodeKey) {
    session
        .midi()
        .play_clip(synth, clip())
        .expect("the synth takes a clip");
    session.update();
}

/// A 90 BPM timeline from beat 0 at `rate`.
fn timeline(rate: f64) -> Arc<OfflineTimeline> {
    Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
        start_beat: Beat(0.0),
        tempo: Bpm::of(90.0),
        sample_rate: SampleRate(rate),
        loop_range: None,
    }))
}

/// Export `source` for 0.8 s at `rate` against [`timeline`].
fn request(source: ExportSource, rate: f64) -> ExportRequest {
    ExportRequest::new(
        source,
        ExportTarget::Buffers,
        ExportConfig {
            render: RenderConfig {
                sample_rate: SampleRate(rate),
                ..config(0.8).render
            },
            ..config(0.8)
        },
        ExportClock::timeline(timeline(rate)),
    )
}

/// `unit` prepared at `rate` and fed a note-on at frame 0 on its event
/// input, rendered in 64-frame blocks by hand (`contract::Direct`): what
/// the note sounds like from its first frame, at `rate`.
fn reference<N: Node>(unit: N, rate: f64) -> Vec<f32> {
    let mut unit = Direct::new(unit, SampleRate(rate), 64);
    let at = Offset::new(0, Samples(64)).expect("inside");
    unit.events(0, &[Event::midi(at, clip_note_on())]);
    let mut out = Vec::with_capacity(NOTE);
    while out.len() < NOTE {
        unit.block();
        out.extend_from_slice(unit.output(0));
    }
    out
}

/// Assert the render is silent until beat 1 and then, sample for sample,
/// `reference` — the note at the render's rate, from its first frame.
fn assert_note_at_beat_1(what: &str, rate: f64, planes: &[Vec<f32>], reference: &[f32]) {
    let beat = BEAT_48K * (rate / RATE) as usize;
    let lead = reference
        .iter()
        .position(|&s| s != 0.0)
        .unwrap_or_else(|| panic!("{what}: the reference note is silent"));
    let left = &planes[0];
    assert_eq!(
        left.iter().position(|&s| s != 0.0),
        Some(beat + lead),
        "{what} at {rate} Hz: the note enters on beat 1"
    );
    assert_eq!(
        &left[beat..beat + NOTE],
        reference,
        "{what} at {rate} Hz: from beat 1, the note at the render's rate"
    );
}

/// The live synth after an export plays its own clip on the **live**
/// transport: rolled and seated on beat 1, the live graph sounds the
/// note. A fork that shared the live clip or synth would have left them
/// where the render did, and the live graph would stay silent.
fn assert_live_untouched(session: &mut Session) {
    let mut heard = false;
    // Rolling at 120 BPM from beat 1.
    for i in 0..4_096 {
        let t = tutti_graph::Transport::new(
            true,
            Bpm::of(120.0),
            Beat(1.0 + f64::from(i) / (RATE / 2.0)),
            None,
        );
        let mut frame = [0.0f32; 2];
        session.render_frame_at(&t, &mut frame);
        heard |= frame[0] != 0.0;
    }
    assert!(heard, "the live synth plays its clip on the live transport");
}

/// **An exported `PolySynth` plays its clip**, on the render's timeline
/// and at the render's rate: a master export at 48 and 96 kHz is silent
/// until beat 1 and then, sample for sample, what the synth renders for
/// the note re-rated to the render's rate. The live synth keeps its clip.
///
/// Mutation (run, failed): `Midi::play_clip` not feeding the clip into the
/// synth's event input (`set_feed` left off) → the export is silent.
#[test]
fn an_exported_polysynth_plays_its_clip() {
    for rate in [RATE, 2.0 * RATE] {
        let (mut session, synth) = session_with(poly());
        install_clip(&mut session, synth);
        let planes = render(&mut session, request(ExportSource::Mix, rate));
        // Prepared at the render's rate, as the fork is.
        let reference = reference(poly(), rate);
        assert_note_at_beat_1("PolySynth", rate, &planes, &reference);
        assert_live_untouched(&mut session);
    }
}

/// **A param written on a synth reaches it and its fork.** The synth is a
/// `ParamNode` added as a graph node, so a write of `UnitParam::Volume` lands
/// in its master-volume cell, which the live synth reads on its next block
/// and an export's fork starts from: the exported note at a quarter volume
/// peaks at a quarter of the note at unity. (The Bevy `AudioParam` that
/// carried the write is the adapter's; the session's write is
/// `Graph::set_param`.)
///
/// Mutation (run, failed): the synth registered with no params (the session's
/// `param_session_node!(PolySynth)` replaced by `impl SessionNode for
/// PolySynth {}`) → the live cell stays at 1 and the export at full volume.
#[test]
fn a_param_on_a_synth_reaches_it_and_its_fork() {
    let peak = |volume: Option<f32>| {
        let mut session = Session::builder()
            .inputs(0)
            .outputs_floor(2)
            .build_detached(SampleRate::DEFAULT)
            .expect("a detached session builds");
        session.graph().set_sample_rate(SampleRate(RATE));
        let (synth, controls) = session.graph().add(poly());
        session.graph().set_outputs_from(synth);
        session.update();
        if let Some(v) = volume {
            session.graph().set_param(synth, UnitParam::Volume, v);
            session.update();
            assert_eq!(
                controls.get(UnitParam::Volume),
                Some(v),
                "the write reaches the live synth's volume cell"
            );
        }
        install_clip(&mut session, synth);
        let planes = render(&mut session, request(ExportSource::Mix, RATE));
        planes[0].iter().fold(0.0f32, |a, s| a.max(s.abs()))
    };
    let full = peak(None);
    assert!(full > 0.0, "the note sounds in the export");
    let quarter = peak(Some(0.25));
    assert!(
        (quarter - 0.25 * full).abs() < 1e-5,
        "the fork renders the volume that was set: {quarter} vs ¼ × {full}"
    );
}

/// **An exported `SoundFontUnit` plays its clip**, on the live unit's
/// preset, over the same decoded SoundFont, at the render's rate — a
/// 48 kHz unit exported at 96 kHz renders what a unit built at 96 kHz
/// does, not the 48 kHz note an octave down at half its frame. The live
/// unit keeps its clip.
///
/// Mutation (run, failed): `SoundFontUnit`'s `Node::prepare` not re-rating
/// it → at 96 kHz the 48 kHz unit renders the note an octave down.
#[test]
fn an_exported_soundfont_plays_its_clip() {
    let font = font();
    for rate in [RATE, 2.0 * RATE] {
        let (mut session, synth) = session_with(sf2(&font, RATE));
        install_clip(&mut session, synth);
        let planes = render(&mut session, request(ExportSource::Mix, rate));
        let reference = reference(sf2(&font, rate), rate);
        assert_note_at_beat_1("SoundFontUnit", rate, &planes, &reference);
        assert_live_untouched(&mut session);
    }
}

/// **A node export of a synth plays its clip**: silent until beat 1,
/// then the synth's own note ([`reference`]) sample for sample, and
/// the golden digest. The fork carries the clip itself.
///
/// The digest is the one the `bevy-tutti` test of this name recorded: that
/// graph was this one (the synth inserted, routed to the outputs, its clip
/// played through `Midi::play_clip`, which is the call the adapter made),
/// exported by the same session call.
///
/// Mutation (run, failed): the clip edge missing (`Midi::play_clip` leaving
/// off its `set_feed`) → silent ("the note enters on beat 1" fails). The
/// synth's fork not reset (`PolySynth::fork_instance` skipping
/// `reset_voices`) changes nothing here: a fork from the template never
/// sounded.
#[test]
fn a_synth_node_export_plays_its_clip() {
    let (mut session, synth) = session_with(poly());
    install_clip(&mut session, synth);
    let exported = render(&mut session, request(ExportSource::Node(synth), RATE));
    let reference = reference(poly(), RATE);
    assert_note_at_beat_1("PolySynth node export", RATE, &exported, &reference);
    assert_golden(
        "the synth's node export",
        digest(&exported),
        0x2f47_791e_730f_81b5,
    );
}
