//! A hosted plugin, forked by state transfer and exported: the reference CLAP
//! plugin through a real `plugin-server`, exported through a fork: a fresh
//! instance in a server of its own, loaded with the live one's state
//! (tutti-plugin's `PluginClient::fork_instance`), so the render thread never
//! drives the **live** plugin. The session halves of what `bevy-tutti`'s
//! export suite pinned before that crate was parked.
//!
//! **Build the server first**: `cargo build -p tutti-plugin-server`. It is
//! not a dev-dependency (a cycle through `tutti-plugin`), and without it every
//! test here fails, naming that command. They must fail rather than skip.

mod support;

use std::sync::Arc;
use std::time::{Duration, Instant};

use support::{clap_probe, plugin_server};
use tutti_export::Produced;
use tutti_fixtures::midi::{note_off, note_on};
use tutti_graph::NodeKey;
use tutti_graph::{Cx, ForkByClone, Io, Node, Prepare, Shape, Status};
use tutti_midi_runtime::TimedMidiEvent;
use tutti_nodes::testing::Const;
use tutti_plugin::catalog::PluginId;
use tutti_plugin_types::PluginForkError;
use tutti_session::{
    Bounce, BounceError, ExportClock, GraphSource, Latency, PluginLoadRequest, Range, Session,
    Stem, Tail,
};
use tutti_transport::{OfflineTimeline, OfflineTimelineConfig};
use tutti_types::ChannelLayout;
use tutti_types::{Beat, Bpm, SampleRate, Seconds};
use tutti_ump::{MidiChannel, MidiGroup};

const RATE: f64 = 48_000.0;

/// The probe's render modes this file drives (`tutti-clap-test-plugin`'s
/// `RenderMode`): `out = in + tag(port, ch)`; `out = in` delayed by the
/// 137 frames it reports; and a note gate.
const TAG_PASSTHROUGH: u32 = 1;
const LATENCY: u32 = 3;
const NOTES: u32 = 4;

/// A bounce of the mix into memory for `seconds` at `rate`: stereo, trimming
/// nothing and keeping no tail, on `clock`.
fn bounce_of(seconds: f64, rate: f64, clock: ExportClock) -> Bounce {
    Bounce::project()
        .master_buffers()
        .channels(ChannelLayout::STEREO)
        .range(Range::Seconds(Seconds(0.0), Seconds(seconds)))
        .sample_rate(SampleRate(rate))
        .clock(clock)
        .latency(Latency::Keep)
        .tail(Tail::None)
}

fn buffers(seconds: f64) -> Bounce {
    bounce_of(seconds, RATE, ExportClock::frozen())
}

/// The planes `bounce` renders; panics with the refusal otherwise.
fn planes(session: &mut Session, bounce: Bounce) -> Vec<Vec<f32>> {
    match session.bounce(bounce) {
        Ok(out) => match out.finished().map(|b| b.into_master()) {
            Some(Produced::Buffers(rendered)) => rendered.into_planes(),
            other => panic!("expected rendered buffers, got {other:?}"),
        },
        Err(e) => panic!("the bounce failed: {e:?}"),
    }
}

/// The planes of the one stem `bounce` names, in memory, rendered beside the
/// master; panics with the refusal otherwise.
fn stem_planes(session: &mut Session, bounce: Bounce) -> Vec<Vec<f32>> {
    match session.bounce(bounce) {
        Ok(out) => {
            let mut targets = out.finished().expect("nothing cancels it").into_targets();
            match targets.pop() {
                Some(Produced::Buffers(rendered)) if targets.len() == 1 => rendered.into_planes(),
                other => panic!("expected one stem held in memory, got {other:?}"),
            }
        }
        Err(e) => panic!("the bounce failed: {e:?}"),
    }
}

/// Set a switch the probe reads when its server starts. A fork's server
/// starts from this process too, so a switch set after the live load
/// reaches the fork only.
fn probe_env(key: &str, value: impl ToString) {
    // SAFETY: nextest runs each test in its own process, but the session, its
    // IPC threads and a live server exist by the time a later switch is set,
    // so this is not single-threaded. What makes it sound: the threads in this
    // process are Rust threads, which read the environment only through
    // `std::env` (behind std's environment lock), and no plugin code runs in
    // it. The server reads a switch only at spawn, and `Command::spawn` takes
    // that same lock to read `environ`; the already-running server never
    // re-reads it.
    unsafe { std::env::set_var(key, value.to_string()) };
}

/// This process's main thread. Each test is its own process under nextest, so
/// the claim cannot be refused.
fn main_thread() -> tutti_plugin_types::MainThread {
    tutti_plugin_types::MainThread::claim().expect("one test, one process")
}

/// A detached stereo session at the export's rate, with the reference plugin
/// loaded from `path` in `mode` and routed to the outputs.
fn session_with_probe_at(mode: u32, path: std::path::PathBuf) -> (Session, NodeKey) {
    probe_env("TUTTI_PLUGIN_SERVER", plugin_server().display());
    probe_env("TUTTI_CLAP_PROBE_RENDER_MODE", mode);
    let mut session = Session::builder()
        .inputs(0)
        .outputs_floor(2)
        .build_detached(SampleRate::DEFAULT)
        .expect("a detached session builds");
    session.graph().set_sample_rate(SampleRate(RATE));
    let probe = session
        .add_plugin_blocking(
            &main_thread(),
            PluginLoadRequest {
                id: PluginId::from_path(path),
                state: None,
            },
        )
        .expect("the reference CLAP plugin loads");
    session.graph().set_outputs_from(probe);
    session.update();
    (session, probe)
}

/// [`session_with_probe_at`] over the reference plugin.
fn session_with_probe(mode: u32) -> (Session, NodeKey) {
    session_with_probe_at(mode, clap_probe())
}

/// Feed the probe's two main inputs `node`'s output.
fn feed_with(
    session: &mut Session,
    probe: NodeKey,
    node: impl tutti_graph::IntoNode<Controls = ()>,
) {
    let (source, ()) = session.graph().insert(node);
    session
        .graph()
        .set_source(probe, 0, GraphSource::Node(source, 0));
    session
        .graph()
        .set_source(probe, 1, GraphSource::Node(source, 0));
    session.update();
}

/// A ramp: frame `n` is `n / 1024`, exact in `f32` over a render, so
/// a render one frame off reads a different value at every frame.
#[derive(Clone)]
struct Ramp {
    n: u32,
}

impl Node for Ramp {
    fn shape(&self) -> Shape {
        Shape::source_unassigned(ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        for y in io.output(0) {
            *y = self.n as f32 / 1024.0;
            self.n += 1;
        }
        Status::Modified
    }
    fn reset(&mut self) {
        self.n = 0;
    }
}

/// **A graph holding a hosted plugin as an effect exports**, rendered by
/// a fork of the plugin, and the trim is the delay it applies. In its
/// latency mode the probe delays its input by the 137 frames it reports;
/// the node adds its 64-frame pipeline block to that, and the export
/// trims the plan's 201. Fed a ramp, the render is the ramp from frame 0,
/// sample for sample: a trim one frame off in either direction reads the
/// neighbouring frame's value everywhere. (A constant input could not
/// tell a trim of 64 from one of 201.)
#[test]
fn a_plugin_effect_exports_through_its_fork() {
    let (mut session, probe) = session_with_probe(LATENCY);
    feed_with(&mut session, probe, ForkByClone(Ramp { n: 0 }));
    let planes = planes(&mut session, buffers(0.1).latency(Latency::Graph));
    for (c, plane) in planes.iter().enumerate().take(2) {
        for (i, &got) in plane.iter().enumerate() {
            let want = i as f32 / 1024.0;
            assert_eq!(got, want, "channel {c}: frame {i}");
        }
    }
}

/// **A plugin fork that cannot be built fails the export by the plugin's
/// key**, before anything renders (`Error::Fork(ForkError::Source)`), with
/// the plugin's own reason: the plugin's file is gone since it loaded, so no
/// fresh instance loads (`PluginForkError::Load`).
#[test]
fn a_plugin_fork_that_cannot_be_built_is_a_named_failure() {
    // A link of our own to the plugin, so removing it touches no other
    // test's (the suites publish one shared `.clap` link).
    let shared = clap_probe();
    let own = shared.with_extension(format!("{}.clap", std::process::id()));
    let _ = std::fs::remove_file(&own);
    let real = std::fs::canonicalize(&shared).expect("the reference plugin");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&real, &own).expect("link the plugin");
    #[cfg(windows)]
    std::fs::copy(&real, &own).expect("copy the plugin");
    let (mut session, probe) = session_with_probe_at(TAG_PASSTHROUGH, own.clone());
    std::fs::remove_file(&own).expect("remove the plugin's file");
    match session.bounce(buffers(0.05)) {
        Err(BounceError::Fork(tutti_graph::ForkError::Source { key, cause })) => {
            assert_eq!(key, probe, "a plugin file gone since it loaded");
            assert!(
                matches!(
                    cause.downcast_ref::<PluginForkError>(),
                    Some(PluginForkError::Load { path, .. }) if *path == own
                ),
                "{cause:?}"
            );
        }
        other => panic!("expected a keyed fork-source failure, got {other:?}"),
    }
}

/// **An exported plugin instrument plays its notes.** The plugin has a
/// MIDI event input, so a clip played at it (`Midi::play_clip`) is a clip
/// node wired to it, which the export forks with it and which reads the
/// render's `Env`. A note from beat 1 to beat 2 holds the probe's gate open
/// from beat 1 to beat 2 of the render's timeline, heard one pipeline
/// chunk (the export's 1024-frame block) later, and nowhere else.
///
/// At 90 BPM and 48 kHz a beat is 32 000 frames, a per-frame step binary
/// cannot hold, and the edges are asserted to the frame: 32 000 and
/// 64 000 (at the default 120 BPM, 24 000 and 48 000). The timeline
/// counts frames and derives the beat, and the clip places by frame, so
/// the edges land exactly; an accumulating timeline would land the note a
/// frame early.
///
/// Not trimmed: the probe reports 137 frames of latency in every mode
/// but delays only in its latency mode, so the graph's figure is not
/// this render's delay.
///
/// Rendered at the device's 48 kHz and at 96 kHz: the fork is launched at
/// the live rate and prepared at the render's, and the clip places its
/// notes at the rate the fork polls it at (a beat is 64 000 frames
/// there).
#[test]
fn an_exported_plugin_instrument_plays_its_clip() {
    for rate in [RATE, 2.0 * RATE] {
        instrument_at(rate);
    }
}

fn instrument_at(rate: f64) {
    let (mut session, probe) = session_with_probe(NOTES);
    let note = |beat: f64, on: bool| {
        let event = if on {
            note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0xFFFF)
        } else {
            note_off(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0)
        };
        TimedMidiEvent::new(Beat(beat), event)
    };
    session
        .midi()
        .play_clip(probe, vec![note(1.0, true), note(2.0, false)])
        .expect("the plugin takes a clip");
    session.update();

    let timeline = Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
        start_beat: Beat(0.0),
        tempo_map: Arc::new(tutti_tempo::TempoMap::constant(Bpm::of(90.0)).expect("a tempo")),
        sample_rate: SampleRate(rate),
        loop_range: None,
    }));
    let planes = stem_planes(
        &mut session,
        bounce_of(1.5, rate, ExportClock::timeline(timeline)).stem_with(Stem::buffers(probe)),
    );
    let open: Vec<usize> = planes[0]
        .iter()
        .enumerate()
        .filter(|&(_, &s)| s != 0.0)
        .map(|(i, _)| i)
        .collect();
    assert!(
        !open.is_empty(),
        "{rate} Hz: the instrument rendered no notes"
    );
    // The plugin's batcher holds one chunk: what it is sent in chunk `k`
    // it returns in chunk `k + 1`. A fork has no device, so its chunk is
    // the export's block.
    const PIPELINE: usize = tutti_export::GRAPH_MAX_BLOCK.get();
    let beat = 32_000 * (rate / RATE) as usize;
    let (on, off) = (open[0], *open.last().unwrap() + 1);
    assert_eq!(
        (on, off),
        (beat + PIPELINE, 2 * beat + PIPELINE),
        "{rate} Hz: the gate is open from beat 1 to beat 2, {PIPELINE} frames late"
    );
    assert_eq!(open.len(), off - on, "one unbroken note");
    assert!(planes[0][on] > 0.9, "the note's velocity");
}

/// **A plugin fork that dies mid-export fails the export by the plugin's
/// key**, promptly, instead of writing the silence it rendered after the
/// crash. The crash switch is set after the live instance loaded, so only
/// the fork's server reads it; it aborts on its 8th block.
#[test]
fn a_plugin_fork_that_crashes_mid_export_is_a_named_failure() {
    let (mut session, probe) = session_with_probe(TAG_PASSTHROUGH);
    feed_with(&mut session, probe, Const::mono(0.25));
    probe_env("TUTTI_CLAP_PROBE_CRASH_ON_BLOCK", 8);
    let started = Instant::now();
    match session.bounce(buffers(2.0)) {
        Err(BounceError::Export(tutti_export::Error::ForkFailed { key, kind, .. })) => {
            assert_eq!(key, probe);
            assert_eq!(kind, tutti_graph::ForkFaultKind::Crashed);
        }
        other => panic!("expected a keyed fork failure, got {other:?}"),
    }
    assert!(
        started.elapsed() < Duration::from_secs(10),
        "the failure took {:?}",
        started.elapsed()
    );
}
